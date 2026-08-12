#![allow(clippy::missing_errors_doc)]

use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug)]
pub struct PreparedArtifacts {
    pub paths: Vec<PathBuf>,
}

pub fn parse_executables(input: impl BufRead) -> io::Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for (index, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed Cargo JSON on line {}: {error}", index + 1),
            )
        })?;
        if value.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let Some(executable) = value.get("executable") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Cargo compiler-artifact on line {} has no executable field",
                    index + 1
                ),
            ));
        };
        if executable.is_null() {
            continue;
        }
        let executable = executable.as_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Cargo executable on line {} is not a string", index + 1),
            )
        })?;
        paths.insert(PathBuf::from(executable));
    }
    Ok(paths.into_iter().collect())
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
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    eprintln!(
        "=== MACOS TEST PREBUILD: cargo {} ===",
        cargo_args.join(" ")
    );
    let mut command = Command::new(cargo);
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
            "Cargo test prebuild failed with {status}; diagnostics: {}",
            stderr_path.display()
        )));
    }
    let paths = parse_executables(BufReader::new(File::open(&json_path)?))?;
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Cargo reported no executable artifacts in {}",
                json_path.display()
            ),
        ));
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
        eprintln!("manifest: {}", manifest.display());
    }
    #[cfg(not(target_os = "macos"))]
    eprintln!(
        "=== TEST SIGNING: non-macOS no-op ({} artifacts) ===",
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
pub fn verify_paths(_paths: &[PathBuf]) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_deduplicates_and_preserves_paths_with_spaces() {
        let input = br#"{"reason":"compiler-artifact","executable":"/tmp/a path/test"}
{"reason":"build-finished","success":true}
{"reason":"compiler-artifact","executable":"/tmp/a path/test"}
{"reason":"compiler-artifact","executable":null}
"#;
        assert_eq!(
            parse_executables(&input[..]).unwrap(),
            vec![PathBuf::from("/tmp/a path/test")]
        );
    }

    #[test]
    fn rejects_malformed_and_missing_artifacts() {
        let error = parse_executables(&b"not-json\n"[..]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let missing = br#"{"reason":"compiler-artifact","target":{}}
"#;
        assert!(parse_executables(&missing[..]).is_err());
    }

    #[test]
    fn platform_preparation_writes_exact_manifest() {
        let root =
            std::env::temp_dir().join(format!("nefor-signing-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        #[cfg(not(target_os = "macos"))]
        {
            let paths = vec![PathBuf::from("/does not need to exist")];
            prepare_paths_for_platform(&paths, &root).unwrap();
            assert_eq!(
                fs::read_to_string(root.join("signed-executables.txt")).unwrap(),
                "/does not need to exist\n"
            );
        }
        #[cfg(target_os = "macos")]
        {
            let source = root.join("fixture.rs");
            let binary = root.join("fixture with spaces");
            fs::write(&source, "fn main() {}\n").unwrap();
            let status = Command::new("rustc")
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap();
            assert!(status.success());
            prepare_paths_for_platform(std::slice::from_ref(&binary), &root).unwrap();
            verify_paths(&[binary]).unwrap();
            let missing = root.join("missing executable");
            let error = sign_and_verify(&[missing]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
