//! Shared destination and source planning for worktree navigation and creation.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::protocol::{BytePath, Effect};

use crate::{
    Worktree,
    branch::{self, Classification},
    config::EffectiveConfig,
    git::{self, Repository},
    hook_approval,
    setup::{self, HookOutcome, HookOutput, OutputPolicy},
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

/// Plans the selected source and byte-preserving destination.
///
/// The result is deterministic. Callers may satisfy a remote-choice blocker and
/// invoke this function again, but must not duplicate branch classification.
///
/// # Errors
///
/// Returns an error when Git cannot classify the branch or configuration cannot be loaded.
#[allow(clippy::too_many_lines)] // One authoritative planner owns every classification and safety check.
pub(crate) fn plan(
    repository: &Repository,
    intent: Intent,
    branch: &str,
    remote: Option<&str>,
    fetch: FetchIntent,
    description: Option<String>,
    dry_run: bool,
) -> Result<Result<Plan, Blocker>> {
    if let Err(error) = git::validate_branch(&repository.current().path, branch) {
        return Ok(Err(Blocker::InvalidBranch {
            message: format!("{error:#}"),
        }));
    }
    let classification = branch::classify(repository, branch)?;
    if let Classification::Registered(worktree) = classification {
        if let Err(error) = git::reject_fetch(fetch.requested(), git::FETCH_REGISTERED_WORKTREE) {
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
    let destination = match git::canonical_or_normalized(&root.join(branch)) {
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
    if destination.starts_with(primary) && !git::would_be_ignored(primary, &destination)? {
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
        repository,
        classification,
        branch,
        remote,
        &config,
        fetch,
        &destination,
    ) {
        Ok(source) => source,
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

fn plan_source(
    repository: &Repository,
    classification: Classification,
    branch: &str,
    remote: Option<&str>,
    config: &EffectiveConfig,
    fetch: FetchIntent,
    destination: &std::path::Path,
) -> Result<Source, Box<Blocker>> {
    match classification {
        Classification::Registered(_) => unreachable!("registered classification returned above"),
        Classification::Local => {
            reject_fetch(fetch, git::FETCH_LOCAL_BRANCH)?;
            if remote.is_some() {
                return Err(Box::new(Blocker::IrrelevantRemote));
            }
            Ok(Source::Local {
                commit: git::branch_commit(&repository.current().path, branch)
                    .expect("classified local branch must remain resolvable while planning"),
            })
        }
        Classification::New => {
            if remote.is_some() {
                return Err(Box::new(Blocker::UnknownRemote));
            }
            if fetch.requested() && config.base == crate::BaseMode::Head {
                reject_fetch(fetch, git::FETCH_HEAD_BASE)?;
            }
            git::plan_new_branch_base(
                &repository.current().path,
                config.base,
                config.target_branch.as_deref(),
                fetch.refreshes(),
            )
            .map(|base| Source::New { base })
            .map_err(|error| {
                Box::new(Blocker::BaseUnavailable {
                    message: format!("{error:#}"),
                })
            })
        }
        Classification::Remotes(remotes) => {
            reject_fetch(fetch, git::FETCH_REMOTE_BRANCH)?;
            match remote {
                Some(remote) => remotes
                    .into_iter()
                    .find(|candidate| {
                        candidate == remote || candidate == &format!("{remote}/{branch}")
                    })
                    .map(|reference| Source::Remote {
                        commit: git::branch_commit(&repository.current().path, &reference)
                            .expect("classified remote ref must remain resolvable while planning"),
                        reference,
                    })
                    .ok_or_else(|| Box::new(Blocker::UnknownRemote)),
                None if remotes.len() == 1 => Ok(Source::Remote {
                    commit: git::branch_commit(&repository.current().path, &remotes[0])
                        .expect("classified remote ref must remain resolvable while planning"),
                    reference: remotes[0].clone(),
                }),
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
    let creation = match &plan.source {
        Source::Registered(_) => unreachable!(),
        Source::Local { .. } => {
            git::add_existing_worktree(&repository.current().path, &plan.destination, &plan.branch)
        }
        Source::Remote { reference, .. } => git::add_tracking_worktree(
            &repository.current().path,
            &plan.destination,
            &plan.branch,
            reference,
        ),
        Source::New { base } => git::add_new_worktree(
            &repository.current().path,
            &plan.destination,
            &plan.branch,
            &base.commit,
        ),
    };
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
        let identity = git::worktree_identity(&plan.destination)
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
        git::set_branch_description(&repository.current().path, &plan.branch, description)
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
        let execution = setup::execute(
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
    let actual = git::branch_commit(&repository.current().path, reference)?;
    if actual != *expected {
        bail!("planned source {reference:?} moved from {expected} to {actual}");
    }
    Ok(())
}

fn revalidate_new(repository: &Repository, base: &git::NewBranchBase) -> Result<()> {
    let actual = match &base.base_ref {
        Some(base_ref) => git::base_ref_commit(&repository.current().path, base_ref)?,
        None => git::head_commit(&repository.current().path)?,
    };
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
    git::reject_fetch(fetch.requested(), because).map_err(|error| {
        Box::new(Blocker::FetchNotApplicable {
            message: format!("{error:#}"),
        })
    })
}
