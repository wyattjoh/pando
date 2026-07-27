use unicode_width::UnicodeWidthStr;

use crate::{Condition, Worktree, ui};

#[must_use]
pub fn table(worktrees: &[Worktree]) -> String {
    let worktrees: Vec<_> = worktrees.iter().collect();
    let branch_width = branch_width(&worktrees, true);
    let mut output = format!(
        "  {}  {}\n",
        ui::muted_style().apply_to(pad("BRANCH", branch_width)),
        ui::muted_style().apply_to("PATH"),
    );
    for worktree in worktrees {
        output.push_str(&styled_row(worktree, branch_width));
        output.push('\n');
    }
    output
}

#[must_use]
pub fn menu_labels(worktrees: &[&Worktree]) -> Vec<String> {
    let branch_width = branch_width(worktrees, false);
    worktrees
        .iter()
        .map(|worktree| {
            format!(
                "{}  {}",
                styled_branch_label(worktree, branch_width, true),
                ui::interactive(ui::worktree_data_style())
                    .apply_to(abbreviated_path(&worktree.path)),
            )
        })
        .collect()
}

fn branch_width(worktrees: &[&Worktree], include_header: bool) -> usize {
    worktrees
        .iter()
        .map(|worktree| UnicodeWidthStr::width(marked_branch_label(worktree).as_str()))
        .max()
        .unwrap_or(0)
        .max(usize::from(include_header) * UnicodeWidthStr::width("BRANCH"))
}

fn styled_row(worktree: &Worktree, branch_width: usize) -> String {
    let current_marker = if worktree.current {
        ui::accent_style().bold().apply_to("*").to_string()
    } else {
        " ".to_owned()
    };
    format!(
        "{current_marker} {}  {}",
        styled_branch_label(worktree, branch_width, false),
        ui::worktree_data_style().apply_to(abbreviated_path(&worktree.path)),
    )
}

fn styled_branch_label(worktree: &Worktree, width: usize, force: bool) -> String {
    let label = worktree.branch_label();
    let maybe_interactive = |style| {
        if force { ui::interactive(style) } else { style }
    };
    let mut output = maybe_interactive(ui::worktree_data_style().bold())
        .apply_to(label)
        .to_string();
    if worktree.condition == Condition::Dirty {
        output.push(' ');
        output.push_str(
            &maybe_interactive(ui::warning_style())
                .apply_to("*")
                .to_string(),
        );
    }
    output.push_str(&" ".repeat(width.saturating_sub(UnicodeWidthStr::width(
        marked_branch_label(worktree).as_str(),
    ))));
    output
}

fn marked_branch_label(worktree: &Worktree) -> String {
    if worktree.condition == Condition::Dirty {
        format!("{} *", worktree.branch_label())
    } else {
        worktree.branch_label().to_owned()
    }
}

fn abbreviated_path(path: &std::path::Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return path.display().to_string();
    };
    let Ok(relative) = path.strip_prefix(home) else {
        return path.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", relative.display())
    }
}

fn pad(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}
