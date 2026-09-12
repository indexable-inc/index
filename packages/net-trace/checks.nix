# net-trace comment-path acceptance (#4031), imported by lib/per-system.nix
# into the per-system check catalog. The test body lives here as an mkCheck
# recipe rather than a committed script because new standalone shell is
# fenced (#3823); recipes stay the one sanctioned shell surface.
{
  lib,
  pkgs,
  paths,
  mkCheck,
}: let
  fs = lib.fileset;
  # Exactly what the check reads: the fixtures and the workflow whose
  # embedded jq it pins, so it reruns only when either changes. The source
  # root stays the repo root so the workflow keeps its repo-relative path.
  testSource = fs.toSource {
    inherit (paths) root;
    fileset = fs.unions [
      ./tests
      (paths.root + "/.github/workflows/check.yml")
    ];
  };
in {
  # Exercises the trusted half of the net-trace PR comment: the validate and
  # render jq embedded in check.yml, extracted from the YAML so this check
  # cannot drift from what the trusted comment job runs. The good summary
  # must pass validation and render the golden comment byte for byte; each
  # hostile fixture pins one fail-closed rule (host charset, label shape,
  # scheme enumeration, required phases array). The proxy and summary logic
  # live in the `net-trace` Rust crate with their own tests.
  net-trace-test = mkCheck "net-trace-test" {
    nativeBuildInputs = [
      pkgs.bash
      pkgs.coreutils
      pkgs.diffutils
      pkgs.jq
      pkgs.yq-go
    ];
    script = ''
      # Scratch space for every artifact below (extracted jq scripts, summary
      # copies, comment.md). install -m, not cp: fixtures are 0444 store
      # files, and a plain cp of one leaves a read-only destination the next
      # overwrite cannot open (root builders mask this; CI's nixbld does not).
      cd "$TMPDIR"
      fixtures=${testSource}/packages/net-trace/tests/fixtures
      workflow=${testSource}/.github/workflows/check.yml
      yq '.jobs.net-trace-comment.steps[] | select(.name == "Validate summary schema").run' "$workflow" > validate.sh
      yq '.jobs.net-trace-comment.steps[] | select(.name == "Render comment").run' "$workflow" > render.sh

      install -m 0644 "$fixtures/good.json" net-trace-summary.json
      bash validate.sh
      # validate.sh is the workflow step body verbatim, so its rejection
      # message is the literal GitHub Actions "::error::" annotation string
      # (#4031). Redirecting only stderr left that string on stdout, so
      # every one of these five expected rejections printed an
      # alarming, failure-shaped line into an otherwise-passing build;
      # readers skimming an unrelated red run for "error" have twice
      # mistaken this line for the cause. Suppress both streams and rely
      # on the exit status the `if` already checks.
      for bad in bad-host bad-label bad-newline bad-scheme missing; do
        install -m 0644 "$fixtures/$bad.json" net-trace-summary.json
        if bash validate.sh >/dev/null 2>&1; then
          echo "validate $bad: accepted a hostile summary" >&2
          exit 1
        fi
      done

      install -m 0644 "$fixtures/good.json" net-trace-summary.json
      bash render.sh
      diff -u "$fixtures/golden.md" comment.md
    '';
  };
}
