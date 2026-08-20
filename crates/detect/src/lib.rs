//! What a coding agent is doing, read from what it says and from what it draws.
//!
//! Hooks are the honest signal: an agent that reports its own lifecycle needs no
//! guessing. Reading one is a matter of knowing that harness's vocabulary —
//! which field of the payload names the event, what it calls the start of a tool
//! call — and [`hooks`] turns a payload into a normalized event on that basis.
//!
//! But not every agent has hooks, not every hook fires, and the one state that
//! matters most — an agent stopped, waiting for a person who has not been told —
//! is exactly the one a missing hook loses. So [`screen`] covers the same ground
//! from the other side. Give it the text on the screen and it answers with the
//! state that text is evidence of, along with what it matched and why.
//!
//! # Data, not code
//!
//! Nothing here knows what any particular agent looks like or says. Each agent
//! is described by TOML **manifests**: screen rules that name a state, a
//! priority, a region of the screen to examine and the matchers that have to
//! hold; and a hook mapping that names the fields a payload keeps its identity
//! in and what each of its event names means. The engine knows how to match; the
//! manifests know what to look for. That split is the whole point — agent
//! interfaces change on their own schedule, and a change answered by shipping a
//! file is a change that can be answered in an afternoon by someone who does not
//! build this software.
//!
//! A corpus of manifests ships inside the library, so it is useful on a machine
//! where nothing has been installed. Newer copies may sit on disk beside it, and
//! [`ManifestStore`] is what decides which copy of an agent's manifest is the
//! one in force — a copy its operator wrote first, a fetched copy that is not
//! older than the bundled one next, and the bundled copy as the floor. It does
//! that once per family, by the same rules, because "which copy is active" is
//! not a question that changes with what the file describes.
//!
//! # Who this is for
//!
//! Any program holding a payload or a screen: a client that a harness runs its
//! hooks through, a host with a live grid of its own, a script piping a captured
//! screen through a command, a supervisor reading a log. The library has no
//! daemon, no sockets and no ambitions about what its caller is; it reads what
//! it is given and answers.
//!
//! # What a verdict is worth
//!
//! A normalized hook event is the agent's own account of itself, and is worth
//! exactly what the agent's report is worth. A screen verdict is a guess, and it
//! says so: an unrecognized screen is reported as calm rather than alarming — a
//! false "blocked" trains people to ignore the signal, which costs more than the
//! occasional missed one. A consumer holding both should prefer the account over
//! the guess, and should show a screen-derived verdict as what it is.

#![warn(missing_docs)]

pub mod explain;
pub mod hooks;
pub mod identify;
pub mod screen;
pub mod store;
pub mod version;

pub use explain::{
    EvaluatedRule, Evidence, Explain, GateCounts, ManifestSource, MatchedRule, MatcherCounts,
    PREVIEW_CHARS, explain,
};
pub use hooks::schema::{
    Condition, EventMapping, HOOKS_ENGINE_VERSION, HookFault, HookManifest, HookManifestError,
    PayloadFields, Projection, Transform,
};
pub use hooks::{CompiledHookManifest, normalize};
pub use identify::{ProcessInfo, identify};
pub use screen::detect;
pub use screen::region::{DEFAULT_REGION, RegionSpec, ScreenInput, UnknownRegion};
pub use screen::rules::{
    CompiledManifest, Detection, KNOWN_AGENT_IDLE_FALLBACK, RuleVerdict, UNKNOWN_AGENT_FALLBACK,
    Verdicts,
};
pub use screen::schema::{
    Gate, GateView, Identify, ManifestFault, Rule, SCREEN_ENGINE_VERSION, ScreenManifest,
    ScreenManifestError, ScreenState,
};
pub use store::{
    Family, Hooks, MAX_MANIFEST_BYTES, ManifestStore, ManifestSummary, Screen, StorePaths,
};
pub use version::{InvalidVersion, ManifestVersion};
