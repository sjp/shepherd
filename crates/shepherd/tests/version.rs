//! `shepherd --version` is compared byte-for-byte by anything that provisions
//! this binary onto another machine, so its output is pinned here, matching
//! the contract `agentbus --version` already holds itself to.
//!
//! Everything asserted here is what the command line does *before* anything is
//! started. What a bare invocation does — start a shell and open a window on it
//! — is not: it needs a display, it does not end on its own, and a test that ran
//! it would be a test of the machine it ran on.

use std::process::Command;

/// The exact bytes `--version` must produce, and nothing more.
const VERSION_LINE: &str = concat!("shepherd ", env!("CARGO_PKG_VERSION"), "\n");

#[test]
fn version_prints_exactly_the_contract_line() {
    let output = Command::new(env!("CARGO_BIN_EXE_shepherd"))
        .arg("--version")
        .output()
        .expect("failed to run shepherd");

    assert_eq!(output.stdout, VERSION_LINE.as_bytes());
    assert_eq!(output.stderr, b"");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn a_verbosity_nothing_understands_is_refused_before_anything_starts() {
    let output = Command::new(env!("CARGO_BIN_EXE_shepherd"))
        .arg("--log-level")
        .arg("=nonsense=")
        .output()
        .expect("failed to run shepherd");

    // Usage, on stderr, with the status a command line that does not make sense
    // is answered with — and nothing started, which is the point of validating
    // it here rather than when the log filter is built.
    assert_eq!(output.stdout, b"");
    assert!(!output.stderr.is_empty());
    assert_eq!(output.status.code(), Some(2));
}

/// The version `[workspace.package]` declares, read out of the manifest that
/// declares it.
///
/// Everything that provisions this binary onto another machine works from
/// that one number: it names the file that is copied, and the line above is
/// how the copy is recognised once it is there. The two are the same number
/// only while every crate takes its version from the workspace, so a crate
/// that started declaring its own would satisfy the test above — the binary
/// would agree with itself — and would still ship under a name nobody could
/// predict.
fn workspace_version() -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest.display()));

    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[workspace.package]";
        } else if inside {
            if let Some(rest) = line.strip_prefix("version") {
                let value = rest
                    .trim_start()
                    .strip_prefix('=')
                    .expect("the version key has no value")
                    .trim();
                return value.trim_matches('"').to_owned();
            }
        }
    }
    panic!("no version under [workspace.package]");
}

#[test]
fn the_version_printed_is_the_workspace_version() {
    assert_eq!(env!("CARGO_PKG_VERSION"), workspace_version());
}
