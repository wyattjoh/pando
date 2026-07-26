use std::{
    env, fs,
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use cliclack::{confirm, input, select};
use siphasher::sip::SipHasher13;

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, HookPhase, HookStep},
    git::{self, Repository},
    render,
    setup::{self, HookOutcome},
    trust,
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
                    println!("No {} are configured.", phase.key());
                } else if trusted {
                    println!("The current {} are trusted.", phase.key());
                } else {
                    println!("The current {} are not trusted.", phase.key());
                }
            }
        }
        TrustCommand::Reset => {
            if trust::reset(&repository)? {
                println!("Reset hook trust for this repository.");
            } else {
                println!("No saved hook trust existed for this repository.");
            }
        }
        TrustCommand::CommitStatus => {
            let config = EffectiveConfig::load(&repository)?;
            if config.generation.command.is_none() {
                println!("No commit generator is configured.");
            } else if trust::generation_hash(&config.generation).is_none() {
                println!("The effective commit generator is user-controlled.");
            } else if trust::is_generation_trusted(&repository, &config.generation)? {
                println!("The effective shared commit generator is trusted.");
            } else {
                println!("The effective shared commit generator is not trusted.");
            }
        }
        TrustCommand::CommitReset => {
            if trust::reset_generation(&repository)? {
                println!("Reset commit generator trust for this repository.");
            } else {
                println!("No saved commit generator trust existed for this repository.");
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
    let mut labels = render::menu_labels(&choices);
    labels.push("Create or switch branch…".to_owned());
    let default = choices
        .iter()
        .position(|worktree| worktree.current)
        .unwrap_or(0);
    let selection: String = prompt_result(
        input("Choose a worktree")
            .default_input(&labels[default])
            .autocomplete(labels.clone())
            .interact(),
        "selection cancelled",
        "failed to read worktree selection from the terminal",
    )?;
    if let Some(index) = labels[..choices.len()]
        .iter()
        .position(|label| label == &selection)
    {
        let chosen = choices[index];
        let branch = match &chosen.kind {
            WorktreeKind::Branch(branch) => Some(branch.as_str()),
            _ => None,
        };
        return enter_existing(repository, &chosen.path, branch);
    }

    if selection != *labels.last().expect("branch action was added") {
        bail!("choose a worktree or the branch creation action from the suggestions");
    }
    let branch = read_branch_name()?;
    resolve_and_switch(repository, &branch)
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
    eprintln!(
        "Create branch {branch:?} from {source} at {}?",
        destination.display()
    );
    if git::is_dirty(&repository.current().path)? {
        eprintln!(
            "Warning: staged, unstaged, and untracked changes remain in the source worktree."
        );
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
    eprintln!("The repository requests these {}:", phase.plural_name());
    for (index, step) in steps.iter().enumerate() {
        eprintln!("  {}: {}", step.label(index), step.command);
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
            eprintln!("Warning: entering once while setup remains incomplete.");
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
    use super::port_for_branch;

    #[test]
    fn ports_match_worktrunk_v0_66_golden_values() {
        assert_eq!(port_for_branch("main"), 12_107);
        assert_eq!(port_for_branch("feature/test"), 18_064);
        assert_eq!(port_for_branch("føø/分支"), 17_537);
    }
}
