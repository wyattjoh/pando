//! Bounded subprocess execution shared by configured text generators.

use std::{
    io::{self, Read, Write},
    process::{Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

pub(crate) const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) stdout: usize,
    pub(crate) stderr: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            stdout: OUTPUT_LIMIT,
            stderr: OUTPUT_LIMIT,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Capture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) original_size: usize,
    pub(crate) truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ErrorKind {
    Spawn,
    Pipe,
    Stdin,
    Stdout,
    Stderr,
    StdoutLimit,
    StderrLimit,
    Wait,
}

impl ErrorKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Spawn => "generator.spawn_failed",
            Self::Pipe => "generator.pipe_unavailable",
            Self::Stdin => "generator.stdin_failed",
            Self::Stdout => "generator.stdout_failed",
            Self::Stderr => "generator.stderr_failed",
            Self::StdoutLimit => "generator.stdout_limit",
            Self::StderrLimit => "generator.stderr_limit",
            Self::Wait => "generator.wait_failed",
        }
    }
}

#[derive(Debug)]
pub(crate) struct RunError {
    pub(crate) kind: ErrorKind,
    pub(crate) message: String,
    pub(crate) stdout: Capture,
    pub(crate) stderr: Capture,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.kind.label(), self.message)
    }
}

impl std::error::Error for RunError {}

#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Capture,
    pub(crate) stderr: Capture,
}

#[allow(clippy::too_many_lines)] // One coordinator owns spawn, cancellation, reaping, and worker results.
pub(crate) fn run(
    command: &mut Command,
    prompt: &[u8],
    limits: Limits,
) -> Result<Output, RunError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| RunError {
        kind: ErrorKind::Spawn,
        message: format!("failed to start generator: {error}"),
        stdout: Capture::default(),
        stderr: Capture::default(),
    })?;
    let Some(stdin) = child.stdin.take() else {
        terminate(&mut child);
        return Err(pipe_error("stdin"));
    };
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        return Err(pipe_error("stdout"));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate(&mut child);
        return Err(pipe_error("stderr"));
    };

    let (cancel_tx, cancel_rx) = mpsc::channel();
    let writer_cancel = cancel_tx.clone();
    let prompt = prompt.to_vec();
    let writer = thread::spawn(move || {
        let result = write_prompt(stdin, &prompt);
        if result.is_err() {
            let _ = writer_cancel.send(());
        }
        result
    });
    let stdout_cancel = cancel_tx.clone();
    let stdout_reader = thread::spawn(move || capture(stdout, limits.stdout, &stdout_cancel));
    let stderr_reader = thread::spawn(move || capture(stderr, limits.stderr, &cancel_tx));

    let mut wait_error = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                wait_error = Some(error.to_string());
                terminate(&mut child);
                break None;
            }
        }
        match cancel_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(()) => {
                let _ = child.kill();
                match child.wait() {
                    Ok(status) => break Some(status),
                    Err(error) => {
                        wait_error = Some(error.to_string());
                        break None;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(10)),
        }
    };

    let writer = writer
        .join()
        .unwrap_or_else(|_| Err("generator stdin worker panicked".into()));
    let stdout = stdout_reader.join().unwrap_or_else(|_| ReaderResult {
        capture: Capture::default(),
        error: Some("generator stdout worker panicked".into()),
    });
    let stderr = stderr_reader.join().unwrap_or_else(|_| ReaderResult {
        capture: Capture::default(),
        error: Some("generator stderr worker panicked".into()),
    });

    if stdout.capture.truncated {
        return Err(run_error(
            ErrorKind::StdoutLimit,
            format!("generator stdout exceeded the {}-byte limit", limits.stdout),
            stdout.capture,
            stderr.capture,
        ));
    }
    if stderr.capture.truncated {
        return Err(run_error(
            ErrorKind::StderrLimit,
            format!("generator stderr exceeded the {}-byte limit", limits.stderr),
            stdout.capture,
            stderr.capture,
        ));
    }
    if let Some(error) = stdout.error {
        return Err(run_error(
            ErrorKind::Stdout,
            format!("failed to read generator stdout: {error}"),
            stdout.capture,
            stderr.capture,
        ));
    }
    if let Some(error) = stderr.error {
        return Err(run_error(
            ErrorKind::Stderr,
            format!("failed to read generator stderr: {error}"),
            stdout.capture,
            stderr.capture,
        ));
    }
    if let Err(error) = writer {
        return Err(run_error(
            ErrorKind::Stdin,
            format!("failed to write generator stdin: {error}"),
            stdout.capture,
            stderr.capture,
        ));
    }
    if let Some(error) = wait_error {
        return Err(run_error(
            ErrorKind::Wait,
            format!("failed to await generator: {error}"),
            stdout.capture,
            stderr.capture,
        ));
    }
    Ok(Output {
        status: status.expect("a generator without a wait error has an exit status"),
        stdout: stdout.capture,
        stderr: stderr.capture,
    })
}

#[derive(Debug)]
struct ReaderResult {
    capture: Capture,
    error: Option<String>,
}

fn capture(mut reader: impl Read, limit: usize, cancel: &mpsc::Sender<()>) -> ReaderResult {
    let mut capture = Capture::default();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                capture.original_size = capture.original_size.saturating_add(read);
                let remaining = limit.saturating_sub(capture.bytes.len());
                capture
                    .bytes
                    .extend_from_slice(&buffer[..read.min(remaining)]);
                if read > remaining && !capture.truncated {
                    capture.truncated = true;
                    let _ = cancel.send(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                let _ = cancel.send(());
                return ReaderResult {
                    capture,
                    error: Some(error.to_string()),
                };
            }
        }
    }
    ReaderResult {
        capture,
        error: None,
    }
}

fn write_prompt(mut stdin: impl Write, prompt: &[u8]) -> Result<(), String> {
    match stdin.write_all(prompt) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn pipe_error(stream: &str) -> RunError {
    run_error(
        ErrorKind::Pipe,
        format!("generator {stream} pipe is unavailable"),
        Capture::default(),
        Capture::default(),
    )
}

fn run_error(kind: ErrorKind, message: String, stdout: Capture, stderr: Capture) -> RunError {
    RunError {
        kind,
        message,
        stdout,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        process::Command,
        time::{Duration, Instant},
    };

    use super::{ErrorKind, Limits, run};

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn drains_output_while_delivering_the_prompt() {
        let prompt = vec![b'p'; 256 * 1024];
        let mut command =
            shell("dd if=/dev/zero bs=1024 count=128 2>/dev/null; cat >/dev/null; printf done");

        let output = run(
            &mut command,
            &prompt,
            Limits {
                stdout: 256 * 1024,
                stderr: 1024,
            },
        )
        .unwrap();

        assert!(output.status.success());
        assert_eq!(output.stdout.original_size, 128 * 1024 + 4);
        assert!(output.stdout.bytes.ends_with(b"done"));
        assert_eq!(output.stderr.original_size, 0);
    }

    #[test]
    fn bounds_streams_independently() {
        for (script, expected) in [
            (
                "dd if=/dev/zero bs=1024 count=80 2>/dev/null",
                ErrorKind::StdoutLimit,
            ),
            (
                "dd if=/dev/zero bs=1024 count=80 2>/dev/null | cat >&2",
                ErrorKind::StderrLimit,
            ),
        ] {
            let mut command = shell(script);
            let error = run(
                &mut command,
                b"",
                Limits {
                    stdout: 64 * 1024,
                    stderr: 64 * 1024,
                },
            )
            .unwrap_err();

            assert_eq!(error.kind, expected);
            assert!(error.stdout.bytes.len() <= 64 * 1024);
            assert!(error.stderr.bytes.len() <= 64 * 1024);
            match expected {
                ErrorKind::StdoutLimit => assert!(error.stdout.truncated),
                ErrorKind::StderrLimit => assert!(error.stderr.truncated),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn overflow_kills_and_reaps_the_child() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let script = format!(
            "printf '%s' $$ > '{}'; dd if=/dev/zero bs=1024 count=80 2>/dev/null; sleep 30",
            pid_file.display()
        );
        let mut command = shell(&script);
        let started = Instant::now();

        let error = run(
            &mut command,
            b"",
            Limits {
                stdout: 1024,
                stderr: 1024,
            },
        )
        .unwrap_err();

        assert_eq!(error.kind, ErrorKind::StdoutLimit);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let reaped = Command::new("/bin/sh")
            .args(["-c", &format!("! kill -0 {pid} 2>/dev/null")])
            .status()
            .unwrap();
        assert!(reaped.success(), "generator process {pid} is still alive");
    }
}
