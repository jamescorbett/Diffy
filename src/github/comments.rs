use super::{Repository, client, credential, error, runtime};
use gpui_kit::component::diff::{DiffLineRange, DiffSide};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CommentSide {
    Left,
    Right,
}
impl CommentSide {
    pub fn diff_side(self) -> DiffSide {
        match self {
            Self::Left => DiffSide::Original,
            Self::Right => DiffSide::Modified,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentAnchor {
    pub path: String,
    pub side: CommentSide,
    pub start: u32,
    pub end: u32,
    pub base_sha: String,
    pub head_sha: String,
}
impl CommentAnchor {
    pub fn from_range(
        range: &DiffLineRange,
        base_sha: String,
        head_sha: String,
    ) -> Result<Self, String> {
        if range.side() != range.end_side() {
            return Err("Select lines on just the original or modified side to comment.".into());
        }
        let start = u32::try_from(range.start().min(range.end()))
            .map_err(|_| "Line number is too large.")?;
        let end = u32::try_from(range.start().max(range.end()))
            .map_err(|_| "Line number is too large.")?;
        Ok(Self {
            path: range.path().to_string(),
            side: if range.side() == DiffSide::Original {
                CommentSide::Left
            } else {
                CommentSide::Right
            },
            start,
            end,
            base_sha,
            head_sha,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommentTarget {
    Line(CommentAnchor),
    Reply(u64),
}

/// Only published review comments; no credentials, HTML, or local file contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub path: String,
    pub line: Option<u32>,
    pub side: CommentSide,
    pub in_reply_to: Option<u64>,
    /// Thread resolution from GraphQL; REST alone cannot determine this.
    pub resolved: Option<bool>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}
#[derive(Deserialize)]
struct WireComment {
    id: u64,
    user: Option<User>,
    body: Option<String>,
    path: String,
    line: Option<u32>,
    side: Option<CommentSide>,
    in_reply_to_id: Option<u64>,
}
impl From<WireComment> for ReviewComment {
    fn from(c: WireComment) -> Self {
        Self {
            id: c.id,
            author: c.user.map_or_else(|| "Deleted user".into(), |u| u.login),
            body: c.body.unwrap_or_default(),
            path: c.path,
            line: c.line,
            side: c.side.unwrap_or(CommentSide::Right),
            in_reply_to: c.in_reply_to_id,
            resolved: None,
        }
    }
}

pub(super) fn list(repo: &Repository, number: u64) -> Result<Vec<ReviewComment>, String> {
    let runtime = runtime()?;
    let _entered = runtime.enter();
    let client = client(credential()?)?;
    runtime.block_on(async {
        let mut comments = tokio::time::timeout(Duration::from_secs(30), async {
            let mut comments = Vec::new();
            let mut bytes = 0;
            for page in 1..=20u32 {
                let result: octocrab::Page<WireComment> = client
                    .get(
                        format!(
                            "/repos/{}/{}/pulls/{number}/comments",
                            repo.owner, repo.name
                        ),
                        Some(&[("per_page", 100), ("page", page)]),
                    )
                    .await
                    .map_err(error)?;
                for comment in result.items {
                    bytes += comment.body.as_ref().map_or(0, String::len);
                    if bytes > 8 * 1024 * 1024 {
                        return Err(
                            "This PR has too many comments to display here. Open it in GitHub."
                                .to_owned(),
                        );
                    }
                    comments.push(ReviewComment::from(comment));
                }
                if result.next.is_none() {
                    return Ok(comments);
                }
            }
            Err("This PR has more than 2,000 comments. Open it in GitHub.".to_owned())
        })
        .await
        .map_err(|_| {
            "Loading comments timed out. Retry when your connection is available.".to_owned()
        })??;
        // Keep published comments readable if GraphQL is unavailable or denied.
        // Unknown is explicit in the UI, never silently treated as unresolved.
        if !comments.is_empty()
            && let Ok(Ok(resolutions)) = tokio::time::timeout(
                Duration::from_secs(10),
                thread_resolutions(&client, repo, number),
            )
            .await
        {
            apply_resolutions(&mut comments, &resolutions);
        }
        Ok(comments)
    })
}

const THREAD_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        nodes { isResolved comments(first: 1) { nodes { databaseId } } }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

#[derive(Deserialize)]
struct ThreadData {
    repository: Option<ThreadRepository>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRepository {
    pull_request: Option<ThreadPull>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPull {
    review_threads: ThreadPage,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPage {
    nodes: Vec<WireThread>,
    page_info: ThreadPageInfo,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireThread {
    is_resolved: bool,
    comments: ThreadRoots,
}
#[derive(Deserialize)]
struct ThreadRoots {
    nodes: Vec<ThreadRoot>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRoot {
    database_id: Option<u64>,
}

async fn thread_resolutions(
    client: &octocrab::Octocrab,
    repo: &Repository,
    number: u64,
) -> Result<HashMap<u64, bool>, String> {
    let mut resolutions = HashMap::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let data: ThreadData = client.graphql(&json!({
            "query": THREAD_QUERY,
            "variables": {"owner": repo.owner, "name": repo.name, "number": number, "after": cursor}
        })).await.map_err(error)?;
        let page = data
            .repository
            .and_then(|r| r.pull_request)
            .ok_or("GitHub couldn't provide thread status.")?
            .review_threads;
        for thread in page.nodes {
            if let Some(id) = thread.comments.nodes.first().and_then(|c| c.database_id) {
                resolutions.insert(id, thread.is_resolved);
            }
        }
        if !page.page_info.has_next_page {
            return Ok(resolutions);
        }
        let next = page
            .page_info
            .end_cursor
            .filter(|c| !c.is_empty())
            .ok_or("GitHub didn't provide the next thread page.")?;
        if cursor.as_ref() == Some(&next) {
            return Err("GitHub repeated a thread page.".into());
        }
        cursor = Some(next);
    }
    Err("This PR has more than 2,000 threads.".into())
}

fn apply_resolutions(comments: &mut [ReviewComment], resolutions: &HashMap<u64, bool>) {
    for comment in comments {
        comment.resolved = resolutions
            .get(&comment.in_reply_to.unwrap_or(comment.id))
            .copied();
    }
}

fn payload(target: &CommentTarget, body: &str) -> Result<Value, String> {
    if body.trim().is_empty() || body.len() > 65536 {
        return Err("Enter a comment between 1 and 65,536 bytes.".into());
    }
    let CommentTarget::Line(anchor) = target else {
        return Ok(json!({"body": body}));
    };
    if anchor.path.is_empty()
        || anchor.start == 0
        || anchor.start > anchor.end
        || anchor.head_sha.is_empty()
        || anchor.base_sha.is_empty()
    {
        return Err(
            "This comment has no valid diff location. Refresh and select its lines again.".into(),
        );
    }
    let mut value = json!({"body": body, "commit_id": anchor.head_sha, "path": anchor.path, "line": anchor.end, "side": anchor.side});
    if anchor.start != anchor.end {
        value["start_line"] = json!(anchor.start);
        value["start_side"] = json!(anchor.side);
    }
    Ok(value)
}

pub(super) fn post(
    repo: &Repository,
    number: u64,
    target: &CommentTarget,
    body: &str,
) -> Result<ReviewComment, String> {
    let payload = payload(target, body)?;
    let runtime = runtime()?;
    let _entered = runtime.enter();
    // POSTs are not idempotent. Cargo.toml deliberately leaves Octocrab's
    // optional `retry` feature disabled; do not add automatic retries here.
    let client = client(credential()?)?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            if let CommentTarget::Line(anchor) = target {
                let latest = client.pulls(&repo.owner, &repo.name).get(number).await.map_err(error)?;
                if latest.head.sha != anchor.head_sha || latest.base.sha != anchor.base_sha {
                    return Err("The PR changed since this draft was started. Your draft is kept; refresh and select the updated lines before posting.".into());
                }
            }
            let route = match target {
                CommentTarget::Line(_) => format!("/repos/{}/{}/pulls/{number}/comments", repo.owner, repo.name),
                CommentTarget::Reply(id) => format!("/repos/{}/{}/pulls/{number}/comments/{id}/replies", repo.owner, repo.name),
            };
            let comment: WireComment = client.post(route, Some(&payload)).await.map_err(|e| match e {
                octocrab::Error::GitHub { ref source, .. } if source.status_code.is_client_error() => {
                    if source.status_code.as_u16() == 422 { "GitHub couldn't attach this comment. Refresh the PR and check the selected lines; your draft is kept.".into() } else { error(e) }
                }
                _ => "Couldn't confirm whether GitHub posted this comment. Your draft is kept. Reload comments and check the thread before submitting again to avoid duplicates.".into(),
            })?;
            Ok(comment.into())
        }).await.map_err(|_| "Posting timed out; the comment may already be on GitHub. Reload comments and check the thread before submitting again. Your draft is kept.".to_owned())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anchors_and_payloads_use_source_lines_not_visual_rows() {
        let range = DiffLineRange::new("renamed.rs", DiffSide::Original, 12, 9);
        let anchor = CommentAnchor::from_range(&range, "base".into(), "head".into()).unwrap();
        assert_eq!(
            (anchor.start, anchor.end, anchor.side),
            (9, 12, CommentSide::Left)
        );
        assert_eq!(
            payload(&CommentTarget::Line(anchor), "hello").unwrap(),
            json!({"body":"hello", "commit_id":"head", "path":"renamed.rs", "line":12, "side":"LEFT", "start_line":9, "start_side":"LEFT"})
        );
        let range = DiffLineRange::new("added.rs", DiffSide::Modified, 3, 3);
        let anchor = CommentAnchor::from_range(&range, "base".into(), "head".into()).unwrap();
        assert_eq!(
            payload(&CommentTarget::Line(anchor), "hello").unwrap(),
            json!({"body":"hello", "commit_id":"head", "path":"added.rs", "line":3, "side":"RIGHT"})
        );
        assert!(
            CommentAnchor::from_range(
                &range.with_end_side(DiffSide::Original),
                "base".into(),
                "head".into()
            )
            .is_err()
        );
        assert_eq!(
            payload(&CommentTarget::Reply(42), "reply").unwrap(),
            json!({"body":"reply"})
        );
        assert!(payload(&CommentTarget::Reply(42), "  \n").is_err());
        assert!(payload(&CommentTarget::Reply(42), &"x".repeat(65537)).is_err());
    }
    #[test]
    fn outdated_and_deleted_author_comments_remain_readable() {
        let wire: WireComment = serde_json::from_value(json!({"id":1,"user":null,"body":"old comment","path":"old.rs","line":null,"side":"LEFT","in_reply_to_id":null})).unwrap();
        let c = ReviewComment::from(wire);
        assert_eq!(c.author, "Deleted user");
        assert_eq!(c.line, None);
        assert_eq!(c.body, "old comment");
        assert_eq!(c.resolved, None, "REST does not report resolution");
    }
    #[test]
    fn resolution_is_shared_by_roots_and_replies_but_missing_metadata_stays_unknown() {
        let mut comments: Vec<_> = [
            (10, None),
            (11, Some(10)),
            (12, None),
            (13, None),
            (15, Some(14)),
        ]
        .into_iter()
        .map(|(id, reply)| {
            ReviewComment::from(
                serde_json::from_value::<WireComment>(json!({
                    "id":id,"user":null,"body":"feedback","path":"f.rs","line":null,
                    "side":"RIGHT","in_reply_to_id":reply
                }))
                .unwrap(),
            )
        })
        .collect();
        apply_resolutions(
            &mut comments,
            &HashMap::from([(10, true), (12, false), (14, true)]),
        );
        assert_eq!(
            comments.iter().map(|c| c.resolved).collect::<Vec<_>>(),
            [Some(true), Some(true), Some(false), None, Some(true)]
        );
    }
    #[test]
    fn graphql_thread_pages_preserve_resolution_root_ids_and_cursor() {
        let data: ThreadData =
            serde_json::from_value(json!({"repository":{"pullRequest":{"reviewThreads":{
                "nodes":[
                    {"isResolved":true,"comments":{"nodes":[{"databaseId":123}]}},
                    {"isResolved":false,"comments":{"nodes":[{"databaseId":456}]}},
                    {"isResolved":true,"comments":{"nodes":[{"databaseId":null}]}}
                ],
                "pageInfo":{"hasNextPage":true,"endCursor":"next-thread-page"}
            }}}}))
            .unwrap();
        let page = data
            .repository
            .unwrap()
            .pull_request
            .unwrap()
            .review_threads;
        assert!(page.nodes[0].is_resolved);
        assert_eq!(page.nodes[0].comments.nodes[0].database_id, Some(123));
        assert!(!page.nodes[1].is_resolved);
        assert_eq!(page.nodes[2].comments.nodes[0].database_id, None);
        assert!(page.page_info.has_next_page);
        assert_eq!(
            page.page_info.end_cursor.as_deref(),
            Some("next-thread-page")
        );
        assert!(
            serde_json::from_value::<ThreadData>(json!({"repository":null}))
                .unwrap()
                .repository
                .is_none()
        );
    }
}
