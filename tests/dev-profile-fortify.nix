# Guard for the `unitHardeningDisable` seam in lib/rust/cargo-unit.nix.
#
# Two halves, because either one alone is a guard that cannot fail in the
# direction that matters.
#
# 1. `premiseStillHolds` -- glibc still refuses to fortify below `-O`. This is
#    the fact the seam exists for, and it is the half that goes stale: if a
#    future glibc drops the `#warning`, the seam becomes dead code that nobody
#    would otherwise find. So the test asserts the failure as well as the
#    success, and turns red when the failure stops happening.
#
# 2. `wiringReachesUnits` -- dev- and test-profile cargo-unit graphs actually carry
#    `hardeningDisable`, and a release one does not. Eval-only, so it costs
#    nothing and it fails the moment someone drops the `inherit` that carries
#    the flag from cargo-unit.nix into the rendered units.
#
# What broke without this: tikv-jemalloc-sys runs its two `strerror_r`
# configure probes under `-Werror`, so the fortify warning became an error,
# both probes failed, and configure aborted with `cannot determine return type
# of strerror_r` -- naming neither optimisation nor hardening. hyperion's two
# dev-profile boot gates were red on it for a day while every release gate was
# green.
{
  lib,
  pkgs,
  ix,
}: let
  # The smallest thing that reproduces the class: a translation unit that
  # includes a glibc header, compiled at `-O0` under `-Werror`, which is what
  # an autoconf feature probe does.
  probe = ''
    cat > probe.c <<'CEOF'
    #include <errno.h>
    #include <string.h>
    int main(void) { char buf[100]; char *s = strerror_r(0, buf, 100); return s == 0; }
    CEOF
  '';

  cflags = "-O0 -fPIC -Wall -Werror -D_GNU_SOURCE";

  premiseStillHolds =
    pkgs.runCommandCC "dev-profile-fortify-premise" {
      # Both directions are compiled here, so the derivation itself must not
      # inherit either answer from the ambient stdenv.
      hardeningDisable = ["fortify" "fortify3"];
    } ''
      ${probe}

      # With fortify off -- what the seam produces -- the probe compiles.
      if ! gcc ${cflags} -c probe.c -o off.o 2> off.log; then
        echo "a -Werror probe failed at -O0 with fortify already disabled." >&2
        echo "The seam is not sufficient, or something else in the C flags is." >&2
        cat off.log >&2
        exit 1
      fi

      # With fortify on -- what nixpkgs does by default -- it must still fail,
      # or the seam is compensating for something that no longer happens.
      if gcc ${cflags} -D_FORTIFY_SOURCE=3 -c probe.c -o on.o 2> on.log; then
        echo "glibc no longer refuses _FORTIFY_SOURCE at -O0." >&2
        echo "" >&2
        echo 'This is good news, and it means unitHardeningDisable in' >&2
        echo 'lib/rust/cargo-unit.nix is now dead code. Delete the seam, the' >&2
        echo 'dev-profile branch that sets it, and this test.' >&2
        exit 1
      fi
      if ! grep -q "_FORTIFY_SOURCE requires compiling with optimization" on.log; then
        echo "the -O0 fortify compile failed, but not for the reason this seam" >&2
        echo "exists for. Read the error before trusting the seam:" >&2
        cat on.log >&2
        exit 1
      fi

      mkdir -p "$out"
      cp on.log "$out/fortify-at-O0.log"
    '';

  fixture = ./fixtures/cargo-unit-hello;

  workspaceForProfile = profile:
    ix.cargoUnit.buildWorkspace ({
        src = fixture;
        workspaceRoot = fixture;
      }
      // lib.optionalAttrs (profile != null) {inherit profile;});

  # Any unit will do: the flag is applied to every unit in the graph, and
  # taking the first by name keeps this independent of the fixture's contents.
  someUnit = workspace: let
    names = lib.attrNames workspace.units;
  in
    assert lib.assertMsg (names != [])
    "the cargo-unit fixture produced no units, so this test would compare two empty hardening lists and pass without exercising anything";
      workspace.units.${lib.head names};

  hardeningOf = workspace: (someUnit workspace).hardeningDisable or [];

  devHardening = hardeningOf (workspaceForProfile "dev");
  testHardening = hardeningOf (workspaceForProfile "test");
  releaseHardening = hardeningOf (workspaceForProfile null);

  wiringReachesUnits = assert lib.assertMsg (lib.elem "fortify" devHardening && lib.elem "fortify3" devHardening) ''
    a dev-profile cargo-unit does not carry hardeningDisable = [ "fortify" "fortify3" ].
    Got: ${lib.generators.toPretty {} devHardening}
    The seam in lib/rust/cargo-unit.nix is not reaching the rendered units;
    check that `unitHardeningDisable` is still inherited into `importUnits`
    and still applied in `mkUnit` in the units.nix template.
  '';
  assert lib.assertMsg (lib.elem "fortify" testHardening && lib.elem "fortify3" testHardening) ''
    a test-profile cargo-unit does not disable fortify for unoptimized C probes.
    Got: ${lib.generators.toPretty {} testHardening}
  '';
  assert lib.assertMsg (!(lib.elem "fortify" releaseHardening)) ''
    a release-profile cargo-unit carries hardeningDisable = ${lib.generators.toPretty {} releaseHardening}.
    Fortify must stay on for release: that artifact ships, and `-O` satisfies
    glibc there, so there is nothing to compensate for.
  '';
    pkgs.runCommand "dev-profile-fortify-wiring" {__structuredAttrs = true;} "touch $out";
in {
  inherit premiseStillHolds wiringReachesUnits;
}
