{
  ix,
  pkgs,
}: let
  # CA realisations are an unstable protocol. Build the evaluator from the
  # same pinned 2.34 component set as the fleet daemon. A separate fetch here
  # previously drifted to Nix master while the daemon stayed on 2.34.7,
  # breaking CA realisation negotiation.
  #
  # Specifically the PATCHED component set, not stock `nixComponents_2_34`:
  # this is the evaluator that parses every repo .nix file in CI
  # (nix-fast-build drives it over ciChecks and the package eval gate), so it
  # must speak the same dialect the patched client and daemon family do --
  # underscore digit separators included. And specifically the ASSEMBLED
  # fork's components (`ix.nixPackageFor`), not `repoPackages.nix-ix`: that
  # recipe's `jjTree` archive lives in the ix repository, so it throws
  # whenever it is forced un-overridden. This package therefore only
  # evaluates with the guest Nix injected, which is why it sits in
  # `needsGuestNix` (lib/per-system.nix) and why ix supplies the evaluator to
  # anything that must run against the standalone tree (the public cache
  # publisher).
  nixPackage = ix.nixPackageFor "packages/nix-eval-jobs";
  package =
    (pkgs.nix-eval-jobs.override {
      nixComponents = nixPackage.passthru.components;
    }).overrideAttrs (old: {
      patches = (old.patches or []) ++ [./rust-session.patch ./live-progress.patch];
      # The worker directly calls the public ixe API, not only nix-cmd.
      buildInputs = (old.buildInputs or []) ++ [nixPackage.nixEvalRs];
      # The public handle ABI must come from the same source as the linked fork.
      # It is not installed by nix-cmd's development output.
      postPatch =
        (old.postPatch or "")
        + ''
          cp ${ix.nixSrc + "/rust/nix-eval-rs/include/ixe.h"} src/ixe.h
        '';
    });

  # The override's real risk is silently relinking against nixpkgs' default
  # Nix family after an update, so the smoke test checks both the executable
  # and its propagated Nix component version -- including the `+ix` build-metadata marker,
  # so a silent fallback to the stock 2.34 components fails here.
  smoke =
    pkgs.runCommand "nix-eval-jobs-smoke"
    {
      nativeBuildInputs = [package];
      strictDeps = true;
    }
    ''
      case ${package.nixComponents.nix-cli.version} in
        2.34.*+ix*) ;;
        *)
          echo "nix-eval-jobs is not linked to the patched 2.34 (+ix) component family" >&2
          exit 1
          ;;
      esac
      help=$(nix-eval-jobs --help 2>&1) || true
      case "$help" in
        *"--check-cache-status"*) ;;
        *)
          echo "nix-eval-jobs --help did not print usage" >&2
          printf '%s\n' "$help" >&2
          exit 1
          ;;
      esac
      ${pkgs.python3}/bin/python3 ${./live-progress-smoke.py} ${package}/bin/nix-eval-jobs
      mkdir -p "$out"
    '';
in
  package.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        tests =
          (old.passthru.tests or {})
          // {
            inherit smoke;
          };
      };
    meta =
      (old.meta or {})
      // {
        description = "nix-eval-jobs built against the patched nix-ix 2.34 components (underscore digit separators)";
        mainProgram = "nix-eval-jobs";
      };
  })
