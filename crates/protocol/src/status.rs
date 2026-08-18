//! The status of a session, as a subscriber sees it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What an agent session is currently doing.
///
/// These six values are the whole vocabulary a receiver has to render, and they
/// are deliberately coarse: they answer "does this need me?" and nothing else.
/// The distinction between [`SessionStatus::Stale`] and [`SessionStatus::Done`]
/// is worth the extra state — an agent that was killed and an agent that is hung
/// look identical without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The session has begun but has not done anything yet.
    Starting,
    /// The agent is doing something.
    Working,
    /// The agent is waiting on a human.
    Blocked,
    /// The agent has finished its turn and is waiting for the user.
    Idle,
    /// The agent was working and has since gone quiet for longer than expected.
    Stale,
    /// The session is over.
    Done,
}

impl SessionStatus {
    /// Every status, in the order they are documented.
    pub const ALL: [Self; 6] = [
        Self::Starting,
        Self::Working,
        Self::Blocked,
        Self::Idle,
        Self::Stale,
        Self::Done,
    ];

    /// The status as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Idle => "idle",
            Self::Stale => "stale",
            Self::Done => "done",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_status_has_its_documented_wire_string() {
        let expected = ["starting", "working", "blocked", "idle", "stale", "done"];
        assert_eq!(SessionStatus::ALL.len(), expected.len());
        for (status, wire) in SessionStatus::ALL.into_iter().zip(expected) {
            assert_eq!(status.as_str(), wire);
            assert_eq!(status.to_string(), wire);
            assert_eq!(serde_json::to_value(status).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<SessionStatus>(json!(wire)).unwrap(),
                status
            );
        }
    }
}
