//! The half of the question every unix answers the same way.
//!
//! Which process group a terminal has in front of it is `tcgetpgrp`, on Linux
//! and on macOS alike, and it is the same call on both. What that group is
//! *called* is where the two part company, which is why that half is in a file
//! per platform and this half is not.

use std::os::fd::AsRawFd;

use super::Pid;
use crate::terminal::Device;

/// Which process group the kernel has in front of `device`.
///
/// `None` for a terminal with no foreground group, and for one this process is
/// not allowed to ask about. Neither is worth a diagnostic of its own — a shell
/// being started and a shell being torn down both pass through it — and both are
/// answered again a fraction of a second later.
pub fn group(device: &Device) -> Option<Pid> {
    // Safe by construction: `tcgetpgrp` reads kernel state for one descriptor,
    // writes nothing this process owns, and is passed a descriptor that is
    // borrowed for the whole of the call.
    let group = unsafe { libc::tcgetpgrp(device.as_fd().as_raw_fd()) };
    if group <= 0 {
        return None;
    }
    Some(group)
}
