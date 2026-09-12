//! The ATerm form of a store derivation: the `.drv` file's bytes.
//!
//! Rung C wants two different things from this file and they have different
//! bars. Step 1 needs an output path, which is a hash *of* these bytes, so a
//! single wrong byte moves the path and the failure surfaces as an unrelated
//! hash mismatch. Step 2 needs the `.drv` files themselves to be identical.
//! Both reduce to "produce exactly the bytes cppnix's `Derivation::unparse`
//! would", so that is what this module is, and it is tested by round-tripping
//! real `.drv` files out of a real store rather than by fixtures written from
//! reading the format.
//!
//! **This deliberately does not model what a derivation means.** cppnix has
//! five output kinds (`InputAddressed`, `CAFixed`, `CAFloating`, `Deferred`,
//! `Impure`) that are distinguished by which of the three fields beside the
//! output name are empty. Interpreting them is needed to *compute* an output
//! path, but not to reproduce bytes, and keeping them as the four raw strings
//! they are on disk means a round-trip cannot be wrong about a case it does
//! not understand. `derivationStrict` will need the interpretation; the writer
//! does not, and conflating the two is how a byte-exactness bug hides inside a
//! semantics bug.

/// A parsed `.drv`, held as close to its on-disk form as possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivation {
    /// `(name, path, hashAlgo, hash)`, exactly the four fields on disk. Which
    /// of the five output kinds this is depends on which are empty; nothing
    /// here needs to know.
    pub outputs: Vec<Output>,
    /// Input derivations, each with the output names depended on.
    pub input_drvs: Vec<InputDrv>,
    pub input_srcs: Vec<String>,
    pub platform: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub path: String,
    pub hash_algo: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDrv {
    pub drv_path: String,
    pub outputs: Vec<String>,
}

impl Derivation {
    /// `writeDerivation`'s reference set (`derivations.cc:172`): the input
    /// sources plus every input derivation, as a set. What an embedder
    /// derives from the parsed ATerm when it writes one; the evaluator never
    /// sends a second copy across the boundary.
    #[must_use]
    pub fn references(&self) -> Vec<String> {
        let mut references = self.input_srcs.clone();
        references.extend(self.input_drvs.iter().map(|d| d.drv_path.clone()));
        references.sort();
        references.dedup();
        references
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// Which of cppnix's five output kinds an [`Output`]'s four raw fields spell.
///
/// The bytes carry no tag: cppnix's `unparse` writes a `std::visit` over the
/// five alternatives and each one leaves a different subset of the three
/// fields beside the name empty (`derivations.cc:663`). Recovering the kind is
/// therefore a decision table over emptiness, and it is here rather than in
/// the writer because the writer does not need it -- see the module comment.
///
/// This exists so a census over a real store can say *which* shapes a
/// round-trip actually covered. "420,831 files agreed" and "420,831 files
/// agreed, of which 3 were content-addressed and none were impure" are
/// different claims, and only the second one is evidence about the five kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// `(name, path, "", "")` -- the ordinary case.
    InputAddressed,
    /// `(name, path, methodAlgo, hex)` -- `outputHash` was given.
    CaFixed,
    /// `(name, "", methodAlgo, "")` -- `__contentAddressed`.
    CaFloating,
    /// `(name, "", "", "")` -- an output path not yet known.
    Deferred,
    /// `(name, "", methodAlgo, "impure")`.
    Impure,
    /// None of the five. cppnix cannot write this, so a file that produces it
    /// is either hand-made or evidence that this table is wrong; either way a
    /// census must report it rather than silently bucket it as the nearest
    /// match.
    Unrecognised,
}

impl Output {
    /// Classify by which fields are empty. See [`OutputKind`].
    #[must_use]
    pub fn kind(&self) -> OutputKind {
        match (
            self.path.is_empty(),
            self.hash_algo.is_empty(),
            self.hash.as_str(),
        ) {
            (false, true, "") => OutputKind::InputAddressed,
            (false, false, h) if !h.is_empty() => OutputKind::CaFixed,
            (true, false, "") => OutputKind::CaFloating,
            (true, true, "") => OutputKind::Deferred,
            (true, false, "impure") => OutputKind::Impure,
            _ => OutputKind::Unrecognised,
        }
    }
}

/// Put every field into the order cppnix's containers hold it in.
///
/// **This is the half of byte-exactness a round trip cannot see.** Parsing
/// preserves the order found on disk and the writer emits it back, so
/// `parse` then `unparse` agrees on any ordering whatsoever, including a wrong
/// one. A derivation *constructed* by `derivationStrict` has no disk order to
/// inherit and must produce cppnix's, which comes from the container types in
/// `BasicDerivation` rather than from any explicit sort:
///
/// | field | cppnix type | order |
/// |---|---|---|
/// | `outputs` | `std::map<std::string, DerivationOutput>` | by output name |
/// | `inputDrvs.map` | `std::map<StorePath, ChildNode>` | by store path |
/// | an input's output names | `StringSet` | by name |
/// | `inputSrcs` | `StorePathSet`, printed through `printStorePathSet` into a `StringSet` | by full path |
/// | `env` | `StringPairs` = `std::map<std::string, std::string>` | by variable name |
/// | `args` | `std::vector<std::string>` | **as given; not sorted** |
///
/// `StorePath`'s `operator<=>` is defaulted over its single `baseName` member
/// (`store/path.hh:56`), and `printStorePath` prefixes every path with the
/// same store directory, so ordering by base name and by printed path agree
/// and this can sort the printed form.
///
/// Comparison is byte-wise in both languages: `std::string`'s `operator<` goes
/// through `char_traits<char>::compare`, i.e. `memcmp`, and Rust's `Ord` for
/// `str` is `[u8]`'s.
pub fn canonicalise(drv: &mut Derivation) {
    drv.outputs.sort_by(|a, b| a.name.cmp(&b.name));
    drv.input_drvs.sort_by(|a, b| a.drv_path.cmp(&b.drv_path));
    for input in &mut drv.input_drvs {
        input.outputs.sort();
    }
    drv.input_srcs.sort();
    drv.env.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Whether every field is already in [`canonicalise`]'s order.
///
/// Run over a real store this is a check *of the ordering rule*, not of the
/// derivation: cppnix wrote those bytes, so any file this rejects means the
/// table above is wrong about what cppnix does.
#[must_use]
pub fn is_canonical(drv: &Derivation) -> bool {
    let mut sorted = drv.clone();
    canonicalise(&mut sorted);
    &sorted == drv
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrvError {
    /// The `DrvWithVersion("xp-dyn-drv",...)` form. cppnix only writes it when
    /// a derivation depends on the output of a derivation that is itself an
    /// output, and its `inputDrvs` is a recursive map rather than a flat list.
    /// Refused by name so a corpus run reports it instead of mis-parsing it.
    DynamicDerivations,
    /// Anything the grammar did not allow, with the byte offset.
    Malformed { at: usize, want: String },
    /// A string field whose bytes are not UTF-8. cppnix holds a derivation's
    /// strings as bytes and does not require it, so this is refused rather
    /// than lossily converted: a replacement character would round-trip to
    /// different bytes than it came from.
    NonUtf8 { at: usize },
}

impl core::fmt::Display for DrvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DrvError::DynamicDerivations => {
                write!(
                    f,
                    "dynamic-derivation ATerm (DrvWithVersion) is unimplemented"
                )
            }
            DrvError::Malformed { at, want } => {
                write!(f, "malformed derivation at byte {at}: expected {want}")
            }
            DrvError::NonUtf8 { at } => write!(f, "non-UTF-8 string field at byte {at}"),
        }
    }
}

impl core::error::Error for DrvError {}

type Result<T> = core::result::Result<T, DrvError>;

/// A cursor over the ATerm bytes. Every read is bounds-checked through
/// `get`, so a truncated `.drv` is a `Malformed` and never a panic.
struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn err<T>(&self, want: &str) -> Result<T> {
        Err(DrvError::Malformed {
            at: self.i,
            want: want.to_owned(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn expect(&mut self, c: u8) -> Result<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err(&format!("'{}'", c as char))
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        let end = self.i.saturating_add(lit.len());
        if self.s.get(self.i..end) == Some(lit.as_bytes()) {
            self.i = end;
            true
        } else {
            false
        }
    }

    /// A quoted string, undoing the four escapes cppnix's `printString`
    /// applies. Unquoted fields parse identically because cppnix never emits a
    /// backslash into one.
    fn string(&mut self) -> Result<String> {
        let start = self.i;
        self.expect(b'"')?;
        // Bytes, not chars. `byte as char` decodes one byte as one codepoint,
        // which silently mangles every multi-byte UTF-8 sequence -- a real
        // derivation carrying "Sørensen" round-tripped to "SÃ¸rensen". The
        // conversion happens once, at the end, over the whole field.
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err("closing '\"'");
            };
            self.i += 1;
            match c {
                b'"' => {
                    return String::from_utf8(out).map_err(|_| DrvError::NonUtf8 { at: start });
                }
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return self.err("escape character");
                    };
                    self.i += 1;
                    out.push(match e {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        other => other,
                    });
                }
                other => out.push(other),
            }
        }
    }

    /// A `[` ... `]` list, with `f` reading one element.
    fn list<T>(&mut self, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(out);
        }
        loop {
            out.push(f(self)?);
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => return self.err("',' or ']'"),
            }
        }
    }
}

/// Parse the bytes of a `.drv`.
pub fn parse(input: &str) -> Result<Derivation> {
    let mut c = Cursor {
        s: input.as_bytes(),
        i: 0,
    };
    if c.eat("DrvWithVersion(") {
        return Err(DrvError::DynamicDerivations);
    }
    if !c.eat("Derive(") {
        return c.err("\"Derive(\" or \"DrvWithVersion(\"");
    }

    let outputs = c.list(|c| {
        c.expect(b'(')?;
        let name = c.string()?;
        c.expect(b',')?;
        let path = c.string()?;
        c.expect(b',')?;
        let hash_algo = c.string()?;
        c.expect(b',')?;
        let hash = c.string()?;
        c.expect(b')')?;
        Ok(Output {
            name,
            path,
            hash_algo,
            hash,
        })
    })?;
    c.expect(b',')?;

    let input_drvs = c.list(|c| {
        c.expect(b'(')?;
        let drv_path = c.string()?;
        c.expect(b',')?;
        let outputs = c.list(Cursor::string)?;
        c.expect(b')')?;
        Ok(InputDrv { drv_path, outputs })
    })?;
    c.expect(b',')?;

    let input_srcs = c.list(Cursor::string)?;
    c.expect(b',')?;
    let platform = c.string()?;
    c.expect(b',')?;
    let builder = c.string()?;
    c.expect(b',')?;
    let args = c.list(Cursor::string)?;
    c.expect(b',')?;

    let env = c.list(|c| {
        c.expect(b'(')?;
        let name = c.string()?;
        c.expect(b',')?;
        let value = c.string()?;
        c.expect(b')')?;
        Ok(EnvVar { name, value })
    })?;
    c.expect(b')')?;
    // Nothing may follow. Without this a file that is a valid derivation
    // followed by anything at all parses, and the harness only notices
    // because the re-render is shorter -- which reports the wrong thing
    // ("differs") about the wrong stage.
    if c.i != c.s.len() {
        return c.err("end of input");
    }

    Ok(Derivation {
        outputs,
        input_drvs,
        input_srcs,
        platform,
        builder,
        args,
        env,
    })
}

/// cppnix's `printString`: the only four escapes it applies.
///
/// Byte-wise like cppnix's loop, copying each escape-free run in one
/// `push_str`: every escape trigger is ASCII, so a trigger's byte offset is a
/// char boundary and the runs between triggers are the string's own bytes.
/// A `&str` can hold nothing but UTF-8, and everything that reaches a
/// [`Derivation`] was text-validated at intake (ENG-13147: a non-UTF-8 byte
/// string refuses at the store boundary by name, so it can never diverge
/// silently here). A char-at-a-time `push` here was 2.1% of a NixOS
/// toplevel's samples (prof4, 2026-09-04): derivation text is mostly long
/// escape-free runs.
fn quoted(out: &mut String, s: &str) {
    out.push('"');
    let mut run = 0;
    for (i, b) in s.bytes().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            _ => continue,
        };
        out.push_str(&s[run..i]);
        out.push_str(escape);
        run = i + 1;
    }
    out.push_str(&s[run..]);
    out.push('"');
}

/// cppnix's `printUnquotedString`: quotes, but no escaping.
///
/// Used for the fields cppnix knows cannot need it -- store paths, output
/// names, hash algorithms, the platform. Reproducing the distinction matters
/// because it is what the bytes on disk do, even though for every well-formed
/// derivation the two agree.
fn unquoted(out: &mut String, s: &str) {
    out.push('"');
    out.push_str(s);
    out.push('"');
}

fn join<T>(out: &mut String, items: &[T], mut f: impl FnMut(&mut String, &T)) {
    out.push('[');
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        f(out, item);
    }
    out.push(']');
}

/// Render a derivation back to ATerm.
///
/// With `mask_outputs`, every output path and every environment variable named
/// after an output is replaced by the empty string, because an output path
/// cannot be part of the input to the hash that produces it. This mirrors
/// cppnix's `maskOutputs` parameter to `Derivation::unparse`
/// (`src/libstore/derivations.cc:637`) and nothing more.
///
/// **Masking alone is not what `hashDerivationModulo` hashes**, and reading it
/// that way is how a wrong `outPath` gets blamed on the hash function.
/// `hashDerivationModulo` (`derivations.cc:893`) does two further things this
/// function deliberately does not:
///
/// * it passes an `actualInputs` map in which every input **derivation path is
///   replaced by that input's own modulo hash**, hex, which also re-sorts the
///   list, since the map is keyed on the hash rather than on the path;
/// * for a fixed-output derivation it does not call `unparse` at all, hashing
///   `fixed:out:<methodAlgo>:<hash>:<path>` per output instead.
///
/// Both belong to the caller that computes output paths, which is why they are
/// not folded in here: this function's contract is "the bytes cppnix would
/// write", and that is the contract the store-wide round trip checks.
#[must_use]
pub fn unparse(drv: &Derivation, mask_outputs: bool) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("Derive(");

    join(&mut out, &drv.outputs, |o, output| {
        o.push('(');
        unquoted(o, &output.name);
        o.push(',');
        unquoted(o, if mask_outputs { "" } else { &output.path });
        o.push(',');
        unquoted(o, &output.hash_algo);
        o.push(',');
        unquoted(o, &output.hash);
        o.push(')');
    });
    out.push(',');

    join(&mut out, &drv.input_drvs, |o, input| {
        o.push('(');
        unquoted(o, &input.drv_path);
        o.push(',');
        join(o, &input.outputs, |o, name| unquoted(o, name));
        o.push(')');
    });
    out.push(',');

    join(&mut out, &drv.input_srcs, |o, src| unquoted(o, src));
    out.push(',');
    unquoted(&mut out, &drv.platform);
    out.push(',');
    quoted(&mut out, &drv.builder);
    out.push(',');
    join(&mut out, &drv.args, |o, a| quoted(o, a));
    out.push(',');

    let is_output = |name: &str| drv.outputs.iter().any(|o| o.name == name);
    join(&mut out, &drv.env, |o, var| {
        o.push('(');
        quoted(o, &var.name);
        o.push(',');
        if mask_outputs && is_output(&var.name) {
            quoted(o, "");
        } else {
            quoted(o, &var.value);
        }
        o.push(')');
    });

    out.push(')');
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Derivation, DrvError, EnvVar, InputDrv, Output, OutputKind, canonicalise, is_canonical,
        parse, quoted, unparse,
    };

    /// `quoted` copies escape-free runs and escapes the five bytes cppnix's
    /// `printString` escapes (four rules; `"` and `\\` share one), at every
    /// position a run boundary can fall:
    /// start, end, adjacent, around multibyte text, and the empty string.
    #[test]
    fn quoted_matches_the_char_at_a_time_rendering() {
        fn reference(s: &str) -> String {
            let mut out = String::from('"');
            for ch in s.chars() {
                match ch {
                    '"' | '\\' => {
                        out.push('\\');
                        out.push(ch);
                    }
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    other => out.push(other),
                }
            }
            out.push('"');
            out
        }
        for s in [
            "",
            "plain",
            "\"",
            "\\",
            "\"\\\n\r\t",
            "a\"b",
            "\"at start",
            "at end\"",
            "tab\tand\nnewline\r",
            "héllo \"wörld\" \u{1F600}\n\u{1F600}",
            "\u{1F600}",
        ] {
            let mut out = String::new();
            quoted(&mut out, s);
            assert_eq!(out, reference(s), "input {s:?}");
        }
        let mut out = String::new();
        quoted(&mut out, "x\"y");
        assert_eq!(out, "\"x\\\"y\"");
    }

    /// Every `DrvError` says what went wrong and where.
    ///
    /// The parse errors are how a damaged or unsupported `.drv` is reported to
    /// a user, and `a_truncated_derivation_reports_an_offset_instead_of_panicking`
    /// already checks that the *offset* is carried in the value. What it does
    /// not check is that the offset reaches the rendered text: a `Display`
    /// returning `Ok(())` without writing anything satisfies every assertion
    /// on the error value and produces a blank error at the terminal. That is
    /// not a Tier 2 wording difference, it is the message going missing, and
    /// nothing else in this suite was watching for it (ENG-13020).
    ///
    /// Asserted on the data each variant carries rather than on the whole
    /// sentence, so rewording stays free.
    #[test]
    fn every_parse_error_renders_what_it_carries() {
        let dynamic = DrvError::DynamicDerivations.to_string();
        assert!(
            dynamic.contains("dynamic-derivation") && dynamic.contains("unimplemented"),
            "{dynamic}"
        );

        let malformed = DrvError::Malformed {
            at: 17,
            want: "a closing paren".to_owned(),
        }
        .to_string();
        assert!(
            malformed.contains("17"),
            "the byte offset is missing: {malformed}"
        );
        assert!(
            malformed.contains("a closing paren"),
            "what was expected is missing: {malformed}"
        );

        let non_utf8 = DrvError::NonUtf8 { at: 42 }.to_string();
        assert!(
            non_utf8.contains("42"),
            "the byte offset is missing: {non_utf8}"
        );

        // And the three do not render alike, which a `Display` collapsed to a
        // constant would also satisfy the checks above with.
        assert_ne!(dynamic, malformed);
        assert_ne!(malformed, non_utf8);
    }

    /// Parse and re-render, reporting a failure as text rather than
    /// unwrapping: the workspace denies `unwrap`, `expect` and `panic`, and a
    /// test that says which stage failed beats one that says "called unwrap
    /// on an Err".
    fn round(text: &str, mask: bool) -> String {
        match parse(text) {
            Ok(drv) => unparse(&drv, mask),
            Err(e) => format!("parse error: {e}"),
        }
    }

    /// The smallest complete derivation, written out by hand so the shape of
    /// the format is visible here and not only in cppnix.
    const SAMPLE: &str = concat!(
        r#"Derive([("out","/nix/store/aaa-x","","")],"#,
        r#"[("/nix/store/bbb-y.drv",["out"])],"#,
        r#"["/nix/store/ccc-src"],"#,
        r#""x86_64-linux","/bin/sh",["-c","echo hi"],"#,
        r#"[("out","/nix/store/aaa-x"),("system","x86_64-linux")])"#,
    );

    #[test]
    fn a_derivation_parses_to_its_parts_and_back_to_its_bytes() {
        let want = Derivation {
            outputs: vec![Output {
                name: "out".into(),
                path: "/nix/store/aaa-x".into(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![InputDrv {
                drv_path: "/nix/store/bbb-y.drv".into(),
                outputs: vec!["out".into()],
            }],
            input_srcs: vec!["/nix/store/ccc-src".into()],
            platform: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
            env: vec![
                EnvVar {
                    name: "out".into(),
                    value: "/nix/store/aaa-x".into(),
                },
                EnvVar {
                    name: "system".into(),
                    value: "x86_64-linux".into(),
                },
            ],
        };
        assert_eq!(parse(SAMPLE), Ok(want));
        assert_eq!(round(SAMPLE, false), SAMPLE);
    }

    /// What `hashDerivationModulo` hashes: an output path cannot be part of
    /// the input to the hash that produces it, so both the output's own path
    /// and the environment variable named after it are blanked.
    #[test]
    fn masking_blanks_output_paths_and_the_env_vars_named_after_them() {
        let masked = round(SAMPLE, true);
        assert!(
            masked.contains(r#"("out","","","")"#),
            "output path not masked: {masked}"
        );
        assert!(
            masked.contains(r#"("out","")"#),
            "output env var not masked: {masked}"
        );
        // A non-output variable is left alone, so the mask is keyed on the
        // name and not on the text of the path.
        assert!(
            masked.contains(r#"("system","x86_64-linux")"#),
            "over-masked: {masked}"
        );
    }

    /// The regression the store-wide round trip found: decoding a string one
    /// byte at a time turns every multi-byte UTF-8 sequence into mojibake,
    /// and it survives any fixture written in ASCII. A real derivation
    /// carrying "Sørensen" came back as "SÃ¸rensen".
    #[test]
    fn a_multibyte_utf8_string_survives_a_round_trip() {
        let text = r#"Derive([],[],[],"s","/bin/sh",["Sørensen–Dice … 日本語"],[])"#;
        assert_eq!(round(text, false), text);
        assert_eq!(
            parse(text).map(|d| d.args),
            Ok(vec!["Sørensen–Dice … 日本語".to_owned()])
        );
    }

    #[test]
    fn the_four_escapes_survive_a_round_trip() {
        let text = r#"Derive([],[],[],"s","/bin/sh",["a\"b\\c\nd\re\tf"],[])"#;
        assert_eq!(round(text, false), text);
        assert_eq!(
            parse(text).map(|d| d.args),
            Ok(vec!["a\"b\\c\nd\re\tf".to_owned()])
        );
    }

    /// Refused by name rather than mis-parsed: its `inputDrvs` is a recursive
    /// map, so reading it with the flat grammar would silently lose structure.
    #[test]
    fn a_dynamic_derivation_is_refused_by_name() {
        let text = r#"DrvWithVersion("xp-dyn-drv",[],[],[],"s","/bin/sh",[],[])"#;
        assert_eq!(parse(text), Err(DrvError::DynamicDerivations));
    }

    /// A truncated file must report where it stopped, never panic. Every read
    /// in the cursor is bounds-checked for exactly this; the loop is the
    /// assertion, since a missing check shows up as an abort rather than a
    /// failed comparison.
    #[test]
    fn a_truncated_derivation_reports_an_offset_instead_of_panicking() {
        for cut in 0..SAMPLE.len() {
            if let Some(prefix) = SAMPLE.get(..cut) {
                let _ = parse(prefix);
            }
        }
        assert!(matches!(
            parse("Derive([("),
            Err(DrvError::Malformed { .. })
        ));
        assert!(matches!(parse("nonsense"), Err(DrvError::Malformed { .. })));
    }

    #[test]
    fn an_empty_list_and_an_empty_string_are_distinguishable() {
        let text = r#"Derive([],[],[],"","",[],[])"#;
        assert_eq!(round(text, false), text);
    }

    /// The guard added because the round trip could not see it: a valid
    /// derivation followed by anything at all used to parse, and the extra
    /// bytes surfaced only as a shorter re-render. Watched failing on real
    /// bytes too -- appending one byte to a store `.drv` now rejects.
    #[test]
    fn trailing_bytes_after_the_closing_paren_are_refused() {
        assert!(parse(SAMPLE).is_ok());
        for suffix in ["\n", " ", ")", "Derive([],[],[],\"\",\"\",[],[])"] {
            let with_suffix = format!("{SAMPLE}{suffix}");
            assert!(
                matches!(parse(&with_suffix), Err(DrvError::Malformed { .. })),
                "accepted trailing {suffix:?}"
            );
        }
    }

    /// The five kinds are told apart by which fields are empty, and a shape
    /// cppnix cannot write is reported rather than bucketed as the closest
    /// match. `CaFloating` and `Impure` differ only in the last field, which
    /// is the pair a decision table is most likely to conflate.
    #[test]
    fn the_five_output_kinds_are_told_apart_by_which_fields_are_empty() {
        let out = |path: &str, algo: &str, hash: &str| Output {
            name: "out".to_owned(),
            path: path.to_owned(),
            hash_algo: algo.to_owned(),
            hash: hash.to_owned(),
        };
        assert_eq!(
            out("/nix/store/a-x", "", "").kind(),
            OutputKind::InputAddressed
        );
        assert_eq!(
            out("/nix/store/a-x", "r:sha256", "abc").kind(),
            OutputKind::CaFixed
        );
        assert_eq!(out("", "r:sha256", "").kind(), OutputKind::CaFloating);
        assert_eq!(out("", "", "").kind(), OutputKind::Deferred);
        assert_eq!(out("", "r:sha256", "impure").kind(), OutputKind::Impure);
        // A path together with a hash algorithm but no hash is none of the
        // five; saying so beats guessing `CaFixed`.
        assert_eq!(
            out("/nix/store/a-x", "r:sha256", "").kind(),
            OutputKind::Unrecognised
        );
        assert_eq!(
            out("/nix/store/a-x", "", "abc").kind(),
            OutputKind::Unrecognised
        );
    }

    /// `canonicalise` is the rule a *constructed* derivation has to follow,
    /// and this is the guard watched failing: the same derivation with two
    /// fields permuted is byte-identical after a round trip and is rejected
    /// here, which is exactly the divergence the store-wide round trip is
    /// structurally unable to report.
    #[test]
    fn canonicalise_orders_every_map_field_and_leaves_args_alone() {
        let scrambled = Derivation {
            outputs: vec![
                Output {
                    name: "lib".to_owned(),
                    path: "/nix/store/b-x-lib".to_owned(),
                    hash_algo: String::new(),
                    hash: String::new(),
                },
                Output {
                    name: "dev".to_owned(),
                    path: "/nix/store/a-x-dev".to_owned(),
                    hash_algo: String::new(),
                    hash: String::new(),
                },
            ],
            input_drvs: vec![
                InputDrv {
                    drv_path: "/nix/store/zzz.drv".to_owned(),
                    outputs: vec!["out".to_owned(), "dev".to_owned()],
                },
                InputDrv {
                    drv_path: "/nix/store/aaa.drv".to_owned(),
                    outputs: vec!["out".to_owned()],
                },
            ],
            input_srcs: vec!["/nix/store/z-s".to_owned(), "/nix/store/a-s".to_owned()],
            platform: "x86_64-linux".to_owned(),
            builder: "/bin/sh".to_owned(),
            // Not a map in cppnix: a builder's argv order is meaning, and
            // sorting it would be a silent miscompilation of the derivation.
            args: vec!["zzz".to_owned(), "aaa".to_owned()],
            env: vec![
                EnvVar {
                    name: "system".to_owned(),
                    value: "x86_64-linux".to_owned(),
                },
                EnvVar {
                    name: "builder".to_owned(),
                    value: "/bin/sh".to_owned(),
                },
            ],
        };

        // The guard fires on the unsorted input. Round-tripping the same
        // derivation does not, which is the point.
        assert!(
            !is_canonical(&scrambled),
            "guard did not fire on scrambled input"
        );
        let text = unparse(&scrambled, false);
        assert_eq!(
            round(&text, false),
            text,
            "round trip should be blind to order"
        );

        let mut sorted = scrambled.clone();
        canonicalise(&mut sorted);
        assert!(is_canonical(&sorted));
        assert_eq!(
            sorted
                .outputs
                .iter()
                .map(|o| o.name.as_str())
                .collect::<Vec<_>>(),
            ["dev", "lib"]
        );
        assert_eq!(
            sorted
                .input_drvs
                .iter()
                .map(|i| i.drv_path.as_str())
                .collect::<Vec<_>>(),
            ["/nix/store/aaa.drv", "/nix/store/zzz.drv"]
        );
        assert_eq!(
            sorted.input_drvs.first().map(|i| i.outputs.clone()),
            Some(vec!["out".to_owned()])
        );
        assert_eq!(
            sorted.input_drvs.last().map(|i| i.outputs.clone()),
            Some(vec!["dev".to_owned(), "out".to_owned()])
        );
        assert_eq!(sorted.input_srcs, ["/nix/store/a-s", "/nix/store/z-s"]);
        assert_eq!(
            sorted
                .env
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["builder", "system"]
        );
        assert_eq!(
            sorted.args,
            ["zzz", "aaa"],
            "argv order is meaning, not a set"
        );
    }

    /// The sample is written in cppnix's order, so the rule agrees with a
    /// derivation nobody sorted on purpose.
    #[test]
    fn a_derivation_written_the_way_cppnix_writes_it_is_already_canonical() {
        // Reported as a failed assertion rather than a panic: the workspace
        // denies `panic`, `unwrap` and `expect`, in tests too.
        assert_eq!(parse(SAMPLE).as_ref().map(is_canonical), Ok(true));
    }
}
