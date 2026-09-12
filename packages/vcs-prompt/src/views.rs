//! Which declared jj view owns the prompt directory, and whether its
//! subtree is the imported tree.
//!
//! `jj view prompt` is the seam: it maps the directory to the repo's
//! `views.toml` and compares the subtree's tree id with the import the
//! manifest records. Both are ids read off the working-copy commit, so the
//! answer is exact and current, and it costs no network and no cache. The
//! old segment carried behind/ahead counts from a survey record whose
//! vintage was routinely hours stale (the `ix⇣241` reading here was 7h41m
//! old and already wrong low); a state has no vintage to get wrong.
//!
//! The protocol is one line of stdout: `name<TAB>state` inside a view, the
//! literal `-` outside every view. Anything else, including a non-zero exit
//! and an empty stdout, is a failure the prompt SHOWS (`view:ERR`). The
//! first version mapped every failure to "no view", so the segment went
//! quietly missing on every fleet host, where `jj` is the plain fork binary
//! without a `view` verb, and nothing distinguished that from a directory
//! outside every view.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr, eyre};

/// The binary that carries `view`. There is one jj: the fleet and the
/// workstation both install ix's native client (`packages/jj-ix`) under this
/// name, and it is the vendored fork's CLI plus the ix store factories and
/// the native verbs, so every stock verb and `view` come out of the same
/// binary. Still a constant rather than a literal at the call site: it names
/// a spawn dependency of this crate, and a reader asking "what does the
/// prompt shell out to" should find one answer.
pub const JJ_CLIENT: &str = "jj";

/// What `jj view prompt` prints outside every view.
const OUTSIDE: &str = "-";

/// `jj view`'s local state, parsed at the protocol boundary so that an
/// unknown word is a failure the prompt SHOWS (`view:ERR`), never a value
/// that renders flagless and reads as pristine. The vocabulary's one
/// emitter is the view CLI (crates/jj/client/cli/src/view/status.rs,
/// `LocalState::as_str`); these six variants are that list, and the test
/// below walks it word by word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Pristine,
    Patched,
    Unanchored,
    Conflicted,
    Missing,
    NotADirectory,
}

impl ViewState {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "pristine" => Some(Self::Pristine),
            "patched" => Some(Self::Patched),
            "unanchored" => Some(Self::Unanchored),
            "conflicted" => Some(Self::Conflicted),
            "missing" => Some(Self::Missing),
            "not-a-directory" => Some(Self::NotADirectory),
            _ => None,
        }
    }
}

/// The view the prompt directory sits inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub name: String,
    pub state: ViewState,
}

/// What the prompt renders for views: nothing, a view, or a failure it
/// must not hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Outside,
    Inside(View),
    Failed,
}

/// The view owning `cwd` in the workspace at `root`: `Ok(None)` outside
/// every view, `Err` for any failure to ask (missing binary, non-zero exit,
/// output off protocol).
pub fn at(root: &Path, cwd: &Path) -> Result<Option<View>> {
    let output = Command::new(JJ_CLIENT)
        .args(["view", "prompt", "--repository"])
        .arg(root)
        .args(["--ignore-working-copy", "--color=never", "--quiet"])
        .current_dir(cwd)
        .output()
        .wrap_err_with(|| format!("failed to run `{JJ_CLIENT} view prompt`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "`{JJ_CLIENT} view prompt` failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .wrap_err_with(|| format!("`{JJ_CLIENT} view prompt` wrote non-UTF-8 output"))?;
    parse(&stdout)
}

/// One `name<TAB>state` line, or the outside token; anything else is a
/// protocol failure, so an empty stdout can never read as "no view".
fn parse(stdout: &str) -> Result<Option<View>> {
    let line = stdout.lines().next().unwrap_or_default();
    if line == OUTSIDE {
        return Ok(None);
    }
    let Some((name, word)) = line
        .split_once('\t')
        .filter(|(name, word)| !name.is_empty() && !word.is_empty())
    else {
        return Err(eyre!(
            "`{JJ_CLIENT} view prompt` printed {stdout:?}, neither `{OUTSIDE}` nor `name<TAB>state`"
        ));
    };
    let Some(state) = ViewState::parse(word) else {
        return Err(eyre!(
            "`{JJ_CLIENT} view prompt` printed unknown state {word:?} for view {name:?}; the \
             vocabulary is emitted by `jj view` (view/status.rs) and an unrecognised word is a \
             failure to show, never a flagless rendering that reads as pristine"
        ));
    };
    Ok(Some(View {
        name: name.to_owned(),
        state,
    }))
}

#[cfg(test)]
mod tests {
    use super::{View, ViewState, parse};

    #[test]
    fn a_view_line_carries_name_and_state() {
        assert_eq!(
            parse("ix\tpatched\n").expect("a view line parses"),
            Some(View {
                name: "ix".to_owned(),
                state: ViewState::Patched,
            })
        );
    }

    /// The emitter's whole vocabulary (view/status.rs, `LocalState::as_str`),
    /// word by word: a new word over there must land here deliberately.
    #[test]
    fn the_state_vocabulary_is_exactly_the_emitters() {
        for (word, state) in [
            ("pristine", ViewState::Pristine),
            ("patched", ViewState::Patched),
            ("unanchored", ViewState::Unanchored),
            ("conflicted", ViewState::Conflicted),
            ("missing", ViewState::Missing),
            ("not-a-directory", ViewState::NotADirectory),
        ] {
            assert_eq!(
                parse(&format!("ix\t{word}\n")).expect("a vocabulary word parses"),
                Some(View {
                    name: "ix".to_owned(),
                    state,
                }),
                "word {word}"
            );
        }
    }

    #[test]
    fn an_unknown_state_word_is_a_failure_not_pristine() {
        assert!(parse("ix\tresplendent\n").is_err());
    }

    #[test]
    fn the_outside_token_is_no_view() {
        assert_eq!(parse("-\n").expect("the outside token parses"), None);
    }

    #[test]
    fn a_line_without_a_state_is_a_protocol_failure() {
        assert!(parse("ix\n").is_err());
    }

    #[test]
    fn an_empty_stdout_is_a_protocol_failure_not_silence() {
        assert!(parse("").is_err());
    }
}
