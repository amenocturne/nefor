#![deny(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::{Deserialize, Serialize};

pub const CREATE_TOOL: &str = "git_worktree_create";
pub const OPEN_TOOL: &str = "git_worktree_open";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSpec {
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenSpec {
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    pub repository: String,
    pub path: String,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeError {
    pub operation: &'static str,
    pub kind: &'static str,
    pub message: String,
}

impl WorktreeError {
    fn new(operation: &'static str, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            operation,
            kind,
            message: message.into(),
        }
    }
}

pub fn create(spec: &CreateSpec) -> Result<Worktree, WorktreeError> {
    const OP: &str = "create";
    validate_absolute(OP, "repository", &spec.repository)?;
    validate_absolute(OP, "path", &spec.path)?;
    validate_non_empty(OP, "branch", &spec.branch)?;
    validate_non_empty(OP, "base", &spec.base)?;

    let repository = canonical_repository(OP, &spec.repository)?;
    if spec.path.exists() {
        return Err(WorktreeError::new(
            OP,
            "path-exists",
            format!("worktree path already exists: {}", spec.path.display()),
        ));
    }
    let path = canonical_missing_path(OP, &spec.path)?;

    match git_status(
        &repository,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", spec.branch),
        ],
    )? {
        0 => {
            return Err(WorktreeError::new(
                OP,
                "branch-exists",
                format!("local branch already exists: {}", spec.branch),
            ));
        }
        1 => {}
        code => {
            return Err(WorktreeError::new(
                OP,
                "git-failed",
                format!("git show-ref exited with status {code}"),
            ));
        }
    }

    let head = git_text(
        OP,
        &repository,
        [
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", spec.base),
        ],
    )
    .map_err(|err| WorktreeError::new(OP, "base-not-found", err.message))?;

    let output = git_output(
        OP,
        &repository,
        [
            "worktree",
            "add",
            "-b",
            &spec.branch,
            path.to_string_lossy().as_ref(),
            &head,
        ],
    )?;
    require_success(OP, output)?;

    Ok(Worktree {
        repository: path_text(&repository),
        path: path_text(&path),
        branch: spec.branch.clone(),
        head,
    })
}

pub fn open(spec: &OpenSpec) -> Result<Worktree, WorktreeError> {
    const OP: &str = "open";
    validate_absolute(OP, "repository", &spec.repository)?;
    validate_absolute(OP, "path", &spec.path)?;
    validate_non_empty(OP, "branch", &spec.branch)?;

    let repository = canonical_repository(OP, &spec.repository)?;
    if !spec.path.exists() {
        return Err(WorktreeError::new(
            OP,
            "worktree-not-found",
            format!("worktree path does not exist: {}", spec.path.display()),
        ));
    }
    let path = std::fs::canonicalize(&spec.path).map_err(|error| {
        WorktreeError::new(
            OP,
            "invalid-path",
            format!("cannot resolve {}: {error}", spec.path.display()),
        )
    })?;

    let path_common = git_text(
        OP,
        &path,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|_| {
        WorktreeError::new(
            OP,
            "worktree-not-found",
            format!("path is not a registered Git worktree: {}", path.display()),
        )
    })?;
    let repository_common = git_text(
        OP,
        &repository,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if canonical_existing_text(&path_common) != canonical_existing_text(&repository_common) {
        return Err(WorktreeError::new(
            OP,
            "repository-mismatch",
            format!("{} belongs to another Git repository", path.display()),
        ));
    }

    if !registered_paths(OP, &repository)?
        .iter()
        .any(|candidate| candidate == &path)
    {
        return Err(WorktreeError::new(
            OP,
            "worktree-not-found",
            format!("path is not registered as a worktree: {}", path.display()),
        ));
    }

    let branch_output = git_output(OP, &path, ["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if branch_output.status.code() == Some(1) {
        return Err(WorktreeError::new(
            OP,
            "detached-head",
            format!("worktree has detached HEAD: {}", path.display()),
        ));
    }
    let branch = require_text(OP, branch_output)?;
    if branch != spec.branch {
        return Err(WorktreeError::new(
            OP,
            "branch-mismatch",
            format!("expected branch {}, found {branch}", spec.branch),
        ));
    }
    let head = git_text(OP, &path, ["rev-parse", "--verify", "HEAD^{commit}"])?;

    Ok(Worktree {
        repository: path_text(&repository),
        path: path_text(&path),
        branch,
        head,
    })
}

fn validate_absolute(
    operation: &'static str,
    field: &str,
    path: &Path,
) -> Result<(), WorktreeError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(WorktreeError::new(
            operation,
            "invalid-path",
            format!("{field} must be an absolute path: {}", path.display()),
        ))
    }
}

fn validate_non_empty(
    operation: &'static str,
    field: &str,
    value: &str,
) -> Result<(), WorktreeError> {
    if value.is_empty() {
        Err(WorktreeError::new(
            operation,
            "invalid-path",
            format!("{field} must not be empty"),
        ))
    } else {
        Ok(())
    }
}

fn canonical_repository(
    operation: &'static str,
    repository: &Path,
) -> Result<PathBuf, WorktreeError> {
    let canonical = std::fs::canonicalize(repository).map_err(|error| {
        WorktreeError::new(
            operation,
            "invalid-repository",
            format!(
                "cannot resolve repository {}: {error}",
                repository.display()
            ),
        )
    })?;
    git_text(operation, &canonical, ["rev-parse", "--git-dir"])
        .map_err(|err| WorktreeError::new(operation, "invalid-repository", err.message))?;
    Ok(canonical)
}

fn canonical_missing_path(operation: &'static str, path: &Path) -> Result<PathBuf, WorktreeError> {
    let parent = path.parent().ok_or_else(|| {
        WorktreeError::new(operation, "invalid-path", "worktree path has no parent")
    })?;
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        WorktreeError::new(
            operation,
            "parent-not-found",
            format!("cannot resolve parent {}: {error}", parent.display()),
        )
    })?;
    let name = path.file_name().ok_or_else(|| {
        WorktreeError::new(
            operation,
            "invalid-path",
            "worktree path has no final component",
        )
    })?;
    Ok(parent.join(name))
}

fn registered_paths(
    operation: &'static str,
    repository: &Path,
) -> Result<Vec<PathBuf>, WorktreeError> {
    let output = git_text(operation, repository, ["worktree", "list", "--porcelain"])?;
    Ok(output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect())
}

fn canonical_existing_text(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

fn git_status<const N: usize>(repository: &Path, args: [&str; N]) -> Result<i32, WorktreeError> {
    let output = git_output("create", repository, args)?;
    Ok(output.status.code().unwrap_or(-1))
}

fn git_text<const N: usize>(
    operation: &'static str,
    repository: &Path,
    args: [&str; N],
) -> Result<String, WorktreeError> {
    let output = git_output(operation, repository, args)?;
    require_text(operation, output)
}

fn git_output<const N: usize>(
    operation: &'static str,
    repository: &Path,
    args: [&str; N],
) -> Result<Output, WorktreeError> {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .map_err(|error| {
            WorktreeError::new(
                operation,
                "git-failed",
                format!("failed to execute git: {error}"),
            )
        })
}

fn require_success(operation: &'static str, output: Output) -> Result<(), WorktreeError> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(operation, output))
    }
}

fn require_text(operation: &'static str, output: Output) -> Result<String, WorktreeError> {
    if !output.status.success() {
        return Err(git_failure(operation, output));
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|error| {
            WorktreeError::new(
                operation,
                "git-failed",
                format!("git returned non-UTF-8 output: {error}"),
            )
        })
}

fn git_failure(operation: &'static str, output: Output) -> WorktreeError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let message = if stderr.is_empty() {
        format!("git exited with status {}", output.status)
    } else {
        stderr
    };
    WorktreeError::new(operation, "git-failed", message)
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        _temp: TempDir,
        repository: PathBuf,
        worktrees: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().expect("tempdir");
            let repository = temp.path().join("repo");
            let worktrees = temp.path().join("worktrees");
            std::fs::create_dir_all(&repository).expect("repo dir");
            std::fs::create_dir_all(&worktrees).expect("worktrees dir");
            git(&repository, ["init", "-b", "main"]);
            git(&repository, ["config", "user.email", "test@example.com"]);
            git(&repository, ["config", "user.name", "Test"]);
            std::fs::write(repository.join("README.md"), "base\n").expect("seed");
            git(&repository, ["add", "README.md"]);
            git(&repository, ["commit", "-m", "base"]);
            Self {
                _temp: temp,
                repository,
                worktrees,
            }
        }

        fn create_spec(&self) -> CreateSpec {
            CreateSpec {
                repository: self.repository.clone(),
                path: self.worktrees.join("feature"),
                branch: "feature".into(),
                base: "main".into(),
            }
        }
    }

    fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("git executes");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_value<const N: usize>(cwd: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .expect("git executes");
        assert!(output.status.success(), "git command succeeds");
        String::from_utf8(output.stdout)
            .expect("git output is UTF-8")
            .trim()
            .to_owned()
    }

    #[test]
    fn create_is_fresh_only_and_open_is_explicit() {
        let fixture = Fixture::new();
        let spec = fixture.create_spec();

        let created = create(&spec).expect("create");
        assert_eq!(created.branch, "feature");
        assert_eq!(
            created.head,
            git_value(&fixture.repository, ["rev-parse", "main^{commit}"])
        );
        assert_eq!(
            created.path,
            path_text(&std::fs::canonicalize(&spec.path).expect("canonical worktree"))
        );

        let collision = create(&spec).expect_err("second create must fail");
        assert_eq!(collision.kind, "path-exists");

        let opened = open(&OpenSpec {
            repository: spec.repository,
            path: spec.path,
            branch: spec.branch,
        })
        .expect("explicit open");
        assert_eq!(opened, created);
    }

    #[test]
    fn create_rejects_existing_branch_even_without_worktree() {
        let fixture = Fixture::new();
        git(&fixture.repository, ["branch", "feature"]);
        let error = create(&fixture.create_spec()).expect_err("branch collision");
        assert_eq!(error.kind, "branch-exists");
        assert_eq!(
            git_value(&fixture.repository, ["rev-parse", "feature^{commit}"]),
            git_value(&fixture.repository, ["rev-parse", "main^{commit}"])
        );
    }

    #[test]
    fn create_reports_missing_base_parent_and_repository_distinctly() {
        let fixture = Fixture::new();

        let mut missing_base = fixture.create_spec();
        missing_base.base = "does-not-exist".into();
        assert_eq!(
            create(&missing_base).expect_err("missing base").kind,
            "base-not-found"
        );

        let mut missing_parent = fixture.create_spec();
        missing_parent.path = fixture.worktrees.join("missing").join("feature");
        assert_eq!(
            create(&missing_parent).expect_err("missing parent").kind,
            "parent-not-found"
        );

        let mut invalid_repository = fixture.create_spec();
        invalid_repository.repository = fixture.worktrees.clone();
        assert_eq!(
            create(&invalid_repository)
                .expect_err("invalid repository")
                .kind,
            "invalid-repository"
        );
    }

    #[test]
    fn open_preserves_dirty_state_and_checks_branch() {
        let fixture = Fixture::new();
        let spec = fixture.create_spec();
        create(&spec).expect("create");
        std::fs::write(spec.path.join("dirty.txt"), "dirty\n").expect("dirty file");

        open(&OpenSpec {
            repository: spec.repository.clone(),
            path: spec.path.clone(),
            branch: spec.branch.clone(),
        })
        .expect("open dirty worktree");
        assert!(spec.path.join("dirty.txt").exists());

        let error = open(&OpenSpec {
            repository: spec.repository,
            path: spec.path,
            branch: "main".into(),
        })
        .expect_err("branch mismatch");
        assert_eq!(error.kind, "branch-mismatch");
    }

    #[test]
    fn open_rejects_missing_worktree() {
        let fixture = Fixture::new();
        let error = open(&OpenSpec {
            repository: fixture.repository,
            path: fixture.worktrees.join("missing"),
            branch: "feature".into(),
        })
        .expect_err("missing");
        assert_eq!(error.kind, "worktree-not-found");
    }

    #[test]
    fn create_treats_metacharacters_in_paths_literally() {
        let fixture = Fixture::new();
        let mut spec = fixture.create_spec();
        spec.path = fixture.worktrees.join("feature; literal path");

        let created = create(&spec).expect("literal path create");
        assert!(Path::new(&created.path).is_dir());
        assert!(!fixture.worktrees.join("feature").exists());
    }

    #[test]
    fn open_rejects_a_worktree_from_another_repository() {
        let first = Fixture::new();
        let second = Fixture::new();
        let spec = first.create_spec();
        create(&spec).expect("create first worktree");

        let error = open(&OpenSpec {
            repository: second.repository,
            path: spec.path,
            branch: spec.branch,
        })
        .expect_err("repository mismatch");
        assert_eq!(error.kind, "repository-mismatch");
    }

    #[test]
    fn open_rejects_detached_head() {
        let fixture = Fixture::new();
        let spec = fixture.create_spec();
        create(&spec).expect("create");
        git(&spec.path, ["checkout", "--detach"]);

        let error = open(&OpenSpec {
            repository: spec.repository,
            path: spec.path,
            branch: spec.branch,
        })
        .expect_err("detached head");
        assert_eq!(error.kind, "detached-head");
    }
}
