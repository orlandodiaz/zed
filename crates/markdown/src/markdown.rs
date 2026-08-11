pub mod html;
mod math;
pub mod page_icon;
pub mod svg_icon;
mod mermaid;
pub mod parser;
mod path_range;

use base64::Engine as _;
use futures::FutureExt as _;
use gpui::EdgesRefinement;
use gpui::HitboxBehavior;
use gpui::UnderlineStyle;
use language::LanguageName;

use log::Level;
use mermaid::{
    MermaidState, ParsedMarkdownMermaidDiagram, extract_mermaid_diagrams, render_mermaid_diagram,
};
pub use path_range::{LineCol, PathWithRange};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::Checkbox;
use ui::CopyButton;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::iter;
use std::mem;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, App, BorderStyle, Bounds, ClipboardItem, CursorStyle, DispatchPhase, Edges, Entity,
    FocusHandle, Focusable, FontStyle, FontWeight, GlobalElementId, Hitbox, Hsla, Image,
    ImageFormat, ImageSource, KeyContext, Length, MouseButton, MouseDownEvent, MouseEvent,
    MouseMoveEvent, MouseUpEvent, Point, ScrollHandle, Stateful, StrikethroughStyle,
    StyleRefinement, StyledText, Task, TextAlign, TextLayout, TextRun, TextStyle,
    TextStyleRefinement, actions, img, point, quad,
};
use language::{CharClassifier, Language, LanguageRegistry, Rope};
use parser::CodeBlockMetadata;
use parser::{
    MarkdownEvent, MarkdownTag, MarkdownTagEnd, parse_links_only, parse_markdown_with_options,
};
use pulldown_cmark::{Alignment, LinkType};
use sum_tree::TreeMap;
use theme::SyntaxTheme;
use ui::{ScrollAxes, Scrollbars, WithScrollbar, prelude::*};
use util::ResultExt;

use crate::parser::CodeBlockKind;

/// A callback function that can be used to customize the style of links based on the destination URL.
/// If the callback returns `None`, the default link style will be used.
type LinkStyleCallback = Rc<dyn Fn(&str, &App) -> Option<TextStyleRefinement>>;
type SourceClickCallback = Box<dyn Fn(usize, usize, &mut Window, &mut App) -> bool>;
type CheckboxToggleCallback = Rc<dyn Fn(Range<usize>, bool, &mut Window, &mut App)>;
/// Defines custom style refinements for each heading level (H1-H6)
#[derive(Clone, Default)]
pub struct HeadingLevelStyles {
    pub h1: Option<TextStyleRefinement>,
    pub h2: Option<TextStyleRefinement>,
    pub h3: Option<TextStyleRefinement>,
    pub h4: Option<TextStyleRefinement>,
    pub h5: Option<TextStyleRefinement>,
    pub h6: Option<TextStyleRefinement>,
}

#[derive(Clone)]
pub struct MarkdownStyle {
    pub base_text_style: TextStyle,
    pub container_style: StyleRefinement,
    pub code_block: StyleRefinement,
    pub code_block_overflow_x_scroll: bool,
    pub inline_code: TextStyleRefinement,
    pub block_quote: TextStyleRefinement,
    pub link: TextStyleRefinement,
    pub link_callback: Option<LinkStyleCallback>,
    pub rule_color: Hsla,
    pub block_quote_border_color: Hsla,
    pub syntax: Arc<SyntaxTheme>,
    pub selection_background_color: Hsla,
    pub heading: StyleRefinement,
    pub heading_level_styles: Option<HeadingLevelStyles>,
    pub height_is_multiple_of_line_height: bool,
    pub prevent_mouse_interaction: bool,
    /// When true, a table shrinks to fit its content (columns sized to content,
    /// box left-aligned) instead of stretching to the full available width.
    pub table_size_to_content: bool,
    /// Text style applied to table header cells. Defaults to semibold; set to a
    /// refinement with `font_weight: None` to render headers at normal weight.
    pub table_header_text: TextStyleRefinement,
    /// Optional cap on the rendered height of embedded images. Aspect ratio is
    /// preserved, so this also bounds width. `None` leaves images at their
    /// intrinsic size (up to the container width).
    pub image_max_height: Option<DefiniteLength>,
    /// How much larger than body text display math (`$$…$$`) renders.
    pub display_math_scale: f32,
    /// When true, inline code (`` `code` ``) is rendered as its own sized box so
    /// it can use `inline_code.font_size` independently of the surrounding line
    /// (a shaped `StyledText` line is locked to a single size, so the run-level
    /// `font_size` is otherwise ignored). Lines containing inline code then flow
    /// as per-word boxes, like inline math. Off by default; the markdown preview
    /// opts in.
    pub inline_code_box: bool,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            base_text_style: Default::default(),
            container_style: Default::default(),
            code_block: Default::default(),
            code_block_overflow_x_scroll: false,
            inline_code: Default::default(),
            block_quote: Default::default(),
            link: Default::default(),
            link_callback: None,
            rule_color: Default::default(),
            block_quote_border_color: Default::default(),
            syntax: Arc::new(SyntaxTheme::default()),
            selection_background_color: Default::default(),
            heading: Default::default(),
            heading_level_styles: None,
            height_is_multiple_of_line_height: false,
            prevent_mouse_interaction: false,
            table_size_to_content: false,
            table_header_text: TextStyleRefinement {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            },
            image_max_height: None,
            display_math_scale: 1.2,
            inline_code_box: false,
        }
    }
}

pub enum MarkdownFont {
    Agent,
    Editor,
}

impl MarkdownStyle {
    pub fn themed(font: MarkdownFont, window: &Window, cx: &App) -> Self {
        let theme_settings = ThemeSettings::get_global(cx);
        let colors = cx.theme().colors();

        let buffer_font_weight = theme_settings.buffer_font.weight;
        let (buffer_font_size, ui_font_size) = match font {
            MarkdownFont::Agent => (
                theme_settings.agent_buffer_font_size(cx),
                theme_settings.agent_ui_font_size(cx),
            ),
            MarkdownFont::Editor => (
                theme_settings.buffer_font_size(cx),
                theme_settings.ui_font_size(cx),
            ),
        };

        let text_color = colors.text;

        let mut text_style = window.text_style();
        let line_height = buffer_font_size * 1.75;

        text_style.refine(&TextStyleRefinement {
            font_family: Some(theme_settings.ui_font.family.clone()),
            font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
            font_features: Some(theme_settings.ui_font.features.clone()),
            font_size: Some(ui_font_size.into()),
            line_height: Some(line_height.into()),
            color: Some(text_color),
            ..Default::default()
        });

        MarkdownStyle {
            base_text_style: text_style.clone(),
            syntax: cx.theme().syntax().clone(),
            selection_background_color: colors.element_selection_background,
            rule_color: colors.border,
            block_quote_border_color: colors.border,
            code_block_overflow_x_scroll: true,
            code_block: StyleRefinement {
                padding: EdgesRefinement {
                    top: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                    left: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                    right: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                    bottom: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                },
                margin: EdgesRefinement {
                    top: Some(Length::Definite(px(8.).into())),
                    left: Some(Length::Definite(px(0.).into())),
                    right: Some(Length::Definite(px(0.).into())),
                    bottom: Some(Length::Definite(px(12.).into())),
                },
                border_style: Some(BorderStyle::Solid),
                border_widths: EdgesRefinement {
                    top: Some(AbsoluteLength::Pixels(px(1.))),
                    left: Some(AbsoluteLength::Pixels(px(1.))),
                    right: Some(AbsoluteLength::Pixels(px(1.))),
                    bottom: Some(AbsoluteLength::Pixels(px(1.))),
                },
                border_color: Some(colors.border_variant),
                background: Some(colors.editor_background.into()),
                text: TextStyleRefinement {
                    font_family: Some(theme_settings.buffer_font.family.clone()),
                    font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                    font_features: Some(theme_settings.buffer_font.features.clone()),
                    font_size: Some(buffer_font_size.into()),
                    font_weight: Some(buffer_font_weight),
                    ..Default::default()
                },
                ..Default::default()
            },
            inline_code: TextStyleRefinement {
                font_family: Some(theme_settings.buffer_font.family.clone()),
                font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                font_features: Some(theme_settings.buffer_font.features.clone()),
                font_size: Some(buffer_font_size.into()),
                font_weight: Some(buffer_font_weight),
                background_color: Some(colors.editor_foreground.opacity(0.08)),
                ..Default::default()
            },
            link: TextStyleRefinement {
                background_color: Some(colors.editor_foreground.opacity(0.025)),
                color: Some(colors.text_accent),
                underline: Some(UnderlineStyle {
                    color: Some(colors.text_accent.opacity(0.5)),
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn with_muted_text(mut self, cx: &App) -> Self {
        let colors = cx.theme().colors();
        self.base_text_style.color = colors.text_muted;
        self
    }
}

pub struct Markdown {
    source: SharedString,
    selection: Selection,
    pressed_link: Option<RenderedLink>,
    pressed_footnote_ref: Option<RenderedFootnoteRef>,
    autoscroll_request: Option<usize>,
    active_root_block: Option<usize>,
    parsed_markdown: ParsedMarkdown,
    images_by_source_offset: HashMap<usize, Arc<Image>>,
    should_reparse: bool,
    pending_parse: Option<Task<()>>,
    focus_handle: FocusHandle,
    language_registry: Option<Arc<LanguageRegistry>>,
    fallback_code_block_language: Option<LanguageName>,
    options: MarkdownOptions,
    mermaid_state: MermaidState,
    copied_code_blocks: HashSet<ElementId>,
    code_block_scroll_handles: BTreeMap<usize, ScrollHandle>,
    table_scroll_handles: BTreeMap<usize, ScrollHandle>,
    context_menu_link: Option<SharedString>,
    context_menu_selected_text: Option<String>,
    search_highlights: Vec<Range<usize>>,
    active_search_highlight: Option<usize>,
}

#[derive(Clone, Copy, Default)]
pub struct MarkdownOptions {
    pub parse_links_only: bool,
    pub parse_html: bool,
    pub render_mermaid_diagrams: bool,
    pub parse_heading_slugs: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CopyButtonVisibility {
    Hidden,
    AlwaysVisible,
    VisibleOnHover,
}

pub enum CodeBlockRenderer {
    Default {
        copy_button_visibility: CopyButtonVisibility,
        border: bool,
    },
    Custom {
        render: CodeBlockRenderFn,
        /// A function that can modify the parent container after the code block
        /// content has been appended as a child element.
        transform: Option<CodeBlockTransformFn>,
    },
}

pub type CodeBlockRenderFn = Arc<
    dyn Fn(
        &CodeBlockKind,
        &ParsedMarkdown,
        Range<usize>,
        CodeBlockMetadata,
        &mut Window,
        &App,
    ) -> Div,
>;

pub type CodeBlockTransformFn =
    Arc<dyn Fn(AnyDiv, Range<usize>, CodeBlockMetadata, &mut Window, &App) -> AnyDiv>;

actions!(
    markdown,
    [
        /// Copies the selected text to the clipboard.
        Copy,
        /// Copies the selected text as markdown to the clipboard.
        CopyAsMarkdown
    ]
);

enum EscapeAction {
    PassThrough,
    Nbsp(usize),
    DoubleNewline,
    PrefixBackslash,
}

impl EscapeAction {
    fn output_len(&self) -> usize {
        match self {
            Self::PassThrough => 1,
            Self::Nbsp(count) => count * '\u{00A0}'.len_utf8(),
            Self::DoubleNewline => 2,
            Self::PrefixBackslash => 2,
        }
    }

    fn write_to(&self, c: char, output: &mut String) {
        match self {
            Self::PassThrough => output.push(c),
            Self::Nbsp(count) => {
                for _ in 0..*count {
                    output.push('\u{00A0}');
                }
            }
            Self::DoubleNewline => {
                output.push('\n');
                output.push('\n');
            }
            Self::PrefixBackslash => {
                // '\\' is a single backslash in Rust, e.g. '|' -> '\|'
                output.push('\\');
                output.push(c);
            }
        }
    }
}

// Valid to operate on raw bytes since multi-byte UTF-8
// sequences never contain ASCII-range bytes.
struct MarkdownEscaper {
    in_leading_whitespace: bool,
}

impl MarkdownEscaper {
    const TAB_SIZE: usize = 4;

    fn new() -> Self {
        Self {
            in_leading_whitespace: true,
        }
    }

    fn next(&mut self, byte: u8) -> EscapeAction {
        let action = if self.in_leading_whitespace && byte == b'\t' {
            EscapeAction::Nbsp(Self::TAB_SIZE)
        } else if self.in_leading_whitespace && byte == b' ' {
            EscapeAction::Nbsp(1)
        } else if byte == b'\n' {
            EscapeAction::DoubleNewline
        } else if byte.is_ascii_punctuation() {
            EscapeAction::PrefixBackslash
        } else {
            EscapeAction::PassThrough
        };

        self.in_leading_whitespace =
            byte == b'\n' || (self.in_leading_whitespace && (byte == b' ' || byte == b'\t'));
        action
    }
}

impl Markdown {
    pub fn new(
        source: SharedString,
        language_registry: Option<Arc<LanguageRegistry>>,
        fallback_code_block_language: Option<LanguageName>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_options(
            source,
            language_registry,
            fallback_code_block_language,
            MarkdownOptions::default(),
            cx,
        )
    }

    pub fn new_with_options(
        source: SharedString,
        language_registry: Option<Arc<LanguageRegistry>>,
        fallback_code_block_language: Option<LanguageName>,
        options: MarkdownOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            source,
            selection: Selection::default(),
            pressed_link: None,
            pressed_footnote_ref: None,
            autoscroll_request: None,
            active_root_block: None,
            should_reparse: false,
            images_by_source_offset: Default::default(),
            parsed_markdown: ParsedMarkdown::default(),
            pending_parse: None,
            focus_handle,
            language_registry,
            fallback_code_block_language,
            options,
            mermaid_state: MermaidState::default(),
            copied_code_blocks: HashSet::default(),
            code_block_scroll_handles: BTreeMap::default(),
            table_scroll_handles: BTreeMap::default(),
            context_menu_link: None,
            context_menu_selected_text: None,
            search_highlights: Vec::new(),
            active_search_highlight: None,
        };
        this.parse(cx);
        this
    }

    pub fn new_text(source: SharedString, cx: &mut Context<Self>) -> Self {
        Self::new_with_options(
            source,
            None,
            None,
            MarkdownOptions {
                parse_links_only: true,
                ..Default::default()
            },
            cx,
        )
    }

    fn code_block_scroll_handle(&mut self, id: usize) -> ScrollHandle {
        self.code_block_scroll_handles
            .entry(id)
            .or_insert_with(ScrollHandle::new)
            .clone()
    }

    fn retain_code_block_scroll_handles(&mut self, ids: &HashSet<usize>) {
        self.code_block_scroll_handles
            .retain(|id, _| ids.contains(id));
    }

    fn table_scroll_handle(&mut self, id: usize) -> ScrollHandle {
        self.table_scroll_handles
            .entry(id)
            .or_insert_with(ScrollHandle::new)
            .clone()
    }

    fn retain_table_scroll_handles(&mut self, ids: &HashSet<usize>) {
        self.table_scroll_handles.retain(|id, _| ids.contains(id));
    }

    fn clear_code_block_scroll_handles(&mut self) {
        self.code_block_scroll_handles.clear();
    }

    fn autoscroll_code_block(&self, source_index: usize, cursor_position: Point<Pixels>) {
        let Some((_, scroll_handle)) = self
            .code_block_scroll_handles
            .range(..=source_index)
            .next_back()
        else {
            return;
        };

        let bounds = scroll_handle.bounds();
        if cursor_position.y < bounds.top() || cursor_position.y > bounds.bottom() {
            return;
        }

        let horizontal_delta = if cursor_position.x < bounds.left() {
            bounds.left() - cursor_position.x
        } else if cursor_position.x > bounds.right() {
            bounds.right() - cursor_position.x
        } else {
            return;
        };

        let offset = scroll_handle.offset();
        scroll_handle.set_offset(point(offset.x + horizontal_delta, offset.y));
    }

    pub fn is_parsing(&self) -> bool {
        self.pending_parse.is_some()
    }

    pub fn scroll_to_heading(&mut self, slug: &str, cx: &mut Context<Self>) -> Option<usize> {
        if let Some(source_index) = self.parsed_markdown.heading_slugs.get(slug).copied() {
            self.autoscroll_request = Some(source_index);
            cx.notify();
            Some(source_index)
        } else {
            None
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn append(&mut self, text: &str, cx: &mut Context<Self>) {
        self.source = SharedString::new(self.source.to_string() + text);
        self.parse(cx);
    }

    pub fn replace(&mut self, source: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.source = source.into();
        self.parse(cx);
    }

    pub fn request_autoscroll_to_source_index(
        &mut self,
        source_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.autoscroll_request = Some(source_index);
        cx.refresh_windows();
    }

    fn footnote_definition_content_start(&self, label: &SharedString) -> Option<usize> {
        self.parsed_markdown
            .footnote_definitions
            .get(label)
            .copied()
    }

    pub fn set_active_root_for_source_index(
        &mut self,
        source_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let active_root_block =
            source_index.and_then(|index| self.parsed_markdown.root_block_for_source_index(index));
        if self.active_root_block == active_root_block {
            return;
        }

        self.active_root_block = active_root_block;
        cx.notify();
    }

    pub fn reset(&mut self, source: SharedString, cx: &mut Context<Self>) {
        if source == self.source() {
            return;
        }
        self.source = source;
        self.selection = Selection::default();
        self.autoscroll_request = None;
        self.pending_parse = None;
        self.should_reparse = false;
        self.search_highlights.clear();
        self.active_search_highlight = None;
        // Don't clear parsed_markdown here - keep existing content visible until new parse completes
        self.parse(cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn parsed_markdown(&self) -> &ParsedMarkdown {
        &self.parsed_markdown
    }

    pub fn escape(s: &str) -> Cow<'_, str> {
        let output_len: usize = {
            let mut escaper = MarkdownEscaper::new();
            s.bytes().map(|byte| escaper.next(byte).output_len()).sum()
        };

        if output_len == s.len() {
            return s.into();
        }

        let mut escaper = MarkdownEscaper::new();
        let mut output = String::with_capacity(output_len);
        for c in s.chars() {
            escaper.next(c as u8).write_to(c, &mut output);
        }
        output.into()
    }

    pub fn selected_text(&self) -> Option<String> {
        if self.selection.end <= self.selection.start {
            None
        } else {
            Some(self.source[self.selection.start..self.selection.end].to_string())
        }
    }

    pub fn set_search_highlights(
        &mut self,
        highlights: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.search_highlights = highlights;
        self.active_search_highlight = active;
        cx.notify();
    }

    pub fn clear_search_highlights(&mut self, cx: &mut Context<Self>) {
        if !self.search_highlights.is_empty() || self.active_search_highlight.is_some() {
            self.search_highlights.clear();
            self.active_search_highlight = None;
            cx.notify();
        }
    }

    pub fn set_active_search_highlight(&mut self, active: Option<usize>, cx: &mut Context<Self>) {
        if self.active_search_highlight != active {
            self.active_search_highlight = active;
            cx.notify();
        }
    }

    pub fn search_highlights(&self) -> &[Range<usize>] {
        &self.search_highlights
    }

    pub fn active_search_highlight(&self) -> Option<usize> {
        self.active_search_highlight
    }

    fn copy(&self, text: &RenderedText, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.end <= self.selection.start {
            return;
        }
        let text = text.text_for_range(self.selection.start..self.selection.end);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn copy_as_markdown(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.context_menu_selected_text.take() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        if self.selection.end <= self.selection.start {
            return;
        }
        let text = self.source[self.selection.start..self.selection.end].to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn capture_for_context_menu(&mut self, link: Option<SharedString>) {
        self.context_menu_selected_text = self.selected_text();
        self.context_menu_link = link;
    }

    /// Returns the URL of the link that was most recently right-clicked, if any.
    /// This is set during a right-click mouse-down event and can be read by parent
    /// views to include a "Copy Link" item in their context menus.
    pub fn context_menu_link(&self) -> Option<&SharedString> {
        self.context_menu_link.as_ref()
    }

    fn parse(&mut self, cx: &mut Context<Self>) {
        if self.source.is_empty() {
            self.should_reparse = false;
            self.pending_parse.take();
            self.parsed_markdown = ParsedMarkdown {
                source: self.source.clone(),
                ..Default::default()
            };
            self.active_root_block = None;
            self.images_by_source_offset.clear();
            self.mermaid_state.clear();
            cx.notify();
            cx.refresh_windows();
            return;
        }

        if self.pending_parse.is_some() {
            self.should_reparse = true;
            return;
        }
        self.should_reparse = false;
        self.pending_parse = Some(self.start_background_parse(cx));
    }

    fn start_background_parse(&self, cx: &Context<Self>) -> Task<()> {
        let source = self.source.clone();
        let should_parse_links_only = self.options.parse_links_only;
        let should_parse_html = self.options.parse_html;
        let should_render_mermaid_diagrams = self.options.render_mermaid_diagrams;
        let should_parse_heading_slugs = self.options.parse_heading_slugs;
        let language_registry = self.language_registry.clone();
        let fallback = self.fallback_code_block_language.clone();

        let parsed = cx.background_spawn(async move {
            if should_parse_links_only {
                return (
                    ParsedMarkdown {
                        events: Arc::from(parse_links_only(source.as_ref())),
                        source,
                        languages_by_name: TreeMap::default(),
                        languages_by_path: TreeMap::default(),
                        root_block_starts: Arc::default(),
                        html_blocks: BTreeMap::default(),
                        mermaid_diagrams: BTreeMap::default(),
                        heading_slugs: HashMap::default(),
                        footnote_definitions: HashMap::default(),
                    },
                    Default::default(),
                );
            }

            let parsed =
                parse_markdown_with_options(&source, should_parse_html, should_parse_heading_slugs);
            let events = parsed.events;
            let language_names = parsed.language_names;
            let paths = parsed.language_paths;
            let root_block_starts = parsed.root_block_starts;
            let html_blocks = parsed.html_blocks;
            let heading_slugs = parsed.heading_slugs;
            let footnote_definitions = parsed.footnote_definitions;
            let mermaid_diagrams = if should_render_mermaid_diagrams {
                extract_mermaid_diagrams(&source, &events)
            } else {
                BTreeMap::default()
            };
            let mut images_by_source_offset = HashMap::default();
            let mut languages_by_name = TreeMap::default();
            let mut languages_by_path = TreeMap::default();
            if let Some(registry) = language_registry.as_ref() {
                for name in language_names {
                    let language = if !name.is_empty() {
                        registry.language_for_name_or_extension(&name).left_future()
                    } else if let Some(fallback) = &fallback {
                        registry.language_for_name(fallback.as_ref()).right_future()
                    } else {
                        continue;
                    };
                    if let Ok(language) = language.await {
                        languages_by_name.insert(name, language);
                    }
                }

                for path in paths {
                    if let Ok(language) = registry
                        .load_language_for_file_path(Path::new(path.as_ref()))
                        .await
                    {
                        languages_by_path.insert(path, language);
                    }
                }
            }

            for (range, event) in &events {
                if let MarkdownEvent::Start(MarkdownTag::Image { dest_url, .. }) = event
                    && let Some(data_url) = dest_url.strip_prefix("data:")
                {
                    let Some((mime_info, data)) = data_url.split_once(',') else {
                        continue;
                    };
                    let Some((mime_type, encoding)) = mime_info.split_once(';') else {
                        continue;
                    };
                    let Some(format) = ImageFormat::from_mime_type(mime_type) else {
                        continue;
                    };
                    let is_base64 = encoding == "base64";
                    if is_base64
                        && let Some(bytes) = base64::prelude::BASE64_STANDARD
                            .decode(data)
                            .log_with_level(Level::Debug)
                    {
                        let image = Arc::new(Image::from_bytes(format, bytes));
                        images_by_source_offset.insert(range.start, image);
                    }
                }
            }

            (
                ParsedMarkdown {
                    source,
                    events: Arc::from(events),
                    languages_by_name,
                    languages_by_path,
                    root_block_starts: Arc::from(root_block_starts),
                    html_blocks,
                    mermaid_diagrams,
                    heading_slugs,
                    footnote_definitions,
                },
                images_by_source_offset,
            )
        });

        cx.spawn(async move |this, cx| {
            let (parsed, images_by_source_offset) = parsed.await;

            this.update(cx, |this, cx| {
                this.parsed_markdown = parsed;
                this.images_by_source_offset = images_by_source_offset;
                if this.active_root_block.is_some_and(|block_index| {
                    block_index >= this.parsed_markdown.root_block_starts.len()
                }) {
                    this.active_root_block = None;
                }
                if this.options.render_mermaid_diagrams {
                    let parsed_markdown = this.parsed_markdown.clone();
                    this.mermaid_state.update(&parsed_markdown, cx);
                } else {
                    this.mermaid_state.clear();
                }
                this.pending_parse.take();
                if this.should_reparse {
                    this.parse(cx);
                }
                cx.notify();
                cx.refresh_windows();
            })
            .ok();
        })
    }
}

impl Focusable for Markdown {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Debug, Default, Clone)]
enum SelectMode {
    #[default]
    Character,
    Word(Range<usize>),
    Line(Range<usize>),
    All,
}

#[derive(Clone, Default)]
struct Selection {
    start: usize,
    end: usize,
    reversed: bool,
    pending: bool,
    mode: SelectMode,
}

impl Selection {
    fn set_head(&mut self, head: usize, rendered_text: &RenderedText) {
        match &self.mode {
            SelectMode::Character => {
                if head < self.tail() {
                    if !self.reversed {
                        self.end = self.start;
                        self.reversed = true;
                    }
                    self.start = head;
                } else {
                    if self.reversed {
                        self.start = self.end;
                        self.reversed = false;
                    }
                    self.end = head;
                }
            }
            SelectMode::Word(original_range) | SelectMode::Line(original_range) => {
                let head_range = if matches!(self.mode, SelectMode::Word(_)) {
                    rendered_text.surrounding_word_range(head)
                } else {
                    rendered_text.surrounding_line_range(head)
                };

                if head < original_range.start {
                    self.start = head_range.start;
                    self.end = original_range.end;
                    self.reversed = true;
                } else if head >= original_range.end {
                    self.start = original_range.start;
                    self.end = head_range.end;
                    self.reversed = false;
                } else {
                    self.start = original_range.start;
                    self.end = original_range.end;
                    self.reversed = false;
                }
            }
            SelectMode::All => {
                self.start = 0;
                self.end = rendered_text
                    .lines
                    .last()
                    .map(|line| line.source_end)
                    .unwrap_or(0);
                self.reversed = false;
            }
        }
    }

    fn tail(&self) -> usize {
        if self.reversed { self.end } else { self.start }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParsedMarkdown {
    pub source: SharedString,
    pub events: Arc<[(Range<usize>, MarkdownEvent)]>,
    pub languages_by_name: TreeMap<SharedString, Arc<Language>>,
    pub languages_by_path: TreeMap<Arc<str>, Arc<Language>>,
    pub root_block_starts: Arc<[usize]>,
    pub(crate) html_blocks: BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    pub(crate) mermaid_diagrams: BTreeMap<usize, ParsedMarkdownMermaidDiagram>,
    pub heading_slugs: HashMap<SharedString, usize>,
    pub footnote_definitions: HashMap<SharedString, usize>,
}

impl ParsedMarkdown {
    pub fn source(&self) -> &SharedString {
        &self.source
    }

    pub fn events(&self) -> &Arc<[(Range<usize>, MarkdownEvent)]> {
        &self.events
    }

    pub fn root_block_starts(&self) -> &Arc<[usize]> {
        &self.root_block_starts
    }

    pub fn root_block_for_source_index(&self, source_index: usize) -> Option<usize> {
        if self.root_block_starts.is_empty() {
            return None;
        }

        let partition = self
            .root_block_starts
            .partition_point(|block_start| *block_start <= source_index);

        Some(partition.saturating_sub(1))
    }
}

pub enum AutoscrollBehavior {
    /// Propagate the request up the element tree for the nearest
    /// scrollable ancestor (e.g. `List`) to handle.
    Propagate,
    /// Directly control a specific scroll handle.
    Controlled(ScrollHandle),
}

pub struct MarkdownElement {
    markdown: Entity<Markdown>,
    style: MarkdownStyle,
    code_block_renderer: CodeBlockRenderer,
    on_url_click: Option<Box<dyn Fn(SharedString, &mut Window, &mut App)>>,
    on_source_click: Option<SourceClickCallback>,
    on_checkbox_toggle: Option<CheckboxToggleCallback>,
    image_resolver: Option<Box<dyn Fn(&str) -> Option<ImageSource>>>,
    /// Given a link's destination (e.g. a wikilink page name), returns the path to
    /// an icon SVG to show before the link when it leads a table cell or list item.
    /// Rendered as crisp vector geometry rather than a rasterized image.
    link_icon_resolver: Option<Box<dyn Fn(&str) -> Option<PathBuf>>>,
    /// Given an emoji shortcode name (the `check` in `:check` / `:check:`),
    /// returns the path to an SVG to render inline in place of the shortcode.
    emoji_icon_resolver: Option<Box<dyn Fn(&str) -> Option<PathBuf>>>,
    show_root_block_markers: bool,
    autoscroll: AutoscrollBehavior,
}

/// Parses an emoji shortcode (`:name` or `:name:`) at the start of `token`,
/// returning the name and the shortcode's byte length. Names are restricted to
/// `[A-Za-z0-9_-]` so paths, URLs and `::` in prose don't parse as shortcodes.
fn parse_emoji_shortcode(token: &str) -> Option<(&str, usize)> {
    let rest = token.strip_prefix(':')?;
    let name_len = rest
        .char_indices()
        .find(|(_, char)| !(char.is_ascii_alphanumeric() || *char == '_' || *char == '-'))
        .map_or(rest.len(), |(index, _)| index);
    if name_len == 0 {
        return None;
    }
    let name = &rest[..name_len];
    let mut len = 1 + name_len;
    if rest[name_len..].starts_with(':') {
        len += 1;
    }
    Some((name, len))
}

impl MarkdownElement {
    pub fn new(markdown: Entity<Markdown>, style: MarkdownStyle) -> Self {
        Self {
            markdown,
            style,
            code_block_renderer: CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                border: false,
            },
            on_url_click: None,
            on_source_click: None,
            on_checkbox_toggle: None,
            image_resolver: None,
            link_icon_resolver: None,
            emoji_icon_resolver: None,
            show_root_block_markers: false,
            autoscroll: AutoscrollBehavior::Propagate,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn rendered_text(
        markdown: Entity<Markdown>,
        cx: &mut gpui::VisualTestContext,
        style: impl FnOnce(&Window, &App) -> MarkdownStyle,
    ) -> String {
        use gpui::size;

        let (text, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |window, cx| Self::new(markdown, style(window, cx)),
        );
        text.text
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn code_block_renderer(mut self, variant: CodeBlockRenderer) -> Self {
        self.code_block_renderer = variant;
        self
    }

    pub fn on_url_click(
        mut self,
        handler: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_url_click = Some(Box::new(handler));
        self
    }

    pub fn on_source_click(
        mut self,
        handler: impl Fn(usize, usize, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_source_click = Some(Box::new(handler));
        self
    }

    pub fn on_checkbox_toggle(
        mut self,
        handler: impl Fn(Range<usize>, bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_checkbox_toggle = Some(Rc::new(handler));
        self
    }

    pub fn image_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<ImageSource> + 'static,
    ) -> Self {
        self.image_resolver = Some(Box::new(resolver));
        self
    }

    pub fn link_icon_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<PathBuf> + 'static,
    ) -> Self {
        self.link_icon_resolver = Some(Box::new(resolver));
        self
    }

    /// If the cell/list-item content beginning at `start_index` leads with a link
    /// whose destination resolves to an icon, returns that icon as a small inline
    /// element (16px), so it can be placed before the link in the block container.
    fn leading_link_icon(
        &self,
        events: &[(Range<usize>, MarkdownEvent)],
        start_index: usize,
    ) -> Option<AnyElement> {
        let resolver = self.link_icon_resolver.as_ref()?;
        let mut in_paragraph = false;
        for (_, event) in events.get(start_index + 1..)?.iter() {
            match event {
                // The block leads with a link — use its icon (or none).
                MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. }) => {
                    // Inside a paragraph wrapper (a loose list item), an iconned
                    // leading link forces that paragraph to wrap into per-word
                    // boxes, and the link handler then injects the icon inline —
                    // placing it here too would show it twice.
                    if in_paragraph {
                        return None;
                    }
                    let icon_path = resolver(dest_url)?;
                    return svg_icon::render_page_icon(&icon_path, px(16.));
                }
                // Skip wrappers/markers that can precede the leading link (a loose
                // list wraps its content in a paragraph; task items emit a marker).
                MarkdownEvent::Start(MarkdownTag::Paragraph) => in_paragraph = true,
                MarkdownEvent::TaskListMarker(_) => continue,
                // End of the block, or any other leading content: no leading link.
                _ => break,
            }
        }
        None
    }

    /// Whether a link destination resolves to a page icon — used to decide
    /// whether a paragraph must wrap so the icon can flow inline before the link.
    fn link_has_icon(&self, dest_url: &str) -> bool {
        self.link_icon_resolver
            .as_ref()
            .and_then(|resolver| resolver(dest_url))
            .is_some()
    }

    pub fn emoji_icon_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<PathBuf> + 'static,
    ) -> Self {
        self.emoji_icon_resolver = Some(Box::new(resolver));
        self
    }

    /// Whether `text` contains an emoji shortcode that resolves to an icon —
    /// used to decide whether a paragraph/list item/table cell must wrap into
    /// per-word boxes so the icon can flow inline.
    fn text_has_emoji(&self, text: &str) -> bool {
        let Some(resolver) = self.emoji_icon_resolver.as_ref() else {
            return false;
        };
        let mut search = text;
        while let Some(position) = search.find(':') {
            if let Some((name, _)) = parse_emoji_shortcode(&search[position..])
                && resolver(name).is_some()
            {
                return true;
            }
            search = &search[position + 1..];
        }
        false
    }

    /// Emit `text`, replacing resolvable emoji shortcodes with their inline SVG
    /// icons. Only valid in a wrapping (per-word) container, where an inline
    /// element can flow between word boxes.
    fn push_text_with_emoji(
        &self,
        builder: &mut MarkdownElementBuilder,
        text: &str,
        range: Range<usize>,
    ) {
        let Some(resolver) = self.emoji_icon_resolver.as_ref() else {
            builder.push_text(text, range);
            return;
        };
        let mut emitted = 0;
        let mut search = 0;
        while let Some(colon) = text[search..].find(':') {
            let token_start = search + colon;
            if let Some((name, token_len)) = parse_emoji_shortcode(&text[token_start..])
                && let Some(icon_path) = resolver(name)
                && let Some(icon) = svg_icon::render_page_icon(&icon_path, px(16.))
            {
                if emitted < token_start {
                    builder.push_text(
                        &text[emitted..token_start],
                        range.start + emitted..range.start + token_start,
                    );
                }
                // Baseline-aligned flex puts the icon's bottom edge on the text
                // baseline; nudge it up so its visual center matches the text's
                // (the wikilink icons' -1.5 reads slightly low next to prose).
                let icon = div()
                    .flex_none()
                    .relative()
                    .top(px(-3.))
                    .child(icon)
                    .into_any_element();
                builder.push_inline_icon(
                    icon,
                    range.start + token_start..range.start + token_start + token_len,
                );
                emitted = token_start + token_len;
                search = emitted;
            } else {
                search = token_start + 1;
            }
        }
        if emitted < text.len() {
            builder.push_text(&text[emitted..], range.start + emitted..range.end);
        }
    }

    pub fn show_root_block_markers(mut self) -> Self {
        self.show_root_block_markers = true;
        self
    }

    pub fn scroll_handle(mut self, scroll_handle: ScrollHandle) -> Self {
        self.autoscroll = AutoscrollBehavior::Controlled(scroll_handle);
        self
    }

    fn push_markdown_image(
        &self,
        builder: &mut MarkdownElementBuilder,
        range: &Range<usize>,
        source: ImageSource,
        width: Option<DefiniteLength>,
        height: Option<DefiniteLength>,
    ) {
        let align = builder.text_style().text_align;
        builder.modify_current_div(|el| {
            let mut image_container = el.flex().flex_row().items_center();

            image_container = match align {
                TextAlign::Left => image_container.justify_start(),
                TextAlign::Center => image_container.justify_center(),
                TextAlign::Right => image_container.justify_end(),
            };

            image_container.child(
                img(source)
                    .id(("markdown-image", range.start))
                    .max_w_full()
                    // The default height cap clamps via aspect ratio, which would
                    // override an explicit `![[img|600]]` size — so only apply it
                    // when no explicit width/height was given.
                    .when(width.is_none() && height.is_none(), |this| {
                        this.when_some(self.style.image_max_height, |this, max_height| {
                            this.max_h(max_height)
                        })
                    })
                    .when_some(height, |this, height| this.h(height))
                    .when_some(width, |this, width| this.w(width)),
            )
        });
    }

    fn push_markdown_paragraph(
        &self,
        builder: &mut MarkdownElementBuilder,
        range: &Range<usize>,
        markdown_end: usize,
        text_align_override: Option<TextAlign>,
        wrap_inline: bool,
    ) {
        let align = text_align_override.unwrap_or(self.style.base_text_style.text_align);
        let in_footnote = builder.in_footnote;
        let mut paragraph = div().when(!self.style.height_is_multiple_of_line_height, |el| {
            // Footnote/source paragraphs stay tight (match the definition row's
            // line height, small bottom margin) so the sources list isn't spread
            // out by full body-paragraph spacing.
            if in_footnote {
                el.mb_1().line_height(rems(1.3))
            } else {
                el.mb_4().line_height(rems(1.5))
            }
        });

        // Paragraphs containing inline math become a wrapping flex row of
        // per-word elements (see `push_wrapped_words`), so the equation flows
        // inline and the line still wraps. Plain paragraphs keep the single
        // shaped-`StyledText` fast path with full kerning.
        if wrap_inline {
            paragraph = paragraph.flex().flex_row().flex_wrap().items_baseline();
        } else {
            paragraph = match align {
                TextAlign::Center => paragraph.text_center(),
                TextAlign::Left => paragraph.text_left(),
                TextAlign::Right => paragraph.text_right(),
            };
        }

        builder.push_text_style(TextStyleRefinement {
            text_align: Some(align),
            ..Default::default()
        });
        builder.push_div(paragraph, range, markdown_end);
        builder.wrap_words = wrap_inline;
    }

    fn pop_markdown_paragraph(&self, builder: &mut MarkdownElementBuilder) {
        builder.wrap_words = false;
        builder.pop_div();
        builder.pop_text_style();
    }

    fn push_markdown_heading(
        &self,
        builder: &mut MarkdownElementBuilder,
        level: pulldown_cmark::HeadingLevel,
        range: &Range<usize>,
        markdown_end: usize,
        text_align_override: Option<TextAlign>,
    ) {
        let align = text_align_override.unwrap_or(self.style.base_text_style.text_align);
        let mut heading = div().mt_4().mb_2();
        heading = apply_heading_style(heading, level, self.style.heading_level_styles.as_ref());

        heading = match align {
            TextAlign::Center => heading.text_center(),
            TextAlign::Left => heading.text_left(),
            TextAlign::Right => heading.text_right(),
        };

        let mut heading_style = self.style.heading.clone();
        let heading_text_style = heading_style.text_style().clone();
        heading.style().refine(&heading_style);

        builder.push_text_style(TextStyleRefinement {
            text_align: Some(align),
            ..heading_text_style
        });
        builder.push_div(heading, range, markdown_end);
    }

    fn pop_markdown_heading(&self, builder: &mut MarkdownElementBuilder) {
        builder.pop_div();
        builder.pop_text_style();
    }

    fn push_markdown_block_quote(
        &self,
        builder: &mut MarkdownElementBuilder,
        range: &Range<usize>,
        markdown_end: usize,
    ) {
        builder.push_text_style(self.style.block_quote.clone());
        builder.push_div(
            div()
                .pl_4()
                .mb_2()
                .border_l_4()
                .border_color(self.style.block_quote_border_color),
            range,
            markdown_end,
        );
    }

    fn pop_markdown_block_quote(&self, builder: &mut MarkdownElementBuilder) {
        builder.pop_div();
        builder.pop_text_style();
    }

    fn push_markdown_list_item(
        &self,
        builder: &mut MarkdownElementBuilder,
        bullet: AnyElement,
        icon: Option<AnyElement>,
        range: &Range<usize>,
        markdown_end: usize,
        wrap_inline: bool,
    ) {
        builder.push_div(
            div()
                .when(!self.style.height_is_multiple_of_line_height, |el| {
                    el.mb_1().gap_2().line_height(rems(1.5))
                })
                .h_flex()
                .items_start()
                .child(bullet)
                // Icon (if any) sits between the bullet and the item's text.
                .children(icon),
            range,
            markdown_end,
        );
        // Without `w_0`, text doesn't wrap to the width of the container.
        let mut content = div().flex_1().w_0();
        // A tight list item with inline math becomes a wrapping flex row of
        // per-word boxes so the equation flows inline (see `push_wrapped_words`).
        if wrap_inline {
            content = content.flex().flex_row().flex_wrap().items_baseline();
        }
        builder.push_div(content, range, markdown_end);
        builder.wrap_words = wrap_inline;
    }

    fn pop_markdown_list_item(&self, builder: &mut MarkdownElementBuilder) {
        builder.wrap_words = false;
        builder.pop_div();
        builder.pop_div();
    }

    fn paint_highlight_range(
        bounds: Bounds<Pixels>,
        start: usize,
        end: usize,
        color: Hsla,
        rendered_text: &RenderedText,
        window: &mut Window,
    ) {
        let start_pos = rendered_text.position_for_source_index(start);
        let end_pos = rendered_text.position_for_source_index(end);
        if let Some(((start_position, start_line_height), (end_position, end_line_height))) =
            start_pos.zip(end_pos)
        {
            if start_position.y == end_position.y {
                window.paint_quad(quad(
                    Bounds::from_corners(
                        start_position,
                        point(end_position.x, end_position.y + end_line_height),
                    ),
                    Pixels::ZERO,
                    color,
                    Edges::default(),
                    Hsla::transparent_black(),
                    BorderStyle::default(),
                ));
            } else {
                window.paint_quad(quad(
                    Bounds::from_corners(
                        start_position,
                        point(bounds.right(), start_position.y + start_line_height),
                    ),
                    Pixels::ZERO,
                    color,
                    Edges::default(),
                    Hsla::transparent_black(),
                    BorderStyle::default(),
                ));

                if end_position.y > start_position.y + start_line_height {
                    window.paint_quad(quad(
                        Bounds::from_corners(
                            point(bounds.left(), start_position.y + start_line_height),
                            point(bounds.right(), end_position.y),
                        ),
                        Pixels::ZERO,
                        color,
                        Edges::default(),
                        Hsla::transparent_black(),
                        BorderStyle::default(),
                    ));
                }

                window.paint_quad(quad(
                    Bounds::from_corners(
                        point(bounds.left(), end_position.y),
                        point(end_position.x, end_position.y + end_line_height),
                    ),
                    Pixels::ZERO,
                    color,
                    Edges::default(),
                    Hsla::transparent_black(),
                    BorderStyle::default(),
                ));
            }
        }
    }

    fn paint_selection(
        &self,
        bounds: Bounds<Pixels>,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) {
        let selection = self.markdown.read(cx).selection.clone();
        Self::paint_highlight_range(
            bounds,
            selection.start,
            selection.end,
            self.style.selection_background_color,
            rendered_text,
            window,
        );
    }

    fn paint_search_highlights(
        &self,
        bounds: Bounds<Pixels>,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) {
        let markdown = self.markdown.read(cx);
        let active_index = markdown.active_search_highlight;
        let colors = cx.theme().colors();

        for (i, highlight_range) in markdown.search_highlights.iter().enumerate() {
            let color = if Some(i) == active_index {
                colors.search_active_match_background
            } else {
                colors.search_match_background
            };
            Self::paint_highlight_range(
                bounds,
                highlight_range.start,
                highlight_range.end,
                color,
                rendered_text,
                window,
            );
        }
    }

    fn paint_mouse_listeners(
        &mut self,
        hitbox: &Hitbox,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.style.prevent_mouse_interaction {
            return;
        }

        let is_hovering_clickable = hitbox.is_hovered(window)
            && !self.markdown.read(cx).selection.pending
            && rendered_text
                .source_index_for_position(window.mouse_position())
                .ok()
                .is_some_and(|source_index| {
                    rendered_text.link_for_source_index(source_index).is_some()
                        || rendered_text
                            .footnote_ref_for_source_index(source_index)
                            .is_some()
                });

        if is_hovering_clickable {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        } else {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }

        let on_open_url = self.on_url_click.take();
        let on_source_click = self.on_source_click.take();

        self.on_mouse_event(window, cx, {
            let hitbox = hitbox.clone();
            let rendered_text = rendered_text.clone();
            move |markdown, event: &MouseDownEvent, phase, window, _cx| {
                if phase.capture()
                    && event.button == MouseButton::Right
                    && hitbox.is_hovered(window)
                {
                    let link = rendered_text
                        .source_index_for_position(event.position)
                        .ok()
                        .and_then(|ix| rendered_text.link_for_source_index(ix))
                        .map(|link| link.destination_url.clone());
                    markdown.capture_for_context_menu(link);
                }
            }
        });

        self.on_mouse_event(window, cx, {
            let rendered_text = rendered_text.clone();
            let hitbox = hitbox.clone();
            move |markdown, event: &MouseDownEvent, phase, window, cx| {
                if hitbox.is_hovered(window) {
                    if phase.bubble() && event.button != MouseButton::Right {
                        let position_result =
                            rendered_text.source_index_for_position(event.position);

                        if let Ok(source_index) = position_result {
                            if let Some(footnote_ref) =
                                rendered_text.footnote_ref_for_source_index(source_index)
                            {
                                markdown.pressed_footnote_ref = Some(footnote_ref.clone());
                            } else if let Some(link) =
                                rendered_text.link_for_source_index(source_index)
                            {
                                markdown.pressed_link = Some(link.clone());
                            }
                        }

                        if markdown.pressed_footnote_ref.is_none()
                            && markdown.pressed_link.is_none()
                        {
                            let source_index = match position_result {
                                Ok(ix) | Err(ix) => ix,
                            };
                            if let Some(handler) = on_source_click.as_ref() {
                                let blocked = handler(source_index, event.click_count, window, cx);
                                if blocked {
                                    markdown.selection = Selection::default();
                                    markdown.pressed_link = None;
                                    window.prevent_default();
                                    cx.notify();
                                    return;
                                }
                            }
                            let (range, mode) = match event.click_count {
                                1 => {
                                    let range = source_index..source_index;
                                    (range, SelectMode::Character)
                                }
                                2 => {
                                    let range = rendered_text.surrounding_word_range(source_index);
                                    (range.clone(), SelectMode::Word(range))
                                }
                                3 => {
                                    let range = rendered_text.surrounding_line_range(source_index);
                                    (range.clone(), SelectMode::Line(range))
                                }
                                _ => {
                                    let range = 0..rendered_text
                                        .lines
                                        .last()
                                        .map(|line| line.source_end)
                                        .unwrap_or(0);
                                    (range, SelectMode::All)
                                }
                            };
                            markdown.selection = Selection {
                                start: range.start,
                                end: range.end,
                                reversed: false,
                                pending: true,
                                mode,
                            };
                            window.focus(&markdown.focus_handle, cx);
                        }

                        window.prevent_default();
                        cx.notify();
                    }
                } else if phase.capture() && event.button == MouseButton::Left {
                    markdown.selection = Selection::default();
                    markdown.pressed_link = None;
                    cx.notify();
                }
            }
        });
        self.on_mouse_event(window, cx, {
            let rendered_text = rendered_text.clone();
            let hitbox = hitbox.clone();
            let was_hovering_clickable = is_hovering_clickable;
            move |markdown, event: &MouseMoveEvent, phase, window, cx| {
                if phase.capture() {
                    return;
                }

                if markdown.selection.pending {
                    let source_index = match rendered_text.source_index_for_position(event.position)
                    {
                        Ok(ix) | Err(ix) => ix,
                    };
                    markdown.selection.set_head(source_index, &rendered_text);
                    markdown.autoscroll_code_block(source_index, event.position);
                    markdown.autoscroll_request = Some(source_index);
                    cx.notify();
                } else {
                    let is_hovering_clickable = hitbox.is_hovered(window)
                        && rendered_text
                            .source_index_for_position(event.position)
                            .ok()
                            .is_some_and(|source_index| {
                                rendered_text.link_for_source_index(source_index).is_some()
                                    || rendered_text
                                        .footnote_ref_for_source_index(source_index)
                                        .is_some()
                            });
                    if is_hovering_clickable != was_hovering_clickable {
                        cx.notify();
                    }
                }
            }
        });
        self.on_mouse_event(window, cx, {
            let rendered_text = rendered_text.clone();
            move |markdown, event: &MouseUpEvent, phase, window, cx| {
                if phase.bubble() {
                    let source_index = rendered_text.source_index_for_position(event.position).ok();
                    if let Some(pressed_footnote_ref) = markdown.pressed_footnote_ref.take()
                        && source_index
                            .and_then(|ix| rendered_text.footnote_ref_for_source_index(ix))
                            == Some(&pressed_footnote_ref)
                    {
                        if let Some(source_index) =
                            markdown.footnote_definition_content_start(&pressed_footnote_ref.label)
                        {
                            markdown.autoscroll_request = Some(source_index);
                            cx.notify();
                        }
                    } else if let Some(pressed_link) = markdown.pressed_link.take()
                        && source_index.and_then(|ix| rendered_text.link_for_source_index(ix))
                            == Some(&pressed_link)
                    {
                        if let Some(open_url) = on_open_url.as_ref() {
                            open_url(pressed_link.destination_url, window, cx);
                        } else {
                            cx.open_url(&pressed_link.destination_url);
                        }
                    }
                } else if markdown.selection.pending {
                    markdown.selection.pending = false;
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    {
                        let text = rendered_text
                            .text_for_range(markdown.selection.start..markdown.selection.end);
                        cx.write_to_primary(ClipboardItem::new_string(text))
                    }
                    cx.notify();
                }
            }
        });
    }

    fn autoscroll(
        &self,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<()> {
        let autoscroll_index = self
            .markdown
            .update(cx, |markdown, _| markdown.autoscroll_request.take())?;
        let (position, line_height) = rendered_text.position_for_source_index(autoscroll_index)?;

        match &self.autoscroll {
            AutoscrollBehavior::Controlled(scroll_handle) => {
                let viewport = scroll_handle.bounds();
                let margin = line_height * 3.;
                let top_goal = viewport.top() + margin;
                let bottom_goal = viewport.bottom() - margin;
                let current_offset = scroll_handle.offset();

                let new_offset_y = if position.y < top_goal {
                    current_offset.y + (top_goal - position.y)
                } else if position.y + line_height > bottom_goal {
                    current_offset.y + (bottom_goal - (position.y + line_height))
                } else {
                    current_offset.y
                };

                scroll_handle.set_offset(point(
                    current_offset.x,
                    new_offset_y.clamp(-scroll_handle.max_offset().y, Pixels::ZERO),
                ));
            }
            AutoscrollBehavior::Propagate => {
                let text_style = self.style.base_text_style.clone();
                let font_id = window.text_system().resolve_font(&text_style.font());
                let font_size = text_style.font_size.to_pixels(window.rem_size());
                let em_width = window.text_system().em_width(font_id, font_size).unwrap();
                window.request_autoscroll(Bounds::from_corners(
                    point(position.x - 3. * em_width, position.y - 3. * line_height),
                    point(position.x + 3. * em_width, position.y + 3. * line_height),
                ));
            }
        }
        Some(())
    }

    fn on_mouse_event<T: MouseEvent>(
        &self,
        window: &mut Window,
        _cx: &mut App,
        mut f: impl 'static
        + FnMut(&mut Markdown, &T, DispatchPhase, &mut Window, &mut Context<Markdown>),
    ) {
        window.on_mouse_event({
            let markdown = self.markdown.downgrade();
            move |event, phase, window, cx| {
                markdown
                    .update(cx, |markdown, cx| f(markdown, event, phase, window, cx))
                    .log_err();
            }
        });
    }
}

impl Styled for MarkdownElement {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style.container_style
    }
}

impl Element for MarkdownElement {
    type RequestLayoutState = RenderedMarkdown;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut builder = MarkdownElementBuilder::new(
            &self.style.container_style,
            self.style.base_text_style.clone(),
            self.style.syntax.clone(),
        );
        let (parsed_markdown, images, active_root_block, render_mermaid_diagrams, mermaid_state) = {
            let markdown = self.markdown.read(cx);
            (
                markdown.parsed_markdown.clone(),
                markdown.images_by_source_offset.clone(),
                markdown.active_root_block,
                markdown.options.render_mermaid_diagrams,
                markdown.mermaid_state.clone(),
            )
        };
        let markdown_end = if let Some(last) = parsed_markdown.events.last() {
            last.0.end
        } else {
            0
        };
        let mut code_block_ids = HashSet::default();
        let mut table_ids = HashSet::default();

        let mut current_img_block_range: Option<Range<usize>> = None;
        let mut handled_html_block = false;
        let mut rendered_mermaid_block = false;
        // A `<center>`/`</center>` HTML block toggles centering instead of
        // pushing a div, so its matching `End(HtmlBlock)` must skip the pop.
        let mut skip_html_block_pop = false;
        for (index, (range, event)) in parsed_markdown.events.iter().enumerate() {
            // Skip alt text for images that rendered
            if let Some(current_img_block_range) = &current_img_block_range
                && current_img_block_range.end > range.end
            {
                continue;
            }

            if handled_html_block {
                if let MarkdownEvent::End(MarkdownTagEnd::HtmlBlock) = event {
                    handled_html_block = false;
                } else {
                    continue;
                }
            }

            if rendered_mermaid_block {
                if matches!(event, MarkdownEvent::End(MarkdownTagEnd::CodeBlock)) {
                    rendered_mermaid_block = false;
                }
                continue;
            }

            match event {
                MarkdownEvent::RootStart => {
                    if self.show_root_block_markers {
                        builder.push_root_block(range, markdown_end);
                    }
                }
                MarkdownEvent::RootEnd(root_block_index) => {
                    if self.show_root_block_markers {
                        builder.pop_root_block(
                            active_root_block == Some(*root_block_index),
                            cx.theme().colors().border,
                            cx.theme().colors().border_variant,
                        );
                    }
                }
                MarkdownEvent::Start(tag) => {
                    match tag {
                        MarkdownTag::Image {
                            dest_url,
                            link_type,
                            ..
                        } => {
                            // Obsidian-style size: `![[image|600]]` / `![[image|600x400]]`.
                            let (width, height) = image_size_override(
                                link_type,
                                &parsed_markdown.events,
                                index,
                                &parsed_markdown.source,
                            );
                            if let Some(image) = images.get(&range.start) {
                                current_img_block_range = Some(range.clone());
                                self.push_markdown_image(
                                    &mut builder,
                                    range,
                                    image.clone().into(),
                                    width,
                                    height,
                                );
                            } else if let Some(source) = self
                                .image_resolver
                                .as_ref()
                                .and_then(|resolve| resolve(dest_url.as_ref()))
                            {
                                current_img_block_range = Some(range.clone());
                                self.push_markdown_image(&mut builder, range, source, width, height);
                            }
                        }
                        MarkdownTag::Paragraph => {
                            // Wrap into per-word boxes when the paragraph must flow
                            // an inline element: inline math, a wikilink whose
                            // page icon is shown before it, or an emoji shortcode.
                            let wrap_inline = parsed_markdown.events[index + 1..]
                                .iter()
                                .take_while(|(_, event)| {
                                    !matches!(
                                        event,
                                        MarkdownEvent::End(MarkdownTagEnd::Paragraph)
                                    )
                                })
                                .any(|(event_range, event)| match event {
                                    MarkdownEvent::InlineMath(_) => true,
                                    MarkdownEvent::Code => self.style.inline_code_box,
                                    MarkdownEvent::Start(MarkdownTag::Link { dest_url, .. }) => {
                                        self.link_has_icon(dest_url)
                                    }
                                    MarkdownEvent::Text => self.text_has_emoji(
                                        &parsed_markdown.source[event_range.clone()],
                                    ),
                                    _ => false,
                                });
                            self.push_markdown_paragraph(
                                &mut builder,
                                range,
                                markdown_end,
                                None,
                                wrap_inline,
                            );
                        }
                        MarkdownTag::Heading { level, .. } => {
                            self.push_markdown_heading(
                                &mut builder,
                                *level,
                                range,
                                markdown_end,
                                None,
                            );
                        }
                        MarkdownTag::BlockQuote => {
                            self.push_markdown_block_quote(&mut builder, range, markdown_end);
                        }
                        MarkdownTag::CodeBlock { kind, .. } => {
                            if render_mermaid_diagrams
                                && let Some(mermaid_diagram) =
                                    parsed_markdown.mermaid_diagrams.get(&range.start)
                            {
                                builder.push_sourced_element(
                                    mermaid_diagram.content_range.clone(),
                                    render_mermaid_diagram(
                                        mermaid_diagram,
                                        &mermaid_state,
                                        &self.style,
                                    ),
                                );
                                rendered_mermaid_block = true;
                                continue;
                            }

                            let language = match kind {
                                CodeBlockKind::Fenced => None,
                                CodeBlockKind::FencedLang(language) => {
                                    parsed_markdown.languages_by_name.get(language).cloned()
                                }
                                CodeBlockKind::FencedSrc(path_range) => parsed_markdown
                                    .languages_by_path
                                    .get(&path_range.path)
                                    .cloned(),
                                _ => None,
                            };

                            let is_indented = matches!(kind, CodeBlockKind::Indented);
                            let scroll_handle = if self.style.code_block_overflow_x_scroll {
                                code_block_ids.insert(range.start);
                                Some(self.markdown.update(cx, |markdown, _| {
                                    markdown.code_block_scroll_handle(range.start)
                                }))
                            } else {
                                None
                            };

                            match (&self.code_block_renderer, is_indented) {
                                (CodeBlockRenderer::Default { .. }, _) | (_, true) => {
                                    // This is a parent container that we can position the copy button inside.
                                    let parent_container =
                                        div().group("code_block").relative().w_full();

                                    let mut parent_container: AnyDiv = if let Some(scroll_handle) =
                                        scroll_handle.as_ref()
                                    {
                                        let scrollbars = Scrollbars::new(ScrollAxes::Horizontal)
                                            .id(("markdown-code-block-scrollbar", range.start))
                                            .tracked_scroll_handle(scroll_handle)
                                            .with_track_along(
                                                ScrollAxes::Horizontal,
                                                cx.theme().colors().editor_background,
                                            )
                                            .notify_content();

                                        parent_container
                                            .rounded_lg()
                                            .custom_scrollbars(scrollbars, window, cx)
                                            .into()
                                    } else {
                                        parent_container.into()
                                    };

                                    if let CodeBlockRenderer::Default { border: true, .. } =
                                        &self.code_block_renderer
                                    {
                                        parent_container = parent_container
                                            .rounded_md()
                                            .border_1()
                                            .border_color(cx.theme().colors().border_variant);
                                    }

                                    parent_container.style().refine(&self.style.code_block);
                                    builder.push_div(parent_container, range, markdown_end);

                                    let code_block = div()
                                        .id(("code-block", range.start))
                                        .rounded_lg()
                                        .map(|mut code_block| {
                                            if let Some(scroll_handle) = scroll_handle.as_ref() {
                                                code_block.style().restrict_scroll_to_axis =
                                                    Some(true);
                                                code_block
                                                    .flex()
                                                    .overflow_x_scroll()
                                                    .track_scroll(scroll_handle)
                                            } else {
                                                code_block.w_full()
                                            }
                                        });

                                    builder.push_text_style(self.style.code_block.text.to_owned());
                                    builder.push_code_block(language);
                                    builder.push_div(code_block, range, markdown_end);
                                }
                                (CodeBlockRenderer::Custom { .. }, _) => {}
                            }
                        }
                        MarkdownTag::HtmlBlock => {
                            // `<center>` / `</center>` (Obsidian-compatible) toggle
                            // centering for the markdown blocks between them, rather
                            // than rendering as an HTML element. The content keeps its
                            // normal markdown pipeline, so inline/display math renders.
                            let raw = parsed_markdown.source[range.clone()].trim();
                            let lower = raw.to_ascii_lowercase();
                            // Only a lone tag on its own line is a directive; a
                            // multi-line block means the user omitted the blank
                            // lines, so fall through and render it as HTML.
                            let single_line = !raw.contains('\n');
                            if single_line && lower.starts_with("<center") {
                                // Center the block as a group while keeping its
                                // lines left-aligned: a full-width row centers a
                                // content-width column. Both divs stay open until
                                // `</center>`; the matching End(HtmlBlock) skips
                                // the default pop.
                                builder.push_div(
                                    div().w_full().flex().flex_row().justify_center(),
                                    range,
                                    markdown_end,
                                );
                                builder.push_div(
                                    div().flex().flex_col().items_start(),
                                    range,
                                    markdown_end,
                                );
                                builder.open_centers += 1;
                                handled_html_block = true;
                                skip_html_block_pop = true;
                            } else if single_line
                                && lower.starts_with("</center")
                                && builder.open_centers > 0
                            {
                                builder.pop_div(); // inner left-aligned column
                                builder.pop_div(); // outer centering row
                                builder.open_centers -= 1;
                                handled_html_block = true;
                                skip_html_block_pop = true;
                            } else {
                                builder.push_div(div(), range, markdown_end);
                                if let Some(block) =
                                    parsed_markdown.html_blocks.get(&range.start)
                                {
                                    self.render_html_block(block, &mut builder, markdown_end, cx);
                                    handled_html_block = true;
                                }
                            }
                        }
                        MarkdownTag::List(bullet_index) => {
                            builder.push_list(*bullet_index);
                            // Inside a wrapping item row (an item whose own line
                            // carries a boxed chip or inline math), a nested list
                            // is a flex sibling of the per-word boxes; without
                            // full width it flows beside them on the same row
                            // instead of breaking below.
                            let mut list = div().pl_2p5();
                            if builder.wrap_words {
                                list = list.w_full();
                            }
                            builder.push_div(list, range, markdown_end);
                        }
                        MarkdownTag::Item => {
                            let bullet =
                                if let Some((task_range, MarkdownEvent::TaskListMarker(checked))) =
                                    parsed_markdown.events.get(index.saturating_add(1))
                                {
                                    let source = &parsed_markdown.source()[range.clone()];
                                    let checked = *checked;
                                    let toggle_state = if checked {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    };

                                    let checkbox = Checkbox::new(
                                        ElementId::Name(source.to_string().into()),
                                        toggle_state,
                                    )
                                    .fill();

                                    if let Some(on_toggle) = self.on_checkbox_toggle.clone() {
                                        let task_source_range = task_range.clone();
                                        checkbox
                                            .on_click(move |_state, window, cx| {
                                                on_toggle(
                                                    task_source_range.clone(),
                                                    !checked,
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .into_any_element()
                                    } else {
                                        checkbox.visualization_only(true).into_any_element()
                                    }
                                } else if let Some(bullet_index) = builder.next_bullet_index() {
                                    div().child(format!("{}.", bullet_index)).into_any_element()
                                } else {
                                    // Vary the marker by nesting depth, like
                                    // Notion: solid dot, hollow circle, square.
                                    match builder.list_stack.len().saturating_sub(1) % 3 {
                                        1 => div().child("◦").into_any_element(),
                                        2 => div().child("▪").into_any_element(),
                                        _ => div()
                                            .font_weight(FontWeight::BOLD)
                                            .child("•")
                                            .into_any_element(),
                                    }
                                };
                            // Detect inline math (or, when `inline_code_box` is
                            // on, inline code) directly in this item (tight list),
                            // excluding any nested in a child block like a
                            // paragraph or sublist (those handle their own
                            // wrapping). Only direct inline elements need the
                            // item's content row to become a wrapping flex row.
                            let mut depth = 0usize;
                            let mut item_has_inline_math = false;
                            for (event_range, event) in &parsed_markdown.events[index + 1..] {
                                match event {
                                    MarkdownEvent::InlineMath(_) if depth == 0 => {
                                        item_has_inline_math = true;
                                        break;
                                    }
                                    MarkdownEvent::Code
                                        if depth == 0 && self.style.inline_code_box =>
                                    {
                                        item_has_inline_math = true;
                                        break;
                                    }
                                    MarkdownEvent::Text
                                        if depth == 0
                                            && self.text_has_emoji(
                                                &parsed_markdown.source[event_range.clone()],
                                            ) =>
                                    {
                                        item_has_inline_math = true;
                                        break;
                                    }
                                    // Inline formatting (bold/italic/strikethrough/
                                    // links) is transparent here: code or math wrapped
                                    // in it still belongs to the item's own line. Only
                                    // child *blocks* (sub-paragraph, sublist) increase
                                    // depth and so are skipped.
                                    MarkdownEvent::Start(
                                        MarkdownTag::Emphasis
                                        | MarkdownTag::Strong
                                        | MarkdownTag::Strikethrough
                                        | MarkdownTag::Link { .. }
                                        | MarkdownTag::Image { .. },
                                    )
                                    | MarkdownEvent::End(
                                        MarkdownTagEnd::Emphasis
                                        | MarkdownTagEnd::Strong
                                        | MarkdownTagEnd::Strikethrough
                                        | MarkdownTagEnd::Link
                                        | MarkdownTagEnd::Image,
                                    ) => {}
                                    MarkdownEvent::Start(_) => depth += 1,
                                    MarkdownEvent::End(MarkdownTagEnd::Item) if depth == 0 => {
                                        break;
                                    }
                                    MarkdownEvent::End(_) => depth = depth.saturating_sub(1),
                                    _ => {}
                                }
                            }
                            // A wrapping item injects its leading link's icon inline
                            // (via the link handler), so don't also place it here.
                            let icon = if item_has_inline_math {
                                None
                            } else {
                                self.leading_link_icon(&parsed_markdown.events, index)
                            };
                            self.push_markdown_list_item(
                                &mut builder,
                                bullet,
                                icon,
                                range,
                                markdown_end,
                                item_has_inline_math,
                            );
                        }
                        MarkdownTag::Emphasis => builder.push_text_style(TextStyleRefinement {
                            font_style: Some(FontStyle::Italic),
                            ..Default::default()
                        }),
                        MarkdownTag::Strong => builder.push_text_style(TextStyleRefinement {
                            font_weight: Some(FontWeight::BOLD),
                            ..Default::default()
                        }),
                        MarkdownTag::Strikethrough => {
                            builder.push_text_style(TextStyleRefinement {
                                strikethrough: Some(StrikethroughStyle {
                                    thickness: px(1.),
                                    color: None,
                                }),
                                ..Default::default()
                            })
                        }
                        MarkdownTag::Link { dest_url, .. } => {
                            if builder.code_block_stack.is_empty() {
                                builder.push_link(dest_url.clone(), range.clone());
                                // Show the target page's icon inline before the
                                // link. Only in paragraphs that wrap into per-word
                                // boxes (so it flows and the line still wraps);
                                // leading icons in lists/tables are handled by
                                // `leading_link_icon`. Never inside a wrapping
                                // table cell: an inline icon element next to a
                                // chip destabilizes the grid's row-height
                                // measurement — the final layout wraps a word the
                                // measured row never budgeted for, and the table
                                // clips the overflow (its rounded corners clip).
                                if builder.wrap_words
                                    && builder.table.alignments.is_empty()
                                    && let Some(resolver) = self.link_icon_resolver.as_ref()
                                    && let Some(icon_path) = resolver(dest_url)
                                    && let Some(icon) =
                                        svg_icon::render_page_icon(&icon_path, px(16.))
                                {
                                    builder.modify_current_div(|el| {
                                        el.child(
                                            div()
                                                .flex_none()
                                                .mr_1()
                                                .relative()
                                                .top(px(-1.5))
                                                .child(icon),
                                        )
                                    });
                                }
                                let style = self
                                    .style
                                    .link_callback
                                    .as_ref()
                                    .and_then(|callback| callback(dest_url, cx))
                                    .unwrap_or_else(|| self.style.link.clone());
                                builder.push_text_style(style)
                            }
                        }
                        MarkdownTag::FootnoteDefinition(label) => {
                            builder.in_footnote = true;
                            if !builder.rendered_footnote_separator {
                                builder.rendered_footnote_separator = true;
                                builder.push_div(
                                    div()
                                        .border_t_1()
                                        .mt_2()
                                        .border_color(self.style.rule_color),
                                    range,
                                    markdown_end,
                                );
                                builder.pop_div();
                            }
                            builder.push_div(
                                div()
                                    .pt_1()
                                    .mb_1()
                                    .line_height(rems(1.3))
                                    .text_size(rems(1.1))
                                    .h_flex()
                                    .items_start()
                                    .gap_2()
                                    .child(
                                        div().text_size(rems(1.1)).child(format!("{}.", label)),
                                    ),
                                range,
                                markdown_end,
                            );
                            builder.push_div(div().flex_1().w_0(), range, markdown_end);
                        }
                        MarkdownTag::MetadataBlock(_) => {}
                        MarkdownTag::Table(alignments) => {
                            builder.table.start(alignments.clone());

                            let column_count = alignments.len() as u16;
                            let table = div()
                                .id(("table", range.start))
                                .grid()
                                .border(px(1.5))
                                .border_color(cx.theme().colors().border)
                                .rounded_sm()
                                .overflow_hidden();
                            if self.style.table_size_to_content {
                                // `auto` grid tracks size each column to its content
                                // (min-content floor), like an HTML table — columns never
                                // collapse below their content and clip it, the failure
                                // mode of zero-minimum `max-content` tracks whenever taffy
                                // under-measures the grid's intrinsic width.
                                //
                                // The scroll viewport is a plain full-width block — never
                                // a flex item, so its width is definite rather than an
                                // intrinsic measurement of the scroll content (which taffy
                                // gets wrong, squeezing the table). The grid must be the
                                // viewport's *direct* child: the scrollable extent is
                                // computed from direct children's bounds, and any block
                                // wrapper in between is laid out at the viewport width, so
                                // the extent came out zero and the table couldn't scroll at
                                // all. As a `flex_none` flex item the grid keeps its
                                // natural (max-content) width; `mx_auto` centers a table
                                // that fits (auto margins only absorb positive free space).
                                // No scrollbar — scrolling is trackpad/wheel only.
                                table_ids.insert(range.start);
                                let scroll_handle = self.markdown.update(cx, |markdown, _| {
                                    markdown.table_scroll_handle(range.start)
                                });
                                builder.push_div(
                                    div()
                                        .id(("table-scroll", range.start))
                                        .w_full()
                                        .mb_4()
                                        .flex()
                                        .flex_row()
                                        .overflow_x_scroll()
                                        .track_scroll(&scroll_handle)
                                        .map(|mut this| {
                                            this.style().restrict_scroll_to_axis = Some(true);
                                            this
                                        }),
                                    range,
                                    markdown_end,
                                );
                                builder.push_div(
                                    table.grid_cols_auto(column_count).flex_none().mx_auto(),
                                    range,
                                    markdown_end,
                                );
                            } else {
                                builder.push_div(
                                    table.mb_4().grid_cols(column_count).w_full(),
                                    range,
                                    markdown_end,
                                );
                            }
                        }
                        MarkdownTag::TableHead => {
                            builder.table.start_head();
                            builder.push_text_style(self.style.table_header_text.clone());
                        }
                        MarkdownTag::TableRow => {
                            builder.table.start_row();
                        }
                        MarkdownTag::TableCell => {
                            let is_header = builder.table.in_head;
                            let row_index = builder.table.row_index;
                            let col_index = builder.table.col_index;
                            // A cell whose content includes inline code (when boxed)
                            // or inline math must wrap into per-word boxes so those
                            // elements flow inline and take their own size. The grid's
                            // `auto` tracks measure such a flex-wrap cell correctly
                            // (max-content = one line, min-content = widest box), so
                            // columns still size to the cell's content.
                            let cell_wraps = parsed_markdown.events[index + 1..]
                                .iter()
                                .take_while(|(_, event)| {
                                    !matches!(
                                        event,
                                        MarkdownEvent::End(MarkdownTagEnd::TableCell)
                                    )
                                })
                                .any(|(event_range, event)| match event {
                                    MarkdownEvent::InlineMath(_) => true,
                                    MarkdownEvent::Code => self.style.inline_code_box,
                                    MarkdownEvent::Text => self.text_has_emoji(
                                        &parsed_markdown.source[event_range.clone()],
                                    ),
                                    _ => false,
                                });
                            // Wrapping cells render links without icons entirely
                            // (see the Link handler): an icon element in the
                            // per-word flow destabilizes row-height measurement.
                            let icon = if cell_wraps {
                                None
                            } else {
                                self.leading_link_icon(&parsed_markdown.events, index)
                            };

                            builder.push_div(
                                div()
                                    .when(col_index > 0, |this| this.border_l_1())
                                    .when(row_index > 0, |this| this.border_t_1())
                                    .border_color(cx.theme().colors().border)
                                    .px_1()
                                    .py_0p5()
                                    .when(is_header, |this| {
                                        this.bg(cx.theme().colors().title_bar_background)
                                    })
                                    .when(!is_header && row_index % 2 == 1, |this| {
                                        this.bg(cx.theme().colors().panel_background)
                                    })
                                    .when(cell_wraps, |this| {
                                        this.flex().flex_row().flex_wrap().items_baseline()
                                    })
                                    // A cell that leads with an iconned link becomes a
                                    // flex row so the icon sits before the link text.
                                    .when_some(icon, |this, icon| {
                                        this.flex().flex_row().items_center().gap_1().child(icon)
                                    }),
                                range,
                                markdown_end,
                            );
                            builder.wrap_words = cell_wraps;
                        }
                        _ => log::debug!("unsupported markdown tag {:?}", tag),
                    }
                }
                MarkdownEvent::End(tag) => match tag {
                    MarkdownTagEnd::Image => {
                        current_img_block_range.take();
                    }
                    MarkdownTagEnd::Paragraph => {
                        self.pop_markdown_paragraph(&mut builder);
                    }
                    MarkdownTagEnd::Heading(_) => {
                        self.pop_markdown_heading(&mut builder);
                    }
                    MarkdownTagEnd::BlockQuote(_kind) => {
                        self.pop_markdown_block_quote(&mut builder);
                    }
                    MarkdownTagEnd::CodeBlock => {
                        builder.trim_trailing_newline();

                        builder.pop_div();
                        builder.pop_code_block();
                        builder.pop_text_style();

                        if let CodeBlockRenderer::Default {
                            copy_button_visibility,
                            ..
                        } = &self.code_block_renderer
                            && *copy_button_visibility != CopyButtonVisibility::Hidden
                        {
                            builder.modify_current_div(|el| {
                                let content_range = parser::extract_code_block_content_range(
                                    &parsed_markdown.source()[range.clone()],
                                );
                                let content_range = content_range.start + range.start
                                    ..content_range.end + range.start;

                                let code = parsed_markdown.source()[content_range].to_string();
                                let codeblock = render_copy_code_block_button(
                                    range.end,
                                    code,
                                    self.markdown.clone(),
                                );
                                el.child(
                                    h_flex()
                                        .w_4()
                                        .absolute()
                                        .justify_end()
                                        .when_else(
                                            *copy_button_visibility
                                                == CopyButtonVisibility::VisibleOnHover,
                                            |this| {
                                                this.top_0()
                                                    .right_0()
                                                    .visible_on_hover("code_block")
                                            },
                                            |this| this.top_1p5().right_1p5(),
                                        )
                                        .child(codeblock),
                                )
                            });
                        }

                        // Pop the parent container.
                        builder.pop_div();
                    }
                    MarkdownTagEnd::HtmlBlock => {
                        if skip_html_block_pop {
                            skip_html_block_pop = false;
                        } else {
                            builder.pop_div();
                        }
                    }
                    MarkdownTagEnd::List(_) => {
                        builder.pop_list();
                        builder.pop_div();
                    }
                    MarkdownTagEnd::Item => {
                        self.pop_markdown_list_item(&mut builder);
                    }
                    MarkdownTagEnd::Emphasis => builder.pop_text_style(),
                    MarkdownTagEnd::Strong => builder.pop_text_style(),
                    MarkdownTagEnd::Strikethrough => builder.pop_text_style(),
                    MarkdownTagEnd::Link => {
                        if builder.code_block_stack.is_empty() {
                            builder.pop_text_style()
                        }
                    }
                    MarkdownTagEnd::Table => {
                        builder.pop_div(); // table grid
                        if self.style.table_size_to_content {
                            // Pop the scroll viewport pushed in `MarkdownTag::Table`.
                            builder.pop_div();
                        }
                        builder.table.end();
                    }
                    MarkdownTagEnd::TableHead => {
                        builder.pop_text_style();
                        builder.table.end_head();
                    }
                    MarkdownTagEnd::TableRow => {
                        builder.table.end_row();
                    }
                    MarkdownTagEnd::TableCell => {
                        builder.replace_pending_checkbox(range);
                        builder.wrap_words = false;
                        builder.pop_div();
                        builder.table.end_cell();
                    }
                    MarkdownTagEnd::FootnoteDefinition => {
                        builder.pop_div();
                        builder.pop_div();
                        builder.in_footnote = false;
                    }
                    _ => log::debug!("unsupported markdown tag end: {:?}", tag),
                },
                MarkdownEvent::Text => {
                    let text = &parsed_markdown.source[range.clone()];
                    // Inline emoji icons can only flow between word boxes, so
                    // they're limited to wrapping containers (which the block
                    // scans above enable whenever text contains a shortcode).
                    if builder.wrap_words {
                        self.push_text_with_emoji(&mut builder, text, range.clone());
                    } else {
                        builder.push_text(text, range.clone());
                    }
                }
                MarkdownEvent::SubstitutedText(text) => {
                    builder.push_text(text, range.clone());
                }
                MarkdownEvent::Code => {
                    // In a wrapping (per-word) line, render inline code as its own
                    // box so it can take `inline_code.font_size` — a shaped line is
                    // locked to one size, so the run-level size is otherwise lost.
                    if builder.wrap_words && self.style.inline_code_box {
                        builder.push_inline_code_box(
                            &parsed_markdown.source[range.clone()],
                            range.clone(),
                            &self.style.inline_code,
                        );
                    } else {
                        builder.push_text_style(self.style.inline_code.clone());
                        builder.push_text(&parsed_markdown.source[range.clone()], range.clone());
                        builder.pop_text_style();
                    }
                }
                MarkdownEvent::Html => {
                    let html = &parsed_markdown.source[range.clone()];
                    if html.starts_with("<!--") {
                        builder.html_comment = true;
                    }
                    if html.trim_end().ends_with("-->") {
                        builder.html_comment = false;
                        continue;
                    }
                    if builder.html_comment {
                        continue;
                    }
                    builder.push_text(html, range.clone());
                }
                MarkdownEvent::InlineHtml => {
                    let html = &parsed_markdown.source[range.clone()];
                    if html.starts_with("<code>") {
                        builder.push_text_style(self.style.inline_code.clone());
                        continue;
                    }
                    if html.trim_end().starts_with("</code>") {
                        builder.pop_text_style();
                        continue;
                    }
                    if parser::is_br_tag(html) {
                        if builder.wrap_words {
                            // In a wrapping (per-word) row a "\n" would only
                            // grow one word box; a full-width zero-height
                            // spacer forces the flex row onto a new line.
                            builder.push_inline_icon(
                                div().w_full().h_0().into_any_element(),
                                range.clone(),
                            );
                        } else {
                            builder.push_text("\n", range.clone());
                        }
                        continue;
                    }
                    builder.push_text(&parsed_markdown.source[range.clone()], range.clone());
                }
                MarkdownEvent::Rule => {
                    builder.push_div(
                        div()
                            .border_b_1()
                            .my_2()
                            .border_color(self.style.rule_color),
                        range,
                        markdown_end,
                    );
                    builder.pop_div()
                }
                MarkdownEvent::SoftBreak => builder.push_text(" ", range.clone()),
                MarkdownEvent::HardBreak => builder.push_text("\n", range.clone()),
                MarkdownEvent::TaskListMarker(_) => {
                    // handled inside the `MarkdownTag::Item` case
                }
                MarkdownEvent::FootnoteReference(label) => {
                    builder.push_footnote_ref(label.clone(), range.clone());
                    builder.push_text_style(self.style.link.clone());
                    builder.push_text(&format!("[{label}]"), range.clone());
                    builder.pop_text_style();
                }
                MarkdownEvent::DisplayMath(latex) => {
                    let text_style = builder.text_style();
                    let em_px = text_style.font_size.to_pixels(window.rem_size());
                    let color = text_style.color;
                    if let Some(element) =
                        math::display_math(latex, em_px, color, self.style.display_math_scale)
                    {
                        builder.push_sourced_element(range.clone(), element);
                    } else {
                        builder.push_text(latex, range.clone());
                    }
                }
                MarkdownEvent::InlineMath(latex) => {
                    let text_style = builder.text_style();
                    let em_px = text_style.font_size.to_pixels(window.rem_size());
                    let color = text_style.color;
                    if let Some(element) = math::inline_math(latex, em_px, color) {
                        // In a math paragraph the current div is already a
                        // `flex_wrap` row (see `push_markdown_paragraph`), so the
                        // equation just flows in as another box and wraps with
                        // the surrounding words.
                        builder.modify_current_div(|el| el.child(element));
                    } else {
                        builder.push_text(latex, range.clone());
                    }
                }
            }
        }
        if self.style.code_block_overflow_x_scroll {
            let code_block_ids = code_block_ids;
            self.markdown.update(cx, move |markdown, _| {
                markdown.retain_code_block_scroll_handles(&code_block_ids);
            });
        } else {
            self.markdown
                .update(cx, |markdown, _| markdown.clear_code_block_scroll_handles());
        }
        self.markdown.update(cx, move |markdown, _| {
            markdown.retain_table_scroll_handles(&table_ids);
        });
        let mut rendered_markdown = builder.build();
        let child_layout_id = rendered_markdown.element.request_layout(window, cx);
        let layout_id = window.request_layout(gpui::Style::default(), [child_layout_id], cx);
        (layout_id, rendered_markdown)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        rendered_markdown: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let focus_handle = self.markdown.read(cx).focus_handle.clone();
        window.set_focus_handle(&focus_handle, cx);
        window.set_view_id(self.markdown.entity_id());

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        rendered_markdown.element.prepaint(window, cx);
        self.autoscroll(&rendered_markdown.text, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        rendered_markdown: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut context = KeyContext::default();
        context.add("Markdown");
        window.set_key_context(context);
        window.on_action(std::any::TypeId::of::<crate::Copy>(), {
            let entity = self.markdown.clone();
            let text = rendered_markdown.text.clone();
            move |_, phase, window, cx| {
                let text = text.clone();
                if phase == DispatchPhase::Bubble {
                    entity.update(cx, move |this, cx| this.copy(&text, window, cx))
                }
            }
        });
        window.on_action(std::any::TypeId::of::<crate::CopyAsMarkdown>(), {
            let entity = self.markdown.clone();
            move |_, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    entity.update(cx, move |this, cx| this.copy_as_markdown(window, cx))
                }
            }
        });

        self.paint_mouse_listeners(hitbox, &rendered_markdown.text, window, cx);
        rendered_markdown.element.paint(window, cx);
        self.paint_search_highlights(bounds, &rendered_markdown.text, window, cx);
        self.paint_selection(bounds, &rendered_markdown.text, window, cx);
    }
}

fn apply_heading_style(
    mut heading: Div,
    level: pulldown_cmark::HeadingLevel,
    custom_styles: Option<&HeadingLevelStyles>,
) -> Div {
    heading = match level {
        pulldown_cmark::HeadingLevel::H1 => heading.text_3xl(),
        pulldown_cmark::HeadingLevel::H2 => heading.text_2xl(),
        pulldown_cmark::HeadingLevel::H3 => heading.text_xl(),
        pulldown_cmark::HeadingLevel::H4 => heading.text_lg(),
        pulldown_cmark::HeadingLevel::H5 => heading.text_base(),
        pulldown_cmark::HeadingLevel::H6 => heading.text_sm(),
    };

    if let Some(styles) = custom_styles {
        let style_opt = match level {
            pulldown_cmark::HeadingLevel::H1 => &styles.h1,
            pulldown_cmark::HeadingLevel::H2 => &styles.h2,
            pulldown_cmark::HeadingLevel::H3 => &styles.h3,
            pulldown_cmark::HeadingLevel::H4 => &styles.h4,
            pulldown_cmark::HeadingLevel::H5 => &styles.h5,
            pulldown_cmark::HeadingLevel::H6 => &styles.h6,
        };

        if let Some(style) = style_opt {
            heading.style().text = style.clone();
        }
    }

    heading
}

fn render_copy_code_block_button(
    id: usize,
    code: String,
    markdown: Entity<Markdown>,
) -> impl IntoElement {
    let id = ElementId::named_usize("copy-markdown-code", id);

    CopyButton::new(id.clone(), code.clone()).custom_on_click({
        let markdown = markdown;
        move |_window, cx| {
            let id = id.clone();
            markdown.update(cx, |this, cx| {
                this.copied_code_blocks.insert(id.clone());

                cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));

                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;

                    cx.update(|cx| {
                        this.update(cx, |this, cx| {
                            this.copied_code_blocks.remove(&id);
                            cx.notify();
                        })
                    })
                    .ok();
                })
                .detach();
            });
        }
    })
}

impl IntoElement for MarkdownElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub enum AnyDiv {
    Div(Div),
    Stateful(Stateful<Div>),
}

impl AnyDiv {
    fn into_any_element(self) -> AnyElement {
        match self {
            Self::Div(div) => div.into_any_element(),
            Self::Stateful(div) => div.into_any_element(),
        }
    }
}

impl From<Div> for AnyDiv {
    fn from(value: Div) -> Self {
        Self::Div(value)
    }
}

impl From<Stateful<Div>> for AnyDiv {
    fn from(value: Stateful<Div>) -> Self {
        Self::Stateful(value)
    }
}

impl Styled for AnyDiv {
    fn style(&mut self) -> &mut StyleRefinement {
        match self {
            Self::Div(div) => div.style(),
            Self::Stateful(div) => div.style(),
        }
    }
}

impl ParentElement for AnyDiv {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        match self {
            Self::Div(div) => div.extend(elements),
            Self::Stateful(div) => div.extend(elements),
        }
    }
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    in_head: bool,
    row_index: usize,
    col_index: usize,
}

impl TableState {
    fn start(&mut self, alignments: Vec<Alignment>) {
        self.alignments = alignments;
        self.in_head = false;
        self.row_index = 0;
        self.col_index = 0;
    }

    fn end(&mut self) {
        self.alignments.clear();
        self.in_head = false;
        self.row_index = 0;
        self.col_index = 0;
    }

    fn start_head(&mut self) {
        self.in_head = true;
    }

    fn end_head(&mut self) {
        self.in_head = false;
    }

    fn start_row(&mut self) {
        self.col_index = 0;
    }

    fn end_row(&mut self) {
        self.row_index += 1;
    }

    fn end_cell(&mut self) {
        self.col_index += 1;
    }
}

struct MarkdownElementBuilder {
    div_stack: Vec<AnyDiv>,
    rendered_lines: Vec<RenderedLine>,
    pending_line: PendingLine,
    rendered_links: Vec<RenderedLink>,
    rendered_footnote_refs: Vec<RenderedFootnoteRef>,
    current_source_index: usize,
    /// When set, text is emitted as one element per word so the current div
    /// (a `flex_wrap` row) can wrap between words and around inline math.
    wrap_words: bool,
    /// When set (inside a footnote/source definition), paragraphs use tight
    /// spacing so the sources list isn't stretched out by body paragraph margins.
    in_footnote: bool,
    /// Number of `<center>` blocks whose centering divs are still on the stack.
    /// Balanced at `build` so malformed input (a `</center>` not separated from
    /// following content by a blank line, which CommonMark glues into one HTML
    /// block) can't leave the div stack unbalanced and crash rendering.
    open_centers: usize,
    html_comment: bool,
    rendered_footnote_separator: bool,
    base_text_style: TextStyle,
    text_style_stack: Vec<TextStyleRefinement>,
    code_block_stack: Vec<Option<Arc<Language>>>,
    list_stack: Vec<ListStackEntry>,
    table: TableState,
    syntax_theme: Arc<SyntaxTheme>,
}

#[derive(Default)]
struct PendingLine {
    text: String,
    runs: Vec<TextRun>,
    source_mappings: Vec<SourceMapping>,
}

struct ListStackEntry {
    bullet_index: Option<u64>,
}

impl MarkdownElementBuilder {
    fn new(
        container_style: &StyleRefinement,
        base_text_style: TextStyle,
        syntax_theme: Arc<SyntaxTheme>,
    ) -> Self {
        Self {
            div_stack: vec![{
                let mut base_div = div();
                base_div.style().refine(container_style);
                base_div.debug_selector(|| "inner".into()).into()
            }],
            rendered_lines: Vec::new(),
            pending_line: PendingLine::default(),
            rendered_links: Vec::new(),
            rendered_footnote_refs: Vec::new(),
            current_source_index: 0,
            wrap_words: false,
            in_footnote: false,
            open_centers: 0,
            html_comment: false,
            rendered_footnote_separator: false,
            base_text_style,
            text_style_stack: Vec::new(),
            code_block_stack: Vec::new(),
            list_stack: Vec::new(),
            table: TableState::default(),
            syntax_theme,
        }
    }

    fn push_text_style(&mut self, style: TextStyleRefinement) {
        self.text_style_stack.push(style);
    }

    fn text_style(&self) -> TextStyle {
        let mut style = self.base_text_style.clone();
        for refinement in &self.text_style_stack {
            style.refine(refinement);
        }
        style
    }

    fn pop_text_style(&mut self) {
        self.text_style_stack.pop();
    }

    fn push_div(&mut self, div: impl Into<AnyDiv>, range: &Range<usize>, markdown_end: usize) {
        let mut div = div.into();
        self.flush_text();

        if range.start == 0 {
            // Remove the top margin on the first element.
            div.style().refine(&StyleRefinement {
                margin: gpui::EdgesRefinement {
                    top: Some(Length::Definite(px(0.).into())),
                    left: None,
                    right: None,
                    bottom: None,
                },
                ..Default::default()
            });
        }

        if range.end == markdown_end {
            div.style().refine(&StyleRefinement {
                margin: gpui::EdgesRefinement {
                    top: None,
                    left: None,
                    right: None,
                    bottom: Some(Length::Definite(rems(0.).into())),
                },
                ..Default::default()
            });
        }

        self.div_stack.push(div);
    }

    fn push_root_block(&mut self, range: &Range<usize>, markdown_end: usize) {
        self.push_div(
            div().group("markdown-root-block").relative(),
            range,
            markdown_end,
        );
        self.push_div(div().pl_4(), range, markdown_end);
    }

    fn modify_current_div(&mut self, f: impl FnOnce(AnyDiv) -> AnyDiv) {
        self.flush_text();
        if let Some(div) = self.div_stack.pop() {
            self.div_stack.push(f(div));
        }
    }

    fn pop_root_block(
        &mut self,
        is_active: bool,
        active_gutter_color: Hsla,
        hovered_gutter_color: Hsla,
    ) {
        self.pop_div();
        self.modify_current_div(|el| {
            el.child(
                div()
                    .h_full()
                    .w(px(4.0))
                    .when(is_active, |this| this.bg(active_gutter_color))
                    .group_hover("markdown-root-block", |this| {
                        if is_active {
                            this
                        } else {
                            this.bg(hovered_gutter_color)
                        }
                    })
                    .rounded_xs()
                    .absolute()
                    .left_0()
                    .top_0(),
            )
        });
        self.pop_div();
    }

    fn pop_div(&mut self) {
        self.flush_text();
        let div = self.div_stack.pop().unwrap().into_any_element();
        self.div_stack.last_mut().unwrap().extend(iter::once(div));
    }

    fn push_sourced_element(&mut self, source_range: Range<usize>, element: impl Into<AnyElement>) {
        self.flush_text();
        let anchor = self.render_source_anchor(source_range);
        self.div_stack.last_mut().unwrap().extend([{
            div()
                .relative()
                .child(anchor)
                .child(element.into())
                .into_any_element()
        }]);
    }

    fn push_list(&mut self, bullet_index: Option<u64>) {
        self.list_stack.push(ListStackEntry { bullet_index });
    }

    fn next_bullet_index(&mut self) -> Option<u64> {
        self.list_stack.last_mut().and_then(|entry| {
            let item_index = entry.bullet_index.as_mut()?;
            *item_index += 1;
            Some(*item_index - 1)
        })
    }

    fn pop_list(&mut self) {
        self.list_stack.pop();
    }

    fn push_code_block(&mut self, language: Option<Arc<Language>>) {
        self.code_block_stack.push(language);
    }

    fn pop_code_block(&mut self) {
        self.code_block_stack.pop();
    }

    fn push_link(&mut self, destination_url: SharedString, source_range: Range<usize>) {
        self.rendered_links.push(RenderedLink {
            source_range,
            destination_url,
        });
    }

    fn push_footnote_ref(&mut self, label: SharedString, source_range: Range<usize>) {
        self.rendered_footnote_refs.push(RenderedFootnoteRef {
            source_range,
            label,
        });
    }

    /// Emit `text` as one `StyledText` element per word into the current
    /// `flex_wrap` div, so it wraps between words and flows around inline elements
    /// (math, link icons). Trailing spaces stay attached to each word to preserve
    /// inter-word spacing. Word boxes lose cross-word shaping, but each is
    /// registered as a `RenderedLine` so text selection still works — selection is
    /// bounds-based (it already handles non-stacked boxes like table columns).
    /// Place a non-text inline element (e.g. an emoji icon) into the current
    /// wrapping div, advancing past its source range. The element registers no
    /// `RenderedLine`, so selection/copy skips over it.
    fn push_inline_icon(&mut self, icon: AnyElement, source_range: Range<usize>) {
        self.current_source_index = source_range.end;
        self.div_stack.last_mut().unwrap().extend([icon]);
    }

    fn push_wrapped_words(&mut self, text: &str, source_range: Range<usize>) {
        let style = self.text_style();
        let mut offset = 0;
        for word in text.split_inclusive(' ') {
            if word.is_empty() {
                continue;
            }
            let word_source_start = source_range.start + offset;
            offset += word.len();
            let styled =
                StyledText::new(word.to_string()).with_runs(vec![style.to_run(word.len())]);
            self.rendered_lines.push(RenderedLine {
                layout: styled.layout().clone(),
                source_mappings: vec![SourceMapping {
                    rendered_index: 0,
                    source_index: word_source_start,
                }],
                source_end: word_source_start + word.len(),
                language: self.code_block_stack.last().cloned().flatten(),
            });
            self.div_stack.last_mut().unwrap().extend([styled.into_any()]);
        }
        self.current_source_index = source_range.end;
    }

    /// Emit one inline-code span as a single sized box into the current
    /// `flex_wrap` div. The box owns its `text_size` (from `code_style`), so
    /// inline code can render at a different point size than the surrounding
    /// line — impossible within a single shaped `StyledText`, whose runs carry
    /// no size. The chip background moves from the run to the box; the span is
    /// registered as one `RenderedLine` so selection (bounds-based) still works.
    fn push_inline_code_box(
        &mut self,
        text: &str,
        source_range: Range<usize>,
        code_style: &TextStyleRefinement,
    ) {
        let mut run_style = self.text_style();
        run_style.refine(code_style);
        // The box paints the chip background; don't also paint it on the run.
        run_style.background_color = None;
        let styled =
            StyledText::new(text.to_string()).with_runs(vec![run_style.to_run(text.len())]);
        self.rendered_lines.push(RenderedLine {
            layout: styled.layout().clone(),
            source_mappings: vec![SourceMapping {
                rendered_index: 0,
                source_index: source_range.start,
            }],
            source_end: source_range.end,
            language: None,
        });
        let chip = div()
            .flex_none()
            .px(px(3.))
            .rounded_sm()
            .when_some(code_style.font_size, |el, size| el.text_size(size))
            .when_some(code_style.background_color, |el, bg| el.bg(bg))
            .child(styled);
        self.current_source_index = source_range.end;
        self.div_stack
            .last_mut()
            .unwrap()
            .extend([chip.into_any_element()]);
    }

    fn push_text(&mut self, text: &str, source_range: Range<usize>) {
        if self.wrap_words {
            self.push_wrapped_words(text, source_range);
            return;
        }
        self.pending_line.source_mappings.push(SourceMapping {
            rendered_index: self.pending_line.text.len(),
            source_index: source_range.start,
        });
        self.pending_line.text.push_str(text);
        self.current_source_index = source_range.end;

        if let Some(Some(language)) = self.code_block_stack.last() {
            let mut offset = 0;
            for (range, highlight_id) in language.highlight_text(&Rope::from(text), 0..text.len()) {
                if range.start > offset {
                    self.pending_line
                        .runs
                        .push(self.text_style().to_run(range.start - offset));
                }

                let mut run_style = self.text_style();
                if let Some(highlight) = self.syntax_theme.get(highlight_id).cloned() {
                    run_style = run_style.highlight(highlight);
                }

                self.pending_line.runs.push(run_style.to_run(range.len()));
                offset = range.end;
            }

            if offset < text.len() {
                self.pending_line
                    .runs
                    .push(self.text_style().to_run(text.len() - offset));
            }
        } else {
            self.pending_line
                .runs
                .push(self.text_style().to_run(text.len()));
        }
    }

    fn trim_trailing_newline(&mut self) {
        if self.pending_line.text.ends_with('\n') {
            self.pending_line
                .text
                .truncate(self.pending_line.text.len() - 1);
            self.pending_line.runs.last_mut().unwrap().len -= 1;
            self.current_source_index -= 1;
        }
    }

    fn replace_pending_checkbox(&mut self, source_range: &Range<usize>) {
        let trimmed = self.pending_line.text.trim();
        if trimmed == "[x]" || trimmed == "[X]" || trimmed == "[ ]" {
            let checked = trimmed != "[ ]";
            self.pending_line = PendingLine::default();
            let checkbox = Checkbox::new(
                ElementId::Name(
                    format!("table_checkbox_{}_{}", source_range.start, source_range.end).into(),
                ),
                if checked {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                },
            )
            .fill()
            .visualization_only(true)
            .into_any_element();
            self.div_stack.last_mut().unwrap().extend([checkbox]);
        }
    }

    fn render_source_anchor(&mut self, source_range: Range<usize>) -> AnyElement {
        let mut text_style = self.base_text_style.clone();
        text_style.color = Hsla::transparent_black();
        let text = "\u{200B}";
        let styled_text = StyledText::new(text).with_runs(vec![text_style.to_run(text.len())]);
        self.rendered_lines.push(RenderedLine {
            layout: styled_text.layout().clone(),
            source_mappings: vec![SourceMapping {
                rendered_index: 0,
                source_index: source_range.start,
            }],
            source_end: source_range.end,
            language: None,
        });
        div()
            .absolute()
            .top_0()
            .left_0()
            .opacity(0.)
            .child(styled_text)
            .into_any_element()
    }

    fn flush_text(&mut self) {
        let line = mem::take(&mut self.pending_line);
        if line.text.is_empty() {
            return;
        }

        let text = StyledText::new(line.text).with_runs(line.runs);
        self.rendered_lines.push(RenderedLine {
            layout: text.layout().clone(),
            source_mappings: line.source_mappings,
            source_end: self.current_source_index,
            language: self.code_block_stack.last().cloned().flatten(),
        });
        self.div_stack.last_mut().unwrap().extend([text.into_any()]);
    }

    fn build(mut self) -> RenderedMarkdown {
        // Close any `<center>` blocks left open by malformed input so the div
        // stack ends balanced (otherwise rendering crashes).
        while self.open_centers > 0 {
            self.pop_div(); // inner left-aligned column
            self.pop_div(); // outer centering row
            self.open_centers -= 1;
        }
        debug_assert_eq!(self.div_stack.len(), 1);
        self.flush_text();
        RenderedMarkdown {
            element: self.div_stack.pop().unwrap().into_any_element(),
            text: RenderedText {
                lines: self.rendered_lines.into(),
                links: self.rendered_links.into(),
                footnote_refs: self.rendered_footnote_refs.into(),
            },
        }
    }
}

struct RenderedLine {
    layout: TextLayout,
    source_mappings: Vec<SourceMapping>,
    source_end: usize,
    language: Option<Arc<Language>>,
}

impl RenderedLine {
    fn rendered_index_for_source_index(&self, source_index: usize) -> usize {
        if source_index >= self.source_end {
            return self.layout.len();
        }

        let mapping = match self
            .source_mappings
            .binary_search_by_key(&source_index, |probe| probe.source_index)
        {
            Ok(ix) => &self.source_mappings[ix],
            Err(ix) => &self.source_mappings[ix - 1],
        };
        (mapping.rendered_index + (source_index - mapping.source_index)).min(self.layout.len())
    }

    fn source_index_for_rendered_index(&self, rendered_index: usize) -> usize {
        if rendered_index >= self.layout.len() {
            return self.source_end;
        }

        let mapping = match self
            .source_mappings
            .binary_search_by_key(&rendered_index, |probe| probe.rendered_index)
        {
            Ok(ix) => &self.source_mappings[ix],
            Err(ix) => &self.source_mappings[ix - 1],
        };
        mapping.source_index + (rendered_index - mapping.rendered_index)
    }

    /// Returns the source index for use as an exclusive range end at a word/selection boundary.
    /// When the rendered index is exactly at the start of a segment with a gap from the previous
    /// segment (e.g., after stripped markdown syntax like backticks), this returns the end of the
    /// previous segment rather than the start of the current one.
    fn source_index_for_exclusive_rendered_end(&self, rendered_index: usize) -> usize {
        if rendered_index >= self.layout.len() {
            return self.source_end;
        }

        let ix = match self
            .source_mappings
            .binary_search_by_key(&rendered_index, |probe| probe.rendered_index)
        {
            Ok(ix) => ix,
            Err(ix) => {
                return self.source_mappings[ix - 1].source_index
                    + (rendered_index - self.source_mappings[ix - 1].rendered_index);
            }
        };

        // Exact match at the start of a segment. Check if there's a gap from the previous segment.
        if ix > 0 {
            let prev_mapping = &self.source_mappings[ix - 1];
            let mapping = &self.source_mappings[ix];
            let prev_segment_len = mapping.rendered_index - prev_mapping.rendered_index;
            let prev_source_end = prev_mapping.source_index + prev_segment_len;
            if prev_source_end < mapping.source_index {
                return prev_source_end;
            }
        }

        self.source_mappings[ix].source_index
    }

    fn source_index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        let line_rendered_index;
        let out_of_bounds;
        match self.layout.index_for_position(position) {
            Ok(ix) => {
                line_rendered_index = ix;
                out_of_bounds = false;
            }
            Err(ix) => {
                line_rendered_index = ix;
                out_of_bounds = true;
            }
        };
        let source_index = self.source_index_for_rendered_index(line_rendered_index);
        if out_of_bounds {
            Err(source_index)
        } else {
            Ok(source_index)
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct SourceMapping {
    rendered_index: usize,
    source_index: usize,
}

pub struct RenderedMarkdown {
    element: AnyElement,
    text: RenderedText,
}

#[derive(Clone)]
struct RenderedText {
    lines: Rc<[RenderedLine]>,
    links: Rc<[RenderedLink]>,
    footnote_refs: Rc<[RenderedFootnoteRef]>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RenderedLink {
    source_range: Range<usize>,
    destination_url: SharedString,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RenderedFootnoteRef {
    source_range: Range<usize>,
    label: SharedString,
}

impl RenderedText {
    fn source_index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        let mut lines = self.lines.iter().peekable();
        let mut fallback_line: Option<&RenderedLine> = None;

        while let Some(line) = lines.next() {
            let line_bounds = line.layout.bounds();

            // Exact match: position is within bounds (handles overlapping bounds like table columns)
            if line_bounds.contains(&position) {
                return line.source_index_for_position(position);
            }

            // Track fallback for Y-coordinate based matching
            if position.y <= line_bounds.bottom() && fallback_line.is_none() {
                fallback_line = Some(line);
            }

            // Handle gap between lines
            if position.y > line_bounds.bottom() {
                if let Some(next_line) = lines.peek()
                    && position.y < next_line.layout.bounds().top()
                {
                    return Err(line.source_end);
                }
            }
        }

        // Fall back to Y-coordinate matched line
        if let Some(line) = fallback_line {
            return line.source_index_for_position(position);
        }

        Err(self.lines.last().map_or(0, |line| line.source_end))
    }

    fn position_for_source_index(&self, source_index: usize) -> Option<(Point<Pixels>, Pixels)> {
        for line in self.lines.iter() {
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            if source_index < line_source_start {
                break;
            } else if source_index > line.source_end {
                continue;
            } else {
                let line_height = line.layout.line_height();
                let rendered_index_within_line = line.rendered_index_for_source_index(source_index);
                let position = line.layout.position_for_index(rendered_index_within_line)?;
                return Some((position, line_height));
            }
        }
        None
    }

    fn surrounding_word_range(&self, source_index: usize) -> Range<usize> {
        for line in self.lines.iter() {
            if source_index > line.source_end {
                continue;
            }

            let line_rendered_start = line.source_mappings.first().unwrap().rendered_index;
            let rendered_index_in_line =
                line.rendered_index_for_source_index(source_index) - line_rendered_start;
            let text = line.layout.text();

            let scope = line.language.as_ref().map(|l| l.default_scope());
            let classifier = CharClassifier::new(scope);

            let mut prev_chars = text[..rendered_index_in_line].chars().rev().peekable();
            let mut next_chars = text[rendered_index_in_line..].chars().peekable();

            let word_kind = std::cmp::max(
                prev_chars.peek().map(|&c| classifier.kind(c)),
                next_chars.peek().map(|&c| classifier.kind(c)),
            );

            let mut start = rendered_index_in_line;
            for c in prev_chars {
                if Some(classifier.kind(c)) == word_kind {
                    start -= c.len_utf8();
                } else {
                    break;
                }
            }

            let mut end = rendered_index_in_line;
            for c in next_chars {
                if Some(classifier.kind(c)) == word_kind {
                    end += c.len_utf8();
                } else {
                    break;
                }
            }

            return line.source_index_for_rendered_index(line_rendered_start + start)
                ..line.source_index_for_exclusive_rendered_end(line_rendered_start + end);
        }

        source_index..source_index
    }

    fn surrounding_line_range(&self, source_index: usize) -> Range<usize> {
        for line in self.lines.iter() {
            if source_index > line.source_end {
                continue;
            }
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            return line_source_start..line.source_end;
        }

        source_index..source_index
    }

    fn text_for_range(&self, range: Range<usize>) -> String {
        let mut accumulator = String::new();

        for line in self.lines.iter() {
            if range.start > line.source_end {
                continue;
            }
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            if range.end < line_source_start {
                break;
            }

            let text = line.layout.text();

            let start = if range.start < line_source_start {
                0
            } else {
                line.rendered_index_for_source_index(range.start)
            };
            let end = if range.end > line.source_end {
                line.rendered_index_for_source_index(line.source_end)
            } else {
                line.rendered_index_for_source_index(range.end)
            }
            .min(text.len());

            accumulator.push_str(&text[start..end]);
            accumulator.push('\n');
        }
        // Remove trailing newline
        accumulator.pop();
        accumulator
    }

    fn link_for_source_index(&self, source_index: usize) -> Option<&RenderedLink> {
        self.links
            .iter()
            .find(|link| link.source_range.contains(&source_index))
    }

    fn footnote_ref_for_source_index(&self, source_index: usize) -> Option<&RenderedFootnoteRef> {
        self.footnote_refs
            .iter()
            .find(|fref| fref.source_range.contains(&source_index))
    }
}

/// Resolves an Obsidian-style size from a wikilink image embed: `![[image|600]]`
/// sets the width and `![[image|600x400]]` sets width and height (in pixels). The
/// size lives in the embed's alt text — the part after `|` — so a plain
/// `![[image]]` (or a real caption like `![[image|Figure 1]]`) yields no override.
/// Width-only keeps the image's aspect ratio (gpui derives the height).
fn image_size_override(
    link_type: &LinkType,
    events: &[(Range<usize>, MarkdownEvent)],
    image_index: usize,
    source: &str,
) -> (Option<DefiniteLength>, Option<DefiniteLength>) {
    // Only an explicit `|...` display part (a "pothole") can carry a size.
    if !matches!(link_type, LinkType::WikiLink { has_pothole: true }) {
        return (None, None);
    }
    for (range, event) in &events[image_index + 1..] {
        match event {
            MarkdownEvent::Text => return parse_image_size(&source[range.clone()]),
            MarkdownEvent::End(MarkdownTagEnd::Image) => break,
            _ => {}
        }
    }
    (None, None)
}

/// Parses `600` or `600x400` (pixels) into `(width, height)`; non-numeric text
/// (a caption) yields `(None, None)`.
fn parse_image_size(text: &str) -> (Option<DefiniteLength>, Option<DefiniteLength>) {
    let pixels = |value: f32| DefiniteLength::Absolute(AbsoluteLength::Pixels(px(value)));
    let text = text.trim();
    if let Some((width, height)) = text.split_once(['x', 'X', '×']) {
        match (width.trim().parse::<f32>(), height.trim().parse::<f32>()) {
            (Ok(width), Ok(height)) => (Some(pixels(width)), Some(pixels(height))),
            _ => (None, None),
        }
    } else if let Ok(width) = text.parse::<f32>() {
        (Some(pixels(width)), None)
    } else {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};
    use language::{Language, LanguageConfig, LanguageMatcher};
    use std::sync::Arc;

    fn ensure_theme_initialized(cx: &mut TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
    }

    #[gpui::test]
    fn test_mappings(cx: &mut TestAppContext) {
        // Formatting.
        assert_mappings(
            &render_markdown("He*l*lo", cx),
            vec![vec![(0, 0), (1, 1), (2, 3), (3, 5), (4, 6), (5, 7)]],
        );

        // Multiple lines.
        assert_mappings(
            &render_markdown("Hello\n\nWorld", cx),
            vec![
                vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)],
                vec![(0, 7), (1, 8), (2, 9), (3, 10), (4, 11), (5, 12)],
            ],
        );

        // Multi-byte characters.
        assert_mappings(
            &render_markdown("αβγ\n\nδεζ", cx),
            vec![
                vec![(0, 0), (2, 2), (4, 4), (6, 6)],
                vec![(0, 8), (2, 10), (4, 12), (6, 14)],
            ],
        );

        // Smart quotes.
        assert_mappings(&render_markdown("\"", cx), vec![vec![(0, 0), (3, 1)]]);
        assert_mappings(
            &render_markdown("\"hey\"", cx),
            vec![vec![(0, 0), (3, 1), (4, 2), (5, 3), (6, 4), (9, 5)]],
        );

        // HTML Comments are ignored
        assert_mappings(
            &render_markdown(
                "<!--\nrdoc-file=string.c\n- str.intern   -> symbol\n- str.to_sym   -> symbol\n-->\nReturns",
                cx,
            ),
            vec![vec![
                (0, 78),
                (1, 79),
                (2, 80),
                (3, 81),
                (4, 82),
                (5, 83),
                (6, 84),
            ]],
        );
    }

    #[gpui::test]
    fn test_vwap_center_repro(cx: &mut TestAppContext) {
        // Mirrors the VWAP wiki page that crashes on open: display math, a
        // <center> block whose </center> is not blank-line-separated from the
        // following list, multibyte dashes, and a footnote.
        let content = "## Equation\n\n\
            $$\\text{VWAP} = \\frac{\\sum_{i} p_i \\cdot v_i}{\\sum_{i} v_i}$$\n\n\
            <center>\n\n\
            $p_i$ — price of trade $i$\n\n\
            $v_i$ — volume of trade $i$\n\n\
            </center>\n\
            - summation is over all trades [^2]\n\n\
            ## Sources\n\n\
            [^2]: Chan, Ernest P. pp. 55–65.\n";
        let _ = render_markdown(content, cx);
    }

    fn render_markdown(markdown: &str, cx: &mut TestAppContext) -> RenderedText {
        render_markdown_with_language_registry(markdown, None, cx)
    }

    fn render_markdown_with_language_registry(
        markdown: &str,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        render_markdown_with_options(markdown, language_registry, MarkdownOptions::default(), cx)
    }

    fn render_markdown_with_options(
        markdown: &str,
        language_registry: Option<Arc<LanguageRegistry>>,
        options: MarkdownOptions,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                markdown.to_string().into(),
                language_registry,
                None,
                options,
                cx,
            )
        });
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, MarkdownStyle::default()).code_block_renderer(
                    CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        border: false,
                    },
                )
            },
        );
        rendered.text
    }

    #[gpui::test]
    fn test_surrounding_word_range(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world tesεζ", cx);

        // Test word selection for "Hello"
        let word_range = rendered.surrounding_word_range(2); // Simulate click on 'l' in "Hello"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "Hello");

        // Test word selection for "world"
        let word_range = rendered.surrounding_word_range(7); // Simulate click on 'o' in "world"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "world");

        // Test word selection for "tesεζ"
        let word_range = rendered.surrounding_word_range(14); // Simulate click on 's' in "tesεζ"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "tesεζ");

        // Test word selection at word boundary (space)
        let word_range = rendered.surrounding_word_range(5); // Simulate click on space between "Hello" and "world", expect highlighting word to the left
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "Hello");
    }

    #[gpui::test]
    fn test_surrounding_line_range(cx: &mut TestAppContext) {
        let rendered = render_markdown("First line\n\nSecond line\n\nThird lineεζ", cx);

        // Test getting line range for first line
        let line_range = rendered.surrounding_line_range(5); // Simulate click somewhere in first line
        let selected_text = rendered.text_for_range(line_range);
        assert_eq!(selected_text, "First line");

        // Test getting line range for second line
        let line_range = rendered.surrounding_line_range(13); // Simulate click at beginning in second line
        let selected_text = rendered.text_for_range(line_range);
        assert_eq!(selected_text, "Second line");

        // Test getting line range for third line
        let line_range = rendered.surrounding_line_range(37); // Simulate click at end of third line with multi-byte chars
        let selected_text = rendered.text_for_range(line_range);
        assert_eq!(selected_text, "Third lineεζ");
    }

    #[gpui::test]
    fn test_selection_head_movement(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world test", cx);

        let mut selection = Selection {
            start: 5,
            end: 5,
            reversed: false,
            pending: false,
            mode: SelectMode::Character,
        };

        // Test forward selection
        selection.set_head(10, &rendered);
        assert_eq!(selection.start, 5);
        assert_eq!(selection.end, 10);
        assert!(!selection.reversed);
        assert_eq!(selection.tail(), 5);

        // Test backward selection
        selection.set_head(2, &rendered);
        assert_eq!(selection.start, 2);
        assert_eq!(selection.end, 5);
        assert!(selection.reversed);
        assert_eq!(selection.tail(), 5);

        // Test forward selection again from reversed state
        selection.set_head(15, &rendered);
        assert_eq!(selection.start, 5);
        assert_eq!(selection.end, 15);
        assert!(!selection.reversed);
        assert_eq!(selection.tail(), 5);
    }

    #[gpui::test]
    fn test_word_selection_drag(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world test", cx);

        // Start with a simulated double-click on "world" (index 6-10)
        let word_range = rendered.surrounding_word_range(7); // Click on 'o' in "world"
        let mut selection = Selection {
            start: word_range.start,
            end: word_range.end,
            reversed: false,
            pending: true,
            mode: SelectMode::Word(word_range),
        };

        // Drag forward to "test" - should expand selection to include "test"
        selection.set_head(13, &rendered); // Index in "test"
        assert_eq!(selection.start, 6); // Start of "world"
        assert_eq!(selection.end, 16); // End of "test"
        assert!(!selection.reversed);
        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(selected_text, "world test");

        // Drag backward to "Hello" - should expand selection to include "Hello"
        selection.set_head(2, &rendered); // Index in "Hello"
        assert_eq!(selection.start, 0); // Start of "Hello"
        assert_eq!(selection.end, 11); // End of "world" (original selection)
        assert!(selection.reversed);
        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(selected_text, "Hello world");

        // Drag back within original word - should revert to original selection
        selection.set_head(8, &rendered); // Back within "world"
        assert_eq!(selection.start, 6); // Start of "world"
        assert_eq!(selection.end, 11); // End of "world"
        assert!(!selection.reversed);
        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(selected_text, "world");
    }

    #[gpui::test]
    fn test_selection_with_markdown_formatting(cx: &mut TestAppContext) {
        let rendered = render_markdown(
            "This is **bold** text, this is *italic* text, use `code` here",
            cx,
        );
        let word_range = rendered.surrounding_word_range(10); // Inside "bold"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "bold");

        let word_range = rendered.surrounding_word_range(32); // Inside "italic"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "italic");

        let word_range = rendered.surrounding_word_range(51); // Inside "code"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "code");
    }

    #[gpui::test]
    fn test_table_column_selection(cx: &mut TestAppContext) {
        let rendered = render_markdown("| a | b |\n|---|---|\n| c | d |", cx);

        assert!(rendered.lines.len() >= 2);
        let first_bounds = rendered.lines[0].layout.bounds();
        let second_bounds = rendered.lines[1].layout.bounds();

        let first_index = match rendered.source_index_for_position(first_bounds.center()) {
            Ok(index) | Err(index) => index,
        };
        let second_index = match rendered.source_index_for_position(second_bounds.center()) {
            Ok(index) | Err(index) => index,
        };

        let first_word = rendered.text_for_range(rendered.surrounding_word_range(first_index));
        let second_word = rendered.text_for_range(rendered.surrounding_word_range(second_index));

        assert_eq!(first_word, "a");
        assert_eq!(second_word, "b");
    }

    #[test]
    fn test_table_checkbox_detection() {
        let md = "| Done |\n|------|\n| [x] |\n| [ ] |";
        let events = crate::parser::parse_markdown_with_options(md, false, false).events;

        let mut in_table = false;
        let mut cell_texts: Vec<String> = Vec::new();
        let mut current_cell = String::new();

        for (range, event) in &events {
            match event {
                MarkdownEvent::Start(MarkdownTag::Table(_)) => in_table = true,
                MarkdownEvent::End(MarkdownTagEnd::Table) => in_table = false,
                MarkdownEvent::Start(MarkdownTag::TableCell) => current_cell.clear(),
                MarkdownEvent::End(MarkdownTagEnd::TableCell) => {
                    if in_table {
                        cell_texts.push(current_cell.clone());
                    }
                }
                MarkdownEvent::Text if in_table => {
                    current_cell.push_str(&md[range.clone()]);
                }
                _ => {}
            }
        }

        let checkbox_cells: Vec<&String> = cell_texts
            .iter()
            .filter(|t| {
                let trimmed = t.trim();
                trimmed == "[x]" || trimmed == "[X]" || trimmed == "[ ]"
            })
            .collect();
        assert_eq!(
            checkbox_cells.len(),
            2,
            "Expected 2 checkbox cells, got: {cell_texts:?}"
        );
        assert_eq!(checkbox_cells[0].trim(), "[x]");
        assert_eq!(checkbox_cells[1].trim(), "[ ]");
    }

    #[gpui::test]
    fn test_inline_code_word_selection_excludes_backticks(cx: &mut TestAppContext) {
        // Test that double-clicking on inline code selects just the code content,
        // not the backticks. This verifies the fix for the bug where selecting
        // inline code would include the trailing backtick.
        let rendered = render_markdown("use `blah` here", cx);

        // Source layout: "use `blah` here"
        //                 0123456789...
        // The inline code "blah" is at source positions 5-8 (content range 5..9)

        // Click inside "blah" - should select just "blah", not "blah`"
        let word_range = rendered.surrounding_word_range(6); // 'l' in "blah"

        // text_for_range extracts from the rendered text (without backticks), so it
        // would return "blah" even with a wrong source range. We check it anyway.
        let selected_text = rendered.text_for_range(word_range.clone());
        assert_eq!(selected_text, "blah");

        // The source range is what matters for copy_as_markdown and selected_text,
        // which extract directly from the source. With the bug, this would be 5..10
        // which includes the closing backtick at position 9.
        assert_eq!(word_range, 5..9);
    }

    #[gpui::test]
    fn test_surrounding_word_range_respects_word_characters(cx: &mut TestAppContext) {
        let rendered = render_markdown("foo.bar() baz", cx);

        // Double clicking on 'f' in "foo" - should select just "foo"
        let word_range = rendered.surrounding_word_range(0);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "foo");

        // Double clicking on 'b' in "bar" - should select just "bar"
        let word_range = rendered.surrounding_word_range(4);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "bar");

        // Double clicking on 'b' in "baz" - should select "baz"
        let word_range = rendered.surrounding_word_range(10);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "baz");

        // Double clicking selects word characters in code blocks
        let javascript_language = Arc::new(Language::new(
            LanguageConfig {
                name: "JavaScript".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["js".to_string()],
                    ..Default::default()
                },
                word_characters: ['$', '#'].into_iter().collect(),
                ..Default::default()
            },
            None,
        ));

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.add(javascript_language);

        let rendered = render_markdown_with_language_registry(
            "```javascript\n$foo #bar\n```",
            Some(language_registry),
            cx,
        );

        let word_range = rendered.surrounding_word_range(14);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "$foo");

        let word_range = rendered.surrounding_word_range(19);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "#bar");
    }

    #[gpui::test]
    fn test_all_selection(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world\n\nThis is a test\n\nwith multiple lines", cx);

        let total_length = rendered
            .lines
            .last()
            .map(|line| line.source_end)
            .unwrap_or(0);

        let mut selection = Selection {
            start: 0,
            end: total_length,
            reversed: false,
            pending: true,
            mode: SelectMode::All,
        };

        selection.set_head(5, &rendered); // Try to set head in middle
        assert_eq!(selection.start, 0);
        assert_eq!(selection.end, total_length);
        assert!(!selection.reversed);

        selection.set_head(25, &rendered); // Try to set head near end
        assert_eq!(selection.start, 0);
        assert_eq!(selection.end, total_length);
        assert!(!selection.reversed);

        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(
            selected_text,
            "Hello world\nThis is a test\nwith multiple lines"
        );
    }

    fn nbsp(n: usize) -> String {
        "\u{00A0}".repeat(n)
    }

    #[test]
    fn test_escape_plain_text() {
        assert_eq!(Markdown::escape("hello world"), "hello world");
        assert_eq!(Markdown::escape(""), "");
        assert_eq!(Markdown::escape("café ☕ naïve"), "café ☕ naïve");
    }

    #[test]
    fn test_escape_punctuation() {
        assert_eq!(Markdown::escape("hello `world`"), r"hello \`world\`");
        assert_eq!(Markdown::escape("a|b"), r"a\|b");
    }

    #[test]
    fn test_escape_leading_spaces() {
        assert_eq!(Markdown::escape("    hello"), [&nbsp(4), "hello"].concat());
        assert_eq!(
            Markdown::escape("    | { a: string }"),
            [&nbsp(4), r"\| \{ a\: string \}"].concat()
        );
        assert_eq!(
            Markdown::escape("  first\n  second"),
            [&nbsp(2), "first\n\n", &nbsp(2), "second"].concat()
        );
        assert_eq!(Markdown::escape("hello   world"), "hello   world");
    }

    #[test]
    fn test_escape_leading_tabs() {
        assert_eq!(Markdown::escape("\thello"), [&nbsp(4), "hello"].concat());
        assert_eq!(
            Markdown::escape("hello\n\t\tindented"),
            ["hello\n\n", &nbsp(8), "indented"].concat()
        );
        assert_eq!(
            Markdown::escape(" \t hello"),
            [&nbsp(1 + 4 + 1), "hello"].concat()
        );
        assert_eq!(Markdown::escape("hello\tworld"), "hello\tworld");
    }

    #[test]
    fn test_escape_newlines() {
        assert_eq!(Markdown::escape("a\nb"), "a\n\nb");
        assert_eq!(Markdown::escape("a\n\nb"), "a\n\n\n\nb");
        assert_eq!(Markdown::escape("\nhello"), "\n\nhello");
    }

    #[test]
    fn test_escape_multiline_diagnostic() {
        assert_eq!(
            Markdown::escape("    | { a: string }\n    | { b: number }"),
            [
                &nbsp(4),
                r"\| \{ a\: string \}",
                "\n\n",
                &nbsp(4),
                r"\| \{ b\: number \}",
            ]
            .concat()
        );
    }

    fn has_code_block(markdown: &str) -> bool {
        let parsed_data = parse_markdown_with_options(markdown, false, false);
        parsed_data
            .events
            .iter()
            .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::CodeBlock { .. })))
    }

    #[test]
    fn test_escape_output_len_matches_precomputed() {
        let cases = [
            "",
            "hello world",
            "hello `world`",
            "    hello",
            "    | { a: string }",
            "\thello",
            "hello\n\t\tindented",
            " \t hello",
            "hello\tworld",
            "a\nb",
            "a\n\nb",
            "\nhello",
            "    | { a: string }\n    | { b: number }",
            "café ☕ naïve",
        ];
        for input in cases {
            let mut escaper = MarkdownEscaper::new();
            let precomputed: usize = input.bytes().map(|b| escaper.next(b).output_len()).sum();

            let mut escaper = MarkdownEscaper::new();
            let mut output = String::new();
            for c in input.chars() {
                escaper.next(c as u8).write_to(c, &mut output);
            }

            assert_eq!(precomputed, output.len(), "length mismatch for {:?}", input);
        }
    }

    #[test]
    fn test_escape_prevents_code_block() {
        let diagnostic = "    | { a: string }";
        assert!(has_code_block(diagnostic));
        assert!(!has_code_block(&Markdown::escape(diagnostic)));
    }

    #[gpui::test]
    fn test_link_detected_for_source_index(cx: &mut TestAppContext) {
        let rendered = render_markdown("[Click here](https://example.com)", cx);

        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.links[0].destination_url, "https://example.com");

        // Source index 1 ('C' in "Click") is inside the link's source range
        let link = rendered.link_for_source_index(1);
        assert!(link.is_some());
        assert_eq!(link.unwrap().destination_url, "https://example.com");

        // A source index past the end of the link range returns None
        let past_end = rendered.links[0].source_range.end;
        assert!(rendered.link_for_source_index(past_end).is_none());
    }

    #[gpui::test]
    fn test_link_for_source_index_ignores_plain_text(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world", cx);

        assert!(rendered.links.is_empty());
        assert!(rendered.link_for_source_index(0).is_none());
        assert!(rendered.link_for_source_index(5).is_none());
    }

    #[gpui::test]
    fn test_context_menu_link_initial_state(cx: &mut TestAppContext) {
        struct TestWindow;
        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown =
            cx.new(|cx| Markdown::new("Hello [world](https://example.com)".into(), None, None, cx));
        cx.run_until_parked();

        cx.update(|_window, cx| {
            assert!(markdown.read(cx).context_menu_link().is_none());
        });
    }

    #[gpui::test]
    fn test_capture_for_context_menu(cx: &mut TestAppContext) {
        struct TestWindow;
        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| Markdown::new("text".into(), None, None, cx));
        cx.run_until_parked();

        // Simulates right-clicking on a link
        let url: SharedString = "https://example.com".into();
        markdown.update(cx, |md, _cx| {
            md.capture_for_context_menu(Some(url.clone()));
        });
        cx.update(|_window, cx| {
            assert_eq!(
                markdown
                    .read(cx)
                    .context_menu_link()
                    .map(SharedString::as_ref),
                Some("https://example.com")
            );
        });

        // Simulates right-clicking on plain text — link is cleared
        markdown.update(cx, |md, _cx| {
            md.capture_for_context_menu(None);
        });
        cx.update(|_window, cx| {
            assert!(markdown.read(cx).context_menu_link().is_none());
        });
    }

    #[track_caller]
    fn assert_mappings(rendered: &RenderedText, expected: Vec<Vec<(usize, usize)>>) {
        assert_eq!(rendered.lines.len(), expected.len(), "line count mismatch");
        for (line_ix, line_mappings) in expected.into_iter().enumerate() {
            let line = &rendered.lines[line_ix];

            assert!(
                line.source_mappings.windows(2).all(|mappings| {
                    mappings[0].source_index < mappings[1].source_index
                        && mappings[0].rendered_index < mappings[1].rendered_index
                }),
                "line {} has duplicate mappings: {:?}",
                line_ix,
                line.source_mappings
            );

            for (rendered_ix, source_ix) in line_mappings {
                assert_eq!(
                    line.source_index_for_rendered_index(rendered_ix),
                    source_ix,
                    "line {}, rendered_ix {}",
                    line_ix,
                    rendered_ix
                );

                assert_eq!(
                    line.rendered_index_for_source_index(source_ix),
                    rendered_ix,
                    "line {}, source_ix {}",
                    line_ix,
                    source_ix
                );
            }
        }
    }

    #[gpui::test]
    fn test_heading_font_sizes_are_distinct(cx: &mut TestAppContext) {
        let rendered = render_markdown("# H1\n\n## H2\n\n### H3\n\nBody text", cx);

        assert!(
            rendered.lines.len() >= 4,
            "expected at least 4 rendered lines, got {}",
            rendered.lines.len()
        );

        let h1_line_height = rendered.lines[0].layout.line_height();
        let h2_line_height = rendered.lines[1].layout.line_height();
        let h3_line_height = rendered.lines[2].layout.line_height();
        let body_line_height = rendered.lines[3].layout.line_height();

        assert!(
            h1_line_height > h2_line_height,
            "H1 line height ({h1_line_height:?}) should be greater than H2 ({h2_line_height:?})"
        );
        assert!(
            h2_line_height > h3_line_height,
            "H2 line height ({h2_line_height:?}) should be greater than H3 ({h3_line_height:?})"
        );
        assert!(
            h3_line_height > body_line_height,
            "H3 line height ({h3_line_height:?}) should be greater than body text ({body_line_height:?})"
        );
    }
}
