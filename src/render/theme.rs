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

    /// Muted meta text (tool headers, byline timestamps) — deliberately a
    /// specific mid-gray rather than the terminal's ANSI gray, which most
    /// palettes render much brighter. Serde-defaulted like `brand`.
    #[serde(default = "default_text_meta")]
    pub text_meta: ColorValue,
    /// Background band behind added diff lines.
    #[serde(default = "default_diff_added_bg")]
    pub diff_added_bg: ColorValue,
    /// Background band behind removed diff lines.
    #[serde(default = "default_diff_removed_bg")]
    pub diff_removed_bg: ColorValue,
    /// Highlight band behind queued (mid-run steering) messages in the
    /// status area.
    #[serde(default = "default_queued_bg")]
    pub queued_bg: ColorValue,
}

/// Mermaid's aqua brand accent, used when a theme omits `brand`.
fn default_brand() -> ColorValue {
    ColorValue::Rgb {
        r: 34,
        g: 211,
        b: 238,
    }
}

/// Dark-theme values double as serde defaults so themes/configs predating
/// these fields keep today's exact colors.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    Rgb { r: u8, g: u8, b: u8 },
    Named(String),
}

impl ColorValue {
    pub fn to_color(&self) -> Color {
        match self {
            Self::Rgb { r, g, b } => Color::Rgb(*r, *g, *b),
            Self::Named(name) => match name.as_str() {
                // The terminal's own default fg/bg — what `Theme::plain()`
                // (NO_COLOR) is built from.
                "default" => Color::Reset,
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
    /// Create a light theme. Selected by `ui.theme = "light"` in config.toml
    /// or `/theme light` (see `render()`'s theme memo).
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
                text_meta: default_text_meta(),
                diff_added_bg: default_diff_added_bg(),
                diff_removed_bg: default_diff_removed_bg(),
                queued_bg: default_queued_bg(),
            },
        }
    }

    /// Colorless theme for `NO_COLOR`: every slot is the terminal's own
    /// default fg/bg (`Color::Reset`), so nothing emits a color at all.
    /// Structure (glyphs, layout, bold/dim) is untouched — diffs still read
    /// via their `+`/`-` prefixes.
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
    fn named_default_maps_to_reset() {
        assert_eq!(
            ColorValue::Named("default".to_string()).to_color(),
            Color::Reset
        );
    }

    #[test]
    fn plain_theme_is_entirely_reset() {
        // Every slot must resolve to the terminal's own default — a single
        // colored slot would defeat NO_COLOR. Serializing the palette and
        // scanning for any non-"default" value covers all fields without
        // enumerating them (new fields are covered automatically).
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
    fn dark_and_light_populate_the_new_slots() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_ne!(theme.colors.diff_added_bg.to_color(), Color::Reset);
            assert_ne!(theme.colors.diff_removed_bg.to_color(), Color::Reset);
            assert_ne!(theme.colors.queued_bg.to_color(), Color::Reset);
            assert_ne!(theme.colors.text_meta.to_color(), Color::Reset);
        }
    }

    #[test]
    fn theme_colors_deserialize_defaults_new_fields() {
        // A theme serialized before the new slots existed still loads, with
        // the dark values as defaults.
        let dark = Theme::dark();
        let mut json = serde_json::to_value(&dark.colors).unwrap();
        let obj = json.as_object_mut().unwrap();
        for field in ["text_meta", "diff_added_bg", "diff_removed_bg", "queued_bg"] {
            obj.remove(field);
        }
        let colors: ThemeColors = serde_json::from_value(json).unwrap();
        assert_eq!(
            colors.text_meta.to_color(),
            dark.colors.text_meta.to_color()
        );
        assert_eq!(
            colors.diff_added_bg.to_color(),
            dark.colors.diff_added_bg.to_color()
        );
    }
}
