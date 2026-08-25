//! What a foreground process group is called, on macOS.
//!
//! There is no process table in the filesystem here, so the name of a group is
//! asked for with a system call: `proc_pidinfo` for the BSD information about
//! one process, out of which two fields are names. A process group's id is its
//! leader's pid, so the group the terminal named is also the process to ask
//! about.
//!
//! # Which of the two names, and why this call rather than `proc_name`
//!
//! The record carries a short name, always present and cut to sixteen bytes, and
//! a longer one that is set for a process whose name did not fit. `proc_name` in
//! the same library is a wrapper that prefers the longer where there is one and
//! falls back to the short, which is exactly the rule wanted here — so that rule
//! is applied directly to the record. Going through the wrapper would mean
//! declaring it here by hand, because it is one of the few in that family that
//! the platform bindings this workspace already depends on do not declare, and
//! hand-written declarations of somebody else's ABI are worth avoiding when the
//! call underneath is already available and says more.
//!
//! # Nothing here fails
//!
//! A group may exit between the kernel naming it and this asking about it. That
//! is the ordinary business of sampling something that is changing, so it
//! answers `None` and writes one line saying what happened.

use std::ffi::{c_char, c_int, c_void};

use tracing::debug;

use super::unix::group;
use super::{Foreground, Pid};
use crate::terminal::Device;

/// The process group in front of `device`, and what it is called.
pub fn foreground(device: &Device) -> Option<Foreground> {
    let group = group(device)?;
    let name = name(group)?;
    Some(Foreground::new(group, name))
}

/// The executable name of the process leading `group`.
fn name(group: Pid) -> Option<String> {
    // Safe by construction: the record is plain integers and arrays of bytes, so
    // an all-zero one is a valid value of it; the call is given that record's own
    // size and writes no more than it; and what it wrote is checked before
    // anything is read back out.
    let mut record: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    let written = unsafe {
        libc::proc_pidinfo(
            group,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut record).cast::<c_void>(),
            size,
        )
    };
    if written != size {
        debug!(group, written, "cannot read what is running in a shell");
        return None;
    }

    text(&record.pbi_name).or_else(|| text(&record.pbi_comm))
}

/// One of the record's name fields, as far as the first terminator.
///
/// A field that is entirely terminator is a name that was never set, which is
/// the whole reason there are two of them; that reads as no name rather than as
/// an empty one.
fn text(field: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
