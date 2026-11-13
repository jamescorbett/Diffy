#[path = "../benches/support/fixture.rs"]
mod fixture;
use diffy::{app, repository::ChangeGroup};
use gpui_kit::{
    AppContext as _, HeadlessAppContext, WindowOptions, assets::Assets, component::diff::DiffSide,
    test::TestWindowExt as _,
};
use std::{sync::Arc, time::Duration};

fn main() {
    let repo = fixture::Fixture::new("content-search");
    repo.write("a.txt", "unchanged context café\nold needle\n");
    repo.write("not-changed.txt", "secret needle\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "base"]);
    repo.write("a.txt", "unchanged context café\nstaged needle\n");
    repo.git(&["add", "a.txt"]);
    repo.write("a.txt", "unchanged context café\nworking needle\n");
    repo.write("new.txt", "needle in an untracked file\n");
    repo.write("binary.bin", "needle\0binary\n");
    let index = std::fs::read(repo.0.join(".git/index")).unwrap();
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
            let mut done = false;
            for _ in 0..500 {
                cx.advance_clock(Duration::from_millis(16));
                cx.run_until_parked();
                done = cx
                    .update_window(handle, |_, w, cx| {
                        w.render_frame(cx);
                        view.read(cx).live_ready() && !view.read(cx).is_busy()
                    })
                    .unwrap();
                if done {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(done, "Search/preview didn't finish");
        }};
    }
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.press("secondary-shift-f", cx);
        w.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        w.click("content-search-input", cx);
        w.input("needle", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        let hits = view.read(cx).content_search_matches();
        assert_eq!(
            hits.len(),
            3,
            "Search changed files, not just the selected preview or all repository files"
        );
        assert!(hits.iter().any(|h| h.position.path().as_str() == "new.txt"));
        assert!(
            !hits
                .iter()
                .any(|h| h.position.path().as_str() == "not-changed.txt")
        );
        w.click("content-search-result-2", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        let hit = view.read(cx).content_search_selected_line(cx).unwrap();
        assert_eq!(
            (hit.path().as_str(), hit.side(), hit.line()),
            ("new.txt", DiffSide::Modified, 1),
            "An uncached result must be revealed after its preview loads"
        );
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("CAFÉ", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    assert_eq!(
        cx.update(|cx| view.read(cx).content_search_matches().len()),
        1,
        "Unchanged context should match once, with Unicode case-insensitive search"
    );
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
        w.input("staged needle", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-result-0", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| {
        let hit = view.read(cx).content_search_selected_line(cx).unwrap();
        assert_eq!(
            (hit.path().as_str(), hit.side(), hit.line()),
            ("a.txt", DiffSide::Original, 2)
        );
    });
    cx.update_window(handle, |_, w, cx| {
        w.dispatch_action(Box::new(app::ToggleGroup), cx);
    })
    .unwrap();
    wait!();
    cx.update(|cx| assert_eq!(view.read(cx).selected_group(), ChangeGroup::Staged));
    cx.update_window(handle, |_, w, cx| {
        w.click("find-in-files", cx);
        w.render_frame(cx);
        w.click("content-search-input", cx);
        w.input("working needle", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    assert!(
        cx.update(|cx| view.read(cx).content_search_matches().is_empty()),
        "Staged search must not read working-tree content"
    );
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("staged needle", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    assert_eq!(
        cx.update(|cx| view.read(cx).content_search_matches().len()),
        1
    );
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).content_search_matches()[0].position.side(),
            DiffSide::Modified
        )
    });
    cx.update_window(handle, |_, w, cx| {
        w.dispatch_action(Box::new(app::ToggleGroup), cx);
    })
    .unwrap();
    wait!();
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("working needle", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    repo.write("a.txt", "new working needle here\n");
    let mut refreshed = false;
    for _ in 0..500 {
        cx.advance_clock(Duration::from_millis(16));
        cx.run_until_parked();
        refreshed = cx
            .update_window(handle, |_, w, cx| {
                w.render_frame(cx);
                view.read(cx)
                    .content_search_matches()
                    .iter()
                    .any(|h| h.position.line() == 1 && h.snippet.contains("new working needle"))
            })
            .unwrap();
        if refreshed {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        refreshed,
        "Watcher changes must invalidate old content matches"
    );
    cx.update_window(handle, |_, w, cx| {
        w.click("content-search-input", cx);
        w.press("secondary-a", cx);
        w.input("needle", cx);
        w.press("secondary-a", cx);
        w.input("no_such_content", cx);
        w.render_frame(cx);
    })
    .unwrap();
    wait!();
    assert!(
        cx.update(|cx| view.read(cx).content_search_matches().is_empty()),
        "Superseded searches must not install stale matches"
    );
    cx.update_window(handle, |_, w, cx| {
        w.press("secondary-f", cx);
        w.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, w, cx| {
        w.render_frame(cx);
        assert!(w.try_find("file-search").is_some());
        assert!(w.try_find("content-search-input").is_none());
    })
    .unwrap();
    assert_eq!(
        std::fs::read(repo.0.join(".git/index")).unwrap(),
        index,
        "Content searches must never write the index"
    );
    println!(
        "Text search scopes, Unicode, removed lines, stale responses, live edits, and filename search passed."
    );
}
