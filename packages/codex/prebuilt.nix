# Prebuilt native artifacts that codex's v8 build script would otherwise download
# from the internet at build time. The nix sandbox has no network, so they are
# pinned here as fixed-output derivations and handed to the build script through
# the two env vars it already consults (RUSTY_V8_ARCHIVE, RUSTY_V8_SRC_BINDING_PATH).
#
# v8 enters codex's graph only through `codex-code-mode-runtime` (the
# `codex-code-mode-host` binary), which enables `v8_enable_sandbox` (implies
# `v8_enable_pointer_compression`). The build script names its prebuilts by that
# feature set, `..._ptrcomp_sandbox_release_<target>`, and denoland/rusty_v8
# publishes no sandbox variant, so upstream codex builds its own and hosts them
# under the openai/codex release `rusty-v8-v<version>` (the same pair
# scripts/codex_package/v8.py and MODULE.bazel consume). Both files must come
# from one release: the binding is generated against that archive.
#
# Keyed by the *target* system, not the build host: a Linux->Darwin cross build
# of codex must fetch the Darwin pair. The version tracks codex's Cargo.lock:
#   - `v8` crate 150.4.0 -> openai/codex release rusty-v8-v150.4.0
# Refresh alongside a Codex view update: read the new v8 version out of
# codex-rs/Cargo.lock (`name = "v8"`), fetch that release's
# `rusty_v8_ptrcomp_sandbox_release_<target>.sha256` manifests (one line per
# artifact, sha256 hex) and convert each with
# `nix hash convert --hash-algo sha256 --to sri <hex>`.
{
  fetchurl,
  runCommand,
}: targetSystem: let
  onlyKnown = attr: name:
    attr.${targetSystem}
    or (throw "codex prebuilt ${name} has no pin for target system ${targetSystem}");

  rustyV8Version = "150.4.0";
  rustyV8Profile = "ptrcomp_sandbox_release";
  releaseUrl = "https://github.com/openai/codex/releases/download/rusty-v8-v${rustyV8Version}";
  rustcTarget = onlyKnown {
    x86_64-linux = "x86_64-unknown-linux-gnu";
    aarch64-linux = "aarch64-unknown-linux-gnu";
    aarch64-darwin = "aarch64-apple-darwin";
  } "rustcTarget";

  # The static archive. The v8 build script consumes the gzipped `.a` directly
  # through RUSTY_V8_ARCHIVE (the same store path shape nixpkgs' codex feeds it),
  # so no decompression step here.
  rustyV8Archive = fetchurl {
    name = "librusty_v8-${rustyV8Version}-${rustyV8Profile}-${targetSystem}.a.gz";
    url = "${releaseUrl}/librusty_v8_${rustyV8Profile}_${rustcTarget}.a.gz";
    hash = onlyKnown {
      x86_64-linux = "sha256-o1x10fJuapg4haRbM0kKTr5U8FBQVosyuJz7QhswtYM=";
      aarch64-linux = "sha256-0VF+7UBUaFNwKbAF1f6ZfsdNXI01H5FrOm3yC30oEbo=";
      aarch64-darwin = "sha256-AK27SHmISMd1UEQcaGc6XoUpuOG3PqvN7iMss5tA9KE=";
    } "librusty_v8 hash";
  };

  # The bindgen output matching the archive. Without RUSTY_V8_SRC_BINDING_PATH
  # the build script looks for `gen/src_binding_<profile>_<target>.rs` inside
  # the crate, which ships every denoland variant but no sandbox one, and then
  # tries to download it.
  rustyV8SrcBinding = fetchurl {
    name = "src_binding-${rustyV8Version}-${rustyV8Profile}-${targetSystem}.rs";
    url = "${releaseUrl}/src_binding_${rustyV8Profile}_${rustcTarget}.rs";
    hash = onlyKnown {
      x86_64-linux = "sha256-dyeCauR5vbZF6Acjn7EtH44uI956bPFvXuWSaQ0dhQY=";
      aarch64-linux = "sha256-dyeCauR5vbZF6Acjn7EtH44uI956bPFvXuWSaQ0dhQY=";
      aarch64-darwin = "sha256-ylrfDPicmnCtRgrnNkiy/om3SqETs8t/dXtqArdYOU8=";
    } "src_binding hash";
  };
in {
  librustyV8 = rustyV8Archive;
  srcBinding = rustyV8SrcBinding;

  # Decompressed `librusty_v8.a` in its own dir, for the v8 crate compile's
  # `-L native=` search. The v8 build script does write a decompressed copy, but
  # under `build_dir()` (an ancestor of OUT_DIR) which does not cross cargoUnit's
  # per-unit boundary, so the compile cannot see it. rustc needs the file named
  # exactly `librusty_v8.a` to satisfy `-l static=rusty_v8`.
  librustyV8Lib = runCommand "librusty_v8-${rustyV8Version}-${rustyV8Profile}-${targetSystem}-lib" {__structuredAttrs = true;} ''
    mkdir -p "$out"
    gzip -dc ${rustyV8Archive} > "$out/librusty_v8.a"
  '';
}
