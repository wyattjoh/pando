use std::env;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use worktrees::{
    commit, git, install, render,
    smart::{self, GetProperty, TrustCommand},
    ui,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    #[default]
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Select human terminal output or one structured JSON document.
    #[arg(long, value_enum, global = true, default_value_t = OutputFormat::Human)]
    output: OutputFormat,
    /// Read a versioned JSON request from stdin and emit JSON.
    #[arg(long, value_enum, global = true)]
    input_output: Option<OutputFormat>,
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
    /// Commit the existing index, optionally staging every change first.
    Commit {
        /// Commit message; omit to use the configured generator.
        #[arg(short, long)]
        message: Option<String>,
        /// Stage tracked, deleted, and untracked changes before committing.
        #[arg(long)]
        stage_all: bool,
        /// Validate and preview without staging, generating, or committing.
        #[arg(long)]
        dry_run: bool,
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
    let args: Vec<_> = env::args_os().collect();
    let json_requested = args
        .windows(2)
        .any(|pair| (pair[0] == "--output" || pair[0] == "--input-output") && pair[1] == "json")
        || args
            .iter()
            .any(|arg| arg == "--output=json" || arg == "--input-output=json");
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) if json_requested => {
            let code = if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                0
            } else {
                2
            };
            commit::render_clap_json(&args, &error);
            std::process::exit(code);
        }
        Err(error) => error.exit(),
    };
    if let Err(error) = run(cli) {
        let interaction = error.downcast_ref::<ui::InteractionError>();
        let rendered = match interaction {
            Some(interaction) if interaction.is_cancelled() => ui::cancel(interaction),
            Some(interaction) => ui::warning(interaction).and_then(|()| {
                interaction.completion().map_or(Ok(()), |completion| {
                    ui::finish(ui::warning_style().apply_to(completion))
                })
            }),
            None => {
                let message = format!("error: {error:#}");
                let rendered = ui::error(&message);
                if rendered.is_err() {
                    eprintln!("{message}");
                }
                rendered
            }
        };
        if rendered.is_err()
            && let Some(interaction) = interaction
        {
            eprintln!("{interaction}");
        }
        if rendered.is_ok() && interaction.is_some_and(ui::InteractionError::is_successful) {
            return;
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    ui::install_theme();
    let json = cli.output == OutputFormat::Json || cli.input_output == Some(OutputFormat::Json);
    let request_mode = cli.input_output == Some(OutputFormat::Json);
    if cli.input_output == Some(OutputFormat::Human) {
        anyhow::bail!("--input-output only supports json");
    }
    match cli.command {
        Commands::List if json => commit::render_unsupported("list"),
        Commands::List => list(),
        Commands::Switch { .. } if json => commit::render_unsupported("switch"),
        Commands::Switch { branch } => {
            smart::switch(branch)?;
            ui::finish(ui::success_style().apply_to("Worktree destination printed."))
        }
        Commands::Get { .. } if json => commit::render_unsupported("get"),
        Commands::Get { property } => {
            smart::get(property)?;
            ui::finish(ui::muted_style().apply_to(format!("{property:?} printed.")))
        }
        Commands::Commit {
            message,
            stage_all,
            dry_run,
        } => commit::run(commit::Invocation {
            message,
            stage_all,
            dry_run,
            json,
            request_mode,
        }),
        Commands::Remove { .. } if json => commit::render_unsupported("remove"),
        Commands::Remove { force, branches } => worktrees::lifecycle::remove(&branches, force),
        Commands::Merge { .. } if json => commit::render_unsupported("merge"),
        Commands::Merge {
            no_rebase,
            no_remove,
        } => worktrees::lifecycle::merge(no_rebase, no_remove),
        Commands::Trust { .. } if json => commit::render_unsupported("trust"),
        Commands::Trust { command } => {
            smart::trust_command(command)?;
            let summary = match command {
                TrustCommand::Status => "Hook trust status checked.",
                TrustCommand::Reset => "Hook trust reset checked.",
                TrustCommand::CommitStatus => "Commit generator trust status checked.",
                TrustCommand::CommitReset => "Commit generator trust reset checked.",
                TrustCommand::CommitApprove => "Commit generator trust approval checked.",
            };
            ui::finish(ui::muted_style().apply_to(summary))
        }
        Commands::Install if json => commit::render_unsupported("install"),
        Commands::Install => install::run(),
    }
}

fn list() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let worktrees = git::discover(&cwd)?;
    ui::info(format!(
        "{}\n{}",
        ui::heading_style().apply_to("Worktrees"),
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
    ui::muted_style().apply_to(summary.join(", ")).to_string()
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
