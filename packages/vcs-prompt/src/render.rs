//! Turn a [`crate::jj`] or [`crate::git`] head into the one line starship
//! prints.
//!
//! The binary colors its own output instead of leaning on the custom module's
//! `style`, because one segment carries several colors (branch, counts,
//! in-progress state) the way starship's own `git_*` modules do.

use std::fmt::Write as _;

use anstyle::{AnsiColor, Reset, Style};

use crate::git::{self, HeadName};
use crate::jj;
use crate::views;

/// nf-dev-git_branch, the symbol this prompt used for `git_branch`.
const GIT_SYMBOL: &str = "\u{e0a0} ";
/// nf-md-source_commit_start, the closest thing to a jj mark in Nerd Fonts.
const JJ_SYMBOL: &str = "\u{f15c6} ";

const NAME: Style = AnsiColor::Magenta.on_default().bold();
const COUNTS: Style = AnsiColor::Red.on_default().bold();
const MUTED: Style = Style::new().dimmed();

/// A segment under construction: text plus whether escapes are wanted.
struct Segment {
    text: String,
    color: bool,
}

impl Segment {
    const fn new(color: bool) -> Self {
        Self {
            text: String::new(),
            color,
        }
    }

    fn push(&mut self, style: Style, text: &str) {
        if self.color {
            let _ = write!(self.text, "{}{text}{}", style.render(), Reset.render());
        } else {
            self.text.push_str(text);
        }
    }

    fn push_plain(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn into_text(self) -> String {
        self.text
    }
}

/// `on 󱗆 ykosps main⇕⇡10⇣97 * ix~`: the working-copy change id, the local
/// bookmark naming it, where it stands against trunk, the state flags, and
/// the view it sits in with one flag for the subtree's state.
///
/// Every number names a comparison a reader can restate: `⇡` is `trunk()..@`
/// less an empty working-copy commit (a placeholder holds nothing trunk
/// lacks), `⇣` is `@..trunk()`. The view flag is an id comparison `jj view`
/// made against the working-copy commit: `~` when the subtree differs from
/// the imported tree, `=` when it is conflicted, `?` when no import is
/// recorded, `!` when the path is missing or not a directory, and nothing
/// when the subtree is the imported tree. A lookup that failed renders
/// `view:ERR`: silence would read as "outside every view". There is no
/// separate dirty count: in jj the edits are already in @, so a non-empty
/// working copy is the dirty signal.
pub fn jj(head: &jj::Head, view: &views::Segment, color: bool) -> String {
    let mut segment = Segment::new(color);
    segment.push_plain("on ");
    segment.push(NAME, JJ_SYMBOL);
    segment.push(NAME, &head.change_prefix);
    // The dimmed remainder of the 8-char change id carried no information the
    // prefix does not; commented out to keep the segment short.
    // segment.push(MUTED, &head.change_rest);

    // The bookmark names the change; the trunk comparison places it. When the
    // nearest bookmark *is* trunk the name is printed once, with the arrows.
    let trunk_name = head.trunk.as_ref().map(|trunk| trunk.name.as_str());
    if let Some(bookmark) = &head.bookmark
        && Some(bookmark.as_str()) != trunk_name
    {
        segment.push_plain(" ");
        segment.push(NAME, bookmark);
    }
    if let Some(trunk) = &head.trunk {
        segment.push_plain(" ");
        segment.push(NAME, &trunk.name);
        let arrows = ahead_behind(trunk.ahead, trunk.behind);
        if !arrows.is_empty() {
            segment.push(COUNTS, &arrows);
        }
    }

    let mut flags = String::new();
    if head.flags.conflict {
        flags.push('=');
    }
    if head.flags.divergent {
        flags.push_str("??");
    }
    if !head.flags.empty {
        flags.push('*');
    }
    if !flags.is_empty() {
        segment.push_plain(" ");
        segment.push(COUNTS, &flags);
    }

    // The view the directory is inside: context the way the submodule
    // breadcrumb is, so the name is muted, with the subtree's state flag
    // beside it when there is one.
    match view {
        views::Segment::Outside => {}
        views::Segment::Inside(view) => {
            segment.push_plain(" ");
            segment.push(MUTED, &view.name);
            if let Some(flag) = view_flag(view.state) {
                segment.push(COUNTS, flag);
            }
        }
        views::Segment::Failed => {
            segment.push_plain(" ");
            segment.push(COUNTS, "view:ERR");
        }
    }

    segment.into_text()
}

/// `on  main !2?1⇡2`, matching the symbols the disabled `git_branch` and
/// `git_status` modules were configured with.
pub fn git(head: &git::Head, color: bool) -> String {
    let mut segment = Segment::new(color);
    segment.push_plain("on ");
    segment.push(NAME, GIT_SYMBOL);
    match &head.name {
        HeadName::Branch(branch) => segment.push(NAME, branch),
        HeadName::Detached(commit) => segment.push(NAME, &format!("({commit})")),
    }

    let counts = counts(&head.counts, head.tracking);
    if !counts.is_empty() {
        segment.push_plain(" ");
        segment.push(COUNTS, &counts);
    }

    segment.into_text()
}

/// Status counts in starship's `$all_status$ahead_behind` order and symbols,
/// so the segment reads the same as before the modules moved in here.
fn counts(counts: &git::Counts, tracking: Option<git::Tracking>) -> String {
    let mut rendered = String::new();
    for (symbol, count) in [
        ("=", counts.conflicted),
        ("✘", counts.deleted),
        ("»", counts.renamed),
        ("!", counts.modified),
        ("+", counts.staged),
        ("?", counts.untracked),
    ] {
        if count > 0 {
            let _ = write!(rendered, "{symbol}{count}");
        }
    }

    if let Some(git::Tracking { ahead, behind }) = tracking {
        rendered.push_str(&ahead_behind(ahead, behind));
    }

    rendered
}

/// The ahead/behind arrows, shared with the git tracking counts so the two
/// read identically.
/// One character per `jj view` state; `Pristine` carries no flag,
/// EXPLICITLY: the match is exhaustive over the enum the protocol boundary
/// parsed (views.rs), so an unknown wire word can never arrive here and
/// render flagless -- it already failed the parse and rendered `view:ERR`.
const fn view_flag(state: views::ViewState) -> Option<&'static str> {
    use crate::views::ViewState;
    match state {
        ViewState::Pristine => None,
        ViewState::Patched => Some("~"),
        ViewState::Conflicted => Some("="),
        ViewState::Unanchored => Some("?"),
        ViewState::Missing | ViewState::NotADirectory => Some("!"),
    }
}

fn ahead_behind(ahead: usize, behind: usize) -> String {
    match (ahead, behind) {
        (0, 0) => String::new(),
        (ahead, 0) => format!("⇡{ahead}"),
        (0, behind) => format!("⇣{behind}"),
        (ahead, behind) => format!("⇕⇡{ahead}⇣{behind}"),
    }
}

#[cfg(test)]
mod tests {
    use crate::git::{Counts as GitCounts, Head as GitHead, HeadName, Tracking};
    use crate::jj::{Flags, Head as JjHead, Trunk};
    use crate::views::{Segment, View};

    fn head(bookmark: Option<&str>, trunk: Option<Trunk>, empty: bool) -> JjHead {
        JjHead {
            change_prefix: "ykosps".to_owned(),
            change_rest: "vq".to_owned(),
            flags: Flags {
                empty,
                conflict: false,
                divergent: false,
            },
            bookmark: bookmark.map(str::to_owned),
            trunk,
        }
    }

    fn trunk(name: &str, ahead: usize, behind: usize) -> Trunk {
        Trunk {
            name: name.to_owned(),
            ahead,
            behind,
        }
    }

    fn view(name: &str, state: &str) -> Segment {
        Segment::Inside(View {
            name: name.to_owned(),
            state: super::views::ViewState::parse(state).expect("test states are vocabulary words"),
        })
    }

    /// The regression this rewrite exists for. The old segment rendered
    /// `main@git+10` -- a distance to jj's `git` pseudo-remote -- and hid the
    /// 97 commits of trunk that @ did not have.
    #[test]
    fn a_diverged_working_copy_shows_both_sides_against_a_named_trunk() {
        let rendered = super::jj(
            &head(None, Some(trunk("main*", 10, 97)), false),
            &Segment::Outside,
            false,
        );

        assert_eq!(
            rendered,
            "on \u{f15c6} ykosps main*\u{21d5}\u{21e1}10\u{21e3}97 *"
        );
        assert!(
            !rendered.contains("@git"),
            "a pseudo-remote reached the prompt"
        );
    }

    #[test]
    fn a_working_copy_level_with_trunk_shows_the_name_alone() {
        assert_eq!(
            super::jj(
                &head(None, Some(trunk("main", 0, 0)), true),
                &Segment::Outside,
                false
            ),
            "on \u{f15c6} ykosps main"
        );
    }

    #[test]
    fn only_ahead_of_trunk_is_one_arrow() {
        assert_eq!(
            super::jj(
                &head(None, Some(trunk("ix-patched", 2, 0)), false),
                &Segment::Outside,
                false
            ),
            "on \u{f15c6} ykosps ix-patched\u{21e1}2 *"
        );
    }

    /// A bookmark that is not trunk is worth its own name; a bookmark that
    /// *is* trunk must not be printed twice.
    #[test]
    fn a_bookmark_off_trunk_is_named_beside_it() {
        assert_eq!(
            super::jj(
                &head(Some("feature"), Some(trunk("main", 3, 1)), true),
                &Segment::Outside,
                false
            ),
            "on \u{f15c6} ykosps feature main\u{21d5}\u{21e1}3\u{21e3}1"
        );
    }

    #[test]
    fn a_bookmark_that_is_trunk_is_printed_once() {
        assert_eq!(
            super::jj(
                &head(Some("main*"), Some(trunk("main*", 1, 0)), true),
                &Segment::Outside,
                false
            ),
            "on \u{f15c6} ykosps main*\u{21e1}1"
        );
    }

    /// With no trunk to compare against, the segment says less rather than
    /// inventing a comparison.
    #[test]
    fn no_trunk_leaves_the_change_id_and_flags() {
        assert_eq!(
            super::jj(&head(None, None, false), &Segment::Outside, false),
            "on \u{f15c6} ykosps *"
        );
    }

    #[test]
    fn a_patched_view_carries_its_flag() {
        assert_eq!(
            super::jj(&head(None, None, true), &view("ix", "patched"), false),
            "on \u{f15c6} ykosps ix~"
        );
    }

    #[test]
    fn a_pristine_view_is_just_its_name() {
        assert_eq!(
            super::jj(&head(None, None, true), &view("ix", "pristine"), false),
            "on \u{f15c6} ykosps ix"
        );
    }

    #[test]
    fn every_other_state_has_one_flag() {
        for (state, flag) in [
            ("conflicted", "="),
            ("unanchored", "?"),
            ("missing", "!"),
            ("not-a-directory", "!"),
        ] {
            assert_eq!(
                super::jj(&head(None, None, true), &view("ix", state), false),
                format!("on \u{f15c6} ykosps ix{flag}"),
                "state {state}"
            );
        }
    }

    #[test]
    fn conflict_and_divergence_still_reach_the_flags() {
        let mut h = head(None, None, false);
        h.flags.conflict = true;
        h.flags.divergent = true;
        assert_eq!(
            super::jj(&h, &Segment::Outside, false),
            "on \u{f15c6} ykosps =??*"
        );
    }

    #[test]
    fn a_failed_view_lookup_is_visible_not_silent() {
        assert_eq!(
            super::jj(&head(None, None, true), &Segment::Failed, false),
            "on \u{f15c6} ykosps view:ERR"
        );
    }

    #[test]
    fn git_counts_follow_starship_order_and_symbols() {
        let head = GitHead {
            name: HeadName::Branch("main".to_owned()),
            tracking: Some(Tracking {
                ahead: 2,
                behind: 1,
            }),
            counts: GitCounts {
                modified: 3,
                untracked: 1,
                ..GitCounts::default()
            },
        };

        assert_eq!(
            super::git(&head, false),
            "on \u{e0a0} main !3?1\u{21d5}\u{21e1}2\u{21e3}1"
        );
    }

    #[test]
    fn a_detached_head_shows_the_commit_in_parentheses() {
        let head = GitHead {
            name: HeadName::Detached("c1b4a88".to_owned()),
            tracking: None,
            counts: GitCounts::default(),
        };

        assert_eq!(super::git(&head, false), "on \u{e0a0} (c1b4a88)");
    }
}
