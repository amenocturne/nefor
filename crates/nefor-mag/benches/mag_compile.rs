use nefor_mag::profile::CompileProfile;
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SAMPLES: usize = 30;
const WARMUP: usize = 3;

#[derive(Serialize)]
struct Report {
    schema_version: u8,
    metadata: Metadata,
    cases: Vec<CaseReport>,
}

#[derive(Serialize)]
struct Metadata {
    git_commit: String,
    git_dirty: bool,
    package_version: &'static str,
    rustc: String,
    target: String,
    os: &'static str,
    arch: &'static str,
    logical_cpus: usize,
    profile: &'static str,
    samples_per_case: usize,
    warmup_iterations: usize,
}

#[derive(Serialize)]
struct CaseReport {
    name: String,
    outcome: &'static str,
    expected_error: Option<&'static str>,
    input_bytes: usize,
    artifact_hash: Option<String>,
    wall_ns: Vec<u64>,
    distribution_ns: Distribution,
    counters: Option<nefor_mag::profile::OperationCounters>,
    phase_median_ns: Option<nefor_mag::profile::PhaseDurations>,
}

#[derive(Default, Serialize)]
struct Distribution {
    min: u64,
    median: u64,
    mean: u64,
    p90: u64,
    p95: u64,
    max: u64,
}

struct Case {
    name: String,
    source_dir: PathBuf,
    entry: String,
    module_roots: Vec<PathBuf>,
    inputs: serde_json::Value,
    input_bytes: usize,
    expected_error: Option<ExpectedError>,
}

#[derive(Clone, Copy)]
enum ExpectedError {
    CallDepthBudget,
}

impl ExpectedError {
    fn category(self) -> &'static str {
        match self {
            Self::CallDepthBudget => "call_depth_budget",
        }
    }

    fn matches(self, error: &nefor_mag::error::MagError) -> bool {
        matches!(
            (self, error),
            (Self::CallDepthBudget, nefor_mag::error::MagError::Budget(message))
                if message == "function call depth limit reached"
        )
    }
}

fn main() {
    let root = workspace_root();
    let samples = match std::env::var("MAG_BENCH_SAMPLES") {
        Ok(value) => value
            .parse::<std::num::NonZeroUsize>()
            .unwrap_or_else(|_| panic!("MAG_BENCH_SAMPLES must be a positive integer: {value:?}"))
            .get(),
        Err(std::env::VarError::NotPresent) => DEFAULT_SAMPLES,
        Err(error) => panic!("cannot read MAG_BENCH_SAMPLES: {error}"),
    };
    let scratch = fresh_scratch();
    let cases = cases(&root, &scratch);
    let reports = cases
        .iter()
        .map(|case| run_case(case, samples))
        .collect::<Vec<_>>();
    fs::remove_dir_all(&scratch).ok();
    println!(
        "{}",
        serde_json::to_string_pretty(&Report {
            schema_version: 1,
            metadata: metadata(&root, samples),
            cases: reports,
        })
        .expect("serialize benchmark report")
    );
}

fn run_case(case: &Case, samples: usize) -> CaseReport {
    for _ in 0..WARMUP {
        assert_outcome(case, compile_unprofiled(case));
    }
    let mut wall_ns = Vec::with_capacity(samples);
    let mut profiles = Vec::with_capacity(samples);
    let mut expected_hash = None;
    for _ in 0..samples {
        let started = Instant::now();
        let result = compile_unprofiled(case);
        wall_ns.push(nanos(started.elapsed()));
        if let Some(program) = assert_outcome(case, result) {
            check_artifact(case, &mut expected_hash, &program);
            black_box(program.artifact);

            let (profiled, profile) = compile_profiled(case)
                .unwrap_or_else(|error| panic!("{} profiled compile failed: {error}", case.name));
            check_artifact(case, &mut expected_hash, &profiled);
            profiles.push(profile);
            black_box(profiled.artifact);
        }
    }
    let counters = profiles.first().map(|profile| profile.counters.clone());
    if let Some(expected) = &counters {
        assert!(profiles.iter().all(|profile| &profile.counters == expected));
    }
    CaseReport {
        name: case.name.clone(),
        outcome: if case.expected_error.is_none() {
            "success"
        } else {
            "expected_failure"
        },
        expected_error: case.expected_error.map(ExpectedError::category),
        input_bytes: case.input_bytes,
        artifact_hash: expected_hash,
        distribution_ns: distribution(&wall_ns),
        phase_median_ns: phase_medians(&profiles),
        counters,
        wall_ns,
    }
}

fn assert_outcome(
    case: &Case,
    result: Result<nefor_mag::LoadedProgram, nefor_mag::error::MagError>,
) -> Option<nefor_mag::LoadedProgram> {
    match (case.expected_error, result) {
        (None, Ok(program)) => Some(program),
        (Some(expected), Err(error)) if expected.matches(&error) => None,
        (None, Err(error)) => panic!("{} failed: {error}", case.name),
        (Some(expected), Err(error)) => panic!(
            "{} failed with {error}, expected {}",
            case.name,
            expected.category()
        ),
        (Some(expected), Ok(_)) => panic!(
            "{} unexpectedly succeeded; expected {}",
            case.name,
            expected.category()
        ),
    }
}

fn check_artifact(
    case: &Case,
    expected_hash: &mut Option<String>,
    program: &nefor_mag::LoadedProgram,
) {
    if let Some(expected) = expected_hash {
        assert_eq!(expected, &program.hash, "{} artifact changed", case.name);
    } else {
        *expected_hash = Some(program.hash.clone());
    }
}

fn compile_unprofiled(case: &Case) -> Result<nefor_mag::LoadedProgram, nefor_mag::error::MagError> {
    let program = nefor_mag::load_with_inputs_and_module_roots(
        &case.source_dir,
        &case.entry,
        case.inputs.clone(),
        &case.module_roots,
    )?;
    nefor_mag::validate_loaded_rules(&program)?;
    Ok(program)
}

fn compile_profiled(
    case: &Case,
) -> Result<(nefor_mag::LoadedProgram, CompileProfile), nefor_mag::error::MagError> {
    let profiler = nefor_mag::profile::CompileProfiler::new();
    let program = nefor_mag::load_with_profiler(
        &case.source_dir,
        &case.entry,
        case.inputs.clone(),
        &case.module_roots,
        &profiler,
    )?;
    nefor_mag::validate_loaded_rules_profiled(&program, &profiler)?;
    Ok((program, profiler.snapshot()))
}

fn cases(root: &Path, scratch: &Path) -> Vec<Case> {
    let mut cases = Vec::new();
    cases.push(write_case(
        scratch,
        "trivial",
        "(artifact \"bench.trivial/v1\" {})",
        vec![scratch.into()],
        json!({}),
        None,
    ));

    let lead = root.join("examples/nefor-agent/agentic-loop/lead-turn.mag");
    let contracts = nefor_mag::registry::load_registry_contracts(
        &root.join("plugins/mag/lua/mag-kernel/init.lua"),
    )
    .expect("load shipped MAG registry contracts");
    cases.push(Case {
        name: "shipped-lead-turn".into(),
        source_dir: root.join("examples/nefor-agent"),
        entry: "agentic-loop/lead-turn.mag".into(),
        module_roots: vec![root.join("examples/nefor-agent/mag/lib")],
        inputs: json!({"foreign_contracts": contracts.clone()}),
        input_bytes: fs::metadata(lead).map(|m| m.len() as usize).unwrap_or(0),
        expected_error: None,
    });

    for size in [2, 4, 8] {
        let source = linear_graph(size);
        cases.push(write_case(
            scratch,
            &format!("linear-{size}"),
            &source,
            vec![scratch.into(), root.join("examples/nefor-agent/mag/lib")],
            json!({"foreign_contracts": contracts.clone()}),
            None,
        ));
    }
    for size in [2, 8, 16] {
        let source = product_fan_in(size);
        cases.push(write_case(
            scratch,
            &format!("product-fan-in-{size}"),
            &source,
            vec![scratch.into(), root.join("examples/nefor-agent/mag/lib")],
            json!({"foreign_contracts": contracts.clone()}),
            None,
        ));
    }
    cases.push(write_case(
        scratch,
        "capacity-call-depth",
        &recursive_limit(),
        vec![scratch.into()],
        json!({}),
        Some(ExpectedError::CallDepthBudget),
    ));
    cases
}

fn write_case(
    scratch: &Path,
    name: &str,
    source: &str,
    module_roots: Vec<PathBuf>,
    inputs: serde_json::Value,
    expected_error: Option<ExpectedError>,
) -> Case {
    let entry = format!("{name}.mag");
    fs::write(scratch.join(&entry), source).expect("write benchmark case");
    Case {
        name: name.into(),
        source_dir: scratch.into(),
        entry,
        module_roots,
        inputs,
        input_bytes: source.len(),
        expected_error,
    }
}

fn linear_graph(size: usize) -> String {
    let mut source = String::from(
        "(require \"nefor.artifact\")\n(require \"nefor.contracts\")\n(require \"nefor.graph\")\n(def pass (fn [[id String]] -> (nefor.graph.Node Int Int) (let [input (nefor.graph.port id (type-tag Int) \"nefor.graph.Value\") output (nefor.graph.port id (type-tag Int) \"nefor.graph.Value\") actor (nefor.graph.actor id (specialize nefor.factory.output [Int]) (as nefor.contracts.OutputParams {}) (nefor.graph.store-port input) [(nefor.graph.store-port output)])] (nefor.graph.node id \"ordinary\" [actor] (as (List nefor.graph.StoredRoute) []) (as (List nefor.graph.Message) []) input output))))\n(let [start (nefor.graph.source \"start\" (type-tag Int) 1)\n",
    );
    for index in 0..size {
        source.push_str(&format!("n{index} (pass \"n{index}\")\n"));
    }
    source.push_str("out (nefor.graph.output \"out\" (type-tag Int))\ntopology (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph (nefor.graph.add-edges graph [");
    if size == 0 {
        source.push_str("(nefor.graph.edge start out)");
    } else {
        source.push_str("(nefor.graph.edge start n0) ");
        for index in 0..size - 1 {
            source.push_str(&format!("(nefor.graph.edge n{index} n{}) ", index + 1));
        }
        source.push_str(&format!("(nefor.graph.edge n{} out)", size - 1));
    }
    source.push_str("]))]\n(nefor.artifact.compile topology))");
    source
}

fn product_fan_in(size: usize) -> String {
    let types = (0..size).map(|_| "Int").collect::<Vec<_>>().join(" ");
    let mut source =
        String::from("(require \"nefor.artifact\")\n(require \"nefor.graph\")\n(let [");
    for index in 0..size {
        source.push_str(&format!(
            "s{index} (nefor.graph.source \"s{index}\" (type-tag Int) {index})\n"
        ));
    }
    source.push_str(&format!("out (nefor.graph.output \"out\" (type-tag (+ {types})))\ntopology (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph (nefor.graph.add-edges graph ["));
    for index in 0..size {
        source.push_str(&format!("(nefor.graph.edge s{index} out) "));
    }
    source.push_str("]))]\n(nefor.artifact.compile topology))");
    source
}

fn recursive_limit() -> String {
    "(def loop (fn [[n Int]] -> Artifact (loop n)))\n(loop 1)".into()
}

fn distribution(samples: &[u64]) -> Distribution {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Distribution {
        min: sorted.first().copied().unwrap_or(0),
        median: percentile(&sorted, 50),
        mean: (sorted.iter().map(|&v| u128::from(v)).sum::<u128>() / sorted.len().max(1) as u128)
            as u64,
        p90: percentile(&sorted, 90),
        p95: percentile(&sorted, 95),
        max: sorted.last().copied().unwrap_or(0),
    }
}

fn phase_medians(profiles: &[CompileProfile]) -> Option<nefor_mag::profile::PhaseDurations> {
    let serialized = profiles
        .iter()
        .map(|p| serde_json::to_value(&p.phases).expect("phase json"))
        .collect::<Vec<_>>();
    let keys = serialized
        .first()?
        .as_object()?
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let mut result = BTreeMap::new();
    for key in keys {
        let mut values = serialized
            .iter()
            .map(|p| p[&key].as_u64().unwrap_or(0))
            .collect::<Vec<_>>();
        values.sort_unstable();
        result.insert(key, percentile(&values, 50));
    }
    serde_json::from_value(serde_json::to_value(result).ok()?).ok()
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    sorted
        .get((sorted.len().saturating_sub(1) * percentile) / 100)
        .copied()
        .unwrap_or(0)
}

fn nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}
fn fresh_scratch() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("nefor-mag-bench-{nonce}"));
    fs::create_dir_all(&path).expect("benchmark scratch");
    path
}
fn output(root: &Path, args: &[&str]) -> String {
    Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}
fn metadata(root: &Path, samples: usize) -> Metadata {
    Metadata {
        git_commit: output(root, &["git", "rev-parse", "HEAD"]),
        git_dirty: !output(root, &["git", "status", "--porcelain"]).is_empty(),
        package_version: env!("CARGO_PKG_VERSION"),
        rustc: output(root, &["rustc", "-Vv"]),
        target: option_env!("TARGET").unwrap_or("unknown").into(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        profile: "bench (optimized)",
        samples_per_case: samples,
        warmup_iterations: WARMUP,
    }
}
