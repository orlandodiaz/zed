use settings::{RegisterSetting, Settings};

/// Settings for the markdown preview.
#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct MarkdownPreviewSettings {
    /// How much larger than body text display math (`$$…$$`) is rendered.
    pub display_math_scale: f32,
}

impl Settings for MarkdownPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let display_math_scale = content
            .markdown_preview
            .as_ref()
            .and_then(|settings| settings.display_math_scale)
            .unwrap_or(1.4);
        Self { display_math_scale }
    }
}
