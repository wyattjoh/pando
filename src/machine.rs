use crate::{
    config::HookPhase,
    git, hook_approval, install,
    protocol::{self, EmptyInput},
    read_only::{self, GetProperty, GetRequest},
    setup::OutputPolicy,
    trust,
    worktree_plan::{
        CreateInput, Intent, OperationContext, OperationInput, OperationResult, SwitchInput,
    },
};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;

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
) -> std::result::Result<(Option<String>, OperationInput), String> {
    if !request_mode {
        return Ok((
            None,
            OperationInput {
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
    let outcome = crate::worktree_plan::operation(intent, &input);
    let failed = outcome.result.is_err();
    emit(
        protocol::adapt(
            command,
            id,
            outcome.result,
            outcome.context,
            outcome.effects,
            outcome.diagnostics,
            outcome.recovery,
        )?,
        failed,
    )
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
        match protocol::read_request::<crate::lifecycle::MergeInput>() {
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
            crate::lifecycle::MergeInput {
                no_rebase,
                no_remove,
                no_squash,
                dry_run,
            },
        )
    };
    let policy = input.policy();
    let plan = match crate::lifecycle::plan_merge(policy) {
        Ok(plan) => plan,
        Err(error) => {
            let outcome = crate::lifecycle::merge_preflight_outcome(&error);
            let response = protocol::adapt(
                "merge",
                id,
                outcome.result,
                outcome.context,
                outcome.effects,
                outcome.diagnostics,
                outcome.recovery,
            )?;
            return emit(response, true);
        }
    };
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
    let outcome = crate::lifecycle::merge_outcome(
        &plan,
        &input,
        pre_merge_approval.as_ref(),
        crate::lifecycle::MergeExecutionMode::Captured,
    );
    let failed = outcome.result.is_err();
    let response = protocol::adapt(
        "merge",
        id,
        outcome.result,
        outcome.context,
        outcome.effects,
        outcome.diagnostics,
        outcome.recovery,
    )?;
    emit(response, failed)
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
        "merge" => json!(schemars::schema_for!(
            protocol::Request<crate::lifecycle::MergeInput>
        )),
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
            crate::worktree_plan::SWITCH_ERRORS,
            crate::worktree_plan::SWITCH_ACTIONS,
        ),
        "create" => (
            crate::worktree_plan::CREATE_ERRORS,
            crate::worktree_plan::CREATE_ACTIONS,
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
            crate::lifecycle::MERGE_ERRORS,
            crate::lifecycle::MERGE_ACTIONS,
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
        "merge" => json!(schemars::schema_for!(crate::lifecycle::MergeResult)),
        "switch" | "create" => json!(schemars::schema_for!(OperationResult)),
        _ => Value::Null,
    };
    let selection_required_context_schema = if command == "switch" {
        json!(schemars::schema_for!(OperationContext))
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
