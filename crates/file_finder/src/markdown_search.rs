//! An Obsidian/Notion-style search modal for Markdown wikis: a single dialog
//! that matches both file names and file contents of `.md` files, showing each
//! result as a title + folder breadcrumb + a highlighted content snippet.
//!
//! For speed, the `.md` contents are read into an in-memory index once when the
//! dialog opens; each keystroke then searches that index synchronously (no
//! per-keystroke disk scan), the way Obsidian's search does.

use std::path::PathBuf;
use std::sync::Arc;

use fs::Fs;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    SharedString, Task, WeakEntity, Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use project::{Project, ProjectPath};
use ui::{Color, HighlightedLabel, Label, LabelCommon, LabelSize, ListItem, prelude::*};
use workspace::{ModalView, Workspace};

actions!(
    markdown_search,
    [
        /// Opens a Markdown search that matches file names and contents.
        Toggle
    ]
);

const MAX_RESULTS: usize = 100;
/// Characters of content shown before and after a match in the snippet.
const SNIPPET_BEFORE: usize = 32;
const SNIPPET_AFTER: usize = 140;
const ELLIPSIS: &str = "…";

pub fn init(cx: &mut App) {
    cx.observe_new(MarkdownSearch::register).detach();
}

pub struct MarkdownSearch {
    picker: Entity<Picker<MarkdownSearchDelegate>>,
}

impl MarkdownSearch {
    pub fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _cx: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            let project = workspace.project().clone();
            let weak_workspace = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, move |window, cx| {
                let delegate = MarkdownSearchDelegate::new(weak_workspace, project);
                // A non-uniform list, since rows vary in height (some results
                // have a content snippet, some don't).
                let picker = cx.new(|cx| Picker::list(delegate, window, cx).width(rems(34.)));
                // Start building the content index immediately so typing is fast.
                picker.update(cx, |picker, cx| picker.delegate.load_index(window, cx));
                // Closing the picker (e.g. confirming a result) closes the modal.
                cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent))
                    .detach();
                MarkdownSearch { picker }
            });
        });
    }
}

impl ModalView for MarkdownSearch {}
impl EventEmitter<DismissEvent> for MarkdownSearch {}

impl Focusable for MarkdownSearch {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for MarkdownSearch {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

/// One indexed markdown file. The `*_lower` fields are ASCII-lowercased copies
/// (same byte length as the original, so match offsets map straight back) used
/// for case-insensitive search.
struct MarkdownDoc {
    project_path: ProjectPath,
    title: String,
    title_lower: String,
    content: String,
    content_lower: String,
}

struct MarkdownMatch {
    project_path: ProjectPath,
    title: SharedString,
    breadcrumb: SharedString,
    snippet: SharedString,
    /// Byte positions in `snippet` to highlight (the matched query characters).
    snippet_highlights: Vec<usize>,
    /// Byte offset of the query within the title, or `usize::MAX` if the title
    /// doesn't match (content-only result). Used for ranking.
    title_pos: usize,
}

pub struct MarkdownSearchDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    index: Option<Arc<Vec<MarkdownDoc>>>,
    matches: Vec<MarkdownMatch>,
    selected_index: usize,
}

impl MarkdownSearchDelegate {
    fn new(workspace: WeakEntity<Workspace>, project: Entity<Project>) -> Self {
        Self {
            workspace,
            project,
            index: None,
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    /// Reads every `.md`/`.markdown` file's contents into an in-memory index,
    /// then re-runs the current query against it.
    fn load_index(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let project = self.project.read(cx);
        let fs = project.fs().clone();
        let mut files: Vec<(ProjectPath, PathBuf, String)> = Vec::new();
        for worktree in project.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            for entry in worktree.entries(true, 0) {
                if !entry.is_file() {
                    continue;
                }
                let Some(name) = entry.path.file_name() else {
                    continue;
                };
                let Some(title) = name
                    .strip_suffix(".md")
                    .or_else(|| name.strip_suffix(".markdown"))
                else {
                    continue;
                };
                files.push((
                    ProjectPath {
                        worktree_id,
                        path: entry.path.clone(),
                    },
                    worktree.absolutize(&entry.path),
                    title.to_string(),
                ));
            }
        }

        cx.spawn_in(window, async move |picker, cx| {
            let mut docs = Vec::with_capacity(files.len());
            for (project_path, abs_path, title) in files {
                let Ok(content) = fs.load(&abs_path).await else {
                    continue;
                };
                docs.push(MarkdownDoc {
                    title_lower: title.to_ascii_lowercase(),
                    content_lower: content.to_ascii_lowercase(),
                    project_path,
                    title,
                    content,
                });
            }
            picker
                .update_in(cx, |picker, window, cx| {
                    picker.delegate.index = Some(Arc::new(docs));
                    picker.refresh(window, cx);
                })
                .ok();
        })
        .detach();
    }
}

impl PickerDelegate for MarkdownSearchDelegate {
    type ListItem = ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search markdown by name or content…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_ascii_lowercase();
        self.selected_index = 0;
        self.matches = if query.is_empty() {
            Vec::new()
        } else if let Some(index) = self.index.clone() {
            search_index(&index, &query)
        } else {
            // Index still loading: show instant file-name matches meanwhile.
            self.filename_matches(&query, cx)
        };
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(m) = self.matches.get(self.selected_index) else {
            return;
        };
        let path = m.project_path.clone();
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(path, None, true, true, true, window, cx)
                    .detach_and_log_err(cx);
            });
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let m = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(m.title.clone()).weight(FontWeight::SEMIBOLD))
                        .when(!m.breadcrumb.is_empty(), |this| {
                            this.child(
                                Label::new(m.breadcrumb.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .when(!m.snippet.is_empty(), |this| {
                            this.child(
                                HighlightedLabel::new(
                                    m.snippet.clone(),
                                    m.snippet_highlights.clone(),
                                )
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                        }),
                ),
        )
    }
}

impl MarkdownSearchDelegate {
    /// Fast file-name-only matches from the worktree entries, shown while the
    /// content index is still loading.
    fn filename_matches(&self, query_lower: &str, cx: &App) -> Vec<MarkdownMatch> {
        let mut out = Vec::new();
        for worktree in self.project.read(cx).visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            for entry in worktree.entries(true, 0) {
                if !entry.is_file() {
                    continue;
                }
                let Some(name) = entry.path.file_name() else {
                    continue;
                };
                let Some(title) = name
                    .strip_suffix(".md")
                    .or_else(|| name.strip_suffix(".markdown"))
                else {
                    continue;
                };
                let Some(pos) = title.to_ascii_lowercase().find(query_lower) else {
                    continue;
                };
                let project_path = ProjectPath {
                    worktree_id,
                    path: entry.path.clone(),
                };
                let breadcrumb = breadcrumb_for(&project_path);
                out.push(MarkdownMatch {
                    project_path,
                    title: title.to_string().into(),
                    breadcrumb,
                    snippet: SharedString::default(),
                    snippet_highlights: Vec::new(),
                    title_pos: pos,
                });
            }
        }
        out.sort_by_key(|m| (m.title_pos, m.title.len()));
        out.truncate(MAX_RESULTS);
        out
    }
}

fn search_index(index: &[MarkdownDoc], query_lower: &str) -> Vec<MarkdownMatch> {
    let mut out = Vec::new();
    for doc in index {
        let title_pos = doc.title_lower.find(query_lower).unwrap_or(usize::MAX);
        let content_pos = doc.content_lower.find(query_lower);
        if title_pos == usize::MAX && content_pos.is_none() {
            continue;
        }
        let (snippet, snippet_highlights) = match content_pos {
            Some(pos) => snippet_around(&doc.content, pos, pos + query_lower.len()),
            None => (String::new(), Vec::new()),
        };
        out.push(MarkdownMatch {
            project_path: doc.project_path.clone(),
            title: doc.title.clone().into(),
            breadcrumb: breadcrumb_for(&doc.project_path),
            snippet: snippet.into(),
            snippet_highlights,
            title_pos,
        });
    }
    // Title matches first (earlier match position, then shorter title), then
    // content-only matches (title_pos == usize::MAX).
    out.sort_by_key(|m| (m.title_pos, m.title.len()));
    out.truncate(MAX_RESULTS);
    out
}

/// Folder path for a result, shown as a breadcrumb (`a  /  b  /  c`).
fn breadcrumb_for(path: &ProjectPath) -> SharedString {
    path.path
        .parent()
        .map(|parent| parent.as_unix_str().replace('/', "  /  "))
        .unwrap_or_default()
        .into()
}

/// Extracts a single-line snippet of `text` around the byte range `start..end`,
/// trimmed to a window with `…` markers, and returns the byte positions inside
/// the snippet to highlight.
fn snippet_around(text: &str, start: usize, end: usize) -> (String, Vec<usize>) {
    let start = start.min(text.len());
    let end = end.min(text.len());

    // Window start: up to SNIPPET_BEFORE chars back, stopping at a line start.
    let mut window_start = start;
    let mut count = 0;
    while window_start > 0 && count < SNIPPET_BEFORE {
        let prev = text[..window_start]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        if text[prev..].starts_with('\n') {
            break;
        }
        window_start = prev;
        count += 1;
    }
    // Window end: up to SNIPPET_AFTER chars forward, stopping at a newline.
    let mut window_end = end;
    let mut count = 0;
    while window_end < text.len() && count < SNIPPET_AFTER {
        let ch = text[window_end..].chars().next().unwrap();
        if ch == '\n' {
            break;
        }
        window_end += ch.len_utf8();
        count += 1;
    }

    let body = text[window_start..window_end].trim_end();
    let body_len = body.len();
    let needs_prefix = window_start > 0 && !text[..window_start].ends_with('\n');
    let needs_suffix = window_end < text.len() && !text[window_end..].starts_with('\n');

    let prefix = if needs_prefix { ELLIPSIS } else { "" };
    let mut snippet = String::with_capacity(prefix.len() + body_len + ELLIPSIS.len());
    snippet.push_str(prefix);
    snippet.push_str(body);
    if needs_suffix {
        snippet.push_str(ELLIPSIS);
    }

    let hl_start = prefix.len() + (start - window_start);
    let hl_end = (prefix.len() + (end - window_start)).min(prefix.len() + body_len);
    let highlights = if hl_start < hl_end {
        snippet[hl_start..hl_end]
            .char_indices()
            .map(|(i, _)| hl_start + i)
            .collect()
    } else {
        Vec::new()
    };
    (snippet, highlights)
}
