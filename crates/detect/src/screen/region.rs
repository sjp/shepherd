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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}
