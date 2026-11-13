#[path = "../benches/support/fixture.rs"]
mod fixture;
use diffy::{
    app,
    github::{
        Backend, CommentSide, CommentTarget, Page, PullDiff, PullRequest, PullSnapshot, Remote,
        Repository, ReviewComment,
    },
};
use gpui_kit::{
    AppContext as _, Bounds, HeadlessAppContext, ScrollDelta, WindowBounds, WindowOptions,
    assets::Assets,
    component::diff::{DiffLineRange, DiffSide},
    point, px, size,
    test::TestWindowExt as _,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct FakeGitHub {
    fail: AtomicBool,
    delayed: AtomicBool,
    release: AtomicBool,
    revision: AtomicUsize,
    calls: AtomicUsize,
    large: AtomicBool,
    list_calls: AtomicUsize,
    with_comments: AtomicBool,
    post_fail: AtomicBool,
    posts: Mutex<Vec<(u64, CommentTarget, String)>>,
    published: Mutex<Vec<(u64, ReviewComment)>>,
}
fn pr(number: u64, revision: usize) -> PullRequest {
    PullRequest {
        number,
        title: format!(
            "Fix refresh and keep this deliberately long pull request title readable {number}"
        ),
        author: "fixture".into(),
        draft: number == 2,
        base: "main".into(),
        head: format!("fix-{number}"),
        base_sha: "base".into(),
        head_sha: format!("head-{number}-{revision}"),
    }
}
impl Backend for FakeGitHub {
    fn comments(&self, _: &Repository, number: u64) -> Result<Vec<ReviewComment>, String> {
        if !self.with_comments.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let mut comments = vec![
            ReviewComment {
                id: 100,
                author: "reviewer".into(),
                body: "Existing inline feedback".into(),
                path: "extra-00.txt".into(),
                line: Some(1),
                side: CommentSide::Right,
                in_reply_to: None,
            },
            ReviewComment {
                id: 101,
                author: "author".into(),
                body: "Existing reply".into(),
                path: "extra-00.txt".into(),
                line: Some(1),
                side: CommentSide::Right,
                in_reply_to: Some(100),
            },
            ReviewComment {
                id: 102,
                author: "reviewer".into(),
                body: "Outdated feedback".into(),
                path: "extra-01.txt".into(),
                line: None,
                side: CommentSide::Left,
                in_reply_to: None,
            },
        ];
        comments.extend(
            self.published
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| *n == number)
                .map(|(_, c)| c.clone()),
        );
        Ok(comments)
    }
    fn post_comment(
        &self,
        _: &Repository,
        number: u64,
        target: &CommentTarget,
        body: &str,
    ) -> Result<ReviewComment, String> {
        self.posts
            .lock()
            .unwrap()
            .push((number, target.clone(), body.into()));
        std::thread::sleep(Duration::from_millis(30));
        if self.post_fail.load(Ordering::Relaxed) {
            return Err("Fixture posting denied; draft kept".into());
        }
        let comment = ReviewComment {
            id: 1000 + self.posts.lock().unwrap().len() as u64,
            author: "fixture".into(),
            body: body.into(),
            path: "extra-00.txt".into(),
            line: Some(1),
            side: CommentSide::Right,
            in_reply_to: if let CommentTarget::Reply(id) = target {
                Some(*id)
            } else {
                None
            },
        };
        self.published
            .lock()
            .unwrap()
            .push((number, comment.clone()));
        Ok(comment)
    }
    fn list(&self, repo: &Repository, page: u32) -> Result<Page, String> {
        self.list_calls.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(30));
        if self.fail.load(Ordering::Relaxed) {
            return Err("Fixture authentication failed · Retry".into());
        }
        if repo.name == "empty" {
            return Ok(Page {
                items: Vec::new(),
                more: false,
            });
        }
        Ok(Page {
            items: if page == 1 {
                vec![pr(1, 0), pr(2, 0)]
            } else {
                vec![pr(3, 0)]
            },
            more: page == 1,
        })
    }
    fn diff(&self, _: &Repository, number: u64) -> Result<PullDiff, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(30));
        if self.delayed.load(Ordering::Relaxed) && number == 2 {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !self.release.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        if self.fail.load(Ordering::Relaxed) {
            return Err("Fixture diff unavailable".into());
        }
        let rev = self.revision.load(Ordering::Relaxed);
        let mut patch = format!(
            "diff --git a/src/review.rs b/src/review.rs\n--- a/src/review.rs\n+++ b/src/review.rs\n@@ -1 +1 @@\n-local\n+PR {number} revision {rev}\ndiff --git a/z.txt b/z.txt\n--- a/z.txt\n+++ b/z.txt\n@@ -1 +1 @@\n-before\n+after\n"
        );
        if self.large.load(Ordering::Relaxed) {
            for i in 0..40 {
                patch.push_str(&format!("diff --git a/extra-{i:02}.txt b/extra-{i:02}.txt\n--- a/extra-{i:02}.txt\n+++ b/extra-{i:02}.txt\n@@ -1 +1 @@\n-before\n+after\n"));
            }
        }
        Ok(PullDiff {
            pr: pr(number, rev),
            snapshot: PullSnapshot::parse(patch)?,
        })
    }
}
fn main() {
    let repo = fixture::Fixture::new("github-ui");
    repo.write("local.txt", "base\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "base"]);
    repo.write("local.txt", "local change\n");
    let index = std::fs::read(repo.0.join(".git/index")).unwrap();
    let backend = Arc::new(FakeGitHub::default());
    std::fs::create_dir_all("target/previews").unwrap();
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        Arc::new(Assets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        app::init(cx);
        cx.set_reduce_motion(true);
    });
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Default::default(),
                        size: size(px(1000.), px(760.)),
                    })),
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        app::Diffy::with_options(vec![repo.0.clone()], false, None, window, cx)
                    })
                },
            )
        })
        .unwrap();
    macro_rules! wait {
        () => {{
            for _ in 0..500 {
                cx.advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                let done = cx
                    .update_window(handle, |_, w, cx| {
                        w.render_frame(cx);
                        !view.read(cx).is_busy()
                            && view.read(cx).worktrees_ready()
                            && !view.read(cx).github_busy()
                    })
                    .unwrap();
                if done {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }};
    }
    wait!();
    let local_selected = cx.update(|cx| view.read(cx).selected_file().cloned());
    let remote = Remote {
        name: "origin".into(),
        repository: Repository {
            owner: "fixture".into(),
            name: "project".into(),
        },
    };
    cx.update_window(handle, |_, w, cx| {
        view.update(cx, |v, cx| {
            v.set_github_fixture(
                vec![
                    remote,
                    Remote {
                        name: "upstream".into(),
                        repository: Repository {
                            owner: "fixture".into(),
                            name: "empty".into(),
                        },
                    },
                ],
                backend.clone(),
                w,
                cx,
            )
        });
        w.render_frame(cx);
        w.click("source-prs", cx);
        w.render_frame(cx);
        assert_eq!(
            w.find("pr-loading-status").label(),
            Some("Loading pull requests…")
        );
        assert!(w.try_find("pr-loading-empty").is_some());
        w.click("refresh", cx);
        w.click("refresh", cx);
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save("target/previews/diffy-github-loading.png")
        .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_selected(), Some(1));
        assert_eq!(
            backend.list_calls.load(Ordering::Relaxed),
            1,
            "Repeated refresh clicks must not duplicate the initial list request"
        );
        assert!(w.try_find("pr-loading-status").is_none());
        assert!(w.try_find("preview-header").is_none());
        assert!(w.try_find("pr-file-header:src/review.rs").is_some());
        assert!(w.try_find("pr-file-header:z.txt").is_some());
        assert!(
            w.find("pr-file-header:src/review.rs").bounds().size.height >= px(32.),
            "File separators should have a distinct padded header band"
        );
        assert_eq!(view.read(cx).github_document_paths(cx).len(), 2);
        let parses = view.read(cx).preview_request_count();
        w.click(("file-name", 1usize), cx);
        w.render_frame(cx);
        assert_eq!(view.read(cx).selected_file().unwrap().as_ref(), "z.txt");
        assert_eq!(view.read(cx).github_document_paths(cx).len(), 2);
        w.click(("file-name", 0usize), cx);
        w.render_frame(cx);
        assert_eq!(view.read(cx).preview_request_count(), parses);
        assert_eq!(
            view.read(cx).selected_file().unwrap().as_ref(),
            "src/review.rs"
        );
        assert!(w.try_find("stage-file").is_none());
        assert!(w.try_find("stage-all").is_none());
        assert!(w.try_find(("file-action", 0usize)).is_none());
        assert!(w.try_find("staged").is_none());
        w.press("secondary-s", cx);
        w.dispatch_action(Box::new(app::StageAll), cx);
        view.update(cx, |v, cx| v.probe_pr_writes(cx));
        w.render_frame(cx);
    })
    .unwrap();
    assert_eq!(
        std::fs::read(repo.0.join(".git/index")).unwrap(),
        index,
        "Every PR staging entry point preserves the local index"
    );
    std::fs::create_dir_all("target/previews").unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save("target/previews/diffy-github-pr.png")
        .unwrap();
    cx.update_window(handle, |_, w, cx| {
        w.click("pr-overflow", cx);
        w.render_frame(cx);
    })
    .unwrap();
    for _ in 0..12 {
        cx.advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
    }
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        w.click("pr-search", cx);
        w.input("#2", cx);
        w.render_frame(cx);
        assert!(w.try_find("choose-pr-1").is_none());
        w.click("choose-pr-2", cx);
        w.render_frame(cx);
        assert_eq!(w.find("pr-loading-status").label(), Some("Loading diff…"));
        assert!(w.try_find("pr-loading-empty").is_some());
    })
    .unwrap();
    wait!();
    let calls = backend.calls.load(Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.click("source-prs", cx);
        w.render_frame(cx);
        w.dispatch_action(Box::new(app::PreviousWorktree), cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_selected(), Some(1));
        w.dispatch_action(Box::new(app::NextWorktree), cx);
    })
    .unwrap();
    wait!();
    assert_eq!(
        backend.calls.load(Ordering::Relaxed),
        calls,
        "Returning to a visited PR should reuse its commit-keyed snapshot"
    );
    cx.update_window(handle, |_, w, cx| {
        assert_eq!(view.read(cx).github_selected(), Some(2));
        w.click("source-worktrees", cx);
        w.render_frame(cx);
        assert!(
            view.read(cx).github_document_paths(cx).is_empty(),
            "Inactive PR source documents must be released"
        );
        assert_eq!(view.read(cx).selected_file().cloned(), local_selected);
        assert!(w.try_find("stage-all").is_some());
        // Editing a local file while PR mode is active must not replace the PR snapshot.
        w.click("source-prs", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    repo.write("local.txt", "another local edit\n");
    for _ in 0..8 {
        cx.advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(5));
    }
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_selected(), Some(2));
        assert_eq!(
            view.read(cx).selected_file().unwrap().as_ref(),
            "src/review.rs"
        );
        // Load the next page through the overflow menu.
        w.click("pr-overflow", cx);
        w.render_frame(cx);
    })
    .unwrap();
    for _ in 0..12 {
        cx.advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
    }
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        w.click("pr-search", cx);
        w.press("secondary-a", cx);
        w.press("backspace", cx);
        w.render_frame(cx);
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save("target/previews/diffy-github-menu.png")
        .unwrap();
    cx.update_window(handle, |_, w, cx| {
        w.click("pr-load-more", cx);
        w.render_frame(cx);
        assert_eq!(w.find("pr-load-more").label(), Some("Loading more…"));
        assert_eq!(w.find("pr-loading-status").label(), Some("Loading more…"));
        assert_eq!(
            view.read(cx).selected_preview_path().unwrap().as_ref(),
            "src/review.rs"
        );
        w.click("pr-load-more", cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.try_find("choose-pr-3").is_some());
        w.click("choose-pr-3", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    let calls = backend.calls.load(Ordering::Relaxed);
    backend.revision.store(1, Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.click("refresh", cx);
        w.render_frame(cx);
        assert_eq!(
            w.find("pr-loading-status").label(),
            Some("Refreshing pull requests…")
        );
        assert_eq!(view.read(cx).github_selected(), Some(3));
        assert_eq!(
            view.read(cx).selected_preview_path().unwrap().as_ref(),
            "src/review.rs"
        );
        assert!(w.try_find("pr-loading-empty").is_none());
        w.click("refresh", cx);
        w.dispatch_action(Box::new(app::Refresh), cx);
    })
    .unwrap();
    wait!();
    assert_eq!(
        backend.calls.load(Ordering::Relaxed),
        calls + 1,
        "Explicit refresh must fetch a fresh snapshot even when the PR is cached"
    );
    cx.update_window(handle, |_, w, cx| {
        assert_eq!(view.read(cx).github_selected(), Some(3));
        w.render_frame(cx);
    })
    .unwrap();
    // Start a delayed response, then leave PR mode before it completes.
    backend.delayed.store(true, Ordering::Relaxed);
    let calls = backend.calls.load(Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.dispatch_action(Box::new(app::NextWorktree), cx);
        w.dispatch_action(Box::new(app::NextWorktree), cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        assert_eq!(view.read(cx).github_selected(), Some(2));
        w.render_frame(cx);
        w.click("refresh", cx);
    })
    .unwrap();
    for _ in 0..500 {
        cx.run_until_parked();
        if backend.calls.load(Ordering::Relaxed) > calls {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(backend.calls.load(Ordering::Relaxed), calls + 1);
    cx.update_window(handle, |_, w, cx| {
        w.click("source-worktrees", cx);
        w.render_frame(cx);
        assert!(
            w.try_find("pr-loading-status").is_none(),
            "Cancelled PR requests must not leave a loading indicator in Worktrees"
        );
    })
    .unwrap();
    backend.release.store(true, Ordering::Relaxed);
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).selected_file().cloned(), local_selected);
        backend.fail.store(true, Ordering::Relaxed);
        w.click("source-prs", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.click("refresh", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.try_find("retry").is_some());
        assert!(
            w.try_find("pr-loading-status").is_none(),
            "Errors must clear loading feedback"
        );
        backend.fail.store(false, Ordering::Relaxed);
        w.click("retry", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    // Remote changes clear the old PR. A PR exceeding the parsed-preview LRU's
    // capacity must remain fully browsable without another GitHub request.
    cx.update_window(handle, |_, w, cx| {
        w.click("pr-overflow", cx);
        w.render_frame(cx);
    })
    .unwrap();
    for _ in 0..12 {
        cx.advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
    }
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        w.click("github-remote-upstream", cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_selected(), None);
        assert_eq!(view.read(cx).selected_file(), None);
        backend.large.store(true, Ordering::Relaxed);
        w.click("github-remote-origin", cx);
    })
    .unwrap();
    wait!();
    let calls = backend.calls.load(Ordering::Relaxed);
    let parses = cx.update(|cx| view.read(cx).preview_request_count());
    backend.fail.store(true, Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_document_paths(cx).len(), 42);
        assert!(w.try_find("pr-file-header:extra-00.txt").is_some());
        assert!(w.try_find("pr-file-header:extra-01.txt").is_some());
        w.press("escape", cx);
        w.scroll(
            "diff-body",
            ScrollDelta::Pixels(point(px(0.), px(-900.))),
            cx,
        );
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    let reading_file = cx.update(|cx| view.read(cx).github_reading_file(cx).unwrap());
    assert_ne!(
        reading_file.as_ref(),
        "extra-00.txt",
        "Wheel scrolling must cross file boundaries"
    );
    cx.update_window(handle, |_, w, cx| {
        w.click("split", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        assert_eq!(
            view.read(cx).github_reading_file(cx).as_ref(),
            Some(&reading_file)
        );
        w.click("source-worktrees", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        assert!(view.read(cx).github_document_paths(cx).is_empty());
        w.click("source-prs", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    let parses = cx.update(|cx| {
        assert_eq!(
            view.read(cx).github_reading_file(cx).as_ref(),
            Some(&reading_file)
        );
        assert_eq!(view.read(cx).preview_request_count(), parses + 1);
        view.read(cx).preview_request_count()
    });
    // Unmeasured header heights are estimates. Continue the wheel gesture as
    // virtualized rows settle, rather than assuming one jump measures the tail.
    for _ in 0..4 {
        cx.update_window(handle, |_, w, cx| {
            w.scroll(
                "diff-body",
                ScrollDelta::Pixels(point(px(0.), px(-100000.))),
                cx,
            );
            w.render_frame(cx);
        })
        .unwrap();
        wait!();
        if cx
            .update_window(handle, |_, w, _| {
                w.try_find("pr-file-header:z.txt").is_some()
            })
            .unwrap()
        {
            break;
        }
    }
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(
            w.try_find("pr-file-header:z.txt").is_some(),
            "One scrollbar must reach the final PR file"
        );
        w.scroll(
            "diff-body",
            ScrollDelta::Pixels(point(px(0.), px(100000.))),
            cx,
        );
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.capture_screenshot(handle)
        .unwrap()
        .save("target/previews/diffy-github-continuous.png")
        .unwrap();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(
            view.read(cx).selected_preview_path().unwrap().as_ref(),
            "extra-00.txt"
        );
        w.press("escape", cx);
        w.dispatch_action(Box::new(app::NextFile), cx);
    })
    .unwrap();
    wait!();
    for _ in 0..8 {
        cx.run_until_parked();
        cx.update_window(handle, |_, w, cx| {
            w.render_frame(cx);
            assert_eq!(
                view.read(cx).selected_preview_path().unwrap().as_ref(),
                "extra-01.txt"
            );
        })
        .unwrap();
    }
    assert_eq!(
        backend.calls.load(Ordering::Relaxed),
        calls,
        "Selecting an uncached file must not refetch the PR"
    );
    for direction in [1, -1] {
        let steps = if direction == 1 { 40 } else { 41 };
        for _ in 0..steps {
            cx.update_window(handle, |_, w, cx| {
                w.dispatch_action(
                    if direction == 1 {
                        Box::new(app::NextFile)
                    } else {
                        Box::new(app::PreviousFile)
                    },
                    cx,
                );
            })
            .unwrap();
            wait!();
            cx.update(|cx| {
                let v = view.read(cx);
                assert_eq!(v.selected_preview_path(), v.selected_file());
                assert!(v.selected_preview_path().is_some());
            });
        }
    }
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).selected_preview_path().unwrap().as_ref(),
            "extra-00.txt"
        );
    });
    // Repeated file jumps must retain the same document, not reset or reparse it.
    cx.update_window(handle, |_, w, cx| {
        for _ in 0..35 {
            w.dispatch_action(Box::new(app::NextFile), cx);
        }
        for _ in 0..35 {
            w.dispatch_action(Box::new(app::PreviousFile), cx);
        }
    })
    .unwrap();
    wait!();
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).selected_preview_path().unwrap().as_ref(),
            "extra-00.txt"
        );
    });
    assert_eq!(
        backend.calls.load(Ordering::Relaxed),
        calls,
        "Traversing all 42 files and revisiting evicted previews must work offline"
    );
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).preview_request_count(),
            parses,
            "Scrolling and tree navigation must not rebuild the PR document"
        );
        assert_eq!(view.read(cx).github_document_paths(cx).len(), 42);
    });
    backend.fail.store(false, Ordering::Relaxed);
    backend.revision.store(2, Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.click("refresh", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).preview_request_count(),
            parses + 1,
            "Refreshing a changed, unselected file must replace the whole scrolling document"
        );
        assert_eq!(view.read(cx).github_document_paths(cx).len(), 42);
        assert_eq!(
            view.read(cx).selected_file().unwrap().as_ref(),
            "extra-00.txt"
        );
    });
    assert_eq!(std::fs::read(repo.0.join(".git/index")).unwrap(), index);
    // Read existing threads and write only after an explicit submit click.
    backend.with_comments.store(true, Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.click("reload-pr-comments", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.try_find("pr-thread-100").is_some());
        assert!(w.try_find("pr-comment-101").is_some());
        assert!(backend.posts.lock().unwrap().is_empty());
        w.within(("diff-file", 0usize))
            .within(("new", 0usize))
            .hover(("line", 0usize), cx);
        w.render_frame(cx);
        w.within(("diff-file", 0usize))
            .within(("new", 0usize))
            .click(("add-annotation", 0usize), cx);
        w.render_frame(cx);
        assert!(w.try_find("pr-draft-1").is_some());
        w.click("pr-comment-input-1", cx);
        w.input("New inline feedback", cx);
        w.render_frame(cx);
    })
    .unwrap();
    assert!(
        backend.posts.lock().unwrap().is_empty(),
        "Typing must never post a comment"
    );
    cx.capture_screenshot(handle)
        .unwrap()
        .save("target/previews/diffy-github-comment-draft.png")
        .unwrap();
    cx.update_window(handle, |_, w, cx| {
        w.click("pr-submit-comment-1", cx);
        w.render_frame(cx);
        w.click("pr-submit-comment-1", cx);
    })
    .unwrap();
    wait!();
    assert_eq!(
        backend.posts.lock().unwrap().len(),
        1,
        "Repeated submit clicks cannot duplicate a write"
    );
    {
        let posts = backend.posts.lock().unwrap();
        assert_eq!(posts[0].0, 1);
        assert_eq!(posts[0].2, "New inline feedback");
        let CommentTarget::Line(anchor) = &posts[0].1 else {
            panic!("Expected a line comment")
        };
        assert_eq!(
            (anchor.path.as_str(), anchor.side, anchor.start, anchor.end),
            ("extra-00.txt", CommentSide::Right, 1, 1)
        );
        assert_eq!(anchor.head_sha, "head-1-2");
    }
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.try_find("pr-draft-1").is_none());
        assert!(w.try_find("pr-comment-1001").is_some());
        w.click("pr-reply-100", cx);
        w.render_frame(cx);
        w.click("pr-comment-input-2", cx);
        w.input("A reply", cx);
        w.render_frame(cx);
        w.click("pr-submit-comment-2", cx);
    })
    .unwrap();
    wait!();
    assert_eq!(
        backend.posts.lock().unwrap()[1].1,
        CommentTarget::Reply(100)
    );
    backend.post_fail.store(true, Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        w.click("pr-reply-100", cx);
        w.render_frame(cx);
        w.click("pr-comment-input-3", cx);
        w.input("Keep this failed draft", cx);
        w.render_frame(cx);
        w.click("pr-submit-comment-3", cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.try_find("pr-comment-error").is_some());
        assert!(w.try_find("pr-draft-3").is_some());
        assert_eq!(
            view.read(cx).github_draft_body(3, cx).as_deref(),
            Some("Keep this failed draft")
        );
        w.click("source-worktrees", cx);
        w.render_frame(cx);
        assert!(w.try_find("pr-comment-status").is_none());
        w.click("source-prs", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(
            w.try_find("pr-draft-3").is_some(),
            "Leaving PR mode must retain failed drafts"
        );
        w.dispatch_action(Box::new(app::NextWorktree), cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_selected(), Some(2));
        assert!(
            w.try_find("pr-draft-3").is_none(),
            "Drafts must never leak into another PR"
        );
        w.dispatch_action(Box::new(app::PreviousWorktree), cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(view.read(cx).github_selected(), Some(1));
        assert_eq!(
            view.read(cx).github_draft_body(3, cx).as_deref(),
            Some("Keep this failed draft")
        );
        w.scroll(
            "diff-body",
            ScrollDelta::Pixels(point(px(0.), px(-220.))),
            cx,
        );
        w.render_frame(cx);
        w.click("pr-cancel-comment-3", cx);
        w.render_frame(cx);
        assert!(w.try_find("pr-draft-3").is_none());
    })
    .unwrap();
    assert_eq!(
        backend.posts.lock().unwrap().len(),
        3,
        "Navigation never retries a failed post"
    );
    cx.update_window(handle, |_, w, cx| {
        w.click(("file-name", 0usize), cx);
        view.update(cx, |v, cx| {
            v.start_inline_comment(
                &DiffLineRange::new("extra-00.txt", DiffSide::Original, 1, 1),
                w,
                cx,
            )
        });
        w.render_frame(cx);
        w.click("pr-comment-input-4", cx);
        w.input("Keep this draft across a force push", cx);
        w.render_frame(cx);
    })
    .unwrap();
    backend.revision.store(3, Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.click("refresh", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(
            view.read(cx).github_draft_body(4, cx).as_deref(),
            Some("Keep this draft across a force push")
        );
        w.click("pr-submit-comment-4", cx);
        w.render_frame(cx);
        w.click("pr-cancel-comment-4", cx);
        w.render_frame(cx);
        w.click(("file-name", 1usize), cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(
            w.try_find("pr-thread-102").is_some(),
            "Outdated comments remain readable at their file header"
        );
    })
    .unwrap();
    assert_eq!(
        backend.posts.lock().unwrap().len(),
        3,
        "A changed head must block stale inline writes"
    );
    // Content search uses cached PR source, not filenames, local disk, or comments.
    let calls = backend.calls.load(Ordering::Relaxed);
    cx.update_window(handle, |_, w, cx| {
        w.click("find-in-files", cx);
        w.render_frame(cx);
        w.click("content-search-input", cx);
        w.input("AFTER", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert_eq!(
            view.read(cx).content_search_matches().len(),
            41,
            "status={:?}, files={}",
            w.find("content-search-status").label(),
            view.read(cx).github_document_paths(cx).len()
        );
    })
    .unwrap();
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-case", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    assert!(cx.update(|cx| view.read(cx).content_search_matches().is_empty()));
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-case", cx);
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("PR 1 revision 3", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        assert_eq!(view.read(cx).content_search_matches().len(), 1);
        w.click("content-search-result-0", cx);
        w.render_frame(cx);
        let position = view.read(cx).content_search_selected_line(cx).unwrap();
        assert_eq!(
            (position.path().as_str(), position.side(), position.line()),
            ("src/review.rs", DiffSide::Modified, 1)
        );
        assert_eq!(
            view.read(cx).selected_preview_path().unwrap().as_ref(),
            "src/review.rs"
        );
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save("target/previews/diffy-github-content-search.png")
        .unwrap();
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("Existing inline feedback", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    assert!(
        cx.update(|cx| view.read(cx).content_search_matches().is_empty()),
        "Inline comment text is not file source"
    );
    cx.update_window(handle, |_, w, cx| {
        w.click("back-to-file-names", cx);
        w.render_frame(cx);
        w.click("file-search", cx);
        w.input("src", cx);
        w.render_frame(cx);
        w.click("find-in-files", cx);
        w.render_frame(cx);
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("before", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| {
        let hits = view.read(cx).content_search_matches();
        assert_eq!(
            hits.len(),
            41,
            "Filename filtering must not restrict content search"
        );
        assert!(hits.iter().all(|h| h.position.side() == DiffSide::Original));
    });
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-input", cx);
        w.press("enter", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| {
        assert_eq!(
            view.read(cx)
                .content_search_selected_line(cx)
                .unwrap()
                .side(),
            DiffSide::Original
        )
    });
    assert_eq!(
        backend.calls.load(Ordering::Relaxed),
        calls,
        "Searching PR text must not make more diff requests"
    );
    cx.update_window(handle, |_, w, cx| {
        w.dispatch_action(Box::new(app::NextWorktree), cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).content_search_matches().len(),
            1,
            "PR changes must discard the previous PR's matches"
        )
    });
    cx.update_window(handle, |_, w, cx| {
        w.dispatch_action(Box::new(app::PreviousWorktree), cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| assert_eq!(view.read(cx).content_search_matches().len(), 41));
    assert_eq!(
        backend.posts.lock().unwrap().len(),
        3,
        "Searching and jumping must not publish comments"
    );
    assert_eq!(std::fs::read(repo.0.join(".git/index")).unwrap(), index);
    println!(
        "GitHub PR header, search, pagination, read-only actions, local restoration, stale responses, and retry passed."
    );
}
