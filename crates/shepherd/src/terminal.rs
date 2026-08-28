//! The shells, on screen.
//!
//! A window over one workspace: the tab whose shell has focus, every shell in
//! that tab drawn as a grid of cells in its own rectangle, and a line above them
//! saying which shell is being typed in and what the bus thinks is running in
//! it. There is no tab bar and there are no dividers — the arrangement is drawn
//! from the model's own layout and nothing more, because drawing it properly is
//! a piece of work of its own.
//!
//! # Drawn from the grid, not from the bytes
//!
//! Nothing here reads a terminal device. A shell parses what its process prints
//! on a thread of its own, into a grid it keeps; this asks that grid what it
//! currently says and turns the answer into a picture. The two are joined by a
//! number: the grid counts how many times it has changed, this remembers the
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
//! # Where focus is
//!
//! Every shell has a focus handle of the toolkit's, and each shell's rectangle
//! is where both kinds of input for that shell are answered: a key press with
//! an action bound to it, and a key press without one. Both arrive at the
//! handler belonging to whichever rectangle the toolkit decided has focus, so
//! there is no way for the shell being typed into and the shell an action acts
//! on to be different shells. The model is told where focus went, once, from
//! the toolkit's own notification — it is never asked, and nothing else writes
//! it.
//!
//! # What this does not do
//!
//! It shows the part of the buffer each shell is scrolled to and offers no way
//! to scroll it, it has no notion of selecting anything, and it does not take
//! the mouse.

#[cfg(test)]
mod tests;

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Pixels, Render, SharedString, Size, Styled, Subscription, Task, Window, canvas,
    div, px, relative,
};
use gpui_component::ActiveTheme;
use shepherd_core::{
    Closed, Direction, Layout, PlacedShell, Shell, ShellAddress, ShellId, ShellOptions, ShellSize,
    SplitTree, TabId, Workspace, WorkspaceId,
};
use tracing::{debug, info, warn};

use crate::frames::Frames;
use crate::grid::{Metrics, painting};
use crate::keymap::{
    self, Close, FocusDown, FocusLeft, FocusRight, FocusUp, NewTab, SplitDown, SplitRight,
};
use crate::keys;
use crate::live::{Live, badged, described};
use crate::screen::Screen;

/// How often the window looks at everything that changes underneath it.
///
/// The grids, what the shells are called, and the bus are all read on this one
/// timer, and a redraw is asked for only where one of them has actually moved.
/// Sixty times a second is a redraw for every frame a display of the usual kind
/// can show, and it is the ceiling on how often anything here is drawn: a
/// process printing faster than this is still read at full speed, it is simply
/// looked at sixty times a second like everything else.
const TICK: Duration = Duration::from_millis(16);

/// How big the text in a grid is.
const FONT_SIZE: Pixels = px(13.0);

/// What a tab opened from the keyboard is called.
const TAB: &str = "shell";

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

/// One shell as the window holds it.
struct Held {
    shell: Shell,
    /// What the toolkit means by "this shell has focus". Keystrokes for this
    /// shell and actions taken on it both arrive by way of it.
    focus: FocusHandle,
    /// The grid's revision as of the last frame this shell was drawn in.
    drawn: u64,
    /// The standing arrangement by which the toolkit says focus arrived here.
    /// Held so that it ends when the shell does.
    _focusing: Subscription,
}

/// The window's view: one workspace, and every shell open in it.
pub struct TerminalView {
    /// The model the bus's sessions are placed against, and the shape a tab
    /// bar and a sidebar would eventually be drawn from.
    layout: Layout,
    /// The workspace this window is open on. There is one.
    workspace: WorkspaceId,
    /// How a shell opened from the keyboard is started.
    options: ShellOptions,
    shells: Vec<Held>,
    /// The tab on screen.
    ///
    /// Which tab is drawn cannot be read off where focus is, however much it
    /// looks like it could: the toolkit only puts focus in something it has
    /// drawn, so a tab that showed itself because focus arrived in it would be
    /// a tab focus could never arrive in. So this is the view's own, the tab
    /// bar's eventual state, and moving focus into a tab that is not this one
    /// means setting it first.
    active: TabId,
    /// Which shell the toolkit last said has focus. Written in one place only,
    /// from the toolkit's own notification. See [`TerminalView::took_focus`].
    focused: ShellId,
    live: Live,
    family: SharedString,
    metrics: Metrics,
    frames: Frames,
    /// The timer that looks at all of it. Held because dropping it stops the
    /// looking.
    _ticking: Task<()>,
}

impl TerminalView {
    /// Shows `first`, which is the shell `layout` was opened with, and starts
    /// every shell opened after it with `options`.
    pub fn new(
        first: Shell,
        layout: Layout,
        options: ShellOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let address = first.address();
        let active = layout
            .workspace(address.workspace)
            .and_then(|workspace| workspace.tab_of(address.shell))
            .expect("the tab holding the shell this window was opened on");
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

        let mut view = Self {
            layout,
            workspace: address.workspace,
            options,
            shells: Vec::new(),
            active,
            focused: address.shell,
            live: Live::new(),
            family,
            metrics,
            frames: Frames::new(),
            _ticking: ticking,
        };
        view.hold(first, window, cx);
        view.focus(address.shell, window);
        view
    }

    /// Takes charge of a shell that has been started: somewhere to draw it,
    /// the focus that says keystrokes are meant for it, and the standing
    /// arrangement by which the toolkit says focus arrived there.
    fn hold(&mut self, shell: Shell, window: &mut Window, cx: &mut Context<Self>) {
        let id = shell.address().shell;
        let focus = cx.focus_handle();
        let focusing = cx.on_focus_in(&focus, window, move |view, _, cx| view.took_focus(id, cx));
        self.shells.push(Held {
            shell,
            focus,
            drawn: 0,
            _focusing: focusing,
        });
    }

    /// Remembers that the toolkit put focus in `shell`.
    ///
    /// This is the only place either this view or the model is told where focus
    /// is, and both are told rather than asked: the toolkit decides, and what
    /// is written here follows whatever it decided, whether focus moved because
    /// an action asked for it or because something else entirely did. A tab's
    /// own record of which of its shells was last typed in is what gets saved
    /// between runs, so it has to be kept — but it is a copy of the toolkit's
    /// answer, not a second opinion.
    fn took_focus(&mut self, shell: ShellId, cx: &mut Context<Self>) {
        self.focused = shell;
        if let Some(tab) = self.open().tab_of(shell) {
            self.active = tab;
            if let Some(open) = self.open_mut().tab_mut(tab) {
                open.focus(shell);
            }
        }
        cx.notify();
    }

    /// Asks the toolkit to put focus in `shell`.
    ///
    /// Nothing is recorded here. The arrangement made when the shell was first
    /// held is what records it, once the toolkit has actually done it.
    fn focus(&self, shell: ShellId, window: &mut Window) {
        if let Some(held) = self.held(shell) {
            window.focus(&held.focus);
        }
    }

    /// Opens a tab with one shell in it, and focuses that shell.
    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.open_mut().open_tab(TAB);
        let Some(shell) = self.open().tab(tab).map(|tab| tab.focused()) else {
            return;
        };
        self.started(tab, shell, window, cx);
    }

    /// Puts a new shell beside `from` on the given side, and focuses it.
    fn split(
        &mut self,
        from: ShellId,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.open().tab_of(from) else {
            return;
        };
        let Some(fresh) = self.open_mut().split(tab, from, direction) else {
            return;
        };
        self.started(tab, fresh, window, cx);
    }

    /// Starts the process for a shell the model has just made room for.
    ///
    /// A shell that will not start leaves the model as it found it. A slot with
    /// no process in it draws as a rectangle nothing can ever appear in, and
    /// there is nothing to be done with one but take it away again — so the
    /// failure is said out loud and the arrangement goes back to what it was.
    fn started(&mut self, tab: TabId, shell: ShellId, window: &mut Window, cx: &mut Context<Self>) {
        let address = ShellAddress::new(self.workspace, shell);
        match Shell::spawn(address, &self.options) {
            Ok(started) => {
                info!(correlation = started.correlation(), "started a shell");
                self.hold(started, window, cx);
                // Before focus is asked for, because a shell in a tab that is
                // not the one on screen is not drawn, and the toolkit will not
                // put focus in something it has not drawn.
                self.active = tab;
                self.focus(shell, window);
            }
            Err(error) => {
                warn!(%error, "cannot start a shell");
                self.open_mut().close_shell(tab, shell);
            }
        }
        cx.notify();
    }

    /// Closes `shell`, and the tab it was in if it was the last one there.
    ///
    /// Closing the last shell of the last tab leaves nothing to show, so the
    /// window goes with it — which is also how this application is quit.
    fn close(&mut self, shell: ShellId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.open().tab_of(shell) else {
            return;
        };
        let was = self.open().tabs().iter().position(|open| open.id() == tab);
        let closed = self.open_mut().close_shell(tab, shell);
        // Dropping it is what ends the process and waits for it.
        self.shells
            .retain(|held| held.shell.address().shell != shell);

        match closed {
            Closed::Removed { successor } => self.focus(successor, window),
            Closed::Emptied => {
                // The tab that was there is gone, so what shows is whatever
                // took its place — or the last tab, where what closed was the
                // right-hand one.
                let next = {
                    let tabs = self.open().tabs();
                    was.filter(|_| !tabs.is_empty())
                        .map(|was| was.min(tabs.len() - 1))
                        .map(|next| (tabs[next].id(), tabs[next].focused()))
                };
                match next {
                    Some((tab, shell)) => {
                        self.active = tab;
                        self.focus(shell, window);
                    }
                    None => window.remove_window(),
                }
            }
            Closed::NotFound => {}
        }
        cx.notify();
    }

    /// Moves focus one shell in `direction` from `from`, if there is one there.
    fn moved(&mut self, from: ShellId, direction: Direction, window: &mut Window) {
        let Some(tab) = self.open().tab_of(from) else {
            return;
        };
        let Some(neighbour) = self
            .open()
            .tab(tab)
            .and_then(|tab| tab.tree().neighbour(from, direction))
        else {
            return;
        };
        self.focus(neighbour, window);
    }

    /// A key press with no action bound to it, while `shell` has focus.
    ///
    /// The shell is the one whose rectangle the toolkit delivered the press to,
    /// which is the same rectangle an action would have been delivered to.
    fn typed(&mut self, shell: ShellId, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(shell) else {
            return;
        };
        // The lock the shell's reading thread needs, held for one copy of a
        // handful of flags: what a key is worth depends on the modes the
        // program has turned on.
        let mode = *self.shells[index].shell.term().lock().mode();
        let Some(bytes) = keys::encoded(&event.keystroke, mode) else {
            return;
        };
        self.shells[index].shell.write(bytes);
        // Said to be handled, so that the platform stops looking for somewhere
        // else to put a key that has already been typed.
        cx.stop_propagation();
    }

    /// Looks at everything that changes underneath the window, and asks for a
    /// redraw where something has.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.live.poll(self.layout.workspaces());
        for held in &mut self.shells {
            changed |= held.shell.poll_name();
        }
        changed |= self.printed();
        if changed {
            cx.notify();
        }
    }

    /// Whether anything on screen has printed since the frame it was last drawn
    /// in.
    ///
    /// Only what is on screen. A shell in a tab nobody is looking at is still
    /// being read and will be drawn as it then is when its tab comes back, but
    /// nothing it prints in the meantime is a reason to draw a frame.
    fn printed(&self) -> bool {
        let showing = self.showing();
        self.shells
            .iter()
            .filter(|held| showing.contains(&held.shell.address().shell))
            .any(|held| held.shell.revision() != held.drawn)
    }

    /// Takes in how much room a shell's grid was given, and tells the shell.
    ///
    /// The process on the far side is told too, which is what makes a
    /// full-screen program redraw itself at the new size. Nothing happens where
    /// the size works out the same, so a window being dragged one pixel at a
    /// time resizes a shell only when a whole row or column appears.
    fn fitted(&mut self, shell: ShellId, room: Size<Pixels>, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(shell) else {
            return;
        };
        let fitted = ShellSize::new(
            fits(room.width, self.metrics.cell),
            fits(room.height, self.metrics.line_height),
        )
        .with_cell(pixels(self.metrics.cell), pixels(self.metrics.line_height));
        let held = &mut self.shells[index];
        if fitted != held.shell.size() {
            debug!(
                %shell,
                columns = fitted.columns(),
                lines = fitted.lines(),
                "a grid was given room for a different number of cells"
            );
            held.shell.resize(fitted);
            cx.notify();
        }
    }

    /// What one shell's screen currently says.
    ///
    /// The terminal's lock is held for exactly this, and the window's own
    /// colours go in because the two ends of a terminal's palette — the default
    /// foreground and the default background — are the window's rather than the
    /// process's.
    fn screen(&self, index: usize, cx: &App) -> Screen {
        let foreground = cx.theme().foreground;
        let background = cx.theme().background;
        let term = self.shells[index].shell.term().lock();
        Screen::of(&term, foreground, background)
    }

    /// The line above the shells: which one is being typed in, what is running
    /// in it, where it sits, and how fast all of it is being drawn.
    fn header(&self, cx: &App) -> impl IntoElement {
        let elsewhere = self.live.elsewhere();
        let focused = self.held(self.focused);
        let state = focused.map(|held| held.shell.state());
        let said = [
            focused.map(|held| held.shell.correlation().to_owned()),
            focused.and_then(|held| held.shell.name().map(ToOwned::to_owned)),
            state
                .filter(|state| !state.is_running())
                .map(|state| match state.code() {
                    Some(code) => format!("exited with status {code}"),
                    None => "exited".to_owned(),
                }),
            Some(self.placement()),
            Some(described(self.live.presence())),
            Some(badged(
                self.live
                    .status_at(ShellAddress::new(self.workspace, self.focused)),
            )),
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

    /// Where in the arrangement the shell being typed in sits.
    ///
    /// Which is what stands in for a tab bar: with no chrome drawn around the
    /// grids, this line is how a tab opening or a shell closing is seen to have
    /// happened at all.
    fn placement(&self) -> String {
        let tabs = self.open().tabs();
        let tab = tabs
            .iter()
            .position(|tab| tab.id() == self.active)
            .map_or(0, |index| index + 1);
        let showing = self.showing();
        let shell = showing
            .iter()
            .position(|shell| *shell == self.focused)
            .map_or(0, |index| index + 1);
        format!(
            "tab {tab} of {}, shell {shell} of {}",
            tabs.len(),
            showing.len()
        )
    }

    /// The shells of the tab on screen, each in its own rectangle.
    ///
    /// The rectangles are the model's own layout of that tab's arrangement in a
    /// unit square, put on screen as fractions of the room the window has. That
    /// is the whole of the arrangement: no dividers, nothing to drag, and no
    /// tab bar above it. This is the least that makes more than one shell
    /// visible, and it is meant to be replaced by something that draws the
    /// arrangement properly.
    fn arranged(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let placed = self
            .tree()
            .map(SplitTree::layout)
            .unwrap_or_default()
            .into_iter()
            .map(|placed| self.framed(placed, window, cx))
            .collect::<Vec<_>>();
        div().relative().flex_1().overflow_hidden().children(placed)
    }

    /// One shell, in the rectangle the arrangement gives it.
    fn framed(
        &mut self,
        placed: PlacedShell,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(index) = self.index_of(placed.shell) else {
            // A slot the model has and no process for. Nothing starts one from
            // here — whatever should have is elsewhere — so what is drawn is
            // the empty rectangle, rather than a rectangle that is not there.
            return div().into_any_element();
        };
        let shell = placed.shell;
        let focus = self.shells[index].focus.clone();
        let focused = focus.is_focused(window);
        let metrics = self.metrics;
        let family = self.family.clone();
        let screen = self.screen(index, cx);
        let measuring = cx.entity();
        self.shells[index].drawn = self.shells[index].shell.revision();

        div()
            .absolute()
            .left(relative(placed.bounds.x))
            .top(relative(placed.bounds.y))
            .w(relative(placed.bounds.width))
            .h(relative(placed.bounds.height))
            .key_context(keymap::CONTEXT)
            .track_focus(&focus)
            .on_key_down(cx.listener(move |view, event: &KeyDownEvent, _, cx| {
                view.typed(shell, event, cx);
            }))
            .on_action(cx.listener(move |view, _: &NewTab, window, cx| view.new_tab(window, cx)))
            .on_action(cx.listener(move |view, _: &SplitRight, window, cx| {
                view.split(shell, Direction::Right, window, cx);
            }))
            .on_action(cx.listener(move |view, _: &SplitDown, window, cx| {
                view.split(shell, Direction::Down, window, cx);
            }))
            .on_action(cx.listener(move |view, _: &Close, window, cx| {
                view.close(shell, window, cx);
            }))
            .on_action(cx.listener(move |view, _: &FocusLeft, window, _| {
                view.moved(shell, Direction::Left, window);
            }))
            .on_action(cx.listener(move |view, _: &FocusRight, window, _| {
                view.moved(shell, Direction::Right, window);
            }))
            .on_action(cx.listener(move |view, _: &FocusUp, window, _| {
                view.moved(shell, Direction::Up, window);
            }))
            .on_action(cx.listener(move |view, _: &FocusDown, window, _| {
                view.moved(shell, Direction::Down, window);
            }))
            .border_1()
            .border_color(if focused {
                cx.theme().ring
            } else {
                cx.theme().border
            })
            .overflow_hidden()
            .px_2()
            .py_1()
            .child(
                canvas(
                    move |bounds, window, cx| {
                        // How many rows and columns a shell gets is the room
                        // this element was actually given, rather than the
                        // window's size less a guess at everything drawn around
                        // it.
                        measuring.update(cx, |view, cx| view.fitted(shell, bounds.size, cx));
                        painting(screen, metrics, &family, bounds, window)
                    },
                    |_, painting, window, cx| painting.paint(window, cx),
                )
                .size_full(),
            )
            .into_any_element()
    }

    /// The one workspace this window is open on.
    fn open(&self) -> &Workspace {
        self.layout
            .workspace(self.workspace)
            .expect("the workspace this window was opened on")
    }

    /// The same, to change.
    fn open_mut(&mut self) -> &mut Workspace {
        let workspace = self.workspace;
        self.layout
            .workspace_mut(workspace)
            .expect("the workspace this window was opened on")
    }

    /// The arrangement on screen: the one belonging to the tab that is showing.
    fn tree(&self) -> Option<&SplitTree> {
        self.open().tab(self.active).map(shepherd_core::Tab::tree)
    }

    /// Every shell on screen, in the order the arrangement puts them.
    fn showing(&self) -> Vec<ShellId> {
        self.tree().map(SplitTree::shells).unwrap_or_default()
    }

    /// One shell this window is holding.
    fn held(&self, shell: ShellId) -> Option<&Held> {
        self.index_of(shell).map(|index| &self.shells[index])
    }

    /// Where in the list one shell is.
    fn index_of(&self, shell: ShellId) -> Option<usize> {
        self.shells
            .iter()
            .position(|held| held.shell.address().shell == shell)
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

        let screen = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.header(cx))
            .child(self.arranged(window, cx));
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
