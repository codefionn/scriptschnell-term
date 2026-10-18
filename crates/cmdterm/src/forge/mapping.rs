//! JSON response bodies → public types. Kept apart from HTTP so tests feed
//! fixture bytes. Strings borrow from the body unless they contain escapes.

use std::borrow::Cow;

use serde::Deserialize;

use crate::forge::{ChangeRequest, ChangeState, ForgeError, Pipeline, PipelineStatus};

type Str<'a> = Cow<'a, str>;

fn own(value: Str<'_>) -> String {
    value.into_owned()
}

// GitLab ------------------------------------------------------------------

#[derive(Deserialize)]
struct GitLabMr<'a> {
    iid: u64,
    #[serde(borrow)]
    title: Str<'a>,
    #[serde(borrow)]
    author: GitLabUser<'a>,
    #[serde(borrow)]
    source_branch: Str<'a>,
    #[serde(borrow)]
    target_branch: Str<'a>,
    #[serde(borrow)]
    web_url: Str<'a>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    work_in_progress: bool,
    #[serde(borrow)]
    state: Str<'a>,
}

#[derive(Deserialize)]
struct GitLabUser<'a> {
    #[serde(borrow)]
    username: Str<'a>,
}

pub(crate) fn gitlab_merge_requests(body: &[u8]) -> Result<Vec<ChangeRequest>, ForgeError> {
    let list: Vec<GitLabMr<'_>> = serde_json::from_slice(body)?;
    Ok(list
        .into_iter()
        .map(|mr| ChangeRequest {
            number: mr.iid,
            state: match &*mr.state {
                "merged" => ChangeState::Merged,
                "closed" => ChangeState::Closed,
                _ => ChangeState::Open,
            },
            title: own(mr.title),
            author: own(mr.author.username),
            source_branch: own(mr.source_branch),
            target_branch: own(mr.target_branch),
            url: own(mr.web_url),
            draft: mr.draft || mr.work_in_progress,
        })
        .collect())
}

#[derive(Deserialize)]
struct GitLabPipeline<'a> {
    #[serde(borrow)]
    status: Str<'a>,
    #[serde(borrow, default)]
    web_url: Option<Str<'a>>,
}

pub(crate) fn gitlab_pipeline(body: &[u8]) -> Result<Option<Pipeline>, ForgeError> {
    let list: Vec<GitLabPipeline<'_>> = serde_json::from_slice(body)?;
    Ok(list.into_iter().next().map(|p| Pipeline {
        status: match &*p.status {
            "created" | "waiting_for_resource" | "preparing" | "pending" | "scheduled" => {
                PipelineStatus::Pending
            }
            "running" => PipelineStatus::Running,
            "success" => PipelineStatus::Success,
            "failed" => PipelineStatus::Failed,
            "canceled" | "canceling" => PipelineStatus::Canceled,
            _ => PipelineStatus::Unknown,
        },
        url: p.web_url.map(own),
    }))
}

// Azure DevOps ------------------------------------------------------------

#[derive(Deserialize)]
struct AzureList<T> {
    value: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzurePr<'a> {
    pull_request_id: u64,
    #[serde(borrow)]
    title: Str<'a>,
    #[serde(borrow)]
    created_by: AzureIdentity<'a>,
    #[serde(borrow)]
    source_ref_name: Str<'a>,
    #[serde(borrow)]
    target_ref_name: Str<'a>,
    #[serde(default)]
    is_draft: bool,
    #[serde(borrow)]
    status: Str<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureIdentity<'a> {
    #[serde(borrow)]
    display_name: Str<'a>,
}

fn branch_name(reference: Str<'_>) -> String {
    match reference.strip_prefix("refs/heads/") {
        Some(name) => name.to_owned(),
        None => own(reference),
    }
}

/// Azure PR JSON has no browser link, so `pr_url` builds it from the number.
pub(crate) fn azure_pull_requests(
    body: &[u8],
    pr_url: impl Fn(u64) -> String,
) -> Result<Vec<ChangeRequest>, ForgeError> {
    let list: AzureList<AzurePr<'_>> = serde_json::from_slice(body)?;
    Ok(list
        .value
        .into_iter()
        .map(|pr| ChangeRequest {
            number: pr.pull_request_id,
            state: match &*pr.status {
                "completed" => ChangeState::Merged,
                "abandoned" => ChangeState::Closed,
                _ => ChangeState::Open,
            },
            url: pr_url(pr.pull_request_id),
            title: own(pr.title),
            author: own(pr.created_by.display_name),
            source_branch: branch_name(pr.source_ref_name),
            target_branch: branch_name(pr.target_ref_name),
            draft: pr.is_draft,
        })
        .collect())
}

#[derive(Deserialize)]
struct AzureBuild<'a> {
    #[serde(borrow)]
    status: Str<'a>,
    #[serde(borrow, default)]
    result: Option<Str<'a>>,
    #[serde(borrow, default, rename = "_links")]
    links: Option<AzureLinks<'a>>,
}

#[derive(Deserialize)]
struct AzureLinks<'a> {
    #[serde(borrow, default)]
    web: Option<AzureLink<'a>>,
}

#[derive(Deserialize)]
struct AzureLink<'a> {
    #[serde(borrow)]
    href: Str<'a>,
}

pub(crate) fn azure_build(body: &[u8]) -> Result<Option<Pipeline>, ForgeError> {
    let list: AzureList<AzureBuild<'_>> = serde_json::from_slice(body)?;
    Ok(list.value.into_iter().next().map(|build| Pipeline {
        status: match (&*build.status, build.result.as_deref()) {
            ("completed", Some("succeeded" | "partiallySucceeded")) => PipelineStatus::Success,
            ("completed", Some("failed")) => PipelineStatus::Failed,
            ("completed", Some("canceled")) | ("cancelling", _) => PipelineStatus::Canceled,
            ("inProgress", _) => PipelineStatus::Running,
            ("notStarted" | "postponed", _) => PipelineStatus::Pending,
            _ => PipelineStatus::Unknown,
        },
        url: build.links.and_then(|l| l.web).map(|w| own(w.href)),
    }))
}

// Forgejo / Gitea ---------------------------------------------------------

#[derive(Deserialize)]
struct ForgejoPull<'a> {
    number: u64,
    #[serde(borrow)]
    title: Str<'a>,
    #[serde(borrow)]
    user: ForgejoUser<'a>,
    #[serde(borrow)]
    head: ForgejoRef<'a>,
    #[serde(borrow)]
    base: ForgejoRef<'a>,
    #[serde(borrow)]
    html_url: Str<'a>,
    #[serde(default)]
    draft: bool,
    #[serde(borrow)]
    state: Str<'a>,
    #[serde(default)]
    merged: bool,
}

#[derive(Deserialize)]
struct ForgejoUser<'a> {
    #[serde(borrow)]
    login: Str<'a>,
}

#[derive(Deserialize)]
struct ForgejoRef<'a> {
    #[serde(borrow, rename = "ref")]
    name: Str<'a>,
}

pub(crate) fn forgejo_pulls(body: &[u8]) -> Result<Vec<ChangeRequest>, ForgeError> {
    let list: Vec<ForgejoPull<'_>> = serde_json::from_slice(body)?;
    Ok(list
        .into_iter()
        .map(|pr| {
            // Older servers lack `draft` and mark drafts by title prefix.
            let draft = pr.draft
                || ["WIP:", "[WIP]"].iter().any(|p| {
                    pr.title
                        .get(..p.len())
                        .is_some_and(|t| t.eq_ignore_ascii_case(p))
                });
            ChangeRequest {
                number: pr.number,
                state: match (&*pr.state, pr.merged) {
                    (_, true) => ChangeState::Merged,
                    ("closed", false) => ChangeState::Closed,
                    _ => ChangeState::Open,
                },
                title: own(pr.title),
                author: own(pr.user.login),
                source_branch: own(pr.head.name),
                target_branch: own(pr.base.name),
                url: own(pr.html_url),
                draft,
            }
        })
        .collect())
}

#[derive(Deserialize)]
struct ForgejoCombinedStatus<'a> {
    #[serde(borrow, default)]
    state: Option<Str<'a>>,
    #[serde(default)]
    total_count: u64,
    /// Gitea sends `null` instead of `[]` on some versions.
    #[serde(borrow, default)]
    statuses: Option<Vec<ForgejoStatus<'a>>>,
}

#[derive(Deserialize)]
struct ForgejoStatus<'a> {
    #[serde(borrow, default)]
    target_url: Option<Str<'a>>,
}

/// Combined commit status. `None` when no CI reported on the commit.
pub(crate) fn forgejo_status(body: &[u8]) -> Result<Option<Pipeline>, ForgeError> {
    let combined: ForgejoCombinedStatus<'_> = serde_json::from_slice(body)?;
    let statuses = combined.statuses.unwrap_or_default();
    if combined.total_count == 0 && statuses.is_empty() {
        return Ok(None);
    }
    let status = match combined.state.as_deref() {
        Some("pending") => PipelineStatus::Pending,
        Some("success") => PipelineStatus::Success,
        Some("failure" | "error") => PipelineStatus::Failed,
        _ => PipelineStatus::Unknown,
    };
    let url = statuses
        .into_iter()
        .find_map(|s| s.target_url.filter(|u| !u.is_empty()))
        .map(own);
    Ok(Some(Pipeline { status, url }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_gitlab_merge_requests() {
        let body = br#"[{"id":9,"iid":42,"title":"Add \"x\"","state":"opened","draft":false,
            "work_in_progress":true,"source_branch":"feat","target_branch":"main",
            "author":{"id":1,"username":"alice","name":"Alice"},
            "web_url":"https://gitlab.com/g/r/-/merge_requests/42","labels":[]}]"#;
        let list = gitlab_merge_requests(body).unwrap();
        assert_eq!(
            list,
            [ChangeRequest {
                number: 42,
                title: "Add \"x\"".into(),
                author: "alice".into(),
                source_branch: "feat".into(),
                target_branch: "main".into(),
                url: "https://gitlab.com/g/r/-/merge_requests/42".into(),
                draft: true,
                state: ChangeState::Open,
            }]
        );
        assert!(gitlab_merge_requests(b"[]").unwrap().is_empty());
        assert!(gitlab_merge_requests(b"{}").is_err());
    }

    #[test]
    fn maps_gitlab_pipelines() {
        let body = br#"[{"id":1,"status":"running","ref":"main","web_url":"https://gitlab.com/g/r/-/pipelines/1"}]"#;
        assert_eq!(
            gitlab_pipeline(body).unwrap(),
            Some(Pipeline {
                status: PipelineStatus::Running,
                url: Some("https://gitlab.com/g/r/-/pipelines/1".into()),
            })
        );
        assert_eq!(gitlab_pipeline(b"[]").unwrap(), None);
        let canceled = br#"[{"status":"canceled"}]"#;
        assert_eq!(
            gitlab_pipeline(canceled).unwrap().unwrap().status,
            PipelineStatus::Canceled
        );
    }

    #[test]
    fn maps_azure_pull_requests() {
        let body = br#"{"value":[{"pullRequestId":7,"status":"active","isDraft":true,
            "title":"Fix","createdBy":{"displayName":"Bob B","uniqueName":"bob@x"},
            "sourceRefName":"refs/heads/fix/a","targetRefName":"refs/heads/main",
            "repository":{"name":"repo"}}],"count":1}"#;
        let list = azure_pull_requests(body, |n| {
            format!("https://dev.azure.com/o/p/_git/r/pullrequest/{n}")
        })
        .unwrap();
        assert_eq!(list.len(), 1);
        let pr = &list[0];
        assert_eq!(pr.number, 7);
        assert_eq!(pr.author, "Bob B");
        assert_eq!(pr.source_branch, "fix/a");
        assert_eq!(pr.target_branch, "main");
        assert!(pr.draft);
        assert_eq!(pr.state, ChangeState::Open);
        assert_eq!(pr.url, "https://dev.azure.com/o/p/_git/r/pullrequest/7");
    }

    #[test]
    fn maps_azure_builds() {
        let body = br#"{"count":1,"value":[{"id":5,"status":"completed","result":"failed",
            "_links":{"web":{"href":"https://dev.azure.com/o/p/_build/results?buildId=5"}}}]}"#;
        assert_eq!(
            azure_build(body).unwrap(),
            Some(Pipeline {
                status: PipelineStatus::Failed,
                url: Some("https://dev.azure.com/o/p/_build/results?buildId=5".into()),
            })
        );
        let running = br#"{"value":[{"status":"inProgress"}]}"#;
        assert_eq!(
            azure_build(running).unwrap().unwrap().status,
            PipelineStatus::Running
        );
        assert_eq!(azure_build(br#"{"count":0,"value":[]}"#).unwrap(), None);
    }

    #[test]
    fn maps_forgejo_pulls() {
        let body = br#"[{"id":3,"number":12,"title":"WIP: thing","state":"open","merged":false,
            "user":{"login":"carol"},"head":{"ref":"thing","label":"carol:thing"},
            "base":{"ref":"main"},"html_url":"https://codeberg.org/o/r/pulls/12"},
            {"number":13,"title":"done","state":"closed","merged":true,"draft":false,
            "user":{"login":"d"},"head":{"ref":"x"},"base":{"ref":"main"},
            "html_url":"https://codeberg.org/o/r/pulls/13"}]"#;
        let list = forgejo_pulls(body).unwrap();
        assert_eq!(list[0].number, 12);
        assert!(list[0].draft);
        assert_eq!(list[0].author, "carol");
        assert_eq!(list[0].source_branch, "thing");
        assert_eq!(list[0].state, ChangeState::Open);
        assert_eq!(list[1].state, ChangeState::Merged);
        assert!(!list[1].draft);
    }

    #[test]
    fn maps_forgejo_status() {
        let body = br#"{"state":"success","sha":"abc","total_count":2,"statuses":[
            {"status":"success","target_url":"","context":"a"},
            {"status":"success","target_url":"https://ci.example/b/1","context":"b"}]}"#;
        assert_eq!(
            forgejo_status(body).unwrap(),
            Some(Pipeline {
                status: PipelineStatus::Success,
                url: Some("https://ci.example/b/1".into()),
            })
        );
        let none = br#"{"state":"","total_count":0,"statuses":[]}"#;
        assert_eq!(forgejo_status(none).unwrap(), None);
        let failing = br#"{"state":"failure","total_count":1,"statuses":[{"target_url":null}]}"#;
        assert_eq!(
            forgejo_status(failing).unwrap(),
            Some(Pipeline {
                status: PipelineStatus::Failed,
                url: None
            })
        );
    }
}
