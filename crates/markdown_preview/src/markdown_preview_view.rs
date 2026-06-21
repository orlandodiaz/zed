use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use editor::scroll::Autoscroll;
use editor::{Editor, EditorEvent, MultiBufferOffset, SelectionEffects};
use gpui::{
    App, ClipboardItem, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, ImageSource, InteractiveElement, IntoElement, IsZero, Pixels, Render, RenderImage,
    Resource, RetainAllImageCache, Rgba, ScrollHandle, SharedString, SharedUri, Subscription,
    SvgRenderer, Task, WeakEntity, Window, point, px,
};
use language::LanguageRegistry;
use markdown::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownFont,
    MarkdownOptions, MarkdownStyle,
};
use project::search::SearchQuery;
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{ContextMenu, Tooltip, prelude::*, right_click_menu, utils::WithRemSize};
use util::markdown::split_local_url_fragment;
use util::normalize_path;
use workspace::item::{Item, ItemBufferKind, ItemHandle};
use workspace::searchable::{
    Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
};
use workspace::{ItemNavHistory, OpenOptions, OpenVisible, Pane, Workspace};

use crate::markdown_preview_settings::MarkdownPreviewSettings;
use crate::{
    OpenFollowingPreview, OpenPreview, OpenPreviewToTheSide, ScrollDown, ScrollDownByItem,
    ToggleEditPreview,
};
use crate::{ScrollPageDown, ScrollPageUp, ScrollToBottom, ScrollToTop, ScrollUp, ScrollUpByItem};

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);

pub struct MarkdownPreviewView {
    workspace: WeakEntity<Workspace>,
    active_editor: Option<EditorState>,
    focus_handle: FocusHandle,
    markdown: Entity<Markdown>,
    _markdown_subscription: Subscription,
    active_source_index: Option<usize>,
    scroll_handle: ScrollHandle,
    image_cache: Entity<RetainAllImageCache>,
    base_directory: Option<PathBuf>,
    /// Obsidian-style image resolution: maps a lowercased image filename to its
    /// path anywhere in the project, so embeds like `![[hammer.svg]]` resolve
    /// even when the file lives in an `attachments/` folder rather than next to
    /// the note. Rebuilt whenever the preview content updates.
    image_index: Rc<HashMap<String, PathBuf>>,
    /// Maps a page name (the stem of `<name>.icon.svg` under an `assets/` folder)
    /// to its icon, so a wikilink leading a table cell or list item can show the
    /// target page's icon. Rebuilt whenever the preview content updates.
    link_icon_index: Rc<HashMap<String, PathBuf>>,
    /// Path to the current page's icon SVG, rendered as vector in the title.
    page_icon_path: Option<PathBuf>,
    /// Rasterized embedded SVGs keyed by (file, modified-time, theme text color):
    /// the mtime means edits to the file show without a restart (gpui's image
    /// cache never reloads a path), and the color means `currentColor` follows the
    /// theme — both without re-rasterizing every frame.
    svg_theme_cache: Rc<RefCell<HashMap<(PathBuf, u64, u32), Arc<RenderImage>>>>,
    pending_update_task: Option<Task<Result<()>>>,
    mode: MarkdownPreviewMode,
    show_footnotes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkdownPreviewMode {
    /// The preview will always show the contents of the provided editor.
    Default,
    /// The preview will "follow" the currently active editor.
    Follow,
}

struct EditorState {
    editor: Entity<Editor>,
    _subscription: Subscription,
}

impl MarkdownPreviewView {
    pub fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        // Editors that should NOT be auto-converted to a preview the next time
        // they're added to a pane. Populated when toggling a preview back to its
        // source editor, so the open-as-preview hook leaves the editor alone.
        let suppress_auto_preview: Rc<RefCell<HashSet<EntityId>>> =
            Rc::new(RefCell::new(HashSet::new()));

        // Open markdown files directly in rendered preview. We can't build a
        // preview from the file-open pipeline (it always builds an Editor for a
        // buffer), so instead we swap the freshly-added editor for a preview
        // synchronously, before the frame is painted, so the editor never shows.
        let workspace_entity = cx.entity();
        cx.subscribe_in(&workspace_entity, window, {
            let suppress_auto_preview = suppress_auto_preview.clone();
            move |workspace, _, event, window, cx| {
                let workspace::Event::ItemAdded { item } = event else {
                    return;
                };
                let Some(editor) = item.downcast::<Editor>() else {
                    return;
                };
                if !Self::is_markdown_file(&editor, cx) {
                    return;
                }
                if suppress_auto_preview
                    .borrow_mut()
                    .remove(&editor.entity_id())
                {
                    // This add came from toggling a preview back to its editor.
                    return;
                }
                if crate::SuppressAutoPreview::take(cx) {
                    // Opened explicitly as an editor (e.g. from the git panel to
                    // view a file's diff); leave it as the source editor.
                    return;
                }
                let Some(pane) = workspace
                    .panes()
                    .iter()
                    .find(|pane| pane.read(cx).index_for_item(&editor).is_some())
                    .cloned()
                else {
                    return;
                };
                let view = Self::create_markdown_view(workspace, editor.clone(), window, cx);
                let view_id = view.entity_id();
                pane.update(cx, |pane, cx| {
                    let Some(index) = pane.index_for_item(&editor) else {
                        return;
                    };
                    // Preserve preview-tab (single-click) behavior: if the editor
                    // was the pane's preview item, make the swapped-in preview the
                    // preview item too, so markdown clicks reuse one tab.
                    let was_preview = pane.preview_item_id() == Some(editor.entity_id());
                    pane.remove_item(editor.entity_id(), false, false, window, cx);
                    pane.add_item(Box::new(view), true, true, Some(index), window, cx);
                    if was_preview {
                        pane.replace_preview_item_id(view_id, window, cx);
                    }
                });
                cx.notify();
            }
        })
        .detach();

        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_markdown_editor(workspace, cx) {
                let view = Self::create_markdown_view(workspace, editor.clone(), window, cx);
                workspace.active_pane().update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_independent_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view.clone()), true, true, None, window, cx)
                    }
                });
                cx.notify();
            }
        });

        let suppress_for_toggle = suppress_auto_preview.clone();
        workspace.register_action(move |workspace, _: &ToggleEditPreview, window, cx| {
            // Rendered preview -> source editor, replacing the item in the same
            // tab. Checked first: when a preview is active, the active item can
            // still `act_as` an editor (its backing editor), so the
            // editor->preview branch below would otherwise spawn a new tab.
            if let Some(preview) = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<MarkdownPreviewView>())
            {
                if let Some(editor) = preview
                    .read(cx)
                    .active_editor
                    .as_ref()
                    .map(|state| state.editor.clone())
                {
                    // The editor is about to be re-added to its pane; don't let
                    // the open-as-preview hook immediately convert it back.
                    suppress_for_toggle.borrow_mut().insert(editor.entity_id());
                    let pane = workspace.active_pane().clone();
                    pane.update(cx, |pane, cx| {
                        let index = pane.active_item_index();
                        pane.remove_item(preview.entity_id(), false, false, window, cx);
                        pane.add_item(Box::new(editor), true, true, Some(index), window, cx);
                    });
                    cx.notify();
                }
                return;
            }
            // Editor -> rendered preview, replacing the item in the same tab.
            if let Some(editor) = Self::resolve_active_item_as_markdown_editor(workspace, cx) {
                let view = Self::create_markdown_view(workspace, editor.clone(), window, cx);
                let pane = workspace.active_pane().clone();
                pane.update(cx, |pane, cx| {
                    let index = pane.active_item_index();
                    pane.remove_item(editor.entity_id(), false, false, window, cx);
                    pane.add_item(Box::new(view), true, true, Some(index), window, cx);
                });
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_markdown_editor(workspace, cx) {
                let view = Self::create_markdown_view(workspace, editor.clone(), window, cx);
                let pane = workspace
                    .find_pane_in_direction(workspace::SplitDirection::Right, cx)
                    .unwrap_or_else(|| {
                        workspace.split_pane(
                            workspace.active_pane().clone(),
                            workspace::SplitDirection::Right,
                            window,
                            cx,
                        )
                    });
                pane.update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_independent_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view.clone()), false, false, None, window, cx)
                    }
                });
                editor.focus_handle(cx).focus(window, cx);
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenFollowingPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_markdown_editor(workspace, cx) {
                // Check if there's already a following preview
                let existing_follow_view_idx = {
                    let active_pane = workspace.active_pane().read(cx);
                    active_pane
                        .items_of_type::<MarkdownPreviewView>()
                        .find(|view| view.read(cx).mode == MarkdownPreviewMode::Follow)
                        .and_then(|view| active_pane.index_for_item(&view))
                };

                if let Some(existing_follow_view_idx) = existing_follow_view_idx {
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.activate_item(existing_follow_view_idx, true, true, window, cx);
                    });
                } else {
                    let view = Self::create_following_markdown_view(workspace, editor, window, cx);
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.add_item(Box::new(view.clone()), true, true, None, window, cx)
                    });
                }
                cx.notify();
            }
        });
    }

    fn find_existing_independent_preview_item_idx(
        pane: &Pane,
        editor: &Entity<Editor>,
        cx: &App,
    ) -> Option<usize> {
        pane.items_of_type::<MarkdownPreviewView>()
            .find(|view| {
                let view_read = view.read(cx);
                // Only look for independent (Default mode) previews, not Follow previews
                view_read.mode == MarkdownPreviewMode::Default
                    && view_read
                        .active_editor
                        .as_ref()
                        .is_some_and(|active_editor| active_editor.editor == *editor)
            })
            .and_then(|view| pane.index_for_item(&view))
    }

    pub fn resolve_active_item_as_markdown_editor(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Editor>> {
        if let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            && Self::is_markdown_file(&editor, cx)
        {
            return Some(editor);
        }
        None
    }

    fn create_markdown_view(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<MarkdownPreviewView> {
        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace_handle = workspace.weak_handle();
        MarkdownPreviewView::new(
            MarkdownPreviewMode::Default,
            editor,
            workspace_handle,
            language_registry,
            window,
            cx,
        )
    }

    fn create_following_markdown_view(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<MarkdownPreviewView> {
        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace_handle = workspace.weak_handle();
        MarkdownPreviewView::new(
            MarkdownPreviewMode::Follow,
            editor,
            workspace_handle,
            language_registry,
            window,
            cx,
        )
    }

    pub fn new(
        mode: MarkdownPreviewMode,
        active_editor: Entity<Editor>,
        workspace: WeakEntity<Workspace>,
        language_registry: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let markdown = cx.new(|cx| {
                Markdown::new_with_options(
                    SharedString::default(),
                    Some(language_registry),
                    None,
                    MarkdownOptions {
                        parse_html: true,
                        render_mermaid_diagrams: true,
                        parse_heading_slugs: true,
                        ..Default::default()
                    },
                    cx,
                )
            });
            let mut this = Self {
                active_editor: None,
                focus_handle: cx.focus_handle(),
                workspace: workspace.clone(),
                _markdown_subscription: cx.observe(
                    &markdown,
                    |this: &mut Self, _: Entity<Markdown>, cx| {
                        this.sync_active_root_block(cx);
                    },
                ),
                markdown,
                active_source_index: None,
                scroll_handle: ScrollHandle::new(),
                image_cache: RetainAllImageCache::new(cx),
                base_directory: None,
                image_index: Rc::new(HashMap::new()),
                link_icon_index: Rc::new(HashMap::new()),
                page_icon_path: None,
                svg_theme_cache: Rc::new(RefCell::new(HashMap::new())),
                pending_update_task: None,
                mode,
                show_footnotes: false,
            };

            this.set_editor(active_editor, window, cx);

            if mode == MarkdownPreviewMode::Follow {
                if let Some(workspace) = &workspace.upgrade() {
                    cx.observe_in(workspace, window, |this, workspace, window, cx| {
                        let item = workspace.read(cx).active_item(cx);
                        this.workspace_updated(item, window, cx);
                    })
                    .detach();
                } else {
                    log::error!("Failed to listen to workspace updates");
                }
            }

            this
        })
    }

    fn workspace_updated(
        &mut self,
        active_item: Option<Box<dyn ItemHandle>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = active_item
            && item.item_id() != cx.entity_id()
            && let Some(editor) = item.act_as::<Editor>(cx)
            && Self::is_markdown_file(&editor, cx)
        {
            self.set_editor(editor, window, cx);
        }
    }

    pub fn is_markdown_file<V>(editor: &Entity<Editor>, cx: &mut Context<V>) -> bool {
        let buffer = editor.read(cx).buffer().read(cx);
        if let Some(buffer) = buffer.as_singleton()
            && let Some(language) = buffer.read(cx).language()
        {
            return language.name() == "Markdown";
        }
        false
    }

    fn set_editor(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active_editor
            && active.editor == editor
        {
            return;
        }

        let subscription = cx.subscribe_in(
            &editor,
            window,
            |this, editor, event: &EditorEvent, window, cx| {
                match event {
                    EditorEvent::Edited { .. }
                    | EditorEvent::BufferEdited { .. }
                    | EditorEvent::DirtyChanged
                    | EditorEvent::BuffersEdited { .. } => {
                        this.update_markdown_from_active_editor(true, false, window, cx);
                    }
                    EditorEvent::SelectionsChanged { .. } => {
                        let (selection_start, editor_is_focused) =
                            editor.update(cx, |editor, cx| {
                                let index = Self::selected_source_index(editor, cx);
                                let focused = editor.focus_handle(cx).is_focused(window);
                                (index, focused)
                            });
                        this.sync_preview_to_source_index(selection_start, editor_is_focused, cx);
                        cx.notify();
                    }
                    _ => {}
                };
            },
        );

        self.base_directory = Self::get_folder_for_active_editor(editor.read(cx), cx);
        self.active_editor = Some(EditorState {
            editor,
            _subscription: subscription,
        });

        self.update_markdown_from_active_editor(false, true, window, cx);
    }

    fn update_markdown_from_active_editor(
        &mut self,
        wait_for_debounce: bool,
        should_reveal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = &self.active_editor {
            // if there is already a task to update the ui and the current task is also debounced (not high priority), do nothing
            if wait_for_debounce && self.pending_update_task.is_some() {
                return;
            }
            self.pending_update_task = Some(self.schedule_markdown_update(
                wait_for_debounce,
                should_reveal,
                state.editor.clone(),
                window,
                cx,
            ));
        }
    }

    fn toggle_footnotes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_footnotes = !self.show_footnotes;
        if let Some(editor) = self.active_editor.as_ref().map(|state| state.editor.clone()) {
            let task = self.schedule_markdown_update(false, false, editor, window, cx);
            self.pending_update_task = Some(task);
        }
        cx.notify();
    }

    fn schedule_markdown_update(
        &mut self,
        wait_for_debounce: bool,
        should_reveal_selection: bool,
        editor: Entity<Editor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        cx.spawn_in(window, async move |view, cx| {
            if wait_for_debounce {
                // Wait for the user to stop typing
                cx.background_executor().timer(REPARSE_DEBOUNCE).await;
            }

            let editor_clone = editor.clone();
            let update = view.update(cx, |view, cx| {
                let is_active_editor = view
                    .active_editor
                    .as_ref()
                    .is_some_and(|active_editor| active_editor.editor == editor_clone);
                if !is_active_editor {
                    return None;
                }

                let (contents, selection_start) = editor_clone.update(cx, |editor, cx| {
                    let contents = editor.buffer().read(cx).snapshot(cx).text();
                    let selection_start = Self::selected_source_index(editor, cx);
                    (contents, selection_start)
                });
                let contents = if view.show_footnotes {
                    contents
                } else {
                    strip_footnotes(&contents)
                };
                Some((SharedString::from(contents), selection_start))
            })?;

            view.update(cx, move |view, cx| {
                if let Some((contents, selection_start)) = update {
                    view.markdown.update(cx, |markdown, cx| {
                        markdown.reset(contents, cx);
                    });
                    view.image_index = Rc::new(build_image_index(
                        &view.workspace,
                        view.base_directory.as_deref(),
                        cx,
                    ));
                    view.link_icon_index = Rc::new(build_link_icon_index(&view.workspace, cx));
                    view.page_icon_path = view.current_page_icon(cx);
                    view.sync_preview_to_source_index(selection_start, should_reveal_selection, cx);
                    cx.emit(SearchEvent::MatchesInvalidated);
                }
                view.pending_update_task = None;
                cx.notify();
            })
        })
    }

    fn selected_source_index(editor: &Editor, cx: &mut App) -> usize {
        editor
            .selections
            .last::<MultiBufferOffset>(&editor.display_snapshot(cx))
            .range()
            .start
            .0
    }

    fn sync_preview_to_source_index(
        &mut self,
        source_index: usize,
        reveal: bool,
        cx: &mut Context<Self>,
    ) {
        self.active_source_index = Some(source_index);
        self.sync_active_root_block(cx);
        self.markdown.update(cx, |markdown, cx| {
            if reveal {
                markdown.request_autoscroll_to_source_index(source_index, cx);
            }
        });
    }

    fn sync_active_root_block(&mut self, cx: &mut Context<Self>) {
        self.markdown.update(cx, |markdown, cx| {
            markdown.set_active_root_for_source_index(self.active_source_index, cx);
        });
    }

    fn move_cursor_to_source_index(
        editor: &Entity<Editor>,
        source_index: usize,
        window: &mut Window,
        cx: &mut App,
    ) {
        editor.update(cx, |editor, cx| {
            let selection = MultiBufferOffset(source_index)..MultiBufferOffset(source_index);
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges(vec![selection]),
            );
            window.focus(&editor.focus_handle(cx), cx);
        });
    }

    /// The absolute path of the file that is currently being previewed.
    fn get_folder_for_active_editor(editor: &Editor, cx: &App) -> Option<PathBuf> {
        if let Some(file) = editor.file_at(MultiBufferOffset(0), cx) {
            if let Some(file) = file.as_local() {
                file.abs_path(cx).parent().map(|p| p.to_path_buf())
            } else {
                None
            }
        } else {
            None
        }
    }

    /// The document title shown at the top of the preview: the file name without
    /// its extension (e.g. `EDW Team.md` -> "EDW Team"), Obsidian-style.
    fn document_title(&self, cx: &App) -> Option<SharedString> {
        let editor = self.active_editor.as_ref()?.editor.read(cx);
        let file = editor.file_at(MultiBufferOffset(0), cx)?;
        let abs_path = file.as_local()?.abs_path(cx);
        let stem = abs_path.file_stem()?.to_string_lossy().into_owned();
        Some(stem.into())
    }

    /// Path to the page's Obsidian-style custom icon (`assets/…/<name>.icon.svg`),
    /// shown before the document title. `None` when the page has no icon file.
    fn current_page_icon(&self, cx: &App) -> Option<PathBuf> {
        let editor = self.active_editor.as_ref()?.editor.read(cx);
        let file = editor.file_at(MultiBufferOffset(0), cx)?;
        let worktree_id = file.worktree_id(cx);
        let page_path = file.path().clone();
        let project = self.workspace.upgrade()?.read(cx).project().clone();
        let worktree = project.read(cx).worktree_for_id(worktree_id, cx)?;
        resolve_markdown_page_icon(worktree.read(cx), &page_path)
    }

    fn line_scroll_amount(&self, cx: &App) -> Pixels {
        let settings = ThemeSettings::get_global(cx);
        settings.buffer_font_size(cx) * settings.buffer_line_height.value()
    }

    fn scroll_by_amount(&self, distance: Pixels) {
        let offset = self.scroll_handle.offset();
        self.scroll_handle
            .set_offset(point(offset.x, offset.y - distance));
    }

    fn scroll_page_up(&mut self, _: &ScrollPageUp, _window: &mut Window, cx: &mut Context<Self>) {
        let viewport_height = self.scroll_handle.bounds().size.height;
        if viewport_height.is_zero() {
            return;
        }

        self.scroll_by_amount(-viewport_height);
        cx.notify();
    }

    fn scroll_page_down(
        &mut self,
        _: &ScrollPageDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_height = self.scroll_handle.bounds().size.height;
        if viewport_height.is_zero() {
            return;
        }

        self.scroll_by_amount(viewport_height);
        cx.notify();
    }

    fn scroll_up(&mut self, _: &ScrollUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            let item_height = bounds.size.height;
            // Scroll no more than the rough equivalent of a large headline
            let max_height = window.rem_size() * 2;
            let scroll_height = min(item_height, max_height);
            self.scroll_by_amount(-scroll_height);
        } else {
            let scroll_height = self.line_scroll_amount(cx);
            if !scroll_height.is_zero() {
                self.scroll_by_amount(-scroll_height);
            }
        }
        cx.notify();
    }

    fn scroll_down(&mut self, _: &ScrollDown, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            let item_height = bounds.size.height;
            // Scroll no more than the rough equivalent of a large headline
            let max_height = window.rem_size() * 2;
            let scroll_height = min(item_height, max_height);
            self.scroll_by_amount(scroll_height);
        } else {
            let scroll_height = self.line_scroll_amount(cx);
            if !scroll_height.is_zero() {
                self.scroll_by_amount(scroll_height);
            }
        }
        cx.notify();
    }

    fn scroll_up_by_item(
        &mut self,
        _: &ScrollUpByItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            self.scroll_by_amount(-bounds.size.height);
        }
        cx.notify();
    }

    fn scroll_down_by_item(
        &mut self,
        _: &ScrollDownByItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bounds) = self
            .scroll_handle
            .bounds_for_item(self.scroll_handle.top_item())
        {
            self.scroll_by_amount(bounds.size.height);
        }
        cx.notify();
    }

    fn scroll_to_top(&mut self, _: &ScrollToTop, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_handle.scroll_to_item(0);
        cx.notify();
    }

    fn scroll_to_bottom(
        &mut self,
        _: &ScrollToBottom,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_handle.scroll_to_bottom();
        cx.notify();
    }

    fn render_markdown_element(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> MarkdownElement {
        let active_editor = self
            .active_editor
            .as_ref()
            .map(|state| state.editor.clone());

        let mut workspace_directory = None;
        if let Some(workspace_entity) = self.workspace.upgrade() {
            let project = workspace_entity.read(cx).project();
            if let Some(tree) = project.read(cx).worktrees(cx).next() {
                workspace_directory = Some(tree.read(cx).abs_path().to_path_buf());
            }
        }

        let mut markdown_style = MarkdownStyle::themed(MarkdownFont::Editor, window, cx);
        // Links shouldn't have a highlighted background box in the preview.
        markdown_style.link.background_color = None;
        // Make headings semibold (they only get a size by default).
        markdown_style.heading.text.font_weight = Some(FontWeight::SEMIBOLD);
        // Don't bold table header cells (wiki tables of links look heavy bolded).
        markdown_style.table_header_text.font_weight = None;
        // Cap embedded images so a large-intrinsic-size SVG (e.g. a 1024px logo)
        // doesn't fill the whole preview width. Aspect ratio is preserved.
        markdown_style.image_max_height = Some(px(360.).into());
        // Display math size is user-tunable via settings (no recompile needed).
        markdown_style.display_math_scale =
            MarkdownPreviewSettings::get_global(cx).display_math_scale;
        let mut markdown_element = MarkdownElement::new(self.markdown.clone(), markdown_style)
        .code_block_renderer(CodeBlockRenderer::Default {
            copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
            border: false,
        })
        .scroll_handle(self.scroll_handle.clone())
        .image_resolver({
            let base_directory = self.base_directory.clone();
            let image_index = self.image_index.clone();
            let svg_renderer = cx.svg_renderer();
            let text_color = cx.theme().colors().text;
            let cache = self.svg_theme_cache.clone();
            move |dest_url| {
                let source = resolve_preview_image(
                    dest_url,
                    base_directory.as_deref(),
                    workspace_directory.as_deref(),
                    &image_index,
                )?;
                // Make embedded SVGs that use `currentColor` follow the theme.
                Some(theme_embedded_svg(source, text_color, &svg_renderer, &cache))
            }
        })
        .link_icon_resolver({
            let link_icon_index = self.link_icon_index.clone();
            move |dest_url| link_icon_index.get(&dest_url.to_ascii_lowercase()).cloned()
        })
        // (resolver returns the icon SVG path; the markdown element renders it as
        // crisp vector geometry rather than a rasterized image.)
        .on_url_click({
            let view_handle = cx.entity().downgrade();
            let workspace = self.workspace.clone();
            let base_directory = self.base_directory.clone();
            move |url, window, cx| {
                handle_url_click(
                    url,
                    &view_handle,
                    base_directory.clone(),
                    &workspace,
                    window,
                    cx,
                );
            }
        });

        if let Some(active_editor) = active_editor {
            let editor_for_checkbox = active_editor.clone();
            let view_handle = cx.entity().downgrade();
            markdown_element = markdown_element
                .on_source_click(move |source_index, click_count, window, cx| {
                    if click_count == 2 {
                        Self::move_cursor_to_source_index(&active_editor, source_index, window, cx);
                        true
                    } else {
                        false
                    }
                })
                .on_checkbox_toggle(move |source_range, new_checked, window, cx| {
                    let task_marker = if new_checked { "[x]" } else { "[ ]" };
                    editor_for_checkbox.update(cx, |editor, cx| {
                        editor.edit(
                            [(
                                MultiBufferOffset(source_range.start)
                                    ..MultiBufferOffset(source_range.end),
                                task_marker,
                            )],
                            cx,
                        );
                    });
                    if let Some(view) = view_handle.upgrade() {
                        cx.update_entity(&view, |this, cx| {
                            this.update_markdown_from_active_editor(false, false, window, cx);
                        });
                    }
                });
        }

        markdown_element
    }
}

fn handle_url_click(
    url: SharedString,
    view: &WeakEntity<MarkdownPreviewView>,
    base_directory: Option<PathBuf>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let (path_part, fragment) = split_local_url_fragment(url.as_ref());

    if path_part.is_empty() {
        if let Some(fragment) = fragment {
            let view = view.clone();
            let slug = SharedString::from(fragment.to_string());
            window.defer(cx, move |window, cx| {
                if let Some(view) = view.upgrade() {
                    let markdown = view.read(cx).markdown.clone();
                    let active_editor = view
                        .read(cx)
                        .active_editor
                        .as_ref()
                        .map(|state| state.editor.clone());

                    let source_index =
                        markdown.update(cx, |markdown, cx| markdown.scroll_to_heading(&slug, cx));

                    if let Some(source_index) = source_index {
                        if let Some(editor) = active_editor {
                            MarkdownPreviewView::move_cursor_to_source_index(
                                &editor,
                                source_index,
                                window,
                                cx,
                            );
                        }
                    }
                }
            });
        }
    } else {
        open_preview_url(
            SharedString::from(path_part.to_string()),
            base_directory,
            workspace,
            window,
            cx,
        );
    }
}

fn open_preview_url(
    url: SharedString,
    base_directory: Option<PathBuf>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(path) = resolve_preview_path(url.as_ref(), base_directory.as_deref())
        && let Some(workspace) = workspace.upgrade()
    {
        let _ = workspace.update(cx, |workspace, cx| {
            workspace
                .open_abs_path(
                    normalize_path(path.as_path()),
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
                .detach();
        });
        return;
    }

    // Obsidian-style wikilink: resolve `[[Name]]` to `Name.md` anywhere in the
    // project and open it.
    if let Some(workspace) = workspace.upgrade()
        && open_wikilink_target(url.as_ref(), &workspace, window, cx)
    {
        return;
    }

    // Only hand off to the OS for real URLs. A bare, unresolved wikilink
    // otherwise triggers a macOS "application can't be opened (-50)" error.
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:") {
        cx.open_url(url.as_ref());
    }
}

/// Resolve an Obsidian-style wikilink target (the text inside `[[ ]]`) to a
/// markdown file with that name anywhere in the project's worktrees and open
/// it. Returns false if no matching file is found.
fn open_wikilink_target(
    name: &str,
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let name = urlencoding::decode(name)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| name.to_string());
    let file_name = if name.ends_with(".md") {
        name.clone()
    } else {
        format!("{name}.md")
    };
    let project_path = {
        let project = workspace.read(cx).project().read(cx);
        project.worktrees(cx).find_map(|worktree| {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            // include_ignored: wiki notes are often gitignored, so search them too.
            worktree.files(true, 0).find_map(|entry| {
                (entry.path.file_name() == Some(file_name.as_str())).then(|| project::ProjectPath {
                    worktree_id,
                    path: entry.path.clone(),
                })
            })
        })
    };
    let Some(project_path) = project_path else {
        return false;
    };
    workspace.update(cx, |workspace, cx| {
        // allow_preview = true so the target opens in the reused preview tab
        // (and the open-as-preview swap keeps that status) instead of piling
        // up a new permanent tab per wikilink click.
        workspace
            .open_path_preview(project_path, None, true, true, true, window, cx)
            .detach();
    });
    true
}

/// Hide inline footnote references (`[^label]`) from the source so they don't
/// clutter the prose, while keeping the footnote definition lines
/// (`[^label]: ...`) intact so the sources/footnotes section still renders.
/// Uses the official `[^...]` syntax, which is unambiguous (unlike bare `[1]`,
/// which collides with array indices in code).
fn strip_footnotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        // Keep footnote definition lines (`[^label]:`) so the sources section
        // still renders; only the inline markers in prose are removed.
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("[^") {
            if let Some(close) = rest.find(']') {
                if rest[close + 1..].starts_with(':') {
                    out.push_str(line);
                    continue;
                }
            }
        }
        // Strip inline references `[^label]` (and a single space before them).
        let mut rest = line;
        while let Some(start) = rest.find("[^") {
            let Some(close_rel) = rest[start + 2..].find(']') else {
                break;
            };
            let mut before = &rest[..start];
            if before.ends_with(' ') {
                before = &before[..before.len() - 1];
            }
            out.push_str(before);
            rest = &rest[start + 2 + close_rel + 1..];
        }
        out.push_str(rest);
    }
    out
}

fn resolve_preview_path(url: &str, base_directory: Option<&Path>) -> Option<PathBuf> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return None;
    }

    let decoded_url = urlencoding::decode(url)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| url.to_string());
    let candidate = PathBuf::from(&decoded_url);

    if candidate.is_absolute() && candidate.exists() {
        return Some(candidate);
    }

    let base_directory = base_directory?;
    let resolved = base_directory.join(decoded_url);
    if resolved.exists() {
        Some(resolved)
    } else {
        None
    }
}

fn resolve_preview_image(
    dest_url: &str,
    base_directory: Option<&Path>,
    workspace_directory: Option<&Path>,
    image_index: &HashMap<String, PathBuf>,
) -> Option<ImageSource> {
    if dest_url.starts_with("data:") {
        return None;
    }

    if dest_url.starts_with("http://") || dest_url.starts_with("https://") {
        return Some(ImageSource::Resource(Resource::Uri(SharedUri::from(
            dest_url.to_string(),
        ))));
    }

    let decoded = urlencoding::decode(dest_url)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| dest_url.to_string());

    let decoded_path = Path::new(&decoded);

    if let Ok(relative_path) = decoded_path.strip_prefix("/") {
        if let Some(root) = workspace_directory {
            let absolute_path = root.join(relative_path);
            if absolute_path.exists() {
                return Some(ImageSource::Resource(Resource::Path(Arc::from(
                    absolute_path.as_path(),
                ))));
            }
        }
    }

    let path = if Path::new(&decoded).is_absolute() {
        Some(PathBuf::from(&decoded))
    } else {
        base_directory.map(|base| base.join(&decoded))
    };

    if let Some(path) = &path
        && path.exists()
    {
        return Some(ImageSource::Resource(Resource::Path(Arc::from(
            path.as_path(),
        ))));
    }

    // Obsidian-style fallback: a bare filename (or a path whose file doesn't
    // exist at the literal location) resolves to a matching image anywhere in
    // the project.
    if let Some(found) = Path::new(&decoded)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| image_index.get(&name.to_ascii_lowercase()))
    {
        return Some(ImageSource::Resource(Resource::Path(Arc::from(
            found.as_path(),
        ))));
    }

    // Preserve the prior behavior of handing back the literal path even when it
    // doesn't exist (the image element shows its own broken-image state).
    let path = path?;
    Some(ImageSource::Resource(Resource::Path(Arc::from(
        path.as_path(),
    ))))
}

/// Rasterizes an embedded SVG ourselves so that (a) any `currentColor` follows
/// the theme's text color, and (b) edits to the file show without a restart —
/// gpui's image cache is keyed by path and never reloads, so we key our own
/// cache on the file's modified-time. Non-SVG sources pass through to `img()`.
fn theme_embedded_svg(
    source: ImageSource,
    text_color: Hsla,
    svg_renderer: &SvgRenderer,
    cache: &RefCell<HashMap<(PathBuf, u64, u32), Arc<RenderImage>>>,
) -> ImageSource {
    let ImageSource::Resource(Resource::Path(path)) = &source else {
        return source;
    };
    let is_svg = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"));
    if !is_svg {
        return source;
    }

    let mtime = std::fs::metadata(path.as_ref())
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|dur| dur.as_millis() as u64)
        .unwrap_or(0);
    let cache_key = (path.to_path_buf(), mtime, theme_color_key(text_color));
    if let Some(image) = cache.borrow().get(&cache_key) {
        return ImageSource::Render(image.clone());
    }

    let Ok(text) = std::fs::read_to_string(path.as_ref()) else {
        return source;
    };
    let themed = if text.contains("currentColor") {
        text.replace("currentColor", &theme_color_hex(text_color))
    } else {
        text
    };
    match svg_renderer.render_single_frame(themed.as_bytes(), 1.0) {
        Ok(image) => {
            cache.borrow_mut().insert(cache_key, image.clone());
            ImageSource::Render(image)
        }
        Err(_) => source,
    }
}

fn theme_color_hex(color: Hsla) -> String {
    let rgba: Rgba = color.into();
    format!(
        "#{:02x}{:02x}{:02x}",
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
    )
}

fn theme_color_key(color: Hsla) -> u32 {
    let rgba: Rgba = color.into();
    ((rgba.r * 255.0).round() as u32) << 16
        | ((rgba.g * 255.0).round() as u32) << 8
        | ((rgba.b * 255.0).round() as u32)
}

/// Image file extensions resolvable via the Obsidian-style filename index.
/// Mirrors the formats gpui's `img()` element can decode (raster + `svg`).
const IMAGE_EXTENSIONS: &[&str] = &[
    "avif", "jpg", "jpeg", "png", "gif", "webp", "tif", "tiff", "tga", "bmp", "ico", "svg",
];

/// Index every image file in the project by its lowercased filename so embeds
/// can be resolved by name alone, like an Obsidian vault. On a filename
/// collision, the file sharing the most path components with the note wins.
fn build_image_index(
    workspace: &WeakEntity<Workspace>,
    base_directory: Option<&Path>,
    cx: &App,
) -> HashMap<String, PathBuf> {
    use std::collections::hash_map::Entry;

    let mut index: HashMap<String, PathBuf> = HashMap::new();
    let Some(workspace) = workspace.upgrade() else {
        return index;
    };
    let project = workspace.read(cx).project().read(cx);
    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path();
        // include_ignored: wiki attachments are often gitignored.
        for entry in worktree.files(true, 0) {
            let Some(extension) = entry.path.extension() else {
                continue;
            };
            if !IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
                continue;
            }
            let Some(file_name) = entry.path.file_name() else {
                continue;
            };
            let key = file_name.to_ascii_lowercase();
            let abs_path = root.join(entry.path.as_std_path());
            match index.entry(key) {
                Entry::Occupied(mut slot) => {
                    if shared_ancestor_len(&abs_path, base_directory)
                        > shared_ancestor_len(slot.get(), base_directory)
                    {
                        slot.insert(abs_path);
                    }
                }
                Entry::Vacant(slot) => {
                    slot.insert(abs_path);
                }
            }
        }
    }
    index
}

/// Resolves an Obsidian-style custom page icon for a markdown file: for `Foo.md`,
/// looks for `Foo.icon.svg` in an `assets/` folder at the page's wiki root,
/// checking the path-mirrored `assets/<page-folder>/Foo.icon.svg` first (so
/// same-named pages stay distinct), then a flat `assets/Foo.icon.svg`. Mirrors
/// `project_panel::resolve_markdown_page_icon` (duplicated to avoid a dependency
/// on the project panel from the preview).
fn resolve_markdown_page_icon(
    worktree: &project::Worktree,
    page_path: &util::rel_path::RelPath,
) -> Option<PathBuf> {
    use util::rel_path::RelPath;

    let extension = page_path.extension()?;
    if !extension.eq_ignore_ascii_case("md") && !extension.eq_ignore_ascii_case("markdown") {
        return None;
    }
    let stem = page_path.file_stem()?;
    let icon_file_name = format!("{stem}.icon.svg");
    let icon_name = RelPath::unix(&icon_file_name).ok()?;
    let assets_dir = RelPath::unix("assets").ok()?;
    let parent = page_path.parent().unwrap_or(RelPath::empty());

    for root in parent.ancestors() {
        let assets = root.join(assets_dir);
        if !worktree
            .entry_for_path(&assets)
            .is_some_and(|entry| entry.is_dir())
        {
            continue;
        }
        if let Ok(subpath) = parent.strip_prefix(root) {
            let mirrored = assets.join(subpath).join(icon_name);
            if worktree.entry_for_path(&mirrored).is_some() {
                return Some(worktree.absolutize(&mirrored));
            }
        }
        let flat = assets.join(icon_name);
        if worktree.entry_for_path(&flat).is_some() {
            return Some(worktree.absolutize(&flat));
        }
    }
    None
}

/// Index page icons by page name for wikilink resolution: every `<name>.icon.svg`
/// under an `assets/` folder maps its lowercased `<name>` to the icon, so a
/// wikilink `[[Name]]` can show the target page's icon.
fn build_link_icon_index(
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> HashMap<String, PathBuf> {
    let mut index: HashMap<String, PathBuf> = HashMap::new();
    let Some(workspace) = workspace.upgrade() else {
        return index;
    };
    let project = workspace.read(cx).project().read(cx);
    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        // include_ignored: wiki assets are often gitignored.
        for entry in worktree.files(true, 0) {
            let Some(file_name) = entry.path.file_name() else {
                continue;
            };
            let lower = file_name.to_ascii_lowercase();
            let Some(stem) = lower.strip_suffix(".icon.svg") else {
                continue;
            };
            if stem.is_empty()
                || !entry
                    .path
                    .ancestors()
                    .any(|ancestor| ancestor.file_name() == Some("assets"))
            {
                continue;
            }
            let abs_path = worktree.absolutize(&entry.path);
            index.insert(stem.to_string(), abs_path);
        }
    }
    index
}

/// Number of leading path components `path` shares with `base`, used to prefer
/// the image closest to the note when a filename appears in multiple folders.
fn shared_ancestor_len(path: &Path, base: Option<&Path>) -> usize {
    let Some(base) = base else {
        return 0;
    };
    path.components()
        .zip(base.components())
        .take_while(|(a, b)| a == b)
        .count()
}

impl Focusable for MarkdownPreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for MarkdownPreviewView {}
impl EventEmitter<SearchEvent> for MarkdownPreviewView {}

impl Item for MarkdownPreviewView {
    type Event = ();

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            self.active_editor
                .as_ref()
                .map(|state| state.editor.clone().into())
        } else {
            None
        }
    }

    fn tab_icon(&self, window: &Window, cx: &App) -> Option<Icon> {
        // Match the source editor's tab icon so the tab is unchanged on toggle.
        self.active_editor
            .as_ref()
            .and_then(|state| state.editor.read(cx).tab_icon(window, cx))
            .or_else(|| Some(Icon::new(IconName::FileDoc)))
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        // Match the source editor's tab title (no "Preview" prefix).
        self.active_editor
            .as_ref()
            .map(|state| state.editor.read(cx).tab_content_text(detail, cx))
            .unwrap_or_else(|| SharedString::from("Markdown Preview"))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        // Report the underlying file so the project panel keeps it selected and
        // breadcrumbs/active-file features work while previewing.
        if let Some(state) = self.active_editor.as_ref() {
            state.editor.read(cx).for_each_project_item(cx, f);
        }
    }

    // Delegate nav-history hooks to the backing editor so back/forward records
    // and restores this previewed file. Without this, previews leave no history
    // entries and Back skips over them to the last plain editor.
    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.active_editor.as_ref() {
            state
                .editor
                .update(cx, |editor, _| editor.set_nav_history(Some(history)));
        }
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.active_editor.as_ref() {
            state
                .editor
                .update(cx, |editor, cx| editor.deactivated(window, cx));
        }
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.active_editor
            .as_ref()
            .map(|state| {
                state
                    .editor
                    .update(cx, |editor, cx| editor.navigate(data, window, cx))
            })
            .unwrap_or(false)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Markdown Preview Opened")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }
}

impl Render for MarkdownPreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Headings (and other rems-based sizes) scale with the rem size; body
        // and code use absolute sizes and are unaffected. Shrink headings a bit.
        let rem_size = window.rem_size() * 0.85;
        div()
            .image_cache(self.image_cache.clone())
            .id("MarkdownPreview")
            .key_context("MarkdownPreview")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(MarkdownPreviewView::scroll_page_up))
            .on_action(cx.listener(MarkdownPreviewView::scroll_page_down))
            .on_action(cx.listener(MarkdownPreviewView::scroll_up))
            .on_action(cx.listener(MarkdownPreviewView::scroll_down))
            .on_action(cx.listener(MarkdownPreviewView::scroll_up_by_item))
            .on_action(cx.listener(MarkdownPreviewView::scroll_down_by_item))
            .on_action(cx.listener(MarkdownPreviewView::scroll_to_top))
            .on_action(cx.listener(MarkdownPreviewView::scroll_to_bottom))
            .size_full()
            .relative()
            .bg(cx.theme().colors().editor_background)
            .child(
                div()
                    .id("markdown-preview-scroll-container")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .p_4()
                    // Body text inherits this size (gpui text runs don't carry a
                    // size); headings set their own size so they're unaffected.
                    .text_size(px(15.0))
                    .children(self.document_title(cx).map(|title| {
                        h_flex()
                            .pb_3()
                            .gap_2()
                            .children(
                                self.page_icon_path
                                    .as_ref()
                                    .and_then(|path| markdown::svg_icon::render_svg_icon(path, px(30.))),
                            )
                            .child(
                                div()
                                    .text_3xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child(title),
                            )
                    }))
                    .child(WithRemSize::new(rem_size).child({
                        let markdown_element = self.render_markdown_element(window, cx);
                        let markdown = self.markdown.clone();
                        right_click_menu("markdown-preview-context-menu")
                            .trigger(move |_, _, _| markdown_element)
                            .menu(move |window, cx| {
                                let focus = window.focused(cx);
                                let context_menu_link =
                                    markdown.read(cx).context_menu_link().cloned();
                                ContextMenu::build(window, cx, move |menu, _, _cx| {
                                    menu.when_some(focus, |menu, focus| menu.context(focus))
                                        .when_some(context_menu_link, |menu, url| {
                                            menu.entry("Copy Link", None, move |_, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    url.to_string(),
                                                ));
                                            })
                                        })
                                })
                            })
                    })),
            )
            .child(
                div()
                    .absolute()
                    .top_2()
                    .right_4()
                    .flex()
                    .gap_1()
                    .child(
                        IconButton::new("markdown-toggle-footnotes", IconName::Hash)
                            .icon_size(IconSize::Small)
                            .toggle_state(!self.show_footnotes)
                            .tooltip(Tooltip::text(if self.show_footnotes {
                                "Hide footnotes"
                            } else {
                                "Show footnotes"
                            }))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.toggle_footnotes(window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("markdown-toggle-edit", IconName::Pencil)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Edit Markdown"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ToggleEditPreview), cx);
                            }),
                    ),
            )
    }
}

impl SearchableItem for MarkdownPreviewView {
    type Match = Range<usize>;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: true,
            word: true,
            regex: true,
            replacement: false,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    fn get_matches(&self, _window: &mut Window, cx: &mut App) -> (Vec<Self::Match>, SearchToken) {
        (
            self.markdown.read(cx).search_highlights().to_vec(),
            SearchToken::default(),
        )
    }

    fn clear_matches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let had_highlights = !self.markdown.read(cx).search_highlights().is_empty();
        self.markdown.update(cx, |markdown, cx| {
            markdown.clear_search_highlights(cx);
        });
        if had_highlights {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old_highlights = self.markdown.read(cx).search_highlights();
        let changed = old_highlights != matches;
        self.markdown.update(cx, |markdown, cx| {
            markdown.set_search_highlights(matches.to_vec(), active_match_index, cx);
        });
        if changed {
            cx.emit(SearchEvent::MatchesInvalidated);
        }
    }

    fn query_suggestion(
        &mut self,
        _ignore_settings: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.markdown.read(cx).selected_text().unwrap_or_default()
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(match_range) = matches.get(index) {
            let start = match_range.start;
            self.markdown.update(cx, |markdown, cx| {
                markdown.set_active_search_highlight(Some(index), cx);
                markdown.request_autoscroll_to_source_index(start, cx);
            });
            cx.emit(SearchEvent::ActiveMatchChanged);
        }
    }

    fn select_matches(
        &mut self,
        _matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn replace(
        &mut self,
        _: &Self::Match,
        _: &SearchQuery,
        _token: SearchToken,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        let source = self.markdown.read(cx).source().to_string();
        cx.background_spawn(async move { query.search_str(&source) })
    }

    fn active_match_index(
        &mut self,
        direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }

        let markdown = self.markdown.read(cx);
        let current_source_index = markdown
            .active_search_highlight()
            .and_then(|i| markdown.search_highlights().get(i))
            .map(|m| m.start)
            .or(self.active_source_index)
            .unwrap_or(0);

        match direction {
            Direction::Next => matches
                .iter()
                .position(|m| m.start >= current_source_index)
                .or(Some(0)),
            Direction::Prev => matches
                .iter()
                .rposition(|m| m.start <= current_source_index)
                .or(Some(matches.len().saturating_sub(1))),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::markdown_preview_view::ImageSource;
    use crate::markdown_preview_view::Resource;
    use crate::markdown_preview_view::resolve_preview_image;
    use anyhow::Result;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    use super::resolve_preview_path;

    #[test]
    fn resolves_relative_preview_paths() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let base_directory = temp_dir.path();
        let file = base_directory.join("notes.md");
        fs::write(&file, "# Notes")?;

        assert_eq!(
            resolve_preview_path("notes.md", Some(base_directory)),
            Some(file)
        );
        assert_eq!(
            resolve_preview_path("nonexistent.md", Some(base_directory)),
            None
        );
        assert_eq!(resolve_preview_path("notes.md", None), None);

        Ok(())
    }

    #[test]
    fn resolves_urlencoded_preview_paths() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let base_directory = temp_dir.path();
        let file = base_directory.join("release notes.md");
        fs::write(&file, "# Release Notes")?;

        assert_eq!(
            resolve_preview_path("release%20notes.md", Some(base_directory)),
            Some(file)
        );

        Ok(())
    }

    #[test]
    fn resolves_workspace_absolute_preview_images() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let workspace_directory = temp_dir.path();

        let base_directory = workspace_directory.join("docs");
        fs::create_dir_all(&base_directory)?;

        let image_file = workspace_directory.join("test_image.png");
        fs::write(&image_file, "mock data")?;

        let resolved_success = resolve_preview_image(
            "/test_image.png",
            Some(&base_directory),
            Some(workspace_directory),
            &HashMap::new(),
        );

        match resolved_success {
            Some(ImageSource::Resource(Resource::Path(p))) => {
                assert_eq!(p.as_ref(), image_file.as_path());
            }
            _ => panic!("Expected successful resolution to be a Resource::Path"),
        }

        let resolved_missing = resolve_preview_image(
            "/missing_image.png",
            Some(&base_directory),
            Some(workspace_directory),
            &HashMap::new(),
        );

        let expected_missing_path = if std::path::Path::new("/missing_image.png").is_absolute() {
            std::path::PathBuf::from("/missing_image.png")
        } else {
            // join is to retain windows path prefix C:/
            #[expect(clippy::join_absolute_paths)]
            base_directory.join("/missing_image.png")
        };

        match resolved_missing {
            Some(ImageSource::Resource(Resource::Path(p))) => {
                assert_eq!(p.as_ref(), expected_missing_path.as_path());
            }
            _ => panic!("Expected missing file to fallback to a Resource::Path"),
        }

        Ok(())
    }

    #[test]
    fn resolves_obsidian_embed_by_filename() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let workspace_directory = temp_dir.path();

        // Note lives in `notes/`, image lives in `attachments/` — the Obsidian
        // layout where the embed only names the file, not its location.
        let base_directory = workspace_directory.join("notes");
        fs::create_dir_all(&base_directory)?;
        let attachments = workspace_directory.join("attachments");
        fs::create_dir_all(&attachments)?;
        let image_file = attachments.join("hammer-candlestick.svg");
        fs::write(&image_file, "<svg/>")?;

        let mut image_index = HashMap::new();
        image_index.insert("hammer-candlestick.svg".to_string(), image_file.clone());

        // A bare filename resolves to the indexed path even though it isn't next
        // to the note.
        let resolved = resolve_preview_image(
            "hammer-candlestick.svg",
            Some(&base_directory),
            Some(workspace_directory),
            &image_index,
        );
        match resolved {
            Some(ImageSource::Resource(Resource::Path(p))) => {
                assert_eq!(p.as_ref(), image_file.as_path());
            }
            _ => panic!("Expected the embed to resolve via the filename index"),
        }

        // A real file next to the note still takes precedence over the index.
        let local_image = base_directory.join("hammer-candlestick.svg");
        fs::write(&local_image, "<svg/>")?;
        let resolved_local = resolve_preview_image(
            "hammer-candlestick.svg",
            Some(&base_directory),
            Some(workspace_directory),
            &image_index,
        );
        match resolved_local {
            Some(ImageSource::Resource(Resource::Path(p))) => {
                assert_eq!(p.as_ref(), local_image.as_path());
            }
            _ => panic!("Expected the local file to take precedence"),
        }

        Ok(())
    }

    #[test]
    fn does_not_treat_web_links_as_preview_paths() {
        assert_eq!(resolve_preview_path("https://zed.dev", None), None);
        assert_eq!(resolve_preview_path("http://example.com", None), None);
    }
}
