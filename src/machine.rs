use crate::{
    BaseMode, Condition, Row, Worktree, WorktreeKind,
    config::{EffectiveConfig, HookPhase},
    git, install,
    protocol::{self, BytePath, Effect, EmptyInput},
    smart, trust,
};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{env, fs};

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
struct WorktreeRecord {
    kind: String,
    branch: Option<String>,
    path: BytePath,
    head: Option<String>,
    /// RFC 3339 committer timestamp for the worktree's HEAD commit.
    last_commit_at: Option<String>,
    condition: String,
    current: bool,
    navigable: bool,
    lock_reason: Option<String>,
    prune_reason: Option<String>,
}

#[derive(Debug, JsonSchema, Serialize)]
struct ListSummary {
    total: usize,
    dirty: usize,
    unknown: usize,
    missing: usize,
    inaccessible: usize,
    bare: usize,
    locked: usize,
    prunable: usize,
}

#[derive(Debug, JsonSchema, Serialize)]
struct ListResult {
    outcome: &'static str,
    worktrees: Vec<WorktreeRecord>,
    summary: ListSummary,
}

#[derive(Debug, JsonSchema, Serialize)]
struct BranchRecord {
    branch: String,
    head: String,
    /// RFC 3339 committer timestamp for the branch tip commit.
    last_commit_at: Option<String>,
    path: Option<BytePath>,
    condition: Option<String>,
    current: bool,
}

#[derive(Debug, JsonSchema, Serialize)]
struct BranchListSummary {
    total: usize,
    checked_out: usize,
    dirty: usize,
}

#[derive(Debug, JsonSchema, Serialize)]
struct BranchListResult {
    outcome: &'static str,
    branches: Vec<BranchRecord>,
    summary: BranchListSummary,
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

/// Describes the single base-ref refresh `fetch` performs, attempted or planned.
fn fetch_effect(base: &git::NewBranchBase, attempted: bool) -> Effect {
    Effect {
        action: "fetch_base_ref".into(),
        attempted,
        completed: attempted,
        details: base
            .base_ref
            .as_ref()
            .map(|base_ref| json!({"ref": base_ref.reference()})),
    }
}

/// Distinguishes the two JSON entry points that share worktree resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    /// Enter an existing worktree; refuse to create a genuinely new branch.
    Switch,
    /// Refuse an existing worktree; create a genuinely new branch unattended.
    Create,
}

impl Intent {
    const fn id(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Create => "create",
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
    if let Err(error) = git::validate_branch(&repo.current().path, &branch) {
        return emit_err(
            command,
            id,
            &format!("{command}.invalid_branch"),
            format!("{error:#}"),
        );
    }
    if let Some(worktree) = repo
        .worktrees
        .iter()
        .find(|w| matches!(&w.kind, WorktreeKind::Branch(value) if value == &branch))
    {
        if input.remote.is_some() {
            return emit_err(
                command,
                id,
                &format!("{command}.irrelevant_remote"),
                "remote is only valid when resolving a remote-tracking branch",
            );
        }
        if let Err(error) = git::reject_fetch(input.fetch, git::FETCH_REGISTERED_WORKTREE) {
            return emit_err(
                command,
                id,
                &format!("{command}.fetch_not_applicable"),
                format!("{error:#}"),
            );
        }
        if !worktree.navigable() {
            return emit_err(
                command,
                id,
                &format!("{command}.destination_unavailable"),
                format!("registered destination is {}", worktree.state_label()),
            );
        }
        if intent == Intent::Create {
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
    let Some(primary) = repo.primary.as_ref() else {
        return emit_err(
            command,
            id,
            "repository.primary_unavailable",
            "a bare repository cannot create a worktree",
        );
    };
    let config = match EffectiveConfig::load(&repo) {
        Ok(value) => value,
        Err(error) => {
            return emit_err(
                command,
                id,
                &format!("{command}.config_invalid"),
                format!("{error:#}"),
            );
        }
    };
    let root = match config.require_root() {
        Ok(value) => value,
        Err(error) => {
            return emit_err(
                command,
                id,
                "repository.root_unavailable",
                format!("{error:#}"),
            );
        }
    };
    let destination = match git::canonical_or_normalized(&root.join(&branch)) {
        Ok(value) => value,
        Err(error) => {
            return emit_err(
                command,
                id,
                &format!("{command}.destination_invalid"),
                format!("{error:#}"),
            );
        }
    };
    if destination.exists() || repo.worktrees.iter().any(|w| w.path == destination) {
        return emit_err(
            command,
            id,
            &format!("{command}.destination_collision"),
            "the configured destination already exists or is registered",
        );
    }
    let local = git::local_branch_exists(&repo.current().path, &branch)?;
    let remotes = if local {
        vec![]
    } else {
        git::remote_matches(&repo.current().path, &branch)?
    };
    if let Err(error) = git::reject_fetch(
        input.fetch && (local || !remotes.is_empty()),
        if local {
            git::FETCH_LOCAL_BRANCH
        } else {
            git::FETCH_REMOTE_BRANCH
        },
    ) {
        return emit_err(
            command,
            id,
            &format!("{command}.fetch_not_applicable"),
            format!("{error:#}"),
        );
    }
    let mut new_base: Option<git::NewBranchBase> = None;
    let selected_remote = if local {
        if input.remote.is_some() {
            return emit_err(
                command,
                id,
                &format!("{command}.irrelevant_remote"),
                "remote does not apply to an existing local branch",
            );
        }
        None
    } else if remotes.is_empty() {
        if input.remote.is_some() {
            return emit_err(
                command,
                id,
                &format!("{command}.unknown_remote"),
                "remote does not match a fetched remote-tracking branch",
            );
        }
        if let Err(error) = git::reject_fetch(
            input.fetch && config.base == BaseMode::Head,
            git::FETCH_HEAD_BASE,
        ) {
            return emit_err(
                command,
                id,
                &format!("{command}.fetch_not_applicable"),
                format!("{error:#}"),
            );
        }
        if intent == Intent::Switch && !input.dry_run {
            return emit_err(
                "switch",
                id,
                "switch.approval_required",
                "creating a genuinely new branch requires a manual human invocation",
            );
        }
        // A dry run resolves the same base the real run would, but never fetches.
        let base = match git::plan_new_branch_base(
            &repo.current().path,
            config.base,
            config.target_branch.as_deref(),
            input.fetch && !input.dry_run,
        ) {
            Ok(value) => value,
            Err(error) => {
                return emit_err(
                    command,
                    id,
                    &format!("{command}.base_unavailable"),
                    format!("{error:#}"),
                );
            }
        };
        if intent == Intent::Switch {
            let mut effects = Vec::new();
            if input.fetch {
                effects.push(fetch_effect(&base, false));
            }
            effects.push(Effect {
                action: "create_branch".into(),
                attempted: false,
                completed: false,
                details: None,
            });
            effects.push(Effect {
                action: "create_worktree".into(),
                attempted: false,
                completed: false,
                details: None,
            });
            let mut result = json!({"outcome":"creation_plan","kind":"new","branch":branch,"destination":BytePath::path(&destination),"start_point":base.commit,"approval_required":true});
            if let Some(base_ref) = &base.base_ref {
                result["base_ref"] = json!(base_ref.reference());
            }
            return emit(
                protocol::success("switch", id, result, json!({}), effects),
                false,
            );
        }
        new_base = Some(base);
        None
    } else {
        match input.remote {
            Some(remote)
                if remotes
                    .iter()
                    .any(|value| value == &format!("{remote}/{branch}") || value == &remote) =>
            {
                remotes
                    .iter()
                    .find(|value| **value == remote || **value == format!("{remote}/{branch}"))
                    .cloned()
            }
            Some(_) => {
                return emit_err(
                    command,
                    id,
                    &format!("{command}.unknown_remote"),
                    "explicit remote does not match an available fetched branch",
                );
            }
            None if remotes.len() == 1 => Some(remotes[0].clone()),
            None => {
                let mut response = protocol::failure(
                    command,
                    id,
                    &format!("{command}.remote_selection_required"),
                    "multiple fetched remotes match this branch",
                );
                response.context = json!({"branch":branch,"remotes":remotes});
                return emit(response, true);
            }
        }
    };
    if !config.post_create.is_empty()
        && !trust::is_trusted(&repo, HookPhase::PostCreate, &config.post_create)?
    {
        return emit_err(
            command,
            id,
            "trust.approval_required",
            "post-create hooks require manual review and approval before mutation",
        );
    }
    let mut effects = Vec::new();
    if let Some(base) = &new_base {
        if input.fetch {
            effects.push(fetch_effect(base, !input.dry_run));
        }
        let mut details = json!({"branch":branch,"start_point":base.commit});
        if let Some(base_ref) = &base.base_ref {
            details["base_ref"] = json!(base_ref.reference());
        }
        effects.push(Effect {
            action: "create_branch".into(),
            attempted: !input.dry_run,
            completed: !input.dry_run,
            details: Some(details),
        });
    }
    effects.push(Effect {
        action: "create_worktree".into(),
        attempted: !input.dry_run,
        completed: !input.dry_run,
        details: Some(json!({"destination":BytePath::path(&destination)})),
    });
    let description_effect = input.description.as_ref().map(|description| {
        let index = effects.len();
        effects.push(Effect {
            action: "set_branch_description".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"branch":branch,"description":description})),
        });
        index
    });
    let mut diagnostics = Vec::new();
    if !input.dry_run {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let pending = if config.post_create.is_empty() {
            None
        } else {
            Some(crate::setup::prepare(
                &repo.common_dir,
                &branch,
                &destination,
            )?)
        };
        let creation = if let Some(base) = new_base.as_ref() {
            git::add_new_worktree(primary, &destination, &branch, &base.commit)
        } else if local {
            git::add_existing_worktree(primary, &destination, &branch)
        } else {
            git::add_tracking_worktree(
                primary,
                &destination,
                &branch,
                selected_remote
                    .as_deref()
                    .context("remote resolution failed")?,
            )
        };
        if let Err(error) = creation {
            if let Some(pending) = pending {
                pending.cancel()?;
            }
            return emit_err(
                command,
                id,
                &format!("{command}.creation_failed"),
                format!("{error:#}"),
            );
        }
        let identity = git::worktree_identity(&destination)?;
        if let Some(pending) = pending {
            pending.commit(&repo.common_dir, &identity)?;
        }
        if let (Some(description), Some(effect_index)) =
            (input.description.as_deref(), description_effect)
        {
            effects[effect_index].attempted = true;
            if let Err(error) =
                git::set_branch_description(&repo.current().path, &branch, description)
            {
                let mut response = protocol::failure(
                    command,
                    id,
                    "create.description_failed",
                    format!(
                        "the worktree was created, but its branch description could not be set: {error:#}"
                    ),
                );
                response.context =
                    json!({"branch":branch,"destination":BytePath::path(&destination)});
                response.effects = effects;
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
                if !config.post_create.is_empty() {
                    response.next_steps.push(protocol::NextStep {
                        action: format!("{command}.recover_setup"),
                        description: "After setting the description, retry or explicitly complete setup interactively".into(),
                        mutation: "setup".into(),
                        requires_human_approval: true,
                        invocation: json!({"argv":["pando","switch",branch],"stdin":null,"working_directory":BytePath::path(&repo.current().path)}),
                    });
                }
                return emit(response, true);
            }
            effects[effect_index].completed = true;
        }
        if !config.post_create.is_empty() {
            let (outcome, output) =
                crate::setup::run_steps_captured(&config.post_create, &destination)?;
            diagnostics = output;
            if outcome != crate::setup::HookOutcome::Success {
                let mut response = protocol::failure(
                    command,
                    id,
                    &format!("{command}.setup_failed"),
                    format!("post-create hook outcome: {outcome:?}; setup remains incomplete"),
                );
                response.context = json!({"branch":branch,"destination":BytePath::path(&destination),"setup":"incomplete"});
                response.effects = effects;
                for (stdout, stderr) in diagnostics {
                    push_diagnostic(&mut response, "hook", "stdout", &stdout);
                    push_diagnostic(&mut response, "hook", "stderr", &stderr);
                }
                response.next_steps.push(protocol::NextStep { action:format!("{command}.recover_setup"), description:"Inspect the worktree and retry or explicitly complete setup interactively".into(), mutation:"setup".into(), requires_human_approval:true, invocation:json!({"argv":["pando","switch",branch],"stdin":null,"working_directory":BytePath::path(&repo.current().path)}) });
                return emit(response, true);
            }
            crate::setup::clear(&repo.common_dir, &identity, Some(&branch))?;
        }
    }
    let mut result = json!({"outcome":if input.dry_run{"creation_plan"}else{"created"},"branch":branch,"destination":BytePath::path(&destination),"remote":selected_remote});
    if let Some(base) = new_base {
        result["kind"] = json!("new");
        result["start_point"] = json!(base.commit);
        if let Some(base_ref) = &base.base_ref {
            result["base_ref"] = json!(base_ref.reference());
        }
    }
    let mut response = protocol::success(command, id, result, json!({}), effects);
    for (stdout, stderr) in diagnostics {
        push_diagnostic(&mut response, "hook", "stdout", &stdout);
        push_diagnostic(&mut response, "hook", "stderr", &stderr);
    }
    emit(response, false)
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct GetInput {
    property: GetProperty,
}
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum GetProperty {
    Branch,
    Port,
    WorktreePath,
    PrimaryWorktreePath,
    WorktreeRoot,
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

fn navigation_branches() -> Result<git::RepositoryBranches> {
    git::repository_with_branches(&env::current_dir().context("failed to read current directory")?)
}

fn condition(value: Condition) -> &'static str {
    match value {
        Condition::Clean => "clean",
        Condition::Dirty => "dirty",
        Condition::Unknown => "unknown",
        Condition::Missing => "missing",
        Condition::Inaccessible => "inaccessible",
    }
}
fn worktree_record(worktree: &Worktree) -> WorktreeRecord {
    let (kind, branch) = match &worktree.kind {
        WorktreeKind::Branch(branch) => ("branch", Some(branch.clone())),
        WorktreeKind::Detached => ("detached", None),
        WorktreeKind::Bare => ("bare", None),
        WorktreeKind::Unknown => ("unknown", None),
    };
    WorktreeRecord {
        kind: kind.to_owned(),
        branch,
        path: BytePath::path(&worktree.path),
        head: worktree.head.clone(),
        last_commit_at: worktree.machine_last_commit_at(),
        condition: condition(worktree.condition).to_owned(),
        current: worktree.current,
        navigable: worktree.navigable(),
        lock_reason: worktree.locked.clone(),
        prune_reason: worktree.prunable.clone(),
    }
}

/// Emits the structured worktree list.
///
/// # Errors
/// Returns an error when stdout cannot be written.
pub fn list(request_mode: bool) -> Result<()> {
    let id = if request_mode {
        match protocol::read_optional_request::<EmptyInput>() {
            Ok(r) => {
                if r.schema_version != 1 {
                    return emit_err(
                        "list",
                        r.request_id,
                        "json.unsupported_schema_version",
                        "unsupported schema version",
                    );
                }
                r.request_id
            }
            Err(e) => return emit_err("list", None, "json.invalid_request", e),
        }
    } else {
        None
    };
    let repo = match navigation_repository() {
        Ok(v) => v,
        Err(e) => return emit_err("list", id, "repository.invalid", format!("{e:#}")),
    };
    let records = repo.worktrees.iter().map(worktree_record).collect();
    let count = |condition| {
        repo.worktrees
            .iter()
            .filter(|worktree| worktree.condition == condition)
            .count()
    };
    let result = ListResult {
        outcome: "listed",
        worktrees: records,
        summary: ListSummary {
            total: repo.worktrees.len(),
            dirty: count(Condition::Dirty),
            unknown: count(Condition::Unknown),
            missing: count(Condition::Missing),
            inaccessible: count(Condition::Inaccessible),
            bare: repo
                .worktrees
                .iter()
                .filter(|worktree| worktree.is_bare())
                .count(),
            locked: repo
                .worktrees
                .iter()
                .filter(|worktree| worktree.locked.is_some())
                .count(),
            prunable: repo
                .worktrees
                .iter()
                .filter(|worktree| worktree.prunable.is_some())
                .count(),
        },
    };
    let mut response =
        protocol::success("list", id, serde_json::to_value(result)?, json!({}), vec![]);
    if let Some(warning) = &repo.metadata_warning {
        push_diagnostic(
            &mut response,
            "git.commit_metadata",
            "metadata",
            warning.as_bytes(),
        );
    }
    emit(response, false)
}

/// Emits the structured branch list, in `for-each-ref` order.
///
/// # Errors
/// Returns an error when stdout cannot be written.
pub fn list_branches(request_mode: bool) -> Result<()> {
    let id = if request_mode {
        match protocol::read_optional_request::<EmptyInput>() {
            Ok(r) => {
                if r.schema_version != 1 {
                    return emit_err(
                        "list",
                        r.request_id,
                        "json.unsupported_schema_version",
                        "unsupported schema version",
                    );
                }
                r.request_id
            }
            Err(e) => return emit_err("list", None, "json.invalid_request", e),
        }
    } else {
        None
    };
    let branches = match navigation_branches() {
        Ok(v) => v,
        Err(e) => return emit_err("list", id, "repository.invalid", format!("{e:#}")),
    };
    let repo = &branches.repository;
    let records: Vec<_> = branches
        .branches
        .iter()
        .map(|record| branch_record(record, &repo.worktrees))
        .collect();
    let checked_out = records.iter().filter(|r| r.path.is_some()).count();
    let dirty = records
        .iter()
        .filter(|r| r.condition.as_deref() == Some(condition(Condition::Dirty)))
        .count();
    let result = BranchListResult {
        outcome: "listed",
        summary: BranchListSummary {
            total: records.len(),
            checked_out,
            dirty,
        },
        branches: records,
    };
    let mut response =
        protocol::success("list", id, serde_json::to_value(result)?, json!({}), vec![]);
    if let Some(warning) = &repo.metadata_warning {
        push_diagnostic(
            &mut response,
            "git.commit_metadata",
            "metadata",
            warning.as_bytes(),
        );
    }
    emit(response, false)
}

fn branch_record(record: &git::BranchRecord, worktrees: &[Worktree]) -> BranchRecord {
    let row = Row::from_branch(record, worktrees);
    BranchRecord {
        branch: record.branch.clone(),
        head: record.head.clone(),
        last_commit_at: row.machine_last_commit_at(),
        path: row.path.as_deref().map(BytePath::path),
        condition: row.condition.map(|value| condition(value).to_owned()),
        current: row.current,
    }
}

/// Emits one naturally typed current-worktree property.
///
/// # Errors
/// Returns an error when stdout cannot be written or a path cannot be resolved.
#[allow(clippy::too_many_lines)]
pub fn get(request_mode: bool, argv: Option<&str>) -> Result<()> {
    let (id, p) = if request_mode {
        if argv.is_some() {
            return emit_err(
                "get",
                None,
                "json.invalid_request",
                "command arguments are forbidden with --input-output json",
            );
        }
        match protocol::read_request::<GetInput>() {
            Ok(r) if r.schema_version == 1 => (r.request_id, r.input.property),
            Ok(r) => {
                return emit_err(
                    "get",
                    r.request_id,
                    "json.unsupported_schema_version",
                    "unsupported schema version",
                );
            }
            Err(e) => return emit_err("get", None, "json.invalid_request", e),
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
    let repo = match repository() {
        Ok(v) => v,
        Err(e) => return emit_err("get", id, "repository.invalid", format!("{e:#}")),
    };
    let (name, value) = match p {
        GetProperty::Branch => match &repo.current().kind {
            WorktreeKind::Branch(v) => ("branch", json!(v)),
            _ => {
                return emit_err(
                    "get",
                    id,
                    "repository.detached",
                    "current worktree has no branch",
                );
            }
        },
        GetProperty::Port => match &repo.current().kind {
            WorktreeKind::Branch(v) => ("port", json!(smart::port_for_branch(v))),
            _ => {
                return emit_err(
                    "get",
                    id,
                    "repository.detached",
                    "current worktree has no branch",
                );
            }
        },
        GetProperty::WorktreePath => (
            "worktree_path",
            serde_json::to_value(BytePath::path(&repo.current().path))?,
        ),
        GetProperty::PrimaryWorktreePath => match repo.primary.as_ref() {
            Some(v) => (
                "primary_worktree_path",
                serde_json::to_value(BytePath::path(v))?,
            ),
            None => {
                return emit_err(
                    "get",
                    id,
                    "repository.primary_unavailable",
                    "primary worktree unavailable",
                );
            }
        },
        GetProperty::WorktreeRoot => {
            let c = EffectiveConfig::load(&repo)?;
            match c.root {
                Some(v) => ("worktree_root", serde_json::to_value(BytePath::path(&v))?),
                None => {
                    return emit_err(
                        "get",
                        id,
                        "repository.root_unavailable",
                        "worktree root unavailable",
                    );
                }
            }
        }
    };
    emit(
        protocol::success(
            "get",
            id,
            json!({"outcome":"value","property":name,"value":value}),
            json!({}),
            vec![],
        ),
        false,
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
        if matches!(command, "trust.status" | "trust.commit_status") {
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
    let result = match command {
        "trust.status" => {
            let c = EffectiveConfig::load(&repo)?;
            let phases:Vec<_>=HookPhase::all().iter().map(|p|{let s=c.hooks(*p);json!({"phase":p.key(),"configured":!s.is_empty(),"trusted":trust::is_trusted(&repo,*p,s).unwrap_or(false),"step_count":s.len(),"source":{"kind":"effective","repository":BytePath::path(&repo.current().path)},"identity":if s.is_empty(){None}else{Some(trust::command_hash(*p,s))}})}).collect();
            json!({"outcome":"status","phases":phases})
        }
        "trust.reset" => {
            let changed = if dry { false } else { trust::reset(&repo)? };
            json!({"outcome":if changed{"reset"}else if dry{"dry_run"}else{"already_reset"}})
        }
        "trust.commit_status" => {
            let c = EffectiveConfig::load(&repo)?;
            let hash = trust::generation_hash(&c.generation);
            let state = if c.generation.command.is_none() {
                "absent"
            } else if hash.is_none() {
                "user_controlled"
            } else if trust::is_generation_trusted(&repo, &c.generation)? {
                "trusted_shared"
            } else {
                "untrusted_shared"
            };
            let source = c
                .generation
                .command
                .as_ref()
                .map(|v| format!("{:?}", v.source).to_lowercase());
            json!({"outcome":"status","state":state,"identity":hash,"source":source})
        }
        "trust.commit_reset" => {
            let changed = if dry {
                false
            } else {
                trust::reset_generation(&repo)?
            };
            json!({"outcome":if changed{"reset"}else if dry{"dry_run"}else{"already_reset"}})
        }
        "trust.commit_approve" => {
            let c = EffectiveConfig::load(&repo)?;
            let details = json!({"command":c.generation.command.as_ref().map(|v|&v.value),"template":c.generation.template.as_ref().map(|v|&v.value),"identity":trust::generation_hash(&c.generation)});
            if dry {
                json!({"outcome":"dry_run","candidate":details})
            } else {
                let mut response = protocol::failure(
                    command,
                    id,
                    "trust.approval_required",
                    "approval requires a manual human invocation",
                );
                response.context = json!({"candidate":details});
                response.next_steps.push(crate::protocol::NextStep {
                    action: "trust.approve_commit_generator".into(), description: "Review these settings and approve interactively".into(), mutation: "trust".into(), requires_human_approval: true,
                    invocation: json!({"argv":["pando","trust","commit-approve"],"stdin":null,"working_directory":BytePath::path(&repo.current().path)}),
                });
                return emit(response, true);
            }
        }
        _ => unreachable!(),
    };
    let effects = if matches!(command, "trust.reset" | "trust.commit_reset") {
        vec![Effect {
            action: command.into(),
            attempted: !dry,
            completed: !dry,
            details: None,
        }]
    } else {
        vec![]
    };
    emit(
        protocol::success(command, id, result, json!({}), effects),
        false,
    )
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoveInput {
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    dry_run: bool,
}

/// Plans or executes structured worktree removal.
///
/// # Errors
/// Returns an error when repository state cannot be inspected, mutation fails, or stdout cannot be written.
#[allow(clippy::too_many_lines)]
pub fn remove(request_mode: bool, branches: Vec<String>, force: bool, dry_run: bool) -> Result<()> {
    let (id, input, force) = if request_mode {
        if !branches.is_empty() || dry_run {
            return emit_err(
                "remove",
                None,
                "json.invalid_request",
                "command options are forbidden with --input-output json",
            );
        }
        match protocol::read_request::<RemoveInput>() {
            Ok(r) if r.schema_version == 1 => (r.request_id, r.input, force),
            Ok(r) => {
                return emit_err(
                    "remove",
                    r.request_id,
                    "json.unsupported_schema_version",
                    "unsupported schema version",
                );
            }
            Err(e) => return emit_err("remove", None, "json.invalid_request", e),
        }
    } else {
        (None, RemoveInput { branches, dry_run }, force)
    };

    let plan = match crate::lifecycle::plan_remove(&input.branches, force) {
        Ok(plan) => plan,
        Err(error) => {
            let message = error.to_string();
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
            return emit_err("remove", id, code, message);
        }
    };
    for target in &plan.targets {
        let trusted = if input.dry_run || target.config.pre_remove.is_empty() {
            true
        } else {
            match trust::is_trusted(
                &plan.repository,
                HookPhase::PreRemove,
                &target.config.pre_remove,
            ) {
                Ok(value) => value,
                Err(error) => {
                    return emit_err("remove", id, "trust.read_failed", format!("{error:#}"));
                }
            }
        };
        if !trusted {
            let mut response = protocol::failure(
                "remove",
                id,
                "trust.approval_required",
                "pre-remove hooks require manual review and approval",
            );
            response.context = json!({"branch":target.worktree.branch_label(),"commands":target.config.pre_remove.iter().map(|s| &s.command).collect::<Vec<_>>()});
            response.next_steps.push(crate::protocol::NextStep { action:"trust.approve_hooks".into(), description:"Review and approve pre-remove hooks interactively".into(), mutation:"trust".into(), requires_human_approval:true, invocation:json!({"argv":["pando","remove",target.worktree.branch_label()],"stdin":null,"working_directory":BytePath::path(&plan.current)}) });
            return emit(response, true);
        }
    }
    let mut effects = Vec::new();
    for target in &plan.targets {
        let details = Some(
            json!({"branch":target.worktree.branch_label(),"path":BytePath::path(&target.worktree.path),"branch_retained":true}),
        );
        effects.push(Effect {
            action: "pre_remove_hooks".into(),
            attempted: false,
            completed: target.config.pre_remove.is_empty(),
            details: details.clone(),
        });
        effects.push(Effect {
            action: "remove_worktree".into(),
            attempted: false,
            completed: false,
            details,
        });
    }
    if input.dry_run {
        return emit(
            protocol::success(
                "remove",
                id,
                json!({"outcome":"dry_run","targets":plan.targets.iter().map(|t|json!({"branch":t.worktree.branch_label(),"path":BytePath::path(&t.worktree.path),"branch_retained":true})).collect::<Vec<_>>(),"force":force}),
                json!({}),
                effects,
            ),
            false,
        );
    }
    let mut response = protocol::success("remove", id, json!({}), json!({}), effects);
    for (index, target) in plan.targets.iter().enumerate() {
        if !target.config.pre_remove.is_empty() {
            response.effects[index * 2].attempted = true;
            let (outcome, output) = match crate::setup::run_steps_captured(
                &target.config.pre_remove,
                &target.worktree.path,
            ) {
                Ok(value) => value,
                Err(error) => {
                    response.error = Some(crate::protocol::ErrorBody {
                        code: "remove.hook_start_failed".into(),
                        message: format!("{error:#}"),
                    });
                    response.status = "error";
                    response.result = None;
                    add_remove_retry(&mut response, &input, force, &plan.current);
                    return emit(response, true);
                }
            };
            for (stdout, stderr) in output {
                push_diagnostic(&mut response, "hook", "stdout", &stdout);
                push_diagnostic(&mut response, "hook", "stderr", &stderr);
            }
            if outcome != crate::setup::HookOutcome::Success {
                response.error = Some(crate::protocol::ErrorBody {
                    code: "remove.hook_failed".into(),
                    message: format!("pre-remove hook outcome: {outcome:?}"),
                });
                response.status = "error";
                response.result = None;
                add_remove_retry(&mut response, &input, force, &plan.current);
                return emit(response, true);
            }
            response.effects[index * 2].completed = true;
        }
        if let Some(path) = &target.stale_journal {
            if let Err(error) = fs::remove_file(path) {
                response.error = Some(crate::protocol::ErrorBody {
                    code: "remove.journal_cleanup_failed".into(),
                    message: error.to_string(),
                });
                response.status = "error";
                response.result = None;
                add_remove_retry(&mut response, &input, force, &plan.current);
                return emit(response, true);
            }
        }
        response.effects[index * 2 + 1].attempted = true;
        let output =
            match git::remove_worktree_captured(&plan.primary, &target.worktree.path, force) {
                Ok(value) => value,
                Err(error) => {
                    response.error = Some(crate::protocol::ErrorBody {
                        code: "remove.git_start_failed".into(),
                        message: format!("{error:#}"),
                    });
                    response.status = "error";
                    response.result = None;
                    add_remove_retry(&mut response, &input, force, &plan.current);
                    return emit(response, true);
                }
            };
        push_diagnostic(&mut response, "git", "stdout", &output.stdout);
        push_diagnostic(&mut response, "git", "stderr", &output.stderr);
        if !output.status.success() {
            response.error = Some(crate::protocol::ErrorBody {
                code: "remove.git_failed".into(),
                message: format!("git worktree remove failed with {}", output.status),
            });
            response.status = "error";
            response.result = None;
            add_remove_retry(&mut response, &input, force, &plan.current);
            return emit(response, true);
        }
        response.effects[index * 2 + 1].completed = true;
    }
    let removed_current = plan.targets.iter().any(|t| t.worktree.path == plan.current);
    response.result = Some(
        json!({"outcome":"removed","targets":plan.targets.iter().map(|t|json!({"branch":t.worktree.branch_label(),"path":BytePath::path(&t.worktree.path),"branch_retained":true})).collect::<Vec<_>>(),"destination":if removed_current{Some(BytePath::path(&plan.primary))}else{None}}),
    );
    emit(response, false)
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
fn add_remove_retry(
    response: &mut crate::protocol::Response,
    input: &RemoveInput,
    force: bool,
    cwd: &std::path::Path,
) {
    let mut argv = vec![
        "pando".to_string(),
        "remove".to_string(),
        "--input-output".to_string(),
        "json".to_string(),
    ];
    if force {
        argv.push("--force".into());
    }
    response.next_steps.push(crate::protocol::NextStep{action:"remove.retry".into(),description:"Retry pending removal targets after resolving the failure".into(),mutation:"worktree".into(),requires_human_approval:force,invocation:json!({"argv":argv,"stdin":{"schema_version":1,"input":input},"working_directory":BytePath::path(cwd)})});
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct MergeInput {
    #[serde(default)]
    no_rebase: bool,
    #[serde(default)]
    no_remove: bool,
    #[serde(default)]
    dry_run: bool,
}

/// Plans or executes the structured merge lifecycle.
///
/// # Errors
/// Returns an error when repository state cannot be inspected, mutation fails, or stdout cannot be written.
#[allow(clippy::too_many_lines, clippy::fn_params_excessive_bools)]
pub fn merge(request_mode: bool, no_rebase: bool, no_remove: bool, dry_run: bool) -> Result<()> {
    let (id, input) = if request_mode {
        if no_rebase || no_remove || dry_run {
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
                dry_run,
            },
        )
    };
    let plan = match crate::lifecycle::plan_merge(input.no_rebase, input.no_remove) {
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
                _ => "merge.blocked",
            };
            return emit_err("merge", id, code, message);
        }
    };
    // An in-place merge owns no topic worktree, so removal and the `cd` destination never apply.
    let removes = !input.no_remove && !plan.context.in_place;
    let context = serde_json::to_value(&plan.context)?;
    let mut effects = vec![
        Effect {
            action: "journal".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"applicable":!plan.context.journaled})),
        },
        Effect {
            action: "rebase".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"applicable":plan.needs_rebase || plan.context.rebase_active})),
        },
        Effect {
            action: "pre_merge_hooks".into(),
            attempted: false,
            completed: false,
            details: Some(
                json!({"configured":!plan.config.pre_merge.is_empty(),"trusted":plan.context.pre_merge_hooks_trusted}),
            ),
        },
        Effect {
            action: "fast_forward_merge".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"applicable":!plan.context.cleanup_pending})),
        },
        Effect {
            action: "pre_remove_hooks".into(),
            attempted: false,
            completed: false,
            details: Some(
                json!({"applicable":removes,"trusted":plan.context.pre_remove_hooks_trusted}),
            ),
        },
        Effect {
            action: "remove_worktree".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"applicable":removes})),
        },
        Effect {
            action: "destination".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"applicable":removes,"path":plan.context.primary_worktree})),
        },
    ];
    let approval_blocked = if plan.context.cleanup_pending {
        !plan.context.pre_remove_hooks_trusted
    } else {
        !plan.context.pre_merge_hooks_trusted
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
        let mut response = protocol::failure(
            "merge",
            id,
            "merge.hook_approval_required",
            "configured lifecycle hooks are not trusted",
        );
        response.context = context;
        response.effects = effects;
        response.next_steps.push(protocol::NextStep { action:"trust.review".into(), description:"Review and explicitly trust the configured lifecycle hooks before retrying".into(), mutation:"trust".into(), requires_human_approval:true, invocation:json!({"argv":["pando","trust","show"],"working_directory":plan.context.topic_worktree}) });
        return emit(response, true);
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
    let output = command.output()?;
    // The human lifecycle remains the single crash-recovery engine; its streams are captured here.
    let after = crate::lifecycle::plan_merge(input.no_rebase, input.no_remove).ok();
    let succeeded = output.status.success();
    let after_cleanup = after.as_ref().is_some_and(|p| p.context.cleanup_pending);
    let after_journaled = after.as_ref().is_some_and(|p| p.context.journaled);
    let after_rebase = after.as_ref().is_some_and(|p| p.context.rebase_active);
    // Record only phases proven by the journal/repository state. A subprocess failure
    // must never make later lifecycle phases look attempted merely because it exited.
    effects[0].attempted = !plan.context.journaled;
    effects[0].completed = plan.context.journaled || after_journaled || succeeded;
    let rebase_applicable = plan.needs_rebase || plan.context.rebase_active;
    effects[1].attempted = rebase_applicable;
    effects[1].completed = rebase_applicable && !after_rebase && (after_cleanup || succeeded);
    effects[2].attempted = !plan.context.cleanup_pending && !after_rebase;
    effects[2].completed = after_cleanup || succeeded;
    effects[3].attempted = !plan.context.cleanup_pending && !after_rebase;
    effects[3].completed = after_cleanup || succeeded;
    let cleanup_attempted = removes && (plan.context.cleanup_pending || after_cleanup || succeeded);
    effects[4].attempted = cleanup_attempted;
    effects[4].completed = removes && succeeded;
    effects[5].attempted = cleanup_attempted;
    effects[5].completed = removes && succeeded;
    effects[6].attempted = removes && succeeded;
    effects[6].completed = removes && succeeded;
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
pub fn install(request_mode: bool, dry_flag: bool) -> Result<()> {
    let (id, dry) = if request_mode {
        if dry_flag {
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
    let plan = install::json_plan()?;
    let effects = ["configuration", "integration", "startup"]
        .into_iter()
        .filter(|key| plan[*key]["would_change"] == true)
        .map(|key| Effect {
            action: "file.write".into(),
            attempted: false,
            completed: false,
            details: Some(json!({"target":plan[key]["path"],"role":key})),
        })
        .collect::<Vec<_>>();
    if plan["changed"] == false {
        return emit(
            protocol::success(
                "install",
                id,
                json!({"outcome":"already_current","plan":plan}),
                json!({}),
                vec![],
            ),
            false,
        );
    }
    if !dry {
        let mut response = protocol::failure(
            "install",
            id,
            "install.approval_required",
            "installation requires a manual human invocation",
        );
        response.effects = effects;
        response.next_steps.push(crate::protocol::NextStep {
            action: "install.approve".into(),
            description: "Review and approve installation interactively".into(),
            mutation: "filesystem".into(),
            requires_human_approval: true,
            invocation: json!({"argv":["pando","install"],"stdin":null}),
        });
        return emit(response, true);
    }
    emit(
        protocol::success(
            "install",
            id,
            json!({"outcome":"dry_run","plan":plan}),
            json!({}),
            effects,
        ),
        false,
    )
}
/// Builds exact-leaf JSON help from the runtime request and response types.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn help(command: &str) -> Value {
    let request_schema = match command {
        "list" | "trust.status" | "trust.commit_status" => json!(schemars::schema_for!(
            protocol::OptionalInputRequest<EmptyInput>
        )),
        "switch" => json!(schemars::schema_for!(protocol::Request<SwitchInput>)),
        "create" => json!(schemars::schema_for!(protocol::Request<CreateInput>)),
        "get" => json!(schemars::schema_for!(protocol::Request<GetInput>)),
        "remove" => json!(schemars::schema_for!(protocol::Request<RemoveInput>)),
        "merge" => json!(schemars::schema_for!(protocol::Request<MergeInput>)),
        "trust.reset" | "trust.commit_reset" | "trust.commit_approve" | "install" => {
            json!(schemars::schema_for!(protocol::Request<DryRunInput>))
        }
        _ => Value::Null,
    };
    let (errors, actions): (&[&str], &[&str]) = match command {
        "list" | "trust.status" | "trust.commit_status" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
            ],
            &[],
        ),
        "get" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "get.invalid_property",
                "repository.invalid",
                "repository.detached",
                "repository.primary_unavailable",
                "repository.root_unavailable",
            ],
            &[],
        ),
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
                "pre_merge_hooks",
                "fast_forward_merge",
                "pre_remove_hooks",
                "remove_worktree",
                "destination",
                "trust.review",
                "merge.retry",
            ],
        ),
        "trust.reset" | "trust.commit_reset" => (
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
        "install" => (
            &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "install.approval_required",
            ],
            &["file.write", "install.approve"],
        ),
        _ => (&[], &[]),
    };
    let result_schema = match command {
        "list" => json!(schemars::schema_for!(ListResult)),
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
