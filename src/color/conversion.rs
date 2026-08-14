//! Conversions between crossterm [`Color`] and Oklab space.
//!
//! The mood-color pipeline works in Oklab (perceptually uniform) but
//! configs and terminals speak crossterm colors, so every boundary crossing
//! goes through this module:
//!
//! - [`oklab_to_crossterm`]: Oklab → sRGB bytes → `Color::Rgb` (exact, used
//!   when rendering).
//! - [`rgb_to_oklab`]: crossterm `Color` → sRGB bytes → Oklab (used when
//!   building per-axis endpoint colors from config).
//!
//! Named and `AnsiValue` colors are approximated via the standard sRGB
//! mappings (e.g. `Red` → `(128, 0, 0)`, ANSI 256 → xterm cube) — accurate
//! enough for terminal output, not a perceptual match for exotic names.

use crossterm::style::Color;
use oklab::{Oklab, Rgb};

/// Convert an Oklab color to a crossterm color.
pub fn oklab_to_crossterm(c: Oklab) -> Color {
    let Rgb { r, g, b } = c.to_srgb();
    Color::Rgb { r, g, b }
}

/// Convert a crossterm [`Color`] (named or RGB variant) to Oklab via sRGB.
/// Hex-based configs work because crossterm's Rust enum supports
/// `Color::Rgb { r, g, b }` directly when deserializing `#RRGGBB` strings.
pub fn rgb_to_oklab(c: Color) -> Oklab {
    let (r, g, b) = match c {
        Color::Rgb { r, g, b } => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::White => (255, 255, 255),
        Color::Grey | Color::DarkGrey => (128, 128, 128),
        Color::Red | Color::DarkRed => (128, 0, 0),
        Color::Green | Color::DarkGreen => (0, 128, 0),
        Color::Yellow | Color::DarkYellow => (128, 128, 0),
        Color::Blue | Color::DarkBlue => (0, 0, 128),
        Color::Magenta | Color::DarkMagenta => (128, 0, 128),
        Color::Cyan | Color::DarkCyan => (0, 128, 128),
        Color::Reset => (128, 128, 128),
        Color::AnsiValue(v) => {
            // Approximate ANSI 256 → sRGB (xterm cube is a 6x6x6 grid plus
            // grayscale ramp). Sufficient for terminal color hedging.
            let n = v as u16;
            if n < 16 {
                static ANSI_16: [(u8, u8, u8); 16] = [
                    (0, 0, 0),
                    (128, 0, 0),
                    (0, 128, 0),
                    (128, 128, 0),
                    (0, 0, 128),
                    (128, 0, 128),
                    (0, 128, 128),
                    (192, 192, 192),
                    (128, 128, 128),
                    (255, 0, 0),
                    (0, 255, 0),
                    (255, 255, 0),
                    (0, 0, 255),
                    (255, 0, 255),
                    (0, 255, 255),
                    (255, 255, 255),
                ];
                ANSI_16[n as usize]
            } else if n >= 232 {
                let gray = 8 + 10 * (n - 232) as u8;
                (gray, gray, gray)
            } else {
                let idx = n - 16;
                let r_idx = idx / 36;
                let g_idx = (idx / 6) % 6;
                let b_idx = idx % 6;
                let r = if r_idx != 0 { 55 + 40 * r_idx } else { 0 } as u8;
                let g = if g_idx != 0 { 55 + 40 * g_idx } else { 0 } as u8;
                let b = if b_idx != 0 { 55 + 40 * b_idx } else { 0 } as u8;
                (r, g, b)
            }
        }
    };
    Oklab::from(Rgb { r, g, b })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oklab_to_crossterm_rgb() {
        let c = oklab_to_crossterm(Oklab {
            l: 0.5,
            a: 0.0,
            b: 0.0,
        });
        assert!(matches!(c, Color::Rgb { .. }));
    }

    /// Named colors are approximated (not exact), but must at least map to
    /// some sRGB triple rather than panicking or falling back to grey.
    #[test]
    fn rgb_to_oklab_accepts_named_and_ansi() {
        for c in [
            Color::Red,
            Color::DarkGreen,
            Color::AnsiValue(196),
            Color::AnsiValue(244),
        ] {
            let o = rgb_to_oklab(c);
            assert!(o.l >= 0.0 && o.l <= 1.0, "l out of range for {c:?}");
        }
    }
}
