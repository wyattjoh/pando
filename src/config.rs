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

#[derive(Debug, Default, Deserialize)]
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedConfig {
    #[serde(default)]
    hooks: Option<HooksConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConfig {
    #[serde(default)]
    worktrees: Option<RootConfig>,
    #[serde(default)]
    hooks: Option<HooksConfig>,
}

#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub root: Option<PathBuf>,
    pub steps: Vec<HookStep>,
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
        let shared_steps = shared.hooks.unwrap_or_default().post_create;
        validate_steps(&shared_steps, &shared_path)?;

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
        Ok(Self { root, steps })
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
