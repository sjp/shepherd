//! Installing for OpenCode.
//!
//! OpenCode has no hooks file to merge into. It loads every script it finds in a
//! plugin directory, so installing is dropping one file in and uninstalling is
//! taking that file back out — no document to parse, nothing of anybody else's
//! in the same file, and no way for this program to damage a configuration by
//! rewriting it.
//!
//! What that costs is the one thing a merge gave for free: a file has no entries
//! to hang a mark on, so ownership is decided by the file's own first line. A
//! script of this name carrying the mark was written here and may be replaced or
//! removed; one without it is somebody's own work that happens to share a name,
//! and this refuses to touch it rather than guessing. The same rule decides
//! whether the directory goes at the end: it is removed only if this program
//! made it, because a plugin directory the user already had is theirs however
//! empty this leaves it.
//!
//! Only the user's own plugin directory is written to. OpenCode reads a second
//! one from inside a project, and that one is checked into version control and
//! shared with everybody who clones the repository — installing one person's
//! local tooling into it would be installing it for their colleagues too.
//!
//! The script names this program's binary by an absolute path rather than by the
//! command `agentbus`, for the reason every agent's installation does: the
//! directory a user installed it into is not guaranteed to be on the `PATH`
//! their agent runs plugins with, and a command that cannot be found fails
//! silently, in the one place nobody is looking.

use std::path::{Path, PathBuf};

use crate::agent::Agent;
use crate::change::Change;
use crate::paths::Environment;
use crate::state::{Ownership, State};
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, json, sentinel};

/// The directory inside OpenCode's configuration that plugins are loaded from.
const PLUGIN_DIR: &str = "plugin";

/// What the dropped script is called.
///
/// Named after this program, because everything in that directory is loaded and
/// a user looking at the list should be able to tell at a glance what each of
/// them is and what to remove to be rid of it.
const PLUGIN_FILE: &str = "agentbus.js";

/// What the template says where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// OpenCode's plugin, as a single script dropped into its plugin directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenCode;

impl Installer for OpenCode {
    fn agent(&self) -> Agent {
        Agent::OpenCode
    }

    /// Writes the script, making the directory it goes in if it is not there.
    ///
    /// Writing a file creates the directories above it anyway, so the first step
    /// exists to *record* that this program was the one that made it — which is
    /// what the uninstall cannot work out for itself later.
    fn plan_install(
        &self,
        env: &Environment,
        _state: &State,
        binary: &Path,
    ) -> Result<Vec<Change>, Error> {
        let dir = plugin_dir(env);
        let contents = generated(binary)?;

        let mut changes = Vec::new();
        if !dir.is_dir() {
            changes.push(Change::Make { path: dir.clone() });
        }
        changes.push(plan_file(&plugin(&dir), &contents)?);
        Ok(changes)
    }

    /// Takes the script away, and the directory with it if this program made it.
    fn plan_uninstall(&self, env: &Environment, state: &State) -> Result<Vec<Change>, Error> {
        let dir = plugin_dir(env);
        let path = plugin(&dir);

        // A file without the mark is refused here rather than reported as an
        // error, unlike on the way in: an uninstall that stopped because
        // something of the user's is in the way would be refusing to do the one
        // thing it can definitely do safely, which is nothing.
        let existing = read(&path)?;
        let mut changes = vec![match existing {
            Some(text) if sentinel::is_generated(&text) => Change::Delete { path },
            _ => Change::Keep { path },
        }];
        if state.ownership(&dir) == Some(Ownership::Created) {
            changes.push(Change::Clear { path: dir });
        }
        Ok(changes)
    }

    /// Reads the dropped script, if the script there is this program's.
    ///
    /// There is nothing else to check. OpenCode loads whatever is in that
    /// directory, so a script that is there is a script that runs, and the file
    /// is the whole of the installation. A file of that name without the mark is
    /// somebody else's, and nothing of this program's is installed.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let Some(text) = read(&plugin(&plugin_dir(env)))? else {
            return Ok(HookStatus::NotInstalled);
        };
        Ok(match sentinel::is_generated(&text) {
            true => HookStatus::of_text(self.agent(), &text),
            false => HookStatus::NotInstalled,
        })
    }
}

/// The directory OpenCode loads this user's plugins from.
fn plugin_dir(env: &Environment) -> PathBuf {
    Agent::OpenCode.config_dir(env).join(PLUGIN_DIR)
}

/// The script this program drops in there.
fn plugin(dir: &Path) -> PathBuf {
    dir.join(PLUGIN_FILE)
}

/// What the script should hold, with the binary written into it.
fn generated(binary: &Path) -> Result<String, Error> {
    Ok(assets::OPENCODE_PLUGIN.replace(BINARY_MARK, &in_javascript(binary)?))
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

/// What writing the script would do, given whatever is at `path` now.
///
/// A file of this name that this program did not write stops the plan. It is the
/// one case where doing nothing is worse than saying so: silently leaving it
/// would report a successful installation of hooks that are not installed, and
/// silently replacing it would delete something a user wrote.
fn plan_file(path: &Path, contents: &str) -> Result<Change, Error> {
    Ok(match read(path)? {
        None => Change::Create {
            path: path.to_owned(),
            contents: contents.to_owned(),
            executable: false,
        },
        Some(text) if !sentinel::is_generated(&text) => {
            return Err(Error::NotOurs {
                path: path.to_owned(),
            });
        }
        Some(text) if text == contents => Change::Keep {
            path: path.to_owned(),
        },
        Some(_) => Change::Rewrite {
            path: path.to_owned(),
            contents: contents.to_owned(),
            executable: false,
        },
    })
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
    use super::*;

    /// A machine with a home directory that really exists and holds nothing.
    fn machine() -> (tempfile::TempDir, Environment) {
        let home = tempfile::tempdir().expect("cannot make a temporary directory");
        let env = Environment::rooted(home.path());
        (home, env)
    }

    /// What installing would put in the script, on a machine with none.
    fn written(env: &Environment, binary: &str) -> String {
        let changes = OpenCode
            .plan_install(env, &State::default(), Path::new(binary))
            .expect("planning failed");
        match changes.as_slice() {
            [
                Change::Make { path: dir },
                Change::Create { path, contents, .. },
            ] => {
                assert_eq!(dir, &plugin_dir(env));
                assert_eq!(path, &plugin(&plugin_dir(env)));
                contents.clone()
            }
            other => panic!("a machine with no plugin should get one: {other:?}"),
        }
    }

    #[test]
    fn the_script_goes_in_the_users_own_plugin_directory_and_nowhere_else() {
        assert_eq!(
            plugin(&plugin_dir(&Environment::rooted("/home/u"))),
            PathBuf::from("/home/u/.config/opencode/plugin/agentbus.js")
        );
    }

    #[test]
    fn the_script_names_the_binary_by_the_path_it_was_given() {
        let (_home, env) = machine();

        let script = written(&env, "/opt/bin/agentbus");

        assert!(
            script.contains("\"/opt/bin/agentbus\""),
            "the binary is not named: {script}"
        );
        assert!(
            !script.contains(BINARY_MARK),
            "the template was left as it was: {script}"
        );
        assert!(
            script.contains("emit --agent opencode"),
            "the script does not emit: {script}"
        );
    }

    #[test]
    fn the_script_carries_the_mark_that_makes_it_findable_again() {
        let (_home, env) = machine();

        assert!(sentinel::is_generated(&written(&env, "/opt/bin/agentbus")));
    }

    #[test]
    fn a_path_with_something_to_escape_survives_being_put_in_a_string() {
        let (_home, env) = machine();

        let script = written(&env, "/opt/a \"b\"\\c/agentbus");

        assert!(
            script.contains(r#""/opt/a \"b\"\\c/agentbus""#),
            "the path was not escaped: {script}"
        );
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
        std::fs::create_dir_all(home.path().join(".config/opencode/plugin")).unwrap();

        let changes = OpenCode
            .plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"))
            .expect("planning failed");

        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, Change::Make { .. })),
            "a directory that was there would have been made again: {changes:?}"
        );
    }

    #[test]
    fn a_script_of_this_programs_own_is_replaced_when_it_has_gone_stale() {
        let (_home, env) = machine();
        let path = plugin(&plugin_dir(&env));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, generated(Path::new("/where/it/used/to/be")).unwrap()).unwrap();

        let stale = plan_file(&path, &generated(Path::new("/opt/bin/agentbus")).unwrap());

        assert!(matches!(stale, Ok(Change::Rewrite { .. })), "{stale:?}");
    }

    #[test]
    fn a_script_that_is_already_right_is_left_exactly_as_it_is() {
        let (_home, env) = machine();
        let path = plugin(&plugin_dir(&env));
        let contents = generated(Path::new("/opt/bin/agentbus")).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &contents).unwrap();

        assert!(matches!(
            plan_file(&path, &contents),
            Ok(Change::Keep { .. })
        ));
    }

    #[test]
    fn a_script_somebody_else_wrote_stops_the_installation() {
        let (_home, env) = machine();
        let path = plugin(&plugin_dir(&env));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "// a plugin of my own, which happens to share a name\n",
        )
        .unwrap();

        let refused =
            OpenCode.plan_install(&env, &State::default(), Path::new("/opt/bin/agentbus"));

        assert!(matches!(refused, Err(Error::NotOurs { .. })), "{refused:?}");
    }

    #[test]
    fn uninstalling_takes_away_this_programs_own_script_and_leaves_anybody_elses() {
        let (_home, env) = machine();
        let path = plugin(&plugin_dir(&env));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let state = State::default();

        std::fs::write(&path, generated(Path::new("/opt/bin/agentbus")).unwrap()).unwrap();
        assert!(matches!(
            OpenCode.plan_uninstall(&env, &state).unwrap().as_slice(),
            [Change::Delete { .. }]
        ));

        std::fs::write(&path, "// mine\n").unwrap();
        assert!(matches!(
            OpenCode.plan_uninstall(&env, &state).unwrap().as_slice(),
            [Change::Keep { .. }]
        ));
    }

    #[test]
    fn the_plugin_directory_goes_only_when_this_program_was_the_one_that_made_it() {
        let (_home, env) = machine();
        let dir = plugin_dir(&env);

        let untouched = OpenCode.plan_uninstall(&env, &State::default()).unwrap();
        assert!(
            !untouched
                .iter()
                .any(|change| matches!(change, Change::Clear { .. })),
            "a directory nobody claimed was going to be removed: {untouched:?}"
        );

        let mut ours = State::default();
        ours.record(&dir, Agent::OpenCode, Ownership::Created);
        let changes = OpenCode.plan_uninstall(&env, &ours).unwrap();
        assert!(
            changes.contains(&Change::Clear { path: dir }),
            "the directory this program made was left behind: {changes:?}"
        );
    }
}
