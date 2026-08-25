//! What a foreground process group is called, on Linux.
//!
//! The kernel publishes the process table as files, so the name of a group is
//! one read of one of them. A process group's id is its leader's pid, so the
//! group the terminal named is also the directory to read.
//!
//! # The process table is read at its own path
//!
//! `/proc` is written here rather than taken as a parameter, which is the
//! opposite of how a general reader of a process table should be built. It is
//! right for this one because of how narrow the question is: this reads one file
//! about a process this very application started, on the machine it is running
//! on, and a caller who could point it elsewhere would be pointing it at a
//! process table that has nothing to do with the terminal in hand.
//!
//! # Nothing here fails
//!
//! A group may exit between the kernel naming it and this reading its name, and
//! a process may be gone by the time the read lands. Both are the ordinary
//! business of sampling something that is changing, so each answers `None` and
//! writes one line saying what happened.

use std::fs;

use tracing::debug;

use super::unix::group;
use super::{Foreground, Pid};
use crate::terminal::Device;

/// The process group in front of `device`, and what it is called.
pub fn foreground(device: &Device) -> Option<Foreground> {
    let group = group(device)?;
    let name = comm(group)?;
    Some(Foreground::new(group, name))
}

/// The executable name of the process leading `group`.
///
/// This is the name every listing of processes on this machine shows, truncated
/// by the kernel to fifteen bytes the same way. The argument vector beside it
/// would give a fuller answer — the whole command line, paths and flags and all
/// — and it is the wrong answer for this: what goes beside a terminal in a list
/// is what is running, not how it was invoked.
fn comm(group: Pid) -> Option<String> {
    let path = format!("/proc/{group}/comm");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            debug!(path, %error, "cannot read what is running in a shell");
            return None;
        }
    };
    let name = text.trim_end_matches('\n');
    if name.is_empty() {
        return None;
    }
    Some(name.to_owned())
}
