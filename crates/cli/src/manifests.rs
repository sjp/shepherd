//! Reporting on the manifests this machine detects with, and fetching newer
//! ones.
//!
//! Detection is data, and data that can be overridden, cached and refetched
//! raises a question the engine itself cannot answer: *which copy is answering,
//! and why that one?* Three copies of the same agent's manifest can sit on one
//! machine — the one inside the binary, one fetched from a catalog, one its
//! operator wrote — and when detection behaves unexpectedly, the first thing
//! worth knowing is which of them is in force.
//!
//! So this is the management surface for those files. Listing says which copy
//! answers for each agent, what the others declare, what was passed over on the
//! way there, and when a check last looked for something newer. Updating runs
//! that check now. Showing prints the copy in force exactly as it is written,
//! which is how an override starts life: take what is answering, redirect it
//! into the override directory, edit one rule.
//!
//! # What belongs on stdout
//!
//! A table and a manifest are both what the caller asked for, so both go to
//! stdout. Everything about *where* an answer came from — the tier, the file,
//! what was shadowed — is commentary on the answer rather than the answer, so
//! it goes to stderr, and `agentbus manifests show claude > claude.toml` writes
//! a file that is byte-for-byte the copy that was in force.

use agentbus_daemon::clock;
use agentbus_detect::{
    CheckResult, Family as _, Hooks, ItemResult, ManifestSource, ManifestStatus, ManifestSummary,
    Screen, Status, StorePaths, UpdateOutcome,
};
use agentbus_protocol::Timestamp;
use clap::ValueEnum;
use serde::Serialize;

use crate::table::{self, ABSENT, Row};

/// The column headings of the list, in order.
const HEADINGS: [&str; 9] = [
    "FAMILY", "ID", "ACTIVE", "BUNDLED", "REMOTE", "OVERRIDE", "CHECKED", "RESULT", "NOTES",
];

/// The column headings of one check's report, in order.
const OUTCOME_HEADINGS: [&str; 6] = ["FAMILY", "ID", "RESULT", "CACHED", "PUBLISHED", "ERROR"];

/// What is said when no check has ever run on this machine.
const UNCHECKED: &str = "no check for newer manifests has run here";

/// What is said when there are no manifests at all, which takes a build with an
/// empty corpus.
const NOTHING: &str = "no manifests";

/// What is said when a catalog was read and lists nothing this build can use.
const NOTHING_PUBLISHED: &str = "the catalog lists no manifests this build understands";

/// The shape `--json` is written in.
const SCHEMA: u32 = 1;

/// What separates one note from the next where several share a cell.
const BETWEEN: &str = "; ";

/// Which kind of manifest a command is about.
///
/// Both kinds are per agent and both are overridable, so every command here
/// takes one, and the one that is meant when nothing is said is the screen
/// rules: they are what changes when an agent redraws its UI, which is the
/// change an operator ends up answering themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Family {
    /// What the agent's terminal looks like in each of its states.
    #[default]
    Screen,
    /// What the agent's hook payloads mean.
    Hooks,
}

impl Family {
    /// The name the store and the files on disk use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Screen => Screen::NAME,
            Self::Hooks => Hooks::NAME,
        }
    }
}

/// One manifest as the list reports it.
#[derive(Debug, Serialize)]
pub struct Listed<'a> {
    /// Which copy answers for this agent, and what the others declare.
    #[serde(flatten)]
    manifest: &'a ManifestSummary,
    /// What the last check that covered it did, when one has.
    checked: Option<Checked>,
}

/// What a check recorded about one manifest, as it is reported.
///
/// The record on disk counts seconds since the epoch, because the library that
/// writes it has no calendar; a report is read by people and by programs that
/// already read this bus's timestamps, so the count becomes one of those here.
#[derive(Debug, Serialize)]
struct Checked {
    /// When the check ran.
    at: Option<Timestamp>,
    /// `updated`, `current` or `failed`.
    result: String,
    /// Why it failed, when it did.
    error: Option<String>,
    /// The version this machine held when the check ran.
    cached_version: Option<String>,
    /// The version the catalog offered.
    attempted_version: Option<String>,
}

impl From<&ManifestStatus> for Checked {
    fn from(status: &ManifestStatus) -> Self {
        Self {
            at: status.last_checked_unix.map(at),
            result: status.last_result.clone(),
            error: status.last_error.clone(),
            cached_version: status.cached_version.clone(),
            attempted_version: status.attempted_version.clone(),
        }
    }
}

/// What one manifest a check acted on is reported as.
#[derive(Debug, Serialize)]
struct Acted<'a> {
    family: &'a str,
    id: &'a str,
    /// Where it was fetched from.
    url: &'a str,
    /// `updated`, `current` or `failed`.
    result: &'a str,
    /// Why it failed, when it did.
    error: Option<&'a str>,
    /// The version this machine held.
    cached_version: Option<String>,
    /// The version the catalog offered.
    attempted_version: Option<String>,
}

/// Every manifest the store knows of, with whatever the last check said about
/// it.
pub fn list<'a>(summaries: &'a [ManifestSummary], status: &Status) -> Vec<Listed<'a>> {
    summaries
        .iter()
        .map(|manifest| Listed {
            manifest,
            checked: status
                .record(manifest.family, &manifest.id)
                .map(Checked::from),
        })
        .collect()
}

/// The list as a table, as of `now`, ending in a newline.
///
/// `styled` says whether the output is going somewhere escape sequences mean
/// something. They are used for one thing: a manifest something was passed over
/// to reach is the row whoever ran this is looking for.
pub fn render(listed: &[Listed<'_>], status: &Status, now: &Timestamp, styled: bool) -> String {
    let mut text = match (status.last_checked_unix, &status.last_result) {
        (Some(unix), Some(result)) => format!(
            "last checked {} ago: {result}\n",
            table::elapsed(now, &at(unix))
        ),
        _ => format!("{UNCHECKED}\n"),
    };
    if listed.is_empty() {
        text.push_str(NOTHING);
        text.push('\n');
        return text;
    }
    let rows: Vec<Row> = listed.iter().map(|listed| row(listed, now)).collect();
    text.push_str(&table::render(&HEADINGS, &rows, styled));
    text
}

/// The whole of the list, for something that is going to read it.
pub fn json(listed: &[Listed<'_>], status: &Status) -> String {
    let check = status.last_checked_unix.map(|unix| {
        serde_json::json!({
            "at": at(unix),
            "result": status.last_result,
        })
    });
    written(&serde_json::json!({
        "v": SCHEMA,
        "check": check,
        "manifests": listed,
    }))
}

/// What one check did, as a table, ending in a newline.
///
/// A check that could not read its catalog prints nothing: it has nothing to
/// report but the reason, and the reason belongs on stderr with everything else
/// that explains rather than answers.
pub fn outcome(outcome: &UpdateOutcome, styled: bool) -> String {
    if let CheckResult::Failed(_) = outcome.result {
        return String::new();
    }
    let mut text = format!("checked {}\n", outcome.catalog_url);
    if outcome.manifests.is_empty() {
        text.push_str(NOTHING_PUBLISHED);
        text.push('\n');
        return text;
    }
    let rows: Vec<Row> = outcome
        .manifests
        .iter()
        .map(|manifest| {
            Row::new(vec![
                manifest.family.to_owned(),
                manifest.id.clone(),
                label(&manifest.result).to_owned(),
                version(manifest.cached_version.as_ref().map(ToString::to_string)),
                version(manifest.attempted_version.as_ref().map(ToString::to_string)),
                error(&manifest.result).unwrap_or(ABSENT).to_owned(),
            ])
            .emphasized(matches!(manifest.result, ItemResult::Updated))
        })
        .collect();
    text.push_str(&table::render(&OUTCOME_HEADINGS, &rows, styled));
    text
}

/// The whole of what one check did, for something that is going to read it.
pub fn outcome_json(outcome: &UpdateOutcome) -> String {
    let acted: Vec<Acted<'_>> = outcome
        .manifests
        .iter()
        .map(|manifest| Acted {
            family: manifest.family,
            id: &manifest.id,
            url: &manifest.url,
            result: label(&manifest.result),
            error: error(&manifest.result),
            cached_version: manifest.cached_version.as_ref().map(ToString::to_string),
            attempted_version: manifest.attempted_version.as_ref().map(ToString::to_string),
        })
        .collect();
    written(&serde_json::json!({
        "v": SCHEMA,
        "catalog": outcome.catalog_url,
        "checked_at": at(outcome.checked_at),
        "result": match outcome.result {
            CheckResult::Checked => "checked",
            CheckResult::Failed(_) => "failed",
        },
        "error": match &outcome.result {
            CheckResult::Checked => None,
            CheckResult::Failed(reason) => Some(reason),
        },
        "manifests": acted,
    }))
}

/// Where the copy in force came from, as a sentence naming the file wherever
/// there is one.
///
/// The path matters more than the tier does: somebody who has just been told
/// which copy is answering is usually about to open it, and a tier is only the
/// name of a directory they would then have to work out.
pub fn describe(source: &ManifestSource, paths: &StorePaths, family: Family, id: &str) -> String {
    match source {
        ManifestSource::Bundled => "the copy inside this binary".to_owned(),
        ManifestSource::Remote { version } => match paths.remote_file(family.name(), id) {
            Some(path) => format!("the fetched copy at {}, version {version}", path.display()),
            None => format!("the fetched copy, version {version}"),
        },
        ManifestSource::Override { path } => format!("the override at {}", path.display()),
    }
}

/// One manifest's row, in the order of [`HEADINGS`].
fn row(listed: &Listed<'_>, now: &Timestamp) -> Row {
    let manifest = listed.manifest;
    let checked = listed.checked.as_ref();
    Row::new(vec![
        manifest.family.to_owned(),
        manifest.id.clone(),
        active(manifest.source.as_ref()).to_owned(),
        version(manifest.bundled_version.as_ref().map(ToString::to_string)),
        version(manifest.remote_version.as_ref().map(ToString::to_string)),
        version(manifest.override_version.as_ref().map(ToString::to_string)),
        checked
            .and_then(|checked| checked.at.as_ref())
            .map_or_else(|| ABSENT.to_owned(), |at| table::elapsed(now, at)),
        checked
            .map_or(ABSENT, |checked| checked.result.as_str())
            .to_owned(),
        notes(listed),
    ])
    .emphasized(!manifest.warnings.is_empty())
}

/// Which copy answers, as one cell.
fn active(source: Option<&ManifestSource>) -> &'static str {
    match source {
        Some(ManifestSource::Bundled) => "bundled",
        Some(ManifestSource::Remote { .. }) => "remote",
        Some(ManifestSource::Override { .. }) => "override",
        None => ABSENT,
    }
}

/// A version, or that there is no copy of that kind to have one.
fn version(version: Option<String>) -> String {
    version.unwrap_or_else(|| ABSENT.to_owned())
}

/// Everything about one manifest that somebody has to read a sentence of: what
/// was passed over to reach the copy in force, and why the last check turned
/// down what it was offered.
fn notes(listed: &Listed<'_>) -> String {
    let mut notes = listed.manifest.warnings.clone();
    notes.extend(
        listed
            .checked
            .as_ref()
            .and_then(|checked| checked.error.clone()),
    );
    match notes.is_empty() {
        true => ABSENT.to_owned(),
        false => notes.join(BETWEEN),
    }
}

/// What a check decided about one manifest, as one word.
fn label(result: &ItemResult) -> &'static str {
    match result {
        ItemResult::Updated => "updated",
        ItemResult::Current => "current",
        ItemResult::Failed(_) => "failed",
    }
}

/// Why it decided that, where the decision was a refusal.
fn error(result: &ItemResult) -> Option<&str> {
    match result {
        ItemResult::Failed(reason) => Some(reason.as_str()),
        _ => None,
    }
}

/// The instant a count of seconds since the epoch names.
fn at(unix: u64) -> Timestamp {
    clock::from_unix_millis(
        i64::try_from(unix)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000),
    )
}

/// A JSON document as one line, ending in a newline.
fn written(value: &serde_json::Value) -> String {
    let mut written = serde_json::to_string(value).unwrap_or_else(|_| String::from("{}"));
    written.push('\n');
    written
}
