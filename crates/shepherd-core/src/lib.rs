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

#![warn(missing_docs)]
