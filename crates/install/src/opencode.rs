//! Installing for OpenCode.
//!
//! Three things go on the machine, and they are two halves of one installation
//! plus the line that joins them.
//!
//! The first is a plugin dropped into the directory the agent loads plugins
//! from. It is handed every event the plugin interface produces and forwards
//! each one, which is where nearly everything this program learns about a
//! session comes from.
//!
//! The second is a plugin the agent's *terminal interface* loads. It forwards
//! nothing, because the interface produces no events; what it has instead is the
//! session the user has open, which it knows the moment they open it. Without
//! it, a session somebody starts and then sits reading is a session this program
//! has never heard of — the events are what name a session, and a session
//! nothing has happened in has produced none. That plugin is registered by name
//! in the interface's own configuration file rather than dropped in beside the
//! first one, because everything in the plugin directory is loaded as an
//! ordinary plugin and this one is not shaped like one.
//!
//! So the third thing is one entry in `tui.jsonc`, which is a file somebody
//! keeps by hand: it is edited through [`crate::cst`] in the dialect its own
//! reader accepts, so their comments, their key order and their line endings
//! come back out exactly as they went in.
//!
//! # Ownership
//!
//! A file has no entries to hang a mark on, so ownership of the two plugins is
//! decided by each file's own first line. A file of one of these names carrying
//! the mark was written here and may be replaced or removed; one without it is
//! somebody's own work that happens to share a name, and this refuses to touch
//! it rather than guessing. The same rule decides whether a directory goes at
//! the end: only one this program's own record says it made, because a directory
//! the user already had is theirs however empty this leaves it.
//!
//! The entry in the configuration file has no room for a mark either — it is one
//! string in an array of strings — so it is recognized by being exactly the
//! string this program writes, and removed by the same test.
//!
//! # Where
//!
//! Only the user's own configuration is written to. OpenCode reads a plugin
//! directory from inside a project as well, and that one is checked into version
//! control and shared with everybody who clones the repository — installing one
//! person's local tooling into it would be installing it for their colleagues
//! too.
//!
//! Both plugins name this program's binary by an absolute path rather than by
//! the command `agentbus`, for the reason every agent's installation does: the
//! directory a user installed it into is not guaranteed to be on the `PATH`
//! their agent runs plugins with, and a command that cannot be found fails
//! silently, in the one place nobody is looking.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::agent::Agent;
use crate::assets::Asset;
use crate::change::Change;
use crate::cst::CstDocument;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, json, sentinel};

/// The directory inside OpenCode's configuration that plugins are loaded from.
const PLUGIN_DIR: &str = "plugin";

/// What the dropped plugin is called.
///
/// Named after this program, because everything in that directory is loaded and
/// a user looking at the list should be able to tell at a glance what each of
/// them is and what to remove to be rid of it.
const PLUGIN_FILE: &str = "agentbus.js";

/// What the plugin the terminal interface loads is called.
///
/// Beside the configuration file that names it rather than in the plugin
/// directory, so that the directory holds only files that are plugins of the
/// ordinary kind.
const TUI_PLUGIN_FILE: &str = "agentbus-tui.js";

/// The file the terminal interface keeps its own settings in.
const TUI_CONFIG: &str = "tui.jsonc";

/// Where in those settings a plugin is named.
const PLUGIN_KEY: &str = "plugin";

/// How the terminal interface's plugin is named there, which is as a path
/// relative to the directory the file itself is in.
const TUI_PLUGIN_SPEC: &str = "./agentbus-tui.js";

/// What a template says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// OpenCode's hooks: a plugin for its events, a plugin for its terminal
/// interface, and the entry that loads the second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCode;

impl Installer for OpenCode {
    fn agent(&self) -> Agent {
        Agent::OpenCode
    }

    /// Writes both plugins, then points the interface's configuration at the
    /// one it loads by name.
    ///
    /// In that order. The entry names a file, and a configuration that named one
    /// which is not there yet would be an interface that fails to start over a
    /// file this program had not finished writing.
    fn plan_install(
        &self,
        env: &Environment,
        _state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        // Writing a file makes the directories above it anyway; these steps are
        // here so that the making is *recorded*, which is the one thing the
        // uninstall cannot work out for itself later. Both, because an agent
        // found by its command alone may have no configuration directory yet,
        // and one this program made is one it has to be able to take away again.
        let mut changes: Vec<Change> = [Agent::OpenCode.config_dir(env), plugin_dir(env)]
            .into_iter()
            .filter(|dir| !dir.is_dir())
            .map(|path| Change::Make { path })
            .collect();
        let platform = env.platform();
        changes.push(plan_file(
            &plugin(env),
            &assets::OPENCODE_PLUGIN,
            platform,
            binary,
        )?);
        changes.push(plan_file(
            &tui_plugin(env),
            &assets::OPENCODE_TUI_PLUGIN,
            platform,
            binary,
        )?);
        changes.push(plan_entry(env)?);
        Ok(changes)
    }

    /// Takes the entry out, then both plugins, then the directories this program
    /// made.
    ///
    /// In that order for the same reason reversed: the entry is what loads a
    /// file, so it goes first and nothing is ever pointed at something that is
    /// no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = vec![plan_entry_removal(env, state)?];
        for path in [tui_plugin(env), plugin(env)] {
            // A file without the mark is passed over here rather than refused,
            // unlike on the way in: an uninstall that stopped because something
            // of the user's is in the way would be refusing to do the one thing
            // it can certainly do safely, which is nothing.
            changes.push(match read(&path)? {
                Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
                _ => Change::Keep { path },
            });
        }
        // Deepest first, and only the directories the record says this program
        // made.
        for dir in [plugin_dir(env), Agent::OpenCode.config_dir(env)] {
            if state.ownership(&dir) == Some(Ownership::Created) {
                changes.push(Change::Clear { path: dir });
            }
        }
        Ok(changes)
    }

    /// Reads the plugin the events come through, and then looks for the rest.
    ///
    /// That file is the one whose generation is reported, because it is the one
    /// every event passes through: an installation whose other half is missing
    /// still emits, and one whose first half is missing emits nothing whatever
    /// else is in place. The rest of it is not optional either, though — a
    /// second plugin the interface is no longer told to load is a session whose
    /// identity is not known until something happens in it — so anything
    /// missing there is said out loud rather than rounded up to working.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let Some(text) = read(&plugin(env))? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_wired(env)?))
    }
}

/// The directory OpenCode loads this user's plugins from.
fn plugin_dir(env: &Environment) -> PathBuf {
    Agent::OpenCode.config_dir(env).join(PLUGIN_DIR)
}

/// The plugin this program drops in there.
fn plugin(env: &Environment) -> PathBuf {
    plugin_dir(env).join(PLUGIN_FILE)
}

/// The plugin the terminal interface is told to load.
fn tui_plugin(env: &Environment) -> PathBuf {
    Agent::OpenCode.config_dir(env).join(TUI_PLUGIN_FILE)
}

/// The file the entry naming it goes into.
fn tui_config(env: &Environment) -> PathBuf {
    Agent::OpenCode.config_dir(env).join(TUI_CONFIG)
}

/// What one of the plugins should hold, with the binary written into it.
fn generated(asset: &Asset, platform: Platform, binary: &Path) -> Result<String, Error> {
    Ok(asset
        .text(platform)
        .replace(BINARY_MARK, &in_javascript(binary)?))
}

/// A path as it is written inside a JavaScript string.
///
/// A path is bytes and a string literal is text, so a path that is not text
/// cannot be put in one. That is refused rather than approximated: the lossy
/// spelling of it would install a plugin that runs a command which is not there,
/// and that shows up as an agent quietly emitting nothing — the hardest kind of
/// failure to attribute to its cause.
fn in_javascript(path: &Path) -> Result<String, Error> {
    let text = path.to_str().ok_or_else(|| Error::Unwritable {
        path: path.to_owned(),
    })?;
    Ok(json::escaped(text))
}

/// What writing one of the plugins would do, given whatever is at `path` now.
///
/// A file of one of these names that this program did not write stops the plan.
/// It is the one case where doing nothing is worse than saying so: silently
/// leaving it would report a successful installation of hooks that are not
/// installed, and silently replacing it would delete something a user wrote.
///
/// A file this program *did* write is replaced whatever it says, which is what
/// carries a machine off an installation made before these files said which
/// generation they were: that one is a single plugin bearing the mark and
/// nothing else, and it is written over by the pair.
fn plan_file(
    path: &Path,
    asset: &Asset,
    platform: Platform,
    binary: &Path,
) -> Result<Change, Error> {
    let contents = generated(asset, platform, binary)?;
    let path = path.to_owned();
    Ok(match read(&path)? {
        None => Change::Create {
            path,
            contents,
            executable: false,
        },
        Some(text) if !sentinel::is_generated(&text) => return Err(Error::NotOurs { path }),
        Some(text) if text == contents => Change::Keep { path },
        Some(_) => Change::Rewrite {
            path,
            contents,
            executable: false,
        },
    })
}

/// Whether `entry` is the one this program writes into the configuration.
///
/// Exact equality with the string it writes, because there is nowhere in a
/// string to put a mark. An entry naming the same plugin some other way is
/// somebody else's line about a file of this program's, and taking it out would
/// be editing something this program did not write.
fn is_ours(entry: &Value) -> bool {
    entry.as_str() == Some(TUI_PLUGIN_SPEC)
}

/// Whether the configuration already names the plugin this program installs.
fn holds(document: &CstDocument) -> bool {
    matches!(
        document.get(&[PLUGIN_KEY]),
        Some(Value::Array(entries)) if entries.iter().any(is_ours)
    )
}

/// What putting the entry into the configuration file would do.
///
/// A file that is not there is written from nothing, which is the case worth
/// having: no merge, no user content, nothing that can go wrong. A file that is
/// there is edited where the entry goes and nowhere else — and a file whose
/// `plugin` is something other than a list of plugins stops the plan, because
/// there is no way to add one to it that its own reader would still understand.
fn plan_entry(env: &Environment) -> Result<Change, Error> {
    let path = tui_config(env);
    let Some(text) = read(&path)? else {
        let mut document = Map::new();
        document.insert(
            PLUGIN_KEY.to_owned(),
            Value::Array(vec![Value::from(TUI_PLUGIN_SPEC)]),
        );
        return Ok(Change::Create {
            path,
            contents: json::render(&Value::Object(document), json::DEFAULT_INDENT),
            executable: false,
        });
    };

    let mut document = CstDocument::parse_jsonc(&path, &text)?;
    // Asked first, because taking the entry out and putting the same one back
    // would move it to the end of a list somebody else keeps the order of — a
    // change to their file made by a run with nothing to do.
    if !holds(&document) {
        document.append(&[PLUGIN_KEY], Value::from(TUI_PLUGIN_SPEC))?;
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

/// What taking the entry back out of the configuration file would do.
///
/// A file that does not mention the plugin at all is left alone without being
/// parsed. That is not a shortcut around the strictness: a file with nothing of
/// this program's in it is a file an uninstall has nothing to do to, and
/// refusing to finish an uninstall over a configuration file this program never
/// wrote into would be refusing to remove everything else on the machine as
/// well.
fn plan_entry_removal(env: &Environment, state: &State) -> Result<Change, Error> {
    let path = tui_config(env);
    let Some(text) = read(&path)? else {
        return Ok(Change::Keep { path });
    };
    if !text.contains(TUI_PLUGIN_SPEC) {
        return Ok(Change::Keep { path });
    }

    let mut document = CstDocument::parse_jsonc(&path, &text)?;
    document.retain(&[PLUGIN_KEY], |entry| !is_ours(entry))?;
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

/// Whether the half of the installation the events do not travel through is
/// there: the plugin the terminal interface loads, at this build's generation,
/// and the entry that tells it to.
///
/// A configuration file that cannot be read as this program reads it counts as
/// naming nothing. It may well be the interface's own to understand, but it is
/// not one an install could add to either, so an installation resting on it is
/// one that needs a person.
fn is_wired(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&tui_plugin(env))? else {
        return Ok(false);
    };
    if !sentinel::is_generated(&text) {
        return Ok(false);
    }
    if !matches!(
        HookStatus::of_text(Agent::OpenCode, &text),
        HookStatus::Current(_)
    ) {
        return Ok(false);
    }

    let path = tui_config(env);
    let Some(text) = read(&path)? else {
        return Ok(false);
    };
    let Ok(document) = CstDocument::parse_jsonc(&path, &text) else {
        return Ok(false);
    };
    Ok(holds(&document))
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

    /// The binary a test installation names.
    const BINARY: &str = "/opt/bin/agentbus";

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// What installing on `env` would do, with nothing recorded before.
    fn plan(env: &Environment) -> Vec<Change> {
        OpenCode
            .plan_install(env, &State::default(), Path::new(BINARY))
            .expect("planning failed")
    }

    /// The step of a plan that is about `path`.
    fn step<'a>(changes: &'a [Change], path: &Path) -> &'a Change {
        changes
            .iter()
            .find(|change| change.path() == Some(path))
            .unwrap_or_else(|| panic!("no step for {}: {changes:?}", path.display()))
    }

    /// What a plan would write to `path`, whether or not there was a file there.
    fn written(changes: &[Change], path: &Path) -> String {
        match step(changes, path) {
            Change::Create { contents, .. } | Change::Rewrite { contents, .. } => contents.clone(),
            other => panic!("{} was {other:?}", path.display()),
        }
    }

    /// Puts a file there, making the directories above it.
    fn put(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("a file has a directory")).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Installs for real, so that a later plan is planned against a machine an
    /// install has actually happened on.
    fn install(env: &Environment) -> State {
        let mut state = State::default();
        for change in plan(env) {
            change.apply(Agent::OpenCode, &mut state).expect("applying");
        }
        state
    }

    /// The configuration file as a plan would leave it, given what is in it now.
    fn config_after(env: &Environment, before: Option<&str>) -> String {
        let path = tui_config(env);
        match before {
            Some(text) => put(&path, text),
            None => {
                let _ = fs::remove_file(&path);
            }
        }
        match plan_entry(env).expect("planning the entry failed") {
            Change::Create { contents, .. } | Change::Rewrite { contents, .. } => contents,
            Change::Keep { .. } => before.expect("kept a file that is not there").to_owned(),
            other => panic!("the configuration was {other:?}"),
        }
    }

    #[test]
    fn both_plugins_obey_the_rules_every_installed_file_obeys() {
        assets::tests::is_well_formed(Agent::OpenCode, &assets::OPENCODE_PLUGIN);
        assets::tests::is_well_formed(Agent::OpenCode, &assets::OPENCODE_TUI_PLUGIN);
    }

    #[test]
    fn the_two_plugins_go_where_the_agent_loads_each_of_them_from() {
        let env = Environment::rooted("/home/u");

        assert_eq!(
            plugin(&env),
            PathBuf::from("/home/u/.config/opencode/plugin/agentbus.js")
        );
        assert_eq!(
            tui_plugin(&env),
            PathBuf::from("/home/u/.config/opencode/agentbus-tui.js")
        );
        assert_eq!(
            tui_config(&env),
            PathBuf::from("/home/u/.config/opencode/tui.jsonc")
        );
        // The entry names the plugin the way its own reader resolves it, which
        // is from the directory the configuration file is in.
        assert_eq!(
            tui_config(&env).parent().unwrap().join(
                TUI_PLUGIN_SPEC
                    .strip_prefix("./")
                    .expect("the entry is a relative path")
            ),
            tui_plugin(&env)
        );
    }

    #[test]
    fn installing_writes_both_plugins_and_the_entry_that_loads_the_second() {
        let (_home, env) = machine();

        let changes = plan(&env);

        for path in [plugin(&env), tui_plugin(&env)] {
            let script = written(&changes, &path);
            assert!(
                script.contains(&format!("\"{BINARY}\"")),
                "{} does not name the binary: {script}",
                path.display()
            );
            assert!(
                !script.contains(BINARY_MARK),
                "{} was left as a template: {script}",
                path.display()
            );
            assert!(
                script.contains(r#""emit", "--agent", "opencode""#),
                "{} does not emit: {script}",
                path.display()
            );
            assert!(
                sentinel::is_generated(&script),
                "{} was left unmarked: {script}",
                path.display()
            );
        }

        let config: Value =
            serde_json::from_str(&written(&changes, &tui_config(&env))).expect("valid JSON");
        assert_eq!(config[PLUGIN_KEY], serde_json::json!([TUI_PLUGIN_SPEC]));
    }

    #[test]
    fn a_path_with_something_to_escape_survives_being_put_in_a_string() {
        let (_home, env) = machine();
        let awkward = Path::new("/opt/a \"b\"\\c/agentbus");

        let changes = OpenCode
            .plan_install(&env, &State::default(), awkward)
            .expect("planning failed");

        for path in [plugin(&env), tui_plugin(&env)] {
            let script = written(&changes, &path);
            assert!(
                script.contains(r#""/opt/a \"b\"\\c/agentbus""#),
                "{} was not escaped: {script}",
                path.display()
            );
        }
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let binary = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/agentbus"));

        assert!(matches!(
            in_javascript(&binary),
            Err(Error::Unwritable { .. })
        ));
        assert!(matches!(
            OpenCode.plan_install(&Environment::rooted("/home/u"), &State::default(), &binary),
            Err(Error::Unwritable { .. })
        ));
    }

    #[test]
    fn a_directory_that_is_already_there_is_not_claimed_as_this_programs_own() {
        let (home, env) = machine();
        fs::create_dir_all(home.path().join(".config/opencode/plugin")).unwrap();

        let changes = plan(&env);

        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, Change::Make { .. })),
            "a directory that was there would have been made again: {changes:?}"
        );
    }

    #[test]
    fn installing_a_second_time_changes_nothing() {
        let (_home, env) = machine();
        install(&env);

        let changes = plan(&env);

        assert!(
            changes.iter().all(|change| matches!(
                change,
                Change::Keep { .. } | Change::Ran { .. } | Change::Setting { .. }
            )),
            "a second install would change something: {changes:?}"
        );
    }

    #[test]
    fn a_configuration_kept_by_hand_keeps_its_comments_and_its_own_plugins() {
        let (_home, env) = machine();
        let theirs = "{\n  // the ones I chose myself\n  \"plugin\": [\"./mine.js\"],\n  \
                      \"theme\": \"gruvbox\",\n}\n";

        let after = config_after(&env, Some(theirs));

        assert!(after.contains("// the ones I chose myself"), "{after}");
        assert!(after.contains("\"./mine.js\""), "{after}");
        assert!(after.contains("\"theme\": \"gruvbox\""), "{after}");
        assert!(after.contains(TUI_PLUGIN_SPEC), "{after}");
        // Everything they wrote comes back out as they wrote it: the only line
        // that differs from theirs is the one this program added.
        let added: Vec<&str> = after
            .lines()
            .filter(|line| !theirs.lines().any(|mine| mine == *line))
            .collect();
        assert_eq!(added.len(), 1, "{after}");
        assert!(added[0].contains(TUI_PLUGIN_SPEC), "{after}");
    }

    #[test]
    fn a_configuration_that_is_not_there_is_written_from_nothing() {
        let (_home, env) = machine();
        fs::create_dir_all(Agent::OpenCode.config_dir(&env)).unwrap();

        let after = config_after(&env, None);

        let config: Value = serde_json::from_str(&after).expect("valid JSON");
        assert_eq!(config, serde_json::json!({ PLUGIN_KEY: [TUI_PLUGIN_SPEC] }));
    }

    #[test]
    fn a_configuration_that_already_names_the_plugin_is_left_exactly_as_it_is() {
        let (_home, env) = machine();
        let theirs = format!("{{\"plugin\": [\"{TUI_PLUGIN_SPEC}\"]}}\n");
        put(&tui_config(&env), &theirs);

        assert!(matches!(plan_entry(&env), Ok(Change::Keep { .. })));
    }

    #[test]
    fn a_configuration_whose_plugin_list_is_not_a_list_stops_the_plan() {
        let (_home, env) = machine();
        let theirs = "{\n  \"plugin\": \"./mine.js\"\n}\n";
        let path = tui_config(&env);
        put(&path, theirs);

        let refused = OpenCode.plan_install(&env, &State::default(), Path::new(BINARY));

        assert!(
            matches!(&refused, Err(Error::Conflict { path: named, .. }) if named == &path),
            "{refused:?}"
        );
        // A refused plan is a plan, and a plan changes nothing.
        assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
        assert!(!plugin(&env).exists());
    }

    #[test]
    fn an_installation_from_before_these_files_said_what_they_were_is_replaced() {
        let (_home, env) = machine();
        // What an earlier build left: the one plugin, marked as this program's
        // and saying nothing about which generation it is.
        let legacy = "// _agentbus — generated\nexport const AgentBus = async () => ({});\n";
        put(&plugin(&env), legacy);

        assert_eq!(
            OpenCode.status(&env).unwrap(),
            HookStatus::Outdated {
                found: None,
                expected: crate::version::expected_version(Agent::OpenCode)
            },
            "a file that says nothing about itself is behind"
        );

        let changes = plan(&env);

        assert!(
            matches!(step(&changes, &plugin(&env)), Change::Rewrite { .. }),
            "{changes:?}"
        );
        assert!(
            matches!(step(&changes, &tui_plugin(&env)), Change::Create { .. }),
            "{changes:?}"
        );
        // And an uninstall of that machine takes the old file away, because the
        // mark is what ownership turns on and it carries one.
        let changes = OpenCode.plan_uninstall(&env, &State::default()).unwrap();
        assert!(
            matches!(step(&changes, &plugin(&env)), Change::Delete { .. }),
            "{changes:?}"
        );
    }

    #[test]
    fn a_plugin_somebody_else_wrote_stops_the_installation_and_survives_an_uninstall() {
        let (_home, env) = machine();
        for path in [plugin(&env), tui_plugin(&env)] {
            let theirs = "// a plugin of my own, which happens to share a name\n";
            put(&path, theirs);

            let refused = OpenCode.plan_install(&env, &State::default(), Path::new(BINARY));
            assert!(matches!(refused, Err(Error::NotOurs { .. })), "{refused:?}");

            let changes = OpenCode.plan_uninstall(&env, &State::default()).unwrap();
            assert!(
                matches!(step(&changes, &path), Change::Keep { .. }),
                "{changes:?}"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
            fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn uninstalling_takes_both_plugins_and_the_entry_and_leaves_the_rest_as_it_was() {
        let (_home, env) = machine();
        let theirs = "{\n  // mine\n  \"plugin\": [\"./mine.js\"],\n}\n";
        put(&tui_config(&env), theirs);
        let state = install(&env);

        for change in OpenCode.plan_uninstall(&env, &state).unwrap() {
            change
                .apply(Agent::OpenCode, &mut state.clone())
                .expect("applying");
        }

        assert!(!plugin(&env).exists(), "the plugin was left behind");
        assert!(
            !tui_plugin(&env).exists(),
            "the other plugin was left behind"
        );
        assert_eq!(
            fs::read_to_string(tui_config(&env)).unwrap(),
            theirs,
            "the configuration was not put back as it was"
        );
    }

    #[test]
    fn a_configuration_this_program_wrote_and_has_just_emptied_goes_with_it() {
        let (_home, env) = machine();
        let state = install(&env);

        let changes = OpenCode.plan_uninstall(&env, &state).unwrap();

        assert!(
            matches!(step(&changes, &tui_config(&env)), Change::Delete { .. }),
            "{changes:?}"
        );
    }

    #[test]
    fn a_configuration_that_never_named_the_plugin_is_not_even_parsed() {
        let (_home, env) = machine();
        // Not JSON of any dialect. An uninstall with nothing to do to it has no
        // business reading it well enough to complain.
        put(&tui_config(&env), "this is not a configuration file\n");

        assert!(matches!(
            plan_entry_removal(&env, &State::default()),
            Ok(Change::Keep { .. })
        ));
    }

    #[test]
    fn the_directories_go_only_when_this_program_was_the_one_that_made_them() {
        let (_home, env) = machine();
        let dirs = [plugin_dir(&env), Agent::OpenCode.config_dir(&env)];

        let untouched = OpenCode.plan_uninstall(&env, &State::default()).unwrap();
        assert!(
            !untouched
                .iter()
                .any(|change| matches!(change, Change::Clear { .. })),
            "a directory nobody claimed was going to be removed: {untouched:?}"
        );

        let state = install(&env);
        let changes = OpenCode.plan_uninstall(&env, &state).unwrap();
        for dir in dirs {
            assert!(
                changes.contains(&Change::Clear { path: dir.clone() }),
                "{} was left behind: {changes:?}",
                dir.display()
            );
        }
    }

    #[test]
    fn nothing_installed_is_nothing_installed() {
        let (_home, env) = machine();

        assert_eq!(OpenCode.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn a_whole_installation_is_current_and_a_missing_half_of_one_needs_repairing() {
        let (_home, env) = machine();
        install(&env);
        let expected = crate::version::expected_version(Agent::OpenCode);

        assert_eq!(
            OpenCode.status(&env).unwrap(),
            HookStatus::Current(expected)
        );

        // Each of the other two things an installation is made of, taken away
        // one at a time. The file the events travel through is still there and
        // still current, and saying so alone would be reporting hooks that only
        // half work as working.
        put(&tui_config(&env), "{\"plugin\": [\"./mine.js\"]}\n");
        assert_eq!(
            OpenCode.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );

        install(&env);
        fs::remove_file(tui_plugin(&env)).unwrap();
        assert_eq!(
            OpenCode.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );
    }

    #[test]
    fn a_plugin_of_somebody_elses_is_not_an_installation_of_this_programs() {
        let (_home, env) = machine();
        install(&env);
        put(&plugin(&env), "// mine\n");

        assert_eq!(OpenCode.status(&env).unwrap(), HookStatus::NotInstalled);
    }
}
