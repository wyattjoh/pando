use std::{
    io::{self, Write},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

pub(crate) fn set_enabled(enabled: bool) {
    STARTED_AT.get_or_init(Instant::now);
    ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        event("diagnostics enabled");
    }
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn event(message: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let elapsed = STARTED_AT.get_or_init(Instant::now).elapsed().as_secs_f64() * 1_000.0;
    let _ = writeln!(io::stderr().lock(), "[verbose +{elapsed:.3}ms] {message}");
}

pub(crate) struct Span {
    label: Option<String>,
    started_at: Instant,
}

impl Span {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        let label = ENABLED.load(Ordering::Relaxed).then(|| label.into());
        if let Some(label) = &label {
            event(&format!("{label} started"));
        }
        Self {
            label,
            started_at: Instant::now(),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(label) = &self.label {
            let elapsed = self.started_at.elapsed().as_secs_f64() * 1_000.0;
            event(&format!("{label} finished in {elapsed:.3}ms"));
        }
    }
}
