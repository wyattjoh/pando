use std::{
    error::Error,
    fmt::{self, Display},
    io::{self, IsTerminal, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use cliclack::{ProgressBar, Theme, ThemeState, log, outro, outro_cancel, set_theme, spinner};
use console::{
    Style, colors_enabled_stderr, set_colors_enabled, set_true_colors_enabled,
    true_colors_enabled_stderr,
};

struct PandoTheme;

/// Whether this run has opened a terminal UI sequence that an outro must close.
static SEQUENCE_OPEN: AtomicBool = AtomicBool::new(false);

fn open_sequence() {
    SEQUENCE_OPEN.store(true, Ordering::Relaxed);
}

#[derive(Debug)]
enum InteractionKind {
    Cancelled,
    Declined,
}

/// A user-directed outcome from an interactive prompt.
#[derive(Debug)]
pub struct InteractionError {
    kind: InteractionKind,
    message: String,
    completion: Option<String>,
}

impl InteractionError {
    fn cancelled(message: impl Into<String>) -> Self {
        Self {
            kind: InteractionKind::Cancelled,
            message: message.into(),
            completion: None,
        }
    }

    fn declined(message: impl Into<String>, completion: Option<String>) -> Self {
        Self {
            kind: InteractionKind::Declined,
            message: message.into(),
            completion,
        }
    }

    /// Returns whether the outcome came from Escape or Ctrl-C.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, InteractionKind::Cancelled)
    }

    /// Returns whether the command treats this deliberate no-op as successful.
    #[must_use]
    pub const fn is_successful(&self) -> bool {
        self.completion.is_some()
    }

    /// Returns the final outro for a successful deliberate no-op.
    #[must_use]
    pub fn completion(&self) -> Option<&str> {
        self.completion.as_deref()
    }
}

impl Display for InteractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for InteractionError {}

impl Theme for PandoTheme {
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
    // Pando is written to stderr. Mirror stderr's detected capabilities so
    // piped machine output does not disable ordinary terminal presentation.
    set_colors_enabled(colors_enabled_stderr());
    set_true_colors_enabled(true_colors_enabled_stderr());
    set_theme(PandoTheme);
}

/// Returns the shared accent style for interactive UI elements.
#[must_use]
pub fn accent_style() -> Style {
    Style::new().green()
}

/// Returns a semantic style for interactive stderr UI.
///
/// Theme installation mirrors stderr's detected color capability to the styles
/// used by Cliclack, so this helper deliberately does not force ANSI output.
#[must_use]
pub const fn interactive(style: Style) -> Style {
    style
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

/// Requires both prompt input and presentation output to be terminals.
///
/// # Errors
///
/// Returns an error when either stdin or stderr is not an interactive terminal.
pub fn ensure_interactive(reason: &str) -> Result<()> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        Ok(())
    } else {
        bail!("{reason}, but no interactive terminal is available")
    }
}

/// Maps Cliclack prompt results into user-directed or operational outcomes.
///
/// # Errors
///
/// Returns a cancellation [`InteractionError`] for Escape or Ctrl-C and
/// preserves other terminal failures with operation context.
pub fn prompt_result<T>(result: io::Result<T>, cancelled: &str, failure: &str) -> Result<T> {
    open_sequence();
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            Err(InteractionError::cancelled(cancelled).into())
        }
        Err(error) => Err(error).context(failure.to_owned()),
    }
}

/// Creates a deliberate-decline outcome for the command boundary.
#[must_use]
pub fn declined(message: impl Into<String>) -> anyhow::Error {
    InteractionError::declined(message, None).into()
}

/// Creates a successful deliberate no-op outcome for the command boundary.
#[must_use]
pub fn declined_noop(message: impl Into<String>, completion: impl Into<String>) -> anyhow::Error {
    InteractionError::declined(message, Some(completion.into())).into()
}

/// How a timed operation renders its success state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    /// A mid-rail step, because more output follows in the same sequence.
    Step,
    /// The closing outro, because the success is the last thing the rail says.
    ///
    /// The message keeps the accent bar and plain text of an info line rather
    /// than the bold success style, so the rail opens and closes in one voice.
    Outro,
}

/// An active timed observation whose terminal state is selected after the
/// command transition has produced its typed result.
pub(crate) struct TimedProgress {
    started: Instant,
    progress: Option<ProgressBar>,
}

impl TimedProgress {
    /// Starts one timed observation without taking ownership of the operation.
    ///
    /// # Errors
    /// Returns an error when a non-animated starting line cannot be rendered.
    pub(crate) fn start(enabled: bool, starting: &str) -> Result<Self> {
        if !enabled {
            return Ok(Self {
                started: Instant::now(),
                progress: None,
            });
        }
        open_sequence();
        let progress = io::stderr().is_terminal().then(|| {
            let elapsed = muted_style().apply_to("{elapsed}");
            let template = format!("{{msg}} {elapsed}");
            let progress = spinner().with_template(&template);
            progress.start(heading_style().apply_to(starting));
            progress
        });
        if progress.is_none() {
            info(heading_style().apply_to(starting))?;
        }
        Ok(Self {
            started: Instant::now(),
            progress,
        })
    }

    #[must_use]
    pub(crate) const fn animated(&self) -> bool {
        self.progress.is_some()
    }

    /// Renders the one successful terminal state for this observation.
    ///
    /// # Errors
    /// Returns an error when the completed state cannot be rendered.
    pub(crate) fn complete(self, completed: &str, completion: Completion) -> Result<()> {
        let elapsed = muted_style().apply_to(format!("{}s", self.started.elapsed().as_secs()));
        match completion {
            Completion::Step => {
                let message = format!("{} {elapsed}", heading_style().apply_to(completed));
                if let Some(progress) = &self.progress {
                    progress.stop(message);
                    Ok(())
                } else {
                    step(message)
                }
            }
            Completion::Outro => {
                if let Some(progress) = &self.progress {
                    progress.clear();
                }
                finish(format!("{completed} {elapsed}"))
            }
        }
    }

    /// Renders the one failed terminal state for this observation.
    ///
    /// # Errors
    /// Returns an error when the failed state cannot be rendered.
    pub(crate) fn fail(self, failed: &str) -> Result<()> {
        if let Some(progress) = &self.progress {
            progress.error(failed);
            Ok(())
        } else {
            warning(failed)
        }
    }
}

/// Runs a potentially slow operation with a timed human-mode progress indicator.
///
/// The operation receives whether an animated spinner owns stderr. Callers that
/// can otherwise stream subprocess progress should capture it only when this is
/// `true`, avoiding interleaved output while preserving progress on plain stderr.
///
/// # Errors
///
/// Returns the operation error after closing the indicator, or a terminal write
/// error when progress output cannot be rendered.
pub fn run_timed<T>(
    enabled: bool,
    starting: &str,
    completed: &str,
    failed: &str,
    operation: impl FnOnce(bool) -> Result<T>,
) -> Result<T> {
    run_timed_completing(
        enabled,
        starting,
        completed,
        failed,
        Completion::Step,
        operation,
    )
}

/// Runs a timed operation whose success may close the sequence instead of stepping it.
///
/// This still owns exactly one terminal state per run: callers must not render
/// their own progress, step, or outro around it.
///
/// # Errors
///
/// Returns the operation error after closing the indicator, or a terminal write
/// error when progress output cannot be rendered.
pub fn run_timed_completing<T>(
    enabled: bool,
    starting: &str,
    completed: &str,
    failed: &str,
    completion: Completion,
    operation: impl FnOnce(bool) -> Result<T>,
) -> Result<T> {
    if !enabled {
        return operation(false);
    }

    let progress = TimedProgress::start(true, starting)?;
    let result = operation(progress.animated());
    match result {
        Ok(value) => {
            progress.complete(completed, completion)?;
            Ok(value)
        }
        Err(error) => {
            progress.fail(failed)?;
            Err(error)
        }
    }
}

/// Writes an informational message through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn info(message: impl Display) -> Result<()> {
    open_sequence();
    log::info(message).context("failed to write terminal message")
}

/// Writes a successful outcome through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn success(message: impl Display) -> Result<()> {
    open_sequence();
    log::success(success_style().apply_to(message)).context("failed to write terminal message")
}

/// Writes a warning through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn warning(message: impl Display) -> Result<()> {
    open_sequence();
    log::warning(warning_style().apply_to(message)).context("failed to write terminal message")
}

/// Writes an error through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn error(message: impl Display) -> Result<()> {
    open_sequence();
    log::error(error_style().apply_to(message)).context("failed to write terminal message")
}

/// Writes a completed step through the terminal UI.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn step(message: impl Display) -> Result<()> {
    open_sequence();
    log::step(message).context("failed to write terminal message")
}

/// Writes a completed step immediately before inherited subprocess output.
///
/// Unlike a regular Cliclack log, this omits the trailing rail spacer so the
/// subprocess output starts on the next line without an empty framed line.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn step_before_stream(message: impl Display) -> Result<()> {
    open_sequence();
    let theme = PandoTheme;
    let rendered =
        theme.format_log_with_spacing(&message.to_string(), &theme.submit_symbol(), false);
    io::stderr()
        .lock()
        .write_all(rendered.as_bytes())
        .context("failed to write terminal message")
}

/// Writes a terminal UI outro.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn finish(message: impl Display) -> Result<()> {
    outro(message).context("failed to write terminal message")
}

/// Closes an open terminal UI sequence, staying silent when none was opened.
///
/// Commands whose only human-facing output is the outro itself would otherwise
/// print a lone closing bar for a run that reported nothing — noise under the
/// shell integration, which captures stdout and leaves stderr on the terminal.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn finish_open_sequence(message: impl Display) -> Result<()> {
    if SEQUENCE_OPEN.load(Ordering::Relaxed) {
        finish(message)
    } else {
        Ok(())
    }
}

/// Writes a cancellation outro without presenting an operational error.
///
/// # Errors
///
/// Returns an error when the terminal cannot be written.
pub fn cancel(message: impl Display) -> Result<()> {
    outro_cancel(message).context("failed to write terminal cancellation")
}

#[cfg(test)]
mod tests {
    use cliclack::{Theme, ThemeState};

    use super::{PandoTheme, heading_style, interactive};

    #[test]
    fn prompt_headers_use_the_shared_heading_style() {
        let rendered = PandoTheme.format_header(&ThemeState::Active, "Branch name:");

        assert!(
            rendered.contains(
                &interactive(heading_style())
                    .apply_to("Branch name:")
                    .to_string()
            )
        );
    }
}
