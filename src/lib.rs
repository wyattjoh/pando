pub mod commit;
pub mod completion;
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
pub mod squash;
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

/// Where `switch` and `create` start a genuinely new branch.
///
/// Deserialized strictly, so an unknown configured value fails with the file
/// that set it rather than silently falling back.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum BaseMode {
    /// The invoking worktree's committed `HEAD`.
    #[default]
    Head,
    /// The target branch's remote-tracking ref, without any implicit fetch.
    Fresh,
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

    #[must_use]
    pub fn has_branch(&self, branch: &str) -> bool {
        matches!(&self.kind, WorktreeKind::Branch(value) if value == branch)
    }
}

/// Finds the worktree checked out at `branch`, if any.
#[must_use]
pub fn worktree_for_branch<'a>(worktrees: &'a [Worktree], branch: &str) -> Option<&'a Worktree> {
    worktrees
        .iter()
        .find(|worktree| worktree.has_branch(branch))
}

/// A display row for either the worktree table or the branch table.
///
/// Worktree rows always carry a path and a condition; branch rows carry
/// them only when the branch has a worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub label: String,
    pub branch: Option<String>,
    pub path: Option<PathBuf>,
    pub last_commit_at: Option<DateTime<FixedOffset>>,
    pub condition: Option<Condition>,
    pub current: bool,
}

impl Row {
    #[must_use]
    pub fn from_worktree(worktree: &Worktree) -> Self {
        Self {
            label: worktree.branch_label().to_owned(),
            branch: match &worktree.kind {
                WorktreeKind::Branch(branch) => Some(branch.clone()),
                WorktreeKind::Detached | WorktreeKind::Bare | WorktreeKind::Unknown => None,
            },
            path: Some(worktree.path.clone()),
            last_commit_at: worktree.last_commit_at,
            condition: Some(worktree.condition),
            current: worktree.current,
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
    pub fn is_dirty(&self) -> bool {
        self.condition == Some(Condition::Dirty)
    }

    /// Builds a branch-view row, left-joining `record` against `worktrees` by branch name.
    #[must_use]
    pub fn from_branch(record: &git::BranchRecord, worktrees: &[Worktree]) -> Self {
        let worktree = worktree_for_branch(worktrees, &record.branch);
        Self {
            label: record.branch.clone(),
            branch: Some(record.branch.clone()),
            path: worktree.map(|worktree| worktree.path.clone()),
            last_commit_at: record.last_commit_at,
            condition: worktree.map(|worktree| worktree.condition),
            current: worktree.is_some_and(|worktree| worktree.current),
        }
    }
}

#[must_use]
pub fn sorted_row_indices(rows: &[&Row], mode: SortMode) -> Vec<usize> {
    let mut indexed: Vec<_> = rows.iter().enumerate().collect();
    indexed.sort_by(|(left_index, left), (right_index, right)| {
        compare_rows(left, right, mode).then_with(|| left_index.cmp(right_index))
    });
    indexed.into_iter().map(|(index, _)| index).collect()
}

fn compare_rows(left: &Row, right: &Row, mode: SortMode) -> Ordering {
    match mode {
        SortMode::Git => Ordering::Equal,
        SortMode::Branch => left.label.cmp(&right.label),
        SortMode::LastCommitAt => right.last_commit_at.cmp(&left.last_commit_at),
        SortMode::Path => match (&left.path, &right.path) {
            (Some(left), Some(right)) => left
                .as_os_str()
                .as_bytes()
                .cmp(right.as_os_str().as_bytes()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
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

#[cfg(test)]
mod row_sort_tests {
    use super::{Row, SortMode, sorted_row_indices};

    fn row(label: &str, path: Option<&str>) -> Row {
        Row {
            label: label.to_owned(),
            branch: Some(label.to_owned()),
            path: path.map(Into::into),
            last_commit_at: None,
            condition: path.map(|_| super::Condition::Clean),
            current: false,
        }
    }

    #[test]
    fn path_sort_places_pathless_rows_after_pathed_rows_preserving_input_order() {
        let rows = [
            row("b", None),
            row("a", Some("/z")),
            row("c", None),
            row("d", Some("/a")),
        ];
        let refs: Vec<_> = rows.iter().collect();

        let order = sorted_row_indices(&refs, SortMode::Path);

        assert_eq!(order, vec![3, 1, 0, 2]);
    }

    #[test]
    fn git_order_returns_input_order_unchanged() {
        let rows = [row("z", Some("/z")), row("a", None), row("m", Some("/m"))];
        let refs: Vec<_> = rows.iter().collect();

        let order = sorted_row_indices(&refs, SortMode::Git);

        assert_eq!(order, vec![0, 1, 2]);
    }
}

#[cfg(test)]
mod branch_row_tests {
    use super::{Condition, Row, Worktree, WorktreeKind};
    use crate::git::BranchRecord;

    fn worktree(branch: &str, current: bool, condition: Condition) -> Worktree {
        Worktree {
            path: format!("/repo/{branch}").into(),
            head: None,
            last_commit_at: None,
            kind: WorktreeKind::Branch(branch.to_owned()),
            locked: None,
            prunable: None,
            current,
            condition,
        }
    }

    fn record(branch: &str) -> BranchRecord {
        BranchRecord {
            branch: branch.to_owned(),
            head: "deadbeef".to_owned(),
            last_commit_at: None,
        }
    }

    #[test]
    fn unattached_branch_has_no_path_or_condition() {
        let worktrees = [worktree("main", true, Condition::Clean)];

        let row = Row::from_branch(&record("feature"), &worktrees);

        assert_eq!(row.path, None);
        assert_eq!(row.condition, None);
        assert!(!row.current);
    }

    #[test]
    fn attached_branch_carries_the_worktrees_path_and_condition() {
        let worktrees = [worktree("feature", false, Condition::Dirty)];

        let row = Row::from_branch(&record("feature"), &worktrees);

        assert_eq!(row.path, Some("/repo/feature".into()));
        assert_eq!(row.condition, Some(Condition::Dirty));
        assert!(row.is_dirty());
    }
}
