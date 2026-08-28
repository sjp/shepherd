//! One shell, on screen.
//!
//! A window with a single terminal in it: the grid a shell has printed, drawn
//! as a monospaced grid of cells, above a line saying which shell this is and
//! what the bus thinks is running in it. There is one of these and there is no
//! way to make another — tabs, splits and a sidebar are the arrangement around
//! this, and none of it exists yet.
//!
//! # Drawn from the grid, not from the bytes
//!
//! Nothing here reads the terminal device. A shell parses what its process
//! prints on a thread of its own, into a grid it keeps; this asks that grid what
//! it currently says and turns the answer into a picture. The two are joined by
//! a number: the grid counts how many times it has changed, this remembers the
//! number it last drew, and a redraw happens when they differ. So a process
//! printing a million lines a second costs one redraw per look rather than a
//! million, and a still screen costs nothing at all.
//!
//! # Three steps, in that order, for a reason
//!
//! Reading the grid, working out the picture and drawing it are three separate
//! things here, and the separation is what keeps a shell producing output at
//! full speed while it is being watched. The grid is behind a lock the shell's
//! reading thread needs; so it is read once, quickly, into an owned description
//! of the screen, and the lock is gone before a single glyph has been shaped.
//!
//! # What this does not do
//!
//! It does not take input. A keyboard has to be turned into bytes through a
//! keymap, and until there is one, what a shell runs is decided when it is
//! started. It shows the part of the buffer the shell is scrolled to and offers
//! no way to scroll it, and it has no notion of selecting anything.

use std::time::{Duration, Instant};

use gpui::{
    App, Context, IntoElement, ParentElement, Pixels, Render, SharedString, Size, Styled, Task,
    Window, canvas, div, px,
};
use gpui_component::ActiveTheme;
use shepherd_core::{Layout, Shell, ShellAddress, ShellSize};
use tracing::{debug, warn};

use crate::frames::Frames;
use crate::grid::{Metrics, painting};
use crate::live::{Live, badged, described};
use crate::screen::Screen;

/// How often the window looks at everything that changes underneath it.
///
/// The grid, what the shell is called, and the bus are all read on this one
/// timer, and a redraw is asked for only where one of them has actually moved.
/// Sixty times a second is a redraw for every frame a display of the usual kind
/// can show, and it is the ceiling on how often anything here is drawn: a
/// process printing faster than this is still read at full speed, it is simply
/// looked at sixty times a second like everything else.
const TICK: Duration = Duration::from_millis(16);

/// How big the text in the grid is.
const FONT_SIZE: Pixels = px(13.0);

/// The families a monospaced font is looked for under, in order.
///
/// A terminal in a proportional font is not a terminal, and which family is
/// installed is a fact about the machine rather than a choice this can make. So
/// the list is asked of the machine and the first one it has is used.
const MONOSPACE: [&str; 7] = [
    "SF Mono",
    "Menlo",
    "Monaco",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Consolas",
    "monospace",
];

/// What is asked for when the machine listed none of them.
///
/// The last of the list above, asked for anyway: on a machine whose font
/// configuration understands it, it is a request for whatever that machine calls
/// its monospaced font rather than the name of a family, so it can be answerable
/// without being listed. Where it is not, the toolkit falls back to a font that
/// will not line up — which is what the warning beside this is for.
const FALLBACK: &str = "monospace";

/// The window's one view: a shell, and what is known about what is running in it.
pub struct TerminalView {
    shell: Shell,
    /// The model the bus's sessions are placed against. One workspace holding
    /// one tab holding one shell — the smallest thing attribution can be asked
    /// about, and the same shape a window full of them would ask with.
    layout: Layout,
    live: Live,
    address: ShellAddress,
    family: SharedString,
    metrics: Metrics,
    frames: Frames,
    /// The grid's revision as of the last frame drawn.
    drawn: u64,
    /// The timer that looks at all of it. Held because dropping it stops the
    /// looking.
    _ticking: Task<()>,
}

impl TerminalView {
    /// Shows `shell`, which is the one shell `layout` holds.
    pub fn new(shell: Shell, layout: Layout, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address = shell.address();
        let family = monospace(window);
        let metrics = Metrics::of(&family, FONT_SIZE, window);
        let ticking = cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                // The view going away is how this stops: there is nothing left
                // to look at and nothing left to draw.
                if view.update(cx, |view, cx| view.tick(cx)).is_err() {
                    return;
                }
            }
        });

        Self {
            shell,
            layout,
            live: Live::watching(address),
            address,
            family,
            metrics,
            frames: Frames::new(),
            drawn: 0,
            _ticking: ticking,
        }
    }

    /// Looks at everything that changes underneath the window, and asks for a
    /// redraw where something has.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.live.poll(self.layout.workspaces());
        changed |= self.shell.poll_name();
        changed |= self.shell.revision() != self.drawn;
        if changed {
            cx.notify();
        }
    }

    /// Takes in how much room the grid was given, and tells the shell.
    ///
    /// The process on the far side is told too, which is what makes a
    /// full-screen program redraw itself at the new size. Nothing happens where
    /// the size works out the same, so a window being dragged one pixel at a
    /// time resizes the shell only when a whole row or column appears.
    fn fitted(&mut self, room: Size<Pixels>, cx: &mut Context<Self>) {
        let fitted = ShellSize::new(
            fits(room.width, self.metrics.cell),
            fits(room.height, self.metrics.line_height),
        )
        .with_cell(pixels(self.metrics.cell), pixels(self.metrics.line_height));
        if fitted != self.shell.size() {
            debug!(
                columns = fitted.columns(),
                lines = fitted.lines(),
                "the grid was given room for a different number of cells"
            );
            self.shell.resize(fitted);
            cx.notify();
        }
    }

    /// What the shell's screen currently says.
    ///
    /// The terminal's lock is held for exactly this, and the window's own
    /// colours go in because the two ends of a terminal's palette — the default
    /// foreground and the default background — are the window's rather than the
    /// process's.
    fn screen(&self, cx: &App) -> Screen {
        let foreground = cx.theme().foreground;
        let background = cx.theme().background;
        let term = self.shell.term().lock();
        Screen::of(&term, foreground, background)
    }

    /// The line above the grid: which shell this is, what is running in it, and
    /// how fast it is being drawn.
    fn header(&self, cx: &App) -> impl IntoElement {
        let elsewhere = self.live.elsewhere();
        let state = self.shell.state();
        let said = [
            Some(self.shell.correlation().to_owned()),
            self.shell.name().map(ToOwned::to_owned),
            (!state.is_running()).then(|| match state.code() {
                Some(code) => format!("exited with status {code}"),
                None => "exited".to_owned(),
            }),
            Some(described(self.live.presence())),
            Some(badged(self.live.status_at(self.address))),
            (elsewhere > 0).then(|| format!("{elsewhere} elsewhere")),
            self.frames.last().map(|report| report.to_string()),
        ];

        div()
            .flex()
            .flex_row()
            .gap_4()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .text_size(px(12.0))
            .text_color(cx.theme().muted_foreground)
            .children(said.into_iter().flatten().map(SharedString::from))
    }

    /// The grid itself.
    fn grid(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let metrics = self.metrics;
        let family = self.family.clone();
        let screen = self.screen(cx);
        let measuring = cx.entity();

        div().flex_1().overflow_hidden().px_2().py_1().child(
            canvas(
                move |bounds, window, cx| {
                    // How many rows and columns the shell gets is the room this
                    // element was actually given, rather than the window's size
                    // less a guess at everything drawn around it.
                    measuring.update(cx, |view, cx| view.fitted(bounds.size, cx));
                    painting(screen, metrics, &family, bounds, window)
                },
                |_, painting, window, cx| painting.paint(window, cx),
            )
            .size_full(),
        )
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let began = Instant::now();
        // After the frame this is the beginning of, so that what is timed is a
        // whole frame rather than the part of it spent in here.
        cx.on_next_frame(window, move |view, _, _| {
            view.frames.drew(began, Instant::now());
        });
        self.drawn = self.shell.revision();

        let screen = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.header(cx))
            .child(self.grid(cx));
        self.frames.built(began.elapsed());
        screen
    }
}

/// The first monospaced family this machine has, of the ones worth asking for.
fn monospace(window: &Window) -> SharedString {
    let installed = window.text_system().all_font_names();
    for family in MONOSPACE {
        if installed.iter().any(|name| name == family) {
            return SharedString::from(family);
        }
    }
    warn!("this machine has none of the monospaced fonts asked for; the grid will not line up");
    SharedString::from(FALLBACK)
}

/// How many whole cells of `cell` fit in `room`.
fn fits(room: Pixels, cell: Pixels) -> u16 {
    if cell <= px(0.0) {
        return 0;
    }
    let fits = (room / cell).floor();
    if fits <= 0.0 {
        0
    } else if fits >= f32::from(u16::MAX) {
        u16::MAX
    } else {
        // Whole, non-negative and below the maximum, all three just checked.
        fits as u16
    }
}

/// A length in whole pixels, for telling a process how big its cells are.
fn pixels(length: Pixels) -> u16 {
    let whole = f32::from(length.round()).max(1.0);
    if whole >= f32::from(u16::MAX) {
        u16::MAX
    } else {
        whole as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grid_is_as_many_whole_cells_as_fit() {
        assert_eq!(fits(px(800.0), px(8.0)), 100);
        assert_eq!(fits(px(805.0), px(8.0)), 100, "a partial column is not one");
    }

    #[test]
    fn a_grid_with_no_room_has_no_cells_in_it() {
        assert_eq!(fits(px(4.0), px(8.0)), 0);
        assert_eq!(fits(px(-10.0), px(8.0)), 0);
        assert_eq!(
            fits(px(800.0), px(0.0)),
            0,
            "a cell of no width fits nowhere"
        );
    }

    #[test]
    fn a_cell_is_told_to_a_process_in_whole_pixels() {
        assert_eq!(pixels(px(8.4)), 8);
        assert_eq!(pixels(px(8.6)), 9);
        assert_eq!(
            pixels(px(0.2)),
            1,
            "no process is told its cells are nothing"
        );
    }
}
