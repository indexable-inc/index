# Blast-radius workflow acceptance (#3898), imported by lib/per-system.nix
# into the per-system check catalog.
{
  lib,
  pkgs,
  paths,
  mkCheck,
}: let
  fs = lib.fileset;
  # Exactly what blast-radius-test reads: the test dir (script + fixtures) and
  # the workflow whose embedded jq it pins, so the check reruns only when
  # either changes instead of on every tracked-file edit (#3895). The source
  # root stays the repo root because the test script resolves the workflow by
  # its repo-relative path.
  testSource = fs.toSource {
    inherit (paths) root;
    fileset = fs.unions [
      ./tests
      (paths.root + "/.github/workflows/blast-radius.yml")
    ];
  };
in {
  # Exercises the trusted half of the blast-radius PR comment: the
  # validate/render jq embedded in its workflow, extracted from the YAML so
  # the test can't drift from what the trusted comment job runs. The
  # report-building logic lives in the `blast-radius` Rust crate and is
  # covered by its own unit tests. See packages/blast-radius/tests/blast-radius-test.sh.
  blast-radius-test = mkCheck "blast-radius-test" {
    nativeBuildInputs = [
      pkgs.bash
      pkgs.coreutils
      pkgs.diffutils
      pkgs.jq
      pkgs.yq-go
    ];
    script = ''
      cp -R ${testSource} source
      chmod -R u+w source
      cd source
      export HOME="$TMPDIR/home"
      mkdir -p "$HOME"
      bash packages/blast-radius/tests/blast-radius-test.sh
    '';
  };
}
