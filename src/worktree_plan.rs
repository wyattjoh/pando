//! Shared destination and source planning for worktree navigation and creation.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::protocol::{
    self, BytePath, Diagnostic, Effect, ErrorBody, MutationClass, RecoveryAction,
    RecoveryInvocation,
};

use crate::{
    Worktree, WorktreeKind,
    branch::{
        self, BaseResolution, Classification, ExactFetch, FETCH_HEAD_BASE, FETCH_LOCAL_BRANCH,
        FETCH_REGISTERED_WORKTREE, FETCH_REMOTE_BRANCH, Snapshot,
    },
    config::{EffectiveConfig, HookPhase},
    debug,
    git::{self, HistoryObservation, Repository, RepositoryObservation},
    hook::{self, CapturedStep, HookOutcome},
    hook_approval, setup, ui,
};

/// Stable error codes advertised by the switch protocol.
pub(crate) const SWITCH_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
    "repository.primary_unavailable",
    "repository.root_unavailable",
    "switch.selection_required",
    "switch.invalid_branch",
    "switch.config_invalid",
    "switch.fetch_not_applicable",
    "switch.base_unavailable",
    "switch.destination_unavailable",
    "switch.destination_invalid",
    "switch.destination_collision",
    "switch.irrelevant_remote",
    "switch.unknown_remote",
    "switch.remote_selection_required",
    "switch.approval_required",
    "switch.plan_stale",
    "switch.creation_failed",
    "switch.setup_failed",
    "switch.setup_incomplete",
    "trust.approval_required",
];

/// Stable error codes advertised by the create protocol.
pub(crate) const CREATE_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
    "repository.primary_unavailable",
    "repository.root_unavailable",
    "create.branch_required",
    "create.invalid_branch",
    "create.branch_registered",
    "create.config_invalid",
    "create.fetch_not_applicable",
    "create.base_unavailable",
    "create.destination_unavailable",
    "create.destination_invalid",
    "create.destination_collision",
    "create.irrelevant_remote",
    "create.unknown_remote",
    "create.remote_selection_required",
    "create.plan_stale",
    "create.creation_failed",
    "create.description_failed",
    "create.setup_failed",
    "trust.approval_required",
];

pub(crate) const SWITCH_ACTIONS: &[&str] = &["fetch_base_ref", "create_branch", "create_worktree"];
pub(crate) const CREATE_ACTIONS: &[&str] = &[
    "fetch_base_ref",
    "create_branch",
    "create_worktree",
    "set_branch_description",
];

/// Strict machine request for switching worktrees.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SwitchInput {
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) remote: Option<String>,
    #[serde(default)]
    pub(crate) fetch: bool,
    #[serde(default)]
    pub(crate) dry_run: bool,
}

/// Strict machine request for creating worktrees.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateInput {
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) remote: Option<String>,
    #[serde(default)]
    pub(crate) fetch: bool,
    #[serde(default)]
    pub(crate) dry_run: bool,
    #[serde(default)]
    pub(crate) description: Option<String>,
}

/// Adapter-neutral input shared by the switch and create operation boundary.
#[derive(Clone, Debug)]
pub(crate) struct OperationInput {
    pub(crate) branch: Option<String>,
    pub(crate) remote: Option<String>,
    pub(crate) fetch: bool,
    pub(crate) dry_run: bool,
    pub(crate) description: Option<String>,
}

impl From<SwitchInput> for OperationInput {
    fn from(input: SwitchInput) -> Self {
        Self {
            branch: input.branch,
            remote: input.remote,
            fetch: input.fetch,
            dry_run: input.dry_run,
            description: None,
        }
    }
}

impl From<CreateInput> for OperationInput {
    fn from(input: CreateInput) -> Self {
        Self {
            branch: input.branch,
            remote: input.remote,
            fetch: input.fetch,
            dry_run: input.dry_run,
            description: input.description,
        }
    }
}

/// Version 1 success variants shared by switch and create adapters.
#[derive(Debug, JsonSchema, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum OperationResult {
    Existing {
        branch: String,
        destination: BytePath,
        dry_run: bool,
    },
    #[serde(rename = "creation_plan")]
    NewBranchApproval {
        branch: String,
        destination: BytePath,
        kind: &'static str,
        start_point: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_ref: Option<String>,
        approval_required: bool,
    },
    CreationPlan {
        branch: String,
        destination: BytePath,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start_point: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_ref: Option<String>,
        remote: Option<String>,
    },
    Created {
        branch: String,
        destination: BytePath,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start_point: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        base_ref: Option<String>,
        remote: Option<String>,
    },
}

/// The public command intent whose policy differences affect planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Intent {
    Switch,
    Create,
}

impl Intent {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Create => "create",
        }
    }
}

/// The selected source for a navigation or creation operation.
#[derive(Clone, Debug)]
enum Source {
    Registered(Worktree),
    Local { commit: String },
    Remote { reference: String, commit: String },
    New { base: git::NewBranchBase },
}

/// Whether the caller requested the only network mutation supported by planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FetchIntent {
    None,
    Refresh,
    Preview,
}

impl FetchIntent {
    const fn new(fetch: bool, dry_run: bool) -> Self {
        match (fetch, dry_run) {
            (false, _) => Self::None,
            (true, false) => Self::Refresh,
            (true, true) => Self::Preview,
        }
    }

    const fn requested(self) -> bool {
        !matches!(self, Self::None)
    }

    const fn refreshes(self) -> bool {
        matches!(self, Self::Refresh)
    }
}

/// A deterministic plan with no terminal or JSON representation concerns.
#[derive(Debug)]
struct Plan {
    intent: Intent,
    branch: String,
    destination: PathBuf,
    source: Source,
    config: Option<EffectiveConfig>,
    description: Option<String>,
    fetch: FetchIntent,
    dry_run: bool,
}

/// The result of executing a navigation or creation plan.
#[derive(Debug)]
struct ExecutionOutcome {
    destination: PathBuf,
    effects: Vec<Effect>,
    hook_output: Vec<CapturedStep>,
}

/// A failed execution together with effects advanced only at real transitions.
#[derive(Debug)]
struct ExecutionFailure {
    code: &'static str,
    error: anyhow::Error,
    effects: Vec<Effect>,
    created: bool,
    setup_incomplete: bool,
    hook_outcome: Option<HookOutcome>,
    hook_output: Vec<CapturedStep>,
    entry: setup::EntryDisposition,
}

/// Complete command-owned outcome for a switch or create request.
#[derive(Debug)]
pub(crate) struct OperationOutcome {
    pub(crate) result: std::result::Result<OperationResult, OperationFailure>,
    pub(crate) context: OperationContext,
    pub(crate) effects: Vec<Effect>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) recovery: Vec<RecoveryAction<protocol::Request<RetryInput>>>,
    destination: Option<PathBuf>,
}

impl OperationOutcome {
    #[must_use]
    pub(crate) fn destination(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    #[must_use]
    pub(crate) fn failure_message(&self) -> Option<&str> {
        self.result
            .as_ref()
            .err()
            .map(|failure| failure.message.as_str())
    }

    #[must_use]
    pub(crate) fn setup_recovery_required(&self) -> bool {
        self.destination.is_none()
            && self
                .result
                .as_ref()
                .is_err_and(|failure| failure.code == "switch.setup_incomplete")
    }
}

#[derive(Debug, JsonSchema, Serialize)]
pub(crate) struct OperationFailure {
    code: String,
    message: String,
}

impl From<OperationFailure> for ErrorBody {
    fn from(value: OperationFailure) -> Self {
        Self {
            code: value.code,
            message: value.message,
        }
    }
}

#[derive(Debug, JsonSchema, Serialize)]
#[serde(untagged)]
pub(crate) enum OperationContext {
    Empty {},
    Selection(SwitchSelectionContext),
    Branch(BranchContext),
    Approval(ApprovalContext),
}

#[derive(Debug, JsonSchema, Serialize)]
pub(crate) struct SwitchSelectionContext {
    choices: Vec<SwitchChoice>,
    unregistered_branch: SelectionHint,
}

#[derive(Debug, JsonSchema, Serialize)]
struct SwitchChoice {
    branch: Option<String>,
    destination: BytePath,
    current: bool,
    last_commit_at: Option<String>,
    retry: RecoveryInvocation<protocol::Request<RetryInput>>,
}

#[derive(Debug, JsonSchema, Serialize)]
struct SelectionHint {
    description: &'static str,
}

#[derive(Debug, JsonSchema, Serialize)]
pub(crate) struct BranchContext {
    branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<BytePath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remotes: Option<Vec<String>>,
}

#[derive(Debug, JsonSchema, Serialize)]
pub(crate) struct ApprovalContext {
    approval: HookApprovalContext,
    branch: String,
    destination: BytePath,
}

#[derive(Debug, JsonSchema, Serialize)]
struct HookApprovalContext {
    phase: String,
    commands: Vec<HookApprovalCommand>,
    repository: String,
    identity: String,
}

#[derive(Debug, JsonSchema, Serialize)]
struct HookApprovalCommand {
    name: Option<String>,
    command: String,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub(crate) struct RetryInput {
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
}

/// A caller decision or safety condition that prevents an executable plan.
#[derive(Debug)]
enum Blocker {
    InvalidBranch {
        message: String,
    },
    ConfigInvalid {
        message: String,
    },
    RegisteredForCreate {
        worktree: Worktree,
    },
    DestinationUnavailable {
        worktree: Worktree,
    },
    PrimaryUnavailable,
    RootUnavailable {
        message: String,
    },
    DestinationInvalid {
        message: String,
    },
    DestinationCollision,
    DestinationNotIgnored {
        first: String,
        gitignore: PathBuf,
    },
    IrrelevantRemote,
    UnknownRemote,
    RemoteSelectionRequired {
        remotes: Vec<String>,
        destination: PathBuf,
    },
    FetchNotApplicable {
        message: String,
    },
    BaseUnavailable {
        message: String,
    },
    ApprovalRequired {
        candidate: hook_approval::Candidate,
        destination: PathBuf,
    },
}

/// Presentation policy for the concrete topic worktree operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Delivery {
    Human,
    Captured,
}

/// Renderable facts for a genuinely new branch decision.
#[derive(Debug)]
pub(crate) struct NewBranchFacts {
    pub(crate) branch: String,
    pub(crate) destination: PathBuf,
    pub(crate) source: String,
    pub(crate) base_ref: Option<String>,
    pub(crate) dirty_source: bool,
    pub(crate) fetch_output: Option<String>,
}

/// Opaque, single-use authority produced from current repository facts.
#[derive(Debug)]
pub(crate) struct PreparedOperation {
    repository: Repository,
    plan: Plan,
    input: OperationInput,
}

/// An explicit human decision for recovering a registered worktree's setup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetupRecoveryDecision {
    Retry,
    EnterOnce,
    MarkComplete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetachedSetupAction {
    NoHooks,
    Recover(SetupRecoveryDecision),
}

/// Opaque command-owned authority for navigating one detached worktree.
#[derive(Debug)]
pub(crate) struct PreparedDetachedNavigation {
    repository: Repository,
    destination: PathBuf,
    action: DetachedSetupAction,
}

/// Typed result for the picker-only detached-worktree navigation path.
#[derive(Debug)]
pub(crate) struct DetachedNavigationOutcome {
    destination: Option<PathBuf>,
    failure: Option<String>,
}

impl DetachedNavigationOutcome {
    #[must_use]
    pub(crate) fn destination(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    #[must_use]
    pub(crate) fn failure_message(&self) -> Option<&str> {
        self.failure.as_deref()
    }
}

/// Fresh detached-worktree preparation after an optional human recovery choice.
#[derive(Debug)]
pub(crate) enum DetachedNavigationPreparation {
    Complete(DetachedNavigationOutcome),
    RecoveryRequired,
    ApprovalRequired(hook_approval::Candidate),
    Ready(PreparedDetachedNavigation),
}

/// Opaque, single-use authority for one freshly prepared setup recovery.
#[derive(Debug)]
pub(crate) struct PreparedSetupRecovery {
    prepared: Box<PreparedOperation>,
    decision: SetupRecoveryDecision,
}

#[derive(Debug)]
enum SetupRecoveryPreparation {
    ApprovalRequired(hook_approval::Candidate),
    Ready(PreparedSetupRecovery),
    Complete(Box<OperationOutcome>),
}

impl SetupRecoveryPreparation {
    fn complete(outcome: OperationOutcome) -> Self {
        Self::Complete(Box::new(outcome))
    }
}

/// Read-only preparation result consumed by both human and JSON adapters.
#[derive(Debug)]
pub(crate) enum Preparation {
    Complete(OperationOutcome),
    RemoteSelection {
        remotes: Vec<String>,
        destination: PathBuf,
        outcome: OperationOutcome,
    },
    ApprovalRequired {
        candidate: hook_approval::Candidate,
        outcome: OperationOutcome,
    },
    NewBranch {
        facts: NewBranchFacts,
        outcome: OperationOutcome,
    },
    SetupRecoveryRequired {
        outcome: OperationOutcome,
    },
    SetupRecoveryReady(PreparedSetupRecovery),
    Ready(Box<PreparedOperation>),
}

/// Prepares the topic worktree operation from current repository facts.
#[allow(clippy::too_many_lines)]
pub(crate) fn prepare(
    intent: Intent,
    input: &OperationInput,
    authorize_new: bool,
    setup_decision: Option<SetupRecoveryDecision>,
) -> Preparation {
    let failure = |code: &str, message: String| {
        Preparation::Complete(OperationOutcome {
            result: Err(OperationFailure {
                code: code.into(),
                message,
            }),
            context: OperationContext::Empty {},
            effects: Vec::new(),
            diagnostics: Vec::new(),
            recovery: Vec::new(),
            destination: None,
        })
    };
    let current_dir = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            return failure(
                "repository.invalid",
                format!("failed to read current directory: {error}"),
            );
        }
    };
    let observation = RepositoryObservation::new(&current_dir);
    let repository = match if input.branch.is_none() && intent == Intent::Switch {
        observation.repository_with_metadata()
    } else {
        observation.repository_for_navigation()
    } {
        Ok(repository) => repository,
        Err(error) => return failure("repository.invalid", format!("{error:#}")),
    };
    let working_directory = BytePath::path(&repository.current().path);
    let Some(branch) = input.branch.clone() else {
        if intent == Intent::Create {
            return failure(
                "create.branch_required",
                "create requires a branch name in input.branch".into(),
            );
        }
        let choices = repository
            .worktrees
            .iter()
            .filter(|worktree| worktree.navigable())
            .map(|worktree| {
                let branch = match &worktree.kind {
                    WorktreeKind::Branch(value) => Some(value.clone()),
                    _ => None,
                };
                SwitchChoice {
                    retry: RecoveryInvocation {
                        argv: vec![
                            "pando".into(),
                            "--input-output".into(),
                            "json".into(),
                            "switch".into(),
                        ],
                        stdin: Some(protocol::Request {
                            schema_version: protocol::SCHEMA_VERSION,
                            request_id: None,
                            input: RetryInput {
                                branch: branch.clone(),
                                remote: None,
                                fetch: None,
                                dry_run: None,
                            },
                        }),
                        working_directory: Some(working_directory.clone()),
                    },
                    branch,
                    destination: BytePath::path(&worktree.path),
                    current: worktree.current,
                    last_commit_at: worktree.machine_last_commit_at(),
                }
            })
            .collect();
        let diagnostics = repository
            .metadata_warning
            .as_ref()
            .map_or_else(Vec::new, |warning| {
                vec![bounded_diagnostic(
                    "git.commit_metadata",
                    "metadata",
                    warning.as_bytes(),
                )]
            });
        return Preparation::Complete(OperationOutcome {
            result: Err(OperationFailure {
                code: "switch.selection_required".into(),
                message:
                    "select a registered worktree, or provide a branch name to resolve or create"
                        .into(),
            }),
            context: OperationContext::Selection(SwitchSelectionContext {
                choices,
                unregistered_branch: SelectionHint {
                    description: "provide input.branch with a branch name not shown above",
                },
            }),
            effects: Vec::new(),
            diagnostics,
            recovery: Vec::new(),
            destination: None,
        });
    };
    let fetch = FetchIntent::new(input.fetch, input.dry_run);
    let registered = registered_plan(
        &repository,
        intent,
        &branch,
        input.remote.as_deref(),
        fetch,
        input.description.clone(),
        input.dry_run,
    );
    let planned = if let Some(registered) = registered {
        Ok(registered)
    } else {
        let snapshot = match Snapshot::observe(&repository) {
            Ok(snapshot) => snapshot,
            Err(error) => return failure("repository.invalid", format!("{error:#}")),
        };
        plan(
            &repository,
            &snapshot,
            intent,
            &branch,
            input.remote.as_deref(),
            fetch,
            input.description.clone(),
            input.dry_run,
        )
    };
    let plan = match planned {
        Ok(Ok(plan)) => plan,
        Ok(Err(Blocker::RemoteSelectionRequired {
            remotes,
            destination,
        })) => {
            let rendered_destination = destination.clone();
            let outcome = blocker_outcome(
                intent,
                Blocker::RemoteSelectionRequired {
                    remotes: remotes.clone(),
                    destination,
                },
                &branch,
                input,
                working_directory,
            );
            return Preparation::RemoteSelection {
                remotes,
                destination: rendered_destination,
                outcome,
            };
        }
        Ok(Err(Blocker::ApprovalRequired {
            candidate,
            destination,
        })) => {
            let outcome = approval_required_outcome(
                intent,
                &candidate,
                &destination,
                &branch,
                working_directory,
            );
            return Preparation::ApprovalRequired { candidate, outcome };
        }
        Ok(Err(blocker)) => {
            return Preparation::Complete(blocker_outcome(
                intent,
                blocker,
                &branch,
                input,
                working_directory,
            ));
        }
        Err(error) => return failure("repository.invalid", format!("{error:#}")),
    };
    if !input.dry_run && matches!(plan.source, Source::Registered(_)) {
        match registered_setup_requires_choice(&repository, &plan) {
            Ok(true) => {
                let Some(decision) = setup_decision else {
                    let outcome = incomplete_setup_outcome(
                        branch,
                        BytePath::path(&repository.current().path),
                    );
                    return Preparation::SetupRecoveryRequired { outcome };
                };
                let destination = plan.destination.clone();
                let prepared = Box::new(PreparedOperation {
                    repository,
                    plan,
                    input: input.clone(),
                });
                return match prepare_setup_recovery(prepared, decision) {
                    SetupRecoveryPreparation::ApprovalRequired(candidate) => {
                        let outcome = approval_required_outcome(
                            intent,
                            &candidate,
                            &destination,
                            &branch,
                            working_directory,
                        );
                        Preparation::ApprovalRequired { candidate, outcome }
                    }
                    SetupRecoveryPreparation::Ready(authority) => {
                        Preparation::SetupRecoveryReady(authority)
                    }
                    SetupRecoveryPreparation::Complete(outcome) => Preparation::Complete(*outcome),
                };
            }
            Ok(false) => {}
            Err(error) => return failure("repository.invalid", format!("{error:#}")),
        }
    }
    if let Source::New { base } = &plan.source
        && !authorize_new
    {
        let dirty_source = match HistoryObservation::new(&repository.current().path).status() {
            Ok(status) => status.is_dirty(),
            Err(error) => return failure("repository.invalid", format!("{error:#}")),
        };
        let facts = NewBranchFacts {
            branch: branch.clone(),
            destination: plan.destination.clone(),
            source: new_branch_source(&repository, base),
            base_ref: base.base_ref.as_ref().map(git::BaseRef::reference),
            dirty_source,
            fetch_output: base.fetch_output.clone(),
        };
        let outcome = if intent == Intent::Switch {
            if input.dry_run {
                OperationOutcome {
                    result: Ok(OperationResult::NewBranchApproval {
                        branch,
                        destination: BytePath::path(&plan.destination),
                        kind: "new",
                        start_point: base.commit.clone(),
                        base_ref: base.base_ref.as_ref().map(git::BaseRef::reference),
                        approval_required: true,
                    }),
                    context: OperationContext::Empty {},
                    effects: planned_effects(&plan),
                    diagnostics: Vec::new(),
                    recovery: Vec::new(),
                    destination: None,
                }
            } else {
                OperationOutcome {
                    result: Err(OperationFailure {
                        code: "switch.approval_required".into(),
                        message:
                            "creating a genuinely new branch requires a manual human invocation"
                                .into(),
                    }),
                    context: OperationContext::Empty {},
                    effects: Vec::new(),
                    diagnostics: Vec::new(),
                    recovery: Vec::new(),
                    destination: None,
                }
            }
        } else {
            OperationOutcome {
                result: Ok(OperationResult::CreationPlan {
                    branch,
                    destination: BytePath::path(&plan.destination),
                    kind: Some("new"),
                    start_point: Some(base.commit.clone()),
                    base_ref: base.base_ref.as_ref().map(git::BaseRef::reference),
                    remote: None,
                }),
                context: OperationContext::Empty {},
                effects: planned_effects(&plan),
                diagnostics: Vec::new(),
                recovery: Vec::new(),
                destination: None,
            }
        };
        return Preparation::NewBranch { facts, outcome };
    }
    Preparation::Ready(Box::new(PreparedOperation {
        repository,
        plan,
        input: input.clone(),
    }))
}

/// Completes prepared work without granting a noninteractive adapter new authority.
#[must_use]
pub(crate) fn finish_noninteractive(preparation: Preparation) -> OperationOutcome {
    match preparation {
        Preparation::Complete(outcome)
        | Preparation::RemoteSelection { outcome, .. }
        | Preparation::ApprovalRequired { outcome, .. }
        | Preparation::NewBranch { outcome, .. }
        | Preparation::SetupRecoveryRequired { outcome } => outcome,
        Preparation::SetupRecoveryReady(authority) => {
            execute_setup_recovery(authority, Delivery::Captured)
        }
        Preparation::Ready(prepared) => execute_prepared(prepared, Delivery::Captured),
    }
}

/// Re-observes and prepares picker navigation to one detached worktree.
#[must_use]
pub(crate) fn prepare_detached_navigation(
    destination: &Path,
    decision: Option<SetupRecoveryDecision>,
) -> DetachedNavigationPreparation {
    let current_dir = match env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            return DetachedNavigationPreparation::Complete(detached_failure(format!(
                "failed to read current directory: {error}"
            )));
        }
    };
    let repository = match RepositoryObservation::new(&current_dir).repository_for_navigation() {
        Ok(repository) => repository,
        Err(error) => {
            return DetachedNavigationPreparation::Complete(detached_failure(format!("{error:#}")));
        }
    };
    let destination = match destination.canonicalize() {
        Ok(destination) => destination,
        Err(error) => {
            return DetachedNavigationPreparation::Complete(detached_failure(format!(
                "failed to resolve path {}: {error}",
                destination.display()
            )));
        }
    };
    let Some(worktree) = repository
        .worktrees
        .iter()
        .find(|worktree| worktree.path == destination && worktree.navigable())
    else {
        return DetachedNavigationPreparation::Complete(detached_failure(
            "the selected detached worktree is no longer registered or navigable".into(),
        ));
    };
    if !matches!(worktree.kind, WorktreeKind::Detached) {
        return DetachedNavigationPreparation::Complete(detached_failure(
            "picker detached navigation requires a detached worktree".into(),
        ));
    }
    let identity = match RepositoryObservation::new(&destination).worktree_identity() {
        Ok(identity) => identity,
        Err(error) => {
            return DetachedNavigationPreparation::Complete(detached_failure(format!("{error:#}")));
        }
    };
    let lifecycle = setup::Lifecycle::new(&repository.common_dir);
    let incomplete = match lifecycle.inspect(setup::SetupTarget {
        worktree_identity: &identity,
        branch: None,
    }) {
        Ok(setup::Inspection::Complete(transition)) => {
            debug_assert_eq!(transition.entry, setup::EntryDisposition::Enter);
            return DetachedNavigationPreparation::Complete(detached_success(destination));
        }
        Ok(setup::Inspection::Incomplete(incomplete)) => incomplete,
        Err(failure) => {
            return DetachedNavigationPreparation::Complete(detached_failure(format!(
                "{:#}",
                failure.error
            )));
        }
    };
    let config = match EffectiveConfig::load(&repository) {
        Ok(config) => config,
        Err(error) => {
            return DetachedNavigationPreparation::Complete(detached_failure(format!("{error:#}")));
        }
    };
    let action = if config.post_create.is_empty() {
        DetachedSetupAction::NoHooks
    } else {
        let Some(decision) = decision else {
            drop(incomplete);
            return DetachedNavigationPreparation::RecoveryRequired;
        };
        if decision == SetupRecoveryDecision::Retry {
            match hook_approval::evaluate(&repository, HookPhase::PostCreate, &config.post_create) {
                Ok(hook_approval::Evaluation::ApprovalRequired(candidate)) => {
                    drop(incomplete);
                    return DetachedNavigationPreparation::ApprovalRequired(candidate);
                }
                Ok(
                    hook_approval::Evaluation::NoCommands
                    | hook_approval::Evaluation::Trusted { .. },
                ) => {}
                Err(error) => {
                    return DetachedNavigationPreparation::Complete(detached_failure(format!(
                        "{error:#}"
                    )));
                }
            }
        }
        DetachedSetupAction::Recover(decision)
    };
    drop(incomplete);
    DetachedNavigationPreparation::Ready(PreparedDetachedNavigation {
        repository,
        destination,
        action,
    })
}

/// Consumes freshly prepared detached navigation and owns setup mutation and destination authority.
#[must_use]
#[allow(clippy::too_many_lines)] // One command-owned transition preserves every detached recovery outcome.
pub(crate) fn execute_detached_navigation(
    prepared: PreparedDetachedNavigation,
    delivery: Delivery,
) -> DetachedNavigationOutcome {
    let PreparedDetachedNavigation {
        repository,
        destination,
        action,
    } = prepared;
    let identity = match RepositoryObservation::new(&destination).worktree_identity() {
        Ok(identity) => identity,
        Err(error) => return detached_failure(format!("{error:#}")),
    };
    let lifecycle = setup::Lifecycle::new(&repository.common_dir);
    let incomplete = match lifecycle.inspect(setup::SetupTarget {
        worktree_identity: &identity,
        branch: None,
    }) {
        Ok(setup::Inspection::Complete(_)) => return detached_success(destination),
        Ok(setup::Inspection::Incomplete(incomplete)) => incomplete,
        Err(failure) => return detached_failure(format!("{:#}", failure.error)),
    };
    let config = match EffectiveConfig::load(&repository) {
        Ok(config) => config,
        Err(error) => return detached_failure(format!("{error:#}")),
    };
    if config.post_create.is_empty() {
        return match incomplete.no_hooks_configured() {
            Ok(_) => detached_success(destination),
            Err(failure) => detached_transition_failure(&failure, destination),
        };
    }
    let DetachedSetupAction::Recover(decision) = action else {
        return detached_failure(
            "post-create setup configuration changed; choose a recovery again".into(),
        );
    };
    match decision {
        SetupRecoveryDecision::Retry => {
            match hook_approval::evaluate(&repository, HookPhase::PostCreate, &config.post_create) {
                Ok(hook_approval::Evaluation::Trusted { .. }) => {}
                Ok(hook_approval::Evaluation::NoCommands) => {
                    return match incomplete.no_hooks_configured() {
                        Ok(_) => detached_success(destination),
                        Err(failure) => detached_transition_failure(&failure, destination),
                    };
                }
                Ok(hook_approval::Evaluation::ApprovalRequired(_)) => {
                    return detached_failure(
                        "post-create hooks require fresh approval before setup recovery".into(),
                    );
                }
                Err(error) => return detached_failure(format!("{error:#}")),
            }
            let mut observations = if delivery == Delivery::Human {
                hook::Observations::human()
            } else {
                hook::Observations::captured()
            };
            let execution = hook::execute(
                HookPhase::PostCreate,
                &config.post_create,
                &destination,
                &mut observations,
            );
            drop(observations.finish());
            let (attempt, outcome) = match execution {
                Ok(execution) => (Ok(execution.outcome), Some(execution.outcome)),
                Err(error) => (Err(error), None),
            };
            match incomplete.recovery_attempt(attempt) {
                Ok(_) => {
                    if delivery == Delivery::Human {
                        let _ = ui::finish("Post-create setup complete");
                    }
                    detached_success(destination)
                }
                Err(failure) => {
                    let message = if outcome == Some(HookOutcome::Interrupted) {
                        "post-create setup was interrupted; setup remains incomplete".into()
                    } else {
                        format!("{:#}", failure.error)
                    };
                    DetachedNavigationOutcome {
                        destination: (failure.transition.entry == setup::EntryDisposition::Enter)
                            .then_some(destination),
                        failure: Some(message),
                    }
                }
            }
        }
        SetupRecoveryDecision::EnterOnce => {
            if delivery == Delivery::Human {
                let _ = ui::warning("Entering once while setup remains incomplete.");
            }
            let transition = incomplete.enter_once();
            DetachedNavigationOutcome {
                destination: (transition.entry == setup::EntryDisposition::Enter)
                    .then_some(destination.clone()),
                failure: Some(format!(
                    "setup remains incomplete for {}",
                    destination.display()
                )),
            }
        }
        SetupRecoveryDecision::MarkComplete => match incomplete.mark_complete() {
            Ok(_) => {
                if delivery == Delivery::Human {
                    let _ = ui::finish("Marked setup complete");
                }
                detached_success(destination)
            }
            Err(failure) => detached_transition_failure(&failure, destination),
        },
    }
}

fn detached_transition_failure(
    failure: &setup::TransitionFailure,
    destination: PathBuf,
) -> DetachedNavigationOutcome {
    DetachedNavigationOutcome {
        destination: (failure.transition.entry == setup::EntryDisposition::Enter)
            .then_some(destination),
        failure: Some(format!("{:#}", failure.error)),
    }
}

fn detached_success(destination: PathBuf) -> DetachedNavigationOutcome {
    DetachedNavigationOutcome {
        destination: Some(destination),
        failure: None,
    }
}

fn detached_failure(message: String) -> DetachedNavigationOutcome {
    DetachedNavigationOutcome {
        destination: None,
        failure: Some(message),
    }
}

fn registered_setup_requires_choice(repository: &Repository, plan: &Plan) -> Result<bool> {
    let Source::Registered(worktree) = &plan.source else {
        return Ok(false);
    };
    let identity = RepositoryObservation::new(&worktree.path).worktree_identity()?;
    let lifecycle = setup::Lifecycle::new(&repository.common_dir);
    let incomplete = match lifecycle
        .inspect(setup::SetupTarget {
            worktree_identity: &identity,
            branch: Some(&plan.branch),
        })
        .map_err(|failure| failure.error)?
    {
        setup::Inspection::Complete(_) => return Ok(false),
        setup::Inspection::Incomplete(incomplete) => incomplete,
    };
    let config = EffectiveConfig::load(repository)?;
    let required = !config.post_create.is_empty();
    drop(incomplete);
    Ok(required)
}

/// Reprepares a selected human setup recovery from current repository and trust facts.
#[must_use]
fn prepare_setup_recovery(
    prepared: Box<PreparedOperation>,
    decision: SetupRecoveryDecision,
) -> SetupRecoveryPreparation {
    let Source::Registered(worktree) = &prepared.plan.source else {
        return SetupRecoveryPreparation::complete(simple_outcome(
            "repository.invalid",
            "setup recovery requires a registered worktree".into(),
        ));
    };
    let identity = match RepositoryObservation::new(&worktree.path).worktree_identity() {
        Ok(identity) => identity,
        Err(error) => {
            return SetupRecoveryPreparation::complete(simple_outcome(
                "repository.invalid",
                format!("{error:#}"),
            ));
        }
    };
    let lifecycle = setup::Lifecycle::new(&prepared.repository.common_dir);
    match lifecycle.inspect(setup::SetupTarget {
        worktree_identity: &identity,
        branch: Some(&prepared.plan.branch),
    }) {
        Ok(setup::Inspection::Complete(_)) => {
            return SetupRecoveryPreparation::complete(registered_outcome(&prepared.plan, false));
        }
        Ok(setup::Inspection::Incomplete(incomplete)) => drop(incomplete),
        Err(failure) => {
            return SetupRecoveryPreparation::complete(simple_outcome(
                "repository.invalid",
                format!("{:#}", failure.error),
            ));
        }
    }
    if decision == SetupRecoveryDecision::Retry {
        let config = match EffectiveConfig::load(&prepared.repository) {
            Ok(config) => config,
            Err(error) => {
                return SetupRecoveryPreparation::complete(simple_outcome(
                    "repository.invalid",
                    format!("{error:#}"),
                ));
            }
        };
        match hook_approval::evaluate(
            &prepared.repository,
            HookPhase::PostCreate,
            &config.post_create,
        ) {
            Ok(hook_approval::Evaluation::ApprovalRequired(candidate)) => {
                return SetupRecoveryPreparation::ApprovalRequired(candidate);
            }
            Ok(
                hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. },
            ) => {}
            Err(error) => {
                return SetupRecoveryPreparation::complete(simple_outcome(
                    "repository.invalid",
                    format!("{error:#}"),
                ));
            }
        }
    }
    SetupRecoveryPreparation::Ready(PreparedSetupRecovery { prepared, decision })
}

/// Consumes one freshly prepared registered-worktree setup recovery.
#[must_use]
#[allow(clippy::too_many_lines)] // One command-owned transition preserves every setup recovery outcome.
pub(crate) fn execute_setup_recovery(
    authority: PreparedSetupRecovery,
    delivery: Delivery,
) -> OperationOutcome {
    let PreparedSetupRecovery { prepared, decision } = authority;
    let prepared = *prepared;
    let Source::Registered(worktree) = &prepared.plan.source else {
        return simple_outcome(
            "repository.invalid",
            "setup recovery requires a registered worktree".into(),
        );
    };
    let destination = worktree.path.clone();
    let branch = prepared.plan.branch.clone();
    let identity = match RepositoryObservation::new(&destination).worktree_identity() {
        Ok(identity) => identity,
        Err(error) => return simple_outcome("repository.invalid", format!("{error:#}")),
    };
    let lifecycle = setup::Lifecycle::new(&prepared.repository.common_dir);
    let incomplete = match lifecycle.inspect(setup::SetupTarget {
        worktree_identity: &identity,
        branch: Some(&branch),
    }) {
        Ok(setup::Inspection::Complete(_)) => return registered_outcome(&prepared.plan, false),
        Ok(setup::Inspection::Incomplete(incomplete)) => incomplete,
        Err(failure) => {
            return simple_outcome("repository.invalid", format!("{:#}", failure.error));
        }
    };
    let config = match EffectiveConfig::load(&prepared.repository) {
        Ok(config) => config,
        Err(error) => return simple_outcome("repository.invalid", format!("{error:#}")),
    };
    if config.post_create.is_empty() {
        return match incomplete.no_hooks_configured() {
            Ok(_) => registered_outcome(&prepared.plan, false),
            Err(failure) => setup_recovery_failure(
                "switch.setup_failed",
                format!("{:#}", failure.error),
                branch,
                destination,
                failure.transition.entry,
                Vec::new(),
            ),
        };
    }
    match decision {
        SetupRecoveryDecision::Retry => {
            match hook_approval::evaluate(
                &prepared.repository,
                HookPhase::PostCreate,
                &config.post_create,
            ) {
                Ok(hook_approval::Evaluation::Trusted { .. }) => {}
                Ok(hook_approval::Evaluation::NoCommands) => {
                    return match incomplete.no_hooks_configured() {
                        Ok(_) => registered_outcome(&prepared.plan, false),
                        Err(failure) => setup_recovery_failure(
                            "switch.setup_failed",
                            format!("{:#}", failure.error),
                            branch,
                            destination,
                            failure.transition.entry,
                            Vec::new(),
                        ),
                    };
                }
                Ok(hook_approval::Evaluation::ApprovalRequired(_)) => {
                    return incomplete_setup_outcome(
                        branch,
                        BytePath::path(&prepared.repository.current().path),
                    );
                }
                Err(error) => {
                    return simple_outcome("repository.invalid", format!("{error:#}"));
                }
            }
            let mut observations = if delivery == Delivery::Human {
                hook::Observations::human()
            } else {
                hook::Observations::captured()
            };
            let execution = hook::execute(
                HookPhase::PostCreate,
                &config.post_create,
                &destination,
                &mut observations,
            );
            drop(observations.finish());
            let (attempt, outcome, output) = match execution {
                Ok(execution) => (
                    Ok(execution.outcome),
                    Some(execution.outcome),
                    execution.output,
                ),
                Err(error) => (Err(error), None, Vec::new()),
            };
            match incomplete.recovery_attempt(attempt) {
                Ok(_) => {
                    if delivery == Delivery::Human {
                        let _ = ui::finish("Post-create setup complete");
                    }
                    registered_outcome(&prepared.plan, false)
                }
                Err(failure) => {
                    let message = if outcome == Some(HookOutcome::Interrupted) {
                        "post-create setup was interrupted; setup remains incomplete".into()
                    } else {
                        format!("{:#}", failure.error)
                    };
                    setup_recovery_failure(
                        "switch.setup_failed",
                        message,
                        branch,
                        destination,
                        failure.transition.entry,
                        hook_diagnostics(output),
                    )
                }
            }
        }
        SetupRecoveryDecision::EnterOnce => {
            if delivery == Delivery::Human {
                let _ = ui::warning("Entering once while setup remains incomplete.");
            }
            let transition = incomplete.enter_once();
            setup_recovery_failure(
                "switch.setup_incomplete",
                format!("setup remains incomplete for {}", destination.display()),
                branch,
                destination,
                transition.entry,
                Vec::new(),
            )
        }
        SetupRecoveryDecision::MarkComplete => match incomplete.mark_complete() {
            Ok(_) => {
                if delivery == Delivery::Human {
                    let _ = ui::finish("Marked setup complete");
                }
                registered_outcome(&prepared.plan, false)
            }
            Err(failure) => setup_recovery_failure(
                "switch.setup_failed",
                format!("{:#}", failure.error),
                branch,
                destination,
                failure.transition.entry,
                Vec::new(),
            ),
        },
    }
}

fn setup_recovery_failure(
    code: &str,
    message: String,
    branch: String,
    destination: PathBuf,
    entry: setup::EntryDisposition,
    diagnostics: Vec<Diagnostic>,
) -> OperationOutcome {
    OperationOutcome {
        result: Err(OperationFailure {
            code: code.into(),
            message,
        }),
        context: OperationContext::Branch(BranchContext {
            branch,
            destination: Some(BytePath::path(&destination)),
            created: Some(false),
            setup: Some("incomplete"),
            hook_outcome: None,
            remotes: None,
        }),
        effects: Vec::new(),
        diagnostics,
        recovery: Vec::new(),
        destination: (entry == setup::EntryDisposition::Enter).then_some(destination),
    }
}

fn registered_outcome(plan: &Plan, dry_run: bool) -> OperationOutcome {
    let Source::Registered(worktree) = &plan.source else {
        unreachable!("registered outcome requires a registered source")
    };
    OperationOutcome {
        result: Ok(OperationResult::Existing {
            branch: plan.branch.clone(),
            destination: BytePath::path(&worktree.path),
            dry_run,
        }),
        context: OperationContext::Empty {},
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
        destination: (!dry_run).then(|| worktree.path.clone()),
    }
}

/// Consumes one opaque prepared operation using the selected presentation policy.
#[must_use]
pub(crate) fn execute_prepared(
    prepared: Box<PreparedOperation>,
    delivery: Delivery,
) -> OperationOutcome {
    let PreparedOperation {
        repository,
        plan,
        input,
    } = *prepared;
    if matches!(plan.source, Source::Registered(_)) {
        return execute_registered(&repository, &plan, &input, delivery);
    }
    let has_hooks = plan
        .config
        .as_ref()
        .is_some_and(|config| !config.post_create.is_empty());
    if delivery == Delivery::Human && !has_hooks && !input.dry_run {
        let progress = ui::TimedProgress::start(true, "Creating worktree...").ok();
        let mut observations = setup::Observations::captured();
        let outcome = execute_planned(&repository, &plan, &input, &mut observations);
        drop(observations.finish());
        if let Some(progress) = progress {
            if outcome.result.is_err() {
                let _ = progress.fail("Failed to create worktree");
            } else {
                let _ = progress.complete("Created worktree", ui::Completion::Outro);
            }
        }
        return outcome;
    }
    let mut observations = if delivery == Delivery::Human {
        setup::Observations::human()
    } else {
        setup::Observations::captured()
    };
    let outcome = execute_planned(&repository, &plan, &input, &mut observations);
    drop(observations.finish());
    if delivery == Delivery::Human && has_hooks && outcome.result.is_ok() {
        let _ = ui::finish("Post-create setup complete");
    }
    outcome
}

fn execute_registered(
    repository: &Repository,
    plan: &Plan,
    input: &OperationInput,
    _delivery: Delivery,
) -> OperationOutcome {
    let _span = debug::Span::new("enter existing worktree");
    let Source::Registered(worktree) = &plan.source else {
        unreachable!("registered execution requires a registered source")
    };
    let branch = plan.branch.clone();
    if !input.dry_run {
        let identity = match RepositoryObservation::new(&worktree.path).worktree_identity() {
            Ok(identity) => identity,
            Err(error) => {
                return simple_outcome("repository.invalid", format!("{error:#}"));
            }
        };
        let setup_lifecycle = setup::Lifecycle::new(&repository.common_dir);
        let incomplete = match setup_lifecycle.inspect(setup::SetupTarget {
            worktree_identity: &identity,
            branch: Some(&branch),
        }) {
            Ok(setup::Inspection::Complete(_)) => None,
            Ok(setup::Inspection::Incomplete(incomplete)) => Some(incomplete),
            Err(failed) => {
                return simple_outcome("repository.invalid", format!("{:#}", failed.error));
            }
        };
        if let Some(incomplete) = incomplete {
            let config = match EffectiveConfig::load(repository) {
                Ok(config) => config,
                Err(error) => {
                    return simple_outcome("repository.invalid", format!("{error:#}"));
                }
            };
            if config.post_create.is_empty() {
                if let Err(failed) = incomplete.no_hooks_configured() {
                    return simple_outcome("switch.setup_failed", format!("{:#}", failed.error));
                }
            } else {
                return incomplete_setup_outcome(
                    branch,
                    BytePath::path(&repository.current().path),
                );
            }
        }
    }
    OperationOutcome {
        result: Ok(OperationResult::Existing {
            branch,
            destination: BytePath::path(&worktree.path),
            dry_run: input.dry_run,
        }),
        context: OperationContext::Empty {},
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
        destination: (!input.dry_run).then(|| worktree.path.clone()),
    }
}

fn simple_outcome(code: &str, message: String) -> OperationOutcome {
    OperationOutcome {
        result: Err(OperationFailure {
            code: code.into(),
            message,
        }),
        context: OperationContext::Empty {},
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
        destination: None,
    }
}

fn incomplete_setup_outcome(branch: String, working_directory: BytePath) -> OperationOutcome {
    OperationOutcome {
        result: Err(OperationFailure {
            code: "switch.setup_incomplete".into(),
            message: "post-create setup remains incomplete; recover it interactively before structured navigation".into(),
        }),
        context: OperationContext::Branch(BranchContext {
            branch: branch.clone(),
            destination: None,
            created: Some(false),
            setup: Some("incomplete"),
            hook_outcome: None,
            remotes: None,
        }),
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: vec![RecoveryAction {
            action: "switch.recover_setup".into(),
            description: "Open the existing topic worktree and finish its pinned post-create setup interactively".into(),
            mutation: MutationClass::Setup,
            requires_human_approval: true,
            invocation: RecoveryInvocation {
                argv: vec!["pando".into(), "switch".into(), branch],
                stdin: None,
                working_directory: Some(working_directory),
            },
        }],
        destination: None,
    }
}

fn new_branch_source(repository: &Repository, base: &git::NewBranchBase) -> String {
    let commit = &base.commit;
    if let Some(base_ref) = &base.base_ref {
        return format!("branch {:?} at {commit}", base_ref.reference());
    }
    match &repository.current().kind {
        WorktreeKind::Branch(source) => format!("branch {source:?} at {commit}"),
        WorktreeKind::Detached => format!("detached commit {commit}"),
        _ => format!("commit {commit}"),
    }
}

fn approval_required_outcome(
    intent: Intent,
    candidate: &hook_approval::Candidate,
    destination: &Path,
    branch: &str,
    working_directory: BytePath,
) -> OperationOutcome {
    let command = intent.id();
    OperationOutcome {
        result: Err(OperationFailure {
            code: "trust.approval_required".into(),
            message: "post-create hooks require manual review and approval before mutation".into(),
        }),
        context: OperationContext::Approval(ApprovalContext {
            approval: HookApprovalContext {
                phase: candidate.phase().key().into(),
                commands: candidate
                    .commands()
                    .iter()
                    .map(|step| HookApprovalCommand {
                        name: step.name.clone(),
                        command: step.command.clone(),
                    })
                    .collect(),
                repository: candidate.repository().into(),
                identity: candidate.identity().into(),
            },
            branch: branch.into(),
            destination: BytePath::path(destination),
        }),
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: vec![RecoveryAction {
            action: "trust.approve_hooks".into(),
            description: "Review and approve post-create hooks interactively".into(),
            mutation: MutationClass::Trust,
            requires_human_approval: true,
            invocation: RecoveryInvocation {
                argv: vec!["pando".into(), command.into(), branch.into()],
                stdin: None,
                working_directory: Some(working_directory),
            },
        }],
        destination: None,
    }
}

/// Executes one already-authorized plan and returns the final command outcome
/// consumed by both presentation adapters.
#[must_use]
fn execute_planned(
    repository: &Repository,
    plan: &Plan,
    input: &OperationInput,
    observations: &mut setup::Observations,
) -> OperationOutcome {
    let command = plan.intent.id();
    let branch = plan.branch.clone();
    let working_directory = BytePath::path(&repository.current().path);
    let destination_path = plan.destination.clone();
    let destination = BytePath::path(&destination_path);
    let new_base = match &plan.source {
        Source::New { base } => Some(base.clone()),
        _ => None,
    };
    match execute(repository, plan, observations) {
        Ok(execution) => {
            let (kind, start_point, base_ref) = new_base.map_or((None, None, None), |base| {
                (
                    Some("new"),
                    Some(base.commit),
                    base.base_ref.map(|reference| reference.reference()),
                )
            });
            let result = if input.dry_run {
                OperationResult::CreationPlan {
                    branch,
                    destination,
                    kind,
                    start_point,
                    base_ref,
                    remote: source_remote(&plan.source),
                }
            } else {
                OperationResult::Created {
                    branch,
                    destination,
                    kind,
                    start_point,
                    base_ref,
                    remote: source_remote(&plan.source),
                }
            };
            OperationOutcome {
                result: Ok(result),
                context: OperationContext::Empty {},
                effects: execution.effects,
                diagnostics: hook_diagnostics(execution.hook_output),
                recovery: Vec::new(),
                destination: (!input.dry_run).then_some(execution.destination),
            }
        }
        Err(execution) => execution_failure_outcome(
            command,
            branch,
            input,
            working_directory,
            destination_path,
            execution,
        ),
    }
}

fn source_remote(source: &Source) -> Option<String> {
    match source {
        Source::Remote { reference, .. } => Some(reference.clone()),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn blocker_outcome(
    intent: Intent,
    blocker: Blocker,
    branch: &str,
    input: &OperationInput,
    working_directory: BytePath,
) -> OperationOutcome {
    let command = intent.id();
    let simple = |code: String, message: String| OperationOutcome {
        result: Err(OperationFailure { code, message }),
        context: OperationContext::Empty {},
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
        destination: None,
    };
    match blocker {
        Blocker::InvalidBranch { message } => simple(format!("{command}.invalid_branch"), message),
        Blocker::ConfigInvalid { message } => simple(format!("{command}.config_invalid"), message),
        Blocker::FetchNotApplicable { message } => {
            simple(format!("{command}.fetch_not_applicable"), message)
        }
        Blocker::BaseUnavailable { message } => {
            simple(format!("{command}.base_unavailable"), message)
        }
        Blocker::DestinationUnavailable { worktree } => simple(
            format!("{command}.destination_unavailable"),
            format!("registered destination is {}", worktree.state_label()),
        ),
        Blocker::PrimaryUnavailable => simple(
            "repository.primary_unavailable".into(),
            "a bare repository cannot create a worktree".into(),
        ),
        Blocker::RootUnavailable { message } => {
            simple("repository.root_unavailable".into(), message)
        }
        Blocker::DestinationInvalid { message } => {
            simple(format!("{command}.destination_invalid"), message)
        }
        Blocker::DestinationCollision => simple(
            format!("{command}.destination_collision"),
            "the configured destination already exists or is registered".into(),
        ),
        Blocker::DestinationNotIgnored { first, gitignore } => simple(
            format!("{command}.destination_invalid"),
            format!(
                "the configured destination is inside the primary worktree but is not ignored; add '/{first}/' to {}",
                gitignore.display()
            ),
        ),
        Blocker::IrrelevantRemote => simple(
            format!("{command}.irrelevant_remote"),
            "remote does not apply to the resolved branch".into(),
        ),
        Blocker::UnknownRemote => simple(
            format!("{command}.unknown_remote"),
            "remote does not match an available fetched branch".into(),
        ),
        Blocker::RegisteredForCreate { worktree } => OperationOutcome {
            result: Err(OperationFailure {
                code: "create.branch_registered".into(),
                message: format!(
                    "branch {branch:?} is already registered at {}; enter it with 'pando switch {branch}'",
                    worktree.path.display()
                ),
            }),
            context: OperationContext::Branch(BranchContext {
                branch: branch.into(),
                destination: Some(BytePath::path(&worktree.path)),
                created: None,
                setup: None,
                hook_outcome: None,
                remotes: None,
            }),
            effects: Vec::new(),
            diagnostics: Vec::new(),
            recovery: vec![RecoveryAction {
                action: "switch".into(),
                description: "Enter the registered worktree instead of creating one".into(),
                mutation: MutationClass::None,
                requires_human_approval: false,
                invocation: RecoveryInvocation {
                    argv: vec![
                        "pando".into(),
                        "--input-output".into(),
                        "json".into(),
                        "switch".into(),
                    ],
                    stdin: Some(protocol::Request {
                        schema_version: protocol::SCHEMA_VERSION,
                        request_id: None,
                        input: RetryInput {
                            branch: Some(branch.into()),
                            remote: None,
                            fetch: None,
                            dry_run: None,
                        },
                    }),
                    working_directory: Some(working_directory),
                },
            }],
            destination: None,
        },
        Blocker::RemoteSelectionRequired { remotes, .. } => {
            let recovery = remotes
                .iter()
                .map(|remote| RecoveryAction {
                    action: "retry_with_remote".into(),
                    description: format!("Retry with {remote} as the selected source"),
                    mutation: MutationClass::None,
                    requires_human_approval: false,
                    invocation: RecoveryInvocation {
                        argv: vec![
                            "pando".into(),
                            "--input-output".into(),
                            "json".into(),
                            command.into(),
                        ],
                        stdin: Some(protocol::Request {
                            schema_version: protocol::SCHEMA_VERSION,
                            request_id: None,
                            input: RetryInput {
                                branch: Some(branch.into()),
                                remote: Some(remote.clone()),
                                fetch: Some(input.fetch),
                                dry_run: Some(input.dry_run),
                            },
                        }),
                        working_directory: Some(working_directory.clone()),
                    },
                })
                .collect();
            OperationOutcome {
                result: Err(OperationFailure {
                    code: format!("{command}.remote_selection_required"),
                    message: "multiple fetched remotes match this branch".into(),
                }),
                context: OperationContext::Branch(BranchContext {
                    branch: branch.into(),
                    destination: None,
                    created: None,
                    setup: None,
                    hook_outcome: None,
                    remotes: Some(remotes),
                }),
                effects: Vec::new(),
                diagnostics: Vec::new(),
                recovery,
                destination: None,
            }
        }
        Blocker::ApprovalRequired {
            candidate,
            destination,
        } => OperationOutcome {
            result: Err(OperationFailure {
                code: "trust.approval_required".into(),
                message: "post-create hooks require manual review and approval before mutation"
                    .into(),
            }),
            context: OperationContext::Approval(ApprovalContext {
                approval: HookApprovalContext {
                    phase: candidate.phase().key().into(),
                    commands: candidate
                        .commands()
                        .iter()
                        .map(|step| HookApprovalCommand {
                            name: step.name.clone(),
                            command: step.command.clone(),
                        })
                        .collect(),
                    repository: candidate.repository().into(),
                    identity: candidate.identity().into(),
                },
                branch: branch.into(),
                destination: BytePath::path(&destination),
            }),
            effects: Vec::new(),
            diagnostics: Vec::new(),
            recovery: vec![RecoveryAction {
                action: "trust.approve_hooks".into(),
                description: "Review and approve post-create hooks interactively".into(),
                mutation: MutationClass::Trust,
                requires_human_approval: true,
                invocation: RecoveryInvocation {
                    argv: vec!["pando".into(), command.into(), branch.into()],
                    stdin: None,
                    working_directory: Some(working_directory),
                },
            }],
            destination: None,
        },
    }
}

fn execution_failure_outcome(
    command: &str,
    branch: String,
    input: &OperationInput,
    working_directory: BytePath,
    destination: PathBuf,
    failure: ExecutionFailure,
) -> OperationOutcome {
    let code = match failure.code {
        "description_failed" => "create.description_failed".into(),
        "setup_failed" => format!("{command}.setup_failed"),
        "plan_stale" => format!("{command}.plan_stale"),
        _ => format!("{command}.creation_failed"),
    };
    let mut recovery = Vec::new();
    if let Some(description) = input
        .description
        .as_deref()
        .filter(|_| code == "create.description_failed")
    {
        recovery.push(RecoveryAction {
            action: "git.set_branch_description".into(),
            description:
                "Set the requested branch description in repository-local Git configuration".into(),
            mutation: MutationClass::Config,
            requires_human_approval: false,
            invocation: RecoveryInvocation {
                argv: vec![
                    "git".into(),
                    "config".into(),
                    "--local".into(),
                    "--replace-all".into(),
                    format!("branch.{branch}.description"),
                    description.into(),
                ],
                stdin: None,
                working_directory: Some(working_directory.clone()),
            },
        });
    }
    if failure.setup_incomplete {
        recovery.push(RecoveryAction {
            action: format!("{command}.recover_setup"),
            description:
                "Inspect the worktree and retry or explicitly complete setup interactively".into(),
            mutation: MutationClass::Setup,
            requires_human_approval: true,
            invocation: RecoveryInvocation {
                argv: vec!["pando".into(), "switch".into(), branch.clone()],
                stdin: None,
                working_directory: Some(working_directory),
            },
        });
    }
    let enters_destination = failure.entry == setup::EntryDisposition::Enter;
    OperationOutcome {
        result: Err(OperationFailure {
            code,
            message: format!("{:#}", failure.error),
        }),
        context: OperationContext::Branch(BranchContext {
            branch,
            destination: Some(BytePath::path(&destination)),
            created: Some(failure.created),
            setup: failure.setup_incomplete.then_some("incomplete"),
            hook_outcome: failure.hook_outcome.map(|outcome| format!("{outcome:?}")),
            remotes: None,
        }),
        effects: failure.effects,
        diagnostics: hook_diagnostics(failure.hook_output),
        recovery,
        destination: enters_destination.then_some(destination),
    }
}

fn hook_diagnostics(output: Vec<CapturedStep>) -> Vec<Diagnostic> {
    output
        .into_iter()
        .flat_map(|step| [("stdout", step.stdout), ("stderr", step.stderr)])
        .filter(|(_, captured)| captured.original_size > 0)
        .map(|(stream, captured)| Diagnostic {
            source: "hook".into(),
            stream: stream.into(),
            content: String::from_utf8_lossy(&captured.content).into_owned(),
            original_size: captured.original_size,
            truncated: captured.truncated,
        })
        .collect()
}

fn bounded_diagnostic(source: &str, stream: &str, bytes: &[u8]) -> Diagnostic {
    const LIMIT: usize = 16 * 1024;
    let kept = &bytes[..bytes.len().min(LIMIT)];
    Diagnostic {
        source: source.into(),
        stream: stream.into(),
        content: String::from_utf8_lossy(kept).into_owned(),
        original_size: bytes.len(),
        truncated: bytes.len() > LIMIT,
    }
}

/// Resolves the registered-worktree fast path without observing unrelated refs.
#[allow(clippy::too_many_arguments)]
fn registered_plan(
    repository: &Repository,
    intent: Intent,
    branch: &str,
    remote: Option<&str>,
    fetch: FetchIntent,
    description: Option<String>,
    dry_run: bool,
) -> Option<Result<Plan, Blocker>> {
    let worktree = crate::worktree_for_branch(&repository.worktrees, branch)?.clone();
    if let Err(error) = branch::reject_fetch(fetch.requested(), FETCH_REGISTERED_WORKTREE) {
        return Some(Err(Blocker::FetchNotApplicable {
            message: format!("{error:#}"),
        }));
    }
    if remote.is_some() {
        return Some(Err(Blocker::IrrelevantRemote));
    }
    if !worktree.navigable() {
        return Some(Err(Blocker::DestinationUnavailable { worktree }));
    }
    if intent == Intent::Create {
        return Some(Err(Blocker::RegisteredForCreate { worktree }));
    }
    Some(Ok(Plan {
        intent,
        branch: branch.to_owned(),
        destination: worktree.path.clone(),
        source: Source::Registered(worktree),
        config: None,
        description,
        fetch,
        dry_run,
    }))
}

/// Plans the selected source and byte-preserving destination.
///
/// The result is deterministic. Callers may satisfy a remote-choice blocker and
/// invoke this function again, but must not duplicate branch classification.
///
/// # Errors
///
/// Returns an error when Git cannot classify the branch or configuration cannot be loaded.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One authoritative planner owns every classification and safety check.
fn plan(
    repository: &Repository,
    snapshot: &Snapshot<'_>,
    intent: Intent,
    branch: &str,
    remote: Option<&str>,
    fetch: FetchIntent,
    description: Option<String>,
    dry_run: bool,
) -> Result<Result<Plan, Blocker>> {
    if let Some(plan) = registered_plan(
        repository,
        intent,
        branch,
        remote,
        fetch,
        description.clone(),
        dry_run,
    ) {
        return Ok(plan);
    }
    if let Err(error) = snapshot.validate(branch) {
        return Ok(Err(Blocker::InvalidBranch {
            message: format!("{error:#}"),
        }));
    }
    let classification = snapshot.classify(branch);
    debug_assert!(!matches!(classification, Classification::Registered(_)));

    let Some(primary) = repository.primary.as_ref() else {
        return Ok(Err(Blocker::PrimaryUnavailable));
    };
    let config = match EffectiveConfig::load(repository) {
        Ok(config) => config,
        Err(error) => {
            return Ok(Err(Blocker::ConfigInvalid {
                message: format!("{error:#}"),
            }));
        }
    };
    let root = match config.require_root() {
        Ok(root) => root,
        Err(error) => {
            return Ok(Err(Blocker::RootUnavailable {
                message: format!("{error:#}"),
            }));
        }
    };
    let destination = match RepositoryObservation::resolve_path(&root.join(branch)) {
        Ok(destination) => destination,
        Err(error) => {
            return Ok(Err(Blocker::DestinationInvalid {
                message: format!("{error:#}"),
            }));
        }
    };
    if destination.exists() || repository.worktrees.iter().any(|w| w.path == destination) {
        return Ok(Err(Blocker::DestinationCollision));
    }
    if destination.starts_with(primary)
        && !RepositoryObservation::new(primary).would_be_ignored(&destination)?
    {
        let relative = destination.strip_prefix(primary).unwrap_or(&destination);
        let first = relative
            .components()
            .next()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        return Ok(Err(Blocker::DestinationNotIgnored {
            first,
            gitignore: primary.join(".gitignore"),
        }));
    }

    // A requested fresh-base fetch is itself a repository mutation, so gate it
    // before source planning. Other source planning is read-only and remains
    // ahead of approval to preserve remote-choice and validation behavior.
    if !dry_run && fetch.refreshes() {
        if let Some(candidate) = approval_candidate(repository, &config)? {
            return Ok(Err(Blocker::ApprovalRequired {
                candidate,
                destination,
            }));
        }
    }

    let source = match plan_source(
        snapshot,
        classification,
        branch,
        remote,
        &config,
        fetch,
        &destination,
    ) {
        Ok(SourceResolution::Planned(source)) => source,
        Ok(SourceResolution::FetchRequired(requirement)) if fetch.refreshes() => {
            let output = git::RefMutation::new(&repository.current().path)
                .fetch_base_ref(&requirement.base_ref)?;
            let refreshed_repository = RepositoryObservation::new(&repository.current().path)
                .repository_for_navigation()?;
            let refreshed_snapshot = Snapshot::observe(&refreshed_repository)?;
            let mut rebuilt = match plan(
                &refreshed_repository,
                &refreshed_snapshot,
                intent,
                branch,
                remote,
                FetchIntent::None,
                description,
                dry_run,
            )? {
                Ok(plan) => plan,
                Err(blocker) => return Ok(Err(blocker)),
            };
            rebuilt.fetch = fetch;
            if let Source::New { base } = &mut rebuilt.source {
                base.fetch_output = Some(output);
            }
            return Ok(Ok(rebuilt));
        }
        Ok(SourceResolution::FetchRequired(requirement)) => {
            return Ok(Err(Blocker::BaseUnavailable {
                message: requirement.unavailable_message(),
            }));
        }
        Err(blocker) => return Ok(Err(*blocker)),
    };
    if !dry_run && !fetch.refreshes() {
        if let Some(candidate) = approval_candidate(repository, &config)? {
            return Ok(Err(Blocker::ApprovalRequired {
                candidate,
                destination,
            }));
        }
    }

    let plan = Plan {
        intent,
        branch: branch.to_owned(),
        destination,
        source,
        config: Some(config),
        description,
        fetch,
        dry_run,
    };
    Ok(Ok(plan))
}

fn approval_candidate(
    repository: &Repository,
    config: &EffectiveConfig,
) -> Result<Option<hook_approval::Candidate>> {
    match hook_approval::evaluate(
        repository,
        crate::config::HookPhase::PostCreate,
        &config.post_create,
    )? {
        hook_approval::Evaluation::ApprovalRequired(candidate) => Ok(Some(candidate)),
        hook_approval::Evaluation::NoCommands | hook_approval::Evaluation::Trusted { .. } => {
            Ok(None)
        }
    }
}

enum SourceResolution {
    Planned(Source),
    FetchRequired(ExactFetch),
}

fn plan_source(
    snapshot: &Snapshot<'_>,
    classification: Classification,
    branch: &str,
    remote: Option<&str>,
    config: &EffectiveConfig,
    fetch: FetchIntent,
    destination: &std::path::Path,
) -> Result<SourceResolution, Box<Blocker>> {
    match classification {
        Classification::Registered(_) => unreachable!("registered classification returned above"),
        Classification::Local => {
            reject_fetch(fetch, FETCH_LOCAL_BRANCH)?;
            if remote.is_some() {
                return Err(Box::new(Blocker::IrrelevantRemote));
            }
            Ok(SourceResolution::Planned(Source::Local {
                commit: snapshot
                    .local_commit(branch)
                    .expect("classified local branch must have a pinned identity")
                    .to_owned(),
            }))
        }
        Classification::New => {
            if remote.is_some() {
                return Err(Box::new(Blocker::UnknownRemote));
            }
            if fetch.requested() && config.base == crate::BaseMode::Head {
                reject_fetch(fetch, FETCH_HEAD_BASE)?;
            }
            if fetch.refreshes() {
                return snapshot
                    .fresh_fetch(config.target_branch.as_deref())
                    .map(SourceResolution::FetchRequired)
                    .map_err(|error| {
                        Box::new(Blocker::BaseUnavailable {
                            message: format!("{error:#}"),
                        })
                    });
            }
            snapshot
                .new_branch_base(config.base, config.target_branch.as_deref())
                .map(|resolution| match resolution {
                    BaseResolution::Resolved(base) => {
                        SourceResolution::Planned(Source::New { base })
                    }
                    BaseResolution::FetchRequired(requirement) => {
                        SourceResolution::FetchRequired(requirement)
                    }
                })
                .map_err(|error| {
                    Box::new(Blocker::BaseUnavailable {
                        message: format!("{error:#}"),
                    })
                })
        }
        Classification::Remotes(remotes) => {
            reject_fetch(fetch, FETCH_REMOTE_BRANCH)?;
            match remote {
                Some(remote) => remotes
                    .into_iter()
                    .find(|candidate| {
                        candidate == remote || candidate == &format!("{remote}/{branch}")
                    })
                    .map(|reference| {
                        SourceResolution::Planned(Source::Remote {
                            commit: snapshot
                                .remote_commit(&reference)
                                .expect("classified remote ref must have a pinned identity")
                                .to_owned(),
                            reference,
                        })
                    })
                    .ok_or_else(|| Box::new(Blocker::UnknownRemote)),
                None if remotes.len() == 1 => Ok(SourceResolution::Planned(Source::Remote {
                    commit: snapshot
                        .remote_commit(&remotes[0])
                        .expect("classified remote ref must have a pinned identity")
                        .to_owned(),
                    reference: remotes[0].clone(),
                })),
                None => Err(Box::new(Blocker::RemoteSelectionRequired {
                    remotes,
                    destination: destination.to_owned(),
                })),
            }
        }
    }
}

/// Executes a navigation or creation plan without reclassifying its branch.
///
/// Setup and hook observations may be presented live or retained for replay,
/// but the returned execution state is identical in both cases.
#[allow(clippy::too_many_lines)] // This is the single explicit worktree execution boundary.
fn execute(
    repository: &Repository,
    plan: &Plan,
    observations: &mut setup::Observations,
) -> std::result::Result<ExecutionOutcome, ExecutionFailure> {
    let mut effects = planned_effects(plan);
    let failure = |code, error, effects, created, setup_incomplete| ExecutionFailure {
        code,
        error,
        effects,
        created,
        setup_incomplete,
        hook_outcome: None,
        hook_output: Vec::new(),
        entry: if created && setup_incomplete {
            setup::EntryDisposition::Enter
        } else {
            setup::EntryDisposition::Stay
        },
    };
    if let Source::Registered(worktree) = &plan.source {
        return Ok(ExecutionOutcome {
            destination: worktree.path.clone(),
            effects,
            hook_output: Vec::new(),
        });
    }
    let config = plan.config.as_ref().expect("creation plan has config");
    if plan.dry_run {
        return Ok(ExecutionOutcome {
            destination: plan.destination.clone(),
            effects,
            hook_output: Vec::new(),
        });
    }
    revalidate(repository, plan)
        .map_err(|error| failure("plan_stale", error, effects.clone(), false, false))?;
    if let Some(parent) = plan.destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create destination parent {}", parent.display()))
            .map_err(|error| failure("creation_failed", error, effects.clone(), false, false))?;
    }
    let setup_lifecycle = setup::Lifecycle::new(&repository.common_dir);
    let pending = (!config.post_create.is_empty())
        .then(|| {
            setup_lifecycle.prepare(setup::SetupIntent {
                branch: &plan.branch,
                destination: &plan.destination,
            })
        })
        .transpose()
        .map_err(|value| failure("setup_failed", value.error, effects.clone(), false, true))?;
    let creation_index = effects
        .iter()
        .position(|effect| effect.action == "create_worktree")
        .expect("creation plans carry a worktree effect");
    effects[creation_index].attempted = true;
    let branch_index = effects
        .iter()
        .position(|effect| effect.action == "create_branch");
    if let Some(index) = branch_index {
        effects[index].attempted = true;
    }
    let mutation = git::WorktreeMutation::new(&repository.current().path);
    let source = match &plan.source {
        Source::Registered(_) => unreachable!(),
        Source::Local { .. } => git::WorktreeSource::Existing,
        Source::Remote { reference, .. } => git::WorktreeSource::Tracking {
            remote_ref: reference,
        },
        Source::New { base } => git::WorktreeSource::New {
            start_point: &base.commit,
        },
    };
    let creation = mutation.create(&plan.destination, &plan.branch, source);
    if let Err(error) = creation {
        if let Some(pending) = pending {
            let value = pending.creation_failed(error);
            return Err(failure(
                "creation_failed",
                value.error,
                effects,
                false,
                false,
            ));
        }
        return Err(failure("creation_failed", error, effects, false, false));
    }
    effects[creation_index].completed = true;
    if let Some(index) = branch_index {
        effects[index].completed = true;
    }
    let incomplete =
        if let Some(pending) = pending {
            let identity = RepositoryObservation::new(&plan.destination).worktree_identity();
            Some(pending.created(identity).map_err(|value| {
                failure("setup_failed", value.error, effects.clone(), true, true)
            })?)
        } else {
            None
        };
    observations.emit(setup::Observation::WorktreeCreated);
    if let Some(description) = plan.description.as_deref() {
        let index = effects
            .iter()
            .position(|effect| effect.action == "set_branch_description")
            .expect("described plans carry a description effect");
        effects[index].attempted = true;
        if let Err(error) = mutation.describe(&plan.branch, description) {
            if let Some(incomplete) = incomplete {
                let value = incomplete.post_creation_failed(error);
                return Err(failure(
                    "description_failed",
                    value.error,
                    effects,
                    true,
                    true,
                ));
            }
            return Err(failure("description_failed", error, effects, true, false));
        }
        effects[index].completed = true;
    }
    if let Some(incomplete) = incomplete {
        let mut hook_observations = if observations.is_human() {
            hook::Observations::human()
        } else {
            hook::Observations::captured()
        };
        let execution = hook::execute(
            crate::config::HookPhase::PostCreate,
            &config.post_create,
            &plan.destination,
            &mut hook_observations,
        );
        drop(hook_observations.finish());
        let (attempt, outcome, hook_output) = match execution {
            Ok(execution) => (
                Ok(execution.outcome),
                Some(execution.outcome),
                execution.output,
            ),
            Err(error) => (Err(error), None, Vec::new()),
        };
        if let Err(value) = incomplete.initial_attempt(attempt) {
            let error = outcome.map_or(value.error, |outcome| {
                anyhow::anyhow!("post-create hook outcome: {outcome:?}; setup remains incomplete")
            });
            return Err(ExecutionFailure {
                code: "setup_failed",
                error,
                effects,
                created: true,
                setup_incomplete: true,
                hook_outcome: outcome,
                hook_output,
                entry: value.transition.entry,
            });
        }
        return Ok(ExecutionOutcome {
            destination: plan.destination.clone(),
            effects,
            hook_output,
        });
    }
    Ok(ExecutionOutcome {
        destination: plan.destination.clone(),
        effects,
        hook_output: Vec::new(),
    })
}

fn revalidate(repository: &Repository, plan: &Plan) -> Result<()> {
    if plan.destination.exists()
        || repository
            .worktrees
            .iter()
            .any(|worktree| worktree.path == plan.destination)
    {
        bail!("the planned destination became occupied before worktree creation");
    }
    let (reference, expected) = match &plan.source {
        Source::Local { commit } => (plan.branch.as_str(), commit),
        Source::Remote { reference, commit } => (reference.as_str(), commit),
        Source::New { base } => return revalidate_new(repository, base),
        Source::Registered(_) => return Ok(()),
    };
    let actual = HistoryObservation::new(&repository.current().path).commit(reference)?;
    if actual != *expected {
        bail!("planned source {reference:?} moved from {expected} to {actual}");
    }
    Ok(())
}

fn revalidate_new(repository: &Repository, base: &git::NewBranchBase) -> Result<()> {
    let reference = base
        .base_ref
        .as_ref()
        .map_or_else(|| "HEAD".to_owned(), git::BaseRef::reference);
    let actual = HistoryObservation::new(&repository.current().path)
        .commit(&reference)
        .context("the planned new-branch source is no longer available")?;
    if actual != base.commit {
        bail!(
            "planned new-branch source moved from {} to {actual}",
            base.commit
        );
    }
    Ok(())
}

fn planned_effects(plan: &Plan) -> Vec<Effect> {
    if matches!(plan.source, Source::Registered(_)) {
        return Vec::new();
    }
    let mut effects = Vec::new();
    if let Source::New { base } = &plan.source {
        if plan.fetch.requested() {
            effects.push(Effect {
                action: "fetch_base_ref".into(),
                attempted: plan.fetch.refreshes(),
                completed: plan.fetch.refreshes(),
                details: base
                    .base_ref
                    .as_ref()
                    .map(|value| json!({"ref":value.reference()})),
            });
        }
        effects.push(Effect {
            action: "create_branch".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"branch":plan.branch,"start_point":base.commit})),
        });
    }
    effects.push(Effect {
        action: "create_worktree".into(),
        attempted: false,
        completed: false,
        details: Some(json!({"destination":crate::protocol::BytePath::path(&plan.destination)})),
    });
    if let Some(description) = &plan.description {
        effects.push(Effect {
            action: "set_branch_description".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"branch":plan.branch,"description":description})),
        });
    }
    effects
}

fn reject_fetch(fetch: FetchIntent, because: &str) -> Result<(), Box<Blocker>> {
    branch::reject_fetch(fetch.requested(), because).map_err(|error| {
        Box::new(Blocker::FetchNotApplicable {
            message: format!("{error:#}"),
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::RetryInput;

    #[test]
    fn selection_retry_preserves_the_version_one_minimal_input_shape() {
        let retry = RetryInput {
            branch: Some("topic".into()),
            remote: None,
            fetch: None,
            dry_run: None,
        };

        assert_eq!(
            serde_json::to_value(retry).unwrap(),
            json!({"branch": "topic"})
        );
    }
}
