# Per-system flake outputs (packages / checks / formatter).
#
# Kept out of flake.nix so the flake top-level can read as a manifest of
# inputs and output categories. Composition logic for workflow tools and
# lint plumbing lives here. Workflow tools (lint, update-mods, ...) are
# exposed under `packages.<system>.<name>` with `meta.mainProgram` set, so
# `nix run .#<name>` and `nix build .#<name>` both work without an `apps`
# entry (see AGENTS.md "Flake.nix style").
{
  system,
  ix,
  nixpkgs,
  paths,
  rust-overlay,
  home-manager,
  disko,
}: let
  inherit (nixpkgs) lib;
  pkgs = import nixpkgs {
    inherit system;
    config = {};
    overlays = [
      rust-overlay.overlays.default
      ix.overlay
    ];
  };
  fs = lib.fileset;
  packageRegistry = import (paths.packagesRoot + "/registry.nix") {
    inherit lib;
    root = paths.packagesRoot;
    inherit (ix.lists) findDuplicates;
  };

  # The vendored trees are not ours to style. Each subtree under `views/` is
  # an imported copy of another project's tree -- `lib/default.nix`'s
  # `viewSource` hands the directory straight to `builtins.path` as the source
  # the fork builds from -- so reformatting, renaming or annotating a file
  # there changes what we build and the copy stops being that project's tree.
  # House style stops at the repository's own files.
  #
  # This is the single place naming what is vendored. Stages that discover
  # their own files go through `owned-files`, and the four tools that walk the
  # tree themselves (statix, deadnix, clone, complexity) exclude the same roots
  # through their own flag.
  #
  # `vendor/` was the name until ix#9905 moved the forks under `views/`. The
  # constant was not renamed with them, so it went on excluding a directory
  # neither repo has while 4350 upstream `.nix` files, 7378 `.rs` and 588 `.py`
  # sat inside the gate -- red main, and `lint --fix` one run away from
  # reformatting all of them (ENG-12609). The throw below is why a name that
  # protects nothing cannot come back quietly: it fails eval instead.
  vendoredDirs = let
    named = ["views"];
    missing = builtins.filter (dir: !builtins.pathExists (paths.root + "/${dir}")) named;
  in
    if missing == []
    then named
    else
      throw ''
        lint: vendored root(s) named here but absent from ${toString paths.root}: ${lib.concatStringsSep ", " missing}.
        A name that matches nothing excludes nothing, which is exactly how ix#9905 went unnoticed.
        Prune the entry or fix the spelling.
      '';

  # A nushell list literal pairing `flag` with each vendored root, rendered by
  # `spell`. The flags disagree on syntax, so each gets its own spelling rather
  # than one string bent to fit all four.
  nuExcludeFlags = flag: spell:
    "[" + lib.concatMapStringsSep " " (dir: ''"${flag}" "${spell dir}"'') vendoredDirs + "]";

  # Each lint stage is one subcommand on a single binary so the spec keys
  # off `lib.getExe lintStage` without registering four sibling packages.
  # The Nu wrapper checks syntax at build time, so a typo in a stage shows
  # up in the `lint` derivation build, not at `nix run` time.
  lintStage = ix.writeNushellApplication pkgs {
    name = "lint-stage";
    meta.description = "One lint stage (alejandra | statix | deadnix | astlog | astlog-rust | astlog-elixir | shell-fence | filenames | dirnames | svg-dark | site-ids | site-frontmatter | ruff | clone | complexity); driven by `lint`";
    runtimeInputs = [
      pkgs.alejandra
      pkgs.deadnix
      pkgs.fd
      pkgs.ruff
      pkgs.statix
      repoPackages.astlog
      repoPackages.clone
      repoPackages.complexity
    ];
    text = ''
      # nu
      # The vendored roots, one spelling per tool, generated from `vendoredDirs`
      # above so the set is named once in Nix and never drifts between flags.
      #
      # `fd` and `statix` take gitignore patterns, where the leading slash
      # anchors the match to the scan root: a bare name matches at any depth and
      # would also drop users/*/config/nushell/vendor/autoload, which is ours
      # and is shell-fenced.
      const vendored_fd_flags = ${nuExcludeFlags "--exclude" (dir: "/" + dir)}
      const vendored_statix_flags = ${nuExcludeFlags "--ignore" (dir: "/" + dir)}
      # `clone` and `complexity` match a `glob::Pattern` against a scan path
      # carrying a `./` prefix, so a bare directory name matches nothing there;
      # `*` crosses separators, so one trailing `*` covers the whole subtree.
      const vendored_scan_flags = ${nuExcludeFlags "--ignore" (dir: "./" + dir + "/*")}
      # `deadnix` takes a path prefix instead of a glob, so the bare directory
      # is the exclusion there; a vendored tree nested deeper would need its own
      # entry.
      const vendored_deadnix_flags = ${nuExcludeFlags "--exclude" (dir: dir)}
      #
      # `fd` restricted to repository-owned files: every file-discovering stage
      # calls this instead of bare `fd`. The arguments are fd's own, passed as a
      # list because a custom nushell command rejects flags its own signature
      # does not declare.
      def owned-files [fd_args: list<string>] { fd ...$vendored_fd_flags ...$fd_args }
      def "main alejandra" [] {
        let nix_files = (owned-files [--extension nix] | lines)
        alejandra --check ...$nix_files
      }
      # `--ignore` extends statix.toml's `ignore` rather than replacing it, so
      # the two module roots listed there stay excluded.
      def "main statix" [] { statix check . ...$vendored_statix_flags }
      # Strict: no `-L`/`--no-lambda-pattern-names`. That flag exists because
      # dropping a pattern name is unsafe without `...` in the pattern (it
      # narrows the callable signature); an unused name here must be deleted
      # (migrating call sites) or kept behind `...`, matching what the LSP
      # already flags as unused.
      # `--exclude` takes a list, so the `--` is load-bearing: written as
      # `deadnix --fail --exclude vendor .` the `.` is read as a second exclude,
      # the default scan path then has everything excluded, and the stage passes
      # having checked nothing.
      def "main deadnix" [] { deadnix --fail ...$vendored_deadnix_flags -- . }
      # The Nix style rules as astlog lint declarations
      # (astlog-rules/nix.astlog, #1060/#1062). `astlog scan` emits one
      # finding per lint-declared relation row and exits nonzero on any
      # error-severity finding, so adding a (lint ...) extends the gate
      # without touching this invocation. Legitimate exceptions are
      # suppressed in place with `astlog-ignore: <rule>` comments. Only
      # .nix files are handed to the corpus: astlog would otherwise parse
      # every known-grammar file in the repo to run nix-only rules.
      def "main astlog" [] {
        let nix_files = (owned-files [--extension nix] | lines)
        astlog scan astlog-rules/nix.astlog ...$nix_files
      }
      # The Rust style rules (astlog-rules/rust.astlog), the successor to the
      # ast-grep rust rules (#1060 ported the nix rules first). Scoped to the
      # corpus/search crates, the `files:` scope those rules carried under
      # ast-grep; astlog walks each directory and runs the rust rules over its
      # .rs files. Both rulesets share the `astlog-rules` flake-check self-test.
      def "main astlog-rust" [] {
        let dirs = (
          [
            packages/indexer
            packages/search
            packages/search-core
            packages/search-py
            packages/source
            packages/sink
          ]
          | where {|d| $d | path exists}
        )
        if ($dirs | is-not-empty) {
          astlog scan astlog-rules/rust.astlog ...$dirs
        }
        # The Cargo/workspace rules (astlog-rules/cargo.astlog, TOML grammar) run
        # over every Cargo.toml in the repo: `no-cargo-path-dep` bans inter-crate
        # `path` deps in member tables so local crates are declared once in a
        # [workspace.dependencies] and inherited with `workspace = true`. A
        # separate ruleset because the `astlog-rules` self-test maps one source
        # extension per ruleset (rust.astlog -> .rs, cargo.astlog -> .toml).
        let cargo_files = (owned-files [--hidden --glob Cargo.toml] | lines)
        if ($cargo_files | is-not-empty) {
          astlog scan astlog-rules/cargo.astlog ...$cargo_files
        }
      }
      # The Elixir lint rules (astlog-rules/elixir.astlog), two families. Type
      # discipline: a struct needs a `@type`, a public `def` needs a preceding
      # `@spec` (behaviour callbacks marked `@impl` are exempt), and a module
      # needs a `@moduledoc` — the lint-level nudge toward the shape Elixir
      # 1.18's set-theoretic checker can check. Correctness/security: no unsafe
      # dynamic atom creation (atom-table DoS), no leftover `IO.inspect`. Run
      # over every package's `lib/` Elixir, not a hand-maintained directory list:
      # the only scoping is to `lib/` itself, because `mix.exs` build functions
      # and `test/` ExUnit helpers are not the type-checked runtime surface and
      # speccing them would be noise. `fd` already skips gitignored `_build`/`deps`.
      def "main astlog-elixir" [] {
        let files = (
          owned-files [--extension ex --extension exs]
          | lines
          | where {|p| $p =~ '(^|/)lib/' }
        )
        if ($files | is-not-empty) {
          astlog scan astlog-rules/elixir.astlog ...$files
        }
      }
      # The shell fence (#3823, phase 1): no NEW generated shell or nushell.
      # Call sites of the write*Application / write*Script builders (matched
      # as AST identifiers by astlog-rules/shell-fence.astlog, so comments
      # never count) and committed .sh/.bash/.nu files are frozen behind
      # shell-allowlist.txt: an entry is `path` for a script file or
      # `path:identifier:count` for the call sites in one .nix file. A
      # scanned occurrence with no matching entry fails (write a compiled
      # Rust tool instead), and an entry whose target shrank or vanished
      # fails too, so the allowlist only shrinks as scripts migrate to Rust.
      # Counts rather than line numbers so unrelated edits that shift lines
      # do not churn the allowlist, while a new call site in an already
      # listed file still trips the fence.
      def "main shell-fence" [] {
        let allowed = (
          open --raw shell-allowlist.txt
          | lines
          | each {|line| $line | str trim }
          | where {|line| $line != "" and not ($line | str starts-with "#") }
        )
        let scripts = (
          owned-files [
            --hidden --type file --exclude .git
            --extension sh --extension bash --extension nu
          ]
          | lines
        )
        let nix_files = (owned-files [--extension nix] | lines)
        # `astlog scan` exits nonzero on findings by design; capture the JSON
        # (an empty corpus still prints `[]`) instead of failing the stage.
        let scan = (do { astlog scan astlog-rules/shell-fence.astlog --json ...$nix_files } | complete)
        if ($scan.stdout | str trim | is-empty) {
          print --stderr $scan.stderr
          error make { msg: "astlog scan emitted no JSON" }
        }
        let call_sites = (
          $scan.stdout
          | from json
          | group-by {|f| $"($f.file):($f.text)" }
          | transpose site findings
          | each {|row| $"($row.site):($row.findings | length)" }
        )
        let actual = ($scripts | append $call_sites | sort)
        let new = ($actual | where {|entry| $entry not-in $allowed })
        let stale = ($allowed | where {|entry| $entry not-in $actual })
        if ($new | is-not-empty) {
          print --stderr "new shell/nushell is fenced (#3823); write a compiled Rust tool instead (ix.rustWorkspace; see packages/config-launch, packages/claude-hooks). Not in shell-allowlist.txt:"
          $new | each {|entry| print --stderr $"  ($entry)" }
        }
        if ($stale | is-not-empty) {
          print --stderr "stale shell-allowlist.txt entries (script gone or call-site count changed); the allowlist only shrinks, update it:"
          $stale | each {|entry| print --stderr $"  ($entry)" }
        }
        if (($new | is-not-empty) or ($stale | is-not-empty)) { exit 1 }
      }
      # Repository configuration belongs in composable Nix expressions. Keep
      # serialized files only where an external consumer owns the filename or
      # the file is generated data, a lock, a fixture, or a protocol payload.
      def "main filenames" [] {
        let allowed = [
          # Ecosystem-owned configuration and manifests.
          '(^|/)Cargo\.toml$'
          '(^|/)pyproject\.toml$'
          '(^|/)rust-toolchain\.toml$'
          '(^|/)mise\.toml$'
          '(^|/)osv-scanner\.toml$'
          '(^|/)ruff\.toml$'
          '(^|/)statix\.toml$'
          '(^|/)\.cargo/config\.toml$'
          '^clone\.toml$'
          '^complexity\.toml$'
          '^packages/cve-scan/whitelist\.toml$'
          '^\.github/.*\.ya?ml$'
          '(^|/)docker-compose\.ya?ml$'
          '(^|/)plugin\.yml$'
          '^\.editorconfig$'
          '^packages/minecraft/minestom/servers/[^/]+/gradle\.properties$'
          '^packages/minecraft/minestom/servers/[^/]+/gradle/verification-metadata\.xml$'
          '^packages/minecraft/minestom/servers/[^/]+/src/main/resources/logback\.xml$'
          # Gradle owns these root-build names; the catalog and verification
          # metadata are generated inputs shared by the Minestom subprojects.
          '^packages/minecraft/minestom/gradle\.properties$'
          '^packages/minecraft/minestom/gradle/libs\.versions\.toml$'
          '^packages/minecraft/minestom/gradle/verification-metadata\.xml$'
          '^packages/minecraft/minestom/gradle/snapshot-metadata\.xml$'
          # Zellij owns this external layout syntax; Loom passes the immutable
          # file straight to Zellij rather than interpreting it as ix config.
          '^packages/loom/layout\.kdl$'

          # Generated manifests, locks, editor settings, and typed data.
          '(^|/)(package|tsconfig)\.json$'
          '(^|/)(package-lock|lock)\.json$'
          '(^|/)(pins|manifest)\.json$'
          '^\.(claude|vscode|zed)/settings\.json$'
          '^\.vscode/extensions\.json$'
          '^\.github/user-owners\.json$'
          '(^|/)(dag|upstream-status)\.json$'
          '(^|/)(fixtures?[^/]*|snapshots?|catalogs?|metadata|sounds|seeds)/.*\.json$'
          '^examples/.*\.json$'
          '^packages/claude-code/system-prompts/models\.json$'
          # Byte-patch mapping data (find/replace pairs) consumed as JSON
          # store paths by claude-code-rainbow's patch-binary.py.
          '^packages/claude-code-rainbow/mappings/[^/]+\.json$'
          '^packages/system-prompt-eval-viewer/src/sample\.json$'
          # URL + hash pin data for the system-card PDF corpus (repo pin
          # policy keeps fetch hashes in data, not inline nix).
          '^packages/system-cards/catalog\.json$'
          '^packages/code-highlight/src/islands-theme\.json$'
          # Generated by `tree-sitter generate` and embedded by the grammar
          # crate's lib.rs (see packages/tree-sitter-nix/README.md).
          '^packages/tree-sitter-nix/src/node-types\.json$'
          '^tests/.*\.json$'
        ]
        let candidates = (
          owned-files [
            --hidden --type file
            --extension toml --extension json --extension yaml --extension yml
            --extension kdl --extension ini --extension conf --extension cfg --extension xml
            --extension properties --extension editorconfig --extension sobelow-conf
            --exclude .git
          ]
          | lines
        )
        let denied = ($candidates | where {|path| not ($allowed | any {|pattern| $path =~ $pattern})})
        if ($denied | is-not-empty) {
          print --stderr "prefer .nix for repository-owned configuration; serialized files require an external filename or generated/data role:"
          $denied | each {|path| print --stderr $"  ($path)" }
          exit 1
        }
      }
      # A grouping directory must never restate its parent's name — the
      # directory-tree form of the scopedNaming rule. The one occurrence,
      # packages/minecraft/minecraft/{bot,nbt,...}, was flattened into
      # packages/minecraft (b32885d); this stage keeps the doubled segment
      # from coming back. Scoped to consecutive duplicates in the grouping
      # hierarchy only: a package root (a `package.nix` or `default.nix`
      # marker, the same markers packages/registry.nix discovers by) and
      # everything beneath it is exempt, because an eponym package inside
      # its area (packages/nix) is deliberate and language layouts
      # inside a package (the mcp server's Python src/slack/slack) repeat a
      # segment by convention. Non-consecutive repeats (foo/bar/foo) are
      # fine and stay out of scope.
      def "main dirnames" [] {
        let offenders = (
          owned-files [--type directory . packages]
          | lines
          | where {|dir| ($dir | path basename) == ($dir | path dirname | path basename) }
          | where {|dir|
              let segments = ($dir | path split)
              let enclosing = (1..($segments | length) | each {|n| $segments | first $n | path join })
              not ($enclosing | any {|scope|
                ["package.nix" "default.nix"] | any {|marker| ($scope | path join $marker | path exists) }
              })
            }
        )
        if ($offenders | is-not-empty) {
          print --stderr "grouping directory restates its parent's name; flatten the child into its parent:"
          $offenders | each {|dir| print --stderr $"  ($dir)" }
          exit 1
        }
      }
      # Safari never evaluates `prefers-color-scheme` (or `light-dark()`)
      # inside an SVG loaded via `<img>` (WebKit bug 199134), so a
      # self-adapting SVG embedded bare renders its light palette for Safari
      # readers on GitHub's dark theme. Any markdown embed of such an SVG must
      # go through a `<picture>` whose dark source points at a committed
      # `-dark.svg` twin. The gate below enforces that pair.
      def "main svg-dark" [] {
        let offenders = (
          owned-files [--hidden --extension md --exclude .git --exclude .claude]
          | lines
          | each {|md|
              let dir = ($md | path dirname)
              let body = (open --raw $md)
              $body
              | parse --regex '(?:src="|\]\()(?<ref>[^"()\s]+\.svg)'
              | each {|row| $row.ref }
              | uniq
              | each {|ref|
                  let svg = (if ($ref | str starts-with '/') { $ref | str substring 1.. } else { $dir | path join $ref })
                  let dark_ref = ($ref | str replace --regex '\.svg$' '-dark.svg')
                  let dark_svg = ($svg | str replace --regex '\.svg$' '-dark.svg')
                  let adaptive = (($svg | path exists) and ((open --raw $svg) =~ 'prefers-color-scheme|light-dark\('))
                  let covered = (($body | str contains $'srcset="($dark_ref)"') and ($dark_svg | path exists))
                  if ($adaptive and (not $covered)) { $"  ($md) embeds ($ref)" } else { null }
                }
              | compact
            }
          | flatten
        )
        if ($offenders | is-not-empty) {
          print --stderr "Safari ignores prefers-color-scheme inside img-loaded SVGs (WebKit bug 199134); embed via <picture><source media=\"(prefers-color-scheme: dark)\" srcset=\"<hero>-dark.svg\"><img src=\"<hero>.svg\"></picture> with a committed dark twin:"
          $offenders | each {|line| print --stderr $line }
          exit 1
        }
      }
      # Site content invariants, front-loaded into `nix run .#lint`.
      #
      # The SvelteKit build is the full validator and the Check gate DOES run
      # it: `site` is in `packageSet`, so `cachePushRoots` carries it and
      # `requiredGateRoots` exposes it as `closure-site`, which `check
      # required` builds on every PR and every push. What it cannot be is
      # early. This repo pushes straight to main, so on a direct push that
      # verdict arrives minutes after the commit is already public -- which is
      # exactly how #3669 (duplicate plan number 0031) and ENG-9863 (a
      # dangling in-page link) each reached main and stayed there. The stages
      # here are the ones cheap enough to run before the push instead.
      #
      # Identity (#3669): within each content directory a four-digit filename
      # prefix is claimed at most once, frontmatter `id` equals the filename
      # stem (stems are unique per directory, so ids stay unique), and a
      # frontmatter `number` agrees with the prefix (the #3668 rename fixed
      # the collision but left the old `number` behind, which kept the site
      # build red).
      #
      # Referential integrity (ENG-9863): every absolute in-page link
      # resolves to a route the site prerenders. Updates are routed at
      # `/<id>`; one `.svx` linking `/updates/<id>` failed `ix-site` on three
      # consecutive main merges. The crawler does catch it, but names the
      # linking ROUTE -- "404 /updates/committed-ix2nix-wasm (linked from
      # /ifd-ix2nix-wasm/)" -- and never the file to edit, so this stage
      # reports path, line, and a suggested route instead.
      #
      # The route inventory is derived, never listed. Static routes are the
      # directories under src/routes owning a `+page`/`+server` file; a
      # dynamic `[id]` directory is expanded with the ids of whichever
      # `$lib/<collection>` its own loader imports. Add a route, rename a
      # collection, or delete a page and the inventory follows. A committed
      # route list would drift, and a link checker that passes a 404 because
      # its inventory is stale is worse than no checker.
      def frontmatter-field [front: list<string>, key: string] {
        let matches = ($front | parse --regex ('^' + $key + ': *(?<value>.+)$'))
        if ($matches | is-empty) { null } else {
          $matches | get 0.value | str trim | str trim --char "'"
        }
      }
      # Every path the site prerenders, as a route key (see `route-key`).
      def site-routes [] {
        let routes_root = "packages/site/src/routes"
        let lib_root = "packages/site/src/lib"
        let owners = (
          owned-files [--type file '^\+(page\.svelte|page\.ts|server\.ts)$' $routes_root]
          | lines
          | each {|file| $file | path dirname }
          | uniq
          | each {|dir|
              {
                dir: $dir
                rel: ($dir | str replace $routes_root "" | str trim --left --char '/')
              }
            }
        )
        let static_routes = (
          $owners
          | where {|owner| not ($owner.rel | split row '/' | any {|seg| $seg =~ '^\[.+\]$' }) }
          | get rel
        )
        # A `[param]` directory serves one page per entry of the collection
        # its loader imports, so read the import rather than assume the name:
        # `/rfcs/[id]` and `/plans/[id]` both draw from `$lib/plans`.
        let generated = (
          $owners
          | where {|owner| ($owner.rel | path basename) =~ '^\[.+\]$' }
          | each {|owner|
              let prefix = ($owner.rel | path dirname | str replace --regex '^\.$' "")
              let loader = ($owner.dir | path join "+page.ts")
              let collections = (
                if ($loader | path exists) {
                  open --raw $loader
                  | parse --regex '\$lib/(?<name>[A-Za-z0-9_-]+)'
                  | get name
                  | uniq
                  | where {|name|
                      let dir = ($lib_root | path join $name)
                      ($dir | path exists) and (($dir | path type) == "dir")
                    }
                } else { [] }
              )
              $collections
              | each {|name|
                  owned-files [--extension svx . ($lib_root | path join $name)]
                  | lines
                  | each {|svx|
                      let stem = ($svx | path parse | get stem)
                      if ($prefix | is-empty) { $stem } else { $"($prefix)/($stem)" }
                    }
                }
              | flatten
            }
          | flatten
        )
        let assets = (
          owned-files [--type file . "packages/site/static"]
          | lines
          | each {|file| $file | str replace "packages/site/static/" "" }
        )
        $static_routes | append $generated | append $assets | uniq
      }
      # One route key per URL: query and fragment dropped, slashes trimmed at
      # both ends, so `/x`, `/x/` and `/x/#top` compare equal and the root is
      # the empty string. Matching ignores the trailing slash deliberately:
      # `trailingSlash = 'always'` makes the slashless spelling a redirect,
      # not a 404, so demanding one here would fail links that work.
      def route-key [link: string] {
        $link
        | split row '#'
        | first
        | split row '?'
        | first
        | str trim --char '/'
      }
      # Lines that could carry an absolute link, minus the ones inside a
      # fenced block, with inline code blanked. An incident write-up quotes
      # the broken link it fixed inside backticks
      # (cache-push-prints-builder-logs.svx does exactly this); the crawler
      # never follows those, so neither does this stage. Candidate lines are
      # selected before anything else runs: a fold over every line of every
      # plan rebuilt an immutable list per line and cost 4s of the lint's
      # 4.5s wall, against 0.1s for the whole stage before that.
      def svx-live-lines [path: string] {
        let all = (open --raw $path | lines)
        # Fence markers pair off, opener to closer; an unclosed final fence
        # runs to end of file, which is how a truncated block still hides.
        let fenced = (
          $all
          | enumerate
          | where {|row| ($row.item | str trim) | str starts-with '```' }
          | get index
          | chunks 2
          | each {|pair|
              {
                start: ($pair | first)
                end: (if ($pair | length) > 1 { $pair | last } else { $all | length })
              }
            }
        )
        $all
        | enumerate
        | where {|row| ($row.item | str contains '](/') or ($row.item | str contains 'href="/') }
        | where {|row| not ($fenced | any {|block| $row.index >= $block.start and $row.index <= $block.end }) }
        | each {|row|
            {
              line: ($row.index + 1)
              text: ($row.item | str replace --all --regex '`[^`]*`' "")
            }
          }
      }
      # Absolute in-page targets on one line: markdown `](/path)` and HTML
      # `href="/path"`. `//host/path` is protocol-relative -- another origin,
      # which the crawler does not follow -- so it is not a route reference.
      def line-links [text: string] {
        [
          ($text | parse --regex '\]\((?<link>/[^)\s]*)')
          ($text | parse --regex 'href="(?<link>/[^"]*)"')
        ]
        | flatten
        | get link
        | where {|link| not ($link | str starts-with '//') }
      }
      # In-page links in the prerendered corpus that resolve to no route.
      # Only `.svx` content is scanned: it is the surface that grows by one
      # file per change, while the components around it route through
      # `resolve()` and are type-checked. Relative links are not checked,
      # because the corpus has none.
      def site-link-errors [] {
        let routes = (site-routes | each {|route| route-key $route })
        [plans updates stories]
        | each {|kind|
            owned-files [--extension svx . $"packages/site/src/lib/($kind)"]
            | lines
            | sort
            | each {|path|
                svx-live-lines $path
                | each {|row|
                    line-links $row.text
                    | each {|link|
                        { path: $path, line: $row.line, link: $link, key: (route-key $link) }
                      }
                  }
                | flatten
              }
            | flatten
          }
        | flatten
        | where {|ref| $ref.key not-in $routes }
        | each {|ref|
            let tail = ($ref.key | split row '/' | last)
            let near = ($routes | where {|route| ($route | split row '/' | last) == $tail })
            let hint = (
              if ($near | is-empty) { "" } else { $"; did you mean /($near | first)/" }
            )
            $"  ($ref.path):($ref.line): ($ref.link) is not a prerendered route($hint)"
          }
      }
      # mdsvex hands an .svx body to the Svelte compiler, which reads `<` as the
      # start of a tag whenever the next character could begin a tag name. A
      # four-space indent does NOT make a code block here -- the renderer emits a
      # `<p>` -- so `is not >= 25 and <= 27` in an indented block compiled to
      # `<p>... <= 27` and failed the whole ix-site build with "Expected a valid
      # element or component name", naming a line number in the generated HTML
      # rather than the source. That is a 20-minute round trip through the
      # flake-check gate for a typo. Fence the block instead.
      #
      # Deliberately narrow: only `<` NOT followed by a letter, `/`, `!` or `{`,
      # and only outside fenced blocks and inline code. Real HTML (`<a href=...>`)
      # and inline `` `< 2.0` `` are both legal and stay legal. Verified to flag
      # zero of the pages that build today.
      def svx-tag-errors [] {
        [plans updates stories]
        | each {|kind|
            owned-files [--extension svx . $"packages/site/src/lib/($kind)"]
            | lines
            | sort
            | each {|path|
                open --raw $path
                | lines
                | enumerate
                | reduce --fold {fenced: false, errs: []} {|row, acc|
                    let line = $row.item
                    if ($line | str trim | str starts-with '```') {
                      {fenced: (not $acc.fenced), errs: $acc.errs}
                    } else if $acc.fenced {
                      $acc
                    } else {
                      let bare = ($line | str replace --all --regex '`[^`]*`' "")
                      if ($bare =~ '<[^A-Za-z/!{]|<$') {
                        {fenced: $acc.fenced, errs: ($acc.errs | append $"  ($path):($row.index + 1): unfenced `<` is read as a tag -- ($line | str trim)")}
                      } else {
                        $acc
                      }
                    }
                  }
                | get errs
              }
            | flatten
          }
        | flatten
      }
      def "main site-ids" [] {
        let errors = (
          [plans updates stories]
          | each {|kind|
              let entries = (
                owned-files [--extension svx . $"packages/site/src/lib/($kind)"]
                | lines
                | sort
                | each {|path|
                    let stem = ($path | path parse | get stem)
                    let front = (open --raw $path | lines | skip 1 | take until {|line| $line == '---'})
                    {
                      path: $path
                      stem: $stem
                      id: (frontmatter-field $front id)
                      number: (frontmatter-field $front number)
                      prefix: ($stem | parse --regex '^(?<n>\d{4})-' | get -o 0.n)
                    }
                  }
              )
              let id_mismatches = (
                $entries
                | where {|e| $e.id != $e.stem}
                | each {|e| $"  ($e.path): frontmatter id '($e.id)' disagrees with filename '($e.stem)'"}
              )
              let number_mismatches = (
                $entries
                | where {|e| $e.number != null and $e.number != $e.prefix}
                | each {|e| $"  ($e.path): frontmatter number '($e.number)' disagrees with filename prefix '($e.prefix)'"}
              )
              let numbered = ($entries | where {|e| $e.prefix != null})
              let duplicate_prefixes = (
                if ($numbered | is-empty) { [] } else {
                  $numbered
                  | group-by prefix
                  | transpose prefix claimants
                  | where {|row| ($row.claimants | length) > 1}
                  | each {|row| $"  number ($row.prefix) claimed by (($row.claimants | get path | str join ' and '))"}
                }
              )
              let identified = ($entries | where {|e| $e.id != null})
              let duplicate_ids = (
                if ($identified | is-empty) { [] } else {
                  $identified
                  | group-by id
                  | transpose id claimants
                  | where {|row| ($row.claimants | length) > 1}
                  | each {|row| $"  id ($row.id) claimed by (($row.claimants | get path | str join ' and '))"}
                }
              )
              [$id_mismatches $number_mismatches $duplicate_prefixes $duplicate_ids] | flatten
            }
          | flatten
        )
        let link_errors = (site-link-errors)
        let tag_errors = (svx-tag-errors)
        if ($errors | is-not-empty) {
          print --stderr "site content identity is enforced by the SvelteKit build, which on a direct push to main reports after the commit is public (#3669); keep filenames and frontmatter agreeing so collisions fail fast:"
          $errors | each {|line| print --stderr $line } | ignore
        }
        if ($link_errors | is-not-empty) {
          print --stderr "the SvelteKit prerender crawler treats a dangling in-page link as fatal and names the linking route, not the file (ENG-9863); every absolute link must resolve to a prerendered route:"
          $link_errors | each {|line| print --stderr $line } | ignore
        }
        if ($tag_errors | is-not-empty) {
          print --stderr "an unfenced `<` in an .svx body fails the Svelte compile for the WHOLE site, reporting a line in the generated HTML instead of the source:"
          $tag_errors | each {|line| print --stderr $line } | ignore
        }
        if (($errors | is-not-empty) or ($link_errors | is-not-empty) or ($tag_errors | is-not-empty)) { exit 1 }
      }
      # Every `.svx` frontmatter block must be valid YAML. mdsvex parses it with
      # a real YAML parser and, when that fails, emits a page carrying no
      # `metadata` export; packages/site/src/lib/updates.ts then throws at load
      # time and the WHOLE ix-site build dies. That build runs in `Cache push`,
      # not in the required `Check`, so branch protection never sees it and main
      # goes red only after the merge (#4236).
      #
      # The trigger is ordinary typing: a plain YAML scalar may not contain a
      # colon followed by a space, so a link label like `index#4218 (git-guard:
      # mutating commands in a protected checkout)` ends the scalar early and
      # invalidates the mapping. Quoting the label fixes it. Parse the block
      # rather than pattern matching on colons, so tabs, bad indentation and
      # unbalanced brackets are caught by the same gate.
      def "main site-frontmatter" [] {
        let errors = (
          [plans updates stories]
          | each {|kind|
              owned-files [--extension svx . $"packages/site/src/lib/($kind)"]
              | lines
              | sort
              | each {|path|
                  let front = (
                    open --raw $path
                    | lines
                    | skip 1
                    | take until {|line| $line == '---'}
                    | str join "\n"
                  )
                  try {
                    $front | from yaml | ignore
                    []
                  } catch {|err|
                    # `$err.msg` is the generic "Unsupported input"; the parser's
                    # own text, carrying its line and column, sits in `$err.debug`.
                    # Read it from there rather than from the structured field,
                    # which nushell renamed from `json` to `details` between
                    # 0.112 and 0.114 and would silently break this stage on the
                    # next bump.
                    let quoted = ($err.debug | parse --regex 'Could not load YAML: (?<detail>[^"]+)')
                    let detail = (if ($quoted | is-empty) { $err.debug } else { $quoted | get 0.detail })
                    # The block opens on file line 2, so the parser's line N is
                    # file line N + 1. Report as `file:line:column` and drop the
                    # parser's own block-relative coordinates from the text, so
                    # an editor can jump straight to the offending line.
                    let place = ($detail | parse --regex 'at line (?<line>\d+) column (?<col>\d+)')
                    let reason = ($detail | str replace --all --regex ' at line \d+ column \d+' "")
                    let at = (
                      if ($place | is-empty) {
                        $path
                      } else {
                        let line = (($place | get 0.line | into int) + 1)
                        $"($path):($line):($place | get 0.col)"
                      }
                    )
                    [$"  ($at): ($reason)"]
                  }
                }
              | flatten
            }
          | flatten
        )
        if ($errors | is-not-empty) {
          print --stderr "an .svx frontmatter block that is not valid YAML compiles to a page with no `metadata` export, and the site loader throws, failing the whole ix-site build (#4236); a plain YAML scalar may not contain a colon followed by a space, so quote any label that has one:"
          $errors | each {|line| print --stderr $line } | ignore
          exit 1
        }
      }
      # Repo-wide Python lint: the shared ruff selector (bug-catchers + security +
      # pathlib + pytest + explicit annotations + no `typing.cast`; see
      # lib/ruff-ann.nix) over EVERY tracked .py, so non-package dirs
      # (tools/, users/, skills/, sdk/, examples/, lib/) are covered too, not just
      # the per-package build gates. `fd` skips gitignored paths; `.claude` (agent
      # worktrees and assets) is filtered out explicitly.
      def "main ruff" [] {
        let py_files = (
          owned-files [--extension py]
          | lines
          | where {|p| not ($p | str starts-with ".claude/") }
        )
        if ($py_files | is-not-empty) {
          ruff check ${ix.ruffAnnArgs} ...$py_files
        }
      }
      # Code clone detection over the whole tree (packages/clone-detect).
      # `clone .` walks up for the repo `clone.toml`, whose `[budget]
      # global_pct` is the ceiling on whole-scan `duplication_pct`; the binary
      # exits nonzero when the global gate fails, so this gate ratchets
      # duplication down without failing on every pre-existing clone. Only the
      # global gate runs here: the diff gate needs a `.git` directory, and the
      # CI lint derivation copies a `.git`-less source tree. `clone` prints the
      # DetectionResult JSON to stdout; redirect it to null so a failing stage's
      # log shows the tracing gate summary (stderr), not the full JSON blob.
      def "main clone" [] {
        clone . ...$vendored_scan_flags out> /dev/null
      }
      # Per-unit complexity over the whole tree (packages/complexity).
      # `complexity .` walks up for the repo `complexity.toml`, whose
      # `[budget] max_over_threshold` caps how many units may sit at or above
      # their language's committed threshold; the binary exits nonzero when
      # that cap is exceeded, so the count ratchets down without failing on
      # every unit that is already large. Same stdout convention as `clone`:
      # the JSON goes to null so a failing stage's log shows the tracing
      # summary naming the worst units, not the whole report.
      def "main complexity" [] {
        complexity . ...$vendored_scan_flags out> /dev/null
      }
      def main [] {
        error make { msg: "specify a stage: alejandra | statix | deadnix | astlog | astlog-rust | astlog-elixir | shell-fence | filenames | dirnames | svg-dark | site-ids | site-frontmatter | ruff | clone | complexity" }
      }
    '';
  };

  # One stage list drives both the dag spec (default human path) and the
  # `--json` runner inside `lint`, so adding a stage cannot update one path
  # and silently miss the other.
  lintStages = [
    "alejandra"
    "statix"
    "deadnix"
    "astlog"
    "astlog-rust"
    "astlog-elixir"
    "shell-fence"
    "filenames"
    "dirnames"
    "svg-dark"
    "site-ids"
    "site-frontmatter"
    "ruff"
    "clone"
    "complexity"
  ];

  lintSpec = (pkgs.formats.json {}).generate "lint-dag.json" {
    nodes = lib.genAttrs lintStages (stage: {
      command = [
        (lib.getExe lintStage)
        stage
      ];
    });
  };

  lint = ix.writeNushellApplication pkgs {
    name = "lint";
    meta.description = "Run all Nix formatting and lint checks in parallel via dag-runner; `--fix` applies the fixer lanes to the worktree";
    runtimeInputs = [
      pkgs.git
      repoPackages.dag-runner
    ];
    text = ''
      # nu
      const stages = ${builtins.toJSON lintStages}
      const stage_bin = "${lib.getExe lintStage}"

      def --wrapped main [...args] {
        # `--json` (#1683) emits one JSON document — [{check, ok, output}] —
        # so agents can load lint results as a dataframe instead of grepping
        # the human log. It runs the same stage binary the dag spec points
        # at; dag-runner is bypassed only because its json mode is an NDJSON
        # event stream that drops the captured diagnostics. Exit code matches
        # the dag-runner contract: the worst stage exit code.
        if "--json" in $args {
          if ($args | length) > 1 {
            error make { msg: "--json takes no other arguments" }
          }
          let runs = (
            $stages
            | par-each --keep-order {|stage|
                let r = (do { ^$stage_bin $stage } | complete)
                {
                  check: $stage
                  ok: ($r.exit_code == 0)
                  # `ansi strip` because the stages color their diagnostics and
                  # nushell's `to json` passes raw ESC bytes through unescaped,
                  # which strict parsers (jq) reject as invalid JSON.
                  output: (($r.stdout + $r.stderr) | ansi strip)
                  exit_code: $r.exit_code
                }
              }
          )
          print ($runs | reject exit_code | to json)
          exit ($runs | get exit_code | math max)
        }
        # `--fix` (#3432) applies the fixer lanes to the worktree: build
        # `.#lint-fix-patch` -- the unified diff from the lanes' input
        # snapshot to the composed fixed tree (see `lintFix` below) -- and
        # `git apply` it from the repo root. A patch rather than a copy of
        # the fixed files is a safety property: the snapshot Nix evaluates
        # can be older than the worktree (a linked git worktree evaluates
        # the committed state), and `git apply` is all-or-nothing on context
        # mismatch, so stale fixes refuse loudly instead of clobbering
        # uncommitted edits. `nix` comes from the ambient PATH for the same
        # reason as `.#check`: a pinned client could mismatch the host daemon.
        if "--fix" in $args {
          if ($args | length) > 1 {
            error make { msg: "--fix takes no other arguments" }
          }
          # Both the flake ref and `git apply` paths are root-relative, so
          # anchor at the repo root instead of requiring callers to be there.
          cd (^git rev-parse --show-toplevel | str trim)
          let patch = (
            ^nix ...[
              "build" ".#lint-fix-patch"
              "--no-link" "--print-out-paths"
              "--option" "extra-experimental-features" "ca-derivations"
            ]
            | str trim
          )
          if (open --raw $patch | is-empty) {
            print "lint --fix: tree already clean, nothing to apply"
          } else {
            ^git apply --stat $patch
            ^git apply $patch
            print "lint --fix: applied; the verdict-only stages (astlog, shell-fence, filenames, dirnames, clone) and unfixable findings still need `nix run .#lint`"
          }
          exit 0
        }
        exec dag-runner ...$args ${lintSpec}
      }
    '';
  };

  # Fixer lanes (#3431/#3432): the fixable lint stages recast as pure tree
  # transformers `src -> src'`. A derivation cannot mutate the repo, so
  # "apply the fixes" means: emit the fixed tree, and let each consumer diff
  # it against what it has (`lint --fix` above today, the CI autofix commit
  # of #3435 later). Lanes split by file domain and build concurrently as
  # independent derivations (wall clock = slowest lane); within a lane the
  # stages run sequentially with the formatter LAST, because fix stages emit
  # unformatted edits -- same-lane fixes computed in parallel against the
  # original tree could union cleanly and still fail the format check.
  # Every derivation here is content-addressed, so a no-op fix realises to
  # its input's content and an already-clean tree is cache hits all the way
  # down to an empty patch.
  lintFix = let
    contentAddressed = {
      __contentAddressed = true;
      outputHashAlgo = "sha256";
      outputHashMode = "recursive";
    };
    # The repo pin (rust-toolchain.toml, via lib/rust/tooling.nix) plus the
    # rustfmt component its component list omits. Bound once and exported so
    # the acceptance check below runs `cargo fmt --check` with byte-for-byte
    # the same cargo-fmt the lane fixes with.
    rustFmtToolchain = ix.repoRustToolchainFor pkgs {
      components = [
        "cargo"
        "rustc"
        "rustfmt"
      ];
    };
    # One lane: copy the scoped source, run the lane's fixers in place, emit
    # the tree. The fixers see nothing outside their scoped `src` by
    # construction, so lane outputs stay as disjoint as lane inputs.
    mkLane = {
      name,
      tools,
      fix,
    }: src:
      pkgs.runCommand "lint-fix-${name}"
      (contentAddressed // {nativeBuildInputs = tools;})
      ''
        cp -R ${src} "$out"
        chmod -R u+w "$out"
        cd "$out"
        ${fix}
      '';
    lanes = {
      # Mirrors the alejandra/statix/deadnix check stages' strictness:
      # `deadnix --edit` without -L deletes an unused lambda pattern name
      # outright, and a call site that still passes the attr surfaces in the
      # eval checks -- exactly the manual migration the check stage's comment
      # prescribes. statix discovers statix.toml at the tree root (the nix
      # lane source carries it). deadnix runs before statix because its
      # edits create statix findings the other order leaves behind: deleting
      # a lambda pattern's last unused name leaves `{}:`, which statix's
      # empty_pattern fix rewrites to `_:`; statix fixes never introduce
      # dead code, so this order converges in one pass.
      nix = mkLane {
        name = "nix";
        tools = [
          pkgs.alejandra
          pkgs.deadnix
          pkgs.statix
        ];
        fix = ''
          deadnix --edit .
          statix fix .
          alejandra --quiet .
        '';
      };
      # The same pinned ruff and selector as the `ruff` check stage -- the
      # fix must not widen or narrow the rule set. `--exit-zero` because
      # findings are this lane's input, not its verdict: an unfixable
      # violation must not fail the lane (the check path still reports it),
      # while a real ruff failure (bad config, panic) still exits 2.
      # `--no-cache` keeps `.ruff_cache` out of the output tree (cwd = $out).
      python = mkLane {
        name = "python";
        tools = [pkgs.ruff];
        fix = ''
          ruff check ${ix.ruffAnnArgs} --fix --exit-zero --no-cache .
        '';
      };
      # `cargo fmt` with the repo-pinned nightly (#3433): the exact
      # toolchain the workspace builds with, plus the rustfmt component the
      # root rust-toolchain.toml's component list omits. cargo-fmt discovers
      # targets via `cargo metadata --no-deps`, which parses manifests only:
      # no Cargo.lock, no network, no dependency sources, so the scoped
      # fileset below suffices. CARGO_HOME points at the build temp dir
      # because cargo wants a writable home even for metadata (cwd is $out,
      # which must stay free of cargo's cache). Formatting is the
      # lane's only stage for now; `clippy --fix` (#3434) slots in BEFORE
      # cargo fmt when it lands, per the formatter-last rule above.
      rust = mkLane {
        name = "rust";
        tools = [rustFmtToolchain];
        fix = ''
          export CARGO_HOME="$TMPDIR/cargo-home"
          cargo fmt --all
        '';
      };
    };

    # Scoped lane inputs: only the files a lane's tools read or rewrite. An
    # edit outside a lane's fileset
    # leaves that lane's input (hence, content-addressed, its output)
    # untouched -- the whole-tree cache invalidation fix from #3431. The
    # filesets must stay pairwise disjoint: `unite` treats a path emitted by
    # two lanes as a scoping bug. Unlike the check stages' `fd` walks, these
    # include tracked files under hidden directories (.github): a deliberate
    # superset, since hidden-and-tracked is still shipped code.
    sources = let
      # The vendored roots come out of every lane, for a sharper reason than
      # they come out of the check stages: a lane REWRITES the tree it is
      # given. `lint --fix` running alejandra over an imported upstream copy is
      # the same violation the check stage reports, one commit further along
      # and already written to disk. `deadnix --edit` is worse than cosmetic
      # there -- views/nix/tests/functional/lang holds fixtures that exist to
      # fail parsing, and editing one destroys the case it asserts.
      vendored = fs.unions (map (dir: paths.root + "/${dir}") vendoredDirs);
      laneSource = fileset:
        fs.toSource {
          inherit (paths) root;
          fileset = fs.difference fileset vendored;
        };
    in {
      nix = laneSource (
        fs.union
        (fs.fileFilter (file: file.hasExt "nix") paths.root)
        (paths.root + "/statix.toml")
      );
      # `.claude` mirrors the ruff check stage's explicit filter (agent
      # worktrees and assets); ruff.toml rides along so tree-root discovery
      # inside the lane matches a checkout, though the inline flags already
      # carry the whole policy.
      python = laneSource (
        fs.difference
        (
          fs.union
          (fs.fileFilter (file: file.hasExt "py") paths.root)
          (paths.root + "/ruff.toml")
        )
        (fs.maybeMissing (paths.root + "/.claude"))
      );
      # Everything cargo-fmt reads: sources to rewrite plus every Cargo.toml
      # (workspace membership and per-target discovery both come from
      # manifests). rustfmt.toml is maybeMissing because the repo has none
      # today; listing it here means adding one starts scoping the lane
      # instead of being silently ignored. The toolchain pin itself needs no
      # fileset entry: the lane's toolchain is a nativeBuildInput, so a pin
      # bump already rebuilds the lane.
      rust = laneSource (
        fs.unions [
          (fs.fileFilter (file: file.hasExt "rs") paths.root)
          (fs.fileFilter (file: file.name == "Cargo.toml") paths.root)
          (fs.maybeMissing (paths.root + "/rustfmt.toml"))
        ]
      );
    };

    # Union lane outputs into one tree. Lane filesets are disjoint, so the
    # same path arriving from two lanes is a lane-scoping bug, not a merge
    # to resolve: fail loudly rather than let one lane's fix silently shadow
    # another's.
    unite = name: trees:
      pkgs.runCommand name contentAddressed ''
        mkdir -p "$out"
        for tree in ${toString trees}; do
          (cd "$tree" && find . -type f -print0) |
            while IFS= read -r -d "" file; do
              rel="''${file#./}"
              if [ -e "$out/$rel" ]; then
                echo "lane union conflict on $rel (from $tree): lane filesets must be disjoint" >&2
                exit 1
              fi
              mkdir -p "$out/$(dirname "$rel")"
              cp "$tree/$rel" "$out/$rel"
            done
        done
      '';

    fixed = unite "lint-fixed" (lib.mapAttrsToList (name: lane: lane sources.${name}) lanes);

    # The artifact `lint --fix` consumes: one unified diff from the lanes'
    # input snapshot to the fixed tree. Symlinking the trees as `a`/`b`
    # makes the hunk headers `a/<path> b/<path>`, exactly what `git apply`'s
    # default -p1 strips; absolute /nix/store labels would not.
    patch = pkgs.runCommand "lint-fix.patch" contentAddressed ''
      ln -s ${unite "lint-fix-input" (lib.attrValues sources)} a
      ln -s ${fixed} b
      status=0
      diff -ruN a b > "$out" || status=$?
      # 0 = trees identical (empty patch: already clean); 1 = fixes to
      # apply; anything else is a diff failure.
      [ "$status" -le 1 ]
    '';
  in {
    inherit lanes unite fixed patch rustFmtToolchain;
  };

  # `check` is the full CI gate as one repo-owned command: check.yml runs
  # `nix run .#check`, so the same two steps run in CI and locally from a single
  # definition. It targets x86_64-linux explicitly because that is the system CI
  # builds for; a linux runner can only pure-eval the cross-platform darwin
  # images, and that cross-eval was most of what made the old single-threaded
  # `nix flake check` slow. `nix` is taken from the ambient PATH on purpose
  # (this is always invoked as `nix run .#check`, so the host's daemon-matched
  # nix is already present); pinning a client nix here could mismatch the host
  # Nix 2.34.x daemon.
  #
  # Step 1 (nix-fast-build) builds every `ciChecks.x86_64-linux` derivation: it
  # evaluates with nix-eval-jobs (parallel) and streams each drv into a build
  # pool as it resolves. --skip-cached drops paths already in a substituter (a
  # warm run does almost no work), --no-nom keeps plain logs, --no-link leaves no
  # result symlinks. It exits nonzero iff a build or eval fails: that is the gate.
  # --eval-workers 16 with --eval-max-memory-size 6144 is a headroom guard rail
  # (above nix-eval-jobs' 4 GiB default per worker, below the old 8 GiB), not a
  # workaround: the per-crate check split (see the `checks` block below) keeps
  # each worker's eval bounded by the largest single crate. Both binaries are
  # nix-fast-build is the repo-built nixpkgs 1.5.0 package with a patch that
  # makes --skip-cached skip a `local` (warm-store) output, not just a remotely
  # `cached` one. nix-eval-jobs is built against the fleet daemon's stable Nix
  # 2.34 protocol family rather than nixpkgs' moving default. The eval
  # cache is disabled for the parallel evaluator: all workers share one
  # per-flake SQLite database, so writes contend and can fail with "database is
  # busy" without providing useful hits on a fresh commit. See the $fast_build
  # and $eval_jobs comments below.
  #
  # Step 2 (nix-eval-jobs) is the schema/eval gate over the package outputs,
  # broader than the `checks` set step 1 built. nix-eval-jobs is the same
  # parallel evaluator nix-fast-build wraps; run eval-only over
  # packages.x86_64-linux it spreads per-attribute eval across 16 workers and
  # realizes IFD (the `site` import-npm-lock source) on demand. Each worker is a
  # full evaluator that can grow to the 4 GiB-per-worker cap and the host runs
  # many CI jobs at once, so 16 both bounds memory and already collapses the eval
  # toward the slowest single attribute (warm store, eval-cache off: 342s at 1
  # worker, 75s at 16, 70s at 32). The eval cache is off because it is keyed per
  # commit (it never hits on a fresh CI commit) and parallel workers otherwise
  # contend writing the same per-commit sqlite ("database is busy"). The
  # resulting cold per-commit re-eval is tracked separately; a flake eval cache
  # would amortize it. nix-eval-jobs
  # reports a per-attribute eval failure as a JSON `error` line and still exits 0,
  # so the gate is the error-line check; a startup or lock failure exits nonzero
  # and aborts the run (Nushell propagates external failures like bash
  # `set -o pipefail`). Uses the repo-built nix-eval-jobs directly by store path
  # rather than `nix run`.
  #
  # `check required` is the required PR path. It builds the namespaced union of
  # ciChecks and cachePushRoots through one 16-worker evaluator pool, then runs
  # the package schema gate. This replaces two competing self-hosted claims
  # without running two 16-worker clients side by side (which has OOM-killed a
  # 96 GiB runner before). `check closure` remains the manual closure probe.
  #
  # Refused at evaluation when this flake carries no guest Nix (`ix.nixPackage`
  # is null: index evaluated standalone, which is what the public mirror's
  # check.yml does). Every mode below forces image closures -- `main` through
  # `ciChecks` (`eval`, `base-image-nix-db`, `nvim-startup`) and the package
  # schema pass over `base`/`vcfs-guest-eval`, `main required` through every
  # `closure-*` root of `requiredGateRoots` -- and each of those throws
  # `modules/profiles/base: no guest Nix was injected` when forced. Letting the
  # gate run would spend a 16-worker evaluator pool discovering that one
  # refusal per image, then report the images as failed checks. One refusal
  # here names where the gate actually runs instead.
  check =
    if ix.nixPackage == null
    then
      throw ''
        packages.${system}.check: this flake was evaluated with no guest Nix (`ix.nixPackage` is null), and every mode of `check` forces image closures that need it (`ciChecks.eval`, `packages.base`, every `closure-*` root of `requiredGateRoots`).

        index cannot assemble that Nix on its own (lib/default.nix `nixPackage`: the jj tree ABI archive the fork links lives in the ix repository). ix instantiates this flake with it (`index.withNixPackage`, ix nix/flake/outputs/workspace.nix `index`) and its required gate absorbs `requiredGateRoots.x86_64-linux` as the `index-*` jobs of `packages.x86_64-linux.required-ci-checks`. Run the gate there, or evaluate one root through ix's `index` flake output.
      ''
    else
      ix.writeNushellApplication pkgs {
        name = "check";
        meta.description = "Run CI gates: default checks, `required` checks plus publishable closure, or `closure` only";
        text = ''
          # Patched nix-fast-build (packages/nix-fast-build): stock --skip-cached
          # only skips a job whose nix-eval-jobs cacheStatus is `cached` (in a remote
          # substituter); a `local` output (already in this warm runner's store but
          # never pushed) falls through and is re-realized every run. On this CI the
          # rust units and image closures are floating-CA and resolve to `local`, so
          # the patch makes --skip-cached skip `local` too. nixpkgs' 1.5.0 tag is the
          # same commit (7f185e0) the flake ref used to pin, so this is a like-for-like
          # source swap plus the patch. Invoked directly by store path, not `nix run`.
          const fast_build = "${lib.getExe repoPackages.nix-fast-build}"
          # nix-eval-jobs is linked to the stable Nix 2.34 components the fleet
          # daemon runs. Built for x86_64-linux (the CI gate system); `check` itself
          # is x86_64-linux-only.
          const eval_jobs = "${lib.getExe repoPackages.nix-eval-jobs}"

          # Shared build gate: build every derivation under $flake with
          # nix-fast-build and exit 1 on any failure, after replaying each failed
          # build's log. `main` runs it over ciChecks, `main required` over the
          # namespaced union of checks and cache-push roots, and `main closure` over
          # cache-push roots alone.
          def build-gate [flake: string] {
            # ca-derivations: the rust workspace units default to
            # `contentAddressed = true` (lib/rust/cargo-unit.nix), so evaluating
            # the target set resolves floating content-addressed drvs. The
            # evaluator (nix-eval-jobs, which nix-fast-build wraps) needs the
            # `ca-derivations` experimental feature, or it aborts with
            # "experimental Nix feature 'ca-derivations' is disabled". The caller
            # owns cache policy: developers may accept the flake config, while
            # self-hosted CI ignores its restricted cache settings. Pin only the CA
            # feature here so nested evaluator processes remain self-contained.
            # --result-format json --result-file emits one record per attr per phase
            # ({attr, type: EVAL|BUILD, duration, success, error, outputs}) into the
            # cwd. blast-radius consumes this on a later PR via `--timings` to
            # annotate the rebuilt-checks list with wall-clock seconds. The path is
            # relative to the runner cwd; check.yml uploads it as an artifact.
            # nix-fast-build prints "Cannot build <drv>" for a failed check but not the
            # build's own output, so a clippy lint or a test panic surfaces only as a
            # bare "build exited with 1" with no diagnostic to act on. Catch the
            # failure, then replay each failed build's log via `nix log` so the actual
            # clippy/test output lands in the CI log. The failed attrs are read from
            # the --result-file this just wrote (one {attr,type,success,...} record
            # per attr per phase); it is written even on failure.
            # `try` returns false on success and the `catch` returns true, so the
            # failure is carried in an immutable binding (nushell forbids mutating an
            # outer `mut` from inside the catch closure).
            let build_failed = (
              try {
                ^$fast_build ...[
                  "--flake" $flake
                  # Drive nix-fast-build with the daemon-family-compatible
                  # evaluator rather than its nixpkgs default.
                  "--nix-eval-jobs" $eval_jobs
                  "--eval-max-memory-size" "6144"
                  "--eval-workers" "16"
                  "--skip-cached"
                  # Stop scheduling new checks as soon as one fails (in-flight
                  # builds still finish). Default nix-fast-build behavior is to
                  # build every remaining check and only report at the end, which
                  # spends the full wall time before flake-check goes red (#2128).
                  # The failed-attr log replay below still works: the result file
                  # is written on failure with the records collected so far.
                  "--fail-fast"
                  "--no-nom"
                  "--no-link"
                  "--result-format" "json"
                  "--result-file" "check-results.json"
                  "--option" "eval-cache" "false"
                  "--option" "extra-experimental-features" "ca-derivations"
                ]
                false
              } catch {
                true
              }
            )

            if ("check-results.json" | path exists) {
              let failed = (
                open check-results.json
                | get results
                | where type == "BUILD" and success == false
              )
              for f in $failed {
                # GitHub Actions log group so a long clippy dump stays collapsible;
                # harmless plain text in a local `nix run .#check`.
                print --stderr $"::group::build log: ($f.attr)"
                let inst = $"($flake).($f.attr)"
                # Fast path: replay the retained build log via `nix log` (works for
                # input-addressed checks like the browser smoke test).
                let drv = (
                  ^nix eval --raw
                    --option extra-experimental-features ca-derivations
                    $"($inst).drvPath"
                  | complete
                )
                let logged = if $drv.exit_code == 0 and (($drv.stdout | str trim) | is-not-empty) {
                  ^nix log ($drv.stdout | str trim) | complete
                } else {
                  { exit_code: 1, stdout: "" }
                }
                if $logged.exit_code == 0 and (($logged.stdout | str trim) | is-not-empty) {
                  print --stderr $logged.stdout
                  # The tail as an annotation too: raw log downloads are blocked
                  # from automation, and the checks API only carries annotations.
                  let tail = (
                    $logged.stdout | lines | last 10 | str join " | " | str substring 0..600
                  )
                  print $"::error title=($f.attr) build log tail::($tail)"
                } else {
                  # A content-addressed build (the rust units default to CA) keeps
                  # its log under the *resolved* drv, which `nix log` cannot fetch by
                  # the original -- so re-run the one failed check with -L to stream
                  # the diagnostic (clippy lint / test output). nix does not cache
                  # failures, so this just re-attempts that single check.
                  let rebuilt = (do {
                    ^nix build ...[
                      $inst
                      "-L"
                      "--no-link"
                      "--option" "extra-experimental-features" "ca-derivations"
                    ]
                  } | complete)
                  print --stderr $rebuilt.stdout
                  print --stderr $rebuilt.stderr
                  let tail = (
                    $"($rebuilt.stdout)\n($rebuilt.stderr)"
                    | lines | where {|l| ($l | str trim) | is-not-empty }
                    | last 10 | str join " | " | str substring 0..600
                  )
                  print $"::error title=($f.attr) build log tail::($tail)"
                }
                print --stderr "::endgroup::"
              }
              # One workflow error annotation per failed attr (EVAL and BUILD),
              # carrying the recorded error text. check.yml cats this log to the
              # step's stdout on failure, where the runner parses `::error::`
              # lines into check-run annotations -- the only failure surface
              # reachable when raw log downloads are blocked (annotations ride
              # the checks API). Harmless plain text in a local run.
              let annotated = (
                open check-results.json
                | get results
                | where success == false
              )
              for f in $annotated {
                let err = (
                  ($f | get -o error | default "")
                  | str replace --all "\n" " | "
                  | str substring 0..500
                )
                print $"::error title=($f.attr) ($f.type)::($err)"
              }
            }

            if $build_failed {
              exit 1
            }
          }

          def eval-package-schema [] {
            let tmp = (mktemp --directory --tmpdir "ix-check.XXXXXX")
            let report = ($tmp | path join "flake-schema-eval.jsonl")
            do --capture-errors {
              ^$eval_jobs ...[
                "--flake" ".#packages.x86_64-linux"
                "--workers" "16"
                "--gc-roots-dir" ($tmp | path join "flake-schema-eval-gc")
                "--option" "eval-cache" "false"
                # See the ca-derivations note above: the package set also resolves
                # content-addressed rust units, so this eval needs the feature too.
                "--option" "extra-experimental-features" "ca-derivations"
              ]
            } | tee { save --raw --force $report }

            # nix-eval-jobs exits 0 even when an attribute fails to evaluate, so this
            # error-line check is the gate; a nonzero exit already aborted above. The
            # report is left in place on failure for inspection.
            if (open --raw $report | lines | any {|line| $line | str contains '"error":' }) {
              print --stderr "flake schema evaluation failed; see the error lines above"
              exit 1
            }
            rm --recursive --force $tmp
          }

          def main [] {
            build-gate ".#ciChecks.x86_64-linux"
            eval-package-schema
          }

          # Required PR/merge-group gate. One nix-fast-build invocation evaluates
          # and builds both check roots and publishable closure roots with the same
          # bounded pool; a second package-schema pass retains the broader eval gate.
          def "main required" [] {
            build-gate ".#requiredGateRoots.x86_64-linux"
            eval-package-schema
          }

          # Pre-merge closure gate (closure-gate.yml, #1873): the same build gate
          # over the roots the post-merge cache-push linux lane publishes, darwin
          # cross closure included -- the set #2690 broke while flake-check stayed
          # green, back when packages were eval-gated only.
          #
          # That last clause is history, not the current state, and it read as
          # current for long enough to mislead: `main required` above builds
          # `requiredGateRoots`, which carries the package closures, so a linux
          # package IS built on every pull request. Believing otherwise argues for
          # holding a pin bump that the required gate already covers.
          #
          # What this lane still uniquely covers is darwin. The required gate is
          # `x86_64-linux` only and this workflow is `workflow_dispatch:`, so no
          # automatically triggered run builds a darwin closure. A flake-input bump
          # therefore gets linux assurance from its own pull request and darwin
          # assurance from nobody unless someone dispatches this.
          #
          # --skip-cached keeps it O(changed): on the warm-store pool only drvs new
          # relative to main's already-built closure realise.
          def "main closure" [] {
            build-gate ".#cachePushRoots.x86_64-linux"
          }
        '';
      };

  updateMods = ix.writePythonApplication pkgs {
    name = "update-mods";
    src = paths.tools.updateMods;
    pyChecker = "zuban";
    # pydantic validates Modrinth API responses at the boundary so upstream
    # drift fails with a path-precise error rather than a bare KeyError.
    python = pkgs.python314.withPackages (ps: [ps.pydantic]);
    meta.description = "Regenerate Minecraft mod catalogs";
  };

  updateLoaders = ix.writePythonApplication pkgs {
    name = "update-loaders";
    src = paths.tools.updateLoaders;
    pyChecker = "zuban";
    # pydantic validates the PaperMC fill v3 response at the boundary so upstream
    # drift fails with a path-precise error rather than a bare KeyError.
    python = pkgs.python314.withPackages (ps: [ps.pydantic]);
    meta.description = "Refresh Minecraft loader (Paper / Velocity / Fabric) catalogs from upstream";
  };

  ixShellSyncIgnored = ix.writePythonApplication pkgs {
    name = "ix-shell-sync-ignored";
    src = paths.tools.ixShellSyncIgnored;
    pyChecker = "zuban";
    runtimeInputs = [
      pkgs.git
      pkgs.gnutar
    ];
    meta.description = "Copy git-ignored files into an ix shell workspace";
  };

  # `nix run .#cve-scan`: scan the whole Nix closure of the repo's key outputs
  # One symlink-free directory holding every skill under `skills/`, ready to
  # copy into `.claude/skills`.
  skillsDir = ix.skills.mkSkillsDir {inherit pkgs;};

  # The `index` Claude Code plugin: every index skill bundled for `--plugin-dir`,
  # invoked as `/index:<skill>`. This is the pure-index default (no hooks, no
  # personal skills); a consumer wanting extras calls `ix.claudePlugin.mkPlugin`
  # with `extraSkills`/`hooks` directly.
  claudePluginDir = ix.claudePlugin.mkPlugin {
    inherit pkgs;
    name = "index";
  };

  # Declarative subagents rendered to a symlink-free `.claude/agents` directory.
  # Keep this outside the Claude plugin: plugins namespace subagent names, but
  # hooks and skills call these by bare `subagent_type` (`code-reviewer`, etc.).
  agentDefinitions = import (paths.packagesRoot + "/agent/subagents.nix") {
    inherit
      ix
      lib
      repoPackages
      ;
  };
  agentsDir = ix.agents.mkAgentsDir {
    inherit pkgs;
    agents = agentDefinitions.renderedAgents;
    inherit (agentDefinitions) rawFiles;
  };

  mcSource = ix.writeNushellApplication pkgs {
    name = "mc-source";
    text = builtins.readFile paths.tools.mcSource;
    runtimeInputs = [
      (pkgs.callPackage packageRegistry.byId.vineflower.path {inherit ix;})
    ];
    meta.description = "Decompile a Minecraft server jar with Mojang mappings via Vineflower";
  };

  updateSounds = ix.writeNushellApplication pkgs {
    name = "update-sounds";
    text = builtins.readFile paths.tools.updateSounds;
    meta.description = "Refresh the pinned Minecraft sound pack in packages/minecraft/sound";
  };

  # The indexbench CLI built for this system, fed to `mkBenchSuite` and the
  # `apps.bench` perf job. Also surfaced as `packages.indexbench` through the
  # registry; this binding just avoids re-resolving the package set here.
  inherit (repoPackages) indexbench;

  # The reproducible alloc-count bench binary from the shared workspace graph.
  # It installs the counting allocator and prints an `@bench name=allocations`
  # line, so its metric is deterministic and gateable as a flake check.
  indexbenchAllocDemo = ix.cargoUnit.selectBinaryWithTests ix.rustWorkspace.units {
    binary = "indexbench-alloc-demo";
    packageName = "indexbench";
    includeTestCases = false;
    meta.mainProgram = "indexbench-alloc-demo";
  };

  # The repo's own demonstration suite: a trivial macro command, run through the
  # framework end to end. `nix run .#bench` invokes this perf job. Consumers add
  # their own suites the same way via `ix.mkBenchSuite`. The `allocCheck` wires
  # the reproducible alloc-count bench into a flake check.
  indexbenchSelfDemo = ix.mkBenchSuite pkgs {
    name = "self-demo";
    inherit indexbench;
    macros = [
      {
        name = "true";
        command = "true";
      }
    ];
    allocCheck = {
      bench = lib.getExe indexbenchAllocDemo;
      # The demo makes exactly 64 heap allocations by construction (see
      # packages/indexbench/src/bin/alloc-demo.rs), so this budget is an exact,
      # toolchain-stable constant; any added allocation trips the gate.
      budgets.allocations = 64;
    };
  };

  # `paths.site` is the git-filtered `site` subtree input (a store copy, not
  # a local path), so no fileset scoping applies to it; the input already
  # scopes source identity to the subtree.
  siteSrc = paths.site;

  siteTests = ix.buildNpmVitest pkgs {
    pname = "ix-site";
    version = "0.1.0";
    src = siteSrc;
    preTest = ''
      node node_modules/@sveltejs/kit/src/cli.js sync
    '';
  };

  repoPackages = ix.packageSetFor pkgs;
  inherit (repoPackages) site;

  # One general updater for every content source in the repo, run in parallel
  # via dag-runner (the same engine `lint` uses). The Minecraft catalog and
  # sound updaters are fixed apps; the pinned prebuilt-binary updaters
  # (claude-code, yc, ...) are discovered from the registry `updateScript` flag,
  # so adding such a package joins this set with no change here. The nodes are
  # independent (each writes its own source files: mod/loader/sound catalogs or
  # packages/<id>/manifest.json), so they run concurrently. dag-runner fails the
  # run if any node exits non-zero, so a bad signature or fetch error surfaces
  # in CI. Each updater writes relative to the repo root, so `update` must run
  # from the repo root.
  updatableEntries = packageRegistry.updateScriptEntriesFor system;
  updaterFor = entry: let
    pkg =
      lib.attrByPath entry.packageSet.attrPath
      (throw "update: package `${entry.id}` is flagged `updateScript = true` but is absent from the package set for ${system}")
      repoPackages;
  in
    lib.getExe (
      pkg.updateScript
        or (throw "update: package `${entry.id}` is flagged `updateScript = true` but exposes no `passthru.updateScript`")
    );
  updateNodes =
    {
      mods.command = [(lib.getExe updateMods)];
      loaders.command = [(lib.getExe updateLoaders)];
      sounds.command = [(lib.getExe updateSounds)];
    }
    // lib.genAttrs' updatableEntries (
      entry: lib.nameValuePair entry.id {command = [(updaterFor entry)];}
    );
  updateSpec = (pkgs.formats.json {}).generate "update-dag.json" {nodes = updateNodes;};
  # Machine-readable registry view for update.yml's "Build changed packages"
  # step: repo-relative package directory -> the flake attr that builds it on
  # this system. The workflow maps each file the updaters changed to its owning
  # package through this table instead of guessing an attr from path segments,
  # which breaks for nested catalog manifests (#2036). Restricted to entries
  # with a `flake` target enabled here, so a platform-gated updater (dia is
  # aarch64-darwin-only) is absent from the Linux map and gets skipped rather
  # than built as a missing attr.
  updatablePackages = lib.genAttrs' (
    lib.filter (entry: entry.updateScript) (packageRegistry.flakeEntriesFor system)
  ) (entry: lib.nameValuePair "packages/${entry.relativePath}" entry.flake.attrName);
  update = ix.writeNushellApplication pkgs {
    name = "update";
    meta.description = "Refresh every repo content source (Minecraft catalogs + pinned binaries) in parallel via dag-runner";
    runtimeInputs = [repoPackages.dag-runner];
    text = ''
      # nu
      def --wrapped main [...args] {
        exec dag-runner ...$args ${updateSpec}
      }
    '';
  };

  # Cross-compiled standalone packages, exposed as
  # `packages.<host>.<attr>-<triple>` and optionally aliased into native Darwin
  # package namespaces by flake.nix. Linux-only: the Apple (zig + macOS SDK) and
  # Rust target graph run on a Linux build host; Darwin hosts build native
  # packages directly and cannot host this Linux→Darwin lane. Package definitions
  # stay target-agnostic: the cross lane swaps the `ix.rustWorkspace.units`
  # handle underneath them instead of passing a separate cross API.
  darwinTargetsBySystem = {
    aarch64-darwin = "aarch64-apple-darwin";
    x86_64-darwin = "x86_64-apple-darwin";
  };
  targetSystemFor = target:
    if lib.hasSuffix "-apple-darwin" target
    then
      if lib.hasPrefix "aarch64-" target
      then "aarch64-darwin"
      else "x86_64-darwin"
    else throw "cross: unsupported target `${target}`";
  crossEntries = packageRegistry.crossEntriesFor system;
  crossWorkspace = ix.rustWorkspaceFor pkgs;
  # One nixpkgs cross scope per darwin target, shared by every cross entry
  # that builds through upstream nixpkgs packaging (nix-ix) rather than the
  # cargo-unit lane. Lazy: rust-only cross entries never force the
  # instantiation, so the scope costs nothing until a C/C++ cross package
  # is actually evaluated.
  crossNixpkgsByTarget = lib.genAttrs (lib.attrValues darwinTargetsBySystem) (
    target: ix.darwinCrossPkgs pkgs target
  );
  crossIxFor = target: let
    targetWorkspace =
      crossWorkspace
      // {
        units = crossWorkspace.unitsFor {inherit target;};
      };
  in
    ix
    // {
      inherit pkgs;
      cargoUnit = ix.cargoUnitFor pkgs;
      rustWorkspace = targetWorkspace;
      cross = {
        isCross = true;
        inherit target;
        targetSystem = targetSystemFor target;
        # nixpkgs' own cross scope for packages that build through upstream
        # packaging rather than the rust unit graph (nix-ix's modular C++
        # closure). See lib/darwin/nixpkgs-cross.nix.
        pkgs = crossNixpkgsByTarget.${target};
      };
      wrapPackage = wrapperPkgs: args: ix.wrapPackage wrapperPkgs (args // {isCross = true;});
    };
  buildCrossPackage = target: entry:
    lib.callPackageWith (
      pkgs
      // {
        inherit entry repoPackages;
        ix = crossIxFor target;
        writeNushellApplication = ix.writeNushellApplication pkgs;
        updateScriptWriter = ix.writeNushellApplication pkgs;
      }
    )
    entry.path {};
  crossPackages = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
    lib.listToAttrs (
      lib.concatMap (
        entry:
          map (
            target: lib.nameValuePair "${entry.cross.attrName}-${target}" (buildCrossPackage target entry)
          )
          entry.cross.targets
      )
      crossEntries
    )
  );
  # The eval-time IFD closure of each cross target's unit graph. A Mac cannot
  # *build* a Linux→Darwin cross output, but the Darwin package aliases force it
  # to *evaluate* the cross derivation, and that eval imports the rendered
  # `cargo-units.nix` (which is generated from `cargo-unit-graph.json`, itself
  # generated from the vendor dir). Those three are build-time deps of the cross
  # outputs, so `attic push` of the outputs' *runtime* closures never carries
  # them (RFC 0009's substitute-or-nothing trap: #1687). Publishing them lets a
  # Mac substitute the IFD outputs instead of trying to build x86_64-linux drvs
  # at eval; because these are input-addressed drvs, their eval-time out paths
  # are known, so cache-push's probe sees the same paths a Mac's eval demands.
  # Keyed by distinct cross target (the unit graph is shared per target, not per
  # package), derived from `crossEntries` so a new cross target or entry joins
  # this set with no hand-kept list. Same Linux-host gate as `crossPackages`:
  # the cross graphs only build on the Linux host that owns the cross lane.
  crossIfdRoots = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
    let
      crossTargets = lib.unique (lib.concatMap (entry: entry.cross.targets) crossEntries);
      rootsForTarget = target: let
        units = crossWorkspace.unitsFor {inherit target;};
      in
        # These three ARE the whole eval-time closure: the `import unitsNix`
        # forces `unitsNix`, which references only `unitGraphJson` and `vendorDir`
        # (the cargo-lock it also reads is a plain flake source path, always
        # present). `cargo-vendor-config.toml` is not a fourth root: it is a
        # build input of the `unitGraphJson` builder, not on the import path and
        # not in `vendorDir`'s closure, so substituting `unitGraphJson`'s output
        # makes it moot -- the Mac never runs that builder.
        {
          "cross-ifd-${target}-units-nix" = units.unitsNix;
          "cross-ifd-${target}-unit-graph" = units.unitGraphJson;
          "cross-ifd-${target}-vendor-dir" = units.vendorDir;
        };
    in
      lib.mergeAttrsList (map rootsForTarget crossTargets)
  );
  # Producer declarations keep helper discovery out of unrelated package
  # values, especially NixOS toplevels. Package/job membership is unchanged.
  workspacePackageIfdRoots = import ./workspace-ifd-roots.nix {
    inherit lib system packageRegistry;
    isLinux = pkgs.stdenv.hostPlatform.isLinux;
    packages = packageSet;
  };
  darwinPackageAliases = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
    lib.genAttrs (lib.attrNames darwinTargetsBySystem) (
      darwinSystem: let
        target = darwinTargetsBySystem.${darwinSystem};
      in
        lib.listToAttrs (
          lib.concatMap (
            entry:
              lib.optional (entry.cross.exposeNativeDarwin && builtins.elem target entry.cross.targets) (
                lib.nameValuePair entry.cross.attrName crossPackages."${entry.cross.attrName}-${target}"
              )
          )
          crossEntries
        )
    )
  );

  repoFlakePackages = lib.genAttrs' (packageRegistry.flakeEntriesFor system) (
    entry:
      lib.nameValuePair entry.flake.attrName (
        lib.attrByPath entry.packageSet.attrPath
        (throw "packages/${entry.relativePath}/package.nix: flake output `${entry.flake.attrName}` needs packageSet.attrPath")
        repoPackages
      )
  );

  rustPackageTestSets = let
    cargoUnit = ix.cargoUnitFor pkgs;
    rustWorkspace = ix.rustWorkspaceFor pkgs;
    # A crate with a `packageSet` is built through `repoPackages` and carries
    # its own `passthru.tests`. A lib-only workspace crate has no `packageSet`
    # and is not in `repoPackages`, so select its library straight from the
    # shared unit graph (same path ix-vt's default.nix uses). The unit graph is
    # keyed by the Cargo package name (library unit key: dashes underscored),
    # which the registry id need not match (packages/usage/core is id
    # `usage-core`, crate `ix-usage-core`), so read the name from the crate's
    # own manifest.
    packageTestsFor = entry:
      if entry.packageSet != null
      then
        (
          lib.attrByPath entry.packageSet.attrPath
          (throw "packages/${entry.relativePath}/package.nix: passthruTests needs packageSet.attrPath")
          repoPackages
        ).passthru.tests or {
        }
      else let
        cargoPackageName = (lib.importTOML (entry.path + "/Cargo.toml")).package.name;
      in
        (cargoUnit.selectLibraryWithTests rustWorkspace.units {
          library = lib.replaceStrings ["-"] ["_"] cargoPackageName;
          packageName = cargoPackageName;
        }).passthru.tests or {
        };
    # Two keyings of the same leaf test derivations:
    #
    #  * `flat` keys each per-#[test] check as its own top-level name
    #    (`<prefix>-<target>-tests-<case>`). This is what the public `checks`
    #    output needs: the flake schema requires every `checks.<system>.<name>`
    #    to be a derivation, so a nested attrset there fails `nix flake check`.
    #
    #  * `sharded` nests each package's checks under one `recurseForDerivations`
    #    attr (`<prefix>.<target>-tests-<case>`). This is what the memory-bounded
    #    CI evaluator (nix-fast-build / nix-eval-jobs / blast-radius) consumes
    #    through the separate `ciChecks` output.
    #
    # Why the sharded shape exists: nix-eval-jobs hands the root attrpath to one
    # worker and forces its child names to recurse. With the flat set, that one
    # worker forces every crate's per-#[test] manifest IFD at once and balloons
    # to tens of GiB, which earlyoom kills on the shared CI host. The nested
    # shape makes the root return cheap per-package names and forces each
    # crate's manifests inside its own worker job, which restarts at the memory
    # cap between packages (ENG-2201). The nested value must stay a thunk:
    # filtering empties (e.g. `tests != {}`) would force every manifest during
    # enumeration and reintroduce the balloon, so empty groups are left in.
    flatPackageChecks = prefix: tests: lib.mapAttrs' (n: t: lib.nameValuePair "${prefix}-${n}" t) tests;
    shardedPackageChecks = prefix: tests: {
      ${prefix} =
        tests
        // {
          recurseForDerivations = true;
        };
    };
    repoEntries = packageRegistry.passthruTestEntriesFor system;
    moduleRustPackages = {
      resource-monitor-stats-writer = cargoUnit.selectBinaryWithTests rustWorkspace.units {
        binary = "resource-monitor-stats-writer";
      };
      sandboxed-agent-egress-check = cargoUnit.selectBinaryWithTests rustWorkspace.units {
        binary = "sandboxed-agent-egress-check";
      };
      sandboxed-agent-launch = cargoUnit.selectBinaryWithTests rustWorkspace.units {
        binary = "sandboxed-agent-launch";
      };
      sandboxed-agent-proxy = cargoUnit.selectBinaryWithTests rustWorkspace.units {
        binary = "sandboxed-agent-proxy";
      };
    };
    # cargoAudit scans the single workspace Cargo.lock against the advisory DB,
    # so it is one lockfile-scoped check (it rebuilds only on a Cargo.lock
    # change, never on a source edit) rather than a per-crate gate. Expose it
    # once instead of aliasing the same derivation onto every crate.
    workspaceAuditTests = lib.optionalAttrs (rustWorkspace.units.policyChecks ? cargoAudit) {
      rust-cargoAudit = rustWorkspace.units.policyChecks.cargoAudit;
    };
    collectRust = group:
      lib.mergeAttrsList (
        map (entry: group entry.passthruTests.prefix (packageTestsFor entry)) repoEntries
        ++ lib.mapAttrsToList (
          packageName: package: group "rust-${packageName}" (package.passthru.tests or {})
        )
        moduleRustPackages
      )
      // workspaceAuditTests;
  in {
    flat = collectRust flatPackageChecks;
    sharded = collectRust shardedPackageChecks;
  };

  # The repo's inline script checks, split out to live next to what they
  # check (#3898). Every file takes only what its checks actually read and
  # builds through the one shared script-check shape (lib/checks.nix), so
  # the success-marker boilerplate exists in exactly one place. Sources are
  # scoped filesets; the whole-tree exception (the lint gate) is documented
  # at its binding in lib/dev/checks.nix.
  mkCheck = (import ./checks.nix {inherit lib;}).mkScriptCheck {
    inherit pkgs;
    prefix = "check";
  };
  repoPolicyChecks = import ./dev/checks.nix {
    inherit lib pkgs paths mkCheck lint lintStage lintFix;
    inherit (ix) ruffAnnArgs;
  };
  libUtilChecks = import ./util/checks.nix {
    inherit pkgs mkCheck;
    inherit (ix) formatProvenance artifacts;
  };
  crossDarwinChecks = import ./darwin/cross-checks.nix {
    inherit pkgs mkCheck crossPackages;
  };
  astlogRuleChecks = import (paths.root + "/astlog-rules/checks.nix") {
    inherit lib pkgs mkCheck;
    inherit (repoPackages) astlog;
  };
  agentSurfaceChecks = import (paths.packagesRoot + "/agent/checks.nix") {
    inherit pkgs paths mkCheck skillsDir agentsDir;
  };
  blastRadiusChecks = import (paths.packagesRoot + "/blast-radius/checks.nix") {
    inherit lib pkgs paths mkCheck;
  };
  netTraceChecks = import (paths.packagesRoot + "/net-trace/checks.nix") {
    inherit lib pkgs paths mkCheck;
  };
  scipqlChecks = import (paths.packagesRoot + "/scipql/checks.nix") {
    inherit mkCheck;
    inherit (repoPackages) scipql;
  };
  personalConfigChecks = import (paths.users + "/andrewgazelka/checks.nix") {
    inherit lib pkgs ix mkCheck;
    inherit (repoPackages) nushell;
  };

  # Fork-syntax island: tests/ may use fork-only syntax (underscore digit
  # separators), so a stock-Nix evaluator must hit the gate's install
  # message when a tests-derived check is forced, not a bare parse error
  # (index#3635).
  tests = ix.evaluatorGate.require "tests" (import paths.tests {
    inherit
      nixpkgs
      ix
      paths
      home-manager
      disko
      ;
  });

  exampleFleets = ix.exampleFleetsFor {hostSystem = system;};

  # `users/harivansh-afk/dev` names its environment here rather than being
  # discovered. `exampleFleetsFor` only walks `paths.examples`, and that is a
  # separate `flake = false` subtree, so an example cannot import the
  # `users/harivansh-afk/home.nix` module this environment exists to reuse.
  # Naming it explicitly is what puts its closure in `cachePushRoots` below,
  # which is the difference between `ix apply` substituting a built system and
  # compiling a neovim/Go/Elixir toolchain per VM.
  #
  # `src` is deliberately not passed: it only controls the `/ix` copy for
  # in-guest recursion, and threading the flake source in here would make the
  # cached closure change on every commit to the repo.
  harivanshDevFleet = (ix.mkDevFor system) {
    module = paths.users + "/harivansh-afk/dev/dev.nix";
  };

  # Same fleets with "health-check-" prepended to every external name, so the
  # lifecycle scripts that force-delete VMs by name can never clobber an
  # unrelated production VM that happens to share the example's node name
  # (`nginx`, `factions`, ...). `withNodePrefix` only rewrites plan data, so
  # both surfaces share one NixOS closure evaluation per node instead of
  # evaluating every example fleet twice (ENG-2411).
  healthCheckExampleFleets =
    lib.mapAttrs (
      _name: fleet: fleet.withNodePrefix "health-check-"
    )
    exampleFleets;

  # Surface every example's `ix fleet <sub>` wrapper as a buildable attr.
  # Each example contributes `<example>-{up,health,...}` under
  # `legacyPackages.<system>` (e.g. `nix build
  # .#legacyPackages.x86_64-linux.nginx-lifecycle-up`). Deliberately NOT in
  # `packages`: the example key set needs the `.ix` converter IFD, and
  # `packages` names must stay enumerable by IFD-forbidding evals
  # (index#4087).
  examplePackages = let
    fleetSubs = [
      "up"
      "health"
      "status"
      "logs"
      "replace"
      "switch"
      "diff"
    ];
  in
    lib.concatMapAttrs (
      name: fleet:
        lib.genAttrs' fleetSubs (sub: {
          name = "${name}-${sub}";
          value = fleet.${sub}.overrideAttrs (old: {
            meta =
              (old.meta or {})
              // {
                description = "Run `ix fleet ${sub}` against the ${name} example fleet";
              };
          });
        })
    )
    exampleFleets;

  healthChecks =
    import ./image/health-checks.nix
    {
      inherit lib pkgs;
      inherit (ix) kdl writeNushellApplication;
      dagRunner = repoPackages.dag-runner;
    }
    {
      exampleFleets = healthCheckExampleFleets;
      exampleNames = lib.attrNames exampleFleets;
    };

  baseImage = ix.mkImage {
    modules = [(paths.root + "/images/system/base")];
  };

  vcfsGuestEvalImage = ix.mkImage {
    modules = [(paths.root + "/images/system/vcfs-guest-eval")];
  };

  # Build the check catalog from a rust-package keying. `checks` (flat: one
  # derivation per `checks.<system>.<name>`, required by the flake schema and
  # `nix flake check`) and `ciChecks` (sharded: one `recurseForDerivations` group
  # per package, what the memory-bounded CI evaluator consumes) share the same
  # explicit checks; only the rust keying differs (ENG-2201). The
  # collision guard runs per keying, so producing `ciChecks` only forces the
  # cheap per-package names, never the flat per-#[test] spine.
  catalogFor = rustPackageSet:
    lib.optionalAttrs (system == ix.system) (
      let
        rustChecks =
          {
            cargo-unit-real-workspaces = tests.cargoUnitRealWorkspaces;
            cargo-unit-prebuilt-library = tests.cargoUnitPrebuiltLibrary;
            # Links a dylib crate and runs it across a `dlopen` boundary
            # (ENG-12078); see tests/cargo-unit-dylib.nix.
            inherit (tests) cargo-unit-dylib;
            # `-Zdump-test-names` discovery parity on the fork toolchain
            # (tests/default.nix `cargoUnitForkDiscoveryTest`). First cold run
            # builds packages/rustc-ix once; every later run substitutes it.
            cargo-unit-fork-discovery = tests.cargoUnitForkDiscovery;
            # End-to-end rmeta byte-stability cutoff on the fork toolchain:
            # a comment edit in a leaf crate converges every store path in a
            # 4-crate chain; a signature edit still cascades
            # (tests/default.nix cargoUnitRmetaCutoffTest).
            cargo-unit-rmeta-cutoff = tests.cargoUnitRmetaCutoff;
            sdk-rust-prebuilt = tests.sdkRustPrebuilt;
          }
          // rustPackageSet;
        explicitChecks =
          {
            inherit (tests) eval;
            # Boots a NixOS VM running the minecraft-blocks producer's Paper
            # server and asserts the BlockEvents plugin's onEnable succeeded
            # with no exception (ENG-2186). Paper's paperclip bootstrap is
            # pre-run at build time so the VM never needs the network; see
            # tests/minecraft-blocks-vm.nix.
            minecraft-blocks-vm = tests.minecraftBlocksVm;
            # Boots a NixOS VM running the Minestom spleef example server under
            # `services.minestom` and asserts it serves the Minecraft protocol
            # (readiness log line, open port, real server-list ping); see
            # tests/minestom-spleef-vm.nix.
            minestom-spleef-vm = tests.minestomSpleefVm;
            # Boots a NixOS VM and runs a real generation switch that removes
            # a mount unit, asserting the system bus survives it, activation
            # runs, the new generation is recorded, and the bus can still be
            # reloaded afterwards. Deliberately NOT the exit code, which the
            # test driver makes unreachable for reasons no guest has; the test
            # says why at length. Every other check here reads a generation it
            # would build, and this is the only one that watches a switch into
            # it -- the gap ENG-11080 went through. See
            # tests/switch-stops-a-mount-vm.nix.
            switch-stops-a-mount-vm = tests.switchStopsAMountVm;
            # Boots a NixOS VM and stats the `StateDirectory=` of five
            # hardened units -- static `User=` and `DynamicUser=`, flat and
            # nested, plus a state tree pre-owned by a stale uid -- and
            # asserts every one is writable by its own service. Pins the
            # measurement that refuted ENG-12400's premise: under
            # `PrivateUsers=true` a static user owns its StateDirectory leaf
            # exactly as a dynamic one does, and only the intermediate parent
            # of a nested path is root-owned, identically in both. See
            # tests/hardened-state-directory-vm.nix.
            hardened-state-directory-vm = tests.hardenedStateDirectoryVm;
            # Boots a NixOS VM whose root is wiped on every boot, reboots it,
            # and asserts the machine came back as itself while unwhitelisted
            # state did not. Every other check on `modules/system/ephemeral-root`
            # is eval-time, and eval-time cannot tell a correct-looking
            # `fileSystems` set from one that boots to an emergency shell or
            # quietly keeps nothing. Its own check because it reboots. See
            # tests/ephemeral-root-vm.nix.
            ephemeral-root-vm = tests.ephemeralRootVm;
            # The same module's other wipe mechanism, `rollback.method =
            # "lvmthin"`, which the check above cannot cover: its root has to
            # be a thin LV that already exists when the initrd starts, and
            # `qemu-vm.nix` replaces `fileSystems` wholesale rather than
            # merging into it, so a runNixOSTest machine cannot boot off a
            # layout it did not invent. This one installs a real GPT + LVM thin
            # pool with disko and boots the installed system off it, twice, so
            # the initrd's `lvrename` + `lvcreate --snapshot` runs against a
            # device node `sysroot.mount` already holds a `Requires=` on. See
            # tests/ephemeral-root-lvmthin-vm.nix.
            ephemeral-root-lvmthin-vm = tests.ephemeralRootLvmThinVm;
            # Builds the base OCI archive and asserts its baked nix store DB
            # registers the pinned nixpkgs source as valid, so a fresh VM's first
            # `nix` command does not re-copy the tree through VCFS (ix
            # #1748/#1749/#1815). Its own check because it builds an image.
            base-image-nix-db = tests.baseImageNixDb;
            # Starts the base image's real Neovim wrapper and asserts it comes
            # up with no config error and treesitter attached. Its own check
            # rather than an eval assertion because the config is only a string
            # until something runs it: nvim-treesitter 0.10 deleted the module
            # nvim/plugins/treesitter.lua called, nixpkgs shipped the rewrite,
            # and every guest opened the editor on E5108 with the whole suite
            # green. See tests/default.nix `nvimStartup`.
            nvim-startup = tests.nvimStartup;
            # Holds the `nix.registry.index.to` construction against both
            # shapes of `self` (narHash-bearing git consumption vs the
            # path-locked submodule seam), the boundary that broke twice on
            # 2026-07-22 (index#3981, fixed in #3988). Pure eval; its own
            # check so a regression fails by name, not inside `eval`.
            image-registry-pin = tests.imageRegistryPin;
            # Holds the premise that `unitHardeningDisable` in
            # lib/rust/cargo-unit.nix exists for: glibc refuses to fortify
            # below `-O`, so a dev-profile C dependency probing under `-Werror`
            # cannot configure. Compiles C both ways and fails if either the
            # seam stops being sufficient OR the glibc behaviour stops
            # happening, the second being the one that would otherwise leave
            # the seam as dead code nobody finds. Its own check because it
            # invokes a compiler; the eval-only half that asserts the flag
            # actually reaches the rendered units rides in `eval`. See
            # tests/dev-profile-fortify.nix.
            inherit (tests) dev-profile-fortify;
            # The commented knob/env reference at the Home Manager consumption
            # site (index#3710) is asserted, at eval, against what the
            # claude-code wrapper actually accepts (functionArgs plus
            # passthru.knobDefaults) and against the pinned CLI version, so
            # the reference cannot silently go stale.
            claude-code-knob-reference = import (paths.packagesRoot + "/claude-code/knob-reference-check.nix") {
              inherit lib pkgs;
              claudeCode = repoPackages.claude-code;
              hmModule = paths.packagesRoot + "/agent/home-manager/claude-code.nix";
            };
            # Two wrappers from one package (the ordinary `claude` plus an
            # overridden strict `claude-kernel`) must merge into a single user
            # profile. buildEnv rejects any relative path both provide, so this
            # is what holds every installed path derived from `binName`.
            claude-code-profile-coexist = import (paths.packagesRoot + "/claude-code/profile-coexist-check.nix") {
              inherit pkgs;
              claudeCode = repoPackages.claude-code;
            };
            run-records-session = repoPackages.run.passthru.tests.recordsSession;
            # hive's quality lane through the same shared ix.buildElixirCheck:
            # `mix compile --warnings-as-errors` (Elixir 1.18's set-theoretic type
            # checker) plus format, `mix credo --strict`, and test. The lint half
            # is also astlog-rules/elixir.astlog. See
            # packages/hive/default.nix.
            hive-elixir = repoPackages.hive.passthru.tests.elixir;
            # Deterministic alloc-count gate for indexbench: runs the counting-
            # allocator demo bench once through `indexbench assert` and fails if its
            # allocation count exceeds the declared budget. Reproducible, unlike
            # timing/RSS, so it earns a flake check; the timing/RSS perf job lives
            # under `apps.bench` instead.
            indexbench-self-demo-alloc = indexbenchSelfDemo.check;
            site-test = siteTests.all;
          }
          // agentSurfaceChecks
          // astlogRuleChecks
          // blastRadiusChecks
          // crossDarwinChecks
          // netTraceChecks
          // libUtilChecks
          // personalConfigChecks
          // repoPolicyChecks
          // scipqlChecks;
        checkNameCollisions = lib.intersectLists (lib.attrNames explicitChecks) (lib.attrNames rustChecks);
      in
        assert lib.assertMsg (checkNameCollisions == [])
        "checks: duplicate names across explicit/rust sets: ${lib.concatStringsSep ", " checkNameCollisions}";
          explicitChecks // rustChecks
    );
  packageSet =
    lib.optionalAttrs (system == ix.system) {
      base = baseImage;
      vcfs-guest-eval = vcfsGuestEvalImage;
    }
    // {
      health-checks = healthChecks.dag;
      health-checks-zellij = healthChecks.zellij;
      inherit lint site;
      # Fixer-lane outputs (#3432): `lint-fixed` is the union of the lanes'
      # fixed trees; `lint-fix-patch` is diff(snapshot, fixed), the artifact
      # `nix run .#lint -- --fix` builds and git-applies. An empty patch
      # means the tree is already clean.
      lint-fixed = lintFix.fixed;
      lint-fix-patch = lintFix.patch;
      site-dev = site.passthru.devServer;
      update-mods = updateMods;
      update-loaders = updateLoaders;
      inherit update;
      ix-shell-sync-ignored = ixShellSyncIgnored;
      mc-source = mcSource;
      update-sounds = updateSounds;
      agents = agentsDir;
      skills = skillsDir;
      claude-plugin = claudePluginDir;
      # CI tools are pinned to the flake's nixpkgs so workflows resolve exact
      # executables with `nix build .#<tool>` instead of trusting runner PATH.
      # This avoids depending on a tool being on the runner PATH or a floating
      # `nixpkgs#` registry reference. The self-hosted runner PATH carries
      # coreutils + nix but not findutils, jq, gh, or node, so the bare
      # commands are `command not found` (the now-removed cve-scan run
      # 28598889924 died on exactly that; its ratchet step died the same way on
      # bare `node` in run 29196909666).
      #
      # cache-push is the only remaining consumer and uses attic/jq/findutils.
      # curl, gnutar, igzip-as-gzip, nodejs, coreutils and gh lost their last
      # workflow consumer when the CVE scan was removed; they stay as ordinary
      # flake outputs rather than being deleted in that change, tracked in
      # ENG-11181.
      inherit
        (pkgs)
        attic-client
        coreutils
        curl
        jq
        findutils
        gh
        gnutar
        nodejs
        ;
    }
    // lib.optionalAttrs (system == "x86_64-linux") {
      inherit check;
      # Guarded to the Linux lane with the rest of the NixOS closures: a
      # `system.build.toplevel` only evaluates there. Landing in `packageSet`
      # is what carries it into `cachePushRoots` (via `imagesAsClosures`,
      # which passes a non-image derivation through unchanged), so merges to
      # main build it once and push it to cache.ix.dev.
      harivansh-dev-system = harivanshDevFleet.systemPackages.dev-system;
    }
    // repoFlakePackages
    // crossPackages;
  securityRootRegistry = let
    mkRoot = ix.securityRoots.mkRoot;
    owner = "indexable-inc/index";
    cachePolicy = {
      inherit owner;
      class = "cache-only";
      environment = "none";
      exposure = "none";
      criticality = "low";
      slaHours = 168;
    };
    baseImagePolicy = {
      inherit owner;
      class = "base-image";
      environment = "development";
      exposure = "internal";
      criticality = "medium";
      slaHours = 72;
    };
    # Business exposure is never inferred from package metadata. Add a complete
    # policy here only when a package is known to be deployed or distributed;
    # every unspecified non-image output remains cache hygiene, not exposure.
    securityRootPolicies = {};
    packageEntries =
      lib.mapAttrs (
        name: package: let
          isImage = package ? passthru.toplevel;
          path = package.passthru.toplevel or (lib.getOutput package.outputName package);
          policy =
            if isImage
            then baseImagePolicy
            else securityRootPolicies.${name} or cachePolicy;
        in {
          inherit path;
          root = mkRoot (
            {
              attr = "packages.${system}.${name}";
              inherit name;
            }
            // policy
          );
        }
      )
      packageSet;
    exampleEntries =
      lib.concatMapAttrs (
        fleetName: fleet:
          lib.mapAttrs' (
            node: path: let
              name = "example-${fleetName}-${node}";
            in
              lib.nameValuePair name {
                inherit path;
                root = mkRoot {
                  attr = "exampleFleets.${system}.${fleetName}.systemPackages.${node}";
                  inherit name owner;
                  class = "deployed-service";
                  environment = "development";
                  exposure = "internal";
                  criticality = "medium";
                  slaHours = 72;
                };
              }
          )
          fleet.systemPackages
      )
      exampleFleets;
    entries =
      if pkgs.stdenv.hostPlatform.isDarwin
      then packageEntries
      else packageEntries // exampleEntries;
  in {
    securityRoots = lib.mapAttrs (_: entry: entry.root) entries;
    securityRootPaths = lib.mapAttrs (_: entry: entry.path) entries;
  };

  # The example fan-out keys need the `.ix` converter, and cargo-unit builds
  # that converter from this workspace, so merely enumerating those keys plans
  # every workspace member manifest. The (since retired) jj-views validate
  # lane checked out a sparse tree that deliberately left
  # index/packages/tree-sitter-nix (and every view but jj/nix) unhydrated, so
  # the legacyPackages spine -- which
  # nix-ix's installable resolution probes on every CLI invocation -- died on
  # the absent manifest and blocked every view PR (runs 31014582142 and
  # 31015350748; still the head-of-main failure in run 31144625873; surfaced
  # by #9932's nix-ix advance). Same rationale as the
  # `builtins ? wasm` guard beside it: a tree whose Cargo workspace is
  # incomplete could never build the converter those keys require, so hiding
  # them here loses nothing that tree could evaluate.
  cargoWorkspaceComplete =
    builtins.all
    (member: builtins.pathExists (paths.root + "/${member}/Cargo.toml"))
    (lib.importTOML (paths.root + "/Cargo.toml")).workspace.members;
in {
  packages = packageSet;

  # Non-schema output consumed by update.yml via `nix eval --json`; see the
  # binding above for what it maps.
  inherit updatablePackages;

  # CI-only push roots for cache-push.yml. Two adjustments to `packages` keep the
  # cache useful to `ix apply` while cutting the monolithic `*-oci.tar` archives that
  # dominate the run -- each is one uncompressed blob that never dedups, cold
  # every run since check.yml only eval-validates packages:
  #
  #   1. Every NixOS image is replaced by its `toplevel` closure -- the artifact
  #      `ix apply` substitutes (consumers reconstruct the archive on demand via
  #      streamLayeredImage). Non-image packages, and non-NixOS OCI images (which
  #      expose no `toplevel`), pass through unchanged. See lib/image/oci-layer.nix.
  #   2. The `health-checks{,-zellij}` runners pin every fleet node's
  #      `toplevel` closure as a build dep (lib/image/health-checks.nix); the
  #      per-example `health-check-*` wrappers live in `legacyPackages`, not
  #      here (index#4087). Drop the runner scripts and add the fleet node
  #      `toplevel` closures directly, so the closures the checks drag in
  #      stay cached without pushing the script derivations.
  #   3. The cross lane's eval-time IFD outputs (`crossIfdRoots`): the rendered
  #      `cargo-units.nix`, its `cargo-unit-graph.json`, and the vendor dir a Mac
  #      forces at eval when it substitutes a Darwin cross output. These are
  #      build-time deps of the cross packages, so they are absent from those
  #      packages' runtime closures; adding them as roots is the fix for #1687.
  #      `workspacePackageIfdRoots` extends this to any package that rides a
  #      second `buildWorkspace` (codex's codex-rs; ix2nix-wasm's wasm32
  #      graph, which pre-#4125 scaffolded `ix init` evals still force by
  #      interpolating the package output into `converter`, #4127), whose
  #      own unit graph `crossIfdRoots` -- keyed off the shared
  #      `crossWorkspace` -- does not see.
  #   4. On Darwin hosts, the native lane's eval-time IFD outputs
  #      (`nativeIfdRoots`): the same three unit-graph artifacts as (3) but for
  #      the host's own target, which a Darwin consumer forces at eval when it
  #      evaluates any native wrapper (codex, claude-code) against the workspace
  #      unit graph. Runtime closures never carry them, so without explicit
  #      roots every Darwin consumer re-vendors and re-renders the graph at
  #      eval -- the same trap as (3), for the darwin cache lane (#1890).
  cachePushRoots = let
    # Per-node `health-check-*` lifecycle packages and the two
    # `health-checks{,-zellij}` runners all share the `health-check` prefix.
    isHealthCheck = lib.hasPrefix "health-check";
    # THE GUEST-NIX FENCE. Standalone (`ix.nixPackage == null`) this attrset
    # is the PUBLIC root set: the ci-dispatcher's public-cache-push unit
    # evaluates exactly this attr from the public index subtree and fails
    # closed on any eval-error row. The entries named here (and the example
    # node toplevels below) need the guest Nix to evaluate at all, and the
    # injected nix-ix embeds the team-tier jj tree ABI archive, so their
    # closures must never reach the public cache regardless of evaluability:
    # they are fenced OUT of the standalone set rather than left to throw
    # into a permanently red publisher. With the guest Nix injected (ix's
    # instantiation) they are present, which is what puts the image
    # `closure-*` roots in ix's absorbed required gate. A name list, and
    # deliberately so: a NEW guest-Nix-dependent entry that is not added here
    # turns the public publisher loudly red (it fails closed on the error
    # row) instead of silently shrinking the push set, and that alarm is the
    # review event. Re-admitting an image to the public cache is a tier
    # decision (public-cache-push.nix `rootsAttr`), not an edit here.
    needsGuestNix = [
      "base"
      "check"
      "harivansh-dev-system"
      # Links the assembled fork's C++ components (its default.nix has the
      # class story) -- and its closure carries the team-tier jj tree ABI
      # archive, so it must stay off the public cache like the images.
      "nix-eval-jobs"
      # Its script text embeds the assembled fork client (`getExe`), same
      # class and same tier consequence as nix-eval-jobs.
      "nix-ninja-build-nix"
      # Its dag spec embeds every registry updateScript, each of which runs
      # the assembled fork client (`ix.nixPackageFor`).
      "update"
      "vcfs-guest-eval"
    ];
    publishablePackageSet =
      if ix.nixPackage == null
      then builtins.removeAttrs packageSet needsGuestNix
      else packageSet;
    imagesAsClosures = lib.mapAttrs (_: p: p.passthru.toplevel or p) (
      lib.filterAttrs (name: _: !isHealthCheck name) publishablePackageSet
    );
    # `fleet.systemPackages` keys each node's toplevel as `<node>-system`; the
    # fleet-name prefix keeps nodes sharing a name across fleets distinct.
    # Guest-Nix images like the entries above, so the same fence applies.
    exampleNodeToplevels = lib.optionalAttrs (ix.nixPackage != null) (
      lib.concatMapAttrs (
        fleetName: fleet:
          lib.mapAttrs' (
            node: toplevel: lib.nameValuePair "${fleetName}-${node}" toplevel
          )
          fleet.systemPackages
      )
      exampleFleets
    );
    # Native analog of `crossIfdRoots` (adjustment 4). `crossWorkspace` with no
    # target override IS the host workspace, so these are exactly the drvs a
    # Darwin consumer's eval of the native wrappers imports.
    nativeIfdRoots = lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
      native-ifd-units-nix = crossWorkspace.units.unitsNix;
      native-ifd-unit-graph = crossWorkspace.units.unitGraphJson;
      native-ifd-vendor-dir = crossWorkspace.units.vendorDir;
    };
  in
    # Fleet node toplevels are NixOS closures: on Darwin they can only
    # eval-error (every `<fleet>-<node>` row in the first darwin lane run was
    # an eval failure, run 28762717645), so they stay a linux-lane concern.
    # Alias-shadowed natives (dag-runner, nix-web-monitor) need no exclusion
    # here: the flake grafts `linuxDarwinAliases` over this set, so the darwin
    # lane sees the cross drvs and its system filter drops them.
    if pkgs.stdenv.hostPlatform.isDarwin
    then imagesAsClosures // nativeIfdRoots
    else imagesAsClosures // exampleNodeToplevels // crossIfdRoots // workspacePackageIfdRoots;

  # The policy manifest is safe to `nix eval --json`: derivations live in the
  # separate securityRootPaths output and must be realized before their terminal
  # store paths are trusted.
  inherit (securityRootRegistry) securityRoots securityRootPaths;

  inherit darwinPackageAliases;

  # Flat keying: one derivation per `checks.<system>.<name>`, as the flake schema
  # and `nix flake check` require. The `.#check` gate and blast-radius consume
  # the sharded `ciChecks` instead, so this output is not what CI enumerates.
  checks = catalogFor rustPackageTestSets.flat;
  # Closure build gates, keyed `<fork>.<patch>` (see the binding above). A
  # non-schema output like `ciChecks`, exposed per system so a darwin host can
  # gate-build natively before an upstream PR.
  # Sharded keying for the memory-bounded CI evaluator (nix-fast-build /
  # nix-eval-jobs / blast-radius): each package's per-#[test] checks sit under one
  # `recurseForDerivations` group, so the evaluator lists cheap per-package names
  # at the root and forces each crate's manifest IFD in its own worker job
  # (ENG-2201). Not a `checks.<system>.<name>` output, because a non-derivation
  # there fails the flake schema.
  ciChecks = catalogFor rustPackageTestSets.sharded;

  # `nix fmt` runs alejandra on the paths it is given. A single `-q`
  # (`--quiet`) drops alejandra's informational chatter -- the
  # "Congratulations! Your code complies with the Alejandra style." success
  # line and the rotating "Special thanks ... for being a sponsor of
  # Alejandra" promo -- while still surfacing genuine formatting/parse errors
  # (a second `-q` would suppress those too, which we do not want). The
  # lint-fix `nix` lane above already runs `alejandra --quiet`; this wraps the
  # interactive `nix fmt` entrypoint the same way.
  #
  # Since Nix 2.24, `nix fmt` invokes the formatter from the flake root with
  # NO path arguments (older Nix appended `.`); bare alejandra then reads
  # stdin, sees EOF, and fails with "<anonymous file on stdin>: unexpected
  # end of file" (#4105). Defaulting to `.` needs an arity check, which
  # makeWrapper cannot express and the shell fence (#3823) bars a new shell
  # wrapper from doing, so this is a compiled exec shim
  # (ix.writeRustApplication) over the realized alejandra store path.
  formatter = ix.writeRustApplication pkgs {
    name = "alejandra-quiet";
    text = ''
      fn main() {
          // for .exec(); kept inside main so the astlog Nushell-text
          // heuristic (which keys on a leading `use `) does not fire
          use std::os::unix::process::CommandExt;

          let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
          if args.is_empty() {
              args.push(".".into());
          }
          let err = std::process::Command::new("${lib.getExe pkgs.alejandra}")
              .arg("--quiet")
              .args(args)
              .exec();
          eprintln!("alejandra-quiet: exec alejandra failed: {err}");
          std::process::exit(1);
      }
    '';
    meta.description = "alejandra for `nix fmt`: quiet, and formats the whole tree when given no paths";
  };

  # Per-TU content-addressed kernel build (kbuild-unit, #3411), exposed under
  # `legacyPackages` so `nix build .#kernel-unit.vmlinux` resolves while the
  # two-stage IFD plan (a full monolithic kbuild at eval time) stays out of
  # `packages` and every gate closure that enumerates it (flake-check,
  # blast-radius, cache-push). x86_64-linux only: the plan replays gcc/binutils
  # saved commands, so there is no Darwin or cross lane to offer.
  legacyPackages =
    # Example-derived fan-outs: the `<example>-{up,health,...}` fleet
    # wrappers and the `health-check-<example>` lifecycle packages. Their
    # KEY SET requires the `.ix` converter IFD (see exampleFleetsFor), so
    # they live here rather than in `packages`, whose names IFD-forbidding
    # evals enumerate (index#4087). The `health-checks{,-zellij}` runners
    # keep their static names in `packages`.
    #
    # Guarded on `builtins ? wasm`: nix's installable resolution probes
    # `legacyPackages.<system>.<attr>` for every `nix run` / `nix build` /
    # `nix eval` against this flake -- even when the attr already resolves
    # in `packages` -- and the probe forces this set's key set. On a stock
    # evaluator (no `builtins.wasm`) the converter's guard throw therefore
    # killed EVERY CLI invocation against the flake, including the
    # `nix run .#nix-ix` bootstrap `ix apply` / `ix eval` rely on: the
    # evaluator needed to read the flake was only obtainable by reading the
    # flake (regression from 9228563b, which turned the fan-out keys
    # wasm-derived). A stock evaluator only loses attrs whose values it
    # could never evaluate anyway; nix-ix sees the full set.
    lib.optionalAttrs (builtins ? wasm && cargoWorkspaceComplete) (
      examplePackages
      // healthChecks.lifecyclePackages
    )
    // {
      #   nix eval .#legacyPackages.<system>.rustUnits.<crate-version-hash>.drvPath
      # Every rustc unit in the workspace graph, keyed `<crate>-<version>-<hash>`.
      # Surfaced so a derivation-input audit can read a third-party crate's drv
      # path straight off the flake instead of reconstructing the workspace by
      # hand: perturb an input, re-evaluate, and diff the paths to see exactly
      # which units that input invalidates (ENG-10672). IFD-backed, hence
      # `legacyPackages`.
      rustUnits = (ix.rustWorkspaceFor pkgs).units.units;
    }
    // lib.optionalAttrs (system == "x86_64-linux") {
      kernel-unit = (ix.kernelUnitFor pkgs).buildKernel {
        inherit (pkgs.linux_6_12) src;
      };
      # defconfig-scale lane (#3412): thousands of units per eval, kept here
      # solely so the eval-cost measurement has a stable attr to time.
      kernel-unit-defconfig = (ix.kernelUnitFor pkgs).buildKernel {
        inherit (pkgs.linux_6_12) src;
        configTarget = "defconfig";
      };
      # Static fallback lane (#3413): keeps the ccache plan strategy exercised
      # so a config whose plan-time tooling rejects skeleton stub objects has a
      # validated escape hatch to flip to (strategy selection is per-lane and
      # never auto-detected; Nix cannot catch a failed drv, and a silent flip
      # would hide skeleton regressions).
      kernel-unit-ccache = (ix.kernelUnitFor pkgs).buildKernel {
        inherit (pkgs.linux_6_12) src;
        planStrategy = "ccache";
      };
    };

  # `nix run .#bench` runs the repo's self-demo perf job (timing + RSS + custom
  # metrics, gated on regressions). The flake's package-with-mainProgram
  # convention already gives `nix run .#indexbench` for the bare CLI; this `apps`
  # entry is the named perf-job entry point the framework documents.
  apps = {
    bench = {
      type = "app";
      program = lib.getExe indexbenchSelfDemo.app;
      meta.description = "Run the indexbench self-demo perf suite";
    };
  };

  # `nix develop .#bench` drops into a shell with the bench + profiling tools.
  # tango is already a workspace dependency (built per-crate by cargo-unit); the
  # shell adds the out-of-process profilers a bench author reaches for.
  devShells = {
    default = pkgs.mkShellNoCC {
      packages = [
        repoPackages.astlog
        pkgs.alejandra
      ];
      # ix-vt-sys's build script reads IX_VT_GHOSTTY_LIB_DIR to emit the
      # libghostty-vt link search path, and the ix-vt/tui test binaries dlopen
      # the dylib at runtime. Export the unit graph's own lib dir
      # (lib/rust/workspace.nix) so plain `cargo test -p tui -p ix-vt` works
      # from the devshell (#3118).
      env = let
        inherit (ix.rustWorkspaceFor pkgs) ghosttyLibDir;
      in
        {
          IX_VT_GHOSTTY_LIB_DIR = ghosttyLibDir;
        }
        // lib.optionalAttrs pkgs.stdenv.isDarwin {
          # Plain cargo emits no rpath entry for the dylib, so darwin's loader
          # resolves @rpath/libghostty-vt.dylib via the fallback path.
          DYLD_FALLBACK_LIBRARY_PATH = ghosttyLibDir;
        };
    };

    bench = pkgs.mkShellNoCC {
      packages = [
        indexbench
        pkgs.hyperfine
        pkgs.valgrind
        pkgs.samply
        pkgs.jemalloc
      ];
    };
  };
}
