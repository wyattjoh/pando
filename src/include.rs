//! Copies the Git-ignored files a `.worktreeinclude` selects into a new worktree.
//!
//! The file uses gitignore syntax and is read from the source worktree. Only
//! untracked files that Git also ignores are candidates, so tracked content is
//! never duplicated and an overly broad pattern cannot pull in stray work.
//! Existing destination entries are never overwritten, and symbolic links are
//! recreated rather than followed.

use std::{
    fs, io,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::git::RepositoryObservation;

/// The include file's name, read from the root of the source worktree.
pub(crate) const FILE_NAME: &str = ".worktreeinclude";

/// Returns the include file inside `source` when one exists.
#[must_use]
pub(crate) fn include_file(source: &Path) -> Option<PathBuf> {
    let path = source.join(FILE_NAME);
    path.is_file().then_some(path)
}

/// Copies every selected file from `source` into `destination`.
///
/// Returns how many entries were copied; entries already present in
/// `destination` are skipped rather than counted.
///
/// # Errors
///
/// Returns an error when Git cannot list the selection or a copy fails.
pub(crate) fn copy(source: &Path, destination: &Path, include_file: &Path) -> Result<usize> {
    let selected = RepositoryObservation::new(source).included_ignored_files(include_file)?;
    let mut copied = 0;
    for relative in selected {
        if copy_entry(&source.join(&relative), &destination.join(&relative))
            .with_context(|| format!("failed to copy {}", relative.display()))?
        {
            copied += 1;
        }
    }
    Ok(copied)
}

fn copy_entry(from: &Path, to: &Path) -> Result<bool> {
    match fs::symlink_metadata(to) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = match fs::symlink_metadata(from) {
        Ok(metadata) => metadata,
        // The file disappeared after Git listed it; there is nothing to copy.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if metadata.file_type().is_symlink() {
        symlink(fs::read_link(from)?, to)?;
    } else if metadata.is_file() {
        fs::copy(from, to)?;
    } else {
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::copy_entry;

    #[test]
    fn existing_destination_entries_are_never_overwritten() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let from = temp.path().join("from");
        let to = temp.path().join("to");
        fs::write(&from, "source")?;
        fs::write(&to, "kept")?;
        assert!(!copy_entry(&from, &to)?);
        assert_eq!(fs::read_to_string(&to)?, "kept");
        Ok(())
    }

    #[test]
    fn symbolic_links_are_recreated_not_followed() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let from = temp.path().join("link");
        std::os::unix::fs::symlink("missing-target", &from)?;
        let to = temp.path().join("nested/link");
        assert!(copy_entry(&from, &to)?);
        assert_eq!(fs::read_link(&to)?, fs::read_link(&from)?);
        Ok(())
    }
}
