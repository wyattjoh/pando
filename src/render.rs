use unicode_width::UnicodeWidthStr;

use crate::Worktree;

#[must_use]
pub fn table(worktrees: &[Worktree]) -> String {
    let worktrees: Vec<_> = worktrees.iter().collect();
    let (branch_width, state_width) = column_widths(&worktrees, true);
    let mut output = format!(
        "  {}  {}  PATH\n",
        pad("BRANCH", branch_width),
        pad("STATE", state_width)
    );
    for worktree in worktrees {
        output.push_str(&row(worktree, branch_width, state_width));
        output.push('\n');
    }
    output
}

#[must_use]
pub fn menu_labels(worktrees: &[&Worktree]) -> Vec<String> {
    let (branch_width, state_width) = column_widths(worktrees, false);
    worktrees
        .iter()
        .map(|worktree| row(worktree, branch_width, state_width))
        .collect()
}

fn column_widths(worktrees: &[&Worktree], include_headers: bool) -> (usize, usize) {
    let mut branch_width = usize::from(include_headers) * UnicodeWidthStr::width("BRANCH");
    let mut state_width = usize::from(include_headers) * UnicodeWidthStr::width("STATE");
    for worktree in worktrees {
        branch_width = branch_width.max(UnicodeWidthStr::width(worktree.branch_label()));
        state_width = state_width.max(UnicodeWidthStr::width(worktree.state_label().as_str()));
    }
    (branch_width, state_width)
}

fn row(worktree: &Worktree, branch_width: usize, state_width: usize) -> String {
    format!(
        "{} {}  {}  {}",
        if worktree.current { '*' } else { ' ' },
        pad(worktree.branch_label(), branch_width),
        pad(&worktree.state_label(), state_width),
        worktree.path.display()
    )
}

fn pad(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}
