use std::env;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCandidates;
use pando::{
    Row, commit, completion,
    config::EffectiveConfig,
    git, install, machine, pr, render,
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
// `name` is pinned rather than inherited from `CARGO_PKG_NAME`: the package is
// `pando-cli` (the bare crate name is taken on crates.io) and `--version` would
// otherwise report a name no user ever types.
#[command(name = "pando", version, about)]
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
    List {
        /// List local branches instead of worktrees.
        #[arg(short = 'b', long)]
        branches: bool,
    },
    /// Choose, create, or switch to a worktree and print its path.
    Switch {
        /// Branch to switch to; omit it to use the interactive picker.
        #[arg(add = ArgValueCandidates::new(completion::switch_candidates))]
        branch: Option<String>,
        /// Open the interactive picker in branch view.
        #[arg(short = 'b', long)]
        branches: bool,
        /// Refresh the fresh base ref before creating a genuinely new branch.
        #[arg(long)]
        fetch: bool,
        /// Validate and preview without mutation.
        #[arg(long)]
        dry_run: bool,
    },
    /// Create a worktree for a branch and print its path, without confirming a new branch.
    Create {
        /// Branch to create a worktree for.
        #[arg(add = ArgValueCandidates::new(completion::create_candidates))]
        branch: Option<String>,
        /// Refresh the fresh base ref before creating a genuinely new branch.
        #[arg(long)]
        fetch: bool,
        /// Validate and preview without mutation.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print one current-worktree property.
    Get {
        #[arg(value_enum)]
        property: Option<GetProperty>,
    },
    /// Remove one or more topic worktrees while retaining their branches.
    Remove {
        #[arg(long)]
        force: bool,
        /// Validate and preview without mutation.
        #[arg(long)]
        dry_run: bool,
        #[arg(add = ArgValueCandidates::new(completion::remove_candidates))]
        branches: Vec<String>,
    },
    /// Integrate the current topic into the configured target branch.
    Merge {
        #[arg(long)]
        no_rebase: bool,
        #[arg(long)]
        no_remove: bool,
        /// Merge the topic's commits as they are instead of squashing them into one.
        #[arg(long)]
        no_squash: bool,
        /// Stage and commit all changes before merging.
        #[arg(long, conflicts_with = "dry_run")]
        yolo: bool,
        /// Validate and preview without mutation.
        #[arg(long)]
        dry_run: bool,
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
        /// Preview a reset or approval without writing trust.
        #[arg(long)]
        dry_run: bool,
        #[command(subcommand)]
        command: TrustCommand,
    },
    /// Create a pull request from the current published topic branch.
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
    /// Install the managed zsh integration and configure Pando with an LLM.
    Install {
        /// Preview installation without writing, prompting, or launching an agent.
        #[arg(long)]
        dry_run: bool,
        /// Install the shell integration without launching guided configuration.
        #[arg(long)]
        no_guide: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PrCommand {
    Create {
        #[arg(short, long)]
        title: Option<String>,
        #[arg(long, conflicts_with = "description_file")]
        description: Option<String>,
        #[arg(long, conflicts_with = "description")]
        description_file: Option<String>,
        #[arg(long, value_enum, default_value_t)]
        status: pr::Status,
        #[arg(long)]
        dry_run: bool,
        /// Select the remote that owns the pull request head.
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        force: bool,
        /// Commit all changes and create a ready pull request without confirmation.
        #[arg(long, conflicts_with_all = ["status", "force", "dry_run"])]
        yolo: bool,
    },
}

fn main() {
    // Must precede every other statement: `complete` writes candidates to stdout
    // and exits when the trigger variable is set, and clap documents that
    // stdout must not be written to beforehand. Placing it here also keeps a
    // completion request out of the `--output json` protocol path below.
    //
    // The variable is `_PANDO_COMPLETE` rather than clap_complete's default
    // `COMPLETE`: that name is generic enough to be set in an environment for
    // unrelated reasons, and the installed zsh dispatcher captures `switch`
    // stdout and `cd`s to it, so an accidental completion script on stdout
    // would silently break directory switching.
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .var("_PANDO_COMPLETE")
        .complete();

    let args: Vec<_> = env::args_os().collect();
    let json_requested = args
        .windows(2)
        .any(|pair| (pair[0] == "--output" || pair[0] == "--input-output") && pair[1] == "json")
        || args
            .iter()
            .any(|arg| arg == "--output=json" || arg == "--input-output=json");
    let json_command = command_id(&args);
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
        if json_requested {
            let response = pando::protocol::failure(
                json_command.as_deref().unwrap_or("cli"),
                None,
                "command.execution_failed",
                format!("{error:#}"),
            );
            let _ = pando::protocol::write(&response);
            std::process::exit(1);
        }
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

fn command_id(args: &[std::ffi::OsString]) -> Option<String> {
    let words: Vec<_> = args.iter().filter_map(|arg| arg.to_str()).collect();
    for command in [
        "list", "switch", "create", "get", "remove", "merge", "commit", "install", "pr",
    ] {
        if words.contains(&command) {
            return Some(command.into());
        }
    }
    words
        .iter()
        .position(|word| *word == "trust")
        .and_then(|index| words.get(index + 1))
        .map(|leaf| format!("trust.{}", leaf.replace('-', "_")))
}

#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> Result<()> {
    ui::install_theme();
    let json = cli.output == OutputFormat::Json || cli.input_output == Some(OutputFormat::Json);
    let request_mode = cli.input_output == Some(OutputFormat::Json);
    if cli.input_output == Some(OutputFormat::Human) {
        anyhow::bail!("--input-output only supports json");
    }
    match cli.command {
        Commands::List { branches: false } if json => machine::list(request_mode),
        Commands::List { branches: true } if json => machine::list_branches(request_mode),
        Commands::List { branches: false } => list(),
        Commands::List { branches: true } => list_branches(),
        Commands::Switch {
            branch,
            branches: _,
            fetch,
            dry_run,
        } if json => machine::switch(request_mode, branch, fetch, dry_run),
        Commands::Switch {
            branch,
            branches,
            fetch,
            dry_run: false,
            // The creation rail closes itself with its own "Created worktree"
            // outro, so no trailing summary follows the destination here.
        } => smart::switch(branch, branches, fetch),
        Commands::Switch {
            branch,
            branches: _,
            fetch,
            dry_run: true,
        } => smart::switch_dry_run(branch, fetch),
        Commands::Create {
            branch,
            fetch,
            dry_run,
        } if json => machine::create(request_mode, branch, fetch, dry_run),
        Commands::Create {
            branch,
            fetch,
            dry_run: false,
        } => smart::create(&branch.context("create requires a branch")?, fetch),
        Commands::Create {
            branch,
            fetch,
            dry_run: true,
        } => smart::create_dry_run(&branch.context("create requires a branch")?, fetch),
        Commands::Get { property } if json => machine::get(
            request_mode,
            property.map(|p| match p {
                GetProperty::Branch => "branch",
                GetProperty::Port => "port",
                GetProperty::WorktreePath => "worktree-path",
                GetProperty::PrimaryWorktreePath => "primary-worktree-path",
                GetProperty::WorktreeRoot => "worktree-root",
            }),
        ),
        Commands::Get { property } => {
            // `get` is a query: the requested value on stdout is the whole
            // output, so no outro closes a sequence it never opened.
            smart::get(property.context("get requires a property")?)
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
        Commands::Remove {
            force,
            dry_run,
            branches,
        } if json => machine::remove(request_mode, branches, force, dry_run),
        Commands::Remove {
            force,
            dry_run: false,
            branches,
        } => pando::lifecycle::remove(&branches, force),
        Commands::Remove {
            force,
            dry_run: true,
            branches,
        } => pando::lifecycle::remove_dry_run(&branches, force),
        Commands::Merge {
            yolo: true,
            dry_run: false,
            ..
        } if json => anyhow::bail!("--yolo only supports human output"),
        Commands::Merge {
            no_rebase,
            no_remove,
            no_squash,
            yolo: false,
            dry_run,
        } if json => machine::merge(request_mode, no_rebase, no_remove, no_squash, dry_run),
        Commands::Merge {
            no_rebase,
            no_remove,
            no_squash,
            yolo: true,
            dry_run: false,
        } => {
            commit::run(commit::Invocation {
                message: None,
                stage_all: true,
                dry_run: false,
                json: false,
                request_mode: false,
            })?;
            pando::lifecycle::merge(no_rebase, no_remove, no_squash)
        }
        Commands::Merge {
            no_rebase,
            no_remove,
            no_squash,
            yolo: false,
            dry_run: false,
        } => pando::lifecycle::merge(no_rebase, no_remove, no_squash),
        Commands::Merge {
            no_rebase,
            no_remove,
            no_squash,
            yolo: false,
            dry_run: true,
        } => pando::lifecycle::merge_dry_run(no_rebase, no_remove, no_squash),
        Commands::Merge {
            yolo: true,
            dry_run: true,
            ..
        } => anyhow::bail!("--yolo cannot be used with --dry-run"),
        Commands::Trust { command, dry_run } if json => {
            let id = match command {
                TrustCommand::Status => "trust.status",
                TrustCommand::Reset => "trust.reset",
                TrustCommand::CommitStatus => "trust.commit_status",
                TrustCommand::CommitReset => "trust.commit_reset",
                TrustCommand::CommitApprove => "trust.commit_approve",
                TrustCommand::PrStatus => "trust.pr_status",
                TrustCommand::PrReset => "trust.pr_reset",
                TrustCommand::PrApprove => "trust.pr_approve",
                TrustCommand::MergeStatus => "trust.merge_status",
                TrustCommand::MergeReset => "trust.merge_reset",
                TrustCommand::MergeApprove => "trust.merge_approve",
            };
            machine::trust(id, request_mode, dry_run)
        }
        Commands::Trust {
            command,
            dry_run: true,
        } => smart::trust_dry_run(command),
        Commands::Trust {
            command,
            dry_run: false,
        } => {
            smart::trust_command(command)?;
            let summary = match command {
                TrustCommand::Status => "Hook trust status checked.",
                TrustCommand::Reset => "Hook trust reset checked.",
                TrustCommand::CommitStatus => "Commit generator trust status checked.",
                TrustCommand::CommitReset => "Commit generator trust reset checked.",
                TrustCommand::CommitApprove => "Commit generator trust approval checked.",
                TrustCommand::PrStatus => "PR generator trust status checked.",
                TrustCommand::PrReset => "PR generator trust reset checked.",
                TrustCommand::PrApprove => "PR generator trust approval checked.",
                TrustCommand::MergeStatus => "Squash message generator trust status checked.",
                TrustCommand::MergeReset => "Squash message generator trust reset checked.",
                TrustCommand::MergeApprove => "Squash message generator trust approval checked.",
            };
            ui::finish(ui::muted_style().apply_to(summary))
        }
        Commands::Install { dry_run, no_guide } if json => {
            machine::install(request_mode, dry_run, no_guide)
        }
        Commands::Install {
            dry_run: false,
            no_guide,
        } => install::run(!no_guide),
        Commands::Install { dry_run: true, .. } => install::preview(),
        Commands::Pr {
            command:
                PrCommand::Create {
                    title,
                    description,
                    description_file,
                    status,
                    dry_run,
                    remote,
                    force,
                    yolo,
                },
        } => pr::run(pr::Invocation {
            title,
            description,
            description_file,
            status,
            dry_run,
            force,
            yolo,
            remote,
            json,
            request_mode,
        }),
    }
}

fn list() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository_with_metadata(&cwd)?;
    let default_sort = EffectiveConfig::load_default_sort(&repository)?;
    if let Some(warning) = &repository.metadata_warning {
        ui::warning(warning)?;
    }
    let rows: Vec<Row> = repository
        .worktrees
        .iter()
        .map(Row::from_worktree)
        .collect();
    ui::info(format!(
        "{}\n{}",
        ui::heading_style().apply_to(format!("Worktrees ({})", default_sort.label())),
        render::table(&rows, default_sort)
    ))?;
    ui::finish(list_summary(&repository.worktrees))
}

fn list_branches() -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let branches = git::repository_with_branches(&cwd)?;
    let repository = &branches.repository;
    let default_sort = EffectiveConfig::load_default_sort(repository)?;
    if let Some(warning) = &repository.metadata_warning {
        ui::warning(warning)?;
    }
    let rows: Vec<Row> = branches
        .branches
        .iter()
        .map(|branch| Row::from_branch(branch, &repository.worktrees))
        .collect();
    ui::info(format!(
        "{}\n{}",
        ui::heading_style().apply_to(format!("Branches ({})", default_sort.label())),
        render::table(&rows, default_sort)
    ))?;
    ui::finish(branch_summary(&rows))
}

fn branch_summary(rows: &[Row]) -> String {
    let noun = if rows.len() == 1 {
        "branch"
    } else {
        "branches"
    };
    let mut summary = vec![format!("{} {noun}", rows.len())];
    let checked_out = rows.iter().filter(|row| row.path.is_some()).count();
    if checked_out > 0 {
        summary.push(format!("{checked_out} checked out"));
    }
    let dirty = rows.iter().filter(|row| row.is_dirty()).count();
    if dirty > 0 {
        summary.push(format!("{dirty} dirty"));
    }
    ui::muted_style().apply_to(summary.join(", ")).to_string()
}

fn list_summary(worktrees: &[pando::Worktree]) -> String {
    use pando::Condition;

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
