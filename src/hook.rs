use std::{
    fs,
    io::{self, Read},
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputPolicy {
    Streamed,
    Captured,
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
pub(crate) enum HookOutput {
    Streamed,
    Captured(Vec<CapturedStep>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HookExecution {
    pub(crate) outcome: HookOutcome,
    pub(crate) output: HookOutput,
}

/// Runs an ordered hook phase from the destination under one closed output policy.
///
/// # Errors
/// Returns an error when a command cannot start, stream output, or be awaited.
pub(crate) fn execute(
    phase: HookPhase,
    steps: &[HookStep],
    destination: &Path,
    policy: OutputPolicy,
) -> Result<HookExecution> {
    let mut captured = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let label = step.label(index);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &step.command]).current_dir(destination);

        let (status, output) = match policy {
            OutputPolicy::Streamed => {
                ui::step(format!(
                    "Running {} {label}:\n{}",
                    phase.key(),
                    step.command
                ))?;
                let hook_stdout = fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/stderr")
                    .with_context(|| format!("failed to open stderr for {} output", phase.key()))?;
                let status = command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::from(hook_stdout))
                    .stderr(Stdio::inherit())
                    .status()
                    .with_context(|| format!("failed to run {} {label}", phase.key()))?;
                (status, None)
            }
            OutputPolicy::Captured => {
                let mut child = command
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .with_context(|| format!("failed to run {} {label}", phase.key()))?;
                let (status, output) = capture_child(&mut child)
                    .with_context(|| format!("failed to capture {} {label}", phase.key()))?;
                (status, Some(output))
            }
        };
        if let Some(output) = output {
            captured.push(output);
        }
        let outcome = classify(status);
        if outcome != HookOutcome::Success {
            return Ok(HookExecution {
                outcome,
                output: output_for(policy, captured),
            });
        }
    }
    Ok(HookExecution {
        outcome: HookOutcome::Success,
        output: output_for(policy, captured),
    })
}

fn capture_child(child: &mut Child) -> Result<(ExitStatus, CapturedStep)> {
    let stdout = child.stdout.take().context("captured hook has no stdout")?;
    let stderr = child.stderr.take().context("captured hook has no stderr")?;
    let stdout_reader = thread::spawn(move || capture_stream(stdout));
    let stderr_reader = thread::spawn(move || capture_stream(stderr));
    let status = child.wait().context("failed to await captured hook")?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("captured hook stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("captured hook stderr reader panicked"))??;
    Ok((status, CapturedStep { stdout, stderr }))
}

fn capture_stream(mut stream: impl Read) -> io::Result<CapturedStream> {
    let mut content = Vec::with_capacity(CAPTURE_LIMIT);
    let mut original_size = 0usize;
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
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

fn output_for(policy: OutputPolicy, captured: Vec<CapturedStep>) -> HookOutput {
    match policy {
        OutputPolicy::Streamed => HookOutput::Streamed,
        OutputPolicy::Captured => HookOutput::Captured(captured),
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
        let execution = execute(
            HookPhase::PostCreate,
            &[step("printf %020000d 0; printf %020001d 0 >&2")],
            directory.path(),
            OutputPolicy::Captured,
        )
        .unwrap();

        assert_eq!(execution.outcome, HookOutcome::Success);
        let HookOutput::Captured(steps) = execution.output else {
            panic!("expected captured output");
        };
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
        let execution = execute(
            HookPhase::PreRemove,
            &[
                step("printf out; printf err >&2; exit 23"),
                step(&format!("touch {}", marker.display())),
            ],
            directory.path(),
            OutputPolicy::Captured,
        )
        .unwrap();

        assert_eq!(execution.outcome, HookOutcome::Failed(23));
        assert!(!marker.exists());
        let HookOutput::Captured(steps) = execution.output else {
            panic!("expected captured output");
        };
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].stdout.content, b"out");
        assert_eq!(steps[0].stderr.content, b"err");
    }

    #[test]
    fn captured_execution_disconnects_stdin_and_classifies_interruption() {
        let directory = tempfile::tempdir().unwrap();
        let eof = execute(
            HookPhase::PostCreate,
            &[step("if read value; then exit 9; fi")],
            directory.path(),
            OutputPolicy::Captured,
        )
        .unwrap();
        let interrupted = execute(
            HookPhase::PostCreate,
            &[step("kill -TERM $$")],
            directory.path(),
            OutputPolicy::Captured,
        )
        .unwrap();

        assert_eq!(eof.outcome, HookOutcome::Success);
        assert_eq!(interrupted.outcome, HookOutcome::Interrupted);
    }

    #[test]
    fn empty_phase_succeeds_without_requiring_a_valid_destination() {
        let execution = execute(
            HookPhase::PreMerge,
            &[],
            Path::new("/path/that/does/not/exist"),
            OutputPolicy::Captured,
        )
        .unwrap();

        assert_eq!(execution.outcome, HookOutcome::Success);
        assert_eq!(execution.output, HookOutput::Captured(Vec::new()));
    }
}
