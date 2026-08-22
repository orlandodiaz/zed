//! Renders a wiki page's dynamic parts to plain markdown on stdout: ```view
//! blocks expand to their tables and inline `` `= formula` `` spans to their
//! computed values, using the same engine as the Zed markdown preview.
//!
//! For CLI/AI consumption of pages whose tables are otherwise virtual:
//!
//! ```sh
//! mdview "List of credit cards.md" [wiki-root]
//! ```
//!
//! The wiki root defaults to the nearest ancestor directory containing a
//! CLAUDE.md, falling back to the page's own directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use markdown_preview::{formulas, views};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(file) = args.next() else {
        eprintln!("usage: mdview <page.md> [wiki-root]");
        std::process::exit(2);
    };
    let file = match std::fs::canonicalize(&file) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("mdview: {file}: {error}");
            std::process::exit(1);
        }
    };
    let root = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_root(&file));
    let source = match std::fs::read_to_string(&file) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("mdview: {}: {error}", file.display());
            std::process::exit(1);
        }
    };
    print!("{}", render(&source, &root, &file));
}

/// Nearest ancestor containing a CLAUDE.md (the wiki root convention), else
/// the page's own directory.
fn default_root(file: &Path) -> PathBuf {
    let file_directory = file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut current = file_directory.clone();
    loop {
        if current.join("CLAUDE.md").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return file_directory,
        }
    }
}

fn render(source: &str, root: &Path, own_path: &Path) -> String {
    // 1. Expand ```view blocks, back-to-front so earlier ranges stay valid.
    let mut output = source.to_string();
    let blocks = views::parse_view_blocks(source);
    for block in blocks.iter().rev() {
        let table = match find_folder(root, &block.from) {
            Some(folder) => {
                views::render_view_table(block, load_records(&folder, Some(own_path)))
            }
            None => format!(
                "**#ERROR: no folder named \"{}\" found for this view**\n",
                block.from
            ),
        };
        output.replace_range(block.range.clone(), &table);
    }

    // 2. Build the field index: page fields, cross-page references, and
    //    database aggregates — mirroring the preview's update pass.
    let mut fields = formulas::build_field_index(&output);
    for page in formulas::referenced_pages(&output) {
        let Some(path) = find_page(root, &page) else {
            continue;
        };
        let Ok(page_source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (key, value) in formulas::build_field_index(&page_source) {
            fields.insert(formulas::cross_page_key(&page, &key), value);
        }
    }
    let mut databases: HashMap<String, Vec<views::ViewRecord>> = HashMap::new();
    for (function, database, field, criteria) in formulas::referenced_database_aggregates(&output)
    {
        let database_key = database.to_ascii_lowercase();
        if !databases.contains_key(&database_key)
            && let Some(folder) = find_folder(root, &database)
        {
            databases.insert(database_key.clone(), load_records(&folder, Some(own_path)));
        }
        let Some(records) = databases.get(&database_key) else {
            continue;
        };
        let filtered: Vec<views::ViewRecord> = records
            .iter()
            .filter(|record| {
                criteria
                    .as_deref()
                    .is_none_or(|criteria| views::record_matches(record, criteria))
            })
            .map(|record| views::ViewRecord {
                page: record.page.clone(),
                fields: record.fields.clone(),
            })
            .collect();
        let field = field.as_deref().filter(|field| !field.is_empty());
        if let Some(value) = views::compute_summary(&function[1..], field, &filtered) {
            fields.insert(
                formulas::database_aggregate_key(
                    &function,
                    &database,
                    field,
                    criteria.as_deref(),
                ),
                value,
            );
        }
    }

    // 3. Replace inline `= formula` code spans with their values, skipping
    //    fenced code blocks.
    let mut rendered = String::with_capacity(output.len());
    let mut in_fence = false;
    for line in output.split_inclusive('\n') {
        if line.trim().starts_with("```") {
            in_fence = !in_fence;
            rendered.push_str(line);
            continue;
        }
        if in_fence {
            rendered.push_str(line);
            continue;
        }
        rendered.push_str(&replace_formulas(line, &fields));
    }
    rendered
}

/// Replaces `` `= expr` `` spans within one line with their computed values.
fn replace_formulas(line: &str, fields: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("`= ") {
        let Some(length) = rest[start + 1..].find('`') else {
            break;
        };
        let expression = &rest[start + 3..start + 1 + length];
        result.push_str(&rest[..start]);
        result.push_str(&formulas::evaluate(expression, fields));
        rest = &rest[start + 2 + length..];
    }
    result.push_str(rest);
    result
}

/// First directory under `root` (recursively) whose name — or root-relative
/// path — matches, like the preview's worktree folder lookup.
fn find_folder(root: &Path, name: &str) -> Option<PathBuf> {
    if let Ok(relative) = std::fs::canonicalize(root.join(name))
        && relative.is_dir()
    {
        return Some(relative);
    }
    walk(root, &mut |path| {
        path.is_dir()
            && path
                .file_name()
                .is_some_and(|file_name| file_name.to_string_lossy().eq_ignore_ascii_case(name))
    })
}

/// First `<name>.md` under `root`, matched case-insensitively by file stem.
fn find_page(root: &Path, name: &str) -> Option<PathBuf> {
    let file_name = format!("{name}.md");
    walk(root, &mut |path| {
        path.is_file()
            && path
                .file_name()
                .is_some_and(|candidate| candidate.to_string_lossy().eq_ignore_ascii_case(&file_name))
    })
}

fn walk(root: &Path, matches: &mut dyn FnMut(&Path) -> bool) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut directories = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if matches(&path) {
            return Some(path);
        }
        if path.is_dir() {
            directories.push(path);
        }
    }
    for directory in directories {
        if let Some(found) = walk(&directory, matches) {
            return Some(found);
        }
    }
    None
}

/// Every markdown page under `folder` as a record, excluding the view's own
/// page.
fn load_records(folder: &Path, own_path: Option<&Path>) -> Vec<views::ViewRecord> {
    let mut records = Vec::new();
    let mut paths = Vec::new();
    collect_markdown(folder, &mut paths);
    for path in paths {
        if own_path.is_some_and(|own| {
            std::fs::canonicalize(&path).ok().as_deref() == Some(own)
        }) {
            continue;
        }
        let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy().to_string()) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        records.push(views::ViewRecord {
            page: stem,
            fields: formulas::build_field_index(&source),
        });
    }
    records
}

fn collect_markdown(directory: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            collect_markdown(&path, paths);
        } else if path.extension().is_some_and(|extension| extension == "md") {
            paths.push(path);
        }
    }
}
