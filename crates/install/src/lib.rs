//! Detection of the coding agents installed on a machine and installation of
//! the hooks that make them emit events onto the bus. Each supported agent has
//! its own configuration surface — a plugin directory, a settings file, a
//! plugin script — so this crate keeps the per-agent knowledge in one place and
//! presents a uniform install/uninstall interface over it. Every operation is
//! idempotent and exactly reversible: installing twice changes nothing the
//! second time, and uninstalling leaves no trace behind.
