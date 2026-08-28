//! The key presses that are the window's rather than the shell's.
//!
//! A terminal owns nearly the whole keyboard: control-C is not a shortcut, it
//! is a byte a program is waiting for, and an application that helps itself to
//! one is an application that cannot run the program. So the set of presses
//! this application answers to is small, deliberately awkward to type by
//! accident, and written down in one table below rather than scattered over the
//! code that acts on it.
//!
//! # Actions, not a dispatch table
//!
//! Each entry is one of the toolkit's own actions, bound to keys through the
//! toolkit's own keymap. That means the binding and the code that answers it
//! never meet: a key press is matched against the keymap, becomes an action,
//! and is delivered to whichever part of the window the toolkit decides should
//! handle it — which, for everything here, is the shell that has focus, because
//! that is where the handlers are registered. Everything that matches nothing
//! here goes on to be typed, and one module over is what it is typed as.
//!
//! # Two tables, because the two platforms disagree
//!
//! There is no arrangement that is idiomatic on both. A Mac's terminals put
//! these on the command key, where nothing a terminal sends can reach; nothing
//! else has a command key, and the same chords built from control alone are
//! exactly the bytes a terminal may not lose. So the defaults follow each
//! platform's own terminals, both tables are compiled everywhere so that both
//! stay honest, and the one that applies is chosen at run time.

use gpui::{App, KeyBinding, actions};

actions!(
    shepherd,
    [
        /// Opens a tab, with a shell in it.
        NewTab,
        /// Shows the next tab along, coming back round at the last.
        NextTab,
        /// Shows the previous one, the same way.
        PreviousTab,
        /// Puts a new shell to the right of this one.
        SplitRight,
        /// Puts a new shell below this one.
        SplitDown,
        /// Closes this shell, and the tab with it if it was the last.
        Close,
        /// Moves focus to the shell to the left.
        FocusLeft,
        /// Moves focus to the shell to the right.
        FocusRight,
        /// Moves focus to the shell above.
        FocusUp,
        /// Moves focus to the shell below.
        FocusDown,
    ]
);

/// The name the bindings below are scoped to, and the name a shell's element
/// announces itself by.
///
/// Everything here applies while a shell has focus and nowhere else, which is
/// what stops a dialog or a text field inheriting a keymap written for a
/// terminal.
pub const CONTEXT: &str = "Shell";

/// Which keys each action answers to.
#[derive(Debug, Clone, Copy)]
pub struct Keys {
    pub new_tab: &'static str,
    pub next_tab: &'static str,
    pub previous_tab: &'static str,
    pub split_right: &'static str,
    pub split_down: &'static str,
    pub close: &'static str,
    pub focus_left: &'static str,
    pub focus_right: &'static str,
    pub focus_up: &'static str,
    pub focus_down: &'static str,
}

/// The defaults on macOS, which are the ones its terminals use.
///
/// Command is free — no terminal sends anything for it — so everything sits
/// there, and splitting keeps the shape a Mac terminal user already has in
/// their hands: one chord for a shell beside this one, the same chord with
/// shift for a shell below it. Stepping between tabs is the bracket pair every
/// application on the platform steps between things with.
const MAC: Keys = Keys {
    new_tab: "cmd-t",
    next_tab: "cmd-shift-]",
    previous_tab: "cmd-shift-[",
    split_right: "cmd-d",
    split_down: "cmd-shift-d",
    close: "cmd-w",
    focus_left: "cmd-alt-left",
    focus_right: "cmd-alt-right",
    focus_up: "cmd-alt-up",
    focus_down: "cmd-alt-down",
};

/// The defaults everywhere else, which are the ones its terminals use.
///
/// Control and shift together, because control alone belongs to the terminal:
/// a keyboard cannot distinguish control-shift-E from control-E on the wire, so
/// nothing is taken from a shell by binding it. That is also why stepping
/// between tabs takes the shift the terminals here manage without: control and
/// a page key on their own are a sequence an application running in a shell is
/// entitled to be sent. Moving focus is the exception and uses alt on its own —
/// it is the action reached for most often, and a three-finger chord for it is
/// the difference between a keyboard people navigate by and one they reach for
/// the mouse instead of.
const ELSEWHERE: Keys = Keys {
    new_tab: "ctrl-shift-t",
    next_tab: "ctrl-shift-pagedown",
    previous_tab: "ctrl-shift-pageup",
    split_right: "ctrl-shift-e",
    split_down: "ctrl-shift-o",
    close: "ctrl-shift-w",
    focus_left: "alt-left",
    focus_right: "alt-right",
    focus_up: "alt-up",
    focus_down: "alt-down",
};

/// The table this machine uses.
///
/// Chosen at run time from a constant compiled on every platform, rather than
/// by compiling only one of them: both tables are then checked by the same
/// tests wherever those tests are run, and a table nobody's machine builds is a
/// table nobody's machine notices is broken.
pub fn table() -> Keys {
    if cfg!(target_os = "macos") {
        MAC
    } else {
        ELSEWHERE
    }
}

/// The keymap, ready to be given to the application.
pub fn bindings() -> Vec<KeyBinding> {
    let keys = table();
    vec![
        KeyBinding::new(keys.new_tab, NewTab, Some(CONTEXT)),
        KeyBinding::new(keys.next_tab, NextTab, Some(CONTEXT)),
        KeyBinding::new(keys.previous_tab, PreviousTab, Some(CONTEXT)),
        KeyBinding::new(keys.split_right, SplitRight, Some(CONTEXT)),
        KeyBinding::new(keys.split_down, SplitDown, Some(CONTEXT)),
        KeyBinding::new(keys.close, Close, Some(CONTEXT)),
        KeyBinding::new(keys.focus_left, FocusLeft, Some(CONTEXT)),
        KeyBinding::new(keys.focus_right, FocusRight, Some(CONTEXT)),
        KeyBinding::new(keys.focus_up, FocusUp, Some(CONTEXT)),
        KeyBinding::new(keys.focus_down, FocusDown, Some(CONTEXT)),
    ]
}

/// Puts the keymap in place, once, for the whole application.
pub fn install(cx: &mut App) {
    cx.bind_keys(bindings());
}

#[cfg(test)]
mod tests {
    use gpui::Keystroke;

    use super::*;

    impl Keys {
        /// Every key in this table, so a test can say something about all of
        /// them without listing them again.
        fn all(self) -> [&'static str; 10] {
            [
                self.new_tab,
                self.next_tab,
                self.previous_tab,
                self.split_right,
                self.split_down,
                self.close,
                self.focus_left,
                self.focus_right,
                self.focus_up,
                self.focus_down,
            ]
        }
    }

    #[test]
    fn every_default_is_a_keystroke_the_toolkit_understands() {
        for table in [MAC, ELSEWHERE] {
            for keys in table.all() {
                Keystroke::parse(keys)
                    .unwrap_or_else(|error| panic!("`{keys}` should parse: {error}"));
            }
        }
    }

    #[test]
    fn no_default_takes_a_key_a_terminal_needs() {
        for table in [MAC, ELSEWHERE] {
            for keys in table.all() {
                let keystroke = Keystroke::parse(keys).expect("a keystroke that parses");
                let modifiers = keystroke.modifiers;
                assert!(
                    !(modifiers.control && !modifiers.shift && !modifiers.alt),
                    "`{keys}` is control and nothing else, which is a control code a \
                     program is entitled to receive"
                );
            }
        }
    }

    #[test]
    fn no_two_actions_answer_to_the_same_keys() {
        for table in [MAC, ELSEWHERE] {
            let mut seen = table.all().to_vec();
            seen.sort_unstable();
            let bound = seen.len();
            seen.dedup();
            assert_eq!(
                bound,
                seen.len(),
                "two actions share a binding in {table:?}"
            );
        }
    }

    #[test]
    fn the_bindings_are_scoped_to_a_shell() {
        for binding in bindings() {
            assert!(
                binding.predicate().is_some(),
                "an unscoped binding would apply wherever focus happened to be"
            );
        }
    }
}
