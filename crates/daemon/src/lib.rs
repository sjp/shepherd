//! The bus itself: the per-user socket directory, ingest of emitted events,
//! the session table and the ring buffer of recent events, fan-out to
//! subscribers, monitoring of the foreground process behind each correlation,
//! and attachment to daemons running on other endpoints so their events merge
//! into the local stream. This crate owns all the state and all the I/O; the
//! command-line front end only starts it and talks to it over the sockets.
