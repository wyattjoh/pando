use std::fmt::Display;

use anyhow::{Context, Result};
use cliclack::{Theme, ThemeState, log, outro, set_theme};
use console::{
    Style, colors_enabled_stderr, set_colors_enabled, set_true_colors_enabled,
    true_colors_enabled_stderr,
};

struct WorktreesTheme;

impl Theme for WorktreesTheme {
    fn bar_color(&self, state: &ThemeState) -> Style {
        match state {
            ThemeState::Active | ThemeState::Submit => accent_style(),
            ThemeState::Cancel => error_style(),
            ThemeState::Error(_) => warning_style(),
        }
    }

    fn state_symbol_color(&self, state: &ThemeState) -> Style {
        interactive(self.bar_color(state))
    }

    fn info_symbol(&self) -> String {
        accent_style().apply_to("●").to_string()
    }

    fn format_outro(&self, message: &str) -> String {
        format!("{}  {message}\n", accent_style().apply_to("└"))
    }

    fn format_header(&self, state: &ThemeState, prompt: &str) -> String {
        prompt
            .lines()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    format!(
                        "{}  {}\n",
                        self.state_symbol(state),
                        interactive(heading_style()).apply_to(line)
                    )
                } else {
                    format!(
                        "{}  {line}\n",
                        interactive(self.bar_color(state)).apply_to("│")
                    )
                }
            })
            .collect()
    }
}

/// Installs the shared terminal theme used by all cliclack prompts.
pub fn install_theme() {
    // `console::Style` targets stdout by default, while all human-facing UI in
    // Worktrees is written to stderr. Mirror stderr's detected capabilities so
    // piped machine output does not disable ordinary terminal presentation.
    set_colors_enabled(colors_enabled_stderr());
    set_true_colors_enabled(true_colors_enabled_stderr());
    set_theme(WorktreesTheme);
}

/// Returns the shared accent style for interactive UI elements.
#[must_use]
pub fn accent_style() -> Style {
    Style::new().green()
}

/// Forces a semantic style for interactive stderr UI while stdout is captured.
#[must_use]
pub fn interactive(style: Style) -> Style {
    style.force_styling(true)
}

/// Returns the shared heading style for terminal UI titles and prompts.
#[must_use]
pub fn heading_style() -> Style {
    accent_style().bold()
}

/// Returns the shared style for branch names, paths, and repository data.
#[must_use]
pub fn worktree_data_style() -> Style {
    Style::new().cyan()
}

/// Returns the shared style for supporting metadata and guidance.
#[must_use]
pub fn muted_style() -> Style {
    Style::new().dim()
}

/// Returns the high-contrast style for active picker content.
#[must_use]
pub fn selected_style() -> Style {
    Style::new().white().bold()
}

/// Returns the shared style for picker shortcut labels.
#[must_use]
pub fn shortcut_style() -> Style {
    Style::new().white()
}

/// Returns the shared style for successful outcomes.
#[must_use]
pub fn success_style() -> Style {
    Style::new().green().bold()
}

/// Returns the shared style for warnings and caution markers.
#[must_use]
pub fn warning_style() -> Style {
    Style::new().yellow()
}

/// Returns the shared style for errors and failed outcomes.
#[must_use]
pub fn error_style() -> Style {
    Style::new().red()
}

/// Writes an informational message through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn info(message: impl Display) -> Result<()> {
    log::info(message).context("failed to write terminal message")
}

/// Writes a successful outcome through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn success(message: impl Display) -> Result<()> {
    log::success(success_style().apply_to(message)).context("failed to write terminal message")
}

/// Writes a warning through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn warning(message: impl Display) -> Result<()> {
    log::warning(warning_style().apply_to(message)).context("failed to write terminal message")
}

/// Writes an error through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn error(message: impl Display) -> Result<()> {
    log::error(error_style().apply_to(message)).context("failed to write terminal message")
}

/// Writes a completed step through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn step(message: impl Display) -> Result<()> {
    log::step(message).context("failed to write terminal message")
}

/// Writes a terminal UI outro.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn finish(message: impl Display) -> Result<()> {
    outro(message).context("failed to write terminal message")
}

#[cfg(test)]
mod tests {
    use cliclack::{Theme, ThemeState};

    use super::{WorktreesTheme, heading_style, interactive};

    #[test]
    fn prompt_headers_use_the_shared_heading_style() {
        let rendered = WorktreesTheme.format_header(&ThemeState::Active, "Branch name:");

        assert!(
            rendered.contains(
                &interactive(heading_style())
                    .apply_to("Branch name:")
                    .to_string()
            )
        );
    }
}
