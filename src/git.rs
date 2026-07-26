use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};

use crate::{Condition, Worktree, WorktreeKind};

/// Discovers and enriches the worktrees for the repository containing `cwd`.
///
/// # Errors
///
/// Returns an error when Git cannot be started, Git rejects the repository
/// context, or its stable porcelain output cannot be parsed.
pub fn discover(cwd: &Path) -> Result<Vec<Worktree>> {
    let output = run_git(cwd, &["worktree", "list", "--porcelain", "-z"])
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Condition::Missing;
        }
        Ok(_) | Err(_) => return Condition::Inaccessible,
    }

    match run_git(
        &worktree.path,
        &["status", "--porcelain", "--untracked-files=normal"],
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

fn run_git(cwd: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git").args(args).current_dir(cwd).output()
}

fn ensure_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        bail!("{operation} failed with {}", output.status);
    }
    bail!("{operation} failed: {detail}");
}

/// Parses NUL-delimited `git worktree list --porcelain -z` output.
///
/// # Errors
///
/// Returns an error when attributes appear outside a worktree record or no
/// worktree records are present.
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
    use crate::WorktreeKind;

    use super::parse_porcelain;

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
