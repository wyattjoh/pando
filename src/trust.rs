use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    config::{self, EffectiveGeneration, GenerationSource, HookPhase, HookStep},
    git::Repository,
    hash,
};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    #[serde(default)]
    repositories: BTreeMap<String, TrustRecord>,
    #[serde(default)]
    commit_generators: BTreeMap<String, String>,
    #[serde(default)]
    pr_generators: BTreeMap<String, String>,
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
pub fn command_hash(phase: HookPhase, steps: &[HookStep]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"worktrees-hook-phase-v1\0");
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
    digest.update(b"worktrees-post-create-v1\0");
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
pub fn is_trusted(repository: &Repository, phase: HookPhase, steps: &[HookStep]) -> Result<bool> {
    let identity = repository_key(repository)?;
    let trust = read_trust()?;
    if steps.is_empty() {
        return Ok(true);
    }
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
pub fn approve(repository: &Repository, phase: HookPhase, steps: &[HookStep]) -> Result<()> {
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
    digest.update(b"worktrees-commit-generation-v1\0");
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
    let Some(hash) = generation_hash_named(generation, b"worktrees-pr-generation-v1") else {
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
    let Some(hash) = generation_hash_named(generation, b"worktrees-pr-generation-v1") else {
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

fn repository_key(repository: &Repository) -> Result<String> {
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
    Ok(config::config_home()?.join("worktrees/trust.json"))
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
        let temporary = parent.join(format!(".worktrees.tmp.{}.{}", std::process::id(), attempt));
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
