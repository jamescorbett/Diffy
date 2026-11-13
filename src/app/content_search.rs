use super::{Diffy, review::ReviewView};
use crate::{
    repository::{self, ChangeGroup, ChangedFile, PreviewKey},
    search::{Match, Matcher, Results},
};
use gpui_kit::StatefulInteractiveElement as _;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, StyledExt as _,
    VirtualListScrollHandle,
    button::{Button, ButtonVariants as _},
    diff::{DiffFile, DiffLinePosition, DiffSide},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    scroll::ScrollableElement as _,
    v_flex, v_virtual_list,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, ScrollStrategy, SharedString, Styled as _, Subscription, Task, Window, div,
    px, size,
};
use std::{
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(super) struct State {
    pub open: bool,
    pub pending: Option<DiffLinePosition>,
    pub error: Option<String>,
    input: Option<Entity<InputState>>,
    query: String,
    case_sensitive: bool,
    matches: Arc<Vec<Match>>,
    selected: Option<usize>,
    loading: bool,
    truncated: bool,
    generation: u64,
    signature: Option<u64>,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    subscription: Option<Subscription>,
    scroll: VirtualListScrollHandle,
    sizes: Rc<Vec<gpui_kit::Size<gpui_kit::Pixels>>>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            open: false,
            pending: None,
            error: None,
            input: None,
            query: String::new(),
            case_sensitive: false,
            matches: Arc::default(),
            selected: None,
            loading: false,
            truncated: false,
            generation: 0,
            signature: None,
            cancel: None,
            task: None,
            subscription: None,
            scroll: VirtualListScrollHandle::new(),
            sizes: Rc::default(),
        }
    }
}
impl State {
    pub(super) fn busy(&self) -> bool {
        self.loading
    }
    pub(super) fn invalidate(&mut self) {
        self.suspend();
        self.generation += 1;
        self.signature = None;
        self.matches = Arc::default();
        self.sizes = Rc::default();
        self.selected = None;
        self.pending = None;
        self.truncated = false;
        self.error = None;
    }
    pub(super) fn suspend(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if self.loading {
            self.signature = None;
            self.generation += 1;
        }
        self.loading = false;
        self.task = None;
        self.pending = None;
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum Key {
    Local(u64, ChangeGroup, Option<PathBuf>),
    Pull(PathBuf),
}
enum Source {
    Documents(Vec<DiffFile>),
    Local {
        root: PathBuf,
        group: ChangeGroup,
        files: Arc<Vec<ChangedFile>>,
    },
}
fn signature(key: &Key, view: &ReviewView) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hash);
    view.content_search.query.hash(&mut hash);
    view.content_search.case_sensitive.hash(&mut hash);
    for file in view.files.iter() {
        file.path.hash(&mut hash);
        file.revision.hash(&mut hash);
    }
    hash.finish()
}

impl Diffy {
    fn content_search_key(&self) -> Option<Key> {
        if self.is_pr() {
            Some(Key::Pull(self.pr_state()?.cache_root.clone()?))
        } else {
            let p = self.projects.get(self.active)?;
            Some(Key::Local(p.id, p.group, p.path.clone()))
        }
    }

    pub(super) fn focus_content_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some()
            || self.palette_open
            || self.import.picker.is_some()
            || self.active_view().is_none()
        {
            return;
        }
        self.stop_file_repeat();
        if self.active_view().unwrap().content_search.input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find text in files"));
            let subscription = cx.subscribe(&input, |this, input, event, cx| {
                if this
                    .active_view()
                    .and_then(|v| v.content_search.input.as_ref())
                    != Some(&input)
                {
                    return;
                }
                match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        let state = &mut this.active_view_mut().unwrap().content_search;
                        state.query = query;
                        state.invalidate();
                        this.ensure_content_search(cx);
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. } => {
                        this.step_content_match(if *shift { -1 } else { 1 }, cx)
                    }
                    _ => {}
                }
            });
            let state = &mut self.active_view_mut().unwrap().content_search;
            state.input = Some(input);
            state.subscription = Some(subscription);
        }
        let state = &mut self.active_view_mut().unwrap().content_search;
        state.open = true;
        let input = state.input.as_ref().unwrap().clone();
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.ensure_content_search(cx);
        cx.notify();
    }

    pub(super) fn ensure_content_search(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self
            .active_view()
            .filter(|v| v.content_search.open && !v.content_search.query.is_empty())
        else {
            return;
        };
        let Some(key) = self.content_search_key() else {
            return;
        };
        let signature = signature(&key, view);
        if view.content_search.signature == Some(signature) {
            return;
        }
        let source = if self.is_pr() {
            let Some(documents) = view.search_documents() else {
                return;
            };
            Source::Documents(documents)
        } else {
            let p = &self.projects[self.active];
            if p.loading || p.refreshing {
                return;
            }
            if let Some(documents) = view.search_documents() {
                Source::Documents(documents)
            } else if let Some(root) = &p.path {
                Source::Local {
                    root: root.clone(),
                    group: p.group,
                    files: view.files.clone(),
                }
            } else {
                return;
            }
        };
        let state = &mut self.active_view_mut().unwrap().content_search;
        state.invalidate();
        let generation = state.generation;
        state.signature = Some(signature);
        state.loading = true;
        let query = state.query.clone();
        let case_sensitive = state.case_sensitive;
        let cancel = Arc::new(AtomicBool::new(false));
        state.cancel = Some(cancel.clone());
        let cache = self.preview_cache.clone();
        let task = cx.spawn(async move |owner, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let result = cx
                .background_spawn(async move {
                    let mut matcher = match Matcher::new(&query, case_sensitive) {
                        Ok(matcher) => matcher,
                        Err(error) => {
                            return Results {
                                error: Some(error),
                                ..Default::default()
                            };
                        }
                    };
                    match source {
                        Source::Documents(documents) => {
                            for file in documents {
                                if !matcher.file(&file, &cancel) {
                                    break;
                                }
                            }
                        }
                        Source::Local { root, group, files } => {
                            for entry in files.iter() {
                                if cancel.load(Ordering::Relaxed) {
                                    break;
                                }
                                let key = PreviewKey {
                                    root: root.clone(),
                                    group,
                                    path: entry.path.clone(),
                                    revision: entry.revision,
                                };
                                let cached =
                                    cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key);
                                let preview = if let Some(preview) = cached {
                                    Ok(preview)
                                } else {
                                    let _slot = repository::refresh_slot().await;
                                    if cancel.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    repository::load_preview(&root, group, entry)
                                };
                                match preview {
                                    Ok(preview) => {
                                        if !matcher.file(&preview.file, &cancel) {
                                            break;
                                        }
                                    }
                                    Err(_) => matcher.results.skipped += 1,
                                }
                            }
                        }
                    }
                    matcher.results
                })
                .await;
            let _ = owner.update(cx, |this, cx| {
                if this.content_search_key().as_ref() != Some(&key) {
                    return;
                }
                let Some(view) = this.active_view_mut() else {
                    return;
                };
                let state = &mut view.content_search;
                if state.generation != generation
                    || !state.open
                    || state.signature != Some(signature)
                {
                    return;
                }
                state.loading = false;
                state.task = None;
                state.cancel = None;
                state.truncated = result.truncated;
                state.error = result.error.or_else(|| {
                    (result.skipped > 0).then(|| {
                        format!(
                            "{} files couldn't be searched. Refresh to retry.",
                            result.skipped
                        )
                    })
                });
                state.sizes = Rc::new(vec![size(px(0.), px(66.)); result.matches.len()]);
                state.matches = Arc::new(result.matches);
                if !state.matches.is_empty() {
                    state.scroll.scroll_to_item(0, ScrollStrategy::Top);
                }
                cx.notify();
            });
        });
        self.active_view_mut().unwrap().content_search.task = Some(task);
        cx.notify();
    }

    fn jump_content_match(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(view) = self.active_view_mut() else {
            return;
        };
        let Some(found) = view.content_search.matches.get(index).cloned() else {
            return;
        };
        view.select(found.position.path().clone(), cx);
        view.content_search.selected = Some(index);
        view.content_search.pending = Some(found.position);
        view.content_search
            .scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        self.ensure_preview(cx);
        if let Some(view) = self.active_view_mut() {
            view.reveal_search_target(cx);
        }
        cx.notify();
    }
    fn step_content_match(&mut self, direction: isize, cx: &mut Context<Self>) {
        let Some(state) = self.active_view().map(|v| &v.content_search) else {
            return;
        };
        if state.matches.is_empty() {
            return;
        }
        let index = match state.selected {
            Some(index) => {
                (index as isize + direction).rem_euclid(state.matches.len() as isize) as usize
            }
            None if direction < 0 => state.matches.len() - 1,
            None => 0,
        };
        self.jump_content_match(index, cx);
    }

    pub(super) fn render_content_search(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.active_view() else {
            return div().into_any_element();
        };
        let state = &view.content_search;
        let Some(input) = &state.input else {
            return div().into_any_element();
        };
        let matches = state.matches.clone();
        let selected = state.selected;
        let owner = cx.entity().downgrade();
        let status = if state.loading {
            "Searching…".to_owned()
        } else if state.query.is_empty() {
            "Type text to search file contents".into()
        } else if self.is_pr() && !view.document_ready() && !view.files.is_empty() {
            "Waiting for PR diff…".into()
        } else {
            format!(
                "{}{} matching {}",
                matches.len(),
                if state.truncated { "+" } else { "" },
                if matches.len() == 1 && !state.truncated {
                    "line"
                } else {
                    "lines"
                }
            )
        };
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .h(px(46.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .child(
                        div()
                            .font_medium()
                            .text_size(px(14.))
                            .child("Find in files"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("back-to-file-names")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronLeft)
                            .label("Files")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.focus_file_search(window, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .gap_1()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input)
                                .id("content-search-input")
                                .small()
                                .w_full(),
                        ),
                    )
                    .child(
                        Button::new("content-search-case")
                            .ghost()
                            .small()
                            .label("Aa")
                            .selected(state.case_sensitive)
                            .tooltip("Match case")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(view) = this.active_view_mut() {
                                    view.content_search.case_sensitive =
                                        !view.content_search.case_sensitive;
                                    view.content_search.invalidate();
                                }
                                this.ensure_content_search(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.is_pr() {
                        "PR patch text · new and removed lines"
                    } else if self.selected_group() == ChangeGroup::Staged {
                        "Staged files · new and removed text"
                    } else {
                        "Changed files · new and removed text"
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .gap_1()
                    .child(
                        div()
                            .id("content-search-status")
                            .test_support()
                            .role(gpui_kit::Role::Status)
                            .aria_label(status.clone())
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .child(status),
                    )
                    .child(
                        Button::new("content-search-previous")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronUp)
                            .disabled(matches.is_empty())
                            .tooltip("Previous matching line (Shift+Enter)")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.step_content_match(-1, cx)),
                            ),
                    )
                    .child(
                        Button::new("content-search-next")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronDown)
                            .disabled(matches.is_empty())
                            .tooltip("Next matching line (Enter)")
                            .on_click(cx.listener(|this, _, _, cx| this.step_content_match(1, cx))),
                    ),
            )
            .children(state.error.as_ref().map(|error| {
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .whitespace_normal()
                    .child(error.clone())
            }))
            .when(state.truncated, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .whitespace_normal()
                        .child("Search limit reached (1,000 lines / 128 MiB). Refine your search."),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .child(
                        v_virtual_list(
                            cx.entity(),
                            "content-search-results",
                            state.sizes.clone(),
                            move |_, range, _, cx| {
                                range
                                    .map(|index| {
                                        let found = &matches[index];
                                        let owner = owner.clone();
                                        ListItem::new(SharedString::from(format!(
                                            "content-search-result-{index}"
                                        )))
                                        .h(px(66.))
                                        .w_full()
                                        .min_w_0()
                                        .px_3()
                                        .selected(selected == Some(index))
                                        .child(
                                            v_flex()
                                                .min_w_0()
                                                .gap_1()
                                                .child(
                                                    h_flex()
                                                        .min_w_0()
                                                        .gap_2()
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w_0()
                                                                .truncate()
                                                                .text_ellipsis_middle()
                                                                .font_medium()
                                                                .text_size(px(12.))
                                                                .child(
                                                                    found.position.path().clone(),
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(
                                                                    if found.position.side()
                                                                        == DiffSide::Original
                                                                    {
                                                                        cx.theme().danger
                                                                    } else {
                                                                        cx.theme().muted_foreground
                                                                    },
                                                                )
                                                                .child(format!(
                                                                    "{} {}",
                                                                    if found.position.side()
                                                                        == DiffSide::Original
                                                                    {
                                                                        "Old"
                                                                    } else {
                                                                        "New"
                                                                    },
                                                                    found.position.line()
                                                                )),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .id(SharedString::from(format!(
                                                            "content-search-snippet-{index}"
                                                        )))
                                                        .test_support()
                                                        .truncate()
                                                        .text_size(px(12.))
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child(found.snippet.clone()),
                                                ),
                                        )
                                        .on_click(move |_, _, cx| {
                                            let _ = owner.update(cx, |this, cx| {
                                                this.jump_content_match(index, cx)
                                            });
                                        })
                                        .into_any_element()
                                    })
                                    .collect()
                            },
                        )
                        .size_full()
                        .track_scroll(&state.scroll),
                    )
                    .vertical_scrollbar(&state.scroll),
            )
            .into_any_element()
    }
}

#[cfg(feature = "ui-tests")]
impl Diffy {
    pub fn content_search_busy(&self) -> bool {
        self.active_view().is_some_and(|v| v.content_search.loading)
    }
    pub fn content_search_matches(&self) -> Vec<Match> {
        self.active_view()
            .map(|v| v.content_search.matches.as_ref().clone())
            .unwrap_or_default()
    }
    pub fn content_search_selected_line(&self, cx: &gpui_kit::App) -> Option<DiffLinePosition> {
        let line = self.active_view()?.diff.read(cx).selected_lines()?;
        Some(DiffLinePosition::new(
            line.path().clone(),
            line.side(),
            line.start(),
        ))
    }
}
