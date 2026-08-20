//! Working out whose manifest a screen should be read with.
//!
//! Detection answers "what is this agent doing"; it cannot start until someone
//! has answered "which agent". Often the caller already knows — it launched the
//! process, or a hook named the session on the way past — and a hint settles the
//! question outright. When it does not, the only evidence to hand is the
//! process: the name the kernel reports and the command line it was started
//! with.
//!
//! That evidence is matched against **data**, never against a table compiled in
//! here. Executable names go stale for the same reasons screens do — an agent
//! renames its binary, ships a second entry point, moves to a different
//! packaging — and a name list frozen into a release is a list that is wrong
//! until the next release. So each manifest carries its own `[identify]`
//! section, and this module knows only how to compare.
//!
//! Nothing here reads `/proc`, a process table, or anything else about the
//! machine. The caller supplies two strings it obtained however it likes; this
//! module is a pure function over them and a set of manifests.

use agentbus_protocol::Agent;

use crate::screen::schema::ScreenManifest;

/// What is known about the process a screen belongs to.
///
/// Both fields are raw, exactly as whatever produced them reported: no
/// canonicalization, no shell quoting rules, no path resolution. An empty
/// string means "not known", which is different from "known to be empty" only
/// in a way nothing here could act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessInfo<'a> {
    /// The short process name — Linux's `comm`, and its equivalent elsewhere.
    pub comm: &'a str,
    /// The full command line, arguments separated by spaces.
    pub cmdline: &'a str,
}

/// How many bytes of a process name Linux keeps.
///
/// `comm` is a fixed 16-byte field including its terminator, so a longer
/// executable name arrives cut off at 15 bytes with nothing to say it was cut.
/// A name that long is therefore compared by prefix rather than by equality.
const COMM_BYTES: usize = 15;

/// Extensions a script file carries that the agent's name does not.
///
/// An agent shipped as a JavaScript package is started as `node …/agent.js`;
/// what identifies it is `agent`, and the extension is packaging.
const SCRIPT_EXTENSIONS: [&str; 3] = [".js", ".mjs", ".cjs"];

/// Names the agent whose manifest should read this screen, if one can be named.
///
/// The hint is tried first and wins outright: a caller that knows which agent it
/// started knows better than any amount of inference. A hint that names no
/// manifest is not an error and not fatal — the caller may know about an agent
/// this corpus has never heard of — but since there is then no manifest to read
/// the screen with, the search simply continues with the process evidence.
///
/// After the hint come two passes over the process: the executable's own name,
/// then, for an agent that runs inside an interpreter, the name of the script
/// that interpreter was handed.
///
/// **Two manifests answering to the same evidence produce `None`**, at every
/// pass. Picking one of them would be a coin toss dressed up as an answer, and a
/// screen read against the wrong agent's manifest is worse than a screen not
/// read at all.
///
/// Comparison is ASCII-case-insensitive throughout, because the file systems
/// these names come off are not uniformly case-sensitive.
pub fn identify<'a>(
    manifests: impl IntoIterator<Item = &'a ScreenManifest>,
    hint: Option<&str>,
    process: Option<ProcessInfo<'_>>,
) -> Option<Agent> {
    let manifests: Vec<&ScreenManifest> = manifests.into_iter().collect();

    if let Some(hint) = hint.filter(|hint| !hint.trim().is_empty()) {
        match pick(&manifests, |manifest| answers_to(manifest, hint)) {
            Pick::One(manifest) => return agent_of(manifest),
            Pick::Ambiguous => return None,
            Pick::None => {}
        }
    }

    let process = process?;
    let argv0 = word(process.cmdline, 0).map(basename);
    let argv1 = word(process.cmdline, 1).map(basename);

    match pick(&manifests, |manifest| {
        named_by(manifest, process.comm, argv0)
    }) {
        Pick::One(manifest) => return agent_of(manifest),
        Pick::Ambiguous => return None,
        Pick::None => {}
    }

    match pick(&manifests, |manifest| wraps(manifest, argv0, argv1)) {
        Pick::One(manifest) => agent_of(manifest),
        Pick::Ambiguous | Pick::None => None,
    }
}

/// What one pass over the manifests concluded.
enum Pick<'a> {
    /// Nothing matched; the next pass may still find something.
    None,
    /// Exactly one manifest matched.
    One(&'a ScreenManifest),
    /// Several matched, which is no answer at all.
    Ambiguous,
}

/// Runs one pass, stopping as soon as a second match makes the answer moot.
fn pick<'a>(
    manifests: &[&'a ScreenManifest],
    mut matches: impl FnMut(&ScreenManifest) -> bool,
) -> Pick<'a> {
    let mut found = None;
    for manifest in manifests {
        if !matches(manifest) {
            continue;
        }
        if found.is_some() {
            return Pick::Ambiguous;
        }
        found = Some(*manifest);
    }
    match found {
        Some(manifest) => Pick::One(manifest),
        None => Pick::None,
    }
}

/// The agent id a manifest describes.
///
/// A manifest whose id is not a usable agent id identifies nothing: it could
/// not be put on a wire, logged or compared, so answering with it would only
/// move the failure somewhere less obvious.
fn agent_of(manifest: &ScreenManifest) -> Option<Agent> {
    Agent::new(manifest.id.trim()).ok()
}

/// Whether a manifest goes by this name — its id, or any alias.
///
/// Aliases are what people call the agent, so they are the right vocabulary for
/// a hint even when they could never be an executable name.
fn answers_to(manifest: &ScreenManifest, hint: &str) -> bool {
    same(&manifest.id, hint) || manifest.aliases.iter().any(|alias| same(alias, hint))
}

/// Whether the running executable is one this manifest claims.
///
/// Both the kernel's name for the process and the name it was invoked under are
/// tried: they disagree whenever the agent is launched through a symlink, a
/// shim, or a rename, and either one alone would miss cases the other catches.
fn named_by(manifest: &ScreenManifest, comm: &str, argv0: Option<&str>) -> bool {
    manifest
        .identify
        .names
        .iter()
        .any(|name| matches_comm(name, comm) || argv0.is_some_and(|argv0| same(name, argv0)))
}

/// Whether this manifest's agent is running inside an interpreter.
///
/// Looking exactly one argument deep is deliberate. `node …/agent.js` and
/// `bun …/agent` put the identity in the first argument; anything further in is
/// a flag, a payload or a guess, and a wrong guess here attributes a screen to
/// an agent that is not on it.
fn wraps(manifest: &ScreenManifest, argv0: Option<&str>, argv1: Option<&str>) -> bool {
    let (Some(argv0), Some(argv1)) = (argv0, argv1) else {
        return false;
    };
    let identify = &manifest.identify;
    if !identify.wrappers.iter().any(|wrapper| same(wrapper, argv0)) {
        return false;
    }
    let script = strip_script_extension(argv1);
    identify.names.iter().any(|name| same(name, script))
}

/// Whether a manifest name matches a process name, allowing for truncation.
///
/// A `comm` of exactly [`COMM_BYTES`] bytes may be a longer name with its tail
/// cut off, so such a value matches any name that begins with it. Shorter values
/// are compared whole: they arrived intact, and a prefix match on them would
/// claim `cli` is `cline`.
fn matches_comm(name: &str, comm: &str) -> bool {
    let comm = comm.trim();
    if comm.is_empty() {
        return false;
    }
    if same(name, comm) {
        return true;
    }
    comm.len() == COMM_BYTES
        && name
            .trim()
            .as_bytes()
            .get(..COMM_BYTES)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(comm.as_bytes()))
}

/// The word at `index` of a command line, arguments split on whitespace.
///
/// Quoting and escaping are not honoured, because the string this reads is
/// already a lossy join of an argument vector — the separators that would tell
/// them apart were gone before it arrived.
fn word(cmdline: &str, index: usize) -> Option<&str> {
    cmdline.split_whitespace().nth(index)
}

/// The final component of a path, for either separator.
///
/// Backslash counts as a separator so that a command line captured on Windows
/// reads correctly; a POSIX file name containing one is legal but not something
/// an agent's installer produces.
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A script's name without the extension that makes it a script.
fn strip_script_extension(name: &str) -> &str {
    let bytes = name.as_bytes();
    for extension in SCRIPT_EXTENSIONS {
        // A name that is nothing but an extension keeps it: stripping would
        // leave nothing to compare.
        if bytes.len() <= extension.len() {
            continue;
        }
        let split = bytes.len() - extension.len();
        // The extension is ASCII, so a suffix that matches it starts with the
        // ASCII '.' — which makes `split` a character boundary by construction.
        if bytes[split..].eq_ignore_ascii_case(extension.as_bytes()) {
            return &name[..split];
        }
    }
    name
}

/// Whether two names are the same name.
pub(crate) fn same(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::bundled::bundled_screen_manifests;

    /// The bundled corpus, parsed once per test that wants realistic data.
    fn corpus() -> Vec<ScreenManifest> {
        bundled_screen_manifests()
            .iter()
            .map(|(id, content)| {
                ScreenManifest::parse(content).unwrap_or_else(|error| panic!("{id}: {error}"))
            })
            .collect()
    }

    /// A minimal manifest, for the cases the corpus cannot express.
    fn manifest(id: &str, aliases: &[&str], names: &[&str], wrappers: &[&str]) -> ScreenManifest {
        fn list(values: &[&str]) -> String {
            let quoted: Vec<String> = values.iter().map(|value| format!("{value:?}")).collect();
            quoted.join(", ")
        }
        let content = format!(
            "id = {id:?}\n\
             aliases = [{}]\n\
             \n\
             [identify]\n\
             names = [{}]\n\
             wrappers = [{}]\n\
             \n\
             [[rules]]\n\
             id = \"only\"\n\
             state = \"idle\"\n\
             contains = [\"anything\"]\n",
            list(aliases),
            list(names),
            list(wrappers),
        );
        ScreenManifest::parse(&content).expect("test manifest is loadable")
    }

    /// The answer, as the id string, so that failures read as names.
    fn answer(
        manifests: &[ScreenManifest],
        hint: Option<&str>,
        comm: &str,
        cmdline: &str,
    ) -> Option<String> {
        let process = Some(ProcessInfo { comm, cmdline });
        identify(manifests, hint, process).map(|agent| agent.as_str().to_owned())
    }

    #[test]
    fn a_process_is_identified_by_its_name_or_its_argv0() {
        let corpus = corpus();
        let cases: &[(&str, &str, Option<&str>)] = &[
            // The plain case: comm alone names the agent.
            ("claude", "", Some("claude")),
            // An alias that is also an executable name.
            ("cursor-agent", "", Some("cursor")),
            // comm and argv0 disagree; argv0 carries the identity.
            ("droid", "/opt/factory/bin/droid --resume", Some("droid")),
            ("sh", "/usr/local/bin/opencode", Some("opencode")),
            // Neither says anything.
            ("bash", "bash -l", None),
            ("", "", None),
            // A name that merely contains an agent's name is not that agent.
            ("my-codex-helper", "", None),
            ("", "/tmp/claude-wrapper", None),
        ];
        for (comm, cmdline, expected) in cases {
            assert_eq!(
                answer(&corpus, None, comm, cmdline).as_deref(),
                *expected,
                "comm {comm:?}, cmdline {cmdline:?}",
            );
        }
    }

    #[test]
    fn a_process_name_cut_off_at_the_kernels_limit_still_matches() {
        let manifests = [manifest("verbose", &[], &["verbose-coding-agent"], &[])];
        // Exactly the 15 bytes Linux keeps of "verbose-coding-agent".
        assert_eq!(
            answer(&manifests, None, "verbose-coding-", ""),
            Some("verbose".to_owned()),
        );
        // A shorter value arrived intact, so it is not a truncated anything.
        assert_eq!(answer(&manifests, None, "verbose-codin", ""), None);
        // Fifteen bytes that are not a prefix of the name match nothing.
        assert_eq!(answer(&manifests, None, "verbose-codxng-", ""), None);
    }

    #[test]
    fn an_agent_running_inside_an_interpreter_is_found_through_it() {
        let corpus = corpus();
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("node", "node /usr/lib/node_modules/claude", Some("claude")),
            ("bun", "bun /home/user/.bun/bin/claude.js", Some("claude")),
            ("node", "node /opt/qwen/dist/qwen.mjs --yolo", Some("qwen")),
            // The interpreter is not one this manifest is shipped inside.
            ("node", "node /opt/bin/maki", None),
            // Only the first argument is looked at; a flag is not a name.
            ("node", "node -e /tmp/claude", None),
            // An interpreter with nothing after it identifies nothing.
            ("node", "node", None),
        ];
        for (comm, cmdline, expected) in cases {
            assert_eq!(
                answer(&corpus, None, comm, cmdline).as_deref(),
                *expected,
                "comm {comm:?}, cmdline {cmdline:?}",
            );
        }
    }

    #[test]
    fn the_direct_name_is_preferred_over_the_wrapped_one() {
        let manifests = [
            manifest("runner", &[], &["node"], &[]),
            manifest("wrapped", &[], &["wrapped"], &["node"]),
        ];
        // Both could answer: "node" is one manifest's own name and the other's
        // wrapper. The executable that is actually running wins.
        assert_eq!(
            answer(&manifests, None, "node", "node /x/wrapped"),
            Some("runner".to_owned()),
        );
    }

    #[test]
    fn evidence_two_manifests_answer_to_is_no_answer() {
        let manifests = [
            manifest("first", &["shared-alias"], &["shared"], &["node"]),
            manifest("second", &["shared-alias"], &["shared"], &["node"]),
        ];
        // Same name.
        assert_eq!(answer(&manifests, None, "shared", ""), None);
        // Same name, reached through the same wrapper.
        assert_eq!(answer(&manifests, None, "node", "node /x/shared.js"), None);
        // Same alias, offered as a hint.
        assert_eq!(answer(&manifests, Some("shared-alias"), "", ""), None);
        // An id is still unique, so it still answers.
        assert_eq!(
            answer(&manifests, Some("second"), "", ""),
            Some("second".to_owned()),
        );
    }

    #[test]
    fn a_hint_outranks_the_process() {
        let corpus = corpus();
        // The caller says codex; the process says claude. The caller wins.
        assert_eq!(
            answer(&corpus, Some("codex"), "claude", "node /x/claude.js"),
            Some("codex".to_owned()),
        );
        // A hint may be any name the agent goes by, including one no
        // executable could ever be called.
        assert_eq!(
            answer(&corpus, Some("claude-code"), "", ""),
            Some("claude".to_owned()),
        );
        assert_eq!(
            answer(&corpus, Some("kilo code"), "", ""),
            Some("kilo".to_owned()),
        );
        // Case is not part of a name.
        assert_eq!(
            answer(&corpus, Some("Claude-Code"), "", ""),
            Some("claude".to_owned()),
        );
    }

    #[test]
    fn a_hint_no_manifest_knows_falls_through_to_the_process() {
        let corpus = corpus();
        // The caller knows about an agent this corpus does not. There is no
        // manifest to read its screen with, so the process gets the last word.
        assert_eq!(
            answer(&corpus, Some("something-else"), "claude", ""),
            Some("claude".to_owned()),
        );
        assert_eq!(answer(&corpus, Some("something-else"), "bash", ""), None);
        // A blank hint is not a hint.
        assert_eq!(
            answer(&corpus, Some("   "), "claude", ""),
            Some("claude".to_owned()),
        );
    }

    #[test]
    fn nothing_to_go_on_is_answered_with_nothing() {
        let corpus = corpus();
        assert_eq!(identify(&corpus, None, None), None);
        assert!(identify(&corpus, Some("claude"), None).is_some());
        assert_eq!(identify(&corpus, Some(""), None), None);
        assert_eq!(answer(&corpus, None, "", ""), None);
        assert_eq!(
            identify(&[] as &[ScreenManifest], Some("claude"), None),
            None
        );
    }

    #[test]
    fn every_bundled_agent_can_be_identified_by_its_own_id() {
        let corpus = corpus();
        for manifest in &corpus {
            assert_eq!(
                answer(&corpus, None, &manifest.id, "").as_deref(),
                Some(manifest.id.as_str()),
                "the corpus cannot identify {:?} from its own id",
                manifest.id,
            );
        }
    }
}
