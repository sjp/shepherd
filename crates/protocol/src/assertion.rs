//! What an observer claims, as opposed to what an agent reports.
//!
//! An [`Event`](crate::event::Event) is edge-triggered: something happened, once,
//! and the fold moves. Something watching from the outside knows a different kind
//! of fact — the current *level*: "as of now this session is blocked, and I can
//! see it". Making such a watcher synthesize transition events would be a lie
//! about what it knows: it cannot tell a state that just began from one that has
//! been true for a minute, so every repeat observation would look like a fresh
//! transition and be counted as one. A [`StateAssertion`] says the level instead,
//! and carries how confident it is in [`StateAssertion::visible`].
//!
//! The emit socket therefore takes two line shapes, and [`parse_emit_line`] is
//! what tells them apart.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::VERSION;
use crate::event::{Agent, UnstampedEvent};
use crate::timestamp::Timestamp;

/// A state an observer can claim.
///
/// Closed, and deliberately smaller than [`SessionStatus`](crate::SessionStatus):
/// these are the levels something watching from outside can honestly distinguish.
/// `starting`, `stale` and `done` are the fold's own conclusions about a session's
/// history and are not an observer's to claim.
///
/// A value outside this set fails to parse, which drops the one line carrying it.
/// That is the right trade here and the opposite of [`Kind`](crate::Kind)'s: an
/// unknown kind is carried so that a subscriber can pass on a stream it does not
/// fully understand, whereas an assertion exists only to be acted on, and one
/// asserting a level nobody can act on is worth nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertedState {
    /// The agent is doing something.
    Working,
    /// The agent is waiting for the user.
    Idle,
    /// The agent is waiting on a human — the state this bus exists to report.
    Blocked,
    /// The observer no longer knows. See [`AssertedState::is_withdrawal`].
    Unknown,
}

impl AssertedState {
    /// Every state, in the order they are documented.
    pub const ALL: [Self; 4] = [Self::Working, Self::Idle, Self::Blocked, Self::Unknown];

    /// The state as it appears on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this asserts nothing and takes back whatever was asserted before.
    ///
    /// "I no longer know" is a claim worth making — an observer that has lost
    /// sight of a terminal, or is looking at something it cannot read, must be
    /// able to say so rather than leave its last confident answer standing.
    pub fn is_withdrawal(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

impl std::fmt::Display for AssertedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A level-triggered claim about what is happening in one correlated slot.
///
/// Read it as "the state *is* X", not "X just happened". Three properties follow
/// from that and every producer and consumer depends on them:
///
/// - **Idempotent.** Re-asserting the same state changes nothing at all, not even
///   when the state began. An observer that sends the same claim ten times a
///   second is saying one thing, not ten.
/// - **Re-asserted while it holds.** A claim's power to override anything decays
///   with age, so an observer that can still see a state should say so again
///   every second or two. Silence is not a withdrawal: an assertion that goes
///   quiet loses its influence, but it never flips a session's state by expiring.
///   Only [`AssertedState::Unknown`] withdraws.
/// - **A floor, not an authority.** An agent's own hooks outrank this, because
///   the agent knows and the observer is guessing. What the guess is worth is
///   [`StateAssertion::visible`].
///
/// [`StateAssertion::correlation`] is required, because an assertion is *about*
/// something and the correlation is the only thing that says what: a claim
/// nobody can attribute is not a weak claim, it is not a claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateAssertion {
    /// Envelope version.
    pub v: u32,
    /// The claimed level.
    pub assert: AssertedState,
    /// Whether the chrome that says so is live on the observed surface *right
    /// now*, as opposed to having been inferred from something older.
    ///
    /// This is confidence, and it is the field that decides what a claim can
    /// override: something a receiver can see this instant is worth more than
    /// something it concluded a while ago. Absent means false.
    #[serde(default, skip_serializing_if = "is_not_visible")]
    pub visible: bool,
    /// The agent the claim is about. [`Agent::UNKNOWN`] is honest and allowed:
    /// an observer often knows the state before it knows whose it is.
    pub agent: Agent,
    /// The opaque string identifying what was observed. Copied from the
    /// observing environment and never parsed here; see
    /// [`Event::correlation`](crate::Event::correlation).
    pub correlation: String,
    /// The agent's own session id, where the observer knows it. Where it does
    /// not, a receiver keys the claim on the correlation instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The working directory, if the observer knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Small normalized extras, such as which rule concluded this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Map<String, Value>>,
    /// The evidence, verbatim: what the observer saw that made it say this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

/// Whether `visible` is the default, which is omitted from the wire.
fn is_not_visible(visible: &bool) -> bool {
    !*visible
}

impl StateAssertion {
    /// Builds the smallest legal assertion: what is being claimed, whose it is,
    /// and what it is about. Everything else is added with the `with_*` methods.
    pub fn new(agent: Agent, correlation: impl Into<String>, assert: AssertedState) -> Self {
        Self {
            v: VERSION,
            assert,
            visible: false,
            agent,
            correlation: correlation.into(),
            session: None,
            cwd: None,
            detail: None,
            raw: None,
        }
    }

    /// Marks the claim as one the observer can see the evidence for as it sends
    /// it. See [`StateAssertion::visible`].
    #[must_use]
    pub fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }

    /// Names the agent's own session id.
    #[must_use]
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the normalized extras.
    #[must_use]
    pub fn with_detail(mut self, detail: Map<String, Value>) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Sets one normalized extra, creating the map if needed.
    #[must_use]
    pub fn with_detail_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.detail
            .get_or_insert_with(Map::new)
            .insert(key.into(), value.into());
        self
    }

    /// Carries the evidence verbatim.
    #[must_use]
    pub fn with_raw(mut self, raw: impl Into<Value>) -> Self {
        self.raw = Some(raw.into());
        self
    }

    /// Stamps the assertion for republication to subscribers, dropping the
    /// evidence. See [`StampedAssertion`].
    pub fn stamp(self, seq: u64, ts: Timestamp) -> StampedAssertion {
        StampedAssertion {
            v: self.v,
            seq,
            ts,
            assert: self.assert,
            visible: self.visible,
            agent: self.agent,
            correlation: self.correlation,
            session: self.session,
            cwd: self.cwd,
            detail: self.detail,
        }
    }
}

/// An assertion as a subscriber reads it: stamped, and without the evidence.
///
/// `raw` is dropped on the way through. Evidence is however big whatever produced
/// it felt like making it — a screenful of text is ordinary — and every
/// subscriber would pay for it on every re-assertion, which is to say a couple of
/// times a second per observed slot. `detail` survives, so a subscriber still
/// learns *why* without being sent the whole of what was seen; anything that
/// genuinely needs the evidence is better off running the observer itself.
///
/// This is a line kind of its own rather than a thirteenth [`Kind`](crate::Kind),
/// for the same reason
/// [`ForegroundChange`](crate::ForegroundChange) is: it is scoped to a
/// correlation, and the status fold is per session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename = "assertion")]
pub struct StampedAssertion {
    /// Envelope version.
    pub v: u32,
    /// The daemon's sequence number for this line.
    pub seq: u64,
    /// When the daemon received the assertion.
    pub ts: Timestamp,
    /// The claimed level.
    pub assert: AssertedState,
    /// Whether the evidence was live when the claim was made. Absent means
    /// false, as on the emit side.
    #[serde(default, skip_serializing_if = "is_not_visible")]
    pub visible: bool,
    /// The agent the claim is about.
    pub agent: Agent,
    /// The opaque string identifying what was observed.
    pub correlation: String,
    /// The agent's own session id, where the observer knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The working directory, if the observer knew one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Small normalized extras, such as which rule concluded this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Map<String, Value>>,
}

/// One line as the emit socket receives it.
///
/// The two shapes are told apart by which field is present — an event has `kind`,
/// an assertion has `assert` — rather than by a tag either would have to carry.
/// That is what makes the addition safe with no coordination at all: a daemon
/// built before assertions existed reads one, fails to find a `kind`, and drops
/// a single line. Dropping one line is exactly the right degradation when a new
/// emitter meets an old daemon across a version boundary, and it is what the
/// alternative — a tag that would have made every line unparseable to that
/// daemon — could not have given.
#[derive(Debug, Clone, PartialEq)]
pub enum EmitLine {
    /// Something happened.
    Event(UnstampedEvent),
    /// Something is the case.
    Assertion(StateAssertion),
}

/// Reads one emit line, or nothing if it is not one.
///
/// Nothing is the answer for anything ambiguous or unusable: a line carrying
/// both discriminating fields or neither, one whose shape does not match the one
/// it claims, and one that is not JSON at all. There is no partial acceptance
/// here because there is nobody to tell — the emit protocol is fire and forget,
/// so a caller's only options are to act on a line or to drop it, and acting on
/// a line whose meaning is unclear is the worse of the two.
pub fn parse_emit_line(bytes: &[u8]) -> Option<EmitLine> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let object = value.as_object()?;
    let line = match (object.contains_key("kind"), object.contains_key("assert")) {
        (true, false) => EmitLine::Event(serde_json::from_value(value).ok()?),
        (false, true) => EmitLine::Assertion(serde_json::from_value(value).ok()?),
        _ => return None,
    };
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;
    use serde_json::json;

    /// Builds an agent id from a literal, which is what every one of these is.
    fn agent(name: &str) -> Agent {
        Agent::new(name).expect("a test's own agent id is a valid one")
    }

    /// The assertion as it is documented, with every field an observer can fill.
    const ASSERTION: &str = r#"{
      "v": 1,
      "assert": "blocked",
      "visible": true,
      "agent": "claude",
      "correlation": "w9:p3",
      "session": "abc123",
      "cwd": "/x",
      "detail": { "rule": "bash_permission_prompt" },
      "raw": { "matched": "Do you want to proceed?" }
    }"#;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-08-17T10:32:01.412Z").unwrap()
    }

    fn documented() -> StateAssertion {
        StateAssertion::new(agent("claude"), "w9:p3", AssertedState::Blocked)
            .with_visible(true)
            .with_session("abc123")
            .with_cwd("/x")
            .with_detail_field("rule", "bash_permission_prompt")
            .with_raw(json!({"matched": "Do you want to proceed?"}))
    }

    #[test]
    fn the_documented_assertion_round_trips() {
        let parsed: StateAssertion = serde_json::from_str(ASSERTION).unwrap();
        assert_eq!(parsed, documented());
        assert_eq!(parsed.assert, AssertedState::Blocked);
        assert!(parsed.visible);
        assert_eq!(parsed.correlation, "w9:p3");

        let expected: Value = serde_json::from_str(ASSERTION).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), expected);
    }

    #[test]
    fn the_smallest_assertion_carries_only_what_is_required() {
        let minimal = StateAssertion::new(Agent::unknown(), "w9:p3", AssertedState::Working);
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            json!({"v": 1, "assert": "working", "agent": "unknown", "correlation": "w9:p3"})
        );
        assert_eq!(
            serde_json::from_str::<StateAssertion>(
                r#"{"v":1,"assert":"working","agent":"unknown","correlation":"w9:p3"}"#
            )
            .unwrap(),
            minimal
        );
        assert!(!minimal.visible);
    }

    #[test]
    fn an_assertion_that_can_be_seen_says_so_and_one_that_cannot_stays_silent() {
        let seen = StateAssertion::new(agent("claude"), "w9:p3", AssertedState::Blocked)
            .with_visible(true);
        assert_eq!(serde_json::to_value(&seen).unwrap()["visible"], json!(true));

        let unseen = seen.clone().with_visible(false);
        let written = serde_json::to_value(&unseen).unwrap();
        assert_eq!(written.get("visible"), None, "{written}");
        assert_eq!(
            serde_json::from_value::<StateAssertion>(written).unwrap(),
            unseen
        );
    }

    #[test]
    fn unknown_is_the_one_state_that_takes_back_what_was_said() {
        for state in AssertedState::ALL {
            assert_eq!(state.is_withdrawal(), state == AssertedState::Unknown);
        }
    }

    #[test]
    fn every_asserted_state_has_its_documented_wire_string() {
        let expected = ["working", "idle", "blocked", "unknown"];
        assert_eq!(AssertedState::ALL.len(), expected.len());
        for (state, wire) in AssertedState::ALL.into_iter().zip(expected) {
            assert_eq!(state.as_str(), wire);
            assert_eq!(state.to_string(), wire);
            assert_eq!(serde_json::to_value(state).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<AssertedState>(json!(wire)).unwrap(),
                state
            );
        }
    }

    #[test]
    fn a_state_outside_the_set_is_refused_rather_than_carried() {
        assert!(serde_json::from_value::<AssertedState>(json!("starting")).is_err());
        assert!(serde_json::from_value::<AssertedState>(json!("invented_later")).is_err());
    }

    #[test]
    fn stamping_publishes_the_reasoning_and_drops_the_evidence() {
        let stamped = documented().stamp(1041, ts());
        assert_eq!(
            serde_json::to_value(&stamped).unwrap(),
            json!({
                "kind": "assertion", "v": 1, "seq": 1041,
                "ts": "2026-08-17T10:32:01.412Z",
                "assert": "blocked", "visible": true, "agent": "claude",
                "correlation": "w9:p3", "session": "abc123", "cwd": "/x",
                "detail": {"rule": "bash_permission_prompt"}
            })
        );
        assert_eq!(
            serde_json::from_value::<StampedAssertion>(serde_json::to_value(&stamped).unwrap())
                .unwrap(),
            stamped
        );
    }

    #[test]
    fn a_stamped_minimal_assertion_omits_everything_it_was_not_told() {
        let stamped =
            StateAssertion::new(Agent::unknown(), "w9:p3", AssertedState::Unknown).stamp(7, ts());
        assert_eq!(
            serde_json::to_value(&stamped).unwrap(),
            json!({
                "kind": "assertion", "v": 1, "seq": 7,
                "ts": "2026-08-17T10:32:01.412Z",
                "assert": "unknown", "agent": "unknown", "correlation": "w9:p3"
            })
        );
    }

    #[test]
    fn an_emit_line_is_told_apart_by_which_field_it_carries() {
        let event = br#"{"v":1,"agent":"claude","session":"abc123","kind":"turn_end"}"#;
        let Some(EmitLine::Event(parsed)) = parse_emit_line(event) else {
            panic!("that is an event");
        };
        assert_eq!(parsed.kind, Kind::TurnEnd);
        assert_eq!(parsed.agent, agent("claude"));

        let Some(EmitLine::Assertion(parsed)) = parse_emit_line(ASSERTION.as_bytes()) else {
            panic!("that is an assertion");
        };
        assert_eq!(parsed, documented());
    }

    #[test]
    fn a_line_that_is_both_or_neither_is_no_line_at_all() {
        // Both: there is no reading of this that is not a guess.
        assert_eq!(
            parse_emit_line(
                br#"{"v":1,"agent":"claude","session":"s","kind":"turn_end",
                     "assert":"blocked","correlation":"w9:p3"}"#
            ),
            None
        );
        // Neither.
        assert_eq!(
            parse_emit_line(br#"{"v":1,"agent":"claude","session":"s"}"#),
            None
        );
        // Not an object, and not JSON.
        assert_eq!(parse_emit_line(b"[1,2,3]"), None);
        assert_eq!(parse_emit_line(b"\"kind\""), None);
        assert_eq!(parse_emit_line(b"not json at all"), None);
        assert_eq!(parse_emit_line(b""), None);
    }

    #[test]
    fn a_line_whose_shape_does_not_match_its_discriminator_is_dropped() {
        // An assertion is nothing without something to attribute it to.
        assert_eq!(
            parse_emit_line(br#"{"v":1,"assert":"blocked","agent":"claude"}"#),
            None
        );
        // A state nobody can act on.
        assert_eq!(
            parse_emit_line(
                br#"{"v":1,"assert":"almost_blocked","agent":"claude","correlation":"w9:p3"}"#
            ),
            None
        );
        // An id that could not be compared or displayed, on either shape.
        assert_eq!(
            parse_emit_line(br#"{"v":1,"assert":"blocked","agent":"a b","correlation":"w9:p3"}"#),
            None
        );
        assert_eq!(
            parse_emit_line(br#"{"v":1,"agent":"a b","session":"s","kind":"turn_end"}"#),
            None
        );
        // An event missing the identity every event is keyed on.
        assert_eq!(parse_emit_line(br#"{"v":1,"kind":"turn_end"}"#), None);
    }

    #[test]
    fn an_unknown_field_on_an_assertion_is_ignored() {
        let Some(EmitLine::Assertion(parsed)) = parse_emit_line(
            br#"{"v":1,"assert":"idle","agent":"claude","correlation":"w9:p3",
                 "invented_later":{"a":1}}"#,
        ) else {
            panic!("that is an assertion");
        };
        assert_eq!(parsed.assert, AssertedState::Idle);
    }
}
