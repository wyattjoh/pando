//! Authoritative branch classification for worktree navigation and creation.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::{
    BaseMode, Worktree,
    git::{self, BaseRef, HistoryObservation, NewBranchBase, PushPlan, Repository},
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

/// One immutable branch/ref observation for a navigation planning attempt.
///
/// A snapshot contains registered worktrees, local refs, and already-fetched
/// remote-tracking refs. Observation never fetches or performs any mutation.
pub(crate) struct Snapshot<'repository> {
    repository: &'repository Repository,
    local: Vec<String>,
    remotes_by_branch: HashMap<String, Vec<String>>,
    remote_commits: HashMap<String, String>,
    local_commits: HashMap<String, String>,
    head_commit: String,
    origin_head: Option<String>,
    remotes: Vec<String>,
    remote_urls: HashMap<String, String>,
    upstreams: HashMap<String, String>,
}

impl<'repository> Snapshot<'repository> {
    /// Observes a complete navigation snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot inspect local or remote-tracking refs.
    pub(crate) fn observe(repository: &'repository Repository) -> Result<Self> {
        let facts = git::observe_branch_refs(&repository.current().path)?;
        let mut remotes_by_branch: HashMap<String, Vec<String>> = HashMap::new();
        let mut remote_commits = HashMap::new();
        let history = HistoryObservation::new(&repository.current().path);
        for remote_branch in facts.remote_branches {
            let Some((_, branch)) = remote_branch.split_once('/') else {
                continue;
            };
            remote_commits.insert(remote_branch.clone(), history.commit(&remote_branch)?);
            remotes_by_branch
                .entry(branch.to_owned())
                .or_default()
                .push(remote_branch);
        }
        let local_commits = facts
            .local
            .iter()
            .map(|branch| {
                history
                    .commit(branch)
                    .map(|commit| (branch.clone(), commit))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            repository,
            local: facts.local,
            remotes_by_branch,
            remote_commits,
            local_commits,
            head_commit: history.head_commit()?,
            origin_head: facts.origin_head,
            remotes: facts.remotes,
            remote_urls: facts.remote_urls,
            upstreams: facts.upstreams,
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

    /// Returns a local branch identity pinned to this observation epoch.
    pub(crate) fn local_commit(&self, branch: &str) -> Option<&str> {
        self.local_commits.get(branch).map(String::as_str)
    }

    /// Returns a remote-tracking identity pinned to this observation epoch.
    pub(crate) fn remote_commit(&self, reference: &str) -> Option<&str> {
        self.remote_commits.get(reference).map(String::as_str)
    }

    /// Fetched remote matches grouped by their unqualified branch name.
    pub(crate) fn remotes(&self) -> &HashMap<String, Vec<String>> {
        &self.remotes_by_branch
    }

    /// Validates a branch name through Git's canonical ref-name parser.
    pub(crate) fn validate(&self, branch: &str) -> Result<()> {
        git::validate_branch_name(&self.repository.current().path, branch)
    }

    /// Resolves the target branch from this observation epoch.
    pub(crate) fn target(&self, configured: Option<&str>) -> Result<String> {
        if let Some(branch) = configured {
            return Ok(branch.to_owned());
        }
        self.origin_head
            .as_deref()
            .filter(|branch| self.local.iter().any(|local| local == *branch))
            .or_else(|| self.local.iter().any(|branch| branch == "main").then_some("main"))
            .or_else(|| self.local.iter().any(|branch| branch == "master").then_some("master"))
            .map(str::to_owned)
            .context("no target branch is configured and no fallback branch exists; configure worktrees.target-branch or create main/master")
    }

    /// Resolves a merge target and pins source and target identities to this epoch.
    pub(crate) fn merge_target(&self, configured: Option<&str>) -> Result<MergeTarget> {
        let branch = self.target(configured)?;
        let target_commit = self
            .local_commits
            .get(&branch)
            .cloned()
            .with_context(|| format!("target branch {branch:?} does not exist locally"))?;
        Ok(MergeTarget {
            branch,
            source_commit: self.head_commit.clone(),
            target_commit,
        })
    }

    /// Resolves the remote repository used by a branch upstream.
    pub(crate) fn upstream_remote(&self, branch: &str) -> Result<Option<String>> {
        self.upstreams
            .get(branch)
            .map(|upstream| {
                upstream
                    .split_once('/')
                    .filter(|(remote, branch)| !remote.is_empty() && !branch.is_empty())
                    .map(|(remote, _)| remote.to_owned())
                    .with_context(|| {
                        format!("configured upstream is not a remote branch: {upstream}")
                    })
            })
            .transpose()
    }

    /// Returns the URL pinned for a configured remote in this snapshot.
    pub(crate) fn remote_url(&self, remote: &str) -> Result<&str> {
        self.remote_urls
            .get(remote)
            .map(String::as_str)
            .with_context(|| format!("remote {remote:?} does not exist"))
    }

    /// Resolves publication without prompting or mutating Git state.
    pub(crate) fn publication(
        &self,
        branch: &str,
        requested: Option<&str>,
    ) -> Result<PushResolution> {
        if let Some(remote) = requested {
            if !self.remotes.iter().any(|name| name == remote) {
                bail!("selected remote {remote:?} does not exist");
            }
            return Ok(PushResolution::Planned(PushPlan {
                remote: remote.to_owned(),
                branch: branch.to_owned(),
                set_upstream: true,
            }));
        }
        if let Some(upstream) = self.upstreams.get(branch) {
            let (remote, upstream_branch) = upstream
                .split_once('/')
                .filter(|(remote, branch)| !remote.is_empty() && !branch.is_empty())
                .with_context(|| {
                    format!("configured upstream is not a remote branch: {upstream}")
                })?;
            return Ok(PushResolution::Planned(PushPlan {
                remote: remote.to_owned(),
                branch: upstream_branch.to_owned(),
                set_upstream: false,
            }));
        }
        if self.remotes.iter().any(|remote| remote == "origin") {
            return Ok(PushResolution::Planned(PushPlan {
                remote: "origin".into(),
                branch: branch.into(),
                set_upstream: true,
            }));
        }
        if let [remote] = self.remotes.as_slice() {
            return Ok(PushResolution::Planned(PushPlan {
                remote: remote.clone(),
                branch: branch.into(),
                set_upstream: true,
            }));
        }
        if self.remotes.is_empty() {
            bail!("no Git remote is configured; add a remote before creating a pull request");
        }
        Ok(PushResolution::Ambiguous(self.remotes.clone()))
    }

    /// Resolves a commit-pinned new-branch base without mutating Git state.
    pub(crate) fn new_branch_base(
        &self,
        mode: BaseMode,
        configured_target: Option<&str>,
    ) -> Result<BaseResolution> {
        if mode == BaseMode::Head {
            return Ok(BaseResolution::Resolved(NewBranchBase {
                commit: self.head_commit.clone(),
                base_ref: None,
                fetch_output: None,
            }));
        }
        let base_ref = self.fresh_base_ref(configured_target)?;
        let reference = base_ref.reference();
        Ok(match self.remote_commits.get(&reference) {
            Some(commit) => BaseResolution::Resolved(NewBranchBase {
                commit: commit.clone(),
                base_ref: Some(base_ref),
                fetch_output: None,
            }),
            None => BaseResolution::FetchRequired(ExactFetch { base_ref }),
        })
    }

    /// Selects the exact ref an explicitly authorized fresh-base fetch refreshes.
    pub(crate) fn fresh_fetch(&self, configured_target: Option<&str>) -> Result<ExactFetch> {
        Ok(ExactFetch {
            base_ref: self.fresh_base_ref(configured_target)?,
        })
    }

    fn fresh_base_ref(&self, configured_target: Option<&str>) -> Result<BaseRef> {
        let branch = configured_target
            .map(str::to_owned)
            .or_else(|| self.origin_head.clone())
            .context(
                "worktrees.base is 'fresh' but no base branch could be resolved: set worktrees.target-branch, or record the remote's default branch with 'git remote set-head origin -a'",
            )?;
        Ok(BaseRef {
            remote: "origin".to_owned(),
            branch,
        })
    }
}

/// A semantic merge target with identities pinned to one snapshot epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MergeTarget {
    pub(crate) branch: String,
    pub(crate) source_commit: String,
    pub(crate) target_commit: String,
}

/// A pure new-branch base resolution from one snapshot epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BaseResolution {
    Resolved(NewBranchBase),
    FetchRequired(ExactFetch),
}

/// The exact ref mutation required before the complete plan can be rebuilt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExactFetch {
    pub(crate) base_ref: BaseRef,
}

impl ExactFetch {
    #[must_use]
    pub(crate) fn unavailable_message(&self) -> String {
        let reference = self.base_ref.reference();
        format!(
            "the base ref {reference:?} has not been fetched into this clone; run 'git fetch {} {}' or pass --fetch",
            self.base_ref.remote, self.base_ref.branch
        )
    }
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

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use tempfile::TempDir;

    use super::{BaseResolution, Classification, PushResolution, Snapshot};
    use crate::{BaseMode, Condition, Worktree, WorktreeKind, git::RepositoryObservation};

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
        run(directory.path(), &["config", "commit.gpgsign", "false"]);
        run(
            directory.path(),
            &["commit", "--allow-empty", "-m", "initial"],
        );
        directory
    }

    #[test]
    fn snapshot_classifies_one_immutable_observation_in_precedence_order() {
        let directory = repository();
        run(directory.path(), &["branch", "local-only"]);
        run(
            directory.path(),
            &["update-ref", "refs/remotes/origin/remote-only", "HEAD"],
        );
        run(
            directory.path(),
            &["update-ref", "refs/remotes/backup/ambiguous", "HEAD"],
        );
        run(
            directory.path(),
            &["update-ref", "refs/remotes/origin/ambiguous", "HEAD"],
        );
        run(directory.path(), &["branch", "exceptional"]);
        run(
            directory.path(),
            &["update-ref", "refs/remotes/origin/exceptional", "HEAD"],
        );
        let mut repository = RepositoryObservation::new(directory.path())
            .repository()
            .expect("repository observation");
        let exceptional = Worktree {
            path: directory.path().join("missing-worktree"),
            head: None,
            last_commit_at: None,
            kind: WorktreeKind::Branch("exceptional".into()),
            locked: None,
            prunable: Some("gitdir file points to non-existent location".into()),
            current: false,
            condition: Condition::Missing,
        };
        repository.worktrees.push(exceptional.clone());

        let snapshot = Snapshot::observe(&repository).expect("snapshot observation");
        assert!(matches!(
            snapshot.classify("main"),
            Classification::Registered(_)
        ));
        assert_eq!(snapshot.classify("local-only"), Classification::Local);
        assert_eq!(
            snapshot.classify("remote-only"),
            Classification::Remotes(vec!["origin/remote-only".into()])
        );
        assert_eq!(
            snapshot.classify("ambiguous"),
            Classification::Remotes(vec!["backup/ambiguous".into(), "origin/ambiguous".into()])
        );
        assert_eq!(
            snapshot.classify("exceptional"),
            Classification::Registered(exceptional)
        );
        assert_eq!(snapshot.classify("new-branch"), Classification::New);

        run(directory.path(), &["branch", "observed-later"]);
        assert_eq!(snapshot.classify("observed-later"), Classification::New);
    }

    #[test]
    fn snapshot_pins_head_and_fresh_bases_to_one_observation() {
        let directory = repository();
        run(
            directory.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        run(
            directory.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let repository = RepositoryObservation::new(directory.path())
            .repository()
            .expect("repository observation");
        let snapshot = Snapshot::observe(&repository).expect("snapshot observation");
        let head = snapshot
            .new_branch_base(BaseMode::Head, None)
            .expect("head base");
        let fresh = snapshot
            .new_branch_base(BaseMode::Fresh, None)
            .expect("fresh base");
        let BaseResolution::Resolved(head) = head else {
            panic!("head must resolve locally");
        };
        let BaseResolution::Resolved(fresh) = fresh else {
            panic!("fresh must resolve locally");
        };
        assert_eq!(head.commit, fresh.commit);

        run(
            directory.path(),
            &["commit", "--allow-empty", "-m", "later"],
        );
        assert_eq!(
            snapshot
                .new_branch_base(BaseMode::Head, None)
                .expect("pinned head base"),
            BaseResolution::Resolved(head)
        );
        assert_eq!(
            snapshot
                .new_branch_base(BaseMode::Fresh, None)
                .expect("pinned fresh base"),
            BaseResolution::Resolved(fresh)
        );
    }

    #[test]
    fn missing_fresh_base_returns_the_exact_non_mutating_fetch_requirement() {
        let directory = repository();
        let repository = RepositoryObservation::new(directory.path())
            .repository()
            .expect("repository observation");
        let snapshot = Snapshot::observe(&repository).expect("snapshot observation");

        let resolution = snapshot
            .new_branch_base(BaseMode::Fresh, Some("release"))
            .expect("typed base resolution");
        let BaseResolution::FetchRequired(requirement) = resolution else {
            panic!("missing ref must require a fetch");
        };
        assert_eq!(requirement.base_ref.reference(), "origin/release");
        assert!(requirement.unavailable_message().contains("--fetch"));
        assert_eq!(snapshot.classify("release"), Classification::New);
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
        let repository = RepositoryObservation::new(directory.path())
            .repository()
            .expect("repository observation");

        assert_eq!(
            Snapshot::observe(&repository)
                .expect("snapshot observation")
                .publication("main", None)
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
        let repository = RepositoryObservation::new(directory.path())
            .repository()
            .expect("repository observation");

        let PushResolution::Planned(plan) = Snapshot::observe(&repository)
            .expect("snapshot observation")
            .publication("main", Some("zeta"))
            .expect("push resolution")
        else {
            panic!("explicit remote must produce a plan");
        };
        assert_eq!(plan.remote, "zeta");
        assert_eq!(plan.branch, "main");
        assert!(plan.set_upstream);
    }
}
