use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use worktrees::{
    commit, git, install, render,
    smart::{self, GetProperty, TrustCommand},
};

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
    /// Choose, create, or switch to a worktree and print its path.
    Switch {
        /// Branch to switch to; omit it to use the interactive picker.
        branch: Option<String>,
    },
    /// Print one current-worktree property.
    Get {
        #[arg(value_enum)]
        property: GetProperty,
    },
    /// Remove one or more topic worktrees while retaining their branches.
    Remove {
        #[arg(long)]
        force: bool,
        branches: Vec<String>,
    },
    /// Integrate the current topic into the configured target branch.
    Merge {
        #[arg(long)]
        no_rebase: bool,
        #[arg(long)]
        no_remove: bool,
    },
    /// Stage all changes and create a commit.
    Commit {
        /// Commit message; omit to use the configured generator.
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Inspect or revoke post-create hook or commit-generation approval.
    Trust {
        #[command(subcommand)]
        command: TrustCommand,
    },
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
        Commands::Switch { branch } => smart::switch(branch),
        Commands::Get { property } => smart::get(property),
        Commands::Commit { message } => commit::run(message),
        Commands::Remove { force, branches } => worktrees::lifecycle::remove(&branches, force),
        Commands::Merge {
            no_rebase,
            no_remove,
        } => worktrees::lifecycle::merge(no_rebase, no_remove),
        Commands::Trust { command } => smart::trust_command(command),
        Commands::Install => install::run(),
    }
}

fn list() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let worktrees = git::discover(&cwd)?;
    print!("{}", render::table(&worktrees));
    Ok(())
}
