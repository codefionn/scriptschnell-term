//! Hosted-forge integration: change requests and CI state for GitLab, Azure
//! DevOps and Forgejo (Codeberg and Gitea-compatible hosts).
//!
//! Blocking HTTP through `ureq` with timeouts; no async runtime. Callers run
//! queries off the UI thread. Response mapping lives in `mapping` and takes
//! plain byte slices, so it is tested without a network.

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod mapping;
mod url;

pub(crate) use url::base64;
use url::{push_encoded, push_encoded_path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    GitLab,
    AzureDevOps,
    Forgejo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeRepo {
    pub kind: ForgeKind,
    /// Web and API host. Includes a port when the remote URL named one for
    /// http(s). Azure DevOps cloud repositories always use `dev.azure.com`.
    pub host: String,
    /// GitLab: full namespace path "group/sub/repo"; Forgejo: "owner/repo";
    /// Azure: "organization/project/repo" (on-premises servers: the
    /// organization part is the collection path, such as "tfs/Coll").
    pub path: String,
}

/// Self-hosted instances: maps a host name to its forge kind.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ForgeHints {
    pub hosts: Vec<(String, ForgeKind)>,
}

/// Parses https, ssh (`git@host:path`, `ssh://git@host[:port]/path`) remote
/// URLs. Knows gitlab.com, codeberg.org, dev.azure.com, ssh.dev.azure.com
/// (v3/org/project/repo), `<org>.visualstudio.com`; other hosts only through
/// hints. Hints win over the built-in hosts.
pub fn detect(remote_url: &str, hints: &ForgeHints) -> Option<ForgeRepo> {
    url::detect(remote_url, hints)
}

/// The lowercase host name and the repository path of a remote URL, the
/// path without surrounding slashes and `.git`.
pub fn split_remote(remote_url: &str) -> Option<(String, String)> {
    url::split(remote_url)
}

/// Token lookup: GITLAB_TOKEN; AZURE_DEVOPS_EXT_PAT or AZURE_DEVOPS_PAT;
/// FORGEJO_TOKEN, then CODEBERG_TOKEN for codeberg.org. Returns None when
/// unset or empty.
pub fn token_from_env(kind: ForgeKind, host: &str) -> Option<String> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    match kind {
        ForgeKind::GitLab => var("GITLAB_TOKEN"),
        ForgeKind::AzureDevOps => var("AZURE_DEVOPS_EXT_PAT").or_else(|| var("AZURE_DEVOPS_PAT")),
        ForgeKind::Forgejo => var("FORGEJO_TOKEN").or_else(|| {
            host.eq_ignore_ascii_case("codeberg.org")
                .then(|| var("CODEBERG_TOKEN"))
                .flatten()
        }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeState {
    Open,
    Merged,
    Closed,
}

/// A merge request (GitLab) or pull request (Azure DevOps, Forgejo).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub source_branch: String,
    pub target_branch: String,
    pub url: String,
    pub draft: bool,
    pub state: ChangeState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineStatus {
    Pending,
    Running,
    Success,
    Failed,
    Canceled,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipeline {
    pub status: PipelineStatus,
    pub url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("request failed: {0}")]
    Http(#[from] ureq::Error),
    /// 401 or 403: missing, expired or under-scoped token.
    #[error("unauthorized (HTTP {code}); check the access token")]
    Unauthorized { code: u16 },
    #[error("HTTP {code}: {body}")]
    Status { code: u16, body: String },
    #[error("unexpected response: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Bodies kept in [`ForgeError::Status`] are cut to this many bytes.
const ERROR_BODY_LIMIT: usize = 512;

/// The proxy for every request the app sends itself: the desktop's proxy
/// setting when it names a server, else the proxy variables.
pub(crate) fn http_proxy() -> Option<ureq::Proxy> {
    let settings = cmdterm_env::os_proxy::current()?;
    cmdterm_update::proxy::build(&settings.server, &settings.no_proxy)
}

pub struct ForgeClient {
    repo: ForgeRepo,
    agent: ureq::Agent,
    /// Ready-made `Authorization`/`PRIVATE-TOKEN` header value.
    auth: Option<(&'static str, String)>,
}

impl ForgeClient {
    pub fn new(repo: ForgeRepo, token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(15)))
            .proxy(crate::forge::http_proxy())
            .tls_config(cmdterm_update::system_tls())
            .http_status_as_error(false)
            .user_agent(concat!("cmdterm/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        let auth = token.map(|token| match repo.kind {
            ForgeKind::GitLab => ("PRIVATE-TOKEN", token),
            ForgeKind::AzureDevOps => {
                let mut credentials = Vec::with_capacity(token.len() + 1);
                credentials.push(b':');
                credentials.extend_from_slice(token.as_bytes());
                (
                    "Authorization",
                    format!("Basic {}", url::base64(&credentials)),
                )
            }
            ForgeKind::Forgejo => ("Authorization", format!("token {token}")),
        });
        Self { repo, agent, auth }
    }

    pub fn repo(&self) -> &ForgeRepo {
        &self.repo
    }

    /// Browser URL of the repository.
    pub fn web_url(&self) -> String {
        let mut out = format!("https://{}/", self.repo.host);
        match self.repo.kind {
            ForgeKind::GitLab | ForgeKind::Forgejo => push_encoded_path(&mut out, &self.repo.path),
            ForgeKind::AzureDevOps => {
                let (org, project, repo) = self.azure_parts();
                push_encoded_path(&mut out, org);
                out.push('/');
                push_encoded(&mut out, project);
                out.push_str("/_git/");
                push_encoded(&mut out, repo);
            }
        }
        out
    }

    /// Browser URL of change request `number`.
    pub fn change_request_url(&self, number: u64) -> String {
        let mut out = self.web_url();
        let segment = match self.repo.kind {
            ForgeKind::GitLab => "/-/merge_requests/",
            ForgeKind::AzureDevOps => "/pullrequest/",
            ForgeKind::Forgejo => "/pulls/",
        };
        out.push_str(segment);
        out.push_str(&number.to_string());
        out
    }

    pub fn open_change_requests(&self) -> Result<Vec<ChangeRequest>, ForgeError> {
        match self.repo.kind {
            ForgeKind::GitLab => {
                let body =
                    self.get(&self.gitlab_url("/merge_requests?state=opened&per_page=100"))?;
                mapping::gitlab_merge_requests(&body)
            }
            ForgeKind::AzureDevOps => {
                let body = self.get(&self.azure_url(
                    "/pullrequests?searchCriteria.status=active&$top=100&api-version=7.1",
                ))?;
                mapping::azure_pull_requests(&body, |n| self.change_request_url(n))
            }
            ForgeKind::Forgejo => {
                let body = self.get(&self.forgejo_url("/pulls?state=open&limit=50"))?;
                mapping::forgejo_pulls(&body)
            }
        }
    }

    /// The change request whose source is `branch`: the open one if any,
    /// else (GitLab, Azure) the most recent merged or closed one. Forgejo
    /// only reports open ones, since its API cannot filter by head branch.
    pub fn change_request_for_branch(
        &self,
        branch: &str,
    ) -> Result<Option<ChangeRequest>, ForgeError> {
        let list = match self.repo.kind {
            ForgeKind::GitLab => {
                let mut query =
                    String::from("/merge_requests?state=all&per_page=20&source_branch=");
                push_encoded(&mut query, branch);
                mapping::gitlab_merge_requests(&self.get(&self.gitlab_url(&query))?)?
            }
            ForgeKind::AzureDevOps => {
                let mut query = String::from(
                    "/pullrequests?searchCriteria.status=all&$top=20&api-version=7.1\
                     &searchCriteria.sourceRefName=refs%2Fheads%2F",
                );
                push_encoded(&mut query, branch);
                let body = self.get(&self.azure_url(&query))?;
                mapping::azure_pull_requests(&body, |n| self.change_request_url(n))?
            }
            ForgeKind::Forgejo => self.open_change_requests()?,
        };
        Ok(pick_for_branch(list, branch))
    }

    /// Latest CI run for `branch`. Forgejo reports the combined commit
    /// status of the branch head. Azure DevOps reports the latest build of
    /// that branch name across the project.
    pub fn branch_pipeline(&self, branch: &str) -> Result<Option<Pipeline>, ForgeError> {
        let result = match self.repo.kind {
            ForgeKind::GitLab => {
                let mut query = String::from("/pipelines?per_page=1&ref=");
                push_encoded(&mut query, branch);
                self.get(&self.gitlab_url(&query))
                    .and_then(|body| mapping::gitlab_pipeline(&body))
            }
            ForgeKind::AzureDevOps => {
                let (org, project, _) = self.azure_parts();
                let mut url = format!("https://{}/", self.repo.host);
                push_encoded_path(&mut url, org);
                url.push('/');
                push_encoded(&mut url, project);
                url.push_str(
                    "/_apis/build/builds?$top=1&api-version=7.1&branchName=refs%2Fheads%2F",
                );
                push_encoded(&mut url, branch);
                self.get(&url).and_then(|body| mapping::azure_build(&body))
            }
            ForgeKind::Forgejo => {
                let mut path = String::from("/commits/");
                push_encoded(&mut path, branch);
                path.push_str("/status");
                self.get(&self.forgejo_url(&path))
                    .and_then(|body| mapping::forgejo_status(&body))
            }
        };
        match result {
            // Unknown branch.
            Err(ForgeError::Status { code: 404, .. }) => Ok(None),
            other => other,
        }
    }

    fn gitlab_url(&self, suffix: &str) -> String {
        let mut url = format!("https://{}/api/v4/projects/", self.repo.host);
        push_encoded(&mut url, &self.repo.path);
        url.push_str(suffix);
        url
    }

    fn forgejo_url(&self, suffix: &str) -> String {
        let mut url = format!("https://{}/api/v1/repos/", self.repo.host);
        push_encoded_path(&mut url, &self.repo.path);
        url.push_str(suffix);
        url
    }

    fn azure_url(&self, suffix: &str) -> String {
        let (org, project, repo) = self.azure_parts();
        let mut url = format!("https://{}/", self.repo.host);
        push_encoded_path(&mut url, org);
        url.push('/');
        push_encoded(&mut url, project);
        url.push_str("/_apis/git/repositories/");
        push_encoded(&mut url, repo);
        url.push_str(suffix);
        url
    }

    /// Splits "org/project/repo"; the org may itself contain slashes.
    fn azure_parts(&self) -> (&str, &str, &str) {
        let path = self.repo.path.as_str();
        let (rest, repo) = path.rsplit_once('/').unwrap_or(("", path));
        let (org, project) = rest.rsplit_once('/').unwrap_or(("", rest));
        (org, project, repo)
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, ForgeError> {
        let mut request = self.agent.get(url).header("Accept", "application/json");
        if let Some((name, value)) = &self.auth {
            request = request.header(*name, value);
        }
        let mut response = request.call()?;
        let code = response.status().as_u16();
        let body = response.body_mut().read_to_vec()?;
        match code {
            200..=299 => Ok(body),
            401 | 403 => Err(ForgeError::Unauthorized { code }),
            // Azure answers an expired PAT with a 203 sign-in page; that
            // lands in the 2xx arm and fails as a parse error instead.
            _ => {
                let end = body.len().min(ERROR_BODY_LIMIT);
                Err(ForgeError::Status {
                    code,
                    body: String::from_utf8_lossy(&body[..end]).into_owned(),
                })
            }
        }
    }
}

/// Open change request for `branch` first, else the first listed (newest).
fn pick_for_branch(list: Vec<ChangeRequest>, branch: &str) -> Option<ChangeRequest> {
    let mut fallback = None;
    for cr in list {
        if cr.source_branch != branch {
            continue;
        }
        if cr.state == ChangeState::Open {
            return Some(cr);
        }
        if fallback.is_none() {
            fallback = Some(cr);
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(kind: ForgeKind, host: &str, path: &str) -> ForgeClient {
        ForgeClient::new(
            ForgeRepo {
                kind,
                host: host.into(),
                path: path.into(),
            },
            Some("secret".into()),
        )
    }

    #[test]
    fn builds_web_and_api_urls() {
        let gl = client(ForgeKind::GitLab, "gitlab.com", "g/sub/r");
        assert_eq!(gl.web_url(), "https://gitlab.com/g/sub/r");
        assert_eq!(
            gl.change_request_url(5),
            "https://gitlab.com/g/sub/r/-/merge_requests/5"
        );
        assert_eq!(
            gl.gitlab_url("/pipelines"),
            "https://gitlab.com/api/v4/projects/g%2Fsub%2Fr/pipelines"
        );
        assert_eq!(gl.auth.as_ref().unwrap().0, "PRIVATE-TOKEN");

        let az = client(ForgeKind::AzureDevOps, "dev.azure.com", "org/My Proj/repo");
        assert_eq!(
            az.web_url(),
            "https://dev.azure.com/org/My%20Proj/_git/repo"
        );
        assert_eq!(
            az.change_request_url(3),
            "https://dev.azure.com/org/My%20Proj/_git/repo/pullrequest/3"
        );
        assert_eq!(
            az.azure_url("/pullrequests"),
            "https://dev.azure.com/org/My%20Proj/_apis/git/repositories/repo/pullrequests"
        );
        assert_eq!(az.auth.as_ref().unwrap().1, "Basic OnNlY3JldA==");

        let tfs = client(ForgeKind::AzureDevOps, "tfs.corp", "tfs/Coll/P/R");
        assert_eq!(tfs.web_url(), "https://tfs.corp/tfs/Coll/P/_git/R");

        let fj = client(ForgeKind::Forgejo, "codeberg.org", "o/r");
        assert_eq!(fj.web_url(), "https://codeberg.org/o/r");
        assert_eq!(fj.change_request_url(9), "https://codeberg.org/o/r/pulls/9");
        assert_eq!(
            fj.forgejo_url("/pulls"),
            "https://codeberg.org/api/v1/repos/o/r/pulls"
        );
        assert_eq!(fj.auth.as_ref().unwrap().1, "token secret");
    }

    #[test]
    fn picks_open_change_request_first() {
        let cr = |number, branch: &str, state| ChangeRequest {
            number,
            title: String::new(),
            author: String::new(),
            source_branch: branch.into(),
            target_branch: "main".into(),
            url: String::new(),
            draft: false,
            state,
        };
        let list = vec![
            cr(3, "b", ChangeState::Merged),
            cr(2, "a", ChangeState::Open),
            cr(1, "b", ChangeState::Open),
        ];
        assert_eq!(pick_for_branch(list.clone(), "b").unwrap().number, 1);
        assert_eq!(pick_for_branch(list[..1].to_vec(), "b").unwrap().number, 3);
        assert!(pick_for_branch(list, "zzz").is_none());
    }

    #[test]
    fn forge_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ForgeKind::AzureDevOps).unwrap(),
            "\"azure_dev_ops\""
        );
    }
}
