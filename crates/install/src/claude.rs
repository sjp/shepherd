//! Installing for Claude Code.
//!
//! Claude reads hooks from a plugin: a directory holding a manifest and a
//! `hooks/hooks.json` in the same shape as the `hooks` block of its settings
//! file. That is the mechanism to use, because the alternatives are both files
//! somebody else maintains — the user's own settings, or a project's settings,
//! which is worse still for being shared with their colleagues through version
//! control.
//!
//! A plugin is not installed by being written somewhere, though. Claude installs
//! plugins from *marketplaces*, and a marketplace is itself just a directory
//! with a manifest listing the plugins in it. So what this generates is a
//! marketplace of exactly one plugin, and what it then does is ask Claude — with
//! Claude's own command line, the only supported way — to add that marketplace
//! and install that plugin from it. Registering it is Claude's business, written
//! wherever Claude keeps such things, and this program neither knows nor touches
//! where that is.
//!
//! The generated files name this program's own binary by an absolute path, and
//! that is what makes an installation go stale: a binary that moved leaves hooks
//! pointing at nothing. Claude copies a plugin's files when it installs them and
//! only refreshes that copy when the version changes, so a reinstall at the same
//! version would silently keep the old copy. This therefore compares what Claude
//! actually installed against what it would generate now, and when they differ
//! it removes the plugin and installs it again — which is the only sequence that
//! makes Claude take a new copy.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent::Agent;
use crate::change::Change;
use crate::command::Invocation;
use crate::paths::Environment;
use crate::state::State;
use crate::status::HookStatus;
use crate::{Error, Installer, assets, file, json};

/// What the plugin, and the marketplace offering it, are both called.
///
/// One name for both because there is one of each and they are the same thing
/// twice: a user reading either name in Claude's own output should see this
/// program's name and nothing to work out.
const NAME: &str = "agentbus";

/// The directory the marketplace is generated into, below the data directory.
const DIR_NAME: &str = "claude-marketplace";

/// The scope every registration is made at.
///
/// The user's, rather than a project's: a project's configuration is shared with
/// everyone who checks the repository out, and installing one person's local
/// tooling into it would be installing it for their colleagues too.
const SCOPE: &str = "user";

/// What the templates say where this program's own binary belongs.
const BINARY_MARK: &str = "@BINARY@";

/// What the templates say where the plugin's version belongs.
const VERSION_MARK: &str = "@VERSION@";

/// The version the generated plugin carries, which is this program's own.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Claude Code's hooks, as a plugin offered by a marketplace of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claude;

impl Installer for Claude {
    fn agent(&self) -> Agent {
        Agent::Claude
    }

    /// Writes the marketplace, then has Claude register it and install from it.
    ///
    /// In that order, because the second half reads the first: a marketplace
    /// Claude is asked about before it exists is a marketplace Claude refuses.
    fn plan_install(&self, env: &Environment, binary: &Path) -> Result<Vec<Change>, Error> {
        let root = root(env);
        let generated = generated(&root, binary)?;

        let mut changes = Vec::new();
        for (path, contents) in &generated {
            changes.push(plan_file(path, contents)?);
        }
        changes.extend(plan_registration(env, &root, &generated));
        Ok(changes)
    }

    /// Has Claude forget the plugin and the marketplace, then removes what was
    /// generated for them.
    ///
    /// In that order, for the same reason reversed: Claude is being asked about
    /// a marketplace that is still on disk, and asking it afterwards would be
    /// asking about a directory that is no longer there.
    ///
    /// The files go whether or not the record says this program created them.
    /// Everywhere else that question decides between deleting litter and
    /// deleting somebody's own file, but this whole directory is generated: it
    /// is below this program's own data directory, nothing else writes to it,
    /// and a lost record is not a reason to leave it behind.
    fn plan_uninstall(&self, env: &Environment, _state: &State) -> Result<Vec<Change>, Error> {
        let root = root(env);

        let mut changes = plan_deregistration(env, &root);
        for path in paths(&root) {
            changes.push(match path.exists() {
                true => Change::Delete { path },
                false => Change::Keep { path },
            });
        }
        changes.push(match root.exists() {
            true => Change::Clear { path: root },
            false => Change::Keep { path: root },
        });
        Ok(changes)
    }

    /// Reads the generated hooks, and looks for the rest of the marketplace
    /// around them.
    ///
    /// The hooks are the file that says which generation this is, because they
    /// are the one whose content is the installation; the two manifests exist to
    /// get Claude to read them. A hooks file with no manifests beside it is one
    /// Claude will never be offered, which is a working file in a broken
    /// installation — exactly the case worth telling somebody about.
    fn status(&self, env: &Environment) -> Result<HookStatus, Error> {
        let root = root(env);
        let [marketplace, plugin, hooks] = paths(&root);

        let status = HookStatus::of_asset(self.agent(), &hooks)?;
        Ok(status.confirmed(marketplace.is_file() && plugin.is_file()))
    }
}

/// Where the marketplace is generated.
fn root(env: &Environment) -> PathBuf {
    env.data_dir().join(DIR_NAME)
}

/// Every file the marketplace is made of, in the order Claude's own layout
/// suggests, so that a report of what was written reads like a walk of the
/// directory rather than like the order the code happened to be written in.
fn paths(root: &Path) -> [PathBuf; 3] {
    let plugin = plugin_dir(root);
    [
        root.join(".claude-plugin").join("marketplace.json"),
        plugin.join(".claude-plugin").join("plugin.json"),
        plugin.join("hooks").join("hooks.json"),
    ]
}

/// Where the plugin itself sits inside the marketplace.
fn plugin_dir(root: &Path) -> PathBuf {
    root.join(NAME)
}

/// Every file the marketplace is made of, and what should be in it.
fn generated(root: &Path, binary: &Path) -> Result<Vec<(PathBuf, String)>, Error> {
    let binary = in_json(binary)?;
    let contents = [
        assets::CLAUDE_MARKETPLACE.to_owned(),
        assets::CLAUDE_PLUGIN.replace(VERSION_MARK, VERSION),
        assets::CLAUDE_HOOKS.replace(BINARY_MARK, &binary),
    ];
    Ok(paths(root).into_iter().zip(contents).collect())
}

/// A path as it is written inside a JSON string.
///
/// A path is bytes and a JSON string is text, so a path that is not text cannot
/// be put in one. That is refused rather than approximated: the lossy spelling
/// of it would parse, install, and produce hooks that run a command which is not
/// there — a failure that shows up as an agent quietly emitting nothing, which
/// is the hardest kind of failure to attribute to its cause.
fn in_json(path: &Path) -> Result<String, Error> {
    let text = path.to_str().ok_or_else(|| Error::Unwritable {
        path: path.to_owned(),
    })?;
    Ok(json::escaped(text))
}

/// What writing one generated file would do.
fn plan_file(path: &Path, contents: &str) -> Result<Change, Error> {
    let existing = file::read(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    Ok(match existing {
        Some(text) if text == contents => Change::Keep {
            path: path.to_owned(),
        },
        Some(_) => Change::Rewrite {
            path: path.to_owned(),
            contents: contents.to_owned(),
            executable: false,
        },
        None => Change::Create {
            path: path.to_owned(),
            contents: contents.to_owned(),
            executable: false,
        },
    })
}

/// What asking Claude to install the plugin would do.
///
/// Each of the two steps is skipped only when Claude itself says it has already
/// happened. Neither answer is required: a Claude that cannot be run, or that
/// answers with something this does not understand, means both steps are
/// planned, and both are safe to take when they were not needed.
fn plan_registration(
    env: &Environment,
    root: &Path,
    generated: &[(PathBuf, String)],
) -> Vec<Change> {
    let claude = command(env);
    let mut changes = Vec::new();

    let add = plugin(&claude, ["marketplace", "add", &root.to_string_lossy()]);
    changes.push(match knows_marketplace(&claude, root) {
        true => Change::Ran { command: add },
        false => Change::Run { command: add },
    });

    let install = plugin(&claude, ["install", &id(), "-s", SCOPE]);
    match installed(&claude) {
        // Installed from a copy Claude took of what was generated last time.
        // Claude refreshes that copy on a version change and on nothing else,
        // so the only way to hand it a new one at the same version is to have
        // it forget the plugin first.
        Some(copy) if !matches(&copy, root, generated) => {
            changes.push(Change::Run {
                command: plugin(&claude, ["uninstall", &id(), "-s", SCOPE]),
            });
            changes.push(Change::Run { command: install });
        }
        Some(_) => changes.push(Change::Ran { command: install }),
        None => changes.push(Change::Run { command: install }),
    }
    changes
}

/// What asking Claude to forget the plugin would do.
fn plan_deregistration(env: &Environment, root: &Path) -> Vec<Change> {
    let claude = command(env);

    let remove = plugin(&claude, ["uninstall", &id(), "-s", SCOPE]);
    let forget = plugin(&claude, ["marketplace", "remove", NAME]);
    vec![
        match installed(&claude).is_some() {
            true => Change::Run { command: remove },
            false => Change::Ran { command: remove },
        },
        match knows_marketplace(&claude, root) {
            true => Change::Run { command: forget },
            false => Change::Ran { command: forget },
        },
    ]
}

/// Whether Claude already offers the marketplace generated at `root`.
///
/// The path is part of the question. A marketplace of this name pointing
/// somewhere else is one this program did not put there, or one left by an
/// installation whose data directory has since moved, and in both cases adding
/// it again is what makes the answer right.
fn knows_marketplace(claude: &Path, root: &Path) -> bool {
    let Some(answer) = plugin(claude, ["marketplace", "list", "--json"]).ask() else {
        return false;
    };
    let Ok(Value::Array(marketplaces)) = serde_json::from_str::<Value>(&answer) else {
        return false;
    };
    marketplaces.iter().any(|marketplace| {
        marketplace.get("name").and_then(Value::as_str) == Some(NAME)
            && marketplace.get("path").and_then(Value::as_str) == root.to_str()
    })
}

/// Where Claude keeps its copy of the plugin, if it has one installed.
fn installed(claude: &Path) -> Option<PathBuf> {
    let answer = plugin(claude, ["list", "--json"]).ask()?;
    let Ok(Value::Array(plugins)) = serde_json::from_str::<Value>(&answer) else {
        return None;
    };
    plugins
        .iter()
        .find(|plugin| plugin.get("id").and_then(Value::as_str) == Some(&id()))
        .and_then(|plugin| plugin.get("installPath"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

/// Whether Claude's copy of the plugin, at `copy`, holds what would be
/// generated now.
///
/// Only the plugin's own files are compared, because only they are copied: the
/// marketplace manifest stays where it was generated and is read from there. A
/// file that cannot be read counts as different, which errs towards installing
/// again — the outcome that is merely wasteful rather than wrong.
fn matches(copy: &Path, root: &Path, generated: &[(PathBuf, String)]) -> bool {
    let plugin = plugin_dir(root);
    generated.iter().all(|(path, contents)| {
        let Ok(relative) = path.strip_prefix(&plugin) else {
            return true;
        };
        file::read(&copy.join(relative)).ok().flatten().as_deref() == Some(contents.as_str())
    })
}

/// How Claude is named when it is run, which is where it was found if it was
/// found at all.
///
/// A Claude that is not on the search path is still named, by the command a user
/// would type. Nothing can be asked of it and every step is planned, and
/// carrying one out fails with that command line in the message — which is what
/// somebody whose Claude is installed where this program cannot see it needs in
/// order to finish the job themselves.
fn command(env: &Environment) -> PathBuf {
    Agent::Claude
        .command(env)
        .unwrap_or_else(|| PathBuf::from(Agent::Claude.name()))
}

/// One `claude plugin` command.
fn plugin<'a>(claude: &Path, args: impl IntoIterator<Item = &'a str>) -> Invocation {
    let mut all = vec!["plugin".to_owned()];
    all.extend(args.into_iter().map(str::to_owned));
    Invocation::new(claude, all)
}

/// How Claude names the plugin once it has been installed from the marketplace.
fn id() -> String {
    format!("{NAME}@{NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated files for a machine rooted at `home`.
    fn files(home: &str, binary: &str) -> Vec<(PathBuf, String)> {
        let env = Environment::rooted(home);
        generated(&root(&env), Path::new(binary)).unwrap()
    }

    #[test]
    fn the_marketplace_is_generated_below_this_programs_own_data_directory() {
        let paths: Vec<PathBuf> = files("/home/u", "/opt/bin/agentbus")
            .into_iter()
            .map(|(path, _)| path)
            .collect();

        assert_eq!(
            paths,
            vec![
                PathBuf::from(
                    "/home/u/.local/share/agentbus/claude-marketplace/.claude-plugin/marketplace.json"
                ),
                PathBuf::from(
                    "/home/u/.local/share/agentbus/claude-marketplace/agentbus/.claude-plugin/plugin.json"
                ),
                PathBuf::from(
                    "/home/u/.local/share/agentbus/claude-marketplace/agentbus/hooks/hooks.json"
                ),
            ]
        );
    }

    #[test]
    fn every_generated_file_is_json_and_carries_no_placeholder_out_of_the_template() {
        for (path, contents) in files("/home/u", "/opt/bin/agentbus") {
            let parsed: Value = serde_json::from_str(&contents)
                .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
            assert!(parsed.is_object(), "{}", path.display());
            assert!(!contents.contains(BINARY_MARK), "{}", path.display());
            assert!(!contents.contains(VERSION_MARK), "{}", path.display());
        }
    }

    #[test]
    fn the_plugin_carries_this_programs_own_version_and_names_nothing_else() {
        let (_, plugin) = files("/home/u", "/opt/bin/agentbus").remove(1);
        let manifest: Value = serde_json::from_str(&plugin).unwrap();

        assert_eq!(manifest["name"], Value::from(NAME));
        assert_eq!(manifest["version"], Value::from(VERSION));
        assert!(
            manifest["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{plugin}"
        );
    }

    #[test]
    fn the_marketplace_offers_exactly_the_generated_plugin() {
        let (_, marketplace) = files("/home/u", "/opt/bin/agentbus").remove(0);
        let manifest: Value = serde_json::from_str(&marketplace).unwrap();

        assert_eq!(manifest["name"], Value::from(NAME));
        let offered = manifest["plugins"].as_array().expect("no plugins listed");
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0]["name"], Value::from(NAME));
        assert_eq!(offered[0]["source"], Value::from(format!("./{NAME}")));
    }

    #[test]
    fn every_hook_runs_the_binary_by_the_path_it_was_given() {
        let (_, hooks) = files("/home/u", "/opt/bin/agentbus").remove(2);
        let document: Value = serde_json::from_str(&hooks).unwrap();

        let events = document["hooks"].as_object().expect("no hooks");
        assert!(!events.is_empty());
        for (event, matchers) in events {
            for entry in matchers.as_array().expect(event) {
                for hook in entry["hooks"].as_array().expect(event) {
                    assert_eq!(hook["type"], Value::from("command"));
                    assert_eq!(
                        hook["command"],
                        Value::from("/opt/bin/agentbus emit --agent claude"),
                        "{event}"
                    );
                    assert_eq!(hook["async"], Value::Bool(true), "{event}");
                    assert_eq!(hook["timeout"], Value::from(5), "{event}");
                }
            }
        }
    }

    #[test]
    fn a_path_that_is_not_text_is_refused_rather_than_approximated() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let path = PathBuf::from(OsStr::from_bytes(b"/opt/\xff/agentbus"));

        assert!(matches!(
            generated(Path::new("/home/u"), &path),
            Err(Error::Unwritable { .. })
        ));
    }

    #[test]
    fn a_path_with_something_to_escape_survives_being_put_in_json() {
        let (_, hooks) = files("/home/u", "/opt/a \"b\"/agentbus").remove(2);
        let document: Value = serde_json::from_str(&hooks).unwrap();

        assert_eq!(
            document["hooks"]["Stop"][0]["hooks"][0]["command"],
            Value::from("/opt/a \"b\"/agentbus emit --agent claude")
        );
    }

    #[test]
    fn a_copy_claude_took_matches_only_while_it_holds_the_plugins_own_files() {
        let home = tempfile::tempdir().unwrap();
        let env = Environment::rooted(home.path());
        let root = root(&env);
        let generated = generated(&root, Path::new("/opt/bin/agentbus")).unwrap();
        let copy = home.path().join("copy");

        assert!(
            !matches(&copy, &root, &generated),
            "a copy that was never taken cannot match"
        );

        for (path, contents) in &generated {
            let Ok(relative) = path.strip_prefix(plugin_dir(&root)) else {
                continue;
            };
            file::write(&copy.join(relative), contents).unwrap();
        }
        assert!(matches(&copy, &root, &generated));

        file::write(&copy.join("hooks/hooks.json"), "{}").unwrap();
        assert!(
            !matches(&copy, &root, &generated),
            "a copy holding something else cannot match"
        );
    }

    #[test]
    fn a_command_is_named_by_where_it_was_found_or_by_what_a_user_would_type() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            command(&Environment::rooted(dir.path())),
            PathBuf::from("claude")
        );

        let invocation = plugin(
            Path::new("/usr/bin/claude"),
            ["install", &id(), "-s", SCOPE],
        );
        assert_eq!(
            invocation.to_string(),
            "claude plugin install agentbus@agentbus -s user"
        );
    }
}
