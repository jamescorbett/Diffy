//! GitHub review access. Credentials stay inside the backend, never in saved state.
mod comments;
use crate::repository::{self, ChangedFile, Preview};
pub use comments::{CommentAnchor, CommentSide, CommentTarget, ReviewComment};
use gpui_kit::{SharedString, component::diff::DiffFile};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    io::Read,
    num::NonZeroUsize,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Repository {
    pub owner: String,
    pub name: String,
}
impl Repository {
    pub fn parse(remote: &str) -> Option<Self> {
        let path = if let Some(path) = remote.strip_prefix("git@github.com:") {
            path.to_owned()
        } else {
            let url = url::Url::parse(remote).ok()?;
            if url.host_str()? != "github.com"
                || !matches!(url.scheme(), "https" | "ssh")
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return None;
            }
            url.path().trim_start_matches('/').to_owned()
        };
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        let (owner, name) = path.split_once('/')?;
        if [owner, name].iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        }) {
            return None;
        }
        Some(Self {
            owner: owner.into(),
            name: name.into(),
        })
    }
    pub fn label(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    pub fn url(&self) -> String {
        format!("https://github.com/{}", self.label())
    }
    pub fn pr_url(&self, number: u64) -> String {
        format!("{}/pull/{number}", self.url())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub repository: Repository,
}
pub fn remotes(path: &Path) -> Result<Vec<Remote>, String> {
    let bytes = repository::git(path, &["remote", "-v"])?;
    let text = String::from_utf8(bytes).map_err(|_| "Couldn't read repository remotes.")?;
    let mut result = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(url), Some("(fetch)")) = (parts.next(), parts.next(), parts.next())
            && let Some(repository) = Repository::parse(url)
        {
            result.push(Remote {
                name: name.into(),
                repository,
            });
        }
    }
    result.sort_by_key(|r| (r.name != "origin", r.name.clone()));
    Ok(result)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub draft: bool,
    pub base: String,
    pub head: String,
    pub base_sha: String,
    pub head_sha: String,
}
impl PullRequest {
    pub fn tooltip(&self) -> String {
        format!(
            "#{} {}{}\n{} · {} → {}",
            self.number,
            self.title,
            if self.draft { " · Draft" } else { "" },
            self.author,
            self.head,
            self.base
        )
    }
    pub fn cache_root(&self, repo: &Repository) -> PathBuf {
        // A cache namespace only; never passed to filesystem or Git operations.
        PathBuf::from(format!(
            "github://{}/{}/{}/{}/{}",
            repo.owner, repo.name, self.number, self.base_sha, self.head_sha
        ))
    }
}
pub struct Page {
    pub items: Vec<PullRequest>,
    pub more: bool,
}
pub struct PullDiff {
    pub pr: PullRequest,
    pub snapshot: PullSnapshot,
}

/// The immutable downloaded diff, indexed by file. Only headers are parsed for
/// the listing; source documents are built on demand, without another request.
pub struct PullSnapshot {
    patch: String,
    files: Vec<ChangedFile>,
    ranges: HashMap<SharedString, Range<usize>>,
}
impl PullSnapshot {
    pub fn parse(patch: String) -> Result<Self, String> {
        if patch.len() > 16 * 1024 * 1024 {
            return Err(too_large());
        }
        let starts: Vec<_> = patch
            .match_indices("diff --git ")
            .filter(|(ix, _)| *ix == 0 || patch.as_bytes()[ix - 1] == b'\n')
            .map(|(ix, _)| ix)
            .collect();
        if starts.is_empty() && !patch.trim().is_empty() {
            return Err("Couldn't read this PR: expected a Git diff.".into());
        }
        let mut files = Vec::with_capacity(starts.len());
        let mut ranges = HashMap::with_capacity(starts.len());
        for (ix, &start) in starts.iter().enumerate() {
            let range = start..starts.get(ix + 1).copied().unwrap_or(patch.len());
            let block = &patch[range.clone()];
            let header = block
                .split_once("\n@@ ")
                .map_or(block, |(header, _)| header);
            let mut parsed =
                DiffFile::parse(header).map_err(|e| format!("Couldn't read this diff: {e}"))?;
            if parsed.len() != 1 {
                return Err("Couldn't read this PR's file header.".into());
            }
            let file = parsed.pop().unwrap();
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            block.hash(&mut hash);
            files.push(ChangedFile::from_diff(&file, hash.finish()));
            if ranges.insert(file.path().clone(), range).is_some() {
                return Err("Couldn't read this PR: duplicate file path.".into());
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let snapshot = Self {
            patch,
            files,
            ranges,
        };
        if snapshot.estimated_size_bytes() > SNAPSHOT_BUDGET {
            return Err(too_large());
        }
        Ok(snapshot)
    }
    pub fn files(&self) -> &[ChangedFile] {
        &self.files
    }
    pub fn load_preview(&self, entry: &ChangedFile) -> Result<Preview, String> {
        let _span = tracing::info_span!("github_preview").entered();
        let range = self
            .ranges
            .get(&entry.path)
            .ok_or("PR file unavailable. Refresh to retry.")?;
        let file = DiffFile::parse(&self.patch[range.clone()])
            .map_err(|e| format!("Couldn't read this diff: {e}"))?
            .into_iter()
            .find(|file| file.path() == &entry.path)
            .ok_or("PR file unavailable. Refresh to retry.")?;
        let bytes = file.estimated_size_bytes();
        if bytes > 32 * 1024 * 1024 {
            return Err(too_large());
        }
        Ok(Preview {
            file,
            fingerprint: entry.revision,
            bytes,
        })
    }
    /// Prepare one bounded document for the continuous, virtualized PR viewer.
    /// Runs on the background executor and never performs network or Git I/O.
    pub fn load_document(&self) -> Result<Vec<Preview>, String> {
        self.load_document_with_budget(32 * 1024 * 1024)
    }
    fn load_document_with_budget(&self, budget: usize) -> Result<Vec<Preview>, String> {
        let mut bytes = 0;
        let mut previews = Vec::with_capacity(self.files.len());
        for entry in &self.files {
            let preview = self.load_preview(entry)?;
            bytes += preview.bytes;
            if bytes > budget {
                return Err(too_large());
            }
            previews.push(preview);
        }
        Ok(previews)
    }
    fn estimated_size_bytes(&self) -> usize {
        self.patch.capacity()
            + self.files.capacity() * std::mem::size_of::<ChangedFile>()
            + self.ranges.capacity() * std::mem::size_of::<(SharedString, Range<usize>)>()
            + self
                .files
                .iter()
                .map(|f| f.path.len() + f.original.as_ref().map_or(0, |p| p.len()))
                .sum::<usize>()
    }
}
fn too_large() -> String {
    "This PR is too large for the desktop preview. Open it in GitHub.".into()
}

const SNAPSHOT_BUDGET: usize = 32 * 1024 * 1024;

/// Separate from the parsed-preview LRU: evicting a source document must not
/// discard the only local copy of the PR and cause a full network refetch.
pub(crate) struct SnapshotCache {
    entries: LruCache<PathBuf, Arc<PullSnapshot>>,
    bytes: usize,
    budget: usize,
}
impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new(8, SNAPSHOT_BUDGET)
    }
}
impl SnapshotCache {
    fn new(capacity: usize, budget: usize) -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(capacity.max(1)).unwrap()),
            bytes: 0,
            budget,
        }
    }
    pub(crate) fn get(&mut self, root: &Path) -> Option<Arc<PullSnapshot>> {
        self.entries.get(root).cloned()
    }
    pub(crate) fn insert(&mut self, root: PathBuf, snapshot: PullSnapshot) {
        let bytes = snapshot.estimated_size_bytes();
        if bytes > self.budget {
            return;
        }
        if let Some(old) = self.entries.pop(&root) {
            self.bytes -= old.estimated_size_bytes();
        }
        while self.bytes + bytes > self.budget || self.entries.len() == self.entries.cap().get() {
            if let Some((_, old)) = self.entries.pop_lru() {
                self.bytes -= old.estimated_size_bytes();
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.put(root, Arc::new(snapshot));
    }
}
pub trait Backend: Send + Sync {
    fn list(&self, repo: &Repository, page: u32) -> Result<Page, String>;
    fn diff(&self, repo: &Repository, number: u64) -> Result<PullDiff, String>;
    fn comments(&self, _repo: &Repository, _number: u64) -> Result<Vec<ReviewComment>, String> {
        Ok(Vec::new())
    }
    fn post_comment(
        &self,
        _repo: &Repository,
        _number: u64,
        _target: &CommentTarget,
        _body: &str,
    ) -> Result<ReviewComment, String> {
        Err("Comment posting is unavailable for this backend.".into())
    }
}
pub struct OctocrabBackend;
fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("diffy-github")
                .enable_all()
                .build()
                .map_err(|_| "Couldn't start GitHub networking.".into())
        })
        .as_ref()
        .map_err(Clone::clone)
}
fn gh_program() -> Option<PathBuf> {
    which::which("gh").ok().or_else(|| {
        let mut candidates = vec![
            PathBuf::from("/opt/homebrew/bin/gh"),
            PathBuf::from("/usr/local/bin/gh"),
            PathBuf::from("/usr/bin/gh"),
        ];
        if let Some(home) = std::env::home_dir() {
            candidates.push(home.join(".local/bin/gh"));
        }
        #[cfg(windows)]
        if let Some(root) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(root).join("GitHub CLI/gh.exe"));
        }
        candidates.into_iter().find(|p| p.is_file())
    })
}
fn credential() -> Result<String, String> {
    let program =
        gh_program().ok_or("Install GitHub CLI, then run gh auth login to view pull requests.")?;
    let mut command = Command::new(program);
    command
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|_| "Couldn't start GitHub CLI. Run gh auth login, then retry.")?;
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader
        .join()
        .ok()
        .and_then(Result::ok)
        .ok_or("Couldn't read GitHub credentials. Run gh auth login, then retry.")?;
    if !status.is_some_and(|s| s.success()) || bytes.len() > 64 * 1024 {
        return Err(
            "GitHub login unavailable. Run gh auth login --hostname github.com, then retry.".into(),
        );
    }
    String::from_utf8(bytes)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Run gh auth login --hostname github.com, then retry.".into())
}
fn client(token: String) -> Result<octocrab::Octocrab, String> {
    octocrab::Octocrab::builder()
        .personal_token(token)
        .build()
        .map_err(|_| "Couldn't initialize GitHub access.".into())
}
fn error(error: octocrab::Error) -> String {
    match error {
        octocrab::Error::GitHub { source, .. } => match source.status_code.as_u16() {
            401 => "GitHub login expired. Run gh auth login --hostname github.com, then retry.".into(),
            403 => "GitHub denied access or reached a rate limit. Check gh auth status and your repository permissions, then retry.".into(),
            429 => "GitHub's rate limit was reached. Wait for it to reset, then retry.".into(),
            404 => "Repository or pull request unavailable. Check the selected remote and your GitHub access.".into(),
            406 | 422 => "GitHub couldn't provide this diff. Open the pull request in GitHub to review it.".into(),
            _ => "GitHub couldn't complete the request. Retry or open the pull request in GitHub.".into(),
        },
        _ => "Couldn't reach GitHub. Check your connection and retry.".into(),
    }
}
fn pull(pr: octocrab::models::pulls::PullRequest) -> PullRequest {
    PullRequest {
        number: pr.number,
        title: pr.title.unwrap_or_default(),
        author: pr.user.map(|u| u.login).unwrap_or_default(),
        draft: pr.draft.unwrap_or(false),
        base: pr.base.ref_field,
        head: pr.head.ref_field,
        base_sha: pr.base.sha,
        head_sha: pr.head.sha,
    }
}
impl Backend for OctocrabBackend {
    fn comments(&self, repo: &Repository, number: u64) -> Result<Vec<ReviewComment>, String> {
        comments::list(repo, number)
    }
    fn post_comment(
        &self,
        repo: &Repository,
        number: u64,
        target: &CommentTarget,
        body: &str,
    ) -> Result<ReviewComment, String> {
        comments::post(repo, number, target, body)
    }
    fn list(&self, repo: &Repository, page: u32) -> Result<Page, String> {
        let runtime = runtime()?;
        let _entered = runtime.enter();
        let client = client(credential()?)?;
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let page = client
                    .pulls(&repo.owner, &repo.name)
                    .list()
                    .state(octocrab::params::State::Open)
                    .sort(octocrab::params::pulls::Sort::Updated)
                    .direction(octocrab::params::Direction::Descending)
                    .per_page(100)
                    .page(page)
                    .send()
                    .await
                    .map_err(error)?;
                Ok(Page {
                    more: page.next.is_some(),
                    items: page.items.into_iter().map(pull).collect(),
                })
            })
            .await
            .map_err(|_| {
                "GitHub request timed out. Retry when your connection is available.".to_owned()
            })?
        })
    }
    fn diff(&self, repo: &Repository, number: u64) -> Result<PullDiff, String> {
        let runtime = runtime()?;
        let _entered = runtime.enter();
        let client = client(credential()?)?;
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let handler = client.pulls(&repo.owner, &repo.name);
                let pr = handler.get(number).await.map_err(error)?;
                let patch = handler.get_diff(number).await.map_err(error)?;
                if patch.len() > 16 * 1024 * 1024 {
                    return Err(too_large());
                }
                // Metadata and the diff are separate requests. Don't cache a newer patch
                // under old commit IDs if a force-push or base update happened in between.
                let latest = handler.get(number).await.map_err(error)?;
                if latest.base.sha != pr.base.sha || latest.head.sha != pr.head.sha {
                    return Err(
                        "This pull request changed while loading. Refresh to get the latest diff."
                            .into(),
                    );
                }
                let snapshot = PullSnapshot::parse(patch)?;
                Ok(PullDiff {
                    pr: pull(pr),
                    snapshot,
                })
            })
            .await
            .map_err(|_| {
                "GitHub request timed out. Retry when your connection is available.".to_owned()
            })?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(path: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-before\n+after\n"
        )
    }

    #[test]
    fn indexed_previews_match_full_diff_in_path_order() {
        let patch = concat!(
            "diff --git a/z.txt b/z.txt\n--- a/z.txt\n+++ b/z.txt\n@@ -1 +1 @@\n-before\n+diff --git a/not-a-file b/not-a-file\n\\ No newline at end of file\n",
            "diff --git a/old name.txt b/new name.txt\nsimilarity index 100%\nrename from old name.txt\nrename to new name.txt\n",
            "diff --git a/empty.txt b/empty.txt\nnew file mode 100644\nindex 0000000..e69de29\n",
            "diff --git a/deleted.txt b/deleted.txt\ndeleted file mode 100644\n--- a/deleted.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n",
            "diff --git a/binary.dat b/binary.dat\nnew file mode 100644\nBinary files /dev/null and b/binary.dat differ\n",
            "diff --git a/mode.sh b/mode.sh\nold mode 100644\nnew mode 100755\n",
            "diff --git a/source.txt b/copy.txt\nsimilarity index 100%\ncopy from source.txt\ncopy to copy.txt\n",
            "diff --git \"a/\\303\\251.txt\" \"b/\\303\\251.txt\"\n--- \"a/\\303\\251.txt\"\n+++ \"b/\\303\\251.txt\"\n@@ -1 +1 @@\n-old\n+new\n",
        );
        for patch in [patch.to_owned(), patch.replace('\n', "\r\n")] {
            let full = crate::repository::GroupSnapshot::parse(patch.clone()).unwrap();
            let snapshot = PullSnapshot::parse(patch).unwrap();
            assert_eq!(snapshot.files().len(), full.files.len());
            for (entry, expected) in snapshot.files().iter().zip(full.files) {
                assert_eq!(
                    entry,
                    &ChangedFile::from_diff(&expected, full.revisions[expected.path()])
                );
                let preview = snapshot.load_preview(entry).unwrap();
                assert_eq!(preview.file.path(), expected.path());
                assert_eq!(preview.file.status(), expected.status());
                assert_eq!(preview.file.original_path(), expected.original_path());
                assert_eq!(preview.file.modified_path(), expected.modified_path());
                assert_eq!(preview.file.additions(), expected.additions());
                assert_eq!(preview.file.deletions(), expected.deletions());
                assert_eq!(preview.file.is_binary(), expected.is_binary());
                assert_eq!(preview.fingerprint, entry.revision);
            }
        }
    }

    #[test]
    fn previews_are_parsed_only_when_requested() {
        let snapshot = PullSnapshot::parse(format!(
            "{}diff --git a/bad.txt b/bad.txt\n--- a/bad.txt\n+++ b/bad.txt\n@@ invalid hunk\n",
            patch("good.txt")
        ))
        .unwrap();
        assert_eq!(snapshot.files().len(), 2);
        assert!(snapshot.load_preview(&snapshot.files()[0]).is_err());
        assert!(snapshot.load_preview(&snapshot.files()[1]).is_ok());
        assert!(
            PullSnapshot::parse(String::new())
                .unwrap()
                .files()
                .is_empty()
        );
        assert!(PullSnapshot::parse("not a diff".into()).is_err());
        assert!(PullSnapshot::parse("x".repeat(16 * 1024 * 1024 + 1)).is_err());
        assert!(
            PullSnapshot::parse(format!("{}{}", patch("same.txt"), patch("same.txt"))).is_err()
        );
    }

    #[test]
    fn continuous_document_keeps_path_order_and_bounds_total_memory() {
        let snapshot =
            PullSnapshot::parse(format!("{}{}", patch("z.txt"), patch("a.txt"))).unwrap();
        let document = snapshot.load_document().unwrap();
        assert_eq!(
            document
                .iter()
                .map(|p| p.file.path().as_str())
                .collect::<Vec<_>>(),
            ["a.txt", "z.txt"]
        );
        let bytes = document.iter().map(|p| p.bytes).sum();
        assert!(snapshot.load_document_with_budget(bytes).is_ok());
        assert!(snapshot.load_document_with_budget(bytes - 1).is_err());
        assert!(
            PullSnapshot::parse(String::new())
                .unwrap()
                .load_document()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn snapshot_cache_is_bounded_by_slots_and_bytes() {
        let snapshot = || PullSnapshot::parse(patch("file.txt")).unwrap();
        let weight = snapshot().estimated_size_bytes();
        let mut cache = SnapshotCache::new(2, weight * 2);
        cache.insert("one".into(), snapshot());
        cache.insert("two".into(), snapshot());
        let retained = cache.get(Path::new("one")).unwrap();
        cache.insert("three".into(), snapshot());
        assert!(cache.get(Path::new("two")).is_none());
        assert!(cache.get(Path::new("one")).is_some());
        cache.insert("one".into(), snapshot());
        assert_eq!(cache.bytes, weight * 2);
        // A preview already parsing in the background survives LRU eviction.
        cache.insert("four".into(), snapshot());
        cache.insert("five".into(), snapshot());
        assert!(cache.get(Path::new("one")).is_none());
        assert!(retained.load_preview(&retained.files()[0]).is_ok());

        let mut cache = SnapshotCache::new(8, weight);
        cache.insert("one".into(), snapshot());
        cache.insert("two".into(), snapshot());
        assert!(cache.get(Path::new("one")).is_none());
        assert!(cache.get(Path::new("two")).is_some());
        assert_eq!(cache.bytes, weight);
    }

    #[test]
    fn snapshot_namespaces_include_repository_and_both_commits() {
        let repo = Repository::parse("https://github.com/owner/repo").unwrap();
        let mut pr = PullRequest {
            number: 1,
            title: String::new(),
            author: String::new(),
            draft: false,
            base: "main".into(),
            head: "feature".into(),
            base_sha: "base".into(),
            head_sha: "head".into(),
        };
        let root = pr.cache_root(&repo);
        pr.head_sha.push('2');
        assert_ne!(root, pr.cache_root(&repo));
        pr.head_sha.pop();
        pr.base_sha.push('2');
        assert_ne!(root, pr.cache_root(&repo));
        pr.base_sha.pop();
        let other = Repository::parse("https://github.com/owner/other").unwrap();
        assert_ne!(root, pr.cache_root(&other));
    }

    #[test]
    fn api_client_initializes_on_a_background_thread() {
        std::thread::spawn(|| {
            let runtime = runtime().unwrap();
            let _entered = runtime.enter();
            // Building Octocrab starts a Tokio task even before the first request.
            assert!(client("fixture-token".into()).is_ok());
        })
        .join()
        .unwrap();
    }
    #[test]
    fn github_remotes_are_unambiguous_and_host_scoped() {
        for url in [
            "git@github.com:hitSlop/diffy.git",
            "https://github.com/hitSlop/diffy.git",
            "ssh://git@github.com/hitSlop/diffy",
            "https://github.com/hitSlop/diffy/",
        ] {
            assert_eq!(Repository::parse(url).unwrap().label(), "hitSlop/diffy");
        }
        for url in [
            "https://example.com/hitSlop/diffy",
            "https://github.com.evil.test/a/b",
            "git@other:org/repo.git",
            "https://github.com/a/b/more",
            "https://github.com/a/..",
            "https://github.com/a/b?token=secret",
        ] {
            assert!(Repository::parse(url).is_none(), "{url}");
        }
    }
}
