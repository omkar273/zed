//! GitHub data for the Pull Requests panel, through `gh`.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;

use super::GhUser;

/// How many pull requests a section fetches at a time.
pub(crate) const PAGE_SIZE: usize = 5;

const LIST_QUERY: &str = "query($searchQuery: String!, $first: Int!, $after: String) {
  search(query: $searchQuery, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title isDraft headRefName reviewDecision
        author { login }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                contexts(first: 100) {
                  nodes {
                    __typename
                    ... on CheckRun { status conclusion }
                    ... on StatusContext { state }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}";

const FILES_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id
      files(first: 100) {
        nodes { path additions deletions changeType viewerViewedState }
      }
    }
  }
}";

const MARK_VIEWED_MUTATION: &str = "mutation($id: ID!, $path: String!) {
  markFileAsViewed(input: { pullRequestId: $id, path: $path }) { clientMutationId }
}";

const UNMARK_VIEWED_MUTATION: &str = "mutation($id: ID!, $path: String!) {
  unmarkFileAsViewed(input: { pullRequestId: $id, path: $path }) { clientMutationId }
}";

async fn gh(dir: &Path, args: &[&str]) -> Result<String> {
    let output = util::command::new_command("gh")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .context("running gh; is the GitHub CLI installed and on PATH?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// A group of pull requests in the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    WaitingForMyReview,
    CreatedByMe,
    AllOpen,
}

impl Section {
    pub const ALL: [Section; 3] = [
        Section::WaitingForMyReview,
        Section::CreatedByMe,
        Section::AllOpen,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::WaitingForMyReview => "Waiting For My Review",
            Section::CreatedByMe => "Created By Me",
            Section::AllOpen => "All Open",
        }
    }

    fn search_query(self) -> &'static str {
        match self {
            Section::WaitingForMyReview => {
                "repo:{owner}/{repo} is:pr is:open sort:created-desc review-requested:@me"
            }
            Section::CreatedByMe => {
                "repo:{owner}/{repo} is:pr is:open sort:created-desc author:@me"
            }
            Section::AllOpen => "repo:{owner}/{repo} is:pr is:open sort:created-desc",
        }
    }
}

/// The combined state of a pull request's checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckStatus {
    Success,
    Failure,
    Pending,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestSummary {
    pub number: u32,
    pub title: String,
    pub author: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub checks: CheckStatus,
    pub review_decision: Option<String>,
}

/// One page of a section's pull requests, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestPage {
    pub pull_requests: Vec<PullRequestSummary>,
    /// How many pull requests match the section, across all pages.
    pub total_count: usize,
    /// Pass to `list_section` to fetch the next page; `None` on the last page.
    pub next_cursor: Option<String>,
}

pub(crate) async fn list_section(
    repo: &Path,
    section: Section,
    after: Option<&str>,
) -> Result<PullRequestPage> {
    let search_query = format!("searchQuery={}", section.search_query());
    let first = format!("first={PAGE_SIZE}");
    let query = format!("query={LIST_QUERY}");
    let mut args: Vec<&str> = vec![
        "api",
        "graphql",
        "-F",
        &search_query,
        "-F",
        &first,
        "-f",
        &query,
    ];
    let after = after.map(|cursor| format!("after={cursor}"));
    if let Some(after) = &after {
        args.extend(["-f", after.as_str()]);
    }
    let output = gh(repo, &args).await?;
    parse_pull_request_page(&output)
}

#[derive(Deserialize)]
struct SearchResponse {
    data: SearchData,
}

#[derive(Deserialize)]
struct SearchData {
    search: SearchConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchConnection {
    issue_count: usize,
    page_info: PageInfo,
    nodes: Vec<Option<SearchPullRequest>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchPullRequest {
    number: u32,
    title: String,
    /// `None` when the author's account was deleted.
    author: Option<GhUser>,
    is_draft: bool,
    head_ref_name: String,
    review_decision: Option<String>,
    commits: Nodes<CommitNode>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Deserialize)]
struct StatusCheckRollup {
    contexts: Nodes<serde_json::Value>,
}

fn parse_pull_request_page(output: &str) -> Result<PullRequestPage> {
    let response: SearchResponse =
        serde_json::from_str(output).context("reading the pull request list")?;
    let search = response.data.search;
    let pull_requests = search
        .nodes
        .into_iter()
        .flatten()
        .map(|pull_request| {
            let checks = pull_request
                .commits
                .nodes
                .into_iter()
                .next()
                .and_then(|node| node.commit.status_check_rollup)
                .map_or(CheckStatus::None, |rollup| {
                    check_status(&rollup.contexts.nodes)
                });
            PullRequestSummary {
                checks,
                number: pull_request.number,
                title: pull_request.title,
                author: pull_request
                    .author
                    .map_or_else(|| "ghost".to_string(), |author| author.login),
                is_draft: pull_request.is_draft,
                head_ref_name: pull_request.head_ref_name,
                review_decision: pull_request
                    .review_decision
                    .filter(|decision| !decision.is_empty()),
            }
        })
        .collect();
    Ok(PullRequestPage {
        pull_requests,
        total_count: search.issue_count,
        next_cursor: search
            .page_info
            .end_cursor
            .filter(|_| search.page_info.has_next_page),
    })
}

/// Folds GitHub's check runs and commit statuses into one state: any failure
/// wins, then anything still running, then success.
fn check_status(rollup: &[serde_json::Value]) -> CheckStatus {
    if rollup.is_empty() {
        return CheckStatus::None;
    }
    let mut pending = false;
    for check in rollup {
        let field = |name: &str| check.get(name).and_then(|value| value.as_str());
        match check.get("__typename").and_then(|value| value.as_str()) {
            Some("StatusContext") => match field("state") {
                Some("FAILURE" | "ERROR") => return CheckStatus::Failure,
                Some("SUCCESS") => {}
                _ => pending = true,
            },
            _ => {
                if field("status") != Some("COMPLETED") {
                    pending = true;
                    continue;
                }
                match field("conclusion") {
                    Some(
                        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
                        | "STARTUP_FAILURE",
                    ) => return CheckStatus::Failure,
                    _ => {}
                }
            }
        }
    }
    if pending {
        CheckStatus::Pending
    } else {
        CheckStatus::Success
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeType {
    Added,
    Deleted,
    Renamed,
    Modified,
}

impl ChangeType {
    fn parse(change_type: &str) -> Self {
        match change_type {
            "ADDED" => ChangeType::Added,
            "DELETED" => ChangeType::Deleted,
            "RENAMED" | "COPIED" => ChangeType::Renamed,
            _ => ChangeType::Modified,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestFile {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub change_type: ChangeType,
    pub viewed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestFiles {
    /// The GraphQL node id, needed to mark files as viewed.
    pub node_id: String,
    pub files: Vec<PullRequestFile>,
}

pub(crate) async fn pull_request_files(repo: &Path, number: u32) -> Result<PullRequestFiles> {
    let query = format!("query={FILES_QUERY}");
    let number = format!("number={number}");
    let output = gh(
        repo,
        &[
            "api",
            "graphql",
            "-F",
            "owner={owner}",
            "-F",
            "name={repo}",
            "-F",
            &number,
            "-f",
            &query,
        ],
    )
    .await?;
    parse_pull_request_files(&output)
}

#[derive(Deserialize)]
struct FilesResponse {
    data: FilesData,
}

#[derive(Deserialize)]
struct FilesData {
    repository: FilesRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesRepository {
    pull_request: Option<FilesPullRequest>,
}

#[derive(Deserialize)]
struct FilesPullRequest {
    id: String,
    files: FilesConnection,
}

#[derive(Deserialize)]
struct FilesConnection {
    nodes: Vec<FileNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileNode {
    path: String,
    additions: u32,
    deletions: u32,
    change_type: String,
    viewer_viewed_state: String,
}

fn parse_pull_request_files(output: &str) -> Result<PullRequestFiles> {
    let response: FilesResponse =
        serde_json::from_str(output).context("reading the pull request's files")?;
    let pull_request = response
        .data
        .repository
        .pull_request
        .ok_or_else(|| anyhow!("GitHub didn't return this pull request"))?;
    let mut files: Vec<PullRequestFile> = pull_request
        .files
        .nodes
        .into_iter()
        .map(|node| PullRequestFile {
            change_type: ChangeType::parse(&node.change_type),
            viewed: node.viewer_viewed_state == "VIEWED",
            path: node.path,
            additions: node.additions,
            deletions: node.deletions,
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(PullRequestFiles {
        node_id: pull_request.id,
        files,
    })
}

pub(crate) async fn set_file_viewed(
    repo: &Path,
    pull_request_node_id: &str,
    path: &str,
    viewed: bool,
) -> Result<()> {
    let mutation = if viewed {
        MARK_VIEWED_MUTATION
    } else {
        UNMARK_VIEWED_MUTATION
    };
    let query = format!("query={mutation}");
    let id = format!("id={pull_request_node_id}");
    let path = format!("path={path}");
    gh(
        repo,
        &["api", "graphql", "-f", &id, "-f", &path, "-f", &query],
    )
    .await?;
    Ok(())
}

pub(crate) async fn open_repository_on_github(repo: &Path) -> Result<()> {
    gh(repo, &["repo", "view", "--web"]).await?;
    Ok(())
}

pub(crate) async fn open_pull_request_on_github(repo: &Path, number: u32) -> Result<()> {
    gh(repo, &["pr", "view", &number.to_string(), "--web"]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_status() {
        let parse = |json: &str| -> Vec<serde_json::Value> {
            serde_json::from_str(json).expect("valid test json")
        };
        assert_eq!(check_status(&[]), CheckStatus::None);
        assert_eq!(
            check_status(&parse(
                r#"[{"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS"},
                    {"__typename":"StatusContext","state":"SUCCESS"}]"#
            )),
            CheckStatus::Success
        );
        assert_eq!(
            check_status(&parse(
                r#"[{"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
                    {"__typename":"StatusContext","state":"SUCCESS"}]"#
            )),
            CheckStatus::Pending
        );
        assert_eq!(
            check_status(&parse(
                r#"[{"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":""},
                    {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}]"#
            )),
            CheckStatus::Failure
        );
        assert_eq!(
            check_status(&parse(
                r#"[{"__typename":"StatusContext","state":"ERROR"}]"#
            )),
            CheckStatus::Failure
        );
    }

    #[test]
    fn test_parse_pull_request_page() {
        let output = r#"{"data":{"search":{"issueCount":7,
            "pageInfo":{"hasNextPage":true,"endCursor":"Y3Vyc29yOjU="},
            "nodes":[
                {"number":66,"title":"Add x","isDraft":true,"headRefName":"feat/x",
                 "reviewDecision":null,"author":{"login":"ada"},
                 "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}},
                {"number":65,"title":"Fix y","isDraft":false,"headRefName":"fix/y",
                 "reviewDecision":"APPROVED","author":null,
                 "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[
                     {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}
                 ]}}}}]}}
            ]}}}"#;
        let page = parse_pull_request_page(output).expect("parses");
        assert_eq!(page.total_count, 7);
        assert_eq!(page.next_cursor.as_deref(), Some("Y3Vyc29yOjU="));
        assert_eq!(
            page.pull_requests,
            vec![
                PullRequestSummary {
                    number: 66,
                    title: "Add x".into(),
                    author: "ada".into(),
                    is_draft: true,
                    head_ref_name: "feat/x".into(),
                    checks: CheckStatus::None,
                    review_decision: None,
                },
                PullRequestSummary {
                    number: 65,
                    title: "Fix y".into(),
                    author: "ghost".into(),
                    is_draft: false,
                    head_ref_name: "fix/y".into(),
                    checks: CheckStatus::Failure,
                    review_decision: Some("APPROVED".into()),
                },
            ]
        );

        let last_page = r#"{"data":{"search":{"issueCount":0,
            "pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}"#;
        let page = parse_pull_request_page(last_page).expect("parses");
        assert!(page.pull_requests.is_empty());
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn test_parse_pull_request_files() {
        let output = r#"{"data":{"repository":{"pullRequest":{"id":"PR_1","files":{"nodes":[
            {"path":"b.rs","additions":1,"deletions":0,"changeType":"ADDED","viewerViewedState":"VIEWED"},
            {"path":"a.rs","additions":2,"deletions":3,"changeType":"MODIFIED","viewerViewedState":"UNVIEWED"}
        ]}}}}}"#;
        let files = parse_pull_request_files(output).expect("parses");
        assert_eq!(files.node_id, "PR_1");
        assert_eq!(files.files.len(), 2);
        assert_eq!(files.files[0].path, "a.rs");
        assert!(!files.files[0].viewed);
        assert_eq!(files.files[1].change_type, ChangeType::Added);
        assert!(files.files[1].viewed);

        let missing = r#"{"data":{"repository":{"pullRequest":null}}}"#;
        assert!(parse_pull_request_files(missing).is_err());
    }
}
