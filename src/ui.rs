use std::fmt::Display;

use anyhow::{Context, Result};
use cliclack::{log, outro};

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
