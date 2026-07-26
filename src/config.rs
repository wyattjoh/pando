use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::git::{self, Repository};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookStep {
    pub command: String,
    #[serde(default)]
    pub name: Option<String>,
}

impl HookStep {
    #[must_use]
    pub fn label(&self, index: usize) -> String {
        self.name.clone().unwrap_or_else(|| (index + 1).to_string())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootConfig {
    root: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksConfig {
    #[serde(default, rename = "post-create")]
    post_create: Vec<HookStep>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalConfig {
    #[serde(default)]
    worktrees: Option<RootConfig>,
    #[serde(default)]
    commit: Option<CommitConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitConfig {
    #[serde(default)]
    generation: Option<GenerationConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationConfig {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    template: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedConfig {
    #[serde(default)]
    hooks: Option<HooksConfig>,
    #[serde(default)]
    commit: Option<CommitConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConfig {
    #[serde(default)]
    worktrees: Option<RootConfig>,
    #[serde(default)]
    hooks: Option<HooksConfig>,
    #[serde(default)]
    commit: Option<CommitConfig>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationSource {
    Global,
    Shared,
    Local,
}

#[derive(Clone, Debug)]
pub struct GenerationValue {
    pub value: String,
    pub source: GenerationSource,
}

#[derive(Clone, Debug, Default)]
pub struct EffectiveGeneration {
    pub command: Option<GenerationValue>,
    pub template: Option<GenerationValue>,
}

#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub root: Option<PathBuf>,
    pub steps: Vec<HookStep>,
    pub generation: EffectiveGeneration,
}

impl EffectiveConfig {
    /// Loads and validates the global, shared, and local configuration layers.
    ///
    /// # Errors
    ///
    /// Returns an error when a file is invalid, inaccessible, or violates local-file safety.
    pub fn load(repository: &Repository) -> Result<Self> {
        let global_path = config_home()?.join("worktrees/config.yaml");
        let global: GlobalConfig = read_yaml_optional(&global_path)?;

        let shared_path = repository.current().path.join(".worktrees.yaml");
        let shared: SharedConfig = read_yaml_optional(&shared_path)?;
        let shared_steps = shared.hooks.clone().unwrap_or_default().post_create;
        validate_steps(&shared_steps, &shared_path)?;
        let shared_generation = shared.commit.and_then(|commit| commit.generation);
        validate_generation(shared_generation.as_ref(), &shared_path)?;

        let local_path = repository
            .primary
            .as_ref()
            .map(|primary| primary.join(".worktrees.local.yaml"));
        let local = if let (Some(primary), Some(path)) = (&repository.primary, &local_path) {
            if path.exists() {
                if !git::is_ignored(primary, path)? {
                    bail!(
                        "{} must be Git-ignored before it can be loaded; add '/.worktrees.local.yaml' to {}",
                        path.display(),
                        primary.join(".gitignore").display()
                    );
                }
                read_yaml_optional::<LocalConfig>(path)?
            } else {
                LocalConfig::default()
            }
        } else {
            LocalConfig::default()
        };

        let global_generation = global.commit.and_then(|commit| commit.generation);
        validate_generation(global_generation.as_ref(), &global_path)?;
        let local_generation = local
            .commit
            .as_ref()
            .and_then(|commit| commit.generation.clone());
        if let Some(path) = &local_path {
            validate_generation(local_generation.as_ref(), path)?;
        }

        let generation = EffectiveGeneration {
            command: resolve_generation_value(
                local_generation
                    .as_ref()
                    .and_then(|value| value.command.clone()),
                shared_generation
                    .as_ref()
                    .and_then(|value| value.command.clone()),
                global_generation
                    .as_ref()
                    .and_then(|value| value.command.clone()),
            ),
            template: resolve_generation_value(
                local_generation
                    .as_ref()
                    .and_then(|value| value.template.clone()),
                shared_generation
                    .as_ref()
                    .and_then(|value| value.template.clone()),
                global_generation
                    .as_ref()
                    .and_then(|value| value.template.clone()),
            ),
        };

        let configured_root = local
            .worktrees
            .map(|section| section.root)
            .or_else(|| global.worktrees.map(|section| section.root));
        let root = configured_root
            .map(|root| resolve_root(repository, &root))
            .transpose()?;

        let local_steps = local.hooks.unwrap_or_default().post_create;
        if let Some(path) = &local_path {
            validate_steps(&local_steps, path)?;
        }
        let mut steps = shared_steps;
        steps.extend(local_steps);
        Ok(Self {
            root,
            steps,
            generation,
        })
    }

    /// Returns the configured, resolved worktree root.
    ///
    /// # Errors
    ///
    /// Returns an error when no root is configured.
    pub fn require_root(&self) -> Result<&Path> {
        self.root.as_deref().context(
            "no worktree root is configured; create ${XDG_CONFIG_HOME:-$HOME/.config}/worktrees/config.yaml with:\nworktrees:\n  root: ../worktrees",
        )
    }
}

fn validate_generation(generation: Option<&GenerationConfig>, source_hint: &Path) -> Result<()> {
    let Some(generation) = generation else {
        return Ok(());
    };
    for (name, value) in [
        ("command", generation.command.as_deref()),
        ("template", generation.template.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            bail!(
                "commit.generation.{name} cannot be empty while loading configuration near {}",
                source_hint.display()
            );
        }
    }
    Ok(())
}

fn resolve_generation_value(
    local: Option<String>,
    shared: Option<String>,
    global: Option<String>,
) -> Option<GenerationValue> {
    local
        .map(|value| GenerationValue {
            value,
            source: GenerationSource::Local,
        })
        .or_else(|| {
            shared.map(|value| GenerationValue {
                value,
                source: GenerationSource::Shared,
            })
        })
        .or_else(|| {
            global.map(|value| GenerationValue {
                value,
                source: GenerationSource::Global,
            })
        })
}

fn validate_steps(steps: &[HookStep], source_hint: &Path) -> Result<()> {
    for (index, step) in steps.iter().enumerate() {
        if step.command.trim().is_empty() {
            bail!(
                "post-create step {} has an empty command while loading configuration near {}",
                index + 1,
                source_hint.display()
            );
        }
        if step
            .name
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            bail!(
                "post-create step {} has an empty name while loading configuration near {}",
                index + 1,
                source_hint.display()
            );
        }
    }
    Ok(())
}

fn resolve_root(repository: &Repository, configured: &Path) -> Result<PathBuf> {
    let absolute = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        repository
            .primary
            .as_ref()
            .context("relative worktree roots require a primary worktree")?
            .join(configured)
    };
    git::canonical_or_normalized(&absolute)
        .context("failed to resolve the configured worktree root")
}

fn read_yaml_optional<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Default,
{
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse configuration file {}", path.display()))
}

/// Returns the XDG-aware Worktrees configuration directory.
///
/// # Errors
///
/// Returns an error when neither `XDG_CONFIG_HOME` nor `HOME` can identify it.
pub fn config_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("HOME is not set; cannot locate Worktrees configuration")?;
    Ok(PathBuf::from(home).join(".config"))
}
