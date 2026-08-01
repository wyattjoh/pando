use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::Status;
use crate::config::PrProvider;

const TEA_LIST_LIMIT: usize = 100;
const TEA_LIST_MAX_PAGES: usize = 100;

pub(super) struct PullRequestRef<'a> {
    pub base_repository: &'a str,
    pub base_branch: &'a str,
    pub head_ref: &'a str,
}

pub(super) struct CreatePullRequest<'a> {
    pub target: PullRequestRef<'a>,
    pub base_remote: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub status: Status,
}

pub(super) trait Provider {
    fn name(&self) -> &'static str;
    fn ensure_ready(&mut self, cwd: &Path) -> Result<()>;
    fn find_open(&self, cwd: &Path, request: &PullRequestRef<'_>) -> Result<Option<String>>;
    fn create(&self, cwd: &Path, request: &CreatePullRequest<'_>) -> Result<String>;
}

pub(super) struct ResolvedProvider {
    pub adapter: Box<dyn Provider>,
    pub base_repository: String,
    pub head_owner: String,
    pub head_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepositoryRef {
    host: String,
    slug: String,
    owner: String,
}

pub(super) fn resolve(
    configured: PrProvider,
    base_url: &str,
    head_url: &str,
    head_branch: &str,
) -> Result<ResolvedProvider> {
    let base = repository_ref(base_url)
        .with_context(|| format!("unsupported base repository remote URL: {base_url}"))?;
    let head = repository_ref(head_url)
        .with_context(|| format!("unsupported head repository remote URL: {head_url}"))?;
    let selected = match configured {
        PrProvider::Auto if base.host == "github.com" => PrProvider::Github,
        PrProvider::Auto => PrProvider::Tea,
        value => value,
    };
    let adapter: Box<dyn Provider> = match selected {
        PrProvider::Auto => unreachable!("auto provider is resolved above"),
        PrProvider::Github => {
            if base.host != "github.com" || head.host != "github.com" {
                bail!(
                    "pr.provider is github, but the base/head repository hosts are {}/{}",
                    base.host,
                    head.host
                );
            }
            Box::new(GithubProvider)
        }
        PrProvider::Tea => {
            if base.host == "github.com" || head.host == "github.com" {
                bail!("pr.provider is tea, but tea does not support GitHub repositories");
            }
            Box::new(TeaProvider {
                base_host: base.host.clone(),
                head_host: head.host.clone(),
                login: None,
            })
        }
    };
    let head_ref = head_ref(&base.slug, &head.slug, &head.owner, head_branch);

    Ok(ResolvedProvider {
        adapter,
        base_repository: base.slug,
        head_owner: head.owner,
        head_ref,
    })
}

struct GithubProvider;

impl Provider for GithubProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    fn ensure_ready(&mut self, _cwd: &Path) -> Result<()> {
        let version = Command::new("gh")
            .arg("--version")
            .output()
            .map_err(|_| anyhow::anyhow!("gh is not installed; install GitHub CLI"))?;
        if !version.status.success() {
            bail!("gh is unavailable; install GitHub CLI and run gh auth login");
        }
        let auth = Command::new("gh")
            .args(["auth", "status", "--hostname", "github.com"])
            .output()
            .context("failed to inspect GitHub CLI authentication")?;
        if !auth.status.success() {
            bail!("GitHub CLI is not authenticated; run gh auth login");
        }
        Ok(())
    }

    fn find_open(&self, _cwd: &Path, request: &PullRequestRef<'_>) -> Result<Option<String>> {
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                request.base_repository,
                "--head",
                request.head_ref,
                "--base",
                request.base_branch,
                "--state",
                "open",
                "--json",
                "url",
            ])
            .output()
            .context("failed to invoke GitHub CLI")?;
        if !output.status.success() {
            bail!(
                "{}",
                command_error("GitHub pull request preflight failed", &output)
            );
        }
        let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .context("GitHub pull request preflight returned malformed JSON")?;
        Ok(values.first().and_then(|value| {
            value
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }))
    }

    fn create(&self, _cwd: &Path, request: &CreatePullRequest<'_>) -> Result<String> {
        let mut command = Command::new("gh");
        command.args([
            "pr",
            "create",
            "--repo",
            request.target.base_repository,
            "--base",
            request.target.base_branch,
            "--head",
            request.target.head_ref,
            "--title",
            request.title,
            "--body",
            request.body,
        ]);
        if request.status == Status::Draft {
            command.arg("--draft");
        }
        let output = command.output().context("failed to invoke GitHub CLI")?;
        if !output.status.success() {
            bail!(
                "{}",
                command_error("GitHub pull request creation failed", &output)
            );
        }
        let url = String::from_utf8(output.stdout)
            .context("GitHub CLI returned a non-UTF-8 pull request URL")?
            .trim()
            .to_owned();
        if url.is_empty() {
            bail!("GitHub CLI did not return a pull request URL");
        }
        Ok(url)
    }
}

struct TeaProvider {
    base_host: String,
    head_host: String,
    login: Option<String>,
}

impl TeaProvider {
    fn login(&self) -> Result<&str> {
        self.login
            .as_deref()
            .context("tea login was not resolved during provider preflight")
    }
}

impl Provider for TeaProvider {
    fn name(&self) -> &'static str {
        "tea"
    }

    fn ensure_ready(&mut self, cwd: &Path) -> Result<()> {
        let version = Command::new("tea")
            .arg("--version")
            .current_dir(cwd)
            .output()
            .map_err(|_| anyhow::anyhow!("tea is not installed; install the Gitea tea CLI"))?;
        if !version.status.success() {
            bail!("tea is unavailable; install tea and run tea login add");
        }
        let output = Command::new("tea")
            .args(["login", "list", "--output", "json"])
            .current_dir(cwd)
            .output()
            .context("failed to list tea logins")?;
        if !output.status.success() {
            bail!("{}", command_error("tea login lookup failed", &output));
        }
        let logins: Vec<TeaLogin> = serde_json::from_slice(&output.stdout)
            .context("tea login list returned malformed JSON")?;
        self.login = logins
            .into_iter()
            .find(|login| {
                login.matches_host(&self.base_host) && login.matches_host(&self.head_host)
            })
            .map(|login| login.name);
        if self.login.is_none() {
            bail!(
                "tea has no single login matching base host {} and head host {}; run tea login add or select remotes from the same Gitea/Forgejo instance",
                self.base_host,
                self.head_host
            );
        }
        Ok(())
    }

    fn find_open(&self, cwd: &Path, request: &PullRequestRef<'_>) -> Result<Option<String>> {
        let login = self.login()?;
        let mut page = 1;
        loop {
            let page_value = page.to_string();
            let limit_value = TEA_LIST_LIMIT.to_string();
            let output = Command::new("tea")
                .args([
                    "pulls",
                    "list",
                    "--login",
                    login,
                    "--repo",
                    request.base_repository,
                    "--state",
                    "open",
                    "--fields",
                    "url,base,head",
                    "--output",
                    "json",
                    "--page",
                    &page_value,
                    "--limit",
                    &limit_value,
                ])
                .current_dir(cwd)
                .output()
                .context("failed to invoke tea")?;
            if !output.status.success() {
                bail!(
                    "{}",
                    command_error("tea pull request preflight failed", &output)
                );
            }
            let values: Vec<TeaPullRequest> = serde_json::from_slice(&output.stdout)
                .context("tea pull request preflight returned malformed JSON")?;
            if let Some(value) = values
                .iter()
                .find(|value| value.base == request.base_branch && value.head == request.head_ref)
            {
                return Ok(Some(value.url.clone()));
            }
            if values.len() < TEA_LIST_LIMIT {
                return Ok(None);
            }
            if page == TEA_LIST_MAX_PAGES {
                bail!("tea pull request preflight exceeded {TEA_LIST_MAX_PAGES} pages");
            }
            page += 1;
        }
    }

    fn create(&self, cwd: &Path, request: &CreatePullRequest<'_>) -> Result<String> {
        let title = tea_title(request.title, request.status);
        let output = Command::new("tea")
            .args([
                "pulls",
                "create",
                "--login",
                self.login()?,
                "--remote",
                request.base_remote,
                "--base",
                request.target.base_branch,
                "--head",
                request.target.head_ref,
                "--title",
                &title,
                "--description",
                request.body,
            ])
            .current_dir(cwd)
            .output()
            .context("failed to invoke tea")?;
        if !output.status.success() {
            bail!(
                "{}",
                command_error("tea pull request creation failed", &output)
            );
        }
        let stdout = String::from_utf8(output.stdout)
            .context("tea returned non-UTF-8 pull request output")?;
        if let Some(url) = tea_pull_request_url(&stdout) {
            return Ok(url);
        }
        self.find_open(cwd, &request.target)?.context(
            "tea created the pull request but neither returned its URL nor exposed it during post-create lookup",
        )
    }
}

#[derive(Deserialize)]
struct TeaLogin {
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    ssh_host: String,
}

impl TeaLogin {
    fn matches_host(&self, host: &str) -> bool {
        normalize_host(&self.ssh_host).is_some_and(|value| value == host)
            || repository_host(&self.url).is_some_and(|value| value == host)
    }
}

#[derive(Deserialize)]
struct TeaPullRequest {
    url: String,
    base: String,
    head: String,
}

fn tea_title(title: &str, status: Status) -> String {
    if status == Status::Ready || title.starts_with("WIP:") || title.starts_with("[WIP]") {
        title.to_owned()
    } else {
        format!("WIP: {title}")
    }
}

fn tea_pull_request_url(output: &str) -> Option<String> {
    output.lines().rev().find_map(|line| {
        ["https://", "http://"].into_iter().find_map(|scheme| {
            line.match_indices(scheme).find_map(|(start, _)| {
                let candidate: String = line[start..]
                    .chars()
                    .take_while(|character| !character.is_control() && !character.is_whitespace())
                    .collect();
                candidate.contains("/pulls/").then_some(candidate)
            })
        })
    })
}

fn head_ref(base_repo: &str, head_repo: &str, head_owner: &str, branch: &str) -> String {
    if base_repo == head_repo {
        branch.to_owned()
    } else {
        format!("{head_owner}:{branch}")
    }
}

fn repository_ref(url: &str) -> Option<RepositoryRef> {
    let value = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let (host, path) = if let Some((_, rest)) = value.split_once("://") {
        let authority_and_path = rest.split_once('/')?;
        let authority = authority_and_path.0.rsplit('@').next()?;
        (normalize_host(authority)?, authority_and_path.1)
    } else {
        let (_, host_and_path) = value.split_once('@')?;
        let (host, path) = host_and_path.split_once(':')?;
        (normalize_host(host)?, path)
    };
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    if owner.is_empty() || repository.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(RepositoryRef {
        host,
        slug: format!("{owner}/{repository}"),
        owner: owner.to_owned(),
    })
}

fn repository_host(url: &str) -> Option<String> {
    let (_, rest) = url.trim().split_once("://")?;
    let authority = rest.split('/').next()?.rsplit('@').next()?;
    normalize_host(authority)
}

fn normalize_host(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let host = if value.starts_with('[') {
        value.strip_prefix('[')?.split_once(']')?.0.to_owned()
    } else {
        value.split(':').next()?.to_owned()
    };
    Some(host.to_ascii_lowercase())
}

fn command_error(context: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    if message.is_empty() {
        context.to_owned()
    } else {
        format!("{context}: {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_selects_github_for_github_dot_com() {
        let provider = resolve(
            PrProvider::Auto,
            "git@github.com:alice/project.git",
            "git@github.com:bob/project.git",
            "topic",
        )
        .unwrap();
        assert_eq!(provider.adapter.name(), "github");
        assert_eq!(provider.base_repository, "alice/project");
        assert_eq!(provider.head_owner, "bob");
        assert_eq!(provider.head_ref, "bob:topic");
    }

    #[test]
    fn auto_selects_tea_for_other_forge_hosts() {
        let provider = resolve(
            PrProvider::Auto,
            "https://forge.example/alice/project.git",
            "https://forge.example/alice/project.git",
            "topic",
        )
        .unwrap();
        assert_eq!(provider.adapter.name(), "tea");
        assert_eq!(provider.head_ref, "topic");
    }

    #[test]
    fn configured_provider_must_support_the_remote_host() {
        assert!(
            resolve(
                PrProvider::Github,
                "https://forge.example/alice/project.git",
                "https://forge.example/alice/project.git",
                "topic",
            )
            .is_err()
        );
        assert!(
            resolve(
                PrProvider::Tea,
                "https://github.com/alice/project.git",
                "https://github.com/alice/project.git",
                "topic",
            )
            .is_err()
        );
    }

    #[test]
    fn repository_ref_accepts_https_and_ssh_remotes() {
        let https = repository_ref("https://forge.example/alice/project.git").unwrap();
        assert_eq!(https.host, "forge.example");
        assert_eq!(https.slug, "alice/project");
        assert_eq!(https.owner, "alice");

        let ssh = repository_ref("ssh://git@forge.example:2222/alice/project.git").unwrap();
        assert_eq!(ssh, https);
        assert_eq!(
            repository_ref("git@forge.example:alice/project.git"),
            Some(https)
        );
    }

    #[test]
    fn repository_ref_rejects_unsupported_shapes() {
        assert!(repository_ref("/tmp/project.git").is_none());
        assert!(repository_ref("https://forge.example/project").is_none());
        assert!(repository_ref("https://forge.example/a/b/c").is_none());
    }

    #[test]
    fn head_ref_qualifies_only_fork_branches() {
        assert_eq!(head_ref("a/project", "a/project", "a", "topic"), "topic");
        assert_eq!(head_ref("a/project", "b/project", "b", "topic"), "b:topic");
    }

    #[test]
    fn tea_drafts_use_the_default_wip_prefix() {
        assert_eq!(tea_title("Add feature", Status::Draft), "WIP: Add feature");
        assert_eq!(
            tea_title("WIP: Add feature", Status::Draft),
            "WIP: Add feature"
        );
        assert_eq!(tea_title("Add feature", Status::Ready), "Add feature");
    }

    #[test]
    fn tea_url_is_read_from_the_last_pull_url_line() {
        let output = "# #42 Add feature (open)\nbody\nhttps://forge.example/a/b/pulls/42\n";
        assert_eq!(
            tea_pull_request_url(output),
            Some("https://forge.example/a/b/pulls/42".into())
        );
    }

    #[test]
    fn tea_url_is_read_from_a_captured_osc_8_hyperlink() {
        let url = "https://forge.example/a/b/pulls/42";
        let output =
            format!("# #42 Add feature (open)\n\u{1b}]8;id=123;{url}\u{7}{url}\u{1b}]8;;\u{7}\n");
        assert_eq!(tea_pull_request_url(&output), Some(url.into()));
    }

    #[test]
    fn tea_login_matches_http_or_ssh_host() {
        let login = TeaLogin {
            name: "forge".into(),
            url: "https://forge.example".into(),
            ssh_host: "ssh.forge.example".into(),
        };
        assert!(login.matches_host("forge.example"));
        assert!(login.matches_host("ssh.forge.example"));
        assert!(!login.matches_host("other.example"));
    }
}
