use std::{
    env, fs,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Condition, Worktree, WorktreeKind,
    config::{EffectiveConfig, HookPhase},
    git::{self, Repository},
    hash,
    setup::{self, HookOutcome},
    smart::approve_hooks,
    trust,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MergeJournal {
    version: u8,
    topic_path: PathBuf,
    topic_identity: PathBuf,
    source_branch: String,
    target_branch: String,
    no_rebase: bool,
    no_remove: bool,
    #[serde(default)]
    cleanup_pending: bool,
}

/// Removes selected topic worktrees and emits a destination only if the current
/// worktree was removed.
///
/// # Errors
///
/// Returns an error when preflight, hook execution, or Git deletion fails.
pub fn remove(branches: &[String], force: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    let primary = repository
        .primary
        .as_ref()
        .context("cannot remove a worktree from a bare repository")?;
    let targets = select_removal_targets(&repository, branches, force)?;
    let mut plans = Vec::with_capacity(targets.len());
    for target in &targets {
        let config = EffectiveConfig::load_for_worktree(&repository, &target.path)?;
        approve_hooks(&repository, HookPhase::PreRemove, &config.pre_remove)?;
        plans.push(config);
    }
    for (target, config) in targets.iter().zip(&plans) {
        run_hooks(HookPhase::PreRemove, &config.pre_remove, &target.path)?;
    }
    for target in &targets {
        check_removable(target, force)?;
    }

    let current = repository.current().path.clone();
    let mut ordered: Vec<_> = targets.iter().collect();
    ordered.sort_by_key(|target| target.path == current);
    let removing_current = targets.iter().any(|target| target.path == current);
    for target in ordered {
        git::remove_worktree(primary, &target.path, force).with_context(|| {
            format!(
                "failed to remove worktree for branch {}",
                target.branch_label()
            )
        })?;
    }
    if removing_current {
        write_destination(primary)?;
    }
    Ok(())
}

/// Integrates the current topic branch into the configured target branch.
///
/// # Errors
///
/// Returns an error when merge preconditions, hooks, Git execution, or cleanup fails.
pub fn merge(no_rebase: bool, no_remove: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    let primary = repository
        .primary
        .as_ref()
        .context("cannot merge from a bare repository")?;
    if repository.current().path == *primary {
        bail!("merge must run from a topic worktree, not the primary worktree");
    }
    let identity = git::worktree_identity(&repository.current().path)?;
    let mut journal = read_journal(&repository.common_dir, &identity)?;
    let rebase_active = git::rebase_in_progress(&repository.current().path)?;
    let source = match &journal {
        Some(state) => state.source_branch.clone(),
        None => git::current_branch(&repository)?.to_owned(),
    };
    if !rebase_active && git::is_dirty(&repository.current().path)? {
        // TODO: support an explicit auto-commit workflow in a future lifecycle release.
        bail!("the topic worktree has local changes; commit or discard them before merging");
    }
    let config = EffectiveConfig::load(&repository)?;
    if let Some(existing) = &journal {
        if existing.topic_identity != identity {
            bail!(
                "a different lifecycle operation is recorded for this topic worktree; inspect its journal before retrying"
            );
        }
        if existing.no_rebase != no_rebase || existing.no_remove != no_remove {
            bail!(
                "merge retry flags conflict with the journaled lifecycle policy; rerun with the original flags"
            );
        }
    }
    let target = match &journal {
        Some(state) => state.target_branch.clone(),
        None => config
            .require_target_branch()
            .context("failed to resolve merge target")?
            .to_owned(),
    };
    git::validate_branch(primary, &target)?;
    let primary_branch = primary_branch(&repository)?;
    if primary_branch != target {
        bail!(
            "configured target branch {target:?} must be checked out in the primary worktree (currently {primary_branch:?})"
        );
    }
    if let Some(state) = journal.as_ref().filter(|state| state.cleanup_pending) {
        return cleanup_merge(&repository, state);
    }
    if journal.is_none() {
        let state = MergeJournal {
            version: 1,
            topic_path: repository.current().path.clone(),
            topic_identity: identity,
            source_branch: source.clone(),
            target_branch: target.clone(),
            no_rebase,
            no_remove,
            cleanup_pending: false,
        };
        write_journal(&repository.common_dir, &state)?;
        journal = Some(state);
    }

    if rebase_active {
        git::rebase_continue(&repository.current().path)?;
    }
    if !git::is_ancestor(&repository.current().path, &target, &source)? {
        if no_rebase {
            bail!(
                "the topic is not fast-forwardable onto {target:?}; rerun without --no-rebase to rebase it"
            );
        }
        git::rebase_onto(&repository.current().path, &target)?;
    }
    let refreshed = git::head_commit(&repository.current().path)?;
    let config = EffectiveConfig::load(&repository)?;
    approve_hooks(&repository, HookPhase::PreMerge, &config.pre_merge)?;
    run_hooks(
        HookPhase::PreMerge,
        &config.pre_merge,
        &repository.current().path,
    )?;
    if git::is_dirty(&repository.current().path)? {
        bail!("pre-merge hooks left the topic worktree dirty; restore cleanliness before retrying");
    }
    if !git::is_ancestor(&repository.current().path, &target, &refreshed)? {
        bail!("the target advanced during validation; rerun merge to revalidate the new candidate");
    }
    git::merge_ff_only(primary, &source)?;
    let mut state = journal.context("lifecycle journal was not recorded before integration")?;
    if state.no_remove {
        remove_journal(&repository.common_dir, &state.topic_identity)?;
        return Ok(());
    }
    state.cleanup_pending = true;
    write_journal(&repository.common_dir, &state)?;
    cleanup_merge(&repository, &state)
}

fn cleanup_merge(repository: &Repository, state: &MergeJournal) -> Result<()> {
    let config = EffectiveConfig::load_for_worktree(repository, &state.topic_path)?;
    approve_hooks(repository, HookPhase::PreRemove, &config.pre_remove)?;
    run_hooks(HookPhase::PreRemove, &config.pre_remove, &state.topic_path)?;
    let worktree = repository
        .worktrees
        .iter()
        .find(|worktree| worktree.path == state.topic_path)
        .context("journaled topic worktree is no longer registered")?;
    check_removable(worktree, false)?;
    let primary = repository
        .primary
        .as_ref()
        .context("cleanup requires a primary worktree")?;
    git::remove_worktree(primary, &state.topic_path, false)?;
    remove_journal(&repository.common_dir, &state.topic_identity)?;
    write_destination(primary)
}

fn select_removal_targets(
    repository: &Repository,
    branches: &[String],
    force: bool,
) -> Result<Vec<Worktree>> {
    let mut names = if branches.is_empty() {
        vec![git::current_branch(repository)?.to_owned()]
    } else {
        branches.to_vec()
    };
    names.sort();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("duplicate branch arguments are not allowed");
    }
    let mut targets = Vec::with_capacity(branches.len().max(1));
    for branch in if branches.is_empty() {
        vec![git::current_branch(repository)?.to_owned()]
    } else {
        branches.to_vec()
    } {
        let target = repository
            .worktrees
            .iter()
            .find(
                |worktree| matches!(&worktree.kind, WorktreeKind::Branch(name) if name == &branch),
            )
            .with_context(|| {
                format!("no registered topic worktree is attached to branch {branch:?}")
            })?;
        if Some(&target.path) == repository.primary.as_ref() {
            bail!("the primary worktree cannot be removed");
        }
        check_removable(target, force)?;
        targets.push(target.clone());
    }
    Ok(targets)
}

fn check_removable(target: &Worktree, force: bool) -> Result<()> {
    if target.locked.is_some()
        || target.prunable.is_some()
        || matches!(
            target.kind,
            WorktreeKind::Detached | WorktreeKind::Bare | WorktreeKind::Unknown
        )
    {
        bail!(
            "worktree {} is not removable: {}",
            target.path.display(),
            target.state_label()
        );
    }
    if !force && target.condition == Condition::Dirty {
        bail!(
            "worktree {} has local changes; rerun with --force to discard only worktree contents",
            target.path.display()
        );
    }
    if !matches!(target.condition, Condition::Clean | Condition::Dirty) {
        bail!(
            "worktree {} is not removable: {}",
            target.path.display(),
            target.state_label()
        );
    }
    Ok(())
}

fn run_hooks(phase: HookPhase, steps: &[crate::config::HookStep], path: &Path) -> Result<()> {
    match setup::run_steps(phase, steps, path)? {
        HookOutcome::Success => Ok(()),
        HookOutcome::Failed(status) => bail!("{} hook failed with status {status}", phase.key()),
        HookOutcome::Interrupted => bail!("{} hook was interrupted", phase.key()),
    }
}

fn primary_branch(repository: &Repository) -> Result<String> {
    let primary = repository
        .primary
        .as_ref()
        .context("repository has no primary worktree")?;
    repository
        .worktrees
        .iter()
        .find(|worktree| worktree.path == *primary)
        .and_then(|worktree| match &worktree.kind {
            WorktreeKind::Branch(branch) => Some(branch.clone()),
            _ => None,
        })
        .context("the primary worktree is not on a named branch")
}

fn journal_path(common_dir: &Path, identity: &Path) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(identity.as_os_str().as_bytes());
    common_dir
        .join("worktrees-state/lifecycle")
        .join(format!("{}.json", hash::encode_hex(&digest.finalize())))
}
fn read_journal(common_dir: &Path, identity: &Path) -> Result<Option<MergeJournal>> {
    let path = journal_path(common_dir, identity);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("failed to parse lifecycle journal {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read lifecycle journal {}", path.display())),
    }
}
fn write_journal(common_dir: &Path, state: &MergeJournal) -> Result<()> {
    let path = journal_path(common_dir, &state.topic_identity);
    trust::write_atomic(&path, &serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("failed to write lifecycle journal {}", path.display()))
}
fn remove_journal(common_dir: &Path, identity: &Path) -> Result<()> {
    let path = journal_path(common_dir, identity);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to remove lifecycle journal {}", path.display())),
    }
}
fn write_destination(destination: &Path) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(destination.as_os_str().as_bytes())
        .context("failed to write primary-worktree destination")?;
    stdout
        .write_all(b"\n")
        .context("failed to terminate primary-worktree destination")?;
    Ok(())
}
