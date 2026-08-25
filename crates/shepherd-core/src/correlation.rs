//! The string a shell is known by outside this process.
//!
//! Every shell is started with its correlation string in its environment, and
//! everything descending from that shell inherits it. Anything watching those
//! processes from the outside reports the string back verbatim, having no idea
//! what is in it — which is the point. The encoding is private: it is written
//! here, read here, and understood nowhere else, so it can change shape without
//! anything outside this crate being taught about the change.
//!
//! A shell is identified by its workspace and its number within that workspace,
//! because shell numbering restarts per workspace. `w9:s3` is the fourth shell
//! handed out in workspace nine.
//!
//! # Why it is this short
//!
//! The string ends up inside filenames that have a hard length limit — a unix
//! socket path is 108 bytes including whatever else is in it — and a correlation
//! that pushes one of those over the edge does not fail loudly, it fails by
//! quietly not connecting. Two letters, two decimal numbers and a colon is
//! twenty-three bytes at its absolute longest and eight in any realistic case.

use thiserror::Error;

use crate::ids::{ShellId, WorkspaceId};

/// The letter that introduces the workspace number.
const WORKSPACE: char = 'w';
/// The letter that introduces the shell number.
const SHELL: char = 's';
/// What separates the two.
const SEPARATOR: char = ':';

/// The correlation string for one shell of one workspace.
pub fn correlation_for(workspace: WorkspaceId, shell: ShellId) -> String {
    format!(
        "{WORKSPACE}{}{SEPARATOR}{SHELL}{}",
        workspace.raw(),
        shell.raw()
    )
}

/// The workspace and shell a correlation string names.
///
/// This is strict: it accepts exactly what [`correlation_for`] produces and
/// nothing else. A string that arrives from outside may have been set by
/// somebody else entirely — the environment variable it travels in belongs to
/// the bus, not to this application, and anything may put anything in it — so
/// the useful answer for a string this application did not write is that it
/// names nothing here.
pub fn parse_correlation(correlation: &str) -> Result<(WorkspaceId, ShellId), CorrelationError> {
    let malformed = || CorrelationError::Malformed(correlation.to_owned());

    let body = correlation.strip_prefix(WORKSPACE).ok_or_else(malformed)?;
    let (workspace, rest) = body.split_once(SEPARATOR).ok_or_else(malformed)?;
    let shell = rest.strip_prefix(SHELL).ok_or_else(malformed)?;

    for number in [workspace, shell] {
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed());
        }
    }

    let out_of_range = || CorrelationError::OutOfRange(correlation.to_owned());
    let workspace = workspace.parse().map_err(|_| out_of_range())?;
    let shell = shell.parse().map_err(|_| out_of_range())?;

    Ok((WorkspaceId::from_raw(workspace), ShellId::from_raw(shell)))
}

/// Why a string is not one of this application's correlations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CorrelationError {
    /// Not shaped like a correlation at all.
    #[error("`{0}` is not shaped like `w<workspace>:s<shell>`")]
    Malformed(String),
    /// The right shape, but one of the numbers is larger than an id can be.
    #[error("`{0}` names a number too large to be an id")]
    OutOfRange(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_correlation_round_trips_across_the_whole_range_of_ids() {
        let interesting = [0, 1, 2, 9, 10, 1_000, u32::MAX - 1, u32::MAX];
        for workspace in interesting {
            for shell in interesting {
                let workspace = WorkspaceId::from_raw(workspace);
                let shell = ShellId::from_raw(shell);
                let correlation = correlation_for(workspace, shell);
                assert_eq!(
                    parse_correlation(&correlation),
                    Ok((workspace, shell)),
                    "{correlation} did not survive the round trip"
                );
            }
        }
    }

    #[test]
    fn the_encoding_is_the_documented_one() {
        assert_eq!(
            correlation_for(WorkspaceId::from_raw(9), ShellId::from_raw(3)),
            "w9:s3"
        );
        assert_eq!(correlation_for(WorkspaceId::FIRST, ShellId::FIRST), "w0:s0");
    }

    #[test]
    fn the_longest_correlation_stays_well_inside_a_socket_paths_budget() {
        let longest = correlation_for(WorkspaceId::LAST, ShellId::LAST);
        assert_eq!(longest, "w4294967295:s4294967295");
        assert_eq!(longest.len(), 23);
        assert!(longest.len() < 108);
    }

    #[test]
    fn anything_this_application_did_not_write_names_nothing() {
        for rubbish in [
            "",
            "w",
            "w1",
            "w1:",
            "w1:s",
            ":s1",
            "1:1",
            "W1:S1",
            "w1;s1",
            "wone:stwo",
            "w-1:s1",
            "w+1:s1",
            "w 1:s1",
            "w1:s1:s2",
            "w1:s1 ",
            "observed:w1:s1",
        ] {
            assert_eq!(
                parse_correlation(rubbish),
                Err(CorrelationError::Malformed(rubbish.to_owned())),
                "{rubbish} was read as a correlation"
            );
        }
    }

    #[test]
    fn a_number_too_large_for_an_id_is_reported_as_such() {
        let too_large = "w4294967296:s0";
        assert_eq!(
            parse_correlation(too_large),
            Err(CorrelationError::OutOfRange(too_large.to_owned()))
        );
    }

    #[test]
    fn the_reason_a_string_was_refused_reads_as_a_sentence() {
        assert_eq!(
            CorrelationError::Malformed("nonsense".to_owned()).to_string(),
            "`nonsense` is not shaped like `w<workspace>:s<shell>`"
        );
        assert_eq!(
            CorrelationError::OutOfRange("w99999999999:s0".to_owned()).to_string(),
            "`w99999999999:s0` names a number too large to be an id"
        );
    }
}
