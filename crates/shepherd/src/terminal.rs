//! The shells, on screen.
//!
//! A window over one workspace: a bar of the tabs open in it, the arrangement
//! of shells belonging to whichever of them is showing, each drawn as a grid of
//! cells in its own rectangle with a divider on every edge two of them share,
//! and a line above all of it saying which shell is being typed in and what the
//! bus thinks is running in it.
//!
//! # The arrangement is the model's, at the window's scale
//!
//! Where a shell sits and where a divider sits are both asked of the tab's own
//! arrangement, laid out in the rectangle this window gave it. Dragging a
//! divider tells that arrangement where its edge is now, and the next frame
//! asks again — so there is no second copy of where the edges are for the two
//! to disagree about, and the shares a drag leaves behind are the ones that get
//! saved.
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
//! # What the mouse does
//!
//! Two things. Pressing in a shell puts focus there, which is the toolkit's own
//! doing rather than anything arranged here: an element that tracks a focus
//! handle takes focus when it is pressed, and the notification that follows is
//! the same one an action moving focus produces. Pressing on a divider takes
//! hold of it instead, and suppresses that — dragging an edge is not a way of
//! choosing which shell to type in.
//!
//! # What this does not do
//!
//! It shows the part of the buffer each shell is scrolled to and offers no way
//! to scroll it, and it has no notion of selecting anything.

#[cfg(test)]
mod tests;

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, CursorStyle, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Render, SharedString, Size, StatefulInteractiveElement as _, Styled,
    Subscription, Task, Window, canvas, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_component::ActiveTheme;
use gpui_component::tab::{Tab as TabButton, TabBar};
use shepherd_core::{
    Axis, Branches, Closed, Direction, Divider, Layout, PlacedDivider, PlacedShell, Rect, Shell,
    ShellAddress, ShellId, ShellOptions, ShellSize, SplitTree, TabId, Workspace, WorkspaceId,
};
use tracing::{debug, info, warn};

use crate::frames::Frames;
use crate::grid::{Metrics, painting};
use crate::keymap::{
    self, Close, FocusDown, FocusLeft, FocusRight, FocusUp, NewTab, NextTab, PreviousTab,
    SplitDown, SplitRight,
};
use crate::keys;
use crate::live::{Live, described};
use crate::screen::Screen;
use crate::sidebar::{Folded, Picked, Showing, Sidebar};

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

/// How wide a divider is to take hold of.
///
/// Wider than the line it sits on, and centred over it, because an edge is a
/// thing a person aims a pointer at rather than a thing they hit exactly. The
/// edge itself is what the shells either side of it draw, so nothing is covered
/// up by this being generous: it overlaps the padding around two grids and
/// nothing that has been printed.
const GRIP: Pixels = px(8.0);

/// What closes a tab, in the corner of the tab.
const CLOSE: &str = "\u{00d7}";

/// What opens another one, at the end of the bar.
const NEW: &str = "+";

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
    /// The model the bus's sessions are placed against, and the tab bar and
    /// the sidebar are both drawn from.
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
    /// Where the arrangement of shells is in the window, as of the last frame.
    ///
    /// The tab bar and the line above it take what room they take, so this is
    /// measured rather than worked out: it is what turns the place a pointer is
    /// into a place in the arrangement.
    area: Bounds<Pixels>,
    /// The divider being dragged, while one is.
    dragging: Option<Divider>,
    live: Live,
    /// What each workspace's folder is checked out on, read when the workspace
    /// is looked at rather than while a frame is being drawn.
    branches: Branches,
    /// What has been folded away in the sidebar.
    folded: Folded,
    family: SharedString,
    metrics: Metrics,
    frames: Frames,
    /// The timer that looks at all of it. Held because dropping it stops the
    /// looking.
    _ticking: Task<()>,
    /// The standing arrangement by which the toolkit says this application is
    /// quitting. Held because letting go of it would cancel it.
    _quitting: Subscription,
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
        // Read once, here, because this window is open on one workspace and
        // this is the moment it becomes the one being looked at.
        let mut branches = Branches::new();
        if let Some(workspace) = layout.workspace(address.workspace) {
            branches.focused(workspace);
        }
        // Quitting is where the bus this window started is put down: a daemon
        // that exists because this wanted one has no reason to outlive the
        // application that wanted it, and once this process is gone there is
        // nothing left that can stop it. The toolkit calls this whichever way
        // the quit was asked for, and calls it while there is still a window;
        // the way out that closes the last window instead takes this view with
        // it, and what covers that is the bus being stopped as it is dropped.
        let quitting = cx.on_app_quit(|view: &mut Self, _| {
            view.live.stop();
            // Nothing to wait for: the stop has already waited, on this thread,
            // for as long as it is prepared to. The toolkit gives what is
            // returned here far less time than a daemon may need to shut down
            // in good order, so what it gets is a future that is already done.
            async {}
        });
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
            area: Bounds::default(),
            dragging: None,
            live: Live::new(),
            branches,
            folded: Folded::default(),
            family,
            metrics,
            frames: Frames::new(),
            _ticking: ticking,
            _quitting: quitting,
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

    /// Shows the tab at `index` in the bar.
    fn show_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.open().tabs().get(index).map(shepherd_core::Tab::id);
        if let Some(tab) = tab {
            self.show(tab, window, cx);
        }
    }

    /// Shows the tab one along from the one on screen, or one back.
    ///
    /// This comes back round at either end, which moving focus between shells
    /// deliberately does not. Tabs are a list somebody steps along and the ends
    /// of it are a few keystrokes apart; an arrangement of shells is a picture,
    /// and leaving the right-hand one by pressing right would be a picture that
    /// moved.
    fn step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let next = {
            let tabs = self.open().tabs();
            let at = tabs.iter().position(|tab| tab.id() == self.active);
            at.map(|at| {
                let step = if forward { 1 } else { tabs.len() - 1 };
                tabs[(at + step) % tabs.len()].id()
            })
        };
        if let Some(tab) = next {
            self.show(tab, window, cx);
        }
    }

    /// Shows `tab`, and puts focus back in whichever of its shells was last
    /// being typed in.
    ///
    /// Which tab is showing is set before focus is asked for, because the
    /// toolkit will not put focus in something it has not drawn — and it is set
    /// again, from the toolkit's own notification, once it has.
    fn show(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(shell) = self.open().tab(tab).map(shepherd_core::Tab::focused) else {
            return;
        };
        self.active = tab;
        self.focus(shell, window);
        cx.notify();
    }

    /// Closes a tab and every shell in it.
    ///
    /// A tab nobody is looking at is taken away without focus being disturbed:
    /// the shell being typed in is in another tab, and closing something out of
    /// sight must not move the cursor out of it. The tab on screen goes shell by
    /// shell instead, so that what is left — the next tab along, or nothing and
    /// the window with it — is settled by the same code that answers closing a
    /// shell from the keyboard.
    fn close_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let closing = self
            .open()
            .tab(tab)
            .map(shepherd_core::Tab::shells)
            .unwrap_or_default();
        if tab == self.active {
            for shell in closing {
                self.close(shell, window, cx);
            }
            return;
        }
        self.open_mut().close_tab(tab);
        // Dropping them is what ends the processes and waits for them.
        self.shells
            .retain(|held| !closing.contains(&held.shell.address().shell));
        cx.notify();
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

    /// Takes hold of a divider, which is what dragging one begins as.
    fn take(&mut self, divider: Divider, window: &mut Window, cx: &mut Context<Self>) {
        self.dragging = Some(divider);
        // Both of these are about what a press on a divider is not. It is not a
        // press on the shell underneath it, so the toolkit is told not to move
        // focus there; and it is not a press on the window either, so nothing
        // further is offered it.
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    /// The pointer has moved while a divider is being held.
    ///
    /// Where the divider now is gets told to the arrangement, and the next
    /// frame lays the shells out from the arrangement as it then is. Nothing
    /// here remembers where the edge was: the model is the only place it is
    /// written down, and it is the place the saved file is written from.
    fn dragged(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(divider) = self.dragging.clone() else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            // Let go of somewhere this window was never told about, which is
            // what happens when a drag ends outside it. The button coming back
            // up is the news, however late it arrives.
            self.dropped(cx);
            return;
        }
        let dividers = self
            .tree()
            .map(|tree| tree.dividers_in(area(self.area)))
            .unwrap_or_default();
        let Some(placed) = dividers
            .into_iter()
            .find(|placed| placed.divider == divider)
        else {
            // The arrangement changed underneath the drag, so there is no
            // longer an edge being held.
            self.dragging = None;
            return;
        };
        let split = placed.within.extent(placed.axis);
        if split <= 0.0 {
            return;
        }
        let along = match placed.axis {
            Axis::Horizontal => f32::from(event.position.x),
            Axis::Vertical => f32::from(event.position.y),
        };
        let position = (along - placed.within.start(placed.axis)) / split;
        let tab = self.active;
        if self.open_mut().resize(tab, &divider, position) {
            cx.notify();
        }
    }

    /// The pointer has been let go of, wherever it happens to be.
    fn dropped(&mut self, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() {
            cx.notify();
        }
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

    /// Everything open and everything running in it, down the left.
    ///
    /// Worked out afresh each frame from the model, the bus and the folders,
    /// because every one of those is somebody else's to change and a copy kept
    /// here would be a copy to keep in step.
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.shown().drawn(cx)
    }

    /// What the sidebar would say, without a window to say it in.
    fn shown(&self) -> Sidebar {
        Sidebar::of(
            &self.layout,
            self.live.attribution(),
            &self.branches,
            &self.folded,
            Showing {
                workspace: self.workspace,
                tab: self.active,
                shell: self.focused,
            },
            |address| {
                self.held(address.shell)
                    .filter(|held| held.shell.address() == address)
                    .and_then(|held| held.shell.name())
                    .map(ToOwned::to_owned)
            },
        )
    }

    /// Answers a press on a row of the sidebar.
    ///
    /// One place for all of them, so that a shell reached from the tree and the
    /// same shell reached from the list of agents cannot come to mean two
    /// different things.
    pub(crate) fn picked(&mut self, picked: Picked, window: &mut Window, cx: &mut Context<Self>) {
        match picked {
            // A shell in a workspace this window is not open on has nowhere
            // here to be shown. There is one workspace, and the sidebar draws
            // the model rather than only the part of it this window can reach.
            Picked::Shell(address) if address.workspace != self.workspace => {}
            Picked::Shell(address) => {
                // Before focus is asked for, because the toolkit will not put
                // focus in something it has not drawn.
                if let Some(tab) = self.open().tab_of(address.shell) {
                    self.active = tab;
                }
                self.focus(address.shell, window);
                cx.notify();
            }
            Picked::Tab(workspace, tab) if workspace == self.workspace => {
                self.show(tab, window, cx);
            }
            Picked::Tab(..) => {}
            Picked::FoldWorkspace(workspace) => {
                self.folded.fold_workspace(workspace);
                cx.notify();
            }
            Picked::FoldTab(workspace, tab) => {
                self.folded.fold_tab(workspace, tab);
                cx.notify();
            }
        }
    }

    /// What the tab bar says: the tabs open in the workspace, in the order they
    /// are drawn, and which of them is the one on screen.
    ///
    /// Kept apart from the element it becomes because it is the whole of what
    /// the bar shows and none of how it looks, and so can be asked without a
    /// window being open to ask it of.
    fn bar(&self) -> (Vec<(TabId, SharedString)>, usize) {
        let open = self.open();
        let showing = open
            .tabs()
            .iter()
            .position(|tab| tab.id() == self.active)
            .unwrap_or(0);
        let tabs = open
            .tabs()
            .iter()
            .map(|tab| (tab.id(), SharedString::from(tab.name().to_owned())))
            .collect();
        (tabs, showing)
    }

    /// The tab bar: every tab open in the workspace, the one on screen marked,
    /// each of them closable, and a control that opens another.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (tabs, showing) = self.bar();
        let view = cx.entity();

        TabBar::new("tabs")
            .selected_index(showing)
            .children(
                tabs.into_iter()
                    .map(|(tab, name)| TabButton::new().label(name).suffix(self.closer(tab, cx))),
            )
            // The bar answers a press on a tab, rather than each tab answering
            // for itself, so which one was pressed is a position in the bar —
            // and the tab at that position is the one this window has.
            .on_click(move |index, window, cx| {
                let index = *index;
                view.update(cx, |view, cx| view.show_tab_at(index, window, cx));
            })
            .suffix(self.opener(cx))
    }

    /// The control on a tab that closes it.
    fn closer(&self, tab: TabId, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("close-tab", tab.raw()))
            .px_1()
            .text_color(cx.theme().muted_foreground)
            .hover(|style| style.text_color(cx.theme().foreground))
            .child(CLOSE)
            .on_click(cx.listener(move |view, _: &ClickEvent, window, cx| {
                // Without this the press would go on to the tab this is drawn
                // in, and showing a tab on its way out is the one thing a
                // control for closing it must not do.
                cx.stop_propagation();
                view.close_tab(tab, window, cx);
            }))
    }

    /// The control at the end of the bar that opens another tab.
    fn opener(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("new-tab")
            .px_2()
            .text_color(cx.theme().muted_foreground)
            .hover(|style| style.text_color(cx.theme().foreground))
            .child(NEW)
            .on_click(cx.listener(|view, _: &ClickEvent, window, cx| view.new_tab(window, cx)))
    }

    /// Where in the arrangement the shell being typed in sits.
    ///
    /// Which tab that is in is the bar's to say; what this adds is where in
    /// that tab's arrangement the shell sits, which nothing else on screen puts
    /// into words.
    fn placement(&self) -> String {
        let showing = self.showing();
        let shell = showing
            .iter()
            .position(|shell| *shell == self.focused)
            .map_or(0, |index| index + 1);
        format!("shell {shell} of {}", showing.len())
    }

    /// The shells of the tab on screen, each in its own rectangle, with a
    /// divider on every edge two of them share.
    ///
    /// The rectangles are the model's own layout of that tab's arrangement in a
    /// unit square, put on screen as fractions of the room this element was
    /// given — so an arrangement of any shape lands where the model says it
    /// does, however irregular, without this having to know what shape it is.
    /// How much room that was is measured at the same time, because it is what
    /// a pointer's position has to be read against.
    fn arranged(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let framed = self
            .tree()
            .map(SplitTree::layout)
            .unwrap_or_default()
            .into_iter()
            .map(|placed| self.framed(placed, window, cx))
            .collect::<Vec<_>>();
        let handles = self
            .tree()
            .map(SplitTree::dividers)
            .unwrap_or_default()
            .into_iter()
            .map(|placed| self.handle(&placed, cx))
            .collect::<Vec<_>>();
        let measuring = cx.entity();

        div()
            .relative()
            .flex_1()
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, _, cx| {
                        measuring.update(cx, |view, _| view.area = bounds);
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(framed)
            // After the shells, so that a press on an edge two of them share is
            // a press on the edge.
            .children(handles)
    }

    /// One divider, as something to take hold of.
    ///
    /// It is drawn as nothing at all until it is being held: the shells either
    /// side of it already draw their own edges, and a second line over the top
    /// of those would be the arrangement saying the same thing twice. What it
    /// has instead is width to aim at and a pointer that says which way it
    /// moves.
    fn handle(&self, placed: &PlacedDivider, cx: &mut Context<Self>) -> AnyElement {
        let held = self.dragging.as_ref() == Some(&placed.divider);
        let divider = placed.divider.clone();
        let bounds = placed.bounds;
        let along = placed.axis;

        div()
            .absolute()
            .left(relative(bounds.x))
            .top(relative(bounds.y))
            .map(|handle| match along {
                Axis::Horizontal => handle
                    .w(GRIP)
                    .h(relative(bounds.height))
                    .ml(-(GRIP / 2.0))
                    .cursor(CursorStyle::ResizeLeftRight),
                Axis::Vertical => handle
                    .h(GRIP)
                    .w(relative(bounds.width))
                    .mt(-(GRIP / 2.0))
                    .cursor(CursorStyle::ResizeUpDown),
            })
            .when(held, |handle| handle.bg(cx.theme().ring))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                    view.take(divider.clone(), window, cx);
                }),
            )
            .into_any_element()
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
            .on_action(cx.listener(move |view, _: &NextTab, window, cx| {
                view.step(true, window, cx);
            }))
            .on_action(cx.listener(move |view, _: &PreviousTab, window, cx| {
                view.step(false, window, cx);
            }))
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
            .flex_row()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // A divider is dragged by the whole window rather than by the
            // divider: the pointer leaves an eight-pixel strip on the first
            // frame it moves, and a drag that stopped there would be a drag
            // nobody could do.
            .on_mouse_move(cx.listener(Self::dragged))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _: &MouseUpEvent, _, cx| view.dropped(cx)),
            )
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(self.header(cx))
                    .child(self.tabs(cx))
                    .child(self.arranged(window, cx)),
            );
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

/// The room the arrangement was given, as the arrangement's own kind of
/// rectangle, so that a tab's shells and dividers can be laid out at the scale
/// a pointer is reported in.
fn area(bounds: Bounds<Pixels>) -> Rect {
    Rect::new(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
    )
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
