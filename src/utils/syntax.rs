use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Syntax highlighter
pub struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    theme_name: String,
}

impl SyntaxHighlighter {
    /// Create a new syntax highlighter
    pub fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            theme_name: "Monokai".to_string(),
        }
    }

    /// Highlight code
    pub fn highlight(&self, code: &str, language: &str) -> Vec<(Style, String)> {
        let syntax = self.syntax_set
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(
            syntax,
            &self.theme_set.themes[&self.theme_name],
        );

        let mut result = Vec::new();
        for line in code.lines() {
            if let Ok(ranges) = highlighter.highlight_line(line, &self.syntax_set) {
                for (style, text) in ranges {
                    result.push((style, text.to_string()));
                }
            }
        }

        result
    }
}