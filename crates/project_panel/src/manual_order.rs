use std::collections::HashMap;

/// File name, placed in each directory, that stores that directory's manual
/// child ordering — one entry name per line.
pub const ORDER_FILE_NAME: &str = ".order";

/// Per-directory manual ordering for the project panel.
///
/// Persisted as a plain-text `.order` file inside each directory (one child
/// name per line). Keys here are worktree-relative directory paths (`""` is the
/// worktree root); values are the ordered child names for that directory.
/// Children that are not listed fall back to the configured automatic sort,
/// placed after the listed ones.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ManualOrder {
    dirs: HashMap<String, Vec<String>>,
}

impl ManualOrder {
    /// Parse a `.order` file's contents into child names: one per line,
    /// trimmed, with blank lines ignored.
    pub fn parse_lines(text: &str) -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Replace `dir`'s ordering with `names` (an empty list clears it).
    pub fn set_dir(&mut self, dir: &str, names: Vec<String>) {
        if names.is_empty() {
            self.dirs.remove(dir);
        } else {
            self.dirs.insert(dir.to_string(), names);
        }
    }

    /// `dir`'s ordering serialized for its `.order` file (one name per line),
    /// or an empty string if the directory has no manual ordering.
    pub fn dir_lines(&self, dir: &str) -> String {
        self.dirs
            .get(dir)
            .map(|names| names.join("\n"))
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.dirs.values().all(|names| names.is_empty())
    }

    /// Rank of `name` within directory `dir`, if it is manually ordered.
    pub fn rank(&self, dir: &str, name: &str) -> Option<usize> {
        self.dirs.get(dir)?.iter().position(|listed| listed == name)
    }

    /// Returns `dir`'s order list, reconciled against the current `siblings`
    /// (display order). The first call materializes the display order so
    /// untouched siblings keep their relative positions; later calls append new
    /// siblings and drop ones that no longer exist.
    fn reconciled(&mut self, dir: &str, siblings: &[impl AsRef<str>]) -> &mut Vec<String> {
        let order = self.dirs.entry(dir.to_string()).or_default();
        if order.is_empty() {
            *order = siblings.iter().map(|s| s.as_ref().to_string()).collect();
        } else {
            for sibling in siblings {
                let sibling = sibling.as_ref();
                if !order.iter().any(|listed| listed == sibling) {
                    order.push(sibling.to_string());
                }
            }
            order.retain(|listed| siblings.iter().any(|s| s.as_ref() == listed));
        }
        order
    }

    /// Move `name` within `dir` by `delta` (negative = earlier/up). Returns true
    /// if the order changed.
    pub fn move_within(
        &mut self,
        dir: &str,
        name: &str,
        delta: isize,
        siblings: &[impl AsRef<str>],
    ) -> bool {
        let order = self.reconciled(dir, siblings);
        let Some(index) = order.iter().position(|listed| listed == name) else {
            return false;
        };
        let target = index as isize + delta;
        if target < 0 || target as usize >= order.len() {
            return false;
        }
        order.swap(index, target as usize);
        true
    }

    /// Move `name` to immediately before (or after) `anchor` within `dir`.
    /// Returns true if the order changed.
    pub fn reorder(
        &mut self,
        dir: &str,
        name: &str,
        anchor: &str,
        before: bool,
        siblings: &[impl AsRef<str>],
    ) -> bool {
        if name == anchor {
            return false;
        }
        let order = self.reconciled(dir, siblings);
        let Some(from) = order.iter().position(|listed| listed == name) else {
            return false;
        };
        let item = order.remove(from);
        let Some(mut anchor_index) = order.iter().position(|listed| listed == anchor) else {
            order.insert(from.min(order.len()), item);
            return false;
        };
        if !before {
            anchor_index += 1;
        }
        order.insert(anchor_index.min(order.len()), item);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rank_unlisted_is_none() {
        let order = ManualOrder::default();
        assert_eq!(order.rank("", "a"), None);
    }

    #[test]
    fn test_is_empty() {
        let mut order = ManualOrder::default();
        assert!(order.is_empty());
        assert!(order.move_within("", "a", 1, &["a", "b"]));
        assert!(!order.is_empty());
    }

    #[test]
    fn test_move_within_materializes_then_swaps() {
        let mut order = ManualOrder::default();
        // The first move materializes the display order, then moves "b" up.
        assert!(order.move_within("", "b", -1, &["a", "b", "c"]));
        assert_eq!(order.rank("", "b"), Some(0));
        assert_eq!(order.rank("", "a"), Some(1));
        assert_eq!(order.rank("", "c"), Some(2));
    }

    #[test]
    fn test_move_within_down() {
        let mut order = ManualOrder::default();
        assert!(order.move_within("", "a", 1, &["a", "b", "c"]));
        assert_eq!(order.rank("", "a"), Some(1));
        assert_eq!(order.rank("", "b"), Some(0));
    }

    #[test]
    fn test_move_within_out_of_bounds_is_noop() {
        let mut order = ManualOrder::default();
        // "a" is already first; moving it up changes nothing.
        assert!(!order.move_within("", "a", -1, &["a", "b", "c"]));
    }

    #[test]
    fn test_reconcile_adds_new_and_drops_missing() {
        let mut order = ManualOrder::default();
        assert!(order.move_within("", "b", -1, &["a", "b", "c"])); // ["b", "a", "c"]
        // "c" is gone and "d" is new; the next move reconciles both.
        assert!(order.move_within("", "d", -1, &["b", "a", "d"]));
        assert_eq!(order.rank("", "c"), None);
        assert!(order.rank("", "d").is_some());
    }

    #[test]
    fn test_reorder_before_and_after() {
        let mut order = ManualOrder::default();
        assert!(order.reorder("", "c", "a", true, &["a", "b", "c"]));
        assert_eq!(order.rank("", "c"), Some(0));
        assert_eq!(order.rank("", "a"), Some(1));
        assert_eq!(order.rank("", "b"), Some(2));

        assert!(order.reorder("", "c", "b", false, &["a", "b", "c"]));
        assert_eq!(order.rank("", "b"), Some(1));
        assert_eq!(order.rank("", "c"), Some(2));
    }

    #[test]
    fn test_reorder_onto_self_is_noop() {
        let mut order = ManualOrder::default();
        assert!(!order.reorder("", "a", "a", true, &["a", "b", "c"]));
    }

    #[test]
    fn test_lines_round_trip() {
        let mut order = ManualOrder::default();
        assert!(order.move_within("", "b", -1, &["a", "b", "c"]));
        let lines = order.dir_lines("");

        let mut restored = ManualOrder::default();
        restored.set_dir("", ManualOrder::parse_lines(&lines));
        assert_eq!(restored.rank("", "b"), Some(0));
        assert_eq!(restored.rank("", "a"), Some(1));
        assert_eq!(restored.rank("", "c"), Some(2));
        // Blank lines and surrounding whitespace are ignored.
        let mut padded = ManualOrder::default();
        padded.set_dir("", ManualOrder::parse_lines("\n a \n\n b \n"));
        assert_eq!(padded.rank("", "a"), Some(0));
        assert_eq!(padded.rank("", "b"), Some(1));
    }
}
