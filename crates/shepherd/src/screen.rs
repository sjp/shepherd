//! What is on a shell's screen, copied out so it can be drawn.
//!
//! The emulator's grid is behind a lock that the thread parsing the shell's
//! output is waiting on. So nothing here draws from that grid: it is read once,
//! quickly, into the owned description below, and everything after that —
//! choosing fonts, shaping glyphs, filling rectangles — happens with the lock
//! long since released.
//!
//! # Runs, not cells
//!
//! A cell at a time would be correct and slow: eighty columns of a shaped glyph
//! each, eighty times over, for a screen that is mostly one colour. So
//! neighbouring cells that are drawn the same way are gathered into runs, and a
//! run is what reaches the text system. A row of ordinary output is usually one
//! of them.
//!
//! The exception is what makes a terminal grid hard, and it is the reason a run
//! says whether it is *pitched*. A run of pitched cells is one character per
//! column, and can be laid out on the grid's own spacing — the text system is
//! told the column width and puts one glyph in each. A cell that is not like
//! that is its own run:
//!
//! - a **wide** character — most of CJK, and emoji — is one character that the
//!   emulator has given two columns, with a spacer cell after it that holds
//!   nothing and is never drawn;
//! - a cell carrying **combining marks** is several characters in one column,
//!   which have to be shaped together for the mark to land on the letter it
//!   belongs to rather than beside it.
//!
//! Both are the emulator's classification rather than one worked out here. It
//! is a terminal emulator: it has already had to decide how many columns every
//! character it printed was worth, and disagreeing with that decision is how a
//! renderer ends up drawing something the process did not mean.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, point_to_viewport};
use gpui::Hsla;

pub use alacritty_terminal::vte::ansi::CursorShape;

use crate::palette::Palette;

/// What one column is worth when a cell holds a character of ordinary width.
const NARROW: usize = 1;

/// And when it holds one the emulator gave two columns to.
const WIDE: usize = 2;

/// One shell's screen, as it was at the moment it was read.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    /// The colour behind everything, before any cell has been drawn.
    pub background: Hsla,
    /// The rows of the viewport, top to bottom.
    pub rows: Vec<Row>,
    /// Where the cursor is, unless it is hidden or scrolled out of view.
    pub cursor: Option<Cursor>,
}

/// One row of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The cells whose background is not the screen's, in column order.
    pub fills: Vec<Fill>,
    /// The text, in column order.
    pub runs: Vec<Run>,
}

/// A stretch of columns with a colour behind them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fill {
    pub column: usize,
    pub columns: usize,
    pub color: Hsla,
}

/// A stretch of columns drawn as one piece of text.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// The column it starts at.
    pub column: usize,
    /// How many columns it covers.
    pub columns: usize,
    pub text: String,
    pub style: Style,
    /// Whether this is one character per column.
    ///
    /// A pitched run can be laid out on the grid's spacing, which is what keeps
    /// a row in its columns whatever font each character was found in. One that
    /// is not pitched is a single cell — a wide character, or a character with
    /// marks over it — and is laid out by the font instead, because the whole
    /// point of it is that its parts belong together.
    pub pitched: bool,
}

/// How a run's text looks, beyond where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub color: Hsla,
    pub bold: bool,
    pub italic: bool,
    pub underline: Option<Underline>,
    pub struck: bool,
}

impl Style {
    /// Plain text of one colour, which is what a cursor draws over itself.
    pub fn plain(color: Hsla) -> Self {
        Self {
            color,
            bold: false,
            italic: false,
            underline: None,
            struck: false,
        }
    }
}

/// The kinds of underline a cell can ask for.
///
/// A terminal can ask for five; a straight line and a wavy one are what can be
/// drawn, so the three that are neither are drawn as the straight line they are
/// a variation of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Underline {
    Straight,
    Wavy,
}

/// Where the cursor is and what it looks like.
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    /// Which row of the viewport it is on, and which column.
    pub line: usize,
    pub column: usize,
    /// How many columns it covers, which is two when it sits on a wide
    /// character.
    pub columns: usize,
    pub shape: CursorShape,
    pub color: Hsla,
    /// The character the cursor is drawn over and the colour to draw it in, for
    /// the shapes that cover it. `None` when it covers nothing worth redrawing.
    pub covered: Option<(String, Hsla)>,
}

impl Screen {
    /// Reads `term`'s viewport, in a window whose text is `foreground` on
    /// `background`.
    ///
    /// Called with the terminal's lock held, so it does exactly as much as it
    /// has to and nothing else.
    pub fn of<T: EventListener>(term: &Term<T>, foreground: Hsla, background: Hsla) -> Self {
        let content = term.renderable_content();
        let palette = Palette::new(*content.colors, foreground, background);
        let offset = content.display_offset;
        let shape = content.cursor.shape;
        let at = content.cursor.point;

        let grid = term.grid();
        let lines = grid.screen_lines();
        let rows = (0..lines)
            .map(|line| {
                let line = i32::try_from(line).unwrap_or(i32::MAX)
                    - i32::try_from(offset).unwrap_or(i32::MAX);
                row(&grid[Line(line)][..], &palette)
            })
            .collect();

        // A cursor that is hidden, or that is somewhere the viewport is no
        // longer showing because the screen has been scrolled back, is not
        // drawn rather than drawn somewhere else.
        let cursor = (shape != CursorShape::Hidden)
            .then(|| point_to_viewport(offset, at))
            .flatten()
            .filter(|point| point.line < lines && point.column.0 < grid.columns())
            .map(|point| {
                let cell = &grid[at];
                let (_, behind) = palette.of(cell.fg, cell.bg, cell.flags);
                let covered = covering(cell).map(|text| (text, behind));
                Cursor {
                    line: point.line,
                    column: point.column.0,
                    columns: columns(cell, grid.columns() - point.column.0),
                    shape,
                    color: palette.cursor(),
                    covered,
                }
            });

        Self {
            background: palette.background(),
            rows,
            cursor,
        }
    }
}

/// One row of cells, as the runs and fills it is drawn as.
fn row(cells: &[Cell], palette: &Palette) -> Row {
    let background = palette.background();
    let mut fills: Vec<Fill> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut column = 0;

    while column < cells.len() {
        let cell = &cells[column];
        // The second half of a wide character. It holds nothing of its own: the
        // character before it is drawn across both.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            column += 1;
            continue;
        }

        let (color, behind) = palette.of(cell.fg, cell.bg, cell.flags);
        let columns = columns(cell, cells.len() - column);

        // Only what differs from the screen's own colour is filled, because the
        // screen has already been filled with that.
        if behind != background {
            match fills.last_mut() {
                Some(last) if last.color == behind && last.column + last.columns == column => {
                    last.columns += columns;
                }
                _ => fills.push(Fill {
                    column,
                    columns,
                    color: behind,
                }),
            }
        }

        let style = Style {
            color,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: underline(cell.flags),
            struck: cell.flags.contains(Flags::STRIKEOUT),
        };
        let marks = cell.zerowidth().unwrap_or_default();
        // A hidden cell still takes up its columns; it just has nothing in them.
        let hidden = cell.flags.contains(Flags::HIDDEN);

        if hidden || (columns == NARROW && marks.is_empty()) {
            let character = if hidden { ' ' } else { plain(cell.c) };
            let extend = matches!(
                runs.last(),
                Some(last) if last.pitched
                    && last.style == style
                    && last.column + last.columns == column
            );
            if !extend {
                runs.push(Run {
                    column,
                    columns: 0,
                    text: String::new(),
                    style,
                    pitched: true,
                });
            }
            let run = runs.last_mut().expect("a run was just pushed or found");
            for _ in 0..columns {
                run.text.push(character);
                run.columns += 1;
            }
        } else {
            let mut text = String::with_capacity(1 + marks.len());
            text.push(plain(cell.c));
            text.extend(marks);
            runs.push(Run {
                column,
                columns,
                text,
                style,
                pitched: false,
            });
        }

        column += columns;
    }

    trim(&mut runs);
    Row { fills, runs }
}

/// How many columns a cell occupies, of the `left` there are after it.
fn columns(cell: &Cell, left: usize) -> usize {
    let columns = if cell.flags.contains(Flags::WIDE_CHAR) {
        WIDE
    } else {
        NARROW
    };
    columns.min(left).max(NARROW)
}

/// What a cell's character is worth on screen.
///
/// A tab is stored as itself in the cell it starts at, so that what a row says
/// can be recovered from it; drawn, it is the blank it looked like when it was
/// printed.
fn plain(character: char) -> char {
    if character == '\t' { ' ' } else { character }
}

/// The underline a cell's flags ask for, if any.
fn underline(flags: Flags) -> Option<Underline> {
    if flags.contains(Flags::UNDERCURL) {
        Some(Underline::Wavy)
    } else if flags.intersects(Flags::ALL_UNDERLINES) {
        Some(Underline::Straight)
    } else {
        None
    }
}

/// What a cursor covers, when that is worth drawing again over the cursor.
fn covering(cell: &Cell) -> Option<String> {
    if cell.flags.contains(Flags::HIDDEN) {
        return None;
    }
    let character = plain(cell.c);
    let marks = cell.zerowidth().unwrap_or_default();
    if character == ' ' && marks.is_empty() {
        return None;
    }
    let mut text = String::with_capacity(1 + marks.len());
    text.push(character);
    text.extend(marks);
    Some(text)
}

/// Drops the blanks at the end of a row.
///
/// Every row is as wide as the screen, and most of them end in a long stretch of
/// nothing. Shaping that stretch would be most of the work of drawing a terminal
/// and none of the result. Cells carrying a line under them or through them are
/// not blank however empty they look, so trimming stops at one.
fn trim(runs: &mut Vec<Run>) {
    while let Some(last) = runs.last_mut() {
        if !last.pitched || last.style.underline.is_some() || last.style.struck {
            return;
        }
        let kept = last.text.trim_end_matches(' ').len();
        // Every trailing blank is one byte and one column, so what was dropped
        // counts the same either way.
        let dropped = last.text.len() - kept;
        if dropped == 0 {
            return;
        }
        last.text.truncate(kept);
        last.columns -= dropped;
        if !last.text.is_empty() {
            return;
        }
        runs.pop();
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::color::Colors;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb, StdSyncHandler};
    use gpui::{Hsla, Rgba};

    use super::*;

    const WHITE: Hsla = Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: 1.0,
    };
    const BLACK: Hsla = Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.0,
        a: 1.0,
    };

    fn palette() -> Palette {
        Palette::new(Colors::default(), WHITE, BLACK)
    }

    /// A row of `columns` cells, the first of them holding `text`.
    fn line(text: &str, columns: usize) -> Vec<Cell> {
        let mut cells = vec![Cell::default(); columns];
        for (cell, character) in cells.iter_mut().zip(text.chars()) {
            cell.c = character;
        }
        cells
    }

    /// A wide character and the spacer the emulator puts after it.
    fn wide(character: char) -> [Cell; 2] {
        [
            Cell {
                c: character,
                flags: Flags::WIDE_CHAR,
                ..Cell::default()
            },
            Cell {
                c: ' ',
                flags: Flags::WIDE_CHAR_SPACER,
                ..Cell::default()
            },
        ]
    }

    /// The colour a cell asking for `rgb` is drawn behind.
    fn colour(r: u8, g: u8, b: u8) -> Hsla {
        Hsla::from(Rgba {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: 1.0,
        })
    }

    #[test]
    fn a_row_of_ordinary_text_is_one_run_on_the_grids_own_spacing() {
        let drawn = row(&line("hello", 80), &palette());
        assert_eq!(drawn.runs.len(), 1);
        let run = &drawn.runs[0];
        assert_eq!(run.column, 0);
        assert_eq!(run.text, "hello");
        assert_eq!(run.columns, 5, "one column per character");
        assert!(run.pitched);
    }

    #[test]
    fn the_blanks_at_the_end_of_a_row_are_not_drawn() {
        let drawn = row(&line("hi", 200), &palette());
        assert_eq!(drawn.runs.len(), 1);
        assert_eq!(drawn.runs[0].columns, 2);
        assert!(
            row(&line("", 200), &palette()).runs.is_empty(),
            "a row of nothing draws nothing"
        );
    }

    #[test]
    fn blanks_a_row_still_needs_are_kept() {
        let drawn = row(&line("a b", 80), &palette());
        assert_eq!(drawn.runs.len(), 1);
        assert_eq!(
            drawn.runs[0].text, "a b",
            "a space between two words is a column"
        );

        let mut cells = line("", 4);
        for cell in &mut cells {
            cell.flags = Flags::UNDERLINE;
        }
        let drawn = row(&cells, &palette());
        assert_eq!(
            drawn.runs.len(),
            1,
            "a blank with a line under it has something to draw"
        );
        assert_eq!(drawn.runs[0].columns, 4);
    }

    #[test]
    fn a_wide_character_covers_two_columns_and_its_spacer_draws_nothing() {
        let mut cells = line("a", 80);
        cells.splice(1..3, wide('\u{4e2d}'));
        let drawn = row(&cells, &palette());

        assert_eq!(drawn.runs.len(), 2);
        assert_eq!(drawn.runs[0].text, "a");
        let run = &drawn.runs[1];
        assert_eq!(run.column, 1);
        assert_eq!(run.columns, 2, "the character is two columns wide");
        assert_eq!(run.text, "\u{4e2d}");
        assert!(
            !run.pitched,
            "a character wider than its column is laid out by the font, not the grid"
        );
    }

    #[test]
    fn what_follows_a_wide_character_starts_at_the_column_after_its_spacer() {
        let mut cells = line("", 80);
        cells.splice(0..2, wide('\u{4e2d}'));
        cells[2].c = 'x';
        let drawn = row(&cells, &palette());

        let last = drawn.runs.last().expect("the row has text in it");
        assert_eq!(last.text, "x");
        assert_eq!(last.column, 2);
    }

    #[test]
    fn a_combining_mark_stays_with_the_character_it_belongs_to() {
        let mut cells = line("ae", 80);
        cells[1].push_zerowidth('\u{0301}');
        let drawn = row(&cells, &palette());

        assert_eq!(drawn.runs.len(), 2);
        assert_eq!(
            drawn.runs[0].text, "a",
            "the plain letter before it is its own run"
        );
        let marked = &drawn.runs[1];
        assert_eq!(marked.text, "e\u{0301}");
        assert_eq!(marked.column, 1);
        assert_eq!(
            marked.columns, 1,
            "a mark over a letter does not take a column of its own"
        );
        assert!(
            !marked.pitched,
            "the letter and its mark are shaped together"
        );
    }

    #[test]
    fn a_change_of_colour_starts_a_new_run() {
        let mut cells = line("abcd", 80);
        cells[2].fg = Color::Named(NamedColor::Red);
        cells[3].fg = Color::Named(NamedColor::Red);
        let drawn = row(&cells, &palette());

        assert_eq!(drawn.runs.len(), 2);
        assert_eq!(drawn.runs[0].text, "ab");
        assert_eq!(drawn.runs[1].text, "cd");
        assert_eq!(drawn.runs[1].column, 2);
        assert_ne!(drawn.runs[0].style.color, drawn.runs[1].style.color);
    }

    #[test]
    fn only_a_background_of_its_own_is_filled() {
        assert!(
            row(&line("abc", 80), &palette()).fills.is_empty(),
            "the screen is already the colour these cells asked for"
        );

        let mut cells = line("abc", 80);
        for cell in &mut cells[0..2] {
            cell.bg = Color::Spec(Rgb { r: 1, g: 2, b: 3 });
        }
        let drawn = row(&cells, &palette());
        assert_eq!(drawn.fills.len(), 1);
        assert_eq!(drawn.fills[0].column, 0);
        assert_eq!(
            drawn.fills[0].columns, 2,
            "neighbours of one colour are one fill"
        );
        assert_eq!(drawn.fills[0].color, colour(1, 2, 3));
    }

    #[test]
    fn a_wide_characters_background_covers_both_its_columns() {
        let mut cells = line("", 80);
        let mut pair = wide('\u{4e2d}');
        for cell in &mut pair {
            cell.bg = Color::Named(NamedColor::Red);
        }
        cells.splice(0..2, pair);
        let drawn = row(&cells, &palette());

        assert_eq!(drawn.fills.len(), 1);
        assert_eq!(drawn.fills[0].columns, 2);
    }

    #[test]
    fn a_hidden_cell_keeps_its_columns_and_shows_nothing() {
        let mut cells = line("abc", 80);
        cells[1].flags = Flags::HIDDEN;
        let drawn = row(&cells, &palette());

        assert_eq!(drawn.runs.len(), 1);
        assert_eq!(drawn.runs[0].text, "a c");
    }

    #[test]
    fn a_tab_is_drawn_as_the_blank_it_looked_like() {
        let mut cells = line("a", 80);
        cells[1].c = '\t';
        cells[8].c = 'b';
        let drawn = row(&cells, &palette());

        assert_eq!(drawn.runs.len(), 1);
        assert_eq!(drawn.runs[0].text, "a       b");
    }

    #[test]
    fn every_kind_of_underline_is_one_of_the_two_that_can_be_drawn() {
        assert_eq!(underline(Flags::empty()), None);
        assert_eq!(underline(Flags::UNDERLINE), Some(Underline::Straight));
        assert_eq!(
            underline(Flags::DOUBLE_UNDERLINE),
            Some(Underline::Straight)
        );
        assert_eq!(
            underline(Flags::DOTTED_UNDERLINE),
            Some(Underline::Straight)
        );
        assert_eq!(
            underline(Flags::DASHED_UNDERLINE),
            Some(Underline::Straight)
        );
        assert_eq!(underline(Flags::UNDERCURL), Some(Underline::Wavy));
    }

    /// A real terminal of `columns` by `lines`, with `printed` printed into it.
    ///
    /// The rows above are cells written by hand, which is the only way to be
    /// sure a particular arrangement is the one under test. These are the other
    /// half of the same question: that what a real emulator does with real
    /// output is read back as what it wrote.
    fn printed(columns: usize, lines: usize, output: &str) -> Term<VoidListener> {
        let mut term = Term::new(
            Config::default(),
            &TermSize::new(columns, lines),
            VoidListener,
        );
        let mut parser = Processor::<StdSyncHandler>::new();
        parser.advance(&mut term, output.as_bytes());
        term
    }

    /// What a row's runs say, as `(column, columns, text)`.
    fn said(row: &Row) -> Vec<(usize, usize, &str)> {
        row.runs
            .iter()
            .map(|run| (run.column, run.columns, run.text.as_str()))
            .collect()
    }

    #[test]
    fn what_a_terminal_printed_is_read_back_in_the_columns_it_put_it_in() {
        let term = printed(20, 3, "a\u{4e2d}be\u{0301}");
        let screen = Screen::of(&term, WHITE, BLACK);

        assert_eq!(
            said(&screen.rows[0]),
            [
                (0, 1, "a"),
                (1, 2, "\u{4e2d}"),
                (3, 1, "b"),
                (4, 1, "e\u{0301}"),
            ]
        );
        assert!(screen.rows[1].runs.is_empty(), "nothing was printed there");
    }

    #[test]
    fn colours_a_terminal_was_asked_for_are_the_ones_read_back() {
        let term = printed(20, 3, "\u{1b}[31mred\u{1b}[0m plain");
        let screen = Screen::of(&term, WHITE, BLACK);
        let runs = &screen.rows[0].runs;

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "red");
        assert_eq!(runs[1].text, " plain");
        assert_ne!(runs[0].style.color, WHITE, "the colour it asked for");
        assert_eq!(runs[1].style.color, WHITE, "and the window's, after it");
    }

    #[test]
    fn attributes_a_terminal_was_asked_for_are_the_ones_read_back() {
        let term = printed(20, 3, "\u{1b}[1mb\u{1b}[0;4mu\u{1b}[0;7mi");
        let screen = Screen::of(&term, WHITE, BLACK);
        let runs = &screen.rows[0].runs;

        assert_eq!(runs.len(), 3);
        assert!(runs[0].style.bold);
        assert_eq!(runs[1].style.underline, Some(Underline::Straight));
        assert_eq!(
            (runs[2].style.color, screen.rows[0].fills[0].color),
            (BLACK, WHITE),
            "inverse video is the window's two colours the other way round"
        );
    }

    #[test]
    fn the_cursor_is_where_the_terminal_left_it() {
        let screen = Screen::of(&printed(20, 3, "abc"), WHITE, BLACK);
        let cursor = screen.cursor.expect("a terminal has a cursor");
        assert_eq!((cursor.line, cursor.column), (0, 3));
        assert_eq!(cursor.columns, 1);
        assert_eq!(cursor.shape, CursorShape::Block);
        assert_eq!(cursor.covered, None, "it is over a blank");
    }

    #[test]
    fn the_cursor_is_the_shape_the_terminal_asked_for() {
        let beam = Screen::of(&printed(20, 3, "\u{1b}[6 q"), WHITE, BLACK);
        assert_eq!(
            beam.cursor.expect("the cursor is still shown").shape,
            CursorShape::Beam
        );
        let underline = Screen::of(&printed(20, 3, "\u{1b}[4 q"), WHITE, BLACK);
        assert_eq!(
            underline.cursor.expect("the cursor is still shown").shape,
            CursorShape::Underline
        );
    }

    #[test]
    fn a_cursor_the_terminal_hid_is_not_drawn() {
        let screen = Screen::of(&printed(20, 3, "\u{1b}[?25l"), WHITE, BLACK);
        assert_eq!(screen.cursor, None);
    }

    #[test]
    fn a_cursor_on_a_wide_character_covers_both_its_columns_and_what_is_under_it() {
        let screen = Screen::of(&printed(20, 3, "\u{4e2d}\u{1b}[1;1H"), WHITE, BLACK);
        let cursor = screen.cursor.expect("a terminal has a cursor");
        assert_eq!(cursor.column, 0);
        assert_eq!(cursor.columns, 2);
        assert_eq!(
            cursor.covered,
            Some(("\u{4e2d}".to_owned(), BLACK)),
            "the character is drawn again over the cursor, in the colour behind it"
        );
    }

    #[test]
    fn a_cursor_over_a_blank_has_nothing_to_draw_again() {
        assert_eq!(covering(&Cell::default()), None);
        assert_eq!(
            covering(&Cell {
                c: 'x',
                ..Cell::default()
            }),
            Some("x".to_owned())
        );
    }
}
