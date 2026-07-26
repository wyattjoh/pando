use std::{
    fs, io,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    config::{HookPhase, HookStep},
    hash, trust, ui,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookOutcome {
    Success,
    Failed(i32),
    Interrupted,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IncompleteRecord {
    branch: String,
    destination: String,
}

/// A pre-creation journal entry that is promoted to the worktree's stable
/// administrative identity immediately after Git creates it.
#[derive(Debug)]
pub struct PendingRecord {
    path: PathBuf,
}

#[must_use]
pub fn marker_path(common_dir: &Path, worktree_identity: &Path) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(worktree_identity.as_os_str().as_encoded_bytes());
    common_dir
        .join("worktrees-state/incomplete")
        .join(format!("{}.json", hash::encode_hex(&digest.finalize())))
}

fn pending_path(common_dir: &Path, branch: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(branch.as_bytes());
    common_dir
        .join("worktrees-state/pending")
        .join(format!("{}.json", hash::encode_hex(&digest.finalize())))
}

/// Journals setup intent before Git mutation so a later marker-write failure
/// cannot make a created worktree appear complete.
///
/// # Errors
///
/// Returns an error when the journal cannot be encoded or written.
pub fn prepare(common_dir: &Path, branch: &str, destination: &Path) -> Result<PendingRecord> {
    let path = pending_path(common_dir, branch);
    let record = IncompleteRecord {
        branch: branch.to_owned(),
        destination: destination.to_string_lossy().into_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&record)?;
    trust::write_atomic(&path, &bytes).with_context(|| {
        format!(
            "failed to prepare incomplete setup state for {}",
            destination.display()
        )
    })?;
    Ok(PendingRecord { path })
}

impl PendingRecord {
    /// Promotes this journal entry to the stable Git worktree identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the final marker directory or atomic rename fails.
    pub fn commit(self, common_dir: &Path, worktree_identity: &Path) -> Result<()> {
        let destination = marker_path(common_dir, worktree_identity);
        let parent = destination
            .parent()
            .context("incomplete setup marker has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::rename(&self.path, &destination).with_context(|| {
            format!(
                "failed to promote incomplete setup state from {} to {}",
                self.path.display(),
                destination.display()
            )
        })
    }

    /// Removes this journal entry after Git creation fails.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry cannot be removed.
    pub fn cancel(self) -> Result<()> {
        remove_record(&self.path)
    }
}

/// Reports whether setup is recorded as incomplete for a worktree.
///
/// # Errors
///
/// Returns an error when either stable or pending state cannot be inspected safely.
pub fn is_incomplete(
    common_dir: &Path,
    worktree_identity: &Path,
    branch: Option<&str>,
) -> Result<bool> {
    if inspect_record(&marker_path(common_dir, worktree_identity))? {
        return Ok(true);
    }
    branch.map_or(Ok(false), |branch| {
        inspect_record(&pending_path(common_dir, branch))
    })
}

/// Removes stable and pending incomplete setup records idempotently.
///
/// # Errors
///
/// Returns an error when an existing record cannot be removed.
pub fn clear(common_dir: &Path, worktree_identity: &Path, branch: Option<&str>) -> Result<()> {
    remove_record(&marker_path(common_dir, worktree_identity))?;
    branch.map_or(Ok(()), |branch| {
        remove_record(&pending_path(common_dir, branch))
    })
}

fn inspect_record(path: &Path) -> Result<bool> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read incomplete setup record {}", path.display())
            });
        }
    };
    serde_json::from_slice::<IncompleteRecord>(&bytes)
        .with_context(|| format!("failed to parse incomplete setup record {}", path.display()))?;
    Ok(true)
}

fn remove_record(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to remove incomplete setup record {}",
                path.display()
            )
        }),
    }
}

/// Runs phase steps sequentially from the destination.
///
/// # Errors
///
/// Returns an error when a command cannot start, stream output, or be awaited.
pub fn run_steps(phase: HookPhase, steps: &[HookStep], destination: &Path) -> Result<HookOutcome> {
    for (index, step) in steps.iter().enumerate() {
        ui::step(format!(
            "Running {} {}:\n{}",
            phase.key(),
            step.label(index),
            step.command
        ))?;
        let hook_stdout = fs::OpenOptions::new()
            .write(true)
            .open("/dev/stderr")
            .with_context(|| format!("failed to open stderr for {} output", phase.key()))?;
        let status = Command::new("/bin/sh")
            .args(["-c", &step.command])
            .current_dir(destination)
            .stdin(Stdio::inherit())
            .stdout(Stdio::from(hook_stdout))
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("failed to run {} {}", phase.key(), step.label(index)))?;
        if status.success() {
            continue;
        }
        if status
            .signal()
            .is_some_and(|signal| matches!(signal, 2 | 3 | 15))
        {
            return Ok(HookOutcome::Interrupted);
        }
        return Ok(HookOutcome::Failed(status.code().unwrap_or(1)));
    }
    Ok(HookOutcome::Success)
}
