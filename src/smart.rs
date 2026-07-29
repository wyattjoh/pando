use std::{
    env,
    fmt::Write as FmtWrite,
    fs,
    hash::{Hash, Hasher},
    io::{self, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use cliclack::{confirm, input, select};
use console::{Key, Term, strip_ansi_codes, truncate_str};
use siphasher::sip::SipHasher13;
use unicode_width::UnicodeWidthStr;

use crate::{
    Row, SortMode, Worktree, WorktreeKind,
    config::{EffectiveConfig, HookPhase, HookStep},
    git::{self, Repository},
    render,
    setup::{self, HookOutcome},
    sorted_row_indices, trust, ui,
};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum GetProperty {
    Branch,
    Port,
    WorktreePath,
    PrimaryWorktreePath,
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
    /// Preview and approve effective shared commit-generation settings.
    CommitApprove,
    PrStatus,
    PrReset,
    PrApprove,
}

/// Resolves, creates when needed, and emits a switch destination.
///
/// # Errors
///
/// Returns an error when repository planning, user approval, creation, or setup fails.
pub fn switch(branch: Option<String>, branches: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let Some(branch) = branch else {
        let initial_view = if branches {
            PickerView::Branch
        } else {
            PickerView::Worktree
        };
        return pick_and_switch(&git::repository_with_branches(&cwd)?, initial_view);
    };
    resolve_and_switch(&git::repository(&cwd)?, &branch, Intent::Switch)
}

/// Creates a worktree for `branch` and emits its destination.
///
/// Unlike [`switch`], a genuinely new branch is created without confirmation, and an
/// already-registered branch is refused rather than entered.
///
/// # Errors
///
/// Returns an error when the branch is already registered, or when repository planning,
/// hook approval, creation, or setup fails.
pub fn create(branch: &str) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    resolve_and_switch(&git::repository(&cwd)?, branch, Intent::Create)
}

/// Distinguishes the two entry points that share worktree resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    /// Enter an existing worktree, and confirm before creating a new branch.
    Switch,
    /// Refuse an existing worktree, and create a new branch without confirmation.
    Create,
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
        GetProperty::PrimaryWorktreePath => GetValue::Path(resolved_path(
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

/// Previews switching without creating directories, worktrees, trust, or setup records.
///
/// # Errors
/// Returns an error when the branch or repository cannot be resolved.
pub fn switch_dry_run(branch: Option<String>) -> Result<()> {
    let branch = branch.context("switch --dry-run requires a branch")?;
    plan_dry_run(&branch, Intent::Switch)
}

/// Previews creation without creating directories, worktrees, trust, or setup records.
///
/// # Errors
/// Returns an error when the branch is already registered or cannot be resolved.
pub fn create_dry_run(branch: &str) -> Result<()> {
    plan_dry_run(branch, Intent::Create)
}

fn plan_dry_run(branch: &str, intent: Intent) -> Result<()> {
    let repository =
        git::repository(&env::current_dir().context("failed to read the current directory")?)?;
    git::validate_branch(&repository.current().path, branch)?;
    if let Some(existing) = repository
        .worktrees
        .iter()
        .find(|w| matches!(&w.kind, WorktreeKind::Branch(value) if value == branch))
    {
        if intent == Intent::Create {
            return Err(already_registered(branch, &existing.path));
        }
        return ui::finish(format!(
            "Would enter {}; no changes made.",
            existing.path.display()
        ));
    }
    let destination = EffectiveConfig::load(&repository)?
        .require_root()?
        .join(branch);
    if destination.exists() || repository.worktrees.iter().any(|w| w.path == destination) {
        bail!("the configured destination already exists or is registered");
    }
    ui::finish(format!(
        "Would create a worktree for {branch} at {}; no changes made.",
        destination.display()
    ))
}

/// Previews a trust command without changing trust storage or prompting.
///
/// # Errors
/// Returns an error when repository or configuration inspection fails.
pub fn trust_dry_run(command: TrustCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    match command {
        TrustCommand::Status | TrustCommand::CommitStatus => trust_command(command),
        TrustCommand::Reset => ui::finish("Would reset hook trust; no changes made."),
        TrustCommand::CommitReset => {
            ui::finish("Would reset commit generator trust; no changes made.")
        }
        TrustCommand::PrStatus | TrustCommand::PrReset | TrustCommand::PrApprove => {
            ui::finish("Would update PR generator trust; no changes made.")
        }
        TrustCommand::CommitApprove => {
            let config = EffectiveConfig::load(&repository)?;
            trust::generation_hash(&config.generation).context(
                "the effective commit generator is user-controlled; no approval is required",
            )?;
            ui::finish(
                "Would approve the effective shared commit generator after human review; no changes made.",
            )
        }
    }
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
        TrustCommand::PrStatus => {
            let config = EffectiveConfig::load(&repository)?;
            if config.pr_generation.command.is_none() {
                ui::info("No PR generator is configured.")?;
            } else if trust::is_pr_generation_trusted(&repository, &config.pr_generation)? {
                ui::success("The effective shared PR generator is trusted.")?;
            } else {
                ui::warning("The effective shared PR generator is not trusted.")?;
            }
        }
        TrustCommand::PrReset => {
            trust::reset_pr_generation(&repository)?;
            ui::success("Reset PR generator trust for this repository.")?;
        }
        TrustCommand::PrApprove => {
            let config = EffectiveConfig::load(&repository)?;
            ui::ensure_interactive("PR generator approval requires an interactive human terminal")?;
            trust::approve_pr_generation(&repository, &config.pr_generation)?;
            ui::success("Approved PR generator for this repository.")?;
        }
        TrustCommand::CommitReset => {
            if trust::reset_generation(&repository)? {
                ui::success("Reset commit generator trust for this repository.")?;
            } else {
                ui::info("No saved commit generator trust existed for this repository.")?;
            }
        }
        TrustCommand::CommitApprove => {
            let config = EffectiveConfig::load(&repository)?;
            let hash = trust::generation_hash(&config.generation).context(
                "the effective commit generator is user-controlled; no approval is required",
            )?;
            if trust::is_generation_trusted(&repository, &config.generation)? {
                ui::info("The effective shared commit generator is already trusted.")?;
                return Ok(());
            }
            ui::ensure_interactive(
                "commit generator approval requires an interactive human terminal",
            )?;
            ui::info("Effective shared commit generation settings:")?;
            if let Some(value) = &config.generation.command {
                ui::step(format!("command: {}", value.value))?;
            }
            if let Some(value) = &config.generation.template {
                ui::step(format!("template:\n{}", value.value))?;
            }
            ui::step(format!("identity: {hash}"))?;
            let approved = ui::prompt_result(
                confirm("Trust these settings for this repository?")
                    .initial_value(false)
                    .interact(),
                "commit generator approval cancelled",
                "failed to read commit generator approval",
            )?;
            if !approved {
                return Err(ui::declined("commit generator approval declined"));
            }
            trust::approve_generation(&repository, &config.generation)?;
            ui::success("Approved commit generator for this repository.")?;
        }
    }
    Ok(())
}

fn pick_and_switch(
    repository_branches: &git::RepositoryBranches,
    initial_view: PickerView,
) -> Result<()> {
    let repository = &repository_branches.repository;
    let choices: Vec<_> = repository
        .worktrees
        .iter()
        .filter(|worktree| worktree.navigable())
        .collect();
    if choices.is_empty() {
        bail!("the current repository has no navigable worktrees");
    }
    let default_sort = EffectiveConfig::load_default_sort(repository)?;
    if let Some(warning) = &repository.metadata_warning {
        ui::warning(warning)?;
    }
    ui::ensure_interactive("worktree selection requires an interactive terminal")?;
    let selection = ui::prompt_result(
        WorktreePicker::new(
            repository,
            &choices,
            &repository_branches.branches,
            default_sort,
            initial_view,
        )
        .interact(),
        "selection cancelled",
        "failed to read worktree selection from the terminal",
    )?;
    match selection {
        PickerChoice::Worktree(identity) => {
            let chosen = choices[identity];
            let branch = match &chosen.kind {
                WorktreeKind::Branch(branch) => Some(branch.as_str()),
                _ => None,
            };
            enter_existing(repository, &chosen.path, branch)
        }
        PickerChoice::Branch(branch) => resolve_and_switch(repository, &branch, Intent::Switch),
        PickerChoice::Create => {
            let branch = read_branch_name()?;
            resolve_and_switch(repository, &branch, Intent::Switch)
        }
    }
}

const PICKER_FRAME_ROWS: usize = 6;
const PICKER_CHOICE_PREFIX: &str = "    ";
const STANDARD_SELECTION_FRAME_ROWS: usize = 3;
const STANDARD_SELECTION_MAX_ROWS: usize = 10;

fn standard_selection_viewport_rows(terminal_rows: u16) -> usize {
    if terminal_rows == 0 {
        return STANDARD_SELECTION_MAX_ROWS;
    }
    usize::from(terminal_rows)
        .saturating_sub(STANDARD_SELECTION_FRAME_ROWS)
        .clamp(1, STANDARD_SELECTION_MAX_ROWS)
}

fn picker_viewport_rows(terminal_rows: u16) -> usize {
    usize::from(terminal_rows)
        .saturating_sub(PICKER_FRAME_ROWS)
        .max(1)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PickerChoice {
    Worktree(usize),
    Branch(String),
    Create,
}

/// Which navigation surface the picker is currently displaying.
///
/// Toggling never runs Git: both views are built up front in [`WorktreePicker::new`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerView {
    Worktree,
    Branch,
}

impl PickerView {
    const fn toggled(self) -> Self {
        match self {
            Self::Worktree => Self::Branch,
            Self::Branch => Self::Worktree,
        }
    }

    const fn heading(self) -> &'static str {
        match self {
            Self::Worktree => "Choose a worktree",
            Self::Branch => "Choose a branch",
        }
    }

    const fn no_match_hint(self) -> &'static str {
        match self {
            Self::Worktree => "No worktrees match this filter",
            Self::Branch => "No branches match this filter",
        }
    }
}

struct PickerItem {
    choice: PickerChoice,
    label: String,
    filter: String,
    current: bool,
    shortcut: bool,
}

impl PickerItem {
    fn new(choice: PickerChoice, label: String, filter: String, current: bool) -> Self {
        Self {
            choice,
            label,
            filter,
            current,
            shortcut: !current,
        }
    }

    fn worktree(identity: usize, worktree: &Worktree, label: String) -> Self {
        Self::new(
            PickerChoice::Worktree(identity),
            label,
            format!(
                "{} {} {}",
                worktree.branch_label(),
                worktree.state_label(),
                worktree.path.display()
            ),
            worktree.current,
        )
    }

    fn branch(
        choice: PickerChoice,
        record: &git::BranchRecord,
        repository: &Repository,
        label: String,
    ) -> Self {
        let worktree = crate::worktree_for_branch(&repository.worktrees, &record.branch);
        let filter = worktree.map_or_else(
            || record.branch.clone(),
            |worktree| {
                format!(
                    "{} {} {}",
                    record.branch,
                    worktree.state_label(),
                    worktree.path.display()
                )
            },
        );
        Self::new(
            choice,
            label,
            filter,
            worktree.is_some_and(|worktree| worktree.current),
        )
    }

    fn create(label: String) -> Self {
        Self {
            choice: PickerChoice::Create,
            label,
            filter: "create switch branches".to_owned(),
            current: false,
            shortcut: true,
        }
    }

    #[cfg(test)]
    fn test(
        identity: usize,
        label: impl Into<String>,
        filter: impl Into<String>,
        current: bool,
        shortcut: bool,
    ) -> Self {
        Self {
            choice: PickerChoice::Worktree(identity),
            label: label.into(),
            filter: filter.into(),
            current,
            shortcut,
        }
    }
}

/// One navigation view's precomputed rows, menu items, and sort order.
struct PickerViewState {
    rows: Vec<Row>,
    items: Vec<PickerItem>,
    order: Vec<usize>,
}

impl PickerViewState {
    fn worktree(choices: &[&Worktree], sort: SortMode) -> Self {
        let rows: Vec<Row> = choices.iter().copied().map(Row::from_worktree).collect();
        let row_refs: Vec<&Row> = rows.iter().collect();
        let labels = render::menu_labels(&row_refs);
        let mut items = choices
            .iter()
            .enumerate()
            .zip(labels)
            .map(|((identity, worktree), label)| PickerItem::worktree(identity, worktree, label))
            .collect::<Vec<_>>();
        items.push(create_item());
        let mut order = sorted_row_indices(&row_refs, sort);
        order.push(items.len() - 1);
        Self { rows, items, order }
    }

    fn branch(
        repository: &Repository,
        choices: &[&Worktree],
        branches: &[git::BranchRecord],
        sort: SortMode,
    ) -> Self {
        let rows: Vec<Row> = branches
            .iter()
            .map(|record| Row::from_branch(record, &repository.worktrees))
            .collect();
        let row_refs: Vec<&Row> = rows.iter().collect();
        let labels = render::menu_labels(&row_refs);
        let mut items = branches
            .iter()
            .zip(labels)
            .map(|(record, label)| {
                let choice = choices
                    .iter()
                    .position(|worktree| worktree.has_branch(&record.branch))
                    .map_or_else(
                        || PickerChoice::Branch(record.branch.clone()),
                        PickerChoice::Worktree,
                    );
                PickerItem::branch(choice, record, repository, label)
            })
            .collect::<Vec<_>>();
        items.push(create_item());
        let mut order = sorted_row_indices(&row_refs, sort);
        order.push(items.len() - 1);
        Self { rows, items, order }
    }

    fn resort(&mut self, sort: SortMode) {
        let row_refs: Vec<&Row> = self.rows.iter().collect();
        let mut order = sorted_row_indices(&row_refs, sort);
        order.push(self.items.len() - 1);
        self.order = order;
    }
}

fn create_item() -> PickerItem {
    PickerItem::create(
        ui::interactive(ui::shortcut_style())
            .apply_to("+ Create or switch branches...")
            .to_string(),
    )
}

struct WorktreePicker {
    view: PickerView,
    worktree: PickerViewState,
    branch: PickerViewState,
    sort: SortMode,
    selected: usize,
    number_start: usize,
    viewport_rows: usize,
    terminal_columns: Option<usize>,
    filter: String,
}

impl WorktreePicker {
    fn new(
        repository: &Repository,
        choices: &[&Worktree],
        branches: &[git::BranchRecord],
        sort: SortMode,
        view: PickerView,
    ) -> Self {
        let worktree = PickerViewState::worktree(choices, sort);
        let branch = PickerViewState::branch(repository, choices, branches, sort);
        let current_choice = PickerChoice::Worktree(
            choices
                .iter()
                .position(|worktree| worktree.current)
                .unwrap_or(0),
        );
        let mut picker = Self {
            view,
            worktree,
            branch,
            sort,
            selected: 0,
            number_start: 1,
            viewport_rows: 20,
            terminal_columns: None,
            filter: String::new(),
        };
        picker.selected = picker.reselect(Some(current_choice));
        picker.number_start = picker.selected.saturating_add(1);
        picker
    }

    #[cfg(test)]
    fn new_test(items: Vec<PickerItem>, selected: usize) -> Self {
        let order: Vec<usize> = (0..items.len()).collect();
        let state = PickerViewState {
            rows: Vec::new(),
            items,
            order,
        };
        Self {
            view: PickerView::Worktree,
            worktree: state,
            branch: PickerViewState {
                rows: Vec::new(),
                items: Vec::new(),
                order: Vec::new(),
            },
            sort: SortMode::Git,
            selected,
            number_start: selected.saturating_add(1),
            viewport_rows: 20,
            terminal_columns: None,
            filter: String::new(),
        }
    }

    fn state(&self) -> &PickerViewState {
        match self.view {
            PickerView::Worktree => &self.worktree,
            PickerView::Branch => &self.branch,
        }
    }

    fn items(&self) -> &[PickerItem] {
        &self.state().items
    }

    fn order(&self) -> &[usize] {
        &self.state().order
    }

    fn rows(&self) -> &[Row] {
        &self.state().rows
    }

    fn reselect(&self, choice: Option<PickerChoice>) -> usize {
        let visible = self.visible();
        choice
            .and_then(|choice| {
                visible
                    .iter()
                    .position(|index| self.items()[*index].choice == choice)
            })
            .unwrap_or(0)
    }

    fn toggle_view(&mut self) {
        let selected_choice = self
            .visible()
            .get(self.selected)
            .map(|index| self.items()[*index].choice.clone());
        self.view = self.view.toggled();
        self.selected = self.reselect(selected_choice);
        self.number_start = self.selected.saturating_add(1);
    }

    fn interact(mut self) -> io::Result<PickerChoice> {
        let term = Term::stderr();
        if !term.is_term() {
            return Err(io::ErrorKind::NotConnected.into());
        }
        term.hide_cursor()?;
        let result = self.interact_inner(&term);
        term.show_cursor()?;
        result
    }

    fn interact_inner(&mut self, mut term: &Term) -> io::Result<PickerChoice> {
        let mut previous_frame = String::new();
        let mut shortcut_prefix = false;
        loop {
            let (rows, columns) = term.size();
            self.viewport_rows = picker_viewport_rows(rows);
            self.terminal_columns = (columns > 0).then_some(usize::from(columns));
            let visible = self.visible();
            if self.selected >= visible.len() {
                self.selected = 0;
            }
            let displayed_start = self.displayed_start(&visible);
            let displayed = self.displayed(&visible, displayed_start);
            let frame = self.render(displayed, displayed_start, visible.len());
            term.clear_last_lines(rendered_physical_rows(
                &previous_frame,
                self.terminal_columns,
            ))?;
            term.write_all(frame.as_bytes())?;
            term.flush()?;
            previous_frame = frame;
            let key = term.read_key_raw()?;
            if let Some(selection) = self.shortcut_selection(&key) {
                return Ok(selection);
            }
            if key == Key::Char('\u{13}') {
                self.cycle_sort();
                shortcut_prefix = false;
                continue;
            }
            if key == Key::Char('\u{2}') {
                self.toggle_view();
                shortcut_prefix = false;
                continue;
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
                            self.number_start + 8 == self.items().len() - 1,
                        )
                        .iter()
                        .find(|(_, shortcut)| *shortcut == number)
                    {
                        return Ok(self.items()[*index].choice.clone());
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
                    let action = self.items().len() - 1;
                    self.number_start = if self.selected + 9 <= action {
                        self.selected + 1
                    } else {
                        action.saturating_sub(8)
                    };
                }
                Key::Enter if !visible.is_empty() => {
                    return Ok(self.items()[visible[self.selected]].choice.clone());
                }
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

    fn cycle_sort(&mut self) {
        let selected_choice = self
            .visible()
            .get(self.selected)
            .map(|index| self.items()[*index].choice.clone());
        self.sort = self.sort.next();
        self.worktree.resort(self.sort);
        self.branch.resort(self.sort);
        self.selected = self.reselect(selected_choice);
        self.number_start = self.selected.saturating_add(1);
    }

    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        let items = self.items();
        self.order()
            .iter()
            .copied()
            .filter(|index| items[*index].filter.to_lowercase().contains(&needle))
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

    fn displayed<'visible>(&self, visible: &'visible [usize], start: usize) -> &'visible [usize] {
        let has_above = start > 0;
        let rows_before_bottom_hint = self.viewport_rows.saturating_sub(usize::from(has_above));
        let has_below = visible.len() > start + rows_before_bottom_hint;
        let item_rows = rows_before_bottom_hint.saturating_sub(usize::from(has_below));
        &visible[start..visible.len().min(start + item_rows)]
    }

    fn shortcut_selection(&self, key: &Key) -> Option<PickerChoice> {
        if key != &Key::BackTab {
            return None;
        }
        self.items()
            .iter()
            .find(|item| item.choice == PickerChoice::Create)
            .map(|item| item.choice.clone())
    }

    fn numbered(
        &self,
        visible: &[usize],
        selected: usize,
        number_start: usize,
        pinned_at_bottom: bool,
    ) -> Vec<(usize, usize)> {
        let items = self.items();
        visible[number_start.min(visible.len())..]
            .iter()
            .copied()
            .filter(|index| {
                items[*index].shortcut
                    && (pinned_at_bottom || visible.get(selected).copied() != Some(*index))
            })
            .take(9)
            .enumerate()
            .map(|(offset, index)| (index, offset + 1))
            .collect()
    }

    fn render(&self, visible: &[usize], displayed_start: usize, visible_len: usize) -> String {
        let mut output = String::new();
        self.render_header(&mut output);
        let numbered = self.numbered(
            visible,
            self.selected.saturating_sub(displayed_start),
            self.number_start.saturating_sub(displayed_start),
            self.number_start + 8 == self.items().len() - 1,
        );
        let pinned_at_bottom = self.number_start + 8 == self.items().len() - 1;
        if visible_len == 0 {
            self.render_hint(&mut output, self.view.no_match_hint().to_owned());
        } else if displayed_start > 0 {
            self.render_hint(&mut output, format!("↑ {displayed_start} more above"));
        }
        for (position, index) in visible.iter().enumerate() {
            self.render_choice(
                &mut output,
                *index,
                displayed_start + position,
                pinned_at_bottom,
                &numbered,
            );
        }
        let more_below = visible_len.saturating_sub(displayed_start + visible.len());
        if more_below > 0 {
            self.render_hint(&mut output, format!("↓ {more_below} more below"));
        }
        self.render_footer(&mut output);
        output
    }

    fn render_header(&self, output: &mut String) {
        let heading_prefix = format!("{}  ", ui::interactive(ui::accent_style()).apply_to("◆"));
        let heading = ui::interactive(ui::heading_style())
            .apply_to(self.view.heading())
            .to_string();
        self.write_fitted_line(output, &heading_prefix, &heading);

        let filter_prefix = format!("{}  ", ui::interactive(ui::accent_style()).apply_to("│"));
        let filter = if self.filter.is_empty() {
            "type to filter"
        } else {
            &self.filter
        };
        let filter = ui::interactive(ui::muted_style())
            .apply_to(filter)
            .to_string();
        self.write_fitted_line(output, &filter_prefix, &filter);

        let row_refs: Vec<&Row> = self.rows().iter().collect();
        let columns = ui::interactive(ui::muted_style())
            .apply_to(format!(
                "{PICKER_CHOICE_PREFIX}{}",
                render::menu_header(&row_refs, self.sort)
            ))
            .to_string();
        self.write_fitted_line(output, &filter_prefix, &columns);
    }

    fn render_hint(&self, output: &mut String, message: String) {
        let prefix = format!("{}  ", ui::interactive(ui::accent_style()).apply_to("│"));
        let message = ui::interactive(ui::muted_style())
            .apply_to(message)
            .to_string();
        self.write_fitted_line(output, &prefix, &message);
    }

    fn render_choice(
        &self,
        output: &mut String,
        index: usize,
        position: usize,
        pinned_at_bottom: bool,
        numbered: &[(usize, usize)],
    ) {
        let items = self.items();
        let marker = if items[index].current {
            ui::interactive(ui::accent_style().bold())
                .apply_to("*")
                .to_string()
        } else if position == self.selected && !pinned_at_bottom {
            " ".to_owned()
        } else {
            numbered
                .iter()
                .find_map(|(numbered_index, number)| (*numbered_index == index).then_some(number))
                .map_or_else(
                    || " ".to_owned(),
                    |number| {
                        ui::interactive(ui::shortcut_style())
                            .apply_to(number)
                            .to_string()
                    },
                )
        };
        let is_selected = position == self.selected;
        let selected = if is_selected {
            ui::interactive(ui::accent_style()).apply_to("●")
        } else {
            ui::interactive(ui::muted_style()).apply_to("○")
        };
        let label = if is_selected {
            ui::interactive(ui::selected_style())
                .apply_to(strip_ansi_codes(&items[index].label))
                .to_string()
        } else {
            items[index].label.clone()
        };
        let prefix = format!(
            "{}  {selected} {marker} ",
            ui::interactive(ui::accent_style()).apply_to("│"),
        );
        let prefix_width = UnicodeWidthStr::width(strip_ansi_codes(&prefix).as_ref());
        if self
            .terminal_columns
            .is_some_and(|columns| prefix_width >= columns)
        {
            let marker = (!strip_ansi_codes(&marker).trim().is_empty()).then_some(marker);
            let compact = format!("{selected}{}{label}", marker.as_deref().unwrap_or(""));
            self.write_compact_line(output, &compact);
        } else {
            self.write_fitted_line(output, &prefix, &label);
        }
    }

    fn render_footer(&self, output: &mut String) {
        writeln!(
            output,
            "{}",
            ui::interactive(ui::accent_style()).apply_to("│")
        )
        .expect("writing to a string cannot fail");
        let prefix = format!("{}  ", ui::interactive(ui::accent_style()).apply_to("└"));
        let help = picker_help();
        self.write_fitted_line(output, &prefix, &help);
    }

    fn write_fitted_line(&self, output: &mut String, prefix: &str, content: &str) {
        writeln!(output, "{}", self.fit_line(prefix, content))
            .expect("writing to a string cannot fail");
    }

    fn write_compact_line(&self, output: &mut String, content: &str) {
        let content = self.terminal_columns.map_or_else(
            || content.to_owned(),
            |columns| truncate_styled(content, columns),
        );
        writeln!(output, "{content}").expect("writing to a string cannot fail");
    }

    fn fit_line(&self, prefix: &str, content: &str) -> String {
        let Some(terminal_columns) = self.terminal_columns else {
            return format!("{prefix}{content}");
        };
        let plain_prefix = strip_ansi_codes(prefix);
        let prefix_width = UnicodeWidthStr::width(plain_prefix.as_ref());
        if prefix_width >= terminal_columns {
            return truncate_styled(content, terminal_columns);
        }
        let available_width = terminal_columns - prefix_width;
        format!("{prefix}{}", truncate_styled(content, available_width))
    }
}

fn rendered_physical_rows(frame: &str, terminal_columns: Option<usize>) -> usize {
    let Some(terminal_columns) = terminal_columns.filter(|columns| *columns > 0) else {
        return frame.lines().count();
    };
    frame
        .lines()
        .map(|line| {
            let plain = strip_ansi_codes(line);
            let width = UnicodeWidthStr::width(plain.as_ref());
            width.saturating_sub(1) / terminal_columns + 1
        })
        .sum()
}

fn truncate_styled(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        String::new()
    } else {
        truncate_str(value, max_width, "…").into_owned()
    }
}

fn picker_help() -> String {
    let mut output = String::new();
    for (index, (shortcut, description)) in [
        ("↑/↓", "navigate"),
        ("Ctrl-A then 1–9", "select"),
        ("Shift-Tab", "create"),
        ("Ctrl-S", "sort"),
        ("Ctrl-B", "branches"),
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
    let value: String = ui::prompt_result(
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

fn resolve_and_switch(repository: &Repository, branch: &str, intent: Intent) -> Result<()> {
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
        if intent == Intent::Create {
            return Err(already_registered(branch, &worktree.path));
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

    create_worktree(repository, branch, &plan, intent)
}

fn already_registered(branch: &str, path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "branch {branch:?} is already registered at {}; enter it with 'worktrees switch {branch}'",
        path.display()
    )
}

fn choose_remote(remotes: &[String], branch: &str) -> Result<String> {
    ui::ensure_interactive(&format!(
        "multiple remote-tracking branches match {branch:?}; choose in a terminal: {}",
        remotes.join(", ")
    ))?;
    let mut prompt = select(format!("Choose the upstream for {branch}"))
        .initial_value(0)
        .max_rows(standard_selection_viewport_rows(Term::stderr().size().0));
    for (index, remote) in remotes.iter().enumerate() {
        prompt = prompt.item(index, remote, "");
    }
    let selection = ui::prompt_result(
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

fn create_worktree(
    repository: &Repository,
    branch: &str,
    kind: &CreationKind,
    intent: Intent,
) -> Result<()> {
    let config = EffectiveConfig::load(repository)?;
    let destination = config.require_root()?.join(branch);
    let destination = git::canonical_or_normalized(&destination)
        .context("failed to resolve worktree destination")?;
    validate_destination(repository, branch, &destination)?;

    if let CreationKind::New { head } = kind {
        match intent {
            Intent::Switch => confirm_new_branch(repository, branch, head, &destination)?,
            Intent::Create => announce_new_branch(repository, branch, head, &destination)?,
        }
    }
    approve_hooks(repository, HookPhase::PostCreate, &config.post_create)?;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create destination parent {}", parent.display()))?;
    }
    let pending = (!config.post_create.is_empty())
        .then(|| setup::prepare(&repository.common_dir, branch, &destination))
        .transpose()?;
    let creation = ui::run_timed(
        true,
        "Creating worktree...",
        "Created worktree",
        "Failed to create worktree",
        |_| match kind {
            CreationKind::Local => {
                git::add_existing_worktree(&repository.current().path, &destination, branch)
            }
            CreationKind::Remote(remote) => {
                git::add_tracking_worktree(&repository.current().path, &destination, branch, remote)
            }
            CreationKind::New { head } => {
                git::add_new_worktree(&repository.current().path, &destination, branch, head)
            }
        },
    );
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
    ui::ensure_interactive("new branch creation requires confirmation")?;
    ui::info(format!(
        "Create branch {branch:?} from {} at {}?",
        new_branch_source(repository, head),
        destination.display()
    ))?;
    warn_dirty_source(repository)?;
    let confirmed = ui::prompt_result(
        confirm("Create this branch and worktree?")
            .initial_value(false)
            .interact(),
        "branch creation cancelled",
        "failed to read new-branch confirmation",
    )?;
    if !confirmed {
        return Err(ui::declined(
            "branch creation declined; no worktree was created",
        ));
    }
    Ok(())
}

/// Reports the branch about to be created without asking to confirm it.
fn announce_new_branch(
    repository: &Repository,
    branch: &str,
    head: &str,
    destination: &Path,
) -> Result<()> {
    ui::info(format!(
        "Creating branch {branch:?} from {} at {}.",
        new_branch_source(repository, head),
        destination.display()
    ))?;
    warn_dirty_source(repository)
}

fn new_branch_source(repository: &Repository, head: &str) -> String {
    match &repository.current().kind {
        WorktreeKind::Branch(source) => format!("branch {source:?} at {head}"),
        WorktreeKind::Detached => format!("detached commit {head}"),
        _ => format!("commit {head}"),
    }
}

fn warn_dirty_source(repository: &Repository) -> Result<()> {
    if git::is_dirty(&repository.current().path)? {
        ui::warning("Staged, unstaged, and untracked changes remain in the source worktree.")?;
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
    ui::ensure_interactive(&format!("{} require approval", phase.plural_name()))?;
    ui::info(format!(
        "The repository requests these {}:",
        phase.plural_name()
    ))?;
    for (index, step) in steps.iter().enumerate() {
        ui::step(format!("{}: {}", step.label(index), step.command))?;
    }
    let confirmed = ui::prompt_result(
        confirm("Trust and run these commands for this repository?")
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
    ui::ensure_interactive("incomplete setup requires a recovery choice")?;
    let choices = ["Retry setup", "Enter once", "Mark setup complete and enter"];
    let mut prompt = select("Setup did not complete for this worktree").initial_value(0);
    for (index, choice) in choices.iter().enumerate() {
        prompt = prompt.item(index, choice, "");
    }
    let choice = ui::prompt_result(
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
    use unicode_width::UnicodeWidthStr;

    use super::{
        PickerChoice, PickerItem, PickerView, WorktreePicker, picker_help, picker_viewport_rows,
        port_for_branch, rendered_physical_rows,
    };
    use crate::{
        Condition, SortMode, Worktree, WorktreeKind,
        git::{BranchRecord, Repository},
        ui,
    };
    use std::path::PathBuf;

    fn picker_item(
        identity: usize,
        label: impl Into<String>,
        current: bool,
        shortcut: bool,
    ) -> PickerItem {
        let label = label.into();
        PickerItem::test(identity, label.clone(), label, current, shortcut)
    }

    fn worktree(path: &str, branch: &str, current: bool) -> Worktree {
        Worktree {
            path: path.into(),
            head: None,
            last_commit_at: None,
            kind: WorktreeKind::Branch(branch.to_owned()),
            locked: None,
            prunable: None,
            current,
            condition: Condition::Clean,
        }
    }

    fn picker_from_worktrees(worktrees: &[&Worktree], sort: SortMode) -> WorktreePicker {
        picker_from_worktrees_and_branches(worktrees, &[], sort)
    }

    fn picker_from_worktrees_and_branches(
        worktrees: &[&Worktree],
        branches: &[BranchRecord],
        sort: SortMode,
    ) -> WorktreePicker {
        let repository = Repository {
            worktrees: worktrees
                .iter()
                .map(|worktree| (*worktree).clone())
                .collect(),
            current_index: 0,
            primary: None,
            common_dir: PathBuf::new(),
            metadata_warning: None,
        };
        WorktreePicker::new(&repository, worktrees, branches, sort, PickerView::Worktree)
    }

    #[test]
    fn picker_reserves_rows_for_column_headers_without_scrolling() {
        assert_eq!(picker_viewport_rows(20), 14);
        assert_eq!(picker_viewport_rows(5), 1);
    }

    #[test]
    fn picker_stops_scrolling_at_the_last_full_page() {
        let mut picker = WorktreePicker::new_test(
            (0..20)
                .map(|index| picker_item(index, index.to_string(), false, true))
                .collect(),
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
        let mut picker = WorktreePicker::new_test(
            (0..20)
                .map(|index| picker_item(index, index.to_string(), false, true))
                .collect(),
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
    fn picker_aligns_column_headers_with_choice_values() {
        let worktree = worktree("/repo", "main", true);
        let picker = picker_from_worktrees(&[&worktree], SortMode::Git);
        let frame = picker.render(&[0], 0, 1);
        let rendered = strip_ansi_codes(&frame);
        let header = rendered
            .lines()
            .find(|line| line.contains("BRANCH"))
            .unwrap();
        let choice = rendered.lines().find(|line| line.contains("main")).unwrap();

        let header_column = UnicodeWidthStr::width(&header[..header.find("BRANCH").unwrap()]);
        let choice_column = UnicodeWidthStr::width(&choice[..choice.find("main").unwrap()]);

        assert_eq!(header_column, choice_column, "{rendered}");
    }

    #[test]
    fn picker_renders_selected_label_in_white() {
        let first = ui::interactive(ui::worktree_data_style())
            .apply_to("first")
            .to_string();
        let second = ui::interactive(ui::worktree_data_style())
            .apply_to("second")
            .to_string();
        let picker = WorktreePicker::new_test(
            vec![
                PickerItem::test(0, first, "first", true, false),
                PickerItem::test(1, second.clone(), "second", false, true),
            ],
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
    fn picker_renders_a_no_match_state_without_a_selection() {
        let mut picker = WorktreePicker::new_test(vec![picker_item(0, "main", true, false)], 0);
        picker.filter = "missing".to_owned();

        let rendered = picker.render(&[], 0, 0);

        assert!(rendered.contains("No worktrees match this filter"));
    }

    #[test]
    fn picker_truncates_lines_to_the_terminal_width() {
        let label = ui::interactive(ui::worktree_data_style())
            .apply_to("feature/with-a-very-long-name  ~/a/path/that-is-too-wide")
            .to_string();
        let mut picker =
            WorktreePicker::new_test(vec![PickerItem::test(0, label, "feature", true, false)], 0);
        let terminal_columns = 48;
        picker.terminal_columns = Some(terminal_columns);
        let rendered = picker.render(&[0], 0, 1);

        assert!(rendered.lines().all(|line| {
            let plain = strip_ansi_codes(line);
            UnicodeWidthStr::width(plain.as_ref()) <= terminal_columns
        }));
    }

    #[test]
    fn picker_compacts_markers_when_structural_chrome_does_not_fit() {
        let mut picker = WorktreePicker::new_test(
            vec![
                picker_item(0, "main", true, false),
                picker_item(1, "feature", false, true),
            ],
            0,
        );
        picker.terminal_columns = Some(4);

        let rendered = picker.render(&[0, 1], 0, 2);
        let plain = strip_ansi_codes(&rendered);

        assert!(rendered.lines().all(|line| {
            let plain = strip_ansi_codes(line);
            UnicodeWidthStr::width(plain.as_ref()) <= 4
        }));
        assert!(plain.contains("●*m…"), "{plain}");
        assert!(plain.contains("○1f…"), "{plain}");
    }

    #[test]
    fn picker_preserves_semantic_styles_when_truncating() {
        let branch_style = ui::worktree_data_style().bold().force_styling(true);
        let warning_style = ui::warning_style().force_styling(true);
        let dirty_label = format!(
            "{} {} suffix",
            branch_style.apply_to("dirty"),
            warning_style.apply_to("*")
        );
        let mut picker = WorktreePicker::new_test(
            vec![
                picker_item(0, "first", true, false),
                PickerItem::test(1, dirty_label, "dirty", false, true),
            ],
            0,
        );
        picker.terminal_columns = Some(15);

        let rendered = picker.render(&[0, 1], 0, 2);

        assert!(rendered.contains(&branch_style.apply_to("dirty").to_string()));
        assert!(rendered.contains(&warning_style.apply_to("*").to_string()));
    }

    #[test]
    fn picker_counts_wrapped_physical_rows_at_the_current_width() {
        assert_eq!(rendered_physical_rows("12345\n12\n", Some(4)), 3);
    }

    #[test]
    fn picker_shift_tab_selects_the_create_action() {
        let picker = WorktreePicker::new_test(
            vec![
                picker_item(0, "main", true, false),
                PickerItem::create("Create".to_owned()),
            ],
            0,
        );

        assert_eq!(
            picker.shortcut_selection(&Key::BackTab),
            Some(PickerChoice::Create)
        );
        assert_eq!(picker.shortcut_selection(&Key::CtrlC), None);
    }

    #[test]
    fn picker_help_has_no_sort_mode_wording() {
        let help = strip_ansi_codes(&picker_help()).to_string();

        assert!(help.contains("Ctrl-S sort"), "{help}");
        assert!(help.ends_with("Esc/Ctrl-C cancel"), "{help}");
        for label in [
            "Git order",
            "branch A-Z",
            "last commit newest-first",
            "path A-Z",
        ] {
            assert!(!help.contains(label), "{help}");
        }
    }

    #[test]
    fn picker_sorting_preserves_choice_identity_and_pins_create_last() {
        let main = worktree("/repo/main", "z-main", true);
        let feature = worktree("/repo/feature", "a-feature", false);
        let mut picker = picker_from_worktrees(&[&main, &feature], SortMode::Git);

        picker.cycle_sort();

        let visible = picker.visible();
        assert_eq!(
            picker.items()[visible[picker.selected]].choice,
            PickerChoice::Worktree(0)
        );
        assert_eq!(
            picker.items()[*visible.last().unwrap()].choice,
            PickerChoice::Create
        );

        picker.selected = visible.len() - 1;
        picker.cycle_sort();

        let visible = picker.visible();
        assert_eq!(
            picker.items()[visible[picker.selected]].choice,
            PickerChoice::Create
        );
        assert_eq!(
            picker.items()[*visible.last().unwrap()].choice,
            PickerChoice::Create
        );
    }

    #[test]
    fn ports_match_worktrunk_v0_66_golden_values() {
        assert_eq!(port_for_branch("main"), 12_107);
        assert_eq!(port_for_branch("feature/test"), 18_064);
        assert_eq!(port_for_branch("føø/分支"), 17_537);
    }
}
