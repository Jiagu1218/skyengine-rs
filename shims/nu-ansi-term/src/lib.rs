//! No-op stand-in for the `nu-ansi-term` crate.
//!
//! The real crate depends on `windows-sys` when compiled for Windows, and on
//! the GNU host toolchain `windows-sys` needs `dlltool`+`as` from a MinGW
//! installation that this machine does not have. The only consumer in our
//! dependency tree is `tracing-subscriber`'s optional ANSI colouring
//! (`use nu_ansi_term::{Color, Style}`), and skyengine never renders tracing
//! output through it. This shim reproduces exactly the API surface that
//! `tracing-subscriber` touches, with `paint` emitting the input verbatim
//! (no ANSI escape codes), so the workspace builds on the GNU host toolchain.
//!
//! Patched in via `[patch.crates-io]` in the workspace manifest.

use std::fmt;

/// ANSI colour, mirroring `nu_ansi_term::Color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Black.
    Black,
    /// Dark gray.
    DarkGray,
    /// Red.
    Red,
    /// Light red.
    LightRed,
    /// Green.
    Green,
    /// Light green.
    LightGreen,
    /// Yellow.
    Yellow,
    /// Light yellow.
    LightYellow,
    /// Blue.
    Blue,
    /// Light blue.
    LightBlue,
    /// Purple.
    Purple,
    /// Light purple.
    LightPurple,
    /// Magenta.
    Magenta,
    /// Light magenta.
    LightMagenta,
    /// Cyan.
    Cyan,
    /// Light cyan.
    LightCyan,
    /// White.
    White,
    /// Light gray.
    LightGray,
    /// 256-colour palette index.
    Fixed(u8),
    /// 24-bit RGB.
    Rgb(u8, u8, u8),
    /// Terminal default colour.
    Default,
}

/// A text style, mirroring `nu_ansi_term::Style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground colour, if any.
    pub foreground: Option<Color>,
    /// Background colour, if any.
    pub background: Option<Color>,
    /// Bold flag.
    pub is_bold: bool,
    /// Dimmed flag.
    pub is_dimmed: bool,
    /// Italic flag.
    pub is_italic: bool,
    /// Underline flag.
    pub is_underline: bool,
    /// Blink flag.
    pub is_blink: bool,
    /// Reverse-video flag.
    pub is_reverse: bool,
    /// Hidden flag.
    pub is_hidden: bool,
    /// Strikethrough flag.
    pub is_strikethrough: bool,
    /// Always start output with a reset code (ignored by this shim).
    pub prefix_with_reset: bool,
}

impl Style {
    /// Creates a style with no properties set.
    pub const fn new() -> Style {
        Style {
            foreground: None,
            background: None,
            is_bold: false,
            is_dimmed: false,
            is_italic: false,
            is_underline: false,
            is_blink: false,
            is_reverse: false,
            is_hidden: false,
            is_strikethrough: false,
            prefix_with_reset: false,
        }
    }

    /// Returns a copy with `prefix_with_reset` set.
    pub const fn reset_before_style(&self) -> Style {
        Style {
            prefix_with_reset: true,
            ..*self
        }
    }

    /// Returns a copy with the given foreground colour.
    pub const fn fg(&self, foreground: Color) -> Style {
        Style {
            foreground: Some(foreground),
            ..*self
        }
    }

    /// Returns a copy with the given background colour.
    pub const fn on(&self, background: Color) -> Style {
        Style {
            background: Some(background),
            ..*self
        }
    }

    /// Returns a copy with the bold flag set.
    pub const fn bold(&self) -> Style {
        Style {
            is_bold: true,
            ..*self
        }
    }

    /// Returns a copy with the dimmed flag set.
    pub const fn dimmed(&self) -> Style {
        Style {
            is_dimmed: true,
            ..*self
        }
    }

    /// Returns a copy with the italic flag set.
    pub const fn italic(&self) -> Style {
        Style {
            is_italic: true,
            ..*self
        }
    }

    /// Returns a copy with the underline flag set.
    pub const fn underline(&self) -> Style {
        Style {
            is_underline: true,
            ..*self
        }
    }

    /// Returns a copy with the blink flag set.
    pub const fn blink(&self) -> Style {
        Style {
            is_blink: true,
            ..*self
        }
    }

    /// Returns a copy with the reverse-video flag set.
    pub const fn reverse(&self) -> Style {
        Style {
            is_reverse: true,
            ..*self
        }
    }

    /// Returns a copy with the hidden flag set.
    pub const fn hidden(&self) -> Style {
        Style {
            is_hidden: true,
            ..*self
        }
    }

    /// Returns a copy with the strikethrough flag set.
    pub const fn strikethrough(&self) -> Style {
        Style {
            is_strikethrough: true,
            ..*self
        }
    }

    /// Returns true when this style carries no colour and no attribute.
    pub fn is_plain(self) -> bool {
        self == Style::new()
    }

    /// Wraps `input` for display. This shim emits the input verbatim.
    pub fn paint<S: fmt::Display>(self, input: S) -> Painted<S> {
        Painted { input }
    }

    /// The prefix bytes for this style; this shim emits nothing.
    pub const fn prefix(self) -> Prefix {
        Prefix
    }

    /// The infix bytes between this style and `next`; this shim emits nothing.
    pub const fn infix(self, _next: Style) -> Infix {
        Infix
    }

    /// The suffix bytes for this style; this shim emits nothing.
    pub const fn suffix(self) -> Suffix {
        Suffix
    }
}

/// Output of [`Style::prefix`]; renders as the empty string.
pub struct Prefix;

/// Output of [`Style::infix`]; renders as the empty string.
pub struct Infix;

/// Output of [`Style::suffix`]; renders as the empty string.
pub struct Suffix;

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("")
    }
}

impl fmt::Display for Infix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("")
    }
}

impl fmt::Display for Suffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("")
    }
}

/// Output of [`Style::paint`]; renders the inner value without escape codes.
pub struct Painted<S> {
    input: S,
}

impl<S: fmt::Display> fmt::Display for Painted<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.input.fmt(f)
    }
}

impl Color {
    /// Returns a style with this colour as the foreground.
    pub const fn normal(self) -> Style {
        Style::new().fg(self)
    }

    /// Returns a bold style with this colour as the foreground.
    pub const fn bold(self) -> Style {
        self.normal().bold()
    }

    /// Returns a dimmed style with this colour as the foreground.
    pub const fn dimmed(self) -> Style {
        self.normal().dimmed()
    }

    /// Returns an italic style with this colour as the foreground.
    pub const fn italic(self) -> Style {
        self.normal().italic()
    }

    /// Returns an underlined style with this colour as the foreground.
    pub const fn underline(self) -> Style {
        self.normal().underline()
    }

    /// Returns a blinking style with this colour as the foreground.
    pub const fn blink(self) -> Style {
        self.normal().blink()
    }

    /// Returns a reverse-video style with this colour as the foreground.
    pub const fn reverse(self) -> Style {
        self.normal().reverse()
    }

    /// Returns a hidden style with this colour as the foreground.
    pub const fn hidden(self) -> Style {
        self.normal().hidden()
    }

    /// Returns a strikethrough style with this colour as the foreground.
    pub const fn strikethrough(self) -> Style {
        self.normal().strikethrough()
    }

    /// Returns a style with this foreground over `background`.
    pub const fn on(self, background: Color) -> Style {
        self.normal().on(background)
    }

    /// Wraps `input` for display. This shim emits the input verbatim.
    pub fn paint<S: fmt::Display>(self, input: S) -> Painted<S> {
        Painted { input }
    }

    /// The prefix bytes for this colour; this shim emits nothing.
    pub const fn prefix(self) -> Prefix {
        Prefix
    }

    /// The infix bytes between this colour and `next`; this shim emits nothing.
    pub const fn infix(self, _next: Color) -> Infix {
        Infix
    }

    /// The suffix bytes for this colour; this shim emits nothing.
    pub const fn suffix(self) -> Suffix {
        Suffix
    }
}

impl From<Color> for Style {
    fn from(color: Color) -> Style {
        color.normal()
    }
}
