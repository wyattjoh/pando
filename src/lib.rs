pub mod commit;
pub mod config;
pub mod git;
mod hash;
pub mod install;
pub mod lifecycle;
pub mod render;
pub mod setup;
pub mod smart;
pub mod trust;
pub mod ui;

use std::path::PathBuf;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
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

fn push_reason(states: &mut Vec<String>, label: &str, reason: Option<&str>) {
    if let Some(reason) = reason {
        states.push(if reason.is_empty() {
            label.to_owned()
        } else {
            format!("{label}: {reason}")
        });
    }
}
