//! The published wire format of the agent event bus: the event envelope, the
//! closed set of normalized event kinds, the fold that turns a stream of events
//! into the current status of each agent session, and the table of sessions a
//! snapshot is built from. Everything here is pure data and pure functions — no
//! sockets, no files, no clock — so the fold, which is the correctness-critical
//! part of the system, can be exercised exhaustively by unit tests. Emitters, the
//! daemon and subscribers all agree on this crate and nothing else, which is what
//! lets them be written, versioned and debugged independently.
//!
//! # The wire format
//!
//! JSON, one object per line, UTF-8, newline-terminated. Framing lines is the
//! daemon's job; this crate deals only in whole values.
//!
//! An emitter builds an [`UnstampedEvent`] and sends it. The daemon stamps it
//! with a sequence number and a timestamp, producing an [`Event`], folds it into
//! its [`SessionTable`], and publishes it. A subscriber reads [`StreamLine`]s: a
//! [`Snapshot`] first, so that it starts up rendering *current state* rather than
//! merely future events, then events, [`Heartbeat`]s and [`ForegroundChange`]s as
//! they happen.
//!
//! # Two rules that keep this stable
//!
//! **The core is small and closed, and `raw` is the escape hatch.** The
//! normalized fields cover what every agent has in common; anything else travels
//! verbatim in [`Event::raw`], which costs nothing to carry and stops a schema
//! gap from blocking a consumer. Resist the urge to promote a field that only one
//! agent can fill: an envelope that names things only one producer emits is a
//! dishonest envelope.
//!
//! **Readers ignore what they do not recognize.** Unknown top-level fields are
//! dropped on read; an unknown event kind deserializes to [`Kind::Unknown`] and
//! an unknown line kind to [`StreamLine::Unknown`], neither of which is an error.
//! A daemon can therefore add a kind without breaking every subscriber that was
//! built before it existed.

#![warn(missing_docs)]

pub mod event;
pub mod fold;
pub mod status;
pub mod stream;
pub mod table;
pub mod timestamp;

pub use event::{Agent, Event, Kind, OriginHop, SSH_CONNECTION_DETAIL, Source, UnstampedEvent};
pub use fold::{DEFAULT_STALE_AFTER, Fold, Input, SessionState, is_activity};
pub use status::SessionStatus;
pub use stream::{
    DaemonIdentity, ForegroundChange, ForegroundEntry, ForegroundState, Heartbeat, SessionEntry,
    Snapshot, StreamLine,
};
pub use table::{
    DEFAULT_DONE_RETENTION, OBSERVED_SESSION_PREFIX, OriginConflict, SessionKey, SessionTable,
    TrackedSession, is_observed_session_id, observed_session_id,
};
pub use timestamp::{Timestamp, TimestampError};

/// The version of the envelope this crate implements.
pub const VERSION: u32 = 1;
