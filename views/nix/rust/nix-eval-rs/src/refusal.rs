//! Stable names for the reasons this evaluator refuses to serve something.
//!
//! A refusal used to carry only prose: `"effect domain 'x'"`, `"path
//! interpolation"`, `"builtins.derivationStrict with a floating output"`. That
//! reads well in one error message and is useless as a population. Counting
//! refusals across a fleet meant slicing the prose --
//! `maintainers/ix/nixpkgs-frontier.sh` did it with `grep -o 'rust-eval
//! unimplemented: .*' | cut -c1-140` -- so two refusals of the same kind that
//! interpolate different names counted as two kinds, and rewording an error
//! message silently reset the census.
//!
//! So a refusal now carries a [`RefusalToken`] as well as its prose. The token
//! is the histogram key and is expected to outlive any particular wording; the
//! prose is what the user reads and stays free to say whatever is most useful.
//! Tokens are lowercase and hyphenated so they survive a journal field, a
//! ClickHouse group-by and a `NIX_SHOW_STATS` JSON key without quoting.

use std::fmt;

/// Why the Rust evaluator would not serve an expression.
///
/// Add a variant rather than reaching for a near-miss: an over-broad token
/// costs a re-run of whatever census motivated it, and the flip criterion
/// (refusal rate zero) is read per token.
///
/// **Remove one the moment its last emission site goes.** Every token here
/// sends a whole evaluation back to the C++ evaluator, so the list is the
/// fallback surface written down, and a token nothing raises overstates it:
/// a census reads a row that cannot move, and the next reader has to
/// rediscover that it is dead. `path-interpolation` sat here for exactly
/// that reason -- ENG-12852 implemented interpolated path literals and left
/// the name behind, and `maintainers/ix/shadow-fleet-run.md` still reports
/// 2,638 of them against a construct that now evaluates. Retiring a token
/// means deleting the variant, its emission sites, its row in every census
/// document, and this crate's count below, in one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefusalToken {
    /// A syntax node the compiler has no lowering for. The detail names the
    /// node kind, which is an open set, so it stays in the detail rather than
    /// splintering the token space.
    UnsupportedSyntax,
    /// An operator the compiler has no lowering for.
    UnsupportedOperator,
    /// An effect the scheduler has no domain for.
    EffectDomain,
    /// `restrict-eval` or `pure-eval` is on, which this evaluator cannot
    /// honour because it reaches the filesystem outside cppnix's access
    /// control. Refusing is the only honest answer: reading anyway would give
    /// a weaker guarantee than the setting promises, silently.
    AccessControl,
    /// The evaluation needed a store and the embedder supplied none.
    StoreUnavailable,
    /// The evaluation needed a search-path resolver and none was supplied, or
    /// the resolver refused the lookup.
    SearchPath,
    /// `builtins.derivationStrict` in a shape this backend does not build.
    DerivationStrict,
    /// A hash over an input this backend cannot read.
    UnreadableInput,
    /// A builtin present in the table but not implemented here.
    UnimplementedBuiltin,
    /// The source is not UTF-8. cppnix accepts arbitrary bytes inside string
    /// literals and this parser (rnix) reads `&str`, so such a file is a
    /// coverage gap rather than a caller error. Its own token because it is
    /// the one refusal raised before anything is compiled, and a census that
    /// could not name it had one bucket it could not explain.
    NonUtf8Source,
    /// `nix-instantiate --eval` without `--strict` on a value with
    /// children: cppnix prints `<CODE>` for the children still unevaluated,
    /// and which those are is evaluator-internal. Scalars are served.
    LazyPrint,
    /// A byte string reached a boundary that is text-only in this backend:
    /// an attribute name headed for the `str`-keyed interner, a string
    /// coerced to a path, a URL, an ABI leg that carries text. String
    /// *values* are arbitrary bytes here as they are in cppnix (ENG-13147);
    /// these few boundaries are not, and cppnix accepts bytes at each of
    /// them, so refusing is a named coverage gap where answering would be
    /// either a lie (lossy repair) or a divergence (an error cppnix does not
    /// raise).
    NonUtf8Boundary,
    /// `builtins.path` in a shape this backend cannot copy: the walk that
    /// applies the filter runs in the evaluator, and there are tree shapes
    /// whose filtered copy it cannot reproduce byte-for-byte. Its own token
    /// rather than `UnimplementedBuiltin`, because the builtin *is*
    /// implemented -- a census that could not tell the two apart would read a
    /// narrow shape gap as the whole primop being missing. The one shape it
    /// names today is a symlinked root under a filter, ENG-12700.
    AddPath,
    /// A comparison whose operand shapes this backend does not order.
    UnorderedComparison,
    /// Printing or coercing a value in a shape this backend does not render.
    UnsupportedRender,
    /// An IR op the VM has no execution for.
    UnsupportedOp,
    // ---- Raised by the command layer, never by the evaluator. -----------
    //
    // These are shapes of *invocation* the Rust backend is not wired for,
    // rather than constructs it cannot evaluate: `nix eval --apply`, XML
    // output, an expression on stdin. They live in this enum anyway, and that
    // is the point -- the census counts refusals from both sides, and two
    // hand-maintained vocabularies would drift the moment one side gained a
    // kind. `RefusalToken::raised_by` records which side raises each, so the
    // split stays visible instead of being folded away.
    /// `--write-to`, which writes a tree rather than printing.
    CommandWriteTo,
    /// `--xml`, an output format this backend does not render.
    CommandXmlOutput,
    /// The expression came from stdin rather than a file or `--expr`.
    CommandStdin,
    /// A store-path installable, which names an already-built object rather
    /// than an expression to evaluate. (Flake installables are served.)
    CommandInstallable,
    /// An output selection (`^out`) on the installable.
    CommandOutputSelection,
    /// A `--file` that is not a plain path.
    CommandFile,
    /// The command is not one the backend serves at all.
    CommandUnsupported,
    /// An installable that does not name a derivation: a store path, or an
    /// attribute set cppnix would recurse into looking for one.
    CommandNotADerivation,
    /// A `meta.outputsToInstall` or `outputSpecified` shape whose reduction
    /// of the output set this backend does not reproduce.
    CommandOutputsToInstall,
    /// An installable `nix run` was given that is not an app the backend
    /// can read: an `apps.*` set whose fields are not the strings cppnix's
    /// `toApp` reads, so cppnix's own diagnostic stands.
    CommandNotAnApp,

    /// A refusal that crossed a boundary carrying no token: a cache row
    /// written before tokens existed, or a producer that does not set one. A
    /// category of its own rather than a guess, so a census can see how much
    /// of its population it cannot classify instead of silently attributing
    /// it to whatever token seemed closest.
    Unrecorded,
}

impl RefusalToken {
    /// The stable name. Changing one of these is a census-visible event and
    /// should be treated like renaming a metric.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RefusalToken::UnsupportedSyntax => "unsupported-syntax",
            RefusalToken::UnsupportedOperator => "unsupported-operator",
            RefusalToken::EffectDomain => "effect-domain",
            RefusalToken::AccessControl => "access-control",
            RefusalToken::StoreUnavailable => "store-unavailable",
            RefusalToken::SearchPath => "search-path",
            RefusalToken::DerivationStrict => "derivation-strict",
            RefusalToken::UnreadableInput => "unreadable-input",
            RefusalToken::UnimplementedBuiltin => "unimplemented-builtin",
            RefusalToken::NonUtf8Source => "non-utf8-source",
            RefusalToken::LazyPrint => "lazy-print",
            RefusalToken::NonUtf8Boundary => "non-utf8-boundary",
            RefusalToken::AddPath => "add-path",
            RefusalToken::UnorderedComparison => "unordered-comparison",
            RefusalToken::UnsupportedRender => "unsupported-render",
            RefusalToken::UnsupportedOp => "unsupported-op",
            RefusalToken::CommandWriteTo => "command-write-to",
            RefusalToken::CommandXmlOutput => "command-xml-output",
            RefusalToken::CommandStdin => "command-stdin",
            RefusalToken::CommandInstallable => "command-installable",
            RefusalToken::CommandOutputSelection => "command-output-selection",
            RefusalToken::CommandFile => "command-file",
            RefusalToken::CommandUnsupported => "command-unsupported",
            RefusalToken::CommandNotADerivation => "command-not-a-derivation",
            RefusalToken::CommandOutputsToInstall => "command-outputs-to-install",
            RefusalToken::CommandNotAnApp => "command-not-an-app",
            RefusalToken::Unrecorded => "unrecorded",
        }
    }

    /// Every token, so a consumer can build a histogram with a denominator
    /// rather than only counting what it happened to see. Held against
    /// `as_str` by `every_token_is_listed_and_named_once`.
    pub const ALL: &'static [RefusalToken] = &[
        RefusalToken::UnsupportedSyntax,
        RefusalToken::UnsupportedOperator,
        RefusalToken::EffectDomain,
        RefusalToken::AccessControl,
        RefusalToken::StoreUnavailable,
        RefusalToken::SearchPath,
        RefusalToken::DerivationStrict,
        RefusalToken::UnreadableInput,
        RefusalToken::UnimplementedBuiltin,
        RefusalToken::NonUtf8Source,
        RefusalToken::LazyPrint,
        RefusalToken::NonUtf8Boundary,
        RefusalToken::AddPath,
        RefusalToken::UnorderedComparison,
        RefusalToken::UnsupportedRender,
        RefusalToken::UnsupportedOp,
        RefusalToken::CommandWriteTo,
        RefusalToken::CommandXmlOutput,
        RefusalToken::CommandStdin,
        RefusalToken::CommandInstallable,
        RefusalToken::CommandOutputSelection,
        RefusalToken::CommandFile,
        RefusalToken::CommandUnsupported,
        RefusalToken::CommandNotADerivation,
        RefusalToken::CommandOutputsToInstall,
        RefusalToken::CommandNotAnApp,
        RefusalToken::Unrecorded,
    ];

    /// Which side of the C ABI raises this token.
    ///
    /// Recorded rather than inferred from the name, so that a census can say
    /// "the evaluator refused nothing" without having to know which prefixes
    /// mean what -- and so that moving a refusal from one side to the other
    /// is a visible edit here.
    #[must_use]
    pub fn raised_by(self) -> RaisedBy {
        match self {
            RefusalToken::CommandWriteTo
            | RefusalToken::CommandXmlOutput
            | RefusalToken::CommandStdin
            | RefusalToken::CommandInstallable
            | RefusalToken::CommandOutputSelection
            | RefusalToken::CommandFile
            | RefusalToken::CommandUnsupported
            | RefusalToken::CommandNotADerivation
            | RefusalToken::CommandOutputsToInstall
            | RefusalToken::CommandNotAnApp => RaisedBy::CommandLayer,
            RefusalToken::Unrecorded => RaisedBy::Sentinel,
            _ => RaisedBy::Evaluator,
        }
    }

    /// The token this name belongs to, or `None` if it names nothing.
    ///
    /// The inverse of [`as_str`](Self::as_str), for the boundaries that carry
    /// a token as text: the session protocol's status field, and anything
    /// reading one back off disk.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.as_str() == name)
    }
}

/// Which layer raises a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaisedBy {
    /// The Rust evaluator, over the C ABI.
    Evaluator,
    /// The C++ command layer, before the evaluator is reached.
    CommandLayer,
    /// Neither exclusively: a token both sides can genuinely raise.
    Either,
    /// Nobody raises it. `Unrecorded` is the name for a *missing* name, not a
    /// kind of refusal, so asking which layer produced it is a category
    /// error: it is assigned at a boundary precisely when no layer said.
    ///
    /// Its own value rather than `Either`, because `Either` includes the
    /// command layer, and a guard that holds every command-layer token
    /// against the C++ constants then demands a constant for a sentinel that
    /// is not a refusal kind. That is not hypothetical -- the phantom-token
    /// guard in `src/nix/rust-eval-refusal-vocabulary-test.cc` failed on its
    /// first real run for exactly this reason.
    Sentinel,
}

/// A refusal: the stable token a census groups by, and the prose a human
/// reads.
///
/// Carried as one payload so that every arm which merely forwards a refusal
/// between error types keeps compiling unchanged; only the places that raise
/// one have to say which kind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub token: RefusalToken,
    pub detail: String,
}

impl Refusal {
    pub fn new(token: RefusalToken, detail: impl Into<String>) -> Self {
        Refusal {
            token,
            detail: detail.into(),
        }
    }
}

/// Renders the prose alone, so existing messages read exactly as they did.
/// The token travels beside the message rather than inside it: a caller that
/// wants to group asks for `.token`, and one that wants to show a user asks
/// for this.
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// `ALL` is hand-written because an enum cannot be iterated, so it can
    /// drift from the match in the one direction the compiler cannot see: a
    /// variant added to both the enum and `as_str` but forgotten here would
    /// be missing from every histogram's denominator without any build
    /// failing.
    #[test]
    fn every_token_is_listed_and_named_once() {
        let names: Vec<&str> = RefusalToken::ALL.iter().map(|t| t.as_str()).collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(
            unique.len(),
            names.len(),
            "two tokens share a name: {names:?}"
        );

        let listed: BTreeSet<RefusalToken> = RefusalToken::ALL.iter().copied().collect();
        assert_eq!(
            listed.len(),
            RefusalToken::ALL.len(),
            "ALL repeats a variant"
        );

        // Catches a variant added to the enum but not to ALL. The match in
        // `as_str` is exhaustive, so the compiler already refuses a variant
        // with no name; this is the count that the compiler cannot check.
        assert_eq!(
            RefusalToken::ALL.len(),
            27,
            "a token was added or removed; update this count deliberately, and \
             check whatever reads the histogram still has a row for it"
        );
    }

    /// Tokens end up as journal fields, ClickHouse group-by keys and JSON
    /// object keys. Anything needing quoting in one of those makes a census
    /// query subtly wrong rather than loudly broken.
    #[test]
    fn token_names_are_safe_as_a_bare_key() {
        for token in RefusalToken::ALL {
            let name = token.as_str();
            assert!(!name.is_empty(), "empty token name");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()),
                "token '{name}' is not lowercase-and-hyphens, so it needs quoting somewhere"
            );
        }
    }

    /// The prose is what a user sees, and it must not gain a token prefix by
    /// accident: the gate greps these messages.
    #[test]
    fn display_is_the_prose_alone() {
        let r = Refusal::new(RefusalToken::EffectDomain, "effect domain 'x'");
        assert_eq!(r.to_string(), "effect domain 'x'");
    }
}
