//! The slices of the evidence a rule may look at.
//!
//! Matching a whole screen is how detection rots: a phrase written for the live
//! chrome at the bottom of the screen eventually matches something a command
//! printed ten minutes ago. Every rule therefore names a **region** — the last N
//! lines, the inside of the prompt box, the terminal title — and sees only
//! that.

use std::fmt;

/// The region a rule looks at when it does not name one.
pub const DEFAULT_REGION: &str = "whole_recent";

/// The engine version that first understood a region, for the ones that were
/// not there from the beginning.
const TOP_NON_EMPTY_LINES_SINCE: u32 = 3;

/// Which slice of the evidence a rule examines.
///
/// The two OSC variants read the side-channels a terminal host can capture; all
/// the others read the screen text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegionSpec {
    /// The whole screen text.
    WholeRecent,
    /// The last N lines, blank ones included.
    BottomLines(u16),
    /// From the Nth-from-last non-blank line to the end.
    BottomNonEmptyLines(u16),
    /// From the top of the text through the Nth non-blank line.
    TopNonEmptyLines(u16),
    /// Everything after the last prompt marker line.
    AfterLastPromptMarker,
    /// Everything before the current prompt marker.
    BeforeCurrentPromptMarker,
    /// The whole screen text, unless a current prompt marker is present.
    WholeRecentWithoutCurrentPromptMarker,
    /// The block marker line that precedes the current prompt marker.
    CurrentPromptBlockMarker,
    /// From that block marker line to the end.
    AfterCurrentPromptBlockMarker,
    /// The lines inside the agent's bordered input box.
    PromptBoxBody,
    /// Everything above that box.
    AbovePromptBox,
    /// The last non-blank line above that box.
    LastNonEmptyAbovePromptBox,
    /// Everything after the last horizontal rule line.
    AfterLastHorizontalRule,
    /// The terminal title reported over OSC, never the screen.
    OscTitle,
    /// The progress payload reported over OSC, never the screen.
    OscProgress,
}

impl RegionSpec {
    /// Parses the `region` a rule declared.
    ///
    /// Surrounding whitespace is ignored. A parameterized region is written
    /// `name(N)`, where N is a positive decimal with no leading zero, at most
    /// 65535 — a spelling narrow enough that `bottom_lines(007)` is a rejection
    /// rather than a surprise.
    pub fn parse(spec: &str) -> Result<Self, UnknownRegion> {
        let trimmed = spec.trim();
        let known = match trimmed {
            "whole_recent" => Some(Self::WholeRecent),
            "after_last_prompt_marker" => Some(Self::AfterLastPromptMarker),
            "before_current_prompt_marker" => Some(Self::BeforeCurrentPromptMarker),
            "whole_recent_without_current_prompt_marker" => {
                Some(Self::WholeRecentWithoutCurrentPromptMarker)
            }
            "current_prompt_block_marker" => Some(Self::CurrentPromptBlockMarker),
            "after_current_prompt_block_marker" => Some(Self::AfterCurrentPromptBlockMarker),
            "prompt_box_body" => Some(Self::PromptBoxBody),
            "above_prompt_box" => Some(Self::AbovePromptBox),
            "last_non_empty_above_prompt_box" => Some(Self::LastNonEmptyAbovePromptBox),
            "after_last_horizontal_rule" => Some(Self::AfterLastHorizontalRule),
            "osc_title" => Some(Self::OscTitle),
            "osc_progress" => Some(Self::OscProgress),
            _ => None,
        };
        if let Some(region) = known {
            return Ok(region);
        }
        for (name, build) in [
            ("bottom_lines", Self::BottomLines as fn(u16) -> Self),
            ("bottom_non_empty_lines", Self::BottomNonEmptyLines),
            ("top_non_empty_lines", Self::TopNonEmptyLines),
        ] {
            if let Some(argument) = argument_of(trimmed, name) {
                let count = line_count(argument).ok_or_else(|| UnknownRegion {
                    spec: trimmed.to_owned(),
                })?;
                return Ok(build(count));
            }
        }
        Err(UnknownRegion {
            spec: trimmed.to_owned(),
        })
    }

    /// The lowest engine version that understands this region.
    ///
    /// A manifest declaring a floor below this one would be read by an engine
    /// that resolves the region to nothing, silently turning a rule that should
    /// match into one that never does — which is why declaring such a floor is
    /// a load-time rejection rather than a runtime surprise.
    pub fn min_engine_version(&self) -> u32 {
        match self {
            Self::TopNonEmptyLines(_) => TOP_NON_EMPTY_LINES_SINCE,
            _ => 1,
        }
    }
}

/// The text between `name(` and the closing `)`, if the spec has that shape.
fn argument_of<'a>(spec: &'a str, name: &str) -> Option<&'a str> {
    spec.strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')
}

/// The line count a parameterized region asked for, if it is well formed.
///
/// The `u16` is the cap: a screen is a screen, and a rule asking for more than
/// 65535 lines is an authoring slip rather than an intention.
fn line_count(argument: &str) -> Option<u16> {
    if argument.is_empty() || argument.starts_with('0') {
        return None;
    }
    if !argument.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    argument.parse::<u16>().ok()
}

/// A rule named a region this engine does not have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown region {spec:?}")]
pub struct UnknownRegion {
    /// The spelling that was rejected, trimmed.
    pub spec: String,
}

impl fmt::Display for RegionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WholeRecent => f.write_str("whole_recent"),
            Self::BottomLines(count) => write!(f, "bottom_lines({count})"),
            Self::BottomNonEmptyLines(count) => write!(f, "bottom_non_empty_lines({count})"),
            Self::TopNonEmptyLines(count) => write!(f, "top_non_empty_lines({count})"),
            Self::AfterLastPromptMarker => f.write_str("after_last_prompt_marker"),
            Self::BeforeCurrentPromptMarker => f.write_str("before_current_prompt_marker"),
            Self::WholeRecentWithoutCurrentPromptMarker => {
                f.write_str("whole_recent_without_current_prompt_marker")
            }
            Self::CurrentPromptBlockMarker => f.write_str("current_prompt_block_marker"),
            Self::AfterCurrentPromptBlockMarker => f.write_str("after_current_prompt_block_marker"),
            Self::PromptBoxBody => f.write_str("prompt_box_body"),
            Self::AbovePromptBox => f.write_str("above_prompt_box"),
            Self::LastNonEmptyAbovePromptBox => f.write_str("last_non_empty_above_prompt_box"),
            Self::AfterLastHorizontalRule => f.write_str("after_last_horizontal_rule"),
            Self::OscTitle => f.write_str("osc_title"),
            Self::OscProgress => f.write_str("osc_progress"),
        }
    }
}

/// The evidence a rule is matched against.
///
/// Three channels, kept apart because they are separately trustworthy: the
/// screen is what the agent drew, while the two OSC fields are what it told the
/// terminal out of band. A rule that names a side-channel never sees the
/// screen, and a rule over the screen never sees the side-channels, so text
/// cannot leak from one into the other.
#[derive(Debug, Clone, Copy)]
pub struct ScreenInput<'a> {
    /// The screen as plain text, escape sequences already removed by whoever
    /// captured it: the rows the agent is drawing on now, not a scrolled-back
    /// view of what it drew earlier.
    pub screen: &'a str,
    /// The most recent title the agent asked the terminal to show. Empty when
    /// none was captured, which is also what a caller with no way to capture
    /// one passes.
    pub osc_title: &'a str,
    /// The most recent progress report the agent sent the terminal. Empty as
    /// above.
    pub osc_progress: &'a str,
}

impl<'a> ScreenInput<'a> {
    /// Evidence that is screen text and nothing else.
    pub fn from_screen(screen: &'a str) -> Self {
        Self {
            screen,
            osc_title: "",
            osc_progress: "",
        }
    }
}

impl RegionSpec {
    /// The slice of `input` this region names.
    ///
    /// The result always borrows from `input`: extraction narrows, it never
    /// rewrites, so a rule sees the author's own bytes and a diagnostic can
    /// quote them without wondering what was normalized on the way.
    ///
    /// A region defined by a landmark the screen does not have resolves to
    /// nothing when the landmark *is* the region — no input box means no box
    /// body — and to the whole screen when the landmark merely bounds it: the
    /// text after the last rule line, when there is no rule line, is all the
    /// text there is.
    pub fn extract<'a>(self, input: ScreenInput<'a>) -> &'a str {
        let screen = input.screen;
        match self {
            Self::OscTitle => input.osc_title,
            Self::OscProgress => input.osc_progress,
            Self::WholeRecent => screen,
            Self::BottomLines(count) => bottom_lines(screen, count),
            Self::BottomNonEmptyLines(count) => bottom_non_empty_lines(screen, count),
            Self::TopNonEmptyLines(count) => top_non_empty_lines(screen, count),
            Self::AfterLastPromptMarker => after_last_prompt_marker(screen),
            Self::BeforeCurrentPromptMarker => before_current_prompt_marker(screen),
            Self::WholeRecentWithoutCurrentPromptMarker => {
                whole_recent_without_current_prompt_marker(screen)
            }
            Self::CurrentPromptBlockMarker => current_prompt_block_marker(screen),
            Self::AfterCurrentPromptBlockMarker => after_current_prompt_block_marker(screen),
            Self::PromptBoxBody => prompt_box_body(screen),
            Self::AbovePromptBox => above_prompt_box(screen),
            Self::LastNonEmptyAbovePromptBox => last_non_blank_line(above_prompt_box(screen)),
            Self::AfterLastHorizontalRule => after_last_horizontal_rule(screen),
        }
    }
}

/// The slice of `input` the region named `declared` covers.
///
/// The name has already been through [`RegionSpec::parse`] by the time there is
/// any evidence to extract from — a manifest naming a region this engine does
/// not have is refused when it is loaded, long before it is run against
/// anything — so a name that does not parse here is a broken invariant rather
/// than bad input, and is treated as one.
pub fn extract<'a>(declared: &str, input: ScreenInput<'a>) -> &'a str {
    match RegionSpec::parse(declared) {
        Ok(region) => region.extract(input),
        Err(error) => unreachable!("{error}: regions are checked when a manifest is loaded"),
    }
}

/// The last `count` lines, blank ones included, or the whole text when it is
/// shorter than that.
fn bottom_lines(text: &str, count: u16) -> &str {
    let lines = split_lines(text);
    let start = lines.len().saturating_sub(usize::from(count));
    from_line(text, &lines, start)
}

/// From the `count`th non-blank line up from the bottom to the end, keeping
/// any blank lines that fall inside that span.
fn bottom_non_empty_lines(text: &str, count: u16) -> &str {
    let lines = split_lines(text);
    match nth_non_blank(&lines, count, Direction::FromBottom) {
        Some(index) => from_line(text, &lines, index),
        None => "",
    }
}

/// From the top of the text through the `count`th non-blank line.
fn top_non_empty_lines(text: &str, count: u16) -> &str {
    let lines = split_lines(text);
    match nth_non_blank(&lines, count, Direction::FromTop) {
        Some(index) => upto_line(text, &lines, index + 1),
        None => "",
    }
}

/// Everything below the last prompt marker anywhere in the text — including
/// one the agent has since answered, which is what makes this the region for
/// asking whether the screen is showing history rather than a live prompt.
fn after_last_prompt_marker(text: &str) -> &str {
    let lines = split_lines(text);
    match lines.iter().rposition(|line| is_prompt_marker(line.text)) {
        Some(index) => from_line(text, &lines, index + 1),
        None => text,
    }
}

/// Everything above the prompt the agent is waiting at now.
fn before_current_prompt_marker(text: &str) -> &str {
    let lines = split_lines(text);
    match current_prompt_marker(&lines) {
        Some(index) => upto_line(text, &lines, index),
        None => text,
    }
}

/// The whole text, but only while the agent is not waiting at a prompt — a way
/// to write a rule that a live prompt should silence.
fn whole_recent_without_current_prompt_marker(text: &str) -> &str {
    let lines = split_lines(text);
    if current_prompt_marker(&lines).is_some() {
        ""
    } else {
        text
    }
}

/// The block marker line that opens the last block above the current prompt:
/// the one line that says how the work the agent just finished ended.
fn current_prompt_block_marker(text: &str) -> &str {
    let lines = split_lines(text);
    match current_prompt_block_marker_index(&lines) {
        Some(index) => lines[index].text,
        None => "",
    }
}

/// That block marker line and everything the agent drew after it.
fn after_current_prompt_block_marker(text: &str) -> &str {
    let lines = split_lines(text);
    match current_prompt_block_marker_index(&lines) {
        Some(index) => from_line(text, &lines, index),
        None => "",
    }
}

/// What is written inside the agent's input box: the lines between its top
/// border and the next border below.
fn prompt_box_body(text: &str) -> &str {
    let lines = split_lines(text);
    let Some((top, bottom)) = prompt_box_borders(&lines) else {
        return "";
    };
    &text[line_start(text, &lines, top + 1)..line_start(text, &lines, bottom)]
}

/// Everything above the input box, or the whole text when there is no box to
/// be above.
fn above_prompt_box(text: &str) -> &str {
    let lines = split_lines(text);
    match prompt_box_borders(&lines) {
        Some((top, _)) => upto_line(text, &lines, top),
        None => text,
    }
}

/// Everything below the last border line — where an agent draws the question
/// it is currently asking.
fn after_last_horizontal_rule(text: &str) -> &str {
    let lines = split_lines(text);
    match lines.iter().rposition(|line| is_horizontal_rule(line.text)) {
        Some(index) => from_line(text, &lines, index + 1),
        None => text,
    }
}

/// The last line with anything on it, on its own.
fn last_non_blank_line(text: &str) -> &str {
    split_lines(text)
        .iter()
        .rev()
        .find(|line| !is_blank(line.text))
        .map_or("", |line| line.text)
}

/// One line of the text, and where it starts in it.
#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    /// Byte offset of the line's first character within the text.
    start: usize,
    /// The line itself, without its terminator.
    text: &'a str,
}

/// Splits on `\n` alone: the terminator belongs to the line it ends, so a text
/// that ends with one has no empty line after it.
fn split_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split('\n') {
        lines.push(Line { start, text: line });
        start += line.len() + 1;
    }
    if text.is_empty() || text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Where line `index` starts, or the end of the text for an index past the
/// last line — so that "from the line after this one" reads as the empty
/// remainder rather than a panic.
fn line_start(text: &str, lines: &[Line<'_>], index: usize) -> usize {
    lines.get(index).map_or(text.len(), |line| line.start)
}

/// The text from line `index` onwards.
fn from_line<'a>(text: &'a str, lines: &[Line<'a>], index: usize) -> &'a str {
    &text[line_start(text, lines, index)..]
}

/// The text up to, but not including, line `index`.
fn upto_line<'a>(text: &'a str, lines: &[Line<'a>], index: usize) -> &'a str {
    &text[..line_start(text, lines, index)]
}

/// Which end of the text a count of non-blank lines starts from.
#[derive(Debug, Clone, Copy)]
enum Direction {
    FromTop,
    FromBottom,
}

/// The index of the `count`th non-blank line from the given end, or of the
/// furthest one there is when the text holds fewer than `count` of them — a
/// rule asking for more lines than the screen has wants the screen, not
/// nothing. `None` only when there is no non-blank line at all.
fn nth_non_blank(lines: &[Line<'_>], count: u16, direction: Direction) -> Option<usize> {
    let mut seen = 0;
    let mut found = None;
    for step in 0..lines.len() {
        let index = match direction {
            Direction::FromTop => step,
            Direction::FromBottom => lines.len() - 1 - step,
        };
        if is_blank(lines[index].text) {
            continue;
        }
        seen += 1;
        found = Some(index);
        if seen == count {
            break;
        }
    }
    found
}

/// Nothing but ASCII whitespace.
fn is_blank(line: &str) -> bool {
    line.trim_ascii().is_empty()
}

/// A line where the agent is inviting input: the marker alone, or the marker
/// and whatever has been typed after it.
fn is_prompt_marker(line: &str) -> bool {
    line == "\u{203a}" || line.starts_with("\u{203a} ")
}

/// A line opening one of the blocks an agent prints as it works — a step, a
/// result, a failure. Only at the very start of the line: these are drawn in
/// the leftmost column, and an indented one is quoted text rather than chrome.
fn is_block_marker(line: &str) -> bool {
    line.starts_with(['\u{2022}', '\u{25a0}', '\u{2717}', '\u{2713}'])
}

/// The prompt marker the agent is waiting at *now*, as opposed to one left
/// behind in the history above.
///
/// A marker is the current one unless a block came after it: once the agent
/// has started printing blocks below a prompt, that prompt has been answered
/// and the screen has moved on. Anything else below it — a hint row, the
/// bottom of a border — is chrome drawn around a live prompt rather than a
/// reply to it.
fn current_prompt_marker(lines: &[Line<'_>]) -> Option<usize> {
    let index = lines.iter().rposition(|line| is_prompt_marker(line.text))?;
    if lines[index + 1..]
        .iter()
        .any(|line| is_block_marker(line.text))
    {
        return None;
    }
    Some(index)
}

/// The last block marker above the current prompt marker.
fn current_prompt_block_marker_index(lines: &[Line<'_>]) -> Option<usize> {
    let prompt = current_prompt_marker(lines)?;
    lines[..prompt]
        .iter()
        .rposition(|line| is_block_marker(line.text))
}

/// A line drawn as a horizontal border.
///
/// Either the line is a run of the box-drawing horizontal and nothing else, or
/// the run is long enough that what follows it reads as a label set into a
/// border rather than as prose that happens to open with a dash.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim_ascii();
    let run = trimmed
        .chars()
        .take_while(|&glyph| glyph == '\u{2500}')
        .count();
    if run == 0 {
        return false;
    }
    let after_run = trimmed
        .char_indices()
        .nth(run)
        .map_or(trimmed.len(), |(offset, _)| offset);
    trimmed[after_run..].trim_ascii().is_empty() || run >= 3
}

/// The two edges of the agent's input box, top first: the last two border
/// lines on the screen. Anything a TUI draws above them is chrome from earlier
/// in the session, and the box is the pair closest to where the agent is
/// currently typing.
fn prompt_box_borders(lines: &[Line<'_>]) -> Option<(usize, usize)> {
    let mut borders = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| is_horizontal_rule(line.text))
        .map(|(index, _)| index);
    let bottom = borders.next()?;
    let top = borders.next()?;
    Some((top, bottom))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_plain_region_name() {
        for name in [
            "whole_recent",
            "after_last_prompt_marker",
            "before_current_prompt_marker",
            "whole_recent_without_current_prompt_marker",
            "current_prompt_block_marker",
            "after_current_prompt_block_marker",
            "prompt_box_body",
            "above_prompt_box",
            "last_non_empty_above_prompt_box",
            "after_last_horizontal_rule",
            "osc_title",
            "osc_progress",
        ] {
            let region = RegionSpec::parse(name).expect("known region");
            assert_eq!(region.to_string(), name, "{name} should round-trip");
        }
    }

    #[test]
    fn parses_the_parameterized_forms() {
        assert_eq!(
            RegionSpec::parse("bottom_lines(1)"),
            Ok(RegionSpec::BottomLines(1))
        );
        assert_eq!(
            RegionSpec::parse("bottom_non_empty_lines(8)"),
            Ok(RegionSpec::BottomNonEmptyLines(8))
        );
        assert_eq!(
            RegionSpec::parse("top_non_empty_lines(65535)"),
            Ok(RegionSpec::TopNonEmptyLines(65535))
        );
        assert_eq!(
            RegionSpec::parse("  bottom_lines(20)  "),
            Ok(RegionSpec::BottomLines(20))
        );
    }

    #[test]
    fn rejects_malformed_parameters() {
        for bad in [
            "bottom_lines(0)",
            "bottom_lines(01)",
            "bottom_lines(65536)",
            "bottom_lines()",
            "bottom_lines(x)",
            "bottom_lines(-1)",
            "bottom_lines(1",
            "bottom_lines 1)",
            "bottom_lines(1))",
            "bottom_lines(1 )",
            "bottom_non_empty_lines(+1)",
            "top_non_empty_lines(1.0)",
        ] {
            assert!(
                RegionSpec::parse(bad).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn rejects_unknown_names() {
        assert_eq!(
            RegionSpec::parse("whole_screen"),
            Err(UnknownRegion {
                spec: "whole_screen".to_owned()
            })
        );
        assert!(RegionSpec::parse("").is_err());
        assert!(RegionSpec::parse("WHOLE_RECENT").is_err());
    }

    #[test]
    fn reports_the_engine_a_region_needs() {
        assert_eq!(RegionSpec::WholeRecent.min_engine_version(), 1);
        assert_eq!(RegionSpec::BottomNonEmptyLines(3).min_engine_version(), 1);
        assert_eq!(RegionSpec::TopNonEmptyLines(3).min_engine_version(), 3);
    }

    #[test]
    fn the_default_region_is_parseable() {
        assert_eq!(
            RegionSpec::parse(DEFAULT_REGION),
            Ok(RegionSpec::WholeRecent)
        );
    }

    /// Every region a manifest may name, the parameterized ones instantiated.
    const EVERY_REGION: &[&str] = &[
        "whole_recent",
        "bottom_lines(3)",
        "bottom_non_empty_lines(3)",
        "top_non_empty_lines(3)",
        "after_last_prompt_marker",
        "before_current_prompt_marker",
        "whole_recent_without_current_prompt_marker",
        "current_prompt_block_marker",
        "after_current_prompt_block_marker",
        "prompt_box_body",
        "above_prompt_box",
        "last_non_empty_above_prompt_box",
        "after_last_horizontal_rule",
        "osc_title",
        "osc_progress",
    ];

    /// The region named by `spec`, over a screen and nothing else.
    fn region<'a>(spec: &str, screen: &'a str) -> &'a str {
        extract(spec, ScreenInput::from_screen(screen))
    }

    /// Whether `part` points into `whole` rather than at a copy of it.
    fn borrows_from(part: &str, whole: &str) -> bool {
        let base = whole.as_ptr().addr();
        let start = part.as_ptr().addr();
        start >= base && start + part.len() <= base + whole.len()
    }

    /// A screen with an input box: two borders, a prompt above them and a hint
    /// row below.
    const BOXED_SCREEN: &str = "\
above one
above two
─────────────── Chat
│ typing here │
───────────────
hint row
";

    /// A screen the way an agent that draws blocks and a prompt marker leaves
    /// it once it has finished and is waiting.
    const MARKED_SCREEN: &str = "\
› earlier question
• running a command
detail line
✓ done
› 
";

    #[test]
    fn the_side_channels_are_never_the_screen() {
        let input = ScreenInput {
            screen: "screen text",
            osc_title: "agent — waiting",
            osc_progress: "42",
        };
        assert_eq!(extract("osc_title", input), "agent — waiting");
        assert_eq!(extract("osc_progress", input), "42");
        assert_eq!(extract("whole_recent", input), "screen text");
        assert_eq!(extract("bottom_lines(1)", input), "screen text");

        let uncaptured = ScreenInput::from_screen("screen text");
        assert_eq!(extract("osc_title", uncaptured), "");
        assert_eq!(extract("osc_progress", uncaptured), "");
    }

    #[test]
    fn whole_recent_is_the_whole_screen() {
        assert_eq!(region("whole_recent", BOXED_SCREEN), BOXED_SCREEN);
        assert_eq!(region("whole_recent", ""), "");
    }

    #[test]
    fn bottom_lines_counts_blank_lines_too() {
        let screen = "one\ntwo\n\nfour\n";
        assert_eq!(region("bottom_lines(1)", screen), "four\n");
        assert_eq!(region("bottom_lines(2)", screen), "\nfour\n");
        assert_eq!(region("bottom_lines(4)", screen), screen);
        assert_eq!(region("bottom_lines(5)", screen), screen);
        assert_eq!(region("bottom_lines(65535)", screen), screen);
        assert_eq!(region("bottom_lines(1)", "a\nb"), "b");
        assert_eq!(region("bottom_lines(1)", ""), "");
    }

    #[test]
    fn bottom_non_empty_lines_keeps_the_blank_lines_inside_its_span() {
        let screen = "alpha\n\nbeta\n\n\ngamma\n";
        assert_eq!(region("bottom_non_empty_lines(1)", screen), "gamma\n");
        assert_eq!(
            region("bottom_non_empty_lines(2)", screen),
            "beta\n\n\ngamma\n"
        );
        assert_eq!(region("bottom_non_empty_lines(3)", screen), screen);
        assert_eq!(region("bottom_non_empty_lines(4)", screen), screen);
        assert_eq!(
            region("bottom_non_empty_lines(1)", "gamma\n\n \n"),
            "gamma\n\n \n"
        );
        assert_eq!(region("bottom_non_empty_lines(1)", "\n \n\t\n"), "");
        assert_eq!(region("bottom_non_empty_lines(1)", ""), "");
    }

    #[test]
    fn top_non_empty_lines_stops_after_the_one_it_was_asked_for() {
        let screen = "\nalpha\n\nbeta\ngamma\n";
        assert_eq!(region("top_non_empty_lines(1)", screen), "\nalpha\n");
        assert_eq!(
            region("top_non_empty_lines(2)", screen),
            "\nalpha\n\nbeta\n"
        );
        assert_eq!(region("top_non_empty_lines(3)", screen), screen);
        assert_eq!(region("top_non_empty_lines(4)", screen), screen);
        assert_eq!(region("top_non_empty_lines(1)", "only"), "only");
        assert_eq!(region("top_non_empty_lines(1)", "\n  \n"), "");
    }

    #[test]
    fn after_last_prompt_marker_takes_the_last_marker_answered_or_not() {
        assert_eq!(region("after_last_prompt_marker", MARKED_SCREEN), "");
        assert_eq!(
            region("after_last_prompt_marker", "› asked\n• working\n"),
            "• working\n"
        );
        assert_eq!(
            region("after_last_prompt_marker", "›\nfirst line was the marker\n"),
            "first line was the marker\n"
        );
    }

    #[test]
    fn a_screen_with_no_prompt_marker_is_all_after_the_last_one() {
        let screen = "nothing here invites input\n";
        assert_eq!(region("after_last_prompt_marker", screen), screen);
        assert_eq!(region("before_current_prompt_marker", screen), screen);
        assert_eq!(
            region("whole_recent_without_current_prompt_marker", screen),
            screen
        );
        assert_eq!(region("current_prompt_block_marker", screen), "");
        assert_eq!(region("after_current_prompt_block_marker", screen), "");
    }

    #[test]
    fn a_marker_needs_the_glyph_alone_or_a_space_after_it() {
        let glued = "›typed\ntail\n";
        let indented = "  › indented\ntail\n";
        assert_eq!(region("after_last_prompt_marker", glued), glued);
        assert_eq!(region("after_last_prompt_marker", indented), indented);
        assert_eq!(region("after_last_prompt_marker", "› \ntail\n"), "tail\n");
        assert_eq!(region("after_last_prompt_marker", "›\ntail\n"), "tail\n");
    }

    #[test]
    fn the_current_marker_is_the_last_one_no_block_came_after() {
        assert_eq!(
            region("before_current_prompt_marker", MARKED_SCREEN),
            "› earlier question\n• running a command\ndetail line\n✓ done\n"
        );
        assert_eq!(
            region("whole_recent_without_current_prompt_marker", MARKED_SCREEN),
            ""
        );

        let working = "› asked\n• still running\n";
        assert_eq!(region("before_current_prompt_marker", working), working);
        assert_eq!(
            region("whole_recent_without_current_prompt_marker", working),
            working
        );
    }

    #[test]
    fn chrome_below_a_marker_leaves_it_current() {
        let screen = "• done\n› \n  send with enter\n";
        assert_eq!(region("before_current_prompt_marker", screen), "• done\n");
        assert_eq!(
            region("whole_recent_without_current_prompt_marker", screen),
            ""
        );
    }

    #[test]
    fn the_block_marker_regions_start_at_the_last_block_above_the_prompt() {
        assert_eq!(
            region("current_prompt_block_marker", MARKED_SCREEN),
            "✓ done"
        );
        assert_eq!(
            region("after_current_prompt_block_marker", MARKED_SCREEN),
            "✓ done\n› \n"
        );

        let unblocked = "just prose\n› \n";
        assert_eq!(region("current_prompt_block_marker", unblocked), "");
        assert_eq!(region("after_current_prompt_block_marker", unblocked), "");
    }

    #[test]
    fn a_block_marker_has_to_open_its_line() {
        let screen = "  • quoted, not chrome\n› \n";
        assert_eq!(region("current_prompt_block_marker", screen), "");
        assert_eq!(region("after_current_prompt_block_marker", screen), "");
    }

    #[test]
    fn every_block_glyph_counts() {
        for glyph in ['•', '■', '✗', '✓'] {
            let screen = format!("{glyph} block\n› \n");
            assert_eq!(
                region("current_prompt_block_marker", &screen),
                format!("{glyph} block"),
                "{glyph} should open a block"
            );
        }
    }

    #[test]
    fn the_prompt_box_is_the_last_two_borders() {
        assert_eq!(region("prompt_box_body", BOXED_SCREEN), "│ typing here │\n");
        assert_eq!(
            region("above_prompt_box", BOXED_SCREEN),
            "above one\nabove two\n"
        );
        assert_eq!(
            region("last_non_empty_above_prompt_box", BOXED_SCREEN),
            "above two"
        );
        assert_eq!(
            region("after_last_horizontal_rule", BOXED_SCREEN),
            "hint row\n"
        );
    }

    #[test]
    fn a_border_may_carry_a_title_but_a_short_run_may_not() {
        let titled = "above\n─── Chat\nbody\n───\ntail\n";
        assert_eq!(region("prompt_box_body", titled), "body\n");
        assert_eq!(region("above_prompt_box", titled), "above\n");

        let too_short = "above\n── Chat\nbody\n───\ntail\n";
        assert_eq!(region("prompt_box_body", too_short), "");
        assert_eq!(region("above_prompt_box", too_short), too_short);
        assert_eq!(region("after_last_horizontal_rule", too_short), "tail\n");
    }

    #[test]
    fn a_bare_run_of_any_length_is_a_border() {
        let screen = "above\n─\nbody\n──\ntail\n";
        assert_eq!(region("prompt_box_body", screen), "body\n");
        assert_eq!(region("above_prompt_box", screen), "above\n");
        assert_eq!(region("after_last_horizontal_rule", screen), "tail\n");
    }

    #[test]
    fn a_screen_with_no_border_has_no_box_but_is_still_all_above_one() {
        let screen = "plain output\nlast line\n";
        assert_eq!(region("prompt_box_body", screen), "");
        assert_eq!(region("above_prompt_box", screen), screen);
        assert_eq!(
            region("last_non_empty_above_prompt_box", screen),
            "last line"
        );
        assert_eq!(region("after_last_horizontal_rule", screen), screen);

        let single = "above\n───────\nbelow\n";
        assert_eq!(region("prompt_box_body", single), "");
        assert_eq!(region("above_prompt_box", single), single);
        assert_eq!(region("after_last_horizontal_rule", single), "below\n");
    }

    #[test]
    fn borders_at_the_first_and_last_line() {
        let screen = "───────\nbody\n───────\n";
        assert_eq!(region("prompt_box_body", screen), "body\n");
        assert_eq!(region("above_prompt_box", screen), "");
        assert_eq!(region("last_non_empty_above_prompt_box", screen), "");
        assert_eq!(region("after_last_horizontal_rule", screen), "");

        let multi_line_body = "───────\nfirst\n\nsecond\n───────\n";
        assert_eq!(
            region("prompt_box_body", multi_line_body),
            "first\n\nsecond\n"
        );
    }

    #[test]
    fn other_box_drawing_is_not_a_border_or_a_marker() {
        let screen = "╭──────────╮\n│ • not a block │\n│ › not a prompt │\n╰──────────╯\n";
        assert_eq!(region("after_last_horizontal_rule", screen), screen);
        assert_eq!(region("prompt_box_body", screen), "");
        assert_eq!(region("above_prompt_box", screen), screen);
        assert_eq!(region("after_last_prompt_marker", screen), screen);
        assert_eq!(region("current_prompt_block_marker", screen), "");

        let heavy = "above\n━━━━━━━\nbelow\n";
        assert_eq!(region("after_last_horizontal_rule", heavy), heavy);
    }

    #[test]
    fn an_empty_screen_is_empty_in_every_region() {
        for spec in EVERY_REGION {
            assert_eq!(region(spec, ""), "", "{spec} should be empty");
        }
    }

    #[test]
    fn extraction_borrows_from_the_input() {
        let screen = String::from("above\n───────\ntyped\n───────\ntail\n");
        for spec in EVERY_REGION {
            let slice = region(spec, &screen);
            if slice.is_empty() {
                continue;
            }
            assert!(
                borrows_from(slice, &screen),
                "{spec} should have borrowed from the screen"
            );
        }

        let title = String::from("agent — waiting");
        let slice = extract(
            "osc_title",
            ScreenInput {
                screen: &screen,
                osc_title: &title,
                osc_progress: "",
            },
        );
        assert!(borrows_from(slice, &title));
    }

    #[test]
    #[should_panic(expected = "unknown region")]
    fn extracting_an_unvalidated_name_is_a_broken_invariant() {
        extract("whole_screen", ScreenInput::from_screen("anything"));
    }

    #[test]
    fn the_box_is_the_lowest_pair_of_borders() {
        let screen = "───\nheader\n───\ntyping\n───\ntail\n";
        assert_eq!(region("prompt_box_body", screen), "typing\n");
        assert_eq!(region("above_prompt_box", screen), "───\nheader\n");
        assert_eq!(region("last_non_empty_above_prompt_box", screen), "header");
        assert_eq!(region("after_last_horizontal_rule", screen), "tail\n");
    }
}
