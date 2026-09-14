//! Interactive worktree cleanup.
//!
//! `clean` is a human-only adapter. It observes the repository, measures how
//! much disk each topic worktree occupies, lets the user check the ones to
//! discard, and hands the selected branches to [`crate::lifecycle`], which owns
//! every removal decision, hook approval, and Git mutation. Nothing here plans
//! or performs a removal of its own.
//!
//! Disk measurement is a background observation: it fills the picker's size
//! column while the user reads, never authorizes or blocks a removal, and is
//! cancelled and joined before any mutation begins.

use std::{
    collections::HashSet,
    env,
    fmt::{self, Write as _},
    fs,
    io::{self, Write as _},
    num::NonZeroUsize,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use console::{Key, Term, strip_ansi_codes};

use crate::{
    Condition, Row, SortMode, Worktree, WorktreeKind, config::EffectiveConfig,
    git::RepositoryObservation, lifecycle, render, sorted_row_indices, ui,
};

/// `st_blocks` is reported in 512-byte units on every supported platform.
const BLOCK_SIZE: u64 = 512;
const CLEAN_FRAME_ROWS: usize = 6;
const CHOICE_PREFIX: &str = "      ";

/// Runs the cleanup picker and removes whatever the user confirms.
///
/// # Errors
///
/// Returns an error when the repository cannot be observed, when no interactive
/// terminal is available, when the user cancels, or when removal fails. A run
/// that deliberately removes nothing returns a successful no-op outcome.
pub fn run(dry_run: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = RepositoryObservation::new(&cwd).repository_with_metadata()?;
    let primary = repository
        .primary
        .clone()
        .context("cannot clean worktrees in a bare repository")?;
    if let Some(warning) = &repository.metadata_warning {
        ui::warning(warning)?;
    }
    let candidates: Vec<Candidate> = repository
        .worktrees
        .iter()
        .filter(|worktree| worktree.path != primary)
        .map(Candidate::new)
        .collect();
    if candidates.is_empty() {
        return Err(ui::declined_noop(
            "This repository has no topic worktrees to clean.",
            "Nothing to clean.",
        ));
    }
    if !ui::is_interactive() {
        anyhow::bail!(
            "worktree cleanup requires an interactive terminal; use `pando remove <branch>` to remove worktrees without one"
        );
    }
    let sort = EffectiveConfig::load_default_sort(&repository)?;
    let excluded: HashSet<PathBuf> = repository
        .worktrees
        .iter()
        .map(|worktree| worktree.path.clone())
        .collect();

    let mut picker = CleanPicker::new(candidates, sort);
    let (events, incoming) = mpsc::channel();
    let sizer = Sizer::start(picker.measurement_tasks(), excluded, &events);
    let keys = Keys::start(events);
    let selection = picker.interact(&incoming, &keys);
    keys.stop();
    sizer.stop();
    let selected = ui::prompt_result(
        selection,
        "cleanup cancelled",
        "failed to read the worktree selection from the terminal",
    )?;
    if selected.is_empty() {
        return Err(ui::declined_noop(
            "No worktrees were selected.",
            "Nothing removed.",
        ));
    }
    apply(&picker, &selected, dry_run)
}

fn apply(picker: &CleanPicker, selected: &[usize], dry_run: bool) -> Result<()> {
    let chosen: Vec<&Candidate> = selected
        .iter()
        .map(|index| &picker.candidates[*index])
        .collect();
    let branches: Vec<String> = chosen
        .iter()
        .filter_map(|candidate| candidate.branch().map(ToOwned::to_owned))
        .collect();
    // Only a selection that already looked dirty asks removal to discard work.
    // A worktree that turned dirty after observation fails preflight instead,
    // because the confirmation the user answered never named it.
    let force = chosen.iter().any(|candidate| candidate.row.is_dirty());
    let reclaim = Reclaim::of(&chosen);
    let dirty: Vec<&str> = chosen
        .iter()
        .filter(|candidate| candidate.row.is_dirty())
        .map(|candidate| candidate.row.label.as_str())
        .collect();
    if !dirty.is_empty() {
        ui::warning(format!(
            "Removing {} discards uncommitted changes.",
            join_labels(&dirty)
        ))?;
    }
    let prompt = format!(
        "Remove {} worktree{} and reclaim {reclaim}?",
        branches.len(),
        plural(branches.len())
    );
    let confirmed = ui::prompt_result(
        cliclack::confirm(prompt).initial_value(false).interact(),
        "cleanup cancelled",
        "failed to read the cleanup confirmation from the terminal",
    )?;
    if !confirmed {
        return Err(ui::declined_noop("Cleanup declined.", "Nothing removed."));
    }
    if dry_run {
        ui::info(format!("Would reclaim {reclaim}."))?;
        return lifecycle::remove_dry_run(&branches, force);
    }
    lifecycle::remove_summarized(&branches, force, Some(&format!("Reclaimed {reclaim}.")))
}

fn join_labels(labels: &[&str]) -> String {
    match labels {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// One selectable row: a topic worktree plus everything the picker shows for it.
struct Candidate {
    worktree: Worktree,
    row: Row,
    filter: String,
    /// Why the worktree cannot be removed, or `None` when it can.
    blocker: Option<String>,
    selected: bool,
    size: Size,
}

impl Candidate {
    fn new(worktree: &Worktree) -> Self {
        Self {
            row: Row::from_worktree(worktree),
            filter: format!(
                "{} {} {}",
                worktree.branch_label(),
                worktree.state_label(),
                worktree.path.display()
            ),
            blocker: blocker(worktree),
            selected: false,
            size: Size::Pending,
            worktree: worktree.clone(),
        }
    }

    fn branch(&self) -> Option<&str> {
        match &self.worktree.kind {
            WorktreeKind::Branch(branch) => Some(branch),
            WorktreeKind::Detached | WorktreeKind::Bare | WorktreeKind::Unknown => None,
        }
    }

    const fn selectable(&self) -> bool {
        self.blocker.is_none()
    }
}

/// Reports why a worktree cannot be removed, mirroring removal preflight.
///
/// This only decides what the picker offers. Removal re-derives every blocker
/// from fresh repository facts, so a worktree that changes between selection
/// and mutation is refused there rather than here.
fn blocker(worktree: &Worktree) -> Option<String> {
    if !matches!(worktree.kind, WorktreeKind::Branch(_)) {
        return Some(worktree.branch_label().trim_matches(['(', ')']).to_owned());
    }
    if worktree.locked.is_some()
        || worktree.prunable.is_some()
        || !matches!(worktree.condition, Condition::Clean | Condition::Dirty)
    {
        return Some(worktree.state_label());
    }
    None
}

/// How much disk one worktree occupies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Size {
    /// Not measured yet; a worker is still walking the directory.
    Pending,
    Measured {
        bytes: u64,
        /// Whether an unreadable subtree was skipped, so the total under-reports.
        partial: bool,
    },
    /// The directory could not be read at all.
    Unavailable,
}

impl Size {
    const fn bytes(self) -> Option<u64> {
        match self {
            Self::Measured { bytes, .. } => Some(bytes),
            Self::Pending | Self::Unavailable => None,
        }
    }
}

impl fmt::Display for Size {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => formatter.write_str("…"),
            Self::Unavailable => formatter.write_str("—"),
            Self::Measured { bytes, partial } => {
                if *partial {
                    formatter.write_str("~")?;
                }
                formatter.write_str(&format_bytes(*bytes))
            }
        }
    }
}

/// The total a confirmed cleanup expects to return to the filesystem.
struct Reclaim {
    bytes: u64,
    /// Whether any selected worktree was unmeasured or only partly readable.
    approximate: bool,
}

impl Reclaim {
    fn of(selected: &[&Candidate]) -> Self {
        let mut bytes: u64 = 0;
        let mut approximate = false;
        for candidate in selected {
            match candidate.size {
                Size::Measured {
                    bytes: measured,
                    partial,
                } => {
                    bytes = bytes.saturating_add(measured);
                    approximate |= partial;
                }
                Size::Pending | Size::Unavailable => approximate = true,
            }
        }
        Self { bytes, approximate }
    }
}

impl fmt::Display for Reclaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.approximate {
            formatter.write_str("~")?;
        }
        formatter.write_str(&format_bytes(self.bytes))
    }
}

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

// The scaled value is only ever rendered to one decimal place, so the precision
// lost converting a byte count to a float can never change what is displayed.
#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// What the picker is waiting on: the next keystroke or the next measurement.
enum Event {
    Key(io::Result<Key>),
    Measured(usize, Size),
}

/// Bounded background measurement of every listed worktree.
struct Sizer {
    cancel: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl Sizer {
    fn start(
        tasks: Vec<(usize, PathBuf)>,
        excluded: HashSet<PathBuf>,
        events: &Sender<Event>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        if tasks.is_empty() {
            return Self {
                cancel,
                workers: Vec::new(),
            };
        }
        let count = thread::available_parallelism()
            .map_or(1, NonZeroUsize::get)
            .min(tasks.len());
        let tasks = Arc::new(tasks);
        let excluded = Arc::new(excluded);
        let cursor = Arc::new(AtomicUsize::new(0));
        let workers = (0..count)
            .map(|_| {
                let tasks = Arc::clone(&tasks);
                let excluded = Arc::clone(&excluded);
                let cursor = Arc::clone(&cursor);
                let cancel = Arc::clone(&cancel);
                let events = events.clone();
                thread::spawn(move || {
                    while let Some((index, path)) =
                        tasks.get(cursor.fetch_add(1, Ordering::Relaxed))
                    {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let Some(size) = measure(path, &excluded, &cancel) else {
                            break;
                        };
                        if events.send(Event::Measured(*index, size)).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        Self { cancel, workers }
    }

    /// Cancels every worker and waits for it, so no walk is live during mutation.
    fn stop(self) {
        self.cancel.store(true, Ordering::Relaxed);
        for worker in self.workers {
            let _ = worker.join();
        }
    }
}

/// Sums the disk a worktree occupies, excluding nested registered worktrees.
///
/// Allocated blocks rather than apparent file lengths, so the total is the space
/// a removal actually returns; a file reached through a second hard link is
/// counted once; symlinks are never followed. Unreadable subtrees are skipped
/// and reported as a partial total rather than failing the measurement.
///
/// Returns `None` when cancellation interrupted the walk, which discards the
/// incomplete total instead of presenting it.
fn measure(root: &Path, excluded: &HashSet<PathBuf>, cancel: &AtomicBool) -> Option<Size> {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return Some(Size::Unavailable);
    };
    if !metadata.is_dir() {
        return Some(Size::Unavailable);
    }
    let mut bytes = metadata.blocks().saturating_mul(BLOCK_SIZE);
    let mut partial = false;
    let mut linked: HashSet<(u64, u64)> = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            partial = true;
            continue;
        };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let Ok(entry) = entry else {
                partial = true;
                continue;
            };
            // `DirEntry::metadata` does not traverse a symlink, so a link is
            // counted as the link itself and never as the tree it points at.
            let Ok(metadata) = entry.metadata() else {
                partial = true;
                continue;
            };
            let path = entry.path();
            if metadata.is_dir() {
                if excluded.contains(&path) {
                    continue;
                }
                pending.push(path);
            } else if metadata.nlink() > 1 && !linked.insert((metadata.dev(), metadata.ino())) {
                continue;
            }
            bytes = bytes.saturating_add(metadata.blocks().saturating_mul(BLOCK_SIZE));
        }
    }
    Some(Size::Measured { bytes, partial })
}

/// Reads one key per request so no thread holds the terminal between frames.
///
/// The picker asks for a key and then waits on the shared channel, which also
/// carries measurements. Reading on demand means that once the picker stops
/// asking, no thread is parked inside a terminal read — leaving stdin free for
/// the confirmation prompt that follows.
struct Keys {
    requests: Sender<()>,
    reader: JoinHandle<()>,
}

impl Keys {
    fn start(events: Sender<Event>) -> Self {
        let (requests, pending) = mpsc::channel::<()>();
        let reader = thread::spawn(move || {
            let term = Term::stderr();
            while pending.recv().is_ok() {
                if events.send(Event::Key(term.read_key_raw())).is_err() {
                    break;
                }
            }
        });
        Self { requests, reader }
    }

    fn request(&self) -> io::Result<()> {
        self.requests
            .send(())
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn stop(self) {
        drop(self.requests);
        let _ = self.reader.join();
    }
}

/// The cleanup picker's ordering.
///
/// Extends the shared sort modes with a size mode this command alone can
/// order by: size is measured per run and is never a configurable default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanSort {
    Shared(SortMode),
    Size,
}

impl CleanSort {
    const fn next(self) -> Self {
        match self {
            Self::Shared(SortMode::Path) => Self::Size,
            Self::Shared(mode) => Self::Shared(mode.next()),
            Self::Size => Self::Shared(SortMode::Git),
        }
    }

    const fn shared(self) -> Option<SortMode> {
        match self {
            Self::Shared(mode) => Some(mode),
            Self::Size => None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Shared(mode) => mode.label(),
            Self::Size => "size largest-first",
        }
    }
}

struct CleanPicker {
    candidates: Vec<Candidate>,
    sort: CleanSort,
    order: Vec<usize>,
    cursor: usize,
    filter: String,
    viewport_rows: usize,
    terminal_columns: Option<usize>,
}

impl CleanPicker {
    fn new(candidates: Vec<Candidate>, sort: SortMode) -> Self {
        let mut picker = Self {
            candidates,
            sort: CleanSort::Shared(sort),
            order: Vec::new(),
            cursor: 0,
            filter: String::new(),
            viewport_rows: 20,
            terminal_columns: None,
        };
        picker.reorder();
        picker
    }

    fn measurement_tasks(&self) -> Vec<(usize, PathBuf)> {
        self.candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| (index, candidate.worktree.path.clone()))
            .collect()
    }

    /// Recomputes the display order for the current sort.
    ///
    /// Size ordering is recomputed only here — never when a measurement lands —
    /// so rows cannot shuffle under the cursor while the user is selecting.
    fn reorder(&mut self) {
        let rows: Vec<&Row> = self
            .candidates
            .iter()
            .map(|candidate| &candidate.row)
            .collect();
        self.order = match self.sort {
            CleanSort::Shared(mode) => sorted_row_indices(&rows, mode),
            CleanSort::Size => {
                let mut order: Vec<usize> = (0..self.candidates.len()).collect();
                order.sort_by_key(|index| {
                    // Unmeasured rows sort last; measured rows sort largest first.
                    let bytes = self.candidates[*index].size.bytes();
                    (
                        bytes.is_none(),
                        std::cmp::Reverse(bytes.unwrap_or(0)),
                        *index,
                    )
                });
                order
            }
        };
    }

    fn labels(&self) -> (String, Vec<String>) {
        let rows: Vec<&Row> = self
            .candidates
            .iter()
            .map(|candidate| &candidate.row)
            .collect();
        let sizes: Vec<String> = self
            .candidates
            .iter()
            .map(|candidate| candidate.size.to_string())
            .collect();
        render::sized_menu(&rows, &sizes, &self.order, self.sort.shared())
    }

    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.order
            .iter()
            .copied()
            .filter(|index| {
                self.candidates[*index]
                    .filter
                    .to_lowercase()
                    .contains(&needle)
            })
            .collect()
    }

    fn selected(&self) -> Vec<usize> {
        self.order
            .iter()
            .copied()
            .filter(|index| self.candidates[*index].selected)
            .collect()
    }

    fn interact(&mut self, events: &Receiver<Event>, keys: &Keys) -> io::Result<Vec<usize>> {
        let term = Term::stderr();
        if !term.is_term() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        term.hide_cursor()?;
        let result = self.interact_inner(&term, events, keys);
        term.show_cursor()?;
        result
    }

    fn interact_inner(
        &mut self,
        mut term: &Term,
        events: &Receiver<Event>,
        keys: &Keys,
    ) -> io::Result<Vec<usize>> {
        let mut previous_frame = String::new();
        let mut awaiting_key = false;
        loop {
            let (rows, columns) = term.size();
            self.viewport_rows = usize::from(rows).saturating_sub(CLEAN_FRAME_ROWS).max(1);
            self.terminal_columns = (columns > 0).then_some(usize::from(columns));
            let visible = self.visible();
            if self.cursor >= visible.len() {
                self.cursor = 0;
            }
            let start = self.displayed_start(&visible);
            let frame = self.render(&visible, start);
            term.clear_last_lines(ui::rendered_physical_rows(
                &previous_frame,
                self.terminal_columns,
            ))?;
            term.write_all(frame.as_bytes())?;
            term.flush()?;
            previous_frame = frame;

            if !awaiting_key {
                keys.request()?;
                awaiting_key = true;
            }
            let key = match events.recv() {
                Ok(Event::Measured(index, size)) => {
                    self.candidates[index].size = size;
                    continue;
                }
                Ok(Event::Key(key)) => {
                    awaiting_key = false;
                    key?
                }
                Err(_) => return Err(io::ErrorKind::BrokenPipe.into()),
            };
            match key {
                // `console` reports Ctrl-A as Home on Unix.
                Key::Home => self.toggle_all(&visible),
                Key::Char(' ') => self.toggle(&visible),
                Key::Char('\u{13}') => self.cycle_sort(),
                Key::ArrowUp if self.cursor > 0 => self.cursor -= 1,
                Key::ArrowDown if self.cursor + 1 < visible.len() => self.cursor += 1,
                Key::Enter => return Ok(self.selected()),
                Key::Escape | Key::CtrlC => return Err(io::ErrorKind::Interrupted.into()),
                Key::Backspace => {
                    self.filter.pop();
                    self.cursor = 0;
                }
                Key::Char(character) if !character.is_control() => {
                    self.filter.push(character);
                    self.cursor = 0;
                }
                _ => {}
            }
        }
    }

    fn toggle(&mut self, visible: &[usize]) {
        let Some(index) = visible.get(self.cursor).copied() else {
            return;
        };
        let candidate = &mut self.candidates[index];
        if candidate.selectable() {
            candidate.selected = !candidate.selected;
        }
    }

    /// Selects every selectable visible row, or clears them when all are selected.
    fn toggle_all(&mut self, visible: &[usize]) {
        let selectable: Vec<usize> = visible
            .iter()
            .copied()
            .filter(|index| self.candidates[*index].selectable())
            .collect();
        let select = !selectable
            .iter()
            .all(|index| self.candidates[*index].selected);
        for index in selectable {
            self.candidates[index].selected = select;
        }
    }

    fn cycle_sort(&mut self) {
        let focused = self.visible().get(self.cursor).copied();
        self.sort = self.sort.next();
        self.reorder();
        self.cursor = focused
            .and_then(|focused| self.visible().iter().position(|index| *index == focused))
            .unwrap_or(0);
    }

    fn displayed_start(&self, visible: &[usize]) -> usize {
        let last_page_start = if visible.len() > self.viewport_rows {
            visible
                .len()
                .saturating_sub(self.viewport_rows.saturating_sub(1))
        } else {
            0
        };
        self.cursor
            .saturating_sub(self.viewport_rows.saturating_sub(2))
            .min(last_page_start)
    }

    fn render(&self, visible: &[usize], start: usize) -> String {
        let (header, labels) = self.labels();
        let mut output = String::new();
        self.write_line(
            &mut output,
            &format!("{}  ", ui::interactive(ui::accent_style()).apply_to("◆")),
            &ui::interactive(ui::heading_style())
                .apply_to(format!(
                    "Select worktrees to remove ({})",
                    self.sort.label()
                ))
                .to_string(),
        );
        let bar = format!("{}  ", ui::interactive(ui::accent_style()).apply_to("│"));
        let filter = if self.filter.is_empty() {
            "type to filter"
        } else {
            &self.filter
        };
        self.write_line(
            &mut output,
            &bar,
            &ui::interactive(ui::muted_style())
                .apply_to(filter)
                .to_string(),
        );
        self.write_line(
            &mut output,
            &bar,
            &ui::interactive(ui::muted_style())
                .apply_to(format!("{CHOICE_PREFIX}{header}"))
                .to_string(),
        );
        if visible.is_empty() {
            self.write_line(
                &mut output,
                &bar,
                &ui::interactive(ui::muted_style())
                    .apply_to("No worktrees match this filter")
                    .to_string(),
            );
        } else if start > 0 {
            self.write_line(
                &mut output,
                &bar,
                &ui::interactive(ui::muted_style())
                    .apply_to(format!("↑ {start} more above"))
                    .to_string(),
            );
        }
        let displayed = self.displayed(visible, start);
        for (position, index) in displayed.iter().enumerate() {
            self.render_choice(&mut output, *index, &labels[*index], start + position);
        }
        let below = visible.len().saturating_sub(start + displayed.len());
        if below > 0 {
            self.write_line(
                &mut output,
                &bar,
                &ui::interactive(ui::muted_style())
                    .apply_to(format!("↓ {below} more below"))
                    .to_string(),
            );
        }
        self.render_footer(&mut output);
        output
    }

    fn displayed<'visible>(&self, visible: &'visible [usize], start: usize) -> &'visible [usize] {
        let has_above = start > 0;
        let before_bottom_hint = self.viewport_rows.saturating_sub(usize::from(has_above));
        let has_below = visible.len() > start + before_bottom_hint;
        let rows = before_bottom_hint.saturating_sub(usize::from(has_below));
        &visible[start.min(visible.len())..visible.len().min(start + rows)]
    }

    fn render_choice(&self, output: &mut String, index: usize, label: &str, position: usize) {
        let candidate = &self.candidates[index];
        let focused = position == self.cursor;
        let cursor = if focused {
            ui::interactive(ui::accent_style()).apply_to("●")
        } else {
            ui::interactive(ui::muted_style()).apply_to("○")
        };
        let checkbox = match (candidate.selectable(), candidate.selected) {
            (false, _) => ui::interactive(ui::muted_style()).apply_to("✕"),
            (true, true) => ui::interactive(ui::accent_style().bold()).apply_to("◼"),
            (true, false) => ui::interactive(ui::muted_style()).apply_to("◻"),
        };
        let current = if candidate.worktree.current {
            ui::interactive(ui::accent_style().bold())
                .apply_to("*")
                .to_string()
        } else {
            " ".to_owned()
        };
        let mut content = if focused {
            ui::interactive(ui::selected_style())
                .apply_to(strip_ansi_codes(label))
                .to_string()
        } else {
            label.to_owned()
        };
        if let Some(blocker) = &candidate.blocker {
            write!(
                content,
                "  {}",
                ui::interactive(ui::warning_style()).apply_to(blocker)
            )
            .expect("writing to a string cannot fail");
        }
        let prefix = format!(
            "{}  {cursor} {checkbox} {current} ",
            ui::interactive(ui::accent_style()).apply_to("│"),
        );
        self.write_line(output, &prefix, &content);
    }

    fn render_footer(&self, output: &mut String) {
        let _ = writeln!(
            output,
            "{}",
            ui::interactive(ui::accent_style()).apply_to("│")
        );
        let prefix = format!("{}  ", ui::interactive(ui::accent_style()).apply_to("└"));
        self.write_line(output, &prefix, &clean_help());
    }

    fn write_line(&self, output: &mut String, prefix: &str, content: &str) {
        let _ = writeln!(
            output,
            "{}",
            ui::fit_line(prefix, content, self.terminal_columns)
        );
    }
}

fn clean_help() -> String {
    let mut output = String::new();
    for (index, (shortcut, description)) in [
        ("↑/↓", "navigate"),
        ("Space", "select"),
        ("Ctrl-A", "all"),
        ("Ctrl-S", "sort"),
        ("type to filter", ""),
        ("Enter", "remove"),
        ("Esc/Ctrl-C", "cancel"),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            let _ = write!(
                output,
                "{}",
                ui::interactive(ui::muted_style()).apply_to(" · ")
            );
        }
        let style = if description.is_empty() {
            ui::muted_style()
        } else {
            ui::shortcut_style()
        };
        let _ = write!(output, "{}", ui::interactive(style).apply_to(shortcut));
        if !description.is_empty() {
            let _ = write!(
                output,
                "{}",
                ui::interactive(ui::muted_style()).apply_to(format!(" {description}"))
            );
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
        sync::atomic::AtomicBool,
    };

    use super::{CleanSort, Size, format_bytes, join_labels, measure};
    use crate::SortMode;

    fn measured(root: &std::path::Path, excluded: &HashSet<PathBuf>) -> Size {
        measure(root, excluded, &AtomicBool::new(false))
            .expect("an uncancelled walk reports a size")
    }

    #[test]
    fn byte_counts_scale_to_binary_units_with_one_decimal() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 2), "2.0 GiB");
    }

    #[test]
    fn a_file_reached_through_two_hard_links_is_counted_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("original"), vec![0u8; 128 * 1024]).unwrap();
        let single = measured(root, &HashSet::new());

        fs::hard_link(root.join("original"), root.join("linked")).unwrap();
        let linked = measured(root, &HashSet::new());

        assert_eq!(single, linked);
    }

    #[test]
    fn a_nested_registered_worktree_is_excluded_from_its_parents_total() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("worktrees").join("topic");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("payload"), vec![0u8; 512 * 1024]).unwrap();

        let including = measured(root, &HashSet::new());
        let excluding = measured(root, &HashSet::from([nested]));

        let (Size::Measured { bytes: with, .. }, Size::Measured { bytes: without, .. }) =
            (including, excluding)
        else {
            panic!("both walks measure a readable directory");
        };
        assert!(with > without + 256 * 1024, "{with} vs {without}");
    }

    #[test]
    fn a_symlinked_tree_is_counted_as_the_link_not_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let outside = temp.path().join("outside");
        fs::create_dir_all(outside.join("deep")).unwrap();
        fs::write(outside.join("deep/payload"), vec![0u8; 512 * 1024]).unwrap();
        let worktree = root.join("worktree");
        fs::create_dir(&worktree).unwrap();
        let empty = measured(&worktree, &HashSet::new());

        symlink(&outside, worktree.join("link")).unwrap();
        let linked = measured(&worktree, &HashSet::new());

        let (Size::Measured { bytes: before, .. }, Size::Measured { bytes: after, .. }) =
            (empty, linked)
        else {
            panic!("both walks measure a readable directory");
        };
        assert!(after < before + 128 * 1024, "{before} vs {after}");
    }

    #[test]
    fn an_unreadable_subtree_yields_a_partial_total() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let blocked = root.join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("payload"), vec![0u8; 1024]).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            // A privileged run reads it anyway, which is not what this asserts.
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let size = measured(root, &HashSet::new());

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(size, Size::Measured { partial: true, .. }),
            "{size:?}"
        );
        assert_eq!(size.to_string().chars().next(), Some('~'));
    }

    #[test]
    fn a_missing_or_non_directory_path_has_no_measurable_size() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        fs::write(&file, "contents").unwrap();

        assert_eq!(
            measured(&temp.path().join("absent"), &HashSet::new()),
            Size::Unavailable
        );
        assert_eq!(measured(&file, &HashSet::new()), Size::Unavailable);
        assert_eq!(Size::Unavailable.to_string(), "—");
        assert_eq!(Size::Pending.to_string(), "…");
    }

    #[test]
    fn sort_cycles_through_the_shared_modes_and_back_through_size() {
        let mut sort = CleanSort::Shared(SortMode::Git);
        let mut seen = Vec::new();
        for _ in 0..5 {
            sort = sort.next();
            seen.push(sort);
        }

        assert_eq!(
            seen,
            vec![
                CleanSort::Shared(SortMode::Branch),
                CleanSort::Shared(SortMode::LastCommitAt),
                CleanSort::Shared(SortMode::Path),
                CleanSort::Size,
                CleanSort::Shared(SortMode::Git),
            ]
        );
        assert_eq!(CleanSort::Size.shared(), None);
    }

    #[test]
    fn dirty_worktrees_are_named_as_a_readable_list() {
        assert_eq!(join_labels(&["one"]), "one");
        assert_eq!(join_labels(&["one", "two"]), "one and two");
        assert_eq!(join_labels(&["one", "two", "three"]), "one, two and three");
    }
}
