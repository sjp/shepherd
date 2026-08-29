//! What the menu bar offers, and the two items on it the application answers
//! for itself.
//!
//! Everything this application does to a shell it does because an action was
//! dispatched, and one module over is the table of chords those actions are
//! bound to. That table is invisible: somebody who has not read it has no way
//! to find out that a tab can be opened at all, or what to press to open one.
//! The menus are where it becomes visible — every item here names the same
//! action its chord names, so the two ways in cannot come to mean different
//! things, and the chord drawn beside each item is looked up by the platform in
//! that same keymap rather than spelt out again here.
//!
//! Two items go the other way and answer to no keys, so that a menu is the only
//! way to reach them: saying what this application is, which no platform has a
//! chord for; and closing a workspace, which takes every shell in one away at
//! once and is better reached deliberately than by a slip of the fingers.
//!
//! # Two of them are the application's own
//!
//! About and Quit are answered here, as standing handlers on the application
//! itself, rather than by the part of the window that draws shells. That is
//! what makes them work when nothing in the window has focus — including while
//! the About panel is up — and what stops the platform drawing them greyed out,
//! which it does to any menu item whose action nothing is currently prepared to
//! answer.
//!
//! # Where there is no menu bar
//!
//! Only macOS puts an application's menus on the screen. Everywhere else the
//! toolkit is given them and has nowhere to show them, and that is the whole of
//! the difference: the actions, the handlers and the keymap are the same
//! everywhere, and what a platform without a menu bar loses is somewhere to
//! point, not something to do.

use gpui::{App, Menu, MenuItem, ParentElement as _, Window};
use gpui_component::WindowExt as _;

use crate::keymap::{
    About, Close, CloseWorkspace, FocusDown, FocusLeft, FocusRight, FocusUp, NewTab, NextTab,
    OpenWorkspace, PreviousTab, Quit, SplitDown, SplitRight,
};

/// What this application is called, wherever it says so.
pub const NAME: &str = "Shepherd";

/// Which build this is.
///
/// The crate's own version, which is the workspace's: everything this
/// repository builds is versioned together, so there is one number and this is
/// it.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The menus, in the order the platform shows them.
///
/// Nothing here says what an item does. Each names an action, and the action
/// goes wherever an action goes — which, for everything in the second menu, is
/// the shell that has focus. That is the same journey the chord makes.
///
/// The second menu holds more than opening and closing: moving between tabs and
/// moving focus around an arrangement are on it too, because a chord that
/// appears in no menu is a chord that has to be read about somewhere else, and
/// there is nowhere else. It begins with the workspaces, which are the folders
/// everything below them is opened inside of, and works inwards from there.
fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: NAME.into(),
            items: vec![
                MenuItem::action(format!("About {NAME}"), About),
                MenuItem::separator(),
                MenuItem::action(format!("Quit {NAME}"), Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open Workspace\u{2026}", OpenWorkspace),
                MenuItem::action("Close Workspace", CloseWorkspace),
                MenuItem::separator(),
                MenuItem::action("New Tab", NewTab),
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PreviousTab),
                MenuItem::separator(),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::separator(),
                MenuItem::action("Focus Left", FocusLeft),
                MenuItem::action("Focus Right", FocusRight),
                MenuItem::action("Focus Up", FocusUp),
                MenuItem::action("Focus Down", FocusDown),
                MenuItem::separator(),
                MenuItem::action("Close Shell", Close),
            ],
        },
    ]
}

/// Puts the menus in place, and answers the two items on them the application
/// answers for itself.
///
/// After the keymap and not before it: the platform reads the keymap as it
/// builds the menus, to draw each item's chord beside it, and a binding
/// installed afterwards is one the menus were built without.
pub fn install(cx: &mut App) {
    // The same way out as closing the last window, and deliberately the same
    // one: the teardown that stops what this application started hangs off the
    // toolkit's own quit, so anything that quits by any other route would skip
    // it.
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &About, cx: &mut App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        // Afterwards rather than now: an action is answered from inside the
        // window it was dispatched in, and a window cannot be asked to show
        // something while it is in the middle of doing that. What is deferred
        // here runs a moment later, out of that call and on the same thread.
        cx.defer(move |cx| {
            // A window that cannot be reached is a window on its way out, which
            // is answer enough to somebody asking what this application is.
            let _ = window.update(cx, |_, window, cx| about(window, cx));
        });
    });
    cx.set_menus(menus());
}

/// Says what this application is: its name, and which build of it this is.
///
/// A panel over the window rather than a window of its own. It has two things
/// to say, and a second window to find, move and close again before a shell can
/// be typed in would be more of an application than either of them is worth.
fn about(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, _| {
        dialog
            .title(NAME)
            .alert()
            .child(format!("version {VERSION}"))
    });
}

#[cfg(test)]
mod tests {
    use gpui::{Action, AppContext as _, Context, IntoElement, Render, TestAppContext, div};
    use gpui_component::Root;

    use super::*;
    use crate::keymap;

    /// Every action the menus name, wherever in them it appears.
    fn actions() -> Vec<Box<dyn Action>> {
        fn walk(items: Vec<MenuItem>, into: &mut Vec<Box<dyn Action>>) {
            for item in items {
                match item {
                    MenuItem::Action { action, .. } => into.push(action),
                    MenuItem::Submenu(menu) => walk(menu.items, into),
                    MenuItem::Separator | MenuItem::SystemMenu(_) => {}
                }
            }
        }

        let mut actions = Vec::new();
        for menu in menus() {
            walk(menu.items, &mut actions);
        }
        actions
    }

    /// The two items that deliberately answer to no keys.
    ///
    /// Saying what this application is, because a chord for it would be one
    /// this application had invented and no platform asks for; and closing a
    /// workspace, because it takes every shell in one away at once and the
    /// obvious chord for it is a shift away from the one that closes a single
    /// shell. Both are reachable by name on a menu, which is the right amount
    /// of reachable for something done rarely and regretted immediately.
    fn unbound() -> Vec<Box<dyn Action>> {
        vec![About.boxed_clone(), CloseWorkspace.boxed_clone()]
    }

    /// A window with nothing in it but the layer a panel is shown on.
    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[test]
    fn every_chord_is_on_a_menu() {
        let menus = actions();
        for binding in keymap::bindings() {
            let action = binding.action();
            assert!(
                menus.iter().any(|named| named.partial_eq(action)),
                "`{}` answers to keys and is on no menu, so nobody can find out it exists",
                action.name()
            );
        }
    }

    #[test]
    fn every_menu_item_shows_a_chord_but_the_two_that_answer_to_none() {
        let bound = keymap::bindings();
        for action in actions() {
            if unbound()
                .iter()
                .any(|none| none.partial_eq(action.as_ref()))
            {
                continue;
            }
            assert!(
                bound
                    .iter()
                    .any(|binding| binding.action().partial_eq(action.as_ref())),
                "`{}` is on a menu with no chord beside it, because nothing binds it",
                action.name()
            );
        }
    }

    #[test]
    fn the_items_meant_to_have_no_chord_have_none() {
        let bound = keymap::bindings();
        for action in unbound() {
            assert!(
                !bound
                    .iter()
                    .any(|binding| binding.action().partial_eq(action.as_ref())),
                "`{}` is bound to keys, and the reason it is on no keymap is written above",
                action.name()
            );
        }
    }

    #[gpui::test]
    fn the_application_answers_for_its_own_two_items(cx: &mut TestAppContext) {
        cx.update(keymap::install);
        cx.update(install);

        cx.update(|cx| {
            for action in [About.boxed_clone(), Quit.boxed_clone()] {
                assert!(
                    cx.is_action_available(action.as_ref()),
                    "`{}` would be drawn greyed out, because nothing is prepared to answer it",
                    action.name()
                );
            }
        });
    }

    #[gpui::test]
    fn the_about_shows_what_this_is(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        cx.update(keymap::install);
        cx.update(install);

        let (_, cx) = cx.add_window_view(|window, cx| {
            let empty = cx.new(|_| Empty);
            Root::new(empty, window, cx)
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();

        cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
        cx.dispatch_action(About);
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "choosing About says nothing about the application"
            );
        });
    }
}
