//! Published inline threads and session-local drafts, scoped to repository/PR.
//! Only an explicit Add comment/Reply click can invoke a write on the backend.
use super::Diffy;
use crate::github::{CommentAnchor, CommentSide, CommentTarget, Repository, ReviewComment};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    diff::{Diff, DiffAnnotation, DiffLinePosition, DiffLineRange},
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, Window, div, px,
};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ReviewKey {
    repository: Repository,
    number: u64,
}
#[derive(Default)]
pub(super) struct Runtime {
    states: HashMap<ReviewKey, State>,
    next_draft: u64,
}
#[derive(Default)]
struct State {
    comments: Vec<ReviewComment>,
    just_posted: HashSet<u64>,
    loaded: bool,
    loading: bool,
    error: Option<String>,
    generation: u64,
    revision: Option<std::path::PathBuf>,
    task: Option<Task<()>>,
    drafts: Vec<Draft>,
}
struct Draft {
    id: u64,
    target: CommentTarget,
    path: String,
    line: Option<u32>,
    side: CommentSide,
    input: Entity<TextareaState>,
    sending: bool,
    error: Option<String>,
    task: Option<Task<()>>,
    _subscription: Subscription,
}
#[derive(Clone, Copy)]
enum Item {
    Thread(u64),
    Draft(u64),
}

impl State {
    fn install_comments(&mut self, mut comments: Vec<ReviewComment>) {
        let received: HashSet<_> = comments.iter().map(|c| c.id).collect();
        self.just_posted.retain(|id| !received.contains(id));
        comments.extend(
            self.comments
                .iter()
                .filter(|c| self.just_posted.contains(&c.id))
                .cloned(),
        );
        comments.sort_by_key(|c| c.id);
        comments.dedup_by_key(|c| c.id);
        self.comments = comments;
        self.loaded = true;
    }
}

fn threads(comments: &[ReviewComment]) -> BTreeMap<u64, Vec<&ReviewComment>> {
    let mut groups: BTreeMap<u64, Vec<&ReviewComment>> = BTreeMap::new();
    for comment in comments {
        groups
            .entry(comment.in_reply_to.unwrap_or(comment.id))
            .or_default()
            .push(comment);
    }
    for group in groups.values_mut() {
        group.sort_by_key(|c| c.id);
    }
    groups
}

fn thread_resolution(id: u64, comments: &[&ReviewComment]) -> Option<bool> {
    comments
        .iter()
        .find(|c| c.id == id)
        .or_else(|| comments.first())
        .and_then(|c| c.resolved)
}

impl Diffy {
    fn comment_key(&self) -> Option<ReviewKey> {
        let state = self.pr_state().filter(|s| s.remote_mode)?;
        Some(ReviewKey {
            repository: state.repository.clone()?,
            number: state.selected?,
        })
    }

    pub(super) fn ensure_pr_comments(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(key) = self.comment_key() else {
            return;
        };
        let Some(revision) = self
            .pr_state()
            .filter(|s| !s.loading)
            .and_then(|s| s.cache_root.clone())
        else {
            return;
        };
        let state = self.github.comments.states.entry(key.clone()).or_default();
        let same_revision = state.revision.as_ref() == Some(&revision);
        if (state.loading && same_revision)
            || (!force && same_revision && (state.loaded || state.error.is_some()))
        {
            return;
        }
        if !same_revision {
            // A new patch may move every anchor. Until the matching read ends,
            // keep existing threads readable at file headers, not on stale lines.
            for comment in &mut state.comments {
                comment.line = None;
            }
        }
        state.revision = Some(revision);
        state.generation += 1;
        let generation = state.generation;
        state.loading = true;
        state.error = None;
        let backend = self.github.backend.clone();
        state.task = Some(cx.spawn(async move |owner, cx| {
            let request = key.clone();
            let result = cx
                .background_spawn(
                    async move { backend.comments(&request.repository, request.number) },
                )
                .await;
            let _ = owner.update(cx, |this, cx| {
                let Some(state) = this.github.comments.states.get_mut(&key) else {
                    return;
                };
                if state.generation != generation {
                    return;
                }
                state.loading = false;
                state.task = None;
                match result {
                    Ok(comments) => state.install_comments(comments),
                    Err(error) => state.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn begin_pr_comment(
        &mut self,
        range: &DiffLineRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.comment_key() else {
            return;
        };
        let Some(state) = self
            .pr_state()
            .filter(|s| !s.loading && !s.listing && s.view.document_ready())
        else {
            return;
        };
        let Some(pr) = state.prs.iter().find(|p| p.number == key.number) else {
            return;
        };
        let anchor =
            match CommentAnchor::from_range(range, pr.base_sha.clone(), pr.head_sha.clone()) {
                Ok(anchor) => anchor,
                Err(error) => {
                    self.github.comments.states.entry(key).or_default().error = Some(error);
                    cx.notify();
                    return;
                }
            };
        let supplied = state
            .view
            .diff
            .read(cx)
            .files()
            .iter()
            .find(|f| f.path().as_str() == anchor.path)
            .is_some_and(|file| {
                anchor.end - anchor.start < 1000
                    && (anchor.start..=anchor.end)
                        .all(|line| file.contains_line(anchor.side.diff_side(), line as usize))
            });
        if !supplied {
            self.github.comments.states.entry(key).or_default().error =
                Some("Select up to 1,000 supplied diff lines on a single side to comment.".into());
            cx.notify();
            return;
        }
        self.new_comment_draft(
            key,
            CommentTarget::Line(anchor.clone()),
            anchor.path,
            Some(anchor.end),
            anchor.side,
            window,
            cx,
        );
    }

    fn begin_pr_reply(
        &mut self,
        key: ReviewKey,
        root: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.comment_key().as_ref() != Some(&key) {
            return;
        }
        let Some(comment) = self
            .github
            .comments
            .states
            .get(&key)
            .and_then(|s| s.comments.iter().find(|c| c.id == root))
            .cloned()
        else {
            return;
        };
        self.new_comment_draft(
            key,
            CommentTarget::Reply(root),
            comment.path,
            comment.line,
            comment.side,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn new_comment_draft(
        &mut self,
        key: ReviewKey,
        target: CommentTarget,
        path: String,
        line: Option<u32>,
        side: CommentSide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.github.comments.states.entry(key).or_default();
        if let Some(draft) = state.drafts.iter().find(|d| d.target == target) {
            draft.input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        self.github.comments.next_draft += 1;
        let id = self.github.comments.next_draft;
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Write an inline comment… Markdown supported on GitHub")
                .rows(4)
        });
        let subscription = cx.subscribe(&input, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        state.drafts.push(Draft {
            id,
            target,
            path,
            line,
            side,
            input,
            sending: false,
            error: None,
            task: None,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn submit_pr_comment(&mut self, key: ReviewKey, id: u64, cx: &mut Context<Self>) {
        // A queued UI event must not post a comment after switching to another PR.
        if self.comment_key().as_ref() != Some(&key) {
            return;
        }
        let Some(pr_state) = self.pr_state().filter(|s| !s.loading && !s.listing) else {
            return;
        };
        let Some(pr) = pr_state
            .prs
            .iter()
            .find(|p| p.number == key.number)
            .cloned()
        else {
            return;
        };
        let Some(draft) = self
            .github
            .comments
            .states
            .get_mut(&key)
            .and_then(|s| s.drafts.iter_mut().find(|d| d.id == id))
        else {
            return;
        };
        if draft.sending {
            return;
        }
        if let CommentTarget::Line(anchor) = &draft.target
            && (anchor.base_sha != pr.base_sha || anchor.head_sha != pr.head_sha)
        {
            draft.error = Some("The PR changed. Your draft is kept; start a new comment on the refreshed lines before posting.".into());
            cx.notify();
            return;
        }
        let body = draft.input.read(cx).value().to_string();
        if body.trim().is_empty() || body.len() > 65536 {
            return;
        }
        let target = draft.target.clone();
        draft.sending = true;
        draft.error = None;
        let backend = self.github.backend.clone();
        // Kept with the draft, not the active view: a PR switch must not lose a
        // successful write or cancel its UI reconciliation halfway through.
        draft.task = Some(cx.spawn(async move |owner, cx| {
            let request = key.clone();
            let request_target = target.clone();
            let result = cx
                .background_spawn(async move {
                    backend.post_comment(
                        &request.repository,
                        request.number,
                        &request_target,
                        &body,
                    )
                })
                .await;
            let _ = owner.update(cx, |this, cx| {
                let Some(state) = this.github.comments.states.get_mut(&key) else {
                    return;
                };
                match result {
                    Ok(mut comment) => {
                        comment.resolved = match &target {
                            CommentTarget::Line(_) => Some(false),
                            CommentTarget::Reply(root) => state
                                .comments
                                .iter()
                                .find(|c| c.id == *root)
                                .and_then(|c| c.resolved),
                        };
                        state.just_posted.insert(comment.id);
                        state.comments.retain(|c| c.id != comment.id);
                        state.comments.push(comment);
                        state.drafts.retain(|d| d.id != id);
                    }
                    Err(error) => {
                        if let Some(draft) = state.drafts.iter_mut().find(|d| d.id == id) {
                            draft.sending = false;
                            draft.task = None;
                            draft.error = Some(error);
                        }
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Keep comments whose source line vanished at the file header instead of
    /// attaching them to the wrong code. Missing files fall back to the first header.
    fn comment_annotation(
        &self,
        id: SharedString,
        path: &str,
        line: Option<u32>,
        side: CommentSide,
        cx: &gpui_kit::App,
    ) -> Option<DiffAnnotation> {
        let files = self.pr_state()?.view.diff.read(cx).files();
        if let Some(file) = files.iter().find(|f| f.path().as_str() == path) {
            if let Some(line) =
                line.filter(|line| file.contains_line(side.diff_side(), *line as usize))
            {
                return Some(DiffAnnotation::line(
                    id,
                    DiffLinePosition::new(file.path().clone(), side.diff_side(), line as usize),
                ));
            }
            return Some(DiffAnnotation::file(id, file.path().clone()));
        }
        files
            .first()
            .map(|file| DiffAnnotation::file(id, file.path().clone()))
    }

    pub(super) fn with_pr_comments(&self, diff: Diff, cx: &mut Context<Self>) -> Diff {
        let Some(key) = self.comment_key() else {
            return diff;
        };
        let mut annotations = Vec::new();
        let mut items = HashMap::new();
        if let Some(state) = self.github.comments.states.get(&key) {
            for (id, group) in threads(&state.comments) {
                let root = group
                    .iter()
                    .find(|c| c.id == id)
                    .copied()
                    .unwrap_or(group[0]);
                if let Some(annotation) = self.comment_annotation(
                    format!("pr-thread-{id}").into(),
                    &root.path,
                    root.line,
                    root.side,
                    cx,
                ) {
                    items.insert(annotation.id().clone(), Item::Thread(id));
                    annotations.push(annotation);
                }
            }
            for draft in &state.drafts {
                if let Some(annotation) = self.comment_annotation(
                    format!("pr-draft-{}", draft.id).into(),
                    &draft.path,
                    draft.line,
                    draft.side,
                    cx,
                ) {
                    items.insert(annotation.id().clone(), Item::Draft(draft.id));
                    annotations.push(annotation);
                }
            }
        }
        let owner = cx.entity().downgrade();
        let editor_owner = owner.clone();
        diff.annotations(annotations)
            .render_annotation(move |annotation, _, cx| {
                let item = items.get(annotation.id()).copied();
                owner
                    .update(cx, |this, cx| match item {
                        Some(Item::Thread(id)) => {
                            this.render_pr_thread(&key, id, annotation.position().is_none(), cx)
                        }
                        Some(Item::Draft(id)) => this.render_pr_draft(&key, id, cx),
                        None => div().into_any_element(),
                    })
                    .unwrap_or_else(|_| div().into_any_element())
            })
            .on_add_annotation(move |range, window, cx| {
                let _ =
                    editor_owner.update(cx, |this, cx| this.begin_pr_comment(range, window, cx));
            })
    }

    pub(super) fn render_pr_comment_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(key) = self.comment_key() else {
            return div().into_any_element();
        };
        let Some(state) = self.github.comments.states.get(&key) else {
            return div().into_any_element();
        };
        let groups = threads(&state.comments);
        let resolved = groups
            .iter()
            .filter(|(id, comments)| thread_resolution(**id, comments) == Some(true))
            .count();
        let open = groups
            .iter()
            .filter(|(id, comments)| thread_resolution(**id, comments) == Some(false))
            .count();
        let unknown = groups.len() - resolved - open;
        h_flex()
            .id("pr-comment-status")
            .test_support()
            .px_3()
            .py_1()
            .gap_3()
            .text_size(px(12.))
            .text_color(cx.theme().muted_foreground)
            .child(if state.loading {
                "Loading inline comments…".to_owned()
            } else {
                format!(
                    "{} open · {} resolved{} · Hover a diff line and click + to comment",
                    open,
                    resolved,
                    if unknown > 0 {
                        format!(" · {unknown} status unavailable")
                    } else {
                        String::new()
                    }
                )
            })
            .child(div().flex_1())
            .children(
                state
                    .error
                    .as_ref()
                    .map(|e| div().text_color(cx.theme().danger).child(e.clone())),
            )
            .child(
                Button::new("reload-pr-comments")
                    .ghost()
                    .xsmall()
                    .label("Reload comments")
                    .disabled(state.loading)
                    .on_click(cx.listener(|this, _, _, cx| this.ensure_pr_comments(true, cx))),
            )
            .into_any_element()
    }

    fn render_pr_thread(
        &self,
        key: &ReviewKey,
        id: u64,
        unplaced: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.github.comments.states.get(key) else {
            return div().into_any_element();
        };
        let groups = threads(&state.comments);
        let Some(comments) = groups.get(&id) else {
            return div().into_any_element();
        };
        let resolution = thread_resolution(id, comments);
        let resolved = resolution == Some(true);
        let status = match resolution {
            Some(true) => "Resolved",
            Some(false) => "Open",
            None => "Status unavailable",
        };
        let key = key.clone();
        v_flex()
            .id(SharedString::from(format!("pr-thread-{id}")))
            .test_support()
            .relative()
            .m_2()
            .p_3()
            .gap_2()
            .min_w_0()
            .rounded(px(6.))
            .border_1()
            .border_color(if resolved {
                cx.theme().success.opacity(0.4)
            } else {
                cx.theme().secondary_hover
            })
            .bg(if resolved {
                cx.theme().muted
            } else {
                cx.theme().secondary
            })
            .when(resolved, |el| el.text_color(cx.theme().muted_foreground))
            .child(
                div()
                    .id(SharedString::from(format!("pr-thread-status-{id}")))
                    .test_support()
                    .role(gpui_kit::Role::Status)
                    .aria_label(status)
                    .absolute()
                    .top_2()
                    .right_3()
                    .text_xs()
                    .font_medium()
                    .text_color(if resolved {
                        cx.theme().success
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(
                        h_flex().child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(px(4.))
                                .bg(if resolved {
                                    cx.theme().success.opacity(0.12)
                                } else {
                                    cx.theme().secondary_hover
                                })
                                .child(status),
                        ),
                    ),
            )
            .when(unplaced, |el| {
                el.child(
                    div()
                        .text_xs()
                        .pr(px(140.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "Outdated or outside the current diff · {}",
                            comments[0].path
                        )),
                )
            })
            .children(comments.iter().map(|comment| {
                v_flex()
                    .gap_1()
                    .pb_2()
                    .child(
                        div()
                            .pr(px(140.))
                            .font_medium()
                            .text_size(px(12.))
                            .child(format!("@{}", comment.author)),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("pr-comment-{}", comment.id)))
                            .test_support()
                            .whitespace_normal()
                            .text_size(px(13.))
                            .child(comment.body.clone()),
                    )
            }))
            .child(
                Button::new(SharedString::from(format!("pr-reply-{id}")))
                    .ghost()
                    .small()
                    .label("Reply")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.begin_pr_reply(key.clone(), id, window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_pr_draft(&self, key: &ReviewKey, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(draft) = self
            .github
            .comments
            .states
            .get(key)
            .and_then(|s| s.drafts.iter().find(|d| d.id == id))
        else {
            return div().into_any_element();
        };
        let body = draft.input.read(cx).value();
        let stale = match &draft.target {
            CommentTarget::Line(a) => self
                .pr_state()
                .and_then(|s| s.prs.iter().find(|p| p.number == key.number))
                .is_none_or(|p| p.head_sha != a.head_sha || p.base_sha != a.base_sha),
            CommentTarget::Reply(_) => false,
        };
        let busy = self.pr_state().is_none_or(|s| s.loading || s.listing);
        let submit_key = key.clone();
        let cancel_key = key.clone();
        v_flex()
            .id(SharedString::from(format!("pr-draft-{id}")))
            .test_support()
            .m_2()
            .p_3()
            .gap_2()
            .min_w_0()
            .rounded(px(6.))
            .border_1()
            .border_color(cx.theme().secondary_hover)
            .bg(cx.theme().secondary)
            .child(div().text_size(px(12.)).font_medium().child(format!(
                    "{} · {}:{}",
                    if matches!(draft.target, CommentTarget::Reply(_)) {
                        "Reply"
                    } else {
                        "New inline comment"
                    },
                    draft.path,
                    draft
                        .line
                        .map_or_else(|| "outdated".into(), |n| n.to_string())
                )))
            .child(
                div()
                    .id(SharedString::from(format!("pr-comment-input-{id}")))
                    .test_support()
                    .child(
                        Textarea::new(&draft.input)
                            .h(px(96.))
                            .disabled(draft.sending),
                    ),
            )
            .children(draft.error.as_ref().map(|e| {
                div()
                    .id("pr-comment-error")
                    .test_support()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(e.clone())
            }))
            .when(stale, |el| {
                el.child(div().text_xs().text_color(cx.theme().danger).child(
                    "The PR changed. Draft kept — select the refreshed lines for a new comment.",
                ))
            })
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Posts immediately to GitHub. Drafts stay in this session."),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("pr-cancel-comment-{id}")))
                            .ghost()
                            .small()
                            .label("Discard")
                            .disabled(draft.sending)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(state) =
                                    this.github.comments.states.get_mut(&cancel_key)
                                {
                                    state.drafts.retain(|d| d.id != id || d.sending);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("pr-submit-comment-{id}")))
                            .primary()
                            .small()
                            .label(if draft.sending {
                                "Posting…"
                            } else {
                                "Add comment"
                            })
                            .disabled(
                                draft.sending
                                    || stale
                                    || busy
                                    || body.trim().is_empty()
                                    || body.len() > 65536,
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.submit_pr_comment(submit_key.clone(), id, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(feature = "ui-tests")]
impl Diffy {
    pub fn github_comments_busy(&self) -> bool {
        self.comment_key()
            .and_then(|key| self.github.comments.states.get(&key))
            .is_some_and(|s| s.loading || s.drafts.iter().any(|d| d.sending))
    }
    pub fn start_inline_comment(
        &mut self,
        range: &DiffLineRange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_pr_comment(range, window, cx);
    }
    pub fn github_draft_body(&self, id: u64, cx: &gpui_kit::App) -> Option<String> {
        self.github
            .comments
            .states
            .get(&self.comment_key()?)?
            .drafts
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.input.read(cx).value().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn comment(id: u64, reply: Option<u64>) -> ReviewComment {
        ReviewComment {
            id,
            author: "fixture".into(),
            body: "comment".into(),
            path: "file.rs".into(),
            line: Some(1),
            side: CommentSide::Right,
            in_reply_to: reply,
            resolved: Some(false),
        }
    }
    #[test]
    fn replies_are_grouped_even_when_the_parent_is_missing_or_arrives_later() {
        let comments = vec![
            comment(3, Some(1)),
            comment(2, None),
            comment(1, None),
            comment(4, Some(99)),
        ];
        let groups = threads(&comments);
        assert_eq!(groups[&1].iter().map(|c| c.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(groups[&99][0].id, 4);
    }
    #[test]
    fn older_reads_cannot_erase_a_successful_post() {
        let mut state = State::default();
        state.comments.push(comment(1, None));
        state.just_posted.insert(1);
        state.install_comments(Vec::new());
        assert_eq!(state.comments.len(), 1);
        state.install_comments(vec![comment(1, None)]);
        assert_eq!(state.comments.len(), 1);
        assert!(state.just_posted.is_empty());
        state.install_comments(Vec::new());
        assert!(
            state.comments.is_empty(),
            "Later server-side deletions should still be reflected"
        );
    }
    #[test]
    fn thread_status_uses_the_root_and_refreshes_without_conflating_outdated() {
        let mut root = comment(1, None);
        root.line = None;
        root.resolved = Some(true);
        let reply = comment(2, Some(1));
        let mut state = State::default();
        state.install_comments(vec![root.clone(), reply.clone()]);
        assert_eq!(
            thread_resolution(1, &threads(&state.comments)[&1]),
            Some(true)
        );
        root.resolved = None;
        state.install_comments(vec![root.clone(), reply.clone()]);
        assert_eq!(thread_resolution(1, &threads(&state.comments)[&1]), None);
        root.resolved = Some(false);
        state.install_comments(vec![root, reply.clone()]);
        assert_eq!(
            thread_resolution(1, &threads(&state.comments)[&1]),
            Some(false)
        );
        assert_eq!(thread_resolution(1, &[&reply]), Some(false));
    }
}
