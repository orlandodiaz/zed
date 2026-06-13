use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Worktree-relative path to the file that stores a worktree's manual ordering.
pub const MANUAL_ORDER_REL_PATH: &str = ".zed/panel-order.json";

/// Per-directory manual ordering for the project panel.
///
/// Persisted to `<worktree-root>/.zed/panel-order.json`. Keys are
/// worktree-relative directory paths (`""` is the worktree root); values are
/// the ordered child names for that directory. Children that are not listed
/// fall back to the configured automatic sort, placed after the listed ones.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManualOrder {
    dirs: HashMap<String, Vec<String>>,
}

impl ManualOrder {
    pub fn from_json(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
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
