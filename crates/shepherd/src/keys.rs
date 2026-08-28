//! What a key press is worth, in bytes.
//!
//! A shell is a process reading from a terminal device, and a terminal device
//! carries bytes. So between a person pressing a key and a program noticing sits
//! a translation with no obvious inverse: `a` is one byte, control-C is one
//! quite different byte, and the left arrow is three or four depending on a mode
//! the program itself turned on. This module is that translation and nothing
//! else — it takes a key press and the modes the emulator is currently in, and
//! answers with the bytes, or with nothing where the press means nothing to a
//! terminal.
//!
//! # Why this is separate from the keymap
//!
//! One module over decides which key presses are *this application's* — open a
//! tab, split, move focus. Those never reach here: they are dispatched as
//! actions and stop there. Everything else is typing, and typing is what this
//! is for. Keeping the two apart is deliberate. A bound key is answered by
//! whichever part of the window the toolkit decides should answer it; an
//! unbound key always goes to the one shell that has focus, whatever else is on
//! screen. Writing them as one table would mean one of those two rules had to
//! bend.
//!
//! # The shape of the sequences
//!
//! Most of what a terminal expects is `ESC [` followed by a parameter and a
//! final letter. The parts that are not regular are historical rather than
//! designed: the arrow keys have a second form a program can ask for by turning
//! on application cursor mode, the first four function keys use a different
//! introducer from the rest, and backspace sends delete. Each of those is
//! written out below where it applies rather than generalised, because the
//! generalisation would be a lie.
//!
//! # Alt is Meta
//!
//! A key pressed with alt held sends an escape and then the key, which is what
//! a shell's line editor and every full-screen program built on one expect.
//! The alternative reading — alt as a way of composing characters the keyboard
//! has no key for, which is what macOS does by default — is the one a terminal
//! gives up in exchange, and it is the trade every terminal aimed at
//! programmers makes.

use alacritty_terminal::term::TermMode;
use gpui::{Keystroke, Modifiers};

/// The escape byte: the introducer of every sequence here, and what Meta puts
/// in front of an ordinary one.
const ESC: u8 = 0x1b;

/// The bytes `key` sends to a terminal in `mode`, or `None` where it sends
/// nothing at all.
///
/// Nothing at all is the honest answer for a chord built on the platform key —
/// command on macOS, super elsewhere — which no terminal has an encoding for.
/// A person pressing one has asked the window for something, and if the window
/// had anything to give they would not have arrived here.
pub fn encoded(key: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    if key.modifiers.platform || key.modifiers.function {
        return None;
    }
    if let Some(bytes) = sequence(&key.key, &key.modifiers, mode) {
        return Some(bytes);
    }
    let text = text(key)?;
    let control = key.modifiers.control.then(|| control(text)).flatten();
    let bytes = match control {
        Some(byte) => vec![byte],
        None => text.as_bytes().to_vec(),
    };
    Some(meta(&key.modifiers, bytes))
}

/// The bytes for the keys that are named rather than typed.
fn sequence(key: &str, modifiers: &Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let bytes = match key {
        "enter" => meta(modifiers, vec![b'\r']),
        "escape" => meta(modifiers, vec![ESC]),
        // Back-tab is its own sequence rather than a tab carrying a shift
        // parameter: it is the form every program that reads a tab already
        // knows, and the parameterised one is not.
        "tab" if modifiers.shift => csi("", b'Z'),
        "tab" => meta(modifiers, vec![b'\t']),
        // Delete, from the key marked backspace. The two have been crossed
        // since terminals had ribbons, and a line editor asked for anything
        // else deletes nothing.
        "backspace" => meta(modifiers, vec![if modifiers.control { 0x08 } else { 0x7f }]),
        "space" => meta(modifiers, vec![if modifiers.control { 0x00 } else { b' ' }]),
        "up" => cursor(b'A', modifiers, mode),
        "down" => cursor(b'B', modifiers, mode),
        "right" => cursor(b'C', modifiers, mode),
        "left" => cursor(b'D', modifiers, mode),
        "home" => cursor(b'H', modifiers, mode),
        "end" => cursor(b'F', modifiers, mode),
        "insert" => tilde(2, modifiers),
        "delete" => tilde(3, modifiers),
        "pageup" => tilde(5, modifiers),
        "pagedown" => tilde(6, modifiers),
        _ => return function(key, modifiers),
    };
    Some(bytes)
}

/// The bytes a function key sends.
///
/// The first four are single-shift sequences and the rest are numbered, and the
/// numbers themselves skip: this is the table a terminal actually sends rather
/// than one anybody would choose.
fn function(key: &str, modifiers: &Modifiers) -> Option<Vec<u8>> {
    const NUMBERED: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];

    let number: usize = key.strip_prefix('f')?.parse().ok()?;
    match number {
        1..=4 => {
            // `P` through `S`, in order, which is why the arithmetic is on the
            // letter rather than in a table.
            let last = b'P' + u8::try_from(number - 1).expect("four at most");
            Some(single_shift(last, modifiers))
        }
        5..=12 => Some(tilde(NUMBERED[number - 5], modifiers)),
        _ => None,
    }
}

/// The bytes an arrow or a home or end key sends.
///
/// Application cursor mode is a program saying it would rather have the second
/// form, and full-screen programs generally do. It only applies to the
/// unmodified press: a modified one has to carry its parameter, and only the
/// first form has anywhere to put one.
fn cursor(last: u8, modifiers: &Modifiers, mode: TermMode) -> Vec<u8> {
    let parameter = parameter(modifiers);
    if parameter > 1 {
        csi(&format!("1;{parameter}"), last)
    } else if mode.contains(TermMode::APP_CURSOR) {
        vec![ESC, b'O', last]
    } else {
        csi("", last)
    }
}

/// A single-shift sequence, and the parameterised form of it that a modifier
/// forces.
fn single_shift(last: u8, modifiers: &Modifiers) -> Vec<u8> {
    match parameter(modifiers) {
        1 => vec![ESC, b'O', last],
        parameter => csi(&format!("1;{parameter}"), last),
    }
}

/// One of the sequences that names a key by number and ends in a tilde.
fn tilde(number: u8, modifiers: &Modifiers) -> Vec<u8> {
    match parameter(modifiers) {
        1 => csi(&number.to_string(), b'~'),
        parameter => csi(&format!("{number};{parameter}"), b'~'),
    }
}

/// `ESC [`, some parameters, and the letter that says which key it was.
fn csi(parameters: &str, last: u8) -> Vec<u8> {
    let mut bytes = vec![ESC, b'['];
    bytes.extend_from_slice(parameters.as_bytes());
    bytes.push(last);
    bytes
}

/// The number a sequence carries its modifiers in: one, plus a bit each for
/// shift, alt and control.
fn parameter(modifiers: &Modifiers) -> u8 {
    1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.control)
}

/// `bytes`, with the escape in front that alt held down means.
fn meta(modifiers: &Modifiers, mut bytes: Vec<u8>) -> Vec<u8> {
    if modifiers.alt {
        bytes.insert(0, ESC);
    }
    bytes
}

/// The text a key press stands for, before control or alt have had their say.
///
/// A key the keyboard names rather than prints — every one of which the table
/// above has already had its chance at — sends nothing: a name that reaches
/// here is a key this does not know, and inventing bytes for it would put the
/// name itself down the terminal.
///
/// Otherwise the character the layout produced is preferred, because it is the
/// one that knows what the key means on this machine — with one exception. Alt
/// is Meta here, so the escape goes in front of the key itself rather than in
/// front of whatever the layout would have composed with alt held; where the
/// composition stayed inside ASCII the two agree anyway, and the composed form
/// is the one carrying the shift.
fn text(key: &Keystroke) -> Option<&str> {
    let printed = single(&key.key)?;
    match key.key_char.as_deref() {
        Some(text) if !key.modifiers.alt || text.is_ascii() => Some(text),
        _ => Some(printed),
    }
}

/// `key`, if it is one character; nothing if it is a name.
fn single(key: &str) -> Option<&str> {
    let mut characters = key.chars();
    characters.next()?;
    characters.next().is_none().then_some(key)
}

/// The control code a character stands for with control held.
///
/// The letters are the regular part: control clears the top bits, so `a`
/// becomes one. The punctuation is the rest of that same arithmetic, and it is
/// listed rather than computed because only some of it survives a keyboard.
fn control(text: &str) -> Option<u8> {
    let character = single(text)?.chars().next()?;
    Some(match character {
        'a'..='z' | 'A'..='Z' => character.to_ascii_lowercase() as u8 - b'a' + 1,
        '@' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' | '/' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key press as the toolkit reports one, with the character a layout
    /// would have produced filled in the way it fills it in.
    fn pressed(keystroke: &str) -> Keystroke {
        Keystroke::parse(keystroke)
            .expect("a keystroke this module's own tests can spell")
            .with_simulated_ime()
    }

    /// What `keystroke` sends, with no mode turned on.
    fn sent(keystroke: &str) -> Vec<u8> {
        encoded(&pressed(keystroke), TermMode::NONE).expect("a key that sends something")
    }

    #[test]
    fn typing_sends_the_characters_typed() {
        assert_eq!(sent("a"), b"a");
        assert_eq!(sent("shift-a"), b"A");
        assert_eq!(sent("7"), b"7");
        assert_eq!(sent("space"), b" ");
    }

    #[test]
    fn the_keys_a_line_is_edited_with_send_what_a_line_editor_reads() {
        assert_eq!(sent("enter"), b"\r");
        assert_eq!(sent("tab"), b"\t");
        assert_eq!(sent("escape"), b"\x1b");
        assert_eq!(
            sent("backspace"),
            b"\x7f",
            "the key marked backspace sends delete"
        );
        assert_eq!(
            sent("shift-tab"),
            b"\x1b[Z",
            "back-tab is its own sequence, not a modified tab"
        );
    }

    #[test]
    fn control_sends_the_control_codes() {
        assert_eq!(sent("ctrl-c"), b"\x03", "interrupt");
        assert_eq!(sent("ctrl-d"), b"\x04", "end of input");
        assert_eq!(sent("ctrl-z"), b"\x1a", "suspend");
        assert_eq!(sent("ctrl-space"), b"\0");
        assert_eq!(sent("ctrl-["), b"\x1b");
        assert_eq!(sent("ctrl-?"), b"\x7f");
    }

    #[test]
    fn control_on_a_key_with_no_code_of_its_own_types_it() {
        assert_eq!(
            sent("ctrl-1"),
            b"1",
            "there is no control-1, so the digit is what was typed"
        );
    }

    #[test]
    fn alt_puts_an_escape_in_front() {
        assert_eq!(sent("alt-b"), b"\x1bb");
        assert_eq!(sent("alt-enter"), b"\x1b\r");
        assert_eq!(sent("alt-backspace"), b"\x1b\x7f");
    }

    #[test]
    fn the_arrows_take_the_form_the_program_asked_for() {
        assert_eq!(sent("up"), b"\x1b[A");
        assert_eq!(sent("down"), b"\x1b[B");
        assert_eq!(sent("right"), b"\x1b[C");
        assert_eq!(sent("left"), b"\x1b[D");

        let application = |keystroke| {
            encoded(&pressed(keystroke), TermMode::APP_CURSOR).expect("a key that sends something")
        };
        assert_eq!(application("up"), b"\x1bOA");
        assert_eq!(application("home"), b"\x1bOH");
        assert_eq!(application("end"), b"\x1bOF");
    }

    #[test]
    fn a_modified_arrow_carries_its_modifiers_as_a_number() {
        assert_eq!(sent("shift-up"), b"\x1b[1;2A");
        assert_eq!(sent("alt-up"), b"\x1b[1;3A");
        assert_eq!(sent("ctrl-up"), b"\x1b[1;5A");
        assert_eq!(sent("ctrl-shift-up"), b"\x1b[1;6A");
        assert_eq!(
            encoded(&pressed("ctrl-left"), TermMode::APP_CURSOR).expect("a key that sends"),
            b"\x1b[1;5D",
            "only the unmodified form has a second version"
        );
    }

    #[test]
    fn the_keys_around_the_arrows_send_their_numbers() {
        assert_eq!(sent("insert"), b"\x1b[2~");
        assert_eq!(sent("delete"), b"\x1b[3~");
        assert_eq!(sent("pageup"), b"\x1b[5~");
        assert_eq!(sent("pagedown"), b"\x1b[6~");
        assert_eq!(sent("shift-delete"), b"\x1b[3;2~");
    }

    #[test]
    fn the_function_keys_send_the_table_a_terminal_sends() {
        assert_eq!(sent("f1"), b"\x1bOP");
        assert_eq!(sent("f4"), b"\x1bOS");
        assert_eq!(sent("f5"), b"\x1b[15~");
        assert_eq!(sent("f6"), b"\x1b[17~", "sixteen is not a function key");
        assert_eq!(sent("f12"), b"\x1b[24~");
        assert_eq!(sent("shift-f1"), b"\x1b[1;2P");
        assert_eq!(
            encoded(&pressed("f13"), TermMode::NONE),
            None,
            "past the end of the table there is nothing to send"
        );
    }

    #[test]
    fn a_chord_on_the_platform_key_sends_nothing() {
        assert_eq!(
            encoded(&pressed("cmd-t"), TermMode::NONE),
            None,
            "no terminal has an encoding for it, so nothing is typed"
        );
    }

    #[test]
    fn a_key_that_is_only_a_name_sends_nothing() {
        assert_eq!(
            encoded(&pressed("f36"), TermMode::NONE),
            None,
            "a key this does not know does not send its own name"
        );
    }
}
