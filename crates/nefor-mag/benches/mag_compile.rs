mod bench_support;
#[path = "support/cache_scenarios.rs"]
mod cache_scenarios;

use bench_support::*;
use cache_scenarios::*;
use mlua::{Lua, LuaSerdeExt, Table, Value as LuaValue};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_SAMPLES: usize = 30;
const DEFAULT_WARMUPS: usize = 3;

const BUILD_SOURCE_ROOT: Option<&str> = option_env!("NEFOR_MAG_BENCH_BUILD_SOURCE_ROOT");
const BUILD_SOURCE_REF: Option<&str> = option_env!("NEFOR_MAG_BENCH_BUILD_SOURCE_REF");
const BUILD_SOURCE_TREE: Option<&str> = option_env!("NEFOR_MAG_BENCH_BUILD_SOURCE_TREE");
const BUILD_SOURCE_DIRTY: Option<&str> = option_env!("NEFOR_MAG_BENCH_BUILD_SOURCE_DIRTY");

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let cache_suite = value_after(&args, "--suite").as_deref() == Some("cache");
    if cache_suite && args.iter().any(|arg| arg == "--worker") {
        if let Err(error) = cache_worker_main(&args) {
            eprintln!("MAG cache-scenario worker failed: {error}");
            std::process::exit(2);
        }
        return;
    }
    if cache_suite && args.iter().any(|arg| arg == "--paired") {
        if let Err(error) = cache_paired_main(&args) {
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    if args.iter().any(|arg| arg == "--worker") {
        if let Err(error) = worker_main(&args) {
            eprintln!("MAG benchmark worker failed: {error}");
            std::process::exit(2);
        }
        return;
    }
    if args.iter().any(|arg| arg == "--paired") {
        if let Err(error) = paired_main(&args) {
            let root = workspace_root();
            let output = value_after(&args, "--output");
            let rejection = json!({
                "schema_version": SCHEMA_VERSION,
                "report_kind": "paired_worker_rejection",
                "authoritative": false,
                "status": "rejected",
                "error": error,
            });
            if let Some(path) = output {
                write_json(&root, &path, &rejection, "paired worker rejection");
            }
            eprintln!("{error}");
            std::process::exit(2);
        }
        return;
    }
    marginal_main(&args);
}

fn marginal_main(args: &[String]) {
    let root = workspace_root();
    let samples = positive_env("MAG_BENCH_SAMPLES", DEFAULT_SAMPLES);
    let warmups = positive_env("MAG_BENCH_WARMUPS", DEFAULT_WARMUPS);
    let baseline = value_after(args, "--baseline");
    let output_path = value_after(args, "--output");
    let comparison_path = value_after(args, "--comparison-output");
    let gate = args.iter().any(|arg| arg == "--gate");
    validate_args(
        args,
        &[
            "--bench",
            "--baseline",
            "--output",
            "--comparison-output",
            "--gate",
        ],
        &["--baseline", "--output", "--comparison-output"],
    );

    let scratch = fresh_scratch(&root);
    let contracts = load_runtime_contracts(&root.join("plugins/mag/lua/mag-kernel/init.lua"));
    let mut timed = timed_cases(&root, &scratch, &contracts);
    let mut oracle_fixtures = oracle_cases(&root, &scratch, &contracts);
    refresh_fixture_fingerprints(&mut timed);
    refresh_fixture_fingerprints(&mut oracle_fixtures);
    let definition_hash = definition_hash(&timed, &oracle_fixtures);
    let inherited_count = timed.len().saturating_sub(12);
    let parent_workload_fingerprint = catalog_fingerprint(&timed[..inherited_count]);
    let workload_fingerprint = catalog_fingerprint(&timed);
    let oracle_fingerprint = catalog_fingerprint(&oracle_fixtures);
    assert_eq!(
        parent_workload_fingerprint,
        CURRENT_MAIN_A0_WORKLOAD_FINGERPRINT
    );
    assert_eq!(oracle_fingerprint, CURRENT_MAIN_A0_ORACLE_FINGERPRINT);
    assert_eq!(workload_fingerprint, PHASE0_WORKLOAD_FINGERPRINT);
    let identity = report_identity(
        &root,
        workload_fingerprint,
        parent_workload_fingerprint,
        oracle_fingerprint,
    );
    let cases = timed
        .iter()
        .map(|case| run_case(case, samples, warmups))
        .collect::<Vec<_>>();
    let oracles = oracle_fixtures.iter().map(observe).collect::<Vec<_>>();
    assert_catalog_membership(&cases[..inherited_count], &oracles);
    let report = Report {
        schema_version: SCHEMA_VERSION,
        identity: Some(identity),
        metadata: metadata(&root, samples, warmups, definition_hash),
        counter_semantics: counter_semantics(),
        statistics_policy: statistics_policy(),
        recommendation: recommendation(&cases),
        exclusive_sections: derived_exclusive_sections(&cases),
        cases,
        oracles,
    };
    let comparison = baseline.map(|path| {
        let baseline_path = absolute_from(&root, &path);
        let baseline: Report =
            serde_json::from_slice(&fs::read(&baseline_path).unwrap_or_else(|error| {
                panic!("read baseline {}: {error}", baseline_path.display())
            }))
            .unwrap_or_else(|error| panic!("parse baseline {path}: {error}"));
        compare_reports(&baseline, &report, gate)
    });
    if gate && comparison.is_none() {
        panic!("--gate requires --baseline");
    }
    if comparison.is_some() && comparison_path.is_none() {
        panic!("comparison requires --comparison-output so the verdict is persistent");
    }
    if let (Some(comparison), Some(path)) = (&comparison, comparison_path) {
        write_json(&root, &path, comparison, "comparison artifact");
    }
    if let Some(path) = output_path {
        write_json(&root, &path, &report, "benchmark report");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize report")
        );
    }
    fs::remove_dir_all(&scratch).ok();
    if gate
        && comparison
            .as_ref()
            .is_some_and(|artifact| !artifact.overall.passed)
    {
        std::process::exit(2);
    }
}

struct CacheWorkerProcess {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    hello: CacheWorkerHello,
}

impl CacheWorkerProcess {
    fn spawn(endpoint: &WorkerEndpoint) -> Result<Self, String> {
        let mut child = Command::new(&endpoint.executable_path)
            .args(["--worker", "--suite", "cache", "--source-root"])
            .arg(&endpoint.clean_source_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn cache-scenario worker: {error}"))?;
        let input = BufWriter::new(child.stdin.take().ok_or("worker stdin unavailable")?);
        let mut output = BufReader::new(child.stdout.take().ok_or("worker stdout unavailable")?);
        let mut line = String::new();
        if output
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Err("cache-scenario worker exited before protocol hello".into());
        }
        let hello = serde_json::from_str(&line)
            .map_err(|error| format!("malformed cache-scenario worker hello: {error}"))?;
        Ok(Self {
            child,
            input,
            output,
            hello,
        })
    }

    fn request(&mut self, request: &CacheWorkerRequest) -> Result<CacheWorkerResponse, String> {
        serde_json::to_writer(&mut self.input, request).map_err(|error| error.to_string())?;
        self.input
            .write_all(b"\n")
            .map_err(|error| error.to_string())?;
        self.input.flush().map_err(|error| error.to_string())?;
        let mut line = String::new();
        if self
            .output
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Err("cache-scenario worker closed before response".into());
        }
        serde_json::from_str(&line)
            .map_err(|error| format!("malformed cache-scenario response: {error}"))
    }
}

impl Drop for CacheWorkerProcess {
    fn drop(&mut self) {
        drop(self.input.flush());
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn cache_paired_main(args: &[String]) -> Result<(), String> {
    validate_args(
        args,
        &[
            "--bench",
            "--paired",
            "--suite",
            "--calibration",
            "--baseline-worker",
            "--baseline-root",
            "--candidate-worker",
            "--candidate-root",
            "--output",
        ],
        &[
            "--suite",
            "--baseline-worker",
            "--baseline-root",
            "--candidate-worker",
            "--candidate-root",
            "--output",
        ],
    );
    let root = workspace_root();
    let calibration = args.iter().any(|arg| arg == "--calibration");
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let endpoint = |worker_flag: &str, root_flag: &str| -> Result<WorkerEndpoint, String> {
        match (value_after(args, worker_flag), value_after(args, root_flag)) {
            (Some(worker), Some(source)) => Ok(WorkerEndpoint {
                executable_path: worker.into(),
                clean_source_root: source.into(),
            }),
            (None, None) if calibration => Ok(WorkerEndpoint {
                executable_path: current_exe.clone(),
                clean_source_root: root.clone(),
            }),
            (None, None) => {
                Err("cache suite requires explicit distinct workers outside calibration".into())
            }
            _ => Err(format!(
                "{worker_flag} and {root_flag} must be supplied together"
            )),
        }
    };
    let baseline_endpoint = endpoint("--baseline-worker", "--baseline-root")?;
    let candidate_endpoint = endpoint("--candidate-worker", "--candidate-root")?;
    let mut baseline = CacheWorkerProcess::spawn(&baseline_endpoint)?;
    let mut candidate = CacheWorkerProcess::spawn(&candidate_endpoint)?;
    validate_cache_worker_endpoint(&baseline_endpoint, &baseline.hello)?;
    validate_cache_worker_endpoint(&candidate_endpoint, &candidate.hello)?;
    validate_hello_pair(&baseline.hello, &candidate.hello)?;
    let identical = baseline.hello.executable_identity.sha256
        == candidate.hello.executable_identity.sha256
        || (baseline.hello.source_identity.source_ref
            == candidate.hello.source_identity.source_ref
            && baseline.hello.source_identity.tree == candidate.hello.source_identity.tree);
    if identical && !calibration {
        return Err("identical cache-scenario endpoints are calibration-only".into());
    }
    let samples = positive_env("MAG_BENCH_SAMPLES", DEFAULT_SAMPLES);
    let mut reports = Vec::new();
    let mut sequence = 0;
    for (index, (left, right)) in baseline
        .hello
        .cases
        .clone()
        .into_iter()
        .zip(candidate.hello.cases.clone())
        .enumerate()
    {
        eprintln!(
            "cache scenario {}/{}: {}",
            index + 1,
            baseline.hello.cases.len(),
            left.definition.name
        );
        let mut baseline_samples = Vec::new();
        let mut candidate_samples = Vec::new();
        let mut baseline_batch_ns = Vec::new();
        let mut candidate_batch_ns = Vec::new();
        for block in 0..samples {
            let request = CacheWorkerRequest {
                sequence,
                scenario_fingerprint: left.definition_fingerprint.clone(),
                batch_count: 1,
            };
            sequence += 1;
            let (baseline_response, candidate_response) = if (index + block) % 2 == 0 {
                (baseline.request(&request)?, candidate.request(&request)?)
            } else {
                let candidate_response = candidate.request(&request)?;
                let baseline_response = baseline.request(&request)?;
                (baseline_response, candidate_response)
            };
            validate_response(&baseline.hello, &left, &request, &baseline_response)?;
            validate_response(&candidate.hello, &right, &request, &candidate_response)?;
            if baseline_response.samples[0].semantic_observation
                != candidate_response.samples[0].semantic_observation
            {
                return Err(format!(
                    "{} exact semantic observation mismatch",
                    left.definition.name
                ));
            }
            baseline_batch_ns.push(baseline_response.raw_batch_ns);
            candidate_batch_ns.push(candidate_response.raw_batch_ns);
            baseline_samples.extend(baseline_response.samples);
            candidate_samples.extend(candidate_response.samples);
        }
        reports.push(CachePairedCaseReport {
            name: left.definition.name,
            baseline_samples,
            candidate_samples,
            baseline_batch_ns,
            candidate_batch_ns,
        });
    }
    let report = CachePairedReport {
        schema_version: 1,
        report_kind: "cache_scenario_calibration".into(),
        authoritative: false,
        protocol_version: CACHE_SCENARIO_PROTOCOL_VERSION.into(),
        catalog_version: CACHE_SCENARIO_CATALOG_VERSION.into(),
        catalog_fingerprint: baseline.hello.catalog_fingerprint.clone(),
        samples_per_case: samples,
        baseline: baseline.hello.clone(),
        candidate: candidate.hello.clone(),
        cases: reports,
    };
    if let Some(path) = value_after(args, "--output") {
        write_json(&root, &path, &report, "cache-scenario calibration");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    }
    Ok(())
}

fn validate_cache_worker_endpoint(
    endpoint: &WorkerEndpoint,
    hello: &CacheWorkerHello,
) -> Result<(), String> {
    if executable_identity(&endpoint.executable_path)? != hello.executable_identity {
        return Err("cache worker executable identity does not match endpoint".into());
    }
    let root = endpoint
        .clean_source_root
        .canonicalize()
        .map_err(|error| format!("canonicalize cache worker source root: {error}"))?;
    if root != hello.source_identity.source_root || hello.source_identity.dirty {
        return Err("cache worker source identity does not match clean endpoint".into());
    }
    Ok(())
}

fn cache_worker_main(args: &[String]) -> Result<(), String> {
    validate_args(
        args,
        &["--worker", "--suite", "--source-root"],
        &["--suite", "--source-root"],
    );
    let root = PathBuf::from(value_after(args, "--source-root").ok_or("missing source root")?)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let source_identity = worker_source_identity(&root)?;
    validate_embedded_build_source(&source_identity)?;
    let executable_identity =
        executable_identity(&std::env::current_exe().map_err(|error| error.to_string())?)?;
    let cases = manifests(&root);
    let hello = CacheWorkerHello {
        protocol_version: CACHE_SCENARIO_PROTOCOL_VERSION.into(),
        catalog_version: CACHE_SCENARIO_CATALOG_VERSION.into(),
        catalog_fingerprint: cache_catalog_fingerprint(),
        executable_identity: executable_identity.clone(),
        source_identity: source_identity.clone(),
        cases,
    };
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    serde_json::to_writer(&mut output, &hello).map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    for line in std::io::stdin().lock().lines() {
        let request: CacheWorkerRequest = serde_json::from_str(&line.map_err(|e| e.to_string())?)
            .map_err(|error| error.to_string())?;
        let manifest = hello
            .cases
            .iter()
            .find(|case| case.definition_fingerprint == request.scenario_fingerprint)
            .ok_or("missing cache scenario")?;
        let (samples, raw_batch_ns) =
            run_batch(&root, &manifest.definition.name, request.batch_count);
        let response = CacheWorkerResponse {
            sequence: request.sequence,
            scenario_name: manifest.definition.name.clone(),
            scenario_fingerprint: manifest.definition_fingerprint.clone(),
            executable_identity: executable_identity.clone(),
            source_identity: source_identity.clone(),
            samples,
            raw_batch_ns,
        };
        serde_json::to_writer(&mut output, &response).map_err(|error| error.to_string())?;
        output.write_all(b"\n").map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
    }
    Ok(())
}

struct WorkerProcess {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    hello: WorkerHello,
}

impl WorkerProcess {
    fn spawn(endpoint: &WorkerEndpoint, warmups: usize) -> Result<Self, String> {
        let mut child = Command::new(&endpoint.executable_path)
            .args(["--worker", "--source-root"])
            .arg(&endpoint.clean_source_root)
            .env("MAG_BENCH_WARMUPS", warmups.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                format!(
                    "spawn worker {}: {error}",
                    endpoint.executable_path.display()
                )
            })?;
        let input = BufWriter::new(child.stdin.take().ok_or("worker stdin unavailable")?);
        let mut output = BufReader::new(child.stdout.take().ok_or("worker stdout unavailable")?);
        let mut line = String::new();
        if output
            .read_line(&mut line)
            .map_err(|error| format!("read worker hello: {error}"))?
            == 0
        {
            return Err("worker exited before protocol hello".into());
        }
        let hello = serde_json::from_str(&line)
            .map_err(|error| format!("malformed worker hello: {error}"))?;
        Ok(Self {
            child,
            input,
            output,
            hello,
        })
    }

    fn request(&mut self, request: &WorkerRequest) -> Result<WorkerResponse, String> {
        serde_json::to_writer(&mut self.input, request)
            .map_err(|error| format!("serialize worker request: {error}"))?;
        self.input
            .write_all(b"\n")
            .map_err(|error| format!("write worker request: {error}"))?;
        self.input
            .flush()
            .map_err(|error| format!("flush worker request: {error}"))?;
        let mut line = String::new();
        if self
            .output
            .read_line(&mut line)
            .map_err(|error| format!("read worker response: {error}"))?
            == 0
        {
            return Err("worker crashed or closed stdout before response".into());
        }
        decode_worker_response(&line)
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        drop(self.input.flush());
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn paired_main(args: &[String]) -> Result<(), String> {
    validate_args(
        args,
        &[
            "--bench",
            "--paired",
            "--baseline-worker",
            "--baseline-root",
            "--candidate-worker",
            "--candidate-root",
            "--target-case",
            "--target-counter",
            "--calibration",
            "--output",
            "--gate",
        ],
        &[
            "--baseline-worker",
            "--baseline-root",
            "--candidate-worker",
            "--candidate-root",
            "--target-case",
            "--target-counter",
            "--output",
        ],
    );
    let root = workspace_root();
    let calibration = args.iter().any(|arg| arg == "--calibration");
    let gate = args.iter().any(|arg| arg == "--gate");
    let target_case = value_after(args, "--target-case");
    let target_counter = value_after(args, "--target-counter");
    if calibration && gate {
        return Err("--calibration cannot satisfy --gate".into());
    }
    if target_case.is_some() != target_counter.is_some() {
        return Err("--target-case and --target-counter must be supplied together".into());
    }
    if gate && target_case.is_none() {
        return Err("--gate requires --target-case and --target-counter".into());
    }
    let samples = positive_env("MAG_BENCH_SAMPLES", DEFAULT_SAMPLES);
    let warmups = positive_env("MAG_BENCH_WARMUPS", DEFAULT_WARMUPS);
    let current_exe =
        std::env::current_exe().map_err(|error| format!("current executable: {error}"))?;
    let endpoint = |worker_flag: &str, root_flag: &str| -> Result<WorkerEndpoint, String> {
        let executable_path = value_after(args, worker_flag).map(PathBuf::from);
        let clean_source_root = value_after(args, root_flag).map(PathBuf::from);
        match (executable_path, clean_source_root) {
            (Some(executable_path), Some(clean_source_root)) => Ok(WorkerEndpoint {
                executable_path,
                clean_source_root,
            }),
            (None, None) if calibration => Ok(WorkerEndpoint {
                executable_path: current_exe.clone(),
                clean_source_root: root.clone(),
            }),
            (None, None) => Err(
                "identical endpoints are calibration-only and cannot satisfy an optimization gate"
                    .into(),
            ),
            _ => Err(format!(
                "{worker_flag} and {root_flag} must be supplied together"
            )),
        }
    };
    let baseline_endpoint = endpoint("--baseline-worker", "--baseline-root")?;
    let candidate_endpoint = endpoint("--candidate-worker", "--candidate-root")?;
    if gate && ![30, 60, 120].contains(&samples) {
        return Err(
            "optimization gates require a preregistered sample count: 30, 60, or 120".into(),
        );
    }
    let mut baseline = WorkerProcess::spawn(&baseline_endpoint, warmups)?;
    let mut candidate = WorkerProcess::spawn(&candidate_endpoint, warmups)?;
    validate_worker_endpoint(&baseline_endpoint, &baseline.hello)?;
    validate_worker_endpoint(&candidate_endpoint, &candidate.hello)?;
    let compatibility_failures =
        validate_worker_pair(&baseline.hello, &candidate.hello, calibration);
    if !compatibility_failures.is_empty() {
        return Err(compatibility_failures.join("; "));
    }
    let baseline_cases = baseline.hello.cases.clone();
    let candidate_cases = candidate.hello.cases.clone();
    let case_count = baseline_cases.len();
    let mut case_reports = Vec::with_capacity(case_count);
    let mut sequence = 0;
    for (baseline_case, candidate_case) in baseline_cases.iter().zip(&candidate_cases) {
        let batch_count = calibrate_batch_count(baseline_case.baseline_warmup_ns, 5_000_000);
        let seed = u64::from_le_bytes(
            Sha256::digest(baseline_case.name.as_bytes())[..8]
                .try_into()
                .map_err(|_| "case seed length")?,
        );
        let generated_order = balanced_pair_schedule(seed, samples);
        let mut actual_order = Vec::with_capacity(samples);
        let mut raw_samples = Vec::with_capacity(samples);
        for (block, order) in generated_order.iter().copied().enumerate() {
            let request = WorkerRequest {
                sequence,
                case_fingerprint: baseline_case.case_fingerprint.clone(),
                batch_count,
                measurement_kind: MeasurementKind::TimedBatch,
            };
            sequence += 1;
            let (baseline_response, candidate_response) = match order {
                PairOrder::AB => (baseline.request(&request)?, candidate.request(&request)?),
                PairOrder::BA => {
                    let candidate_response = candidate.request(&request)?;
                    let baseline_response = baseline.request(&request)?;
                    (baseline_response, candidate_response)
                }
            };
            validate_worker_response(&baseline.hello, baseline_case, &request, &baseline_response)?;
            validate_worker_response(
                &candidate.hello,
                candidate_case,
                &request,
                &candidate_response,
            )?;
            if !observations_match(
                &baseline_response.semantic_observation,
                &candidate_response.semantic_observation,
            ) {
                return Err(format!(
                    "{} exact semantic observation mismatch",
                    baseline_case.name
                ));
            }
            actual_order.push(order);
            raw_samples.push(PairedSample {
                block,
                generated_order: order,
                actual_order: order,
                batch_count,
                baseline_batch_ns: baseline_response.raw_batch_ns,
                candidate_batch_ns: candidate_response.raw_batch_ns,
            });
        }
        let semantic_match = observations_match(
            &baseline_case.semantic_observation,
            &candidate_case.semantic_observation,
        );
        case_reports.push(PairedCaseReport {
            name: baseline_case.name.clone(),
            family: baseline_case.family.clone(),
            stage: baseline_case.stage.clone(),
            seed,
            generated_order,
            actual_order,
            batch_count,
            analysis: analyze_paired_samples(&raw_samples, case_count),
            raw_paired_batch_samples: raw_samples,
            baseline_semantic_observation: baseline_case.semantic_observation.clone(),
            candidate_semantic_observation: candidate_case.semantic_observation.clone(),
            semantic_match,
            baseline_logical_counters: baseline_case.logical_counters.clone(),
            candidate_logical_counters: candidate_case.logical_counters.clone(),
            baseline_attribution: None,
            candidate_attribution: None,
        });
    }
    let collect_attributions = |cases: &[WorkerCaseManifest]| {
        let mut out = Vec::new();
        for larger in cases {
            let Some(prerequisite) = larger.declared_direct_prerequisite.as_deref() else {
                continue;
            };
            if let Some(smaller) = cases.iter().find(|case| {
                case.family == larger.family
                    && case.size == larger.size
                    && case.stage == prerequisite
            }) {
                out.push(stage_attribution(smaller, larger));
            }
        }
        out
    };
    let baseline_attributions = collect_attributions(&baseline.hello.cases);
    let candidate_attributions = collect_attributions(&candidate.hello.cases);
    for report in &mut case_reports {
        let binding = baseline
            .hello
            .cases
            .iter()
            .find(|case| case.name == report.name)
            .map(|case| case.forced_terminal_binding.as_str())
            .unwrap_or("");
        report.baseline_attribution = baseline_attributions
            .iter()
            .find(|attribution| attribution.forced_terminal_binding == binding)
            .cloned();
        report.candidate_attribution = candidate_attributions
            .iter()
            .find(|attribution| attribution.forced_terminal_binding == binding)
            .cloned();
    }
    let semantic_pass = case_reports.iter().all(|case| case.semantic_match)
        && baseline.hello.oracles.len() == candidate.hello.oracles.len()
        && baseline
            .hello
            .oracles
            .iter()
            .zip(&candidate.hello.oracles)
            .all(|(left, right)| {
                left.name == right.name
                    && left.policy == right.policy
                    && left.observation == right.observation
            });
    let performance_pass = case_reports
        .iter()
        .all(|case| case.analysis.verdict == ConfidenceVerdict::Pass);
    let selected_target = target_case
        .as_deref()
        .and_then(|name| case_reports.iter().find(|case| case.name == name));
    let (target_median, target_logical_counter) =
        paired_target_verdicts(selected_target, target_counter.as_deref());
    let compatibility = GateVerdict { passed: true, detail: "clean source roots, executables, worker protocols, workloads, fixtures, and policies match".into() };
    let semantic = GateVerdict {
        passed: semantic_pass,
        detail: if semantic_pass {
            "all exact timed and oracle observations match"
        } else {
            "one or more exact timed or oracle observations differ"
        }
        .into(),
    };
    let performance = GateVerdict {
        passed: performance_pass,
        detail: if performance_pass {
            "every simultaneous upper p90 bound is <= 1.10"
        } else {
            "one or more p90 bounds are regression or inconclusive"
        }
        .into(),
    };
    let authoritative = gate && !calibration;
    let overall_pass = authoritative
        && semantic_pass
        && performance_pass
        && target_median.passed
        && target_logical_counter.passed;
    let report = PairedWorkerReport {
        schema_version: SCHEMA_VERSION,
        report_kind: if calibration {
            "a0_self_calibration"
        } else {
            "same_version_optimization_gate"
        }
        .into(),
        authoritative,
        status: if calibration {
            "calibration_only_non_authoritative"
        } else if overall_pass {
            "pass"
        } else {
            "failed"
        }
        .into(),
        protocol_version: WORKER_PROTOCOL_VERSION.into(),
        benchmark_definition_identity: baseline.hello.benchmark_definition_identity.clone(),
        statistics_policy: statistics_policy(),
        baseline: baseline.hello.clone(),
        candidate: candidate.hello.clone(),
        compatibility,
        semantic,
        performance,
        target_case,
        target_counter,
        target_median,
        target_logical_counter,
        cases: case_reports,
        baseline_stage_attributions: baseline_attributions,
        candidate_stage_attributions: candidate_attributions,
        overall: GateVerdict {
            passed: overall_pass,
            detail: if calibration {
                "A0 self-comparison is calibration only and can never create an optimization pass"
            } else if overall_pass {
                "distinct clean baseline and candidate satisfy every paired gate"
            } else {
                "candidate failed one or more required paired gates"
            }
            .into(),
        },
    };
    if let Some(path) = value_after(args, "--output") {
        write_json(&root, &path, &report, "paired worker report");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    }
    if gate && !report.overall.passed {
        std::process::exit(2);
    }
    Ok(())
}

fn worker_main(args: &[String]) -> Result<(), String> {
    validate_args(args, &["--worker", "--source-root"], &["--source-root"]);
    let root =
        PathBuf::from(value_after(args, "--source-root").ok_or("--worker requires --source-root")?)
            .canonicalize()
            .map_err(|error| format!("canonicalize worker source root: {error}"))?;
    let source_identity = worker_source_identity(&root)?;
    validate_embedded_build_source(&source_identity)?;
    let executable_identity = executable_identity(
        &std::env::current_exe().map_err(|error| format!("current executable: {error}"))?,
    )?;
    let warmups = positive_env("MAG_BENCH_WARMUPS", DEFAULT_WARMUPS);
    let scratch = fresh_scratch(&root);
    let contracts = load_runtime_contracts(&root.join("plugins/mag/lua/mag-kernel/init.lua"));
    let mut timed = timed_cases(&root, &scratch, &contracts);
    let mut oracle_fixtures = oracle_cases(&root, &scratch, &contracts);
    refresh_fixture_fingerprints(&mut timed);
    refresh_fixture_fingerprints(&mut oracle_fixtures);
    let inherited_count = timed.len().saturating_sub(12);
    let parent = catalog_fingerprint(&timed[..inherited_count]);
    let workload = catalog_fingerprint(&timed);
    let oracle = catalog_fingerprint(&oracle_fixtures);
    if parent != CURRENT_MAIN_A0_WORKLOAD_FINGERPRINT
        || workload != PHASE0_WORKLOAD_FINGERPRINT
        || oracle != CURRENT_MAIN_A0_ORACLE_FINGERPRINT
    {
        return Err("compiled benchmark catalog identity does not match worker fixtures".into());
    }
    let benchmark_definition_identity = definition_hash(&timed, &oracle_fixtures);
    let mut cases = Vec::with_capacity(timed.len());
    for case in &timed {
        let semantic_observation = worker_case_observation(case);
        let logical_counters = worker_case_counters(case);
        let baseline_warmup_ns = warm_worker_case(case, warmups);
        let boundary_fingerprint = stage_fixture_source_fingerprint(case);
        cases.push(WorkerCaseManifest {
            name: case.name.clone(),
            family: case.family.clone(),
            stage: case.stage.clone(),
            policy: case.policy.clone(),
            size: case.size,
            case_fingerprint: case.fixture_fingerprint.clone(),
            workload_fingerprint: workload.clone(),
            fixture_source_fingerprint: boundary_fingerprint,
            topology_fingerprint: case.topology_fingerprint.clone(),
            forced_terminal_binding: forced_terminal_binding(&case.stage).into(),
            forcing_dependency_proof: forcing_dependency_proof(case),
            declared_direct_prerequisite: declared_direct_prerequisite(&case.stage)
                .map(str::to_owned),
            expected_semantic_artifact_fingerprint: semantic_fingerprint(&semantic_observation),
            semantic_observation,
            logical_counters,
            baseline_warmup_ns,
        });
    }
    let oracles = oracle_fixtures.iter().map(observe).collect::<Vec<_>>();
    let hello = WorkerHello {
        protocol_version: WORKER_PROTOCOL_VERSION.into(),
        executable_identity: executable_identity.clone(),
        source_identity: source_identity.clone(),
        benchmark_definition_identity,
        workload_catalog_version: PHASE0_WORKLOAD_CATALOG_VERSION.into(),
        oracle_catalog_version: ORACLE_CATALOG_VERSION.into(),
        statistics_policy_version: STATISTICS_POLICY_VERSION.into(),
        warmup_iterations: warmups,
        cases,
        oracles,
    };
    let stdout = std::io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    serde_json::to_writer(&mut output, &hello)
        .map_err(|error| format!("serialize worker hello: {error}"))?;
    output
        .write_all(b"\n")
        .map_err(|error| format!("write worker hello: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("flush worker hello: {error}"))?;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| format!("read worker request: {error}"))?;
        let request: WorkerRequest = serde_json::from_str(&line)
            .map_err(|error| format!("malformed worker request: {error}"))?;
        let (index, manifest) = hello
            .cases
            .iter()
            .enumerate()
            .find(|(_, case)| case.case_fingerprint == request.case_fingerprint)
            .ok_or_else(|| format!("missing case {}", request.case_fingerprint))?;
        let raw_batch_ns = match request.measurement_kind {
            MeasurementKind::TimedBatch => measure_worker_batch(&timed[index], request.batch_count),
        };
        let response = WorkerResponse {
            sequence: request.sequence,
            case_name: manifest.name.clone(),
            case_fingerprint: manifest.case_fingerprint.clone(),
            executable_identity: executable_identity.clone(),
            source_identity: source_identity.clone(),
            semantic_observation: manifest.semantic_observation.clone(),
            logical_counters: manifest.logical_counters.clone(),
            raw_batch_ns,
        };
        serde_json::to_writer(&mut output, &response)
            .map_err(|error| format!("serialize worker response: {error}"))?;
        output
            .write_all(b"\n")
            .map_err(|error| format!("write worker response: {error}"))?;
        output
            .flush()
            .map_err(|error| format!("flush worker response: {error}"))?;
    }
    fs::remove_dir_all(scratch).ok();
    Ok(())
}

fn validate_embedded_build_source(runtime: &WorkerSourceIdentity) -> Result<(), String> {
    let build_root =
        BUILD_SOURCE_ROOT.ok_or("worker executable lacks embedded build source root")?;
    let build_ref = BUILD_SOURCE_REF.ok_or("worker executable lacks embedded build source ref")?;
    let build_tree =
        BUILD_SOURCE_TREE.ok_or("worker executable lacks embedded build source tree")?;
    let build_dirty =
        BUILD_SOURCE_DIRTY.ok_or("worker executable lacks embedded build dirty state")?;
    let canonical_build_root = PathBuf::from(build_root)
        .canonicalize()
        .map_err(|error| format!("canonicalize embedded build source root: {error}"))?;
    if build_dirty != "false" || runtime.dirty {
        return Err("worker executable and source root must come from a clean tree".into());
    }
    if canonical_build_root != runtime.source_root
        || build_ref != runtime.source_ref
        || build_tree != runtime.tree
    {
        return Err("worker executable provenance does not match its clean source identity".into());
    }
    Ok(())
}

fn worker_source_identity(root: &Path) -> Result<WorkerSourceIdentity, String> {
    let source_ref = checked_command(root, &["git", "rev-parse", "HEAD"])?;
    let tree = checked_command(root, &["git", "rev-parse", "HEAD^{tree}"])?;
    let dirty = !checked_command(root, &["git", "status", "--porcelain"])?.is_empty();
    Ok(WorkerSourceIdentity {
        source_root: root.to_path_buf(),
        source_ref,
        tree,
        dirty,
    })
}

fn checked_command(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .output()
        .map_err(|error| format!("run {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

fn validate_args(args: &[String], flags: &[&str], value_flags: &[&str]) {
    for (index, arg) in args.iter().enumerate() {
        if flags.contains(&arg.as_str()) {
            continue;
        }
        if index > 0 && value_flags.contains(&args[index - 1].as_str()) {
            continue;
        }
        panic!("unknown benchmark argument: {arg}");
    }
}

fn absolute_from(root: &Path, path: &str) -> PathBuf {
    if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    }
}

fn write_json(root: &Path, path: &str, value: &impl Serialize, label: &str) {
    let path = absolute_from(root, path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create artifact directory");
    }
    let encoded = serde_json::to_string_pretty(value).expect("serialize artifact");
    fs::write(&path, format!("{encoded}\n")).expect("write artifact");
    eprintln!("wrote {label} {}", path.display());
}
fn nefor_module_roots(root: &Path) -> Vec<ModuleRoot> {
    vec![
        ModuleRoot::implementation("nefor-mag", root.join("mag/lib")),
        ModuleRoot::workload(
            "nefor-agent-config",
            root.join("examples/nefor-agent/mag/lib"),
        ),
    ]
}

fn timed_cases(root: &Path, scratch: &Path, contracts: &Value) -> Vec<Fixture> {
    let core_roots = vec![];
    let nefor_roots = nefor_module_roots(root);
    let nefor_inputs = json!({"factory_contracts": contracts});
    let mut cases = vec![fixture(
        scratch,
        "trivial",
        "trivial",
        "compile",
        None,
        "type Empty {}\nartifact(Empty {})",
        core_roots.clone(),
        json!({}),
        None,
        "timed",
        None,
    )];
    for size in [16, 64, 256] {
        for (family, source) in [
            ("core-dead-locals", core_dead_locals(size)),
            ("core-generic-calls", core_generic_calls(size)),
            ("core-concat-growth", core_concat_growth(size)),
            ("core-artifact-control", core_artifact_control(size)),
        ] {
            cases.push(fixture(
                scratch,
                &format!("{family}-{size}"),
                family,
                "compile",
                Some(size),
                &source,
                core_roots.clone(),
                json!({}),
                None,
                "timed",
                None,
            ));
        }
    }
    for size in [2, 8, 12] {
        for stage in ["build", "validate", "lower", "compile"] {
            let source = linear_graph(size, stage);
            let name = if stage == "compile" {
                format!("linear-{size}")
            } else {
                format!("nefor-linear-{stage}-{size}")
            };
            cases.push(fixture(
                scratch,
                &name,
                "nefor-linear",
                stage,
                Some(size),
                &source,
                nefor_roots.clone(),
                nefor_inputs.clone(),
                None,
                "timed",
                None,
            ));
        }
    }
    for size in [2, 8, 16] {
        for stage in ["build", "lower", "compile"] {
            let source = fan_in_graph(size, stage);
            let name = if stage == "compile" {
                format!("product-fan-in-{size}")
            } else {
                format!("nefor-fan-in-{stage}-{size}")
            };
            cases.push(fixture(
                scratch,
                &name,
                "nefor-fan-in",
                stage,
                Some(size),
                &source,
                nefor_roots.clone(),
                nefor_inputs.clone(),
                None,
                "timed",
                None,
            ));
        }
    }
    cases.push(Fixture {
        name: "shipped-lead-turn".into(),
        family: "production".into(),
        stage: "compile".into(),
        size: None,
        source_dir: root.join("examples/nefor-agent"),
        entry: "agentic-loop/lead-turn.mag".into(),
        module_roots: nefor_roots,
        inputs: nefor_inputs.clone(),
        fixture_fingerprint: String::new(),
        topology_fingerprint: None,
        fixture_files: vec![PathBuf::from("agentic-loop/lead-turn.mag")],
        expected_error: None,
        policy: "timed".into(),
        expected_artifact: None,
        compiler_options: nefor_mag::CompilerOptions::default(),
    });
    for (width, depth) in [(4, 4), (8, 8), (16, 8)] {
        for stage in [
            "analysis",
            "forward-reachability",
            "both-reachability",
            "lower",
        ] {
            let (source, topology_fingerprint) = broad_frontier_graph(width, depth, stage);
            let mut generated = fixture(
                scratch,
                &format!("broad-frontier-{stage}-{width}x{depth}"),
                "broad-frontier",
                stage,
                Some(width * (depth + 1) + 1),
                &source,
                nefor_module_roots(root),
                nefor_inputs.clone(),
                None,
                "timed",
                None,
            );
            generated.topology_fingerprint = Some(topology_fingerprint);
            generated.compiler_options.limits.call_depth = 1024;
            generated.compiler_options.limits.expression_depth = 2048;
            cases.push(generated);
        }
    }
    cases
}

fn oracle_cases(root: &Path, scratch: &Path, contracts: &Value) -> Vec<Fixture> {
    let core = vec![];
    let nefor = nefor_module_roots(root);
    let inputs = json!({"factory_contracts": contracts});
    let mut out = Vec::new();
    let errors = [
        ("dead-binding-unresolved-symbol", "type Empty {}\nlet run: fn() -> Artifact = | | => { let dead = missing\n artifact(Empty {}) }\nrun()", "unresolved", "static"),
        ("dead-function-body-return-type-error", "type Empty {}\nlet run: fn() -> Int = | | => \"wrong\"\nartifact(Empty {})", "type", "static"),
        ("dead-strict-inference-cycle", "type Empty {}\nlet a = b\nlet b = a\nartifact(Empty {})", "type", "static"),
        ("unused-required-module-resolution-error", "import missing.module.{}\ntype Empty {}\nartifact(Empty {})", "evaluation", "module"),
        ("demanded-local-partial-builtin", "let run: fn() -> Artifact = | | => { let bad = remove_at([1], 9)\n artifact(bad) }\nrun()", "evaluation", "demanded-runtime"),
        ("dead-local-recursion-budget", "type Ok {ok: Bool}\nlet loop: fn(Int) -> Int = |n| => loop(n)\nlet run: fn() -> Artifact = | | => { let dead = loop(0)\n artifact(Ok {ok: true}) }\nrun()", "budget", "dead_local_may_elide"),
        ("demanded-local-recursion-budget", "let loop: fn(Int) -> Int = |n| => loop(n)\nlet run: fn() -> Artifact = | | => { let dead = loop(0)\n artifact(dead) }\nrun()", "budget", "demanded-runtime"),
        ("top-level-dead-partial-builtin-remains-eager", "type Ok {ok: Bool}\nlet dead = remove_at([1], 9)\nartifact(Ok {ok: true})", "evaluation", "demanded-runtime"),
    ];
    for (name, source, class, policy) in errors {
        let expected = (policy == "dead_local_may_elide").then(|| json!({"ok": true}));
        out.push(fixture(
            scratch,
            name,
            "oracle",
            "oracle",
            None,
            source,
            core.clone(),
            json!({}),
            Some(class),
            policy,
            expected,
        ));
    }
    out.push(fixture(
        scratch, "dead-local-partial-builtin", "oracle", "oracle", None,
        "type Ok {ok: Bool}\nlet run: fn() -> Artifact = | | => { let bad = remove_at([1], 9)\n artifact(Ok {ok: true}) }\nrun()",
        core.clone(), json!({}), Some("evaluation"), "dead_local_may_elide", Some(json!({"ok": true})),
    ));
    out.push(fixture(
        scratch, "untaken-branch-does-not-demand-local", "oracle", "oracle", None,
        "type Ok {ok: Bool}\nlet run: fn() -> Artifact = | | => if false then artifact(remove_at([1], 9)) else artifact(Ok {ok: true})\nrun()",
        core.clone(), json!({}), None, "demanded-runtime", Some(json!({"ok": true})),
    ));

    let mut unused_module = fixture(
        scratch,
        "unused-required-module-static-error",
        "oracle",
        "oracle",
        None,
        "import broken.{}\ntype Empty {}\nartifact(Empty {})",
        core.clone(),
        json!({}),
        Some("unresolved"),
        "module",
        None,
    );
    write_module(&mut unused_module, "broken.mag", "let broken = missing");
    out.push(unused_module);

    let mut files = fixture(
        scratch, "read-and-read-json", "oracle", "oracle", None,
        "type ReadArtifact {text: String, items: List<String>}\nlet text = read(\"message.txt\")\nlet data = read_json(\"manifest.json\")\nartifact(ReadArtifact {text: text, items: (get(data, \"items\"): List<String>)})",
        core.clone(), json!({}), None, "file-input",
        Some(json!({"text":"hello\n","items":["second","first"]})),
    );
    write_fixture_file(&mut files, "message.txt", b"hello\n");
    write_fixture_file(
        &mut files,
        "manifest.json",
        br#"{"items":["second","first"]}"#,
    );
    out.push(files);

    out.push(fixture(
        scratch, "nested-host-input", "oracle", "oracle", None,
        "type Step {enabled: Bool, label: String}\ntype Config {steps: List<Step>}\nartifact(host_input(\"config\", type_tag<Config>()))",
        core.clone(), json!({"config":{"steps":[{"enabled":true,"label":"build"}]}}), None, "host-input",
        Some(json!({"steps":[{"enabled":true,"label":"build"}]})),
    ));
    out.push(fixture(
        scratch,
        "missing-host-input",
        "oracle",
        "oracle",
        None,
        "artifact(host_input(\"count\", type_tag<Int>()))",
        core.clone(),
        json!({}),
        Some("type"),
        "host_input",
        None,
    ));
    out.push(fixture(
        scratch, "wrong-nested-host-input", "oracle", "oracle", None,
        "type Step {enabled: Bool, label: String}\ntype Config {steps: List<Step>}\nartifact(host_input(\"config\", type_tag<Config>()))",
        core.clone(), json!({"config":{"steps":[{"enabled":"yes","label":"build"}]}}), Some("type"), "host-input", None,
    ));

    out.push(fixture(
        scratch, "function-local-closure-capture", "oracle", "oracle", None,
        "type ClosureArtifact {captured: Int, suffix: String}\nlet run: fn(Int) -> Artifact = |value| => { let captured = value\n let emit: fn(String) -> Artifact = |suffix| => artifact(ClosureArtifact {captured: captured, suffix: suffix})\n emit(\"ok\") }\nrun(6)",
        core.clone(), json!({}), None, "closure", Some(json!({"captured":6,"suffix":"ok"})),
    ));
    out.push(fixture(
        scratch, "closures-in-strict-values", "oracle", "oracle", None,
        "type Handlers {even: fn(List<Int>) -> Bool, odd: fn(List<Int>) -> Bool}\nlet handlers = Handlers {even: ((|items| => if (=)(count(items), 0) then true else get(handlers, \"odd\")(remove_at(items, 0))): fn(List<Int>) -> Bool), odd: ((|items| => if (=)(count(items), 0) then false else get(handlers, \"even\")(remove_at(items, 0))): fn(List<Int>) -> Bool)}\nartifact(get(handlers, \"even\")([1, 2]))",
        core.clone(), json!({}), None, "closure", Some(json!(true)),
    ));

    let mut nominal = fixture(
        scratch, "same-shaped-module-nominals", "oracle", "oracle", None,
        "import left.types.{}\nimport right.types.{}\nlet accept_left: fn(left.types.Payload) -> left.types.Payload = |value| => value\nartifact(accept_left(right.types.Payload {value: 1}))",
        core.clone(), json!({}), Some("type"), "nominal", None,
    );
    write_module(&mut nominal, "left/types.mag", "type Payload {value: Int}");
    write_module(&mut nominal, "right/types.mag", "type Payload {value: Int}");
    out.push(nominal);

    let mut ordering = fixture(
        scratch, "deterministic-derived-ordering", "oracle", "oracle", None,
        "import ordered.values.{}\ntype Ranked {rank: String, label: String}\ntype OrderingArtifact {collection: List<String>, module: List<String>, file: List<String>}\nlet manifest = read_json(\"order.json\")\nlet ranked = [Ranked {rank: \"2\", label: \"b\"}, Ranked {rank: \"1\", label: \"a\"}]\nlet sorted_ranked = sort_by(((|entry| => get(entry, \"rank\")): fn(Ranked) -> String), ranked)\nlet collection = map(((|entry| => get(entry, \"label\")): fn(Ranked) -> String), sorted_ranked)\nartifact(OrderingArtifact {collection: collection, module: ordered.values.items, file: (get(manifest, \"items\"): List<String>)})",
        core.clone(), json!({}), None, "ordering",
        Some(json!({"collection":["a","b"],"module":["module-z","module-a"],"file":["file-2","file-1"]})),
    );
    write_module(
        &mut ordering,
        "ordered/values.mag",
        "let items = [\"module-z\", \"module-a\"]",
    );
    write_fixture_file(
        &mut ordering,
        "order.json",
        br#"{"items":["file-2","file-1"]}"#,
    );
    out.push(ordering);

    let type_descriptor = json!({"kind":"named","name":"main.Choice","arguments":[],"body":{"kind":"record","fields":[{"name":"label","type":{"kind":"primitive","name":"String"}}]}});
    let type_schema = json!({"version":2,"root":{"kind":"named","name":"main.Choice","body":{"kind":"record","fields":[{"name":"label","schema":{"kind":"string"}}]}}});
    out.push(fixture(
        scratch, "evidence-artifact-identity", "oracle", "oracle", None,
        "type Choice {label: String}\ntype EvidenceArtifact {descriptor: TypeDescriptor, schema: TypeSchema, semantic_id: SemanticTypeId, selected: Choice}\nlet value = Choice {label: \"yes\"}\nartifact(EvidenceArtifact {descriptor: type_evidence(type_tag<Choice>()), schema: type_schema(type_tag<Choice>()), semantic_id: type_id(type_evidence(type_tag<Choice>())), selected: value})",
        core.clone(), json!({}), None, "static",
        Some(json!({"descriptor":type_descriptor,"schema":type_schema,"semantic_id":"sha256:604d7d96efdd1a0250532974cc8fd2729a659f6d2f67fc25e28d06b32d97dd10","selected":{"label":"yes"}})),
    ));

    out.push(fixture(
        scratch,
        "graph-conflicting-node-definition",
        "oracle",
        "oracle",
        None,
        &invalid_conflict_graph(),
        nefor.clone(),
        inputs.clone(),
        Some("evaluation"),
        "graph-validation",
        None,
    ));
    out.push(fixture(
        scratch, "graph-validation-priority", "oracle", "oracle", None,
        "import nefor.artifact.{}\nimport nefor.graph.{}\nlet topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => graph\nnefor.artifact.compile(topology)",
        nefor, inputs, Some("evaluation"), "graph-validation", None,
    ));
    out
}

fn core_dead_locals(size: usize) -> String {
    let mut source = String::from("type Ok {ok: Bool}\nlet work: fn(Int) -> Int = |x| => count(map(((|v| => v): fn(Int) -> Int), [1, 2, 3, 4]))\nlet run: fn() -> Artifact = | | => {\n");
    for index in 0..size {
        source.push_str(&format!("  let dead{index} = work({index})\n"));
    }
    source.push_str("  artifact(Ok {ok: true})\n}\nrun()");
    source
}
fn core_generic_calls(size: usize) -> String {
    let values = (0..size)
        .map(|i| format!("GenericValue {{value: {i}}}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("type GenericValue {{value: Int}}\ntype CountArtifact {{count: Int}}\nlet identity<T>: fn(T) -> T = |value| => value\nlet values = [{values}]\nlet copied = map(((|value| => identity(value)): fn(GenericValue) -> GenericValue), values)\nartifact(CountArtifact {{count: count(copied)}})")
}
fn core_concat_growth(size: usize) -> String {
    let values = (0..size)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("type CountArtifact {{count: Int}}\nlet values = [{values}]\nlet grown = fold(((|out, value| => concat(out, [value])): fn(List<Int>, Int) -> List<Int>), ([]: List<Int>), values)\nartifact(CountArtifact {{count: count(grown)}})")
}
fn core_artifact_control(size: usize) -> String {
    let values = (0..size)
        .map(|i| format!("Item {{index: {i}, label: \"item-{i}\"}}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("type Item {{index: Int, label: String}}\ntype ItemsArtifact {{items: List<Item>}}\nartifact(ItemsArtifact {{items: [{values}]}})")
}

fn graph_prelude() -> String {
    r#"import core.set.{}
import core.validated.{}
import nefor.artifact.{}
import nefor.contracts.{}
import nefor.graph.{}
type EdgeSummary {edges: Int}
type LowerSummary {actors: Int, messages: Int, forced: Bool}
type FrontierSummary {nodes: Int, edges: Int, roots: Int, outputs: Int}
type FrontierProof {summary: FrontierSummary, forced: Bool}
type LowerFrontier {summary: FrontierSummary, lowered: nefor.graph.Modification, forced: String}
let contracts = host_input("factory_contracts", type_tag<List<nefor.graph.FactoryContract>>())
let pass: fn(String) -> nefor.graph.Node<Int, Int> = |id| => nefor.graph.identity<Int>(id)
"#
    .into()
}
fn linear_graph(size: usize, stage: &str) -> String {
    let mut source = graph_prelude();
    source.push_str("let start = nefor.graph.source(\"start\", 1)\n");
    for index in 0..size {
        source.push_str(&format!("let n{index} = pass(\"n{index}\")\n"));
    }
    source.push_str("let out = nefor.graph.output<Int>(\"out\")\nlet topology = nefor.graph.add_edges(nefor.graph.empty_graph, [");
    source.push_str("nefor.graph.edge(start, n0), ");
    for index in 0..size - 1 {
        source.push_str(&format!("nefor.graph.edge(n{index}, n{}), ", index + 1));
    }
    source.push_str(&format!("nefor.graph.edge(n{}, out)])\n", size - 1));
    source.push_str(&stage_artifact(stage));
    source
}
fn fan_in_graph(size: usize, stage: &str) -> String {
    let mut source = graph_prelude();
    for index in 0..size {
        source.push_str(&format!(
            "let s{index} = nefor.graph.source(\"s{index}\", {index})\n"
        ));
    }
    let types = (0..size).map(|_| "Int").collect::<Vec<_>>().join(", ");
    source.push_str(&format!("let out = nefor.graph.output<({types})>(\"out\")\nlet topology = nefor.graph.add_edges(nefor.graph.empty_graph, ["));
    for index in 0..size {
        source.push_str(&format!("nefor.graph.edge(s{index}, out), "));
    }
    source.push_str("])\n");
    source.push_str(&stage_artifact(stage));
    source
}
fn broad_frontier_graph(width: usize, depth: usize, stage: &str) -> (String, String) {
    let mut source = graph_prelude();
    for chain in 0..width {
        source.push_str(&format!(
            "let s{chain} = nefor.graph.source(\"s{chain}\", {chain})\n"
        ));
        for level in 0..depth {
            source.push_str(&format!(
                "let n{chain}_{level} = pass(\"n{chain}_{level}\")\n"
            ));
        }
    }
    let types = (0..width).map(|_| "Int").collect::<Vec<_>>().join(", ");
    source.push_str(&format!("let out = nefor.graph.output<({types})>(\"out\")\nlet topology = nefor.graph.add_edges(nefor.graph.empty_graph, ["));
    for chain in 0..width {
        source.push_str(&format!("nefor.graph.edge(s{chain}, n{chain}_0), "));
        for level in 0..depth - 1 {
            source.push_str(&format!(
                "nefor.graph.edge(n{chain}_{level}, n{chain}_{}), ",
                level + 1
            ));
        }
        source.push_str(&format!("nefor.graph.edge(n{chain}_{}, out), ", depth - 1));
    }
    source.push_str("])\n");
    let topology_fingerprint = fingerprint(source.as_bytes());
    source.push_str("let analysis = nefor.graph.analyze_graph(topology)\n");
    source.push_str("let summary = FrontierSummary {nodes: count(get(analysis, \"nodes\")), edges: count(get(analysis, \"edges\")), roots: count(get(analysis, \"roots\")), outputs: count(get(analysis, \"outputs\"))}\n");
    match stage {
        "analysis" => {}
        "forward-reachability" => source.push_str(&format!("let forward = nefor.graph.forward_reachable(analysis)\nlet forward_count = core.set.count(forward)\nlet forward_proof = if (=)(forward_count, {}) then true else fail(\"broad-frontier forward reachability changed\")\n", width * (depth + 1) + 1)),
        "both-reachability" => source.push_str(&format!("let forward = nefor.graph.forward_reachable(analysis)\nlet forward_count = core.set.count(forward)\nlet forward_proof = if (=)(forward_count, {}) then true else fail(\"broad-frontier forward reachability changed\")\nlet force_reverse: fn(Bool) -> Set<String> = |proof| => nefor.graph.reverse_reachable(analysis, first(get(analysis, \"outputs\")))\nlet reverse = force_reverse(forward_proof)\nlet reverse_count = core.set.count(reverse)\nlet reverse_proof = if (=)(reverse_count, {}) then true else fail(\"broad-frontier reverse reachability changed\")\n", width * (depth + 1) + 1, width * (depth + 1) + 1)),
        "lower" => {
            let terminal_actor_ids = (0..width)
                .map(|chain| format!("n{chain}_{}", depth - 1))
                .collect::<Vec<_>>();
            let selected = terminal_actor_ids
                .iter()
                .map(|actor_id| format!("(=)(get(candidate, \"id\"), \"{actor_id}\")"))
                .reduce(|left, right| format!("or({left}, {right})"))
                .expect("positive width");
            let expected_actor_ids = terminal_actor_ids
                .iter()
                .map(|actor_id| format!("\"{actor_id}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let expected_positions = lexical_product_positions(&terminal_actor_ids)
                .into_iter()
                .map(|position| position.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            source.push_str("let lowered = nefor.graph.lower(topology)\nlet forced = canonical(lowered)\n");
            source.push_str(&format!("let final_actors = filter(((|candidate| => {selected}): fn(nefor.graph.LowerActor) -> Bool), get(lowered, \"actors\"))\n"));
            source.push_str("let final_actor_ids = map(((|candidate| => get(candidate, \"id\")): fn(nefor.graph.LowerActor) -> String), final_actors)\n");
            source.push_str("let positions = map(((|candidate| => get(first(__map_get_or(get(candidate, \"routes\"), \"nefor.graph.Value\", ([]: List<nefor.graph.LowerDestination>))), \"product_position\")): fn(nefor.graph.LowerActor) -> Int), final_actors)\n");
            source.push_str(&format!("let actor_order_proof = if (=)(final_actor_ids, [{expected_actor_ids}]) then true else fail(\"broad-frontier terminal actor order changed\")\n"));
            source.push_str(&format!("let route_order_proof = if (=)(positions, [{expected_positions}]) then actor_order_proof else fail(\"broad-frontier product positions changed\")\n"));
        }
        _ => unreachable!(),
    }
    match stage {
        "analysis" => source.push_str("artifact(summary)"),
        "forward-reachability" => {
            source.push_str("artifact(FrontierProof {summary: summary, forced: forward_proof})")
        }
        "both-reachability" => {
            source.push_str("artifact(FrontierProof {summary: summary, forced: reverse_proof})")
        }
        "lower" => source.push_str(
            "artifact(LowerFrontier {summary: summary, lowered: lowered, forced: forced})",
        ),
        _ => unreachable!(),
    }
    (source, topology_fingerprint)
}

fn stage_artifact(stage: &str) -> String {
    match stage {
        "build" => "artifact(EdgeSummary {edges: count(get(topology, \"edges\"))})".into(),
        "validate" => "let checked = nefor.graph.validate(topology, contracts)\nmatch checked { case Valid(accepted) => artifact(EdgeSummary {edges: count(get(accepted, \"edges\"))}), case Invalid(rejected) => fail(get(rejected, \"errors\")), }".into(),
        "lower" => "let lowered = nefor.graph.lower(topology)\nlet forced = canonical(lowered)\nartifact(LowerSummary {actors: count(get(lowered, \"actors\")), messages: count(get(lowered, \"messages\")), forced: not((=)(forced, \"\"))})".into(),
        "compile" => "let topology_fn: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, get(topology, \"edges\"))\nnefor.artifact.compile(topology_fn)".into(),
        _ => unreachable!(),
    }
}
fn invalid_conflict_graph() -> String {
    let mut source = graph_prelude();
    source.push_str(r#"let start = nefor.graph.source("start", 1)
let left = pass("same")
let right_input = nefor.graph.junction_port("same", type_tag<Int>(), "in")
let right_output = nefor.graph.junction_port("same", type_tag<Int>(), "different")
let right_junction = nefor.graph.junction("same", nefor.graph.pass_operation, [nefor.graph.store_port(right_input)], [nefor.graph.store_port(right_output)])
let right = nefor.graph.node_with_junctions("same", "ordinary", [], [right_junction], [], [], right_input, right_output)
let out = nefor.graph.output<Int>("out")
let topology: fn(nefor.graph.Graph) -> nefor.graph.Graph = |graph| => nefor.graph.add_edges(graph, [nefor.graph.edge(start, left), nefor.graph.edge(start, right), nefor.graph.edge(left, out)])
nefor.artifact.compile(topology)"#);
    source
}

fn metadata(root: &Path, samples: usize, warmups: usize, case_definition_hash: String) -> Metadata {
    Metadata {
        git_commit: command_output(root, &["git", "rev-parse", "HEAD"]),
        git_dirty: !command_output(root, &["git", "status", "--porcelain"]).is_empty(),
        git_tree: command_output(root, &["git", "rev-parse", "HEAD^{tree}"]),
        git_diff_digest: dirty_diff_digest(root),
        package_version: env!("CARGO_PKG_VERSION").into(),
        rustc: command_output(root, &["rustc", "-Vv"]),
        target: rustc_host(root),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        profile: "bench (optimized)".into(),
        samples_per_case: samples,
        warmup_iterations: warmups,
        quantile_policy: "nearest-rank empirical quantile: rank = ceil(p*n), one-based; n=3 p90 is max, n=30 p90 is rank 27".into(),
        case_definition_hash,
        size_replacements: std::collections::BTreeMap::from([(
            "nefor-linear requested 16 (expression nesting limit)".into(),
            12,
        )]),
    }
}
fn dirty_diff_digest(root: &Path) -> Option<String> {
    let status = command_output(root, &["git", "status", "--porcelain"]);
    if status.is_empty() {
        return None;
    }
    let output = Command::new("git")
        .args(["diff", "--binary", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git diff for candidate identity");
    Some(fingerprint(&output.stdout))
}

fn definition_hash(cases: &[Fixture], oracles: &[Fixture]) -> String {
    fingerprint(
        cases
            .iter()
            .chain(oracles)
            .flat_map(|case| {
                format!(
                    "{}:{}:{}:{}:{}\n",
                    case.name,
                    case.family,
                    case.stage,
                    case.size.map_or_else(|| "none".into(), |v| v.to_string()),
                    case.fixture_fingerprint
                )
                .into_bytes()
            })
            .collect::<Vec<_>>()
            .as_slice(),
    )
}
fn positive_env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<std::num::NonZeroUsize>()
                .unwrap_or_else(|_| panic!("{name} must be positive"))
                .get()
        })
        .unwrap_or(default)
}
fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|arg| arg == flag).map(|index| {
        args.get(index + 1)
            .unwrap_or_else(|| panic!("{flag} requires a path"))
            .clone()
    })
}
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}
fn fresh_scratch(root: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = root
        .join("tmp/mag-optimization-cycle-1")
        .join(format!("scratch-{nonce}"));
    fs::create_dir_all(&path).expect("create scratch");
    path
}
fn rustc_host(root: &Path) -> String {
    command_output(root, &["rustc", "-Vv"])
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap_or("unknown")
        .to_owned()
}
fn command_output(root: &Path, args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn load_runtime_contracts(path: &Path) -> Value {
    let source = fs::read_to_string(path).expect("read shipped MAG registry");
    let lua = Lua::new();
    install_runtime_registry_host(&lua);
    let directory = path.parent().expect("registry parent");
    let package: Table = lua.globals().get("package").expect("Lua package table");
    let current: String = package.get("path").expect("Lua package path");
    let prefix = [
        directory.join("?.lua"),
        directory.join("?/init.lua"),
        directory.join("../../../../lua/?.lua"),
        directory.join("../../../../lua/?/init.lua"),
    ]
    .iter()
    .map(|pattern| pattern.display().to_string())
    .collect::<Vec<_>>()
    .join(";");
    package
        .set("path", format!("{prefix};{current}"))
        .expect("set Lua package path");
    let registry: Table = lua
        .load(&source)
        .set_name(path.display().to_string())
        .eval()
        .expect("load registry");
    let contracts: mlua::Function = registry
        .get("registry_contracts")
        .expect("registry_contracts");
    let value: LuaValue = contracts
        .call(lua.array_metatable())
        .expect("read registry contracts");
    lua.from_value(value).expect("serialize registry contracts")
}
fn install_runtime_registry_host(lua: &Lua) {
    let nefor = lua.create_table().expect("host");
    nefor
        .set(
            "log",
            lua.create_function(|_, _: String| Ok(())).expect("log"),
        )
        .expect("install log");
    let semantic = lua.create_table().expect("semantic");
    semantic
        .set(
            "id",
            lua.create_function(|lua, descriptor: LuaValue| {
                let descriptor: Value = lua.from_value(descriptor)?;
                let descriptor = nefor_mag::json::concrete_type_from_json(&descriptor)
                    .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                Ok(descriptor.stable_id().to_string())
            })
            .expect("id"),
        )
        .expect("install id");
    nefor.set("semantic_type", semantic).expect("semantic host");
    lua.globals().set("nefor", nefor).expect("registry host");
}
