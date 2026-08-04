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
    New,
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
) -> Result<Result<Plan, Blocker>> {
    let classification = branch::classify(repository, branch)?;
    if let Classification::Registered(worktree) = classification {
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

    let source = match classification {
        Classification::Registered(_) => unreachable!("registered classification returned above"),
        Classification::Local => {
            if remote.is_some() {
                return Ok(Err(Blocker::IrrelevantRemote));
            }
            Source::Local
        }
        Classification::New => {
            if remote.is_some() {
                return Ok(Err(Blocker::UnknownRemote));
            }
            Source::New
        }
        Classification::Remotes(remotes) => {
            let selected = match remote {
                Some(remote) => remotes
                    .iter()
                    .find(|candidate| {
                        candidate.as_str() == remote
                            || candidate.as_str() == format!("{remote}/{branch}")
                    })
                    .cloned()
                    .ok_or(Blocker::UnknownRemote),
                None if remotes.len() == 1 => Ok(remotes[0].clone()),
                None => Err(Blocker::RemoteSelectionRequired { remotes }),
            };
            match selected {
                Ok(remote) => Source::Remote(remote),
                Err(blocker) => return Ok(Err(blocker)),
            }
        }
    };

    Ok(Ok(Plan {
        intent,
        branch: branch.to_owned(),
        destination,
        source,
        config: Some(config),
    }))
}
