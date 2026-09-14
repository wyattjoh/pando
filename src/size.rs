//! Disk measurement for worktree directories.
//!
//! Sizes are presentation-only: they tell a person how much space a removal
//! returns and never decide whether one happens. `clean` measures in the
//! background to fill its picker; `remove` measures its targets before deleting
//! them so its progress can say how much data is going.

use std::{
    collections::HashSet,
    fmt, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// `st_blocks` is reported in 512-byte units on every supported platform.
const BLOCK_SIZE: u64 = 512;
const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// How much disk one worktree occupies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Size {
    /// Not measured yet; a worker is still walking the directory.
    Pending,
    Measured {
        bytes: u64,
        /// Whether an unreadable subtree was skipped, so the total under-reports.
        partial: bool,
    },
    /// The directory could not be read at all.
    Unavailable,
}

impl Size {
    pub(crate) const fn bytes(self) -> Option<u64> {
        match self {
            Self::Measured { bytes, .. } => Some(bytes),
            Self::Pending | Self::Unavailable => None,
        }
    }
}

impl fmt::Display for Size {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("…"),
            Self::Unavailable => formatter.write_str("—"),
            Self::Measured { bytes, partial } => {
                if *partial {
                    formatter.write_str("~")?;
                }
                formatter.write_str(&format_bytes(*bytes))
            }
        }
    }
}

/// The space a set of worktrees occupies together.
///
/// A total is marked approximate when any member was unmeasured or only partly
/// readable, so an under-count is never presented as exact.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Total {
    bytes: u64,
    approximate: bool,
}

impl Total {
    pub(crate) fn of<'sizes>(sizes: impl IntoIterator<Item = &'sizes Size>) -> Self {
        let mut total = Self::default();
        for size in sizes {
            match size {
                Size::Measured { bytes, partial } => {
                    total.bytes = total.bytes.saturating_add(*bytes);
                    total.approximate |= *partial;
                }
                Size::Pending | Size::Unavailable => total.approximate = true,
            }
        }
        total
    }
}

impl fmt::Display for Total {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.approximate {
            formatter.write_str("~")?;
        }
        formatter.write_str(&format_bytes(self.bytes))
    }
}

// The scaled value is only ever rendered to one decimal place, so the precision
// lost converting a byte count to a float can never change what is displayed.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Measures a worktree without a cancellation source.
pub(crate) fn measure_now(root: &Path, excluded: &HashSet<PathBuf>) -> Size {
    measure(root, excluded, &AtomicBool::new(false))
        .expect("an uncancellable walk always reports a size")
}

/// Sums the disk a worktree occupies, excluding nested registered worktrees.
///
/// Allocated blocks rather than apparent file lengths, so the total is the space
/// a removal actually returns; a file reached through a second hard link is
/// counted once; symlinks are never followed. Unreadable subtrees are skipped
/// and reported as a partial total rather than failing the measurement.
///
/// Returns `None` when cancellation interrupted the walk, which discards the
/// incomplete total instead of presenting it.
pub(crate) fn measure(
    root: &Path,
    excluded: &HashSet<PathBuf>,
    cancel: &AtomicBool,
) -> Option<Size> {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return Some(Size::Unavailable);
    };
    if !metadata.is_dir() {
        return Some(Size::Unavailable);
    }
    let mut bytes = metadata.blocks().saturating_mul(BLOCK_SIZE);
    let mut partial = false;
    let mut linked: HashSet<(u64, u64)> = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            partial = true;
            continue;
        };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Ok(entry) = entry else {
                partial = true;
                continue;
            };
            // `DirEntry::metadata` does not traverse a symlink, so a link is
            // counted as the link itself and never as the tree it points at.
            let Ok(metadata) = entry.metadata() else {
                partial = true;
                continue;
            };
            let path = entry.path();
            if metadata.is_dir() {
                if excluded.contains(&path) {
                    continue;
                }
                pending.push(path);
            } else if metadata.nlink() > 1 && !linked.insert((metadata.dev(), metadata.ino())) {
                continue;
            }
            bytes = bytes.saturating_add(metadata.blocks().saturating_mul(BLOCK_SIZE));
        }
    }
    Some(Size::Measured { bytes, partial })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use super::{Size, Total, format_bytes, measure_now};

    #[test]
    fn byte_counts_scale_to_binary_units_with_one_decimal() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.0 GiB");
    }

    #[test]
    fn a_file_reached_through_two_hard_links_is_counted_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("original"), vec![0u8; 128 * 1024]).unwrap();
        let single = measure_now(root, &HashSet::new());

        fs::hard_link(root.join("original"), root.join("linked")).unwrap();
        let linked = measure_now(root, &HashSet::new());

        assert_eq!(single, linked);
    }

    #[test]
    fn a_nested_registered_worktree_is_excluded_from_its_parents_total() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("worktrees").join("topic");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("payload"), vec![0u8; 512 * 1024]).unwrap();

        let including = measure_now(root, &HashSet::new());
        let excluding = measure_now(root, &HashSet::from([nested]));

        let (Size::Measured { bytes: with, .. }, Size::Measured { bytes: without, .. }) =
            (including, excluding)
        else {
            panic!("both walks measure a readable directory");
        };
        assert!(with > without + 256 * 1024, "{with} vs {without}");
    }

    #[test]
    fn a_symlinked_tree_is_counted_as_the_link_not_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(outside.join("deep")).unwrap();
        fs::write(outside.join("deep/payload"), vec![0u8; 512 * 1024]).unwrap();
        let worktree = temp.path().join("worktree");
        fs::create_dir(&worktree).unwrap();
        let empty = measure_now(&worktree, &HashSet::new());

        symlink(&outside, worktree.join("link")).unwrap();
        let linked = measure_now(&worktree, &HashSet::new());

        let (Size::Measured { bytes: before, .. }, Size::Measured { bytes: after, .. }) =
            (empty, linked)
        else {
            panic!("both walks measure a readable directory");
        };
        assert!(after < before + 128 * 1024, "{before} vs {after}");
    }

    #[test]
    fn an_unreadable_subtree_yields_a_partial_total() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let blocked = root.join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("payload"), vec![0u8; 1024]).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            // A privileged run reads it anyway, which is not what this asserts.
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let size = measure_now(root, &HashSet::new());

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(size, Size::Measured { partial: true, .. }),
            "{size:?}"
        );
        assert_eq!(size.to_string().chars().next(), Some('~'));
    }

    #[test]
    fn a_missing_or_non_directory_path_has_no_measurable_size() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "contents").unwrap();

        assert_eq!(
            measure_now(&temp.path().join("absent"), &HashSet::new()),
            Size::Unavailable
        );
        assert_eq!(measure_now(&file, &HashSet::new()), Size::Unavailable);
        assert_eq!(Size::Unavailable.to_string(), "—");
        assert_eq!(Size::Pending.to_string(), "…");
    }

    #[test]
    fn a_total_is_approximate_when_any_member_is_partial_or_unmeasured() {
        let exact = [
            Size::Measured {
                bytes: 1024,
                partial: false,
            },
            Size::Measured {
                bytes: 1024,
                partial: false,
            },
        ];
        assert_eq!(Total::of(&exact).to_string(), "2.0 KiB");

        let partial = [
            Size::Measured {
                bytes: 1024,
                partial: true,
            },
            Size::Unavailable,
        ];
        assert_eq!(Total::of(&partial).to_string(), "~1.0 KiB");
    }
}
