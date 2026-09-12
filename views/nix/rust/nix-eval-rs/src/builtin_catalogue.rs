//! Owned language catalogue: global spellings, capabilities and documentation.
//! Implementations live in `builtins::TABLE`; no external registry controls availability.

/// Features that change which implemented builtins are in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinFeatures {
    pub flakes: bool,
    pub fetch_tree: bool,
    pub wasm: bool,
}

impl BuiltinFeatures {
    pub const ALL: Self = Self {
        flakes: true,
        fetch_tree: true,
        wasm: true,
    };
    pub const NONE: Self = Self {
        flakes: false,
        fetch_tree: false,
        wasm: false,
    };
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !7 != 0 {
            return None;
        }
        Some(Self {
            flakes: bits & 1 != 0,
            fetch_tree: bits & 2 != 0,
            wasm: bits & 4 != 0,
        })
    }
    pub const fn bits(self) -> u32 {
        self.flakes as u32 | ((self.fetch_tree as u32) << 1) | ((self.wasm as u32) << 2)
    }
}

impl Default for BuiltinFeatures {
    fn default() -> Self {
        Self::ALL
    }
}

/// Registered global spellings. Public attrset spellings omit the `__` prefix.
pub static GLOBAL_NAMES: &[&str] = &[
    "__add",
    "__addDrvOutputDependencies",
    "__addErrorContext",
    "__all",
    "__any",
    "__appendContext",
    "__attrNames",
    "__attrValues",
    "__bitAnd",
    "__bitOr",
    "__bitXor",
    "__catAttrs",
    "__ceil",
    "__compareVersions",
    "__concatLists",
    "__concatMap",
    "__concatStringsSep",
    "__convertHash",
    "__deepSeq",
    "__div",
    "__elem",
    "__elemAt",
    "__fetchurl",
    "__filter",
    "__filterSource",
    "__findFile",
    "__floor",
    "__foldl'",
    "__fromJSON",
    "__functionArgs",
    "__genericClosure",
    "__genList",
    "__getAttr",
    "__getContext",
    "__getEnv",
    "__groupBy",
    "__hasAttr",
    "__hasContext",
    "__hashFile",
    "__hashString",
    "__head",
    "__intersectAttrs",
    "__isAttrs",
    "__isBool",
    "__isFloat",
    "__isFunction",
    "__isInt",
    "__isList",
    "__isPath",
    "__isString",
    "__length",
    "__lessThan",
    "__listToAttrs",
    "__mapAttrs",
    "__match",
    "__mul",
    "__parseDrvName",
    "__partition",
    "__path",
    "__pathExists",
    "__readDir",
    "__readFile",
    "__readFileType",
    "__replaceStrings",
    "__seq",
    "__sort",
    "__split",
    "__splitVersion",
    "__storePath",
    "__stringLength",
    "__sub",
    "__substring",
    "__tail",
    "__toFile",
    "__toJSON",
    "__toPath",
    "__toXML",
    "__trace",
    "__traceVerbose",
    "__tryEval",
    "__typeOf",
    "__unsafeDiscardOutputDependency",
    "__unsafeDiscardStringContext",
    "__unsafeGetAttrPos",
    "__warn",
    "__wasm",
    "__zipAttrsWith",
    "abort",
    "baseNameOf",
    "derivationStrict",
    "dirOf",
    "fetchFinalTree",
    "fetchGit",
    "fetchTarball",
    "fetchTree",
    "fromTOML",
    "import",
    "isNull",
    "map",
    "placeholder",
    "removeAttrs",
    "throw",
    "toString",
];

pub static EXTRA_MEMBERS: &[&str] = &[
    "currentSystem",
    "derivation",
    "langVersion",
    "nixPath",
    "nixVersion",
    "storeDir",
];
pub static EXTRA_GLOBALS: &[&str] = &["__nixPath", "derivation"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gate {
    Flakes,
    FetchTree,
    Wasm,
    Never,
}

pub fn gate_of(name: &str) -> Option<Gate> {
    match name.strip_prefix("__").unwrap_or(name) {
        "getFlake" | "parseFlakeRef" | "flakeRefToString" => Some(Gate::Flakes),
        "fetchTree" => Some(Gate::FetchTree),
        "wasm" => Some(Gate::Wasm),
        "fetchFinalTree" => Some(Gate::Never),
        _ => None,
    }
}

pub(crate) fn enabled(settings: &crate::eval::Settings, name: &str) -> bool {
    let member = name.strip_prefix("__").unwrap_or(name);
    if settings.pure_eval && member == "currentSystem" {
        return false;
    }
    match gate_of(member) {
        None => true,
        Some(Gate::Never) => false,
        Some(Gate::Flakes) => settings.builtin_features.flakes,
        Some(Gate::FetchTree) => {
            settings.builtin_features.flakes || settings.builtin_features.fetch_tree
        }
        Some(Gate::Wasm) => settings.builtin_features.wasm,
    }
}

/// All implemented language entries, including feature-gated entries with their requirement.
pub fn language_docs() -> Result<String, serde_json::Error> {
    let mut docs: serde_json::Map<String, serde_json::Value> = serde_json::from_str(DOCUMENTATION)?;
    let settings = crate::eval::Settings::default();
    docs.retain(|name, _| {
        crate::builtins::set_member_names(&settings).any(|member| member == name)
    });
    for (name, entry) in &mut docs {
        let feature = match gate_of(name) {
            Some(Gate::Flakes) => Some("flakes"),
            Some(Gate::FetchTree) => Some("fetch-tree"),
            Some(Gate::Wasm) => Some("wasm-builtin"),
            _ => None,
        };
        if let (Some(feature), Some(fields)) = (feature, entry.as_object_mut()) {
            fields.insert("experimental-feature".into(), feature.into());
        }
    }
    serde_json::to_string(&docs)
}

const DOCUMENTATION: &str = r###"{
  "abort": {
    "args": [
      "s"
    ],
    "doc": "Abort Nix expression evaluation and print the error message *s*."
  },
  "add": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the sum of the numbers *e1* and *e2*."
  },
  "addDrvOutputDependencies": {
    "args": [
      "s"
    ],
    "doc": "Create a copy of the given string where a single\n[constant](@docroot@/language/string-context.md#string-context-constant)\nstring context element is turned into a\n[derivation deep](@docroot@/language/string-context.md#string-context-element-derivation-deep)\nstring context element.\n\nThe store path that is the constant string context element should point to a valid derivation, and end in `.drv`.\n\nThe original string context element must not be empty or have multiple elements, and it must not have any other type of element other than a constant or derivation deep element.\nThe latter is supported so this function is idempotent.\n\nThis is the opposite of [`builtins.unsafeDiscardOutputDependency`](#builtins-unsafeDiscardOutputDependency)."
  },
  "all": {
    "args": [
      "pred",
      "list"
    ],
    "doc": "Return `true` if the function *pred* returns `true` for all elements\nof *list*, and `false` otherwise."
  },
  "any": {
    "args": [
      "pred",
      "list"
    ],
    "doc": "Return `true` if the function *pred* returns `true` for at least one\nelement of *list*, and `false` otherwise."
  },
  "attrNames": {
    "args": [
      "set"
    ],
    "doc": "Return the names of the attributes in the set *set* in an\nalphabetically sorted list. For instance, `builtins.attrNames { y\n= 1; x = \"foo\"; }` evaluates to `[ \"x\" \"y\" ]`."
  },
  "attrValues": {
    "args": [
      "set"
    ],
    "doc": "Return the values of the attributes in the set *set* in the order\ncorresponding to the sorted attribute names."
  },
  "baseNameOf": {
    "args": [
      "x"
    ],
    "doc": "Return the *base name* of either a [path value](@docroot@/language/types.md#type-path) *x* or a string *x*, depending on which type is passed, and according to the following rules.\n\nFor a path value, the *base name* is considered to be the part of the path after the last directory separator, including any file extensions.\nThis is the simple case, as path values don't have trailing slashes.\n\nWhen the argument is a string, a more involved logic applies. If the string ends with a `/`, only this one final slash is removed.\n\nAfter this, the *base name* is returned as previously described, assuming `/` as the directory separator. (Note that evaluation must be platform independent.)\n\nThis is somewhat similar to the [GNU `basename`](https://www.gnu.org/software/coreutils/manual/html_node/basename-invocation.html) command, but GNU `basename` strips any number of trailing slashes."
  },
  "bitAnd": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the bitwise AND of the integers *e1* and *e2*."
  },
  "bitOr": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the bitwise OR of the integers *e1* and *e2*."
  },
  "bitXor": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the bitwise XOR of the integers *e1* and *e2*."
  },
  "builtins": {
    "type": "Set",
    "doc": "The set of language builtins available under the active settings."
  },
  "catAttrs": {
    "args": [
      "attr",
      "list"
    ],
    "doc": "Collect each attribute named *attr* from a list of attribute\nsets.  Attrsets that don't contain the named attribute are\nignored. For example,\n\n```nix\nbuiltins.catAttrs \"a\" [{a = 1;} {b = 0;} {a = 2;}]\n```\n\nevaluates to `[1 2]`."
  },
  "ceil": {
    "args": [
      "number"
    ],
    "doc": "Rounds and converts *number* to the next higher NixInt value if possible, i.e. `ceil *number* >= *number*` and\n`ceil *number* - *number* < 1`.\n\nAn evaluation error is thrown, if there exists no such NixInt value `ceil *number*`.\nDue to bugs in previous Nix versions an evaluation error might be thrown, if the datatype of *number* is\na NixInt and if `*number* < -9007199254740992` or `*number* > 9007199254740992`.\n\nIf the datatype of *number* is neither a NixInt (signed 64-bit integer) nor a NixFloat\n(IEEE-754 double-precision floating-point number), an evaluation error is thrown."
  },
  "compareVersions": {
    "args": [
      "s1",
      "s2"
    ],
    "doc": "Compare two strings representing versions and return `-1` if\nversion *s1* is older than version *s2*, `0` if they are the same,\nand `1` if *s1* is newer than *s2*. The version comparison\nalgorithm is the same as the one used by [`nix-env\n-u`](../command-ref/nix-env/upgrade.md)."
  },
  "concatLists": {
    "args": [
      "lists"
    ],
    "doc": "Concatenate a list of lists into a single list."
  },
  "concatMap": {
    "args": [
      "f",
      "list"
    ],
    "doc": "This function is equivalent to `builtins.concatLists (map f list)`\nbut is more efficient."
  },
  "concatStringsSep": {
    "args": [
      "separator",
      "list"
    ],
    "doc": "Concatenate a list of strings with a separator between each\nelement, e.g. `concatStringsSep \"/\" [\"usr\" \"local\" \"bin\"] ==\n\"usr/local/bin\"`."
  },
  "convertHash": {
    "args": [
      "args"
    ],
    "doc": "Return the specified representation of a hash string, based on the attributes presented in *args*:\n\n- `hash`\n\n  The hash to be converted.\n  The hash format is detected automatically.\n\n- `hashAlgo`\n\n  The algorithm used to create the hash. Must be one of\n  - `\"md5\"`\n  - `\"sha1\"`\n  - `\"sha256\"`\n  - `\"sha512\"`\n\n  The attribute may be omitted when `hash` is an [SRI hash](https://www.w3.org/TR/SRI/#the-integrity-attribute) or when the hash is prefixed with the hash algorithm name followed by a colon.\n  That `<hashAlgo>:<hashBody>` syntax is supported for backwards compatibility with existing tooling.\n\n- `toHashFormat`\n\n  The format of the resulting hash. Must be one of\n  - `\"base16\"`\n  - `\"nix32\"`\n  - `\"base32\"` (deprecated alias for `\"nix32\"`)\n  - `\"base64\"`\n  - `\"sri\"`\n\nThe result hash is the *toHashFormat* representation of the hash *hash*.\n\n> **Example**\n>\n>   Convert a SHA256 hash in Base16 to SRI:\n>\n> ```nix\n> builtins.convertHash {\n>   hash = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\";\n>   toHashFormat = \"sri\";\n>   hashAlgo = \"sha256\";\n> }\n> ```\n>\n>     \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\"\n\n> **Example**\n>\n>   Convert a SHA256 hash in SRI to Base16:\n>\n> ```nix\n> builtins.convertHash {\n>   hash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";\n>   toHashFormat = \"base16\";\n> }\n> ```\n>\n>     \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\"\n\n> **Example**\n>\n>   Convert a hash in the form `<hashAlgo>:<hashBody>` in Base16 to SRI:\n>\n> ```nix\n> builtins.convertHash {\n>   hash = \"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\";\n>   toHashFormat = \"sri\";\n> }\n> ```\n>\n>     \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\""
  },
  "currentSystem": {
    "doc": "The value of the\n[`eval-system`](@docroot@/command-ref/conf-file.md#conf-eval-system)\nor else\n[`system`](@docroot@/command-ref/conf-file.md#conf-system)\nconfiguration option.\n\nIt can be used to set the `system` attribute for [`builtins.derivation`](@docroot@/language/derivations.md) such that the resulting derivation can be built on the same system that evaluates the Nix expression:\n\n```nix\n builtins.derivation {\n   # ...\n   system = builtins.currentSystem;\n}\n```\n\nIt can be overridden in order to create derivations for different system than the current one:\n\n```console\n$ nix-instantiate --system \"mips64-linux\" --eval --expr 'builtins.currentSystem'\n\"mips64-linux\"\n```",
    "impure-only": true,
    "type": "string"
  },
  "deepSeq": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "This is like `seq e1 e2`, except that *e1* is evaluated *deeply*:\nif it’s a list or set, its elements or attributes are also\nevaluated recursively."
  },
  "dirOf": {
    "args": [
      "s"
    ],
    "doc": "Return the directory part of the string *s*, that is, everything\nbefore the final slash in the string. This is similar to the GNU\n`dirname` command."
  },
  "div": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the quotient of the numbers *e1* and *e2*."
  },
  "elem": {
    "args": [
      "x",
      "xs"
    ],
    "doc": "Return `true` if a value equal to *x* occurs in the list *xs*, and\n`false` otherwise."
  },
  "elemAt": {
    "args": [
      "xs",
      "n"
    ],
    "doc": "Return element *n* from the list *xs*. Elements are counted starting\nfrom 0. A fatal error occurs if the index is out of bounds."
  },
  "false": {
    "doc": "Primitive value.\n\nIt can be returned by\n[comparison operators](@docroot@/language/operators.md#comparison)\nand used in\n[conditional expressions](@docroot@/language/syntax.md#conditionals).\n\nThe name `false` is not special, and can be shadowed:\n\n```nix-repl\nnix-repl> let false = 1; in false\n1\n```",
    "type": "Boolean"
  },
  "fetchGit": {
    "args": [
      "args"
    ],
    "doc": "Fetch a path from git. *args* can be a URL, in which case the HEAD\nof the repo at that URL is fetched. Otherwise, it can be an\nattribute with the following attributes (all except `url` optional):\n\n- `url`\n\n  The URL of the repo.\n\n- `name` (default: `source`)\n\n  The name of the directory the repo should be exported to in the store.\n\n- `rev` (default: *the tip of `ref`*)\n\n  The [Git revision] to fetch.\n  This is typically a commit hash.\n\n  [Git revision]: https://git-scm.com/docs/git-rev-parse#_specifying_revisions\n\n- `ref` (default: `HEAD`)\n\n  The [Git reference] under which to look for the requested revision.\n  This is often a branch or tag name.\n\n  [Git reference]: https://git-scm.com/book/en/v2/Git-Internals-Git-References\n\n  This option has no effect once `shallow` cloning is enabled.\n\n  By default, the `ref` value is prefixed with `refs/heads/`.\n  As of 2.3.0, Nix doesn't prefix `refs/heads/` if `ref` starts with `refs/`.\n\n- `submodules` (default: `false`)\n\n  A Boolean parameter that specifies whether submodules should be checked out.\n\n- `exportIgnore` (default: `true`)\n\n  A Boolean parameter that specifies whether `export-ignore` from `.gitattributes` should be applied.\n  This approximates part of the `git archive` behavior.\n\n  Enabling this option is not recommended because it is unknown whether the Git developers commit to the reproducibility of `export-ignore` in newer Git versions.\n\n- `shallow` (default: `false`)\n\n  Make a shallow clone when fetching the Git tree.\n  When this is enabled, the options `ref` and `allRefs` have no effect anymore.\n\n- `lfs` (default: `false`)\n\n  A boolean that when `true` specifies that [Git LFS] files should be fetched.\n\n  [Git LFS]: https://git-lfs.com/\n\n- `allRefs`\n\n  Whether to fetch all references (eg. branches and tags) of the repository.\n  With this argument being true, it's possible to load a `rev` from *any* `ref`.\n  (by default only `rev`s from the specified `ref` are supported).\n\n  This option has no effect once `shallow` cloning is enabled.\n\n- `exportHistory` (default: `false`)\n\n  Export the commit history of the fetched revision as a `history`\n  attribute on the result: a list of commits, newest first, in a\n  deterministic topological order. Each commit is an attribute set\n  with `rev`, `parents`, `author`, `committer` (each of the latter\n  two with `name`, `email`, `time`, `tzOffset`), `message`, and --\n  unless `historyPaths = false` -- `paths` (the files touched\n  relative to the first parent, each with `path` and a Git-style\n  `status` letter such as `\"A\"`, `\"M\"`, `\"D\"` or `\"T\"`).\n\n  The history is a pure function of `rev`, so it never appears in\n  lock files; it is cached in the fetcher cache. It cannot be\n  combined with `shallow = true`.\n\n  Requires the [`git-export-history` experimental feature](@docroot@/development/experimental-features.md#xp-feature-git-export-history).\n\n- `historyDepth` (default: `100`)\n\n  Bound the exported history to at most this many commits, selected\n  level by level outward from the fetched revision (nearest commits\n  first; ties are broken by commit hash, so the set is\n  deterministic). `0` means unlimited. Only meaningful together\n  with `exportHistory`.\n\n- `historyPaths` (default: `true`)\n\n  Whether the exported history includes the `paths` touched by each\n  commit. Computing `paths` requires a tree diff per commit, which\n  dominates the export cost on repositories with large trees (for\n  example nixpkgs); set this to `false` for cheap metadata-only\n  history. Only meaningful together with `exportHistory`.\n\n- `verifyCommit` (default: `true` if `publicKey` or `publicKeys` are provided, otherwise `false`)\n\n  Whether to check `rev` for a signature matching `publicKey` or `publicKeys`.\n  Requires the [`verified-fetches` experimental feature](@docroot@/development/experimental-features.md#xp-feature-verified-fetches).\n\n- `publicKey`\n\n  The public key against which `rev` is verified if `verifyCommit` is enabled.\n  Requires the [`verified-fetches` experimental feature](@docroot@/development/experimental-features.md#xp-feature-verified-fetches).\n\n- `keytype` (default: `\"ssh-ed25519\"`)\n\n  The key type of `publicKey`.\n  Possible values:\n  - `\"ssh-dsa\"`\n  - `\"ssh-ecdsa\"`\n  - `\"ssh-ecdsa-sk\"`\n  - `\"ssh-ed25519\"`\n  - `\"ssh-ed25519-sk\"`\n  - `\"ssh-rsa\"`\n  Requires the [`verified-fetches` experimental feature](@docroot@/development/experimental-features.md#xp-feature-verified-fetches).\n\n- `publicKeys`\n\n  The public keys against which `rev` is verified if `verifyCommit` is enabled.\n  Must be given as a list of attribute sets with the following form:\n\n  ```nix\n  {\n    key = \"<public key>\";\n    type = \"<key type>\"; # optional, default: \"ssh-ed25519\"\n  }\n  ```\n\n  Requires the [`verified-fetches` experimental feature](@docroot@/development/experimental-features.md#xp-feature-verified-fetches).\n\n\nHere are some examples of how to use `fetchGit`.\n\n  - To fetch a private repository over SSH:\n\n    ```nix\n    builtins.fetchGit {\n      url = \"git@github.com:my-secret/repository.git\";\n      ref = \"master\";\n      rev = \"adab8b916a45068c044658c4158d81878f9ed1c3\";\n    }\n    ```\n\n  - To fetch an arbitrary reference:\n\n    ```nix\n    builtins.fetchGit {\n      url = \"https://github.com/NixOS/nix.git\";\n      ref = \"refs/heads/0.5-release\";\n    }\n    ```\n\n  - If the revision you're looking for is in the default branch of\n    the git repository you don't strictly need to specify the branch\n    name in the `ref` attribute.\n\n    However, if the revision you're looking for is in a future\n    branch for the non-default branch you will need to specify the\n    the `ref` attribute as well.\n\n    ```nix\n    builtins.fetchGit {\n      url = \"https://github.com/nixos/nix.git\";\n      rev = \"841fcbd04755c7a2865c51c1e2d3b045976b7452\";\n      ref = \"1.11-maintenance\";\n    }\n    ```\n\n    > **Note**\n    >\n    > It is nice to always specify the branch which a revision\n    > belongs to. Without the branch being specified, the fetcher\n    > might fail if the default branch changes. Additionally, it can\n    > be confusing to try a commit from a non-default branch and see\n    > the fetch fail. If the branch is specified the fault is much\n    > more obvious.\n\n  - If the revision you're looking for is in the default branch of\n    the git repository you may omit the `ref` attribute.\n\n    ```nix\n    builtins.fetchGit {\n      url = \"https://github.com/nixos/nix.git\";\n      rev = \"841fcbd04755c7a2865c51c1e2d3b045976b7452\";\n    }\n    ```\n\n  - To fetch a specific tag:\n\n    ```nix\n    builtins.fetchGit {\n      url = \"https://github.com/nixos/nix.git\";\n      ref = \"refs/tags/1.9\";\n    }\n    ```\n\n  - To fetch the latest version of a remote branch:\n\n    ```nix\n    builtins.fetchGit {\n      url = \"ssh://git@github.com/nixos/nix.git\";\n      ref = \"master\";\n    }\n    ```\n\n  - To verify the commit signature:\n\n    ```nix\n    builtins.fetchGit {\n      url = \"ssh://git@github.com/nixos/nix.git\";\n      verifyCommit = true;\n      publicKeys = [\n          {\n            type = \"ssh-ed25519\";\n            key = \"AAAAC3NzaC1lZDI1NTE5AAAAIArPKULJOid8eS6XETwUjO48/HKBWl7FTCK0Z//fplDi\";\n          }\n      ];\n    }\n    ```\n\n    Nix refetches the branch according to the [`tarball-ttl`](@docroot@/command-ref/conf-file.md#conf-tarball-ttl) setting.\n\n    This behavior is disabled in [pure evaluation mode](@docroot@/command-ref/conf-file.md#conf-pure-eval).\n\n  - To fetch the commit a local repository has checked out:\n\n    ```nix\n    builtins.fetchGit ./work-dir\n    ```\n\nIf the URL points to a local directory, and no `ref` or `rev` is\ngiven, `fetchGit` fetches the commit that directory has checked\nout, exactly as if that commit had been passed as `rev`. The\nworking tree itself is never the source: only a commit has an\nidentity that the result can be locked to, and the files a build\nsees are the ones in that commit, not the ones on disk.\n\nA working tree with uncommitted changes to tracked files therefore\ncannot be fetched at all; `fetchGit` reports which files differ.\nCommit them, or pass the `rev` you mean. Untracked files are not\nchanges: they belong to no commit, so they neither block the fetch\nnor appear in the result."
  },
  "fetchTarball": {
    "args": [
      "args"
    ],
    "doc": "Download the specified URL, unpack it and return the path of the\nunpacked tree. The file must be a tape archive (`.tar`) compressed\nwith `gzip`, `bzip2` or `xz`. If the tarball consists of a\nsingle directory, then the top-level path component of the files\nin the tarball is removed. The typical use of the function is to\nobtain external Nix expression dependencies, such as a\nparticular version of Nixpkgs, e.g.\n\n```nix\nwith import (fetchTarball https://github.com/NixOS/nixpkgs/archive/nixos-14.12.tar.gz) {};\n\nstdenv.mkDerivation { … }\n```\n\nThe fetched tarball is cached for a certain amount of time (1\nhour by default) in `~/.cache/nix/tarballs/`. You can change the\ncache timeout either on the command line with `--tarball-ttl`\n*number-of-seconds* or in the Nix configuration file by adding\nthe line `tarball-ttl = ` *number-of-seconds*.\n\nNote that when obtaining the hash with `nix-prefetch-url` the\noption `--unpack` is required.\n\nThis function can also verify the contents against a hash. In that\ncase, the function takes a set instead of a URL. The set requires\nthe attribute `url` and the attribute `sha256`, e.g.\n\n```nix\nwith import (fetchTarball {\n  url = \"https://github.com/NixOS/nixpkgs/archive/nixos-14.12.tar.gz\";\n  sha256 = \"1jppksrfvbk5ypiqdz4cddxdl8z6zyzdb2srq8fcffr327ld5jj2\";\n}) {};\n\nstdenv.mkDerivation { … }\n```\n\nNot available in [restricted evaluation mode](@docroot@/command-ref/conf-file.md#conf-restrict-eval)."
  },
  "fetchTree": {
    "args": [
      "input"
    ],
    "doc": "Fetch a file system tree or a plain file using one of the supported backends and return an attribute set with:\n\n- the resulting fixed-output [store path](@docroot@/store/store-path.md)\n- the corresponding [NAR](@docroot@/store/file-system-object/content-address.md#serial-nix-archive) hash\n- backend-specific metadata (currently not documented). <!-- TODO: document output attributes -->\n\n*input* must be an attribute set with the following attributes:\n\n- `type` (String, required)\n\n  One of the [supported source types](#source-types).\n  This determines other required and allowed input attributes.\n\n- `narHash` (String, optional)\n\n  The `narHash` parameter can be used to substitute the source of the tree.\n  It also allows for verification of tree contents that may not be provided by the underlying transfer mechanism.\n  If `narHash` is set, the source is first looked up is the Nix store and [substituters](@docroot@/command-ref/conf-file.md#conf-substituters), and only fetched if not available.\n\n- `treeHash` (String, optional)\n\n  For `jj` sources, the BLAKE3 Jujutsu tree id of the root tree, in SRI form (`blake3-...`); it takes the place of `narHash`.\n  If `treeHash` is set and the store object it names is already valid in the Nix store, the source is served from the store without the repository; substituters are not consulted.\n  Otherwise the repository is read and the id it reports must match.\n\nA subset of the output attributes of `fetchTree` can be re-used for subsequent calls to `fetchTree` to produce the same result again.\nThat is, `fetchTree` is idempotent.\n\nDownloads are cached in `$XDG_CACHE_HOME/nix`.\nThe remote source is fetched from the network if both are true:\n- A NAR hash is supplied and the corresponding store path is not [valid](@docroot@/glossary.md#gloss-validity), that is, not available in the store\n\n  > **Note**\n  >\n  > [Substituters](@docroot@/command-ref/conf-file.md#conf-substituters) are not used in fetching.\n\n- There is no cache entry or the cache entry is older than [`tarball-ttl`](@docroot@/command-ref/conf-file.md#conf-tarball-ttl)\n\n## Source types\n\nThe following source types and associated input attributes are supported.\n\n<!-- TODO: It would be soooo much more predictable to work with (and\ndocument) if `fetchTree` was a curried call with the first parameter for\n`type` or an attribute like `builtins.fetchTree.git`! -->\n\n\n- `\"file\"`\n\n  \n  Place a plain file into the Nix store.\n  This is similar to [`builtins.fetchurl`](@docroot@/language/builtins.md#builtins-fetchurl)\n  \n\n  - `lastModified` (String, required)\n\n\n  - `name` (String, required)\n\n\n  - `narHash` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `revCount` (String, required)\n\n\n  - `unpack` (String, required)\n\n\n  - `url` (String, required)\n\n    \n    Supported protocols:\n    \n    - `https`\n    \n      > **Example**\n      >\n      > ```nix\n      > fetchTree {\n      >   type = \"file\";\n      >   url = \"https://example.com/index.html\";\n      > }\n      > ```\n    \n    - `http`\n    \n      Insecure HTTP transfer for legacy sources.\n    \n      > **Warning**\n      >\n      > HTTP performs no encryption or authentication.\n      > Use a `narHash` known in advance to ensure the output has expected contents.\n    \n    - `file`\n    \n      A file on the local file system.\n    \n      > **Example**\n      >\n      > ```nix\n      > fetchTree {\n      >   type = \"file\";\n      >   url = \"file:///home/eelco/nix/README.md\";\n      > }\n      > ```\n    \n\n- `\"git\"`\n\n  \n  Fetch a Git tree and copy it to the Nix store.\n  This is similar to [`builtins.fetchGit`](@docroot@/language/builtins.md#builtins-fetchGit).\n  \n\n  - `allRefs` (Bool, optional)\n\n    \n    By default, this has no effect. This becomes relevant only once `shallow` cloning is disabled.\n    \n    Whether to fetch all references (eg. branches and tags) of the repository.\n    With this argument being true, it's possible to load a `rev` from *any* `ref`.\n    (Without setting this option, only `rev`s from the specified `ref` are supported).\n    \n    Default: `false`\n    \n\n  - `exportHistory` (Bool, optional)\n\n    \n    Export the commit history of the fetched revision.\n    \n    When enabled, the resulting attribute set gains a\n    `history` attribute: a list of commits, newest first, in\n    a deterministic topological order. Each commit is an\n    attribute set with `rev`, `parents`, `author`,\n    `committer` (each with `name`, `email`, `time`,\n    `tzOffset`), `message`, and -- unless `historyPaths =\n    false` -- `paths` (the files touched relative to the\n    first parent, each with `path` and a Git-style `status`\n    letter).\n    \n    The history is derived entirely from the fetched\n    revision, so it does not appear in lock files (like\n    `revCount`, it is a pure function of `rev`). It cannot\n    be combined with `shallow = true`.\n    \n    Requires the `git-export-history` experimental feature.\n    \n    Default: `false`\n    \n\n  - `exportIgnore` (String, required)\n\n\n  - `historyDepth` (Integer, optional)\n\n    \n    Bound the history exported by `exportHistory` to at most\n    this many commits, selected level by level outward from\n    the fetched revision (nearest commits first; determinism\n    ties are broken by commit hash). `0` means unlimited.\n    `parents` fields may name commits outside the exported\n    set.\n    \n    Only meaningful together with `exportHistory = true`.\n    \n    Default: `100`\n    \n\n  - `historyPaths` (Bool, optional)\n\n    \n    Whether the history exported by `exportHistory` includes\n    the `paths` touched by each commit. Path extraction is a\n    tree diff per commit and dominates the export cost on\n    repositories with large trees (for example nixpkgs);\n    set this to `false` for cheap metadata-only history\n    (`rev`, `parents`, `author`, `committer`, `message`).\n    \n    Only meaningful together with `exportHistory = true`.\n    \n    Default: `true`\n    \n\n  - `keytype` (String, required)\n\n\n  - `lastModified` (Integer, optional)\n\n    \n    Unix timestamp of the fetched commit.\n    \n    If set, pass through the value to the output attribute set.\n    Otherwise, generated from the fetched Git tree.\n    \n\n  - `lfs` (Bool, optional)\n\n    \n    Fetch any [Git LFS](https://git-lfs.com/) files.\n    \n    Default: `false`\n    \n\n  - `name` (String, required)\n\n\n  - `narHash` (String, required)\n\n\n  - `publicKey` (String, required)\n\n\n  - `publicKeys` (String, required)\n\n\n  - `ref` (String, optional)\n\n    \n    By default, this has no effect. This becomes relevant only once `shallow` cloning is disabled.\n    \n    A [Git reference](https://git-scm.com/book/en/v2/Git-Internals-Git-References), such as a branch or tag name.\n    \n    Default: `\"HEAD\"`\n    \n\n  - `rev` (String, optional)\n\n    \n    A Git revision; a commit hash.\n    \n    Default: the tip of `ref`\n    \n\n  - `revCount` (Integer, optional)\n\n    \n    Number of revisions in the history of the Git repository before the fetched commit.\n    \n    If set, pass through the value to the output attribute set.\n    Otherwise, generated from the fetched Git tree.\n    \n\n  - `shallow` (Bool, optional)\n\n    \n    Make a shallow clone when fetching the Git tree.\n    When this is enabled, the options `ref` and `allRefs` have no effect anymore.\n    \n    Default: `true`\n    \n\n  - `submodules` (Bool, optional)\n\n    \n    Also fetch submodules if available.\n    \n    Default: `false`\n    \n\n  - `url` (String, required)\n\n    \n    The URL formats supported are the same as for Git itself.\n    \n    > **Example**\n    >\n    > ```nix\n    > fetchTree {\n    >   type = \"git\";\n    >   url = \"git@github.com:NixOS/nixpkgs.git\";\n    > }\n    > ```\n    \n    > **Note**\n    >\n    > If the URL points to a local directory, and no `ref` or `rev` is given, Nix only considers files added to the Git index, as listed by `git ls-files` but uses the *current file contents* of the Git working directory.\n    \n\n  - `verifyCommit` (String, required)\n\n\n- `\"github\"`\n\n\n  - `host` (String, required)\n\n\n  - `lastModified` (String, required)\n\n\n  - `lfs` (Bool, optional)\n\n    \n    Also fetch Git LFS files. Forge archive tarballs contain\n    LFS pointer files rather than the large file content, so\n    enabling this fetches the repository through the\n    equivalent `git+https` input instead, like `submodules`.\n    \n    Default: `false`\n    \n\n  - `narHash` (String, required)\n\n\n  - `owner` (String, required)\n\n\n  - `ref` (String, required)\n\n\n  - `repo` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `submodules` (Bool, optional)\n\n    \n    Also fetch submodules. Forge archive tarballs never\n    contain submodule content, so enabling this fetches the\n    repository through the equivalent `git+https` input\n    instead (same revision, `submodules = true`); see the\n    `git` input scheme for the exact fetch semantics.\n    \n    Note that a Git checkout is not always bit-identical to\n    the forge's archive of the same revision: the archive\n    honors the `export-ignore` and `export-subst` Git\n    attributes, a Git checkout does not. Inputs that request\n    neither `submodules` nor `lfs` are unaffected and keep\n    tarball semantics and hashes.\n    \n    Default: `false`\n    \n\n- `\"gitlab\"`\n\n\n  - `host` (String, required)\n\n\n  - `lastModified` (String, required)\n\n\n  - `lfs` (Bool, optional)\n\n    \n    Also fetch Git LFS files. Forge archive tarballs contain\n    LFS pointer files rather than the large file content, so\n    enabling this fetches the repository through the\n    equivalent `git+https` input instead, like `submodules`.\n    \n    Default: `false`\n    \n\n  - `narHash` (String, required)\n\n\n  - `owner` (String, required)\n\n\n  - `ref` (String, required)\n\n\n  - `repo` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `submodules` (Bool, optional)\n\n    \n    Also fetch submodules. Forge archive tarballs never\n    contain submodule content, so enabling this fetches the\n    repository through the equivalent `git+https` input\n    instead (same revision, `submodules = true`); see the\n    `git` input scheme for the exact fetch semantics.\n    \n    Note that a Git checkout is not always bit-identical to\n    the forge's archive of the same revision: the archive\n    honors the `export-ignore` and `export-subst` Git\n    attributes, a Git checkout does not. Inputs that request\n    neither `submodules` nor `lfs` are unaffected and keep\n    tarball semantics and hashes.\n    \n    Default: `false`\n    \n\n- `\"hg\"`\n\n\n  - `name` (String, required)\n\n\n  - `narHash` (String, required)\n\n\n  - `ref` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `revCount` (String, required)\n\n\n  - `url` (String, required)\n\n\n- `\"indirect\"`\n\n\n  - `id` (String, required)\n\n\n  - `narHash` (String, required)\n\n\n  - `ref` (String, required)\n\n\n  - `rev` (String, required)\n\n\n- `\"jj\"`\n\n  a Jujutsu (jj) repository on jj's native object store, read in-process by tree id\n\n  - `lastModified` (String, required)\n\n\n  - `name` (String, required)\n\n\n  - `ref` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `treeHash` (String, required)\n\n\n  - `url` (String, required)\n\n\n- `\"path\"`\n\n\n  - `lastModified` (String, required)\n\n\n  - `narHash` (String, required)\n\n\n  - `path` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `revCount` (String, required)\n\n\n- `\"sourcehut\"`\n\n\n  - `host` (String, required)\n\n\n  - `lastModified` (String, required)\n\n\n  - `lfs` (Bool, optional)\n\n    \n    Also fetch Git LFS files. Forge archive tarballs contain\n    LFS pointer files rather than the large file content, so\n    enabling this fetches the repository through the\n    equivalent `git+https` input instead, like `submodules`.\n    \n    Default: `false`\n    \n\n  - `narHash` (String, required)\n\n\n  - `owner` (String, required)\n\n\n  - `ref` (String, required)\n\n\n  - `repo` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `submodules` (Bool, optional)\n\n    \n    Also fetch submodules. Forge archive tarballs never\n    contain submodule content, so enabling this fetches the\n    repository through the equivalent `git+https` input\n    instead (same revision, `submodules = true`); see the\n    `git` input scheme for the exact fetch semantics.\n    \n    Note that a Git checkout is not always bit-identical to\n    the forge's archive of the same revision: the archive\n    honors the `export-ignore` and `export-subst` Git\n    attributes, a Git checkout does not. Inputs that request\n    neither `submodules` nor `lfs` are unaffected and keep\n    tarball semantics and hashes.\n    \n    Default: `false`\n    \n\n- `\"tarball\"`\n\n  \n  Download a tar archive and extract it into the Nix store.\n  This has the same underlying implementation as [`builtins.fetchTarball`](@docroot@/language/builtins.md#builtins-fetchTarball)\n  \n\n  - `lastModified` (String, required)\n\n\n  - `name` (String, required)\n\n\n  - `narHash` (String, required)\n\n\n  - `rev` (String, required)\n\n\n  - `revCount` (String, required)\n\n\n  - `unpack` (String, required)\n\n\n  - `url` (String, required)\n\n    \n    > **Example**\n    >\n    > ```nix\n    > fetchTree {\n    >   type = \"tarball\";\n    >   url = \"https://github.com/NixOS/nixpkgs/tarball/nixpkgs-23.11\";\n    > }\n    > ```\n    \n\n\n The following input types are still subject to change:\n\n - `\"path\"`\n - `\"github\"`\n - `\"gitlab\"`\n - `\"sourcehut\"`\n - `\"mercurial\"`\n\n*input* can also be a [URL-like reference](@docroot@/command-ref/new-cli/nix3-flake.md#flake-references).\nThe additional input types and the URL-like syntax requires the [`flakes` experimental feature](@docroot@/development/experimental-features.md#xp-feature-flakes) to be enabled.\n\n > **Example**\n >\n > Fetch a GitHub repository using the attribute set representation:\n >\n > ```nix\n > builtins.fetchTree {\n >   type = \"github\";\n >   owner = \"NixOS\";\n >   repo = \"nixpkgs\";\n >   rev = \"ae2e6b3958682513d28f7d633734571fb18285dd\";\n > }\n > ```\n >\n > This evaluates to the following attribute set:\n >\n > ```nix\n > {\n >   lastModified = 1686503798;\n >   lastModifiedDate = \"20230611171638\";\n >   narHash = \"sha256-rA9RqKP9OlBrgGCPvfd5HVAXDOy8k2SmPtB/ijShNXc=\";\n >   outPath = \"/nix/store/l5m6qlvfs9sdw14ja3qbzpglcjlb6j1x-source\";\n >   rev = \"ae2e6b3958682513d28f7d633734571fb18285dd\";\n >   shortRev = \"ae2e6b3\";\n > }\n > ```\n\n > **Example**\n >\n > Fetch the same GitHub repository using the URL-like syntax:\n >\n >   ```nix\n >   builtins.fetchTree \"github:NixOS/nixpkgs/ae2e6b3958682513d28f7d633734571fb18285dd\"\n >   ```",
    "experimental-feature": "fetch-tree"
  },
  "fetchurl": {
    "args": [
      "arg"
    ],
    "doc": "Download the specified URL and return the path of the downloaded file.\n`arg` can be either a string denoting the URL, or an attribute set with the following attributes:\n\n- `url`\n\n  The URL of the file to download.\n\n- `name` (default: the last path component of the URL)\n\n  A name for the file in the store. This can be useful if the URL has any\n  characters that are invalid for the store.\n\nNot available in [restricted evaluation mode](@docroot@/command-ref/conf-file.md#conf-restrict-eval)."
  },
  "filter": {
    "args": [
      "f",
      "list"
    ],
    "doc": "Return a list consisting of the elements of *list* for which the\nfunction *f* returns `true`."
  },
  "filterSource": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "> **Warning**\n>\n> `filterSource` should not be used to filter store paths. Since\n> `filterSource` uses the name of the input directory while naming\n> the output directory, doing so produces a directory name in\n> the form of `<hash2>-<hash>-<name>`, where `<hash>-<name>` is\n> the name of the input directory. Since `<hash>` depends on the\n> unfiltered directory, the name of the output directory\n> indirectly depends on files that are filtered out by the\n> function. This triggers a rebuild even when a filtered out\n> file is changed. Use `builtins.path` instead, which allows\n> specifying the name of the output directory. This applies to a\n> source copied by its NAR serialisation; a source served from a\n> Jujutsu object store is addressed by the filtered tree's id (below)\n> and does not move when a filtered-out file changes.\n\nThis function allows you to copy sources into the Nix store while\nfiltering certain files. For instance, suppose that you want to use\nthe directory `source-dir` as an input to a Nix expression, e.g.\n\n```nix\nstdenv.mkDerivation {\n  ...\n  src = ./source-dir;\n}\n```\n\nHowever, if `source-dir` is a Subversion working copy, then all of\nthose annoying `.svn` subdirectories are also copied to the\nstore. Worse, the contents of those directories may change a lot,\ncausing lots of spurious rebuilds. With `filterSource` you can\nfilter out the `.svn` directories:\n\n```nix\nsrc = builtins.filterSource\n  (path: type: type != \"directory\" || baseNameOf path != \".svn\")\n  ./source-dir;\n```\n\nThus, the first argument *e1* must be a predicate function that is\ncalled for each regular file, directory or symlink in the source\ntree *e2*. If the function returns `true`, the file is copied to the\nNix store, otherwise it is omitted. The function is called with two\narguments. The first is the full path of the file. The second is a\nstring that identifies the type of the file, which is either\n`\"regular\"`, `\"directory\"`, `\"symlink\"` or `\"unknown\"` (for other\nkinds of files such as device nodes or fifos — but note that those\ncannot be copied to the Nix store, so if the predicate returns\n`true` for them, the copy fails). If you exclude a directory,\nthe entire corresponding subtree of *e2* is excluded.\n\nWhen *e2* is a directory served from a Jujutsu object store (a `jj`\nflake input, including a flake's own `./.`), the result is the tree\nthe predicate leaves, addressed by that tree's own id and mounted\nlazily: no file is read or copied until something forces the store\nobject (a build input, an evaluation result carrying its context,\n`nix flake archive`). The store path then depends on the kept files\nalone. One consequence: a jj tree has no empty directories, so a\ndirectory the predicate accepts but whose every entry it rejects is\nabsent from the result, where a NAR copy would keep it empty."
  },
  "findFile": {
    "args": [
      "search-path",
      "lookup-path"
    ],
    "doc": "Find *lookup-path* in *search-path*.\n\n[Lookup path](@docroot@/language/constructs/lookup-path.md) expressions are [desugared](https://en.wikipedia.org/wiki/Syntactic_sugar) using this and [`builtins.nixPath`](#builtins-nixPath):\n\n```nix\n<nixpkgs>\n```\n\nis equivalent to:\n\n```nix\nbuiltins.findFile builtins.nixPath \"nixpkgs\"\n```\n\nA search path is represented as a list of [attribute sets](./types.md#type-attrs) with two attributes:\n- `prefix` is a relative path.\n- `path` denotes a file system location\n\nExamples of search path attribute sets:\n\n- ```\n  {\n    prefix = \"\";\n    path = \"/nix/var/nix/profiles/per-user/root/channels\";\n  }\n  ```\n- ```\n  {\n    prefix = \"nixos-config\";\n    path = \"/etc/nixos/configuration.nix\";\n  }\n  ```\n- ```\n  {\n    prefix = \"nixpkgs\";\n    path = \"https://github.com/NixOS/nixpkgs/tarballs/master\";\n  }\n  ```\n- ```\n  {\n    prefix = \"nixpkgs\";\n    path = \"channel:nixpkgs-unstable\";\n  }\n  ```\n- ```\n  {\n    prefix = \"flake-compat\";\n    path = \"flake:github:edolstra/flake-compat\";\n  }\n  ```\n\nThe lookup algorithm checks each entry until a match is found, returning a [path value](@docroot@/language/types.md#type-path) of the match:\n\n- If a prefix of `lookup-path` matches `prefix`, then the remainder of *lookup-path* (the \"suffix\") is searched for within the directory denoted by `path`.\n  The contents of `path` may need to be downloaded at this point to look inside.\n\n- If the suffix is found inside that directory, then the entry is a match.\n  The combined absolute path of the directory (now downloaded if need be) and the suffix is returned.\n\n> **Example**\n>\n> A *search-path* value\n>\n> ```\n> [\n>   {\n>     prefix = \"\";\n>     path = \"/home/eelco/Dev\";\n>   }\n>   {\n>     prefix = \"nixos-config\";\n>     path = \"/etc/nixos\";\n>   }\n> ]\n> ```\n>\n> and a *lookup-path* value `\"nixos-config\"` causes Nix to try `/home/eelco/Dev/nixos-config` and `/etc/nixos` in that order and return the first path that exists.\n\nIf `path` starts with `http://` or `https://`, it is interpreted as the URL of a tarball to be downloaded and unpacked to a temporary location.\nThe tarball must consist of a single top-level directory.\n\nThe URLs of the tarballs from the official `nixos.org` channels can be abbreviated as `channel:<channel-name>`.\nSee [documentation on `nix-channel`](@docroot@/command-ref/nix-channel.md) for details about channels.\n\n> **Example**\n>\n> These two search path entries are equivalent:\n>\n> - ```\n>   {\n>     prefix = \"nixpkgs\";\n>     path = \"channel:nixpkgs-unstable\";\n>   }\n>   ```\n> - ```\n>   {\n>     prefix = \"nixpkgs\";\n>     path = \"https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz\";\n>   }\n>   ```\n\nSearch paths can also point to source trees using [flake URLs](@docroot@/command-ref/new-cli/nix3-flake.md#url-like-syntax).\n\n\n> **Example**\n>\n> The search path entry\n>\n> ```\n> {\n>   prefix = \"nixpkgs\";\n>   path = \"flake:nixpkgs\";\n> }\n> ```\n> specifies that the prefix `nixpkgs` shall refer to the source tree downloaded from the `nixpkgs` entry in the flake registry.\n>\n> Similarly\n>\n> ```\n> {\n>   prefix = \"nixpkgs\";\n>   path = \"flake:github:nixos/nixpkgs/nixos-22.05\";\n> }\n> ```\n>\n> makes `<nixpkgs>` refer to a particular branch of the `NixOS/nixpkgs` repository on GitHub."
  },
  "flakeRefToString": {
    "args": [
      "reference"
    ],
    "doc": "Render flake reference attributes as a reference string."
  },
  "floor": {
    "args": [
      "number"
    ],
    "doc": "Rounds and converts *number* to the next lower NixInt value if possible, i.e. `floor *number* <= *number*` and\n`*number* - floor *number* < 1`.\n\nAn evaluation error is thrown, if there exists no such NixInt value `floor *number*`.\nDue to bugs in previous Nix versions an evaluation error might be thrown, if the datatype of *number* is\na NixInt and if `*number* < -9007199254740992` or `*number* > 9007199254740992`.\n\nIf the datatype of *number* is neither a NixInt (signed 64-bit integer) nor a NixFloat\n(IEEE-754 double-precision floating-point number), an evaluation error will be thrown."
  },
  "foldl'": {
    "args": [
      "op",
      "nul",
      "list"
    ],
    "doc": "Reduce a list by applying a binary operator, from left to right,\ne.g. `foldl' op nul [x0 x1 x2 ...] = op (op (op nul x0) x1) x2)\n...`.\n\nFor example, `foldl' (acc: elem: acc + elem) 0 [1 2 3]` evaluates\nto `6` and `foldl' (acc: elem: { \"${elem}\" = elem; } // acc) {}\n[\"a\" \"b\"]` evaluates to `{ a = \"a\"; b = \"b\"; }`.\n\nThe first argument of `op` is the accumulator whereas the second\nargument is the current element being processed. The return value\nof each application of `op` is evaluated immediately, even for\nintermediate values."
  },
  "fromJSON": {
    "args": [
      "e"
    ],
    "doc": "Convert a JSON string to a Nix value. For example,\n\n```nix\nbuiltins.fromJSON ''{\"x\": [1, 2, 3], \"y\": null}''\n```\n\nreturns the value `{ x = [ 1 2 3 ]; y = null; }`."
  },
  "fromTOML": {
    "args": [
      "e"
    ],
    "doc": "Convert a TOML string to a Nix value. For example,\n\n```nix\nbuiltins.fromTOML ''\n  x=1\n  s=\"a\"\n  [table]\n  y=2\n''\n```\n\nreturns the value `{ s = \"a\"; table = { y = 2; }; x = 1; }`."
  },
  "functionArgs": {
    "args": [
      "f"
    ],
    "doc": "Return a set containing the names of the formal arguments expected\nby the function *f*. The value of each attribute is a Boolean\ndenoting whether the corresponding argument has a default value. For\ninstance, `functionArgs ({ x, y ? 123}: ...) = { x = false; y =\ntrue; }`.\n\n\"Formal argument\" here refers to the attributes pattern-matched by\nthe function. Plain lambdas are not included, e.g. `functionArgs (x:\n...) = { }`."
  },
  "genList": {
    "args": [
      "generator",
      "length"
    ],
    "doc": "Generate list of size *length*, with each element *i* equal to the\nvalue returned by *generator* `i`. For example,\n\n```nix\nbuiltins.genList (x: x * x) 5\n```\n\nreturns the list `[ 0 1 4 9 16 ]`."
  },
  "genericClosure": {
    "args": [
      "attrset"
    ],
    "doc": "`builtins.genericClosure` iteratively computes the transitive closure over an arbitrary relation defined by a function.\n\nIt takes *attrset* with two attributes named `startSet` and `operator`, and returns a list of attribute sets:\n\n- `startSet`:\n  The initial list of attribute sets.\n\n- `operator`:\n  A function that takes an attribute set and returns a list of attribute sets.\n  It defines how each item in the current set is processed and expanded into more items.\n\nEach attribute set in the list `startSet` and the list returned by `operator` must have an attribute `key`, which must support equality comparison.\nThe value of `key` can be one of the following types:\n\n- [Int](@docroot@/language/types.md#type-int)\n- [Float](@docroot@/language/types.md#type-float)\n- [Boolean](@docroot@/language/types.md#type-bool)\n- [String](@docroot@/language/types.md#type-string)\n- [Path](@docroot@/language/types.md#type-path)\n- [List](@docroot@/language/types.md#type-list)\n\nThe result is produced by calling the `operator` on each `item` that has not been called yet, including newly added items, until no new items are added.\nItems are compared by their `key` attribute.\n\nCommon usages are:\n\n- Generating unique collections of items, such as dependency graphs.\n- Traversing through structures that may contain cycles or loops.\n- Processing data structures with complex internal relationships.\n\n> **Example**\n>\n> ```nix\n> builtins.genericClosure {\n>   startSet = [ {key = 5;} ];\n>   operator = item: [{\n>     key = if (item.key / 2 ) * 2 == item.key\n>          then item.key / 2\n>          else 3 * item.key + 1;\n>   }];\n> }\n> ```\n>\n> evaluates to\n>\n> ```nix\n> [ { key = 5; } { key = 16; } { key = 8; } { key = 4; } { key = 2; } { key = 1; } ]\n> ```"
  },
  "getAttr": {
    "args": [
      "s",
      "set"
    ],
    "doc": "`getAttr` returns the attribute named *s* from *set*. Evaluation\naborts if the attribute doesn’t exist. This is a dynamic version of\nthe `.` operator, since *s* is an expression rather than an\nidentifier."
  },
  "getContext": {
    "args": [
      "s"
    ],
    "doc": "Return the string context of *s*.\n\nThe string context tracks references to derivations within a string.\nIt is represented as an attribute set of [store derivation](@docroot@/glossary.md#gloss-store-derivation) paths mapping to output names.\n\nUsing [string interpolation](@docroot@/language/string-interpolation.md) on a derivation adds that derivation to the string context.\nFor example,\n\n```nix\nbuiltins.getContext \"${derivation { name = \"a\"; builder = \"b\"; system = \"c\"; }}\"\n```\n\nevaluates to\n\n```\n{ \"/nix/store/arhvjaf6zmlyn8vh8fgn55rpwnxq0n7l-a.drv\" = { outputs = [ \"out\" ]; }; }\n```"
  },
  "getEnv": {
    "args": [
      "s"
    ],
    "doc": "`getEnv` returns the value of the environment variable *s*, or an\nempty string if the variable doesn’t exist. This function should be\nused with care, as it can introduce all sorts of nasty environment\ndependencies in your Nix expression.\n\n`getEnv` is used in Nix Packages to locate the file\n`~/.nixpkgs/config.nix`, which contains user-local settings for Nix\nPackages. (That is, it does a `getEnv \"HOME\"` to locate the user’s\nhome directory.)"
  },
  "getFlake": {
    "args": [
      "args"
    ],
    "doc": "Fetch a flake from a flake reference, and return its output attributes and some metadata. For example:\n\n```nix\n(builtins.getFlake \"nix/55bc52401966fbffa525c574c14f67b00bc4fb3a\").packages.x86_64-linux.nix\n```\n\nUnless impure evaluation is allowed (`--impure`), the flake reference\nmust be \"locked\", e.g. contain a Git revision or content hash. An\nexample of an unlocked usage is:\n\n```nix\n(builtins.getFlake \"github:edolstra/dwarffs\").rev\n```",
    "experimental-feature": "flakes"
  },
  "groupBy": {
    "args": [
      "f",
      "list"
    ],
    "doc": "Groups elements of *list* together by the string returned from the\nfunction *f* called on each element. It returns an attribute set\nwhere each attribute value contains the elements of *list* that are\nmapped to the same corresponding attribute name returned by *f*.\n\nFor example,\n\n```nix\nbuiltins.groupBy (builtins.substring 0 1) [\"foo\" \"bar\" \"baz\"]\n```\n\nevaluates to\n\n```nix\n{ b = [ \"bar\" \"baz\" ]; f = [ \"foo\" ]; }\n```"
  },
  "hasAttr": {
    "args": [
      "s",
      "set"
    ],
    "doc": "`hasAttr` returns `true` if *set* has an attribute named *s*, and\n`false` otherwise. This is a dynamic version of the `?` operator,\nsince *s* is an expression rather than an identifier."
  },
  "hasContext": {
    "args": [
      "s"
    ],
    "doc": "Return `true` if string *s* has a non-empty context.\nThe context can be obtained with\n[`getContext`](#builtins-getContext).\n\n> **Example**\n>\n> Many operations require a string context to be empty because they are intended only to work with \"regular\" strings, and also to help users avoid unintentionally loosing track of string context elements.\n> `builtins.hasContext` can help create better domain-specific errors in those case.\n>\n> ```nix\n> name: meta:\n>\n> if builtins.hasContext name\n> then throw \"package name cannot contain string context\"\n> else { ${name} = meta; }\n> ```"
  },
  "hashFile": {
    "args": [
      "type",
      "p"
    ],
    "doc": "Return a base-16 representation of the cryptographic hash of the\nfile at path *p*. The hash algorithm specified by *type* must be one\nof `\"md5\"`, `\"sha1\"`, `\"sha256\"` or `\"sha512\"`."
  },
  "hashString": {
    "args": [
      "type",
      "s"
    ],
    "doc": "Return a base-16 representation of the cryptographic hash of string\n*s*. The hash algorithm specified by *type* must be one of `\"md5\"`,\n`\"sha1\"`, `\"sha256\"` or `\"sha512\"`."
  },
  "head": {
    "args": [
      "list"
    ],
    "doc": "Return the first element of a list; abort evaluation if the argument\nisn’t a list or is an empty list. You can test whether a list is\nempty by comparing it with `[]`."
  },
  "import": {
    "args": [
      "path"
    ],
    "doc": "Load, parse, and return the Nix expression in the file *path*.\n\n> **Note**\n>\n> Unlike some languages, `import` is a regular function in Nix.\n\nThe *path* argument must meet the same criteria as an [interpolated expression](@docroot@/language/string-interpolation.md#interpolated-expression).\n\nIf *path* is a directory, the file `default.nix` in that directory is used if it exists.\n\n> **Example**\n>\n> ```console\n> $ echo 123 > default.nix\n> ```\n>\n> Import `default.nix` from the current directory.\n>\n> ```nix\n> import ./.\n> ```\n>\n>     123\n\nEvaluation aborts if the file doesn’t exist or contains an invalid Nix expression.\n\nA Nix expression loaded by `import` must not contain any *free variables*, that is, identifiers that are not defined in the Nix expression itself and are not built-in.\nTherefore, it cannot refer to variables that are in scope at the call site.\n\n> **Example**\n>\n> If you have a calling expression\n>\n> ```nix\n> rec {\n>   x = 123;\n>   y = import ./foo.nix;\n> }\n> ```\n>\n>  then the following `foo.nix` throws an error:\n>\n>  ```nix\n>  # foo.nix\n>  x + 456\n>  ```\n>\n>  since `x` is not in scope in `foo.nix`.\n> If you want `x` to be available in `foo.nix`, pass it as a function argument:\n>\n>  ```nix\n>  rec {\n>    x = 123;\n>    y = import ./foo.nix x;\n>  }\n>  ```\n>\n>  and\n>\n>  ```nix\n>  # foo.nix\n>  x: x + 456\n>  ```\n>\n>  The function argument doesn’t have to be called `x` in `foo.nix`; any name would work."
  },
  "intersectAttrs": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return a set consisting of the attributes in the set *e2* which have the\nsame name as some attribute in *e1*.\n\nPerforms in O(*n* log *m*) where *n* is the size of the smaller set and *m* the larger set's size."
  },
  "isAttrs": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a set, and `false` otherwise."
  },
  "isBool": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a bool, and `false` otherwise."
  },
  "isFloat": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a float, and `false` otherwise."
  },
  "isFunction": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a function, and `false` otherwise."
  },
  "isInt": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to an integer, and `false` otherwise."
  },
  "isList": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a list, and `false` otherwise."
  },
  "isNull": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to `null`, and `false` otherwise.\n\nThis is equivalent to `e == null`."
  },
  "isPath": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a path, and `false` otherwise."
  },
  "isString": {
    "args": [
      "e"
    ],
    "doc": "Return `true` if *e* evaluates to a string, and `false` otherwise."
  },
  "langVersion": {
    "doc": "The current version of the Nix language.",
    "type": "integer"
  },
  "length": {
    "args": [
      "e"
    ],
    "doc": "Return the length of the list *e*."
  },
  "lessThan": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return `true` if the value *e1* is less than the value *e2*, and `false` otherwise.\nEvaluation aborts if either *e1* or *e2* does not evaluate to a number, string or path.\nFurthermore, it aborts if *e2* does not match *e1*'s type according to the aforementioned classification of number, string or path."
  },
  "listToAttrs": {
    "args": [
      "e"
    ],
    "doc": "Construct a set from a list specifying the names and values of each\nattribute. Each element of the list should be a set consisting of a\nstring-valued attribute `name` specifying the name of the attribute,\nand an attribute `value` specifying its value.\n\nIn case of duplicate occurrences of the same name, the first\ntakes precedence.\n\nExample:\n\n```nix\nbuiltins.listToAttrs\n  [ { name = \"foo\"; value = 123; }\n    { name = \"bar\"; value = 456; }\n    { name = \"bar\"; value = 420; }\n  ]\n```\n\nevaluates to\n\n```nix\n{ foo = 123; bar = 456; }\n```"
  },
  "map": {
    "args": [
      "f",
      "list"
    ],
    "doc": "Apply the function *f* to each element in the list *list*. For\nexample,\n\n```nix\nmap (x: \"foo\" + x) [ \"bar\" \"bla\" \"abc\" ]\n```\n\nevaluates to `[ \"foobar\" \"foobla\" \"fooabc\" ]`."
  },
  "mapAttrs": {
    "args": [
      "f",
      "attrset"
    ],
    "doc": "Apply function *f* to every element of *attrset*. For example,\n\n```nix\nbuiltins.mapAttrs (name: value: value * 10) { a = 1; b = 2; }\n```\n\nevaluates to `{ a = 10; b = 20; }`."
  },
  "match": {
    "args": [
      "regex",
      "str"
    ],
    "doc": "Returns a list if the [extended POSIX regular\nexpression](http://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap09.html#tag_09_04)\n*regex* matches *str* precisely, otherwise returns `null`. Each item\nin the list is a regex group.\n\n```nix\nbuiltins.match \"ab\" \"abc\"\n```\n\nEvaluates to `null`.\n\n```nix\nbuiltins.match \"abc\" \"abc\"\n```\n\nEvaluates to `[ ]`.\n\n```nix\nbuiltins.match \"a(b)(c)\" \"abc\"\n```\n\nEvaluates to `[ \"b\" \"c\" ]`.\n\n```nix\nbuiltins.match \"[[:space:]]+([[:upper:]]+)[[:space:]]+\" \"  FOO   \"\n```\n\nEvaluates to `[ \"FOO\" ]`."
  },
  "mul": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the product of the numbers *e1* and *e2*."
  },
  "nixPath": {
    "doc": "A list of search path entries used to resolve [lookup paths](@docroot@/language/constructs/lookup-path.md).\nIts value is primarily determined by the [`nix-path` configuration setting](@docroot@/command-ref/conf-file.md#conf-nix-path), which are\n- Overridden by the [`NIX_PATH`](@docroot@/command-ref/env-common.md#env-NIX_PATH) environment variable or the `--nix-path` option\n- Extended by the [`-I` option](@docroot@/command-ref/opt-common.md#opt-I) or `--extra-nix-path`\n\n> **Example**\n>\n> ```bash\n> $ NIX_PATH= nix-instantiate --eval --expr \"builtins.nixPath\" -I foo=bar --no-pure-eval\n> [ { path = \"bar\"; prefix = \"foo\"; } ]\n> ```\n\nLookup path expressions are [desugared](https://en.wikipedia.org/wiki/Syntactic_sugar) using this and\n[`builtins.findFile`](./builtins.html#builtins-findFile):\n\n```nix\n<nixpkgs>\n```\n\nis equivalent to:\n\n```nix\nbuiltins.findFile builtins.nixPath \"nixpkgs\"\n```",
    "type": "list"
  },
  "nixVersion": {
    "doc": "The version of Nix.\n\nFor example, where the command line returns the current Nix version,\n\n```shell-session\n$ nix --version\nnix (Nix) 2.16.0\n```\n\nthe Nix language evaluator returns the same value:\n\n```nix-repl\nnix-repl> builtins.nixVersion\n\"2.16.0\"\n```",
    "type": "string"
  },
  "null": {
    "doc": "Primitive value.\n\nThe name `null` is not special, and can be shadowed:\n\n```nix-repl\nnix-repl> let null = 1; in null\n1\n```",
    "type": "null"
  },
  "parseDrvName": {
    "args": [
      "s"
    ],
    "doc": "Split the string *s* into a package name and version. The package\nname is everything up to but not including the first dash not followed\nby a letter, and the version is everything following that dash. The\nresult is returned in a set `{ name, version }`. Thus,\n`builtins.parseDrvName \"nix-0.12pre12876\"` returns `{ name =\n\"nix\"; version = \"0.12pre12876\"; }`."
  },
  "parseFlakeRef": {
    "args": [
      "reference"
    ],
    "doc": "Parse a flake reference into its attributes."
  },
  "partition": {
    "args": [
      "pred",
      "list"
    ],
    "doc": "Given a predicate function *pred*, this function returns an\nattrset containing a list named `right`, containing the elements\nin *list* for which *pred* returned `true`, and a list named\n`wrong`, containing the elements for which it returned\n`false`. For example,\n\n```nix\nbuiltins.partition (x: x > 10) [1 23 9 3 42]\n```\n\nevaluates to\n\n```nix\n{ right = [ 23 42 ]; wrong = [ 1 9 3 ]; }\n```"
  },
  "path": {
    "args": [
      "args"
    ],
    "doc": "An enrichment of the built-in path type, based on the attributes\npresent in *args*. All are optional except `path`:\n\n  - path\\\n    The underlying path.\n\n  - name\\\n    The name of the path when added to the store. This can used to\n    reference paths that have nix-illegal characters in their names,\n    like `@`.\n\n  - filter\\\n    A function of the type expected by [`builtins.filterSource`](#builtins-filterSource),\n    with the same semantics.\n\n  - recursive\\\n    When `false`, when `path` is added to the store it is with a\n    [flat hash](@docroot@/store/file-system-object/content-address.md#serial-flat),\n    rather than a hash of the\n    [NAR serialization](@docroot@/store/file-system-object/content-address.md#serial-nix-archive)\n    of the file. Thus, `path` must refer to a regular file, not a\n    directory. This allows similar behavior to `fetchurl`. Defaults\n    to `true`.\n\n  - sha256\\\n    When provided, this is the expected\n    [content hash](@docroot@/store/file-system-object/content-address.md)\n    of the path. Evaluation fails if the hash is incorrect,\n    and providing a hash allows `builtins.path` to be used even\n    when the `pure-eval` nix config option is on."
  },
  "pathExists": {
    "args": [
      "path"
    ],
    "doc": "Return `true` if the path *path* exists at evaluation time, and\n`false` otherwise."
  },
  "placeholder": {
    "args": [
      "output"
    ],
    "doc": "Return an\n[output placeholder string](@docroot@/store/derivation/index.md#output-placeholder)\nfor the specified *output* that will be substituted by the corresponding\n[output path](@docroot@/glossary.md#gloss-output-path)\nat build time.\n\nTypical outputs would be `\"out\"`, `\"bin\"` or `\"dev\"`."
  },
  "readDir": {
    "args": [
      "path"
    ],
    "doc": "Return the contents of the directory *path* as a set mapping\ndirectory entries to the corresponding file type. For instance, if\ndirectory `A` contains a regular file `B` and another directory\n`C`, then `builtins.readDir ./A` returns the set\n\n```nix\n{ B = \"regular\"; C = \"directory\"; }\n```\n\nThe possible values for the file type are `\"regular\"`,\n`\"directory\"`, `\"symlink\"` and `\"unknown\"`."
  },
  "readFile": {
    "args": [
      "path"
    ],
    "doc": "Return the contents of the file *path* as a string."
  },
  "readFileType": {
    "args": [
      "p"
    ],
    "doc": "Determine the directory entry type of a filesystem node, being\none of `\"directory\"`, `\"regular\"`, `\"symlink\"`, or `\"unknown\"`."
  },
  "removeAttrs": {
    "args": [
      "set",
      "list"
    ],
    "doc": "Remove the attributes listed in *list* from *set*. The attributes\ndon’t have to exist in *set*. For instance,\n\n```nix\nremoveAttrs { x = 1; y = 2; z = 3; } [ \"a\" \"x\" \"z\" ]\n```\n\nevaluates to `{ y = 2; }`."
  },
  "replaceStrings": {
    "args": [
      "from",
      "to",
      "s"
    ],
    "doc": "Given string *s*, replace every occurrence of the strings in *from*\nwith the corresponding string in *to*.\n\nThe argument *to* is lazy, that is, it is only evaluated when its corresponding pattern in *from* is matched in the string *s*\n\nExample:\n\n```nix\nbuiltins.replaceStrings [\"oo\" \"a\"] [\"a\" \"i\"] \"foobar\"\n```\n\nevaluates to `\"fabir\"`."
  },
  "seq": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Evaluate *e1*, then evaluate and return *e2*. This ensures that a\ncomputation is strict in the value of *e1*."
  },
  "sort": {
    "args": [
      "comparator",
      "list"
    ],
    "doc": "Return *list* in sorted order. It repeatedly calls the function\n*comparator* with two elements. The comparator should return `true`\nif the first element is less than the second, and `false` otherwise.\nFor example,\n\n```nix\nbuiltins.sort builtins.lessThan [ 483 249 526 147 42 77 ]\n```\n\nproduces the list `[ 42 77 147 249 483 526 ]`.\n\nThis is a stable sort: it preserves the relative order of elements\ndeemed equal by the comparator.\n\n*comparator* must impose a strict weak ordering on the set of values\nin the *list*. This means that for any elements *a*, *b* and *c* from the\n*list*, *comparator* must satisfy the following relations:\n\n  1. Transitivity\n\n  If a is less than b and b is less than c, then it follows that a is less than c.\n\n  ```nix\n  comparator a b && comparator b c -> comparator a c\n  ```\n\n  1. Irreflexivity\n\n  ```nix\n  comparator a a == false\n  ```\n\n  1. Transitivity of equivalence\n\n  First, two values a and b are considered equivalent with respect to the comparator if:\n\n  ```\n  !comparator a b && !comparator b a\n  ```\n\n  In other words, neither is considered \"less than\" the other.\n\n  Transitivity of equivalence means:\n\n  If a is equivalent to b, and b is equivalent to c, then a must also be equivalent to c.\n\n  ```nix\n  let\n    equiv = x: y: (!comparator x y && !comparator y x);\n  in\n    equiv a b && equiv b c -> equiv a c\n  ```\n\nIf the *comparator* violates any of these properties, then `builtins.sort`\nreorders elements in an unspecified manner."
  },
  "split": {
    "args": [
      "regex",
      "str"
    ],
    "doc": "Returns a list composed of non matched strings interleaved with the\nlists of the [extended POSIX regular\nexpression](http://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap09.html#tag_09_04)\n*regex* matches of *str*. Each item in the lists of matched\nsequences is a regex group.\n\n```nix\nbuiltins.split \"(a)b\" \"abc\"\n```\n\nEvaluates to `[ \"\" [ \"a\" ] \"c\" ]`.\n\n```nix\nbuiltins.split \"([ac])\" \"abc\"\n```\n\nEvaluates to `[ \"\" [ \"a\" ] \"b\" [ \"c\" ] \"\" ]`.\n\n```nix\nbuiltins.split \"(a)|(c)\" \"abc\"\n```\n\nEvaluates to `[ \"\" [ \"a\" null ] \"b\" [ null \"c\" ] \"\" ]`.\n\n```nix\nbuiltins.split \"([[:upper:]]+)\" \" FOO \"\n```\n\nEvaluates to `[ \" \" [ \"FOO\" ] \" \" ]`."
  },
  "splitVersion": {
    "args": [
      "s"
    ],
    "doc": "Split a string representing a version into its components, by the\nsame version splitting logic underlying the version comparison in\n[`nix-env -u`](../command-ref/nix-env/upgrade.md)."
  },
  "storeDir": {
    "doc": "Logical file system location of the [Nix store](@docroot@/glossary.md#gloss-store) currently in use.\n\nThis value is determined by the `store` parameter in [Store URLs](@docroot@/store/types/index.md#store-url-format):\n\n```shell-session\n$ nix-instantiate --store 'dummy://?store=/blah' --eval --expr builtins.storeDir\n\"/blah\"\n```",
    "type": "string"
  },
  "storePath": {
    "args": [
      "path"
    ],
    "doc": "This function allows you to define a dependency on an already\nexisting store path. For example, the derivation attribute `src\n= builtins.storePath /nix/store/f1d18v1y…-source` causes the\nderivation to depend on the specified path, which must exist or\nbe substitutable. Note that this differs from a plain path\n(e.g. `src = /nix/store/f1d18v1y…-source`) in that the latter\ncauses the path to be *copied* again to the Nix store, resulting\nin a new path (e.g. `/nix/store/ld01dnzc…-source-source`).\n\nNot available in [pure evaluation mode](@docroot@/command-ref/conf-file.md#conf-pure-eval).\n\nSee also [`builtins.fetchClosure`](#builtins-fetchClosure)."
  },
  "stringLength": {
    "args": [
      "e"
    ],
    "doc": "Return the number of bytes of the string *e*. If *e* is not a string,\nevaluation is aborted."
  },
  "sub": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Return the difference between the numbers *e1* and *e2*."
  },
  "substring": {
    "args": [
      "start",
      "len",
      "s"
    ],
    "doc": "Return the substring of *s* from byte position *start*\n(zero-based) up to but not including *start + len*. If *start* is\ngreater than the length of the string, an empty string is returned.\nIf *start + len* lies beyond the end of the string or *len* is `-1`,\nonly the substring up to the end of the string is returned.\n*start* must be non-negative.\nFor example,\n\n```nix\nbuiltins.substring 0 3 \"nixos\"\n```\n\nevaluates to `\"nix\"`."
  },
  "tail": {
    "args": [
      "list"
    ],
    "doc": "Return the list without its first item; abort evaluation if\nthe argument isn’t a list or is an empty list.\n\n> **Warning**\n>\n> This function should generally be avoided since it's inefficient:\n> unlike Haskell's `tail`, it takes O(n) time, so recursing over a\n> list by repeatedly calling `tail` takes O(n^2) time."
  },
  "throw": {
    "args": [
      "s"
    ],
    "doc": "Throw an error message *s*. This usually aborts Nix expression\nevaluation, but in `nix-env -qa` and other commands that try to\nevaluate a set of derivations to get information about those\nderivations, a derivation that throws an error is silently skipped\n(which is not the case for `abort`)."
  },
  "toFile": {
    "args": [
      "name",
      "s"
    ],
    "doc": "Store the string *s* in a file in the Nix store and return its\npath.  The file has suffix *name*. This file can be used as an\ninput to derivations. One application is to write builders\n“inline”. For instance, the following Nix expression combines the\nNix expression for GNU Hello and its build script into one file:\n\n```nix\n{ stdenv, fetchurl, perl }:\n\nstdenv.mkDerivation {\n  name = \"hello-2.1.1\";\n\n  builder = builtins.toFile \"builder.sh\" \"\n    source $stdenv/setup\n\n    PATH=$perl/bin:$PATH\n\n    tar xvfz $src\n    cd hello-*\n    ./configure --prefix=$out\n    make\n    make install\n  \";\n\n  src = fetchurl {\n    url = \"http://ftp.nluug.nl/pub/gnu/hello/hello-2.1.1.tar.gz\";\n    sha256 = \"1md7jsfd8pa45z73bz1kszpp01yw6x5ljkjk2hx7wl800any6465\";\n  };\n  inherit perl;\n}\n```\n\nIt is even possible for one file to refer to another, e.g.,\n\n```nix\nbuilder = let\n  configFile = builtins.toFile \"foo.conf\" \"\n    # This is some dummy configuration file.\n    ...\n  \";\nin builtins.toFile \"builder.sh\" \"\n  source $stdenv/setup\n  ...\n  cp ${configFile} $out/etc/foo.conf\n\";\n```\n\nNote that `${configFile}` is a\n[string interpolation](@docroot@/language/types.md#type-string), so the result of the\nexpression `configFile`\n(i.e., a path like `/nix/store/m7p7jfny445k...-foo.conf`) will be\nspliced into the resulting string.\n\nIt is however *not* allowed to have files mutually referring to each\nother, like so:\n\n```nix\nlet\n  foo = builtins.toFile \"foo\" \"...${bar}...\";\n  bar = builtins.toFile \"bar\" \"...${foo}...\";\nin foo\n```\n\nThis is not allowed because it would cause a cyclic dependency in\nthe computation of the cryptographic hashes for `foo` and `bar`.\n\nIt is also not possible to reference the result of a derivation. If\nyou are using Nixpkgs, the `writeTextFile` function is able to do\nthat."
  },
  "toJSON": {
    "args": [
      "e"
    ],
    "doc": "Return a string containing a JSON representation of *e*. Strings,\nintegers, floats, booleans, nulls and lists are mapped to their JSON\nequivalents. Sets (except derivations) are represented as objects.\nDerivations are translated to a JSON string containing the\nderivation’s output path. Paths are copied to the store and\nrepresented as a JSON string of the resulting store path."
  },
  "toPath": {
    "args": [
      "s"
    ],
    "doc": "**DEPRECATED.** Use `/. + \"/path\"` to convert a string into an absolute\npath. For relative paths, use `./. + \"/path\"`."
  },
  "toString": {
    "args": [
      "e"
    ],
    "doc": "Convert the expression *e* to a string. *e* can be:\n\n  - A string (in which case the string is returned unmodified).\n\n  - A path (e.g., `toString /foo/bar` yields `\"/foo/bar\"`.\n\n  - A set containing `{ __toString = self: ...; }` or `{ outPath = ...; }`.\n\n  - An integer.\n\n  - A list, in which case the string representations of its elements\n    are joined with spaces.\n\n  - A Boolean (`false` yields `\"\"`, `true` yields `\"1\"`).\n\n  - `null`, which yields the empty string."
  },
  "toXML": {
    "args": [
      "e"
    ],
    "doc": "Return a string containing an XML representation of *e*. The main\napplication for `toXML` is to communicate information with the\nbuilder in a more structured format than plain environment\nvariables.\n\nHere is an example where this is the case:\n\n```nix\n{ stdenv, fetchurl, libxslt, jira, uberwiki }:\n\nstdenv.mkDerivation (rec {\n  name = \"web-server\";\n\n  buildInputs = [ libxslt ];\n\n  builder = builtins.toFile \"builder.sh\" \"\n    source $stdenv/setup\n    mkdir $out\n    echo \"$servlets\" | xsltproc ${stylesheet} - > $out/server-conf.xml ①\n  \";\n\n  stylesheet = builtins.toFile \"stylesheet.xsl\" ②\n   \"<?xml version='1.0' encoding='UTF-8'?>\n    <xsl:stylesheet xmlns:xsl='http://www.w3.org/1999/XSL/Transform' version='1.0'>\n      <xsl:template match='/'>\n        <Configure>\n          <xsl:for-each select='/expr/list/attrs'>\n            <Call name='addWebApplication'>\n              <Arg><xsl:value-of select=\\\"attr[@name = 'path']/string/@value\\\" /></Arg>\n              <Arg><xsl:value-of select=\\\"attr[@name = 'war']/path/@value\\\" /></Arg>\n            </Call>\n          </xsl:for-each>\n        </Configure>\n      </xsl:template>\n    </xsl:stylesheet>\n  \";\n\n  servlets = builtins.toXML [ ③\n    { path = \"/bugtracker\"; war = jira + \"/lib/atlassian-jira.war\"; }\n    { path = \"/wiki\"; war = uberwiki + \"/uberwiki.war\"; }\n  ];\n})\n```\n\nThe builder is supposed to generate the configuration file for a\n[Jetty servlet container](http://jetty.mortbay.org/). A servlet\ncontainer contains a number of servlets (`*.war` files) each\nexported under a specific URI prefix. So the servlet configuration\nis a list of sets containing the `path` and `war` of the servlet\n(①). This kind of information is difficult to communicate with the\nnormal method of passing information through an environment\nvariable, which just concatenates everything together into a\nstring (which might just work in this case, but wouldn’t work if\nfields are optional or contain lists themselves). Instead the Nix\nexpression is converted to an XML representation with `toXML`,\nwhich is unambiguous and can easily be processed with the\nappropriate tools. For instance, in the example an XSLT stylesheet\n(at point ②) is applied to it (at point ①) to generate the XML\nconfiguration file for the Jetty server. The XML representation\nproduced at point ③ by `toXML` is as follows:\n\n```xml\n<?xml version='1.0' encoding='utf-8'?>\n<expr>\n  <list>\n    <attrs>\n      <attr name=\"path\">\n        <string value=\"/bugtracker\" />\n      </attr>\n      <attr name=\"war\">\n        <path value=\"/nix/store/d1jh9pasa7k2...-jira/lib/atlassian-jira.war\" />\n      </attr>\n    </attrs>\n    <attrs>\n      <attr name=\"path\">\n        <string value=\"/wiki\" />\n      </attr>\n      <attr name=\"war\">\n        <path value=\"/nix/store/y6423b1yi4sx...-uberwiki/uberwiki.war\" />\n      </attr>\n    </attrs>\n  </list>\n</expr>\n```\n\nNote that we used the `toFile` built-in to write the builder and\nthe stylesheet “inline” in the Nix expression. The path of the\nstylesheet is spliced into the builder using the syntax `xsltproc\n${stylesheet}`."
  },
  "trace": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Evaluate *e1* and print its abstract syntax representation on\nstandard error. Then return *e2*. This function is useful for\ndebugging.\n\nIf the\n[`debugger-on-trace`](@docroot@/command-ref/conf-file.md#conf-debugger-on-trace)\noption is set to `true` and the `--debugger` flag is given, the\ninteractive debugger is started when `trace` is called (like\n[`break`](@docroot@/language/builtins.md#builtins-break))."
  },
  "traceVerbose": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Evaluate *e1* and print its abstract syntax representation on standard\nerror if `--trace-verbose` is enabled. Then return *e2*. This function\nis useful for debugging."
  },
  "true": {
    "doc": "Primitive value.\n\nIt can be returned by\n[comparison operators](@docroot@/language/operators.md#comparison)\nand used in\n[conditional expressions](@docroot@/language/syntax.md#conditionals).\n\nThe name `true` is not special, and can be shadowed:\n\n```nix-repl\nnix-repl> let true = 1; in true\n1\n```",
    "type": "Boolean"
  },
  "tryEval": {
    "args": [
      "e"
    ],
    "doc": "Try to shallowly evaluate *e*. Return a set containing the\nattributes `success` (`true` if *e* evaluated successfully,\n`false` if an error was thrown) and `value`, equalling *e* if\nsuccessful and `false` otherwise. `tryEval` only prevents\nerrors created by `throw` or `assert` from being thrown.\nErrors `tryEval` doesn't catch are, for example, those created\nby `abort` and type errors generated by builtins. Also note that\nthis doesn't evaluate *e* deeply, so `let e = { x = throw \"\"; };\nin (builtins.tryEval e).success` is `true`. Using\n`builtins.deepSeq` one can get the expected result:\n`let e = { x = throw \"\"; }; in\n(builtins.tryEval (builtins.deepSeq e e)).success` is\n`false`.\n\n`tryEval` intentionally does not return the error message, because that risks bringing non-determinism into the evaluation result, and it would become very difficult to improve error reporting without breaking existing expressions.\nInstead, use [`builtins.addErrorContext`](@docroot@/language/builtins.md#builtins-addErrorContext) to add context to the error message, and use a Nix unit testing tool for testing."
  },
  "typeOf": {
    "args": [
      "e"
    ],
    "doc": "Return a string representing the type of the value *e*, namely\n`\"int\"`, `\"bool\"`, `\"string\"`, `\"path\"`, `\"null\"`, `\"set\"`,\n`\"list\"`, `\"lambda\"` or `\"float\"`."
  },
  "unsafeDiscardOutputDependency": {
    "args": [
      "s"
    ],
    "doc": "Create a copy of the given string where every\n[derivation deep](@docroot@/language/string-context.md#string-context-element-derivation-deep)\nstring context element is turned into a\n[constant](@docroot@/language/string-context.md#string-context-constant)\nstring context element.\n\nThis is the opposite of [`builtins.addDrvOutputDependencies`](#builtins-addDrvOutputDependencies).\n\nThis is unsafe because it allows us to \"forget\" store objects we would have otherwise referred to with the string context,\nwhereas Nix normally tracks all dependencies consistently.\nSafe operations \"grow\" but never \"shrink\" string contexts.\n[`builtins.addDrvOutputDependencies`] in contrast is safe because \"derivation deep\" string context element always refers to the underlying derivation (among many more things).\nReplacing a constant string context element with a \"derivation deep\" element is a safe operation that just enlargens the string context without forgetting anything.\n\n[`builtins.addDrvOutputDependencies`]: #builtins-addDrvOutputDependencies"
  },
  "unsafeDiscardStringContext": {
    "args": [
      "s"
    ],
    "doc": "Discard the [string context](@docroot@/language/string-context.md) from a value that can be coerced to a string."
  },
  "unsafeGetAttrPos": {
    "args": [
      "s",
      "set"
    ],
    "doc": "`unsafeGetAttrPos` returns the position of the attribute named *s*\nfrom *set*. This is used by Nixpkgs to provide location information\nin error messages."
  },
  "warn": {
    "args": [
      "e1",
      "e2"
    ],
    "doc": "Evaluate *e1*, which must be a string, and print it on standard error as a warning.\nThen return *e2*.\nThis function is useful for non-critical situations where attention is advisable.\n\nIf the\n[`debugger-on-trace`](@docroot@/command-ref/conf-file.md#conf-debugger-on-trace)\nor [`debugger-on-warn`](@docroot@/command-ref/conf-file.md#conf-debugger-on-warn)\noption is set to `true` and the `--debugger` flag is given, the\ninteractive debugger will be started when `warn` is called (like\n[`break`](@docroot@/language/builtins.md#builtins-break)).\n\nIf the\n[`abort-on-warn`](@docroot@/command-ref/conf-file.md#conf-abort-on-warn)\noption is set, the evaluation is aborted after the warning is printed.\nThis is useful to reveal the stack trace of the warning, when the context is non-interactive and a debugger can not be launched."
  },
  "zipAttrsWith": {
    "args": [
      "f",
      "list"
    ],
    "doc": "Transpose a list of attribute sets into an attribute set of lists,\nthen apply `mapAttrs`.\n\n`f` receives two arguments: the attribute name and a non-empty\nlist of all values encountered for that attribute name.\n\nThe result is an attribute set where the attribute names are the\nunion of the attribute names in each element of `list`. The attribute\nvalues are the return values of `f`.\n\n```nix\nbuiltins.zipAttrsWith\n  (name: values: { inherit name values; })\n  [ { a = \"x\"; } { a = \"y\"; b = \"z\"; } ]\n```\n\nevaluates to\n\n```\n{\n  a = { name = \"a\"; values = [ \"x\" \"y\" ]; };\n  b = { name = \"b\"; values = [ \"z\" ]; };\n}\n```"
  },
  "wasm": {
    "args": [
      "options",
      "argument"
    ],
    "doc": "Evaluate a WebAssembly module using the evaluator host interface."
  },
  "derivationStrict": {
    "args": [
      "attributes"
    ],
    "doc": "Construct a store derivation from its attributes, returning the derivation path and output paths."
  },
  "derivation": {
    "args": [
      "attributes"
    ],
    "doc": "Construct a lazy derivation from its attributes and expose its named outputs."
  },
  "addErrorContext": {
    "args": [
      "context",
      "value"
    ],
    "doc": "Evaluate value, adding context to an evaluation error if one is raised. The context is evaluated only when needed."
  },
  "appendContext": {
    "args": [
      "string",
      "context"
    ],
    "doc": "Return string with the specified store dependencies added to its string context."
  }
}"###;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_header_bits_match_the_typed_settings() -> Result<(), String> {
        let header = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/ixe.h"),
        )
        .map_err(|error| error.to_string())?;
        let mut actual = std::collections::BTreeMap::new();
        for line in header.lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("#define") {
                continue;
            }
            let Some(name) = words.next() else {
                continue;
            };
            if !name.starts_with("IXE_BUILTIN_") {
                continue;
            }
            let value = words
                .next()
                .ok_or_else(|| format!("missing value for {name}"))?
                .trim_end_matches(['u', 'U'])
                .parse::<u32>()
                .map_err(|e| e.to_string())?;
            assert!(
                actual.insert(name, value).is_none(),
                "duplicate feature {name}"
            );
        }
        let expected = std::collections::BTreeMap::from([
            (
                "IXE_BUILTIN_FLAKES",
                BuiltinFeatures {
                    flakes: true,
                    ..BuiltinFeatures::NONE
                }
                .bits(),
            ),
            (
                "IXE_BUILTIN_FETCH_TREE",
                BuiltinFeatures {
                    fetch_tree: true,
                    ..BuiltinFeatures::NONE
                }
                .bits(),
            ),
            (
                "IXE_BUILTIN_WASM",
                BuiltinFeatures {
                    wasm: true,
                    ..BuiltinFeatures::NONE
                }
                .bits(),
            ),
        ]);
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn documentation_abi_owns_its_output_and_rejects_invalid_slots() -> Result<(), String> {
        let mut output = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        // SAFETY: distinct writable slots; returned strings are freed below.
        let status = unsafe { crate::capi::ixe_language_docs(&mut output, &mut error) };
        assert_eq!(status, 0);
        assert!(error.is_null());
        assert!(!output.is_null());
        // SAFETY: success returned an owned NUL-terminated string.
        let actual = unsafe { std::ffi::CStr::from_ptr(output) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: exactly the allocation returned above, freed once.
        unsafe {
            crate::capi::ixe_string_free(output);
        }
        assert_eq!(actual, language_docs().map_err(|e| e.to_string())?);
        // SAFETY: deliberately invalid slots are rejected before being dereferenced.
        assert_eq!(
            unsafe { crate::capi::ixe_language_docs(std::ptr::null_mut(), &mut error) },
            4
        );
        // SAFETY: deliberately aliased slots are rejected before being dereferenced.
        assert_eq!(
            unsafe { crate::capi::ixe_language_docs(&mut error, &mut error) },
            4
        );
        Ok(())
    }

    #[test]
    fn feature_setter_rejects_unknown_bits_without_mutation() {
        let _globals = crate::eval::globals_moving();
        let before = crate::eval::builtin_features();
        assert_eq!(crate::capi::ixe_set_builtin_features(8), 4);
        assert_eq!(crate::eval::builtin_features(), before);
        assert_eq!(crate::capi::ixe_set_builtin_features(0), 0);
        assert_eq!(crate::eval::builtin_features(), BuiltinFeatures::NONE);
        assert_eq!(crate::capi::ixe_set_builtin_features(before.bits()), 0);
    }

    #[test]
    fn documentation_matches_the_implemented_catalogue() -> Result<(), String> {
        let text = language_docs().map_err(|e| e.to_string())?;
        let docs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let settings = crate::eval::Settings::default();
        let members: std::collections::BTreeSet<_> =
            crate::builtins::set_member_names(&settings).collect();
        let documented: std::collections::BTreeSet<_> = docs.keys().map(String::as_str).collect();
        assert_eq!(members, documented);
        for (name, entry) in &docs {
            assert!(
                entry
                    .get("doc")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|doc| !doc.is_empty()),
                "missing docs for {name}"
            );
            if let Some(index) = crate::builtins::global_index(name) {
                let Some(builtin) = crate::builtins::TABLE.get(usize::from(index)) else {
                    return Err(format!("invalid builtin index for {name}"));
                };
                assert_eq!(
                    entry
                        .get("args")
                        .and_then(serde_json::Value::as_array)
                        .map(Vec::len),
                    Some(builtin.arity),
                    "argument docs for {name}"
                );
            } else if name != "derivation" {
                assert!(
                    entry.get("type").is_some_and(serde_json::Value::is_string),
                    "missing constant type for {name}"
                );
            }
        }
        assert_eq!(
            docs.get("wasm")
                .and_then(|entry| entry.get("experimental-feature"))
                .and_then(serde_json::Value::as_str),
            Some("wasm-builtin")
        );
        assert_eq!(
            docs.get("getFlake")
                .and_then(|entry| entry.get("experimental-feature"))
                .and_then(serde_json::Value::as_str),
            Some("flakes")
        );
        Ok(())
    }

    #[test]
    fn every_public_implementation_has_a_catalogue_spelling() {
        let members: Vec<_> =
            crate::builtins::set_member_names(&crate::eval::Settings::default()).collect();
        for global in GLOBAL_NAMES {
            let member = global.strip_prefix("__").unwrap_or(global);
            assert!(
                crate::builtins::global_index(member).is_some(),
                "unsupported global {global}"
            );
        }
        for builtin in crate::builtins::TABLE {
            if gate_of(builtin.name) == Some(Gate::Never) {
                continue;
            }
            assert!(
                members.contains(&builtin.name),
                "{} has no public spelling",
                builtin.name
            );
        }
    }
}
