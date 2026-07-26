use std::{
    env,
    io::{self, IsTerminal, Read, Write},
    process::{Command, Stdio},
    thread,
};

use anyhow::{Context, Result, bail};
use cliclack::{confirm, spinner};
use console::style;
use minijinja::{Environment, context};

use crate::{
    WorktreeKind,
    config::{EffectiveConfig, GenerationSource},
    git::{self, Repository},
    trust, ui,
};

const BUILTIN_TEMPLATE: &str = r"Write a factual conventional commit message for this staged change.
Return only the commit message.
The subject must use imperative mood and be fewer than 50 characters.
Follow it with a blank line and at least two concrete bullet items.
Do not claim changes not evidenced by the staged diff.

Repository: {{ repo }}
Branch: {{ branch }}
Recent commits:
{% for subject in recent_commits %}- {{ subject }}
{% endfor %}
Staged diffstat:
{{ git_diff_stat }}

Staged diff:
{{ git_diff }}
";

/// Stages all changes and creates a commit, generating a message when absent.
///
/// # Errors
///
/// Returns an error when repository discovery, configuration, generation, staging, or Git commit fails.
pub fn run(message: Option<String>) -> Result<()> {
    let cwd = env::current_dir().context("failed to read the current directory")?;
    let repository = git::repository(&cwd)?;
    ensure_worktree(&repository)?;
    if let Some(message) = message {
        stage_changes(&repository)?;
        return commit_with_feedback(&repository, &message, "Created commit");
    }

    let config = EffectiveConfig::load(&repository)?;
    let command = config.generation.command.as_ref().context(
        "no commit generator is configured; create ${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml with:\ncommit:\n  generation:\n    command: your-generator-command",
    )?;
    let template = config
        .generation
        .template
        .as_ref()
        .map_or(BUILTIN_TEMPLATE, |value| value.value.as_str());
    validate_template(template)?;
    approve_shared_generation(&repository, &config)?;

    stage_changes(&repository)?;
    let prompt = render_prompt(&repository, template)?;
    let spinner = io::stderr().is_terminal().then(|| {
        let spinner = spinner().with_template("{msg} [{elapsed_precise}]");
        spinner.start("Generating commit message…");
        spinner
    });
    if spinner.is_none() {
        ui::info("Generating commit message…")?;
    }
    let generated = match run_generator(&repository, &command.value, &prompt) {
        Ok(generated) => {
            if let Some(spinner) = &spinner {
                spinner.clear();
            }
            generated
        }
        Err(error) => {
            if let Some(spinner) = &spinner {
                spinner.error("Failed to generate commit message");
            }
            return Err(error);
        }
    };
    commit_with_feedback(&repository, &generated, "Generated commit message:")
}

fn commit_with_feedback(repository: &Repository, message: &str, status: &str) -> Result<()> {
    git::commit(&repository.current().path, message)?;
    let status = style(status).green().bold();
    let message = render_commit_message(message);
    ui::step(format!("{status}\n{message}"))?;
    let hash = git::head_commit(&repository.current().path)?;
    let hash = hash.get(..7).unwrap_or(&hash);
    ui::finish(format!(
        "{} {}",
        style("Committed changes @").green().bold(),
        style(hash).color256(244)
    ))
}

fn render_commit_message(message: &str) -> String {
    match message.split_once('\n') {
        Some((subject, body)) => format!("{}\n{body}", style(subject).cyan().bold()),
        None => style(message).cyan().bold().to_string(),
    }
}

fn ensure_worktree(repository: &Repository) -> Result<()> {
    if repository.current().is_bare() {
        bail!("the current repository is bare; commit requires a worktree");
    }
    Ok(())
}

fn stage_changes(repository: &Repository) -> Result<()> {
    git::stage_all(&repository.current().path)?;
    ensure_changes(repository)?;
    let diffstat = git::staged_diff_stat(&repository.current().path)?;
    ui::info(format!(
        "{}\n{}",
        style("Staged changes:").green().bold(),
        colorize_diffstat(&diffstat)
    ))
}

fn colorize_diffstat(diffstat: &str) -> String {
    trim_git_margin(diffstat)
        .lines()
        .map(|line| match line.split_once(" | ") {
            Some((path, stat)) => format!("{} | {stat}", style(path).cyan()),
            None => style(line).dim().to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn trim_git_margin(output: &str) -> String {
    output
        .lines()
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_changes(repository: &Repository) -> Result<()> {
    if git::has_staged_changes(&repository.current().path)? {
        Ok(())
    } else {
        bail!("nothing to commit after staging all changes")
    }
}

fn validate_template(template: &str) -> Result<()> {
    let mut environment = Environment::new();
    environment
        .add_template("commit", template)
        .context("failed to parse commit generation template")?;
    Ok(())
}

fn render_prompt(repository: &Repository, template: &str) -> Result<String> {
    let mut environment = Environment::new();
    environment
        .add_template("commit", template)
        .context("failed to parse commit generation template")?;
    let branch = match &repository.current().kind {
        WorktreeKind::Branch(branch) => branch.as_str(),
        WorktreeKind::Detached => "(detached)",
        WorktreeKind::Bare | WorktreeKind::Unknown => "(unknown)",
    };
    let repo = repository.current().path.file_name().map_or_else(
        || "(unknown)".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    environment
        .get_template("commit")?
        .render(context! {
            git_diff => git::staged_diff(&repository.current().path)?,
            git_diff_stat => git::staged_diff_stat(&repository.current().path)?,
            branch,
            repo,
            recent_commits => git::recent_subjects(&repository.current().path)?,
        })
        .context("failed to render commit generation template")
}

fn run_generator(repository: &Repository, command: &str, prompt: &str) -> Result<String> {
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(&repository.current().path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start commit generator")?;
    let mut stdout = child
        .stdout
        .take()
        .context("commit generator stdout was unavailable; staged changes remain in the index")?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let mut stdin = child
        .stdin
        .take()
        .context("commit generator stdin was unavailable; staged changes remain in the index")?;
    let write_result = stdin.write_all(prompt.as_bytes());
    drop(stdin);
    write_result
        .context("failed to send prompt to commit generator; staged changes remain in the index")?;
    let status = child
        .wait()
        .context("failed to wait for commit generator; staged changes remain in the index")?;
    let stdout = reader
        .join()
        .map_err(|_| {
            anyhow::anyhow!(
                "commit generator output reader panicked; staged changes remain in the index"
            )
        })?
        .context("failed to read commit generator output; staged changes remain in the index")?;
    if !status.success() {
        bail!("commit generator failed with status {status}; staged changes remain in the index");
    }
    let message = String::from_utf8(stdout).context(
        "commit generator produced non-UTF-8 output; staged changes remain in the index",
    )?;
    let message = message.trim().to_owned();
    if message.is_empty() {
        bail!("commit generator produced an empty message; staged changes remain in the index");
    }
    Ok(message)
}

fn approve_shared_generation(repository: &Repository, config: &EffectiveConfig) -> Result<()> {
    let shared = [
        config.generation.command.as_ref(),
        config.generation.template.as_ref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.source == GenerationSource::Shared);
    if !shared || trust::is_generation_trusted(repository, &config.generation)? {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!(
            "shared commit generation settings require approval, but no interactive terminal is available"
        );
    }
    ui::info("The repository requests these commit generation settings:")?;
    if let Some(value) = &config.generation.command
        && value.source == GenerationSource::Shared
    {
        ui::step(format!("command: {}", value.value))?;
    }
    if let Some(value) = &config.generation.template
        && value.source == GenerationSource::Shared
    {
        ui::step(format!("template:\n{}", value.value))?;
    }
    let approved = confirm("Trust these settings for this repository?")
        .initial_value(false)
        .interact()
        .context("failed to read commit generator approval")?;
    if !approved {
        bail!("commit generator approval declined; no changes were staged");
    }
    trust::approve_generation(repository, &config.generation)
}

#[cfg(test)]
mod tests {
    use console::strip_ansi_codes;

    use super::{render_commit_message, trim_git_margin};

    #[test]
    fn trims_the_git_output_margin_from_each_line() {
        assert_eq!(
            trim_git_margin("first\n second\n  third"),
            "first\nsecond\n third"
        );
    }

    #[test]
    fn renders_a_bold_subject_without_styling_the_body() {
        assert_eq!(
            strip_ansi_codes(&render_commit_message("feat: subject\n\nbody")),
            "feat: subject\n\nbody"
        );
    }
}
