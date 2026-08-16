use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

use crate::theme::{StyleToken, ThemeToken};

/// A viewport representing the terminal or output surface dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    pub width: u16,
    pub height: u16,
}

impl Viewport {
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    #[must_use]
    pub const fn area(self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }
}

/// A 2D rectangular area on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    #[must_use]
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Border styling options for containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderStyle {
    #[default]
    None,
    Plain,
    Rounded,
    Double,
    Focused,
    TopBottom,
    Left,
}

/// Layout flow direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlexDirection {
    #[default]
    Vertical,
    Horizontal,
}

/// Sizing constraint for child elements in a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    Length(u16),
    Min(u16),
    Max(u16),
    Percentage(u16),
    Ratio(u32, u32),
    Fill(u16),
}

/// A styled segment of text within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub content: Cow<'static, str>,
    pub style: StyleToken,
}

impl Span {
    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            content: Cow::Owned(content.into()),
            style: StyleToken::default(),
        }
    }

    #[must_use]
    pub fn styled(content: impl Into<String>, style: StyleToken) -> Self {
        Self {
            content: Cow::Owned(content.into()),
            style,
        }
    }

    #[must_use]
    pub fn width(&self) -> usize {
        UnicodeWidthStr::width(self.content.as_ref())
    }
}

/// A line composed of styled spans.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    #[must_use]
    pub const fn new() -> Self {
        Self { spans: Vec::new() }
    }

    #[must_use]
    pub fn raw(content: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::raw(content)],
        }
    }

    #[must_use]
    pub fn styled(content: impl Into<String>, style: StyleToken) -> Self {
        Self {
            spans: vec![Span::styled(content, style)],
        }
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.content.is_empty())
    }

    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }
}

impl From<Vec<Span>> for Line {
    fn from(spans: Vec<Span>) -> Self {
        Self { spans }
    }
}

impl From<Span> for Line {
    fn from(span: Span) -> Self {
        Self { spans: vec![span] }
    }
}

impl From<String> for Line {
    fn from(s: String) -> Self {
        Self {
            spans: vec![Span::raw(s)],
        }
    }
}

impl From<&'static str> for Line {
    fn from(s: &'static str) -> Self {
        Self {
            spans: vec![Span::raw(s)],
        }
    }
}

/// Declarative Virtual UI Node tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiNode {
    Empty,
    Text {
        lines: Vec<Line>,
        wrap: bool,
    },
    Container {
        direction: FlexDirection,
        constraints: Vec<Constraint>,
        children: Vec<UiNode>,
        border: BorderStyle,
        border_token: Option<ThemeToken>,
        title: Option<String>,
        bg: Option<ThemeToken>,
    },
}

impl UiNode {
    #[must_use]
    pub const fn empty() -> Self {
        Self::Empty
    }

    #[must_use]
    pub fn text(lines: Vec<Line>) -> Self {
        Self::Text { lines, wrap: false }
    }

    #[must_use]
    pub fn wrapped_text(lines: Vec<Line>) -> Self {
        Self::Text { lines, wrap: true }
    }

    #[must_use]
    pub fn vertical(children: Vec<UiNode>, constraints: Vec<Constraint>) -> Self {
        Self::Container {
            direction: FlexDirection::Vertical,
            constraints,
            children,
            border: BorderStyle::None,
            border_token: None,
            title: None,
            bg: None,
        }
    }

    #[must_use]
    pub fn horizontal(children: Vec<UiNode>, constraints: Vec<Constraint>) -> Self {
        Self::Container {
            direction: FlexDirection::Horizontal,
            constraints,
            children,
            border: BorderStyle::None,
            border_token: None,
            title: None,
            bg: None,
        }
    }

    #[must_use]
    pub fn with_border(mut self, border: BorderStyle, token: Option<ThemeToken>) -> Self {
        if let Self::Container {
            border: ref mut b,
            border_token: ref mut t,
            ..
        } = self
        {
            *b = border;
            *t = token;
        }
        self
    }

    #[must_use]
    pub fn with_title(mut self, title: String) -> Self {
        if let Self::Container {
            title: ref mut t, ..
        } = self
        {
            *t = Some(title);
        }
        self
    }

    #[must_use]
    pub fn with_bg(mut self, token: ThemeToken) -> Self {
        if let Self::Container { ref mut bg, .. } = self {
            *bg = Some(token);
        }
        self
    }
}
