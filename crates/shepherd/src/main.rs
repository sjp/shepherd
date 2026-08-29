//! The Shepherd binary.
//!
//! One window on one folder, with shells in it, and the event bus read behind
//! them. That is deliberately less than a terminal multiplexer: what it is for
//! is the seam between the two halves this repository builds — live processes
//! feeding terminal grids, those grids reaching pixels and a keyboard, and an
//! agent started in one of those shells being recognised as having been started
//! *there* — with as little else in the way as possible.
//!
//! # How an agent gets found
//!
//! Each shell is started with the event bus's environment variable set to a
//! string naming that shell. An agent started in it inherits the variable; the
//! agent's hooks report it to the bus; the bus copies it onto everything it says
//! about that agent, never looking inside it; and this reads the bus's stream
//! and joins the string back to the shell it named. Nothing was added to the bus
//! for any of that, and nothing in the bus knows this program exists.
//!
//! # What is on screen
//!
//! Down the left, everything that is open — the workspace, its tabs, the shells
//! in each of them — with a badge on every row saying what the bus knows about
//! what is running there, and beneath that one flat list of every agent it is
//! reporting, including the ones running somewhere this application cannot
//! claim. The rest of the window is the tabs and the arrangement of shells
//! belonging to whichever tab is showing.

mod frames;
mod grid;
mod keymap;
mod keys;
mod live;
mod menu;
mod palette;
mod screen;
mod sidebar;
mod terminal;

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use clap::Parser;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px,
    size as window_size,
};
use gpui_component::Root;
use shepherd_core::{Layout, Program, Shell, ShellAddress, ShellOptions};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::terminal::TerminalView;

/// The environment variable that says how much this application says about
/// itself, for a person who wants to see the bus being read rather than infer it
/// from the window.
const LOG_VAR: &str = "SHEPHERD_LOG";

/// What the desktop groups this application's windows under.
const APP_ID: &str = "shepherd";

/// How big the window is when nothing has said otherwise.
const WIDTH: f32 = 1024.0;

/// How tall the window is when nothing has said otherwise.
const HEIGHT: f32 = 700.0;

/// What the one tab is called, for the one shell it holds.
const TAB: &str = "shell";

/// The `shepherd` command line.
#[derive(Debug, Parser)]
#[command(name = "shepherd", version)]
struct Cli {
    /// How much to say on stderr: a level — off, error, warn, info, debug, trace — or a filter naming targets
    #[arg(
        long,
        value_name = "LEVEL",
        env = LOG_VAR,
        default_value = "info",
        value_parser = filter,
    )]
    log_level: String,

    /// What to run in the first shell; without it, your login shell
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log_level);
    match open(&cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // On stderr rather than through the logger, because a window that
            // never opened is a failure a person is owed an account of whatever
            // they set the log level to.
            eprintln!("shepherd: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Starts a shell, opens a window on it, and runs until the window is closed.
fn open(command: &[String]) -> Result<()> {
    let directory = std::env::current_dir().context("cannot tell which directory this is")?;

    // One workspace, one tab, one shell. The model is the full one rather than a
    // pair of numbers, because it is what the bus's sessions are placed against
    // and that placement is the thing being proved.
    let mut layout = Layout::new();
    let workspace = layout.open(directory.clone());
    let address = {
        let open = layout
            .workspace_mut(workspace)
            .expect("the workspace just opened");
        let tab = open.open_tab(TAB);
        let shell = open.tab(tab).expect("the tab just opened").focused();
        ShellAddress::new(workspace, shell)
    };

    // Shells opened later run the login shell in the same folder: what was
    // asked for on the command line was asked for once, the way it is of a
    // terminal started to run one thing.
    let options = ShellOptions::new().directory(&directory);
    let mut first = options.clone();
    if let Some((program, arguments)) = command.split_first() {
        first = first.program(Program::new(program).with_args(arguments.to_vec()));
    }
    let shell = Shell::spawn(address, &first).context("cannot start a shell")?;
    info!(
        correlation = shell.correlation(),
        directory = %directory.display(),
        "started a shell"
    );

    // A window that would not open is the one failure that happens after the
    // application has started, and it has to come back out here: a program that
    // showed nobody anything and exited nought is a program a script cannot tell
    // worked.
    let refused = Rc::new(RefCell::new(None));
    Application::new().run({
        let refused = Rc::clone(&refused);
        move |cx: &mut App| {
            gpui_component::init(cx);
            keymap::install(cx);
            menu::install(cx);
            let opened = cx.open_window(window(cx), |window, cx| {
                let view = cx.new(|cx| TerminalView::new(shell, layout, options, window, cx));
                // The first layer in the window is the widget layer's own root,
                // which is where anything drawn over the window — a dialog, a
                // notification — is put.
                cx.new(|cx| Root::new(view, window, cx))
            });
            if let Err(error) = opened {
                *refused.borrow_mut() = Some(error);
                cx.quit();
                return;
            }
            // Closing the only window is how this application is quit: there is
            // one, and nothing else to go back to once it is gone.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        }
    });

    match refused.borrow_mut().take() {
        Some(error) => Err(error.context("cannot open a window")),
        None => Ok(()),
    }
}

/// The window this opens.
fn window(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            window_size(px(WIDTH), px(HEIGHT)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(menu::NAME.into()),
            ..TitlebarOptions::default()
        }),
        app_id: Some(APP_ID.to_owned()),
        ..WindowOptions::default()
    }
}

/// Accepts a verbosity the log filter understands, and hands it back unchanged.
///
/// Refused here, when the command line is parsed, rather than when the filter is
/// built — so an unusable value is answered with usage on stderr before a shell
/// has been started and a window opened.
fn filter(level: &str) -> Result<String, String> {
    EnvFilter::try_new(level)
        .map(|_| level.to_owned())
        .map_err(|error| error.to_string())
}

/// Sends diagnostics to stderr, at the verbosity asked for.
///
/// This is a program somebody starts from a terminal and watches, so what it
/// says goes to the terminal they started it from — and never to stdout, which
/// carries the version line and nothing else.
fn init_logging(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
