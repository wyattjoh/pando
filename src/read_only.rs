use std::{env, path::PathBuf};

use anyhow::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, Serializer};

use crate::{
    Condition, Row, Worktree, WorktreeKind,
    config::EffectiveConfig,
    git,
    protocol::{BytePath, Diagnostic, ErrorBody},
    smart::port_for_branch,
};

pub const LIST_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "repository.invalid",
];
pub const GET_ERRORS: &[&str] = &[
    "json.invalid_request",
    "json.unsupported_schema_version",
    "get.invalid_property",
    "repository.invalid",
    "repository.detached",
    "repository.primary_unavailable",
    "repository.root_unavailable",
];
pub const ACTIONS: &[&str] = &[];

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {}

#[derive(Clone, Debug, Default, JsonSchema, Serialize)]
pub struct QueryContext {}

#[derive(Clone, Debug)]
pub struct QueryFailure {
    pub code: &'static str,
    pub message: String,
    pub human_message: Option<String>,
}

impl QueryFailure {
    #[must_use]
    pub fn repository(error: &anyhow::Error) -> Self {
        Self {
            code: "repository.invalid",
            message: format!("{error:#}"),
            human_message: None,
        }
    }
}

impl From<QueryFailure> for ErrorBody {
    fn from(failure: QueryFailure) -> Self {
        Self {
            code: failure.code.into(),
            message: failure.message,
        }
    }
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct WorktreeRecord {
    kind: String,
    branch: Option<String>,
    path: BytePath,
    head: Option<String>,
    /// RFC 3339 committer timestamp for the worktree's HEAD commit.
    last_commit_at: Option<String>,
    condition: String,
    current: bool,
    navigable: bool,
    lock_reason: Option<String>,
    prune_reason: Option<String>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct ListSummary {
    total: usize,
    dirty: usize,
    unknown: usize,
    missing: usize,
    inaccessible: usize,
    bare: usize,
    locked: usize,
    prunable: usize,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct ListResult {
    outcome: &'static str,
    worktrees: Vec<WorktreeRecord>,
    summary: ListSummary,
}

#[derive(Debug)]
pub struct WorktreeListOutcome {
    pub result: ListResult,
    pub repository: git::Repository,
    pub rows: Vec<Row>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct BranchRecord {
    branch: String,
    head: String,
    /// RFC 3339 committer timestamp for the branch tip commit.
    last_commit_at: Option<String>,
    path: Option<BytePath>,
    condition: Option<String>,
    current: bool,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct BranchListSummary {
    total: usize,
    checked_out: usize,
    dirty: usize,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct BranchListResult {
    outcome: &'static str,
    branches: Vec<BranchRecord>,
    summary: BranchListSummary,
}

#[derive(Debug)]
pub struct BranchListOutcome {
    pub result: BranchListResult,
    pub repository: git::Repository,
    pub rows: Vec<Row>,
    pub diagnostics: Vec<Diagnostic>,
}

fn condition(value: Condition) -> &'static str {
    match value {
        Condition::Clean => "clean",
        Condition::Dirty => "dirty",
        Condition::Unknown => "unknown",
        Condition::Missing => "missing",
        Condition::Inaccessible => "inaccessible",
    }
}

fn diagnostic(warning: Option<&str>) -> Vec<Diagnostic> {
    warning.map_or_else(Vec::new, |warning| {
        vec![Diagnostic {
            source: "git.commit_metadata".into(),
            stream: "metadata".into(),
            content: warning.into(),
            original_size: warning.len(),
            truncated: false,
        }]
    })
}

fn worktree_record(worktree: &Worktree) -> WorktreeRecord {
    let (kind, branch) = match &worktree.kind {
        WorktreeKind::Branch(branch) => ("branch", Some(branch.clone())),
        WorktreeKind::Detached => ("detached", None),
        WorktreeKind::Bare => ("bare", None),
        WorktreeKind::Unknown => ("unknown", None),
    };
    WorktreeRecord {
        kind: kind.into(),
        branch,
        path: BytePath::path(&worktree.path),
        head: worktree.head.clone(),
        last_commit_at: worktree.machine_last_commit_at(),
        condition: condition(worktree.condition).into(),
        current: worktree.current,
        navigable: worktree.navigable(),
        lock_reason: worktree.locked.clone(),
        prune_reason: worktree.prunable.clone(),
    }
}

/// Observes the repository once for both human and structured worktree listing.
///
/// # Errors
/// Returns a typed repository failure when the current directory or Git metadata cannot be read.
pub fn list_worktrees() -> Result<WorktreeListOutcome, QueryFailure> {
    let cwd = env::current_dir()
        .context("failed to read current directory")
        .map_err(|error| QueryFailure::repository(&error))?;
    let repository =
        git::repository_with_metadata(&cwd).map_err(|error| QueryFailure::repository(&error))?;
    let count = |wanted| {
        repository
            .worktrees
            .iter()
            .filter(|worktree| worktree.condition == wanted)
            .count()
    };
    let result = ListResult {
        outcome: "listed",
        worktrees: repository.worktrees.iter().map(worktree_record).collect(),
        summary: ListSummary {
            total: repository.worktrees.len(),
            dirty: count(Condition::Dirty),
            unknown: count(Condition::Unknown),
            missing: count(Condition::Missing),
            inaccessible: count(Condition::Inaccessible),
            bare: repository
                .worktrees
                .iter()
                .filter(|worktree| worktree.is_bare())
                .count(),
            locked: repository
                .worktrees
                .iter()
                .filter(|worktree| worktree.locked.is_some())
                .count(),
            prunable: repository
                .worktrees
                .iter()
                .filter(|worktree| worktree.prunable.is_some())
                .count(),
        },
    };
    let rows = repository
        .worktrees
        .iter()
        .map(Row::from_worktree)
        .collect();
    let diagnostics = diagnostic(repository.metadata_warning.as_deref());
    Ok(WorktreeListOutcome {
        result,
        repository,
        rows,
        diagnostics,
    })
}

fn branch_record(record: &git::BranchRecord, worktrees: &[Worktree]) -> BranchRecord {
    let row = Row::from_branch(record, worktrees);
    BranchRecord {
        branch: record.branch.clone(),
        head: record.head.clone(),
        last_commit_at: row.machine_last_commit_at(),
        path: row.path.as_deref().map(BytePath::path),
        condition: row.condition.map(|value| condition(value).into()),
        current: row.current,
    }
}

/// Observes the repository once for both human and structured branch listing.
///
/// # Errors
/// Returns a typed repository failure when the current directory or Git metadata cannot be read.
pub fn list_branches() -> Result<BranchListOutcome, QueryFailure> {
    let cwd = env::current_dir()
        .context("failed to read current directory")
        .map_err(|error| QueryFailure::repository(&error))?;
    let branches =
        git::repository_with_branches(&cwd).map_err(|error| QueryFailure::repository(&error))?;
    let repository = branches.repository;
    let rows: Vec<_> = branches
        .branches
        .iter()
        .map(|branch| Row::from_branch(branch, &repository.worktrees))
        .collect();
    let records: Vec<_> = branches
        .branches
        .iter()
        .map(|branch| branch_record(branch, &repository.worktrees))
        .collect();
    let result = BranchListResult {
        outcome: "listed",
        summary: BranchListSummary {
            total: records.len(),
            checked_out: records
                .iter()
                .filter(|record| record.path.is_some())
                .count(),
            dirty: records
                .iter()
                .filter(|record| record.condition.as_deref() == Some("dirty"))
                .count(),
        },
        branches: records,
    };
    let diagnostics = diagnostic(repository.metadata_warning.as_deref());
    Ok(BranchListOutcome {
        result,
        repository,
        rows,
        diagnostics,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GetProperty {
    Branch,
    Port,
    WorktreePath,
    PrimaryWorktreePath,
    WorktreeRoot,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GetRequest {
    pub property: GetProperty,
}

#[derive(Clone, Debug)]
pub enum PropertyValue {
    Text(String),
    Port(u16),
    Path(PathBuf),
}

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum PropertyWireValue {
    Text(String),
    Port(u16),
    Path(BytePath),
}

impl Serialize for PropertyValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Text(value) => value.serialize(serializer),
            Self::Port(value) => value.serialize(serializer),
            Self::Path(value) => BytePath::path(value).serialize(serializer),
        }
    }
}

impl JsonSchema for PropertyValue {
    fn schema_name() -> String {
        PropertyWireValue::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::schema::Schema {
        PropertyWireValue::json_schema(generator)
    }
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
pub struct GetResult {
    outcome: &'static str,
    property: &'static str,
    value: PropertyValue,
}

impl GetResult {
    #[must_use]
    pub fn value(&self) -> &PropertyValue {
        &self.value
    }
}

/// Reads one current-worktree property for either public adapter.
///
/// # Errors
/// Returns a typed failure when repository context or the requested value is unavailable.
pub fn get(property: GetProperty) -> Result<GetResult, QueryFailure> {
    let cwd = env::current_dir()
        .context("failed to read current directory")
        .map_err(|error| QueryFailure::repository(&error))?;
    let repository = git::repository(&cwd).map_err(|error| QueryFailure::repository(&error))?;
    let (property, value) = match property {
        GetProperty::Branch => match &repository.current().kind {
            WorktreeKind::Branch(branch) => ("branch", PropertyValue::Text(branch.clone())),
            _ => return Err(detached_failure(&repository)),
        },
        GetProperty::Port => match &repository.current().kind {
            WorktreeKind::Branch(branch) => ("port", PropertyValue::Port(port_for_branch(branch))),
            _ => return Err(detached_failure(&repository)),
        },
        GetProperty::WorktreePath => (
            "worktree_path",
            PropertyValue::Path(resolved_path(&repository.current().path)?),
        ),
        GetProperty::PrimaryWorktreePath => (
            "primary_worktree_path",
            PropertyValue::Path(resolved_path(repository.primary.as_ref().ok_or_else(
                || QueryFailure {
                    code: "repository.primary_unavailable",
                    message: "primary worktree unavailable".into(),
                    human_message: Some("the current repository has no primary worktree".into()),
                },
            )?)?),
        ),
        GetProperty::WorktreeRoot => {
            if repository.primary.is_none() {
                return Err(QueryFailure {
                    code: "repository.root_unavailable",
                    message: "worktree root unavailable".into(),
                    human_message: Some(
                        "the current repository has no primary worktree; a creation root is unavailable"
                            .into(),
                    ),
                });
            }
            let config = EffectiveConfig::load(&repository)
                .map_err(|error| QueryFailure::repository(&error))?;
            let root = config.root.ok_or_else(|| QueryFailure {
                code: "repository.root_unavailable",
                message: "worktree root unavailable".into(),
                human_message: Some("worktree root is not configured".into()),
            })?;
            ("worktree_root", PropertyValue::Path(root))
        }
    };
    Ok(GetResult {
        outcome: "value",
        property,
        value,
    })
}

fn detached_failure(repository: &git::Repository) -> QueryFailure {
    QueryFailure {
        code: "repository.detached",
        message: "current worktree has no branch".into(),
        human_message: Some(format!(
            "the current worktree at {} is detached; this query requires a named branch",
            repository.current().path.display()
        )),
    }
}

fn resolved_path(path: &std::path::Path) -> Result<PathBuf, QueryFailure> {
    path.canonicalize().map_err(|error| QueryFailure {
        code: "repository.invalid",
        message: format!("failed to resolve path {}: {error}", path.display()),
        human_message: None,
    })
}
