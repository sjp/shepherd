//! Installing for Hermes.
//!
//! Two things go on the machine, and they are one installation. A plugin
//! directory is dropped into the directory Hermes loads plugins from, holding
//! the module Hermes imports and the small file that tells it what the
//! directory is; and the plugin's name goes into a list in the `config.yaml`
//! beside it, which is what makes Hermes load a plugin it has found rather than
//! merely know about it.
//!
//! The module is where the generation mark lives, so a machine can be asked
//! what it is carrying. The entry in the list is what makes any of it ever run.
//!
//! # A plugin that hands over and forgets
//!
//! Hermes calls a plugin back with keyword arguments and no standard input, so
//! there is nothing to forward as it arrived. What the module sends instead is
//! a small object of its own — the callback's name, the session, the interface
//! that session is running under, and the directory the agent is working in —
//! and the mapping for this agent is written against that shape. Nothing in the
//! module decides what any of it means, including which sessions are worth
//! hearing about: that judgement is data, it lives with every other judgement
//! about an event, and it can be changed by shipping a binary rather than by
//! editing somebody's plugin directory.
//!
//! # Ownership
//!
//! Neither file has entries to hang a mark on, so ownership of each is decided
//! by its own first line, exactly as it is for every other whole file this
//! program writes. A file of one of these names carrying the mark was written
//! here and may be replaced or removed; one without it is somebody's own work
//! that happens to share a name, and this refuses to touch it rather than
//! guessing. The directories go the same way: only the ones this program's own
//! record says it made.
//!
//! The entry in `config.yaml` has nowhere to put a mark either — it is one name
//! in a list of names — so it is recognized by being exactly the name this
//! program writes. The keys above it are a different question: a
//! `plugins.enabled` holding nothing but this plugin looks the same whether
//! this program wrote those keys or found them empty and filled them, and that
//! is the one fact about the edit the file cannot answer later. So it is
//! recorded when the entry goes in and read back when it comes out.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::change::Change;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, json, sentinel, yaml_text};

/// The directory inside Hermes' configuration that plugins are loaded from.
const PLUGINS_DIR: &str = "plugins";

/// What this program's plugin is called.
///
/// The name of its directory, the name inside the file that describes it, and
/// the name it is switched on by in the configuration file, all at once — Hermes
/// joins the three, so they are one constant here.
const PLUGIN_NAME: &str = "agentbus";

/// The module Hermes imports out of the plugin directory.
const MODULE_FILE: &str = "__init__.py";

/// The file beside it that says what the directory is.
const MANIFEST_FILE: &str = "plugin.yaml";

/// The file Hermes reads its settings, and its list of plugins, out of.
const CONFIG_FILE: &str = "config.yaml";

/// Where in those settings the plugins to load are named.
const LIST: &[&str] = &["plugins", "enabled"];

/// What the module says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// Hermes' hooks: a plugin of two files, and the entry in the user's own
/// configuration that loads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hermes;

impl Installer for Hermes {
    fn agent(&self) -> Agent {
        Agent::Hermes
    }

    /// Writes the plugin, then puts its name in the list that loads it.
    ///
    /// In that order. The entry names a directory, and a configuration naming
    /// one that is not there yet would be an agent failing to start over a
    /// plugin this program had not finished writing.
    fn plan_install(
        &self,
        env: &Environment,
        state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        let home = config_dir(env);
        // Never made here. A configuration directory that is not there means
        // this agent has never run on the machine, and an installer that made
        // one would be creating another program's home on the strength of a
        // guess about its layout.
        if !home.is_dir() {
            return Err(Error::Absent {
                agent: self.agent(),
                path: home,
            });
        }
        // Writing a file makes the directories above it anyway; these steps are
        // here so that the making is *recorded*, which is the one thing the
        // uninstall cannot work out for itself later.
        let mut changes: Vec<Change> = [plugins_dir(env), plugin_dir(env)]
            .into_iter()
            .filter(|dir| !dir.is_dir())
            .map(|path| Change::Make { path })
            .collect();
        let platform = env.platform();
        changes.push(plan_file(module(env), generated(platform, binary)?)?);
        changes.push(plan_file(
            manifest(env),
            assets::HERMES_PLUGIN_MANIFEST.text(platform).to_owned(),
        )?);
        changes.extend(plan_entry(env, state)?);
        Ok(changes)
    }

    /// Takes the entry out, then the plugin, then the directories this program
    /// made.
    ///
    /// The same order reversed: nothing is ever left loading something that is
    /// no longer there.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let mut changes = plan_entry_removal(env, state)?;
        for path in [manifest(env), module(env)] {
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
        for dir in [plugin_dir(env), plugins_dir(env)] {
            if state.ownership(&dir) == Some(Ownership::Created) {
                changes.push(Change::Clear { path: dir });
            }
        }
        Ok(changes)
    }

    /// Reads the module, and then looks for the rest.
    ///
    /// The module is the file that says which generation this is, because it is
    /// the one whose content is the installation. The rest of it is not
    /// optional, though: a plugin directory Hermes cannot recognize, or a name
    /// missing from the list, is a module sitting on disk that nothing ever
    /// imports — so anything missing there is said out loud rather than rounded
    /// up to working.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let Some(text) = read(&self.asset(env))? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        let status = HookStatus::of_text(self.agent(), &text);
        Ok(status.confirmed(is_wired(env)?))
    }

    /// The module this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        module(env)
    }
}

/// Where the agent keeps its configuration.
fn config_dir(env: &Environment) -> PathBuf {
    Agent::Hermes.config_dir(env)
}

/// The directory Hermes loads this user's plugins from.
fn plugins_dir(env: &Environment) -> PathBuf {
    config_dir(env).join(PLUGINS_DIR)
}

/// The directory this program's own plugin is.
fn plugin_dir(env: &Environment) -> PathBuf {
    plugins_dir(env).join(PLUGIN_NAME)
}

/// The module inside it that Hermes imports.
fn module(env: &Environment) -> PathBuf {
    plugin_dir(env).join(MODULE_FILE)
}

/// The file beside it that describes the directory.
fn manifest(env: &Environment) -> PathBuf {
    plugin_dir(env).join(MANIFEST_FILE)
}

/// The file the entry naming the plugin goes into.
fn config(env: &Environment) -> PathBuf {
    config_dir(env).join(CONFIG_FILE)
}

/// What the module should hold, with the binary written into it.
fn generated(platform: Platform, binary: &Path) -> Result<String, Error> {
    Ok(assets::HERMES_PLUGIN
        .text(platform)
        .replace(BINARY_MARK, &in_python(binary)?))
}

/// A path as it is written inside a Python string.
///
/// Spelled the way a JSON string spells it, which Python reads as the same
/// text: both give a backslash, a quote and a character outside the printable
/// range the same three escapes. A path that is not text cannot be put in a
/// string at all, and that is refused rather than approximated — the lossy
/// spelling of it would install a plugin that runs a command which is not
/// there, and that shows up as an agent quietly emitting nothing, the hardest
/// kind of failure to attribute to its cause.
fn in_python(path: &Path) -> Result<String, Error> {
    let text = path.to_str().ok_or_else(|| Error::Unwritable {
        path: path.to_owned(),
    })?;
    Ok(json::escaped(text))
}

/// What writing one of the plugin's files would do, given whatever is at its
/// path now.
///
/// A file of one of these names that this program did not write stops the plan.
/// It is the one case where doing nothing is worse than saying so: silently
/// leaving it would report a successful installation of hooks that are not
/// installed, and silently replacing it would delete something a user wrote.
fn plan_file(path: PathBuf, contents: String) -> Result<Change, Error> {
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

/// How the key `depth` keys along the way to the list is named in the record.
///
/// Written the way somebody reading their own file would say it, so that the
/// record names a line they can find.
fn setting(depth: usize) -> String {
    LIST[..=depth].join(".")
}

/// One step saying who a key on the way to the list belongs to.
fn claim(path: &Path, setting: String, ours: bool) -> Change {
    Change::Setting {
        path: path.to_owned(),
        setting,
        ours,
    }
}

/// What putting the plugin's name into the configuration file would do, and who
/// the keys above it belong to afterwards.
///
/// A file that is not there is read as an empty one, so that writing the entry
/// into nothing goes through the same editor as writing it into somebody's
/// settings — one spelling of the list, arrived at one way. A key the record
/// already claims stays claimed, because a second install finds it there and
/// would otherwise conclude it had always been.
fn plan_entry(env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
    let path = config(env);
    let existing = read(&path)?;
    let before = existing.as_deref().unwrap_or_default();
    let added = yaml_text::add_to_list(before, LIST, PLUGIN_NAME).map_err(problem(&path))?;

    let file = match existing {
        None => Change::Create {
            path: path.clone(),
            contents: added.text,
            executable: false,
        },
        Some(text) if text == added.text => Change::Keep { path: path.clone() },
        Some(_) => Change::Rewrite {
            path: path.clone(),
            contents: added.text,
            executable: false,
        },
    };

    let mut changes = vec![file];
    // The keys that had to be written are counted from the innermost outwards,
    // which is the only way they can be missing: a key is not there unless the
    // key above it was there to hold it.
    changes.extend((0..LIST.len()).map(|depth| {
        let named = setting(depth);
        let ours = depth + added.created >= LIST.len() || state.claimed(&path, &named);
        claim(&path, named, ours)
    }));
    Ok(changes)
}

/// What taking the plugin's name back out would do.
///
/// A file that does not mention the plugin at all is left alone without being
/// parsed. That is not a shortcut around the strictness: a file with nothing of
/// this program's in it is a file an uninstall has nothing to do to, and
/// refusing to finish an uninstall over a configuration file this program never
/// wrote into would be refusing to remove everything else on the machine as
/// well.
fn plan_entry_removal(env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
    let path = config(env);
    // Only the keys claimed inwards from the innermost, because that is the
    // shape an install can leave: a key was written only if everything below it
    // was written too.
    let created = (0..LIST.len())
        .rev()
        .take_while(|depth| state.claimed(&path, &setting(*depth)))
        .count();

    let file = match read(&path)? {
        Some(text) if text.contains(PLUGIN_NAME) => {
            let contents = yaml_text::remove_from_list(&text, LIST, PLUGIN_NAME, created)
                .map_err(problem(&path))?;
            // A file this program created and has just emptied is litter. One it
            // merely added a name to is the user's, however little of it is left.
            let ours = state.ownership(&path) == Some(Ownership::Created);
            match contents {
                _ if ours && contents.trim().is_empty() => Change::Delete { path: path.clone() },
                ref same if *same == text => Change::Keep { path: path.clone() },
                contents => Change::Rewrite {
                    path: path.clone(),
                    contents,
                    executable: false,
                },
            }
        }
        _ => Change::Keep { path: path.clone() },
    };

    let mut changes = vec![file];
    // Given up whichever way that went, including where the file itself has
    // gone: a record still claiming a key in a file nobody can find is the one
    // thing an uninstall would otherwise leave behind.
    changes.extend((0..LIST.len()).map(|depth| claim(&path, setting(depth), false)));
    Ok(changes)
}

/// Whether the half of the installation the events do not travel through is
/// there: the file that makes the directory a plugin, at this build's
/// generation, and the entry that switches it on.
///
/// A configuration file that cannot be read as this program reads it counts as
/// naming nothing. It may well be Hermes' own to understand, but it is not one
/// an install could add to either, so an installation resting on it is one that
/// needs a person.
fn is_wired(env: &Environment) -> Result<bool, Error> {
    let Some(text) = read(&manifest(env))? else {
        return Ok(false);
    };
    if !sentinel::is_generated(&text) {
        return Ok(false);
    }
    if !matches!(
        HookStatus::of_text(Agent::Hermes, &text),
        HookStatus::Current(_)
    ) {
        return Ok(false);
    }
    let Some(text) = read(&config(env))? else {
        return Ok(false);
    };
    Ok(yaml_text::contains(&text, LIST, PLUGIN_NAME).unwrap_or(false))
}

/// How a file whose list cannot be edited a line at a time is refused, named.
fn problem(path: &Path) -> impl Fn(yaml_text::Problem) -> Error {
    let path = path.to_owned();
    move |problem| Error::NotListable {
        path: path.clone(),
        problem,
    }
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
    use crate::assets::tests::is_well_formed;
    use crate::version::expected_version;

    /// The agent this module installs for.
    const AGENT: Agent = Agent::Hermes;

    /// Where this program's own binary is, as far as these tests are concerned.
    const BINARY: &str = "/opt/bin/agentbus";

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path()).with_platform(Platform::Unix);
        (home, env)
    }

    /// The same machine, with the agent's configuration directory on it.
    fn machine_with_agent() -> (tempfile::TempDir, Environment) {
        let (home, env) = machine();
        fs::create_dir_all(config_dir(&env)).expect("cannot make the agent's directory");
        (home, env)
    }

    /// What installing on `env` would do, given the record `state`.
    fn plan_with(env: &Environment, state: &State) -> Vec<Change> {
        Hermes
            .plan_install(env, state, Path::new(BINARY))
            .expect("planning failed")
    }

    /// What installing on `env` would do, with nothing recorded before.
    fn plan(env: &Environment) -> Vec<Change> {
        plan_with(env, &State::default())
    }

    /// Carries `changes` out, so that the next plan sees the machine they left.
    fn apply(changes: &[Change], state: &mut State) {
        for change in changes {
            change
                .apply(AGENT, state)
                .expect("carrying out a step failed");
        }
    }

    /// Installs for real, and answers with the record it left.
    fn install(env: &Environment) -> State {
        let mut state = State::default();
        apply(&plan(env), &mut state);
        state
    }

    /// Uninstalls for real, from the record an install left.
    fn uninstall(env: &Environment, state: &mut State) {
        let changes = Hermes.plan_uninstall(env, state).expect("planning failed");
        apply(&changes, state);
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

    /// The configuration file as it stands.
    fn config_text(env: &Environment) -> Option<String> {
        fs::read_to_string(config(env)).ok()
    }

    #[test]
    fn the_module_obeys_the_rules_every_file_installed_into_an_agent_obeys() {
        is_well_formed(AGENT, &assets::HERMES_PLUGIN);
    }

    /// What a module running inside somebody's session may not be caught
    /// saying, whether in its code or in a comment about its code.
    ///
    /// Between them: nothing goes to the output the agent is reading, nothing
    /// is waited for, and the command is never asked to answer for itself. Each
    /// is a single spelling because there is a single way to do it, and a
    /// module that grew a second way would be a module worth reading again.
    const NEVER: [&str; 6] = [
        "print(",
        "sys.stdout",
        "sys.stderr",
        ".wait(",
        ".communicate(",
        "check=True",
    ];

    #[test]
    fn the_module_keeps_the_promises_a_file_that_runs_inside_a_session_makes() {
        let text = assets::HERMES_PLUGIN.text(Platform::Unix);

        for spelling in NEVER {
            assert!(!text.contains(spelling), "the module says `{spelling}`");
        }
        assert!(
            text.contains("stdout=subprocess.DEVNULL")
                && text.contains("stderr=subprocess.DEVNULL"),
            "the command's output is not thrown away: {text}",
        );
    }

    #[test]
    fn the_file_that_describes_the_plugin_says_who_wrote_it_and_which_generation_it_is() {
        for platform in [Platform::Unix, Platform::Windows] {
            let text = assets::HERMES_PLUGIN_MANIFEST.text(platform);
            assert!(sentinel::is_generated(text), "{text}");
            assert_eq!(
                crate::version::parse_asset_version(text),
                Some(expected_version(AGENT)),
                "{text}",
            );
            // The three names the agent joins up: this file's, the directory's,
            // and the one the configuration switches on.
            assert!(
                text.contains(&format!("name: {PLUGIN_NAME}")),
                "the plugin does not call itself what it is switched on by: {text}",
            );
        }
    }

    #[test]
    fn the_plugin_goes_where_the_agent_loads_plugins_from() {
        let env = Environment::rooted("/home/u").with_platform(Platform::Unix);

        assert_eq!(
            plugin_dir(&env),
            PathBuf::from("/home/u/.hermes/plugins/agentbus")
        );
        assert_eq!(
            module(&env),
            PathBuf::from("/home/u/.hermes/plugins/agentbus/__init__.py")
        );
        assert_eq!(
            manifest(&env),
            PathBuf::from("/home/u/.hermes/plugins/agentbus/plugin.yaml")
        );
        assert_eq!(config(&env), PathBuf::from("/home/u/.hermes/config.yaml"));
        // The entry names the directory, so the two have to agree.
        assert_eq!(plugin_dir(&env).file_name().unwrap(), PLUGIN_NAME);
    }

    #[test]
    fn a_fresh_install_writes_the_plugin_and_the_entry_that_loads_it() {
        let (_home, env) = machine_with_agent();

        let changes = plan(&env);

        let script = written(&changes, &module(&env));
        assert!(sentinel::is_generated(&script), "{script}");
        assert!(
            script.contains(&format!("\"{BINARY}\"")),
            "the module does not name the binary: {script}",
        );
        assert!(
            !script.contains(BINARY_MARK),
            "the module was left as a template: {script}",
        );
        assert!(
            script.contains(r#""emit", "--agent", "hermes""#),
            "the module hands nothing over: {script}",
        );
        assert!(matches!(
            step(&changes, &manifest(&env)),
            Change::Create { .. }
        ));

        let after = written(&changes, &config(&env));
        assert_eq!(after, "plugins:\n  enabled:\n    - agentbus\n", "{after}");
    }

    #[test]
    fn a_path_with_something_to_escape_survives_being_put_in_a_string() {
        let (_home, env) = machine_with_agent();
        let awkward = Path::new("/opt/a \"b\"\\c/agentbus");

        let changes = Hermes
            .plan_install(&env, &State::default(), awkward)
            .expect("planning failed");

        let script = written(&changes, &module(&env));
        assert!(
            script.contains(r#""/opt/a \"b\"\\c/agentbus""#),
            "the path was not escaped: {script}",
        );
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (_home, env) = machine_with_agent();
        let binary = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/agentbus"));

        assert!(matches!(in_python(&binary), Err(Error::Unwritable { .. })));
        assert!(matches!(
            Hermes.plan_install(&env, &State::default(), &binary),
            Err(Error::Unwritable { .. })
        ));
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_refused_and_nothing_is_written() {
        let (_home, env) = machine();

        let refusal = Hermes
            .plan_install(&env, &State::default(), Path::new(BINARY))
            .expect_err("a missing configuration directory has to stop the plan");

        let said = refusal.to_string();
        let home = config_dir(&env);
        assert!(
            said.contains(&home.display().to_string()) && said.contains(AGENT.name()),
            "{said:?} says neither where nor which agent",
        );
        assert!(!home.exists(), "the agent had its home made for it");
    }

    #[test]
    fn a_configuration_kept_by_hand_keeps_its_comments_and_its_own_plugins() {
        let (_home, env) = machine_with_agent();
        let theirs = "# my settings\nmodel: a\n\nplugins:\n  enabled:\n    - theirs   # mine\n";
        put(&config(&env), theirs);

        let mut state = install(&env);

        let after = config_text(&env).expect("no configuration file");
        assert!(
            after.starts_with(theirs),
            "the user's own bytes went: {after}"
        );
        let added: Vec<&str> = after
            .lines()
            .filter(|line| !theirs.lines().any(|mine| mine == *line))
            .collect();
        assert_eq!(added, [format!("    - {PLUGIN_NAME}")], "{after}");

        uninstall(&env, &mut state);
        assert_eq!(
            config_text(&env).as_deref(),
            Some(theirs),
            "uninstalling did not give back the bytes the install was handed",
        );
    }

    #[test]
    fn a_configuration_this_program_cannot_read_is_refused_and_nothing_is_written() {
        // A list written on one line, and a key holding something that is not a
        // list at all: two shapes an editor working by line has no safe way to
        // add to.
        for theirs in [
            "plugins:\n  enabled: [theirs]\n",
            "plugins:\n  enabled: none\n",
        ] {
            let (_home, env) = machine_with_agent();
            put(&config(&env), theirs);

            let refusal = Hermes
                .plan_install(&env, &State::default(), Path::new(BINARY))
                .expect_err("a file that cannot be read has to stop the plan");

            assert!(matches!(refusal, Error::NotListable { .. }), "{refusal:?}");
            // A refused plan is a plan, and a plan changes nothing.
            assert_eq!(config_text(&env).as_deref(), Some(theirs));
            assert!(!module(&env).exists(), "the plugin was written anyway");
        }
    }

    #[test]
    fn installing_a_second_time_changes_nothing() {
        let (_home, env) = machine_with_agent();
        let state = install(&env);
        let before = config_text(&env);

        let again = plan_with(&env, &state);

        assert!(
            again
                .iter()
                .all(|change| matches!(change, Change::Keep { .. } | Change::Setting { .. })),
            "{again:?}",
        );
        assert_eq!(config_text(&env), before);
    }

    #[test]
    fn a_dry_run_says_exactly_what_a_real_one_would_do() {
        let (_home, env) = machine_with_agent();
        let described = plan(&env);

        assert!(!module(&env).exists());
        assert!(!config(&env).exists());

        let carried_out = plan(&env);
        assert_eq!(described, carried_out);

        let mut state = State::default();
        apply(&carried_out, &mut state);
        assert!(module(&env).exists() && manifest(&env).exists() && config(&env).exists());
    }

    #[test]
    fn a_directory_that_is_already_there_is_not_claimed_as_this_programs_own() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(plugin_dir(&env)).unwrap();

        let changes = plan(&env);

        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, Change::Make { .. })),
            "a directory that was there would have been made again: {changes:?}",
        );
    }

    #[test]
    fn uninstalling_leaves_nothing_of_its_own_behind() {
        let (_home, env) = machine_with_agent();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert!(!module(&env).exists(), "the module was left behind");
        assert!(!manifest(&env).exists(), "the plugin file was left behind");
        assert!(
            !plugin_dir(&env).exists() && !plugins_dir(&env).exists(),
            "a directory this program made was left standing empty",
        );
        assert!(
            !config(&env).exists(),
            "a configuration file this program made was left standing empty",
        );
        assert!(
            config_dir(&env).is_dir(),
            "a directory that was never this program's was removed",
        );
    }

    #[test]
    fn the_directories_go_only_when_this_program_was_the_one_that_made_them() {
        let (_home, env) = machine_with_agent();
        fs::create_dir_all(plugin_dir(&env)).unwrap();
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert!(
            plugin_dir(&env).is_dir() && plugins_dir(&env).is_dir(),
            "a directory the user already had was removed",
        );
    }

    #[test]
    fn a_key_this_program_did_not_write_survives_the_entry_being_taken_out() {
        let (_home, env) = machine_with_agent();
        // A `plugins` the user keeps, with nothing under it that this program
        // put there. The key stays; only the name goes.
        let theirs = "plugins:\n  other: 1\n";
        put(&config(&env), theirs);
        let mut state = install(&env);

        uninstall(&env, &mut state);

        assert_eq!(config_text(&env).as_deref(), Some(theirs));
    }

    #[test]
    fn a_file_somebody_else_wrote_stops_the_installation_and_survives_an_uninstall() {
        for path in [module, manifest] {
            let (_home, env) = machine_with_agent();
            let theirs = "# a plugin of my own, which happens to share a name\n";
            let path = path(&env);
            put(&path, theirs);

            let refused = Hermes.plan_install(&env, &State::default(), Path::new(BINARY));
            assert!(matches!(refused, Err(Error::NotOurs { .. })), "{refused:?}");

            let changes = Hermes.plan_uninstall(&env, &State::default()).unwrap();
            assert!(
                matches!(step(&changes, &path), Change::Keep { .. }),
                "{changes:?}",
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), theirs);
        }
    }

    #[test]
    fn a_configuration_that_never_named_the_plugin_is_not_even_parsed() {
        let (_home, env) = machine_with_agent();
        // Not YAML this editor could read. An uninstall with nothing to do to
        // it has no business reading it well enough to complain.
        put(&config(&env), "plugins:\n  enabled: [theirs]\n");

        let changes = plan_entry_removal(&env, &State::default()).expect("planning failed");

        assert!(matches!(step(&changes, &config(&env)), Change::Keep { .. }));
    }

    #[test]
    fn what_is_installed_is_what_the_machine_is_asked_about() {
        let (_home, env) = machine_with_agent();
        assert_eq!(Hermes.status(&env).unwrap(), HookStatus::NotInstalled);

        let mut state = install(&env);
        let expected = expected_version(AGENT);
        assert_eq!(Hermes.status(&env).unwrap(), HookStatus::Current(expected));

        // Each of the other two things the installation is made of, taken away
        // one at a time. The module is still there and still current, and
        // saying so alone would report hooks that never run as working.
        fs::write(config(&env), "plugins:\n  enabled:\n    - theirs\n").unwrap();
        assert_eq!(
            Hermes.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );

        apply(&plan_with(&env, &state), &mut state);
        assert_eq!(Hermes.status(&env).unwrap(), HookStatus::Current(expected));

        fs::remove_file(manifest(&env)).unwrap();
        assert_eq!(
            Hermes.status(&env).unwrap(),
            HookStatus::NeedsRepair(expected)
        );

        apply(&plan_with(&env, &state), &mut state);
        uninstall(&env, &mut state);
        assert_eq!(Hermes.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn a_module_of_somebody_elses_is_not_an_installation_of_this_programs() {
        let (_home, env) = machine_with_agent();
        install(&env);
        fs::write(module(&env), "# mine\n").unwrap();

        assert_eq!(Hermes.status(&env).unwrap(), HookStatus::NotInstalled);
    }

    #[test]
    fn a_module_that_says_nothing_about_which_generation_it_is_is_behind() {
        let (_home, env) = machine_with_agent();
        put(&module(&env), "# _agentbus — generated\n");

        assert_eq!(
            Hermes.status(&env).unwrap(),
            HookStatus::Outdated {
                found: None,
                expected: expected_version(AGENT),
            },
        );

        let changes = plan(&env);
        assert!(matches!(
            step(&changes, &module(&env)),
            Change::Rewrite { .. }
        ));
    }
}
