//! What is in front of a terminal, on Windows: not written, and loud about it.
//!
//! Everything above this file is ready for the answer. What is missing is the
//! answer itself, and it is missing because the question is a different one: a
//! pseudo-console has no controlling terminal, no foreground process group and
//! no `tcgetpgrp`, so what is running in one is found by walking from the
//! console's own process to whatever it started rather than by asking the kernel
//! which group is in front of it.
//!
//! Writing that means writing this one function. Nothing else has to move: the
//! trait, the polling, the interval and the rule that a name somebody chose wins
//! are all platform-independent already, and the type this is handed —
//! [`Device`] — exists here for exactly this reason.
//!
//! Until then it panics rather than answering `None`. `None` is a real answer
//! that this module gives all the time on the platforms it is written for — a
//! terminal with nothing in front of it — and a platform arm that quietly
//! borrowed it would leave every shell nameless with nothing anywhere saying
//! why.

use super::Foreground;
use crate::terminal::Device;

/// The process group in front of `device`, and what it is called.
///
/// # Panics
///
/// Always: this platform's arm has not been written.
pub fn foreground(device: &Device) -> Option<Foreground> {
    let _ = device;
    unimplemented!("what is in front of a Windows pseudo-console")
}
