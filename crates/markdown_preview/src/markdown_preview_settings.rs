use settings::{RegisterSetting, Settings};

/// Settings for the markdown preview.
#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct MarkdownPreviewSettings {
    /// How much larger than body text display math (`$$…$$`) is rendered.
    pub display_math_scale: f32,
    /// Multiplier on the code (monospace) font size, for inline code and code blocks.
    pub code_font_scale: f32,
}

impl Settings for MarkdownPreviewSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let markdown_preview = content.markdown_preview.as_ref();
        let display_math_scale = markdown_preview
            .and_then(|settings| settings.display_math_scale)
            .unwrap_or(1.4);
        let code_font_scale = markdown_preview
            .and_then(|settings| settings.code_font_scale)
            .unwrap_or(0.85);
        Self {
            display_math_scale,
            code_font_scale,
        }
    }
}
