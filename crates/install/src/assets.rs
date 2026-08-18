//! The hook and plugin templates that are written into the agents.
//!
//! Each is compiled in with `include_str!` from the crate's `assets` directory,
//! rather than read from disk at install time, because the same binary is copied
//! onto machines that have no checkout of this source and no directory to read
//! from. A template that could go missing between building and installing would
//! be a hook installation that fails on exactly the machines it is hardest to
//! debug on.
//!
//! One constant per agent, added by the code that installs for it.
