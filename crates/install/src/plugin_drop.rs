//! Installing for the agents whose whole configuration surface is a directory
//! they load.
//!
//! Three agents here take no entry anywhere. Each of them reads a directory of
//! its own at startup and loads whatever is in it, so an installation is one
//! dropped file and an uninstall is its removal. Writing three installers for
//! that would mean three copies of the same care about marks, refusals and
//! reversal, drifting apart one fix at a time. So there is one installer here
//! and three descriptions of an agent, and a fourth agent that turns up
//! configured this way should be a fourth description rather than a fourth
//! module.
//!
//! What differs between them is small enough to list: where the directory is
//! below the agent's own, what the file in it is called, what is in that file,
//! and — for one pair — the other agent that must not be pointed at the same
//! directory.
//!
//! # Ownership
//!
//! A dropped file has no entries to hang a mark on, so ownership of it is
//! decided by its own first line, exactly as it is for every other whole file
//! this program writes. A file of that name carrying the mark was written here
//! and may be replaced or removed; one without it is somebody's own work that
//! happens to share a name, and this refuses to touch it rather than guessing.
//! The directory goes the same way: only one this program's own record says it
//! made.
//!
//! # A missing configuration directory is a refusal
//!
//! The agent's own directory is never made here. One that is not there means
//! the agent has never run on the machine, and an installer that made one would
//! be creating another program's home on the strength of a guess about its
//! layout — leaving a directory behind that the agent may never read and that an
//! uninstall of *this* program is then responsible for. The directory the
//! plugins go in, one level below it, *is* made: that one is this program's own
//! business to create, and a user who has never installed a plugin will not have
//! it.
//!
//! # There is nothing here to repair
//!
//! Everywhere else in this crate an installation is a file plus an entry
//! pointing at it, and status has to ask whether the entry is still there. Here
//! there is no entry: the file being in the directory is the whole of it. So
//! what the mark inside the file says is the entire answer, and
//! [`HookStatus::NeedsRepair`] is not a state any of these three can be in.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::assets::Asset;
use crate::change::Change;
use crate::paths::{Environment, Platform};
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, json, sentinel};

/// What a plugin says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// One agent that loads a directory, described by what makes it different from
/// the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginDrop {
    /// The agent this installs for.
    agent: Agent,
    /// The directory below its configuration directory that it loads from.
    dir: &'static str,
    /// What the dropped file is called in there.
    file: &'static str,
    /// What is written into it.
    plugin: &'static Asset,
    /// An agent reading the same layout, which a user may have pointed at the
    /// same directory as this one.
    shares_layout_with: Option<Agent>,
}

/// Pi's hooks: one extension in the directory it loads extensions from.
pub static PI: PluginDrop = PluginDrop {
    agent: Agent::Pi,
    dir: "extensions",
    file: "agentbus.ts",
    plugin: &assets::PI_EXTENSION,
    // Named here as well, so that a user who has pointed both agents at one
    // directory is told from whichever of the two they install first.
    shares_layout_with: Some(Agent::Omp),
};

/// Omp's hooks: the same, in the directory its own copy of that layout names.
///
/// Its file carries the agent's name where Pi's does not, because the two may
/// be aimed at one directory and a file has to be certain which agent it speaks
/// for. An installation that could not tell them apart is one an uninstall
/// would take the wrong half of.
pub static OMP: PluginDrop = PluginDrop {
    agent: Agent::Omp,
    dir: "extensions",
    file: "agentbus-omp.ts",
    plugin: &assets::OMP_EXTENSION,
    shares_layout_with: Some(Agent::Pi),
};

/// Kilo's hooks: one plugin in the directory it loads plugins from.
pub static KILO: PluginDrop = PluginDrop {
    agent: Agent::Kilo,
    // Singular, which is what the agent reads. The name of a directory is not
    // this program's to tidy.
    dir: "plugin",
    file: "agentbus.js",
    plugin: &assets::KILO_PLUGIN,
    shares_layout_with: None,
};

impl Installer for PluginDrop {
    fn agent(&self) -> Agent {
        self.agent
    }

    /// Drops the file in, and makes the directory it goes in if it is not
    /// there.
    fn plan_install(
        &self,
        env: &Environment,
        _state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        let home = self.agent.config_dir(env);
        if !home.is_dir() {
            return Err(Error::Absent {
                agent: self.agent,
                path: home,
            });
        }
        self.alone(env)?;
        let mut changes = Vec::new();
        // Writing a file makes the directory above it anyway; this is here so
        // that the making is *recorded*, which is the one thing the uninstall
        // cannot work out for itself later.
        let dir = self.dir(env);
        if !dir.is_dir() {
            changes.push(Change::Make { path: dir });
        }
        changes.push(self.plan_plugin(env, binary)?);
        Ok(changes)
    }

    /// Takes the file away, and then the directory if this program made it.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let path = self.plugin(env);
        // A file without the mark is passed over here rather than refused,
        // unlike on the way in: an uninstall that stopped because something of
        // the user's is in the way would be refusing to do the one thing it can
        // certainly do safely, which is nothing.
        let mut changes = vec![match read(&path)? {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        }];
        // Only the directory the record says this program made: one the user
        // already had is theirs however empty this leaves it, and it holds
        // their own plugins in any case. The agent's own configuration
        // directory is never among them, because an installation that found it
        // missing refused rather than making one.
        let dir = self.dir(env);
        if state.ownership(&dir) == Some(Ownership::Created) {
            changes.push(Change::Clear { path: dir });
        }
        Ok(changes)
    }

    /// Reads the file, and that is the whole of it.
    ///
    /// There is no entry anywhere pointing at it — the agent loads the
    /// directory — so there is nothing else to confirm and nothing that can be
    /// in need of repair. What the mark inside the file says is the answer.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let Some(text) = read(&self.asset(env))? else {
            return Ok(HookStatus::NotInstalled);
        };
        if !sentinel::is_generated(&text) {
            return Ok(HookStatus::NotInstalled);
        }
        Ok(HookStatus::of_text(self.agent, &text))
    }

    /// The plugin this program drops in, which is the file the mark is in.
    fn asset(&self, env: &Environment) -> PathBuf {
        self.plugin(env)
    }
}

impl PluginDrop {
    /// The directory the agent loads.
    fn dir(&self, env: &Environment) -> PathBuf {
        self.agent.config_dir(env).join(self.dir)
    }

    /// The file this program drops in there.
    fn plugin(&self, env: &Environment) -> PathBuf {
        self.dir(env).join(self.file)
    }

    /// Refuses when another agent reading the same layout has been pointed at
    /// this agent's directory.
    ///
    /// Both agents load everything they find, so one directory serving two of
    /// them is a machine where each would load the other's plugin and report
    /// its own sessions under the other's name. Nothing this program writes can
    /// prevent that from inside the directory, so the answer is to say which
    /// two agents have been aimed at one place and stop. A user who wants both
    /// gives them the separate directories their own settings are there to
    /// provide.
    fn alone(&self, env: &Environment) -> Result<(), Error> {
        let Some(other) = self.shares_layout_with else {
            return Ok(());
        };
        let path = self.agent.config_dir(env);
        match other.config_dir(env) == path {
            true => Err(Error::Shared {
                agent: self.agent,
                other,
                path,
            }),
            false => Ok(()),
        }
    }

    /// What the plugin should hold, with the binary written into it.
    fn generated(&self, platform: Platform, binary: &Path) -> Result<String, Error> {
        Ok(self
            .plugin
            .text(platform)
            .replace(BINARY_MARK, &in_javascript(binary)?))
    }

    /// What writing the plugin would do, given whatever is at its path now.
    ///
    /// A file of that name this program did not write stops the plan. It is the
    /// one case where doing nothing is worse than saying so: silently leaving it
    /// would report a successful installation of hooks that are not installed,
    /// and silently replacing it would delete something a user wrote.
    fn plan_plugin(&self, env: &Environment, binary: &Path) -> Result<Change, Error> {
        let path = self.plugin(env);
        let contents = self.generated(env.platform(), binary)?;
        Ok(match read(&path)? {
            None => Change::Create {
                path,
                contents,
                // Nothing runs this file; the agent reads it and interprets it
                // with the runtime it brought with it.
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
    use crate::agent::{PI_AGENT_DIR_VAR, PI_CONFIG_DIR_VAR};
    use crate::assets::tests::is_well_formed;
    use crate::{Mode, Outcome, version};

    /// Every agent this module installs for.
    const ALL: [&PluginDrop; 3] = [&PI, &OMP, &KILO];

    /// The binary a test installation names.
    const BINARY: &str = "/opt/bin/agentbus";

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// The same machine, with `agent`'s configuration directory on it.
    fn ready(agent: &PluginDrop) -> (tempfile::TempDir, Environment) {
        let (home, env) = machine();
        fs::create_dir_all(agent.agent.config_dir(&env)).expect("cannot make the directory");
        (home, env)
    }

    /// What installing on `env` would do, with nothing recorded before.
    fn plan(agent: &PluginDrop, env: &Environment) -> Vec<Change> {
        agent
            .plan_install(env, &State::default(), Path::new(BINARY))
            .expect("planning failed")
    }

    /// Installs for real, so that a later plan is planned against a machine an
    /// install has actually happened on.
    fn install(agent: &PluginDrop, env: &Environment) -> State {
        let mut state = State::default();
        for change in plan(agent, env) {
            change.apply(agent.agent, &mut state).expect("applying");
        }
        state
    }

    /// Uninstalls for real, against the record an install left.
    fn uninstall(agent: &PluginDrop, env: &Environment, state: &mut State) -> Vec<Change> {
        let changes = agent
            .plan_uninstall(env, state)
            .expect("planning the uninstall failed");
        for change in &changes {
            change.apply(agent.agent, state).expect("applying");
        }
        changes
    }

    /// What a plan would write to `path`, whether or not there was a file there.
    fn written(changes: &[Change], path: &Path) -> String {
        let step = changes
            .iter()
            .find(|change| change.path() == Some(path))
            .unwrap_or_else(|| panic!("no step for {}: {changes:?}", path.display()));
        match step {
            Change::Create { contents, .. } | Change::Rewrite { contents, .. } => contents.clone(),
            other => panic!("{} was {other:?}", path.display()),
        }
    }

    #[test]
    fn every_plugin_obeys_the_rules_every_installed_file_obeys() {
        for agent in ALL {
            is_well_formed(agent.agent, agent.plugin);
        }
    }

    #[test]
    fn each_plugin_goes_where_its_agent_loads_one_from() {
        let env = Environment::rooted("/home/u");
        let documented = [
            (&PI, "/home/u/.pi/agent/extensions/agentbus.ts"),
            (&OMP, "/home/u/.omp/agent/extensions/agentbus-omp.ts"),
            (&KILO, "/home/u/.config/kilo/plugin/agentbus.js"),
        ];

        assert_eq!(documented.len(), ALL.len(), "an agent is missing a place");
        for (agent, path) in documented {
            assert_eq!(agent.plugin(&env), PathBuf::from(path), "{}", agent.agent);
        }
    }

    #[test]
    fn installing_writes_the_plugin_and_makes_the_directory_it_goes_in() {
        for agent in ALL {
            let (_home, env) = ready(agent);

            let changes = plan(agent, &env);

            assert!(
                changes.contains(&Change::Make {
                    path: agent.dir(&env)
                }),
                "{}: the directory was not recorded as made: {changes:?}",
                agent.agent,
            );
            let plugin = written(&changes, &agent.plugin(&env));
            assert!(
                plugin.contains(&format!("\"{BINARY}\"")),
                "{}: does not name the binary: {plugin}",
                agent.agent,
            );
            assert!(
                !plugin.contains(BINARY_MARK),
                "{}: was left as a template: {plugin}",
                agent.agent,
            );
            assert!(
                plugin.contains(&format!(r#""emit", "--agent", "{}""#, agent.agent.name())),
                "{}: does not emit under its own name: {plugin}",
                agent.agent,
            );
            assert!(
                sentinel::is_generated(&plugin),
                "{}: was left unmarked: {plugin}",
                agent.agent,
            );
        }
    }

    #[test]
    fn an_agent_that_is_not_on_the_machine_is_said_to_be_missing_rather_than_installed_for() {
        for agent in ALL {
            let (_home, env) = machine();

            let refusal = agent
                .plan_install(&env, &State::default(), Path::new(BINARY))
                .expect_err("planning should have refused");

            assert!(
                matches!(refusal, Error::Absent { agent: named, ref path }
                    if named == agent.agent && *path == agent.agent.config_dir(&env)),
                "{}: {refusal:?}",
                agent.agent,
            );
            assert!(
                !agent.dir(&env).exists(),
                "{}: the directory was made anyway",
                agent.agent,
            );
        }
    }

    #[test]
    fn a_plan_is_a_plan_until_it_is_carried_out() {
        for agent in ALL {
            let (_home, env) = ready(agent);

            let outcomes = crate::install(&env, &[agent.agent], Mode::DryRun).expect("planning");

            assert!(outcomes.iter().any(Outcome::is_change), "{}", agent.agent);
            assert!(
                !agent.plugin(&env).exists(),
                "{}: a dry run wrote the plugin",
                agent.agent,
            );
            assert!(
                !agent.dir(&env).exists(),
                "{}: a dry run made the directory",
                agent.agent,
            );
        }
    }

    #[test]
    fn installing_twice_changes_nothing_the_second_time() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            install(agent, &env);

            let changes = plan(agent, &env);

            assert_eq!(
                changes,
                vec![Change::Keep {
                    path: agent.plugin(&env)
                }],
                "{}",
                agent.agent,
            );
        }
    }

    #[test]
    fn a_plugin_this_program_wrote_at_another_generation_is_written_again() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            let path = agent.plugin(&env);
            fs::create_dir_all(path.parent().expect("a file has a directory")).unwrap();
            fs::write(&path, format!("// {} — an older one\n", sentinel::KEY)).unwrap();

            let changes = plan(agent, &env);

            assert!(
                matches!(changes.last(), Some(Change::Rewrite { path: at, .. }) if *at == path),
                "{}: {changes:?}",
                agent.agent,
            );
        }
    }

    #[test]
    fn a_file_of_the_same_name_that_somebody_else_wrote_stops_the_install() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            let path = agent.plugin(&env);
            let theirs = "// a plugin of my own\n";
            fs::create_dir_all(path.parent().expect("a file has a directory")).unwrap();
            fs::write(&path, theirs).unwrap();

            let refusal = agent
                .plan_install(&env, &State::default(), Path::new(BINARY))
                .expect_err("planning should have refused");

            assert!(
                matches!(refusal, Error::NotOurs { path: ref at } if *at == path),
                "{}: {refusal:?}",
                agent.agent,
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                theirs,
                "{}",
                agent.agent
            );
        }
    }

    #[test]
    fn a_file_of_the_same_name_that_somebody_else_wrote_survives_an_uninstall() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            let path = agent.plugin(&env);
            let theirs = "// a plugin of my own\n";
            fs::create_dir_all(path.parent().expect("a file has a directory")).unwrap();
            fs::write(&path, theirs).unwrap();

            let changes = uninstall(agent, &env, &mut State::default());

            assert_eq!(
                changes,
                vec![Change::Keep { path: path.clone() }],
                "{}",
                agent.agent,
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                theirs,
                "{}",
                agent.agent
            );
        }
    }

    #[test]
    fn uninstalling_takes_away_everything_installing_put_there() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            let before = walk(&env);
            let mut state = install(agent, &env);
            assert_ne!(walk(&env), before, "{}: nothing was installed", agent.agent);

            uninstall(agent, &env, &mut state);

            assert_eq!(walk(&env), before, "{}", agent.agent);
        }
    }

    #[test]
    fn a_directory_the_user_already_had_survives_an_uninstall() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            let dir = agent.dir(&env);
            fs::create_dir_all(&dir).unwrap();
            let mut state = install(agent, &env);

            uninstall(agent, &env, &mut state);

            assert!(dir.is_dir(), "{}: {} went", agent.agent, dir.display());
        }
    }

    #[test]
    fn two_agents_sharing_one_directory_are_named_rather_than_installed_over() {
        let (home, env) = machine();
        let shared = home.path().join("agent");
        let env = env.with_var(PI_AGENT_DIR_VAR, &shared);
        fs::create_dir_all(&shared).unwrap();
        assert_eq!(PI.agent.config_dir(&env), OMP.agent.config_dir(&env));

        for agent in [&PI, &OMP] {
            let refusal = agent
                .plan_install(&env, &State::default(), Path::new(BINARY))
                .expect_err("planning should have refused");

            assert!(
                matches!(refusal, Error::Shared { agent: named, other, ref path }
                    if named == agent.agent && other != agent.agent && *path == shared),
                "{}: {refusal:?}",
                agent.agent,
            );
        }
        assert!(
            !shared.join("extensions").exists(),
            "a refused install made a directory"
        );

        // Given directories of their own, both install, and neither file is the
        // other's to remove — the names differ as well as the directories, so a
        // machine that later has both aimed at one place still has two files
        // each certain which agent it speaks for.
        let (_apart_home, apart) = machine();
        let apart = apart.with_var(PI_CONFIG_DIR_VAR, ".omp");
        assert_ne!(PI.agent.config_dir(&apart), OMP.agent.config_dir(&apart));
        for agent in [&PI, &OMP] {
            fs::create_dir_all(agent.agent.config_dir(&apart)).unwrap();
            install(agent, &apart);
        }
        assert_ne!(
            PI.plugin(&apart).file_name(),
            OMP.plugin(&apart).file_name()
        );
    }

    #[test]
    fn what_is_on_the_machine_is_read_off_the_file_and_nothing_else() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            assert_eq!(
                agent.status(&env).expect("reading"),
                HookStatus::NotInstalled,
                "{}",
                agent.agent,
            );

            install(agent, &env);
            assert_eq!(
                agent.status(&env).expect("reading"),
                HookStatus::Current(version::expected_version(agent.agent)),
                "{}",
                agent.agent,
            );

            // Nothing else on the machine can take that answer away, because
            // nothing else on the machine is part of the installation.
            fs::write(agent.plugin(&env), format!("// {}\n", sentinel::KEY)).unwrap();
            assert_eq!(
                agent.status(&env).expect("reading"),
                HookStatus::Outdated {
                    found: None,
                    expected: version::expected_version(agent.agent),
                },
                "{}",
                agent.agent,
            );

            // And a file somebody else wrote is not an installation at all.
            fs::write(agent.plugin(&env), "// mine\n").unwrap();
            assert_eq!(
                agent.status(&env).expect("reading"),
                HookStatus::NotInstalled,
                "{}",
                agent.agent,
            );
        }
    }

    #[test]
    fn a_path_with_something_to_escape_survives_being_put_in_a_string() {
        for agent in ALL {
            let (_home, env) = ready(agent);
            let awkward = Path::new("/opt/a \"b\"\\c/agentbus");

            let changes = agent
                .plan_install(&env, &State::default(), awkward)
                .expect("planning failed");

            let plugin = written(&changes, &agent.plugin(&env));
            assert!(
                plugin.contains(r#""/opt/a \"b\"\\c/agentbus""#),
                "{}: was not escaped: {plugin}",
                agent.agent,
            );
        }
    }

    /// Every path below the home directory of `env`, sorted.
    fn walk(env: &Environment) -> Vec<PathBuf> {
        fn below(dir: &Path, found: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    below(&path, found);
                }
                found.push(path);
            }
        }

        let mut found = Vec::new();
        below(env.home(), &mut found);
        found.sort();
        found
    }
}
