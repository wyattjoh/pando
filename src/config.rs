use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    BaseMode, SortMode,
    git::{Repository, RepositoryObservation},
};

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
struct LocalWorktreesConfig {
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default, rename = "default-sort")]
    default_sort: Option<SortMode>,
    #[serde(default)]
    base: Option<BaseMode>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorktreesConfig {
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default, rename = "target-branch")]
    target_branch: Option<String>,
    #[serde(default, rename = "default-sort")]
    default_sort: Option<SortMode>,
    #[serde(default)]
    base: Option<BaseMode>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetConfig {
    #[serde(default, rename = "target-branch")]
    target_branch: Option<String>,
    #[serde(default)]
    base: Option<BaseMode>,
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
    install: Option<InstallConfig>,
    #[serde(default)]
    commit: Option<CommitConfig>,
    #[serde(default)]
    merge: Option<MergeConfig>,
    #[serde(default)]
    pr: Option<PrConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallConfig {
    #[serde(default)]
    command: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitConfig {
    #[serde(default)]
    generation: Option<GenerationConfig>,
}

/// Squash policy and the generator that writes the squashed commit's message.
///
/// `squash` is legal in every layer for the same reason `worktrees.base` is: a
/// project may commit its integration convention while a clone overrides it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeConfig {
    #[serde(default)]
    squash: Option<bool>,
    #[serde(default)]
    generation: Option<GenerationConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrConfig {
    #[serde(default)]
    provider: Option<PrProvider>,
    #[serde(default)]
    generation: Option<GenerationConfig>,
    #[serde(default, rename = "pull-request-template")]
    pull_request_template: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum PrProvider {
    #[default]
    Auto,
    Github,
    Tea,
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
    #[serde(default)]
    merge: Option<MergeConfig>,
    #[serde(default)]
    pr: Option<PrConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalConfig {
    #[serde(default)]
    worktrees: Option<LocalWorktreesConfig>,
    #[serde(default)]
    hooks: Option<HooksConfig>,
    #[serde(default)]
    commit: Option<CommitConfig>,
    #[serde(default)]
    merge: Option<MergeConfig>,
    #[serde(default)]
    pr: Option<PrConfig>,
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
    pub default_sort: SortMode,
    pub base: BaseMode,
    pub post_create: Vec<HookStep>,
    pub pre_merge: Vec<HookStep>,
    pub pre_remove: Vec<HookStep>,
    pub generation: EffectiveGeneration,
    /// Whether `pando merge` collapses the topic into one commit. Defaults to true.
    pub squash: bool,
    /// Generator for the squashed commit's message. Its `command` falls back to
    /// [`Self::generation`]'s so a single configured generator covers both.
    pub merge_generation: EffectiveGeneration,
    pub pr_provider: PrProvider,
    pub pr_generation: EffectiveGeneration,
    pub pull_request_template: Option<GenerationValue>,
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

    /// Loads the personal human-interface sort preference without resolving placement.
    ///
    /// # Errors
    ///
    /// Returns an error when configured files are invalid or inaccessible.
    pub fn load_default_sort(repository: &Repository) -> Result<SortMode> {
        Ok(
            Self::load_for_worktree_inner(repository, &repository.current().path, false)?
                .default_sort,
        )
    }

    /// Loads configuration with shared lifecycle hooks from `worktree`.
    ///
    /// # Errors
    ///
    /// Returns an error when configured files are invalid or inaccessible.
    pub fn load_for_worktree(repository: &Repository, worktree: &Path) -> Result<Self> {
        Self::load_for_worktree_inner(repository, worktree, true)
    }

    #[allow(clippy::too_many_lines)]
    fn load_for_worktree_inner(
        repository: &Repository,
        worktree: &Path,
        resolve_placement: bool,
    ) -> Result<Self> {
        let global_path = config_home()?.join("pando/config.yaml");
        let global: GlobalConfig = read_yaml_optional(&global_path)?;
        validate_install_command(global.install.as_ref(), &global_path)?;

        let shared_path = worktree.join(".pando.yaml");
        let shared: SharedConfig = read_yaml_optional(&shared_path)?;
        let shared_hooks = shared.hooks.clone().unwrap_or_default();
        validate_hooks(&shared_hooks, &shared_path)?;
        let shared_generation = shared
            .commit
            .as_ref()
            .and_then(|commit| commit.generation.clone());
        let shared_merge = shared.merge.clone().unwrap_or_default();
        let shared_pr_provider = shared.pr.as_ref().and_then(|pr| pr.provider);
        let shared_pr_generation = shared.pr.as_ref().and_then(|pr| pr.generation.clone());
        let shared_pr_template = shared
            .pr
            .as_ref()
            .and_then(|pr| pr.pull_request_template.clone());
        validate_generation("commit", shared_generation.as_ref(), &shared_path)?;

        let local_path = repository
            .primary
            .as_ref()
            .map(|primary| primary.join(".pando.local.yaml"));
        let local = if let (Some(primary), Some(path)) = (&repository.primary, &local_path) {
            if path.exists() {
                if !RepositoryObservation::new(primary).is_ignored(path)? {
                    bail!(
                        "{} must be Git-ignored before it can be loaded; add '/.pando.local.yaml' to {}",
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

        let global_generation = global
            .commit
            .as_ref()
            .and_then(|commit| commit.generation.clone());
        let global_merge = global.merge.clone().unwrap_or_default();
        let global_pr_provider = global.pr.as_ref().and_then(|pr| pr.provider);
        let global_pr_generation = global.pr.as_ref().and_then(|pr| pr.generation.clone());
        let global_pr_template = global
            .pr
            .as_ref()
            .and_then(|pr| pr.pull_request_template.clone());
        validate_generation("commit", global_generation.as_ref(), &global_path)?;
        let local_generation = local
            .commit
            .as_ref()
            .and_then(|commit| commit.generation.clone());
        let local_merge = local.merge.clone().unwrap_or_default();
        let local_pr_provider = local.pr.as_ref().and_then(|pr| pr.provider);
        let local_pr_generation = local.pr.as_ref().and_then(|pr| pr.generation.clone());
        let local_pr_template = local
            .pr
            .as_ref()
            .and_then(|pr| pr.pull_request_template.clone());
        if let Some(path) = &local_path {
            validate_generation("commit", local_generation.as_ref(), path)?;
            validate_pull_request_template(local_pr_template.as_deref(), path)?;
        }

        validate_generation("pr", shared_pr_generation.as_ref(), &shared_path)?;
        validate_generation("pr", global_pr_generation.as_ref(), &global_path)?;
        validate_generation("merge", shared_merge.generation.as_ref(), &shared_path)?;
        validate_generation("merge", global_merge.generation.as_ref(), &global_path)?;
        if let Some(path) = &local_path {
            validate_generation("merge", local_merge.generation.as_ref(), path)?;
        }
        validate_pull_request_template(shared_pr_template.as_deref(), &shared_path)?;
        validate_pull_request_template(global_pr_template.as_deref(), &global_path)?;
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

        let (root, default_sort) = resolve_worktree_settings(
            global.worktrees.as_ref(),
            local.worktrees.as_ref(),
            repository,
            resolve_placement,
        )?;
        let local_hooks = local.hooks.unwrap_or_default();
        if let Some(path) = &local_path {
            validate_hooks(&local_hooks, path)?;
        }
        let target_branch = shared
            .worktrees
            .as_ref()
            .and_then(|section| section.target_branch.clone())
            .or_else(|| {
                global
                    .worktrees
                    .as_ref()
                    .and_then(|section| section.target_branch.clone())
            });
        // Unlike placement, the base is legal in every layer: local wins, then
        // the committed project convention, then the personal global default.
        let base = local
            .worktrees
            .as_ref()
            .and_then(|section| section.base)
            .or_else(|| shared.worktrees.as_ref().and_then(|section| section.base))
            .or_else(|| global.worktrees.as_ref().and_then(|section| section.base))
            .unwrap_or_default();
        Ok(Self {
            root,
            target_branch,
            default_sort,
            base,
            post_create: combine(&shared_hooks, &local_hooks, HookPhase::PostCreate),
            pre_merge: combine(&shared_hooks, &local_hooks, HookPhase::PreMerge),
            pre_remove: combine(&shared_hooks, &local_hooks, HookPhase::PreRemove),
            squash: local_merge
                .squash
                .or(shared_merge.squash)
                .or(global_merge.squash)
                .unwrap_or(true),
            merge_generation: EffectiveGeneration {
                // The command falls back to the commit generator so one
                // configured process writes both kinds of message.
                command: resolve_generation_value(
                    local_merge
                        .generation
                        .as_ref()
                        .and_then(|v| v.command.clone()),
                    shared_merge
                        .generation
                        .as_ref()
                        .and_then(|v| v.command.clone()),
                    global_merge
                        .generation
                        .as_ref()
                        .and_then(|v| v.command.clone()),
                )
                .or_else(|| generation.command.clone()),
                template: resolve_generation_value(
                    local_merge
                        .generation
                        .as_ref()
                        .and_then(|v| v.template.clone()),
                    shared_merge
                        .generation
                        .as_ref()
                        .and_then(|v| v.template.clone()),
                    global_merge
                        .generation
                        .as_ref()
                        .and_then(|v| v.template.clone()),
                ),
            },
            generation,
            pr_provider: resolve_pr_provider(
                local_pr_provider,
                shared_pr_provider,
                global_pr_provider,
            ),
            pr_generation: EffectiveGeneration {
                command: resolve_generation_value(
                    local_pr_generation.as_ref().and_then(|v| v.command.clone()),
                    shared_pr_generation
                        .as_ref()
                        .and_then(|v| v.command.clone()),
                    global_pr_generation
                        .as_ref()
                        .and_then(|v| v.command.clone()),
                ),
                template: resolve_generation_value(
                    local_pr_generation
                        .as_ref()
                        .and_then(|v| v.template.clone()),
                    shared_pr_generation
                        .as_ref()
                        .and_then(|v| v.template.clone()),
                    global_pr_generation
                        .as_ref()
                        .and_then(|v| v.template.clone()),
                ),
            },
            pull_request_template: resolve_generation_value(
                local_pr_template,
                shared_pr_template,
                global_pr_template,
            ),
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
            "no worktree root is configured; create ${XDG_CONFIG_HOME:-$HOME/.config}/pando/config.yaml with:\nworktrees:\n  root: ../worktrees",
        )
    }

    /// Returns the configured target branch.
    ///
    /// # Errors
    ///
    /// Returns an error when no target branch is configured.
    pub fn require_target_branch(&self) -> Result<&str> {
        self.target_branch.as_deref().context("no target branch is configured; add worktrees.target-branch to .pando.yaml or global config")
    }
}

fn resolve_worktree_settings(
    global: Option<&WorktreesConfig>,
    local: Option<&LocalWorktreesConfig>,
    repository: &Repository,
    resolve_placement: bool,
) -> Result<(Option<PathBuf>, SortMode)> {
    let configured_root = local
        .and_then(|section| section.root.clone())
        .or_else(|| global.and_then(|section| section.root.clone()));
    let root = if resolve_placement {
        configured_root
            .map(|root| resolve_root(repository, &root))
            .transpose()?
    } else {
        None
    };
    let default_sort = local
        .and_then(|section| section.default_sort)
        .or_else(|| global.and_then(|section| section.default_sort))
        .unwrap_or_default();
    Ok((root, default_sort))
}

fn combine(shared: &HooksConfig, local: &HooksConfig, phase: HookPhase) -> Vec<HookStep> {
    let mut steps = shared.steps(phase).to_vec();
    steps.extend_from_slice(local.steps(phase));
    steps
}

fn validate_install_command(install: Option<&InstallConfig>, source_hint: &Path) -> Result<()> {
    if install
        .and_then(|value| value.command.as_deref())
        .is_some_and(|command| command.trim().is_empty())
    {
        bail!(
            "install.command cannot be empty while loading configuration near {}",
            source_hint.display()
        );
    }
    Ok(())
}

fn validate_generation(
    section: &str,
    generation: Option<&GenerationConfig>,
    source_hint: &Path,
) -> Result<()> {
    let Some(generation) = generation else {
        return Ok(());
    };
    for (name, value) in [
        ("command", generation.command.as_deref()),
        ("template", generation.template.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            bail!(
                "{section}.generation.{name} cannot be empty while loading configuration near {}",
                source_hint.display()
            );
        }
    }
    Ok(())
}

fn validate_pull_request_template(value: Option<&str>, source_hint: &Path) -> Result<()> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        bail!(
            "pr.pull-request-template cannot be empty while loading configuration near {}",
            source_hint.display()
        );
    }
    Ok(())
}

fn resolve_pr_provider(
    local: Option<PrProvider>,
    shared: Option<PrProvider>,
    global: Option<PrProvider>,
) -> PrProvider {
    local.or(shared).or(global).unwrap_or_default()
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
    RepositoryObservation::resolve_path(&absolute)
        .context("failed to resolve the configured worktree root")
}

/// Loads the command saved for LLM-guided installation.
///
/// # Errors
///
/// Returns an error when the global configuration is inaccessible, invalid, or
/// contains an empty installer command.
pub fn load_install_command(path: &Path) -> Result<Option<String>> {
    let global: GlobalConfig = read_yaml_optional(path)?;
    validate_install_command(global.install.as_ref(), path)?;
    Ok(global.install.and_then(|install| install.command))
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

/// Returns the XDG-aware Pando configuration directory.
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
        .context("HOME is not set; cannot locate Pando configuration")?;
    Ok(PathBuf::from(home).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_install_command_is_loaded_and_must_not_be_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.yaml");
        fs::write(&path, "install:\n  command: claude --model opus\n").unwrap();
        assert_eq!(
            load_install_command(&path).unwrap().as_deref(),
            Some("claude --model opus")
        );

        fs::write(&path, "install:\n  command: '  '\n").unwrap();
        assert!(
            load_install_command(&path)
                .unwrap_err()
                .to_string()
                .contains("install.command cannot be empty")
        );
    }

    #[test]
    fn pr_provider_defaults_to_auto() {
        let config: PrConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.provider.unwrap_or_default(), PrProvider::Auto);
    }

    #[test]
    fn pr_provider_accepts_supported_values() {
        for (value, expected) in [
            ("auto", PrProvider::Auto),
            ("github", PrProvider::Github),
            ("tea", PrProvider::Tea),
        ] {
            let config: PrConfig = serde_yaml::from_str(&format!("provider: {value}\n")).unwrap();
            assert_eq!(config.provider, Some(expected));
        }
        assert!(serde_yaml::from_str::<PrConfig>("provider: gitlab\n").is_err());
    }

    #[test]
    fn pr_provider_resolves_local_then_shared_then_global() {
        assert_eq!(
            resolve_pr_provider(
                Some(PrProvider::Github),
                Some(PrProvider::Tea),
                Some(PrProvider::Auto),
            ),
            PrProvider::Github
        );
        assert_eq!(
            resolve_pr_provider(None, Some(PrProvider::Tea), Some(PrProvider::Github)),
            PrProvider::Tea
        );
        assert_eq!(
            resolve_pr_provider(None, None, Some(PrProvider::Github)),
            PrProvider::Github
        );
    }
}
