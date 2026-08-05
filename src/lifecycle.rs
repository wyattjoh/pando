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

/// Validated, read-only merge plan shared by command adapters.
#[derive(Debug)]
pub struct MergePlan {
    pub repository: Repository,
    pub context: MergeContext,
    pub config: EffectiveConfig,
    pub needs_rebase: bool,
    pub squash: squash::SquashPlan,
    /// Ordered lifecycle effects. Planning never attempts or completes one.
    pub effects: Vec<Effect>,
}

impl MergePlan {
    /// Whether this plan is the smallest clean lifecycle: integrate an already
    /// fast-forwardable topic and intentionally retain its worktree.
    #[must_use]
    pub const fn is_clean_retained(&self) -> bool {
        self.is_retained_execution() && !self.context.rebase_active && !self.needs_rebase
    }

    /// Whether the shared executor can run this integration lifecycle without
    /// removing a worktree.
    #[must_use]
    pub const fn is_retained_execution(&self) -> bool {
        (self.context.policy.no_remove || self.context.in_place) && !self.context.cleanup_pending
    }
}

/// Controls whether lifecycle transcripts are rendered immediately or returned
/// to a protocol adapter as diagnostics.
#[derive(Clone, Copy, Debug)]
pub enum MergeExecutionMode {
    Human,
    Captured,
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

/// Plans and executes one merge request as a command-owned protocol outcome.
///
/// This is the complete machine adapter seam. It owns planning and approval
/// inspection so protocol adapters never infer lifecycle state from Git or the
/// journal.
#[must_use]
pub fn execute_merge_request(input: &MergeInput, mode: MergeExecutionMode) -> MergeOutcome {
    let plan = match plan_merge(input.policy()) {
        Ok(plan) => plan,
        Err(error) => return merge_preflight_outcome(&error),
    };
    let approval = if plan.context.cleanup_pending {
        None
    } else {
        match hook_approval::evaluate(
            &plan.repository,
            HookPhase::PreMerge,
            &plan.config.pre_merge,
        ) {
            Ok(hook_approval::Evaluation::ApprovalRequired(candidate)) => Some(candidate),
            Ok(
                hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. },
            ) => None,
            Err(error) => {
                return MergeOutcome {
                    result: Err(MergeError {
                        code: "trust.read_failed".into(),
                        message: format!("{error:#}"),
                    }),
                    context: MergeOutcomeContext::Lifecycle(plan.context.clone()),
                    effects: plan.effects.clone(),
                    diagnostics: Vec::new(),
                    recovery: Vec::new(),
                };
            }
        }
    };
    merge_outcome(&plan, input, approval.as_ref(), mode)
}

/// Converts one validated merge plan into the command-owned protocol outcome.
///
/// Human presentation uses this lower seam after gathering any interactive
/// approval. Machine adapters must call [`execute_merge_request`] instead.
#[must_use]
#[allow(clippy::too_many_lines)] // The outcome records every version 1 lifecycle contract in one seam.
pub fn merge_outcome(
    plan: &MergePlan,
    input: &MergeInput,
    approval: Option<&hook_approval::Candidate>,
    mode: MergeExecutionMode,
) -> MergeOutcome {
    let squash_blocked = !plan.context.cleanup_pending && plan.squash.approval_required();
    let hook_blocked = if plan.context.cleanup_pending {
        !plan.context.pre_remove_hooks_trusted
    } else {
        approval.is_some()
    };
    let approval_blocked = squash_blocked || hook_blocked;
    let context = approval.map_or_else(
        || MergeOutcomeContext::Lifecycle(plan.context.clone()),
        |candidate| MergeOutcomeContext::Approval {
            lifecycle: plan.context.clone(),
            approval: merge_approval_context(candidate),
        },
    );
    if input.dry_run {
        return MergeOutcome {
            result: Ok(MergeResult::DryRun {
                plan: if plan.context.in_place {
                    "in_place"
                } else if input.no_remove {
                    "retained_topic"
                } else {
                    "cleanup"
                }
                .into(),
                policy: plan.context.policy,
                ready: !approval_blocked,
                approval_required: approval_blocked,
            }),
            context,
            effects: plan.effects.clone(),
            diagnostics: Vec::new(),
            recovery: if approval_blocked {
                vec![merge_trust_recovery(plan, false)]
            } else {
                Vec::new()
            },
        };
    }
    if approval_blocked {
        return MergeOutcome {
            result: Err(MergeError {
                code: if squash_blocked {
                    "merge.squash_approval_required"
                } else {
                    "merge.hook_approval_required"
                }
                .into(),
                message: if squash_blocked {
                    "the shared squash message generator is not trusted"
                } else {
                    "configured lifecycle hooks are not trusted"
                }
                .into(),
            }),
            context,
            effects: plan.effects.clone(),
            diagnostics: Vec::new(),
            recovery: vec![merge_trust_recovery(plan, squash_blocked)],
        };
    }
    let execution = execute_merge(plan, mode);
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
pub fn plan_merge(policy: MergePolicy) -> PreflightResult<MergePlan> {
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
        if state.version != 1
            || state.topic_path != repository.current().path
            || state.topic_identity != identity
        {
            return Err(anyhow::anyhow!(
                "lifecycle journal is unsupported or does not match the current topic worktree"
            )
            .into());
        }
        if state.no_rebase != policy.no_rebase
            || state.no_remove != policy.no_remove
            || state.no_squash != policy.no_squash
        {
            return Err(preflight(
                PreflightFailureKind::PolicyConflict,
                "merge retry flags conflict with the journaled lifecycle policy; rerun with the original flags",
            ));
        }
    }
    if !rebase_active && current_history.status()?.is_dirty() {
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
        && !journal.as_ref().is_some_and(|state| state.squashed);
    let squash = squash::plan(
        &repository,
        &config,
        &target,
        squash_enabled,
        !rebase_active,
        false,
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
        squashes: squash.applicable,
        squash_commits: squash.commit_count,
        squash_generator_configured: squash.generator_configured,
        squash_generator_trusted: squash.generator_trusted,
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
    /// Stage local changes into the generated squash commit.
    #[serde(default)]
    yolo_stage_all: bool,
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

#[derive(Clone, Copy)]
enum MergeWorktreeOutcome {
    Retained,
    Removed,
    SwitchedInPlace,
}

#[derive(Clone, Copy)]
enum MergeIntent {
    Normal,
    StageAll,
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
    let plan = plan_merge(MergePolicy::new(no_rebase, no_remove, no_squash))?;
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
    } else if plan.context.policy.no_remove {
        " and retain the topic worktree".to_owned()
    } else {
        " and remove the topic worktree".to_owned()
    };
    ui::finish(format!(
        "Would merge {} into {}{follow_up}; no changes made.",
        plan.context.source_branch, plan.context.target_branch,
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
    mode: MergeExecutionMode,
    diagnostics: &mut Vec<MergeDiagnostic>,
) -> Result<()> {
    let policy = match mode {
        MergeExecutionMode::Human => hook::OutputPolicy::Streamed,
        MergeExecutionMode::Captured => hook::OutputPolicy::Captured,
    };
    let execution = hook::execute(hook_phase, steps, destination, policy)?;
    if let hook::HookOutput::Captured(output) = execution.output {
        for step in output {
            push_captured_merge_diagnostic(diagnostics, diagnostic_phase, "stdout", step.stdout);
            push_captured_merge_diagnostic(diagnostics, diagnostic_phase, "stderr", step.stderr);
        }
    }
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
pub fn execute_merge(plan: &MergePlan, mode: MergeExecutionMode) -> MergeExecutionOutcome {
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
            version: 1,
            topic_path: current.path.clone(),
            topic_identity: identity,
            source_branch: plan.context.source_branch.clone(),
            target_branch: plan.context.target_branch.clone(),
            no_rebase: plan.context.policy.no_rebase,
            no_remove: plan.context.policy.no_remove,
            no_squash: plan.context.policy.no_squash,
            yolo_stage_all: false,
            squashed: false,
            cleanup_pending: false,
            validated_source: None,
            validated_target: None,
        }
    };
    if plan.context.cleanup_pending {
        return execute_merge_cleanup(plan, &state, effects, diagnostics, mode);
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

    if plan.needs_rebase || plan.context.rebase_active {
        mark_effect(&mut effects, "rebase", true, false);
        let rebase_result = match (mode, plan.context.rebase_active) {
            (MergeExecutionMode::Human, true) => ui::run_timed(
                true,
                "Continuing rebase...",
                "Continued rebase",
                "Failed to continue the rebase",
                |animated| {
                    LifecycleMutation::new(&current.path).continue_rebase(output_for(animated))
                },
            )
            .and_then(|transcript| {
                report(&transcript)?;
                Ok(transcript)
            }),
            (MergeExecutionMode::Human, false) => ui::run_timed(
                true,
                &format!("Rebasing onto {}...", state.target_branch),
                &format!("Rebased onto {}", state.target_branch),
                &format!("Failed to rebase onto {}", state.target_branch),
                |animated| {
                    LifecycleMutation::new(&current.path)
                        .rebase_onto(&state.target_branch, output_for(animated))
                },
            )
            .and_then(|transcript| {
                report(&transcript)?;
                Ok(transcript)
            }),
            (MergeExecutionMode::Captured, true) => {
                LifecycleMutation::new(&current.path).continue_rebase(LifecycleOutput::Captured)
            }
            (MergeExecutionMode::Captured, false) => LifecycleMutation::new(&current.path)
                .rebase_onto(&state.target_branch, LifecycleOutput::Captured),
        };
        match rebase_result {
            Ok(transcript) => {
                if matches!(mode, MergeExecutionMode::Captured) && !transcript.is_empty() {
                    push_merge_diagnostic(
                        &mut diagnostics,
                        "rebase",
                        "stderr",
                        transcript.as_bytes(),
                    );
                }
                mark_effect(&mut effects, "rebase", true, true);
            }
            Err(error) => {
                if matches!(mode, MergeExecutionMode::Captured) {
                    push_merge_diagnostic(
                        &mut diagnostics,
                        "rebase",
                        "stderr",
                        error.to_string().as_bytes(),
                    );
                }
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
    if plan.squash.applicable && !state.squashed {
        mark_effect(&mut effects, "squash", true, false);
        let refreshed = match squash::plan(
            &plan.repository,
            &plan.config,
            &state.target_branch,
            true,
            true,
            false,
        ) {
            Ok(refreshed) => refreshed,
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
        if refreshed.approval_required() || !refreshed.generator_configured {
            return execution_failure(
                plan,
                effects,
                diagnostics,
                MergePhase::Squash,
                MergeExecutionFailureKind::StalePlan,
                "squash generator readiness changed after merge planning; retry after resolving trust or configuration",
            );
        }
        if refreshed.applicable {
            let message = match mode {
                MergeExecutionMode::Human => ui::run_timed(
                    true,
                    "Generating squash commit message...",
                    "Generated squash commit message:",
                    "Failed to generate the squash commit message",
                    |_| {
                        squash::generate_message(
                            &plan.repository,
                            &plan.config,
                            &state.target_branch,
                            false,
                        )
                    },
                )
                .and_then(|message| {
                    ui::step(render::commit_message(&message))?;
                    Ok(message)
                }),
                MergeExecutionMode::Captured => squash::generate_message(
                    &plan.repository,
                    &plan.config,
                    &state.target_branch,
                    false,
                ),
            };
            let message = match message {
                Ok(message) => message,
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
            if matches!(mode, MergeExecutionMode::Captured) {
                push_merge_diagnostic(&mut diagnostics, "squash", "stderr", message.as_bytes());
            }
            let collapse = match mode {
                MergeExecutionMode::Human => ui::run_timed(
                    true,
                    &format!("Squashing {} commits...", refreshed.commit_count),
                    "Squashed the topic into a single commit",
                    "Failed to squash the topic",
                    |_| squash::collapse(&plan.repository, &state.target_branch, &message),
                ),
                MergeExecutionMode::Captured => {
                    squash::collapse(&plan.repository, &state.target_branch, &message)
                }
            };
            if let Err(error) = collapse {
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
            state.squashed = true;
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
        }
        mark_effect(&mut effects, "squash", true, true);
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
            mode,
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
        let switch_result = match mode {
            MergeExecutionMode::Human => ui::run_timed(
                true,
                &format!("Switching to {}...", state.target_branch),
                &format!("Switched to {}", state.target_branch),
                &format!("Failed to switch to {}", state.target_branch),
                |animated| {
                    LifecycleMutation::new(primary)
                        .switch_branch(&state.target_branch, output_for(animated))
                },
            )
            .and_then(|transcript| {
                report(&transcript)?;
                Ok(())
            }),
            MergeExecutionMode::Captured => LifecycleMutation::new(primary)
                .switch_branch(&state.target_branch, LifecycleOutput::Captured)
                .map(|_| ()),
        };
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
    let merge_result = match mode {
        MergeExecutionMode::Human => ui::run_timed(
            true,
            &format!("Merging into {}...", state.target_branch),
            &format!("Merged into {}", state.target_branch),
            &format!("Failed to merge into {}", state.target_branch),
            |animated| {
                LifecycleMutation::new(primary)
                    .fast_forward(&state.source_branch, output_for(animated))
            },
        )
        .and_then(|transcript| {
            report(&transcript)?;
            Ok(transcript)
        }),
        MergeExecutionMode::Captured => LifecycleMutation::new(primary)
            .fast_forward(&state.source_branch, LifecycleOutput::Captured),
    };
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
    if matches!(mode, MergeExecutionMode::Captured) && !transcript.is_empty() {
        push_merge_diagnostic(
            &mut diagnostics,
            "integration",
            "stderr",
            transcript.as_bytes(),
        );
    }
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
        return execute_merge_cleanup(plan, &state, effects, diagnostics, mode);
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
    mode: MergeExecutionMode,
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
        mode,
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
    let removal = match mode {
        MergeExecutionMode::Human => mutation
            .remove(
                &state.topic_path,
                git::RemovalMode::Safe,
                git::RemovalOutput::Displayed,
            )
            .map(|_| ()),
        MergeExecutionMode::Captured => mutation
            .remove(
                &state.topic_path,
                git::RemovalMode::Safe,
                git::RemovalOutput::Captured,
            )
            .and_then(|output| {
                let output = output.expect("captured removal returns diagnostics");
                push_merge_diagnostic(&mut diagnostics, "cleanup", "stdout", &output.stdout);
                push_merge_diagnostic(&mut diagnostics, "cleanup", "stderr", &output.stderr);
                if output.status.success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("git worktree remove failed"))
                }
            }),
    };
    if let Err(error) = removal {
        if matches!(mode, MergeExecutionMode::Human) {
            push_merge_diagnostic(
                &mut diagnostics,
                "cleanup",
                "stderr",
                error.to_string().as_bytes(),
            );
        }
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
    merge_inner(
        MergePolicy::new(no_rebase, no_remove, no_squash),
        MergeIntent::Normal,
    )
}

/// Integrates local changes directly into one generated squash commit.
///
/// # Errors
///
/// Returns an error when merge preconditions, hooks, Git execution, or cleanup fails.
pub fn merge_yolo(no_rebase: bool, no_remove: bool) -> Result<()> {
    merge_inner(
        MergePolicy::new(no_rebase, no_remove, false),
        MergeIntent::StageAll,
    )
}

#[allow(clippy::too_many_lines)] // This is the explicit lifecycle state-machine boundary.
fn merge_inner(policy: MergePolicy, intent: MergeIntent) -> Result<()> {
    if matches!(intent, MergeIntent::Normal) {
        let plan = plan_merge(policy)?;
        if !plan.context.cleanup_pending && plan.squash.approval_required() {
            bail!(
                "shared squash message generator approval is required; run pando trust merge-approve, or rerun with --no-squash"
            );
        }
        if !plan.context.cleanup_pending {
            hook_approval::approve_interactively(
                &plan.repository,
                HookPhase::PreMerge,
                &plan.config.pre_merge,
            )?;
        }
        if plan.context.cleanup_pending || policy.removes_topic(plan.context.in_place) {
            hook_approval::approve_interactively(
                &plan.repository,
                HookPhase::PreRemove,
                &plan.config.pre_remove,
            )?;
        }
        // Approval changes trust state, so execution consumes a fresh
        // validated plan rather than the pre-approval snapshot.
        let plan = plan_merge(policy)?;
        let input = MergeInput {
            no_rebase: policy.no_rebase,
            no_remove: policy.no_remove,
            no_squash: policy.no_squash,
            dry_run: false,
        };
        let outcome = merge_outcome(&plan, &input, None, MergeExecutionMode::Human);
        let destination = match &outcome.result {
            Ok(
                MergeResult::InPlace { destination }
                | MergeResult::Removed { destination }
                | MergeResult::Retained { destination },
            ) => destination.is_some(),
            Ok(MergeResult::DryRun { .. }) | Err(_) => false,
        };
        if destination {
            write_destination(
                plan.repository
                    .primary
                    .as_ref()
                    .expect("a merge plan always has a primary worktree"),
            )?;
        }
        let epilogue = match &outcome.result {
            Ok(MergeResult::InPlace { .. }) => "; primary worktree switched to target.",
            Ok(MergeResult::Retained { .. }) => "; worktree retained.",
            Ok(MergeResult::Removed { .. }) => "; worktree removed.",
            Ok(MergeResult::DryRun { .. }) => {
                unreachable!("human merge execution is not a dry run")
            }
            Err(error) => bail!(error.message.clone()),
        };
        let squashed = outcome
            .effects
            .iter()
            .find(|effect| effect.action == "squash")
            .is_some_and(|effect| effect.completed);
        return ui::finish(styled_merge_summary(
            &plan.context.source_branch,
            &plan.context.target_branch,
            squashed,
            epilogue,
        ));
    }
    let yolo_stage_all = matches!(intent, MergeIntent::StageAll);
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = RepositoryObservation::new(&cwd).repository()?;
    let primary = repository
        .primary
        .as_ref()
        .context("cannot merge from a bare repository")?;
    let in_place = repository.current().path == *primary;
    let current_history = HistoryObservation::new(&repository.current().path);
    let primary_history = HistoryObservation::new(primary);
    let identity = RepositoryObservation::new(&repository.current().path).worktree_identity()?;
    let mut journal = read_journal(&repository.common_dir, &identity)?;
    let rebase_active = LifecycleMutation::new(&repository.current().path).rebase_in_progress()?;
    let source = match &journal {
        Some(state) => state.source_branch.clone(),
        None => repository.current_branch()?.to_owned(),
    };
    let dirty = !rebase_active && current_history.status()?.is_dirty();
    if dirty && !yolo_stage_all {
        bail!("the topic worktree has local changes; commit or discard them before merging");
    }
    if yolo_stage_all && journal.is_none() && !dirty {
        bail!("nothing to commit");
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
        if existing.no_rebase != policy.no_rebase
            || existing.no_remove != policy.no_remove
            || existing.no_squash != policy.no_squash
            || existing.yolo_stage_all != yolo_stage_all
        {
            bail!(
                "merge retry flags conflict with the journaled lifecycle policy; rerun with the original flags"
            );
        }
    }
    let snapshot = Snapshot::observe(&repository)?;
    let target = match &journal {
        Some(state) => state.target_branch.clone(),
        None => snapshot
            .target(config.target_branch.as_deref())
            .context("failed to resolve merge target")?,
    };
    snapshot.validate(&target)?;
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
    if !policy.no_squash && !journal.as_ref().is_some_and(|state| state.squashed) {
        squash::ensure_ready(
            &repository,
            &config,
            &target,
            !rebase_active,
            yolo_stage_all,
        )?;
    }
    // Preserve squash-generator preflight precedence, then approve validation
    // hooks before the journal or any Git mutation protected by them.
    hook_approval::approve_interactively(&repository, HookPhase::PreMerge, &config.pre_merge)?;
    if journal.is_none() {
        let state = MergeJournal {
            version: 1,
            topic_path: repository.current().path.clone(),
            topic_identity: identity,
            source_branch: source.clone(),
            target_branch: target.clone(),
            no_rebase: policy.no_rebase,
            no_remove: policy.no_remove,
            no_squash: policy.no_squash,
            yolo_stage_all,
            squashed: false,
            cleanup_pending: false,
            validated_source: None,
            validated_target: None,
        };
        write_journal(&repository.common_dir, &state)?;
        journal = Some(state);
    }

    if yolo_stage_all
        && journal.as_ref().is_some_and(|state| !state.squashed)
        && current_history.status()?.is_dirty()
    {
        // Preflight and journal creation must precede this mutation.
        ui::run_timed(
            true,
            "Staging all changes...",
            "Staged all changes",
            "Failed to stage changes",
            |_| LifecycleMutation::new(&repository.current().path).stage_all(),
        )?;
    }
    if rebase_active {
        report(&ui::run_timed(
            true,
            "Continuing rebase...",
            "Continued rebase",
            "Failed to continue the rebase",
            |animated| {
                LifecycleMutation::new(&repository.current().path)
                    .continue_rebase(output_for(animated))
            },
        )?)?;
    }
    if !current_history.is_ancestor(&target, &source)? {
        if policy.no_rebase {
            bail!(
                "the topic is not fast-forwardable onto {target:?}; rerun without --no-rebase to rebase it"
            );
        }
        report(&ui::run_timed(
            true,
            &format!("Rebasing onto {target}..."),
            &format!("Rebased onto {target}"),
            &format!("Failed to rebase onto {target}"),
            |animated| {
                if yolo_stage_all {
                    LifecycleMutation::new(&repository.current().path)
                        .rebase_onto_autostash(&target, output_for(animated))
                } else {
                    LifecycleMutation::new(&repository.current().path)
                        .rebase_onto(&target, output_for(animated))
                }
            },
        )?)?;
    }
    if yolo_stage_all
        && journal.as_ref().is_some_and(|state| !state.squashed)
        && current_history.status()?.is_dirty()
    {
        ui::run_timed(
            true,
            "Staging all changes...",
            "Staged all changes",
            "Failed to stage changes",
            |_| LifecycleMutation::new(&repository.current().path).stage_all(),
        )?;
    }
    let mut state = journal.context("lifecycle journal was not recorded before integration")?;
    // Squash after the rebase so the collapse starts from the replayed history,
    // and before validation so the pre-merge hooks see the commit that actually
    // lands on the target.
    if !state.no_squash && !state.squashed {
        let plan = squash::plan(&repository, &config, &target, true, true, yolo_stage_all)?;
        if plan.applicable {
            // Generate first and show the message, so the rail reports what is
            // about to be committed before the collapse rewrites history.
            let message = ui::run_timed(
                true,
                "Generating squash commit message...",
                "Generated squash commit message:",
                "Failed to generate the squash commit message",
                |_| squash::generate_message(&repository, &config, &target, yolo_stage_all),
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
    let refreshed = current_history.head_commit()?;
    let target_commit = primary_history.commit(&target)?;
    if state.validated_source.as_deref() != Some(&refreshed)
        || state.validated_target.as_deref() != Some(&target_commit)
    {
        let config = EffectiveConfig::load(&repository)?;
        hook_approval::approve_interactively(&repository, HookPhase::PreMerge, &config.pre_merge)?;
        execute_merge_hooks(
            HookPhase::PreMerge,
            "pre_merge",
            &config.pre_merge,
            &repository.current().path,
            MergeExecutionMode::Human,
            &mut Vec::new(),
        )?;
        if current_history.status()?.is_dirty() {
            bail!(
                "pre-merge hooks left the topic worktree dirty; restore cleanliness before retrying"
            );
        }
        if current_history.head_commit()? != refreshed {
            return merge_inner(policy, intent);
        }
        state.validated_source = Some(refreshed.clone());
        state.validated_target = Some(target_commit);
        write_journal(&repository.common_dir, &state)?;
    }
    if !current_history.is_ancestor(&target, &refreshed)? {
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
            |animated| LifecycleMutation::new(primary).switch_branch(&target, output_for(animated)),
        )?)?;
    }
    report(&ui::run_timed(
        true,
        &format!("Merging into {target}..."),
        &format!("Merged into {target}"),
        &format!("Failed to merge into {target}"),
        |animated| LifecycleMutation::new(primary).fast_forward(&source, output_for(animated)),
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
    hook_approval::approve_interactively(repository, HookPhase::PreRemove, &config.pre_remove)?;
    execute_merge_hooks(
        HookPhase::PreRemove,
        "pre_remove",
        &config.pre_remove,
        &state.topic_path,
        MergeExecutionMode::Human,
        &mut Vec::new(),
    )?;
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
    git::WorktreeMutation::new(primary).remove(
        &state.topic_path,
        git::RemovalMode::Safe,
        git::RemovalOutput::Displayed,
    )?;
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
    styled_merge_summary(
        &state.source_branch,
        &state.target_branch,
        state.squashed,
        &epilogue,
    )
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
