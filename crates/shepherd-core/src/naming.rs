//! What to call a shell, taken from whatever is running in it.
//!
//! A terminal in a list needs a name, and the name worth showing is what is
//! happening in it: the agent while an agent is working, the editor while a file
//! is open, the shell's own name when it is sitting at a prompt. None of that is
//! something to be told — it is something to be looked at — so a shell is named
//! after the process in front of its terminal, and renames itself when that
//! changes.
//!
//! # Asked of the terminal this application already holds
//!
//! The question goes to the shell's own pseudo-terminal: which process group has
//! the kernel put in front of it, and what is that group called. Both halves are
//! the platform's business — the group comes from `tcgetpgrp` on every unix, and
//! the name comes from the process table, which Linux exposes as files and macOS
//! does not expose at all — so the whole of it sits behind
//! [`ForegroundProcess`], with one implementation per platform and nothing
//! conditional at any call site.
//!
//! Asking the terminal is what makes this work the same way everywhere. The
//! other way to find out what is running in a shell is to ask something that
//! watches the machine's whole process table from outside, and that is a worse
//! answer twice over: it is a round trip through another process to learn
//! something about a terminal this application is holding open, and the thing
//! doing the watching needs a process table it can read, which one of the two
//! platforms this has to run on does not have.
//!
//! # A name somebody chose is never taken away
//!
//! [`ShellName::set`] names a shell for good. Whatever is running in it is still
//! looked at and still remembered — so that [`ShellName::clear`] has something to
//! go back to — but it stops deciding what the shell is called. A person who
//! named a terminal `deploy` did so precisely because they did not want it
//! called `sh` again in half a second.
//!
//! # It is a poll, and it is a slow one
//!
//! Nothing anywhere tells a program that the process in front of a terminal
//! changed, so this is asked rather than received. [`ShellName::poll`] may be
//! called as often as a caller likes — every frame, if that is what a caller has
//! — and looks at most once per [`FOREGROUND_INTERVAL`], which is slower than
//! anybody can read and far faster than anybody would notice.

use std::time::{Duration, Instant};

use crate::terminal::Device;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;

/// How long a look at what is running lasts before it is worth taking another.
///
/// A shell changes what it is running when somebody types a command and when
/// that command finishes, and neither of those is a moment anybody is timing.
/// Three quarters of a second is under the threshold at which a name that is
/// out of date reads as wrong rather than as new, and it is two file reads per
/// shell per interval — affordable for every shell open, which is the number
/// that has to be affordable, because a shell nobody is looking at is still
/// running something worth naming.
pub const FOREGROUND_INTERVAL: Duration = Duration::from_millis(750);

/// A process id, signed the way the kernels this runs on report one.
///
/// The signedness is the platform's rather than a choice: `tcgetpgrp` answers
/// `-1` for a terminal with no foreground group, and keeping the type it
/// answers in means that value arrives here to be recognised rather than
/// wrapping into something enormous on the way.
pub type Pid = i32;

/// The process group in front of a terminal, and what to call it.
///
/// A group rather than a process because that is what a terminal has: every
/// stage of a pipeline is in one group, and the name is the group's — its
/// leader's — rather than any particular member's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    group: Pid,
    name: String,
}

impl Foreground {
    /// The group `group`, called `name`.
    pub fn new(group: Pid, name: impl Into<String>) -> Self {
        Self {
            group,
            name: name.into(),
        }
    }

    /// Which process group has the terminal.
    pub fn group(&self) -> Pid {
        self.group
    }

    /// What it is called.
    ///
    /// Short: both platforms answer with the executable's name and both truncate
    /// it — Linux at fifteen bytes, macOS at a little more — which is what a
    /// process is called in every other listing a person has ever read, and is
    /// what fits beside a terminal in a list.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Something that can say what is running in front of a terminal.
///
/// [`Kernel`] is the implementation that answers by asking this machine, and is
/// what a shell uses. The trait exists because that answer is arrived at
/// differently on every platform and because a caller — a test, most of all —
/// may want to say what is running rather than run it.
pub trait ForegroundProcess {
    /// The process group in front of `device`, and its name.
    ///
    /// `None` is an ordinary answer, not a failure: a terminal may have no
    /// foreground group at all, and a group may exit between being named and
    /// being asked about. Nothing is retried and nothing is raised — the next
    /// look is along shortly.
    fn foreground(&self, device: &Device) -> Option<Foreground>;
}

/// This machine, asked through its own kernel.
///
/// The one implementation that talks to a real terminal. What it does differs by
/// platform and nothing outside this module has to know how: a process group
/// comes from `tcgetpgrp` on every unix this is built for, and the name for that
/// group comes from the process table, which is files on Linux and a system call
/// on macOS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kernel;

impl ForegroundProcess for Kernel {
    fn foreground(&self, device: &Device) -> Option<Foreground> {
        platform(device)
    }
}

#[cfg(target_os = "linux")]
use linux::foreground as platform;
#[cfg(target_os = "macos")]
use macos::foreground as platform;
#[cfg(windows)]
use windows::foreground as platform;

/// Anywhere that is none of the three above.
///
/// Nothing is built for a fourth platform today, and this is what the fourth
/// would replace: one function, behind the same trait as the others, in a file
/// of its own beside them. Reaching this is a build for a platform whose arm was
/// never written, which is worth a panic that says so rather than a shell that
/// quietly never learns its own name.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform(device: &Device) -> Option<Foreground> {
    let _ = device;
    unimplemented!("what is in front of a terminal on this platform")
}

/// What one shell is called: what somebody named it, or failing that what is
/// running in it.
///
/// Both are kept, always. The automatic name goes on being looked at while a
/// chosen one is in force, so that giving the chosen one up shows what is
/// running now rather than what was running when it was set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellName {
    chosen: Option<String>,
    running: Option<Foreground>,
    looked: Option<Instant>,
}

impl ShellName {
    /// A shell nobody has named and nothing has looked at yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// What to call the shell, or nothing when nobody has named it and nothing
    /// could be found running in it.
    ///
    /// Nothing is invented here. A shell that has just started and has not been
    /// looked at yet, and one whose process has gone, both have no name, and
    /// what to show in their place is a decision for whatever is doing the
    /// showing.
    pub fn name(&self) -> Option<&str> {
        self.chosen
            .as_deref()
            .or_else(|| self.running.as_ref().map(Foreground::name))
    }

    /// The name somebody chose, if they did.
    pub fn chosen(&self) -> Option<&str> {
        self.chosen.as_deref()
    }

    /// Whether this shell is named by hand rather than after what is in it.
    pub fn is_chosen(&self) -> bool {
        self.chosen.is_some()
    }

    /// Names the shell, until somebody says otherwise.
    pub fn set(&mut self, name: impl Into<String>) {
        self.chosen = Some(name.into());
    }

    /// Gives the chosen name up, so the shell goes back to being called after
    /// whatever is running in it.
    ///
    /// The name that takes over is the one from the most recent look, which is
    /// at most [`FOREGROUND_INTERVAL`] old — not the one that was current when
    /// the chosen name was set.
    pub fn clear(&mut self) {
        self.chosen = None;
    }

    /// What was running in front of the terminal when it was last looked at.
    pub fn foreground(&self) -> Option<&Foreground> {
        self.running.as_ref()
    }

    /// Looks at what is running in `device`, unless that was done recently
    /// enough.
    ///
    /// Answers whether what the shell is *called* changed, which is what a
    /// caller redrawing a list wants to know: a shell named by hand answers
    /// `false` however much its foreground process moves around, and so does a
    /// look that was skipped for being too soon.
    pub fn poll(&mut self, device: &Device, processes: &impl ForegroundProcess) -> bool {
        let now = Instant::now();
        if self
            .looked
            .is_some_and(|last| now.duration_since(last) < FOREGROUND_INTERVAL)
        {
            return false;
        }
        self.looked = Some(now);

        let found = processes.foreground(device);
        let renamed =
            found.as_ref().map(Foreground::name) != self.running.as_ref().map(Foreground::name);
        self.running = found;
        renamed && self.chosen.is_none()
    }
}
