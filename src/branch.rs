//! Authoritative branch classification for worktree navigation and creation.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail};

use crate::{
    BaseMode, Worktree,
    git::{self, BaseRef, NewBranchBase, PushPlan, Repository, RepositoryObservation},
    worktree_for_branch,
};

/// The first matching branch category in Pando's resolution order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Classification {
    /// A worktree is already registered for the branch, including exceptional states.
    Registered(Worktree),
    /// A local branch exists without a registered worktree.
    Local,
    /// One or more fetched remote-tracking branches match.
    Remotes(Vec<String>),
    /// No registered, local, or fetched remote branch matches.
    New,
}

/// Read-only branch facts shared by command planning and completion.
///
/// Facts are one snapshot of registered worktrees, local refs, and already
/// fetched remote-tracking refs. Discovery never fetches or performs any other
/// mutation.
pub(crate) struct Facts<'repository> {
    repository: &'repository Repository,
    local: Vec<String>,
    remotes_by_branch: HashMap<String, Vec<String>>,
}

impl<'repository> Facts<'repository> {
    /// Discovers a complete branch-fact snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot inspect local or remote-tracking refs.
    pub(crate) fn discover(repository: &'repository Repository) -> Result<Self> {
        let observation = RepositoryObservation::new(&repository.current().path);
        let local = observation
            .branches()?
            .into_iter()
            .map(|record| record.branch)
            .collect();
        let mut remotes_by_branch: HashMap<String, Vec<String>> = HashMap::new();
        for remote_branch in observation.remote_branches()? {
            let Some((_, branch)) = remote_branch.split_once('/') else {
                continue;
            };
            remotes_by_branch
                .entry(branch.to_owned())
                .or_default()
                .push(remote_branch);
        }
        Ok(Self {
            repository,
            local,
            remotes_by_branch,
        })
    }

    /// Classifies `branch` using Pando's established resolution order.
    #[must_use]
    pub(crate) fn classify(&self, branch: &str) -> Classification {
        if let Some(worktree) = worktree_for_branch(&self.repository.worktrees, branch) {
            return Classification::Registered(worktree.clone());
        }
        if self.local.iter().any(|local| local == branch) {
            return Classification::Local;
        }
        self.remotes_by_branch
            .get(branch)
            .map_or(Classification::New, |remotes| {
                Classification::Remotes(remotes.clone())
            })
    }

    /// Local branch names in Git discovery order.
    pub(crate) fn local(&self) -> &[String] {
        &self.local
    }

    /// Registered worktrees, including the primary worktree.
    pub(crate) fn registered(&self) -> &[Worktree] {
        &self.repository.worktrees
    }

    /// Fetched remote matches grouped by their unqualified branch name.
    pub(crate) fn remotes(&self) -> &HashMap<String, Vec<String>> {
        &self.remotes_by_branch
    }
}

/// Classifies `branch` using Pando's established resolution order.
///
/// This function is deliberately deterministic and noninteractive. Callers own
/// intent-specific policy such as `create` refusing a registered worktree or
/// `switch` requiring confirmation before creating a genuinely new branch.
///
/// # Errors
///
/// Returns an error when Git cannot inspect local or remote-tracking refs.
pub(crate) fn classify(repository: &Repository, branch: &str) -> Result<Classification> {
    Ok(Facts::discover(repository)?.classify(branch))
}

pub(crate) const FETCH_REGISTERED_WORKTREE: &str = "the branch already has a registered worktree";
pub(crate) const FETCH_LOCAL_BRANCH: &str = "the branch already exists locally";
pub(crate) const FETCH_REMOTE_BRANCH: &str = "the branch already has a remote-tracking ref";
pub(crate) const FETCH_HEAD_BASE: &str =
    "the effective worktrees.base is 'head', which branches from the invoking worktree";

pub(crate) fn reject_fetch(inapplicable: bool, because: &str) -> Result<()> {
    if inapplicable {
        bail!(
            "a base-ref fetch only applies to a genuinely new branch on a 'fresh' base, but {because}"
        );
    }
    Ok(())
}

/// A push plan or the explicit adapter decision needed to complete one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PushResolution {
    Planned(PushPlan),
    Ambiguous(Vec<String>),
}

/// Concrete interface for branch and ref resolution.
///
/// This capability owns Git's precedence and probing choreography. It is
/// deliberately noninteractive: ambiguous remotes are returned to adapters.
#[derive(Clone, Copy)]
pub(crate) struct Resolver<'repository> {
    repository: &'repository Repository,
}

impl<'repository> Resolver<'repository> {
    #[must_use]
    pub(crate) fn new(repository: &'repository Repository) -> Self {
        Self { repository }
    }

    fn cwd(self) -> &'repository Path {
        &self.repository.current().path
    }

    pub(crate) fn validate(self, branch: &str) -> Result<()> {
        git::validate_branch(self.cwd(), branch)
    }

    pub(crate) fn reject_registered_fetch(requested: bool) -> Result<()> {
        reject_fetch(requested, FETCH_REGISTERED_WORKTREE)
    }

    pub(crate) fn classify(self, branch: &str) -> Result<Classification> {
        classify(self.repository, branch)
    }

    pub(crate) fn target(self, configured: Option<&str>) -> Result<String> {
        git::resolve_target_branch(self.cwd(), configured)
    }

    pub(crate) fn upstream_remote(self, branch: &str) -> Result<Option<String>> {
        Ok(git::branch_upstream(self.cwd(), branch)?
            .as_deref()
            .and_then(|upstream| upstream.split_once('/'))
            .map(|(remote, _)| remote.to_owned()))
    }

    pub(crate) fn remote_url(self, remote: &str) -> Result<String> {
        git::remote_url(self.cwd(), remote)
    }

    /// Plans an ordinary push without prompting or choosing among ambiguous remotes.
    pub(crate) fn push(self, branch: &str, requested: Option<&str>) -> Result<PushResolution> {
        if let Some(remote) = requested {
            if !git::configured_remotes(self.cwd())?
                .iter()
                .any(|name| name == remote)
            {
                bail!("selected remote {remote:?} does not exist");
            }
            return Ok(PushResolution::Planned(PushPlan {
                remote: remote.to_owned(),
                branch: branch.to_owned(),
                set_upstream: true,
            }));
        }
        if let Some(remote) = self.upstream_remote(branch)? {
            let upstream = git::branch_upstream(self.cwd(), branch)?
                .context("configured upstream is not a remote branch")?;
            let (_, upstream_branch) = upstream
                .split_once('/')
                .context("configured upstream is not a remote branch")?;
            if remote.is_empty() || upstream_branch.is_empty() {
                bail!("configured upstream is not a remote branch: {upstream}");
            }
            return Ok(PushResolution::Planned(PushPlan {
                remote,
                branch: upstream_branch.to_owned(),
                set_upstream: false,
            }));
        }
        let remotes = git::configured_remotes(self.cwd())?;
        if remotes.iter().any(|remote| remote == "origin") {
            return Ok(PushResolution::Planned(PushPlan {
                remote: "origin".into(),
                branch: branch.into(),
                set_upstream: true,
            }));
        }
        if let [remote] = remotes.as_slice() {
            return Ok(PushResolution::Planned(PushPlan {
                remote: remote.clone(),
                branch: branch.into(),
                set_upstream: true,
            }));
        }
        if remotes.is_empty() {
            bail!("no Git remote is configured; add a remote before creating a pull request");
        }
        Ok(PushResolution::Ambiguous(remotes))
    }

    pub(crate) fn publish(self, plan: &PushPlan, display_output: bool) -> Result<()> {
        git::push(self.cwd(), plan, display_output)
    }

    pub(crate) fn new_branch_base(
        self,
        mode: BaseMode,
        configured_target: Option<&str>,
        fetch: bool,
    ) -> Result<NewBranchBase> {
        git::plan_new_branch_base(self.cwd(), mode, configured_target, fetch)
    }

    pub(crate) fn base_commit(self, base: &BaseRef) -> Result<String> {
        git::base_ref_commit(self.cwd(), base)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use tempfile::TempDir;

    use super::{PushResolution, Resolver};
    use crate::git;

    fn run(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git should start");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository() -> TempDir {
        let directory = tempfile::tempdir().expect("temporary repository");
        run(directory.path(), &["init", "-b", "main"]);
        run(directory.path(), &["config", "user.name", "Pando Test"]);
        run(
            directory.path(),
            &["config", "user.email", "pando@example.com"],
        );
        run(
            directory.path(),
            &["commit", "--allow-empty", "-m", "initial"],
        );
        directory
    }

    #[test]
    fn push_ambiguity_is_typed_and_sorted() {
        let directory = repository();
        run(
            directory.path(),
            &["remote", "add", "zeta", "https://example.com/zeta.git"],
        );
        run(
            directory.path(),
            &["remote", "add", "alpha", "https://example.com/alpha.git"],
        );
        let repository = git::repository(directory.path()).expect("repository observation");

        assert_eq!(
            Resolver::new(&repository)
                .push("main", None)
                .expect("push resolution"),
            PushResolution::Ambiguous(vec!["alpha".into(), "zeta".into()])
        );
    }

    #[test]
    fn explicit_push_remote_has_precedence_over_ambiguity() {
        let directory = repository();
        run(
            directory.path(),
            &["remote", "add", "zeta", "https://example.com/zeta.git"],
        );
        run(
            directory.path(),
            &["remote", "add", "alpha", "https://example.com/alpha.git"],
        );
        let repository = git::repository(directory.path()).expect("repository observation");

        let PushResolution::Planned(plan) = Resolver::new(&repository)
            .push("main", Some("zeta"))
            .expect("push resolution")
        else {
            panic!("explicit remote must produce a plan");
        };
        assert_eq!(plan.remote, "zeta");
        assert_eq!(plan.branch, "main");
        assert!(plan.set_upstream);
    }
}
