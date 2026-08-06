use std::{
    fs,
    io::{self, Read, Write},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, SyncSender},
    thread,
};

use anyhow::{Context, Result};

use crate::{
    config::{HookPhase, HookStep},
    ui,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookOutcome {
    Success,
    Failed(i32),
    Interrupted,
}

pub(crate) const CAPTURE_LIMIT: usize = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Observation {
    StepStarted {
        phase: HookPhase,
        label: String,
        command: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delivery {
    Human,
    Captured,
}

#[derive(Debug)]
pub(crate) struct Observations {
    delivery: Delivery,
    events: Vec<Observation>,
}

impl Observations {
    #[must_use]
    pub(crate) const fn human() -> Self {
        Self {
            delivery: Delivery::Human,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) const fn captured() -> Self {
        Self {
            delivery: Delivery::Captured,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) const fn is_human(&self) -> bool {
        matches!(self.delivery, Delivery::Human)
    }

    pub(crate) fn emit(&mut self, observation: Observation) {
        if self.is_human() {
            let _ = render_observation(&observation);
        }
        self.events.push(observation);
    }

    fn relay(&self) -> Option<fs::File> {
        self.is_human()
            .then(|| fs::OpenOptions::new().write(true).open("/dev/stderr").ok())
            .flatten()
    }

    /// Completes infallible, presentation-only observation delivery.
    #[must_use]
    pub(crate) fn finish(self) -> Vec<Observation> {
        self.events
    }
}

fn render_observation(observation: &Observation) -> Result<()> {
    match observation {
        Observation::StepStarted {
            phase,
            label,
            command,
        } => ui::step(format!("Running {} {label}:\n{command}", phase.key())),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapturedStream {
    pub(crate) content: Vec<u8>,
    pub(crate) original_size: usize,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapturedStep {
    pub(crate) stdout: CapturedStream,
    pub(crate) stderr: CapturedStream,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HookExecution {
    pub(crate) outcome: HookOutcome,
    pub(crate) output: Vec<CapturedStep>,
}

/// Runs an ordered hook phase and records bounded output independently from
/// how its in-flight observations are delivered.
///
/// # Errors
/// Returns an error when a command cannot start, read output, or be awaited.
pub(crate) fn execute(
    phase: HookPhase,
    steps: &[HookStep],
    destination: &Path,
    observations: &mut Observations,
) -> Result<HookExecution> {
    let mut captured = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let label = step.label(index);
        observations.emit(Observation::StepStarted {
            phase,
            label: label.clone(),
            command: step.command.clone(),
        });
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &step.command])
            .current_dir(destination)
            .stdin(Stdio::inherit());
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {} {label}", phase.key()))?;
        let (status, output) = capture_child(&mut child, observations.relay())
            .with_context(|| format!("failed to capture {} {label}", phase.key()))?;
        captured.push(output);
        let outcome = classify(status);
        if outcome != HookOutcome::Success {
            return Ok(HookExecution {
                outcome,
                output: captured,
            });
        }
    }
    Ok(HookExecution {
        outcome: HookOutcome::Success,
        output: captured,
    })
}

fn capture_child(child: &mut Child, relay: Option<fs::File>) -> Result<(ExitStatus, CapturedStep)> {
    let stdout = child.stdout.take().context("captured hook has no stdout")?;
    let stderr = child.stderr.take().context("captured hook has no stderr")?;
    let (stdout_relay, stderr_relay, relay_writer) = relay.map_or_else(
        || (None, None, None),
        |output| {
            let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(8);
            let writer = thread::spawn(move || relay_output(output, &receiver));
            (Some(sender.clone()), Some(sender), Some(writer))
        },
    );
    let stdout_reader = thread::spawn(move || capture_stream(stdout, stdout_relay));
    let stderr_reader = thread::spawn(move || capture_stream(stderr, stderr_relay));
    let status = child.wait().context("failed to await captured hook")?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("captured hook stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("captured hook stderr reader panicked"))??;
    if let Some(writer) = relay_writer {
        let _ = writer.join();
    }
    Ok((status, CapturedStep { stdout, stderr }))
}

fn relay_output(mut output: fs::File, receiver: &mpsc::Receiver<Vec<u8>>) {
    let mut writable = true;
    while let Ok(bytes) = receiver.recv() {
        if writable
            && output
                .write_all(&bytes)
                .and_then(|()| output.flush())
                .is_err()
        {
            writable = false;
        }
    }
}

fn capture_stream(
    mut stream: impl Read,
    mut relay: Option<SyncSender<Vec<u8>>>,
) -> io::Result<CapturedStream> {
    let mut content = Vec::with_capacity(CAPTURE_LIMIT);
    let mut original_size = 0usize;
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if relay
            .as_ref()
            .is_some_and(|output| output.send(buffer[..count].to_vec()).is_err())
        {
            relay = None;
        }
        original_size = original_size.saturating_add(count);
        let remaining = CAPTURE_LIMIT.saturating_sub(content.len());
        content.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(CapturedStream {
        content,
        original_size,
        truncated: original_size > CAPTURE_LIMIT,
    })
}

fn classify(status: ExitStatus) -> HookOutcome {
    if status.success() {
        HookOutcome::Success
    } else if status
        .signal()
        .is_some_and(|signal| matches!(signal, 2 | 3 | 15))
    {
        HookOutcome::Interrupted
    } else {
        HookOutcome::Failed(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(command: &str) -> HookStep {
        HookStep {
            name: None,
            command: command.into(),
        }
    }

    #[test]
    fn captured_execution_bounds_and_attributes_each_stream() {
        let directory = tempfile::tempdir().unwrap();
        let mut observations = Observations::captured();
        let execution = execute(
            HookPhase::PostCreate,
            &[step("printf %020000d 0; printf %020001d 0 >&2")],
            directory.path(),
            &mut observations,
        )
        .unwrap();

        assert_eq!(execution.outcome, HookOutcome::Success);
        let steps = execution.output;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].stdout.content.len(), CAPTURE_LIMIT);
        assert_eq!(steps[0].stdout.original_size, 20_000);
        assert!(steps[0].stdout.truncated);
        assert_eq!(steps[0].stderr.content.len(), CAPTURE_LIMIT);
        assert_eq!(steps[0].stderr.original_size, 20_001);
        assert!(steps[0].stderr.truncated);
    }

    #[test]
    fn captured_execution_stops_after_first_failure() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("later");
        let mut observations = Observations::captured();
        let execution = execute(
            HookPhase::PreRemove,
            &[
                step("printf out; printf err >&2; exit 23"),
                step(&format!("touch {}", marker.display())),
            ],
            directory.path(),
            &mut observations,
        )
        .unwrap();

        assert_eq!(execution.outcome, HookOutcome::Failed(23));
        assert!(!marker.exists());
        let steps = execution.output;
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].stdout.content, b"out");
        assert_eq!(steps[0].stderr.content, b"err");
    }

    #[test]
    fn execution_classifies_eof_and_interruption() {
        let directory = tempfile::tempdir().unwrap();
        let mut eof_observations = Observations::captured();
        let eof = execute(
            HookPhase::PostCreate,
            &[step("if read value </dev/null; then exit 9; fi")],
            directory.path(),
            &mut eof_observations,
        )
        .unwrap();
        let mut interrupted_observations = Observations::captured();
        let interrupted = execute(
            HookPhase::PostCreate,
            &[step("kill -TERM $$")],
            directory.path(),
            &mut interrupted_observations,
        )
        .unwrap();

        assert_eq!(eof.outcome, HookOutcome::Success);
        assert_eq!(interrupted.outcome, HookOutcome::Interrupted);
    }

    #[test]
    fn human_delivery_and_captured_replay_preserve_the_execution_result() {
        let directory = tempfile::tempdir().unwrap();
        let steps = [step("printf event-output; printf event-error >&2")];
        let mut captured = Observations::captured();
        let captured_execution = execute(
            HookPhase::PostCreate,
            &steps,
            directory.path(),
            &mut captured,
        )
        .unwrap();
        let captured_events = captured.finish();

        let mut human = Observations::human();
        let human_execution =
            execute(HookPhase::PostCreate, &steps, directory.path(), &mut human).unwrap();
        let human_events = human.finish();

        assert_eq!(human_execution, captured_execution);
        assert_eq!(human_events, captured_events);

        let mut replay = Observations::captured();
        for event in &captured_events {
            replay.emit(event.clone());
        }
        assert_eq!(replay.finish(), captured_events);
    }

    #[test]
    fn empty_phase_succeeds_without_requiring_a_valid_destination() {
        let mut observations = Observations::captured();
        let execution = execute(
            HookPhase::PreMerge,
            &[],
            Path::new("/path/that/does/not/exist"),
            &mut observations,
        )
        .unwrap();

        assert_eq!(execution.outcome, HookOutcome::Success);
        assert!(execution.output.is_empty());
    }
}
