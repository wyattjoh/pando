use std::{
    env,
    io::{self, Write},
    os::unix::ffi::OsStrExt,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use dialoguer::{Select, theme::ColorfulTheme};
use worktrees::{git, install, render};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// List worktrees belonging to the current repository.
    List,
    /// Interactively choose a worktree and print its path.
    Switch,
    /// Install the managed zsh integration.
    Install,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::List => list(),
        Commands::Switch => switch(),
        Commands::Install => install::run(),
    }
}

fn list() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let worktrees = git::discover(&cwd)?;
    print!("{}", render::table(&worktrees));
    Ok(())
}

fn switch() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let worktrees = git::discover(&cwd)?;
    let choices: Vec<_> = worktrees
        .iter()
        .filter(|worktree| worktree.navigable())
        .collect();
    if choices.is_empty() {
        bail!("the current repository has no navigable worktrees");
    }
    let labels = render::menu_labels(&choices);
    let default = choices
        .iter()
        .position(|worktree| worktree.current)
        .unwrap_or(0);
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Choose a worktree")
        .items(&labels)
        .default(default)
        .max_length(20)
        .interact_opt()
        .context("failed to read worktree selection from the terminal")?;
    if let Some(index) = selection {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(choices[index].path.as_os_str().as_bytes())
            .context("failed to write the selected worktree path")?;
        stdout
            .write_all(b"\n")
            .context("failed to terminate the selected worktree path")?;
    } else {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "selection cancelled").into());
    }
    Ok(())
}
