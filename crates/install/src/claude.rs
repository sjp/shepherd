//! Installing for Claude Code.
//!
//! Two things go on the machine. A wrapper script is dropped into a `hooks`
//! directory beside Claude's own configuration, and one entry is added to
//! `settings.json` telling Claude to run it when a session starts. The wrapper
//! is where the generation mark lives, so a machine can be asked what it is
//! carrying; the entry is what makes the wrapper ever run.
//!
//! The entry goes into a file somebody keeps by hand, so it is edited through
//! [`crate::cst`] rather than parsed and written back: their comments are not
//! allowed there by Claude's own reader, but their key order, their indentation
//! and their line endings are theirs, and an installer that reformatted the
//! file would be handing them a diff of the whole thing where they asked for
//! four lines.
//!
//! The one event hooked is the start of a session, because what this program
//! needs from Claude at install time is the identity of the session that is
//! running: which session it is, and where its transcript is. What every other
//! event *means* is decided from the payload the wrapper forwards, and adding
//! more of them to somebody's settings file is a cost paid on every session for
//! events nothing is waiting for.
//!
//! # What was here before
//!
//! Earlier builds installed for Claude by generating a marketplace of one
//! plugin below this program's data directory and asking Claude's own command
//! line to add the marketplace and install the plugin from it. That mechanism
//! is retired, and a machine still carrying it is cleaned up on the way past —
//! by both directions, because a user who installs again and a user who gives
//! up and uninstalls are equally entitled to be rid of it. Nothing is asked of
//! Claude's command line unless something of that installation is actually
//! there, so the ordinary install on an ordinary machine runs no subprocess at
//! all.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::change::Change;
use crate::command::{self, Invocation};
use crate::cst::CstDocument;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, json, sentinel};

/// The directory inside Claude's configuration that the wrapper is dropped
/// into.
///
/// Claude does not require hooks to live anywhere in particular — the entry
/// names the script by an absolute path — but a directory of that name is where
/// a user would look for one, and putting it there keeps a generated file out of
/// the top of a directory they read.
const HOOKS_DIR: &str = "hooks";

/// The file Claude keeps this user's own settings in, and the one the entry
/// goes into.
///
/// The user's, rather than a project's: a project's settings are checked into
/// version control and shared with everyone who clones the repository, and
/// installing one person's local tooling into it would be installing it for
/// their colleagues too.
const SETTINGS: &str = "settings.json";

/// Where in the settings the entry goes.
const HOOKS_KEY: &str = "hooks";

/// The event the wrapper is run on.
const EVENT: &str = "SessionStart";

/// What the entry matches, which is everything.
///
/// The event carries no tool name to match on, so the pattern is the one that
/// says so rather than an omission a reader would have to interpret.
const MATCHER: &str = "*";

/// How long Claude is asked to allow the wrapper, in seconds.
///
/// Far longer than the wrapper can take, and there so that a machine which has
/// somehow made it slow cannot make somebody's session slow with it.
const TIMEOUT: u64 = 5;

/// What the wrapper says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// What the plugin, and the marketplace that offered it, were both called.
const RETIRED_NAME: &str = "agentbus";

/// The directory the retired marketplace was generated into, below the data
/// directory.
const RETIRED_DIR: &str = "claude-marketplace";

/// The scope the retired registration was made at.
const RETIRED_SCOPE: &str = "user";

/// Claude Code's hooks: a wrapper script, and one entry in the settings file
/// that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claude;

impl Installer for Claude {
    fn agent(&self) -> Agent {
        Agent::Claude
    }

    /// Clears away anything an earlier build left, writes the wrapper, then
    /// points the settings at it.
    ///
    /// In that order. The entry names a file, and a settings file that named one
    /// which is not there yet would be a session's worth of hooks that do
    /// nothing — a window that is short, avoidable, and hard to explain
    /// afterwards.
    fn plan_install(
        &self,
        env: &Environment,
        state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        let mut changes = plan_retirement(env, state);
        // Writing a file makes the directories above it anyway; these are here
        // so that the making is recorded, which is the one thing the uninstall
        // cannot work out for itself later. Both, because an agent found by its
        // command alone may have no configuration directory yet, and one this
        // program made is one it has to be able to take away again.
        for dir in [Agent::Claude.config_dir(env), hooks_dir(env)] {
            if !dir.is_dir() {
                changes.push(Change::Make { path: dir });
            }
        }
        changes.push(plan_wrapper(env, binary)?);
        changes.push(plan_entry(env)?);
        Ok(changes)
    }

    /// Takes the entry out, then the wrapper, and clears away anything an
    /// earlier build left as well.
    ///
    /// In that order for the same reason reversed: the entry is what runs the
    /// file, so it goes first and nothing is ever pointed at something that is
    /// no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = plan_retirement(env, state);
        changes.push(plan_entry_removal(env, state)?);

        let path = wrapper(env);
        // A file without the mark is passed over here rather than refused,
        // unlike on the way in: an uninstall that stopped because something of
        // the user's is in the way would be refusing to do the one thing it can
        // certainly do safely, which is nothing.
        changes.push(match read(&path)? {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        });
        // Deepest first, and only the directories the record says this program
        // made: one the user already had is theirs however empty this leaves it.
        for dir in [hooks_dir(env), Agent::Claude.config_dir(env)] {
            if state.ownership(&dir) == Some(Ownership::Created) {
                changes.push(Change::Clear { path: dir });
            }
        }
        Ok(changes)
    }

    /// Reads the wrapper, and then looks for the entry that runs it.
    ///
    /// The wrapper is the file that says which generation this is, because it is
    /// the one whose content is the installation. A wrapper nothing calls is a
    /// working file in an installation that never runs, which is exactly the
    /// case worth telling somebody about — and so is an entry left over from an
    /// installation whose wrapper has since moved, because it names the path it
    /// was written with and not wherever one is now.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let path = self.asset(env);
        let Some(text) = read(&path)? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_pointed_at(env)?))
    }

    /// The wrapper this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        wrapper(env)
    }
}

/// The directory the wrapper is dropped into.
fn hooks_dir(env: &Environment) -> PathBuf {
    Agent::Claude.config_dir(env).join(HOOKS_DIR)
}

/// The wrapper this program drops in there.
fn wrapper(env: &Environment) -> PathBuf {
    hooks_dir(env).join(match env.platform() {
        Platform::Unix => "agentbus.sh",
        Platform::Windows => "agentbus.ps1",
    })
}

/// The settings file the entry goes into.
fn settings(env: &Environment) -> PathBuf {
    Agent::Claude.config_dir(env).join(SETTINGS)
}

/// What the wrapper should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    let named = match platform {
        Platform::Unix => command::in_shell(binary)?,
        Platform::Windows => command::in_powershell(binary)?,
    };
    Ok(assets::CLAUDE_WRAPPER
        .text(platform)
        .replace(BINARY_MARK, &named))
}

/// What writing the wrapper would do, given whatever is at its path now.
///
/// A file of that name this program did not write stops the plan. It is the one
/// case where doing nothing is worse than saying so: silently leaving it would
/// report a successful installation of hooks that are not installed, and
/// silently replacing it would delete something a user wrote.
fn plan_wrapper(env: &Environment, binary: &Path) -> Result<Change, Error> {
    let path = wrapper(env);
    let contents = generated(env.platform(), binary)?;
    Ok(match read(&path)? {
        None => Change::Create {
            path,
            contents,
            executable: true,
        },
        Some(text) if !sentinel::is_generated(&text) => return Err(Error::NotOurs { path }),
        Some(text) if text == contents => Change::Keep { path },
        Some(_) => Change::Rewrite {
            path,
            contents,
            executable: true,
        },
    })
}

/// The entry that tells Claude to run the wrapper, marked as this program's.
///
/// The mark goes in first so that a user opening their settings file reads who
/// wrote this before they read what it does.
fn entry(env: &Environment) -> Result<Value, Error> {
    let mut hook = Map::new();
    hook.insert("type".to_owned(), Value::from("command"));
    hook.insert(
        "command".to_owned(),
        Value::from(command::hook_command(
            Agent::Claude,
            env.platform(),
            &wrapper(env),
            None,
        )?),
    );
    // Claude runs an asynchronous hook without waiting for it, which is what
    // this one wants: nothing here has anything to say back, and a session that
    // paused for it would be paying for an event it is not waiting on.
    hook.insert("async".to_owned(), Value::Bool(true));
    hook.insert("timeout".to_owned(), Value::from(TIMEOUT));

    let mut entry = Map::new();
    sentinel::mark(&mut entry);
    entry.insert("matcher".to_owned(), Value::from(MATCHER));
    entry.insert("hooks".to_owned(), Value::Array(vec![Value::Object(hook)]));
    Ok(Value::Object(entry))
}

/// What putting the entry into the settings file would do.
///
/// A file that is not there is written from nothing, which is the case worth
/// having: no merge, no user content, nothing that can go wrong. A file that is
/// there is edited in place, and this program's own previous entries come out as
/// the new one goes in — so an upgrade replaces what an older build wrote rather
/// than accumulating beside it.
fn plan_entry(env: &Environment) -> Result<Change, Error> {
    let path = settings(env);
    let ours = entry(env)?;

    let Some(text) = read(&path)? else {
        let mut hooks = Map::new();
        hooks.insert(EVENT.to_owned(), Value::Array(vec![ours]));
        let mut document = Map::new();
        document.insert(HOOKS_KEY.to_owned(), Value::Object(hooks));
        return Ok(Change::Create {
            path,
            contents: json::render(&Value::Object(document), json::DEFAULT_INDENT),
            executable: false,
        });
    };

    let mut document = CstDocument::parse_strict(&path, &text)?;
    // Asked first, because taking the entry out and putting the same one back
    // would move the containers it lives in to the end of the objects that hold
    // them — a change to somebody's file made by a run with nothing to do.
    if !holds(&document, &ours) {
        document.retain(&[HOOKS_KEY, EVENT], |value| !sentinel::is_marked(value))?;
        document.append(&[HOOKS_KEY, EVENT], ours)?;
    }
    let contents = document.render();
    Ok(match contents == text {
        true => Change::Keep { path },
        false => Change::Rewrite {
            path,
            contents,
            executable: false,
        },
    })
}

/// What taking this program's entries back out of the settings file would do.
///
/// A file holding no mark at all is left alone without being parsed. That is not
/// a shortcut around the strictness: a file with nothing of this program's in it
/// is a file an uninstall has nothing to do to, and refusing to finish an
/// uninstall over a settings file this program never wrote into would be
/// refusing to remove everything else on the machine as well.
fn plan_entry_removal(env: &Environment, state: &State) -> Result<Change, Error> {
    let path = settings(env);
    let Some(text) = read(&path)? else {
        return Ok(Change::Keep { path });
    };
    if !text.contains(sentinel::KEY) {
        return Ok(Change::Keep { path });
    }

    let mut document = CstDocument::parse_strict(&path, &text)?;
    document.retain(&[HOOKS_KEY, EVENT], |value| !sentinel::is_marked(value))?;
    let contents = document.render();
    if contents == text {
        return Ok(Change::Keep { path });
    }
    // A file this program created and has just emptied is litter. One it merely
    // added to is the user's, however little of it is left.
    let ours = state.ownership(&path) == Some(Ownership::Created);
    if ours && document.get(&[]).is_some_and(sentinel::is_vacant) {
        return Ok(Change::Delete { path });
    }
    Ok(Change::Rewrite {
        path,
        contents,
        executable: false,
    })
}

/// Whether the settings already hold exactly this program's entry and no other
/// of its own.
fn holds(document: &CstDocument, ours: &Value) -> bool {
    let Some(Value::Array(entries)) = document.get(&[HOOKS_KEY, EVENT]) else {
        return false;
    };
    let mut marked = entries.iter().filter(|entry| sentinel::is_marked(entry));
    marked.next() == Some(ours) && marked.next().is_none()
}

/// Whether the settings on this machine run the wrapper this program would
/// install now.
///
/// A settings file that cannot be read as this program reads it counts as not
/// pointing anywhere. It may well be Claude's to understand, but it is not one
/// an install could add to either, so an installation resting on it is one that
/// needs a person.
fn is_pointed_at(env: &Environment) -> Result<bool, Error> {
    let path = settings(env);
    let Some(text) = read(&path)? else {
        return Ok(false);
    };
    let Ok(document) = CstDocument::parse_strict(&path, &text) else {
        return Ok(false);
    };
    Ok(holds(&document, &entry(env)?))
}

/// What clearing away an installation made the way an earlier build made them
/// would do, and nothing at all when there is no sign of one.
///
/// The signs are the directory that build generated into and this program's own
/// record of the files it put there. The record is looked at as well as the
/// directory because it is the only thing that still names a file somebody has
/// deleted by hand — and a record naming files that no longer exist is what
/// makes the *next* uninstall unable to say it left nothing behind.
///
/// Claude's own command line is asked about the plugin only once something has
/// been found, so an install on a machine that never had one runs no other
/// program. What it says is not required: a Claude that cannot be run, or that
/// answers with something this does not understand, means the registration is
/// left where it is and the generated files go anyway — which is the outcome a
/// user can finish by hand, and the one they get for free if they ever run
/// `claude plugin uninstall` themselves.
fn plan_retirement(env: &Environment, state: &State) -> Vec<Change> {
    let root = env.data_dir().join(RETIRED_DIR);
    let mut generated = state.recorded_below(&root);
    for path in generated_paths(&root) {
        if path.exists() && !generated.contains(&path) {
            generated.push(path);
        }
    }
    generated.sort();
    if generated.is_empty() && !root.exists() {
        return Vec::new();
    }

    let tool = tool(env);
    let forget = plugin(&tool, ["uninstall", &id(), "-s", RETIRED_SCOPE]);
    let remove = plugin(&tool, ["marketplace", "remove", RETIRED_NAME]);
    let mut changes = vec![
        match is_registered(&tool) {
            true => Change::Run { command: forget },
            false => Change::Ran { command: forget },
        },
        match knows_marketplace(&tool, &root) {
            true => Change::Run { command: remove },
            false => Change::Ran { command: remove },
        },
    ];
    changes.extend(generated.into_iter().map(|path| Change::Delete { path }));
    changes.push(Change::Clear { path: root });
    changes
}

/// Every file the retired marketplace was made of, in the layout that build
/// wrote them in.
fn generated_paths(root: &Path) -> [PathBuf; 3] {
    let plugin = root.join(RETIRED_NAME);
    [
        root.join(".claude-plugin").join("marketplace.json"),
        plugin.join(".claude-plugin").join("plugin.json"),
        plugin.join("hooks").join("hooks.json"),
    ]
}

/// Whether Claude still has the retired plugin installed.
fn is_registered(tool: &Path) -> bool {
    let Some(answer) = plugin(tool, ["list", "--json"]).ask() else {
        return false;
    };
    let Ok(Value::Array(plugins)) = serde_json::from_str::<Value>(&answer) else {
        return false;
    };
    plugins
        .iter()
        .any(|plugin| plugin.get("id").and_then(Value::as_str) == Some(&id()))
}

/// Whether Claude still offers the marketplace generated at `root`.
///
/// The path is part of the question. A marketplace of that name pointing
/// somewhere else is one this program did not put there, and removing it would
/// be taking away something of somebody else's that happens to share a name.
fn knows_marketplace(tool: &Path, root: &Path) -> bool {
    let Some(answer) = plugin(tool, ["marketplace", "list", "--json"]).ask() else {
        return false;
    };
    let Ok(Value::Array(marketplaces)) = serde_json::from_str::<Value>(&answer) else {
        return false;
    };
    marketplaces.iter().any(|marketplace| {
        marketplace.get("name").and_then(Value::as_str) == Some(RETIRED_NAME)
            && marketplace.get("path").and_then(Value::as_str) == root.to_str()
    })
}

/// How Claude is named when it is run, which is where it was found if it was
/// found at all.
///
/// A Claude that is not on the search path is still named, by the command a user
/// would type, so that a step which has to be taken by hand is printed as
/// something they can copy.
fn tool(env: &Environment) -> PathBuf {
    Agent::Claude
        .command(env)
        .unwrap_or_else(|| PathBuf::from(Agent::Claude.name()))
}

/// One `claude plugin` command.
fn plugin<'a>(tool: &Path, args: impl IntoIterator<Item = &'a str>) -> Invocation {
    let mut all = vec!["plugin".to_owned()];
    all.extend(args.into_iter().map(str::to_owned));
    Invocation::new(tool, all)
}

/// How Claude named the retired plugin once it had been installed from the
/// marketplace.
fn id() -> String {
    format!("{RETIRED_NAME}@{RETIRED_NAME}")
}

/// Reads a file that may not be there, naming it if reading fails.
fn read(path: &Path) -> Result<Option<String>, Error> {
    file::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// What installing on `env` would do, with nothing installed before.
    fn plan(env: &Environment) -> Vec<Change> {
        Claude
            .plan_install(env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect("planning failed")
    }

    /// The settings file as a plan would leave it, given what is in it now.
    fn settings_after(env: &Environment, before: Option<&str>) -> String {
        let path = settings(env);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        match before {
            Some(text) => fs::write(&path, text).unwrap(),
            None => {
                let _ = fs::remove_file(&path);
            }
        }
        match plan(env).pop().expect("no step for the settings") {
            Change::Create { contents, .. } | Change::Rewrite { contents, .. } => contents,
            Change::Keep { .. } => before.expect("kept a file that is not there").to_owned(),
            other => panic!("the settings were {other:?}"),
        }
    }

    /// The entry this program would write, as a value.
    fn ours(env: &Environment) -> Value {
        entry(env).expect("the entry cannot be built")
    }

    #[test]
    fn the_wrapper_obeys_the_rules_every_wrapper_obeys() {
        assets::tests::is_well_formed(Agent::Claude, &assets::CLAUDE_WRAPPER);
    }

    #[test]
    fn installing_writes_a_runnable_wrapper_and_an_entry_that_runs_it() {
        let (_home, env) = machine();

        let changes = plan(&env);

        let Some(Change::Create {
            path,
            contents,
            executable,
        }) = changes
            .iter()
            .find(|change| change.path() == Some(&wrapper(&env)))
        else {
            panic!("the wrapper was not written: {changes:?}");
        };
        assert_eq!(path, &env.home().join(".claude/hooks/agentbus.sh"));
        assert!(
            executable,
            "a script nothing may run is a script nothing runs"
        );
        assert!(contents.contains("'/opt/bin/agentbus'"), "{contents}");
        assert!(!contents.contains(BINARY_MARK), "{contents}");

        let hook = &ours(&env)["hooks"][0];
        assert_eq!(
            hook["command"],
            Value::from(format!("bash '{}'", path.display()))
        );
        assert_eq!(hook["async"], Value::Bool(true));
        assert_eq!(hook["timeout"], Value::from(TIMEOUT));
    }

    #[test]
    fn a_machine_that_runs_its_scripts_by_extension_gets_the_other_wrapper() {
        let (_home, env) = machine();
        let env = env.with_platform(Platform::Windows);

        let changes = plan(&env);
        let written = changes
            .iter()
            .find_map(|change| match change {
                Change::Create { path, contents, .. } => Some((path, contents)),
                _ => None,
            })
            .expect("nothing was written");

        assert!(written.0.ends_with("agentbus.ps1"), "{:?}", written.0);
        assert!(written.1.contains("'/opt/bin/agentbus'"), "{}", written.1);
        assert!(
            ours(&env)["hooks"][0]["command"]
                .as_str()
                .is_some_and(|command| command.contains("powershell")),
            "{:?}",
            ours(&env)
        );
    }

    #[test]
    fn a_binary_whose_path_needs_quoting_survives_being_put_in_the_wrapper() {
        let contents = generated(Platform::Unix, Path::new("/opt/a 'b'/agentbus")).unwrap();
        assert!(
            contents.contains(r"'/opt/a '\''b'\''/agentbus'"),
            "{contents}"
        );

        let contents = generated(Platform::Windows, Path::new("/opt/a 'b'/agentbus")).unwrap();
        assert!(contents.contains("'/opt/a ''b''/agentbus'"), "{contents}");
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/agentbus"));

        assert!(matches!(
            generated(Platform::Unix, &path),
            Err(Error::Unwritable { .. })
        ));
    }

    #[test]
    fn a_settings_file_that_is_not_there_is_written_from_nothing() {
        let (_home, env) = machine();

        let written = settings_after(&env, None);

        let document: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(document["hooks"][EVENT][0], ours(&env));
        assert!(written.ends_with("}\n"), "{written}");
    }

    #[test]
    fn what_the_user_wrote_around_the_entry_comes_back_out_byte_for_byte() {
        let (_home, env) = machine();
        let before = "{\n\t\"model\": \"the one they chose\",\r\n\
                      \t\"hooks\": {\r\n\
                      \t\t\"SessionStart\": [\r\n\
                      \t\t\t{ \"hooks\": [{ \"type\": \"command\", \"command\": \"theirs\" }] }\r\n\
                      \t\t]\r\n\
                      \t}\r\n\
                      }\r\n";

        let after = settings_after(&env, Some(before));

        assert!(
            after.starts_with("{\n\t\"model\": \"the one they chose\",\r\n"),
            "{after}"
        );
        assert!(after.contains("\"command\": \"theirs\""), "{after}");
        let document: Value = serde_json::from_str(&after).unwrap();
        let entries = document["hooks"][EVENT].as_array().unwrap();
        assert_eq!(entries.len(), 2, "{after}");
        assert_eq!(entries[1], ours(&env));
    }

    #[test]
    fn a_settings_file_written_on_one_line_stays_on_one_line() {
        let (_home, env) = machine();
        let before = "{\"model\":\"theirs\"}\n";

        let after = settings_after(&env, Some(before));

        assert!(after.starts_with("{\"model\":\"theirs\","), "{after}");
        let document: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(document["model"], Value::from("theirs"));
        assert_eq!(document["hooks"][EVENT][0], ours(&env));
    }

    #[test]
    fn installing_twice_leaves_the_settings_exactly_as_they_were() {
        let (_home, env) = machine();
        let path = settings(&env);
        let written = settings_after(&env, None);
        fs::write(&path, &written).unwrap();

        let again = plan(&env).pop().expect("no step for the settings");

        assert_eq!(again, Change::Keep { path });
    }

    #[test]
    fn a_settings_file_that_cannot_be_rewritten_stops_the_plan() {
        let (_home, env) = machine();
        let path = settings(&env);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Reads perfectly well, and writing it back out would silently drop the
        // first of the two.
        fs::write(&path, "{\n  \"hooks\": {},\n  \"hooks\": {}\n}\n").unwrap();

        assert!(matches!(
            Claude.plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus")),
            Err(Error::NotRewritable { .. })
        ));

        fs::write(&path, "{ not json at all }").unwrap();
        assert!(matches!(
            Claude.plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus")),
            Err(Error::NotRewritable { .. })
        ));
    }

    #[test]
    fn a_wrapper_of_the_users_own_with_the_same_name_stops_the_plan() {
        let (_home, env) = machine();
        let path = wrapper(&env);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# a hook I wrote myself, which happens to share a name\n",
        )
        .unwrap();

        assert!(matches!(
            Claude.plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus")),
            Err(Error::NotOurs { .. })
        ));
    }

    #[test]
    fn uninstalling_takes_the_entry_out_and_leaves_the_rest_of_the_file() {
        let (_home, env) = machine();
        let path = settings(&env);
        let before = "{\n  \"model\": \"theirs\",\n  \"hooks\": {\n    \"SessionStart\": [\n      { \"hooks\": [{ \"type\": \"command\", \"command\": \"theirs\" }] }\n    ]\n  }\n}\n";
        let after = settings_after(&env, Some(before));
        fs::write(&path, &after).unwrap();

        let changes = Claude.plan_uninstall(&env, &State::default()).unwrap();
        let Some(Change::Rewrite { contents, .. }) =
            changes.iter().find(|change| change.path() == Some(&*path))
        else {
            panic!("the settings were not written back: {changes:?}");
        };

        assert_eq!(contents, before, "the file did not come back as it went in");
    }

    #[test]
    fn uninstalling_a_settings_file_this_program_made_takes_the_file_with_it() {
        let (_home, env) = machine();
        let path = settings(&env);
        fs::write(&path, settings_after(&env, None)).unwrap();
        let mut state = State::default();
        state.record(&path, Agent::Claude, Ownership::Created);

        let changes = Claude.plan_uninstall(&env, &state).unwrap();

        assert!(
            changes.contains(&Change::Delete { path: path.clone() }),
            "{changes:?}"
        );
    }

    #[test]
    fn uninstalling_leaves_a_settings_file_with_nothing_of_ours_in_it_alone() {
        let (_home, env) = machine();
        let path = settings(&env);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Not something this program could have written into, and not something
        // it has to be able to, because there is nothing of its own in it.
        let theirs = "// their own file, with a comment in it\n{ \"model\": \"theirs\" }\n";
        fs::write(&path, theirs).unwrap();

        let changes = Claude.plan_uninstall(&env, &State::default()).unwrap();

        assert!(
            changes.iter().all(|change| !change.is_change()),
            "{changes:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
    }

    #[test]
    fn nothing_is_asked_of_the_agent_on_a_machine_that_never_had_the_old_install() {
        let (_home, env) = machine();

        for changes in [
            plan(&env),
            Claude.plan_uninstall(&env, &State::default()).unwrap(),
        ] {
            assert!(
                changes.iter().all(|change| change.command().is_none()),
                "{changes:?}"
            );
        }
    }

    #[test]
    fn an_installation_made_the_old_way_is_taken_away_by_both_directions() {
        let (_home, env) = machine();
        let root = env.data_dir().join(RETIRED_DIR);
        let hooks = generated_paths(&root)[2].clone();
        file::write(&hooks, "{}").unwrap();

        for changes in [
            plan(&env),
            Claude.plan_uninstall(&env, &State::default()).unwrap(),
        ] {
            let commands: Vec<String> = changes
                .iter()
                .filter_map(Change::command)
                .map(Invocation::to_string)
                .collect();
            assert_eq!(
                commands,
                vec![
                    "claude plugin uninstall agentbus@agentbus -s user".to_owned(),
                    "claude plugin marketplace remove agentbus".to_owned(),
                ],
                "{changes:?}"
            );
            assert!(
                changes.contains(&Change::Delete {
                    path: hooks.clone()
                }),
                "{changes:?}"
            );
            assert!(
                changes.contains(&Change::Clear { path: root.clone() }),
                "{changes:?}"
            );
        }
    }

    #[test]
    fn a_file_only_the_record_still_names_is_taken_away_as_well() {
        let (_home, env) = machine();
        let root = env.data_dir().join(RETIRED_DIR);
        let forgotten = root.join("agentbus/hooks/hooks.json");
        let mut state = State::default();
        state.record(&forgotten, Agent::Claude, Ownership::Created);

        let changes = Claude
            .plan_install(&env, &state, Path::new("/opt/bin/agentbus"))
            .unwrap();

        assert!(
            changes.contains(&Change::Delete { path: forgotten }),
            "{changes:?}"
        );
    }

    #[test]
    fn nothing_is_installed_until_the_wrapper_is_there_to_be_pointed_at() {
        let (_home, env) = machine();

        let changes = plan(&env);
        let order: Vec<&Path> = changes.iter().filter_map(Change::path).collect();

        let wrapper = order
            .iter()
            .position(|path| *path == wrapper(&env))
            .expect("no wrapper");
        let settings = order
            .iter()
            .position(|path| *path == settings(&env))
            .expect("no settings");
        assert!(wrapper < settings, "{order:?}");
    }

    #[test]
    fn what_is_installed_is_read_back_off_the_machine() {
        let (_home, env) = machine();
        assert_eq!(Claude.status(&env).unwrap(), HookStatus::NotInstalled);

        let wrapper = wrapper(&env);
        file::write(
            &wrapper,
            &generated(Platform::Unix, Path::new("/opt/bin/agentbus")).unwrap(),
        )
        .unwrap();
        let expected = crate::version::expected_version(Agent::Claude);
        assert_eq!(
            Claude.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
            "a wrapper nothing runs is not an installation"
        );

        let path = settings(&env);
        file::write(&path, &settings_after(&env, None)).unwrap();
        assert_eq!(Claude.status(&env).unwrap(), HookStatus::Current(expected));

        file::write(&path, "{\n  \"hooks\": {}\n}\n").unwrap();
        assert_eq!(
            Claude.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected),
            "an entry that has been taken out is one that no longer runs"
        );
    }

    #[test]
    fn a_wrapper_somebody_else_wrote_is_nothing_of_ours_installed() {
        let (_home, env) = machine();
        file::write(&wrapper(&env), "# mine\n").unwrap();

        assert_eq!(Claude.status(&env).unwrap(), HookStatus::NotInstalled);
    }
}
