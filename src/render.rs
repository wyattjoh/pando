use unicode_width::UnicodeWidthStr;

use crate::{Row, SortMode, sorted_row_indices, ui};

const LAST_COMMIT_WIDTH: usize = 16;

#[must_use]
pub fn table(rows: &[Row], sort: SortMode) -> String {
    let row_refs: Vec<_> = rows.iter().collect();
    let branch_width = branch_width(&row_refs);
    let mut output = format!(
        "  {}  {}  {}\n",
        ui::muted_style().apply_to(pad(branch_header(sort), branch_width)),
        ui::muted_style().apply_to(pad(last_commit_header(sort), LAST_COMMIT_WIDTH)),
        ui::muted_style().apply_to(path_header(sort)),
    );
    for index in sorted_row_indices(&row_refs, sort) {
        output.push_str(&styled_row(row_refs[index], branch_width));
        output.push('\n');
    }
    output
}

#[must_use]
pub fn menu_labels(rows: &[&Row]) -> Vec<String> {
    let branch_width = branch_width(rows);
    rows.iter()
        .map(|row| {
            format!(
                "{}  {}  {}",
                styled_branch_label(row, branch_width, true),
                ui::interactive(ui::muted_style())
                    .apply_to(pad(&row.human_last_commit_at(), LAST_COMMIT_WIDTH)),
                ui::interactive(ui::worktree_data_style())
                    .apply_to(abbreviated_path(row.path.as_deref())),
            )
        })
        .collect()
}

#[must_use]
pub fn menu_header(rows: &[&Row], sort: SortMode) -> String {
    format!(
        "{}  {}  {}",
        pad(branch_header(sort), branch_width(rows)),
        pad(last_commit_header(sort), LAST_COMMIT_WIDTH),
        path_header(sort),
    )
}

/// Styles captured Git output for presentation inside the terminal UI rail.
///
/// Diffstat rows keep their path highlighted; every other line stays muted so
/// Git's own prose never competes with the command's own reporting.
#[must_use]
pub fn git_output(output: &str) -> String {
    output
        .lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .map(|line| match line.split_once(" | ") {
            Some((path, stat)) => format!("{} | {stat}", ui::worktree_data_style().apply_to(path)),
            None => ui::muted_style().apply_to(line).to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn branch_header(sort: SortMode) -> &'static str {
    if sort == SortMode::Branch {
        "BRANCH ↑"
    } else {
        "BRANCH"
    }
}

fn last_commit_header(sort: SortMode) -> &'static str {
    if sort == SortMode::LastCommitAt {
        "LAST COMMIT AT ↓"
    } else {
        "LAST COMMIT AT"
    }
}

fn path_header(sort: SortMode) -> &'static str {
    if sort == SortMode::Path {
        "PATH ↑"
    } else {
        "PATH"
    }
}

fn branch_width(rows: &[&Row]) -> usize {
    rows.iter()
        .map(|row| UnicodeWidthStr::width(marked_branch_label(row).as_str()))
        .max()
        .unwrap_or(0)
        .max(UnicodeWidthStr::width("BRANCH ↑"))
}

fn styled_row(row: &Row, branch_width: usize) -> String {
    let current_marker = if row.current {
        ui::accent_style().bold().apply_to("*").to_string()
    } else {
        " ".to_owned()
    };
    format!(
        "{current_marker} {}  {}  {}",
        styled_branch_label(row, branch_width, false),
        ui::muted_style().apply_to(pad(&row.human_last_commit_at(), LAST_COMMIT_WIDTH)),
        ui::worktree_data_style().apply_to(abbreviated_path(row.path.as_deref())),
    )
}

fn styled_branch_label(row: &Row, width: usize, force: bool) -> String {
    let maybe_interactive = |style| {
        if force { ui::interactive(style) } else { style }
    };
    let mut output = maybe_interactive(ui::worktree_data_style().bold())
        .apply_to(&row.label)
        .to_string();
    if row.is_dirty() {
        output.push(' ');
        output.push_str(
            &maybe_interactive(ui::warning_style())
                .apply_to("*")
                .to_string(),
        );
    }
    output.push_str(
        &" ".repeat(
            width.saturating_sub(UnicodeWidthStr::width(marked_branch_label(row).as_str())),
        ),
    );
    output
}

fn marked_branch_label(row: &Row) -> String {
    if row.is_dirty() {
        format!("{} *", row.label)
    } else {
        row.label.clone()
    }
}

fn abbreviated_path(path: Option<&std::path::Path>) -> String {
    let Some(path) = path else {
        return String::new();
    };
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
