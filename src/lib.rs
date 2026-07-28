pub mod commit;
pub mod config;
pub mod git;
mod hash;
pub mod install;
pub mod lifecycle;
pub mod machine;
pub mod pr;
pub mod protocol;
pub mod render;
pub mod setup;
pub mod smart;
pub mod trust;
pub mod ui;

use std::{cmp::Ordering, os::unix::ffi::OsStrExt, path::PathBuf};

use chrono::{DateTime, FixedOffset, Local, SecondsFormat};
use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub last_commit_at: Option<DateTime<FixedOffset>>,
    pub kind: WorktreeKind,
    pub locked: Option<String>,
    pub prunable: Option<String>,
    pub current: bool,
    pub condition: Condition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeKind {
    Branch(String),
    Detached,
    Bare,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Condition {
    Clean,
    Dirty,
    Unknown,
    Missing,
    Inaccessible,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SortMode {
    #[default]
    Git,
    Branch,
    LastCommitAt,
    Path,
}

impl SortMode {
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Git => Self::Branch,
            Self::Branch => Self::LastCommitAt,
            Self::LastCommitAt => Self::Path,
            Self::Path => Self::Git,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Git => "Git order",
            Self::Branch => "branch A-Z",
            Self::LastCommitAt => "last commit newest-first",
            Self::Path => "path A-Z",
        }
    }
}

impl Worktree {
    #[must_use]
    pub fn branch_label(&self) -> &str {
        match &self.kind {
            WorktreeKind::Branch(branch) => branch,
            WorktreeKind::Detached => "(detached)",
            WorktreeKind::Bare => "(bare)",
            WorktreeKind::Unknown => "(unknown)",
        }
    }

    #[must_use]
    pub fn human_last_commit_at(&self) -> String {
        self.last_commit_at.as_ref().map_or_else(
            || "unknown".to_owned(),
            |timestamp| {
                timestamp
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            },
        )
    }

    #[must_use]
    pub fn machine_last_commit_at(&self) -> Option<String> {
        self.last_commit_at
            .as_ref()
            .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Secs, false))
    }

    #[must_use]
    pub fn is_bare(&self) -> bool {
        self.kind == WorktreeKind::Bare
    }

    #[must_use]
    pub fn state_label(&self) -> String {
        let mut states = Vec::new();
        match self.condition {
            Condition::Clean => {}
            Condition::Dirty => states.push("dirty".to_owned()),
            Condition::Unknown => states.push("unknown".to_owned()),
            Condition::Missing => states.push("missing".to_owned()),
            Condition::Inaccessible => states.push("inaccessible".to_owned()),
        }
        if self.is_bare() {
            states.push("bare".to_owned());
        }
        push_reason(&mut states, "locked", self.locked.as_deref());
        push_reason(&mut states, "prunable", self.prunable.as_deref());
        states.join(", ")
    }

    #[must_use]
    pub fn navigable(&self) -> bool {
        !self.is_bare()
            && matches!(
                self.condition,
                Condition::Clean | Condition::Dirty | Condition::Unknown
            )
    }
}

#[must_use]
pub fn sorted_worktree_indices(worktrees: &[&Worktree], mode: SortMode) -> Vec<usize> {
    let mut indexed: Vec<_> = worktrees.iter().enumerate().collect();
    indexed.sort_by(|(left_index, left), (right_index, right)| {
        compare_worktrees(left, right, mode).then_with(|| left_index.cmp(right_index))
    });
    indexed.into_iter().map(|(index, _)| index).collect()
}

fn compare_worktrees(left: &Worktree, right: &Worktree, mode: SortMode) -> Ordering {
    match mode {
        SortMode::Git => Ordering::Equal,
        SortMode::Branch => left.branch_label().cmp(right.branch_label()),
        SortMode::LastCommitAt => right.last_commit_at.cmp(&left.last_commit_at),
        SortMode::Path => left
            .path
            .as_os_str()
            .as_bytes()
            .cmp(right.path.as_os_str().as_bytes()),
    }
}

fn push_reason(states: &mut Vec<String>, label: &str, reason: Option<&str>) {
    if let Some(reason) = reason {
        states.push(if reason.is_empty() {
            label.to_owned()
        } else {
            format!("{label}: {reason}")
        });
    }
}
