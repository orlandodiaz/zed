use serde::Deserialize;
use settings::{RegisterSetting, Settings};

/// Settings for the markdown search (`markdown_search::Toggle`).
#[derive(Deserialize, Debug, Clone, PartialEq, RegisterSetting)]
pub struct MarkdownSearchSettings {
    /// Directory globs (worktree-relative) whose folders count as "wiki" folders.
    pub wiki_paths: Vec<String>,
}

impl Settings for MarkdownSearchSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let wiki_paths = content
            .markdown_search
            .as_ref()
            .and_then(|settings| settings.wiki_paths.clone())
            .unwrap_or_else(|| vec!["**/*wiki*".to_string()]);
        Self { wiki_paths }
    }
}
