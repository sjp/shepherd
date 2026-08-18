//! `agentbus --version` is compared byte-for-byte by anything that provisions
//! this binary onto another machine, so its output is pinned here.

use std::process::Command;

/// The exact bytes `--version` must produce, and nothing more.
const VERSION_LINE: &str = concat!("agentbus ", env!("CARGO_PKG_VERSION"), "\n");

#[test]
fn version_prints_exactly_the_contract_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .arg("--version")
        .output()
        .expect("failed to run agentbus");

    assert_eq!(output.stdout, VERSION_LINE.as_bytes());
    assert_eq!(output.stderr, b"");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn bare_invocation_writes_usage_to_stderr_and_fails() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentbus"))
        .output()
        .expect("failed to run agentbus");

    assert_eq!(output.stdout, b"");
    assert!(!output.stderr.is_empty());
    assert!(!output.status.success());
}
