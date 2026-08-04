//! Shared destination and source planning for worktree navigation and creation.

use std::path::PathBuf;

use anyhow::Result;

use crate::{
    Worktree,
    branch::{self, Classification},
    config::EffectiveConfig,
    git::{self, Repository},
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
    Local,
    Remote(String),
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

    let source = match plan_source(repository, classification, branch, remote, &config, fetch) {
        Ok(source) => source,
        Err(blocker) => return Ok(Err(*blocker)),
    };

    Ok(Ok(Plan {
        intent,
        branch: branch.to_owned(),
        destination,
        source,
        config: Some(config),
    }))
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
            Ok(Source::Local)
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
                    .map(Source::Remote)
                    .ok_or_else(|| Box::new(Blocker::UnknownRemote)),
                None if remotes.len() == 1 => Ok(Source::Remote(remotes[0].clone())),
                None => Err(Box::new(Blocker::RemoteSelectionRequired { remotes })),
            }
        }
    }
}

fn reject_fetch(fetch: FetchIntent, because: &str) -> Result<(), Box<Blocker>> {
    git::reject_fetch(fetch.requested(), because).map_err(|error| {
        Box::new(Blocker::FetchNotApplicable {
            message: format!("{error:#}"),
        })
    })
}
