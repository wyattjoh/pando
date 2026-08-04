use crate::{
    WorktreeKind,
    config::HookPhase,
    git, hook_approval, install,
    protocol::{self, BytePath, EmptyInput},
    read_only::{self, GetProperty, GetRequest},
    setup::{self, OutputPolicy},
    trust,
    worktree_plan::Intent,
};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct SwitchInput {
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    remote: Option<String>,
    /// Refresh the resolved base ref before creating a genuinely new branch.
    #[serde(default)]
    fetch: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    remote: Option<String>,
    /// Refresh the resolved base ref before creating a genuinely new branch.
    #[serde(default)]
    fetch: bool,
    #[serde(default)]
    dry_run: bool,
    /// Repository-local Git description to set on the resolved branch.
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug)]
struct ResolveInput {
    branch: Option<String>,
    remote: Option<String>,
    fetch: bool,
    dry_run: bool,
    description: Option<String>,
}

impl From<SwitchInput> for ResolveInput {
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

impl From<CreateInput> for ResolveInput {
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

#[derive(Debug, JsonSchema, Serialize)]
struct SwitchChoice {
    branch: Option<String>,
    destination: BytePath,
    current: bool,
    /// RFC 3339 committer timestamp for the worktree's HEAD commit.
    last_commit_at: Option<String>,
    retry: Value,
}

#[derive(Debug, JsonSchema, Serialize)]
struct SwitchSelectionContext {
    choices: Vec<SwitchChoice>,
    unregistered_branch: Value,
}

fn checked_request<I>(
    request: protocol::Request<I>,
) -> std::result::Result<(Option<String>, I), String> {
    if request.schema_version != protocol::SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema version {}",
            request.schema_version
        ));
    }
    Ok((request.request_id, request.input))
}

fn resolve_request(
    intent: Intent,
    request_mode: bool,
    branch: Option<String>,
) -> std::result::Result<(Option<String>, ResolveInput), String> {
    if !request_mode {
        return Ok((
            None,
            ResolveInput {
                branch,
                remote: None,
                fetch: false,
                dry_run: false,
                description: None,
            },
        ));
    }
    if branch.is_some() {
        return Err("command arguments are forbidden with --input-output json".into());
    }
    match intent {
        Intent::Switch => {
            let (id, input) = checked_request(protocol::read_request::<SwitchInput>()?)?;
            Ok((id, input.into()))
        }
        Intent::Create => {
            let (id, input) = checked_request(protocol::read_request::<CreateInput>()?)?;
            Ok((id, input.into()))
        }
    }
}

/// Runs the non-interactive switch interface.
///
/// # Errors
/// Returns an error only when response output fails or an underlying Git operation cannot be represented locally.
pub fn switch(
    request_mode: bool,
    branch: Option<String>,
    fetch: bool,
    dry_run: bool,
) -> Result<()> {
    resolve(Intent::Switch, request_mode, branch, fetch, dry_run)
}

/// Runs the non-interactive create interface.
///
/// Unlike [`switch`], this is the one machine entry point permitted to create a genuinely
/// new branch without a human confirmation.
///
/// # Errors
/// Returns an error only when response output fails or an underlying Git operation cannot be represented locally.
pub fn create(
    request_mode: bool,
    branch: Option<String>,
    fetch: bool,
    dry_run: bool,
) -> Result<()> {
    resolve(Intent::Create, request_mode, branch, fetch, dry_run)
}

#[allow(clippy::too_many_lines)]
fn resolve(
    intent: Intent,
    request_mode: bool,
    branch: Option<String>,
    fetch: bool,
    dry_run: bool,
) -> Result<()> {
    let command = intent.id();
    if request_mode && (dry_run || fetch) {
        return emit_err(
            command,
            None,
            "json.invalid_request",
            "command options are forbidden with --input-output json",
        );
    }
    let (id, mut input) = match resolve_request(intent, request_mode, branch) {
        Ok(value) => value,
        Err(error) => return emit_err(command, None, "json.invalid_request", error),
    };
    if !request_mode {
        input.dry_run = dry_run;
        input.fetch = fetch;
    }
    let repo = match if input.branch.is_none() && intent == Intent::Switch {
        navigation_repository()
    } else {
        repository()
    } {
        Ok(value) => value,
        Err(error) => return emit_err(command, id, "repository.invalid", format!("{error:#}")),
    };
    let Some(branch) = input.branch else {
        if intent == Intent::Create {
            return emit_err(
                command,
                id,
                "create.branch_required",
                "create requires a branch name in input.branch",
            );
        }
        let choices: Vec<_> = repo
            .worktrees
            .iter()
            .filter(|worktree| worktree.navigable())
            .map(|worktree| {
                let branch = match &worktree.kind {
                    WorktreeKind::Branch(value) => Some(value.clone()),
                    _ => None,
                };
                SwitchChoice {
                    retry: json!({"argv":["pando","--input-output","json","switch"],"stdin":{"schema_version":1,"input":{"branch":branch.clone()}}, "working_directory":BytePath::path(&repo.current().path)}),
                    branch,
                    destination: BytePath::path(&worktree.path),
                    current: worktree.current,
                    last_commit_at: worktree.machine_last_commit_at(),
                }
            })
            .collect();
        let mut response = protocol::failure(
            "switch",
            id,
            "switch.selection_required",
            "select a registered worktree, or provide a branch name to resolve or create",
        );
        response.context = serde_json::to_value(SwitchSelectionContext {
            choices,
            unregistered_branch: json!({"description":"provide input.branch with a branch name not shown above"}),
        })?;
        if let Some(warning) = &repo.metadata_warning {
            push_diagnostic(
                &mut response,
                "git.commit_metadata",
                "metadata",
                warning.as_bytes(),
            );
        }
        return emit(response, true);
    };
    let shared_plan = match crate::worktree_plan::plan(
        &repo,
        intent,
        &branch,
        input.remote.as_deref(),
        crate::worktree_plan::FetchIntent::new(input.fetch, input.dry_run),
        input.description.clone(),
        input.dry_run,
    )? {
        Ok(plan) => plan,
        Err(crate::worktree_plan::Blocker::InvalidBranch { message }) => {
            return emit_err(command, id, &format!("{command}.invalid_branch"), message);
        }
        Err(crate::worktree_plan::Blocker::ConfigInvalid { message }) => {
            return emit_err(command, id, &format!("{command}.config_invalid"), message);
        }
        Err(crate::worktree_plan::Blocker::RegisteredForCreate { worktree }) => {
            let mut response = protocol::failure(
                command,
                id,
                "create.branch_registered",
                "the branch already has a registered worktree; create will not adopt or replace it",
            );
            response.context =
                json!({"branch":branch,"destination":BytePath::path(&worktree.path)});
            response.next_steps.push(protocol::NextStep {
                action: "switch".into(),
                description: "Enter the registered worktree instead of creating one".into(),
                mutation: "none".into(),
                requires_human_approval: false,
                invocation: json!({"argv":["pando","--input-output","json","switch"],"stdin":{"schema_version":1,"input":{"branch":branch}},"working_directory":BytePath::path(&repo.current().path)}),
            });
            return emit(response, true);
        }
        Err(crate::worktree_plan::Blocker::RemoteSelectionRequired { remotes, .. }) => {
            let mut response = protocol::failure(
                command,
                id,
                &format!("{command}.remote_selection_required"),
                "multiple fetched remotes match this branch",
            );
            response.context = json!({"branch":branch,"remotes":remotes});
            for remote in &remotes {
                response.next_steps.push(protocol::NextStep {
                    action: "retry_with_remote".into(),
                    description: format!("Retry with {remote} as the selected source"),
                    mutation: "none".into(),
                    requires_human_approval: false,
                    invocation: json!({
                        "argv": ["pando", "--input-output", "json", command],
                        "stdin": {"schema_version": 1, "input": {
                            "branch": branch,
                            "remote": remote,
                            "fetch": input.fetch,
                            "dry_run": input.dry_run,
                        }},
                        "working_directory": BytePath::path(&repo.current().path),
                    }),
                });
            }
            return emit(response, true);
        }
        Err(crate::worktree_plan::Blocker::FetchNotApplicable { message }) => {
            return emit_err(
                command,
                id,
                &format!("{command}.fetch_not_applicable"),
                message,
            );
        }
        Err(crate::worktree_plan::Blocker::BaseUnavailable { message }) => {
            return emit_err(command, id, &format!("{command}.base_unavailable"), message);
        }
        Err(crate::worktree_plan::Blocker::ApprovalRequired {
            candidate,
            destination,
        }) => {
            let mut response = protocol::failure(
                command,
                id,
                "trust.approval_required",
                "post-create hooks require manual review and approval before mutation",
            );
            response.context = json!({
                "approval": {
                    "phase": candidate.phase().key(),
                    "commands": candidate.commands().iter().map(|step| json!({
                        "name": step.name,
                        "command": step.command,
                    })).collect::<Vec<_>>(),
                    "repository": candidate.repository(),
                    "identity": candidate.identity(),
                },
                "branch": branch,
                "destination": BytePath::path(&destination),
            });
            response.next_steps.push(protocol::NextStep {
                action: "trust.approve_hooks".into(),
                description: "Review and approve post-create hooks interactively".into(),
                mutation: "trust".into(),
                requires_human_approval: true,
                invocation: json!({
                    "argv": ["pando", command, branch],
                    "stdin": null,
                    "working_directory": BytePath::path(&repo.current().path),
                }),
            });
            return emit(response, true);
        }
        Err(crate::worktree_plan::Blocker::DestinationUnavailable { worktree }) => {
            return emit_err(
                command,
                id,
                &format!("{command}.destination_unavailable"),
                format!("registered destination is {}", worktree.state_label()),
            );
        }
        Err(crate::worktree_plan::Blocker::PrimaryUnavailable) => {
            return emit_err(
                command,
                id,
                "repository.primary_unavailable",
                "a bare repository cannot create a worktree",
            );
        }
        Err(crate::worktree_plan::Blocker::RootUnavailable { message }) => {
            return emit_err(command, id, "repository.root_unavailable", message);
        }
        Err(crate::worktree_plan::Blocker::DestinationInvalid { message }) => {
            return emit_err(
                command,
                id,
                &format!("{command}.destination_invalid"),
                message,
            );
        }
        Err(crate::worktree_plan::Blocker::DestinationCollision) => {
            return emit_err(
                command,
                id,
                &format!("{command}.destination_collision"),
                "the configured destination already exists or is registered",
            );
        }
        Err(crate::worktree_plan::Blocker::DestinationNotIgnored { first, gitignore }) => {
            return emit_err(
                command,
                id,
                &format!("{command}.destination_invalid"),
                format!(
                    "the configured destination is inside the primary worktree but is not ignored; add '/{first}/' to {}",
                    gitignore.display()
                ),
            );
        }
        Err(crate::worktree_plan::Blocker::IrrelevantRemote) => {
            return emit_err(
                command,
                id,
                &format!("{command}.irrelevant_remote"),
                "remote does not apply to the resolved branch",
            );
        }
        Err(crate::worktree_plan::Blocker::UnknownRemote) => {
            return emit_err(
                command,
                id,
                &format!("{command}.unknown_remote"),
                "remote does not match an available fetched branch",
            );
        }
    };
    debug_assert_eq!(shared_plan.intent, intent);
    debug_assert_eq!(shared_plan.branch, branch);
    let destination = shared_plan.destination.clone();
    if let crate::worktree_plan::Source::Registered(worktree) = &shared_plan.source {
        return emit(
            protocol::success(
                command,
                id,
                json!({"outcome":"existing","branch":branch,"destination":BytePath::path(&worktree.path),"dry_run":input.dry_run}),
                json!({}),
                vec![],
            ),
            false,
        );
    }
    let selected_remote = match &shared_plan.source {
        crate::worktree_plan::Source::Remote { reference, .. } => Some(reference.clone()),
        _ => None,
    };
    let new_base = match &shared_plan.source {
        crate::worktree_plan::Source::New { base } => Some(base.clone()),
        _ => None,
    };
    if let (Intent::Switch, Some(base)) = (intent, new_base.as_ref()) {
        if !input.dry_run {
            return emit_err(
                "switch",
                id,
                "switch.approval_required",
                "creating a genuinely new branch requires a manual human invocation",
            );
        }
        let effects = crate::worktree_plan::planned_effects(&shared_plan);
        let mut result = json!({"outcome":"creation_plan","kind":"new","branch":branch,"destination":BytePath::path(&destination),"start_point":base.commit,"approval_required":true});
        if let Some(base_ref) = &base.base_ref {
            result["base_ref"] = json!(base_ref.reference());
        }
        return emit(
            protocol::success("switch", id, result, json!({}), effects),
            false,
        );
    }
    {
        let execution = crate::worktree_plan::execute(
            &repo,
            &shared_plan,
            crate::setup::OutputPolicy::Captured,
            || Ok(()),
        );
        let outcome = match execution {
            Ok(outcome) => outcome,
            Err(failure) => {
                let code = match failure.code {
                    "description_failed" => "create.description_failed".to_owned(),
                    "setup_failed" => format!("{command}.setup_failed"),
                    "plan_stale" => format!("{command}.plan_stale"),
                    _ => format!("{command}.creation_failed"),
                };
                let mut response =
                    protocol::failure(command, id, &code, format!("{:#}", failure.error));
                response.context = json!({
                    "branch": branch,
                    "destination": BytePath::path(&destination),
                    "created": failure.created,
                    "setup": failure.setup_incomplete.then_some("incomplete"),
                    "hook_outcome": failure.hook_outcome.map(|outcome| format!("{outcome:?}")),
                });
                response.effects = failure.effects;
                if let crate::setup::HookOutput::Captured(output) = failure.hook_output {
                    push_hook_diagnostics(&mut response, output);
                }
                if code == "create.description_failed" {
                    if let Some(description) = input.description.as_deref() {
                        response.next_steps.push(protocol::NextStep {
                            action: "git.set_branch_description".into(),
                            description: "Set the requested branch description in repository-local Git configuration".into(),
                            mutation: "config".into(),
                            requires_human_approval: false,
                            invocation: json!({
                                "argv":["git","config","--local","--replace-all",format!("branch.{branch}.description"),description],
                                "stdin":null,
                                "working_directory":BytePath::path(&repo.current().path)
                            }),
                        });
                    }
                }
                if failure.setup_incomplete {
                    response.next_steps.push(protocol::NextStep {
                        action: format!("{command}.recover_setup"),
                        description: "Inspect the worktree and retry or explicitly complete setup interactively".into(),
                        mutation: "setup".into(),
                        requires_human_approval: true,
                        invocation: json!({"argv":["pando","switch",branch],"stdin":null,"working_directory":BytePath::path(&repo.current().path)}),
                    });
                }
                return emit(response, true);
            }
        };
        let effects = outcome.effects;
        let hook_output = outcome.hook_output;
        let mut result = json!({
            "outcome": if input.dry_run { "creation_plan" } else { "created" },
            "branch": branch,
            "destination": BytePath::path(&destination),
            "remote": selected_remote,
        });
        if let Some(base) = new_base {
            result["kind"] = json!("new");
            result["start_point"] = json!(base.commit);
            if let Some(base_ref) = &base.base_ref {
                result["base_ref"] = json!(base_ref.reference());
            }
        }
        let mut response = protocol::success(command, id, result, json!({}), effects);
        if let crate::setup::HookOutput::Captured(output) = hook_output {
            push_hook_diagnostics(&mut response, output);
        }
        emit(response, false)
    }
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct DryRunInput {
    #[serde(default)]
    dry_run: bool,
}

fn repository() -> Result<git::Repository> {
    git::repository(&env::current_dir().context("failed to read current directory")?)
}

fn navigation_repository() -> Result<git::Repository> {
    git::repository_with_metadata(&env::current_dir().context("failed to read current directory")?)
}

fn read_list_request(request_mode: bool) -> Result<Option<String>> {
    if !request_mode {
        return Ok(None);
    }
    match protocol::read_optional_request::<read_only::ListRequest>() {
        Ok(request) if request.schema_version == protocol::SCHEMA_VERSION => Ok(request.request_id),
        Ok(request) => {
            emit_err(
                "list",
                request.request_id,
                "json.unsupported_schema_version",
                "unsupported schema version",
            )?;
            unreachable!()
        }
        Err(error) => {
            emit_err("list", None, "json.invalid_request", error)?;
            unreachable!()
        }
    }
}

/// Emits the structured worktree list.
///
/// # Errors
/// Returns an error when stdout cannot be written.
pub fn list(request_mode: bool) -> Result<()> {
    let id = read_list_request(request_mode)?;
    match read_only::list_worktrees() {
        Ok(outcome) => emit(
            protocol::adapt(
                "list",
                id,
                Ok::<_, read_only::QueryFailure>(outcome.result),
                read_only::QueryContext::default(),
                vec![],
                outcome.diagnostics,
                Vec::<protocol::RecoveryAction<EmptyInput>>::new(),
            )?,
            false,
        ),
        Err(failure) => emit(
            protocol::adapt(
                "list",
                id,
                Err::<read_only::ListResult, _>(failure),
                read_only::QueryContext::default(),
                vec![],
                vec![],
                Vec::<protocol::RecoveryAction<EmptyInput>>::new(),
            )?,
            true,
        ),
    }
}

/// Emits the structured branch list, in `for-each-ref` order.
///
/// # Errors
/// Returns an error when stdout cannot be written.
pub fn list_branches(request_mode: bool) -> Result<()> {
    let id = read_list_request(request_mode)?;
    match read_only::list_branches() {
        Ok(outcome) => emit(
            protocol::adapt(
                "list",
                id,
                Ok::<_, read_only::QueryFailure>(outcome.result),
                read_only::QueryContext::default(),
                vec![],
                outcome.diagnostics,
                Vec::<protocol::RecoveryAction<EmptyInput>>::new(),
            )?,
            false,
        ),
        Err(failure) => emit(
            protocol::adapt(
                "list",
                id,
                Err::<read_only::BranchListResult, _>(failure),
                read_only::QueryContext::default(),
                vec![],
                vec![],
                Vec::<protocol::RecoveryAction<EmptyInput>>::new(),
            )?,
            true,
        ),
    }
}

/// Emits one naturally typed current-worktree property.
///
/// # Errors
/// Returns an error when stdout cannot be written or a path cannot be resolved.
pub fn get(request_mode: bool, argv: Option<&str>) -> Result<()> {
    let (id, property) = if request_mode {
        if argv.is_some() {
            return emit_err(
                "get",
                None,
                "json.invalid_request",
                "command arguments are forbidden with --input-output json",
            );
        }
        match protocol::read_request::<GetRequest>() {
            Ok(request) if request.schema_version == protocol::SCHEMA_VERSION => {
                (request.request_id, request.input.property)
            }
            Ok(request) => {
                return emit_err(
                    "get",
                    request.request_id,
                    "json.unsupported_schema_version",
                    "unsupported schema version",
                );
            }
            Err(error) => return emit_err("get", None, "json.invalid_request", error),
        }
    } else {
        (
            None,
            match argv.unwrap_or("") {
                "branch" => GetProperty::Branch,
                "port" => GetProperty::Port,
                "worktree-path" => GetProperty::WorktreePath,
                "primary-worktree-path" => GetProperty::PrimaryWorktreePath,
                "worktree-root" => GetProperty::WorktreeRoot,
                _ => return emit_err("get", None, "get.invalid_property", "invalid property"),
            },
        )
    };
    let result = read_only::get(property);
    let failed = result.is_err();
    emit(
        protocol::adapt(
            "get",
            id,
            result,
            read_only::QueryContext::default(),
            vec![],
            vec![],
            Vec::<protocol::RecoveryAction<EmptyInput>>::new(),
        )?,
        failed,
    )
}

/// Emits trust status, reset, or approval-preview outcomes.
///
/// # Errors
/// Returns an error when configuration or trust storage cannot be inspected or stdout cannot be written.
#[allow(clippy::too_many_lines)]
pub fn trust(command: &str, request_mode: bool, dry_run_flag: bool) -> Result<()> {
    let input = if request_mode {
        if dry_run_flag {
            return emit_err(
                command,
                None,
                "json.invalid_request",
                "command options are forbidden with --input-output json",
            );
        }
        if matches!(
            command,
            "trust.status" | "trust.commit_status" | "trust.merge_status"
        ) {
            match protocol::read_optional_request::<EmptyInput>() {
                Ok(r) if r.schema_version == 1 => (r.request_id, false),
                Ok(r) => {
                    return emit_err(
                        command,
                        r.request_id,
                        "json.unsupported_schema_version",
                        "unsupported schema version",
                    );
                }
                Err(e) => return emit_err(command, None, "json.invalid_request", e),
            }
        } else {
            match protocol::read_request::<DryRunInput>() {
                Ok(r) if r.schema_version == 1 => (r.request_id, r.input.dry_run),
                Ok(r) => {
                    return emit_err(
                        command,
                        r.request_id,
                        "json.unsupported_schema_version",
                        "unsupported schema version",
                    );
                }
                Err(e) => return emit_err(command, None, "json.invalid_request", e),
            }
        }
    } else {
        (None, dry_run_flag)
    };
    let (id, dry) = input;
    let repo = match repository() {
        Ok(v) => v,
        Err(e) => return emit_err(command, id, "repository.invalid", format!("{e:#}")),
    };
    let leaf = match command {
        "trust.status" => trust::Command::HooksStatus,
        "trust.reset" => trust::Command::HooksReset,
        "trust.commit_status" => trust::Command::CommitStatus,
        "trust.commit_reset" => trust::Command::CommitReset,
        "trust.commit_approve" => trust::Command::CommitApprove,
        "trust.merge_status" => trust::Command::MergeStatus,
        "trust.merge_reset" => trust::Command::MergeReset,
        "trust.merge_approve" => trust::Command::MergeApprove,
        // PR trust leaves intentionally retain their published version 1 refusal.
        _ => {
            return emit_err(
                command,
                id,
                "trust.json_unsupported",
                format!("{command} does not support structured output; run it interactively"),
            );
        }
    };
    let outcome = trust::execute(&repo, leaf, dry)?;
    let failed = outcome.result.is_err();
    let response = protocol::adapt(
        leaf.id(),
        id,
        outcome.result,
        outcome.context,
        outcome.effects,
        Vec::new(),
        outcome.recovery,
    )?;
    emit(response, failed)
}

/// Plans or executes structured worktree removal.
///
/// # Errors
/// Returns an error when repository state cannot be inspected, mutation fails, or stdout cannot be written.
pub fn remove(request_mode: bool, branches: Vec<String>, force: bool, dry_run: bool) -> Result<()> {
    let (id, input) = if request_mode {
        if !branches.is_empty() || dry_run {
            return emit_err(
                "remove",
                None,
                "json.invalid_request",
                "command options are forbidden with --input-output json",
            );
        }
        match protocol::read_request::<crate::lifecycle::RemovalInput>() {
            Ok(request) if request.schema_version == 1 => (request.request_id, request.input),
            Ok(request) => {
                return emit_err(
                    "remove",
                    request.request_id,
                    "json.unsupported_schema_version",
                    "unsupported schema version",
                );
            }
            Err(error) => return emit_err("remove", None, "json.invalid_request", error),
        }
    } else {
        (None, crate::lifecycle::RemovalInput { branches, dry_run })
    };

    let plan = match crate::lifecycle::plan_remove(&input.branches, force) {
        Ok(plan) => plan,
        Err(error) => {
            let code = match error.kind {
                crate::lifecycle::PreflightFailureKind::DuplicateTarget => {
                    "remove.duplicate_target"
                }
                crate::lifecycle::PreflightFailureKind::PrimaryForbidden => {
                    "remove.primary_forbidden"
                }
                crate::lifecycle::PreflightFailureKind::ForceRequired => "remove.force_required",
                crate::lifecycle::PreflightFailureKind::LifecycleActive => {
                    "remove.lifecycle_active"
                }
                crate::lifecycle::PreflightFailureKind::JournalInvalid => "remove.journal_invalid",
                crate::lifecycle::PreflightFailureKind::UnknownTarget => "remove.unknown_target",
                _ => "remove.preflight_failed",
            };
            return emit_err("remove", id, code, error.to_string());
        }
    };
    let outcome = if input.dry_run {
        crate::lifecycle::RemovalOutcome {
            result: Ok(crate::lifecycle::RemovalResult::DryRun {
                targets: plan.context.targets.clone(),
                force,
            }),
            context: crate::lifecycle::RemovalOutcomeContext {
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
        }
    } else {
        crate::lifecycle::removal_outcome(
            crate::lifecycle::execute_removal_with_policy(&plan, OutputPolicy::Captured),
            &input,
        )
    };
    let failed = outcome.result.is_err();
    let response = protocol::adapt(
        "remove",
        id,
        outcome.result,
        outcome.context,
        outcome.effects,
        outcome.diagnostics,
        outcome.recovery,
    )?;
    emit(response, failed)
}

fn push_hook_diagnostics(
    response: &mut protocol::Response,
    steps: Vec<crate::setup::CapturedStep>,
) {
    for step in steps {
        push_captured_diagnostic(response, "hook", "stdout", &step.stdout);
        push_captured_diagnostic(response, "hook", "stderr", &step.stderr);
    }
}

fn push_captured_diagnostic(
    response: &mut protocol::Response,
    source: &str,
    stream: &str,
    captured: &setup::CapturedStream,
) {
    if captured.original_size == 0 {
        return;
    }
    response.diagnostics.push(crate::protocol::Diagnostic {
        source: source.into(),
        stream: stream.into(),
        content: String::from_utf8_lossy(&captured.content).into_owned(),
        original_size: captured.original_size,
        truncated: captured.truncated,
    });
}

fn push_diagnostic(response: &mut protocol::Response, source: &str, stream: &str, bytes: &[u8]) {
    const LIMIT: usize = 16 * 1024;
    if bytes.is_empty() {
        return;
    }
    let kept = &bytes[..bytes.len().min(LIMIT)];
    response.diagnostics.push(crate::protocol::Diagnostic {
        source: source.into(),
        stream: stream.into(),
        content: String::from_utf8_lossy(kept).into_owned(),
        original_size: bytes.len(),
        truncated: bytes.len() > LIMIT,
    });
}
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Each flag is an independent policy opt-out.
struct MergeInput {
    #[serde(default)]
    no_rebase: bool,
    #[serde(default)]
    no_remove: bool,
    #[serde(default)]
    no_squash: bool,
    #[serde(default)]
    dry_run: bool,
}

/// Plans or executes the structured merge lifecycle.
///
/// # Errors
/// Returns an error when repository state cannot be inspected, mutation fails, or stdout cannot be written.
///
/// # Panics
///
/// Panics if a recorded lifecycle phase was not planned, which the effect list
/// above makes impossible.
#[allow(clippy::too_many_lines, clippy::fn_params_excessive_bools)]
pub fn merge(
    request_mode: bool,
    no_rebase: bool,
    no_remove: bool,
    no_squash: bool,
    dry_run: bool,
) -> Result<()> {
    let (id, input) = if request_mode {
        if no_rebase || no_remove || no_squash || dry_run {
            return emit_err(
                "merge",
                None,
                "json.invalid_request",
                "command options are forbidden with --input-output json",
            );
        }
        match protocol::read_request::<MergeInput>() {
            Ok(r) if r.schema_version == 1 => (r.request_id, r.input),
            Ok(r) => {
                return emit_err(
                    "merge",
                    r.request_id,
                    "json.unsupported_schema_version",
                    "unsupported schema version",
                );
            }
            Err(e) => return emit_err("merge", None, "json.invalid_request", e),
        }
    } else {
        (
            None,
            MergeInput {
                no_rebase,
                no_remove,
                no_squash,
                dry_run,
            },
        )
    };
    let policy =
        crate::lifecycle::MergePolicy::new(input.no_rebase, input.no_remove, input.no_squash);
    let plan = match crate::lifecycle::plan_merge(policy) {
        Ok(plan) => plan,
        Err(e) => {
            let message = e.to_string();
            let code = match e.kind {
                crate::lifecycle::PreflightFailureKind::PolicyConflict => "merge.policy_conflict",
                crate::lifecycle::PreflightFailureKind::Dirty => "merge.dirty",
                crate::lifecycle::PreflightFailureKind::NotFastForwardable => {
                    "merge.not_fast_forwardable"
                }
                crate::lifecycle::PreflightFailureKind::NothingToMerge => "merge.nothing_to_merge",
                crate::lifecycle::PreflightFailureKind::SquashGeneratorMissing => {
                    "merge.squash_generator_missing"
                }
                _ => "merge.blocked",
            };
            return emit_err("merge", id, code, message);
        }
    };
    // An in-place merge owns no topic worktree, so removal and the `cd` destination never apply.
    let removes = !input.no_remove && !plan.context.in_place;
    let pre_merge_approval = if plan.context.cleanup_pending {
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
                return emit_err("merge", id, "trust.read_failed", format!("{error:#}"));
            }
        }
    };
    let mut context = serde_json::to_value(&plan.context)?;
    if let Some(candidate) = &pre_merge_approval {
        context["approval"] = json!({
            "phase": candidate.phase().key(),
            "commands": candidate.commands().iter().map(|step| json!({
                "name": step.name,
                "command": step.command,
            })).collect::<Vec<_>>(),
            "repository": candidate.repository(),
            "identity": candidate.identity(),
        });
    }
    let mut effects = plan.effects.clone();
    let approval_blocked = if plan.context.cleanup_pending {
        !plan.context.pre_remove_hooks_trusted
    } else {
        pre_merge_approval.is_some() || plan.squash.approval_required()
    };
    if input.dry_run {
        let mut response = protocol::success(
            "merge",
            id,
            json!({"outcome":"dry_run","plan":if plan.context.in_place{"in_place"}else if input.no_remove{"retained_topic"}else{"cleanup"},"policy":plan.context.policy,"ready":!approval_blocked,"approval_required":approval_blocked}),
            context,
            effects,
        );
        if approval_blocked {
            response.next_steps.push(protocol::NextStep { action:"trust.review".into(), description:"Review and explicitly trust the configured lifecycle hooks".into(), mutation:"trust".into(), requires_human_approval:true, invocation:json!({"argv":["pando","trust","show"],"working_directory":plan.context.topic_worktree}) });
        }
        return emit(response, false);
    }
    if approval_blocked {
        let squash_blocked = !plan.context.cleanup_pending && plan.squash.approval_required();
        let mut response = if squash_blocked {
            protocol::failure(
                "merge",
                id,
                "merge.squash_approval_required",
                "the shared squash message generator is not trusted",
            )
        } else {
            protocol::failure(
                "merge",
                id,
                "merge.hook_approval_required",
                "configured lifecycle hooks are not trusted",
            )
        };
        response.context = context;
        response.effects = effects;
        response.next_steps.push(if squash_blocked {
            protocol::NextStep { action:"trust.review_squash_generator".into(), description:"Review and explicitly trust the shared squash message generator before retrying, or retry with no_squash".into(), mutation:"trust".into(), requires_human_approval:true, invocation:json!({"argv":["pando","trust","merge-approve"],"working_directory":plan.context.topic_worktree}) }
        } else {
            protocol::NextStep { action:"trust.review".into(), description:"Review and explicitly trust the configured lifecycle hooks before retrying".into(), mutation:"trust".into(), requires_human_approval:true, invocation:json!({"argv":["pando","trust","show"],"working_directory":plan.context.topic_worktree}) }
        });
        return emit(response, true);
    }
    if plan.is_retained_execution() {
        let outcome =
            crate::lifecycle::execute_merge(&plan, crate::lifecycle::MergeExecutionMode::Captured);
        let mut response = if let Some((kind, message)) = &outcome.failure {
            let code = match kind {
                crate::lifecycle::MergeExecutionFailureKind::StalePlan => "merge.stale_plan",
                crate::lifecycle::MergeExecutionFailureKind::Rebase => "merge.rebase_conflict",
                crate::lifecycle::MergeExecutionFailureKind::Squash
                | crate::lifecycle::MergeExecutionFailureKind::Integration => {
                    "merge.execution_failed"
                }
                crate::lifecycle::MergeExecutionFailureKind::Validation => {
                    "merge.validation_failed"
                }
                crate::lifecycle::MergeExecutionFailureKind::Cleanup => "merge.cleanup_failed",
                crate::lifecycle::MergeExecutionFailureKind::Removal => "merge.remove_failed",
                crate::lifecycle::MergeExecutionFailureKind::Journal
                | crate::lifecycle::MergeExecutionFailureKind::JournalCleanup => {
                    "merge.journal_failed"
                }
            };
            protocol::failure("merge", id, code, message)
        } else {
            protocol::success(
                "merge",
                id,
                json!({"outcome":if plan.context.in_place{"in_place"}else{"retained"},"destination":outcome.destination}),
                json!({"initial":plan.context,"phase":outcome.context.phase}),
                outcome.effects.clone(),
            )
        };
        if outcome.failure.is_some() {
            response.context = serde_json::to_value(&outcome.context)?;
            response.effects = outcome.effects;
            response.next_steps.push(protocol::NextStep { action:"merge.retry".into(), description:"Resolve the reported blocker and retry the journaled lifecycle with its pinned policy".into(), mutation:"repository".into(), requires_human_approval:false, invocation:json!({"argv":["pando","merge","--input-output","json"],"stdin":{"schema_version":1,"input":input},"working_directory":plan.context.topic_worktree}) });
        }
        for diagnostic in outcome.diagnostics {
            push_diagnostic(
                &mut response,
                diagnostic.phase,
                diagnostic.stream,
                &diagnostic.content,
            );
        }
        let failed = outcome.failure.is_some();
        return emit(response, failed);
    }
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("merge")
        .current_dir(&plan.repository.current().path)
        .stdin(std::process::Stdio::null());
    if input.no_rebase {
        command.arg("--no-rebase");
    }
    if input.no_remove {
        command.arg("--no-remove");
    }
    if input.no_squash {
        command.arg("--no-squash");
    }
    let output = command.output()?;
    // The human lifecycle remains the single crash-recovery engine; its streams are captured here.
    let after = crate::lifecycle::plan_merge(policy).ok();
    let succeeded = output.status.success();
    let after_cleanup = after.as_ref().is_some_and(|p| p.context.cleanup_pending);
    let after_journaled = after.as_ref().is_some_and(|p| p.context.journaled);
    let after_rebase = after.as_ref().is_some_and(|p| p.context.rebase_active);
    // Record only phases proven by the journal/repository state. A subprocess failure
    // must never make later lifecycle phases look attempted merely because it exited.
    // The topic no longer needs squashing once it has been collapsed, so a
    // replanned context that dropped the flag proves the phase ran.
    let after_squashed = after.as_ref().is_some_and(|p| !p.context.squashes);
    let rebase_applicable = plan.needs_rebase || plan.context.rebase_active;
    let integration_attempted = !plan.context.cleanup_pending && !after_rebase;
    let cleanup_attempted = removes && (plan.context.cleanup_pending || after_cleanup || succeeded);
    // Address effects by action rather than index; the phase list grows.
    let mut record = |action: &str, attempted: bool, completed: bool| {
        let effect = effects
            .iter_mut()
            .find(|effect| effect.action == action)
            .expect("every recorded phase is planned above");
        effect.attempted = attempted;
        effect.completed = completed;
    };
    record(
        "journal",
        !plan.context.journaled,
        plan.context.journaled || after_journaled || succeeded,
    );
    record(
        "rebase",
        rebase_applicable,
        rebase_applicable && !after_rebase && (after_cleanup || succeeded),
    );
    record(
        "squash",
        plan.context.squashes && integration_attempted,
        plan.context.squashes && (after_squashed || after_cleanup || succeeded),
    );
    record(
        "pre_merge_hooks",
        integration_attempted,
        after_cleanup || succeeded,
    );
    record(
        "fast_forward_merge",
        integration_attempted,
        after_cleanup || succeeded,
    );
    record("pre_remove_hooks", cleanup_attempted, removes && succeeded);
    record("remove_worktree", cleanup_attempted, removes && succeeded);
    record("destination", removes && succeeded, removes && succeeded);
    let mut response = if succeeded {
        protocol::success(
            "merge",
            id,
            json!({"outcome":if removes{"removed"}else{"retained"},"destination":if removes{Some(&plan.context.primary_worktree)}else{None}}),
            json!({"initial":plan.context,"phase":"complete"}),
            effects,
        )
    } else {
        let mut r = protocol::failure(
            "merge",
            id,
            if after.as_ref().is_some_and(|p| p.context.rebase_active) {
                "merge.rebase_conflict"
            } else if after.as_ref().is_some_and(|p| p.context.cleanup_pending) {
                "merge.cleanup_failed"
            } else {
                "merge.execution_failed"
            },
            "merge lifecycle did not complete",
        );
        r.context = after.as_ref().map_or(context, |p| {
            serde_json::to_value(&p.context).unwrap_or_default()
        });
        r.effects = effects;
        r
    };
    push_diagnostic(&mut response, "merge", "stdout", &output.stdout);
    push_diagnostic(&mut response, "merge", "stderr", &output.stderr);
    if !output.status.success() {
        response.next_steps.push(protocol::NextStep { action:"merge.retry".into(), description:"Resolve the reported blocker and retry the journaled lifecycle with its pinned policy".into(), mutation:"repository".into(), requires_human_approval:false, invocation:json!({"argv":["pando","merge","--input-output","json"],"stdin":{"schema_version":1,"input":input},"working_directory":plan.context.topic_worktree}) });
    }
    emit(response, !output.status.success())
}

/// Emits a structured installation plan or approval requirement.
///
/// # Errors
/// Returns an error when installation paths cannot be inspected or stdout cannot be written.
pub fn install(request_mode: bool, dry_flag: bool, no_guide: bool) -> Result<()> {
    let (id, dry) = if request_mode {
        if dry_flag || no_guide {
            return emit_err(
                "install",
                None,
                "json.invalid_request",
                "command options are forbidden with --input-output json",
            );
        }
        match protocol::read_request::<DryRunInput>() {
            Ok(r) if r.schema_version == 1 => (r.request_id, r.input.dry_run),
            Ok(r) => {
                return emit_err(
                    "install",
                    r.request_id,
                    "json.unsupported_schema_version",
                    "unsupported schema version",
                );
            }
            Err(e) => return emit_err("install", None, "json.invalid_request", e),
        }
    } else {
        (None, dry_flag)
    };
    let outcome = install::inspect(&install::InstallInput { dry_run: dry })?;
    let failed = outcome.result.is_err();
    let response = protocol::adapt(
        "install",
        id,
        outcome.result,
        protocol::EmptyInput::default(),
        outcome.effects,
        Vec::new(),
        outcome.recovery,
    )?;
    emit(response, failed)
}
/// Builds exact-leaf JSON help from the runtime request and response types.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn help(command: &str) -> Value {
    let request_schema = match command {
        "list" | "trust.status" | "trust.commit_status" | "trust.merge_status" => {
            json!(schemars::schema_for!(
                protocol::OptionalInputRequest<EmptyInput>
            ))
        }
        "switch" => json!(schemars::schema_for!(protocol::Request<SwitchInput>)),
        "create" => json!(schemars::schema_for!(protocol::Request<CreateInput>)),
        "get" => json!(schemars::schema_for!(protocol::Request<GetRequest>)),
        "remove" => json!(schemars::schema_for!(
            protocol::Request<crate::lifecycle::RemovalInput>
        )),
        "merge" => json!(schemars::schema_for!(protocol::Request<MergeInput>)),
        "trust.reset"
        | "trust.commit_reset"
        | "trust.commit_approve"
        | "trust.merge_reset"
        | "trust.merge_approve" => json!(schemars::schema_for!(protocol::Request<DryRunInput>)),
        "install" => json!(schemars::schema_for!(
            protocol::Request<install::InstallInput>
        )),
        _ => Value::Null,
    };
    let (errors, actions): (&[&str], &[&str]) = match command {
        "list" => (read_only::LIST_ERRORS, read_only::ACTIONS),
        "trust.status" | "trust.commit_status" | "trust.merge_status" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
            ],
            &[],
        ),
        "get" => (read_only::GET_ERRORS, read_only::ACTIONS),
        "switch" => (
            &[
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
            ],
            &["fetch_base_ref", "create_branch", "create_worktree"],
        ),
        "create" => (
            &[
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
            ],
            &[
                "fetch_base_ref",
                "create_branch",
                "create_worktree",
                "set_branch_description",
            ],
        ),
        "remove" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
                "remove.unknown_target",
                "remove.force_required",
                "remove.primary_forbidden",
                "remove.target_unavailable",
                "trust.approval_required",
            ],
            &["pre_remove_hooks", "remove_worktree"],
        ),
        "merge" => (
            &[
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
                "merge.rebase_conflict",
                "merge.cleanup_failed",
                "merge.execution_failed",
                "merge.blocked",
            ],
            &[
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
            ],
        ),
        "trust.reset" | "trust.commit_reset" | "trust.merge_reset" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
            ],
            &[command],
        ),
        "trust.commit_approve" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
                "trust.approval_required",
            ],
            &["trust.approve_commit_generator"],
        ),
        "trust.merge_approve" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
                "trust.approval_required",
            ],
            &["trust.approve_merge_generator"],
        ),
        "install" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "install.approval_required",
                "install.write_failed",
            ],
            &["file.write", "install.approve"],
        ),
        _ => (&[], &[]),
    };
    let result_schema = match command {
        "list" => json!(schemars::schema_for!(read_only::ListResult)),
        "get" => json!(schemars::schema_for!(read_only::GetResult)),
        "install" => json!(schemars::schema_for!(install::InstallResult)),
        _ => Value::Null,
    };
    let selection_required_context_schema = if command == "switch" {
        json!(schemars::schema_for!(SwitchSelectionContext))
    } else {
        Value::Null
    };
    json!({"outcome":"help","request_schema":request_schema,"response_schema":schemars::schema_for!(protocol::Response),"result_schema":result_schema,"selection_required_context_schema":selection_required_context_schema,"error_codes":errors,"actions":actions})
}

fn emit_err(c: &str, id: Option<String>, code: &str, msg: impl Into<String>) -> Result<()> {
    emit(protocol::failure(c, id, code, msg), true)
}
#[allow(clippy::needless_pass_by_value)]
fn emit(r: protocol::Response, failed: bool) -> Result<()> {
    protocol::write(&r)?;
    if failed {
        std::process::exit(1)
    }
    Ok(())
}
