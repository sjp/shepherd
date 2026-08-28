//! What colour a cell is drawn in.
//!
//! A terminal cell does not carry a colour. It carries a *reference* to one —
//! one of the sixteen colours a terminal has had since the 1980s, one of the
//! 240 more that came with 256-colour terminals, a literal 24-bit value, or the
//! word "the default". Turning that reference into something to draw with is
//! this module's whole job.
//!
//! # Whose colour is whose
//!
//! Three parties have a say and they are kept apart deliberately.
//!
//! The *process* has the first say: a program can redefine any slot in the
//! palette, and one that has done so — an editor with a theme, a program using
//! `OSC 4` — is describing colours it will then use, so what it said is used
//! unchanged. The emulator records exactly that and nothing else, which is why
//! most slots are empty most of the time.
//!
//! The *window's theme* has the last say over two of them, and only two: the
//! default foreground and the default background. Those are the colours a
//! terminal shares with the application around it — a window whose text is
//! black on white while the rest of it is white on black is not a window, it is
//! two windows — and they are the only ones the theme is allowed to touch. A
//! program asking for red means red, whatever the rest of the window looks
//! like.
//!
//! Everything left over is this module's table, below: the values a terminal
//! that has been told nothing uses.

use std::mem;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::{COUNT, Colors};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui::{Hsla, Rgba};

/// The sixteen colours a terminal has when nothing has said otherwise.
///
/// These are the values a stock `xterm` uses, which is what the terminfo entry
/// this application claims describes. Their oddities — a red that is not
/// `ff0000`, a "white" that is light grey — are the point rather than an
/// approximation of it: a program printing colour 1 on any other terminal gets
/// this colour, and matching that is what makes coloured output look the way
/// its author saw it.
const ANSI: [u32; 16] = [
    0x00_00_00, // black
    0xcd_00_00, // red
    0x00_cd_00, // green
    0xcd_cd_00, // yellow
    0x00_00_ee, // blue
    0xcd_00_cd, // magenta
    0x00_cd_cd, // cyan
    0xe5_e5_e5, // white
    0x7f_7f_7f, // bright black
    0xff_00_00, // bright red
    0x00_ff_00, // bright green
    0xff_ff_00, // bright yellow
    0x5c_5c_ff, // bright blue
    0xff_00_ff, // bright magenta
    0x00_ff_ff, // bright cyan
    0xff_ff_ff, // bright white
];

/// The first of the eight bright colours, and so the count of dark ones.
const BRIGHT: u8 = 8;

/// The six levels each channel of the 216-colour cube is built from.
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Where the colour cube starts in the palette.
const CUBE_BASE: u8 = 16;

/// Where the 24-step grey ramp starts in the palette.
const GREYS_BASE: u8 = 232;

/// The darkest grey in that ramp, and the step between one grey and the next.
const GREY_FLOOR: u8 = 8;
const GREY_STEP: u8 = 10;

/// How much of a colour is left when a cell is dim.
///
/// Two thirds, which is what terminals have settled on for a mode whose only
/// definition is "less than normal".
const DIM: f32 = 0.66;

/// The slots that are not one of the 256 indexed colours.
const FOREGROUND: usize = NamedColor::Foreground as usize;
const BACKGROUND: usize = NamedColor::Background as usize;
const CURSOR: usize = NamedColor::Cursor as usize;
const DIM_BLACK: usize = NamedColor::DimBlack as usize;
const DIM_WHITE: usize = NamedColor::DimWhite as usize;
const BRIGHT_FOREGROUND: usize = NamedColor::BrightForeground as usize;
const DIM_FOREGROUND: usize = NamedColor::DimForeground as usize;

/// The colours one shell's grid is drawn in.
#[derive(Clone, Copy)]
pub struct Palette {
    /// The slots the process has redefined. Everything it has not touched is
    /// empty here and answered from the table above.
    asked: Colors,
    foreground: Hsla,
    background: Hsla,
}

impl Palette {
    /// The palette a process asking for `asked` is drawn with, in a window whose
    /// text is `foreground` on `background`.
    pub fn new(asked: Colors, foreground: Hsla, background: Hsla) -> Self {
        Self {
            asked,
            foreground,
            background,
        }
    }

    /// The colour behind a cell that asked for nothing, which is the colour the
    /// whole grid is filled with before anything is drawn on it.
    pub fn background(&self) -> Hsla {
        self.slot(BACKGROUND)
    }

    /// The colour the cursor is drawn in.
    pub fn cursor(&self) -> Hsla {
        self.slot(CURSOR)
    }

    /// What a cell is drawn in: its text's colour, and the colour behind it.
    ///
    /// Inverse video is resolved here rather than left to the caller, because
    /// it is a swap of two colours that have already been worked out and not a
    /// property of either of them.
    pub fn of(&self, foreground: Color, background: Color, flags: Flags) -> (Hsla, Hsla) {
        let mut text = self.text(foreground, flags);
        let mut behind = self.colour(background);
        if flags.contains(Flags::INVERSE) {
            mem::swap(&mut text, &mut behind);
        }
        (text, behind)
    }

    /// The colour text is drawn in, dimmed if the cell is.
    fn text(&self, color: Color, flags: Flags) -> Hsla {
        if !flags.contains(Flags::DIM) {
            return self.colour(color);
        }
        match color {
            // The emulator's own opinion of what each colour's dim counterpart
            // is, rather than a second opinion formed here.
            Color::Named(named) => self.slot(named.to_dim() as usize),
            Color::Indexed(index) if index < BRIGHT => self.slot(DIM_BLACK + usize::from(index)),
            other => dim(self.colour(other)),
        }
    }

    /// The colour one reference names.
    fn colour(&self, color: Color) -> Hsla {
        match color {
            Color::Named(named) => self.slot(named as usize),
            Color::Indexed(index) => self.slot(usize::from(index)),
            Color::Spec(rgb) => from_rgb(rgb),
        }
    }

    /// What is in one slot of the palette: what the process put there, or what
    /// is there when it has put nothing.
    fn slot(&self, slot: usize) -> Hsla {
        if slot < COUNT
            && let Some(rgb) = self.asked[slot]
        {
            return from_rgb(rgb);
        }
        match slot {
            FOREGROUND | BRIGHT_FOREGROUND | CURSOR => self.foreground,
            BACKGROUND => self.background,
            DIM_FOREGROUND => dim(self.foreground),
            DIM_BLACK..=DIM_WHITE => dim(indexed(index_of(slot - DIM_BLACK))),
            _ => indexed(index_of(slot)),
        }
    }
}

/// The colour at one of the 256 indexed slots, when nothing has redefined it.
fn indexed(index: u8) -> Hsla {
    match index {
        0..CUBE_BASE => hex(ANSI[usize::from(index)]),
        CUBE_BASE..GREYS_BASE => {
            let step = index - CUBE_BASE;
            let level = |divisor: u8| CUBE[usize::from(step / divisor % 6)];
            rgb(level(36), level(6), level(1))
        }
        GREYS_BASE..=u8::MAX => {
            // Saturating because the top of the ramp is 238, which fits, and a
            // future arithmetic slip is better as a white than a panic.
            let grey = GREY_FLOOR.saturating_add(GREY_STEP.saturating_mul(index - GREYS_BASE));
            rgb(grey, grey, grey)
        }
    }
}

/// A palette slot as an index into the 256 colours, saturating rather than
/// wrapping so that an out-of-range slot is a colour rather than a panic.
fn index_of(slot: usize) -> u8 {
    u8::try_from(slot).unwrap_or(u8::MAX)
}

/// Two thirds of a colour, channel by channel.
fn dim(color: Hsla) -> Hsla {
    let color = Rgba::from(color);
    Hsla::from(Rgba {
        r: color.r * DIM,
        g: color.g * DIM,
        b: color.b * DIM,
        a: color.a,
    })
}

/// A colour the emulator reported, in the window's own terms.
fn from_rgb(color: Rgb) -> Hsla {
    rgb(color.r, color.g, color.b)
}

/// One opaque colour, from three channels.
fn rgb(r: u8, g: u8, b: u8) -> Hsla {
    Hsla::from(Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    })
}

/// One opaque colour, from the `0xrrggbb` the table above is written in.
fn hex(color: u32) -> Hsla {
    let channel = |shift: u32| u8::try_from((color >> shift) & 0xff).unwrap_or(u8::MAX);
    rgb(channel(16), channel(8), channel(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A palette a process has asked nothing of, on white text over black.
    fn plain() -> Palette {
        Palette::new(Colors::default(), hex(0xff_ff_ff), hex(0x00_00_00))
    }

    #[test]
    fn the_default_foreground_and_background_come_from_the_window() {
        let palette = plain();
        let (text, behind) = palette.of(
            Color::Named(NamedColor::Foreground),
            Color::Named(NamedColor::Background),
            Flags::empty(),
        );
        assert_eq!(text, hex(0xff_ff_ff));
        assert_eq!(behind, hex(0x00_00_00));
        assert_eq!(palette.background(), behind);
    }

    #[test]
    fn a_colour_the_process_set_is_used_unchanged() {
        let mut asked = Colors::default();
        asked[NamedColor::Red] = Some(Rgb { r: 1, g: 2, b: 3 });
        asked[NamedColor::Background] = Some(Rgb { r: 4, g: 5, b: 6 });
        let palette = Palette::new(asked, hex(0xff_ff_ff), hex(0x00_00_00));

        let (text, behind) = palette.of(
            Color::Named(NamedColor::Red),
            Color::Named(NamedColor::Background),
            Flags::empty(),
        );
        assert_eq!(text, rgb(1, 2, 3));
        assert_eq!(
            behind,
            rgb(4, 5, 6),
            "a process that redefined the background is drawn on the one it asked for"
        );
    }

    #[test]
    fn the_sixteen_named_colours_are_the_ones_every_terminal_has() {
        let palette = plain();
        assert_eq!(
            palette.colour(Color::Named(NamedColor::Red)),
            hex(0xcd_00_00)
        );
        assert_eq!(palette.colour(Color::Indexed(1)), hex(0xcd_00_00));
        assert_eq!(
            palette.colour(Color::Named(NamedColor::BrightRed)),
            hex(0xff_00_00)
        );
        assert_eq!(palette.colour(Color::Indexed(9)), hex(0xff_00_00));
    }

    #[test]
    fn the_colour_cube_is_six_levels_of_each_channel() {
        assert_eq!(indexed(16), rgb(0, 0, 0), "the corner the cube starts at");
        assert_eq!(indexed(231), rgb(255, 255, 255), "and the one it ends at");
        assert_eq!(indexed(17), rgb(0, 0, 95), "one step of blue");
        assert_eq!(indexed(22), rgb(0, 95, 0), "one step of green");
        assert_eq!(indexed(52), rgb(95, 0, 0), "one step of red");
    }

    #[test]
    fn the_greys_are_a_ramp_of_twenty_four() {
        assert_eq!(indexed(232), rgb(8, 8, 8));
        assert_eq!(indexed(255), rgb(238, 238, 238));
    }

    #[test]
    fn a_literal_colour_is_taken_as_it_was_given() {
        assert_eq!(
            plain().colour(Color::Spec(Rgb {
                r: 10,
                g: 20,
                b: 30
            })),
            rgb(10, 20, 30)
        );
    }

    #[test]
    fn inverse_video_swaps_the_two_colours_a_cell_was_going_to_be() {
        let palette = plain();
        let (text, behind) = palette.of(
            Color::Named(NamedColor::Red),
            Color::Named(NamedColor::Blue),
            Flags::INVERSE,
        );
        assert_eq!(text, hex(0x00_00_ee));
        assert_eq!(behind, hex(0xcd_00_00));
    }

    #[test]
    fn a_dim_cell_is_dimmed_in_the_text_and_not_behind_it() {
        let palette = plain();
        let (text, behind) = palette.of(
            Color::Named(NamedColor::Red),
            Color::Named(NamedColor::Blue),
            Flags::DIM,
        );
        assert_eq!(text, dim(hex(0xcd_00_00)));
        assert_eq!(
            behind,
            hex(0x00_00_ee),
            "the colour behind a cell is not dim"
        );
    }

    #[test]
    fn a_dim_bright_colour_is_the_plain_one_the_emulator_says_it_is() {
        let palette = plain();
        assert_eq!(
            palette.text(Color::Named(NamedColor::BrightRed), Flags::DIM),
            hex(0xcd_00_00),
        );
        assert_eq!(
            palette.text(Color::Indexed(1), Flags::DIM),
            dim(hex(0xcd_00_00)),
            "one of the first eight asked for by number dims like the colour it is",
        );
        assert_eq!(
            palette.text(Color::Indexed(200), Flags::DIM),
            dim(indexed(200)),
            "and one from the cube is simply dimmed",
        );
    }

    #[test]
    fn the_cursor_is_the_default_foreground_until_the_process_says_otherwise() {
        assert_eq!(plain().cursor(), hex(0xff_ff_ff));
        let mut asked = Colors::default();
        asked[NamedColor::Cursor] = Some(Rgb { r: 7, g: 8, b: 9 });
        assert_eq!(
            Palette::new(asked, hex(0xff_ff_ff), hex(0x00_00_00)).cursor(),
            rgb(7, 8, 9)
        );
    }
}
