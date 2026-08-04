use std::{
    collections::{BTreeSet, HashMap},
    ffi::{OsStr, OsString},
    fs,
    io::Write,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, FixedOffset};

use crate::{BaseMode, Condition, Worktree, WorktreeKind};

/// Private execution kernel for the installed Git executable.
///
/// Callers choose only the stream policy needed by the repository operation;
/// executable selection, working directory, terminal routing, and optional
/// editor suppression stay local to this module.
struct GitProcess {
    command: Command,
}

struct PipedContexts {
    start: &'static str,
    open_input: &'static str,
    write_input: &'static str,
    writer_panicked: &'static str,
    await_output: &'static str,
}

impl GitProcess {
    fn new<I, S>(cwd: &Path, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command.args(args).current_dir(cwd);
        Self { command }
    }

    fn suppress_editor(mut self) -> Self {
        self.command.env("GIT_EDITOR", "true");
        self
    }

    fn disable_terminal_prompt(mut self) -> Self {
        self.command.env("GIT_TERMINAL_PROMPT", "0");
        self
    }

    fn captured(mut self) -> std::io::Result<Output> {
        self.command.stdin(Stdio::null()).output()
    }

    fn captured_inheriting_stdin(mut self) -> std::io::Result<Output> {
        self.command.stdin(Stdio::inherit()).output()
    }

    fn displayed(mut self) -> Result<ExitStatus> {
        self.command
            .stdin(Stdio::inherit())
            .stdout(Stdio::from(open_stderr()?))
            .stderr(Stdio::inherit())
            .status()
            .map_err(Into::into)
    }

    fn piped(mut self, input: Vec<u8>, contexts: &PipedContexts) -> Result<Output> {
        let mut child = self
            .command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(contexts.start)?;
        let mut stdin = child.stdin.take().context(contexts.open_input)?;
        let write_context = contexts.write_input;
        let writer =
            thread::spawn(move || -> Result<()> { stdin.write_all(&input).context(write_context) });
        let output = child.wait_with_output();
        let writer = writer
            .join()
            .map_err(|_| anyhow!(contexts.writer_panicked))?;
        let output = output.context(contexts.await_output)?;
        writer?;
        Ok(output)
    }
}

/// A typed source for creating a registered worktree.
///
/// Keeping these variants semantic prevents command callers from assembling
/// `git worktree add` arguments or accidentally changing tracking behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorktreeSource<'source> {
    Existing,
    Tracking { remote_ref: &'source str },
    New { start_point: &'source str },
}

/// Whether removal may discard uncommitted worktree changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemovalMode {
    Safe,
    Force,
}

/// Whether removal diagnostics are rendered by Git or returned to the owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemovalOutput {
    Captured,
    Displayed,
}

/// Captured diagnostics from a worktree removal.
#[derive(Debug)]
pub(crate) struct RemovalDiagnostics {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Concrete interface for safe worktree creation, description, and removal.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorktreeMutation<'cwd> {
    cwd: &'cwd Path,
}

impl<'cwd> WorktreeMutation<'cwd> {
    #[must_use]
    pub(crate) fn new(cwd: &'cwd Path) -> Self {
        Self { cwd }
    }

    /// Creates a worktree from a semantic branch source.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot create the worktree. Git remains the
    /// safety authority and refuses destination collisions rather than
    /// adopting, repairing, pruning, moving, or deleting an existing path.
    pub(crate) fn create(
        self,
        destination: &Path,
        branch: &str,
        source: WorktreeSource<'_>,
    ) -> Result<()> {
        match source {
            WorktreeSource::Existing => run_worktree_add(
                self.cwd,
                vec![
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    destination.as_os_str(),
                    OsStr::new(branch),
                ],
                branch,
                destination,
            ),
            WorktreeSource::Tracking { remote_ref } => run_worktree_add(
                self.cwd,
                vec![
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    OsStr::new("--track"),
                    OsStr::new("-b"),
                    OsStr::new(branch),
                    destination.as_os_str(),
                    OsStr::new(remote_ref),
                ],
                branch,
                destination,
            ),
            WorktreeSource::New { start_point } => run_worktree_add(
                self.cwd,
                vec![
                    OsStr::new("worktree"),
                    OsStr::new("add"),
                    OsStr::new("-b"),
                    OsStr::new(branch),
                    destination.as_os_str(),
                    OsStr::new(start_point),
                ],
                branch,
                destination,
            ),
        }
    }

    /// Sets the repository-local description after creation succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot update the local configuration.
    pub(crate) fn describe(self, branch: &str, description: &str) -> Result<()> {
        let key = format!("branch.{branch}.description");
        let output = GitProcess::new(
            self.cwd,
            ["config", "--local", "--replace-all", &key, description],
        )
        .captured()
        .context("failed to start Git while setting the branch description")?;
        if output.status.success() {
            Ok(())
        } else {
            bail!(
                "Git failed to set the description for branch {branch:?}: {}",
                stderr_detail(&output)
            )
        }
    }

    /// Removes a registered worktree while retaining its branch.
    ///
    /// # Errors
    ///
    /// Returns an error when displayed removal fails or Git cannot start.
    pub(crate) fn remove(
        self,
        path: &Path,
        mode: RemovalMode,
        output: RemovalOutput,
    ) -> Result<Option<RemovalDiagnostics>> {
        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if mode == RemovalMode::Force {
            args.push(OsStr::new("--force"));
        }
        args.push(path.as_os_str());
        match output {
            RemovalOutput::Displayed => {
                let status = GitProcess::new(self.cwd, args)
                    .displayed()
                    .context("failed to start git worktree remove")?;
                if status.success() {
                    Ok(None)
                } else {
                    bail!("git worktree remove failed with {status}")
                }
            }
            RemovalOutput::Captured => {
                let output = GitProcess::new(self.cwd, args)
                    .captured()
                    .context("failed to start git worktree remove")?;
                Ok(Some(RemovalDiagnostics {
                    status: output.status,
                    stdout: output.stdout,
                    stderr: output.stderr,
                }))
            }
        }
    }
}

/// Controls whether a lifecycle operation is rendered directly or captured
/// for an owning adapter. The operation still chooses stdin and environment
/// policy, so callers cannot accidentally inherit an editor or prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleOutput {
    Captured,
    Displayed,
}

/// Concrete mutation interface for Git operations that change branch history
/// or the index during commit and merge lifecycles.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LifecycleMutation<'cwd> {
    cwd: &'cwd Path,
}

impl<'cwd> LifecycleMutation<'cwd> {
    #[must_use]
    pub(crate) fn new(cwd: &'cwd Path) -> Self {
        Self { cwd }
    }

    fn run(self, args: &[&str], operation: &str, output: LifecycleOutput) -> Result<String> {
        run_lifecycle_git(self.cwd, args, operation, output)
    }

    pub(crate) fn switch_branch(self, branch: &str, output: LifecycleOutput) -> Result<String> {
        self.run(&["switch", branch], "switch branch", output)
    }

    pub(crate) fn fast_forward(self, branch: &str, output: LifecycleOutput) -> Result<String> {
        self.run(
            &["merge", "--ff-only", branch],
            "fast-forward merge",
            output,
        )
    }

    pub(crate) fn rebase_onto(self, target: &str, output: LifecycleOutput) -> Result<String> {
        self.run(&["rebase", target], "rebase", output)
    }

    pub(crate) fn rebase_onto_autostash(
        self,
        target: &str,
        output: LifecycleOutput,
    ) -> Result<String> {
        self.run(&["rebase", "--autostash", target], "rebase", output)
    }

    pub(crate) fn continue_rebase(self, output: LifecycleOutput) -> Result<String> {
        self.run(&["rebase", "--continue"], "continue rebase", output)
    }

    pub(crate) fn rebase_in_progress(self) -> Result<bool> {
        let git_dir = worktree_identity(self.cwd)
            .with_context(|| format!("failed to inspect rebase state in {}", self.cwd.display()))?;
        Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
    }

    pub(crate) fn stage_all(self) -> Result<()> {
        let output = run_git(self.cwd, ["add", "--all"])
            .with_context(|| format!("failed to start git add --all in {}", self.cwd.display()))?;
        ensure_success(&output, "git add --all")
            .with_context(|| format!("failed to stage changes in {}", self.cwd.display()))
    }

    pub(crate) fn reset_soft(self, commit: &str) -> Result<String> {
        self.run(
            &["reset", "--soft", commit],
            "soft reset",
            LifecycleOutput::Captured,
        )
    }

    pub(crate) fn commit_message(self, message: &str) -> Result<String> {
        commit_message_stdin_impl(self.cwd, message)
            .with_context(|| format!("failed to create commit in {}", self.cwd.display()))
    }

    pub(crate) fn commit(self, message: &str) -> Result<()> {
        commit_impl(self.cwd, message)
            .with_context(|| format!("failed to create commit in {}", self.cwd.display()))
    }

    pub(crate) fn commit_captured(self, message: &str) -> Result<Output> {
        GitProcess::new(self.cwd, ["commit", "-m", message])
            .disable_terminal_prompt()
            .captured()
            .with_context(|| format!("failed to start git commit in {}", self.cwd.display()))
    }
}

#[derive(Debug)]
pub struct Discovery {
    pub worktrees: Vec<Worktree>,
    pub metadata_warning: Option<String>,
}

/// One byte-safe entry from Git's NUL-delimited porcelain status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusEntry {
    pub(crate) code: [u8; 2],
    pub(crate) path: OsString,
    pub(crate) original_path: Option<OsString>,
}

impl StatusEntry {
    #[must_use]
    pub(crate) fn is_staged(&self) -> bool {
        self.code != *b"??" && self.code[0] != b' '
    }

    #[must_use]
    pub(crate) fn is_unstaged(&self) -> bool {
        self.code != *b"??" && self.code[1] != b' '
    }

    #[must_use]
    pub(crate) fn is_untracked(&self) -> bool {
        self.code == *b"??"
    }
}

/// A semantic snapshot of staged, unstaged, and untracked paths.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StatusSnapshot {
    pub(crate) entries: Vec<StatusEntry>,
}

impl StatusSnapshot {
    #[must_use]
    pub(crate) fn is_dirty(&self) -> bool {
        !self.entries.is_empty()
    }

    pub(crate) fn into_porcelain(self) -> Vec<u8> {
        let mut output = Vec::new();
        for entry in self.entries {
            output.extend_from_slice(&entry.code);
            output.push(b' ');
            output.extend_from_slice(entry.path.as_bytes());
            output.push(0);
            if let Some(original_path) = entry.original_path {
                output.extend_from_slice(original_path.as_bytes());
                output.push(0);
            }
        }
        output
    }
}

/// A patch and its statistics observed from the Git index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StagedChanges {
    pub(crate) patch: String,
    pub(crate) statistics: String,
}

/// Selects whether a range snapshot observes committed or staged tree changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RangeDiffSource {
    Committed,
    Staged,
}

/// Commit messages and diff material for `ancestor..descendant`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitRange {
    pub(crate) head_commit: String,
    pub(crate) commit_count: usize,
    pub(crate) messages: Vec<String>,
    pub(crate) patch: String,
    pub(crate) statistics: String,
}

/// Diff material and committed subjects used to generate pull-request metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PullRequestChanges {
    pub(crate) patch: String,
    pub(crate) statistics: String,
    pub(crate) subjects: String,
}

/// Concrete interface to commit graph, index, status, and diff observations.
///
/// Range construction, stable diff flags, message ordering, and byte-safe
/// status parsing stay behind this capability rather than leaking Git command
/// choreography into command modules.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HistoryObservation<'cwd> {
    cwd: &'cwd Path,
}

impl<'cwd> HistoryObservation<'cwd> {
    #[must_use]
    pub(crate) fn new(cwd: &'cwd Path) -> Self {
        Self { cwd }
    }

    pub(crate) fn head_commit(self) -> Result<String> {
        head_commit_observed(self.cwd)
    }

    pub(crate) fn commit(self, reference: &str) -> Result<String> {
        branch_commit_observed(self.cwd, reference)
    }

    pub(crate) fn is_ancestor(self, ancestor: &str, descendant: &str) -> Result<bool> {
        ancestry_observed(self.cwd, ancestor, descendant)
    }

    pub(crate) fn status(self) -> Result<StatusSnapshot> {
        status_observed(self.cwd)
    }

    pub(crate) fn has_staged_changes(self) -> Result<bool> {
        staged_changes_present(self.cwd)
    }

    pub(crate) fn staged(self, target: Option<&str>) -> Result<StagedChanges> {
        Ok(StagedChanges {
            patch: staged_patch(self.cwd, target)?,
            statistics: staged_statistics(self.cwd, target)?,
        })
    }

    pub(crate) fn pull_request_changes(self, base: &str) -> Result<PullRequestChanges> {
        let diff_range = format!("{base}...HEAD");
        let subject_range = format!("{base}..HEAD");
        Ok(PullRequestChanges {
            patch: stable_diff(self.cwd, &diff_range, None)?,
            statistics: stable_diff(self.cwd, &diff_range, Some("--stat"))?,
            subjects: git_stdout(self.cwd, ["log", "--format=%s", &subject_range])?,
        })
    }

    pub(crate) fn count_from_head(self, ancestor: &str) -> Result<usize> {
        let head = self.head_commit()?;
        range_commit_count(self.cwd, ancestor, &head)
    }

    pub(crate) fn range_from_head(
        self,
        ancestor: &str,
        diff_source: RangeDiffSource,
    ) -> Result<CommitRange> {
        let head_commit = self.head_commit()?;
        let commit_count = range_commit_count(self.cwd, ancestor, &head_commit)?;
        let messages = range_commit_messages(self.cwd, ancestor, &head_commit)?;
        let (patch, statistics) = match diff_source {
            RangeDiffSource::Committed => (
                range_patch(self.cwd, ancestor, &head_commit, None)?,
                range_patch(self.cwd, ancestor, &head_commit, Some("--stat"))?,
            ),
            RangeDiffSource::Staged => (
                staged_patch(self.cwd, Some(ancestor))?,
                staged_statistics(self.cwd, Some(ancestor))?,
            ),
        };
        Ok(CommitRange {
            head_commit,
            commit_count,
            messages,
            patch,
            statistics,
        })
    }

    pub(crate) fn recent_subjects(self) -> Result<Vec<String>> {
        recent_subjects_observed(self.cwd)
    }
}

/// Concrete read-only interface to facts reported by the installed Git executable.
///
/// This interface owns discovery and enrichment choreography so callers receive
/// typed snapshots without learning command order or structured-output formats.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RepositoryObservation<'cwd> {
    cwd: &'cwd Path,
}

impl<'cwd> RepositoryObservation<'cwd> {
    #[must_use]
    pub(crate) fn new(cwd: &'cwd Path) -> Self {
        Self { cwd }
    }

    fn discover(self) -> Result<Discovery> {
        Ok(Discovery {
            worktrees: discover_worktrees(self.cwd)?,
            metadata_warning: None,
        })
    }

    fn discover_with_metadata(self) -> Result<Discovery> {
        let mut discovery = self.discover()?;
        discovery.metadata_warning = enrich_last_commit_at(self.cwd, &mut discovery.worktrees)
            .err()
            .map(|error| format!("failed to load last-commit metadata: {error:#}"));
        Ok(discovery)
    }

    pub(crate) fn repository(self) -> Result<Repository> {
        repository_from_worktrees(self.cwd, self.discover()?)
    }

    pub(crate) fn repository_with_metadata(self) -> Result<Repository> {
        repository_from_worktrees(self.cwd, self.discover_with_metadata()?)
    }

    pub(crate) fn repository_with_branches(self) -> Result<RepositoryBranches> {
        repository_with_branches_observed(self.cwd)
    }

    /// Resolves a worktree's stable administrative identity.
    ///
    /// # Errors
    ///
    /// Returns an error when Git cannot resolve the worktree's administrative directory.
    pub(crate) fn worktree_identity(self) -> Result<PathBuf> {
        worktree_identity(self.cwd)
    }

    pub(crate) fn is_ignored(self, path: &Path) -> Result<bool> {
        is_ignored(self.cwd, path)
    }

    pub(crate) fn would_be_ignored(self, path: &Path) -> Result<bool> {
        would_be_ignored(self.cwd, path)
    }

    pub(crate) fn resolve_path(path: &Path) -> Result<PathBuf> {
        canonical_or_normalized(path)
    }

    pub(crate) fn branches(self) -> Result<Vec<BranchRecord>> {
        Ok(discover_branch_refs(self.cwd)?.0)
    }

    pub(crate) fn configured_editor(self) -> Result<Option<String>> {
        let output = run_git(self.cwd, ["config", "--get", "core.editor"])?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((!value.is_empty()).then_some(value))
    }

    pub(crate) fn committed_file(self, path: &str) -> Result<Option<String>> {
        let object = format!("HEAD:{path}");
        let output = run_git(self.cwd, ["show", &object])?;
        if !output.status.success() {
            return Ok(None);
        }
        String::from_utf8(output.stdout)
            .context("repository pull-request template is not UTF-8")
            .map(Some)
    }

    pub(crate) fn remote_branches(self) -> Result<Vec<String>> {
        let output = run_git(
            self.cwd,
            [
                "for-each-ref",
                "--format=%(refname:short)%00%(symref)%00",
                "refs/remotes",
            ],
        )
        .context("failed to list remote branches")?;
        ensure_success(&output, "git for-each-ref")?;
        Ok(parse_remote_branch_refs(&output.stdout))
    }
}

#[derive(Debug)]
pub struct Repository {
    pub worktrees: Vec<Worktree>,
    pub current_index: usize,
    pub primary: Option<PathBuf>,
    pub common_dir: PathBuf,
    pub metadata_warning: Option<String>,
}

impl Repository {
    #[must_use]
    pub fn current(&self) -> &Worktree {
        &self.worktrees[self.current_index]
    }

    /// Returns the canonical primary-worktree path used as repository identity.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no primary worktree or its path cannot be resolved.
    pub fn identity(&self) -> Result<PathBuf> {
        let primary = self
            .primary
            .as_ref()
            .context("the current repository has no primary worktree")?;
        canonical_or_normalized(primary)
            .with_context(|| format!("failed to resolve repository path {}", primary.display()))
    }

    /// Returns the current named branch from this repository snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for detached, bare, or unknown current worktree states.
    pub fn current_branch(&self) -> Result<&str> {
        match &self.current().kind {
            WorktreeKind::Branch(branch) => Ok(branch),
            WorktreeKind::Detached => bail!(
                "the current worktree at {} is detached; this query requires a named branch",
                self.current().path.display()
            ),
            WorktreeKind::Bare => {
                bail!("the current repository is bare; this query requires a worktree")
            }
            WorktreeKind::Unknown => {
                bail!("Git did not report a named branch for the current worktree")
            }
        }
    }
}

/// Discovers the worktrees for the repository containing `cwd`.
///
/// This base discovery does not query commit objects. Navigation surfaces that
/// display commit timestamps should use [`discover_with_metadata`] instead.
///
/// # Errors
///
/// Returns an error when Git cannot run or its worktree output is invalid.
pub fn discover(cwd: &Path) -> Result<Discovery> {
    RepositoryObservation::new(cwd).discover()
}

/// Discovers worktrees and enriches them with HEAD commit timestamps.
///
/// Metadata failures are systemic but non-fatal. They are returned as one
/// warning while every unavailable timestamp remains `None`.
///
/// # Errors
///
/// Returns an error when base Git discovery fails or its output is invalid.
pub fn discover_with_metadata(cwd: &Path) -> Result<Discovery> {
    RepositoryObservation::new(cwd).discover_with_metadata()
}

fn discover_worktrees(cwd: &Path) -> Result<Vec<Worktree>> {
    let output = run_git(cwd, ["worktree", "list", "--porcelain", "-z"])
        .context("failed to list Git worktrees for the current repository")?;
    ensure_success(&output, "git worktree list")?;

    let mut worktrees = parse_porcelain(&output.stdout)?;
    let current = current_record(&worktrees, cwd);
    for (index, worktree) in worktrees.iter_mut().enumerate() {
        worktree.current = current == Some(index);
        worktree.condition = inspect_condition(worktree);
    }
    Ok(worktrees)
}

/// Resolves repository-level worktree and common-directory context.
///
/// This base repository does not query commit objects. Navigation surfaces that
/// display commit timestamps should use [`repository_with_metadata`] instead.
///
/// # Errors
///
/// Returns an error when Git context or a required path cannot be resolved.
pub fn repository(cwd: &Path) -> Result<Repository> {
    RepositoryObservation::new(cwd).repository()
}

/// Resolves repository context and enriches worktrees with commit timestamps.
///
/// # Errors
///
/// Returns an error when base repository discovery or path resolution fails.
pub fn repository_with_metadata(cwd: &Path) -> Result<Repository> {
    RepositoryObservation::new(cwd).repository_with_metadata()
}

fn repository_from_worktrees(cwd: &Path, discovery: Discovery) -> Result<Repository> {
    let worktrees = discovery.worktrees;
    let current_index = worktrees
        .iter()
        .position(|worktree| worktree.current)
        .context("the current directory is not inside a registered Git worktree")?;
    let primary = worktrees
        .first()
        .filter(|worktree| !worktree.is_bare())
        .map(|worktree| canonical_or_normalized(&worktree.path))
        .transpose()?;
    let common = git_stdout(
        cwd,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .context("failed to resolve Git's common directory")?;
    let common_dir = canonical_or_normalized(Path::new(&common))?;
    Ok(Repository {
        worktrees,
        current_index,
        primary,
        common_dir,
        metadata_warning: discovery.metadata_warning,
    })
}

fn enrich_last_commit_at(cwd: &Path, worktrees: &mut [Worktree]) -> Result<()> {
    let heads: BTreeSet<_> = worktrees
        .iter()
        .filter_map(|worktree| worktree.head.as_deref())
        .filter_map(normalized_head)
        .collect();
    let resolved = resolve_commit_timestamps(cwd, &heads)?;
    for worktree in worktrees {
        worktree.last_commit_at = worktree
            .head
            .as_deref()
            .and_then(normalized_head)
            .and_then(|head| resolved.get(&head).copied());
    }
    Ok(())
}

/// Resolves committer timestamps for a batch of commit object ids in one `cat-file` call.
///
/// `oids` must already be normalized (lowercase, valid hex). An empty set resolves
/// without starting a subprocess.
fn resolve_commit_timestamps(
    cwd: &Path,
    oids: &BTreeSet<String>,
) -> Result<HashMap<String, DateTime<FixedOffset>>> {
    if oids.is_empty() {
        return Ok(HashMap::new());
    }

    let requests = oids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let output = GitProcess::new(cwd, ["cat-file", "--batch"]).piped(
        format!("{requests}\n").into_bytes(),
        &PipedContexts {
            start: "failed to start git cat-file --batch",
            open_input: "failed to open git cat-file input",
            write_input: "failed to write git cat-file input",
            writer_panicked: "git cat-file input writer panicked",
            await_output: "failed to read git cat-file output",
        },
    )?;
    ensure_success(&output, "git cat-file --batch")?;
    parse_commit_batch(&output.stdout, oids)
}

/// One local branch (`refs/heads`) as reported by `git for-each-ref`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchRecord {
    pub branch: String,
    pub head: String,
    pub last_commit_at: Option<DateTime<FixedOffset>>,
}

/// A repository's worktrees together with its local branches.
///
/// Both share committer timestamps resolved in a single `cat-file --batch` pass,
/// so building this costs exactly one additional Git subprocess (`for-each-ref`)
/// over [`repository_with_metadata`].
#[derive(Debug)]
pub struct RepositoryBranches {
    pub repository: Repository,
    pub branches: Vec<BranchRecord>,
}

/// Discovers the repository's worktrees and local branches together.
///
/// Only `refs/heads` is inspected; no fetch happens and no remote-tracking
/// refs are considered. A ref whose short name is not valid UTF-8 is excluded
/// and reported in the metadata warning rather than silently dropped.
///
/// # Errors
///
/// Returns an error when Git cannot run or its worktree or ref output is invalid.
pub fn repository_with_branches(cwd: &Path) -> Result<RepositoryBranches> {
    RepositoryObservation::new(cwd).repository_with_branches()
}

fn repository_with_branches_observed(cwd: &Path) -> Result<RepositoryBranches> {
    let mut worktrees = discover_worktrees(cwd)?;
    let (mut branches, non_utf8) = discover_branch_refs(cwd)?;

    let worktree_heads = worktrees
        .iter()
        .filter_map(|worktree| worktree.head.as_deref())
        .filter_map(normalized_head);
    let branch_heads = branches
        .iter()
        .filter_map(|branch| normalized_head(&branch.head));
    let all_heads: BTreeSet<_> = worktree_heads.chain(branch_heads).collect();

    let mut metadata_warning = match resolve_commit_timestamps(cwd, &all_heads) {
        Ok(resolved) => {
            for worktree in &mut worktrees {
                worktree.last_commit_at = worktree
                    .head
                    .as_deref()
                    .and_then(normalized_head)
                    .and_then(|head| resolved.get(&head).copied());
            }
            for branch in &mut branches {
                branch.last_commit_at =
                    normalized_head(&branch.head).and_then(|head| resolved.get(&head).copied());
            }
            None
        }
        Err(error) => Some(format!("failed to load last-commit metadata: {error:#}")),
    };
    if !non_utf8.is_empty() {
        let suffix = format!("excluded non-UTF-8 branch name(s): {}", non_utf8.join(", "));
        metadata_warning = Some(
            metadata_warning.map_or(suffix.clone(), |existing| format!("{existing}; {suffix}")),
        );
    }

    let repository = repository_from_worktrees(
        cwd,
        Discovery {
            worktrees,
            metadata_warning,
        },
    )?;
    Ok(RepositoryBranches {
        repository,
        branches,
    })
}

/// Discovers local branches without resolving commit timestamps.
///
/// # Errors
///
/// Returns an error when Git cannot list local refs.
pub fn discover_branches(cwd: &Path) -> Result<Vec<BranchRecord>> {
    RepositoryObservation::new(cwd).branches()
}

/// Lists remote-tracking branches as short names such as `origin/feature`.
///
/// Only `refs/remotes` is inspected; no fetch happens. Symbolic refs such as
/// `origin/HEAD` are excluded because they point at a branch rather than being
/// one. A ref whose short name is not valid UTF-8 is dropped.
///
/// # Errors
///
/// Returns an error when Git cannot run or fails to list remote refs.
pub fn discover_remote_branches(cwd: &Path) -> Result<Vec<String>> {
    RepositoryObservation::new(cwd).remote_branches()
}

fn discover_branch_refs(cwd: &Path) -> Result<(Vec<BranchRecord>, Vec<String>)> {
    let output = run_git(
        cwd,
        [
            "for-each-ref",
            "--format=%(objectname)%00%(refname:short)%00",
            "refs/heads",
        ],
    )
    .context("failed to list local branches")?;
    ensure_success(&output, "git for-each-ref")?;
    Ok(parse_branch_refs(&output.stdout))
}

/// Parses NUL-delimited `<objectname>\0<refname:short>\0` records.
fn parse_branch_refs(bytes: &[u8]) -> (Vec<BranchRecord>, Vec<String>) {
    let mut records = Vec::new();
    let mut excluded = Vec::new();
    let mut fields = bytes.split(|byte| *byte == 0);
    while let Some(raw_head) = fields.next() {
        let head_field = raw_head.strip_prefix(b"\n").unwrap_or(raw_head);
        if head_field.is_empty() {
            break;
        }
        let Some(raw_branch) = fields.next() else {
            break;
        };
        let branch_field = raw_branch.strip_prefix(b"\n").unwrap_or(raw_branch);
        let head = String::from_utf8_lossy(head_field).into_owned();
        match std::str::from_utf8(branch_field) {
            Ok(branch) => records.push(BranchRecord {
                branch: branch.to_owned(),
                head,
                last_commit_at: None,
            }),
            Err(_) => excluded.push(String::from_utf8_lossy(branch_field).into_owned()),
        }
    }
    (records, excluded)
}

/// Parses NUL-delimited `<refname:short>\0<symref>\0` records.
fn parse_remote_branch_refs(bytes: &[u8]) -> Vec<String> {
    let mut records = Vec::new();
    let mut fields = bytes.split(|byte| *byte == 0);
    while let Some(raw_name) = fields.next() {
        let name_field = raw_name.strip_prefix(b"\n").unwrap_or(raw_name);
        if name_field.is_empty() {
            break;
        }
        let Some(raw_symref) = fields.next() else {
            break;
        };
        let symref_field = raw_symref.strip_prefix(b"\n").unwrap_or(raw_symref);
        if !symref_field.is_empty() {
            continue;
        }
        if let Ok(name) = std::str::from_utf8(name_field) {
            records.push(name.to_owned());
        }
    }
    records
}

fn normalized_head(head: &str) -> Option<String> {
    let valid_length = matches!(head.len(), 40 | 64);
    (valid_length
        && head.bytes().all(|byte| byte.is_ascii_hexdigit())
        && head.bytes().any(|byte| byte != b'0'))
    .then(|| head.to_ascii_lowercase())
}

fn parse_commit_batch(
    bytes: &[u8],
    heads: &BTreeSet<String>,
) -> Result<HashMap<String, DateTime<FixedOffset>>> {
    let mut cursor = 0;
    let mut resolved = HashMap::new();
    for head in heads {
        let header_end = bytes[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| cursor + offset)
            .context("git cat-file returned an incomplete header")?;
        let header = std::str::from_utf8(&bytes[cursor..header_end])
            .context("git cat-file returned a non-UTF-8 header")?;
        cursor = header_end + 1;
        if header.ends_with(" missing") {
            continue;
        }
        let mut fields = header.split_whitespace();
        let _object = fields.next().context("git cat-file omitted an object id")?;
        let kind = fields
            .next()
            .context("git cat-file omitted an object type")?;
        let size: usize = fields
            .next()
            .context("git cat-file omitted an object size")?
            .parse()
            .context("git cat-file returned an invalid object size")?;
        if fields.next().is_some() || cursor + size >= bytes.len() {
            bail!("git cat-file returned malformed batch output");
        }
        let object = &bytes[cursor..cursor + size];
        cursor += size;
        if bytes.get(cursor) != Some(&b'\n') {
            bail!("git cat-file omitted an object separator");
        }
        cursor += 1;
        if kind == "commit"
            && let Some(timestamp) = parse_committer_timestamp(object)
        {
            resolved.insert(head.clone(), timestamp);
        }
    }
    Ok(resolved)
}

fn parse_committer_timestamp(commit: &[u8]) -> Option<DateTime<FixedOffset>> {
    let line = commit
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty())
        .find(|line| line.starts_with(b"committer "))?;
    let mut fields = line.rsplit(|byte| *byte == b' ');
    let timezone = fields.next()?;
    let seconds = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    if timezone.len() != 5 || !matches!(timezone[0], b'+' | b'-') {
        return None;
    }
    let hours: i32 = std::str::from_utf8(&timezone[1..3]).ok()?.parse().ok()?;
    let minutes: i32 = std::str::from_utf8(&timezone[3..5]).ok()?.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let sign = if timezone[0] == b'-' { -1 } else { 1 };
    let offset = FixedOffset::east_opt(sign * (hours * 60 + minutes) * 60)?;
    DateTime::from_timestamp(seconds, 0).map(|timestamp| timestamp.with_timezone(&offset))
}

/// Returns the current named branch.
///
/// # Errors
///
/// Returns an error for detached, bare, or unknown current worktree states.
/// Returns the configured origin URL, when one is available.
///
/// # Errors
/// Returns an error when origin is missing or Git cannot be invoked.
pub fn origin_url(cwd: &Path) -> Result<String> {
    git_stdout(cwd, ["remote", "get-url", "origin"])
}

/// Asks Git to validate a proposed branch name.
///
/// # Errors
///
/// Returns an error when Git cannot run or rejects the name.
pub(crate) fn validate_branch(cwd: &Path, branch: &str) -> Result<()> {
    if branch.is_empty() {
        bail!("branch name cannot be empty");
    }
    let output = run_git(cwd, ["check-ref-format", "--branch", branch])
        .context("failed to ask Git to validate the branch name")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "Git rejected branch name {branch:?}: {}",
            stderr_detail(&output)
        )
    }
}

/// Reports whether an exact local branch exists.
///
/// # Errors
///
/// Returns an error when Git cannot inspect local refs.
fn local_branch_exists(cwd: &Path, branch: &str) -> Result<bool> {
    let reference = format!("refs/heads/{branch}");
    let output = run_git(cwd, ["show-ref", "--verify", "--quiet", &reference])
        .context("failed to inspect local branches")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to inspect local branch {branch:?}: {}",
            stderr_detail(&output)
        ),
    }
}

/// Returns already-fetched remote-tracking refs matching a branch name.
///
/// # Errors
///
/// Returns an error when Git cannot inspect remote refs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PushPlan {
    pub(crate) remote: String,
    pub(crate) branch: String,
    pub(crate) set_upstream: bool,
}

pub(crate) fn branch_upstream(cwd: &Path, branch: &str) -> Result<Option<String>> {
    let output = run_git(
        cwd,
        [
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

pub(crate) fn configured_remotes(cwd: &Path) -> Result<Vec<String>> {
    let output = run_git(cwd, ["remote"])?;
    ensure_success(&output, "git remote")?;
    let mut remotes: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    remotes.sort();
    Ok(remotes)
}

/// Resolves the target branch, preserving an explicit configuration value.
/// Otherwise, uses the already-fetched `origin/HEAD` branch, then local `main`,
/// then local `master`.
///
/// # Errors
/// Returns an error when Git cannot inspect refs or no fallback branch exists.
pub(crate) fn resolve_target_branch(cwd: &Path, configured: Option<&str>) -> Result<String> {
    if let Some(branch) = configured {
        return Ok(branch.to_owned());
    }
    let origin_head = match origin_head_branch(cwd) {
        Some(branch) if local_branch_exists(cwd, &branch)? => Some(branch),
        _ => None,
    };
    let has_main = local_branch_exists(cwd, "main")?;
    let has_master = local_branch_exists(cwd, "master")?;
    fallback_target_branch(origin_head.as_deref(), has_main, has_master).map(str::to_owned).context(
        "no target branch is configured and no fallback branch exists; configure worktrees.target-branch or create main/master",
    )
}

fn fallback_target_branch(
    origin_head: Option<&str>,
    has_main: bool,
    has_master: bool,
) -> Option<&str> {
    origin_head
        .or(has_main.then_some("main"))
        .or(has_master.then_some("master"))
}

#[cfg(test)]
mod target_branch_tests {
    #[test]
    fn explicit_configuration_has_precedence() {
        assert_eq!(
            super::fallback_target_branch(Some("release"), true, true),
            Some("release")
        );
    }

    #[test]
    fn origin_head_is_first_fallback() {
        assert_eq!(
            super::fallback_target_branch(Some("origin-head"), true, true),
            Some("origin-head")
        );
    }

    #[test]
    fn main_is_second_fallback() {
        assert_eq!(
            super::fallback_target_branch(None, true, true),
            Some("main")
        );
    }

    #[test]
    fn master_is_last_fallback() {
        assert_eq!(
            super::fallback_target_branch(None, false, true),
            Some("master")
        );
    }
}

/// The remote-tracking ref a `fresh` new branch is cut from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BaseRef {
    /// The remote that owns the ref, always `origin`.
    pub(crate) remote: String,
    /// The target branch name on that remote.
    pub(crate) branch: String,
}

impl BaseRef {
    /// Returns the short remote-tracking ref name, for example `origin/main`.
    #[must_use]
    pub(crate) fn reference(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// Resolves the remote-tracking ref that `worktrees.base: fresh` branches from.
///
/// Reads only local refs: the configured target branch when set, otherwise the
/// branch named by the remote's `origin/HEAD` symbolic ref.
///
/// # Errors
/// Returns an error when neither source names a branch.
fn resolve_base_ref(cwd: &Path, configured_target: Option<&str>) -> Result<BaseRef> {
    let branch = match configured_target {
        Some(branch) => branch.to_owned(),
        None => origin_head_branch(cwd).context(
            "worktrees.base is 'fresh' but no base branch could be resolved: set worktrees.target-branch, or record the remote's default branch with 'git remote set-head origin -a'",
        )?,
    };
    Ok(BaseRef {
        remote: "origin".to_owned(),
        branch,
    })
}

fn origin_head_branch(cwd: &Path) -> Option<String> {
    git_stdout(cwd, ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .ok()
        .and_then(|value| {
            value
                .trim()
                .strip_prefix("refs/remotes/origin/")
                .map(str::to_owned)
        })
        .filter(|branch| !branch.is_empty())
}

/// The start point a genuinely new branch will be cut from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewBranchBase {
    /// The commit the branch starts at.
    pub(crate) commit: String,
    /// The remote-tracking ref the commit came from, absent in `head` mode.
    pub(crate) base_ref: Option<BaseRef>,
    /// Git's captured output from an explicit `--fetch`, when one ran.
    pub(crate) fetch_output: Option<String>,
}

/// Plans the start point for a genuinely new branch under the effective base mode.
///
/// This is the single place either interface resolves a start point, so human
/// `switch`/`create`, their dry runs, and the JSON variants cannot diverge.
/// `fetch` refreshes exactly the resolved base ref first; it is the caller's job
/// to reject it before reaching here when the mode is `head` or the branch is
/// not genuinely new.
///
/// # Errors
/// Returns an error when `HEAD`, the base ref, or its commit cannot be resolved.
pub(crate) fn plan_new_branch_base(
    cwd: &Path,
    mode: BaseMode,
    configured_target: Option<&str>,
    fetch: bool,
) -> Result<NewBranchBase> {
    if mode == BaseMode::Head {
        return Ok(NewBranchBase {
            commit: head_commit(cwd)?,
            base_ref: None,
            fetch_output: None,
        });
    }
    let base_ref = resolve_base_ref(cwd, configured_target)?;
    let fetch_output = fetch.then(|| fetch_base_ref(cwd, &base_ref)).transpose()?;
    Ok(NewBranchBase {
        commit: base_ref_commit(cwd, &base_ref)?,
        base_ref: Some(base_ref),
        fetch_output,
    })
}

/// Resolves the commit a fresh base ref points at, without fetching.
///
/// # Errors
/// Returns an error naming the fix when the ref has never been fetched.
pub(crate) fn base_ref_commit(cwd: &Path, base: &BaseRef) -> Result<String> {
    let reference = base.reference();
    git_stdout(
        cwd,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{reference}^{{commit}}"),
        ],
    )
    .with_context(|| {
        format!(
            "the base ref {reference:?} has not been fetched into this clone; run 'git fetch {} {}' or pass --fetch",
            base.remote, base.branch
        )
    })
}

/// Fetches exactly the one base ref, leaving every other remote-tracking ref alone.
///
/// # Errors
/// Returns an error, including Git's captured output, when the fetch fails.
fn fetch_base_ref(cwd: &Path, base: &BaseRef) -> Result<String> {
    let refspec = format!(
        "+refs/heads/{branch}:refs/remotes/{remote}/{branch}",
        branch = base.branch,
        remote = base.remote,
    );
    run_lifecycle_git(
        cwd,
        &["fetch", &base.remote, &refspec],
        &format!("fetch of {}", base.reference()),
        LifecycleOutput::Captured,
    )
}

/// Returns the configured URL for a named remote.
///
/// # Errors
/// Returns an error when the remote is missing or Git cannot read it.
pub(crate) fn remote_url(cwd: &Path, remote: &str) -> Result<String> {
    let output = run_git(cwd, ["remote", "get-url", remote])?;
    ensure_success(&output, "git remote get-url")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Publishes a branch with an ordinary fast-forward-safe push.
///
/// # Errors
/// Returns an error when Git rejects or cannot execute the push.
pub(crate) fn push(cwd: &Path, plan: &PushPlan, inherit: bool) -> Result<()> {
    let refspec = format!("{}:{}", plan.branch, plan.branch);
    if inherit {
        let status = GitProcess::new(cwd, ["push", "-u", &plan.remote, &refspec])
            .displayed()
            .context("failed to start git push")?;
        if status.success() {
            Ok(())
        } else {
            bail!("git push failed with {status}")
        }
    } else {
        let output = run_git(cwd, ["push", "-u", &plan.remote, &refspec])
            .context("failed to start git push")?;
        ensure_success(&output, "git push")
    }
}

/// Returns already-fetched remote-tracking refs matching a branch name.
///
/// # Errors
/// Returns an error when Git cannot inspect configured remotes or remote-tracking refs.
pub fn remote_matches(cwd: &Path, branch: &str) -> Result<Vec<String>> {
    let output = run_git(cwd, ["remote"]).context("failed to inspect configured remotes")?;
    ensure_success(&output, "git remote")?;
    let mut matches = Vec::new();
    for remote in String::from_utf8_lossy(&output.stdout).lines() {
        let short_name = format!("{remote}/{branch}");
        let reference = format!("refs/remotes/{short_name}");
        let output = run_git(cwd, ["show-ref", "--verify", "--quiet", &reference])
            .context("failed to inspect remote-tracking branches")?;
        match output.status.code() {
            Some(0) => matches.push(short_name),
            Some(1) => {}
            _ => bail!(
                "failed to inspect remote-tracking branch {reference:?}: {}",
                stderr_detail(&output)
            ),
        }
    }
    matches.sort();
    Ok(matches)
}

/// Resolves the stable Git administrative directory for one worktree.
///
/// # Errors
///
/// Returns an error when Git cannot resolve the worktree's administrative directory.
pub fn worktree_identity(cwd: &Path) -> Result<PathBuf> {
    let git_dir = git_stdout(cwd, ["rev-parse", "--path-format=absolute", "--git-dir"])
        .context("failed to resolve the worktree's Git administrative directory")?;
    canonical_or_normalized(Path::new(&git_dir))
}

/// Resolves the invoking worktree's committed `HEAD`.
///
/// # Errors
///
/// Returns an error when Git cannot resolve `HEAD`.
pub fn head_commit(cwd: &Path) -> Result<String> {
    HistoryObservation::new(cwd).head_commit()
}

fn head_commit_observed(cwd: &Path) -> Result<String> {
    git_stdout(cwd, ["rev-parse", "--verify", "HEAD"])
        .context("failed to resolve the invoking worktree's HEAD")
}

/// Resolves the commit a branch points at.
///
/// Unlike [`head_commit`] this never depends on what is checked out, so a
/// lifecycle running inside the primary worktree can still read its target.
///
/// # Errors
///
/// Returns an error when Git cannot resolve the branch.
pub fn branch_commit(cwd: &Path, branch: &str) -> Result<String> {
    HistoryObservation::new(cwd).commit(branch)
}

fn branch_commit_observed(cwd: &Path, branch: &str) -> Result<String> {
    git_stdout(
        cwd,
        ["rev-parse", "--verify", &format!("{branch}^{{commit}}")],
    )
    .with_context(|| format!("failed to resolve branch {branch:?}"))
}

/// Runs a lifecycle Git operation, capturing its output unless `inherit` is set.
///
/// Captured output is returned on success and folded into the error on failure,
/// so a caller rendering progress owns every line Git produced. A captured run
/// has no terminal to hand an editor, so `GIT_EDITOR` is neutralized — it
/// outranks `core.editor`, and an inherited `EDITOR=nvim` would otherwise leave
/// `rebase --continue` drawing a full-screen editor into a pipe. Continuation
/// therefore reuses the commit message Git already recorded.
fn run_lifecycle_git(
    cwd: &Path,
    args: &[&str],
    operation: &str,
    output: LifecycleOutput,
) -> Result<String> {
    if output == LifecycleOutput::Displayed {
        return run_git_inherit(cwd, args, operation).map(|()| String::new());
    }
    let output = GitProcess::new(cwd, args)
        .suppress_editor()
        .captured()
        .with_context(|| {
            format!(
                "failed to start git {operation} in repository {}",
                cwd.display()
            )
        })?;
    let transcript = combined_output(&output);
    if output.status.success() {
        return Ok(transcript);
    }
    if transcript.is_empty() {
        bail!(
            "git {operation} failed with {} in repository {}",
            output.status,
            cwd.display()
        );
    }
    bail!(
        "git {operation} failed with {} in repository {}\n{transcript}",
        output.status,
        cwd.display()
    );
}

/// Joins a captured command's streams in the order Git presents them.
///
/// Git redraws counters such as `Rebasing (1/3)` with carriage returns, which a
/// pipe preserves verbatim. Only each line's final revision survives, so the
/// transcript reads the way it would have on a terminal.
fn combined_output(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim_end(), stderr.trim_end()]
        .into_iter()
        .filter(|stream| !stream.is_empty())
        .flat_map(|stream| stream.lines())
        .map(|line| {
            let line = line.trim_end_matches('\r');
            line.rsplit('\r').next().unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reports whether `ancestor` is an ancestor of `descendant`.
///
/// # Errors
///
/// Returns an error when Git cannot inspect ancestry.
pub fn is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    HistoryObservation::new(cwd).is_ancestor(ancestor, descendant)
}

fn ancestry_observed(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = run_git(cwd, ["merge-base", "--is-ancestor", ancestor, descendant])
        .context("failed to inspect commit ancestry")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor failed: {}",
            stderr_detail(&output)
        ),
    }
}

fn staged_changes_present(cwd: &Path) -> Result<bool> {
    let output = run_git(cwd, ["diff", "--cached", "--quiet"])
        .context("failed to inspect staged changes")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => bail!(
            "git diff --cached --quiet failed: {}",
            stderr_detail(&output)
        ),
    }
}

fn staged_patch(cwd: &Path, target: Option<&str>) -> Result<String> {
    match target {
        Some(target) => git_stdout(
            cwd,
            [
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--cached",
                target,
            ],
        ),
        None => git_stdout(
            cwd,
            [
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--cached",
            ],
        ),
    }
}

fn staged_statistics(cwd: &Path, target: Option<&str>) -> Result<String> {
    match target {
        Some(target) => git_stdout(
            cwd,
            [
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--cached",
                "--stat",
                target,
            ],
        ),
        None => git_stdout(
            cwd,
            [
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--cached",
                "--stat",
            ],
        ),
    }
}

fn range_commit_count(cwd: &Path, ancestor: &str, descendant: &str) -> Result<usize> {
    let range = format!("{ancestor}..{descendant}");
    git_stdout(cwd, ["rev-list", "--count", &range])
        .with_context(|| format!("failed to count commits in {range}"))?
        .trim()
        .parse()
        .with_context(|| format!("git rev-list --count {range} returned a non-numeric count"))
}

fn range_commit_messages(cwd: &Path, ancestor: &str, descendant: &str) -> Result<Vec<String>> {
    let range = format!("{ancestor}..{descendant}");
    // %x00 terminates each record so a multi-line body cannot be mistaken for
    // a record boundary the way a newline separator would be.
    let raw = git_stdout(cwd, ["log", "--reverse", "--format=%B%x00", &range])
        .with_context(|| format!("failed to read commit messages in {range}"))?;
    Ok(raw
        .split('\0')
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_owned)
        .collect())
}

fn range_patch(
    cwd: &Path,
    ancestor: &str,
    descendant: &str,
    extra: Option<&str>,
) -> Result<String> {
    stable_diff(cwd, &format!("{ancestor}..{descendant}"), extra)
}

fn stable_diff(cwd: &Path, range: &str, extra: Option<&str>) -> Result<String> {
    let output = run_git(
        cwd,
        [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            extra.unwrap_or("--patch"),
            range,
        ],
    )
    .with_context(|| format!("failed to diff {range}"))?;
    if !output.status.success() {
        bail!("git diff {range} failed: {}", stderr_detail(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Moves the current branch to `commit` while leaving the index and worktree alone.
///
/// This is how a squash collapses history: the tree is already correct, so only
/// the branch pointer moves and every change becomes staged.
///
/// # Errors
///
/// Returns an error when Git cannot move the branch.
/// Creates a commit from the current index, returning Git's captured output.
///
/// Unlike [`commit`], the message arrives on stdin rather than the command
/// line, so an arbitrarily long generated body cannot run into `ARG_MAX`.
///
/// # Errors
///
/// Returns an error when Git cannot create the commit.
///
/// # Panics
///
/// Panics if the child's piped stdin is unavailable, which cannot happen for a
/// process spawned with `Stdio::piped`.
fn commit_message_stdin_impl(cwd: &Path, message: &str) -> Result<String> {
    let output = GitProcess::new(cwd, ["commit", "--file", "-"])
        .suppress_editor()
        .piped(
            message.as_bytes().to_vec(),
            &PipedContexts {
                start: "failed to start git commit",
                open_input: "failed to open git commit input",
                write_input: "failed to send the commit message to git commit",
                writer_panicked: "git commit input writer panicked",
                await_output: "failed to await git commit",
            },
        )?;
    let transcript = combined_output(&output);
    if output.status.success() {
        return Ok(transcript);
    }
    if transcript.is_empty() {
        bail!("git commit failed with {}", output.status);
    }
    bail!("git commit failed with {}\n{transcript}", output.status)
}

fn recent_subjects_observed(cwd: &Path) -> Result<Vec<String>> {
    let output =
        run_git(cwd, ["log", "-10", "--format=%s"]).context("failed to read recent commits")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect());
    }
    let verify = run_git(cwd, ["rev-parse", "--verify", "HEAD"])?;
    if !verify.status.success() {
        return Ok(Vec::new());
    }
    bail!("git log failed: {}", stderr_detail(&output))
}

/// Commits using the supplied message while capturing Git's status output.
///
/// # Errors
///
/// Returns an error when Git cannot create the commit.
fn commit_impl(cwd: &Path, message: &str) -> Result<()> {
    let output = GitProcess::new(cwd, ["commit", "-m", message])
        .captured_inheriting_stdin()
        .context("failed to start git commit")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("git commit failed: {}", stderr_detail(&output))
    }
}

fn status_observed(cwd: &Path) -> Result<StatusSnapshot> {
    let output = GitProcess::new(cwd, ["status", "--porcelain=v1", "-z"])
        .captured()
        .context("failed to start git status")?;
    if !output.status.success() {
        bail!("git status failed");
    }
    parse_status_porcelain(&output.stdout)
}

fn parse_status_porcelain(raw: &[u8]) -> Result<StatusSnapshot> {
    let mut records = raw.split(|byte| *byte == 0).peekable();
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            if records.peek().is_none() {
                break;
            }
            bail!("git status returned an empty porcelain record");
        }
        if record.len() < 4 || record[2] != b' ' {
            bail!("git status returned a malformed porcelain record");
        }
        let code = [record[0], record[1]];
        let original_path = if code.into_iter().any(|byte| matches!(byte, b'R' | b'C')) {
            let path = records
                .next()
                .filter(|path| !path.is_empty())
                .context("git status omitted the original path for a rename or copy")?;
            Some(OsString::from_vec(path.to_vec()))
        } else {
            None
        };
        entries.push(StatusEntry {
            code,
            path: OsString::from_vec(record[3..].to_vec()),
            original_path,
        });
    }
    Ok(StatusSnapshot { entries })
}

/// Reports whether the invoking worktree has staged, unstaged, or untracked changes.
///
/// # Errors
///
/// Returns an error when Git cannot inspect status.
pub fn is_dirty(cwd: &Path) -> Result<bool> {
    Ok(HistoryObservation::new(cwd).status()?.is_dirty())
}

/// Reports whether Git ignores an existing, untracked path.
///
/// # Errors
///
/// Returns an error when Git cannot inspect ignore rules.
pub fn is_ignored(cwd: &Path, path: &Path) -> Result<bool> {
    check_ignored(cwd, path, false)
}

/// Reports whether Git would ignore a path that may not exist yet.
///
/// # Errors
///
/// Returns an error when Git cannot inspect ignore rules.
pub fn would_be_ignored(cwd: &Path, path: &Path) -> Result<bool> {
    check_ignored(cwd, path, true)
}

fn check_ignored(cwd: &Path, path: &Path, no_index: bool) -> Result<bool> {
    let mut args = vec![OsStr::new("check-ignore"), OsStr::new("--quiet")];
    if no_index {
        args.push(OsStr::new("--no-index"));
    }
    args.push(path.as_os_str());
    let output = GitProcess::new(cwd, args)
        .captured()
        .context("failed to ask Git whether the path is ignored")?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git check-ignore failed for {}: {}",
            path.display(),
            stderr_detail(&output)
        ),
    }
}

fn run_worktree_add(cwd: &Path, args: Vec<&OsStr>, branch: &str, destination: &Path) -> Result<()> {
    let output = GitProcess::new(cwd, args).captured().with_context(|| {
        format!(
            "failed to start Git while creating worktree for {branch:?} at {}",
            destination.display()
        )
    })?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "Git failed to create worktree for {branch:?} at {}: {}",
            destination.display(),
            stderr_detail(&output)
        )
    }
}

fn open_stderr() -> Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .open("/dev/stderr")
        .map_err(Into::into)
}

fn run_git_inherit(cwd: &Path, args: &[&str], operation: &str) -> Result<()> {
    let status = GitProcess::new(cwd, args).displayed().with_context(|| {
        format!(
            "failed to start git {operation} in repository {}",
            cwd.display()
        )
    })?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "git {operation} failed with {status} in repository {}",
            cwd.display()
        )
    }
}

fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> Result<String> {
    let operation = format!("git {}", args.join(" "));
    let output = run_git(cwd, args).with_context(|| format!("failed to start {operation}"))?;
    ensure_success(&output, &operation)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn current_record(worktrees: &[Worktree], cwd: &Path) -> Option<usize> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    worktrees
        .iter()
        .enumerate()
        .filter_map(|(index, worktree)| {
            let path = worktree
                .path
                .canonicalize()
                .unwrap_or_else(|_| worktree.path.clone());
            cwd.starts_with(&path)
                .then(|| (index, path.components().count()))
        })
        .max_by_key(|(_, depth)| *depth)
        .map(|(index, _)| index)
}

fn inspect_condition(worktree: &Worktree) -> Condition {
    if worktree.is_bare() {
        return Condition::Clean;
    }
    match fs::metadata(&worktree.path) {
        Ok(metadata) if metadata.is_dir() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Condition::Missing,
        Ok(_) | Err(_) => return Condition::Inaccessible,
    }

    match run_git(
        &worktree.path,
        ["status", "--porcelain", "--untracked-files=normal"],
    ) {
        Ok(output) if output.status.success() && output.stdout.is_empty() => Condition::Clean,
        Ok(output) if output.status.success() => Condition::Dirty,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Condition::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Condition::Inaccessible
        }
        Ok(_) | Err(_) => Condition::Unknown,
    }
}

fn run_git<const N: usize>(cwd: &Path, args: [&str; N]) -> std::io::Result<Output> {
    GitProcess::new(cwd, args).captured()
}

fn stderr_detail(output: &Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        output.status.to_string()
    } else {
        detail
    }
}

fn ensure_success(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!("{operation} failed: {}", stderr_detail(output));
}

/// Canonicalizes the longest existing path prefix while preserving missing suffixes.
///
/// # Errors
///
/// Returns an error when no existing ancestor or canonical path can be resolved.
pub fn canonical_or_normalized(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path.canonicalize().map_err(Into::into);
    }
    let mut missing = Vec::<OsString>::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let name = ancestor
            .file_name()
            .context("configured path has no existing ancestor")?;
        missing.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .context("configured path has no existing ancestor")?;
    }
    let mut normalized = ancestor.canonicalize()?;
    for component in missing.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

/// Parses NUL-delimited `git worktree list --porcelain -z` output.
///
/// # Errors
///
/// Returns an error for malformed or empty Git output.
pub fn parse_porcelain(bytes: &[u8]) -> Result<Vec<Worktree>> {
    let mut records = Vec::new();
    let mut current: Option<Worktree> = None;

    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(Worktree {
                path: PathBuf::from(OsString::from_vec(path.to_vec())),
                head: None,
                last_commit_at: None,
                kind: WorktreeKind::Unknown,
                locked: None,
                prunable: None,
                current: false,
                condition: Condition::Unknown,
            });
            continue;
        }
        let Some(record) = current.as_mut() else {
            bail!("invalid Git worktree output: attribute before worktree record");
        };
        if let Some(value) = field.strip_prefix(b"HEAD ") {
            record.head = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            let branch = String::from_utf8_lossy(value);
            record.kind = WorktreeKind::Branch(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&branch)
                    .to_owned(),
            );
        } else if field == b"detached" {
            record.kind = WorktreeKind::Detached;
        } else if field == b"bare" {
            record.kind = WorktreeKind::Bare;
        } else if field == b"locked" {
            record.locked = Some(String::new());
        } else if let Some(value) = field.strip_prefix(b"locked ") {
            record.locked = Some(String::from_utf8_lossy(value).into_owned());
        } else if field == b"prunable" {
            record.prunable = Some(String::new());
        } else if let Some(value) = field.strip_prefix(b"prunable ") {
            record.prunable = Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    if let Some(record) = current {
        records.push(record);
    }
    if records.is_empty() {
        bail!("Git returned no worktree records");
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, os::unix::ffi::OsStrExt};

    use super::{
        parse_branch_refs, parse_commit_batch, parse_committer_timestamp, parse_porcelain,
        parse_remote_branch_refs, parse_status_porcelain,
    };
    use crate::WorktreeKind;

    #[test]
    fn parses_nul_delimited_branch_refs() {
        let input = b"aaaa\0feature/a\0\nbbbb\0main\0\n";

        let (records, excluded) = parse_branch_refs(input);

        assert!(excluded.is_empty());
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].branch, "feature/a");
        assert_eq!(records[0].head, "aaaa");
        assert_eq!(records[1].branch, "main");
        assert_eq!(records[1].head, "bbbb");
    }

    #[test]
    fn excludes_non_utf8_branch_names_without_dropping_them_silently() {
        let mut input = b"aaaa\0".to_vec();
        input.extend_from_slice(&[0xFF, 0xFE]);
        input.extend_from_slice(b"\0\n");

        let (records, excluded) = parse_branch_refs(&input);

        assert!(records.is_empty());
        assert_eq!(excluded.len(), 1);
    }

    #[test]
    fn status_snapshot_preserves_byte_paths_and_change_distinctions() {
        let mut input = b"M  staged\0 M unstaged\0?? untracked\0R  renamed\0original".to_vec();
        input.extend_from_slice(&[0xFF, 0]);

        let status = parse_status_porcelain(&input).unwrap();

        assert_eq!(status.entries.len(), 4);
        assert!(status.entries[0].is_staged());
        assert!(!status.entries[0].is_unstaged());
        assert!(status.entries[1].is_unstaged());
        assert!(status.entries[2].is_untracked());
        assert_eq!(status.entries[3].path.as_bytes(), b"renamed");
        assert_eq!(
            status.entries[3]
                .original_path
                .as_deref()
                .unwrap()
                .as_bytes(),
            b"original\xff"
        );
    }

    #[test]
    fn status_snapshot_rejects_a_rename_without_its_original_path() {
        let error = parse_status_porcelain(b"R  renamed\0").unwrap_err();

        assert!(error.to_string().contains("omitted the original path"));
    }

    #[test]
    fn parses_optional_porcelain_attributes() {
        let input = b"worktree /repo\0HEAD abc\0branch refs/heads/main\0locked reason\0\0worktree /bare\0bare\0prunable\0\0";
        let records = parse_porcelain(input).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, WorktreeKind::Branch("main".to_owned()));
        assert_eq!(records[0].locked.as_deref(), Some("reason"));
        assert_eq!(records[1].kind, WorktreeKind::Bare);
        assert_eq!(records[1].prunable.as_deref(), Some(""));
    }

    #[test]
    fn parses_committer_timestamp_instead_of_author_timestamp() {
        let commit = b"tree abc\nauthor Test <test@example.com> 1 +0000\ncommitter Test <test@example.com> 1704164645 -0500\n\nmessage\n";
        let timestamp = parse_committer_timestamp(commit).unwrap();

        assert_eq!(timestamp.to_rfc3339(), "2024-01-01T22:04:05-05:00");
    }

    #[test]
    fn ignores_committer_like_text_outside_commit_headers() {
        let commit = b"tree abc\nauthor Test <test@example.com> 1 +0000\n\ncommitter Test <test@example.com> 1704164645 -0500\n";

        assert_eq!(parse_committer_timestamp(commit), None);
    }

    #[test]
    fn batch_parser_keeps_missing_objects_unresolved() {
        let missing = "1111111111111111111111111111111111111111".to_owned();
        let commit_id = "2222222222222222222222222222222222222222".to_owned();
        let commit = b"tree abc\ncommitter Test <test@example.com> 1704164645 +0000\n\nmessage\n";
        let output = [
            format!("{missing} missing\n").into_bytes(),
            format!("{commit_id} commit {}\n", commit.len()).into_bytes(),
            commit.to_vec(),
            b"\n".to_vec(),
        ]
        .concat();
        let heads = BTreeSet::from([missing.clone(), commit_id.clone()]);

        let parsed = parse_commit_batch(&output, &heads).unwrap();

        assert!(!parsed.contains_key(&missing));
        assert_eq!(parsed[&commit_id].to_rfc3339(), "2024-01-02T03:04:05+00:00");
    }

    #[test]
    fn remote_branch_refs_are_parsed_and_symbolic_refs_excluded() {
        // `<refname:short>\0<symref>\0` per record. `origin/HEAD` carries a symref
        // target and must be excluded: it is a pointer, not a branch.
        let bytes =
            b"origin/main\x00\x00origin/HEAD\x00refs/remotes/origin/main\x00upstream/dev\x00\x00";

        assert_eq!(
            parse_remote_branch_refs(bytes),
            vec!["origin/main".to_owned(), "upstream/dev".to_owned()]
        );
    }

    #[test]
    fn non_utf8_remote_branch_refs_are_dropped() {
        let bytes = b"origin/good\x00\x00origin/ba\xffd\x00\x00";

        assert_eq!(
            parse_remote_branch_refs(bytes),
            vec!["origin/good".to_owned()]
        );
    }
}
