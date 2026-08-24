//! The Shepherd binary.
//!
//! This is a skeleton: it understands `--version` and `--help`, both handled
//! by `clap` without any code of its own, and otherwise starts and exits
//! having done nothing. The window, the renderer and everything a person
//! actually runs this for arrive once there is a terminal core to show.

use clap::Parser;

/// The `shepherd` command line.
///
/// There are no subcommands yet, so parsing this only ever succeeds or
/// requests `--help`/`--version`; a bare invocation is a valid, empty parse.
#[derive(Debug, Parser)]
#[command(name = "shepherd", version)]
struct Cli;

fn main() {
    Cli::parse();
}
