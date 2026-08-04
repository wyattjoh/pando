use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
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
    let mut trust = read_trust()?;
    let approvals = match trust.repositories.remove(&identity) {
        Some(TrustRecord::Legacy(post_create)) => PhaseApprovals {
            post_create: Some(post_create),
            ..PhaseApprovals::default()
        },
        Some(TrustRecord::Phases(approvals)) => approvals,
        None => PhaseApprovals::default(),
    };
    let mut approvals = approvals;
    approvals.set(phase, command_hash(phase, steps));
    trust
        .repositories
        .insert(identity, TrustRecord::Phases(approvals));
    write_trust(&trust)
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
    let mut trust = read_trust()?;
    trust
        .commit_generators
        .insert(repository_key(repository)?, hash);
    write_trust(&trust)
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
    let mut trust = read_trust()?;
    trust
        .pr_generators
        .insert(repository_key(repository)?, hash);
    write_trust(&trust)
}

/// Resets PR generator approval.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn reset_pr_generation(repository: &Repository) -> Result<bool> {
    let mut trust = read_trust()?;
    let removed = trust
        .pr_generators
        .remove(&repository_key(repository)?)
        .is_some();
    if removed {
        write_trust(&trust)?;
    }
    Ok(removed)
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
    let mut trust = read_trust()?;
    trust
        .merge_generators
        .insert(repository_key(repository)?, hash);
    write_trust(&trust)
}

/// Resets squash-message generator approval.
///
/// # Errors
/// Returns an error when trust storage cannot be updated.
pub fn reset_merge_generation(repository: &Repository) -> Result<bool> {
    let mut trust = read_trust()?;
    let removed = trust
        .merge_generators
        .remove(&repository_key(repository)?)
        .is_some();
    if removed {
        write_trust(&trust)?;
    }
    Ok(removed)
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
    let mut trust = read_trust()?;
    let removed = trust
        .commit_generators
        .remove(&repository_key(repository)?)
        .is_some();
    if removed {
        write_trust(&trust)?;
    }
    Ok(removed)
}

/// Removes this clone's phase approvals and reports whether any existed.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be updated.
pub fn reset(repository: &Repository) -> Result<bool> {
    let identity = repository_key(repository)?;
    let mut trust = read_trust()?;
    let removed = trust.repositories.remove(&identity).is_some();
    if removed {
        write_trust(&trust)?;
    }
    Ok(removed)
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
    let path = trust_path()?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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

fn write_trust(trust: &TrustFile) -> Result<()> {
    let path = trust_path()?;
    let bytes = serde_json::to_vec_pretty(trust).context("failed to encode trust storage")?;
    write_atomic(&path, &bytes)
}

/// Atomically replaces a state file beside its destination.
///
/// # Errors
///
/// Returns an error when the parent or temporary file cannot be written or renamed.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("trust path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    for attempt in 0..100_u8 {
        let temporary = parent.join(format!(".pando.tmp.{}.{}", std::process::id(), attempt));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        };
        let result = (|| -> Result<()> {
            file.write_all(content)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result.with_context(|| format!("failed to atomically update {}", path.display()));
    }
    bail!(
        "could not allocate a temporary trust file beside {}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::{TrustRecord, legacy_post_create_hash};
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
}
