//! These tests open a real window on the toolkit's own headless platform and
//! start real processes on real terminal devices. Nothing about the keyboard is
//! stubbed: a key press goes in as the platform would deliver it, is matched
//! against the same keymap the application installs, and either becomes an
//! action or becomes bytes on a terminal — so what is being asserted is the
//! path, not a description of it.

use std::thread;
use std::time::{Duration, Instant};

use gpui::{Entity, TestAppContext, VisualTestContext};
use shepherd_core::{Program, ShellAddress};

use super::*;

/// How long a test waits for something a real process has to do before it
/// concludes the process is never going to do it. Generous: this has to hold on
/// a loaded machine running the rest of the suite alongside it, and the cost of
/// being generous is only paid when a test is failing anyway.
const PATIENCE: Duration = Duration::from_secs(20);

/// How often it looks while waiting.
const GLANCE: Duration = Duration::from_millis(10);

/// The keys these tests press, which are the ones this machine's table binds.
///
/// The table is chosen for the platform at run time, so a test that spelled the
/// chords itself would be asserting the other platform's keymap half the time.
fn keys() -> crate::keymap::Keys {
    crate::keymap::table()
}

/// A window showing one workspace with one shell in it, running `/bin/sh`.
///
/// The shell is chosen rather than inherited: every unix has it, and a test
/// that ran whatever the person running it happens to use would pass and fail
/// for reasons that have nothing to do with the code.
fn opened(cx: &mut TestAppContext) -> (Entity<TerminalView>, &mut VisualTestContext) {
    cx.update(crate::keymap::install);
    cx.update(gpui_component::init);

    let mut layout = Layout::new();
    let workspace = layout.open("/");
    let address = {
        let open = layout
            .workspace_mut(workspace)
            .expect("the workspace just opened");
        let tab = open.open_tab(TAB);
        let shell = open.tab(tab).expect("the tab just opened").focused();
        ShellAddress::new(workspace, shell)
    };

    let options = ShellOptions::new()
        .program(Program::new("/bin/sh"))
        // An empty prompt keeps the grids to the output of what was asked for.
        .env("PS1", "")
        .env("ENV", "");
    let first = Shell::spawn(address, &options).expect("a shell to start");

    let (view, cx) =
        cx.add_window_view(|window, cx| TerminalView::new(first, layout, options, window, cx));
    // The toolkit reports focus moving within a window only while that window
    // is the active one, and a window opened in a test is not active until it
    // is said to be.
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    (view, cx)
}

/// Waits until `ready`, or fails saying what it was waiting for and what the
/// shells said instead.
fn wait_for(
    view: &Entity<TerminalView>,
    cx: &mut VisualTestContext,
    expectation: &str,
    mut ready: impl FnMut(&TerminalView) -> bool,
) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if view.read_with(cx, |view, _| ready(view)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for {expectation}; the screens said:\n{}",
            view.read_with(cx, screens)
        );
        thread::sleep(GLANCE);
    }
}

/// Waits until some row of some shell's screen is exactly `line`.
///
/// Exactly, rather than containing it: a command is echoed back by the terminal
/// as it is typed, so a screen showing `echo hello` proves only that the
/// keystrokes arrived. A row that is just `hello` is the process's answer.
fn wait_for_line(view: &Entity<TerminalView>, cx: &mut VisualTestContext, line: &str) {
    wait_for(view, cx, &format!("a row reading `{line}`"), |view| {
        view.shells
            .iter()
            .any(|held| held.shell.screen().iter().any(|row| row == line))
    });
}

/// Every shell's screen, for a failure message.
fn screens(view: &TerminalView, _: &App) -> String {
    view.shells
        .iter()
        .map(|held| {
            format!(
                "--- {} ---\n{}",
                held.shell.address().shell,
                held.shell.screen().join("\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rows one shell has on screen.
fn screen_of(view: &TerminalView, shell: ShellId) -> Vec<String> {
    view.held(shell)
        .map(|held| held.shell.screen())
        .unwrap_or_default()
}

/// Types `text` one key at a time, the way a person would.
fn type_out(cx: &mut VisualTestContext, text: &str) {
    for character in text.chars() {
        let keystroke = match character {
            ' ' => "space".to_owned(),
            other => other.to_string(),
        };
        cx.simulate_keystrokes(&keystroke);
    }
}

#[test]
fn a_grid_is_as_many_whole_cells_as_fit() {
    assert_eq!(fits(px(800.0), px(8.0)), 100);
    assert_eq!(fits(px(805.0), px(8.0)), 100, "a partial column is not one");
}

#[test]
fn a_grid_with_no_room_has_no_cells_in_it() {
    assert_eq!(fits(px(4.0), px(8.0)), 0);
    assert_eq!(fits(px(-10.0), px(8.0)), 0);
    assert_eq!(
        fits(px(800.0), px(0.0)),
        0,
        "a cell of no width fits nowhere"
    );
}

#[test]
fn a_cell_is_told_to_a_process_in_whole_pixels() {
    assert_eq!(pixels(px(8.4)), 8);
    assert_eq!(pixels(px(8.6)), 9);
    assert_eq!(
        pixels(px(0.2)),
        1,
        "no process is told its cells are nothing"
    );
}

#[gpui::test]
fn what_is_typed_reaches_the_shell_that_has_focus(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);

    type_out(cx, "echo hello");
    cx.simulate_keystrokes("enter");

    wait_for_line(&view, cx, "hello");
}

#[gpui::test]
fn control_c_interrupts_what_the_shell_is_running(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);

    // Long enough that a shell which queued the interrupt rather than taking
    // it would still be sleeping when this test gives up waiting.
    type_out(cx, "sleep 300");
    cx.simulate_keystrokes("enter");
    wait_for(&view, cx, "the sleep to be running", |view| {
        view.shells[0]
            .shell
            .screen()
            .iter()
            .any(|row| row == "sleep 300")
    });

    cx.simulate_keystrokes("ctrl-c");

    // A prompt that answers again is a prompt that got its terminal back.
    type_out(cx, "echo interrupted");
    cx.simulate_keystrokes("enter");
    wait_for_line(&view, cx, "interrupted");
}

#[gpui::test]
fn splitting_puts_a_shell_beside_the_one_with_focus(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let first = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().split_right);

    let (shells, focused) = view.read_with(cx, |view, _| (view.showing(), view.focused));
    assert_eq!(shells.len(), 2, "the tab now holds two shells");
    assert_eq!(shells[0], first, "the shell split from is still first");
    assert_ne!(focused, first, "the new shell is the one being typed in");
    assert_eq!(
        view.read_with(cx, |view, _| view.shells.len()),
        2,
        "and it has a process in it"
    );

    cx.simulate_keystrokes(keys().split_down);
    assert_eq!(
        view.read_with(cx, |view, _| view.showing().len()),
        3,
        "splitting the other way splits the shell that had focus"
    );
}

#[gpui::test]
fn a_new_tab_holds_a_shell_of_its_own(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let first = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().new_tab);

    view.read_with(cx, |view, _| {
        assert_eq!(view.open().tabs().len(), 2, "there are two tabs");
        assert_eq!(view.showing().len(), 1, "the new one holds one shell");
        assert_ne!(view.focused, first, "which is the one being typed in");
        assert_eq!(view.shells.len(), 2, "both shells have processes");
    });
}

#[gpui::test]
fn focus_moves_to_the_shell_in_the_direction_asked_for(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let left = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().split_right);
    let right = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().focus_left);
    assert_eq!(view.read_with(cx, |view, _| view.focused), left);

    cx.simulate_keystrokes(keys().focus_right);
    assert_eq!(view.read_with(cx, |view, _| view.focused), right);

    cx.simulate_keystrokes(keys().focus_up);
    assert_eq!(
        view.read_with(cx, |view, _| view.focused),
        right,
        "there is nothing above, and focus does not wrap around"
    );

    cx.simulate_keystrokes(keys().split_down);
    let below = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().focus_up);
    assert_eq!(view.read_with(cx, |view, _| view.focused), right);

    cx.simulate_keystrokes(keys().focus_down);
    assert_eq!(view.read_with(cx, |view, _| view.focused), below);
}

#[gpui::test]
fn closing_a_shell_leaves_focus_where_the_arrangement_put_it(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let first = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().split_right);
    cx.simulate_keystrokes(keys().close);

    view.read_with(cx, |view, _| {
        assert_eq!(view.showing(), vec![first], "the split collapsed");
        assert_eq!(view.focused, first, "onto the shell that is left");
        assert_eq!(view.shells.len(), 1, "and the other process is gone");
    });
}

#[gpui::test]
fn closing_a_tab_s_last_shell_closes_the_tab(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let first = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().new_tab);
    cx.simulate_keystrokes(keys().close);

    view.read_with(cx, |view, _| {
        assert_eq!(view.open().tabs().len(), 1, "the tab went with its shell");
        assert_eq!(
            view.focused, first,
            "focus came back to the tab that is left"
        );
    });
}

#[gpui::test]
fn typing_follows_the_focus_the_actions_moved(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let left = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().split_right);
    let right = view.read_with(cx, |view, _| view.focused);

    type_out(cx, "echo right");
    cx.simulate_keystrokes("enter");
    wait_for(&view, cx, "the new shell to answer", |view| {
        screen_of(view, right).iter().any(|row| row == "right")
    });
    assert!(
        view.read_with(cx, |view, _| screen_of(view, left)
            .iter()
            .all(String::is_empty)),
        "nothing was typed into the shell that did not have focus"
    );

    cx.simulate_keystrokes(keys().focus_left);
    type_out(cx, "echo left");
    cx.simulate_keystrokes("enter");
    wait_for(&view, cx, "the first shell to answer", |view| {
        screen_of(view, left).iter().any(|row| row == "left")
    });
    assert!(
        view.read_with(cx, |view, _| {
            screen_of(view, right).iter().all(|row| row != "left")
        }),
        "and the shell that lost focus was not typed into"
    );
}

#[gpui::test]
fn the_model_is_told_where_the_toolkit_put_focus(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);

    cx.simulate_keystrokes(keys().split_right);

    // Focus is moved the way anything other than an action would move it, and
    // both the view's record of it and the model's follow.
    let first = view.read_with(cx, |view, _| view.showing()[0]);
    let handle = view.read_with(cx, |view, _| {
        view.held(first).expect("a shell being shown").focus.clone()
    });
    cx.update(|window, _| window.focus(&handle));
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(view.focused, first);
        let tab = view.open().tab_of(first).expect("the tab holding it");
        assert_eq!(
            view.open().tab(tab).expect("that tab").focused(),
            first,
            "the model was told, rather than keeping an opinion of its own"
        );
    });
}

#[gpui::test]
fn a_key_the_keymap_binds_is_not_also_typed(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);

    cx.simulate_keystrokes(keys().new_tab);
    let fresh = view.read_with(cx, |view, _| view.focused);

    // Nothing is waited for: the point is that nothing arrives. A shell that
    // was sent the chord would show it echoed back within a frame or two, and
    // the sleep is what gives it the chance.
    thread::sleep(Duration::from_millis(200));
    assert!(
        view.read_with(cx, |view, _| screen_of(view, fresh)
            .iter()
            .all(String::is_empty)),
        "a bound chord became an action and stopped there"
    );
}
