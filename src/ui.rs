use std::fmt::Display;

use anyhow::{Context, Result};
use cliclack::{Theme, ThemeState, log, outro, set_theme};
use console::Style;

struct WorktreesTheme;

impl Theme for WorktreesTheme {
    fn bar_color(&self, state: &ThemeState) -> Style {
        match state {
            ThemeState::Active | ThemeState::Submit => accent_style().force_styling(true),
            ThemeState::Cancel => Style::new().red().force_styling(true),
            ThemeState::Error(_) => Style::new().yellow().force_styling(true),
        }
    }

    fn state_symbol_color(&self, state: &ThemeState) -> Style {
        self.bar_color(state)
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
                        header_style().force_styling(true).apply_to(line)
                    )
                } else {
                    format!("{}  {line}\n", self.bar_color(state).apply_to("│"))
                }
            })
            .collect()
    }
}

/// Installs the shared terminal theme used by all cliclack prompts.
pub fn install_theme() {
    set_theme(WorktreesTheme);
}

/// Returns the shared accent style for interactive UI elements.
#[must_use]
pub fn accent_style() -> Style {
    Style::new().green()
}

/// Returns the shared heading style for terminal UI titles and prompts.
#[must_use]
pub fn header_style() -> Style {
    accent_style().bold()
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
    log::success(message).context("failed to write terminal message")
}

/// Writes a warning through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn warning(message: impl Display) -> Result<()> {
    log::warning(message).context("failed to write terminal message")
}

/// Writes an error through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn error(message: impl Display) -> Result<()> {
    log::error(message).context("failed to write terminal message")
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

    use super::{WorktreesTheme, header_style};

    #[test]
    fn prompt_headers_use_the_shared_heading_style() {
        let rendered = WorktreesTheme.format_header(&ThemeState::Active, "Branch name:");

        assert!(
            rendered.contains(
                &header_style()
                    .force_styling(true)
                    .apply_to("Branch name:")
                    .to_string()
            )
        );
    }
}
