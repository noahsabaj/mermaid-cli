use ratatui::style::Color;

pub use mermaid_ui::theme::*;

pub trait ColorValueExt {
    fn to_color(&self) -> Color;
}

impl ColorValueExt for ColorValue {
    fn to_color(&self) -> Color {
        match self {
            Self::Rgb { r, g, b } => Color::Rgb(*r, *g, *b),
            Self::Named(name) => match name.as_str() {
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
