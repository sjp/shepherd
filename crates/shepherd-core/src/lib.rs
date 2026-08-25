//! The shared model behind a terminal multiplexer, with no window in sight.
//!
//! This crate owns everything a GUI needs in order to know what to draw and
//! when to redraw it, but nothing about how to draw it: the workspace, tab and
//! shell model and the status each level rolls up to, a subscriber that turns
//! the bus's event stream into that model's live state, the terminal core each
//! shell runs against, and the parts of a workspace's context — its
//! configuration, its devcontainer, its git branch — that come from reading
//! the filesystem rather than a socket. The window, the renderer, input and
//! chrome live one crate over; this one has no dependency capable of opening
//! one, so it builds and its tests run on a machine that has no display at
//! all.
//!
//! # Why the split
//!
//! Keeping this crate free of a rendering dependency is what lets the bulk of
//! a terminal multiplexer's logic be exercised by an ordinary test binary. A
//! model, a fold, a subscriber and a parser either behave correctly given
//! known inputs or they do not, and that question should never need a window
//! to open in order to be answered.
//!
//! # The model
//!
//! A [`Workspace`] is a folder somebody opened. It holds [`Tab`]s, each of
//! which holds a [`SplitTree`] — an arrangement of shells, where a shell is one
//! terminal running one process and a leaf of the tree is where one sits. That
//! is the whole shape, and everything else here is either a fact about
//! something in it or a way of getting from one part of it to another.
//!
//! Shells are how this crate meets the world outside the process. Each is
//! started knowing the string [`correlation_for`] gives it, everything
//! descending from it inherits that string, and whatever is watching those
//! processes reports it back without ever being told what is in it.
//!
//! # The live half
//!
//! [`Subscriber`] is where those reports come back. It reads the event bus's
//! published stream on a thread of its own and hands over [`Update`]s;
//! [`BusState`] folds them into what is currently true of every agent session,
//! which is the other half of what a sidebar needs in order to draw a badge.
//! Neither of them knows anything about this model — the bus is not allowed to,
//! and the half that reads it has no reason to.
//!
//! [`Attribution`] is where the two meet, and it is the only place they do: it
//! takes the workspaces as they are and the sessions as the bus reports them and
//! says which shell each is running in. What it cannot place it says so about,
//! rather than guessing.
//!
//! # The running half
//!
//! [`Shell`] is a slot in that model with a process actually in it: a terminal
//! device, whatever is attached to it, and the grid of everything it has
//! printed. It is where [`correlation_for`]'s string is put into the world —
//! set in the process's environment before it starts, so that everything
//! descending from it carries the same one — and it is read from continuously,
//! whether or not anybody is looking at it.
//!
//! It also says what is in it. [`ShellName`] asks the shell's own terminal which
//! process the kernel has in front of it, so a shell is called after whatever it
//! is currently running and renames itself when that changes — unless somebody
//! has named it themselves, which wins for as long as it stands.
//!
//! [`Shells`] is where a shell for a workspace is started, and it is the one
//! place that knows a shell does not always run on this machine: a workspace
//! set to use its project's development container has its shells started inside
//! one, with everything a shell needs — the correlation above first among it —
//! carried across that boundary explicitly, because nothing crosses it by
//! itself.
//!
//! # The half that outlives the process
//!
//! [`Config`] is the file a [`Layout`] — every workspace, and everything open in
//! each of them — is kept in between runs. What is saved is the arrangement and
//! nothing else: a restored shell is a fresh process started where the last one
//! was, because nothing here holds a terminal open across a restart.

#![warn(missing_docs)]

pub mod attribution;
pub mod bus;
pub mod config;
pub mod correlation;
pub mod devcontainer;
pub mod ids;
pub mod naming;
pub mod rollup;
pub mod split;
pub mod terminal;
pub mod workspace;

pub use attribution::{Attribution, ShellStatus, status_source};
pub use bus::{BusState, Subscriber, SubscriberHandle, Update};
pub use config::{CONFIG_VAR, Config, ConfigError, TreeError};
pub use correlation::{CorrelationError, correlation_for, parse_correlation};
pub use devcontainer::{
    ContainerError, Containers, Devcontainer, Machine, Outcome, Shells, StartError, described,
};
pub use ids::{ShellAddress, ShellId, ShellIds, TabId, TabIds, WorkspaceId, WorkspaceIds};
pub use naming::{FOREGROUND_INTERVAL, Foreground, ForegroundProcess, Kernel, Pid, ShellName};
pub use rollup::{RollupStatus, rollup, shell_status};
pub use split::{
    Axis, Branch, Closed, Direction, MalformedSplit, PlacedShell, Rect, Split, SplitTree,
};
pub use terminal::{
    CORRELATION_VAR, Device, Program, Shell, ShellOptions, ShellSize, ShellState, SpawnError,
    TERM_VAR,
};
pub use workspace::{Layout, MalformedLayout, Tab, Workspace, WorkspaceSettings};
