//! Shared destination and source planning for worktree navigation and creation.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::protocol::Effect;

use crate::{
    Worktree,
    branch::{self, Classification},
    config::EffectiveConfig,
    git::{self, Repository},
    hook_approval,
};

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
    pub(crate) dry_run: bool,
}

/// The result of executing a navigation or hook-free creation plan.
#[derive(Debug)]
pub(crate) struct ExecutionOutcome {
    pub(crate) destination: PathBuf,
    pub(crate) effects: Vec<Effect>,
}

/// A failed execution together with effects advanced only at real transitions.
#[derive(Debug)]
pub(crate) struct ExecutionFailure {
    pub(crate) code: &'static str,
    pub(crate) error: anyhow::Error,
    pub(crate) effects: Vec<Effect>,
    pub(crate) created: bool,
}

/// A caller decision or safety condition that prevents an executable plan.
#[derive(Debug)]
pub(crate) enum Blocker {
    RegisteredForCreate { worktree: Worktree },
    DestinationUnavailable { worktree: Worktree },
    PrimaryUnavailable,
    RootUnavailable { message: String },
    DestinationInvalid { message: String },
    DestinationCollision,
    DestinationNotIgnored { first: String, gitignore: PathBuf },
    IrrelevantRemote,
    UnknownRemote,
    RemoteSelectionRequired { remotes: Vec<String> },
    FetchNotApplicable { message: String },
    BaseUnavailable { message: String },
    ApprovalRequired { candidate: hook_approval::Candidate },
}

/// Plans the selected source and byte-preserving destination.
///
/// The result is deterministic. Callers may satisfy a remote-choice blocker and
/// invoke this function again, but must not duplicate branch classification.
///
/// # Errors
///
/// Returns an error when Git cannot classify the branch or configuration cannot be loaded.
pub(crate) fn plan(
    repository: &Repository,
    intent: Intent,
    branch: &str,
    remote: Option<&str>,
    fetch: FetchIntent,
    description: Option<String>,
    dry_run: bool,
) -> Result<Result<Plan, Blocker>> {
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
            dry_run,
        }));
    }

    let Some(primary) = repository.primary.as_ref() else {
        return Ok(Err(Blocker::PrimaryUnavailable));
    };
    let config = EffectiveConfig::load(repository)?;
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
    if fetch.refreshes()
        && let Some(candidate) = approval_candidate(repository, &config)?
    {
        return Ok(Err(Blocker::ApprovalRequired { candidate }));
    }

    let source = match plan_source(repository, classification, branch, remote, &config, fetch) {
        Ok(source) => source,
        Err(blocker) => return Ok(Err(*blocker)),
    };
    if !fetch.refreshes()
        && let Some(candidate) = approval_candidate(repository, &config)?
    {
        return Ok(Err(Blocker::ApprovalRequired { candidate }));
    }

    let plan = Plan {
        intent,
        branch: branch.to_owned(),
        destination,
        source,
        config: Some(config),
        description,
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
                None => Err(Box::new(Blocker::RemoteSelectionRequired { remotes })),
            }
        }
    }
}

/// Executes a navigation or creation plan without reclassifying its branch.
///
/// Hook-bearing plans remain owned by the setup executor and are rejected here.
#[allow(clippy::too_many_lines)] // This is the single explicit worktree execution boundary.
pub(crate) fn execute(
    repository: &Repository,
    plan: &Plan,
) -> std::result::Result<ExecutionOutcome, ExecutionFailure> {
    let mut effects = planned_effects(plan);
    if let Source::Registered(worktree) = &plan.source {
        return Ok(ExecutionOutcome {
            destination: worktree.path.clone(),
            effects,
        });
    }
    let config = plan.config.as_ref().expect("creation plan has config");
    if !config.post_create.is_empty() {
        return Err(ExecutionFailure {
            code: "hooks_present",
            error: anyhow::anyhow!("hook-bearing plan requires the setup executor"),
            effects,
            created: false,
        });
    }
    if plan.dry_run {
        return Ok(ExecutionOutcome {
            destination: plan.destination.clone(),
            effects,
        });
    }
    if let Err(error) = revalidate(repository, plan) {
        return Err(ExecutionFailure {
            code: "plan_stale",
            error,
            effects,
            created: false,
        });
    }
    if let Some(parent) = plan.destination.parent() {
        if let Err(error) = fs::create_dir_all(parent)
            .with_context(|| format!("failed to create destination parent {}", parent.display()))
        {
            return Err(ExecutionFailure {
                code: "creation_failed",
                error,
                effects,
                created: false,
            });
        }
    }
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
        return Err(ExecutionFailure {
            code: "creation_failed",
            error,
            effects,
            created: false,
        });
    }
    effects[creation_index].completed = true;
    if let Some(index) = branch_index {
        effects[index].completed = true;
    }
    if let Some(description) = plan.description.as_deref() {
        let index = effects
            .iter()
            .position(|effect| effect.action == "set_branch_description")
            .expect("described plans carry a description effect");
        effects[index].attempted = true;
        if let Err(error) =
            git::set_branch_description(&repository.current().path, &plan.branch, description)
        {
            return Err(ExecutionFailure {
                code: "description_failed",
                error,
                effects,
                created: true,
            });
        }
        effects[index].completed = true;
    }
    Ok(ExecutionOutcome {
        destination: plan.destination.clone(),
        effects,
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

fn planned_effects(plan: &Plan) -> Vec<Effect> {
    if matches!(plan.source, Source::Registered(_)) {
        return Vec::new();
    }
    let mut effects = Vec::new();
    if let Source::New { base } = &plan.source {
        if base.fetch_output.is_some() {
            effects.push(Effect {
                action: "fetch_base_ref".into(),
                attempted: true,
                completed: true,
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
