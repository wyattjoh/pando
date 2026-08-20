use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    config::{self, EffectiveConfig, EffectiveGeneration, GenerationSource, HookPhase, HookStep},
    git::Repository,
    hash, hook_approval,
    protocol::{BytePath, Effect, ErrorBody, MutationClass, RecoveryAction, RecoveryInvocation},
};

/// A stable trust command leaf owned by the trust domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    HooksStatus,
    HooksReset,
    CommitStatus,
    CommitReset,
    CommitApprove,
    PrStatus,
    PrReset,
    PrApprove,
    MergeStatus,
    MergeReset,
    MergeApprove,
}

impl Command {
    /// Returns the version 1 protocol command identifier.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::HooksStatus => "trust.status",
            Self::HooksReset => "trust.reset",
            Self::CommitStatus => "trust.commit_status",
            Self::CommitReset => "trust.commit_reset",
            Self::CommitApprove => "trust.commit_approve",
            Self::PrStatus => "trust.pr_status",
            Self::PrReset => "trust.pr_reset",
            Self::PrApprove => "trust.pr_approve",
            Self::MergeStatus => "trust.merge_status",
            Self::MergeReset => "trust.merge_reset",
            Self::MergeApprove => "trust.merge_approve",
        }
    }

    /// Resolves a published version 1 protocol command identifier.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "trust.status" => Some(Self::HooksStatus),
            "trust.reset" => Some(Self::HooksReset),
            "trust.commit_status" => Some(Self::CommitStatus),
            "trust.commit_reset" => Some(Self::CommitReset),
            "trust.commit_approve" => Some(Self::CommitApprove),
            "trust.pr_status" => Some(Self::PrStatus),
            "trust.pr_reset" => Some(Self::PrReset),
            "trust.pr_approve" => Some(Self::PrApprove),
            "trust.merge_status" => Some(Self::MergeStatus),
            "trust.merge_reset" => Some(Self::MergeReset),
            "trust.merge_approve" => Some(Self::MergeApprove),
            _ => None,
        }
    }

    /// Whether this leaf supports version 1 structured execution.
    #[must_use]
    pub const fn supports_structured(self) -> bool {
        !matches!(self, Self::PrStatus | Self::PrReset | Self::PrApprove)
    }

    /// Returns the stable version 1 error catalog for this leaf.
    #[must_use]
    pub const fn errors(self) -> &'static [&'static str] {
        match self {
            Self::PrStatus | Self::PrReset | Self::PrApprove => &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
                "trust.json_unsupported",
            ],
            Self::HooksReset | Self::CommitReset | Self::MergeReset => &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
                "trust.busy",
            ],
            Self::CommitApprove | Self::MergeApprove => &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
                "trust.approval_required",
            ],
            _ => &[
                "json.invalid_request",
                "json.unsupported_schema_version",
                "repository.invalid",
            ],
        }
    }

    /// Returns the stable version 1 action catalog for this leaf.
    #[must_use]
    pub const fn actions(self) -> &'static [&'static str] {
        match self {
            Self::HooksReset => &["trust.reset"],
            Self::CommitReset => &["trust.commit_reset"],
            Self::CommitApprove => &["trust.approve_commit_generator"],
            Self::MergeReset => &["trust.merge_reset"],
            Self::MergeApprove => &["trust.approve_merge_generator"],
            _ => &[],
        }
    }
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct HookStatus {
    pub phase: &'static str,
    pub configured: bool,
    pub trusted: bool,
    pub step_count: usize,
    pub source: HookSource,
    pub identity: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct HookSource {
    pub kind: &'static str,
    pub repository: BytePath,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct Candidate {
    pub command: Option<String>,
    pub template: Option<String>,
    pub identity: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Success {
    Status {
        phases: Vec<HookStatus>,
    },
    GeneratorStatus {
        state: &'static str,
        identity: Option<String>,
        source: Option<String>,
    },
    Reset,
    AlreadyReset,
    DryRun {
        #[serde(skip_serializing_if = "Option::is_none")]
        candidate: Option<Candidate>,
    },
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

impl From<Failure> for ErrorBody {
    fn from(value: Failure) -> Self {
        Self {
            code: value.code.into(),
            message: value.message,
        }
    }
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(untagged)]
pub enum ContextBody {
    Empty {},
    Candidate { candidate: Candidate },
}

/// One command-owned trust result, ready for either presentation adapter.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub result: std::result::Result<Success, Failure>,
    pub context: ContextBody,
    pub effects: Vec<Effect>,
    pub recovery: Vec<RecoveryAction<()>>,
}

impl Outcome {
    fn success(result: Success, effects: Vec<Effect>) -> Self {
        Self {
            result: Ok(result),
            context: ContextBody::Empty {},
            effects,
            recovery: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct StoreBusy;

impl std::fmt::Display for StoreBusy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("another trust update is already in progress; retry after it finishes")
    }
}

impl std::error::Error for StoreBusy {}

/// Reports whether an error is bounded trust-store contention.
#[must_use]
pub(crate) fn is_store_busy(error: &anyhow::Error) -> bool {
    error.chain().any(<dyn std::error::Error>::is::<StoreBusy>)
}

/// Executes a noninteractive trust leaf and returns domain-owned protocol data.
///
/// Approval leaves never persist approval. They return a preview for dry runs and
/// an approval-required failure otherwise. The human adapter must gather consent
/// before calling the explicit approval persistence functions.
///
/// # Errors
/// Returns an error when configuration or trust storage cannot be inspected or updated.
#[allow(clippy::too_many_lines)]
pub fn execute(repository: &Repository, command: Command, dry_run: bool) -> Result<Outcome> {
    let mutation = |result| {
        Outcome::success(
            result,
            vec![Effect {
                action: command.id().into(),
                attempted: !dry_run,
                completed: !dry_run,
                details: None,
            }],
        )
    };
    match command {
        Command::HooksStatus => {
            let config = EffectiveConfig::load(repository)?;
            let phases = HookPhase::all()
                .iter()
                .map(|phase| {
                    let steps = config.hooks(*phase);
                    let (trusted, identity) =
                        match hook_approval::evaluate(repository, *phase, steps)? {
                            hook_approval::Evaluation::NoCommands => (false, None),
                            hook_approval::Evaluation::Trusted { identity } => {
                                (true, Some(identity))
                            }
                            hook_approval::Evaluation::ApprovalRequired(candidate) => {
                                (false, Some(candidate.identity().to_owned()))
                            }
                        };
                    Ok(HookStatus {
                        phase: phase.key(),
                        configured: !steps.is_empty(),
                        trusted,
                        step_count: steps.len(),
                        source: HookSource {
                            kind: "effective",
                            repository: BytePath::path(&repository.current().path),
                        },
                        identity,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Outcome::success(Success::Status { phases }, Vec::new()))
        }
        Command::HooksReset => {
            let changed = !dry_run && reset(repository)?;
            Ok(mutation(reset_result(changed, dry_run)))
        }
        Command::CommitStatus | Command::PrStatus | Command::MergeStatus => {
            let config = EffectiveConfig::load(repository)?;
            let generation = match command {
                Command::CommitStatus => &config.generation,
                Command::PrStatus => &config.pr_generation,
                Command::MergeStatus => &config.merge_generation,
                _ => unreachable!(),
            };
            let identity = match command {
                Command::CommitStatus => generation_hash(generation),
                Command::PrStatus => generation_hash_named(generation, b"pando-pr-generation-v1"),
                Command::MergeStatus => merge_generation_hash(generation),
                _ => unreachable!(),
            };
            let trusted = match command {
                Command::CommitStatus => is_generation_trusted(repository, generation)?,
                Command::PrStatus => is_pr_generation_trusted(repository, generation)?,
                Command::MergeStatus => is_merge_generation_trusted(repository, generation)?,
                _ => unreachable!(),
            };
            let state = if generation.command.is_none() {
                "absent"
            } else if identity.is_none() {
                "user_controlled"
            } else if trusted {
                "trusted_shared"
            } else {
                "untrusted_shared"
            };
            let source = generation
                .command
                .as_ref()
                .map(|value| format!("{:?}", value.source).to_lowercase());
            Ok(Outcome::success(
                Success::GeneratorStatus {
                    state,
                    identity,
                    source,
                },
                Vec::new(),
            ))
        }
        Command::CommitReset | Command::PrReset | Command::MergeReset => {
            let changed = if dry_run {
                false
            } else {
                match command {
                    Command::CommitReset => reset_generation(repository)?,
                    Command::PrReset => reset_pr_generation(repository)?,
                    Command::MergeReset => reset_merge_generation(repository)?,
                    _ => unreachable!(),
                }
            };
            Ok(mutation(reset_result(changed, dry_run)))
        }
        Command::CommitApprove | Command::PrApprove | Command::MergeApprove => {
            let config = EffectiveConfig::load(repository)?;
            let generation = match command {
                Command::CommitApprove => &config.generation,
                Command::PrApprove => &config.pr_generation,
                Command::MergeApprove => &config.merge_generation,
                _ => unreachable!(),
            };
            let identity = match command {
                Command::CommitApprove => generation_hash(generation),
                Command::PrApprove => generation_hash_named(generation, b"pando-pr-generation-v1"),
                Command::MergeApprove => merge_generation_hash(generation),
                _ => unreachable!(),
            };
            let candidate = Candidate {
                command: generation.command.as_ref().map(|value| value.value.clone()),
                template: generation
                    .template
                    .as_ref()
                    .map(|value| value.value.clone()),
                identity,
            };
            if dry_run {
                return Ok(Outcome::success(
                    Success::DryRun {
                        candidate: Some(candidate),
                    },
                    Vec::new(),
                ));
            }
            let action = match command {
                Command::CommitApprove => "trust.approve_commit_generator",
                Command::PrApprove => "trust.approve_pr_generator",
                Command::MergeApprove => "trust.approve_merge_generator",
                _ => unreachable!(),
            };
            Ok(Outcome {
                result: Err(Failure {
                    code: "trust.approval_required",
                    message: "approval requires a manual human invocation".into(),
                }),
                context: ContextBody::Candidate { candidate },
                effects: Vec::new(),
                recovery: vec![RecoveryAction {
                    action: action.into(),
                    description: "Review these settings and approve interactively".into(),
                    mutation: MutationClass::Trust,
                    requires_human_approval: true,
                    invocation: RecoveryInvocation {
                        argv: vec!["pando".into(), "trust".into(), human_leaf(command).into()],
                        stdin: None,
                        working_directory: Some(BytePath::path(&repository.current().path)),
                    },
                }],
            })
        }
    }
}

const fn human_leaf(command: Command) -> &'static str {
    match command {
        Command::CommitApprove => "commit-approve",
        Command::PrApprove => "pr-approve",
        Command::MergeApprove => "merge-approve",
        _ => unreachable!(),
    }
}

fn reset_result(changed: bool, dry_run: bool) -> Success {
    if dry_run {
        Success::DryRun { candidate: None }
    } else if changed {
        Success::Reset
    } else {
        Success::AlreadyReset
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    #[serde(default)]
    repositories: BTreeMap<String, TrustRecord>,
    #[serde(default)]
    commit_generators: BTreeMap<String, String>,
    #[serde(default)]
    pr_generators: BTreeMap<String, String>,
    #[serde(default)]
    merge_generators: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum TrustRecord {
    /// Legacy records approved only post-create commands.
    Legacy(String),
    Phases(PhaseApprovals),
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PhaseApprovals {
    #[serde(default, rename = "post-create")]
    post_create: Option<String>,
    #[serde(default, rename = "pre-merge")]
    pre_merge: Option<String>,
    #[serde(default, rename = "pre-remove")]
    pre_remove: Option<String>,
}

impl PhaseApprovals {
    fn get(&self, phase: HookPhase) -> Option<&String> {
        match phase {
            HookPhase::PostCreate => self.post_create.as_ref(),
            HookPhase::PreMerge => self.pre_merge.as_ref(),
            HookPhase::PreRemove => self.pre_remove.as_ref(),
        }
    }

    fn set(&mut self, phase: HookPhase, hash: String) {
        match phase {
            HookPhase::PostCreate => self.post_create = Some(hash),
            HookPhase::PreMerge => self.pre_merge = Some(hash),
            HookPhase::PreRemove => self.pre_remove = Some(hash),
        }
    }
}

/// Returns the deterministic executable identity of an ordered command list for one phase.
#[must_use]
pub(crate) fn command_hash(phase: HookPhase, steps: &[HookStep]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pando-hook-phase-v1\0");
    digest.update(phase.key().as_bytes());
    digest.update(b"\0");
    for step in steps {
        let bytes = step.command.as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    hash::encode_hex(&digest.finalize())
}

fn legacy_post_create_hash(steps: &[HookStep]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"pando-post-create-v1\0");
    for step in steps {
        let bytes = step.command.as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    hash::encode_hex(&digest.finalize())
}

/// Reports whether the ordered commands are trusted for this clone and phase.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be resolved.
pub(crate) fn is_trusted(
    repository: &Repository,
    phase: HookPhase,
    steps: &[HookStep],
) -> Result<bool> {
    if steps.is_empty() {
        return Ok(true);
    }
    let identity = repository_key(repository)?;
    let trust = read_trust()?;
    let approved = match trust.repositories.get(&identity) {
        Some(TrustRecord::Legacy(hash)) if phase == HookPhase::PostCreate => {
            hash == &legacy_post_create_hash(steps)
        }
        Some(TrustRecord::Phases(approvals)) => {
            approvals.get(phase) == Some(&command_hash(phase, steps))
        }
        _ => false,
    };
    Ok(approved)
}

/// Atomically saves approval for one ordered phase plan.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be updated.
pub(crate) fn approve(repository: &Repository, phase: HookPhase, steps: &[HookStep]) -> Result<()> {
    let identity = repository_key(repository)?;
    let approved_hash = command_hash(phase, steps);
    update_trust(|trust| {
        let approvals = match trust.repositories.remove(&identity) {
            Some(TrustRecord::Legacy(post_create)) => PhaseApprovals {
                post_create: Some(post_create),
                ..PhaseApprovals::default()
            },
            Some(TrustRecord::Phases(approvals)) => approvals,
            None => PhaseApprovals::default(),
        };
        let mut approvals = approvals;
        approvals.set(phase, approved_hash);
        trust
            .repositories
            .insert(identity, TrustRecord::Phases(approvals));
        ((), true)
    })
}

/// Returns the approval identity for effective shared generation fields.
#[must_use]
pub fn generation_hash(generation: &EffectiveGeneration) -> Option<String> {
    let mut digest = Sha256::new();
    digest.update(b"pando-commit-generation-v1\0");
    let mut has_shared = false;
    for (name, value) in [
        (b"command".as_slice(), generation.command.as_ref()),
        (b"template".as_slice(), generation.template.as_ref()),
    ] {
        if let Some(value) = value.filter(|value| value.source == GenerationSource::Shared) {
            has_shared = true;
            digest.update((name.len() as u64).to_be_bytes());
            digest.update(name);
            digest.update((value.value.len() as u64).to_be_bytes());
            digest.update(value.value.as_bytes());
        }
    }
    has_shared.then(|| hash::encode_hex(&digest.finalize()))
}

/// Reports whether the effective shared generator values are approved for this clone.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be resolved.
pub fn is_generation_trusted(
    repository: &Repository,
    generation: &EffectiveGeneration,
) -> Result<bool> {
    let Some(hash) = generation_hash(generation) else {
        return Ok(true);
    };
    Ok(read_trust()?
        .commit_generators
        .get(&repository_key(repository)?)
        == Some(&hash))
}

/// Saves approval for effective shared generator values.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be updated.
pub fn approve_generation(repository: &Repository, generation: &EffectiveGeneration) -> Result<()> {
    let Some(hash) = generation_hash(generation) else {
        return Ok(());
    };
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        trust.commit_generators.insert(identity, hash);
        ((), true)
    })
}

/// Removes generator approval for this clone and reports whether one existed.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be updated.
pub fn is_pr_generation_trusted(
    repository: &Repository,
    generation: &EffectiveGeneration,
) -> Result<bool> {
    let Some(hash) = generation_hash_named(generation, b"pando-pr-generation-v1") else {
        return Ok(true);
    };
    Ok(read_trust()?
        .pr_generators
        .get(&repository_key(repository)?)
        == Some(&hash))
}

/// Approves shared PR generation settings.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn approve_pr_generation(
    repository: &Repository,
    generation: &EffectiveGeneration,
) -> Result<()> {
    let Some(hash) = generation_hash_named(generation, b"pando-pr-generation-v1") else {
        return Ok(());
    };
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        trust.pr_generators.insert(identity, hash);
        ((), true)
    })
}

/// Resets PR generator approval.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn reset_pr_generation(repository: &Repository) -> Result<bool> {
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        let removed = trust.pr_generators.remove(&identity).is_some();
        (removed, removed)
    })
}

/// Returns the approval identity for the effective shared squash-message generator.
///
/// `None` means every effective value is user-controlled, so no approval applies.
#[must_use]
pub fn merge_generation_hash(generation: &EffectiveGeneration) -> Option<String> {
    generation_hash_named(generation, b"pando-merge-generation-v1")
}

/// Reports whether the effective shared squash-message generator is approved.
///
/// # Errors
/// Returns an error when repository identity or trust storage cannot be resolved.
pub fn is_merge_generation_trusted(
    repository: &Repository,
    generation: &EffectiveGeneration,
) -> Result<bool> {
    let Some(hash) = merge_generation_hash(generation) else {
        return Ok(true);
    };
    Ok(read_trust()?
        .merge_generators
        .get(&repository_key(repository)?)
        == Some(&hash))
}

/// Approves shared squash-message generation settings.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn approve_merge_generation(
    repository: &Repository,
    generation: &EffectiveGeneration,
) -> Result<()> {
    let Some(hash) = merge_generation_hash(generation) else {
        return Ok(());
    };
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        trust.merge_generators.insert(identity, hash);
        ((), true)
    })
}

/// Resets squash-message generator approval.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn reset_merge_generation(repository: &Repository) -> Result<bool> {
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        let removed = trust.merge_generators.remove(&identity).is_some();
        (removed, removed)
    })
}

fn generation_hash_named(generation: &EffectiveGeneration, domain: &[u8]) -> Option<String> {
    let mut digest = Sha256::new();
    digest.update(domain);
    let mut shared = false;
    for (name, value) in [
        (b"command".as_slice(), generation.command.as_ref()),
        (b"template".as_slice(), generation.template.as_ref()),
    ] {
        if let Some(value) = value.filter(|value| value.source == GenerationSource::Shared) {
            shared = true;
            digest.update((name.len() as u64).to_be_bytes());
            digest.update(name);
            digest.update((value.value.len() as u64).to_be_bytes());
            digest.update(value.value.as_bytes());
        }
    }
    shared.then(|| hash::encode_hex(&digest.finalize()))
}

/// Resets commit generator approval.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn reset_generation(repository: &Repository) -> Result<bool> {
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        let removed = trust.commit_generators.remove(&identity).is_some();
        (removed, removed)
    })
}

/// Removes this clone's phase approvals and reports whether any existed.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be updated.
pub fn reset(repository: &Repository) -> Result<bool> {
    let identity = repository_key(repository)?;
    update_trust(|trust| {
        let removed = trust.repositories.remove(&identity).is_some();
        (removed, removed)
    })
}

pub(crate) fn repository_key(repository: &Repository) -> Result<String> {
    let path = repository.identity()?;
    let mut key = String::with_capacity(4 + path.as_os_str().as_bytes().len() * 2);
    key.push_str("hex:");
    for byte in path.as_os_str().as_bytes() {
        use std::fmt::Write as _;
        write!(&mut key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(key)
}

fn trust_path() -> Result<PathBuf> {
    Ok(config::config_home()?.join("pando/trust.json"))
}

fn read_trust() -> Result<TrustFile> {
    read_trust_at(&trust_path()?)
}

fn read_trust_at(path: &Path) -> Result<TrustFile> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TrustFile::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read trust storage {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse trust storage {}", path.display()))
}

const TRUST_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const TRUST_LOCK_RETRY: Duration = Duration::from_millis(10);

fn update_trust<T>(update: impl FnOnce(&mut TrustFile) -> (T, bool)) -> Result<T> {
    let path = trust_path()?;
    update_trust_at(&path, TRUST_LOCK_TIMEOUT, update)
}

fn update_trust_at<T>(
    path: &Path,
    timeout: Duration,
    update: impl FnOnce(&mut TrustFile) -> (T, bool),
) -> Result<T> {
    let parent = path
        .parent()
        .context("trust path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let lock_path = path.with_extension("lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open trust-store lock {}", lock_path.display()))?;
    let started = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= timeout {
                    return Err(StoreBusy.into());
                }
                thread::sleep(TRUST_LOCK_RETRY.min(timeout.saturating_sub(started.elapsed())));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to acquire trust-store lock {}", lock_path.display())
                });
            }
        }
    }
    let mut trust = read_trust_at(path)?;
    let (result, changed) = update(&mut trust);
    if changed {
        let bytes = serde_json::to_vec_pretty(&trust).context("failed to encode trust storage")?;
        write_atomic(path, &bytes)?;
    }
    Ok(result)
}

/// Atomically replaces a state file beside its destination and syncs the
/// directory entries changed by the rename.
///
/// # Errors
///
/// Returns an error when the parent, temporary file, destination, or changed
/// parent directory cannot be written or synchronized.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    write_atomic_with(path, content, sync_directory)
}

fn write_atomic_with(
    path: &Path,
    content: &[u8],
    sync: impl FnMut(&Path) -> io::Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .context("state path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    for attempt in 0..100_u8 {
        let temporary = parent.join(format!(".pando.tmp.{}.{}", std::process::id(), attempt));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        };
        let result = (|| -> Result<()> {
            file.write_all(content)?;
            file.sync_all()?;
            drop(file);
            rename_durable_with(&temporary, path, sync)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result.with_context(|| format!("failed to atomically update {}", path.display()));
    }
    bail!(
        "could not allocate a temporary state file beside {}",
        path.display()
    )
}

/// Renames one state record and durably commits every changed directory entry.
pub(crate) fn rename_durable(source: &Path, destination: &Path) -> Result<()> {
    rename_durable_with(source, destination, sync_directory)
}

fn rename_durable_with(
    source: &Path,
    destination: &Path,
    mut sync: impl FnMut(&Path) -> io::Result<()>,
) -> Result<()> {
    let source_parent = source
        .parent()
        .context("state source has no parent directory")?;
    let destination_parent = destination
        .parent()
        .context("state destination has no parent directory")?;
    fs::rename(source, destination).with_context(|| {
        format!(
            "failed to rename state from {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    sync(destination_parent).with_context(|| {
        format!(
            "failed to synchronize state directory {}",
            destination_parent.display()
        )
    })?;
    if source_parent != destination_parent {
        sync(source_parent).with_context(|| {
            format!(
                "failed to synchronize state directory {}",
                source_parent.display()
            )
        })?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use tempfile::tempdir;

    use super::*;
    use crate::config::{HookPhase, HookStep};

    #[test]
    fn legacy_records_preserve_post_create_approval() {
        let steps = vec![HookStep {
            command: "make".into(),
            name: None,
        }];
        let record = TrustRecord::Legacy(legacy_post_create_hash(&steps));
        assert!(matches!(record, TrustRecord::Legacy(_)));
        assert_ne!(
            legacy_post_create_hash(&steps),
            super::command_hash(HookPhase::PostCreate, &steps)
        );
    }

    #[test]
    fn atomic_replacement_syncs_after_the_rename_and_reports_sync_failure() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("state/value.json");
        let parent = path.parent().expect("state parent").to_path_buf();
        let error = write_atomic_with(&path, b"complete", |observed| {
            assert_eq!(observed, parent);
            assert_eq!(fs::read(&path)?, b"complete");
            assert!(fs::read_dir(&parent)?.all(|entry| {
                !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pando.tmp.")
            }));
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected directory sync failure",
            ))
        })
        .unwrap_err();

        assert!(format!("{error:#}").contains("failed to synchronize state directory"));
        assert_eq!(fs::read(path)?, b"complete");
        Ok(())
    }

    #[test]
    fn trust_transactions_preserve_concurrent_updates() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("pando/trust.json");
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_path = path.clone();
        let first = thread::spawn(move || {
            update_trust_at(&first_path, Duration::from_secs(2), |trust| {
                locked_tx.send(()).expect("announce acquired lock");
                release_rx.recv().expect("release first transaction");
                trust.commit_generators.insert("first".into(), "one".into());
                ((), true)
            })
        });
        locked_rx.recv()?;

        let (started_tx, started_rx) = mpsc::channel();
        let second_path = path.clone();
        let second = thread::spawn(move || {
            started_tx.send(()).expect("announce second transaction");
            update_trust_at(&second_path, Duration::from_secs(2), |trust| {
                trust
                    .commit_generators
                    .insert("second".into(), "two".into());
                ((), true)
            })
        });
        started_rx.recv()?;
        release_tx.send(())?;
        first.join().expect("first transaction")?;
        second.join().expect("second transaction")?;

        let trust = read_trust_at(&path)?;
        assert_eq!(
            trust.commit_generators.get("first").map(String::as_str),
            Some("one")
        );
        assert_eq!(
            trust.commit_generators.get("second").map(String::as_str),
            Some("two")
        );
        Ok(())
    }

    #[test]
    fn concurrent_reset_and_approval_are_serializable() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("pando/trust.json");
        update_trust_at(&path, Duration::from_secs(2), |trust| {
            trust.commit_generators.insert("old".into(), "hash".into());
            ((), true)
        })?;

        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let reset_path = path.clone();
        let reset = thread::spawn(move || {
            update_trust_at(&reset_path, Duration::from_secs(2), |trust| {
                locked_tx.send(()).expect("announce acquired reset lock");
                release_rx.recv().expect("release reset transaction");
                let removed = trust.commit_generators.remove("old").is_some();
                (removed, removed)
            })
        });
        locked_rx.recv()?;

        let approval_path = path.clone();
        let approval = thread::spawn(move || {
            update_trust_at(&approval_path, Duration::from_secs(2), |trust| {
                trust.commit_generators.insert("new".into(), "hash".into());
                ((), true)
            })
        });
        release_tx.send(())?;
        assert!(reset.join().expect("reset transaction")?);
        approval.join().expect("approval transaction")?;

        let trust = read_trust_at(&path)?;
        assert!(!trust.commit_generators.contains_key("old"));
        assert_eq!(
            trust.commit_generators.get("new").map(String::as_str),
            Some("hash")
        );
        Ok(())
    }

    #[test]
    fn trust_lock_contention_is_bounded_and_typed() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("pando/trust.json");
        fs::create_dir_all(path.parent().expect("trust parent"))?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        FileExt::lock_exclusive(&lock)?;

        let error = update_trust_at(&path, Duration::ZERO, |_| ((), false)).unwrap_err();
        assert!(is_store_busy(&error), "{error:#}");
        Ok(())
    }
}
