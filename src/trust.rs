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
    config::{self, HookStep},
    git::Repository,
    hash,
};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    #[serde(default)]
    repositories: BTreeMap<String, String>,
}

/// Returns the deterministic executable identity of an ordered command list.
#[must_use]
pub fn command_hash(steps: &[HookStep]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"worktrees-post-create-v1\0");
    for step in steps {
        let bytes = step.command.as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    hash::encode_hex(&digest.finalize())
}

/// Reports whether the current ordered commands are trusted for this clone.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be resolved.
pub fn is_trusted(repository: &Repository, steps: &[HookStep]) -> Result<bool> {
    let identity = repository_key(repository)?;
    let trust = read_trust()?;
    if steps.is_empty() {
        return Ok(true);
    }
    Ok(trust.repositories.get(&identity) == Some(&command_hash(steps)))
}

/// Atomically saves approval for the current ordered commands.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be updated.
pub fn approve(repository: &Repository, steps: &[HookStep]) -> Result<()> {
    let identity = repository_key(repository)?;
    let mut trust = read_trust()?;
    trust.repositories.insert(identity, command_hash(steps));
    write_trust(&trust)
}

/// Removes this clone's trust record and reports whether one existed.
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
