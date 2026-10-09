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
    pub header: ColorValue,

    // Text colors
    pub text_primary: ColorValue,
    pub text_secondary: ColorValue,
    pub text_disabled: ColorValue,
    pub text_highlight: ColorValue,

    /// Full-width background band behind the user's submitted prompt
    /// (Claude-Code style). A clearly-visible neutral gray, a step above the
    /// main background — not a blue tint. The `/load` and rewind pickers lay
    /// their highlighted row on it too.
    pub user_message_background: ColorValue,

    // Code highlighting
    pub code_background: ColorValue,
    pub code_foreground: ColorValue,
    pub code_keyword: ColorValue,
    pub code_string: ColorValue,
    pub code_comment: ColorValue,

    // Status colors
    pub success: ColorValue,
    pub warning: ColorValue,
    pub error: ColorValue,
    pub info: ColorValue,

    /// Brand accent (aqua #22D3EE; the light theme carries a deeper aqua that
    /// reads on white) — the `ask_user_question` modal's header chip and
    /// border, the checklist marks, the model-picker cursor. Serde-defaulted
    /// so themes/configs predating it still load.
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
    #[must_use]
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

/// A fixed 24-bit colour, which renders the same on every terminal palette.
const fn rgb(r: u8, g: u8, b: u8) -> ColorValue {
    ColorValue::Rgb { r, g, b }
}

impl Theme {
    /// The palette a frame draws with. `NO_COLOR` beats the theme choice
    /// (colours off entirely); otherwise `ui.theme` / `/theme` picks dark or
    /// light. The TUI and the `--resume` picker both resolve through here, so
    /// the picker can never disagree with the session it opens.
    #[must_use]
    pub fn resolve(choice: mermaid_domain::ThemeChoice, no_color: bool) -> Self {
        if no_color {
            return Self::plain();
        }
        match choice {
            mermaid_domain::ThemeChoice::Dark => Self::dark(),
            mermaid_domain::ThemeChoice::Light => Self::light(),
        }
    }

    /// Create a light theme. Selected by `ui.theme = "light"` in config.toml
    /// or `/theme light` (see `render()`'s theme memo).
    ///
    /// The greys, the green, the yellow and the brand accent are fixed RGB
    /// here, not ANSI names: a terminal palette tunes its gray, green and
    /// yellow for a dark background, and on a light one they wash out (on
    /// Windows Terminal's default palette, gray reads at 1.5:1 against this
    /// background and yellow at 2.5:1). Each fixed value clears WCAG AA on
    /// the ground it is drawn on, 4.5:1 for text and 3:1 for a border, which
    /// `light_theme_fixed_inks_meet_contrast_on_their_grounds` pins.
    #[must_use]
    pub fn light() -> Self {
        Self {
            name: "Light".to_string(),
            colors: ThemeColors {
                background: rgb(250, 250, 250),
                foreground: rgb(30, 30, 30),

                border: rgb(112, 112, 112),
                header: ColorValue::Named("blue".to_string()),

                text_primary: ColorValue::Named("black".to_string()),
                text_secondary: rgb(85, 85, 85),
                text_disabled: rgb(112, 112, 112),
                text_highlight: ColorValue::Named("magenta".to_string()),

                user_message_background: rgb(230, 230, 230),

                code_background: rgb(240, 240, 240),
                code_foreground: rgb(85, 85, 85),
                code_keyword: ColorValue::Named("magenta".to_string()),
                code_string: rgb(20, 117, 54),
                code_comment: rgb(105, 105, 105),

                success: rgb(20, 117, 54),
                warning: rgb(148, 98, 0),
                error: ColorValue::Named("red".to_string()),
                info: ColorValue::Named("blue".to_string()),

                // The dark theme's aqua is 1.7:1 on this background; the same
                // hue, deepened until it reads as text and under the chip label.
                brand: rgb(14, 116, 144),
                text_meta: rgb(110, 110, 110),
                diff_added_bg: rgb(220, 245, 220),
                diff_removed_bg: rgb(250, 225, 225),
                queued_bg: rgb(225, 225, 240),
            },
        }
    }

    /// Create the default dark theme
    #[must_use]
    pub fn dark() -> Self {
        Self {
            name: "Dark".to_string(),
            colors: ThemeColors {
                background: rgb(20, 20, 20),
                foreground: rgb(230, 230, 230),

                border: ColorValue::Named("dark_gray".to_string()),
                header: ColorValue::Named("cyan".to_string()),

                text_primary: ColorValue::Named("white".to_string()),
                text_secondary: ColorValue::Named("gray".to_string()),
                text_disabled: ColorValue::Named("dark_gray".to_string()),
                text_highlight: ColorValue::Named("yellow".to_string()),

                user_message_background: rgb(54, 54, 54),

                code_background: rgb(40, 40, 40),
                code_foreground: ColorValue::Named("gray".to_string()),
                code_keyword: ColorValue::Named("magenta".to_string()),
                code_string: ColorValue::Named("green".to_string()),
                code_comment: ColorValue::Named("dark_gray".to_string()),

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
                header: d(),
                text_primary: d(),
                text_secondary: d(),
                text_disabled: d(),
                text_highlight: d(),
                user_message_background: d(),
                code_background: d(),
                code_foreground: d(),
                code_keyword: d(),
                code_string: d(),
                code_comment: d(),
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

    /// WCAG 2 relative luminance of a fixed RGB slot, or `None` for a named
    /// ANSI slot, whose shade belongs to the terminal's palette.
    fn luminance(value: &ColorValue) -> Option<f64> {
        let ColorValue::Rgb { r, g, b } = value else {
            return None;
        };
        let linear = |channel: u8| {
            let srgb = f64::from(channel) / 255.0;
            if srgb <= 0.04045 {
                srgb / 12.92
            } else {
                ((srgb + 0.055) / 1.055).powf(2.4)
            }
        };
        Some(
            [(0.2126, *r), (0.7152, *g), (0.0722, *b)]
                .into_iter()
                .map(|(weight, channel)| weight * linear(channel))
                .sum(),
        )
    }

    #[test]
    fn light_theme_fixed_inks_meet_contrast_on_their_grounds() {
        // Each light slot the theme pins to RGB, on the ground it is drawn
        // on: 4.5:1 for text, 3:1 for a border (WCAG AA). A slot switched
        // back to an ANSI name fails here too, since its contrast is then the
        // terminal's to decide.
        let c = Theme::light().colors;
        let pairs = [
            ("text_secondary", &c.text_secondary, &c.background, 4.5),
            ("text_disabled", &c.text_disabled, &c.background, 4.5),
            ("text_meta", &c.text_meta, &c.background, 4.5),
            ("brand", &c.brand, &c.background, 4.5),
            ("the question chip label", &c.background, &c.brand, 4.5),
            ("success", &c.success, &c.background, 4.5),
            ("success on its diff row", &c.success, &c.diff_added_bg, 4.5),
            ("warning", &c.warning, &c.background, 4.5),
            (
                "code_foreground",
                &c.code_foreground,
                &c.code_background,
                4.5,
            ),
            ("code_string", &c.code_string, &c.code_background, 4.5),
            ("code_comment", &c.code_comment, &c.code_background, 4.5),
            ("border", &c.border, &c.background, 3.0),
        ];
        for (slot, ink, ground, floor) in pairs {
            let ratio = luminance(ink)
                .zip(luminance(ground))
                .map_or(0.0, |(ink, ground)| {
                    (ink.max(ground) + 0.05) / (ink.min(ground) + 0.05)
                });
            assert!(
                ratio >= floor,
                "light {slot} is {ratio:.2}:1, under the {floor}:1 floor \
                 (0 means an ANSI name, whose contrast the terminal decides)"
            );
        }
    }

    #[test]
    fn resolve_lets_no_color_beat_the_theme_choice() {
        use mermaid_domain::ThemeChoice;
        assert_eq!(Theme::resolve(ThemeChoice::Dark, false).name, "Dark");
        assert_eq!(Theme::resolve(ThemeChoice::Light, false).name, "Light");
        assert_eq!(Theme::resolve(ThemeChoice::Light, true).name, "Plain");
        assert_eq!(Theme::resolve(ThemeChoice::Dark, true).name, "Plain");
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
