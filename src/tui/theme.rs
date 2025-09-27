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
            },
        }
    }

    /// Create a light theme
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

                border: ColorValue::Rgb {
                    r: 200,
                    g: 200,
                    b: 200,
                },
                border_focused: ColorValue::Rgb {
                    r: 0,
                    g: 100,
                    b: 200,
                },
                header: ColorValue::Rgb {
                    r: 0,
                    g: 100,
                    b: 200,
                },
                status_bar: ColorValue::Rgb {
                    r: 240,
                    g: 240,
                    b: 240,
                },

                text_primary: ColorValue::Named("black".to_string()),
                text_secondary: ColorValue::Rgb {
                    r: 100,
                    g: 100,
                    b: 100,
                },
                text_disabled: ColorValue::Rgb {
                    r: 150,
                    g: 150,
                    b: 150,
                },
                text_highlight: ColorValue::Rgb {
                    r: 200,
                    g: 100,
                    b: 0,
                },

                user_message: ColorValue::Rgb {
                    r: 0,
                    g: 50,
                    b: 200,
                },
                assistant_message: ColorValue::Rgb {
                    r: 0,
                    g: 150,
                    b: 50,
                },
                system_message: ColorValue::Rgb {
                    r: 200,
                    g: 150,
                    b: 0,
                },

                code_background: ColorValue::Rgb {
                    r: 245,
                    g: 245,
                    b: 245,
                },
                code_foreground: ColorValue::Rgb {
                    r: 50,
                    g: 50,
                    b: 50,
                },
                code_keyword: ColorValue::Rgb {
                    r: 150,
                    g: 0,
                    b: 150,
                },
                code_string: ColorValue::Rgb { r: 0, g: 120, b: 0 },
                code_comment: ColorValue::Rgb {
                    r: 120,
                    g: 120,
                    b: 120,
                },

                mode_normal: ColorValue::Rgb {
                    r: 0,
                    g: 150,
                    b: 50,
                },
                mode_accept_edits: ColorValue::Rgb {
                    r: 200,
                    g: 150,
                    b: 0,
                },
                mode_plan: ColorValue::Rgb {
                    r: 0,
                    g: 100,
                    b: 200,
                },
                mode_bypass_all: ColorValue::Rgb { r: 200, g: 0, b: 0 },

                success: ColorValue::Rgb {
                    r: 0,
                    g: 150,
                    b: 50,
                },
                warning: ColorValue::Rgb {
                    r: 200,
                    g: 150,
                    b: 0,
                },
                error: ColorValue::Rgb { r: 200, g: 0, b: 0 },
                info: ColorValue::Rgb {
                    r: 0,
                    g: 150,
                    b: 200,
                },
            },
        }
    }

    /// Create a high-contrast theme
    pub fn high_contrast() -> Self {
        Self {
            name: "High Contrast".to_string(),
            colors: ThemeColors {
                background: ColorValue::Named("black".to_string()),
                foreground: ColorValue::Named("white".to_string()),

                border: ColorValue::Named("white".to_string()),
                border_focused: ColorValue::Named("yellow".to_string()),
                header: ColorValue::Named("yellow".to_string()),
                status_bar: ColorValue::Named("black".to_string()),

                text_primary: ColorValue::Named("white".to_string()),
                text_secondary: ColorValue::Named("cyan".to_string()),
                text_disabled: ColorValue::Named("gray".to_string()),
                text_highlight: ColorValue::Named("yellow".to_string()),

                user_message: ColorValue::Named("cyan".to_string()),
                assistant_message: ColorValue::Named("magenta".to_string()),
                system_message: ColorValue::Named("yellow".to_string()),

                code_background: ColorValue::Rgb {
                    r: 10,
                    g: 10,
                    b: 10,
                },
                code_foreground: ColorValue::Named("white".to_string()),
                code_keyword: ColorValue::Named("yellow".to_string()),
                code_string: ColorValue::Named("cyan".to_string()),
                code_comment: ColorValue::Named("green".to_string()),

                mode_normal: ColorValue::Named("green".to_string()),
                mode_accept_edits: ColorValue::Named("yellow".to_string()),
                mode_plan: ColorValue::Named("cyan".to_string()),
                mode_bypass_all: ColorValue::Named("red".to_string()),

                success: ColorValue::Named("green".to_string()),
                warning: ColorValue::Named("yellow".to_string()),
                error: ColorValue::Named("red".to_string()),
                info: ColorValue::Named("cyan".to_string()),
            },
        }
    }
}

/// Theme manager for handling theme switching
pub struct ThemeManager {
    current_theme: Theme,
    available_themes: Vec<Theme>,
}

impl ThemeManager {
    pub fn new() -> Self {
        Self {
            current_theme: Theme::dark(),
            available_themes: vec![Theme::dark(), Theme::light(), Theme::high_contrast()],
        }
    }

    pub fn current(&self) -> &Theme {
        &self.current_theme
    }

    pub fn set_theme(&mut self, name: &str) {
        if let Some(theme) = self.available_themes.iter().find(|t| t.name == name) {
            self.current_theme = theme.clone();
        }
    }

    pub fn cycle_theme(&mut self) {
        let current_index = self
            .available_themes
            .iter()
            .position(|t| t.name == self.current_theme.name)
            .unwrap_or(0);

        let next_index = (current_index + 1) % self.available_themes.len();
        self.current_theme = self.available_themes[next_index].clone();
    }

    pub fn available_themes(&self) -> Vec<String> {
        self.available_themes
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}
