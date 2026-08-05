//! Shared destination and source planning for worktree navigation and creation.

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::protocol::{
    self, BytePath, Diagnostic, Effect, ErrorBody, MutationClass, RecoveryAction,
    RecoveryInvocation,
};

use crate::{
    Worktree,
    branch::{
        self, BaseResolution, Classification, ExactFetch, FETCH_HEAD_BASE, FETCH_LOCAL_BRANCH,
        FETCH_REGISTERED_WORKTREE, FETCH_REMOTE_BRANCH, Snapshot,
    },
    config::EffectiveConfig,
    git::{self, HistoryObservation, Repository, RepositoryObservation},
    hook::{self, HookOutcome, HookOutput, OutputPolicy},
    hook_approval, setup,
};

/// Stable error codes advertised by the switch protocol.
pub(crate) const SWITCH_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
    "switch.selection_required",
    "switch.invalid_branch",
    "switch.destination_unavailable",
    "switch.destination_collision",
    "switch.remote_selection_required",
    "switch.fetch_not_applicable",
    "switch.base_unavailable",
    "switch.approval_required",
    "trust.approval_required",
];

/// Stable error codes advertised by the create protocol.
pub(crate) const CREATE_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
    "create.branch_required",
    "create.invalid_branch",
    "create.branch_registered",
    "create.destination_unavailable",
    "create.destination_collision",
    "create.remote_selection_required",
    "create.fetch_not_applicable",
    "create.base_unavailable",
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
#[derive(Debug)]
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
pub(crate) enum Source {
    Registered(Worktree),
    Local { commit: String },
    Remote { reference: String, commit: String },
    New { base: git::NewBranchBase },
}

/// Whether the caller requested the only network mutation supported by planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FetchIntent {
    None,
    Refresh,
    Preview,
}

impl FetchIntent {
    pub(crate) const fn new(fetch: bool, dry_run: bool) -> Self {
        match (fetch, dry_run) {
            (false, _) => Self::None,
            (true, false) => Self::Refresh,
            (true, true) => Self::Preview,
        }
    }

    pub(crate) const fn requested(self) -> bool {
        !matches!(self, Self::None)
    }

    const fn refreshes(self) -> bool {
        matches!(self, Self::Refresh)
    }
}

/// A deterministic plan with no terminal or JSON representation concerns.
#[derive(Debug)]
pub(crate) struct Plan {
    pub(crate) intent: Intent,
    pub(crate) branch: String,
    pub(crate) destination: PathBuf,
    pub(crate) source: Source,
    pub(crate) config: Option<EffectiveConfig>,
    pub(crate) description: Option<String>,
    pub(crate) fetch: FetchIntent,
    pub(crate) dry_run: bool,
}

/// The result of executing a navigation or creation plan.
#[derive(Debug)]
pub(crate) struct ExecutionOutcome {
    pub(crate) destination: PathBuf,
    pub(crate) effects: Vec<Effect>,
    pub(crate) hook_output: HookOutput,
}

/// A failed execution together with effects advanced only at real transitions.
#[derive(Debug)]
pub(crate) struct ExecutionFailure {
    pub(crate) code: &'static str,
    pub(crate) error: anyhow::Error,
    pub(crate) effects: Vec<Effect>,
    pub(crate) created: bool,
    pub(crate) setup_incomplete: bool,
    pub(crate) hook_outcome: Option<HookOutcome>,
    pub(crate) hook_output: HookOutput,
}

/// Complete command-owned outcome for a switch or create request.
#[derive(Debug)]
pub(crate) struct OperationOutcome {
    pub(crate) result: std::result::Result<OperationResult, OperationFailure>,
    pub(crate) context: OperationContext,
    pub(crate) effects: Vec<Effect>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) recovery: Vec<RecoveryAction<protocol::Request<RetryInput>>>,
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
pub(crate) enum Blocker {
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

/// Runs the complete noninteractive worktree operation and returns typed protocol data.
#[allow(clippy::too_many_lines)]
pub(crate) fn operation(intent: Intent, input: &OperationInput) -> OperationOutcome {
    let command = intent.id();
    let failure = |code: &str, message: String| OperationOutcome {
        result: Err(OperationFailure {
            code: code.into(),
            message,
        }),
        context: OperationContext::Empty {},
        effects: Vec::new(),
        diagnostics: Vec::new(),
        recovery: Vec::new(),
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
        observation.repository()
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
                    crate::WorktreeKind::Branch(value) => Some(value.clone()),
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
        return OperationOutcome {
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
        };
    };
    let snapshot = match Snapshot::observe(&repository) {
        Ok(snapshot) => snapshot,
        Err(error) => return failure("repository.invalid", format!("{error:#}")),
    };
    let plan = match plan(
        &repository,
        &snapshot,
        intent,
        &branch,
        input.remote.as_deref(),
        FetchIntent::new(input.fetch, input.dry_run),
        input.description.clone(),
        input.dry_run,
    ) {
        Ok(Ok(plan)) => plan,
        Ok(Err(blocker)) => {
            return blocker_outcome(intent, blocker, &branch, input, working_directory);
        }
        Err(error) => return failure("repository.invalid", format!("{error:#}")),
    };
    if let Source::Registered(worktree) = &plan.source {
        return OperationOutcome {
            result: Ok(OperationResult::Existing {
                branch,
                destination: BytePath::path(&worktree.path),
                dry_run: input.dry_run,
            }),
            context: OperationContext::Empty {},
            effects: Vec::new(),
            diagnostics: Vec::new(),
            recovery: Vec::new(),
        };
    }
    let destination = BytePath::path(&plan.destination);
    let new_base = match &plan.source {
        Source::New { base } => Some(base.clone()),
        _ => None,
    };
    if intent == Intent::Switch
        && let Some(base) = new_base.clone()
    {
        if !input.dry_run {
            return OperationOutcome {
                result: Err(OperationFailure {
                    code: "switch.approval_required".into(),
                    message: "creating a genuinely new branch requires a manual human invocation"
                        .into(),
                }),
                context: OperationContext::Empty {},
                effects: Vec::new(),
                diagnostics: Vec::new(),
                recovery: Vec::new(),
            };
        }
        return OperationOutcome {
            result: Ok(OperationResult::NewBranchApproval {
                branch,
                destination,
                kind: "new",
                start_point: base.commit,
                base_ref: base.base_ref.as_ref().map(git::BaseRef::reference),
                approval_required: true,
            }),
            context: OperationContext::Empty {},
            effects: planned_effects(&plan),
            diagnostics: Vec::new(),
            recovery: Vec::new(),
        };
    }
    match execute(&repository, &plan, OutputPolicy::Captured, || Ok(())) {
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
            }
        }
        Err(execution) => execution_failure_outcome(
            command,
            branch,
            input,
            working_directory,
            destination,
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
    };
    match blocker {
        Blocker::InvalidBranch { message } => simple(format!("{command}.invalid_branch"), message),
        Blocker::ConfigInvalid { message } => simple(format!("{command}.config_invalid"), message),
        Blocker::FetchNotApplicable { message } => simple(format!("{command}.fetch_not_applicable"), message),
        Blocker::BaseUnavailable { message } => simple(format!("{command}.base_unavailable"), message),
        Blocker::DestinationUnavailable { worktree } => simple(format!("{command}.destination_unavailable"), format!("registered destination is {}", worktree.state_label())),
        Blocker::PrimaryUnavailable => simple("repository.primary_unavailable".into(), "a bare repository cannot create a worktree".into()),
        Blocker::RootUnavailable { message } => simple("repository.root_unavailable".into(), message),
        Blocker::DestinationInvalid { message } => simple(format!("{command}.destination_invalid"), message),
        Blocker::DestinationCollision => simple(format!("{command}.destination_collision"), "the configured destination already exists or is registered".into()),
        Blocker::DestinationNotIgnored { first, gitignore } => simple(format!("{command}.destination_invalid"), format!("the configured destination is inside the primary worktree but is not ignored; add '/{first}/' to {}", gitignore.display())),
        Blocker::IrrelevantRemote => simple(format!("{command}.irrelevant_remote"), "remote does not apply to the resolved branch".into()),
        Blocker::UnknownRemote => simple(format!("{command}.unknown_remote"), "remote does not match an available fetched branch".into()),
        Blocker::RegisteredForCreate { worktree } => OperationOutcome { result: Err(OperationFailure { code: "create.branch_registered".into(), message: "the branch already has a registered worktree; create will not adopt or replace it".into() }), context: OperationContext::Branch(BranchContext { branch: branch.into(), destination: Some(BytePath::path(&worktree.path)), created: None, setup: None, hook_outcome: None, remotes: None }), effects: Vec::new(), diagnostics: Vec::new(), recovery: vec![RecoveryAction { action: "switch".into(), description: "Enter the registered worktree instead of creating one".into(), mutation: MutationClass::None, requires_human_approval: false, invocation: RecoveryInvocation { argv: vec!["pando".into(), "--input-output".into(), "json".into(), "switch".into()], stdin: Some(protocol::Request { schema_version: protocol::SCHEMA_VERSION, request_id: None, input: RetryInput { branch: Some(branch.into()), remote: None, fetch: None, dry_run: None } }), working_directory: Some(working_directory) } }] },
        Blocker::RemoteSelectionRequired { remotes, .. } => { let recovery = remotes.iter().map(|remote| RecoveryAction { action: "retry_with_remote".into(), description: format!("Retry with {remote} as the selected source"), mutation: MutationClass::None, requires_human_approval: false, invocation: RecoveryInvocation { argv: vec!["pando".into(), "--input-output".into(), "json".into(), command.into()], stdin: Some(protocol::Request { schema_version: protocol::SCHEMA_VERSION, request_id: None, input: RetryInput { branch: Some(branch.into()), remote: Some(remote.clone()), fetch: Some(input.fetch), dry_run: Some(input.dry_run) } }), working_directory: Some(working_directory.clone()) } }).collect(); OperationOutcome { result: Err(OperationFailure { code: format!("{command}.remote_selection_required"), message: "multiple fetched remotes match this branch".into() }), context: OperationContext::Branch(BranchContext { branch: branch.into(), destination: None, created: None, setup: None, hook_outcome: None, remotes: Some(remotes) }), effects: Vec::new(), diagnostics: Vec::new(), recovery } }
        Blocker::ApprovalRequired { candidate, destination } => OperationOutcome { result: Err(OperationFailure { code: "trust.approval_required".into(), message: "post-create hooks require manual review and approval before mutation".into() }), context: OperationContext::Approval(ApprovalContext { approval: HookApprovalContext { phase: candidate.phase().key().into(), commands: candidate.commands().iter().map(|step| HookApprovalCommand { name: step.name.clone(), command: step.command.clone() }).collect(), repository: candidate.repository().into(), identity: candidate.identity().into() }, branch: branch.into(), destination: BytePath::path(&destination) }), effects: Vec::new(), diagnostics: Vec::new(), recovery: vec![RecoveryAction { action: "trust.approve_hooks".into(), description: "Review and approve post-create hooks interactively".into(), mutation: MutationClass::Trust, requires_human_approval: true, invocation: RecoveryInvocation { argv: vec!["pando".into(), command.into(), branch.into()], stdin: None, working_directory: Some(working_directory) } }] },
    }
}

fn execution_failure_outcome(
    command: &str,
    branch: String,
    input: &OperationInput,
    working_directory: BytePath,
    destination: BytePath,
    failure: ExecutionFailure,
) -> OperationOutcome {
    let code = match failure.code {
        "description_failed" => "create.description_failed".into(),
        "setup_failed" => format!("{command}.setup_failed"),
        "plan_stale" => format!("{command}.plan_stale"),
        _ => format!("{command}.creation_failed"),
    };
    let mut recovery = Vec::new();
    if code == "create.description_failed"
        && let Some(description) = input.description.as_deref()
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
    OperationOutcome {
        result: Err(OperationFailure {
            code,
            message: format!("{:#}", failure.error),
        }),
        context: OperationContext::Branch(BranchContext {
            branch,
            destination: Some(destination),
            created: Some(failure.created),
            setup: failure.setup_incomplete.then_some("incomplete"),
            hook_outcome: failure.hook_outcome.map(|outcome| format!("{outcome:?}")),
            remotes: None,
        }),
        effects: failure.effects,
        diagnostics: hook_diagnostics(failure.hook_output),
        recovery,
    }
}

fn hook_diagnostics(output: HookOutput) -> Vec<Diagnostic> {
    match output {
        HookOutput::Streamed => Vec::new(),
        HookOutput::Captured(steps) => steps
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
            .collect(),
    }
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

/// Plans the selected source and byte-preserving destination.
///
/// The result is deterministic. Callers may satisfy a remote-choice blocker and
/// invoke this function again, but must not duplicate branch classification.
///
/// # Errors
///
/// Returns an error when Git cannot classify the branch or configuration cannot be loaded.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One authoritative planner owns every classification and safety check.
pub(crate) fn plan(
    repository: &Repository,
    snapshot: &Snapshot<'_>,
    intent: Intent,
    branch: &str,
    remote: Option<&str>,
    fetch: FetchIntent,
    description: Option<String>,
    dry_run: bool,
) -> Result<Result<Plan, Blocker>> {
    if let Err(error) = snapshot.validate(branch) {
        return Ok(Err(Blocker::InvalidBranch {
            message: format!("{error:#}"),
        }));
    }
    let classification = snapshot.classify(branch);
    if let Classification::Registered(worktree) = classification {
        if let Err(error) = branch::reject_fetch(fetch.requested(), FETCH_REGISTERED_WORKTREE) {
            return Ok(Err(Blocker::FetchNotApplicable {
                message: format!("{error:#}"),
            }));
        }
        if remote.is_some() {
            return Ok(Err(Blocker::IrrelevantRemote));
        }
        if !worktree.navigable() {
            return Ok(Err(Blocker::DestinationUnavailable { worktree }));
        }
        if intent == Intent::Create {
            return Ok(Err(Blocker::RegisteredForCreate { worktree }));
        }
        return Ok(Ok(Plan {
            intent,
            branch: branch.to_owned(),
            destination: worktree.path.clone(),
            source: Source::Registered(worktree),
            config: None,
            description,
            fetch,
            dry_run,
        }));
    }

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
    if !dry_run
        && fetch.refreshes()
        && let Some(candidate) = approval_candidate(repository, &config)?
    {
        return Ok(Err(Blocker::ApprovalRequired {
            candidate,
            destination,
        }));
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
            let refreshed_repository =
                RepositoryObservation::new(&repository.current().path).repository()?;
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
    if !dry_run
        && !fetch.refreshes()
        && let Some(candidate) = approval_candidate(repository, &config)?
    {
        return Ok(Err(Blocker::ApprovalRequired {
            candidate,
            destination,
        }));
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
/// The output policy is the sole adapter-specific input: human callers stream
/// hooks, while structured callers capture bounded diagnostics.
#[allow(clippy::too_many_lines)] // This is the single explicit worktree execution boundary.
pub(crate) fn execute(
    repository: &Repository,
    plan: &Plan,
    output_policy: OutputPolicy,
    on_created: impl FnOnce() -> Result<()>,
) -> std::result::Result<ExecutionOutcome, ExecutionFailure> {
    let mut effects = planned_effects(plan);
    let empty_output = || match output_policy {
        OutputPolicy::Streamed => HookOutput::Streamed,
        OutputPolicy::Captured => HookOutput::Captured(Vec::new()),
    };
    let failure = |code, error, effects, created, setup_incomplete| ExecutionFailure {
        code,
        error,
        effects,
        created,
        setup_incomplete,
        hook_outcome: None,
        hook_output: empty_output(),
    };
    if let Source::Registered(worktree) = &plan.source {
        return Ok(ExecutionOutcome {
            destination: worktree.path.clone(),
            effects,
            hook_output: empty_output(),
        });
    }
    let config = plan.config.as_ref().expect("creation plan has config");
    if plan.dry_run {
        return Ok(ExecutionOutcome {
            destination: plan.destination.clone(),
            effects,
            hook_output: empty_output(),
        });
    }
    revalidate(repository, plan)
        .map_err(|error| failure("plan_stale", error, effects.clone(), false, false))?;
    if let Some(parent) = plan.destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create destination parent {}", parent.display()))
            .map_err(|error| failure("creation_failed", error, effects.clone(), false, false))?;
    }
    let pending = (!config.post_create.is_empty())
        .then(|| setup::prepare(&repository.common_dir, &plan.branch, &plan.destination))
        .transpose()
        .map_err(|error| failure("setup_failed", error, effects.clone(), false, true))?;
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
        if let Some(pending) = pending
            && let Err(cancel_error) = pending.cancel()
        {
            return Err(failure(
                "creation_failed",
                cancel_error.context(format!(
                    "worktree creation failed ({error:#}) and pending setup state could not be cleared"
                )),
                effects,
                false,
                true,
            ));
        }
        return Err(failure("creation_failed", error, effects, false, false));
    }
    effects[creation_index].completed = true;
    if let Some(index) = branch_index {
        effects[index].completed = true;
    }
    let identity = if let Some(pending) = pending {
        let identity = RepositoryObservation::new(&plan.destination)
            .worktree_identity()
            .map_err(|error| failure("setup_failed", error, effects.clone(), true, true))?;
        pending
            .commit(&repository.common_dir, &identity)
            .map_err(|error| failure("setup_failed", error, effects.clone(), true, true))?;
        Some(identity)
    } else {
        None
    };
    on_created().map_err(|error| {
        failure(
            "setup_failed",
            error,
            effects.clone(),
            true,
            identity.is_some(),
        )
    })?;
    if let Some(description) = plan.description.as_deref() {
        let index = effects
            .iter()
            .position(|effect| effect.action == "set_branch_description")
            .expect("described plans carry a description effect");
        effects[index].attempted = true;
        mutation
            .describe(&plan.branch, description)
            .map_err(|error| {
                failure(
                    "description_failed",
                    error,
                    effects.clone(),
                    true,
                    identity.is_some(),
                )
            })?;
        effects[index].completed = true;
    }
    if let Some(identity) = identity {
        let execution = hook::execute(
            crate::config::HookPhase::PostCreate,
            &config.post_create,
            &plan.destination,
            output_policy,
        )
        .map_err(|error| failure("setup_failed", error, effects.clone(), true, true))?;
        if execution.outcome != HookOutcome::Success {
            return Err(ExecutionFailure {
                code: "setup_failed",
                error: anyhow::anyhow!(
                    "post-create hook outcome: {:?}; setup remains incomplete",
                    execution.outcome
                ),
                effects,
                created: true,
                setup_incomplete: true,
                hook_outcome: Some(execution.outcome),
                hook_output: execution.output,
            });
        }
        setup::clear(&repository.common_dir, &identity, Some(&plan.branch))
            .map_err(|error| failure("setup_failed", error, effects.clone(), true, true))?;
        return Ok(ExecutionOutcome {
            destination: plan.destination.clone(),
            effects,
            hook_output: execution.output,
        });
    }
    Ok(ExecutionOutcome {
        destination: plan.destination.clone(),
        effects,
        hook_output: empty_output(),
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
    let snapshot = Snapshot::observe(repository)?;
    let mode = if base.base_ref.is_some() {
        crate::BaseMode::Fresh
    } else {
        crate::BaseMode::Head
    };
    let configured_target = base
        .base_ref
        .as_ref()
        .map(|base_ref| base_ref.branch.as_str());
    let BaseResolution::Resolved(current) = snapshot.new_branch_base(mode, configured_target)?
    else {
        bail!("the planned new-branch source is no longer available");
    };
    let actual = current.commit;
    if actual != base.commit {
        bail!(
            "planned new-branch source moved from {} to {actual}",
            base.commit
        );
    }
    Ok(())
}

pub(crate) fn planned_effects(plan: &Plan) -> Vec<Effect> {
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
