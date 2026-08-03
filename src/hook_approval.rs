use anyhow::{Result, bail};

use crate::{
    config::{HookPhase, HookStep},
    git::Repository,
    trust, ui,
};

/// The result of evaluating one configured hook phase against repository trust.
#[derive(Debug, Eq, PartialEq)]
pub enum Evaluation {
    NoCommands,
    Trusted,
    ApprovalRequired(Candidate),
}

/// An evaluated set of hook commands that may be approved for one repository.
#[derive(Debug, Eq, PartialEq)]
pub struct Candidate {
    phase: HookPhase,
    commands: Vec<HookStep>,
    repository: String,
    identity: String,
}

impl Candidate {
    /// Returns the hook phase awaiting approval.
    #[must_use]
    pub fn phase(&self) -> HookPhase {
        self.phase
    }

    /// Returns the exact ordered commands awaiting approval.
    #[must_use]
    pub fn commands(&self) -> &[HookStep] {
        &self.commands
    }

    /// Returns the repository binding used by trust storage.
    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// Returns the executable identity of the ordered commands.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }
}

/// Evaluates whether one hook phase needs explicit approval.
///
/// Empty phases short-circuit without resolving repository identity or reading
/// trust storage.
///
/// # Errors
///
/// Returns an error when repository identity or trust storage cannot be resolved.
pub fn evaluate(
    repository: &Repository,
    phase: HookPhase,
    commands: &[HookStep],
) -> Result<Evaluation> {
    if commands.is_empty() {
        return Ok(Evaluation::NoCommands);
    }
    if trust::is_trusted(repository, phase, commands)? {
        return Ok(Evaluation::Trusted);
    }
    Ok(Evaluation::ApprovalRequired(Candidate {
        phase,
        commands: commands.to_vec(),
        repository: trust::repository_key(repository)?,
        identity: trust::command_hash(phase, commands),
    }))
}

/// Persists a previously evaluated approval candidate.
///
/// # Errors
///
/// Returns an error if the candidate does not belong to this repository or its
/// command identity is inconsistent, or when trust storage cannot be updated.
pub fn approve(repository: &Repository, candidate: &Candidate) -> Result<()> {
    if candidate.repository != trust::repository_key(repository)? {
        bail!("hook approval candidate belongs to a different repository");
    }
    if candidate.identity != trust::command_hash(candidate.phase, &candidate.commands) {
        bail!("hook approval candidate identity no longer matches its commands");
    }
    trust::approve(repository, candidate.phase, &candidate.commands)
}

/// Human interaction adapter for the typed hook approval policy.
///
/// # Errors
///
/// Returns an error when evaluation fails, approval requires a noninteractive
/// terminal, the user declines or cancels, or approval cannot be persisted.
pub fn approve_interactively(
    repository: &Repository,
    phase: HookPhase,
    commands: &[HookStep],
) -> Result<()> {
    let Evaluation::ApprovalRequired(candidate) = evaluate(repository, phase, commands)? else {
        return Ok(());
    };
    ui::ensure_interactive(&format!("{} require approval", phase.plural_name()))?;
    ui::info(format!(
        "The repository requests these {}:",
        phase.plural_name()
    ))?;
    for (index, step) in candidate.commands().iter().enumerate() {
        ui::step(format!("{}: {}", step.label(index), step.command))?;
    }
    let confirmed = ui::prompt_result(
        cliclack::confirm("Trust and run these commands for this repository?")
            .initial_value(false)
            .interact(),
        &format!("{} approval cancelled", phase.plural_name()),
        "failed to read hook approval",
    )?;
    if !confirmed {
        return Err(ui::declined(format!(
            "{} approval declined; no commands were run",
            phase.plural_name()
        )));
    }
    approve(repository, &candidate)
}
