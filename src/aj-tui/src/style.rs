//! Terminal styling helpers.
//!
//! Provides simple functions for applying ANSI SGR styles to text.

/// Apply bold styling to text.
pub fn bold(text: &str) -> String {
    format!("\x1b[1m{}\x1b[22m", text)
}

/// Apply dim styling to text.
pub fn dim(text: &str) -> String {
    format!("\x1b[2m{}\x1b[22m", text)
}

/// Apply italic styling to text.
pub fn italic(text: &str) -> String {
    format!("\x1b[3m{}\x1b[23m", text)
}

/// Apply underline styling to text.
pub fn underline(text: &str) -> String {
    format!("\x1b[4m{}\x1b[24m", text)
}

/// Apply strikethrough styling to text.
pub fn strikethrough(text: &str) -> String {
    format!("\x1b[9m{}\x1b[29m", text)
}

/// Apply inverse (reverse video) styling to text.
pub fn inverse(text: &str) -> String {
    format!("\x1b[7m{}\x1b[27m", text)
}

/// Apply a foreground color (standard 4-bit: 30-37, 90-97).
pub fn fg(text: &str, color_code: u8) -> String {
    format!("\x1b[{}m{}\x1b[39m", color_code, text)
}

/// Apply a background color (standard 4-bit: 40-47, 100-107).
pub fn bg(text: &str, color_code: u8) -> String {
    format!("\x1b[{}m{}\x1b[49m", color_code, text)
}

/// Apply a 256-color foreground.
pub fn fg256(text: &str, color: u8) -> String {
    format!("\x1b[38;5;{}m{}\x1b[39m", color, text)
}

/// Apply a 256-color background.
pub fn bg256(text: &str, color: u8) -> String {
    format!("\x1b[48;5;{}m{}\x1b[49m", color, text)
}

/// Apply an RGB foreground color.
pub fn fg_rgb(text: &str, r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{};{};{}m{}\x1b[39m", r, g, b, text)
}

/// Apply an RGB background color.
pub fn bg_rgb(text: &str, r: u8, g: u8, b: u8) -> String {
    format!("\x1b[48;2;{};{};{}m{}\x1b[49m", r, g, b, text)
}

// Standard color constants for convenience.

/// Red foreground.
pub fn red(text: &str) -> String {
    fg(text, 31)
}

/// Green foreground.
pub fn green(text: &str) -> String {
    fg(text, 32)
}

/// Yellow foreground.
pub fn yellow(text: &str) -> String {
    fg(text, 33)
}

/// Blue foreground.
pub fn blue(text: &str) -> String {
    fg(text, 34)
}

/// Magenta foreground.
pub fn magenta(text: &str) -> String {
    fg(text, 35)
}

/// Cyan foreground.
pub fn cyan(text: &str) -> String {
    fg(text, 36)
}

/// White foreground.
pub fn white(text: &str) -> String {
    fg(text, 37)
}

/// Gray (bright black) foreground.
pub fn gray(text: &str) -> String {
    fg(text, 90)
}
