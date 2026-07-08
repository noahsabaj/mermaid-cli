use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// Theme configuration for the TUI
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
    /// Full-width background band behind the user's submitted prompt
    /// (Claude-Code style). A clearly-visible neutral gray, a step above the
    /// main background — not a blue tint.
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

    /// Brand accent (aqua #22D3EE) — the `ask_user_question` modal's header chip
    /// and border. Serde-defaulted so themes/configs predating it still load.
    #[serde(default = "default_brand")]
    pub brand: ColorValue,
}

/// Mermaid's aqua brand accent, used when a theme omits `brand`.
fn default_brand() -> ColorValue {
    ColorValue::Rgb {
        r: 34,
        g: 211,
        b: 238,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    Rgb { r: u8, g: u8, b: u8 },
    Named(String),
}

impl ColorValue {
    pub fn to_color(&self) -> Color {
        match self {
            ColorValue::Rgb { r, g, b } => Color::Rgb(*r, *g, *b),
            ColorValue::Named(name) => match name.as_str() {
                "black" => Color::Black,
                "red" => Color::Red,
                "green" => Color::Green,
                "yellow" => Color::Yellow,
                "blue" => Color::Blue,
                "magenta" => Color::Magenta,
                "cyan" => Color::Cyan,
                "white" => Color::White,
                "gray" | "grey" => Color::Gray,
                "dark_gray" | "dark_grey" => Color::DarkGray,
                _ => Color::White,
            },
        }
    }
}

impl Theme {
    /// Create a light theme.
    ///
    /// Not wired into any config reader yet — the `Deserialize` impl on
    /// `Theme` is retained so a future patch can load
    /// `config.ui.theme = "light" | "dark"` from config.toml and select
    /// the constructor. Until then, call this explicitly to test.
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
            },
        }
    }

    /// Create the default dark theme
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
            },
        }
    }
}
