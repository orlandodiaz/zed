//! Resolves a markdown page's icon from its YAML frontmatter so several pages
//! can share one icon file instead of each needing its own copy. A page declares
//!
//! ```text
//! ---
//! icon: huntington
//! ---
//! ```
//!
//! and the name resolves to a single shared `assets/huntington.svg`. The
//! worktree-relative lookup of that file lives with each caller (the preview and
//! the project panel) since it needs their worktree handle; this module only
//! parses the frontmatter, caching disk reads by modification time so a panel
//! that re-resolves every frame doesn't re-read every page.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// Frontmatter sits at the very top of a page, so only the head needs reading.
const FRONTMATTER_READ_LIMIT: u64 = 4096;

/// Extracts the `icon:` value from a leading YAML frontmatter block (`---` … `---`).
/// Returns `None` when the source has no frontmatter or no `icon` key. The value
/// names a shared icon in `assets/` (e.g. `icon: huntington`).
pub fn frontmatter_icon(source: &str) -> Option<String> {
    let mut lines = source.lines();
    // The opening fence must be the first line; a bare `---` elsewhere is a rule.
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("icon:") {
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

struct CacheEntry {
    mtime: Option<SystemTime>,
    icon: Option<String>,
}

static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Reads the `icon:` frontmatter of the page at `path`, caching by modification
/// time so repeated lookups (e.g. one per visible panel row, every frame) cost a
/// `stat` rather than a read+parse.
pub fn frontmatter_icon_for_file(path: &Path) -> Option<String> {
    let key = path.to_string_lossy().into_owned();
    let mtime = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok();
    let mut cache = CACHE.lock().ok()?;
    if let Some(entry) = cache.get(&key)
        && entry.mtime == mtime
    {
        return entry.icon.clone();
    }
    let icon = read_head(path).and_then(|head| frontmatter_icon(&head));
    cache.insert(key, CacheEntry { mtime, icon: icon.clone() });
    icon
}

fn read_head(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.take(FRONTMATTER_READ_LIMIT)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

#[cfg(test)]
mod tests {
    use super::frontmatter_icon;

    #[test]
    fn reads_icon_key() {
        assert_eq!(
            frontmatter_icon("---\nicon: huntington\ntitle: HNB\n---\n# Body"),
            Some("huntington".to_string())
        );
    }

    #[test]
    fn strips_quotes_and_whitespace() {
        assert_eq!(
            frontmatter_icon("---\nicon:  \"brands/dxc\" \n---\n"),
            Some("brands/dxc".to_string())
        );
    }

    #[test]
    fn ignores_rule_that_is_not_frontmatter() {
        assert_eq!(frontmatter_icon("# Title\n\n---\nicon: x\n---\n"), None);
    }

    #[test]
    fn none_when_key_absent_or_empty() {
        assert_eq!(frontmatter_icon("---\ntitle: HNB\n---\n"), None);
        assert_eq!(frontmatter_icon("---\nicon:\n---\n"), None);
        assert_eq!(frontmatter_icon("no frontmatter here"), None);
    }
}
