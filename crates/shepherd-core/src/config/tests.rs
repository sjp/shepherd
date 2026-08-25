use super::*;

use std::collections::HashMap;

use tempfile::TempDir;

use crate::split::Direction;

/// A layout worth saving: two workspaces, one of them with two tabs and an
/// arrangement several splits deep, and something chosen about the other.
fn layout() -> Layout {
    let mut layout = Layout::new();

    let thing = layout.open("/home/someone/projects/thing");
    let workspace = layout.workspace_mut(thing).expect("just opened");
    let build = workspace.open_tab("build");
    let first = workspace.tab(build).unwrap().focused();
    let second = workspace.split(build, first, Direction::Right).unwrap();
    let third = workspace.split(build, second, Direction::Down).unwrap();
    workspace.split(build, third, Direction::Down).unwrap();
    let notes = workspace.open_tab("notes");
    workspace.tab_mut(notes).unwrap().set_name("notes and such");

    let other = layout.open("/home/someone/projects/other");
    let workspace = layout.workspace_mut(other).expect("just opened");
    workspace.settings_mut().devcontainer = true;
    workspace.open_tab("shell");
    workspace.set_name("the other one");

    layout
}

/// A configuration file in a directory of its own, with the directory kept
/// alive alongside it.
fn config() -> (TempDir, Config) {
    let dir = tempfile::tempdir().expect("cannot make a directory to write in");
    let config = Config::at(dir.path().join("nested").join("config.toml"));
    (dir, config)
}

/// A file with `text` in it, and a configuration pointed at it.
fn holding(text: &str) -> (TempDir, Config) {
    let (dir, config) = config();
    fs::create_dir_all(config.path().parent().unwrap()).unwrap();
    fs::write(config.path(), text).unwrap();
    (dir, config)
}

/// What the environment looks like to [`Config::resolve`].
fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let vars: HashMap<String, OsString> = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), OsString::from(*value)))
        .collect();
    move |name| vars.get(name).cloned()
}

#[test]
fn a_layout_survives_being_saved_and_loaded() {
    let (_dir, mut config) = config();
    let saved = layout();

    config.save(&saved).expect("cannot save");
    let restored = Config::at(config.path()).load().expect("cannot load");

    assert_eq!(restored, saved);
}

#[test]
fn the_file_written_is_the_documented_shape() {
    let (_dir, mut config) = config();
    let mut layout = Layout::new();
    let id = layout.open("/home/someone/projects/thing");
    let workspace = layout.workspace_mut(id).unwrap();
    let tab = workspace.open_tab("build");
    let first = workspace.tab(tab).unwrap().focused();
    workspace.split(tab, first, Direction::Right).unwrap();

    config.save(&layout).expect("cannot save");

    assert_eq!(
        fs::read_to_string(config.path()).unwrap(),
        r#"version = 1

[[workspace]]
id = 0
name = "thing"
path = "/home/someone/projects/thing"
devcontainer = false

[[workspace.tab]]
id = 0
name = "build"
focused = 1
layout = "h(0.5:s0, 0.5:s1)"
"#
    );
}

#[test]
fn a_first_launch_finds_nothing_and_that_is_not_a_failure() {
    let (_dir, mut config) = config();

    assert_eq!(
        config.load().expect("a missing file is not a failure"),
        Layout::new()
    );
    assert!(!config.is_held(), "there was nothing to lose");
    assert!(!config.path().exists(), "loading wrote something");
}

#[test]
fn a_file_that_cannot_be_read_is_reported_and_then_not_written_over() {
    let (_dir, mut config) = holding("this is not a configuration file");

    let problem = config
        .load()
        .expect_err("nonsense was read as configuration");
    assert!(matches!(problem, ConfigError::Parse { .. }), "{problem}");
    assert!(config.is_held());

    let refused = config
        .save(&layout())
        .expect_err("a file nobody has seen was written over");
    assert!(matches!(refused, ConfigError::Held { .. }), "{refused}");
    assert_eq!(
        fs::read_to_string(config.path()).unwrap(),
        "this is not a configuration file",
        "the file was written over anyway"
    );

    config.overwrite();
    config.save(&layout()).expect("cannot save once told to");
    assert_eq!(Config::at(config.path()).load().unwrap(), layout());
}

#[test]
fn a_file_from_another_format_is_named_as_such_rather_than_parsed() {
    let (_dir, mut config) = holding("version = 2\nwhatever = true\n");

    let problem = config.load().expect_err("a later format was read anyway");
    assert!(
        matches!(problem, ConfigError::Version { version: 2, .. }),
        "{problem}"
    );

    let (_dir, mut config) = holding("workspaces = []\n");
    let problem = config.load().expect_err("a file with no version was read");
    assert!(
        matches!(problem, ConfigError::Unversioned { .. }),
        "{problem}"
    );
}

#[test]
fn every_way_a_description_can_be_untrue_is_reported_with_where_it_is() {
    let cases = [
        ("h(0.5:s0", "focused = 0"),
        ("h(0.5:s0, 0.5:s0)", "focused = 0"),
    ];
    for (arrangement, focus) in cases {
        let (_dir, mut config) = holding(&file_with(arrangement, focus));
        let problem = config.load().expect_err("{arrangement} was accepted");
        let ConfigError::Arrangement { at, .. } = &problem else {
            panic!("{problem}");
        };
        assert_eq!(at, r#"workspace 0 "thing", tab 0 "build""#);
    }

    let (_dir, mut config) = holding(&file_with("h(0.5:s0, 0.5:s1)", "focused = 7"));
    let problem = config.load().expect_err("a tab was focused on nothing");
    let ConfigError::Describes { at, source, .. } = &problem else {
        panic!("{problem}");
    };
    assert_eq!(at, r#"workspace 0 "thing", tab 0 "build""#);
    assert_eq!(
        *source,
        MalformedLayout::FocusElsewhere(ShellId::from_raw(7))
    );
}

#[test]
fn one_shell_in_two_tabs_is_refused_at_the_workspace_it_is_in() {
    let (_dir, mut config) = holding(
        r#"version = 1

[[workspace]]
id = 0
path = "/home/someone/projects/thing"

[[workspace.tab]]
id = 0
layout = "s0"

[[workspace.tab]]
id = 1
layout = "s0"
"#,
    );

    let problem = config.load().expect_err("one shell was in two tabs");
    let ConfigError::Describes { at, source, .. } = &problem else {
        panic!("{problem}");
    };
    assert_eq!(at, r#"workspace 0 "thing""#);
    assert_eq!(
        *source,
        MalformedLayout::DuplicateShell(ShellId::from_raw(0))
    );
}

#[test]
fn two_workspaces_with_one_id_are_refused_by_the_file_as_a_whole() {
    let (_dir, mut config) = holding(
        r#"version = 1

[[workspace]]
id = 3
path = "/home/someone/projects/thing"

[[workspace]]
id = 3
path = "/home/someone/projects/other"
"#,
    );

    let problem = config.load().expect_err("two workspaces shared an id");
    let ConfigError::Describes { at, source, .. } = &problem else {
        panic!("{problem}");
    };
    assert_eq!(at, "the list of workspaces");
    assert_eq!(
        *source,
        MalformedLayout::DuplicateWorkspace(WorkspaceId::from_raw(3))
    );
}

#[test]
fn what_a_file_leaves_out_is_filled_in_rather_than_refused() {
    let (_dir, mut config) = holding(
        r#"version = 1

[[workspace]]
id = 0
path = "/home/someone/projects/thing"

[[workspace.tab]]
id = 0
layout = "h(1:s4, 1:s5)"
"#,
    );

    let layout = config.load().expect("a sparse file was refused");
    let workspace = layout.workspace(WorkspaceId::FIRST).expect("the workspace");
    assert_eq!(workspace.name(), "thing", "the folder names the workspace");
    assert!(!workspace.settings().devcontainer);

    let tab = &workspace.tabs()[0];
    assert_eq!(tab.name(), "");
    assert_eq!(
        tab.focused(),
        ShellId::from_raw(4),
        "the first shell takes focus where nothing says otherwise"
    );
    assert_eq!(
        tree::write(tab.tree()),
        "h(0.5:s4, 0.5:s5)",
        "shares written by hand were not rescaled"
    );
}

#[test]
fn a_restored_workspace_hands_out_ids_after_the_ones_it_holds() {
    let (_dir, mut config) = config();
    let mut saved = layout();
    let id = saved.workspaces()[0].id();
    config.save(&saved).unwrap();

    let mut restored = Config::at(config.path()).load().unwrap();
    let held: Vec<ShellId> = restored.workspace(id).unwrap().shells();
    let workspace = restored.workspace_mut(id).unwrap();
    let tab = workspace.tabs()[0].id();
    let fresh = workspace
        .split(tab, held[0], Direction::Right)
        .expect("cannot split a restored tab");

    assert!(
        !held.contains(&fresh),
        "a restored shell's id was handed out again"
    );
    assert_eq!(
        saved.open("/home/someone/projects/third"),
        restored.open("/home/someone/projects/third")
    );
}

#[test]
fn saving_what_is_already_there_writes_nothing() {
    let (_dir, mut config) = config();
    let layout = layout();
    config.save(&layout).unwrap();

    // Written behind the application's back: if the save below writes at all,
    // this is what it replaces.
    fs::write(config.path(), "untouched").unwrap();
    config.save(&layout).expect("cannot save");

    assert_eq!(fs::read_to_string(config.path()).unwrap(), "untouched");
}

#[test]
fn a_load_leaves_nothing_for_the_next_save_to_do() {
    let (_dir, mut config) = config();
    config.save(&layout()).unwrap();

    let mut config = Config::at(config.path());
    let layout = config.load().unwrap();
    fs::write(config.path(), "untouched").unwrap();
    config.save(&layout).unwrap();

    assert_eq!(fs::read_to_string(config.path()).unwrap(), "untouched");
}

#[test]
fn a_change_is_written_where_the_configuration_belongs() {
    let (_dir, mut config) = config();
    let mut layout = layout();
    config.save(&layout).unwrap();

    let id = layout.workspaces()[0].id();
    layout.workspace_mut(id).unwrap().open_tab("another");
    config.save(&layout).expect("cannot save a change");

    assert_eq!(Config::at(config.path()).load().unwrap(), layout);
}

#[test]
fn each_platform_keeps_configuration_where_that_platform_keeps_configuration() {
    let vars = env(&[
        ("HOME", "/home/someone"),
        ("APPDATA", r"C:\Users\someone\AppData\Roaming"),
    ]);

    assert_eq!(
        Config::resolve(Convention::XdgConfigHome, &vars),
        Some(PathBuf::from("/home/someone/.config/shepherd/config.toml"))
    );
    assert_eq!(
        Config::resolve(Convention::ApplicationSupport, &vars),
        Some(PathBuf::from(
            "/home/someone/Library/Application Support/Shepherd/config.toml"
        ))
    );
    // Joined rather than spelled out, because the separator between the
    // components is the running platform's and this test runs on all of them.
    assert_eq!(
        Config::resolve(Convention::RoamingAppData, &vars),
        Some(
            PathBuf::from(r"C:\Users\someone\AppData\Roaming")
                .join("Shepherd")
                .join("config.toml")
        )
    );
}

#[test]
fn the_base_directory_variable_is_honoured_only_where_it_means_something() {
    let absolute = env(&[("HOME", "/home/someone"), ("XDG_CONFIG_HOME", "/elsewhere")]);
    assert_eq!(
        Config::resolve(Convention::XdgConfigHome, &absolute),
        Some(PathBuf::from("/elsewhere/shepherd/config.toml"))
    );

    // Its own specification says a relative value is to be ignored, and it is
    // read at all only by the convention that defines it.
    let relative = env(&[("HOME", "/home/someone"), ("XDG_CONFIG_HOME", "relative")]);
    assert_eq!(
        Config::resolve(Convention::XdgConfigHome, &relative),
        Some(PathBuf::from("/home/someone/.config/shepherd/config.toml"))
    );
    assert_eq!(
        Config::resolve(Convention::ApplicationSupport, &absolute),
        Some(PathBuf::from(
            "/home/someone/Library/Application Support/Shepherd/config.toml"
        ))
    );
}

#[test]
fn a_file_named_outright_wins_everywhere_and_an_empty_variable_names_nothing() {
    let named = env(&[
        ("SHEPHERD_CONFIG", "/tmp/somewhere/else.toml"),
        ("HOME", "/home/someone"),
        ("APPDATA", r"C:\Users\someone\AppData\Roaming"),
    ]);
    for convention in [
        Convention::XdgConfigHome,
        Convention::ApplicationSupport,
        Convention::RoamingAppData,
    ] {
        assert_eq!(
            Config::resolve(convention, &named),
            Some(PathBuf::from("/tmp/somewhere/else.toml"))
        );
    }

    let empty = env(&[("SHEPHERD_CONFIG", ""), ("HOME", "/home/someone")]);
    assert_eq!(
        Config::resolve(Convention::XdgConfigHome, &empty),
        Some(PathBuf::from("/home/someone/.config/shepherd/config.toml"))
    );
}

#[test]
fn a_machine_that_names_nowhere_is_told_so_rather_than_given_a_relative_path() {
    let nowhere = env(&[]);
    for convention in [
        Convention::XdgConfigHome,
        Convention::ApplicationSupport,
        Convention::RoamingAppData,
    ] {
        assert_eq!(Config::resolve(convention, &nowhere), None);
    }
}

/// A one-workspace, one-tab file holding `arrangement`, with `focus` as the
/// tab's focus line.
fn file_with(arrangement: &str, focus: &str) -> String {
    format!(
        r#"version = 1

[[workspace]]
id = 0
name = "thing"
path = "/home/someone/projects/thing"

[[workspace.tab]]
id = 0
name = "build"
{focus}
layout = "{arrangement}"
"#
    )
}
