//! A screen, in pixels.
//!
//! One module over is what a shell's screen says. This is where that becomes a
//! picture: rectangles for the colours behind the text, shaped glyphs for the
//! text, and a cursor over the top of both.
//!
//! # Why the glyphs are told where to go
//!
//! A terminal is a grid, and a grid is a promise: the character in column forty
//! is directly above the character in column forty of the row below it, in every
//! row, whatever either of them happens to be. Text is normally laid out by the
//! font instead — each glyph advanced by whatever the one before it was worth —
//! and for a monospaced font asked to draw ordinary letters the two agree. They
//! stop agreeing the moment a character is not in that font: a box-drawing
//! character, a symbol, anything the machine had to go and find elsewhere. One
//! such glyph, a fraction of a pixel wider than the column it sits in, and every
//! column after it on that row is wrong.
//!
//! So a run of one-character-per-column cells is laid out on the grid's own
//! spacing rather than the font's, which is what the text system's forced width
//! does: one glyph per column, at the column's own position, whichever font each
//! glyph came from. Ligatures are turned off for the same reason — two
//! characters drawn as one glyph is one column too few, and a terminal's columns
//! are not the font's to combine.
//!
//! The cells that cannot be laid out that way are exactly the ones for which the
//! grid is not the whole story, and they are drawn one at a time: a wide
//! character, which is one glyph the emulator has given two columns, and a
//! character with marks over it, which is several characters that have to be
//! shaped together to land on top of each other. Each is placed at its own
//! column and shaped by the font within it.

use gpui::{
    App, Bounds, FontFeatures, Pixels, Point, ShapedLine, SharedString, StrikethroughStyle,
    TextRun, UnderlineStyle, Window, font, point, px, size,
};
use gpui::{BorderStyle, PaintQuad, fill, outline};

use crate::screen::{CursorShape, Screen, Style, Underline};

/// How tall a row is, as a multiple of the font's size.
const LINE_SPACING: f32 = 1.3;

/// How thick a line drawn under or through text is.
const RULE: Pixels = px(1.0);

/// How thick the cursor is when it is a bar rather than a block.
const BAR: Pixels = px(2.0);

/// How big one cell is, in the font the grid is drawn in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub font_size: Pixels,
    pub line_height: Pixels,
    /// How far one character advances the next, which for a monospaced font is
    /// how wide a column is.
    ///
    /// Deliberately not rounded to whole pixels. It is what the text system is
    /// told to space glyphs by, and a column wider than the font's own advance
    /// would push every glyph a little further from where the font shaped it
    /// than the one before.
    pub cell: Pixels,
}

impl Metrics {
    /// How big a cell is in `family` at `font_size`, on this machine.
    pub fn of(family: &SharedString, font_size: Pixels, window: &Window) -> Self {
        let text = window.text_system();
        let resolved = text.resolve_font(&font(family.clone()));
        Self {
            font_size,
            line_height: (font_size * LINE_SPACING).round(),
            // A font whose advance cannot be measured is one the grid cannot be
            // laid out in either, so the guess only has to be close enough that
            // a window opens and says so.
            cell: text
                .em_advance(resolved, font_size)
                .unwrap_or(font_size * 0.6),
        }
    }
}

/// Everything one frame of a grid draws, worked out before any of it is drawn.
///
/// Kept apart from the drawing because the two happen at different moments: the
/// window works out what a frame contains, and then paints it. Shaping text is
/// the expensive half and belongs in the first.
pub struct Painting {
    line_height: Pixels,
    /// The screen's own colour, and then every cell that asked for a different
    /// one.
    fills: Vec<PaintQuad>,
    texts: Vec<Text>,
    /// Drawn over the text, so that a block covers what is under it.
    cursor: Option<PaintQuad>,
    /// And that character drawn again on top of the cursor, in the colour behind
    /// it, so it can still be read.
    covered: Option<Text>,
}

/// One piece of shaped text and where it goes.
struct Text {
    origin: Point<Pixels>,
    line: ShapedLine,
}

/// Works out everything `screen` draws, in `bounds`.
pub fn painting(
    screen: Screen,
    metrics: Metrics,
    family: &SharedString,
    bounds: Bounds<Pixels>,
    window: &Window,
) -> Painting {
    let origin = bounds.origin;
    let corner = |column: usize, line: usize| {
        point(
            origin.x + metrics.cell * column as f32,
            origin.y + metrics.line_height * line as f32,
        )
    };
    let cells = |column: usize, line: usize, columns: usize| {
        Bounds::new(
            corner(column, line),
            size(metrics.cell * columns as f32, metrics.line_height),
        )
    };

    let mut fills = vec![fill(bounds, screen.background)];
    let mut texts = Vec::new();
    for (line, row) in screen.rows.into_iter().enumerate() {
        for behind in row.fills {
            fills.push(fill(
                cells(behind.column, line, behind.columns),
                behind.color,
            ));
        }
        for run in row.runs {
            texts.push(Text {
                origin: corner(run.column, line),
                line: shape(run.text, run.style, run.pitched, metrics, family, window),
            });
        }
    }

    let (cursor, covered) = match screen.cursor {
        None => (None, None),
        Some(cursor) => {
            let over = cells(cursor.column, cursor.line, cursor.columns);
            let quad = match cursor.shape {
                CursorShape::Block => Some(fill(over, cursor.color)),
                CursorShape::HollowBlock => Some(outline(over, cursor.color, BorderStyle::Solid)),
                CursorShape::Underline => Some(fill(
                    Bounds::new(
                        point(over.origin.x, over.origin.y + over.size.height - BAR),
                        size(over.size.width, BAR),
                    ),
                    cursor.color,
                )),
                CursorShape::Beam => Some(fill(
                    Bounds::new(over.origin, size(BAR, over.size.height)),
                    cursor.color,
                )),
                // A hidden cursor was read as no cursor at all, so this cannot
                // happen; if it ever did, drawing nothing is what it means.
                CursorShape::Hidden => None,
            };
            // Only a shape that covers the character under it has to put it
            // back; the others leave it visible where it was drawn.
            let covered = cursor
                .covered
                .filter(|_| cursor.shape == CursorShape::Block)
                .map(|(text, color)| Text {
                    origin: over.origin,
                    line: shape(text, Style::plain(color), false, metrics, family, window),
                });
            (quad, covered)
        }
    };

    Painting {
        line_height: metrics.line_height,
        fills,
        texts,
        cursor,
        covered,
    }
}

impl Painting {
    /// Draws it.
    pub fn paint(self, window: &mut Window, cx: &mut App) {
        for quad in self.fills {
            window.paint_quad(quad);
        }
        for text in self.texts {
            text.paint(self.line_height, window, cx);
        }
        if let Some(cursor) = self.cursor {
            window.paint_quad(cursor);
        }
        if let Some(covered) = self.covered {
            covered.paint(self.line_height, window, cx);
        }
    }
}

impl Text {
    fn paint(self, line_height: Pixels, window: &mut Window, cx: &mut App) {
        // A line that will not draw is one glyph of one row of one frame. There
        // is no useful account to give of it sixty times a second, and the
        // alternative to carrying on is a window that stops.
        let _ = self.line.paint(self.origin, line_height, window, cx);
    }
}

/// Shapes one run's text, on the grid's spacing where the run allows it.
fn shape(
    text: String,
    style: Style,
    pitched: bool,
    metrics: Metrics,
    family: &SharedString,
    window: &Window,
) -> ShapedLine {
    let mut font = font(family.clone());
    if style.bold {
        font = font.bold();
    }
    if style.italic {
        font = font.italic();
    }
    font.features = FontFeatures::disable_ligatures();

    let styled = TextRun {
        len: text.len(),
        font,
        color: style.color,
        background_color: None,
        underline: style.underline.map(|kind| UnderlineStyle {
            thickness: RULE,
            color: Some(style.color),
            wavy: kind == Underline::Wavy,
        }),
        strikethrough: style.struck.then_some(StrikethroughStyle {
            thickness: RULE,
            color: Some(style.color),
        }),
    };
    window.text_system().shape_line(
        SharedString::from(text),
        metrics.font_size,
        &[styled],
        pitched.then_some(metrics.cell),
    )
}
