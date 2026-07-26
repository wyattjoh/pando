use std::{
    env,
    fmt::Write as FmtWrite,
    fs,
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use cliclack::{confirm, input, select};
use console::{Key, Term, strip_ansi_codes};
use siphasher::sip::SipHasher13;

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, HookPhase, HookStep},
    git::{self, Repository},
    render,
    setup::{self, HookOutcome},
    trust, ui,
};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum GetProperty {
    Branch,
    Port,
    WorktreePath,
    MainWorktreePath,
    WorktreeRoot,
}

#[derive(Clone, Copy, Debug, clap::Subcommand)]
pub enum TrustCommand {
    /// Show configured and trusted state for every hook phase.
    Status,
    /// Revoke every hook-phase approval for this repository clone.
    Reset,
    /// Show approval state for the effective commit generator settings.
    CommitStatus,
    /// Revoke commit-generator approval for this repository clone.
    CommitReset,
}

/// Resolves, creates when needed, and emits a switch destination.
///
/// # Errors
///
/// Returns an error when repository planning, user approval, creation, or setup fails.
pub fn switch(branch: Option<String>) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    match branch {
        Some(branch) => resolve_and_switch(&repository, &branch),
        None => pick_and_switch(&repository),
    }
}

enum GetValue {
    Text(String),
    Path(PathBuf),
}

/// Prints one stable current-worktree property.
///
/// # Errors
///
/// Returns an error when repository context or the requested value is unavailable.
pub fn get(property: GetProperty) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    let value = match property {
        GetProperty::Branch => GetValue::Text(git::current_branch(&repository)?.to_owned()),
        GetProperty::Port => {
            GetValue::Text(port_for_branch(git::current_branch(&repository)?).to_string())
        }
        GetProperty::WorktreePath => GetValue::Path(resolved_path(&repository.current().path)?),
        GetProperty::MainWorktreePath => GetValue::Path(resolved_path(
            repository
                .primary
                .as_ref()
                .context("the current repository has no primary worktree")?,
        )?),
        GetProperty::WorktreeRoot => {
            repository.primary.as_ref().context(
                "the current repository has no primary worktree; a creation root is unavailable",
            )?;
            GetValue::Path(
                EffectiveConfig::load(&repository)?
                    .require_root()?
                    .to_path_buf(),
            )
        }
    };
    let mut stdout = io::stdout().lock();
    match value {
        GetValue::Text(value) => stdout.write_all(value.as_bytes()),
        GetValue::Path(value) => stdout.write_all(value.as_os_str().as_bytes()),
    }
    .context("failed to write requested worktree property")?;
    stdout
        .write_all(b"\n")
        .context("failed to terminate requested worktree property")
}

/// Inspects or resets post-create trust for the current repository.
///
/// # Errors
///
/// Returns an error when repository, configuration, or trust storage is invalid.
pub fn trust_command(command: TrustCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    match command {
        TrustCommand::Status => {
            let config = EffectiveConfig::load(&repository)?;
            for phase in HookPhase::all() {
                let steps = config.hooks(phase);
                let trusted = trust::is_trusted(&repository, phase, steps)?;
                if steps.is_empty() {
                    ui::info(format!("No {} are configured.", phase.key()))?;
                } else if trusted {
                    ui::success(format!("The current {} are trusted.", phase.key()))?;
                } else {
                    ui::warning(format!("The current {} are not trusted.", phase.key()))?;
                }
            }
        }
        TrustCommand::Reset => {
            if trust::reset(&repository)? {
                ui::success("Reset hook trust for this repository.")?;
            } else {
                ui::info("No saved hook trust existed for this repository.")?;
            }
        }
        TrustCommand::CommitStatus => {
            let config = EffectiveConfig::load(&repository)?;
            if config.generation.command.is_none() {
                ui::info("No commit generator is configured.")?;
            } else if trust::generation_hash(&config.generation).is_none() {
                ui::info("The effective commit generator is user-controlled.")?;
            } else if trust::is_generation_trusted(&repository, &config.generation)? {
                ui::success("The effective shared commit generator is trusted.")?;
            } else {
                ui::warning("The effective shared commit generator is not trusted.")?;
            }
        }
        TrustCommand::CommitReset => {
            if trust::reset_generation(&repository)? {
                ui::success("Reset commit generator trust for this repository.")?;
            } else {
                ui::info("No saved commit generator trust existed for this repository.")?;
            }
        }
    }
    Ok(())
}

fn pick_and_switch(repository: &Repository) -> Result<()> {
    let choices: Vec<_> = repository
        .worktrees
        .iter()
        .filter(|worktree| worktree.navigable())
        .collect();
    if choices.is_empty() {
        bail!("the current repository has no navigable worktrees");
    }
    let default = choices
        .iter()
        .position(|worktree| worktree.current)
        .unwrap_or(0);
    let branch_action = choices.len();
    let mut labels = render::menu_labels(&choices);
    labels.push(
        ui::interactive(ui::shortcut_style())
            .apply_to("+ Create or switch branches...")
            .to_string(),
    );
    let current = choices
        .iter()
        .map(|worktree| worktree.current)
        .chain(std::iter::once(false))
        .collect();
    let shortcuts = choices
        .iter()
        .map(|worktree| !worktree.current)
        .chain(std::iter::once(true))
        .collect();
    let filters = choices
        .iter()
        .map(|worktree| {
            format!(
                "{} {} {}",
                worktree.branch_label(),
                worktree.state_label(),
                worktree.path.display()
            )
        })
        .chain(std::iter::once("create switch branches".to_owned()))
        .collect();
    let selection = prompt_result(
        WorktreePicker::new(labels, filters, current, shortcuts, default).interact(),
        "selection cancelled",
        "failed to read worktree selection from the terminal",
    )?;
    if selection < choices.len() {
        let chosen = choices[selection];
        let branch = match &chosen.kind {
            WorktreeKind::Branch(branch) => Some(branch.as_str()),
            _ => None,
        };
        return enter_existing(repository, &chosen.path, branch);
    }

    debug_assert_eq!(selection, branch_action);
    let branch = read_branch_name()?;
    resolve_and_switch(repository, &branch)
}

const PICKER_FRAME_ROWS: usize = 5;

fn picker_viewport_rows(terminal_rows: u16) -> usize {
    usize::from(terminal_rows)
        .saturating_sub(PICKER_FRAME_ROWS)
        .max(1)
}

struct WorktreePicker {
    labels: Vec<String>,
    filters: Vec<String>,
    current: Vec<bool>,
    shortcuts: Vec<bool>,
    selected: usize,
    number_start: usize,
    viewport_rows: usize,
    filter: String,
}

impl WorktreePicker {
    fn new(
        labels: Vec<String>,
        filters: Vec<String>,
        current: Vec<bool>,
        shortcuts: Vec<bool>,
        selected: usize,
    ) -> Self {
        Self {
            labels,
            filters,
            current,
            shortcuts,
            selected,
            number_start: selected.saturating_add(1),
            viewport_rows: 20,
            filter: String::new(),
        }
    }

    fn interact(mut self) -> io::Result<usize> {
        let term = Term::stderr();
        if !term.is_term() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        self.viewport_rows = picker_viewport_rows(term.size().0);
        term.hide_cursor()?;
        let result = self.interact_inner(&term);
        term.show_cursor()?;
        result
    }

    fn interact_inner(&mut self, mut term: &Term) -> io::Result<usize> {
        let mut previous_lines = 0;
        let mut shortcut_prefix = false;
        loop {
            let visible = self.visible();
            if self.selected >= visible.len() {
                self.selected = 0;
            }
            let displayed_start = self.displayed_start(&visible);
            let displayed = self.displayed(&visible, displayed_start);
            let frame = self.render(displayed, displayed_start, visible.len());
            term.clear_last_lines(previous_lines)?;
            term.write_all(frame.as_bytes())?;
            term.flush()?;
            previous_lines = frame.lines().count();
            let key = term.read_key_raw()?;
            if let Some(selection) = self.shortcut_selection(&key) {
                return Ok(selection);
            }
            // `console` reports Ctrl-A as Home on Unix.
            if key == Key::Home {
                shortcut_prefix = true;
                continue;
            }
            if shortcut_prefix {
                shortcut_prefix = false;
                if let Some(number) = shortcut_number(&key) {
                    if let Some((index, _)) = self
                        .numbered(
                            &visible,
                            self.selected,
                            self.number_start,
                            self.number_start + 8 == self.shortcuts.len() - 1,
                        )
                        .iter()
                        .find(|(_, shortcut)| *shortcut == number)
                    {
                        return Ok(*index);
                    }
                    continue;
                }
            }
            match key {
                Key::ArrowUp if self.selected > 0 => {
                    self.selected -= 1;
                    self.number_start = self.selected.saturating_sub(9);
                }
                Key::ArrowDown if self.selected + 1 < visible.len() => {
                    self.selected += 1;
                    let action = self.shortcuts.len() - 1;
                    self.number_start = if self.selected + 9 <= action {
                        self.selected + 1
                    } else {
                        action.saturating_sub(8)
                    };
                }
                Key::Enter => return Ok(visible[self.selected]),
                Key::Escape | Key::CtrlC => return Err(io::ErrorKind::Interrupted.into()),
                Key::Backspace => {
                    self.filter.pop();
                    self.selected = 0;
                    self.number_start = 1;
                }
                Key::Char(character) if !character.is_control() => {
                    self.filter.push(character);
                    self.selected = 0;
                    self.number_start = 1;
                }
                _ => {}
            }
        }
    }

    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        self.filters
            .iter()
            .enumerate()
            .filter_map(|(index, filter)| filter.to_lowercase().contains(&needle).then_some(index))
            .collect()
    }

    fn displayed_start(&self, visible: &[usize]) -> usize {
        let desired_start = self
            .selected
            .saturating_sub(self.viewport_rows.saturating_sub(10));
        let last_page_start = if visible.len() > self.viewport_rows {
            visible
                .len()
                .saturating_sub(self.viewport_rows.saturating_sub(1))
        } else {
            0
        };
        desired_start.min(last_page_start)
    }

    fn displayed<'a>(&self, visible: &'a [usize], start: usize) -> &'a [usize] {
        let has_above = start > 0;
        let rows_before_bottom_hint = self.viewport_rows.saturating_sub(usize::from(has_above));
        let has_below = visible.len() > start + rows_before_bottom_hint;
        let item_rows = rows_before_bottom_hint.saturating_sub(usize::from(has_below));
        &visible[start..visible.len().min(start + item_rows)]
    }

    fn shortcut_selection(&self, key: &Key) -> Option<usize> {
        (key == &Key::BackTab).then_some(self.labels.len() - 1)
    }

    fn numbered(
        &self,
        visible: &[usize],
        selected: usize,
        number_start: usize,
        pinned_at_bottom: bool,
    ) -> Vec<(usize, usize)> {
        visible[number_start.min(visible.len())..]
            .iter()
            .copied()
            .filter(|index| {
                self.shortcuts[*index] && (pinned_at_bottom || *index != visible[selected])
            })
            .take(9)
            .enumerate()
            .map(|(offset, index)| (index, offset + 1))
            .collect()
    }

    fn render(&self, visible: &[usize], displayed_start: usize, visible_len: usize) -> String {
        let filter = if self.filter.is_empty() {
            "type to filter".to_owned()
        } else {
            self.filter.clone()
        };
        let mut output = format!(
            "{}  {}\n{}  {}\n",
            ui::interactive(ui::accent_style()).apply_to("◆"),
            ui::interactive(ui::heading_style()).apply_to("Choose a worktree"),
            ui::interactive(ui::accent_style()).apply_to("│"),
            ui::interactive(ui::muted_style()).apply_to(filter)
        );
        let numbered = self.numbered(
            visible,
            self.selected.saturating_sub(displayed_start),
            self.number_start.saturating_sub(displayed_start),
            self.number_start + 8 == self.shortcuts.len() - 1,
        );
        let pinned_at_bottom = self.number_start + 8 == self.shortcuts.len() - 1;
        if displayed_start > 0 {
            writeln!(
                output,
                "{}  {}",
                ui::interactive(ui::accent_style()).apply_to("│"),
                ui::interactive(ui::muted_style())
                    .apply_to(format!("↑ {displayed_start} more above"))
            )
            .expect("writing to a string cannot fail");
        }
        for (position, index) in visible.iter().enumerate() {
            let marker = if self.current[*index] {
                ui::interactive(ui::accent_style().bold())
                    .apply_to("*")
                    .to_string()
            } else if displayed_start + position == self.selected && !pinned_at_bottom {
                " ".to_owned()
            } else {
                numbered
                    .iter()
                    .find_map(|(numbered_index, number)| {
                        (numbered_index == index).then_some(number)
                    })
                    .map_or_else(
                        || " ".to_owned(),
                        |number| {
                            ui::interactive(ui::shortcut_style())
                                .apply_to(number)
                                .to_string()
                        },
                    )
            };
            let is_selected = displayed_start + position == self.selected;
            let selected = if is_selected {
                ui::interactive(ui::accent_style()).apply_to("●")
            } else {
                ui::interactive(ui::muted_style()).apply_to("○")
            };
            let label = if is_selected {
                ui::interactive(ui::selected_style())
                    .apply_to(strip_ansi_codes(&self.labels[*index]))
                    .to_string()
            } else {
                self.labels[*index].clone()
            };
            writeln!(
                output,
                "{}  {selected} {marker} {label}",
                ui::interactive(ui::accent_style()).apply_to("│"),
            )
            .expect("writing to a string cannot fail");
        }
        let more_below = visible_len.saturating_sub(displayed_start + visible.len());
        if more_below > 0 {
            writeln!(
                output,
                "{}  {}",
                ui::interactive(ui::accent_style()).apply_to("│"),
                ui::interactive(ui::muted_style()).apply_to(format!("↓ {more_below} more below"))
            )
            .expect("writing to a string cannot fail");
        }
        writeln!(
            output,
            "{}\n{}  {}",
            ui::interactive(ui::accent_style()).apply_to("│"),
            ui::interactive(ui::accent_style()).apply_to("└"),
            picker_help()
        )
        .expect("writing to a string cannot fail");
        output
    }
}

fn picker_help() -> String {
    let mut output = String::new();
    for (index, (shortcut, description)) in [
        ("↑/↓", "navigate"),
        ("Ctrl-A then 1–9", "select"),
        ("Shift-Tab", "create"),
        ("type to filter", ""),
        ("Enter", "select"),
        ("Esc/Ctrl-C", "cancel"),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            write!(
                output,
                "{}",
                ui::interactive(ui::muted_style()).apply_to(" · ")
            )
            .expect("writing to a string cannot fail");
        }
        let style = if description.is_empty() {
            ui::muted_style()
        } else {
            ui::shortcut_style()
        };
        write!(output, "{}", ui::interactive(style).apply_to(shortcut))
            .expect("writing to a string cannot fail");
        if !description.is_empty() {
            write!(
                output,
                "{}",
                ui::interactive(ui::muted_style()).apply_to(format!(" {description}"))
            )
            .expect("writing to a string cannot fail");
        }
    }
    output
}

fn shortcut_number(key: &Key) -> Option<usize> {
    match key {
        Key::Char('1') => Some(1),
        Key::Char('2') => Some(2),
        Key::Char('3') => Some(3),
        Key::Char('4') => Some(4),
        Key::Char('5') => Some(5),
        Key::Char('6') => Some(6),
        Key::Char('7') => Some(7),
        Key::Char('8') => Some(8),
        Key::Char('9') => Some(9),
        _ => None,
    }
}

fn read_branch_name() -> Result<String> {
    let value: String = prompt_result(
        input("Branch name:")
            .validate(|value: &String| {
                if value.trim().is_empty() {
                    Err("branch name cannot be empty")
                } else {
                    Ok(())
                }
            })
            .interact(),
        "branch entry cancelled",
        "failed to read branch name",
    )?;
    Ok(value.trim().to_owned())
}

fn resolve_and_switch(repository: &Repository, branch: &str) -> Result<()> {
    git::validate_branch(&repository.current().path, branch)?;
    if let Some(worktree) = repository
        .worktrees
        .iter()
        .find(|worktree| matches!(&worktree.kind, WorktreeKind::Branch(name) if name == branch))
    {
        if !worktree.navigable() {
            bail!(
                "branch {branch:?} is registered at {} but that worktree is {}; inspect it with 'git worktree list' and repair or prune it explicitly with Git",
                worktree.path.display(),
                worktree.state_label()
            );
        }
        return enter_existing(repository, &worktree.path, Some(branch));
    }

    if repository.primary.is_none() {
        bail!("creating worktrees from a bare repository is not supported");
    }

    let plan = if git::local_branch_exists(&repository.current().path, branch)? {
        CreationKind::Local
    } else {
        let remotes = git::remote_matches(&repository.current().path, branch)?;
        match remotes.len() {
            0 => CreationKind::New {
                head: git::head_commit(&repository.current().path)?,
            },
            1 => CreationKind::Remote(remotes[0].clone()),
            _ => CreationKind::Remote(choose_remote(&remotes, branch)?),
        }
    };

    create(repository, branch, &plan)
}

fn choose_remote(remotes: &[String], branch: &str) -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!(
            "multiple remote-tracking branches match {branch:?}; choose in a terminal: {}",
            remotes.join(", ")
        );
    }
    let mut prompt = select(format!("Choose the upstream for {branch}")).initial_value(0);
    for (index, remote) in remotes.iter().enumerate() {
        prompt = prompt.item(index, remote, "");
    }
    let selection = prompt_result(
        prompt.interact(),
        "remote selection cancelled",
        "failed to read remote selection",
    )?;
    Ok(remotes[selection].clone())
}

#[derive(Debug)]
enum CreationKind {
    Local,
    Remote(String),
    New { head: String },
}

fn create(repository: &Repository, branch: &str, kind: &CreationKind) -> Result<()> {
    let config = EffectiveConfig::load(repository)?;
    let destination = config.require_root()?.join(branch);
    let destination = git::canonical_or_normalized(&destination)
        .context("failed to resolve worktree destination")?;
    validate_destination(repository, branch, &destination)?;

    if let CreationKind::New { head } = kind {
        confirm_new_branch(repository, branch, head, &destination)?;
    }
    approve_hooks(repository, HookPhase::PostCreate, &config.post_create)?;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create destination parent {}", parent.display()))?;
    }
    let pending = (!config.post_create.is_empty())
        .then(|| setup::prepare(&repository.common_dir, branch, &destination))
        .transpose()?;
    let creation = match kind {
        CreationKind::Local => {
            git::add_existing_worktree(&repository.current().path, &destination, branch)
        }
        CreationKind::Remote(remote) => {
            git::add_tracking_worktree(&repository.current().path, &destination, branch, remote)
        }
        CreationKind::New { head } => {
            git::add_new_worktree(&repository.current().path, &destination, branch, head)
        }
    };
    if let Err(error) = creation {
        if let Some(pending) = pending {
            pending
                .cancel()
                .context("worktree creation failed and pending setup state could not be cleared")?;
        }
        return Err(error);
    }

    let Some(pending) = pending else {
        return write_destination(&destination);
    };
    let worktree_identity = git::worktree_identity(&destination)?;
    pending.commit(&repository.common_dir, &worktree_identity)?;
    finish_setup(
        repository,
        &config,
        &worktree_identity,
        Some(branch),
        &destination,
    )
}

fn validate_destination(repository: &Repository, branch: &str, destination: &Path) -> Result<()> {
    if destination.exists() {
        bail!(
            "destination {} for branch {branch:?} already exists; Worktrees will not adopt, move, or delete it",
            destination.display()
        );
    }
    let primary = repository
        .primary
        .as_ref()
        .expect("creation requires primary");
    if destination.starts_with(primary) && !git::would_be_ignored(primary, destination)? {
        let relative = destination.strip_prefix(primary).unwrap_or(destination);
        let first = relative
            .components()
            .next()
            .map(|part| part.as_os_str().to_string_lossy())
            .unwrap_or_default();
        bail!(
            "destination {} is inside the primary worktree but is not ignored; add '/{first}/' to {}",
            destination.display(),
            primary.join(".gitignore").display()
        );
    }
    Ok(())
}

fn confirm_new_branch(
    repository: &Repository,
    branch: &str,
    head: &str,
    destination: &Path,
) -> Result<()> {
    ensure_interactive("new branch creation requires confirmation")?;
    let source = match &repository.current().kind {
        WorktreeKind::Branch(source) => format!("branch {source:?} at {head}"),
        WorktreeKind::Detached => format!("detached commit {head}"),
        _ => format!("commit {head}"),
    };
    ui::info(format!(
        "Create branch {branch:?} from {source} at {}?",
        destination.display()
    ))?;
    if git::is_dirty(&repository.current().path)? {
        ui::warning("Staged, unstaged, and untracked changes remain in the source worktree.")?;
    }
    let confirmed = confirm("Create this branch and worktree?")
        .initial_value(false)
        .interact()
        .context("failed to read new-branch confirmation")?;
    if !confirmed {
        bail!("branch creation declined");
    }
    Ok(())
}

pub(crate) fn approve_hooks(
    repository: &Repository,
    phase: HookPhase,
    steps: &[HookStep],
) -> Result<()> {
    if steps.is_empty() || trust::is_trusted(repository, phase, steps)? {
        return Ok(());
    }
    ensure_interactive(&format!("{} require approval", phase.plural_name()))?;
    ui::info(format!(
        "The repository requests these {}:",
        phase.plural_name()
    ))?;
    for (index, step) in steps.iter().enumerate() {
        ui::step(format!("{}: {}", step.label(index), step.command))?;
    }
    let confirmed = confirm("Trust and run these commands for this repository?")
        .initial_value(false)
        .interact()
        .context("failed to read hook approval")?;
    if !confirmed {
        bail!("{} approval declined", phase.plural_name());
    }
    trust::approve(repository, phase, steps)
}

fn enter_existing(repository: &Repository, destination: &Path, branch: Option<&str>) -> Result<()> {
    let destination = resolved_path(destination)?;
    let worktree_identity = git::worktree_identity(&destination)?;
    if !setup::is_incomplete(&repository.common_dir, &worktree_identity, branch)? {
        return write_destination(&destination);
    }
    let config = EffectiveConfig::load(repository)?;
    if config.post_create.is_empty() {
        setup::clear(&repository.common_dir, &worktree_identity, branch)?;
        return write_destination(&destination);
    }
    ensure_interactive("incomplete setup requires a recovery choice")?;
    let choices = ["Retry setup", "Enter once", "Mark setup complete and enter"];
    let mut prompt = select("Setup did not complete for this worktree").initial_value(0);
    for (index, choice) in choices.iter().enumerate() {
        prompt = prompt.item(index, choice, "");
    }
    let choice = prompt_result(
        prompt.interact(),
        "setup recovery cancelled",
        "failed to read setup recovery choice",
    )?;
    match choice {
        0 => {
            approve_hooks(repository, HookPhase::PostCreate, &config.post_create)?;
            finish_setup(
                repository,
                &config,
                &worktree_identity,
                branch,
                &destination,
            )
        }
        1 => {
            ui::warning("Entering once while setup remains incomplete.")?;
            write_destination(&destination)?;
            bail!("setup remains incomplete for {}", destination.display())
        }
        2 => {
            setup::clear(&repository.common_dir, &worktree_identity, branch)?;
            write_destination(&destination)
        }
        _ => unreachable!(),
    }
}

fn finish_setup(
    repository: &Repository,
    config: &EffectiveConfig,
    worktree_identity: &Path,
    branch: Option<&str>,
    destination: &Path,
) -> Result<()> {
    match setup::run_steps(HookPhase::PostCreate, &config.post_create, destination)? {
        HookOutcome::Success => {
            setup::clear(&repository.common_dir, worktree_identity, branch)?;
            write_destination(destination)
        }
        HookOutcome::Failed(status) => {
            write_destination(destination)?;
            bail!("post-create setup failed with status {status}; setup remains incomplete")
        }
        HookOutcome::Interrupted => {
            bail!("post-create setup was interrupted; setup remains incomplete")
        }
    }
}

fn prompt_result<T>(result: io::Result<T>, cancelled: &str, failure: &str) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            Err(io::Error::new(io::ErrorKind::Interrupted, cancelled.to_owned()).into())
        }
        Err(error) => Err(error).context(failure.to_owned()),
    }
}

fn ensure_interactive(reason: &str) -> Result<()> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        Ok(())
    } else {
        bail!("{reason}, but no interactive terminal is available")
    }
}

fn write_destination(destination: &Path) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(destination.as_os_str().as_bytes())
        .context("failed to write worktree destination")?;
    stdout
        .write_all(b"\n")
        .context("failed to terminate worktree destination")
}

fn resolved_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve path {}", path.display()))
}

#[must_use]
pub fn port_for_branch(branch: &str) -> u16 {
    // Worktrunk v0.66.0 used DefaultHasher (SipHash 1-3 with zero keys). Pin the
    // algorithm explicitly so future standard-library changes cannot move ports.
    let mut hasher = SipHasher13::new();
    branch.hash(&mut hasher);
    10_000 + (hasher.finish() % 10_000) as u16
}

#[cfg(test)]
mod tests {
    use console::{Key, strip_ansi_codes};

    use super::{WorktreePicker, picker_viewport_rows, port_for_branch};
    use crate::ui;

    #[test]
    fn picker_reserves_a_row_to_avoid_scrolling_its_header() {
        assert_eq!(picker_viewport_rows(20), 15);
        assert_eq!(picker_viewport_rows(5), 1);
    }

    #[test]
    fn picker_stops_scrolling_at_the_last_full_page() {
        let mut picker = WorktreePicker::new(
            (0..20).map(|index| index.to_string()).collect(),
            (0..20).map(|index| index.to_string()).collect(),
            vec![false; 20],
            vec![true; 20],
            19,
        );
        picker.viewport_rows = 15;
        let visible = picker.visible();
        let start = picker.displayed_start(&visible);

        assert_eq!(start, 6);
        assert_eq!(picker.displayed(&visible, start).len(), 14);
    }

    #[test]
    fn picker_shows_hints_for_results_outside_the_page() {
        let mut picker = WorktreePicker::new(
            (0..20).map(|index| index.to_string()).collect(),
            (0..20).map(|index| index.to_string()).collect(),
            vec![false; 20],
            vec![true; 20],
            0,
        );
        picker.viewport_rows = 15;
        let visible = picker.visible();

        let first_page = picker.displayed(&visible, 0);
        let first_rendered = picker.render(first_page, 0, visible.len());
        assert!(first_rendered.contains("↓ 6 more below"));
        assert!(!first_rendered.contains("more above"));

        let final_page = picker.displayed(&visible, 6);
        let final_rendered = picker.render(final_page, 6, visible.len());
        assert!(final_rendered.contains("↑ 6 more above"));
        assert!(!final_rendered.contains("more below"));
    }

    #[test]
    fn picker_renders_selected_label_in_white() {
        let first = ui::interactive(ui::worktree_data_style())
            .apply_to("first")
            .to_string();
        let second = ui::interactive(ui::worktree_data_style())
            .apply_to("second")
            .to_string();
        let picker = WorktreePicker::new(
            vec![first, second.clone()],
            vec!["first".to_owned(), "second".to_owned()],
            vec![true, false],
            vec![false, true],
            0,
        );

        let rendered = picker.render(&[0, 1], 0, 2);

        assert!(
            rendered.contains(
                &ui::interactive(ui::selected_style())
                    .apply_to("first")
                    .to_string()
            )
        );
        assert!(rendered.contains(&second));
        assert!(strip_ansi_codes(&rendered).contains("Shift-Tab create"));
    }

    #[test]
    fn picker_shift_tab_selects_the_create_action() {
        let picker = WorktreePicker::new(
            vec!["main".to_owned(), "Create".to_owned()],
            vec!["main".to_owned(), "Create".to_owned()],
            vec![true, false],
            vec![false, true],
            0,
        );

        assert_eq!(picker.shortcut_selection(&Key::BackTab), Some(1));
        assert_eq!(picker.shortcut_selection(&Key::CtrlC), None);
    }

    #[test]
    fn ports_match_worktrunk_v0_66_golden_values() {
        assert_eq!(port_for_branch("main"), 12_107);
        assert_eq!(port_for_branch("feature/test"), 18_064);
        assert_eq!(port_for_branch("føø/分支"), 17_537);
    }
}
