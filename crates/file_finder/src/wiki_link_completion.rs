//! Obsidian-style `[[wiki link]]` autocomplete for Markdown buffers.
//!
//! Typing `[[` in a Markdown file pops up a completion menu of wiki page
//! titles (the same `wiki_paths`-scoped set the markdown search modal shows).
//! Confirming a page replaces the partial link with `[[Page Title]]`,
//! consuming any autoclosed `]]` after the cursor.
//!
//! Implemented as a `CompletionProvider` that wraps the editor's default
//! project provider: outside of a `[[` context every call is delegated, so
//! LSP/snippet/word completions behave exactly as before.

use std::rc::Rc;

use anyhow::Result;
use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{App, Context, Entity, Task, Window};
use language::{Buffer, CodeLabel, ToOffset as _};
use project::lsp_store::CompletionDocumentation;
use project::{
    Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource, Project,
};

use crate::markdown_search;

/// The query may not span lines and is capped so a stray `[[` in a long
/// paragraph doesn't scan (or match) half the document.
const MAX_QUERY_LEN: usize = 256;

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, _cx: &mut Context<Editor>| {
        // Editors constructed with a project get that project as their
        // default completion provider, which this wraps. Special editors
        // (console, prompts, …) install their own provider after creation,
        // which replaces this one again.
        let Some(project) = editor.project().cloned() else {
            return;
        };
        editor.set_completion_provider(Some(Rc::new(WikiLinkCompletionProvider { project })));
    })
    .detach();
}

struct WikiLinkCompletionProvider {
    project: Entity<Project>,
}

/// Byte offsets describing an unclosed `[[…` immediately before the cursor.
struct WikiLinkContext {
    /// Start of the opening `[[`.
    link_start: usize,
    /// Start of the typed query (just after `[[`).
    query_start: usize,
    /// End of the replaced text: the cursor, plus any autoclosed `]`s
    /// directly after it (typing `[[` autocloses to `[[]]`).
    replace_end: usize,
}

/// Finds the `[[query` the cursor sits in, if any: a `[[` earlier on the same
/// line with no `]` or further `[` between it and the cursor, in a Markdown
/// buffer.
fn wiki_link_context(buffer: &Buffer, position: language::Anchor) -> Option<WikiLinkContext> {
    let language = buffer.language_at(position)?;
    if language.name() != "Markdown" {
        return None;
    }
    let cursor = position.to_offset(buffer);
    let mut query_len = 0;
    let mut chars = buffer.reversed_chars_at(cursor).peekable();
    loop {
        let char = chars.next()?;
        match char {
            '\n' | ']' => return None,
            '[' => {
                if chars.peek() == Some(&'[') {
                    break;
                }
                // A single `[` is a plain markdown link, not a wiki link.
                return None;
            }
            _ => {
                query_len += char.len_utf8();
                if query_len > MAX_QUERY_LEN {
                    return None;
                }
            }
        }
    }
    let trailing_brackets = buffer
        .chars_at(cursor)
        .take(2)
        .take_while(|char| *char == ']')
        .map(|char| char.len_utf8())
        .sum::<usize>();
    let query_start = cursor - query_len;
    Some(WikiLinkContext {
        link_start: query_start - 2,
        query_start,
        replace_end: cursor + trailing_brackets,
    })
}

impl WikiLinkCompletionProvider {
    fn wiki_link_completions(
        &self,
        buffer: &Entity<Buffer>,
        link: WikiLinkContext,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let project = self.project.read(cx);
        let buffer = buffer.read(cx);
        let replace_range =
            buffer.anchor_before(link.link_start)..buffer.anchor_after(link.replace_end);
        // Anchoring the filter query after `[[` makes the menu match the full
        // typed text — spaces included — so "Debt Ma" still finds
        // "Debt Manager" (a word-based query would restart at the space).
        let match_start = buffer.anchor_before(link.query_start);

        let completions = markdown_search::wiki_pages(project, cx)
            .into_iter()
            .map(|(project_path, title)| {
                let icon_path = markdown_search::page_icon(project, &project_path, cx);
                let breadcrumb = markdown_search::breadcrumb_for(&project_path);
                Completion {
                    replace_range: replace_range.clone(),
                    new_text: format!("[[{title}]]"),
                    label: CodeLabel::plain(title, None),
                    documentation: (!breadcrumb.is_empty())
                        .then(|| CompletionDocumentation::SingleLine(breadcrumb)),
                    source: CompletionSource::Custom,
                    icon_path,
                    match_start: Some(match_start),
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                }
            })
            .collect();

        Task::ready(Ok(vec![CompletionResponse {
            completions,
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: false,
        }]))
    }
}

impl CompletionProvider for WikiLinkCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        buffer_position: language::Anchor,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        if let Some(link) = wiki_link_context(buffer.read(cx), buffer_position) {
            return self.wiki_link_completions(buffer, link, cx);
        }
        CompletionProvider::completions(&self.project, buffer, buffer_position, trigger, window, cx)
    }

    fn resolve_completions(
        &self,
        buffer: Entity<Buffer>,
        completion_indices: Vec<usize>,
        completions: Rc<std::cell::RefCell<Box<[Completion]>>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<bool>> {
        CompletionProvider::resolve_completions(
            &self.project,
            buffer,
            completion_indices,
            completions,
            cx,
        )
    }

    fn apply_additional_edits_for_completion(
        &self,
        buffer: Entity<Buffer>,
        completions: Rc<std::cell::RefCell<Box<[Completion]>>>,
        completion_index: usize,
        push_to_history: bool,
        all_commit_ranges: Vec<std::ops::Range<language::Anchor>>,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Option<language::Transaction>>> {
        CompletionProvider::apply_additional_edits_for_completion(
            &self.project,
            buffer,
            completions,
            completion_index,
            push_to_history,
            all_commit_ranges,
            cx,
        )
    }

    fn is_completion_trigger(
        &self,
        buffer: &Entity<Buffer>,
        position: language::Anchor,
        text: &str,
        trigger_in_words: bool,
        cx: &mut Context<Editor>,
    ) -> bool {
        if text.ends_with('[') && wiki_link_context(buffer.read(cx), position).is_some() {
            return true;
        }
        CompletionProvider::is_completion_trigger(
            &self.project,
            buffer,
            position,
            text,
            trigger_in_words,
            cx,
        )
    }

    fn show_snippets(&self) -> bool {
        CompletionProvider::show_snippets(&self.project)
    }
}
