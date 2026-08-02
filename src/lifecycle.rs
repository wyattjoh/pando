use std::{
    env, fs,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Condition, Worktree, WorktreeKind,
    config::{EffectiveConfig, HookPhase},
    git::{self, Repository},
    hash,
    protocol::BytePath,
    render,
    setup::{self, HookOutcome},
    smart::approve_hooks,
    squash, trust, ui,
};

/// Stable, journal-aware merge state exposed to command adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Protocol facts are intentionally explicit, not state switches.
pub struct MergeContext {
    pub source_branch: String,
    pub target_branch: String,
    pub phase: MergePhase,
    pub policy: MergePolicy,
    pub source_commit: String,
    pub target_commit: String,
    pub topic_worktree: BytePath,
    pub primary_worktree: BytePath,
    /// The topic branch is checked out in the primary worktree itself, so the
    /// merge switches that worktree to the target instead of removing anything.
    pub in_place: bool,
    pub cleanup_pending: bool,
    pub journaled: bool,
    pub rebase_active: bool,
    /// The topic will be collapsed into one generated-message commit.
    pub squashes: bool,
    /// Commits between the target and the topic's `HEAD` at plan time. When a
    /// rebase is still pending this previews what the squash would collapse.
    pub squash_commits: usize,
    pub squash_generator_configured: bool,
    pub squash_generator_trusted: bool,
    pub pre_merge_hooks_trusted: bool,
    pub pre_remove_hooks_trusted: bool,
}

#[derive(Clone, Copy, Debug, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePhase {
    Planned,
    Rebase,
    Squash,
    Validation,
    Integration,
    Cleanup,
    Complete,
}

#[derive(Clone, Copy, Debug, JsonSchema, Serialize)]
pub struct MergePolicy {
    pub no_rebase: bool,
    pub no_remove: bool,
    pub no_squash: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum PreflightFailureKind {
    DuplicateTarget,
    PrimaryForbidden,
    ForceRequired,
    LifecycleActive,
    JournalInvalid,
    UnknownTarget,
    NothingToMerge,
    PolicyConflict,
    SquashGeneratorMissing,
    Dirty,
    NotFastForwardable,
    Blocked,
}
#[derive(Debug)]
pub struct PreflightFailure {
    pub kind: PreflightFailureKind,
    error: anyhow::Error,
}
impl std::fmt::Display for PreflightFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}
impl std::error::Error for PreflightFailure {}
impl From<anyhow::Error> for PreflightFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            kind: PreflightFailureKind::Blocked,
            error,
        }
    }
}
fn preflight(kind: PreflightFailureKind, message: impl Into<String>) -> PreflightFailure {
    PreflightFailure {
        kind,
        error: anyhow::anyhow!(message.into()),
    }
}
type PreflightResult<T> = std::result::Result<T, PreflightFailure>;

/// Read-only plan. It deliberately contains no journal path or identity.
#[derive(Debug)]
pub struct MergePlan {
    pub repository: Repository,
    pub context: MergeContext,
    pub config: EffectiveConfig,
    pub needs_rebase: bool,
    pub squash: squash::SquashPlan,
}

/// Performs all lifecycle checks without changing Git, trust, hooks, or the journal.
///
/// # Errors
/// Returns an error for an invalid or blocked lifecycle state.
#[allow(clippy::too_many_lines)]
pub fn plan_merge(no_rebase: bool, no_remove: bool, no_squash: bool) -> PreflightResult<MergePlan> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    let primary = repository
        .primary
        .as_ref()
        .context("cannot merge from a bare repository")?;
    let in_place = repository.current().path == *primary;
    let identity = git::worktree_identity(&repository.current().path)?;
    let journal = read_journal(&repository.common_dir, &identity)?;
    let rebase_active = git::rebase_in_progress(&repository.current().path)?;
    if let Some(state) = &journal {
        if state.version != 1
            || state.topic_path != repository.current().path
            || state.topic_identity != identity
        {
            return Err(anyhow::anyhow!(
                "lifecycle journal is unsupported or does not match the current topic worktree"
            )
            .into());
        }
        if state.no_rebase != no_rebase
            || state.no_remove != no_remove
            || state.no_squash != no_squash
        {
            return Err(preflight(
                PreflightFailureKind::PolicyConflict,
                "merge retry flags conflict with the journaled lifecycle policy; rerun with the original flags",
            ));
        }
    }
    if !rebase_active && git::is_dirty(&repository.current().path)? {
        return Err(preflight(
            PreflightFailureKind::Dirty,
            "the topic worktree has local changes; commit or discard them before merging",
        ));
    }
    let config = EffectiveConfig::load(&repository)?;
    let source = journal.as_ref().map_or_else(
        || git::current_branch(&repository).map(str::to_owned),
        |s| Ok(s.source_branch.clone()),
    )?;
    let target = journal.as_ref().map_or_else(
        || {
            git::resolve_target_branch(
                repository
                    .primary
                    .as_ref()
                    .context("repository has no primary worktree")?,
                config.target_branch.as_deref(),
            )
        },
        |s| Ok(s.target_branch.clone()),
    )?;
    git::validate_branch(primary, &target)?;
    let checked_out = primary_branch(&repository)?;
    if in_place {
        if journal.is_none() && checked_out == target {
            return Err(preflight(
                PreflightFailureKind::NothingToMerge,
                format!(
                    "the primary worktree is already on {target:?}; check out a topic branch before merging"
                ),
            ));
        }
    } else if checked_out != target {
        return Err(anyhow::anyhow!(
            "configured target branch {target:?} must be checked out in the primary worktree"
        )
        .into());
    }
    let source_commit = git::head_commit(&repository.current().path)?;
    let target_commit = git::branch_commit(primary, &target)?;
    let cleanup_pending = journal.as_ref().is_some_and(|s| s.cleanup_pending);
    let needs_rebase = !cleanup_pending
        && !rebase_active
        && !git::is_ancestor(&repository.current().path, &target, &source)?;
    if needs_rebase && no_rebase {
        return Err(preflight(
            PreflightFailureKind::NotFastForwardable,
            format!(
                "the topic is not fast-forwardable onto {target:?}; rerun without --no-rebase to rebase it"
            ),
        ));
    }
    // Squashing is off the table once the branch has already been collapsed,
    // and during cleanup the integration is behind us entirely.
    let squash_enabled =
        !no_squash && !cleanup_pending && !journal.as_ref().is_some_and(|state| state.squashed);
    let squash = squash::plan(
        &repository,
        &config,
        &target,
        squash_enabled,
        !rebase_active,
    )?;
    if squash.applicable && !squash.generator_configured {
        return Err(preflight(
            PreflightFailureKind::SquashGeneratorMissing,
            "no squash message generator is configured; set merge.generation.command or commit.generation.command, or rerun with --no-squash",
        ));
    }
    let phase = if cleanup_pending {
        MergePhase::Cleanup
    } else if rebase_active {
        MergePhase::Rebase
    } else if squash.applicable {
        MergePhase::Squash
    } else {
        MergePhase::Planned
    };
    let pre_merge_hooks_trusted = config.pre_merge.is_empty()
        || trust::is_trusted(&repository, HookPhase::PreMerge, &config.pre_merge)?;
    // An in-place merge removes nothing, so its pre-remove hooks never run.
    let pre_remove_hooks_trusted = if in_place {
        true
    } else {
        let remove_config =
            EffectiveConfig::load_for_worktree(&repository, &repository.current().path)?;
        remove_config.pre_remove.is_empty()
            || trust::is_trusted(&repository, HookPhase::PreRemove, &remove_config.pre_remove)?
    };
    let context = MergeContext {
        source_branch: source,
        target_branch: target,
        phase,
        policy: MergePolicy {
            no_rebase,
            no_remove,
            no_squash,
        },
        source_commit,
        target_commit,
        topic_worktree: BytePath::path(&repository.current().path),
        primary_worktree: BytePath::path(primary),
        in_place,
        cleanup_pending,
        journaled: journal.is_some(),
        rebase_active,
        squashes: squash.applicable,
        squash_commits: squash.commit_count,
        squash_generator_configured: squash.generator_configured,
        squash_generator_trusted: squash.generator_trusted,
        pre_merge_hooks_trusted,
        pre_remove_hooks_trusted,
    };
    Ok(MergePlan {
        repository,
        context,
        config,
        needs_rebase,
        squash,
    })
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // A journal records pinned policy facts, not state switches.
struct MergeJournal {
    version: u8,
    topic_path: PathBuf,
    topic_identity: PathBuf,
    source_branch: String,
    target_branch: String,
    no_rebase: bool,
    no_remove: bool,
    #[serde(default)]
    no_squash: bool,
    /// The topic has already been collapsed, so a retry must not squash again.
    #[serde(default)]
    squashed: bool,
    #[serde(default)]
    cleanup_pending: bool,
    #[serde(default)]
    validated_source: Option<String>,
    #[serde(default)]
    validated_target: Option<String>,
}

/// A fully validated, read-only removal plan shared by human and JSON adapters.
#[derive(Debug)]
pub struct RemovalPlan {
    pub repository: Repository,
    pub primary: PathBuf,
    pub current: PathBuf,
    pub targets: Vec<RemovalTarget>,
    pub force: bool,
}

/// One target and the configuration captured during removal preflight.
#[derive(Clone, Debug)]
pub struct RemovalTarget {
    pub worktree: Worktree,
    pub config: EffectiveConfig,
    pub stale_journal: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum MergeWorktreeOutcome {
    Retained,
    Removed,
    SwitchedInPlace,
}

/// Removes selected topic worktrees and emits a destination only if the current
/// worktree was removed.
///
/// # Errors
///
/// Returns an error when preflight, hook execution, or Git deletion fails.
pub fn remove(branches: &[String], force: bool) -> Result<()> {
    let plan = plan_remove(branches, force)?;
    for target in &plan.targets {
        approve_hooks(
            &plan.repository,
            HookPhase::PreRemove,
            &target.config.pre_remove,
        )?;
    }
    for target in &plan.targets {
        run_hooks(
            HookPhase::PreRemove,
            &target.config.pre_remove,
            &target.worktree.path,
        )?;
    }
    for target in &plan.targets {
        if let Some(path) = &target.stale_journal {
            fs::remove_file(path).with_context(|| {
                format!("failed to clear stale lifecycle journal {}", path.display())
            })?;
        }
        git::remove_worktree(&plan.primary, &target.worktree.path, force).with_context(|| {
            format!(
                "failed to remove worktree for branch {}",
                target.worktree.branch_label()
            )
        })?;
    }
    let removing_current = plan
        .targets
        .iter()
        .any(|target| target.worktree.path == plan.current);
    if removing_current {
        write_destination(&plan.primary)?;
    }
    let count = plan.targets.len();
    ui::finish(ui::success_style().apply_to(format!(
        "Removed {count} worktree{}; branches retained.",
        plural(count)
    )))
}

/// Prints a human-readable removal plan without mutation or approval.
///
/// # Errors
/// Returns an error when preflight or terminal rendering fails.
pub fn remove_dry_run(branches: &[String], force: bool) -> Result<()> {
    let plan = plan_remove(branches, force)?;
    for target in &plan.targets {
        ui::info(format!(
            "Would remove worktree for {} at {}; branch retained{}.",
            target.worktree.branch_label(),
            target.worktree.path.display(),
            if target.config.pre_remove.is_empty() {
                ""
            } else {
                "; pre-remove hooks require approval and would run"
            }
        ))?;
    }
    ui::finish(format!(
        "Removal plan ready for {} worktree{}.",
        plan.targets.len(),
        plural(plan.targets.len())
    ))
}

/// Prints a fully validated merge plan without writing journals, running hooks, or changing Git.
///
/// # Errors
/// Returns an error when merge preflight fails or output cannot be rendered.
pub fn merge_dry_run(no_rebase: bool, no_remove: bool, no_squash: bool) -> Result<()> {
    let plan = plan_merge(no_rebase, no_remove, no_squash)?;
    if plan.squash.applicable {
        ui::info(if plan.squash.commit_count == 0 {
            format!(
                "Would squash the topic into a single generated-message commit after the rebase onto {}.",
                plan.context.target_branch
            )
        } else {
            format!(
                "Would squash {} commits into a single generated-message commit.",
                plan.squash.commit_count
            )
        })?;
    }
    let follow_up = if plan.context.in_place {
        format!(
            " and switch the primary worktree to {}",
            plan.context.target_branch
        )
    } else if no_remove {
        " and retain the topic worktree".to_owned()
    } else {
        " and remove the topic worktree".to_owned()
    };
    ui::finish(format!(
        "Would merge {} into {}{follow_up}; no changes made.",
        plan.context.source_branch, plan.context.target_branch,
    ))
}

/// Integrates the current topic branch into the resolved target branch.
#[allow(clippy::too_many_lines)] // This is the explicit lifecycle state-machine boundary.
///
/// # Errors
///
/// Returns an error when merge preconditions, hooks, Git execution, or cleanup fails.
pub fn merge(no_rebase: bool, no_remove: bool, no_squash: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    let primary = repository
        .primary
        .as_ref()
        .context("cannot merge from a bare repository")?;
    let in_place = repository.current().path == *primary;
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
        if existing.version != 1 || existing.topic_path != repository.current().path {
            bail!("lifecycle journal is unsupported or does not match the current topic worktree");
        }
        if existing.topic_identity != identity {
            bail!(
                "a different lifecycle operation is recorded for this topic worktree; inspect its journal before retrying"
            );
        }
        if existing.no_rebase != no_rebase
            || existing.no_remove != no_remove
            || existing.no_squash != no_squash
        {
            bail!(
                "merge retry flags conflict with the journaled lifecycle policy; rerun with the original flags"
            );
        }
    }
    let target = match &journal {
        Some(state) => state.target_branch.clone(),
        None => git::resolve_target_branch(primary, config.target_branch.as_deref())
            .context("failed to resolve merge target")?,
    };
    git::validate_branch(primary, &target)?;
    let checked_out = primary_branch(&repository)?;
    if in_place {
        if journal.is_none() && checked_out == target {
            bail!(
                "the primary worktree is already on {target:?}; check out a topic branch before merging"
            );
        }
    } else if checked_out != target {
        bail!(
            "configured target branch {target:?} must be checked out in the primary worktree (currently {checked_out:?})"
        );
    }
    if let Some(state) = journal.as_ref().filter(|state| state.cleanup_pending) {
        return cleanup_merge(&repository, state);
    }
    // Refuse an impossible squash before the journal, the rebase, or any other
    // mutation. Discovering a missing or untrusted generator only after the
    // rebase has landed would leave work to recover for no reason.
    if !no_squash && !journal.as_ref().is_some_and(|state| state.squashed) {
        squash::ensure_ready(&repository, &config, &target, !rebase_active)?;
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
            no_squash,
            squashed: false,
            cleanup_pending: false,
            validated_source: None,
            validated_target: None,
        };
        write_journal(&repository.common_dir, &state)?;
        journal = Some(state);
    }

    if rebase_active {
        report(&ui::run_timed(
            true,
            "Continuing rebase...",
            "Continued rebase",
            "Failed to continue the rebase",
            |animated| git::rebase_continue(&repository.current().path, !animated),
        )?)?;
    }
    if !git::is_ancestor(&repository.current().path, &target, &source)? {
        if no_rebase {
            bail!(
                "the topic is not fast-forwardable onto {target:?}; rerun without --no-rebase to rebase it"
            );
        }
        report(&ui::run_timed(
            true,
            &format!("Rebasing onto {target}..."),
            &format!("Rebased onto {target}"),
            &format!("Failed to rebase onto {target}"),
            |animated| git::rebase_onto(&repository.current().path, &target, !animated),
        )?)?;
    }
    let mut state = journal.context("lifecycle journal was not recorded before integration")?;
    // Squash after the rebase so the collapse starts from the replayed history,
    // and before validation so the pre-merge hooks see the commit that actually
    // lands on the target.
    if !state.no_squash && !state.squashed {
        let plan = squash::plan(&repository, &config, &target, true, true)?;
        if plan.applicable {
            // Generate first and show the message, so the rail reports what is
            // about to be committed before the collapse rewrites history.
            let message = ui::run_timed(
                true,
                "Generating squash commit message...",
                "Generated squash commit message:",
                "Failed to generate the squash commit message",
                |_| squash::generate_message(&repository, &config, &target),
            )?;
            ui::step(render::commit_message(&message))?;
            ui::run_timed(
                true,
                &format!("Squashing {} commits...", plan.commit_count),
                "Squashed the topic into a single commit",
                "Failed to squash the topic",
                |_| squash::collapse(&repository, &target, &message),
            )?;
            state.squashed = true;
            write_journal(&repository.common_dir, &state)?;
        }
    }
    let refreshed = git::head_commit(&repository.current().path)?;
    let target_commit = git::branch_commit(primary, &target)?;
    if state.validated_source.as_deref() != Some(&refreshed)
        || state.validated_target.as_deref() != Some(&target_commit)
    {
        let config = EffectiveConfig::load(&repository)?;
        approve_hooks(&repository, HookPhase::PreMerge, &config.pre_merge)?;
        run_hooks(
            HookPhase::PreMerge,
            &config.pre_merge,
            &repository.current().path,
        )?;
        if git::is_dirty(&repository.current().path)? {
            bail!(
                "pre-merge hooks left the topic worktree dirty; restore cleanliness before retrying"
            );
        }
        if git::head_commit(&repository.current().path)? != refreshed {
            return merge(no_rebase, no_remove, no_squash);
        }
        state.validated_source = Some(refreshed.clone());
        state.validated_target = Some(target_commit);
        write_journal(&repository.common_dir, &state)?;
    }
    if !git::is_ancestor(&repository.current().path, &target, &refreshed)? {
        bail!("the target advanced during validation; rerun merge to revalidate the new candidate");
    }
    // In place, the target is not checked out anywhere yet; claim it in the
    // primary worktree so the fast-forward has somewhere to land.
    if in_place && primary_branch(&repository)? != target {
        report(&ui::run_timed(
            true,
            &format!("Switching to {target}..."),
            &format!("Switched to {target}"),
            &format!("Failed to switch to {target}"),
            |animated| git::switch_branch(primary, &target, !animated),
        )?)?;
    }
    report(&ui::run_timed(
        true,
        &format!("Merging into {target}..."),
        &format!("Merged into {target}"),
        &format!("Failed to merge into {target}"),
        |animated| git::merge_ff_only(primary, &source, !animated),
    )?)?;
    if in_place {
        remove_journal(&repository.common_dir, &state.topic_identity)?;
        return ui::finish(merge_summary(&state, MergeWorktreeOutcome::SwitchedInPlace));
    }
    if state.no_remove {
        remove_journal(&repository.common_dir, &state.topic_identity)?;
        return ui::finish(merge_summary(&state, MergeWorktreeOutcome::Retained));
    }
    state.cleanup_pending = true;
    write_journal(&repository.common_dir, &state)?;
    cleanup_merge(&repository, &state)
}

/// Renders a captured Git transcript inside the terminal UI rail.
///
/// Nothing is written when the operation streamed its own output to stderr or
/// had nothing to say, so a plain-stderr run never doubles up on Git's output.
fn report(transcript: &str) -> Result<()> {
    if transcript.trim().is_empty() {
        return Ok(());
    }
    ui::step(render::git_output(transcript.trim_end()))
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
    write_destination(primary)?;
    ui::finish(merge_summary(state, MergeWorktreeOutcome::Removed))
}

fn merge_summary(state: &MergeJournal, worktree_outcome: MergeWorktreeOutcome) -> String {
    let epilogue = match worktree_outcome {
        MergeWorktreeOutcome::Retained => "; worktree retained.".to_owned(),
        MergeWorktreeOutcome::Removed => "; worktree removed.".to_owned(),
        MergeWorktreeOutcome::SwitchedInPlace => format!(
            "; primary worktree now on {}, branch retained.",
            state.target_branch
        ),
    };
    format!(
        "{} {} {} {}{}",
        ui::success_style().apply_to(if state.squashed {
            "Squashed and merged"
        } else {
            "Merged"
        }),
        ui::worktree_data_style().apply_to(&state.source_branch),
        ui::success_style().apply_to("into"),
        ui::worktree_data_style().apply_to(&state.target_branch),
        ui::success_style().apply_to(epilogue),
    )
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Performs complete removal preflight without hooks, Git mutation, or journal cleanup.
///
/// # Errors
/// Returns an error for invalid repository state, targets, journals, or force policy.
pub fn plan_remove(branches: &[String], force: bool) -> PreflightResult<RemovalPlan> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    let primary = repository
        .primary
        .clone()
        .context("cannot remove a worktree from a bare repository")?;
    let worktrees = select_removal_targets(&repository, branches, force).map_err(|error| {
        error
            .downcast::<PreflightFailure>()
            .unwrap_or_else(PreflightFailure::from)
    })?;
    let mut targets = Vec::with_capacity(worktrees.len());
    for worktree in worktrees {
        let stale_journal = inspect_removal_state(&repository, &worktree).map_err(|error| {
            error
                .downcast::<PreflightFailure>()
                .unwrap_or_else(PreflightFailure::from)
        })?;
        let config = EffectiveConfig::load_for_worktree(&repository, &worktree.path)?;
        targets.push(RemovalTarget {
            worktree,
            config,
            stale_journal,
        });
    }
    targets.sort_by_key(|target| target.worktree.path == repository.current().path);
    let current = repository.current().path.clone();
    Ok(RemovalPlan {
        repository,
        primary,
        current,
        targets,
        force,
    })
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
        return Err(preflight(
            PreflightFailureKind::DuplicateTarget,
            "duplicate branch arguments are not allowed",
        )
        .into());
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
            .ok_or_else(|| {
                preflight(
                    PreflightFailureKind::UnknownTarget,
                    format!("no registered topic worktree is attached to branch {branch:?}"),
                )
            })?;
        if Some(&target.path) == repository.primary.as_ref() {
            return Err(preflight(
                PreflightFailureKind::PrimaryForbidden,
                "the primary worktree cannot be removed",
            )
            .into());
        }
        check_removable(target, force)?;
        targets.push(target.clone());
    }
    Ok(targets)
}

fn inspect_removal_state(repository: &Repository, target: &Worktree) -> Result<Option<PathBuf>> {
    let identity = git::worktree_identity(&target.path)?;
    let Some(state) = read_journal(&repository.common_dir, &identity)? else {
        return Ok(None);
    };
    if state.version != 1 || state.topic_identity != identity || state.topic_path != target.path {
        return Err(preflight(
            PreflightFailureKind::JournalInvalid,
            format!(
                "lifecycle journal for {} is malformed or does not match its worktree identity",
                target.path.display()
            ),
        )
        .into());
    }
    if state.cleanup_pending || git::rebase_in_progress(&target.path)? {
        return Err(preflight(PreflightFailureKind::LifecycleActive, format!("worktree {} has an active lifecycle operation; rerun pando merge to recover it before removal", target.path.display())).into());
    }
    Ok(Some(journal_path(&repository.common_dir, &identity)))
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
    if !force && git::is_dirty(&target.path)? {
        return Err(preflight(PreflightFailureKind::ForceRequired, format!("worktree {} has local changes; rerun with --force to discard only worktree contents", target.path.display())).into());
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
        .join("pando-state/lifecycle")
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
