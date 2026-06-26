use gpui::Hsla;
use settings::{RegisterSetting, Settings};

/// Settings for the markdown preview.
#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct MarkdownPreviewSettings {
    /// How much larger than body text display math (`$$…$$`) is rendered.
    pub display_math_scale: f32,
    /// Multiplier on the monospace font size of fenced code blocks.
    pub code_font_scale: f32,
    /// Multiplier on the monospace font size of inline `code` spans.
    pub inline_code_font_scale: f32,
    /// Background color of inline `code` chips. `None` keeps the theme default.
    pub inline_code_background: Option<Hsla>,
    /// Text color of inline `code`. `None` keeps the surrounding text color.
    pub inline_code_color: Option<Hsla>,
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
        let inline_code_font_scale = markdown_preview
            .and_then(|settings| settings.inline_code_font_scale)
            .unwrap_or(1.0);
        let inline_code_background = markdown_preview
            .and_then(|settings| settings.inline_code_background)
            .map(Hsla::from);
        let inline_code_color = markdown_preview
            .and_then(|settings| settings.inline_code_color)
            .map(Hsla::from);
        Self {
            display_math_scale,
            code_font_scale,
            inline_code_font_scale,
            inline_code_background,
            inline_code_color,
        }
    }
}
