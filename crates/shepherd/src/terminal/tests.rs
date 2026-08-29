//! These tests open a real window on the toolkit's own headless platform and
//! start real processes on real terminal devices. Nothing about the keyboard is
//! stubbed: a key press goes in as the platform would deliver it, is matched
//! against the same keymap the application installs, and either becomes an
//! action or becomes bytes on a terminal — so what is being asserted is the
//! path, not a description of it.

use std::thread;
use std::time::{Duration, Instant};

use agentbus_protocol::SessionStatus::{Blocked, Working};
use agentbus_protocol::{Agent, SessionEntry, SessionStatus, Snapshot, Source, Timestamp};
use gpui::{Action, Entity, Modifiers, Point, TestAppContext, VisualTestContext, point};
use shepherd_core::provision::Standing;
use shepherd_core::{Program, ShellAddress, ShellStatus, Update};

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

/// Every shell on screen, addressed.
///
/// The arrangement deals in numbers, and every workspace numbers its own shells
/// from one — so a number names a shell here only alongside the workspace whose
/// arrangement it came out of.
fn shown(view: &TerminalView) -> Vec<ShellAddress> {
    view.showing()
        .into_iter()
        .map(|shell| view.at(shell))
        .collect()
}

/// The rows one shell has on screen.
fn screen_of(view: &TerminalView, shell: ShellAddress) -> Vec<String> {
    view.held(shell)
        .map(|held| held.shell.screen())
        .unwrap_or_default()
}

/// The middle of where one shell is drawn, in the window's own coordinates.
///
/// Worked out from the same two things the frame was drawn from — the tab's
/// arrangement, and the room the window measured for it — so a test presses
/// where the shell actually is rather than where the arithmetic in the test
/// says it should be.
fn middle_of(view: &TerminalView, shell: ShellAddress) -> Point<Pixels> {
    let bounds = view
        .tree()
        .expect("a tab on screen")
        .layout_in(area(view.area))
        .into_iter()
        .find(|placed| placed.shell == shell.shell)
        .expect("the shell is in the tab on screen")
        .bounds;
    point(
        px(bounds.x + bounds.width / 2.0),
        px(bounds.y + bounds.height / 2.0),
    )
}

/// The middle of the one divider in the tab on screen.
fn the_divider(view: &TerminalView) -> (PlacedDivider, Point<Pixels>) {
    let dividers = view
        .tree()
        .expect("a tab on screen")
        .dividers_in(area(view.area));
    assert_eq!(dividers.len(), 1, "one divider in this arrangement");
    let placed = dividers[0].clone();
    let at = point(
        px(placed.bounds.x + placed.bounds.width / 2.0),
        px(placed.bounds.y + placed.bounds.height / 2.0),
    );
    (placed, at)
}

/// Moves the pointer to `at` and presses, the way a person takes hold of
/// something.
fn press(cx: &mut VisualTestContext, at: Point<Pixels>) {
    cx.simulate_mouse_move(at, None, Modifiers::none());
    cx.simulate_mouse_down(at, gpui::MouseButton::Left, Modifiers::none());
}

/// Where one shell sits in the tab on screen, as a fraction of it.
fn share_of(view: &TerminalView, shell: ShellAddress) -> shepherd_core::Rect {
    view.tree()
        .expect("a tab on screen")
        .layout()
        .into_iter()
        .find(|placed| placed.shell == shell.shell)
        .expect("the shell is in the tab on screen")
        .bounds
}

/// Every action a menu offers that acts on a shell, paired with the chord bound
/// to the same one, in an order that gets through all of them: a tab opened,
/// stepped away from and back to, a shell split both ways, focus walked around
/// what that made, and the last shell closed.
///
/// What is not here is [`unwalkable`].
fn both_ways() -> Vec<(Box<dyn Action>, &'static str)> {
    let keys = keys();
    vec![
        (Box::new(NewTab), keys.new_tab),
        (Box::new(NextTab), keys.next_tab),
        (Box::new(PreviousTab), keys.previous_tab),
        (Box::new(SplitRight), keys.split_right),
        (Box::new(SplitDown), keys.split_down),
        (Box::new(FocusLeft), keys.focus_left),
        (Box::new(FocusRight), keys.focus_right),
        (Box::new(FocusUp), keys.focus_up),
        (Box::new(FocusDown), keys.focus_down),
        (Box::new(Close), keys.close),
    ]
}

/// The bound actions the script above deliberately leaves out.
///
/// Quitting ends the process, which is not a thing a test can do halfway; and
/// opening a workspace waits on the platform's own folder picker, which the
/// platform these tests run on has none of. What the window does once a folder
/// has been chosen is asserted directly instead, without a dialog in the way.
fn unwalkable() -> Vec<Box<dyn Action>> {
    vec![
        crate::keymap::Quit.boxed_clone(),
        OpenWorkspace.boxed_clone(),
    ]
}

/// Which of the two ways in a step of that script is taken by.
#[derive(Debug, Clone, Copy)]
enum By {
    Chord,
    Menu,
}

/// Runs the whole script one of the two ways, in a window of its own, and
/// answers what that window was showing after each step.
fn walked(cx: &mut TestAppContext, by: By) -> Vec<String> {
    let (view, cx) = opened(cx);
    let mut said = Vec::new();
    for (action, chord) in both_ways() {
        match by {
            By::Chord => cx.simulate_keystrokes(chord),
            // Choosing a menu item is this and nothing else: the platform hands
            // the action back and the toolkit dispatches it, with no idea where
            // it will be answered.
            By::Menu => cx.update(|window, cx| window.dispatch_action(action, cx)),
        }
        cx.run_until_parked();
        said.push(view.read_with(cx, arrangement));
    }
    said
}

/// What the window is showing, in a line: how many tabs are open, the shells in
/// the one on screen, and which of them is being typed in.
fn arrangement(view: &TerminalView, _: &App) -> String {
    format!(
        "{} tabs; showing {:?}; typing in {:?}",
        view.open().tabs().len(),
        shown(view),
        view.focused,
    )
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

    let (shells, focused) = view.read_with(cx, |view, _| (shown(view), view.focused));
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
        assert_eq!(shown(view), vec![first], "the split collapsed");
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
    let first = view.read_with(cx, |view, _| shown(view)[0]);
    let handle = view.read_with(cx, |view, _| {
        view.held(first).expect("a shell being shown").focus.clone()
    });
    cx.update(|window, _| window.focus(&handle));
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(view.focused, first);
        let tab = view.open().tab_of(first.shell).expect("the tab holding it");
        assert_eq!(
            view.open().tab(tab).expect("that tab").focused(),
            first.shell,
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

#[gpui::test]
fn the_bar_holds_every_tab_and_marks_the_one_on_screen(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);

    cx.simulate_keystrokes(keys().new_tab);
    cx.simulate_keystrokes(keys().new_tab);

    let (tabs, showing) = view.read_with(cx, |view, _| view.bar());
    assert_eq!(
        tabs.len(),
        3,
        "three tabs are open and the bar holds all three"
    );
    assert_eq!(
        tabs[showing].0,
        view.read_with(cx, |view, _| view.active),
        "and the one marked is the one on screen"
    );
    assert_eq!(showing, 2, "which is the one just opened");
}

#[gpui::test]
fn stepping_between_tabs_shows_the_one_stepped_to(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let first = view.read_with(cx, |view, _| view.active);

    cx.simulate_keystrokes(keys().new_tab);
    cx.simulate_keystrokes(keys().new_tab);
    let third = view.read_with(cx, |view, _| view.active);

    cx.simulate_keystrokes(keys().next_tab);
    assert_eq!(
        view.read_with(cx, |view, _| view.active),
        first,
        "stepping on from the last tab comes back round to the first"
    );
    assert_eq!(
        view.read_with(cx, |view, _| (view.bar().1, view.showing().len())),
        (0, 1),
        "and what is on screen is that tab and its shell"
    );

    cx.simulate_keystrokes(keys().previous_tab);
    assert_eq!(view.read_with(cx, |view, _| view.active), third);

    // Typing goes to the tab that is showing, which is what makes the switch a
    // switch rather than a change of highlight.
    let shell = view.read_with(cx, |view, _| view.focused);
    type_out(cx, "echo third");
    cx.simulate_keystrokes("enter");
    wait_for(&view, cx, "the shell in the tab stepped to", |view| {
        screen_of(view, shell).iter().any(|row| row == "third")
    });
}

#[gpui::test]
fn a_tab_nobody_is_looking_at_keeps_running(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let hidden = view.read_with(cx, |view, _| view.focused);

    // Started here, finished somewhere else: by the time this prints, the tab
    // it printed in is not the one on screen.
    type_out(cx, "sleep 1; echo carried on");
    cx.simulate_keystrokes("enter");
    cx.simulate_keystrokes(keys().new_tab);
    let showing = view.read_with(cx, |view, _| view.active);

    wait_for(&view, cx, "the hidden shell to answer", |view| {
        screen_of(view, hidden)
            .iter()
            .any(|row| row == "carried on")
    });
    assert_eq!(
        view.read_with(cx, |view, _| view.active),
        showing,
        "and nothing about that brought its tab back to the front"
    );
}

#[gpui::test]
fn pressing_in_a_shell_is_where_typing_goes(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let left = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().split_right);
    let right = view.read_with(cx, |view, _| view.focused);
    assert_ne!(left, right, "the split left focus in the new shell");

    let at = view.read_with(cx, |view, _| middle_of(view, left));
    cx.simulate_click(at, Modifiers::none());

    view.read_with(cx, |view, _| {
        assert_eq!(view.focused, left, "the shell pressed in has focus");
        let tab = view.open().tab_of(left.shell).expect("the tab holding it");
        assert_eq!(
            view.open().tab(tab).expect("that tab").focused(),
            left.shell,
            "and the model was told the same thing the keyboard tells it"
        );
    });

    type_out(cx, "echo pressed");
    cx.simulate_keystrokes("enter");
    wait_for(&view, cx, "the shell pressed in to answer", |view| {
        screen_of(view, left).iter().any(|row| row == "pressed")
    });
    assert!(
        view.read_with(cx, |view, _| screen_of(view, right)
            .iter()
            .all(|row| row != "pressed")),
        "and nothing was typed into the other one"
    );
}

#[gpui::test]
fn dragging_a_divider_moves_the_edge_and_leaves_the_arrangement_holding_it(
    cx: &mut TestAppContext,
) {
    let (view, cx) = opened(cx);
    let left = view.read_with(cx, |view, _| view.focused);

    cx.simulate_keystrokes(keys().split_right);
    let right = view.read_with(cx, |view, _| view.focused);
    assert_eq!(
        view.read_with(cx, |view, _| share_of(view, left).width),
        0.5,
        "a split starts even"
    );

    let (placed, at) = view.read_with(cx, |view, _| the_divider(view));
    let quarter = px(placed.within.x + placed.within.width / 4.0);
    press(cx, at);
    cx.simulate_mouse_move(
        point(quarter, at.y),
        gpui::MouseButton::Left,
        Modifiers::none(),
    );

    view.read_with(cx, |view, _| {
        let narrowed = share_of(view, left).width;
        assert!(
            (narrowed - 0.25).abs() < 0.01,
            "the left shell is a quarter of the tab, and is {narrowed}"
        );
        assert!(
            (share_of(view, right).width - 0.75).abs() < 0.01,
            "and the right one has the rest"
        );
        assert_eq!(
            view.focused, right,
            "taking hold of an edge is not a way of choosing a shell to type in"
        );
    });

    cx.simulate_mouse_up(
        point(quarter, at.y),
        gpui::MouseButton::Left,
        Modifiers::none(),
    );
    let settled = view.read_with(cx, |view, _| share_of(view, left).width);

    // Let go of, and then moved past: what the arrangement holds is where the
    // divider was left, not wherever the pointer went afterwards.
    cx.simulate_mouse_move(
        point(px(placed.within.x + placed.within.width * 0.9), at.y),
        None,
        Modifiers::none(),
    );
    assert_eq!(
        view.read_with(cx, |view, _| share_of(view, left).width),
        settled
    );
}

#[gpui::test]
fn closing_a_tab_takes_its_shells_with_it_and_leaves_focus_alone(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let first = view.read_with(cx, |view, _| view.active);

    cx.simulate_keystrokes(keys().new_tab);
    cx.simulate_keystrokes(keys().split_right);
    let typing_in = view.read_with(cx, |view, _| view.focused);
    let showing = view.read_with(cx, |view, _| view.active);

    // The tab closed is the one out of sight, which must not disturb where
    // somebody is typing.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.close_tab(first, window, cx));
    });

    view.read_with(cx, |view, _| {
        assert_eq!(view.open().tabs().len(), 1, "the tab is gone");
        assert_eq!(view.shells.len(), 2, "and so is the shell that was in it");
        assert_eq!(view.focused, typing_in, "and focus stayed where it was");
        assert_eq!(view.bar().0.len(), 1, "the bar says so too");
    });

    // And now the one on screen, which has to hand both over to what is left.
    cx.simulate_keystrokes(keys().new_tab);
    let opened = view.read_with(cx, |view, _| view.active);
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.close_tab(opened, window, cx));
    });

    view.read_with(cx, |view, _| {
        assert_eq!(view.active, showing, "the tab that is left came forward");
        assert_eq!(
            view.focused, typing_in,
            "and with it the shell it was last being typed in"
        );
        assert_eq!(view.shells.len(), 2);
    });
}

#[gpui::test]
fn an_irregular_arrangement_puts_every_shell_where_the_model_says(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let tall = view.read_with(cx, |view, _| view.focused);

    // One tall shell on the left, two stacked on the right: the arrangement a
    // grid of a fixed shape cannot hold.
    cx.simulate_keystrokes(keys().split_right);
    let upper = view.read_with(cx, |view, _| view.focused);
    cx.simulate_keystrokes(keys().split_down);
    let lower = view.read_with(cx, |view, _| view.focused);

    let (left, top, bottom) = view.read_with(cx, |view, _| {
        (
            share_of(view, tall),
            share_of(view, upper),
            share_of(view, lower),
        )
    });
    assert_eq!(left.height, 1.0, "the left shell is the height of the tab");
    assert_eq!(top.height, 0.5, "and the other two share the right of it");
    assert!(
        top.y + top.height <= bottom.y + f32::EPSILON,
        "with nothing of one over the other"
    );

    // Pressing in each of the three lands in that one, which is the whole of
    // what "drawn in the right place" means from outside.
    for shell in [tall, upper, lower] {
        let at = view.read_with(cx, |view, _| middle_of(view, shell));
        cx.simulate_click(at, Modifiers::none());
        assert_eq!(
            view.read_with(cx, |view, _| view.focused),
            shell,
            "pressing at {at:?} should land in the shell drawn there"
        );
    }
}

/// One session as the bus reports it, running in `shell`.
///
/// It carries the string this application gave that shell — which is the whole
/// of the join, and the reason the bus never has to be told anything about
/// shells for one of these to land on the right row.
fn running_in(view: &TerminalView, shell: ShellAddress, status: SessionStatus) -> SessionEntry {
    SessionEntry {
        session: "s1".to_owned(),
        agent: Agent::new("claude").expect("a valid agent id"),
        status,
        source: Source::Hook,
        status_source: None,
        cwd: None,
        correlation: Some(view.open().correlation(shell.shell)),
        origin: Vec::new(),
        since: Timestamp::parse("2026-08-17T10:31:02.006Z").expect("a well-formed timestamp"),
    }
}

/// Tells the window's half of the bus that `sessions` are running, as though a
/// daemon had said so.
fn the_bus_says(
    view: &Entity<TerminalView>,
    cx: &mut VisualTestContext,
    sessions: Vec<SessionEntry>,
) {
    view.update(cx, |view, cx| {
        let workspaces = view.layout.workspaces().to_vec();
        view.live
            .heard(&Update::Reset(Snapshot::new(1, sessions)), &workspaces);
        cx.notify();
    });
}

#[gpui::test]
fn the_sidebar_is_the_tree_of_what_is_open(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    cx.simulate_keystrokes(keys().split_right);
    cx.simulate_keystrokes(keys().new_tab);

    let sidebar = view.read_with(cx, |view, _| view.shown());
    assert_eq!(sidebar.workspaces.len(), 1, "one workspace is open");
    let workspace = &sidebar.workspaces[0];
    assert_eq!(workspace.tabs.len(), 2);
    assert_eq!(
        workspace.tabs[0].shells.len(),
        2,
        "the tab that was split holds both of its shells"
    );
    assert!(
        workspace.tabs[1].showing,
        "the tab just opened is the one on screen"
    );
    assert_eq!(
        workspace.tabs[1].shells[0].address,
        view.read_with(cx, |view, _| view.focused),
        "and its shell is the one being typed in"
    );
    assert!(workspace.tabs[1].shells[0].focused);
}

#[gpui::test]
fn a_shell_the_bus_calls_blocked_is_blocked_all_the_way_up_the_sidebar(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    // In a tab nobody is looking at, so that what the badges say cannot be
    // coming from what is on screen.
    let hidden = view.read_with(cx, |view, _| view.focused);
    cx.simulate_keystrokes(keys().new_tab);

    let blocked = view.read_with(cx, |view, _| running_in(view, hidden, Blocked));
    the_bus_says(&view, cx, vec![blocked]);

    let sidebar = view.read_with(cx, |view, _| view.shown());
    let workspace = &sidebar.workspaces[0];
    let hook = ShellStatus {
        status: Blocked.into(),
        source: Some(Source::Hook),
    };
    assert_eq!(workspace.status, hook, "the workspace says so");
    assert_eq!(workspace.tabs[0].status, hook, "the tab holding it says so");
    assert_eq!(workspace.tabs[0].shells[0].status, hook);
    assert_eq!(
        workspace.tabs[1].status,
        ShellStatus::NONE,
        "the tab on screen has nothing running in it and says nothing"
    );

    assert_eq!(sidebar.agents.len(), 1, "one agent, in one row");
    assert_eq!(sidebar.agents[0].status, hook);
    assert!(
        sidebar.elsewhere.is_empty(),
        "it was placed, so there is nothing that could not be"
    );
}

#[gpui::test]
fn what_the_bus_says_next_is_what_the_sidebar_says_next(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let shell = view.read_with(cx, |view, _| view.focused);

    let working = view.read_with(cx, |view, _| running_in(view, shell, Working));
    the_bus_says(&view, cx, vec![working]);
    assert_eq!(
        view.read_with(cx, |view, _| view.shown()).workspaces[0]
            .status
            .status,
        Working.into()
    );

    let blocked = view.read_with(cx, |view, _| running_in(view, shell, Blocked));
    the_bus_says(&view, cx, vec![blocked]);
    assert_eq!(
        view.read_with(cx, |view, _| view.shown()).workspaces[0]
            .status
            .status,
        Blocked.into()
    );

    the_bus_says(&view, cx, Vec::new());
    assert_eq!(
        view.read_with(cx, |view, _| view.shown()).workspaces[0].status,
        ShellStatus::NONE,
        "a snapshot that no longer lists it is a session that is over"
    );
}

#[gpui::test]
fn pressing_an_agent_shows_the_shell_it_is_running_in(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let hidden = view.read_with(cx, |view, _| view.focused);
    let first = view.read_with(cx, |view, _| view.active);
    cx.simulate_keystrokes(keys().new_tab);
    assert_ne!(
        view.read_with(cx, |view, _| view.active),
        first,
        "the shell the agent is in is not the one on screen"
    );

    let session = view.read_with(cx, |view, _| running_in(view, hidden, Blocked));
    the_bus_says(&view, cx, vec![session]);

    let row = view.read_with(cx, |view, _| view.shown()).agents[0].address;
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.picked(Picked::Shell(row), window, cx));
    });
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(view.active, first, "its tab is the one showing");
        assert_eq!(
            view.focused, hidden,
            "and its shell is the one being typed in"
        );
    });
}

#[gpui::test]
fn folding_a_workspace_takes_its_tabs_off_the_sidebar_and_leaves_its_badge(
    cx: &mut TestAppContext,
) {
    let (view, cx) = opened(cx);
    let shell = view.read_with(cx, |view, _| view.focused);
    let workspace = view.read_with(cx, |view, _| view.workspace);
    let session = view.read_with(cx, |view, _| running_in(view, shell, Blocked));
    the_bus_says(&view, cx, vec![session]);

    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.picked(Picked::FoldWorkspace(workspace), window, cx);
        });
    });

    let sidebar = view.read_with(cx, |view, _| view.shown());
    assert!(sidebar.workspaces[0].folded);
    assert!(
        sidebar.workspaces[0].tabs.is_empty(),
        "what is under it is folded away"
    );
    assert_eq!(
        sidebar.workspaces[0].status.status,
        Blocked.into(),
        "and its badge is what is left saying something in there needs somebody"
    );
    assert_eq!(
        sidebar.agents.len(),
        1,
        "the list of agents is not folded away with it"
    );
}

/// A folder to open a workspace on, and its path as the shell running in it
/// will report it.
///
/// Resolved, because a temporary directory is very often reached through a
/// symbolic link — `/tmp` on a Mac is one — and a shell asked where it is
/// answers with where it actually is.
fn folder() -> (tempfile::TempDir, std::path::PathBuf) {
    let folder = tempfile::tempdir().expect("a temporary directory");
    let resolved = folder
        .path()
        .canonicalize()
        .expect("a temporary directory that is there");
    (folder, resolved)
}

/// Opens a workspace on `on`, the way choosing it in the folder picker does,
/// and waits for the window to settle.
///
/// The picker itself is not here: the platform these tests run on has no
/// dialogs to show, and what it would hand back is a path, which is what this
/// hands over instead.
fn open_workspace(view: &Entity<TerminalView>, cx: &mut VisualTestContext, on: &std::path::Path) {
    let on = on.to_owned();
    cx.update(|window, cx| {
        view.update(cx, |view, cx| view.open_workspace(&on, window, cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
fn opening_a_folder_puts_a_workspace_on_it_with_one_shell_in_it(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let (_folder, path) = folder();

    open_workspace(&view, cx, &path);

    let opened = view.read_with(cx, |view, _| {
        let workspaces = view.layout.workspaces();
        assert_eq!(workspaces.len(), 2, "the folder chosen is a workspace now");
        let opened = &workspaces[1];
        assert_eq!(opened.path(), path, "on the folder that was chosen");
        assert_eq!(opened.tabs().len(), 1, "with one tab in it");
        assert_eq!(opened.tabs()[0].shells().len(), 1, "and one shell in that");
        assert_eq!(view.shells.len(), 2, "which has a process of its own");
        assert_eq!(view.workspace, opened.id(), "it is the one on screen");
        assert_eq!(
            view.focused.workspace,
            opened.id(),
            "and its shell is the one being typed in"
        );
        opened.id()
    });

    // The shell it opened with runs in the folder the workspace is on, which is
    // the whole of what the workspace contributes to starting one — and the
    // reason a folder set up to run its shells somewhere else needs nothing
    // added here to do so.
    type_out(cx, "pwd");
    cx.simulate_keystrokes("enter");
    wait_for_line(&view, cx, &path.display().to_string());

    let sidebar = view.read_with(cx, |view, _| view.shown());
    let listed = &sidebar.workspaces[1];
    assert_eq!(
        listed.workspace, opened,
        "the sidebar lists it straight away"
    );
    assert_eq!(
        listed.name.as_ref(),
        path.file_name().expect("a folder with a name"),
        "under the name of the folder it is on"
    );
    assert_eq!(listed.tabs.len(), 1, "with its tab under it");
}

#[gpui::test]
fn a_folder_already_open_is_shown_rather_than_opened_twice(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let (_folder, path) = folder();
    let first = view.read_with(cx, |view, _| view.workspace);

    open_workspace(&view, cx, &path);
    let second = view.read_with(cx, |view, _| view.workspace);
    assert_ne!(second, first, "the folder chosen was not the one open");

    // The folder this window was opened on, chosen again from inside it.
    let already = view.read_with(cx, |view, _| {
        view.layout
            .workspace(first)
            .expect("the workspace this window opened on")
            .path()
            .to_owned()
    });
    open_workspace(&view, cx, &already);

    view.read_with(cx, |view, _| {
        assert_eq!(
            view.layout.workspaces().len(),
            2,
            "a folder already open is not opened a second time"
        );
        assert_eq!(
            view.shells.len(),
            2,
            "and no second shell was started in it"
        );
        assert_eq!(view.workspace, first, "it is shown instead");
        assert_eq!(
            view.focused.workspace, first,
            "with focus back in the shell it was left in"
        );
    });
}

#[gpui::test]
fn closing_a_workspace_takes_its_shells_with_it(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let (_folder, path) = folder();
    let first = view.read_with(cx, |view, _| view.workspace);

    open_workspace(&view, cx, &path);
    // A second shell in it, so that what is closed is a workspace with an
    // arrangement in it rather than a single shell wearing a workspace's name.
    cx.simulate_keystrokes(keys().split_right);
    assert_eq!(view.read_with(cx, |view, _| view.shells.len()), 3);

    cx.dispatch_action(CloseWorkspace);
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(view.layout.workspaces().len(), 1, "the workspace is gone");
        assert_eq!(
            view.shells.len(),
            1,
            "and both processes that were running in it with it"
        );
        assert_eq!(view.workspace, first, "what is left is what is on screen");
        assert_eq!(
            view.focused.workspace, first,
            "with focus in one of its shells"
        );
    });
    assert!(
        !cx.windows().is_empty(),
        "closing one workspace of two is not a way out of the application"
    );
}

#[gpui::test]
fn the_last_workspace_cannot_be_closed(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);

    cx.dispatch_action(CloseWorkspace);
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(
            view.layout.workspaces().len(),
            1,
            "emptying the application is not something it offers"
        );
        assert_eq!(view.shells.len(), 1, "and its shell is still running");
    });
    assert!(!cx.windows().is_empty(), "nor is it a way out of it");
}

#[gpui::test]
fn closing_the_last_shell_of_a_workspace_closes_the_workspace(cx: &mut TestAppContext) {
    let (view, cx) = opened(cx);
    let (_folder, path) = folder();
    let first = view.read_with(cx, |view, _| view.workspace);

    open_workspace(&view, cx, &path);
    cx.simulate_keystrokes(keys().close);
    cx.run_until_parked();

    view.read_with(cx, |view, _| {
        assert_eq!(
            view.layout.workspaces().len(),
            1,
            "a workspace with nothing open in it is not something to look at"
        );
        assert_eq!(view.workspace, first);
        assert_eq!(view.shells.len(), 1);
    });
    assert!(!cx.windows().is_empty());
}

#[gpui::test]
fn a_menu_item_does_what_its_chord_does(cx: &mut TestAppContext) {
    let script = both_ways();
    for binding in crate::keymap::bindings() {
        let action = binding.action();
        assert!(
            unwalkable().iter().any(|out| out.partial_eq(action))
                || script.iter().any(|(named, _)| named.partial_eq(action)),
            "`{}` can be chosen or pressed, and nothing here does both",
            action.name()
        );
    }

    let by_chord = walked(cx, By::Chord);
    let by_menu = walked(cx, By::Menu);

    assert_eq!(
        by_menu, by_chord,
        "the two ways in reach the same actions, so they cannot leave the window \
         saying different things"
    );
}

/// What the stub container command below calls the container it brings up.
const CONTAINER: &str = "brave_kepler";

/// What the stub bus below writes down every run of it in.
const INSTALLED: &str = "installed";

/// A directory holding a stand-in for the container command and one for the
/// bus, and a window whose workspace runs its shells in a container through
/// them.
///
/// The container command is written rather than borrowed from the machine
/// because these tests have to run where no container runtime exists, and
/// because what is worth asserting is what the application asks for. It answers
/// being asked to bring a container up the way the real one does — with an
/// account naming the container — and answers being asked to run something by
/// running it here, with the environment it was told to carry across, which is
/// what makes the shell in these tests a real shell on a real terminal device.
///
/// The bus's stand-in writes down what it was asked and does nothing else, and
/// `refuses` is whether it then says no.
fn in_a_container<'a>(
    cx: &'a mut TestAppContext,
    dir: &Path,
    refuses: bool,
) -> (Entity<TerminalView>, &'a mut VisualTestContext) {
    use std::os::unix::fs::PermissionsExt;

    let runnable = std::fs::Permissions::from_mode(0o755);
    let container = dir.join(shepherd_core::devcontainer::COMMAND);
    std::fs::write(
        &container,
        format!(
            r#"#!/bin/sh
case "$1" in
up)
  echo '{{"outcome":"success","containerId":"{CONTAINER}"}}'
  ;;
exec)
  shift
  while [ $# -gt 0 ]; do
    case "$1" in
      --workspace-folder) shift 2 ;;
      --remote-env) export "$2"; shift 2 ;;
      --) shift; break ;;
      *) shift ;;
    esac
  done
  exec "$@"
  ;;
esac
"#
        ),
    )
    .expect("a container command");
    std::fs::set_permissions(&container, runnable.clone()).expect("one anybody may run");

    let bus = dir.join(shepherd_core::daemon::COMMAND);
    let refusal = if refuses { "exit 1" } else { "exit 0" };
    std::fs::write(
        &bus,
        format!("#!/bin/sh\necho \"$@\" >> \"$(dirname \"$0\")/{INSTALLED}\"\n{refusal}\n"),
    )
    .expect("a bus command");
    std::fs::set_permissions(&bus, runnable).expect("one anybody may run");

    let (view, cx) = opened(cx);
    view.update(cx, |view, _| view.commands_in(dir));
    // The choice a person makes on the menu: this workspace's shells go in its
    // development container from now on.
    cx.update(|window, cx| window.dispatch_action(Box::new(UseContainer), cx));
    cx.run_until_parked();
    (view, cx)
}

/// Everything the stub bus has been asked, one line per run of it.
fn installed(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join(INSTALLED))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect()
}

/// Waits until `ready`, letting whatever the window has running away from this
/// thread finish between looks.
fn wait_while_working(
    view: &Entity<TerminalView>,
    cx: &mut VisualTestContext,
    expectation: &str,
    mut ready: impl FnMut(&TerminalView) -> bool,
) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, _| ready(view)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waited {PATIENCE:?} for {expectation}"
        );
        thread::sleep(GLANCE);
    }
}

#[gpui::test]
#[cfg(unix)]
fn a_shell_of_a_workspace_using_a_container_is_started_inside_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (view, cx) = in_a_container(cx, dir.path(), false);

    // Bringing a container up may build an image, and a window that waited for
    // that on the thread it draws on would be a window that had stopped
    // drawing. So asking for a tab comes straight back, with the tab open and
    // no shell in it yet: what the window did was ask, not wait.
    view.update_in(cx, |view, window, cx| view.new_tab(window, cx));
    view.read_with(cx, |view, _| {
        assert_eq!(view.open().tabs().len(), 2);
        assert_eq!(
            view.shells.len(),
            1,
            "the window waited for a container on the thread it draws on"
        );
    });

    wait_while_working(&view, cx, "the shell in the container to start", |view| {
        view.shells.len() == 2
    });

    // A shell that answers is a shell whose process was started through the
    // container command, since that command is the only thing on the path this
    // window looks at and it is what ran the shell.
    type_out(cx, "echo inside");
    cx.simulate_keystrokes("enter");
    wait_for_line(&view, cx, "inside");
}

#[gpui::test]
#[cfg(unix)]
fn the_bus_is_put_into_the_container_once_however_many_shells_open_in_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (view, cx) = in_a_container(cx, dir.path(), false);
    let workspace = view.read_with(cx, |view, _| view.workspace);

    cx.simulate_keystrokes(keys().new_tab);
    wait_while_working(&view, cx, "the bus to go into the container", |view| {
        view.provisioning.of(workspace) == Some(Standing::Ready)
    });

    assert_eq!(
        installed(dir.path()),
        vec![format!("install docker {CONTAINER}")],
        "the container was named to the bus in some other way than the command a person would type"
    );

    // A second shell in the same container, and a third: the command is
    // idempotent, and running it again would be time spent for nothing.
    cx.simulate_keystrokes(keys().new_tab);
    cx.simulate_keystrokes(keys().split_right);
    wait_while_working(&view, cx, "both further shells to start", |view| {
        view.shells.len() == 4
    });

    assert_eq!(
        installed(dir.path()).len(),
        1,
        "the bus was put into one container more than once"
    );
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.shown().workspaces[0]
                .container
                .as_ref()
                .map(AsRef::as_ref),
            Some("container"),
            "the row does not say where this workspace's shells run"
        );
    });
}

#[gpui::test]
#[cfg(unix)]
fn a_bus_that_will_not_go_in_leaves_the_workspace_working_and_says_so(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (view, cx) = in_a_container(cx, dir.path(), true);
    let workspace = view.read_with(cx, |view, _| view.workspace);

    cx.simulate_keystrokes(keys().new_tab);
    wait_while_working(
        &view,
        cx,
        "the bus to refuse to go into the container",
        |view| view.provisioning.of(workspace) == Some(Standing::Unreported),
    );

    view.read_with(cx, |view, _| {
        assert_eq!(
            view.shells.len(),
            2,
            "the shell in the container did not start"
        );
        assert_eq!(
            view.layout.workspaces().len(),
            1,
            "the workspace was taken away"
        );
        assert_eq!(
            view.shown().workspaces[0]
                .container
                .as_ref()
                .map(AsRef::as_ref),
            Some("container: agents unreported"),
            "the row does not say that agents in this container will go unreported"
        );
    });
    // And the shell in there is a working shell, whatever the bus did.
    type_out(cx, "echo working");
    cx.simulate_keystrokes("enter");
    wait_for_line(&view, cx, "working");
}

#[gpui::test]
#[cfg(unix)]
fn a_workspace_taken_back_off_its_container_starts_its_shells_here_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (view, cx) = in_a_container(cx, dir.path(), false);
    let workspace = view.read_with(cx, |view, _| view.workspace);

    cx.simulate_keystrokes(keys().new_tab);
    wait_while_working(&view, cx, "the bus to go into the container", |view| {
        view.provisioning.of(workspace) == Some(Standing::Ready)
    });

    cx.update(|window, cx| window.dispatch_action(Box::new(UseContainer), cx));
    cx.run_until_parked();
    cx.simulate_keystrokes(keys().new_tab);
    wait_while_working(&view, cx, "a shell on this machine to start", |view| {
        view.shells.len() == 3
    });

    view.read_with(cx, |view, _| {
        assert_eq!(
            view.starting.get(&workspace),
            Some(&Shells::ThisMachine),
            "shells opened after the choice was taken back still go into the container"
        );
        assert_eq!(
            view.shown().workspaces[0].container,
            None,
            "the row still says the workspace's shells run somewhere else"
        );
    });
}
