use std::{
    env, fs,
    io::{self, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) mod journaled_merge;

use crate::{
    Condition, Worktree, WorktreeKind,
    branch::Snapshot,
    config::{EffectiveConfig, HookPhase},
    git::{
        self, HistoryObservation, LifecycleMutation, LifecycleOutput, Repository,
        RepositoryObservation,
    },
    hash,
    hook::{self, HookOutcome},
    hook_approval,
    protocol::{
        self, BytePath, Diagnostic, Effect, ErrorBody, MutationClass, RecoveryAction,
        RecoveryInvocation,
    },
    render, squash, trust, ui,
};

/// Stable, journal-aware merge state exposed to command adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Protocol facts are intentionally explicit, not state switches.
pub struct MergeContext {
    /// Stable Git worktree identity used to bind a plan to its journal.
    pub repository_identity: BytePath,
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

impl MergePolicy {
    #[must_use]
    pub const fn new(no_rebase: bool, no_remove: bool, no_squash: bool) -> Self {
        Self {
            no_rebase,
            no_remove,
            no_squash,
        }
    }

    #[must_use]
    pub const fn removes_topic(self, in_place: bool) -> bool {
        !self.no_remove && !in_place
    }
}

/// Strict version 1 input for the merge lifecycle.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Each flag is an independent policy opt-out.
pub struct MergeInput {
    #[serde(default)]
    pub no_rebase: bool,
    #[serde(default)]
    pub no_remove: bool,
    #[serde(default)]
    pub no_squash: bool,
    #[serde(default)]
    pub dry_run: bool,
}

impl MergeInput {
    #[must_use]
    pub const fn policy(&self) -> MergePolicy {
        MergePolicy::new(self.no_rebase, self.no_remove, self.no_squash)
    }
}

/// Public result shared by human and JSON merge adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MergeResult {
    DryRun {
        plan: String,
        policy: MergePolicy,
        ready: bool,
        approval_required: bool,
    },
    InPlace {
        destination: Option<BytePath>,
    },
    Removed {
        destination: Option<BytePath>,
    },
    Retained {
        destination: Option<BytePath>,
    },
}

/// Stable merge failure rendered by protocol adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct MergeError {
    pub code: String,
    pub message: String,
}

impl From<MergeError> for ErrorBody {
    fn from(error: MergeError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

/// One exact hook command awaiting merge approval.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct MergeApprovalCommand {
    pub name: Option<String>,
    pub command: String,
}

/// Hook trust facts preserved when a merge is blocked before mutation.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct MergeApprovalContext {
    pub phase: String,
    pub commands: Vec<MergeApprovalCommand>,
    pub repository: String,
    pub identity: String,
}

/// Adapter-neutral context for every merge outcome shape in protocol version 1.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(untagged)]
pub enum MergeOutcomeContext {
    Unavailable {},
    Lifecycle(MergeContext),
    Approval {
        #[serde(flatten)]
        lifecycle: MergeContext,
        approval: MergeApprovalContext,
    },
    Completed {
        initial: MergeContext,
        phase: MergePhase,
    },
}

/// Complete command-owned merge outcome before presentation adaptation.
#[derive(Debug)]
pub struct MergeOutcome {
    pub result: std::result::Result<MergeResult, MergeError>,
    pub context: MergeOutcomeContext,
    pub effects: Vec<Effect>,
    pub diagnostics: Vec<Diagnostic>,
    pub recovery: Vec<RecoveryAction<protocol::Request<MergeInput>>>,
}

/// Stable version 1 merge protocol catalogs owned by the merge command.
pub const MERGE_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
    "merge.primary_forbidden",
    "merge.dirty",
    "merge.not_fast_forwardable",
    "merge.squash_generator_missing",
    "merge.squash_approval_required",
    "merge.policy_conflict",
    "merge.hook_approval_required",
    "merge.nothing_to_merge",
    "merge.stale_plan",
    "merge.rebase_conflict",
    "merge.validation_failed",
    "merge.cleanup_failed",
    "merge.remove_failed",
    "merge.journal_failed",
    "merge.execution_failed",
    "merge.blocked",
    "trust.read_failed",
];
pub const MERGE_ACTIONS: &[&str] = &[
    "journal",
    "rebase",
    "squash",
    "pre_merge_hooks",
    "fast_forward_merge",
    "pre_remove_hooks",
    "remove_worktree",
    "destination",
    "trust.review",
    "merge.retry",
    "trust.review_squash_generator",
];

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

/// Converts a merge preflight blocker to its stable command outcome.
#[must_use]
pub fn merge_preflight_outcome(error: &PreflightFailure) -> MergeOutcome {
    let code = match error.kind {
        PreflightFailureKind::PolicyConflict => "merge.policy_conflict",
        PreflightFailureKind::Dirty => "merge.dirty",
        PreflightFailureKind::NotFastForwardable => "merge.not_fast_forwardable",
        PreflightFailureKind::NothingToMerge => "merge.nothing_to_merge",
        PreflightFailureKind::SquashGeneratorMissing => "merge.squash_generator_missing",
        _ => "merge.blocked",
    };
    MergeOutcome {
        result: Err(MergeError {
            code: code.into(),
            message: error.to_string(),
        }),
        context: MergeOutcomeContext::Unavailable {},
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
    }
}

type PreflightResult<T> = std::result::Result<T, PreflightFailure>;

/// Validated, read-only state held behind the journaled merge preparation seam.
#[derive(Debug)]
struct MergePlan {
    pub repository: Repository,
    pub context: MergeContext,
    pub config: EffectiveConfig,
    pub needs_rebase: bool,
    pub(crate) squash: squash::Assessment,
    resuming_squash: bool,
    /// Ordered lifecycle effects. Planning never attempts or completes one.
    pub effects: Vec<Effect>,
}

fn output_for(animated: bool) -> LifecycleOutput {
    if animated {
        LifecycleOutput::Captured
    } else {
        LifecycleOutput::Displayed
    }
}

/// One diagnostic produced by a lifecycle phase.
#[derive(Clone, Debug)]
pub struct MergeDiagnostic {
    pub phase: &'static str,
    pub stream: &'static str,
    pub content: Vec<u8>,
    pub original_size: usize,
    pub truncated: bool,
}

/// Typed failure returned at the plan-to-execution seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeExecutionFailureKind {
    StalePlan,
    Journal,
    Rebase,
    Squash,
    Validation,
    Integration,
    Cleanup,
    Removal,
    JournalCleanup,
}

/// Result of executing a validated merge plan.
#[derive(Debug)]
pub struct MergeExecutionOutcome {
    pub context: MergeContext,
    pub effects: Vec<Effect>,
    pub diagnostics: Vec<MergeDiagnostic>,
    pub destination: Option<BytePath>,
    pub failure: Option<(MergeExecutionFailureKind, String)>,
}

impl MergeExecutionOutcome {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.failure.is_none()
    }
}

fn merge_failure_code(kind: MergeExecutionFailureKind) -> &'static str {
    match kind {
        MergeExecutionFailureKind::StalePlan => "merge.stale_plan",
        MergeExecutionFailureKind::Rebase => "merge.rebase_conflict",
        MergeExecutionFailureKind::Squash | MergeExecutionFailureKind::Integration => {
            "merge.execution_failed"
        }
        MergeExecutionFailureKind::Validation => "merge.validation_failed",
        MergeExecutionFailureKind::Cleanup => "merge.cleanup_failed",
        MergeExecutionFailureKind::Removal => "merge.remove_failed",
        MergeExecutionFailureKind::Journal | MergeExecutionFailureKind::JournalCleanup => {
            "merge.journal_failed"
        }
    }
}

fn merge_diagnostics(diagnostics: Vec<MergeDiagnostic>) -> Vec<Diagnostic> {
    diagnostics
        .into_iter()
        .filter(|diagnostic| !diagnostic.content.is_empty())
        .map(|diagnostic| Diagnostic {
            source: diagnostic.phase.into(),
            stream: diagnostic.stream.into(),
            content: String::from_utf8_lossy(&diagnostic.content).into_owned(),
            original_size: diagnostic.original_size,
            truncated: diagnostic.truncated,
        })
        .collect()
}

fn merge_approval_context(candidate: &hook_approval::Candidate) -> MergeApprovalContext {
    MergeApprovalContext {
        phase: candidate.phase().key().into(),
        commands: candidate
            .commands()
            .iter()
            .map(|step| MergeApprovalCommand {
                name: step.name.clone(),
                command: step.command.clone(),
            })
            .collect(),
        repository: candidate.repository().into(),
        identity: candidate.identity().into(),
    }
}

/// Executes one structured merge request through read-only preparation and the
/// opaque journaled merge authority.
#[must_use]
pub(crate) fn execute_merge_request(input: &MergeInput) -> MergeOutcome {
    match journaled_merge::prepare(journaled_merge::MergeRequest::ordinary(input)) {
        journaled_merge::Preparation::Ready(prepared) => {
            prepared.run(&journaled_merge::MergeExecutionOutput::Captured)
        }
        journaled_merge::Preparation::ApprovalRequired(pending) => pending.into_outcome(),
        journaled_merge::Preparation::Complete(outcome) => outcome,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)] // The outcome records every version 1 lifecycle contract in one seam.
fn run_prepared_merge(
    plan: &MergePlan,
    input: &MergeInput,
    changes: journaled_merge::ChangePolicy,
    output: &journaled_merge::MergeExecutionOutput,
) -> MergeOutcome {
    let execution = execute_merge(plan, changes, output);
    let diagnostics = merge_diagnostics(execution.diagnostics);
    if let Some((kind, message)) = execution.failure {
        let working_directory = execution
            .destination
            .unwrap_or_else(|| plan.context.topic_worktree.clone());
        return MergeOutcome {
            result: Err(MergeError {
                code: merge_failure_code(kind).into(),
                message,
            }),
            context: MergeOutcomeContext::Lifecycle(execution.context),
            effects: execution.effects,
            diagnostics,
            recovery: vec![RecoveryAction {
                action: "merge.retry".into(),
                description: "Resolve the reported blocker and retry the journaled lifecycle with its pinned policy".into(),
                mutation: MutationClass::Repository,
                requires_human_approval: false,
                invocation: RecoveryInvocation {
                    argv: vec!["pando".into(), "merge".into(), "--input-output".into(), "json".into()],
                    stdin: Some(protocol::Request {
                        schema_version: 1,
                        request_id: None,
                        input: input.clone(),
                    }),
                    working_directory: Some(working_directory),
                },
            }],
        };
    }
    let result = if plan.context.in_place {
        MergeResult::InPlace {
            destination: execution.destination,
        }
    } else if input.no_remove {
        MergeResult::Retained {
            destination: execution.destination,
        }
    } else {
        MergeResult::Removed {
            destination: execution.destination,
        }
    };
    MergeOutcome {
        result: Ok(result),
        context: MergeOutcomeContext::Completed {
            initial: plan.context.clone(),
            phase: execution.context.phase,
        },
        effects: execution.effects,
        diagnostics,
        recovery: Vec::new(),
    }
}

fn merge_trust_recovery(
    plan: &MergePlan,
    squash_blocked: bool,
) -> RecoveryAction<protocol::Request<MergeInput>> {
    RecoveryAction {
        action: if squash_blocked {
            "trust.review_squash_generator"
        } else {
            "trust.review"
        }
        .into(),
        description: if squash_blocked {
            "Review and explicitly trust the shared squash message generator before retrying, or retry with no_squash"
        } else {
            "Review and explicitly trust the configured lifecycle hooks before retrying"
        }
        .into(),
        mutation: MutationClass::Trust,
        requires_human_approval: true,
        invocation: RecoveryInvocation {
            argv: if squash_blocked {
                vec!["pando".into(), "trust".into(), "merge-approve".into()]
            } else {
                vec!["pando".into(), "trust".into(), "show".into()]
            },
            stdin: None,
            working_directory: Some(plan.context.topic_worktree.clone()),
        },
    }
}

/// Performs all lifecycle checks without changing Git, trust, hooks, or the journal.
///
/// # Errors
/// Returns an error for an invalid or blocked lifecycle state.
#[allow(clippy::too_many_lines)]
fn plan_merge(
    policy: MergePolicy,
    changes: journaled_merge::ChangePolicy,
) -> PreflightResult<MergePlan> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = RepositoryObservation::new(&cwd).repository()?;
    let primary = repository
        .primary
        .as_ref()
        .context("cannot merge from a bare repository")?;
    let in_place = repository.current().path == *primary;
    let current_history = HistoryObservation::new(&repository.current().path);
    let identity = RepositoryObservation::new(&repository.current().path).worktree_identity()?;
    let journal = read_journal(&repository.common_dir, &identity)?;
    let rebase_active = LifecycleMutation::new(&repository.current().path).rebase_in_progress()?;
    if rebase_active && journal.is_none() {
        return Err(preflight(
            PreflightFailureKind::LifecycleActive,
            "an unrelated Git rebase is already in progress; finish or abort it before merging",
        ));
    }
    if let Some(state) = &journal {
        if state.topic_path != repository.current().path || state.topic_identity != identity {
            return Err(anyhow::anyhow!(
                "lifecycle journal is unsupported or does not match the current topic worktree"
            )
            .into());
        }
        if state.no_rebase != policy.no_rebase
            || state.no_remove != policy.no_remove
            || state.no_squash != policy.no_squash
            || state.yolo_stage_all != matches!(changes, journaled_merge::ChangePolicy::IncludeAll)
        {
            return Err(preflight(
                PreflightFailureKind::PolicyConflict,
                "merge retry flags conflict with the journaled lifecycle policy; rerun with the original flags",
            ));
        }
    }
    let dirty = !rebase_active && current_history.status()?.is_dirty();
    if journal.is_none() && matches!(changes, journaled_merge::ChangePolicy::IncludeAll) && !dirty {
        return Err(preflight(
            PreflightFailureKind::NothingToMerge,
            "nothing to commit",
        ));
    }
    if dirty
        && journal
            .as_ref()
            .is_none_or(|state| !state.squash.prepared())
        && matches!(changes, journaled_merge::ChangePolicy::RequireClean)
    {
        return Err(preflight(
            PreflightFailureKind::Dirty,
            "the topic worktree has local changes; commit or discard them before merging",
        ));
    }
    let config = EffectiveConfig::load(&repository)?;
    let source = journal.as_ref().map_or_else(
        || repository.current_branch().map(str::to_owned),
        |s| Ok(s.source_branch.clone()),
    )?;
    let snapshot = Snapshot::observe(&repository)?;
    let merge_target = snapshot.merge_target(
        journal
            .as_ref()
            .map(|state| state.target_branch.as_str())
            .or(config.target_branch.as_deref()),
    )?;
    let target = merge_target.branch;
    snapshot.validate(&target)?;
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
    let source_commit = merge_target.source_commit;
    let target_commit = merge_target.target_commit;
    let cleanup_pending = journal.as_ref().is_some_and(|s| s.cleanup_pending);
    let needs_rebase =
        !cleanup_pending && !rebase_active && !current_history.is_ancestor(&target, &source)?;
    if needs_rebase && policy.no_rebase {
        return Err(preflight(
            PreflightFailureKind::NotFastForwardable,
            format!(
                "the topic is not fast-forwardable onto {target:?}; rerun without --no-rebase to rebase it"
            ),
        ));
    }
    // Squashing is off the table once the branch has already been collapsed,
    // and during cleanup the integration is behind us entirely.
    let squash_enabled = !policy.no_squash
        && !cleanup_pending
        && journal
            .as_ref()
            .is_none_or(|state| state.squash.not_started());
    let squash = squash::assess(squash::AssessRequest {
        repository: &repository,
        config: &config,
        target: &target,
        enabled: squash_enabled,
        final_history: !rebase_active,
        include_staged: matches!(changes, journaled_merge::ChangePolicy::IncludeAll),
    })?;
    if squash.applicable() && !squash.generator_configured() {
        return Err(preflight(
            PreflightFailureKind::SquashGeneratorMissing,
            "no squash message generator is configured; set merge.generation.command or commit.generation.command, or rerun with --no-squash",
        ));
    }
    let resuming_squash = journal
        .as_ref()
        .is_some_and(|state| state.squash.prepared());
    let phase = if cleanup_pending {
        MergePhase::Cleanup
    } else if rebase_active {
        MergePhase::Rebase
    } else if squash.applicable() || resuming_squash {
        MergePhase::Squash
    } else {
        MergePhase::Planned
    };
    let pre_merge_hooks_trusted = matches!(
        hook_approval::evaluate(&repository, HookPhase::PreMerge, &config.pre_merge)?,
        hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. }
    );
    // An in-place merge removes nothing, so its pre-remove hooks never run.
    let pre_remove_hooks_trusted = if in_place {
        true
    } else {
        let remove_config =
            EffectiveConfig::load_for_worktree(&repository, &repository.current().path)?;
        matches!(
            hook_approval::evaluate(&repository, HookPhase::PreRemove, &remove_config.pre_remove,)?,
            hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. }
        )
    };
    let context = MergeContext {
        repository_identity: BytePath::path(&identity),
        source_branch: source,
        target_branch: target,
        phase,
        policy,
        source_commit,
        target_commit,
        topic_worktree: BytePath::path(&repository.current().path),
        primary_worktree: BytePath::path(primary),
        in_place,
        cleanup_pending,
        journaled: journal.is_some(),
        rebase_active,
        squashes: squash.applicable(),
        squash_commits: squash.commit_count(),
        squash_generator_configured: squash.generator_configured(),
        squash_generator_trusted: squash.generator_trusted(),
        pre_merge_hooks_trusted,
        pre_remove_hooks_trusted,
    };
    let removes = policy.removes_topic(in_place);
    let effects = planned_merge_effects(&context, &config, needs_rebase, removes);
    Ok(MergePlan {
        repository,
        context,
        config,
        needs_rebase,
        squash,
        resuming_squash,
        effects,
    })
}

fn planned_merge_effects(
    context: &MergeContext,
    config: &EffectiveConfig,
    needs_rebase: bool,
    removes: bool,
) -> Vec<Effect> {
    let effect = |action: &str, details| Effect {
        action: action.into(),
        attempted: false,
        completed: false,
        details: Some(details),
    };
    vec![
        effect(
            "journal",
            serde_json::json!({"applicable":!context.journaled}),
        ),
        effect(
            "rebase",
            serde_json::json!({"applicable":needs_rebase || context.rebase_active}),
        ),
        effect(
            "squash",
            serde_json::json!({"applicable":context.squashes,"commits":context.squash_commits,"trusted":context.squash_generator_trusted}),
        ),
        effect(
            "pre_merge_hooks",
            serde_json::json!({"configured":!config.pre_merge.is_empty(),"trusted":context.pre_merge_hooks_trusted}),
        ),
        effect(
            "fast_forward_merge",
            serde_json::json!({"applicable":!context.cleanup_pending}),
        ),
        effect(
            "pre_remove_hooks",
            serde_json::json!({"applicable":removes,"trusted":context.pre_remove_hooks_trusted}),
        ),
        effect("remove_worktree", serde_json::json!({"applicable":removes})),
        effect(
            "destination",
            serde_json::json!({"applicable":removes,"path":context.primary_worktree}),
        ),
        effect("journal_cleanup", serde_json::json!({"applicable":true})),
    ]
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)] // A journal records pinned policy facts, not state switches.
struct MergeJournal {
    topic_path: PathBuf,
    topic_identity: PathBuf,
    source_branch: String,
    target_branch: String,
    no_rebase: bool,
    no_remove: bool,
    no_squash: bool,
    /// Stage local changes into the generated squash commit.
    yolo_stage_all: bool,
    squash: SquashStateV2,
    cleanup_pending: bool,
    validated_source: Option<String>,
    validated_target: Option<String>,
}

#[derive(Deserialize)]
struct JournalVersion {
    version: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Compatibility DTO mirrors the closed historical wire shape.
struct MergeJournalV1 {
    version: u8,
    topic_path: PathBuf,
    topic_identity: PathBuf,
    source_branch: String,
    target_branch: String,
    no_rebase: bool,
    no_remove: bool,
    #[serde(default)]
    no_squash: bool,
    #[serde(default)]
    yolo_stage_all: bool,
    #[serde(default)]
    squashed: bool,
    #[serde(default)]
    cleanup_pending: bool,
    #[serde(default)]
    validated_source: Option<String>,
    #[serde(default)]
    validated_target: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Compatibility DTO mirrors pinned policy facts on the wire.
struct MergeJournalV2 {
    version: u8,
    topic_path: EncodedPath,
    topic_identity: EncodedPath,
    source_branch: String,
    target_branch: String,
    no_rebase: bool,
    no_remove: bool,
    no_squash: bool,
    yolo_stage_all: bool,
    squashed: SquashStateV2,
    cleanup_pending: bool,
    validated_source: Option<String>,
    validated_target: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum SquashStateV2 {
    NotStarted,
    Skipped {
        reason: SquashSkipReasonV2,
    },
    Prepared {
        checkpoint: SquashCheckpointV2,
    },
    Completed {
        resulting_commit: String,
    },
    #[serde(skip)]
    LegacyCompleted,
}

impl SquashStateV2 {
    #[cfg(test)]
    const fn completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    const fn prepared(&self) -> bool {
        matches!(self, Self::Prepared { .. })
    }

    const fn not_started(&self) -> bool {
        matches!(self, Self::NotStarted)
    }
}

fn initial_squash_state(squash_disabled: bool, assessment: &squash::Assessment) -> SquashStateV2 {
    if squash_disabled {
        SquashStateV2::Skipped {
            reason: SquashSkipReasonV2::Disabled,
        }
    } else if matches!(
        assessment,
        squash::Assessment::Skipped {
            reason: squash::SkipReason::SingleCommit,
            ..
        }
    ) {
        SquashStateV2::Skipped {
            reason: SquashSkipReasonV2::SingleCommit,
        }
    } else {
        SquashStateV2::NotStarted
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SquashSkipReasonV2 {
    Disabled,
    SingleCommit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SquashCheckpointV2 {
    topic_commit: String,
    target_commit: String,
    expected_tree: String,
    message_bytes: EncodedBytes,
    topic_branch: String,
    target_branch: String,
    topic_worktree: EncodedPath,
    commit_count: usize,
    include_staged: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedBytes {
    encoding: ByteEncoding,
    value: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ByteEncoding {
    Base64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodedPath {
    encoding: ByteEncoding,
    value: String,
    display: String,
}

/// Strict version 1 input for worktree removal.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemovalInput {
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// Public result shared by human and JSON removal adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RemovalResult {
    DryRun {
        targets: Vec<RemovalTargetContext>,
        force: bool,
    },
    Removed {
        targets: Vec<RemovalTargetContext>,
        destination: Option<BytePath>,
    },
}

/// Stable removal failure rendered by protocol adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalError {
    pub code: String,
    pub message: String,
}

impl From<RemovalError> for ErrorBody {
    fn from(error: RemovalError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

/// Partial-completion state exposed by both removal adapters.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalOutcomeContext {
    #[serde(flatten)]
    pub removal: RemovalContext,
    pub completed_targets: Vec<RemovalTargetContext>,
    pub failed_targets: Vec<RemovalTargetContext>,
    pub pending_targets: Vec<RemovalTargetContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<BytePath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<RemovalApprovalContext>,
}

/// Hook trust facts preserved when removal is blocked before mutation.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalApprovalContext {
    pub phase: String,
    pub commands: Vec<RemovalApprovalCommand>,
    pub repository: String,
    pub identity: String,
}

/// One exact hook command awaiting approval.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalApprovalCommand {
    pub name: Option<String>,
    pub command: String,
}

/// Complete command-owned removal outcome before presentation adaptation.
#[derive(Debug)]
pub struct RemovalOutcome {
    pub result: std::result::Result<RemovalResult, RemovalError>,
    pub context: RemovalOutcomeContext,
    pub effects: Vec<Effect>,
    pub diagnostics: Vec<Diagnostic>,
    pub recovery: Vec<RecoveryAction<protocol::Request<RemovalInput>>>,
}

/// Stable version 1 removal error catalog.
pub const REMOVE_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
    "remove.duplicate_target",
    "remove.primary_forbidden",
    "remove.force_required",
    "remove.lifecycle_active",
    "remove.journal_invalid",
    "remove.unknown_target",
    "remove.preflight_failed",
    "remove.target_unavailable",
    "trust.approval_required",
];

/// Stable version 1 removal action catalog.
pub const REMOVE_ACTIONS: &[&str] = &["pre_remove_hooks", "remove_worktree"];

/// Adapter-neutral facts captured by removal preflight.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalContext {
    pub primary_worktree: BytePath,
    pub current_worktree: BytePath,
    pub force: bool,
    pub targets: Vec<RemovalTargetContext>,
    pub destination: Option<BytePath>,
}

/// Stable facts about one ordered removal target.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalTargetContext {
    pub branch: String,
    pub path: BytePath,
    pub branch_retained: bool,
    pub current: bool,
    pub force: bool,
    pub pre_remove_hooks: usize,
}

/// A fully validated, read-only removal plan shared by human and JSON adapters.
#[derive(Debug)]
pub struct RemovalPlan {
    pub repository: Repository,
    pub primary: PathBuf,
    pub current: PathBuf,
    pub targets: Vec<RemovalTarget>,
    pub force: bool,
    pub context: RemovalContext,
    /// Ordered hook, removal, and destination effects. Planning never attempts them.
    pub effects: Vec<Effect>,
}

/// One target and the configuration captured during removal preflight.
#[derive(Clone, Debug)]
pub struct RemovalTarget {
    pub worktree: Worktree,
    pub config: EffectiveConfig,
    pub stale_journal: Option<PathBuf>,
}

/// Execution state of one ordered target.
#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalTargetStatus {
    Completed,
    Failed,
    Pending,
}

/// One target's state after execution stops or completes.
#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct RemovalTargetOutcome {
    pub target: RemovalTargetContext,
    pub status: RemovalTargetStatus,
}

/// Typed removal failure at the plan-to-execution seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemovalFailureKind {
    ApprovalRequired,
    HookStart,
    Hook,
    JournalCleanup,
    GitStart,
    Git,
}

/// Captured output from removal hooks or Git.
#[derive(Clone, Debug)]
pub struct RemovalDiagnostic {
    pub source: &'static str,
    pub stream: &'static str,
    pub content: Vec<u8>,
    pub original_size: usize,
    pub truncated: bool,
}

/// Adapter-neutral result of executing a removal plan.
#[derive(Debug)]
pub struct RemovalExecutionOutcome {
    pub context: RemovalContext,
    pub targets: Vec<RemovalTargetOutcome>,
    pub approval: Option<RemovalApprovalContext>,
    pub effects: Vec<Effect>,
    pub diagnostics: Vec<RemovalDiagnostic>,
    pub failure: Option<(RemovalFailureKind, String)>,
}

impl RemovalExecutionOutcome {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.failure.is_none()
    }
}

/// Executes a validated removal plan with bounded captured output.
///
/// Approval is evaluated for every target before any hook, journal, or Git
/// mutation. Execution stops at the first failure and reports completed,
/// failed, and still-pending targets for a reproducible retry.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn execute_removal(plan: &RemovalPlan) -> RemovalExecutionOutcome {
    execute_removal_with_policy(plan, hook::OutputPolicy::Captured)
}

/// Executes the shared removal operation with adapter-specific child output routing.
#[must_use]
#[allow(clippy::too_many_lines)]
pub(crate) fn execute_removal_with_policy(
    plan: &RemovalPlan,
    output_policy: hook::OutputPolicy,
) -> RemovalExecutionOutcome {
    let mut effects = plan.effects.clone();
    let mut diagnostics = Vec::new();
    let mut targets = plan
        .context
        .targets
        .iter()
        .cloned()
        .map(|target| RemovalTargetOutcome {
            target,
            status: RemovalTargetStatus::Pending,
        })
        .collect::<Vec<_>>();

    for (index, target) in plan.targets.iter().enumerate() {
        match hook_approval::evaluate(
            &plan.repository,
            HookPhase::PreRemove,
            &target.config.pre_remove,
        ) {
            Ok(hook_approval::Evaluation::ApprovalRequired(candidate)) => {
                targets[index].status = RemovalTargetStatus::Failed;
                let mut outcome = removal_failure(
                    plan,
                    targets,
                    effects,
                    diagnostics,
                    RemovalFailureKind::ApprovalRequired,
                    format!(
                        "pre-remove hooks for {} require approval",
                        target.worktree.branch_label()
                    ),
                );
                outcome.approval = Some(RemovalApprovalContext {
                    phase: candidate.phase().key().into(),
                    commands: candidate
                        .commands()
                        .iter()
                        .map(|step| RemovalApprovalCommand {
                            name: step.name.clone(),
                            command: step.command.clone(),
                        })
                        .collect(),
                    repository: candidate.repository().into(),
                    identity: candidate.identity().into(),
                });
                return outcome;
            }
            Ok(_) => {}
            Err(error) => {
                targets[index].status = RemovalTargetStatus::Failed;
                return removal_failure(
                    plan,
                    targets,
                    effects,
                    diagnostics,
                    RemovalFailureKind::ApprovalRequired,
                    error,
                );
            }
        }
    }

    for (index, target) in plan.targets.iter().enumerate() {
        let hook_effect = index * 2;
        if target.config.pre_remove.is_empty() {
            effects[hook_effect].completed = true;
        } else {
            effects[hook_effect].attempted = true;
            let execution = match hook::execute(
                HookPhase::PreRemove,
                &target.config.pre_remove,
                &target.worktree.path,
                output_policy,
            ) {
                Ok(execution) => execution,
                Err(error) => {
                    targets[index].status = RemovalTargetStatus::Failed;
                    return removal_failure(
                        plan,
                        targets,
                        effects,
                        diagnostics,
                        RemovalFailureKind::HookStart,
                        error,
                    );
                }
            };
            if let hook::HookOutput::Captured(output) = execution.output {
                for step in output {
                    for (stream, captured) in [("stdout", step.stdout), ("stderr", step.stderr)] {
                        if captured.original_size > 0 {
                            diagnostics.push(RemovalDiagnostic {
                                source: "hook",
                                stream,
                                content: captured.content,
                                original_size: captured.original_size,
                                truncated: captured.truncated,
                            });
                        }
                    }
                }
            }
            if execution.outcome != HookOutcome::Success {
                targets[index].status = RemovalTargetStatus::Failed;
                return removal_failure(
                    plan,
                    targets,
                    effects,
                    diagnostics,
                    RemovalFailureKind::Hook,
                    format!("pre-remove hook outcome: {:?}", execution.outcome),
                );
            }
            effects[hook_effect].completed = true;
        }

        if let Some(path) = &target.stale_journal
            && let Err(error) = fs::remove_file(path)
        {
            targets[index].status = RemovalTargetStatus::Failed;
            return removal_failure(
                plan,
                targets,
                effects,
                diagnostics,
                RemovalFailureKind::JournalCleanup,
                error,
            );
        }

        let remove_effect = hook_effect + 1;
        effects[remove_effect].attempted = true;
        let removal_output = match output_policy {
            hook::OutputPolicy::Captured => git::RemovalOutput::Captured,
            hook::OutputPolicy::Streamed => git::RemovalOutput::Displayed,
        };
        let removal_mode = if plan.force {
            git::RemovalMode::Force
        } else {
            git::RemovalMode::Safe
        };
        let removal = git::WorktreeMutation::new(&plan.primary).remove(
            &target.worktree.path,
            removal_mode,
            removal_output,
        );
        let captured = match removal {
            Ok(captured) => captured,
            Err(error) => {
                targets[index].status = RemovalTargetStatus::Failed;
                return removal_failure(
                    plan,
                    targets,
                    effects,
                    diagnostics,
                    if matches!(output_policy, hook::OutputPolicy::Captured) {
                        RemovalFailureKind::GitStart
                    } else {
                        RemovalFailureKind::Git
                    },
                    error,
                );
            }
        };
        if let Some(output) = captured {
            push_removal_git_diagnostic(&mut diagnostics, "stdout", &output.stdout);
            push_removal_git_diagnostic(&mut diagnostics, "stderr", &output.stderr);
            if !output.status.success() {
                targets[index].status = RemovalTargetStatus::Failed;
                return removal_failure(
                    plan,
                    targets,
                    effects,
                    diagnostics,
                    RemovalFailureKind::Git,
                    format!("git worktree remove failed with {}", output.status),
                );
            }
        }
        effects[remove_effect].completed = true;
        targets[index].status = RemovalTargetStatus::Completed;
    }

    if plan.context.destination.is_some() {
        let destination = effects.len() - 1;
        effects[destination].attempted = true;
        effects[destination].completed = true;
    }
    RemovalExecutionOutcome {
        context: plan.context.clone(),
        targets,
        effects,
        diagnostics,
        failure: None,
        approval: None,
    }
}

/// Plans and executes one removal request through the command-owned interface.
///
/// # Errors
///
/// Returns a typed preflight failure before execution can begin.
pub(crate) fn execute_removal_request(
    input: &RemovalInput,
    force: bool,
    output_policy: hook::OutputPolicy,
) -> std::result::Result<RemovalOutcome, PreflightFailure> {
    let plan = plan_remove(&input.branches, force)?;
    if input.dry_run {
        return Ok(RemovalOutcome {
            result: Ok(RemovalResult::DryRun {
                targets: plan.context.targets.clone(),
                force,
            }),
            context: RemovalOutcomeContext {
                removal: plan.context.clone(),
                completed_targets: Vec::new(),
                failed_targets: Vec::new(),
                pending_targets: plan.context.targets.clone(),
                branch: None,
                path: None,
                approval: None,
            },
            effects: plan.effects.clone(),
            diagnostics: Vec::new(),
            recovery: Vec::new(),
        });
    }
    Ok(removal_outcome(
        execute_removal_with_policy(&plan, output_policy),
        input,
    ))
}

/// Returns the stable protocol code for a removal preflight failure.
#[must_use]
pub const fn removal_preflight_code(kind: PreflightFailureKind) -> &'static str {
    match kind {
        PreflightFailureKind::DuplicateTarget => "remove.duplicate_target",
        PreflightFailureKind::PrimaryForbidden => "remove.primary_forbidden",
        PreflightFailureKind::ForceRequired => "remove.force_required",
        PreflightFailureKind::LifecycleActive => "remove.lifecycle_active",
        PreflightFailureKind::JournalInvalid => "remove.journal_invalid",
        PreflightFailureKind::UnknownTarget => "remove.unknown_target",
        _ => "remove.preflight_failed",
    }
}

/// Converts execution state into the single typed outcome consumed by adapters.
#[must_use]
pub fn removal_outcome(execution: RemovalExecutionOutcome, input: &RemovalInput) -> RemovalOutcome {
    let diagnostics = execution
        .diagnostics
        .into_iter()
        .map(|diagnostic| Diagnostic {
            source: diagnostic.source.into(),
            stream: diagnostic.stream.into(),
            content: String::from_utf8_lossy(&diagnostic.content).into_owned(),
            original_size: diagnostic.original_size,
            truncated: diagnostic.truncated,
        })
        .collect();
    let result = match execution.failure.as_ref() {
        None => Ok(RemovalResult::Removed {
            targets: execution.context.targets.clone(),
            destination: execution.context.destination.clone(),
        }),
        Some((kind, message)) => Err(RemovalError {
            code: removal_failure_code(*kind).into(),
            message: message.clone(),
        }),
    };
    let recovery = execution
        .failure
        .as_ref()
        .map_or_else(Vec::new, |(kind, _)| {
            let approval = *kind == RemovalFailureKind::ApprovalRequired;
            let branches = execution
                .targets
                .iter()
                .filter(|target| {
                    if approval {
                        target.status == RemovalTargetStatus::Failed
                    } else {
                        target.status != RemovalTargetStatus::Completed
                    }
                })
                .map(|target| target.target.branch.clone())
                .collect::<Vec<_>>();
            let mut argv = vec!["pando".into(), "remove".into()];
            let stdin = if approval {
                argv.extend(branches.clone());
                None
            } else {
                argv.extend(["--input-output".into(), "json".into()]);
                if execution.context.force {
                    argv.push("--force".into());
                }
                Some(protocol::Request {
                    schema_version: protocol::SCHEMA_VERSION,
                    request_id: None,
                    input: RemovalInput {
                        branches,
                        dry_run: input.dry_run,
                    },
                })
            };
            vec![RecoveryAction {
                action: if approval {
                    "trust.approve_hooks".into()
                } else {
                    "remove.retry".into()
                },
                description: if approval {
                    "Review and approve pre-remove hooks interactively".into()
                } else {
                    "Retry pending removal targets after resolving the failure".into()
                },
                mutation: if approval {
                    MutationClass::Trust
                } else {
                    MutationClass::Worktree
                },
                requires_human_approval: approval || execution.context.force,
                invocation: RecoveryInvocation {
                    argv,
                    stdin,
                    working_directory: Some(execution.context.current_worktree.clone()),
                },
            }]
        });
    let completed_targets = targets_with_status(&execution.targets, RemovalTargetStatus::Completed);
    let failed_targets = targets_with_status(&execution.targets, RemovalTargetStatus::Failed);
    let pending_targets = targets_with_status(&execution.targets, RemovalTargetStatus::Pending);
    RemovalOutcome {
        result,
        context: RemovalOutcomeContext {
            removal: execution.context,
            completed_targets,
            failed_targets: failed_targets.clone(),
            pending_targets,
            branch: failed_targets.first().map(|target| target.branch.clone()),
            path: failed_targets.first().map(|target| target.path.clone()),
            approval: execution.approval,
        },
        effects: execution.effects,
        diagnostics,
        recovery,
    }
}

fn targets_with_status(
    targets: &[RemovalTargetOutcome],
    status: RemovalTargetStatus,
) -> Vec<RemovalTargetContext> {
    targets
        .iter()
        .filter(|target| target.status == status)
        .map(|target| target.target.clone())
        .collect()
}

const fn removal_failure_code(kind: RemovalFailureKind) -> &'static str {
    match kind {
        RemovalFailureKind::ApprovalRequired => "trust.approval_required",
        RemovalFailureKind::HookStart => "remove.hook_start_failed",
        RemovalFailureKind::Hook => "remove.hook_failed",
        RemovalFailureKind::JournalCleanup => "remove.journal_cleanup_failed",
        RemovalFailureKind::GitStart => "remove.git_start_failed",
        RemovalFailureKind::Git => "remove.git_failed",
    }
}

fn removal_failure(
    plan: &RemovalPlan,
    targets: Vec<RemovalTargetOutcome>,
    effects: Vec<Effect>,
    diagnostics: Vec<RemovalDiagnostic>,
    kind: RemovalFailureKind,
    error: impl std::fmt::Display,
) -> RemovalExecutionOutcome {
    RemovalExecutionOutcome {
        context: plan.context.clone(),
        targets,
        effects,
        diagnostics,
        failure: Some((kind, error.to_string())),
        approval: None,
    }
}

fn push_removal_git_diagnostic(
    diagnostics: &mut Vec<RemovalDiagnostic>,
    stream: &'static str,
    bytes: &[u8],
) {
    const LIMIT: usize = 16 * 1024;
    if bytes.is_empty() {
        return;
    }
    diagnostics.push(RemovalDiagnostic {
        source: "git",
        stream,
        content: bytes[..bytes.len().min(LIMIT)].to_vec(),
        original_size: bytes.len(),
        truncated: bytes.len() > LIMIT,
    });
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
        hook_approval::approve_interactively(
            &plan.repository,
            HookPhase::PreRemove,
            &target.config.pre_remove,
        )?;
    }
    let input = RemovalInput {
        branches: branches.to_vec(),
        dry_run: false,
    };
    let outcome = removal_outcome(
        execute_removal_with_policy(&plan, hook::OutputPolicy::Streamed),
        &input,
    );
    if let Err(error) = outcome.result {
        bail!(error.message);
    }
    if plan.context.destination.is_some() {
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
    let input = MergeInput {
        no_rebase,
        no_remove,
        no_squash,
        dry_run: true,
    };
    let journaled_merge::Preparation::Complete(outcome) =
        journaled_merge::prepare(journaled_merge::MergeRequest::ordinary(&input))
    else {
        unreachable!("dry-run preparation never grants mutation authority")
    };
    if let Err(error) = &outcome.result {
        bail!(error.message.clone());
    }
    let context = match &outcome.context {
        MergeOutcomeContext::Lifecycle(context)
        | MergeOutcomeContext::Approval {
            lifecycle: context, ..
        } => context,
        MergeOutcomeContext::Completed { .. } | MergeOutcomeContext::Unavailable {} => {
            unreachable!("a successful dry run has planned lifecycle context")
        }
    };
    if context.squashes {
        ui::info(if context.squash_commits == 0 {
            format!(
                "Would squash the topic into a single generated-message commit after the rebase onto {}.",
                context.target_branch
            )
        } else {
            format!(
                "Would squash {} commits into a single generated-message commit.",
                context.squash_commits
            )
        })?;
    }
    let follow_up = if context.in_place {
        format!(
            " and switch the primary worktree to {}",
            context.target_branch
        )
    } else if context.policy.no_remove {
        " and retain the topic worktree".to_owned()
    } else {
        " and remove the topic worktree".to_owned()
    };
    ui::finish(format!(
        "Would merge {} into {}{follow_up}; no changes made.",
        context.source_branch, context.target_branch,
    ))
}

fn mark_effect(effects: &mut [Effect], action: &str, attempted: bool, completed: bool) {
    let effect = effects
        .iter_mut()
        .find(|effect| effect.action == action)
        .expect("every lifecycle transition has a planned effect");
    effect.attempted = attempted;
    effect.completed = completed;
}

fn push_merge_diagnostic(
    diagnostics: &mut Vec<MergeDiagnostic>,
    phase: &'static str,
    stream: &'static str,
    content: &[u8],
) {
    const LIMIT: usize = 16 * 1024;
    if content.is_empty() {
        return;
    }
    diagnostics.push(MergeDiagnostic {
        phase,
        stream,
        content: content[..content.len().min(LIMIT)].to_vec(),
        original_size: content.len(),
        truncated: content.len() > LIMIT,
    });
}

fn push_captured_merge_diagnostic(
    diagnostics: &mut Vec<MergeDiagnostic>,
    phase: &'static str,
    stream: &'static str,
    captured: hook::CapturedStream,
) {
    if captured.original_size == 0 {
        return;
    }
    diagnostics.push(MergeDiagnostic {
        phase,
        stream,
        content: captured.content,
        original_size: captured.original_size,
        truncated: captured.truncated,
    });
}

fn execute_merge_hooks(
    hook_phase: HookPhase,
    diagnostic_phase: &'static str,
    steps: &[crate::config::HookStep],
    destination: &Path,
    output: &journaled_merge::MergeExecutionOutput,
    diagnostics: &mut Vec<MergeDiagnostic>,
) -> Result<()> {
    let execution = hook::execute(hook_phase, steps, destination, output.hook_policy())?;
    output.record_hook_output(diagnostics, diagnostic_phase, execution.output);
    match execution.outcome {
        HookOutcome::Success => Ok(()),
        HookOutcome::Failed(status) => Err(anyhow::anyhow!(
            "{} hook failed with status {status}",
            hook_phase.key()
        )),
        HookOutcome::Interrupted => Err(anyhow::anyhow!("{} hook interrupted", hook_phase.key())),
    }
}

fn execution_failure(
    plan: &MergePlan,
    effects: Vec<Effect>,
    diagnostics: Vec<MergeDiagnostic>,
    phase: MergePhase,
    kind: MergeExecutionFailureKind,
    error: impl std::fmt::Display,
) -> MergeExecutionOutcome {
    let mut context = plan.context.clone();
    context.phase = phase;
    context.journaled = effects
        .iter()
        .find(|effect| effect.action == "journal")
        .is_some_and(|effect| effect.completed)
        || plan.context.journaled;
    MergeExecutionOutcome {
        context,
        effects,
        diagnostics,
        destination: None,
        failure: Some((kind, error.to_string())),
    }
}

/// Executes an already validated merge plan, including cleanup-only recovery.
///
/// The journal is established before the first Git mutation, and effects are
/// updated beside their transitions so adapters never infer progress.
///
/// # Panics
/// Panics if the validated plan lacks a primary worktree or a planned lifecycle
/// effect, both of which are planner invariants.
#[must_use]
#[allow(clippy::too_many_lines)]
fn execute_merge(
    plan: &MergePlan,
    changes: journaled_merge::ChangePolicy,
    output: &journaled_merge::MergeExecutionOutput,
) -> MergeExecutionOutcome {
    let mut effects = plan.effects.clone();
    let mut diagnostics = Vec::new();
    let current = plan.repository.current();
    let primary = plan
        .repository
        .primary
        .as_ref()
        .expect("a merge plan always has a primary worktree");
    let current_history = HistoryObservation::new(&current.path);
    let primary_history = HistoryObservation::new(primary);
    if !current_history
        .head_commit()
        .is_ok_and(|head| head == plan.context.source_commit)
        || !primary_history
            .commit(&plan.context.target_branch)
            .is_ok_and(|head| head == plan.context.target_commit)
        || (!plan.context.rebase_active
            && !plan.resuming_squash
            && matches!(changes, journaled_merge::ChangePolicy::RequireClean)
            && current_history
                .status()
                .map_or(true, |status| status.is_dirty()))
    {
        return execution_failure(
            plan,
            effects,
            diagnostics,
            MergePhase::Planned,
            MergeExecutionFailureKind::StalePlan,
            "repository state changed after merge planning; retry to create a fresh plan",
        );
    }
    let identity = match RepositoryObservation::new(&current.path).worktree_identity() {
        Ok(identity) => identity,
        Err(error) => {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Planned,
                MergeExecutionFailureKind::StalePlan,
                error,
            );
        }
    };
    let mut state = if plan.context.journaled {
        match read_journal(&plan.repository.common_dir, &identity) {
            Ok(Some(state)) => state,
            Ok(None) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Planned,
                    MergeExecutionFailureKind::StalePlan,
                    "the lifecycle journal disappeared after merge planning; retry to create a fresh plan",
                );
            }
            Err(error) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Planned,
                    MergeExecutionFailureKind::Journal,
                    error,
                );
            }
        }
    } else {
        MergeJournal {
            topic_path: current.path.clone(),
            topic_identity: identity,
            source_branch: plan.context.source_branch.clone(),
            target_branch: plan.context.target_branch.clone(),
            no_rebase: plan.context.policy.no_rebase,
            no_remove: plan.context.policy.no_remove,
            no_squash: plan.context.policy.no_squash,
            yolo_stage_all: matches!(changes, journaled_merge::ChangePolicy::IncludeAll),
            squash: initial_squash_state(
                plan.context.policy.no_squash || !plan.config.squash,
                &plan.squash,
            ),
            cleanup_pending: false,
            validated_source: None,
            validated_target: None,
        }
    };
    if plan.context.cleanup_pending {
        return execute_merge_cleanup(plan, &state, effects, diagnostics, output);
    }
    if !plan.context.journaled {
        mark_effect(&mut effects, "journal", true, false);
        if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Planned,
                MergeExecutionFailureKind::Journal,
                error,
            );
        }
        mark_effect(&mut effects, "journal", true, true);
    }

    if matches!(changes, journaled_merge::ChangePolicy::IncludeAll)
        && state.squash.not_started()
        && current_history
            .status()
            .is_ok_and(|status| status.is_dirty())
    {
        if let Err(error) = output.run_action(
            "Staging all changes...",
            "Staged all changes",
            "Failed to stage changes",
            || LifecycleMutation::new(&current.path).stage_all(),
        ) {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Planned,
                MergeExecutionFailureKind::StalePlan,
                error,
            );
        }
    }

    if plan.needs_rebase || plan.context.rebase_active {
        mark_effect(&mut effects, "rebase", true, false);
        let rebase_result = if plan.context.rebase_active {
            output.run_git(
                "Continuing rebase...",
                "Continued rebase",
                "Failed to continue the rebase",
                |policy| LifecycleMutation::new(&current.path).continue_rebase(policy),
            )
        } else {
            output.run_git(
                &format!("Rebasing onto {}...", state.target_branch),
                &format!("Rebased onto {}", state.target_branch),
                &format!("Failed to rebase onto {}", state.target_branch),
                |policy| {
                    if matches!(changes, journaled_merge::ChangePolicy::IncludeAll) {
                        LifecycleMutation::new(&current.path)
                            .rebase_onto_autostash(&state.target_branch, policy)
                    } else {
                        LifecycleMutation::new(&current.path)
                            .rebase_onto(&state.target_branch, policy)
                    }
                },
            )
        };
        match rebase_result {
            Ok(transcript) => {
                output.record_transcript(&mut diagnostics, "rebase", &transcript);
                mark_effect(&mut effects, "rebase", true, true);
            }
            Err(error) => {
                output.record_error(&mut diagnostics, "rebase", &error);
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Rebase,
                    MergeExecutionFailureKind::Rebase,
                    error,
                );
            }
        }
    }
    if matches!(changes, journaled_merge::ChangePolicy::IncludeAll)
        && state.squash.not_started()
        && current_history
            .status()
            .is_ok_and(|status| status.is_dirty())
    {
        if let Err(error) = output.run_action(
            "Staging all changes...",
            "Staged all changes",
            "Failed to stage changes",
            || LifecycleMutation::new(&current.path).stage_all(),
        ) {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Rebase,
                MergeExecutionFailureKind::StalePlan,
                error,
            );
        }
    }
    if let SquashStateV2::Prepared { checkpoint } = &state.squash {
        mark_effect(&mut effects, "squash", true, false);
        let checkpoint = match squash_checkpoint(checkpoint) {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Squash,
                    MergeExecutionFailureKind::Journal,
                    error,
                );
            }
        };
        let prepared = match squash::resume(checkpoint) {
            Ok(prepared) => prepared,
            Err(error) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Squash,
                    MergeExecutionFailureKind::Squash,
                    error,
                );
            }
        };
        if let Err(error) = output.present_commit_message(&mut diagnostics, prepared.message()) {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Squash,
                MergeExecutionFailureKind::Squash,
                error,
            );
        }
        let commit_count = prepared.commit_count();
        let collapse = output.run_action(
            &format!("Resuming squash of {commit_count} commits..."),
            "Squashed the topic into a single commit",
            "Failed to resume the squash",
            || squash::collapse(prepared).map_err(anyhow::Error::from),
        );
        let collapsed = match collapse {
            Ok(collapsed) => collapsed,
            Err(error) => {
                let detail = error.downcast_ref::<squash::CollapseFailure>().map_or_else(
                    || error.to_string(),
                    |failure| format!("collapse progress {:?}: {failure}", failure.progress()),
                );
                push_merge_diagnostic(&mut diagnostics, "squash", "stderr", detail.as_bytes());
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Squash,
                    MergeExecutionFailureKind::Squash,
                    error,
                );
            }
        };
        state.squash = SquashStateV2::Completed {
            resulting_commit: collapsed.commit().to_owned(),
        };
        if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Squash,
                MergeExecutionFailureKind::Journal,
                error,
            );
        }
        mark_effect(&mut effects, "squash", true, true);
    }
    if plan.squash.applicable() && state.squash.not_started() {
        'squash_phase: {
            mark_effect(&mut effects, "squash", true, false);
            let assessment = squash::assess(squash::AssessRequest {
                repository: &plan.repository,
                config: &plan.config,
                target: &state.target_branch,
                enabled: true,
                final_history: true,
                include_staged: matches!(changes, journaled_merge::ChangePolicy::IncludeAll),
            });
            let required = match assessment {
                Ok(squash::Assessment::Required(required)) => required,
                Ok(squash::Assessment::Skipped { .. }) => {
                    state.squash = SquashStateV2::Skipped {
                        reason: SquashSkipReasonV2::SingleCommit,
                    };
                    if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
                        return execution_failure(
                            plan,
                            effects,
                            diagnostics,
                            MergePhase::Squash,
                            MergeExecutionFailureKind::Journal,
                            error,
                        );
                    }
                    mark_effect(&mut effects, "squash", true, true);
                    break 'squash_phase;
                }
                Ok(blocked @ squash::Assessment::Blocked { .. }) => {
                    let error = blocked
                        .into_required()
                        .expect_err("blocked assessment has no capability");
                    return execution_failure(
                        plan,
                        effects,
                        diagnostics,
                        MergePhase::Squash,
                        MergeExecutionFailureKind::StalePlan,
                        error,
                    );
                }
                Ok(squash::Assessment::PendingFinalHistory) => {
                    return execution_failure(
                        plan,
                        effects,
                        diagnostics,
                        MergePhase::Squash,
                        MergeExecutionFailureKind::StalePlan,
                        anyhow::anyhow!(
                            "final post-rebase history is required before squash preparation"
                        ),
                    );
                }
                Err(error) => {
                    return execution_failure(
                        plan,
                        effects,
                        diagnostics,
                        MergePhase::Squash,
                        MergeExecutionFailureKind::Squash,
                        error,
                    );
                }
            };
            let preparation = output.run_action(
                "Generating squash commit message...",
                "Generated squash commit message:",
                "Failed to generate the squash commit message",
                || {
                    let preparation = squash::prepare(required)?;
                    if let squash::Preparation::Prepared(prepared) = &preparation {
                        state.squash = SquashStateV2::Prepared {
                            checkpoint: squash_checkpoint_v2(&prepared.checkpoint()),
                        };
                        write_journal(&plan.repository.common_dir, &state)?;
                    }
                    Ok(preparation)
                },
            );
            let prepared = match preparation {
                Ok(squash::Preparation::Prepared(prepared)) => prepared,
                Ok(squash::Preparation::Skipped) => {
                    state.squash = SquashStateV2::Skipped {
                        reason: SquashSkipReasonV2::SingleCommit,
                    };
                    if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
                        return execution_failure(
                            plan,
                            effects,
                            diagnostics,
                            MergePhase::Squash,
                            MergeExecutionFailureKind::Journal,
                            error,
                        );
                    }
                    mark_effect(&mut effects, "squash", true, true);
                    break 'squash_phase;
                }
                Err(error) => {
                    push_merge_diagnostic(
                        &mut diagnostics,
                        "squash",
                        "stderr",
                        error.to_string().as_bytes(),
                    );
                    return execution_failure(
                        plan,
                        effects,
                        diagnostics,
                        MergePhase::Squash,
                        MergeExecutionFailureKind::Squash,
                        error,
                    );
                }
            };
            if let Err(error) = output.present_commit_message(&mut diagnostics, prepared.message())
            {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Squash,
                    MergeExecutionFailureKind::Squash,
                    error,
                );
            }
            let commit_count = prepared.commit_count();
            let collapse = output.run_action(
                &format!("Squashing {commit_count} commits..."),
                "Squashed the topic into a single commit",
                "Failed to squash the topic",
                || squash::collapse(prepared).map_err(anyhow::Error::from),
            );
            let collapsed = match collapse {
                Ok(collapsed) => collapsed,
                Err(error) => {
                    push_merge_diagnostic(
                        &mut diagnostics,
                        "squash",
                        "stderr",
                        error.to_string().as_bytes(),
                    );
                    return execution_failure(
                        plan,
                        effects,
                        diagnostics,
                        MergePhase::Squash,
                        MergeExecutionFailureKind::Squash,
                        error,
                    );
                }
            };
            state.squash = SquashStateV2::Completed {
                resulting_commit: collapsed.commit().to_owned(),
            };
            if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Squash,
                    MergeExecutionFailureKind::Journal,
                    error,
                );
            }
            mark_effect(&mut effects, "squash", true, true);
        }
    }
    let mut candidate = match current_history.head_commit() {
        Ok(candidate) => candidate,
        Err(error) => {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Squash,
                MergeExecutionFailureKind::StalePlan,
                error,
            );
        }
    };

    loop {
        match hook_approval::evaluate(
            &plan.repository,
            HookPhase::PreMerge,
            &plan.config.pre_merge,
        ) {
            Ok(
                hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. },
            ) => {}
            Ok(hook_approval::Evaluation::ApprovalRequired(_)) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Validation,
                    MergeExecutionFailureKind::StalePlan,
                    "pre-merge hook trust changed before execution; retry after approving the current commands",
                );
            }
            Err(error) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Validation,
                    MergeExecutionFailureKind::Validation,
                    error,
                );
            }
        }
        mark_effect(&mut effects, "pre_merge_hooks", true, false);
        let hook_result = execute_merge_hooks(
            HookPhase::PreMerge,
            "validation",
            &plan.config.pre_merge,
            &current.path,
            output,
            &mut diagnostics,
        );
        if let Err(error) = hook_result {
            push_merge_diagnostic(
                &mut diagnostics,
                "validation",
                "stderr",
                error.to_string().as_bytes(),
            );
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Validation,
                MergeExecutionFailureKind::Validation,
                error,
            );
        }
        if current_history
            .status()
            .map_or(true, |status| status.is_dirty())
        {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Validation,
                MergeExecutionFailureKind::StalePlan,
                "pre-merge hooks left the candidate worktree dirty; clean it before retrying",
            );
        }
        let refreshed = match current_history.head_commit() {
            Ok(refreshed) => refreshed,
            Err(error) => {
                return execution_failure(
                    plan,
                    effects,
                    diagnostics,
                    MergePhase::Validation,
                    MergeExecutionFailureKind::StalePlan,
                    error,
                );
            }
        };
        if !primary_history
            .commit(&plan.context.target_branch)
            .is_ok_and(|head| head == plan.context.target_commit)
        {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Validation,
                MergeExecutionFailureKind::StalePlan,
                "the target advanced during validation; retry to validate the new candidate",
            );
        }
        if refreshed == candidate {
            break;
        }
        candidate = refreshed;
    }
    mark_effect(&mut effects, "pre_merge_hooks", true, true);
    state.validated_source = Some(candidate.clone());
    state.validated_target = Some(plan.context.target_commit.clone());
    if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
        return execution_failure(
            plan,
            effects,
            diagnostics,
            MergePhase::Validation,
            MergeExecutionFailureKind::Journal,
            error,
        );
    }

    if !current_history
        .head_commit()
        .is_ok_and(|head| head == candidate)
        || !primary_history
            .commit(&plan.context.source_branch)
            .is_ok_and(|head| head == candidate)
        || !primary_history
            .commit(&plan.context.target_branch)
            .is_ok_and(|head| head == plan.context.target_commit)
        || !primary_history
            .is_ancestor(&plan.context.target_branch, &candidate)
            .unwrap_or(false)
    {
        return execution_failure(
            plan,
            effects,
            diagnostics,
            MergePhase::Validation,
            MergeExecutionFailureKind::StalePlan,
            "repository state changed after validation; retry before fast-forwarding",
        );
    }
    if plan.context.in_place {
        let switch_result = output
            .run_git(
                &format!("Switching to {}...", state.target_branch),
                &format!("Switched to {}", state.target_branch),
                &format!("Failed to switch to {}", state.target_branch),
                |policy| {
                    LifecycleMutation::new(primary).switch_branch(&state.target_branch, policy)
                },
            )
            .map(|_| ());
        if let Err(error) = switch_result {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Integration,
                MergeExecutionFailureKind::Integration,
                error,
            );
        }
    }
    let refreshed_repository = match RepositoryObservation::new(primary).repository() {
        Ok(repository) => repository,
        Err(error) => {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Integration,
                MergeExecutionFailureKind::StalePlan,
                error,
            );
        }
    };
    if !refreshed_repository
        .worktrees
        .iter()
        .any(|worktree| worktree.path == current.path)
        || refreshed_repository.current_branch().ok() != Some(state.target_branch.as_str())
        || !primary_history
            .commit(&state.source_branch)
            .is_ok_and(|head| head == candidate)
        || !primary_history
            .commit(&state.target_branch)
            .is_ok_and(|head| head == plan.context.target_commit)
        || !primary_history
            .is_ancestor(&state.target_branch, &candidate)
            .unwrap_or(false)
    {
        return execution_failure(
            plan,
            effects,
            diagnostics,
            MergePhase::Integration,
            MergeExecutionFailureKind::StalePlan,
            "source, target, checkout, ancestry, or worktree registration changed before integration; retry",
        );
    }
    mark_effect(&mut effects, "fast_forward_merge", true, false);
    let merge_result = output.run_git(
        &format!("Merging into {}...", state.target_branch),
        &format!("Merged into {}", state.target_branch),
        &format!("Failed to merge into {}", state.target_branch),
        |policy| LifecycleMutation::new(primary).fast_forward(&state.source_branch, policy),
    );
    let transcript = match merge_result {
        Ok(transcript) => transcript,
        Err(error) => {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Integration,
                MergeExecutionFailureKind::Integration,
                error,
            );
        }
    };
    output.record_transcript(&mut diagnostics, "integration", &transcript);
    mark_effect(&mut effects, "fast_forward_merge", true, true);
    if plan.context.policy.removes_topic(plan.context.in_place) {
        state.cleanup_pending = true;
        if let Err(error) = write_journal(&plan.repository.common_dir, &state) {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Integration,
                MergeExecutionFailureKind::Journal,
                error,
            );
        }
        return execute_merge_cleanup(plan, &state, effects, diagnostics, output);
    }
    mark_effect(&mut effects, "journal_cleanup", true, false);
    if let Err(error) = remove_journal(&plan.repository.common_dir, &state.topic_identity) {
        return execution_failure(
            plan,
            effects,
            diagnostics,
            MergePhase::Complete,
            MergeExecutionFailureKind::JournalCleanup,
            error,
        );
    }
    mark_effect(&mut effects, "journal_cleanup", true, true);
    let mut context = plan.context.clone();
    context.phase = MergePhase::Complete;
    context.journaled = false;
    MergeExecutionOutcome {
        context,
        effects,
        diagnostics,
        destination: None,
        failure: None,
    }
}

#[allow(clippy::too_many_lines)] // Cleanup records each transition beside its failure.
fn execute_merge_cleanup(
    plan: &MergePlan,
    state: &MergeJournal,
    mut effects: Vec<Effect>,
    mut diagnostics: Vec<MergeDiagnostic>,
    output: &journaled_merge::MergeExecutionOutput,
) -> MergeExecutionOutcome {
    let failure = |effects, diagnostics, kind, error: anyhow::Error| {
        execution_failure(plan, effects, diagnostics, MergePhase::Cleanup, kind, error)
    };
    match hook_approval::evaluate(
        &plan.repository,
        HookPhase::PreRemove,
        &plan.config.pre_remove,
    ) {
        Ok(hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. }) => {}
        Ok(hook_approval::Evaluation::ApprovalRequired(_)) => {
            return failure(
                effects,
                diagnostics,
                MergeExecutionFailureKind::StalePlan,
                anyhow::anyhow!(
                    "pre-remove hook trust changed before cleanup; retry after approving the current commands"
                ),
            );
        }
        Err(error) => {
            return failure(
                effects,
                diagnostics,
                MergeExecutionFailureKind::Cleanup,
                error,
            );
        }
    }
    mark_effect(&mut effects, "pre_remove_hooks", true, false);
    let hook_result = execute_merge_hooks(
        HookPhase::PreRemove,
        "cleanup",
        &plan.config.pre_remove,
        &state.topic_path,
        output,
        &mut diagnostics,
    );
    if let Err(error) = hook_result {
        push_merge_diagnostic(
            &mut diagnostics,
            "cleanup",
            "stderr",
            error.to_string().as_bytes(),
        );
        return failure(
            effects,
            diagnostics,
            MergeExecutionFailureKind::Cleanup,
            error,
        );
    }
    mark_effect(&mut effects, "pre_remove_hooks", true, true);

    let Some(worktree) = plan
        .repository
        .worktrees
        .iter()
        .find(|worktree| worktree.path == state.topic_path)
    else {
        return failure(
            effects,
            diagnostics,
            MergeExecutionFailureKind::Removal,
            anyhow::anyhow!("journaled topic worktree is no longer registered"),
        );
    };
    if let Err(error) = check_removable(worktree, false) {
        return failure(
            effects,
            diagnostics,
            MergeExecutionFailureKind::Removal,
            error,
        );
    }
    let primary = plan
        .repository
        .primary
        .as_ref()
        .expect("a merge plan always has a primary worktree");
    mark_effect(&mut effects, "remove_worktree", true, false);
    let mutation = git::WorktreeMutation::new(primary);
    let removal = mutation
        .remove(
            &state.topic_path,
            git::RemovalMode::Safe,
            output.removal_output(),
        )
        .and_then(|removal| output.finish_removal(removal, &mut diagnostics));
    if let Err(error) = removal {
        output.record_removal_error(&mut diagnostics, &error);
        return failure(
            effects,
            diagnostics,
            MergeExecutionFailureKind::Removal,
            error,
        );
    }
    mark_effect(&mut effects, "remove_worktree", true, true);
    mark_effect(&mut effects, "destination", true, true);
    let destination = Some(BytePath::path(primary));
    if let Err(error) = output.write_destination(primary) {
        let mut outcome = failure(
            effects,
            diagnostics,
            MergeExecutionFailureKind::Cleanup,
            error,
        );
        outcome.destination = destination;
        return outcome;
    }

    mark_effect(&mut effects, "journal_cleanup", true, false);
    if let Err(error) = remove_journal(&plan.repository.common_dir, &state.topic_identity) {
        let mut outcome = failure(
            effects,
            diagnostics,
            MergeExecutionFailureKind::JournalCleanup,
            error,
        );
        outcome.destination = destination;
        return outcome;
    }
    mark_effect(&mut effects, "journal_cleanup", true, true);
    let mut context = plan.context.clone();
    context.phase = MergePhase::Complete;
    context.cleanup_pending = false;
    context.journaled = false;
    MergeExecutionOutcome {
        context,
        effects,
        diagnostics,
        destination,
        failure: None,
    }
}

/// Integrates the current topic branch into the resolved target branch.
///
/// # Errors
///
/// Returns an error when merge preconditions, hooks, Git execution, or cleanup fails.
pub fn merge(no_rebase: bool, no_remove: bool, no_squash: bool) -> Result<()> {
    let input = MergeInput {
        no_rebase,
        no_remove,
        no_squash,
        dry_run: false,
    };
    run_human_merge(journaled_merge::MergeRequest::ordinary(&input))
}

fn run_human_merge(request: journaled_merge::MergeRequest) -> Result<()> {
    let mut preparation = journaled_merge::prepare(request);
    loop {
        match preparation {
            journaled_merge::Preparation::Ready(prepared) => {
                return finish_human_merge(
                    &prepared.run(&journaled_merge::MergeExecutionOutput::Human),
                );
            }
            journaled_merge::Preparation::ApprovalRequired(pending) => {
                if pending.requirement() == journaled_merge::ApprovalRequirement::SquashGenerator {
                    return pending.approve_interactively();
                }
                pending.approve_interactively()?;
                preparation = pending.reprepare();
            }
            journaled_merge::Preparation::Complete(outcome) => {
                return finish_human_merge(&outcome);
            }
        }
    }
}

fn finish_human_merge(outcome: &MergeOutcome) -> Result<()> {
    let epilogue = match &outcome.result {
        Ok(MergeResult::InPlace { .. }) => "; primary worktree switched to target.",
        Ok(MergeResult::Retained { .. }) => "; worktree retained.",
        Ok(MergeResult::Removed { .. }) => "; worktree removed.",
        Ok(MergeResult::DryRun { .. }) => {
            unreachable!("human merge execution is not a dry run")
        }
        Err(error) => bail!(error.message.clone()),
    };
    let context = match &outcome.context {
        MergeOutcomeContext::Lifecycle(context)
        | MergeOutcomeContext::Approval {
            lifecycle: context, ..
        }
        | MergeOutcomeContext::Completed {
            initial: context, ..
        } => context,
        MergeOutcomeContext::Unavailable {} => {
            unreachable!("a successful merge always has lifecycle context")
        }
    };
    let squashed = outcome
        .effects
        .iter()
        .find(|effect| effect.action == "squash")
        .is_some_and(|effect| effect.completed);
    ui::finish(styled_merge_summary(
        &context.source_branch,
        &context.target_branch,
        squashed,
        epilogue,
    ))
}

/// Integrates local changes directly into one generated squash commit.
///
/// # Errors
///
/// Returns an error when merge preconditions, hooks, Git execution, or cleanup fails.
pub fn merge_yolo(no_rebase: bool, no_remove: bool) -> Result<()> {
    run_human_merge(journaled_merge::MergeRequest::include_all(&MergeInput {
        no_rebase,
        no_remove,
        no_squash: false,
        dry_run: false,
    }))
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

fn styled_merge_summary(source: &str, target: &str, squashed: bool, epilogue: &str) -> String {
    format!(
        "{} {} {} {}{}",
        ui::success_style().apply_to(if squashed {
            "Squashed and merged"
        } else {
            "Merged"
        }),
        ui::worktree_data_style().apply_to(source),
        ui::success_style().apply_to("into"),
        ui::worktree_data_style().apply_to(target),
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
    let repository = RepositoryObservation::new(&cwd).repository()?;
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
    let target_context = targets
        .iter()
        .map(|target| RemovalTargetContext {
            branch: target.worktree.branch_label().to_owned(),
            path: BytePath::path(&target.worktree.path),
            branch_retained: true,
            current: target.worktree.path == current,
            force,
            pre_remove_hooks: target.config.pre_remove.len(),
        })
        .collect::<Vec<_>>();
    let destination = target_context
        .iter()
        .any(|target| target.current)
        .then(|| BytePath::path(&primary));
    let mut effects = Vec::with_capacity(targets.len() * 2 + usize::from(destination.is_some()));
    for target in &target_context {
        let details = Some(serde_json::json!({
            "branch": target.branch,
            "path": target.path,
            "branch_retained": target.branch_retained,
        }));
        effects.push(Effect {
            action: "pre_remove_hooks".into(),
            attempted: false,
            completed: false,
            details: details.clone(),
        });
        effects.push(Effect {
            action: "remove_worktree".into(),
            attempted: false,
            completed: false,
            details,
        });
    }
    if let Some(destination) = &destination {
        effects.push(Effect {
            action: "destination".into(),
            attempted: false,
            completed: false,
            details: Some(serde_json::json!({"path": destination})),
        });
    }
    let context = RemovalContext {
        primary_worktree: BytePath::path(&primary),
        current_worktree: BytePath::path(&current),
        force,
        targets: target_context,
        destination,
    };
    Ok(RemovalPlan {
        repository,
        primary,
        current,
        targets,
        force,
        context,
        effects,
    })
}

fn select_removal_targets(
    repository: &Repository,
    branches: &[String],
    force: bool,
) -> Result<Vec<Worktree>> {
    let mut names = if branches.is_empty() {
        vec![repository.current_branch()?.to_owned()]
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
        vec![repository.current_branch()?.to_owned()]
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
    let identity = RepositoryObservation::new(&target.path).worktree_identity()?;
    let Some(state) = read_journal(&repository.common_dir, &identity)? else {
        return Ok(None);
    };
    if state.topic_identity != identity || state.topic_path != target.path {
        return Err(preflight(
            PreflightFailureKind::JournalInvalid,
            format!(
                "lifecycle journal for {} is malformed or does not match its worktree identity",
                target.path.display()
            ),
        )
        .into());
    }
    if state.cleanup_pending || LifecycleMutation::new(&target.path).rebase_in_progress()? {
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
    if !force && HistoryObservation::new(&target.path).status()?.is_dirty() {
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
fn decode_journal(bytes: &[u8]) -> Result<MergeJournal> {
    let version: JournalVersion = serde_json::from_slice(bytes)
        .context("lifecycle journal must contain only a supported numeric version before its body is decoded")?;
    match version.version {
        1 => {
            let dto: MergeJournalV1 = serde_json::from_slice(bytes)?;
            if dto.version != 1 {
                bail!("lifecycle journal version changed during decoding")
            }
            let squash = if dto.squashed {
                dto.validated_source
                    .clone()
                    .map_or(SquashStateV2::LegacyCompleted, |resulting_commit| {
                        SquashStateV2::Completed { resulting_commit }
                    })
            } else if dto.no_squash {
                SquashStateV2::Skipped {
                    reason: SquashSkipReasonV2::Disabled,
                }
            } else {
                SquashStateV2::NotStarted
            };
            Ok(MergeJournal {
                topic_path: dto.topic_path,
                topic_identity: dto.topic_identity,
                source_branch: dto.source_branch,
                target_branch: dto.target_branch,
                no_rebase: dto.no_rebase,
                no_remove: dto.no_remove,
                no_squash: dto.no_squash,
                yolo_stage_all: dto.yolo_stage_all,
                squash,
                cleanup_pending: dto.cleanup_pending,
                validated_source: dto.validated_source,
                validated_target: dto.validated_target,
            })
        }
        2 => {
            let dto: MergeJournalV2 = serde_json::from_slice(bytes)?;
            if dto.version != 2 {
                bail!("lifecycle journal version changed during decoding")
            }
            if let SquashStateV2::Prepared { checkpoint } = &dto.squashed {
                decode_bytes(&checkpoint.message_bytes)?;
                decode_path(checkpoint.topic_worktree.clone())?;
            }
            Ok(MergeJournal {
                topic_path: decode_path(dto.topic_path)?,
                topic_identity: decode_path(dto.topic_identity)?,
                source_branch: dto.source_branch,
                target_branch: dto.target_branch,
                no_rebase: dto.no_rebase,
                no_remove: dto.no_remove,
                no_squash: dto.no_squash,
                yolo_stage_all: dto.yolo_stage_all,
                squash: dto.squashed,
                cleanup_pending: dto.cleanup_pending,
                validated_source: dto.validated_source,
                validated_target: dto.validated_target,
            })
        }
        version => bail!("unsupported lifecycle journal version {version}"),
    }
}

fn encode_path(path: &Path) -> EncodedPath {
    EncodedPath {
        encoding: ByteEncoding::Base64,
        value: BASE64.encode(path.as_os_str().as_bytes()),
        display: path.display().to_string(),
    }
}

fn decode_bytes(encoded: &EncodedBytes) -> Result<Vec<u8>> {
    let bytes = BASE64
        .decode(&encoded.value)
        .context("invalid base64 byte payload")?;
    if BASE64.encode(&bytes) != encoded.value {
        bail!("byte payload is not canonical base64")
    }
    Ok(bytes)
}

fn decode_path(path: EncodedPath) -> Result<PathBuf> {
    let bytes = decode_bytes(&EncodedBytes {
        encoding: path.encoding,
        value: path.value,
    })?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn squash_checkpoint_v2(checkpoint: &squash::PreparedCheckpoint) -> SquashCheckpointV2 {
    SquashCheckpointV2 {
        topic_commit: checkpoint.expected_topic_commit().to_owned(),
        target_commit: checkpoint.expected_target_commit().to_owned(),
        expected_tree: checkpoint.expected_result_tree().to_owned(),
        message_bytes: EncodedBytes {
            encoding: ByteEncoding::Base64,
            value: BASE64.encode(checkpoint.message().as_bytes()),
        },
        topic_branch: checkpoint.topic_branch().to_owned(),
        target_branch: checkpoint.target_branch().to_owned(),
        topic_worktree: encode_path(checkpoint.topic_worktree()),
        commit_count: checkpoint.commit_count(),
        include_staged: checkpoint.include_staged(),
    }
}

fn squash_checkpoint(checkpoint: &SquashCheckpointV2) -> Result<squash::PreparedCheckpoint> {
    Ok(squash::PreparedCheckpoint::from_persisted(
        decode_path(checkpoint.topic_worktree.clone())?,
        checkpoint.topic_branch.clone(),
        checkpoint.target_branch.clone(),
        checkpoint.topic_commit.clone(),
        checkpoint.target_commit.clone(),
        checkpoint.expected_tree.clone(),
        checkpoint.commit_count,
        checkpoint.include_staged,
        String::from_utf8(decode_bytes(&checkpoint.message_bytes)?)
            .context("prepared squash message is not valid UTF-8")?,
    ))
}

fn journal_v2(state: &MergeJournal) -> Result<MergeJournalV2> {
    if matches!(state.squash, SquashStateV2::LegacyCompleted) {
        bail!("cannot serialize an unbound version 1 completed squash")
    }
    Ok(MergeJournalV2 {
        version: 2,
        topic_path: encode_path(&state.topic_path),
        topic_identity: encode_path(&state.topic_identity),
        source_branch: state.source_branch.clone(),
        target_branch: state.target_branch.clone(),
        no_rebase: state.no_rebase,
        no_remove: state.no_remove,
        no_squash: state.no_squash,
        yolo_stage_all: state.yolo_stage_all,
        squashed: state.squash.clone(),
        cleanup_pending: state.cleanup_pending,
        validated_source: state.validated_source.clone(),
        validated_target: state.validated_target.clone(),
    })
}

fn bind_legacy_completed(mut state: MergeJournal) -> Result<MergeJournal> {
    if matches!(state.squash, SquashStateV2::LegacyCompleted) {
        let resulting_commit = HistoryObservation::new(&state.topic_path)
            .head_commit()
            .with_context(|| {
                format!(
                    "failed to bind version 1 completed squash to HEAD at {}",
                    state.topic_path.display()
                )
            })?;
        state.squash = SquashStateV2::Completed { resulting_commit };
    }
    Ok(state)
}

fn read_journal(common_dir: &Path, identity: &Path) -> Result<Option<MergeJournal>> {
    let path = journal_path(common_dir, identity);
    match fs::read(&path) {
        Ok(bytes) => decode_journal(&bytes)
            .and_then(bind_legacy_completed)
            .map(Some)
            .with_context(|| format!("failed to parse lifecycle journal {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read lifecycle journal {}", path.display())),
    }
}
fn write_journal(common_dir: &Path, state: &MergeJournal) -> Result<()> {
    let path = journal_path(common_dir, &state.topic_identity);
    let dto = journal_v2(state)?;
    trust::write_atomic(&path, &serde_json::to_vec_pretty(&dto)?)
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

#[cfg(test)]
mod journal_tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    use super::*;

    const V1: &str = r#"{"version":1,"topic_path":"/tmp/topic","topic_identity":"/tmp/id","source_branch":"topic","target_branch":"main","no_rebase":false,"no_remove":false,"no_squash":false,"yolo_stage_all":false,"squashed":false,"cleanup_pending":false,"validated_source":null,"validated_target":null}"#;
    const V2: &str = r#"{"version":2,"topic_path":{"encoding":"base64","value":"L3RtcC90b3BpYw==","display":"/tmp/topic"},"topic_identity":{"encoding":"base64","value":"L3RtcC9pZA==","display":"/tmp/id"},"source_branch":"topic","target_branch":"main","no_rebase":false,"no_remove":false,"no_squash":false,"yolo_stage_all":false,"squashed":{"state":"not_started"},"cleanup_pending":false,"validated_source":null,"validated_target":null}"#;

    #[test]
    fn decodes_exact_v1_and_v2_fixtures() {
        let v1 = decode_journal(V1.as_bytes()).unwrap();
        let v2 = decode_journal(V2.as_bytes()).unwrap();
        assert_eq!(v1.topic_path, Path::new("/tmp/topic"));
        assert_eq!(v2.topic_identity, Path::new("/tmp/id"));
    }

    #[test]
    fn new_write_has_exact_v2_fixture() {
        let state = decode_journal(V1.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_string(&journal_v2(&state).unwrap()).unwrap(),
            V2
        );
    }

    #[test]
    fn all_squash_states_survive_decode_and_reencode_exactly() {
        let shapes = [
            r#"{"state":"not_started"}"#,
            r#"{"state":"skipped","reason":"single_commit"}"#,
            r#"{"state":"prepared","checkpoint":{"topic_commit":"aaaa","target_commit":"bbbb","expected_tree":"cccc","message_bytes":{"encoding":"base64","value":"/wA="},"topic_branch":"topic","target_branch":"main","topic_worktree":{"encoding":"base64","value":"L3RtcC90b3BpYy3+","display":"informational path"},"commit_count":2,"include_staged":false}}"#,
            r#"{"state":"completed","resulting_commit":"deadbeef"}"#,
        ];

        for shape in shapes {
            let encoded = V2.replace(r#"{"state":"not_started"}"#, shape);
            let state = decode_journal(encoded.as_bytes()).unwrap();
            assert_eq!(
                serde_json::to_string(&journal_v2(&state).unwrap().squashed).unwrap(),
                shape
            );
        }
    }

    #[test]
    fn prepared_message_and_path_bytes_round_trip_exactly() {
        let shape = r#"{"state":"prepared","checkpoint":{"topic_commit":"aaaa","target_commit":"bbbb","expected_tree":"cccc","message_bytes":{"encoding":"base64","value":"/wA="},"topic_branch":"topic","target_branch":"main","topic_worktree":{"encoding":"base64","value":"L3RtcC90b3BpYy3+","display":"not derived from bytes"},"commit_count":2,"include_staged":false}}"#;
        let encoded = V2.replace(r#"{"state":"not_started"}"#, shape);
        let state = decode_journal(encoded.as_bytes()).unwrap();
        let SquashStateV2::Prepared { checkpoint } = &state.squash else {
            panic!("expected prepared squash state");
        };
        assert_eq!(decode_bytes(&checkpoint.message_bytes).unwrap(), b"\xff\0");
        assert_eq!(
            decode_path(checkpoint.topic_worktree.clone())
                .unwrap()
                .as_os_str()
                .as_bytes(),
            b"/tmp/topic-\xfe"
        );
        assert_eq!(
            serde_json::to_string(&journal_v2(&state).unwrap().squashed).unwrap(),
            shape
        );
    }

    #[test]
    fn skipped_reasons_survive_safe_writes() {
        for reason in ["disabled", "single_commit"] {
            let shape = format!(r#"{{"state":"skipped","reason":"{reason}"}}"#);
            let encoded = V2.replace(r#"{"state":"not_started"}"#, &shape);
            let state = decode_journal(encoded.as_bytes()).unwrap();
            assert_eq!(
                serde_json::to_string(&journal_v2(&state).unwrap().squashed).unwrap(),
                shape
            );
        }
    }

    #[test]
    fn new_journals_persist_policy_and_single_commit_skips() {
        let skipped = squash::Assessment::Skipped {
            reason: squash::SkipReason::SingleCommit,
            commit_count: 1,
        };
        assert_eq!(
            initial_squash_state(true, &skipped),
            SquashStateV2::Skipped {
                reason: SquashSkipReasonV2::Disabled
            }
        );
        assert_eq!(
            initial_squash_state(false, &skipped),
            SquashStateV2::Skipped {
                reason: SquashSkipReasonV2::SingleCommit
            }
        );
        assert_eq!(
            initial_squash_state(
                false,
                &squash::Assessment::Skipped {
                    reason: squash::SkipReason::Disabled,
                    commit_count: 0,
                },
            ),
            SquashStateV2::NotStarted
        );
    }

    #[test]
    fn version_one_reader_rejects_version_two_shape() {
        assert!(serde_json::from_str::<MergeJournalV1>(V2).is_err());
    }

    #[test]
    fn refuses_unknown_versions_fields_states_and_noncanonical_bytes() {
        assert!(decode_journal(V2.replace("\"version\":2", "\"version\":3").as_bytes()).is_err());
        assert!(
            decode_journal(
                V2.replace("\"state\":\"not_started\"", "\"state\":\"future\"")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(
            decode_journal(
                V2.replace(
                    "\"display\":\"/tmp/topic\"",
                    "\"display\":\"/tmp/topic\",\"extra\":true"
                )
                .as_bytes()
            )
            .is_err()
        );
        assert!(
            decode_journal(V2.replace("L3RtcC90b3BpYw==", "L3RtcC90b3BpYw").as_bytes()).is_err()
        );
    }

    #[test]
    fn v2_path_bytes_round_trip_without_using_display() {
        let path = PathBuf::from(OsString::from_vec(b"/tmp/topic-\xff".to_vec()));
        let mut encoded = encode_path(&path);
        encoded.display = "informational only".into();
        assert_eq!(decode_path(encoded).unwrap(), path);
    }

    #[test]
    fn prepared_checkpoint_requires_exact_canonical_bytes() {
        let prepared = V2.replace(
            r#"{"state":"not_started"}"#,
            r#"{"state":"prepared","checkpoint":{"topic_commit":"aaaa","target_commit":"bbbb","expected_tree":"cccc","message_bytes":{"encoding":"base64","value":"/w=="},"topic_branch":"topic","target_branch":"main","topic_worktree":{"encoding":"base64","value":"L3RtcC90b3BpYw==","display":"topic"},"commit_count":2,"include_staged":false}}"#,
        );
        let journal = decode_journal(prepared.as_bytes()).unwrap();
        assert!(journal.squash.prepared());
        assert!(decode_journal(prepared.replace("/w==", "/w").as_bytes()).is_err());
        assert!(
            decode_journal(
                prepared
                    .replace(
                        "\"topic_commit\":\"aaaa\"",
                        "\"topic_commit\":\"aaaa\",\"extra\":true"
                    )
                    .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn reading_v1_does_not_rewrite_it_until_a_safe_write() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Path::new("/tmp/id");
        let path = journal_path(directory.path(), identity);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, V1).unwrap();
        let state = read_journal(directory.path(), identity).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), V1);
        write_journal(directory.path(), &state).unwrap();
        assert_eq!(
            serde_json::from_slice::<JournalVersion>(&fs::read(path).unwrap())
                .unwrap()
                .version,
            2
        );
    }

    #[test]
    fn unvalidated_v1_completed_squash_binds_topic_head_before_safe_write() {
        let directory = tempfile::tempdir().unwrap();
        let topic = directory.path().join("topic");
        fs::create_dir(&topic).unwrap();
        git::initialize_test_repository_with_commit(&topic).unwrap();
        let head = HistoryObservation::new(&topic).head_commit().unwrap();

        let identity = directory.path().join("topic-id");
        let path = journal_path(directory.path(), &identity);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut fixture = serde_json::from_str::<serde_json::Value>(V1).unwrap();
        fixture["topic_path"] = serde_json::Value::String(topic.display().to_string());
        fixture["topic_identity"] = serde_json::Value::String(identity.display().to_string());
        fixture["squashed"] = serde_json::Value::Bool(true);
        let original = serde_json::to_vec(&fixture).unwrap();
        fs::write(&path, &original).unwrap();

        let decoded = decode_journal(&original).unwrap();
        assert_eq!(decoded.squash, SquashStateV2::LegacyCompleted);
        assert!(journal_v2(&decoded).is_err());

        let state = read_journal(directory.path(), &identity).unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(state.squash.completed());
        assert!(!state.squash.not_started());
        assert_eq!(
            state.squash,
            SquashStateV2::Completed {
                resulting_commit: head.clone()
            }
        );

        write_journal(directory.path(), &state).unwrap();
        let written: MergeJournalV2 = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(written.version, 2);
        assert_eq!(
            written.squashed,
            SquashStateV2::Completed {
                resulting_commit: head
            }
        );
    }
}
