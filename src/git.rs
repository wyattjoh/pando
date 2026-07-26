use std::{
    ffi::{OsStr, OsString},
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::{Condition, Worktree, WorktreeKind};

#[derive(Debug)]
pub struct Repository {
    pub worktrees: Vec<Worktree>,
    pub current_index: usize,
    pub primary: Option<PathBuf>,
    pub common_dir: PathBuf,
}

impl Repository {
    #[must_use]
    pub fn current(&self) -> &Worktree {
        &self.worktrees[self.current_index]
    }

    /// Returns the canonical primary-worktree path used as repository identity.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no primary worktree or its path cannot be resolved.
    pub fn identity(&self) -> Result<PathBuf> {
        let primary = self
            .primary
            .as_ref()
            .context("the current repository has no primary worktree")?;
        canonical_or_normalized(primary)
            .with_context(|| format!("failed to resolve repository path {}", primary.display()))
    }
}

/// Discovers and enriches the worktrees for the repository containing `cwd`.
///
/// # Errors
///
/// Returns an error when Git cannot run or its worktree output is invalid.
pub fn discover(cwd: &Path) -> Result<Vec<Worktree>> {
    let output = run_git(cwd, ["worktree", "list", "--porcelain", "-z"])
        .context("failed to list Git worktrees for the current repository")?;
    ensure_success(&output, "git worktree list")?;

    let mut worktrees = parse_porcelain(&output.stdout)?;
    let current = current_record(&worktrees, cwd);
    for (index, worktree) in worktrees.iter_mut().enumerate() {
        worktree.current = current == Some(index);
        worktree.condition = inspect_condition(worktree);
    }
    Ok(worktrees)
}

/// Resolves repository-level worktree and common-directory context.
///
/// # Errors
///
/// Returns an error when Git context or a required path cannot be resolved.
pub fn repository(cwd: &Path) -> Result<Repository> {
    let worktrees = discover(cwd)?;
    let current_index = worktrees
        .iter()
        .position(|worktree| worktree.current)
        .context("the current directory is not inside a registered Git worktree")?;
    let primary = worktrees
        .first()
        .filter(|worktree| !worktree.is_bare())
        .map(|worktree| canonical_or_normalized(&worktree.path))
        .transpose()?;
    let common = git_stdout(
        cwd,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .context("failed to resolve Git's common directory")?;
    let common_dir = canonical_or_normalized(Path::new(&common))?;
    Ok(Repository {
        worktrees,
        current_index,
        primary,
        common_dir,
    })
}

/// Returns the current named branch.
///
/// # Errors
///
/// Returns an error for detached, bare, or unknown current worktree states.
pub fn current_branch(repository: &Repository) -> Result<&str> {
    match &repository.current().kind {
        WorktreeKind::Branch(branch) => Ok(branch),
        WorktreeKind::Detached => bail!(
            "the current worktree at {} is detached; this query requires a named branch",
            repository.current().path.display()
        ),
        WorktreeKind::Bare => {
            bail!("the current repository is bare; this query requires a worktree")
        }
        WorktreeKind::Unknown => {
            bail!("Git did not report a named branch for the current worktree")
        }
    }
}

/// Asks Git to validate a proposed branch name.
///
/// # Errors
///
/// Returns an error when Git cannot run or rejects the name.
pub fn validate_branch(cwd: &Path, branch: &str) -> Result<()> {
    if branch.is_empty() {
        bail!("branch name cannot be empty");
    }
    let output = run_git(cwd, ["check-ref-format", "--branch", branch])
        .context("failed to ask Git to validate the branch name")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "Git rejected branch name {branch:?}: {}",
            stderr_detail(&output)
        )
    }
}

/// Reports whether an exact local branch exists.
///
/// # Errors
///
/// Returns an error when Git cannot inspect local refs.
pub fn local_branch_exists(cwd: &Path, branch: &str) -> Result<bool> {
    let reference = format!("refs/heads/{branch}");
    let output = run_git(cwd, ["show-ref", "--verify", "--quiet", &reference])
        .context("failed to inspect local branches")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to inspect local branch {branch:?}: {}",
            stderr_detail(&output)
        ),
    }
}

/// Returns already-fetched remote-tracking refs matching a branch name.
///
/// # Errors
///
/// Returns an error when Git cannot inspect remote refs.
pub fn remote_matches(cwd: &Path, branch: &str) -> Result<Vec<String>> {
    let output = run_git(cwd, ["remote"]).context("failed to inspect configured remotes")?;
    ensure_success(&output, "git remote")?;
    let mut matches = Vec::new();
    for remote in String::from_utf8_lossy(&output.stdout).lines() {
        let short_name = format!("{remote}/{branch}");
        let reference = format!("refs/remotes/{short_name}");
        let output = run_git(cwd, ["show-ref", "--verify", "--quiet", &reference])
            .context("failed to inspect remote-tracking branches")?;
        match output.status.code() {
            Some(0) => matches.push(short_name),
            Some(1) => {}
            _ => bail!(
                "failed to inspect remote-tracking branch {reference:?}: {}",
                stderr_detail(&output)
            ),
        }
    }
    matches.sort();
    Ok(matches)
}

/// Resolves the stable Git administrative directory for one worktree.
///
/// # Errors
///
/// Returns an error when Git cannot resolve the worktree's administrative directory.
pub fn worktree_identity(cwd: &Path) -> Result<PathBuf> {
    let git_dir = git_stdout(cwd, ["rev-parse", "--path-format=absolute", "--git-dir"])
        .context("failed to resolve the worktree's Git administrative directory")?;
    canonical_or_normalized(Path::new(&git_dir))
}

/// Resolves the invoking worktree's committed `HEAD`.
///
/// # Errors
///
/// Returns an error when Git cannot resolve `HEAD`.
pub fn head_commit(cwd: &Path) -> Result<String> {
    git_stdout(cwd, ["rev-parse", "--verify", "HEAD"])
        .context("failed to resolve the invoking worktree's HEAD")
}

/// Runs a fast-forward-only merge, preserving Git output on the caller's stderr.
///
/// # Errors
///
/// Returns an error when Git cannot execute the merge.
pub fn merge_ff_only(cwd: &Path, branch: &str) -> Result<()> {
    run_git_inherit(cwd, ["merge", "--ff-only", branch], "fast-forward merge")
}

/// Rebases the current branch onto `target`, preserving Git output on stderr.
///
/// # Errors
///
/// Returns an error when Git cannot execute the rebase.
pub fn rebase_onto(cwd: &Path, target: &str) -> Result<()> {
    run_git_inherit(cwd, ["rebase", target], "rebase")
}

/// Continues an already active rebase, preserving Git output on stderr.
///
/// # Errors
///
/// Returns an error when Git cannot continue the rebase.
pub fn rebase_continue(cwd: &Path) -> Result<()> {
    run_git_inherit(cwd, ["rebase", "--continue"], "rebase continuation")
}

/// Removes a registered worktree without deleting its branch.
///
/// # Errors
///
/// Returns an error when Git cannot remove the worktree.
pub fn remove_worktree(cwd: &Path, path: &Path, force: bool) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("worktree").arg("remove");
    if force {
        command.arg("--force");
    }
    let status = command
        .arg(path)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::from(open_stderr()?))
        .stderr(Stdio::inherit())
        .status()
        .context("failed to start git worktree remove")?;
    if status.success() {
        Ok(())
    } else {
        bail!("git worktree remove failed with {status}")
    }
}

/// Reports whether `ancestor` is an ancestor of `descendant`.
///
/// # Errors
///
/// Returns an error when Git cannot inspect ancestry.
pub fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = run_git(cwd, ["merge-base", "--is-ancestor", ancestor, descendant])
        .context("failed to inspect commit ancestry")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor failed: {}",
            stderr_detail(&output)
        ),
    }
}

/// Reports whether a Git rebase is active in this worktree.
///
/// # Errors
///
/// Returns an error when Git state cannot be inspected.
pub fn rebase_in_progress(cwd: &Path) -> Result<bool> {
    let git_dir = worktree_identity(cwd)?;
    Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
}

/// Reports whether the invoking worktree has staged, unstaged, or untracked changes.
///
/// # Errors
///
/// Returns an error when Git cannot inspect status.
/// Stages every tracked and untracked change in the worktree.
///
/// # Errors
///
/// Returns an error when Git cannot stage the worktree changes.
pub fn stage_all(cwd: &Path) -> Result<()> {
    let output = run_git(cwd, ["add", "--all"]).context("failed to start git add --all")?;
    ensure_success(&output, "git add --all")
}

/// Reports whether the Git index contains changes relative to `HEAD`.
///
/// # Errors
///
/// Returns an error when Git cannot inspect the index.
pub fn has_staged_changes(cwd: &Path) -> Result<bool> {
    let output = run_git(cwd, ["diff", "--cached", "--quiet"])
        .context("failed to inspect staged changes")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!(
            "git diff --cached --quiet failed: {}",
            stderr_detail(&output)
        ),
    }
}

/// Returns stable staged patch output without colour or external diff commands.
///
/// # Errors
///
/// Returns an error when Git cannot produce the staged diff.
pub fn staged_diff(cwd: &Path) -> Result<String> {
    git_stdout(
        cwd,
        [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
        ],
    )
}

/// Returns stable staged diff statistics.
///
/// # Errors
///
/// Returns an error when Git cannot produce staged diff statistics.
pub fn staged_diff_stat(cwd: &Path) -> Result<String> {
    git_stdout(
        cwd,
        [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--stat",
        ],
    )
}

/// Returns up to ten reachable commit subjects, newest first.
///
/// # Errors
///
/// Returns an error when Git cannot read commit history other than an unborn `HEAD`.
pub fn recent_subjects(cwd: &Path) -> Result<Vec<String>> {
    let output =
        run_git(cwd, ["log", "-10", "--format=%s"]).context("failed to read recent commits")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect());
    }
    let verify = run_git(cwd, ["rev-parse", "--verify", "HEAD"])?;
    if !verify.status.success() {
        return Ok(Vec::new());
    }
    bail!("git log failed: {}", stderr_detail(&output))
}

/// Commits using the supplied message while inheriting Git's output streams.
///
/// # Errors
///
/// Returns an error when Git cannot create the commit.
pub fn commit(cwd: &Path, message: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to start git commit")?;
    if status.success() {
        Ok(())
    } else {
        bail!("git commit failed with status {status}")
    }
}

/// Reports whether the invoking worktree has staged, unstaged, or untracked changes.
///
/// # Errors
///
/// Returns an error when Git cannot inspect status.
pub fn is_dirty(cwd: &Path) -> Result<bool> {
    let output = run_git(cwd, ["status", "--porcelain", "--untracked-files=normal"])
        .context("failed to inspect invoking worktree changes")?;
    ensure_success(&output, "git status")?;
    Ok(!output.stdout.is_empty())
}

/// Reports whether Git ignores an existing, untracked path.
///
/// # Errors
///
/// Returns an error when Git cannot inspect ignore rules.
pub fn is_ignored(cwd: &Path, path: &Path) -> Result<bool> {
    check_ignored(cwd, path, false)
}

/// Reports whether Git would ignore a path that may not exist yet.
///
/// # Errors
///
/// Returns an error when Git cannot inspect ignore rules.
pub fn would_be_ignored(cwd: &Path, path: &Path) -> Result<bool> {
    check_ignored(cwd, path, true)
}

fn check_ignored(cwd: &Path, path: &Path, no_index: bool) -> Result<bool> {
    let mut command = Command::new("git");
    command.args([OsStr::new("check-ignore"), OsStr::new("--quiet")]);
    if no_index {
        command.arg("--no-index");
    }
    let output = command
        .arg(path)
        .current_dir(cwd)
        .output()
        .context("failed to ask Git whether the path is ignored")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git check-ignore failed for {}: {}",
            path.display(),
            stderr_detail(&output)
        ),
    }
}

/// Adds a worktree for an existing local branch.
///
/// # Errors
///
/// Returns an error when Git cannot create the worktree.
pub fn add_existing_worktree(cwd: &Path, destination: &Path, branch: &str) -> Result<()> {
    run_worktree_add(
        cwd,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            destination.as_os_str(),
            OsStr::new(branch),
        ],
        branch,
        destination,
    )
}

/// Adds a local tracking branch and its worktree.
///
/// # Errors
///
/// Returns an error when Git cannot create the branch or worktree.
pub fn add_tracking_worktree(
    cwd: &Path,
    destination: &Path,
    branch: &str,
    remote: &str,
) -> Result<()> {
    run_worktree_add(
        cwd,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--track"),
            OsStr::new("-b"),
            OsStr::new(branch),
            destination.as_os_str(),
            OsStr::new(remote),
        ],
        branch,
        destination,
    )
}

/// Adds a new branch at `head` and its worktree.
///
/// # Errors
///
/// Returns an error when Git cannot create the branch or worktree.
pub fn add_new_worktree(cwd: &Path, destination: &Path, branch: &str, head: &str) -> Result<()> {
    run_worktree_add(
        cwd,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(branch),
            destination.as_os_str(),
            OsStr::new(head),
        ],
        branch,
        destination,
    )
}

fn run_worktree_add<const N: usize>(
    cwd: &Path,
    args: [&OsStr; N],
    branch: &str,
    destination: &Path,
) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| {
            format!(
                "failed to start Git while creating worktree for {branch:?} at {}",
                destination.display()
            )
        })?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "Git failed to create worktree for {branch:?} at {}: {}",
            destination.display(),
            stderr_detail(&output)
        )
    }
}

fn open_stderr() -> Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .open("/dev/stderr")
        .map_err(Into::into)
}

fn run_git_inherit<const N: usize>(cwd: &Path, args: [&str; N], operation: &str) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::from(open_stderr()?))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start git {operation}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("git {operation} failed with {status}")
    }
}

fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let operation = format!("git {}", args.join(" "));
    let output = run_git(cwd, args).with_context(|| format!("failed to start {operation}"))?;
    ensure_success(&output, &operation)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn current_record(worktrees: &[Worktree], cwd: &Path) -> Option<usize> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    worktrees
        .iter()
        .enumerate()
        .filter_map(|(index, worktree)| {
            let path = worktree
                .path
                .canonicalize()
                .unwrap_or_else(|_| worktree.path.clone());
            cwd.starts_with(&path)
                .then(|| (index, path.components().count()))
        })
        .max_by_key(|(_, depth)| *depth)
        .map(|(index, _)| index)
}

fn inspect_condition(worktree: &Worktree) -> Condition {
    if worktree.is_bare() {
        return Condition::Clean;
    }
    match fs::metadata(&worktree.path) {
        Ok(metadata) if metadata.is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Condition::Missing,
        Ok(_) | Err(_) => return Condition::Inaccessible,
    }

    match run_git(
        &worktree.path,
        ["status", "--porcelain", "--untracked-files=normal"],
    ) {
        Ok(output) if output.status.success() && output.stdout.is_empty() => Condition::Clean,
        Ok(output) if output.status.success() => Condition::Dirty,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Condition::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Condition::Inaccessible
        }
        Ok(_) | Err(_) => Condition::Unknown,
    }
}

fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> std::io::Result<Output> {
    Command::new("git").args(args).current_dir(cwd).output()
}

fn stderr_detail(output: &Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        output.status.to_string()
    } else {
        detail
    }
}

fn ensure_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!("{operation} failed: {}", stderr_detail(output));
}

/// Canonicalizes the longest existing path prefix while preserving missing suffixes.
///
/// # Errors
///
/// Returns an error when no existing ancestor or canonical path can be resolved.
pub fn canonical_or_normalized(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path.canonicalize().map_err(Into::into);
    }
    let mut missing = Vec::<OsString>::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .context("configured path has no existing ancestor")?;
        missing.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .context("configured path has no existing ancestor")?;
    }
    let mut normalized = ancestor.canonicalize()?;
    for component in missing.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

/// Parses NUL-delimited `git worktree list --porcelain -z` output.
///
/// # Errors
///
/// Returns an error for malformed or empty Git output.
pub fn parse_porcelain(bytes: &[u8]) -> Result<Vec<Worktree>> {
    let mut records = Vec::new();
    let mut current: Option<Worktree> = None;

    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(Worktree {
                path: PathBuf::from(OsString::from_vec(path.to_vec())),
                head: None,
                kind: WorktreeKind::Unknown,
                locked: None,
                prunable: None,
                current: false,
                condition: Condition::Unknown,
            });
            continue;
        }
        let Some(record) = current.as_mut() else {
            bail!("invalid Git worktree output: attribute before worktree record");
        };
        if let Some(value) = field.strip_prefix(b"HEAD ") {
            record.head = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            let branch = String::from_utf8_lossy(value);
            record.kind = WorktreeKind::Branch(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&branch)
                    .to_owned(),
            );
        } else if field == b"detached" {
            record.kind = WorktreeKind::Detached;
        } else if field == b"bare" {
            record.kind = WorktreeKind::Bare;
        } else if field == b"locked" {
            record.locked = Some(String::new());
        } else if let Some(value) = field.strip_prefix(b"locked ") {
            record.locked = Some(String::from_utf8_lossy(value).into_owned());
        } else if field == b"prunable" {
            record.prunable = Some(String::new());
        } else if let Some(value) = field.strip_prefix(b"prunable ") {
            record.prunable = Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    if let Some(record) = current {
        records.push(record);
    }
    if records.is_empty() {
        bail!("Git returned no worktree records");
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::parse_porcelain;
    use crate::WorktreeKind;

    #[test]
    fn parses_optional_porcelain_attributes() {
        let input = b"worktree /repo\0HEAD abc\0branch refs/heads/main\0locked reason\0\0worktree /bare\0bare\0prunable\0\0";
        let records = parse_porcelain(input).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, WorktreeKind::Branch("main".to_owned()));
        assert_eq!(records[0].locked.as_deref(), Some("reason"));
        assert_eq!(records[1].kind, WorktreeKind::Bare);
        assert_eq!(records[1].prunable.as_deref(), Some(""));
    }
}
