use gpui::{App, Global, actions};
use workspace::Workspace;

pub mod markdown_preview_view;

pub use zed_actions::preview::markdown::{OpenPreview, OpenPreviewToTheSide};

/// Pending requests to leave the next opened markdown file as a plain editor
/// instead of auto-converting it to a rendered preview. The git panel uses this
/// so opening a changed markdown file shows its diff in the editor. It's a
/// simple counter (not keyed by path) so it can't be defeated by path
/// representation mismatches or the buffer's file association not being ready at
/// the instant the editor is added.
#[derive(Default)]
pub struct SuppressAutoPreview {
    pending: usize,
}

impl Global for SuppressAutoPreview {}

impl SuppressAutoPreview {
    /// Request that the next opened markdown file stay a plain editor.
    pub fn suppress_next(cx: &mut App) {
        cx.default_global::<Self>().pending += 1;
    }

    /// Consume one pending suppression, returning whether one was pending.
    pub fn take(cx: &mut App) -> bool {
        let global = cx.default_global::<Self>();
        if global.pending > 0 {
            global.pending -= 1;
            true
        } else {
            false
        }
    }
}

actions!(
    markdown,
    [
        /// Scrolls up by one page in the markdown preview.
        #[action(deprecated_aliases = ["markdown::MovePageUp"])]
        ScrollPageUp,
        /// Scrolls down by one page in the markdown preview.
        #[action(deprecated_aliases = ["markdown::MovePageDown"])]
        ScrollPageDown,
        /// Scrolls up by approximately one visual line.
        ScrollUp,
        /// Scrolls down by approximately one visual line.
        ScrollDown,
        /// Scrolls up by one markdown element in the markdown preview
        ScrollUpByItem,
        /// Scrolls down by one markdown element in the markdown preview
        ScrollDownByItem,
        /// Scrolls to the top of the markdown preview.
        ScrollToTop,
        /// Scrolls to the bottom of the markdown preview.
        ScrollToBottom,
        /// Opens a following markdown preview that syncs with the editor.
        OpenFollowingPreview,
        /// Toggles the current markdown tab between the rendered preview and the
        /// source editor, in place (no separate tab).
        ToggleEditPreview
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        markdown_preview_view::MarkdownPreviewView::register(workspace, window, cx);
    })
    .detach();
}
