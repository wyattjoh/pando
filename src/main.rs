use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use console::style;
use worktrees::{
    commit, git, install, render,
    smart::{self, GetProperty, TrustCommand},
    ui,
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
        let message = format!("error: {error:#}");
        if ui::error(&message).is_err() {
            eprintln!("{message}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    ui::install_theme();
    match Cli::parse().command {
        Commands::List => list(),
        Commands::Switch { branch } => {
            smart::switch(branch)?;
            ui::finish(style("Worktree destination printed.").green().bold())
        }
        Commands::Get { property } => {
            smart::get(property)?;
            ui::finish(style(format!("{property:?} printed.")).dim())
        }
        Commands::Commit { message } => commit::run(message),
        Commands::Remove { force, branches } => worktrees::lifecycle::remove(&branches, force),
        Commands::Merge {
            no_rebase,
            no_remove,
        } => worktrees::lifecycle::merge(no_rebase, no_remove),
        Commands::Trust { command } => {
            smart::trust_command(command)?;
            let summary = match command {
                TrustCommand::Status => "Hook trust status checked.",
                TrustCommand::Reset => "Hook trust reset checked.",
                TrustCommand::CommitStatus => "Commit generator trust status checked.",
                TrustCommand::CommitReset => "Commit generator trust reset checked.",
            };
            ui::finish(style(summary).dim())
        }
        Commands::Install => install::run(),
    }
}

fn list() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let worktrees = git::discover(&cwd)?;
    ui::info(format!(
        "{}\n{}",
        ui::header_style().apply_to("Worktrees"),
        render::table(&worktrees)
    ))?;
    ui::finish(list_summary(&worktrees))
}

fn list_summary(worktrees: &[worktrees::Worktree]) -> String {
    use worktrees::Condition;

    let mut summary = vec![format!(
        "{} worktree{}",
        worktrees.len(),
        plural(worktrees.len())
    )];
    for (label, count) in [
        (
            "dirty",
            worktrees
                .iter()
                .filter(|worktree| worktree.condition == Condition::Dirty)
                .count(),
        ),
        (
            "unknown",
            worktrees
                .iter()
                .filter(|worktree| worktree.condition == Condition::Unknown)
                .count(),
        ),
        (
            "missing",
            worktrees
                .iter()
                .filter(|worktree| worktree.condition == Condition::Missing)
                .count(),
        ),
        (
            "inaccessible",
            worktrees
                .iter()
                .filter(|worktree| worktree.condition == Condition::Inaccessible)
                .count(),
        ),
        (
            "bare",
            worktrees
                .iter()
                .filter(|worktree| worktree.is_bare())
                .count(),
        ),
        (
            "locked",
            worktrees
                .iter()
                .filter(|worktree| worktree.locked.is_some())
                .count(),
        ),
        (
            "prunable",
            worktrees
                .iter()
                .filter(|worktree| worktree.prunable.is_some())
                .count(),
        ),
    ] {
        if count > 0 {
            summary.push(format!("{count} {label}"));
        }
    }
    style(summary.join(", ")).dim().to_string()
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
