//! Declaring endpoints to attach to, and reporting what came of it.
//!
//! Two files answer between them: one holds what somebody asked for and outlives
//! every daemon, the other holds what a daemon is currently doing and disappears
//! with it. Neither command here needs a daemon to be running — declaring an
//! endpoint on a machine whose bus is not up is an ordinary thing to do, and the
//! daemon acts on it whenever it next starts — so all of this is files, and the
//! only thing a missing daemon changes is what there is to report.
//!
//! # Which transport a declaration names
//!
//! A declaration is a transport and the words that transport was given, and the
//! words are stored exactly as they were typed because the program that will
//! read them is the one entitled to interpret them. That leaves one thing to
//! decide on the command line: which of the two words is which. The rule is the
//! smallest one that covers what people write —
//!
//! ```text
//! agentbus attach -- -p 2222 bob@fs.example.net   reached over ssh
//! agentbus attach docker eager_mclean             a container, named
//! agentbus attach ssh -- fileserver               ssh, said out loud
//! ```
//!
//! — which is: a first word naming a transport names it, and anything else is
//! the arguments of the one that needs naming least. The cost is that a host
//! whose whole argument vector is the single word `docker` has to be declared
//! as `agentbus attach ssh -- docker`, and it is worth it for a command line
//! nobody has to read the manual for.

use std::fmt;

use agentbus_daemon::remote::attachments::{Entry, Sharing, State};
use agentbus_daemon::remote::targets::{DOCKER, SSH, Target};
use agentbus_protocol::Timestamp;
use serde::{Serialize, Serializer};
use thiserror::Error;

use crate::table::{self, ABSENT, Row};

/// The column headings, in order.
const HEADINGS: [&str; 7] = [
    "IDENTITY",
    "TRANSPORT",
    "ALIASES",
    "CONNECTION",
    "STATE",
    "SINCE",
    "ERROR",
];

/// What stands in the identity column for an endpoint that has been worked out
/// and not yet asked.
///
/// The words that reach a machine are not what is on it, so what is known before
/// anything has answered is a guess — good enough to have stopped a second
/// attachment being reported, and not good enough to print as though the far end
/// had confirmed it.
const PROVISIONAL: &str = "(provisional)";

/// What is printed when nothing has been declared and nothing is attached.
const NOTHING: &str = "no targets";

/// The shape `--json` is written in.
const SCHEMA: u32 = 1;

/// What separates one alias from the next where several are printed as one
/// cell.
const BETWEEN: &str = ", ";

/// What a command line declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration<'a> {
    /// The transport that will reach it.
    pub transport: &'a str,
    /// What that transport is to be given, verbatim.
    pub args: &'a [String],
}

impl Declaration<'_> {
    /// The declaration as it would be typed, for saying what was done.
    pub fn said(&self) -> String {
        match self.args.is_empty() {
            true => self.transport.to_owned(),
            false => format!("{} {}", self.transport, self.args.join(" ")),
        }
    }
}

/// Why a command line does not declare anything.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Problem {
    /// A transport was named and nothing was said about what to reach.
    #[error("{transport} needs something to reach: {expected}")]
    Nothing {
        /// The transport that was named.
        transport: String,
        /// What it should have been followed by.
        expected: &'static str,
    },
}

/// Reads a declaration off the words of a command line.
pub fn declared(words: &[String]) -> Result<Declaration<'_>, Problem> {
    let (transport, rest, expected) = match words.first().map(String::as_str) {
        Some(DOCKER) => (DOCKER, &words[1..], "a container"),
        Some(SSH) => (SSH, &words[1..], "the arguments to reach it with"),
        // Nothing names a transport, so these are the arguments of the one a
        // declaration means when it does not say.
        _ => (SSH, words, "the arguments to reach it with"),
    };
    // Whatever escaped the options of the command line itself is not one of the
    // words a transport was given.
    let args = match rest.first().map(String::as_str) {
        Some("--") => &rest[1..],
        _ => rest,
    };
    match args.is_empty() {
        true => Err(Problem::Nothing {
            transport: transport.to_owned(),
            expected,
        }),
        false => Ok(Declaration { transport, args }),
    }
}

/// What is being reported about one endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// What the daemon here is doing about it.
    Doing(State),
    /// There is a daemon here and it is not attached to this.
    NotAttached,
    /// There is no daemon here, so nothing is attached to anything.
    NoDaemon,
}

impl Shown {
    /// The word it is written as where a machine is reading.
    fn as_key(self) -> &'static str {
        match self {
            Self::Doing(State::Connecting) => "connecting",
            Self::Doing(State::Attached) => "attached",
            Self::Doing(State::Reconnecting) => "reconnecting",
            Self::Doing(State::NeedsAttention) => "needs_attention",
            Self::Doing(State::Detaching) => "detaching",
            Self::NotAttached => "not_attached",
            Self::NoDaemon => "daemon_not_running",
        }
    }

    /// Whether this is the row somebody has to do something about.
    fn wants_attention(self) -> bool {
        self == Self::Doing(State::NeedsAttention)
    }
}

impl fmt::Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Doing(state) => f.write_str(state.as_str()),
            Self::NotAttached => f.write_str("not attached"),
            Self::NoDaemon => f.write_str("daemon not running"),
        }
    }
}

impl Serialize for Shown {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_key())
    }
}

/// One endpoint, as both files together describe it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Known {
    /// Which transport reaches it.
    pub transport: String,
    /// What it turned out to be, once the far end said so.
    pub identity: Option<String>,
    /// The way in to it, as much as could be told without reaching it.
    pub way_in: Option<String>,
    /// Whether every set of words below reaches it the same way.
    pub sharing: Option<Sharing>,
    /// Every set of words known to reach it, in the order they were declared in.
    pub aliases: Vec<Vec<String>>,
    /// What the daemon calls it, where a daemon has met it.
    pub label: Option<String>,
    /// What is happening to it.
    pub state: Shown,
    /// How many attempts to reach it have failed since the last one worked.
    pub attempt: u32,
    /// What went wrong, where something did.
    pub last_error: Option<String>,
    /// When it entered this state.
    pub since: Option<Timestamp>,
    /// Whether a daemon found it rather than being told about it.
    pub auto: bool,
    /// Whether it is in the declarations.
    pub declared: bool,
    /// When it was declared.
    pub added: Option<Timestamp>,
}

/// Everything known about the endpoints of one machine, from the declarations
/// and from whatever the daemon there is doing about them.
///
/// A declaration and an attachment are matched on the words that were declared,
/// against both the words an attachment was started with and any others that
/// turned out to reach the same place — so several declarations that collapse
/// onto one endpoint are one row, listing all of them, rather than the same
/// attachment reported once per name. An attachment nothing declared is reported
/// too: a transport that finds its own endpoints has just as much right to be on
/// this list.
pub fn merge(declared: &[Target], attached: Option<&[Entry]>) -> Vec<Known> {
    let entries = attached.unwrap_or_default();
    let mut known = Vec::new();
    let mut shown: Vec<usize> = Vec::new();
    for target in declared {
        match entries.iter().position(|entry| covers(entry, target)) {
            // Already on the list under another of its names.
            Some(index) if shown.contains(&index) => {}
            Some(index) => {
                shown.push(index);
                known.push(from(&entries[index], Some(target)));
            }
            None => known.push(Known {
                transport: target.transport.clone(),
                identity: None,
                way_in: None,
                sharing: None,
                aliases: vec![target.args.clone()],
                label: None,
                state: match attached {
                    Some(_) => Shown::NotAttached,
                    None => Shown::NoDaemon,
                },
                attempt: 0,
                last_error: None,
                since: None,
                auto: false,
                declared: true,
                added: Some(target.added.clone()),
            }),
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        if !shown.contains(&index) {
            known.push(from(entry, None));
        }
    }
    known
}

/// Whether an attachment is what `target` declared.
fn covers(entry: &Entry, target: &Target) -> bool {
    entry.transport == target.transport
        && (entry.args == target.args || entry.aliases.contains(&target.args))
}

/// One endpoint a daemon has said something about.
fn from(entry: &Entry, declared: Option<&Target>) -> Known {
    Known {
        transport: entry.transport.clone(),
        identity: entry.identity.clone(),
        way_in: entry.way_in.clone(),
        sharing: entry.sharing,
        aliases: match entry.aliases.is_empty() {
            true => vec![entry.args.clone()],
            false => entry.aliases.clone(),
        },
        label: Some(entry.label.clone()),
        state: Shown::Doing(entry.state),
        attempt: entry.attempt,
        last_error: entry.last_error.clone(),
        since: Some(entry.since.clone()),
        auto: entry.auto,
        declared: declared.is_some(),
        added: declared.map(|target| target.added.clone()),
    }
}

/// The table for `known`, as of `now`, ending in a newline.
///
/// `styled` says whether the output is going somewhere escape sequences mean
/// something. They are used for one thing: an endpoint that is going nowhere
/// until a person does something about it is the row that person is looking for.
pub fn render(known: &[Known], now: &Timestamp, styled: bool) -> String {
    if known.is_empty() {
        return format!("{NOTHING}\n");
    }
    let rows: Vec<Row> = known.iter().map(|known| row(known, now)).collect();
    table::render(&HEADINGS, &rows, styled)
}

/// The whole of what is known, for something that is going to read it.
pub fn json(known: &[Known], daemon: bool) -> String {
    let mut written = serde_json::to_string(&serde_json::json!({
        "v": SCHEMA,
        "daemon": daemon,
        "targets": known,
    }))
    .unwrap_or_else(|_| String::from("{}"));
    written.push('\n');
    written
}

/// One endpoint's row, in the order of [`HEADINGS`].
fn row(known: &Known, now: &Timestamp) -> Row {
    Row::new(vec![
        identity(known),
        known.transport.clone(),
        aliases(&known.aliases),
        known
            .sharing
            .map_or_else(|| ABSENT.to_owned(), |sharing| sharing.to_string()),
        known.state.to_string(),
        known
            .since
            .as_ref()
            .map_or_else(|| ABSENT.to_owned(), |since| table::elapsed(now, since)),
        known
            .last_error
            .clone()
            .unwrap_or_else(|| ABSENT.to_owned()),
    ])
    .emphasized(known.state.wants_attention())
}

/// What an endpoint is, as one cell: what it said it is, that nothing has asked
/// it yet, or that nothing whatever is known.
fn identity(known: &Known) -> String {
    match (&known.identity, &known.way_in) {
        (Some(identity), _) => identity.clone(),
        (None, Some(_)) => PROVISIONAL.to_owned(),
        (None, None) => ABSENT.to_owned(),
    }
}

/// Every set of words that reaches one endpoint, as one cell.
fn aliases(aliases: &[Vec<String>]) -> String {
    aliases
        .iter()
        .map(|alias| alias.join(" "))
        .collect::<Vec<String>>()
        .join(BETWEEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        Timestamp::parse(text).expect("not a timestamp")
    }

    fn now() -> Timestamp {
        at("2026-08-17T10:05:00.000Z")
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn target(transport: &str, args: &[&str]) -> Target {
        Target::new(transport, &words(args), at("2026-08-17T10:00:00.000Z"))
    }

    fn entry(transport: &str, args: &[&str], state: State) -> Entry {
        Entry {
            transport: transport.to_owned(),
            args: words(args),
            identity: Some("9f3c:1000".to_owned()),
            way_in: Some("bob@fileserver:22".to_owned()),
            sharing: None,
            aliases: vec![words(args)],
            label: args.join(" "),
            state,
            attempt: 0,
            last_error: None,
            since: at("2026-08-17T10:04:00.000Z"),
            auto: false,
        }
    }

    /// What a command line declares, as the two things it settles.
    fn read(typed: &[&str]) -> (String, Vec<String>) {
        let typed = words(typed);
        let declaration = declared(&typed).expect("nothing was declared");
        (declaration.transport.to_owned(), declaration.args.to_vec())
    }

    #[test]
    fn everything_after_the_escape_is_the_arguments_of_the_transport_that_needs_no_naming() {
        assert_eq!(
            read(&["-p", "2222", "bob@fs.example.net"]),
            (SSH.to_owned(), words(&["-p", "2222", "bob@fs.example.net"]))
        );
    }

    #[test]
    fn a_first_word_naming_a_transport_names_it() {
        assert_eq!(
            read(&["docker", "eager_mclean"]),
            (DOCKER.to_owned(), words(&["eager_mclean"]))
        );
        assert_eq!(
            read(&["ssh", "fileserver"]),
            (SSH.to_owned(), words(&["fileserver"]))
        );
    }

    #[test]
    fn an_escape_the_command_line_left_behind_is_not_one_of_the_words() {
        assert_eq!(
            read(&["ssh", "--", "-p", "2222", "fileserver"]),
            (SSH.to_owned(), words(&["-p", "2222", "fileserver"]))
        );
    }

    #[test]
    fn a_transport_with_nothing_to_reach_is_refused() {
        for typed in [vec!["docker"], vec!["ssh", "--"], Vec::new()] {
            let typed = words(&typed);
            assert!(
                matches!(declared(&typed), Err(Problem::Nothing { .. })),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn a_declaration_and_what_the_daemon_did_about_it_are_one_row() {
        let known = merge(
            &[target(SSH, &["fileserver"])],
            Some(&[entry(SSH, &["fileserver"], State::Attached)]),
        );

        assert_eq!(known.len(), 1);
        assert_eq!(known[0].state, Shown::Doing(State::Attached));
        assert!(known[0].declared);
        assert_eq!(known[0].identity.as_deref(), Some("9f3c:1000"));
        assert_eq!(known[0].added, Some(at("2026-08-17T10:00:00.000Z")));
    }

    #[test]
    fn a_declaration_no_daemon_has_seen_says_which_of_the_two_reasons_it_is() {
        let declared = [target(SSH, &["fileserver"])];

        let without = merge(&declared, None);
        let with = merge(&declared, Some(&[]));

        assert_eq!(without[0].state, Shown::NoDaemon);
        assert_eq!(with[0].state, Shown::NotAttached);
        // And either way it is reported, rather than the whole command failing.
        assert_eq!(without[0].aliases, vec![words(&["fileserver"])]);
    }

    #[test]
    fn several_names_for_one_endpoint_are_one_row_listing_all_of_them() {
        let mut attached = entry(SSH, &["fileserver"], State::Attached);
        attached.aliases = vec![words(&["fileserver"]), words(&["192.168.0.4"])];
        attached.sharing = Some(Sharing::Shared);

        let known = merge(
            &[target(SSH, &["fileserver"]), target(SSH, &["192.168.0.4"])],
            Some(&[attached]),
        );

        assert_eq!(known.len(), 1);
        assert_eq!(
            known[0].aliases,
            vec![words(&["fileserver"]), words(&["192.168.0.4"])]
        );
        // Both names, what it turned out to be, and that reaching it by either
        // of them costs one connection rather than two.
        let table = render(&known, &now(), false);
        assert!(table.contains("fileserver, 192.168.0.4"), "{table}");
        assert!(table.contains("9f3c:1000"), "{table}");
        assert!(table.contains("shared"), "{table}");
    }

    #[test]
    fn an_endpoint_nothing_has_answered_for_yet_is_not_reported_as_though_it_had() {
        let mut connecting = entry(SSH, &["fileserver"], State::Connecting);
        connecting.identity = None;

        let table = render(&merge(&[], Some(&[connecting.clone()])), &now(), false);

        // What is known is where ssh would go, which is not what is there.
        assert!(table.contains("(provisional)"), "{table}");
        // And a target no daemon has met at all knows even less than that.
        let nothing = render(&merge(&[target(SSH, &["fileserver"])], None), &now(), false);
        assert!(!nothing.contains("(provisional)"), "{nothing}");
        // The guess is still written down for anything reading this properly.
        let written: serde_json::Value =
            serde_json::from_str(&json(&merge(&[], Some(&[connecting])), true)).expect("not json");
        assert_eq!(written["targets"][0]["identity"], serde_json::Value::Null);
        assert_eq!(written["targets"][0]["way_in"], "bob@fileserver:22");
    }

    #[test]
    fn an_attachment_nobody_declared_is_reported_too() {
        let mut found = entry(DOCKER, &["eager_mclean"], State::Attached);
        found.auto = true;

        let known = merge(&[], Some(&[found]));

        assert_eq!(known.len(), 1);
        assert!(known[0].auto);
        assert!(!known[0].declared);
    }

    #[test]
    fn every_column_is_headed_and_filled() {
        let mut refused = entry(SSH, &["fileserver"], State::NeedsAttention);
        refused.last_error = Some("permission denied".to_owned());
        refused.sharing = Some(Sharing::Separate);

        let table = render(&merge(&[], Some(&[refused])), &now(), false);

        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines[0].split_whitespace().collect::<Vec<&str>>(), HEADINGS);
        assert!(lines[1].contains("9f3c:1000"), "{table}");
        assert!(lines[1].contains("fileserver"), "{table}");
        assert!(lines[1].contains("separate"), "{table}");
        assert!(lines[1].contains("needs attention"), "{table}");
        assert!(lines[1].contains("1m0s"), "{table}");
        assert!(lines[1].contains("permission denied"), "{table}");
    }

    #[test]
    fn a_target_that_wants_attention_is_the_row_that_stands_out() {
        let refused = entry(SSH, &["fileserver"], State::NeedsAttention);
        let working = entry(SSH, &["other"], State::Attached);

        let table = render(&merge(&[], Some(&[refused, working])), &now(), true);

        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[1].starts_with('\x1b'), "{table}");
        assert!(!lines[2].contains('\x1b'), "{table}");
    }

    #[test]
    fn nothing_declared_and_nothing_attached_says_so() {
        assert_eq!(render(&[], &now(), false), "no targets\n");
    }

    #[test]
    fn what_is_written_for_a_machine_carries_the_state_as_one_word() {
        let known = merge(&[target(SSH, &["fileserver"])], None);

        let written: serde_json::Value =
            serde_json::from_str(&json(&known, false)).expect("not json");

        assert_eq!(written["v"], 1);
        assert_eq!(written["daemon"], false);
        assert_eq!(written["targets"][0]["state"], "daemon_not_running");
        assert_eq!(written["targets"][0]["aliases"][0][0], "fileserver");
        assert_eq!(written["targets"][0]["declared"], true);
    }
}
