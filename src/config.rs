use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::git::{self, Repository};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HookPhase {
    PostCreate,
    PreMerge,
    PreRemove,
}
impl HookPhase {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::PostCreate => "post-create",
            Self::PreMerge => "pre-merge",
            Self::PreRemove => "pre-remove",
        }
    }
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::PostCreate, Self::PreMerge, Self::PreRemove]
    }
    #[must_use]
    pub const fn plural_name(self) -> &'static str {
        match self {
            Self::PostCreate => "post-create commands",
            Self::PreMerge => "pre-merge commands",
            Self::PreRemove => "pre-remove commands",
        }
    }
}

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
struct WorktreesConfig {
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default, rename = "target-branch")]
    target_branch: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetConfig {
    #[serde(default, rename = "target-branch")]
    target_branch: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksConfig {
    #[serde(default, rename = "post-create")]
    post_create: Vec<HookStep>,
    #[serde(default, rename = "pre-merge")]
    pre_merge: Vec<HookStep>,
    #[serde(default, rename = "pre-remove")]
    pre_remove: Vec<HookStep>,
}
impl HooksConfig {
    fn steps(&self, phase: HookPhase) -> &[HookStep] {
        match phase {
            HookPhase::PostCreate => &self.post_create,
            HookPhase::PreMerge => &self.pre_merge,
            HookPhase::PreRemove => &self.pre_remove,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalConfig {
    #[serde(default)]
    worktrees: Option<WorktreesConfig>,
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
    worktrees: Option<TargetConfig>,
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
    pub target_branch: Option<String>,
    pub post_create: Vec<HookStep>,
    pub pre_merge: Vec<HookStep>,
    pub pre_remove: Vec<HookStep>,
    pub generation: EffectiveGeneration,
}

impl EffectiveConfig {
    /// Loads and validates the global, shared, and local configuration layers.
    ///
    /// # Errors
    ///
    /// Returns an error when a file is invalid, inaccessible, or violates local-file safety.
    pub fn load(repository: &Repository) -> Result<Self> {
        Self::load_for_worktree(repository, &repository.current().path)
    }

    /// Loads configuration with shared lifecycle hooks from `worktree`.
    ///
    /// # Errors
    ///
    /// Returns an error when configured files are invalid or inaccessible.
    pub fn load_for_worktree(repository: &Repository, worktree: &Path) -> Result<Self> {
        let global_path = config_home()?.join("worktrees/config.yaml");
        let global: GlobalConfig = read_yaml_optional(&global_path)?;

        let shared_path = worktree.join(".worktrees.yaml");
        let shared: SharedConfig = read_yaml_optional(&shared_path)?;
        let shared_hooks = shared.hooks.clone().unwrap_or_default();
        validate_hooks(&shared_hooks, &shared_path)?;
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

        let configured_root = local.worktrees.map(|section| section.root).or_else(|| {
            global
                .worktrees
                .as_ref()
                .and_then(|section| section.root.clone())
        });
        let root = configured_root
            .map(|root| resolve_root(repository, &root))
            .transpose()?;

        let local_hooks = local.hooks.unwrap_or_default();
        if let Some(path) = &local_path {
            validate_hooks(&local_hooks, path)?;
        }
        let target_branch = shared
            .worktrees
            .and_then(|section| section.target_branch)
            .or_else(|| global.worktrees.and_then(|section| section.target_branch));
        Ok(Self {
            root,
            target_branch,
            post_create: combine(&shared_hooks, &local_hooks, HookPhase::PostCreate),
            pre_merge: combine(&shared_hooks, &local_hooks, HookPhase::PreMerge),
            pre_remove: combine(&shared_hooks, &local_hooks, HookPhase::PreRemove),
            generation,
        })
    }

    #[must_use]
    pub fn hooks(&self, phase: HookPhase) -> &[HookStep] {
        match phase {
            HookPhase::PostCreate => &self.post_create,
            HookPhase::PreMerge => &self.pre_merge,
            HookPhase::PreRemove => &self.pre_remove,
        }
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

    /// Returns the configured target branch.
    ///
    /// # Errors
    ///
    /// Returns an error when no target branch is configured.
    pub fn require_target_branch(&self) -> Result<&str> {
        self.target_branch.as_deref().context("no target branch is configured; add worktrees.target-branch to .worktrees.yaml or global config")
    }
}

fn combine(shared: &HooksConfig, local: &HooksConfig, phase: HookPhase) -> Vec<HookStep> {
    let mut steps = shared.steps(phase).to_vec();
    steps.extend_from_slice(local.steps(phase));
    steps
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

fn validate_hooks(hooks: &HooksConfig, source_hint: &Path) -> Result<()> {
    for phase in HookPhase::all() {
        for (index, step) in hooks.steps(phase).iter().enumerate() {
            if step.command.trim().is_empty() {
                bail!(
                    "{} step {} has an empty command while loading configuration near {}",
                    phase.key(),
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
                    "{} step {} has an empty name while loading configuration near {}",
                    phase.key(),
                    index + 1,
                    source_hint.display()
                );
            }
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
