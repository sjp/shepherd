//! A shell, running: a process, the terminal device it is attached to, and the
//! grid of what it has printed.
//!
//! The model one module over says how shells are arranged. This one is what
//! sits in each of those slots — a pseudo-terminal with a process on the far
//! side of it, an emulator parsing that process's output into a grid, and a
//! thread doing that continuously for as long as the shell exists.
//!
//! # It is always reading
//!
//! There is nothing here that pauses a shell, and that is deliberate rather
//! than incidental. A shell whose tab is not the open one, whose split is
//! covered, whose window is behind another window, is still being read from and
//! its grid is still being kept up to date. Anything that wants to know what a
//! shell has been doing gets the whole answer, not the part of it that happened
//! while somebody was watching — and an agent that finished an hour ago in a
//! tab nobody has opened since has still finished.
//!
//! The cost of that is one thread and one grid per shell, which is what a
//! terminal multiplexer costs. If it ever stops being affordable, the answer is
//! to make reading cheaper, not to stop doing it: a status that is only correct
//! for the shell being looked at is worse than no status, because there is no
//! way to tell from the outside which kind you are looking at.
//!
//! # The environment is the whole integration
//!
//! Every shell is started with [`CORRELATION_VAR`] set to the string
//! [`correlation_for`] gives its address, and that is the entire mechanism by
//! which anything started in that shell — a coding agent, and every process it
//! in turn starts — can be recognised as having been started *there*. It is set
//! before the process is, so there is no window in which a descendant could
//! miss it, and nothing further is asked of the agent, the shell or the person
//! using either.
//!
//! # It says what is in it
//!
//! A shell is called after whatever process is in front of its terminal, which
//! it finds out by asking — see [`Shell::poll_name`] and the module behind it —
//! and renames itself when that changes. A name somebody chose with
//! [`Shell::set_name`] outranks that for as long as it stands.
//!
//! # What is not here
//!
//! Nothing draws anything. The grid is exposed — [`Shell::term`] for a caller
//! that wants cells and colours, [`Shell::screen`] and [`Shell::buffer`] for one
//! that wants text — and what to do with it belongs to whatever has pixels.
//! Likewise [`Shell::write`] takes bytes: turning a key press into bytes is a
//! keymap's job and a keymap needs a keyboard.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, State};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as TtyOptions, Pty, Shell as TtyProgram};
use thiserror::Error;
use tracing::debug;

use crate::correlation::correlation_for;
use crate::ids::ShellAddress;
use crate::naming::{Kernel, ShellName};

pub use alacritty_terminal::index::{Column, Line, Point};

#[cfg(test)]
mod tests;

/// The environment variable a shell's correlation string travels in.
///
/// This is the event bus's variable, not this application's: the bus copies
/// whatever it finds here onto everything it reports about a process, without
/// looking inside it. Writing the name out here rather than importing it is
/// what keeps the dependency pointing the way it does — the bus is a thing this
/// application talks to, and the name of an environment variable is the whole
/// of the contract.
pub const CORRELATION_VAR: &str = "AGENTBUS_PANE";

/// The variable a terminal describes itself to its process with.
pub const TERM_VAR: &str = "TERM";

/// What `TERM` is set to for a shell started here.
///
/// The emulator underneath understands rather more than this describes, but a
/// terminfo entry is only useful if the machine the shell runs on has it, and
/// this is the entry every unix has had for decades. Claiming a name that is
/// not installed — on a fresh container, on a server somebody sshed to — makes
/// every full-screen program fall back to something far worse than this.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// How many lines of scrolled-off output a shell remembers.
///
/// Enough that a build's output, or an agent's last several minutes of
/// thinking, is still there to scroll back to. This is stated rather than left
/// to the emulator's default so that it is a number somebody chose.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// How wide a grid is before anything has said otherwise.
pub const DEFAULT_COLUMNS: u16 = 80;

/// How tall a grid is before anything has said otherwise.
pub const DEFAULT_LINES: u16 = 24;

/// The narrowest grid there can be.
///
/// A single column cannot hold a double-width character — the emulator writes
/// one of those into a cell and its spacer into the next — so a grid one column
/// wide is not a grid that can show everything a process may print.
pub const MIN_COLUMNS: u16 = 2;

/// The shortest grid there can be.
pub const MIN_LINES: u16 = 1;

/// How wide one cell is in pixels, before a font has been measured.
///
/// Only ever handed to the process, which passes it on to whatever wants to
/// draw at pixel resolution inside a terminal. Nothing here draws, so nothing
/// here is affected by it being an approximation.
pub const DEFAULT_CELL_WIDTH: u16 = 8;

/// How tall one cell is in pixels, before a font has been measured.
pub const DEFAULT_CELL_HEIGHT: u16 = 16;

/// How big a shell's grid is, and how big one of its cells is on a screen.
///
/// The two are separate questions with separate answers: the emulator lays out
/// text in columns and rows and needs only those, while the process on the far
/// side is told all four, because a program drawing an image into a terminal
/// has to know how many pixels a cell is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellSize {
    columns: u16,
    lines: u16,
    cell_width: u16,
    cell_height: u16,
}

impl ShellSize {
    /// A grid `columns` wide and `lines` tall, with cells of the default size.
    ///
    /// Both are brought up to [`MIN_COLUMNS`] and [`MIN_LINES`] rather than
    /// refused: a size arrives from a layout, a layout goes through zero on its
    /// way to somewhere sensible, and an error at that moment would be an error
    /// about nothing.
    pub const fn new(columns: u16, lines: u16) -> Self {
        Self {
            columns: if columns < MIN_COLUMNS {
                MIN_COLUMNS
            } else {
                columns
            },
            lines: if lines < MIN_LINES { MIN_LINES } else { lines },
            cell_width: DEFAULT_CELL_WIDTH,
            cell_height: DEFAULT_CELL_HEIGHT,
        }
    }

    /// The same grid, with cells of a size somebody has actually measured.
    #[must_use]
    pub const fn with_cell(mut self, width: u16, height: u16) -> Self {
        self.cell_width = width;
        self.cell_height = height;
        self
    }

    /// How many columns across.
    pub const fn columns(self) -> u16 {
        self.columns
    }

    /// How many rows down.
    pub const fn lines(self) -> u16 {
        self.lines
    }

    /// How wide one cell is in pixels.
    pub const fn cell_width(self) -> u16 {
        self.cell_width
    }

    /// How tall one cell is in pixels.
    pub const fn cell_height(self) -> u16 {
        self.cell_height
    }
}

impl Default for ShellSize {
    fn default() -> Self {
        Self::new(DEFAULT_COLUMNS, DEFAULT_LINES)
    }
}

impl Dimensions for ShellSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.lines)
    }

    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

impl From<ShellSize> for WindowSize {
    fn from(size: ShellSize) -> Self {
        Self {
            num_lines: size.lines,
            num_cols: size.columns,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

/// What to run in a shell.
///
/// Left unsaid, a shell runs whatever the person using it has chosen as their
/// login shell. Said, it runs exactly this — which is how a shell that is
/// really a command in a container, or a single program somebody wanted a
/// terminal around, is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    program: String,
    args: Vec<String>,
}

impl Program {
    /// Runs `program` with no arguments.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Runs it with these arguments instead.
    #[must_use]
    pub fn with_args<A: Into<String>>(mut self, args: impl IntoIterator<Item = A>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// What is run.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// What it is run with.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Everything that can be decided about a shell before it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOptions {
    program: Option<Program>,
    directory: Option<PathBuf>,
    env: BTreeMap<String, String>,
    size: ShellSize,
    scrollback: usize,
}

impl ShellOptions {
    /// The default shell, in no particular directory, at the default size.
    pub fn new() -> Self {
        Self {
            program: None,
            directory: None,
            env: BTreeMap::new(),
            size: ShellSize::default(),
            scrollback: DEFAULT_SCROLLBACK,
        }
    }

    /// Runs `program` rather than the person's login shell.
    #[must_use]
    pub fn program(mut self, program: Program) -> Self {
        self.program = Some(program);
        self
    }

    /// Starts the shell in `directory`.
    #[must_use]
    pub fn directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = Some(directory.into());
        self
    }

    /// Puts one more variable in the shell's environment.
    ///
    /// [`CORRELATION_VAR`] is not settable this way — whatever is put there is
    /// replaced by the shell's own correlation, because a shell lying about
    /// which shell it is would misattribute everything running in it.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Opens the grid at this size.
    #[must_use]
    pub const fn size(mut self, size: ShellSize) -> Self {
        self.size = size;
        self
    }

    /// Remembers this many scrolled-off lines rather than [`DEFAULT_SCROLLBACK`].
    #[must_use]
    pub const fn scrollback(mut self, lines: usize) -> Self {
        self.scrollback = lines;
        self
    }

    /// What will be run.
    pub fn chosen_program(&self) -> Option<&Program> {
        self.program.as_ref()
    }

    /// Where it will be run.
    pub fn chosen_directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }

    /// Every variable a shell started with these options is given, correlated
    /// as `correlation`.
    ///
    /// Three things go in: what a caller asked for, the terminal's own
    /// description of itself, and the correlation — the last of which is
    /// written unconditionally, over whatever a caller put there.
    ///
    /// This is answerable without starting anything because a shell does not
    /// always run on this machine. One that runs somewhere else is started by a
    /// command that has to be *told* what environment to give it, and a
    /// container or a remote host that were told only some of this would leave
    /// a shell that either renders wrongly or cannot be attributed at all.
    pub fn environment(&self, correlation: &str) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        env.entry(TERM_VAR.to_owned())
            .or_insert_with(|| DEFAULT_TERM.to_owned());
        // Last, and unconditionally: a caller that set this was mistaken, and
        // this application's own environment may well carry a correlation of
        // its own — the one for the shell somebody started it from — which this
        // has to overwrite rather than inherit.
        env.insert(CORRELATION_VAR.to_owned(), correlation.to_owned());
        env
    }
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a shell's process is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellState {
    /// There is a process on the far side of the terminal.
    Running,
    /// There is not any more.
    ///
    /// The status is what the process ended with, where the platform said. It
    /// is `None` for an end that was noticed without being described — the
    /// reading thread stopping for a reason of its own, most of all — which is
    /// still an ending and is still better said than left to be guessed at.
    Exited(Option<ExitStatus>),
}

impl ShellState {
    /// Whether there is still a process.
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    /// What the process exited with, if it exited and said so.
    pub fn code(self) -> Option<i32> {
        match self {
            Self::Running => None,
            Self::Exited(status) => status.and_then(|status| status.code()),
        }
    }
}

/// Why a shell could not be started.
#[derive(Debug, Error)]
pub enum SpawnError {
    /// The terminal device, or the process on the far side of it, would not
    /// start.
    #[error("cannot start a shell for `{correlation}`: {source}")]
    Start {
        /// The shell that was being started.
        correlation: String,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The process started, but nothing could be arranged to read from it —
    /// which would leave a running process nobody is listening to, so it is
    /// stopped again rather than returned.
    #[error("cannot read the shell started for `{correlation}`: {source}")]
    Read {
        /// The shell that was being started.
        correlation: String,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
}

/// One shell: a process, its terminal, and everything it has printed.
///
/// Dropping it ends the process and waits for it, so a shell never outlives
/// whatever was holding it.
pub struct Shell {
    address: ShellAddress,
    correlation: String,
    name: ShellName,
    term: Arc<FairMutex<Term<ShellListener>>>,
    listener: ShellListener,
    channel: EventLoopSender,
    device: Device,
    /// Held in an [`Option`] only so that dropping the shell can take the
    /// handle in order to wait for the thread.
    reading: Option<JoinHandle<(EventLoop<Pty, ShellListener>, State)>>,
}

impl Shell {
    /// Starts a shell for `address`.
    ///
    /// The process is running and being read from by the time this returns.
    /// Its environment carries [`CORRELATION_VAR`], so anything it starts is
    /// recognisable as having been started here.
    pub fn spawn(address: ShellAddress, options: &ShellOptions) -> Result<Self, SpawnError> {
        let correlation = correlation_for(address.workspace, address.shell);
        let size = options.size;

        let listener = ShellListener::new(size);
        let term = Term::new(
            Config {
                scrolling_history: options.scrollback,
                ..Config::default()
            },
            &size,
            listener.clone(),
        );
        let term = Arc::new(FairMutex::new(term));

        let pty = tty::new(&options.tty(&correlation), size.into(), 0).map_err(|source| {
            SpawnError::Start {
                correlation: correlation.clone(),
                source,
            }
        })?;
        let device = Device::take(&pty).map_err(|source| SpawnError::Read {
            correlation: correlation.clone(),
            source,
        })?;

        // `drain_on_exit`: the last thing a process prints is often the only
        // thing worth reading — a compiler's error, a shell's "command not
        // found" — and it is written immediately before the exit that would
        // otherwise stop anyone reading it.
        let reading = EventLoop::new(Arc::clone(&term), listener.clone(), pty, true, false)
            .map_err(|source| SpawnError::Read {
                correlation: correlation.clone(),
                source,
            })?;
        let channel = reading.channel();
        // Before the thread starts, so that the first thing the process prints
        // can already be replied to.
        listener.replies(channel.clone());
        let reading = reading.spawn();

        debug!(%correlation, columns = size.columns, lines = size.lines, "started a shell");
        Ok(Self {
            address,
            correlation,
            name: ShellName::new(),
            term,
            listener,
            channel,
            device,
            reading: Some(reading),
        })
    }

    /// Which shell of which workspace this is.
    pub fn address(&self) -> ShellAddress {
        self.address
    }

    /// The string this shell is known by outside this process.
    pub fn correlation(&self) -> &str {
        &self.correlation
    }

    /// The grid, and everything else the emulator knows.
    ///
    /// The lock is the one the reading thread takes to parse into: hold it for
    /// as long as it takes to read what is wanted and no longer, because a
    /// shell producing output is waiting on it.
    pub fn term(&self) -> &FairMutex<Term<ShellListener>> {
        &self.term
    }

    /// The terminal device this shell is attached to.
    ///
    /// Reading a shell's grid says what it has printed. This is how the kernel
    /// is asked about it instead — most usefully, which process group has the
    /// terminal in front of it, which is what says *what* is running in a shell
    /// rather than what it has said. Nothing here asks; the handle is kept
    /// because this is the only moment it can be taken, the terminal itself
    /// having been handed to the thread that reads it.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// What to call this shell: what somebody named it, or failing that what is
    /// running in it as of the last look.
    ///
    /// `None` for a shell nobody has named and nothing has looked at yet — see
    /// [`Shell::poll_name`], which is what does the looking.
    pub fn name(&self) -> Option<&str> {
        self.name.name()
    }

    /// Names the shell, until [`Shell::clear_name`] says otherwise.
    ///
    /// Nothing running in it renames it after this, however long it runs and
    /// whatever it is.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name.set(name);
    }

    /// Gives that name up, so the shell goes back to being called after whatever
    /// is running in it.
    pub fn clear_name(&mut self) {
        self.name.clear();
    }

    /// Looks at what is running in front of this shell's terminal, unless that
    /// was done within the last [`FOREGROUND_INTERVAL`].
    ///
    /// Answers whether what the shell is called changed, so a caller drawing a
    /// list of shells can call it on every one of them as often as it likes and
    /// redraw only when there is something different to draw.
    ///
    /// [`FOREGROUND_INTERVAL`]: crate::naming::FOREGROUND_INTERVAL
    pub fn poll_name(&mut self) -> bool {
        self.name.poll(&self.device, &Kernel)
    }

    /// Everything known about what this shell is called, including what is
    /// running in it when that is not what it is called.
    pub fn naming(&self) -> &ShellName {
        &self.name
    }

    /// How big the grid is.
    pub fn size(&self) -> ShellSize {
        self.listener.size()
    }

    /// How many times the contents have changed since the shell started.
    ///
    /// Only ever goes up, and goes up whenever there is something new to look
    /// at. A caller that has drawn the grid can remember this number and know,
    /// by comparing it, whether drawing again would produce a different
    /// picture — without holding the terminal's lock to find out.
    pub fn revision(&self) -> u64 {
        self.listener.revision()
    }

    /// Whether the process is still running, and what it ended with if not.
    pub fn state(&self) -> ShellState {
        let said = self.listener.state();
        if said.is_running() && self.reading.as_ref().is_some_and(JoinHandle::is_finished) {
            // The thread that would have said so has stopped without saying
            // anything. Nothing is reading the terminal any more, so whatever
            // is or is not still running behind it, this shell is over.
            return ShellState::Exited(None);
        }
        said
    }

    /// Sends bytes to the process as though they had been typed.
    ///
    /// Bytes, not keys: what a key press is worth depends on the keyboard, the
    /// modifiers and the modes the terminal is in, and none of those are
    /// questions this can answer.
    pub fn write(&mut self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        // A process that has gone is a send that fails, which is ordinary
        // rather than exceptional: a shell whose process exited is still on
        // screen and can still be typed at.
        let _ = self.channel.send(Msg::Input(bytes.into()));
    }

    /// Changes how big the grid is.
    ///
    /// Both halves are told: the emulator, which reflows what is already there,
    /// and the process, which is what makes a full-screen program redraw itself
    /// at the new size.
    pub fn resize(&mut self, size: ShellSize) {
        self.term.lock().resize(size);
        self.listener.resized(size);
        let _ = self.channel.send(Msg::Resize(size.into()));
    }

    /// The rows currently on screen, top to bottom, without trailing blanks.
    ///
    /// This is the viewport rather than the buffer: a shell scrolled back shows
    /// what it is scrolled back to, which is what somebody looking at it would
    /// see.
    pub fn screen(&self) -> Vec<String> {
        let term = self.term.lock();
        let grid = term.grid();
        let offset = i32::try_from(grid.display_offset()).unwrap_or(i32::MAX);
        let lines = i32::try_from(grid.screen_lines()).unwrap_or(i32::MAX);
        (0..lines)
            .map(|line| row(grid, Line(line - offset)))
            .collect()
    }

    /// Every row the terminal still remembers, oldest first.
    ///
    /// The scrollback and then the screen. What has fallen off the end of the
    /// scrollback is gone, which is what a scrollback of a chosen size means.
    pub fn buffer(&self) -> Vec<String> {
        let term = self.term.lock();
        let grid = term.grid();
        let history = i32::try_from(grid.history_size()).unwrap_or(i32::MAX);
        let lines = i32::try_from(grid.screen_lines()).unwrap_or(i32::MAX);
        (-history..lines)
            .map(|line| row(grid, Line(line)))
            .collect()
    }

    /// Where the cursor is.
    pub fn cursor(&self) -> Point {
        self.term.lock().grid().cursor.point
    }

    /// Ends the shell's process and waits for it, which is also what dropping
    /// it does.
    pub fn close(self) {}
}

impl fmt::Debug for Shell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shell")
            .field("address", &self.address)
            .field("correlation", &self.correlation)
            .field("name", &self.name)
            .field("size", &self.size())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        // The send is what wakes the thread: it notifies the poller the thread
        // is otherwise blocked on. Waiting for it then hands back the terminal
        // device it was reading, and letting go of that is what ends the
        // process — so the wait is not politeness, it is the shutdown.
        let _ = self.channel.send(Msg::Shutdown);
        if let Some(reading) = self.reading.take() {
            let _ = reading.join();
        }
    }
}

impl ShellOptions {
    /// This, said the way the terminal device wants to hear it.
    fn tty(&self, correlation: &str) -> TtyOptions {
        let env = self.environment(correlation).into_iter().collect();

        TtyOptions {
            shell: self
                .program
                .as_ref()
                .map_or_else(fallback_program, |program| {
                    Some(TtyProgram::new(
                        program.program.clone(),
                        program.args.clone(),
                    ))
                }),
            working_directory: self.directory.clone(),
            drain_on_exit: true,
            env,
            // A [`Program`]'s arguments are separate strings here, so they are
            // separate arguments there too rather than one command line that
            // happens to have spaces in it.
            #[cfg(windows)]
            escape_args: true,
        }
    }
}

/// What to run when nobody has said and the environment does not either.
///
/// Saying nothing lets the terminal device pick, which it does by reading
/// `$SHELL` and then the password database — and on macOS by going through
/// `login`, so that the shell is a real session with a real utmp entry rather
/// than an orphan. That is the better answer wherever it can be had, so it is
/// only overridden where it cannot: an environment with no `$SHELL` in it at
/// all, which is what a graphical application launched from a desktop can
/// inherit, gets the one shell every unix is required to have.
fn fallback_program() -> Option<TtyProgram> {
    if std::env::var_os("SHELL").is_some() {
        return None;
    }
    Some(TtyProgram::new("/bin/sh".to_owned(), Vec::new()))
}

/// One row of a grid as text, without the blanks it ends in.
fn row(grid: &Grid<Cell>, line: Line) -> String {
    let row = &grid[line];
    let mut text = String::with_capacity(grid.columns());
    for column in 0..grid.columns() {
        let cell = &row[Column(column)];
        // A double-width character occupies its own cell and reserves the one
        // after it; the reserved cell holds no character of its own and
        // spelling it out would double every one of them.
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
    }
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);
    text
}

/// What the emulator says back to the shell it belongs to.
///
/// The emulator is handed one of these and calls it as it parses, from the
/// thread doing the parsing and — this is the constraint everything here is
/// shaped by — while holding the terminal's own lock. So nothing here may take
/// that lock, which is why what is kept is a counter, a state and a way of
/// writing to the process, and not a copy of anything on screen.
#[derive(Clone)]
pub struct ShellListener(Arc<Reports>);

/// What a [`ShellListener`] has to say, and what it needs in order to answer.
struct Reports {
    revision: AtomicU64,
    state: Mutex<ShellState>,
    size: Mutex<ShellSize>,
    /// The way back to the process, for the sequences it expects an answer to.
    /// Set once, immediately after the loop that carries it exists and before
    /// anything can have been parsed.
    replies: OnceLock<EventLoopSender>,
}

impl ShellListener {
    fn new(size: ShellSize) -> Self {
        Self(Arc::new(Reports {
            revision: AtomicU64::new(0),
            state: Mutex::new(ShellState::Running),
            size: Mutex::new(size),
            replies: OnceLock::new(),
        }))
    }

    fn replies(&self, channel: EventLoopSender) {
        let _ = self.0.replies.set(channel);
    }

    fn revision(&self) -> u64 {
        self.0.revision.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        self.0.revision.fetch_add(1, Ordering::Relaxed);
    }

    fn state(&self) -> ShellState {
        *lock(&self.0.state)
    }

    fn size(&self) -> ShellSize {
        *lock(&self.0.size)
    }

    fn resized(&self, size: ShellSize) {
        *lock(&self.0.size) = size;
        // Reflowed text is different text, even though no byte arrived to make
        // it so.
        self.changed();
    }

    /// Records an ending, keeping the first account of it: the exit status
    /// comes from the platform and is the better answer, and the notice that
    /// follows it carries nothing.
    fn ended(&self, status: Option<ExitStatus>) {
        let mut state = lock(&self.0.state);
        if state.is_running() {
            *state = ShellState::Exited(status);
        }
    }

    /// Writes an answer back to the process.
    fn reply(&self, bytes: Vec<u8>) {
        if let Some(replies) = self.0.replies.get() {
            let _ = replies.send(Msg::Input(bytes.into()));
        }
    }
}

impl EventListener for ShellListener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Wakeup => self.changed(),
            TermEvent::ChildExit(status) => {
                self.ended(Some(status));
                self.changed();
            }
            TermEvent::Exit => {
                self.ended(None);
                self.changed();
            }
            // A sequence the process expects an answer to — what kind of
            // terminal this is, where the cursor is. Unanswered, a program that
            // asked sits waiting for its own timeout, so these are answered
            // here rather than passed to a caller who may not be looking.
            TermEvent::PtyWrite(text) => self.reply(text.into_bytes()),
            TermEvent::TextAreaSizeRequest(format) => self.reply(format(self.size().into()).into()),
            // Everything else needs something this does not have. A colour
            // request wants the palette a renderer would have chosen; the
            // clipboard requests want a clipboard; the title and the bell want
            // somewhere to show them. None of those exist yet, and answering
            // any of them with a guess would be worse than the silence a
            // program asking already has to cope with.
            _ => {}
        }
    }
}

impl fmt::Debug for ShellListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellListener")
            .field("revision", &self.revision())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// Reads a lock, taking what is behind it even if whoever held it last
/// panicked. Nothing here holds one of these across anything that can panic, so
/// a poisoned lock says only that a thread died elsewhere — and a shell that
/// stopped reporting its state because of that would be a second failure caused
/// by the first.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A handle on the terminal device behind a shell, held open for as long as the
/// shell is.
///
/// What can be asked through it is the platform's business, which is why this
/// is a type rather than a file descriptor: unix has one and answers questions
/// about process groups through it, and Windows' pseudo-console is a different
/// kind of object that answers a differently-shaped question. Keeping the type
/// on both means the thing that eventually asks can be written for both without
/// the shape of everything around it having to change first.
#[derive(Debug)]
pub struct Device {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
}

impl Device {
    /// The device's file descriptor.
    #[cfg(unix)]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;

        self.fd.as_fd()
    }

    /// Takes a handle that outlives the terminal it came from.
    ///
    /// The terminal itself is given to the thread that reads it and is not
    /// shared, so this is the last moment anything else can hold on to it.
    fn take(pty: &Pty) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                fd: pty.file().try_clone()?.into(),
            })
        }
        #[cfg(windows)]
        {
            let _ = pty;
            unimplemented!("a handle on a Windows pseudo-console")
        }
    }
}
