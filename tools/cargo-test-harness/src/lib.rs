#![allow(clippy::missing_errors_doc)]

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const TIMEOUT_EXIT_CODE: i32 = 124;
pub const TEST_FAILURE_EXIT_CODE: i32 = 101;

#[derive(Debug)]
pub struct PreparedArtifacts {
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestLane {
    Default,
    Full,
}

impl TestLane {
    #[must_use]
    pub fn is_full(self) -> bool {
        self == Self::Full
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Full => "full",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeparatePackage {
    pub package: String,
    pub test_args: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct FullExecutionPlan {
    pub harness_packages: Vec<String>,
    pub separate: Vec<SeparatePackage>,
}

impl FullExecutionPlan {
    #[must_use]
    pub fn full_packages(&self) -> Vec<&str> {
        let mut packages = self
            .harness_packages
            .iter()
            .map(String::as_str)
            .chain(self.separate.iter().map(|entry| entry.package.as_str()))
            .collect::<Vec<_>>();
        packages.sort_unstable();
        packages.dedup();
        packages
    }

    #[must_use]
    pub fn feature_spec(&self) -> String {
        self.full_packages()
            .into_iter()
            .map(|package| format!("{package}/full-tests"))
            .collect::<Vec<_>>()
            .join(",")
    }

    #[must_use]
    pub fn producer_args(&self, lane: TestLane) -> Vec<String> {
        let mut args = vec![
            "build".to_owned(),
            "--workspace".to_owned(),
            "--locked".to_owned(),
            "--lib".to_owned(),
            "--bins".to_owned(),
            "--tests".to_owned(),
        ];
        if lane.is_full() {
            args.extend(["--features".to_owned(), self.feature_spec()]);
        }
        args
    }

    #[must_use]
    pub fn doctest_args(&self, lane: TestLane, test_args: &[OsString]) -> Vec<OsString> {
        let mut args = vec![
            "test".into(),
            "--workspace".into(),
            "--locked".into(),
            "--doc".into(),
        ];
        if lane.is_full() {
            args.extend(["--features".into(), self.feature_spec().into()]);
        }
        if !test_args.is_empty() {
            args.push("--".into());
            args.extend(test_args.iter().cloned());
        }
        args
    }

    #[must_use]
    pub fn package_test_args(&self, lane: TestLane, package: &str) -> Vec<OsString> {
        if !lane.is_full() {
            return Vec::new();
        }
        self.separate
            .iter()
            .find(|entry| entry.package == package)
            .map(|entry| entry.test_args.iter().map(OsString::from).collect())
            .unwrap_or_default()
    }
}

pub fn load_full_execution_plan(repository_root: &Path) -> io::Result<FullExecutionPlan> {
    let source = fs::read_to_string(repository_root.join("tools/test-lanes.json"))?;
    parse_full_execution_plan(&source)
}

fn parse_full_execution_plan(source: &str) -> io::Result<FullExecutionPlan> {
    let value: serde_json::Value = serde_json::from_str(source).map_err(io::Error::other)?;
    let cargo_full = value.get("cargo_full").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "test-lanes.json lacks cargo_full",
        )
    })?;
    let harness_packages = string_array(cargo_full, "harness_packages")?;
    let separate = cargo_full
        .get("separate")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cargo_full.separate must be an array",
            )
        })?
        .iter()
        .map(|entry| {
            let package = required_string(entry, "package")?;
            let test_args = string_array(entry, "test_args")?;
            Ok(SeparatePackage { package, test_args })
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(FullExecutionPlan {
        harness_packages,
        separate,
    })
}

fn required_string(value: &serde_json::Value, key: &str) -> io::Result<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_data(format!("{key} must be a string")))
}

fn string_array(value: &serde_json::Value, key: &str) -> io::Result<Vec<String>> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid_data(format!("{key} must be an array")))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_data(format!("every {key} entry must be a string")))
        })
        .collect()
}

#[derive(Clone, Debug)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
    test: bool,
    doctest: bool,
    required_features: Vec<String>,
}

#[derive(Clone, Debug)]
struct CargoPackage {
    name: String,
    cwd: PathBuf,
    targets: Vec<CargoTarget>,
}

#[derive(Debug)]
pub struct WorkspaceInventory {
    target_dir: PathBuf,
    packages: BTreeMap<String, CargoPackage>,
}

impl WorkspaceInventory {
    pub fn parse(source: &[u8]) -> io::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(source).map_err(io::Error::other)?;
        let target_dir = required_value_string(&value, "target_directory")?.into();
        let members = value
            .get("workspace_members")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| invalid_data("Cargo metadata lacks workspace_members"))?
            .iter()
            .map(|member| {
                member
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid_data("workspace member ID is not a string"))
            })
            .collect::<io::Result<BTreeSet<_>>>()?;
        let mut packages = BTreeMap::new();
        for package in value
            .get("packages")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| invalid_data("Cargo metadata lacks packages"))?
        {
            let id = required_value_string(package, "id")?;
            if !members.contains(&id) {
                continue;
            }
            let manifest_path = PathBuf::from(required_value_string(package, "manifest_path")?);
            let cwd = manifest_path
                .parent()
                .ok_or_else(|| invalid_data("package manifest has no parent"))?
                .to_path_buf();
            let targets = package
                .get("targets")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| invalid_data("package lacks targets"))?
                .iter()
                .map(parse_metadata_target)
                .collect::<io::Result<Vec<_>>>()?;
            packages.insert(
                id,
                CargoPackage {
                    name: required_value_string(package, "name")?,
                    cwd,
                    targets,
                },
            );
        }
        if packages.len() != members.len() {
            return Err(invalid_data(
                "Cargo metadata omitted a workspace package needed for artifact classification",
            ));
        }
        Ok(Self {
            target_dir,
            packages,
        })
    }

    #[must_use]
    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }
}

fn parse_metadata_target(value: &serde_json::Value) -> io::Result<CargoTarget> {
    Ok(CargoTarget {
        name: required_value_string(value, "name")?,
        kind: required_string_list(value, "kind")?,
        src_path: required_value_string(value, "src_path")?.into(),
        test: required_bool(value, "test")?,
        doctest: required_bool(value, "doctest")?,
        required_features: optional_string_list(value, "required-features")?,
    })
}

fn required_value_string(value: &serde_json::Value, key: &str) -> io::Result<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid_data(format!("{key} is missing or not a string")))
}

fn required_bool(value: &serde_json::Value, key: &str) -> io::Result<bool> {
    value
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| invalid_data(format!("{key} is missing or not a boolean")))
}

fn required_string_list(value: &serde_json::Value, key: &str) -> io::Result<Vec<String>> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid_data(format!("{key} is missing or not an array")))?
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_data(format!("{key} contains a non-string")))
        })
        .collect()
}

fn optional_string_list(value: &serde_json::Value, key: &str) -> io::Result<Vec<String>> {
    match value.get(key) {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(_) => required_string_list(value, key),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Debug)]
struct CompilerArtifact {
    package_id: String,
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
    test_profile: bool,
    executable: PathBuf,
}

fn parse_compiler_artifacts(input: impl BufRead) -> io::Result<Vec<CompilerArtifact>> {
    let mut artifacts = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
            invalid_data(format!(
                "malformed Cargo JSON on line {}: {error}",
                index + 1
            ))
        })?;
        if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(executable) = value.get("executable") else {
            return Err(invalid_data(format!(
                "Cargo compiler-artifact on line {} has no executable field",
                index + 1
            )));
        };
        if executable.is_null() {
            continue;
        }
        let target = value
            .get("target")
            .ok_or_else(|| invalid_data("compiler-artifact lacks target"))?;
        let profile = value
            .get("profile")
            .ok_or_else(|| invalid_data("compiler-artifact lacks profile"))?;
        artifacts.push(CompilerArtifact {
            package_id: required_value_string(&value, "package_id")?,
            name: required_value_string(target, "name")?,
            kind: required_string_list(target, "kind")?,
            src_path: required_value_string(target, "src_path")?.into(),
            test_profile: required_bool(profile, "test")?,
            executable: PathBuf::from(
                executable
                    .as_str()
                    .ok_or_else(|| invalid_data("Cargo executable is not a string"))?,
            ),
        });
    }
    Ok(artifacts)
}

pub fn parse_executables(input: impl BufRead) -> io::Result<Vec<PathBuf>> {
    let paths = parse_compiler_artifacts(input)?
        .into_iter()
        .map(|artifact| artifact.executable)
        .collect::<BTreeSet<_>>();
    Ok(paths.into_iter().collect())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreparedArtifactRole {
    RuntimeHelper,
    TestExecutable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedArtifact {
    pub package: String,
    pub target: String,
    pub target_kind: Vec<String>,
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub role: PreparedArtifactRole,
    pub test_args: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
    pub timeout_millis: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedTestManifest {
    pub schema_version: u32,
    pub lane: TestLane,
    pub repository_root: PathBuf,
    pub target_dir: PathBuf,
    pub artifacts: Vec<PreparedArtifact>,
    pub doctest_targets: Vec<String>,
}

impl PreparedTestManifest {
    #[must_use]
    pub fn signed_paths(&self) -> Vec<PathBuf> {
        self.artifacts
            .iter()
            .map(|artifact| artifact.executable.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn tests(&self) -> impl Iterator<Item = &PreparedArtifact> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.role == PreparedArtifactRole::TestExecutable)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(invalid_data(format!(
                "unsupported prepared-test manifest schema {}",
                self.schema_version
            )));
        }
        if !self.repository_root.is_absolute() || !self.target_dir.is_absolute() {
            return Err(invalid_data(
                "prepared-test manifest roots must be absolute",
            ));
        }
        let mut identities = BTreeSet::new();
        for artifact in &self.artifacts {
            if !artifact.executable.is_absolute() || !artifact.cwd.is_absolute() {
                return Err(invalid_data("prepared artifact paths must be absolute"));
            }
            if !artifact.executable.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "prepared {} is missing: {}",
                        artifact.target,
                        artifact.executable.display()
                    ),
                ));
            }
            let identity = (
                artifact.package.as_str(),
                artifact.target.as_str(),
                artifact.target_kind.as_slice(),
                artifact.role,
            );
            if !identities.insert(identity) {
                return Err(invalid_data(format!(
                    "duplicate prepared artifact identity {}::{} {:?} ({:?})",
                    artifact.package, artifact.target, artifact.target_kind, artifact.role
                )));
            }
        }
        if self.tests().next().is_none() {
            return Err(invalid_data("prepared-test manifest contains no tests"));
        }
        if !self.artifacts.iter().any(|artifact| {
            artifact.role == PreparedArtifactRole::RuntimeHelper
                && artifact.target == "nefor-test-watchdog"
        }) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "prepared-test manifest is missing runtime helper nefor-test-watchdog",
            ));
        }
        Ok(())
    }

    pub fn watchdog_path(&self) -> io::Result<&Path> {
        self.artifacts
            .iter()
            .find(|artifact| {
                artifact.role == PreparedArtifactRole::RuntimeHelper
                    && artifact.target == "nefor-test-watchdog"
            })
            .map(|artifact| artifact.executable.as_path())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "prepared-test manifest is missing runtime helper nefor-test-watchdog",
                )
            })
    }
}

fn target_enabled(target: &CargoTarget, lane: TestLane) -> bool {
    target.required_features.is_empty()
        || (lane.is_full() && target.required_features == ["full-tests"])
}

pub fn build_prepared_manifest(
    repository_root: &Path,
    inventory: &WorkspaceInventory,
    plan: &FullExecutionPlan,
    lane: TestLane,
    producer_json: impl BufRead,
) -> io::Result<PreparedTestManifest> {
    let root = repository_root.canonicalize()?;
    let target_dir = inventory.target_dir.canonicalize()?;
    let compiler_artifacts = parse_compiler_artifacts(producer_json)?;
    let (mut artifacts, seen_targets) =
        classify_produced_artifacts(inventory, plan, lane, compiler_artifacts)?;
    let (expected_tests, expected_helpers, mut doctest_targets) =
        expected_inventory(inventory, lane);
    require_complete_inventory(&seen_targets, &expected_tests, &expected_helpers)?;

    artifacts.sort_by(|left, right| {
        left.role
            .cmp(&right.role)
            .then_with(|| left.package.cmp(&right.package))
            .then_with(|| left.target.cmp(&right.target))
            .then_with(|| left.executable.cmp(&right.executable))
    });
    doctest_targets.sort();
    doctest_targets.dedup();
    let manifest = PreparedTestManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        lane,
        repository_root: root,
        target_dir,
        artifacts,
        doctest_targets,
    };
    manifest.validate()?;
    Ok(manifest)
}

type ArtifactIdentity = (String, String, Vec<String>, PreparedArtifactRole);

fn classify_produced_artifacts(
    inventory: &WorkspaceInventory,
    plan: &FullExecutionPlan,
    lane: TestLane,
    compiler_artifacts: Vec<CompilerArtifact>,
) -> io::Result<(Vec<PreparedArtifact>, BTreeSet<ArtifactIdentity>)> {
    let mut artifacts = Vec::new();
    let mut seen_targets = BTreeSet::new();
    for artifact in compiler_artifacts {
        let Some(package) = inventory.packages.get(&artifact.package_id) else {
            continue;
        };
        let target = package
            .targets
            .iter()
            .find(|target| {
                target.name == artifact.name
                    && target.kind == artifact.kind
                    && target.src_path == artifact.src_path
            })
            .ok_or_else(|| {
                invalid_data(format!(
                    "Cargo produced unknown workspace target {}::{}",
                    package.name, artifact.name
                ))
            })?;
        if !target_enabled(target, lane) {
            return Err(invalid_data(format!(
                "Cargo produced target outside the {} lane: {}::{}",
                lane.as_str(),
                package.name,
                target.name
            )));
        }
        let role = if artifact.test_profile {
            if !target.test {
                return Err(invalid_data(format!(
                    "Cargo marked non-test target as a test executable: {}::{}",
                    package.name, target.name
                )));
            }
            PreparedArtifactRole::TestExecutable
        } else if target.kind.iter().any(|kind| kind == "bin") {
            PreparedArtifactRole::RuntimeHelper
        } else {
            continue;
        };
        let executable = artifact.executable.canonicalize()?;
        seen_targets.insert((
            package.name.clone(),
            target.name.clone(),
            target.kind.clone(),
            role,
        ));
        artifacts.push(PreparedArtifact {
            package: package.name.clone(),
            target: target.name.clone(),
            target_kind: target.kind.clone(),
            executable,
            cwd: package.cwd.canonicalize()?,
            role,
            test_args: plan.package_test_args(lane, &package.name),
            environment: BTreeMap::new(),
            timeout_millis: None,
        });
    }
    Ok((artifacts, seen_targets))
}

fn expected_inventory(
    inventory: &WorkspaceInventory,
    lane: TestLane,
) -> (
    BTreeSet<ArtifactIdentity>,
    BTreeSet<ArtifactIdentity>,
    Vec<String>,
) {
    let mut expected_tests = BTreeSet::new();
    let mut expected_helpers = BTreeSet::new();
    let mut doctest_targets = Vec::new();
    for package in inventory.packages.values() {
        for target in &package.targets {
            if !target_enabled(target, lane) {
                continue;
            }
            if target.test {
                expected_tests.insert((
                    package.name.clone(),
                    target.name.clone(),
                    target.kind.clone(),
                    PreparedArtifactRole::TestExecutable,
                ));
            }
            if target.kind.iter().any(|kind| kind == "bin") {
                expected_helpers.insert((
                    package.name.clone(),
                    target.name.clone(),
                    target.kind.clone(),
                    PreparedArtifactRole::RuntimeHelper,
                ));
            }
            if target.doctest {
                doctest_targets.push(format!("{}::{}", package.name, target.name));
            }
        }
    }
    (expected_tests, expected_helpers, doctest_targets)
}

fn require_complete_inventory(
    seen_targets: &BTreeSet<ArtifactIdentity>,
    expected_tests: &BTreeSet<ArtifactIdentity>,
    expected_helpers: &BTreeSet<ArtifactIdentity>,
) -> io::Result<()> {
    let missing_tests = expected_tests
        .difference(seen_targets)
        .map(|(package, target, kind, _)| format!("{package}::{target} {kind:?}"))
        .collect::<Vec<_>>();
    if !missing_tests.is_empty() {
        return Err(invalid_data(format!(
            "Cargo producer omitted ordinary test artifacts: {}",
            missing_tests.join(", ")
        )));
    }
    let missing_helpers = expected_helpers
        .difference(seen_targets)
        .map(|(package, target, kind, _)| format!("{package}::{target} {kind:?}"))
        .collect::<Vec<_>>();
    if !missing_helpers.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Cargo producer omitted runtime helpers: {}",
                missing_helpers.join(", ")
            ),
        ));
    }
    Ok(())
}

pub fn write_immutable_manifest(manifest: &PreparedTestManifest, path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, manifest).map_err(io::Error::other)?;
    writeln!(file)?;
    file.sync_all()?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    Ok(())
}

pub fn read_manifest(path: &Path) -> io::Result<PreparedTestManifest> {
    let manifest: PreparedTestManifest =
        serde_json::from_reader(File::open(path)?).map_err(io::Error::other)?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn run_cargo_metadata(
    repository_root: &Path,
    artifact_dir: &Path,
) -> io::Result<WorkspaceInventory> {
    fs::create_dir_all(artifact_dir)?;
    let stdout_path = artifact_dir.join("cargo-metadata.json");
    let stderr_path = artifact_dir.join("cargo-metadata.stderr.log");
    phase_start("metadata", "cargo-metadata");
    let started = Instant::now();
    let output = Command::new(cargo_command())
        .current_dir(repository_root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    fs::write(&stdout_path, &output.stdout)?;
    fs::write(&stderr_path, &output.stderr)?;
    phase_end("metadata", "cargo-metadata", started, output.status);
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Cargo metadata failed with {}; diagnostics: {}",
            output.status,
            stderr_path.display()
        )));
    }
    WorkspaceInventory::parse(&output.stdout)
}

pub fn run_cargo_producer(
    repository_root: &Path,
    cargo_args: &[String],
    artifact_dir: &Path,
) -> io::Result<PathBuf> {
    fs::create_dir_all(artifact_dir)?;
    let json_path = artifact_dir.join("cargo-producer.jsonl");
    let stderr_path = artifact_dir.join("cargo-producer.stderr.log");
    let stdout = File::create(&json_path)?;
    let stderr = File::create(&stderr_path)?;
    phase_start("producer", "cargo-producer");
    eprintln!("cargo args: {}", cargo_args.join(" "));
    let started = Instant::now();
    let status = Command::new(cargo_command())
        .current_dir(repository_root)
        .args(cargo_args)
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()?;
    phase_end("producer", "cargo-producer", started, status);
    if !status.success() {
        return Err(io::Error::other(format!(
            "Cargo artifact producer failed with {status}; diagnostics: {}",
            stderr_path.display()
        )));
    }
    Ok(json_path)
}

pub fn run_doctests(
    repository_root: &Path,
    args: &[OsString],
    artifact_dir: &Path,
) -> io::Result<ExitStatus> {
    let stdout_path = artifact_dir.join("doctest.stdout.log");
    let stderr_path = artifact_dir.join("doctest.stderr.log");
    let stdout = File::create(&stdout_path)?;
    let stderr = File::create(&stderr_path)?;
    phase_start("doctest", "cargo-doctest-exception");
    eprintln!(
        "stable rustdoc exception: doctests compile and execute conventionally before signing"
    );
    let started = Instant::now();
    let status = Command::new(cargo_command())
        .current_dir(repository_root)
        .args(args)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .status()?;
    phase_end("doctest", "cargo-doctest-exception", started, status);
    if !status.success() {
        eprintln!("doctest stdout: {}", stdout_path.display());
        eprintln!("doctest stderr: {}", stderr_path.display());
    }
    Ok(status)
}

fn cargo_command() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn phase_start(name: &str, child_process_kind: &str) {
    eprintln!("=== PREPARED TEST PHASE START: {name} child_process_kind={child_process_kind} ===");
}

fn phase_end(name: &str, child_process_kind: &str, started: Instant, status: ExitStatus) {
    eprintln!(
        "=== PREPARED TEST PHASE END: {name} child_process_kind={child_process_kind} status={} elapsed={:.3}s ===",
        status,
        started.elapsed().as_secs_f64()
    );
}

pub fn run_cargo_and_prepare(
    repository_root: &Path,
    cargo_args: &[&str],
    target_dir: Option<&Path>,
    artifact_dir: &Path,
) -> io::Result<PreparedArtifacts> {
    fs::create_dir_all(artifact_dir)?;
    let json_path = artifact_dir.join("cargo-artifacts.jsonl");
    let stderr_path = artifact_dir.join("cargo-prebuild.stderr.log");
    let stdout = File::create(&json_path)?;
    let stderr = File::create(&stderr_path)?;
    let mut command = Command::new(cargo_command());
    command
        .current_dir(repository_root)
        .args(cargo_args)
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(target_dir) = target_dir {
        command.env("CARGO_TARGET_DIR", target_dir);
    }
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "Cargo preparation failed with {status}; diagnostics: {}",
            stderr_path.display()
        )));
    }
    let paths = parse_executables(BufReader::new(File::open(&json_path)?))?;
    if paths.is_empty() {
        return Err(invalid_data(format!(
            "Cargo reported no executable artifacts in {}",
            json_path.display()
        )));
    }
    prepare_paths_for_platform(&paths, artifact_dir)?;
    Ok(PreparedArtifacts { paths })
}

pub fn prepare_paths_for_platform(paths: &[PathBuf], artifact_dir: &Path) -> io::Result<()> {
    let manifest = artifact_dir.join("signed-executables.txt");
    let mut output = File::create(&manifest)?;
    for path in paths {
        writeln!(output, "{}", path.display())?;
    }
    #[cfg(target_os = "macos")]
    {
        sign_and_verify(paths)?;
        eprintln!(
            "=== MACOS TEST SIGNING: signed and verified {} Cargo-reported executables ===",
            paths.len()
        );
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!(
        "=== TEST SIGNING: non-macOS no-op ({} artifacts) ===",
        paths.len()
    );
    Ok(())
}

pub fn prepare_manifest_for_platform(
    manifest: &PreparedTestManifest,
    artifact_dir: &Path,
) -> io::Result<()> {
    let paths = manifest.signed_paths();
    phase_start("sign-and-verify", "codesign");
    let started = Instant::now();
    prepare_paths_for_platform(&paths, artifact_dir)?;
    eprintln!(
        "=== PREPARED TEST PHASE END: sign-and-verify child_process_kind=codesign status=success elapsed={:.3}s artifacts={} ===",
        started.elapsed().as_secs_f64(),
        paths.len()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn sign_and_verify(paths: &[PathBuf]) -> io::Result<()> {
    for path in paths {
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Cargo-reported executable is missing: {}", path.display()),
            ));
        }
        run_codesign(
            [
                OsStr::new("--force"),
                OsStr::new("--sign"),
                OsStr::new("-"),
                path.as_os_str(),
            ],
            path,
            "sign",
        )?;
        run_codesign(
            [
                OsStr::new("--verify"),
                OsStr::new("--strict"),
                path.as_os_str(),
            ],
            path,
            "verify",
        )?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_codesign<const N: usize>(args: [&OsStr; N], path: &Path, operation: &str) -> io::Result<()> {
    let output = Command::new("/usr/bin/codesign").args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "codesign {operation} failed for {} with {}: {}",
        path.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(target_os = "macos")]
pub fn verify_paths(paths: &[PathBuf]) -> io::Result<()> {
    for path in paths {
        run_codesign(
            [
                OsStr::new("--verify"),
                OsStr::new("--strict"),
                path.as_os_str(),
            ],
            path,
            "post-test verify",
        )?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn verify_paths(paths: &[PathBuf]) -> io::Result<()> {
    for path in paths {
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("prepared executable is missing: {}", path.display()),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub struct ExecutionSummary {
    pub passed: usize,
    pub failed: usize,
    pub timed_out: usize,
}

impl ExecutionSummary {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.timed_out > 0 {
            TIMEOUT_EXIT_CODE
        } else if self.failed > 0 {
            TEST_FAILURE_EXIT_CODE
        } else {
            0
        }
    }
}

pub fn execute_prepared_manifest(
    manifest: &PreparedTestManifest,
    timeout: Duration,
    global_test_args: &[OsString],
    artifact_dir: &Path,
) -> io::Result<ExecutionSummary> {
    manifest.validate()?;
    let signed_paths = manifest.signed_paths();
    verify_paths(&signed_paths)?;
    let watchdog = manifest.watchdog_path()?;
    let started = Instant::now();
    let mut summary = ExecutionSummary {
        passed: 0,
        failed: 0,
        timed_out: 0,
    };
    phase_start("execute", "prepared-test-via-watchdog");
    for test in manifest.tests() {
        let Some(global_remaining) = timeout.checked_sub(started.elapsed()) else {
            summary.timed_out += 1;
            eprintln!(
                "prepared test deadline exhausted before {}::{}",
                test.package, test.target
            );
            break;
        };
        let remaining = test.timeout_millis.map_or(global_remaining, |millis| {
            global_remaining.min(Duration::from_millis(millis))
        });
        let phase = format!("{}-{}", test.package, test.target);
        eprintln!(
            "=== PREPARED TEST START: package={} target={} child_process_kind=prepared-test executable={} ===",
            test.package,
            test.target,
            test.executable.display()
        );
        let test_started = Instant::now();
        let mut command = Command::new(watchdog);
        command
            .current_dir(&manifest.repository_root)
            .arg("--phase")
            .arg(&phase)
            .arg("--timeout-seconds")
            .arg(format!("{:.6}", remaining.as_secs_f64()))
            .arg("--artifact-root")
            .arg(artifact_dir.join("watchdog"))
            .arg("--working-directory")
            .arg(&test.cwd)
            .arg("--")
            .arg(&test.executable)
            .args(global_test_args)
            .args(&test.test_args)
            .env("CARGO_TARGET_DIR", &manifest.target_dir)
            .envs(&test.environment);
        preserve_cargo_dynamic_library_path(&mut command, &manifest.target_dir)?;
        let status = command.status()?;
        if status.success() {
            summary.passed += 1;
        } else if status.code() == Some(TIMEOUT_EXIT_CODE) {
            summary.timed_out += 1;
        } else {
            summary.failed += 1;
        }
        eprintln!(
            "=== PREPARED TEST END: package={} target={} status={} elapsed={:.3}s ===",
            test.package,
            test.target,
            status,
            test_started.elapsed().as_secs_f64()
        );
    }
    verify_paths(&signed_paths).map_err(|error| {
        io::Error::other(format!(
            "a prepared artifact was replaced or lost integrity after execution: {error}"
        ))
    })?;
    eprintln!(
        "=== PREPARED TEST PHASE END: execute child_process_kind=prepared-test-via-watchdog status={} elapsed={:.3}s passed={} failed={} timed_out={} ===",
        summary.exit_code(),
        started.elapsed().as_secs_f64(),
        summary.passed,
        summary.failed,
        summary.timed_out
    );
    Ok(summary)
}

fn preserve_cargo_dynamic_library_path(command: &mut Command, target_dir: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    const VARIABLE: &str = "DYLD_FALLBACK_LIBRARY_PATH";
    #[cfg(all(unix, not(target_os = "macos")))]
    const VARIABLE: &str = "LD_LIBRARY_PATH";
    #[cfg(not(unix))]
    const VARIABLE: &str = "PATH";

    let mut paths = vec![target_dir.join("debug/deps")];
    if let Some(existing) = std::env::var_os(VARIABLE) {
        paths.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(paths).map_err(io::Error::other)?;
    command.env(VARIABLE, joined);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory_fixture() -> (PathBuf, FullExecutionPlan, WorkspaceInventory, String) {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let root = repository
            .join("tmp/cargo-test-harness-tests")
            .join(format!("inventory-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("target/debug/deps")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        for name in ["watchdog.rs", "ordinary.rs", "full.rs"] {
            fs::write(root.join(name), "").unwrap();
        }
        let watchdog = root.join("target/debug/nefor-test-watchdog");
        let ordinary = root.join("target/debug/deps/ordinary-abc");
        let full = root.join("target/debug/deps/full-def");
        fs::write(&watchdog, "fixture").unwrap();
        fs::write(&ordinary, "fixture").unwrap();
        fs::write(&full, "fixture").unwrap();
        let package_id = "path+file:///fixture#fixture@0.1.0";
        let metadata = serde_json::json!({
            "target_directory": root.join("target"),
            "workspace_members": [package_id],
            "packages": [{
                "id": package_id,
                "name": "fixture",
                "manifest_path": root.join("Cargo.toml"),
                "targets": [
                    {"name":"nefor-test-watchdog","kind":["bin"],"src_path":root.join("watchdog.rs"),"test":false,"doctest":false,"required-features":[]},
                    {"name":"ordinary","kind":["test"],"src_path":root.join("ordinary.rs"),"test":true,"doctest":false,"required-features":[]},
                    {"name":"full","kind":["test"],"src_path":root.join("full.rs"),"test":true,"doctest":false,"required-features":["full-tests"]}
                ]
            }]
        });
        let inventory = WorkspaceInventory::parse(&serde_json::to_vec(&metadata).unwrap()).unwrap();
        let plan = parse_full_execution_plan(
            r#"{"cargo_full":{"harness_packages":["fixture"],"separate":[]}}"#,
        )
        .unwrap();
        let producer = [
            serde_json::json!({"reason":"compiler-artifact","package_id":package_id,"target":{"name":"nefor-test-watchdog","kind":["bin"],"src_path":root.join("watchdog.rs")},"profile":{"test":false},"executable":watchdog}),
            serde_json::json!({"reason":"compiler-artifact","package_id":package_id,"target":{"name":"ordinary","kind":["test"],"src_path":root.join("ordinary.rs")},"profile":{"test":true},"executable":ordinary}),
            serde_json::json!({"reason":"compiler-artifact","package_id":package_id,"target":{"name":"full","kind":["test"],"src_path":root.join("full.rs")},"profile":{"test":true},"executable":full}),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        (root, plan, inventory, producer)
    }

    #[test]
    fn parses_registry_as_the_only_full_package_owner() {
        let plan = parse_full_execution_plan(
            r#"{
              "cargo_full": {
                "harness_packages": ["alpha"],
                "separate": [{"package":"serial","test_args":["--test-threads=1"]}]
              }
            }"#,
        )
        .unwrap();
        assert_eq!(plan.feature_spec(), "alpha/full-tests,serial/full-tests");
        assert_eq!(
            plan.package_test_args(TestLane::Full, "serial"),
            [OsString::from("--test-threads=1")]
        );
        assert_eq!(
            plan.package_test_args(TestLane::Default, "serial"),
            Vec::<OsString>::new()
        );
    }

    #[test]
    fn parses_deduplicates_and_preserves_paths_with_spaces() {
        let input = br#"{"reason":"compiler-artifact","package_id":"a","target":{"name":"a","kind":["bin"],"src_path":"/tmp/a.rs"},"profile":{"test":false},"executable":"/tmp/a path/test"}
{"reason":"build-finished","success":true}
{"reason":"compiler-artifact","package_id":"a","target":{"name":"a","kind":["bin"],"src_path":"/tmp/a.rs"},"profile":{"test":false},"executable":"/tmp/a path/test"}
{"reason":"compiler-artifact","package_id":"a","target":{"name":"a","kind":["bin"],"src_path":"/tmp/a.rs"},"profile":{"test":false},"executable":null}
"#;
        assert_eq!(
            parse_executables(&input[..]).unwrap(),
            vec![PathBuf::from("/tmp/a path/test")]
        );
    }

    #[test]
    fn rejects_malformed_and_incomplete_artifacts() {
        let error = parse_executables(&b"not-json\n"[..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let missing = br#"{"reason":"compiler-artifact","target":{},"executable":"/tmp/a"}
"#;
        assert!(parse_executables(&missing[..]).is_err());
    }

    #[test]
    fn metadata_and_producer_inventory_have_exact_lane_parity() {
        let (root, plan, inventory, producer) = inventory_fixture();
        let full = build_prepared_manifest(
            &root,
            &inventory,
            &plan,
            TestLane::Full,
            producer.as_bytes(),
        )
        .unwrap();
        assert_eq!(full.tests().count(), 2);
        assert_eq!(full.signed_paths().len(), 3);

        let default_producer = producer
            .lines()
            .filter(|line| !line.contains("\"name\":\"full\""))
            .collect::<Vec<_>>()
            .join("\n");
        let default = build_prepared_manifest(
            &root,
            &inventory,
            &plan,
            TestLane::Default,
            default_producer.as_bytes(),
        )
        .unwrap();
        assert_eq!(default.tests().count(), 1);

        let missing_helper = producer
            .lines()
            .filter(|line| !line.contains("nefor-test-watchdog"))
            .collect::<Vec<_>>()
            .join("\n");
        let error = build_prepared_manifest(
            &root,
            &inventory,
            &plan,
            TestLane::Full,
            missing_helper.as_bytes(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("runtime helpers"));
        fs::remove_dir_all(root).unwrap();
    }
}
