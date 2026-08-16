use serde::{Deserialize, Serialize};

/// Semantic theme token that decouples widgets from concrete RGB/ANSI values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThemeToken {
    Background,
    Foreground,
    Border,
    BorderFocused,
    Header,
    StatusBar,
    TextPrimary,
    TextSecondary,
    TextDisabled,
    TextHighlight,
    TextMeta,
    UserMessage,
    AssistantMessage,
    SystemMessage,
    UserMessageBackground,
    CodeBackground,
    CodeForeground,
    CodeKeyword,
    CodeString,
    CodeComment,
    ModeNormal,
    ModeAcceptEdits,
    ModePlan,
    ModeBypassAll,
    Success,
    Warning,
    Error,
    Info,
    Brand,
    DiffAddedBg,
    DiffRemovedBg,
    QueuedBg,
    Reset,
}

impl ThemeToken {
    #[must_use]
    pub fn resolve(self, theme: &Theme) -> ColorValue {
        theme.colors.get_token(self).clone()
    }
}

/// Text style token describing styling and formatting for a span of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct StyleToken {
    pub fg: Option<ThemeToken>,
    pub bg: Option<ThemeToken>,
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

impl StyleToken {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            dim: false,
            underline: false,
            strikethrough: false,
            reversed: false,
        }
    }

    #[must_use]
    pub const fn fg(mut self, token: ThemeToken) -> Self {
        self.fg = Some(token);
        self
    }

    #[must_use]
    pub const fn bg(mut self, token: ThemeToken) -> Self {
        self.bg = Some(token);
        self
    }

    #[must_use]
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    #[must_use]
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    #[must_use]
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    #[must_use]
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    #[must_use]
    pub const fn reversed(mut self) -> Self {
        self.reversed = true;
        self
    }
}

/// Theme configuration for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub colors: ThemeColors,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeColors {
    // Primary colors
    pub background: ColorValue,
    pub foreground: ColorValue,

    // UI elements
    pub border: ColorValue,
    pub border_focused: ColorValue,
    pub header: ColorValue,
    pub status_bar: ColorValue,

    // Text colors
    pub text_primary: ColorValue,
    pub text_secondary: ColorValue,
    pub text_disabled: ColorValue,
    pub text_highlight: ColorValue,

    // Message colors
    pub user_message: ColorValue,
    pub assistant_message: ColorValue,
    pub system_message: ColorValue,
    /// Full-width background band behind the user's submitted prompt.
    pub user_message_background: ColorValue,

    // Code highlighting
    pub code_background: ColorValue,
    pub code_foreground: ColorValue,
    pub code_keyword: ColorValue,
    pub code_string: ColorValue,
    pub code_comment: ColorValue,

    // Mode colors
    pub mode_normal: ColorValue,
    pub mode_accept_edits: ColorValue,
    pub mode_plan: ColorValue,
    pub mode_bypass_all: ColorValue,

    // Status colors
    pub success: ColorValue,
    pub warning: ColorValue,
    pub error: ColorValue,
    pub info: ColorValue,

    /// Brand accent (aqua #22D3EE).
    #[serde(default = "default_brand")]
    pub brand: ColorValue,

    /// Muted meta text (tool headers, byline timestamps).
    #[serde(default = "default_text_meta")]
    pub text_meta: ColorValue,
    /// Background band behind added diff lines.
    #[serde(default = "default_diff_added_bg")]
    pub diff_added_bg: ColorValue,
    /// Background band behind removed diff lines.
    #[serde(default = "default_diff_removed_bg")]
    pub diff_removed_bg: ColorValue,
    /// Highlight band behind queued messages in the status area.
    #[serde(default = "default_queued_bg")]
    pub queued_bg: ColorValue,
}

fn default_brand() -> ColorValue {
    ColorValue::Rgb {
        r: 34,
        g: 211,
        b: 238,
    }
}

fn default_text_meta() -> ColorValue {
    ColorValue::Rgb {
        r: 136,
        g: 136,
        b: 136,
    }
}

fn default_diff_added_bg() -> ColorValue {
    ColorValue::Rgb {
        r: 20,
        g: 50,
        b: 20,
    }
}

fn default_diff_removed_bg() -> ColorValue {
    ColorValue::Rgb {
        r: 60,
        g: 20,
        b: 20,
    }
}

fn default_queued_bg() -> ColorValue {
    ColorValue::Rgb {
        r: 60,
        g: 60,
        b: 80,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    Rgb { r: u8, g: u8, b: u8 },
    Named(String),
}

impl ThemeColors {
    #[must_use]
    pub fn get_token(&self, token: ThemeToken) -> &ColorValue {
        match token {
            ThemeToken::Background => &self.background,
            ThemeToken::Foreground => &self.foreground,
            ThemeToken::Border => &self.border,
            ThemeToken::BorderFocused => &self.border_focused,
            ThemeToken::Header => &self.header,
            ThemeToken::StatusBar => &self.status_bar,
            ThemeToken::TextPrimary => &self.text_primary,
            ThemeToken::TextSecondary => &self.text_secondary,
            ThemeToken::TextDisabled => &self.text_disabled,
            ThemeToken::TextHighlight => &self.text_highlight,
            ThemeToken::TextMeta => &self.text_meta,
            ThemeToken::UserMessage => &self.user_message,
            ThemeToken::AssistantMessage => &self.assistant_message,
            ThemeToken::SystemMessage => &self.system_message,
            ThemeToken::UserMessageBackground => &self.user_message_background,
            ThemeToken::CodeBackground => &self.code_background,
            ThemeToken::CodeForeground => &self.code_foreground,
            ThemeToken::CodeKeyword => &self.code_keyword,
            ThemeToken::CodeString => &self.code_string,
            ThemeToken::CodeComment => &self.code_comment,
            ThemeToken::ModeNormal => &self.mode_normal,
            ThemeToken::ModeAcceptEdits => &self.mode_accept_edits,
            ThemeToken::ModePlan => &self.mode_plan,
            ThemeToken::ModeBypassAll => &self.mode_bypass_all,
            ThemeToken::Success => &self.success,
            ThemeToken::Warning => &self.warning,
            ThemeToken::Error => &self.error,
            ThemeToken::Info => &self.info,
            ThemeToken::Brand => &self.brand,
            ThemeToken::DiffAddedBg => &self.diff_added_bg,
            ThemeToken::DiffRemovedBg => &self.diff_removed_bg,
            ThemeToken::QueuedBg => &self.queued_bg,
            ThemeToken::Reset => &RESET_COLOR,
        }
    }
}

static RESET_COLOR: ColorValue = ColorValue::Named(String::new());

impl Theme {
    #[must_use]
    pub fn light() -> Self {
        Self {
            name: "Light".to_string(),
            colors: ThemeColors {
                background: ColorValue::Rgb {
                    r: 250,
                    g: 250,
                    b: 250,
                },
                foreground: ColorValue::Rgb {
                    r: 30,
                    g: 30,
                    b: 30,
                },
                border: ColorValue::Named("gray".to_string()),
                border_focused: ColorValue::Named("blue".to_string()),
                header: ColorValue::Named("blue".to_string()),
                status_bar: ColorValue::Named("white".to_string()),
                text_primary: ColorValue::Named("black".to_string()),
                text_secondary: ColorValue::Named("dark_gray".to_string()),
                text_disabled: ColorValue::Named("gray".to_string()),
                text_highlight: ColorValue::Named("magenta".to_string()),
                user_message: ColorValue::Named("blue".to_string()),
                assistant_message: ColorValue::Named("green".to_string()),
                system_message: ColorValue::Named("yellow".to_string()),
                user_message_background: ColorValue::Rgb {
                    r: 230,
                    g: 230,
                    b: 230,
                },
                code_background: ColorValue::Rgb {
                    r: 240,
                    g: 240,
                    b: 240,
                },
                code_foreground: ColorValue::Named("dark_gray".to_string()),
                code_keyword: ColorValue::Named("magenta".to_string()),
                code_string: ColorValue::Named("green".to_string()),
                code_comment: ColorValue::Named("gray".to_string()),
                mode_normal: ColorValue::Named("green".to_string()),
                mode_accept_edits: ColorValue::Named("yellow".to_string()),
                mode_plan: ColorValue::Named("blue".to_string()),
                mode_bypass_all: ColorValue::Named("red".to_string()),
                success: ColorValue::Named("green".to_string()),
                warning: ColorValue::Named("yellow".to_string()),
                error: ColorValue::Named("red".to_string()),
                info: ColorValue::Named("blue".to_string()),
                brand: default_brand(),
                text_meta: ColorValue::Rgb {
                    r: 110,
                    g: 110,
                    b: 110,
                },
                diff_added_bg: ColorValue::Rgb {
                    r: 220,
                    g: 245,
                    b: 220,
                },
                diff_removed_bg: ColorValue::Rgb {
                    r: 250,
                    g: 225,
                    b: 225,
                },
                queued_bg: ColorValue::Rgb {
                    r: 225,
                    g: 225,
                    b: 240,
                },
            },
        }
    }

    #[must_use]
    pub fn dark() -> Self {
        Self {
            name: "Dark".to_string(),
            colors: ThemeColors {
                background: ColorValue::Rgb {
                    r: 20,
                    g: 20,
                    b: 20,
                },
                foreground: ColorValue::Rgb {
                    r: 230,
                    g: 230,
                    b: 230,
                },
                border: ColorValue::Named("dark_gray".to_string()),
                border_focused: ColorValue::Named("cyan".to_string()),
                header: ColorValue::Named("cyan".to_string()),
                status_bar: ColorValue::Named("black".to_string()),
                text_primary: ColorValue::Named("white".to_string()),
                text_secondary: ColorValue::Named("gray".to_string()),
                text_disabled: ColorValue::Named("dark_gray".to_string()),
                text_highlight: ColorValue::Named("yellow".to_string()),
                user_message: ColorValue::Named("blue".to_string()),
                assistant_message: ColorValue::Named("green".to_string()),
                system_message: ColorValue::Named("yellow".to_string()),
                user_message_background: ColorValue::Rgb {
                    r: 54,
                    g: 54,
                    b: 54,
                },
                code_background: ColorValue::Rgb {
                    r: 40,
                    g: 40,
                    b: 40,
                },
                code_foreground: ColorValue::Named("gray".to_string()),
                code_keyword: ColorValue::Named("magenta".to_string()),
                code_string: ColorValue::Named("green".to_string()),
                code_comment: ColorValue::Named("dark_gray".to_string()),
                mode_normal: ColorValue::Named("green".to_string()),
                mode_accept_edits: ColorValue::Named("yellow".to_string()),
                mode_plan: ColorValue::Named("blue".to_string()),
                mode_bypass_all: ColorValue::Named("red".to_string()),
                success: ColorValue::Named("green".to_string()),
                warning: ColorValue::Named("yellow".to_string()),
                error: ColorValue::Named("red".to_string()),
                info: ColorValue::Named("cyan".to_string()),
                brand: default_brand(),
                text_meta: default_text_meta(),
                diff_added_bg: default_diff_added_bg(),
                diff_removed_bg: default_diff_removed_bg(),
                queued_bg: default_queued_bg(),
            },
        }
    }

    #[must_use]
    pub fn plain() -> Self {
        fn d() -> ColorValue {
            ColorValue::Named("default".to_string())
        }
        Self {
            name: "Plain".to_string(),
            colors: ThemeColors {
                background: d(),
                foreground: d(),
                border: d(),
                border_focused: d(),
                header: d(),
                status_bar: d(),
                text_primary: d(),
                text_secondary: d(),
                text_disabled: d(),
                text_highlight: d(),
                user_message: d(),
                assistant_message: d(),
                system_message: d(),
                user_message_background: d(),
                code_background: d(),
                code_foreground: d(),
                code_keyword: d(),
                code_string: d(),
                code_comment: d(),
                mode_normal: d(),
                mode_accept_edits: d(),
                mode_plan: d(),
                mode_bypass_all: d(),
                success: d(),
                warning: d(),
                error: d(),
                info: d(),
                brand: d(),
                text_meta: d(),
                diff_added_bg: d(),
                diff_removed_bg: d(),
                queued_bg: d(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_theme_is_entirely_default() {
        let theme = Theme::plain();
        let json = serde_json::to_value(&theme.colors).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.is_empty());
        for (field, value) in obj {
            assert_eq!(
                value.as_str(),
                Some("default"),
                "plain theme leaks color through `{field}`: {value}"
            );
        }
    }

    #[test]
    fn dark_and_light_populate_the_slots() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_ne!(
                theme.colors.get_token(ThemeToken::DiffAddedBg),
                &ColorValue::Named("default".to_string())
            );
            assert_ne!(
                theme.colors.get_token(ThemeToken::DiffRemovedBg),
                &ColorValue::Named("default".to_string())
            );
            assert_ne!(
                theme.colors.get_token(ThemeToken::QueuedBg),
                &ColorValue::Named("default".to_string())
            );
            assert_ne!(
                theme.colors.get_token(ThemeToken::TextMeta),
                &ColorValue::Named("default".to_string())
            );
        }
    }
}
