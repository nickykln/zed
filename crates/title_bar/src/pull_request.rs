use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Request};
use serde::Deserialize;

/// What GitHub says about the pull request of a branch.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BranchPullRequest {
    Found(PullRequest),
    NotFound,
    /// GitHub couldn't be asked: `gh` is missing or signed out, and the
    /// repository isn't public.
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PullRequest {
    pub number: u64,
    pub state: PullRequestState,
    pub url: SharedString,
    /// The branch the pull request merges into, as a remote-tracking ref like
    /// `origin/main`, when a remote of the repository points to its repository.
    pub base_ref: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PullRequestState {
    pub fn label(self) -> &'static str {
        match self {
            PullRequestState::Open => "Open",
            PullRequestState::Draft => "Draft",
            PullRequestState::Merged => "Merged",
            PullRequestState::Closed => "Closed",
        }
    }
}

/// A remote hosted on github.com.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GitHubRemote {
    name: String,
    owner: String,
    repo: String,
}

/// Looks up the pull request of the branch checked out in `work_directory`,
/// first with the `gh` command, which can see private repositories, then with
/// the public GitHub API. Returns `None` when no remote is on GitHub.
pub(crate) async fn branch_pull_request(
    work_directory: &Path,
    branch: &str,
    http_client: Arc<dyn HttpClient>,
) -> Option<BranchPullRequest> {
    let remotes = github_remotes(work_directory).await;
    if remotes.is_empty() {
        return None;
    }
    if let Some(pull_request) = pull_request_from_gh(work_directory, branch, &remotes).await {
        return Some(pull_request);
    }
    Some(pull_request_from_public_api(&remotes, branch, http_client).await)
}

async fn github_remotes(work_directory: &Path) -> Vec<GitHubRemote> {
    let output = util::command::new_command("git")
        .args(["remote", "-v"])
        .current_dir(work_directory)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            parse_github_remotes(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => {
            log::warn!(
                "failed to list git remotes: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Vec::new()
        }
        Err(error) => {
            log::warn!("failed to list git remotes: {error}");
            Vec::new()
        }
    }
}

/// Parses the output of `git remote -v`.
fn parse_github_remotes(output: &str) -> Vec<GitHubRemote> {
    let mut remotes: Vec<GitHubRemote> = Vec::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(url)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Some((owner, repo)) = parse_github_url(url) else {
            continue;
        };
        if remotes.iter().all(|remote| remote.name != name) {
            remotes.push(GitHubRemote {
                name: name.to_string(),
                owner,
                repo,
            });
        }
    }
    remotes
}

/// Owner and repository of a github.com URL, of a remote or a pull request.
fn parse_github_url(url: &str) -> Option<(String, String)> {
    let path = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut segments = path.split('/');
    let owner = segments.next().filter(|owner| !owner.is_empty())?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!repo.is_empty()).then(|| (owner.to_string(), repo.to_string()))
}

/// The remote-tracking ref of `branch` in the repository of a pull request.
fn base_ref(
    pull_request_url: &str,
    branch: &str,
    remotes: &[GitHubRemote],
) -> Option<SharedString> {
    let (owner, repo) = parse_github_url(pull_request_url)?;
    let remote = remotes.iter().find(|remote| {
        remote.owner.eq_ignore_ascii_case(&owner) && remote.repo.eq_ignore_ascii_case(&repo)
    })?;
    Some(format!("{}/{branch}", remote.name).into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u64,
    state: String,
    is_draft: bool,
    url: String,
    base_ref_name: String,
}

/// Asks `gh` for the pull request of the branch: in the repository `gh`
/// picks, then in each GitHub remote, since `gh` only looks in one of them.
/// Returns `None` when `gh` can't tell, because it is missing or signed out.
async fn pull_request_from_gh(
    work_directory: &Path,
    branch: &str,
    remotes: &[GitHubRemote],
) -> Option<BranchPullRequest> {
    if let found @ BranchPullRequest::Found(_) = gh_pr_view(work_directory, &[], remotes).await? {
        return Some(found);
    }
    for remote in remotes {
        let repository = format!("{}/{}", remote.owner, remote.repo);
        if let found @ BranchPullRequest::Found(_) =
            gh_pr_view(work_directory, &[branch, "--repo", &repository], remotes).await?
        {
            return Some(found);
        }
    }
    Some(BranchPullRequest::NotFound)
}

async fn gh_pr_view(
    work_directory: &Path,
    extra_args: &[&str],
    remotes: &[GitHubRemote],
) -> Option<BranchPullRequest> {
    let output = util::command::new_command("gh")
        .args(["pr", "view"])
        .args(extra_args)
        .args(["--json", "number,state,isDraft,url,baseRefName"])
        .current_dir(work_directory)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        let pull_request = serde_json::from_slice::<GhPullRequest>(&output.stdout)
            .context("failed to parse the output of `gh pr view`")
            .map_err(|error| log::warn!("{error:#}"))
            .ok()?;
        return Some(BranchPullRequest::Found(PullRequest {
            number: pull_request.number,
            state: match (pull_request.state.as_str(), pull_request.is_draft) {
                ("MERGED", _) => PullRequestState::Merged,
                ("CLOSED", _) => PullRequestState::Closed,
                (_, true) => PullRequestState::Draft,
                _ => PullRequestState::Open,
            },
            base_ref: base_ref(&pull_request.url, &pull_request.base_ref_name, remotes),
            url: pull_request.url.into(),
        }));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.to_lowercase().contains("no pull requests found") {
        return Some(BranchPullRequest::NotFound);
    }
    log::info!("`gh pr view` failed: {stderr}");
    None
}

#[derive(Deserialize)]
struct ApiPullRequest {
    number: u64,
    state: String,
    draft: Option<bool>,
    merged_at: Option<String>,
    html_url: String,
    base: ApiBranch,
}

#[derive(Deserialize)]
struct ApiBranch {
    #[serde(rename = "ref")]
    name: String,
}

/// Asks each GitHub remote for a pull request of `branch` pushed to it. Only
/// works for public repositories.
async fn pull_request_from_public_api(
    remotes: &[GitHubRemote],
    branch: &str,
    http_client: Arc<dyn HttpClient>,
) -> BranchPullRequest {
    let mut any_answered = false;
    for remote in remotes {
        match fetch_api_pull_requests(remote, branch, &http_client).await {
            Ok(pull_requests) => {
                any_answered = true;
                if let Some(pull_request) = pull_requests.into_iter().next() {
                    let state = if pull_request.merged_at.is_some() {
                        PullRequestState::Merged
                    } else if pull_request.state == "closed" {
                        PullRequestState::Closed
                    } else if pull_request.draft == Some(true) {
                        PullRequestState::Draft
                    } else {
                        PullRequestState::Open
                    };
                    return BranchPullRequest::Found(PullRequest {
                        number: pull_request.number,
                        state,
                        base_ref: base_ref(
                            &pull_request.html_url,
                            &pull_request.base.name,
                            remotes,
                        ),
                        url: pull_request.html_url.into(),
                    });
                }
            }
            Err(error) => log::info!("failed to ask GitHub for the pull request: {error:#}"),
        }
    }
    if any_answered {
        BranchPullRequest::NotFound
    } else {
        BranchPullRequest::Unknown
    }
}

async fn fetch_api_pull_requests(
    remote: &GitHubRemote,
    branch: &str,
    http_client: &Arc<dyn HttpClient>,
) -> Result<Vec<ApiPullRequest>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/pulls?state=all&per_page=1&head={}:{}",
        remote.owner,
        remote.repo,
        remote.owner,
        urlencoding::encode(branch)
    );
    let request = Request::get(&url)
        .header("Accept", "application/vnd.github+json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll)
        .body(AsyncBody::default())?;
    let mut response = http_client.send(request).await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "status {} for {url}",
        response.status().as_u16()
    );
    serde_json::from_slice(&body).context("failed to parse the pull requests of GitHub")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_remotes() {
        let remotes = parse_github_remotes(
            "fork\tgit@github.com:someone/zed.git (fetch)\n\
             fork\tgit@github.com:someone/zed.git (push)\n\
             origin\thttps://github.com/zed-industries/zed (fetch)\n\
             gitlab\thttps://gitlab.com/someone/zed.git (fetch)\n",
        );
        assert_eq!(
            remotes,
            vec![
                GitHubRemote {
                    name: "fork".into(),
                    owner: "someone".into(),
                    repo: "zed".into(),
                },
                GitHubRemote {
                    name: "origin".into(),
                    owner: "zed-industries".into(),
                    repo: "zed".into(),
                },
            ]
        );
    }

    #[test]
    fn test_base_ref_uses_the_remote_of_the_pull_request() {
        let remotes = parse_github_remotes(
            "fork\tgit@github.com:someone/zed.git (fetch)\n\
             origin\tssh://git@github.com/zed-industries/zed.git (fetch)\n",
        );
        assert_eq!(
            base_ref(
                "https://github.com/zed-industries/zed/pull/42",
                "main",
                &remotes
            ),
            Some("origin/main".into())
        );
        assert_eq!(
            base_ref("https://github.com/someone/zed/pull/7", "develop", &remotes),
            Some("fork/develop".into())
        );
        assert_eq!(
            base_ref("https://github.com/other/zed/pull/1", "main", &remotes),
            None
        );
    }
}
