# Family policy for flox-class runner pools, layered ON platform.nix (the
# flake's `flox` attr imports module.nix + platform.nix + this file; never
# this file alone). Same shape as baml.nix, the reference family layer:
# this file is the DELTA over platform.nix and nothing else. Everything
# platform.nix already states with the same values - cache.ix.dev +
# cache.nixos.org substitution, the nix GC headroom, the gai.conf v4
# preference, the Ubuntu build-essential parity packages, the
# MISE_*_COMPILE pins, stateVersion - is deliberately not repeated here.
# platform.nix's `jobEnvironment` values are `mkDefault`, so this layer
# overrides them in nix where flox needs a different number.
#
# Unlike the baml family, flox needs almost no toolchain baked in: every
# flox CI step runs inside `nix develop`, so the devshell closure carries
# rust, just, pre-commit and the whole bats dependency set (see flox's
# pkgs/flox-cli-tests/default.nix, which already lists bats, expect,
# procps, openssh, podman, ...). What flox DOES need from the image is
# substitution against its own binary cache and honest parallelism for a
# suite tuned on 8-core hosted runners.
{pkgs, ...}: {
  # THE load-bearing line of this file.
  #
  # flox's root flake declares `nixConfig.extra-substituters =
  # ["https://cache.flox.dev"]`, but module.nix pins `trusted-users = ["root"]`,
  # so a job running as the unprivileged `runner` user cannot add a
  # substituter at runtime -- `--accept-flake-config` is silently ignored for
  # untrusted users. Without this, every `nix develop` and every
  # `nix build .#flox*` compiles flox's patched Nix and its Rust workspace
  # FROM SOURCE. Measured on a stock platform template guest: `nix.conf`
  # carries only cache.ix.dev and cache.nixos.org.
  #
  # Substituters are additive across modules, so platform.nix's two entries
  # stay; this appends the third.
  nix.settings = {
    substituters = ["https://cache.flox.dev"];
    trusted-public-keys = [
      "flox-cache-public-1:7F4OyH7ZCnFhcze3fJdfyXYLQw/aV7GEed86nQ7IsOs="
    ];

    # Parallelism honesty for the *nix daemon*, which is where flox's CI
    # spends most of its wall clock (`nix develop`, `nix build .#flox-cli`,
    # `nix run .#flox-cli-tests`). The daemon defaults to `max-jobs = auto`
    # (one per visible core) and `cores = 0` (all visible cores per build):
    # on an ix guest that advertises ~100 vCPUs but boots with ~22 GiB of
    # virtio-mem RAM, that is 100 concurrent derivations each running
    # `make -j100`, which OOMs before it finishes. 4 x 8 is the
    # ubuntu-22.04-8core-class shape flox's suites are tuned against, with
    # headroom for the elastic guest to grow into.
    max-jobs = 4;
    cores = 8;
  };

  services.ix-runner = {
    # Delta over platform.nix's build-essential set (gcc, gnumake, cmake,
    # pkg-config, python3, perl, openssl, git-lfs, glibc.bin are already
    # there; `extraPackages` concatenates across modules, so restating them
    # would put duplicate store paths on the job PATH).
    #
    # Deliberately short. flox is a nix-first repository: the only things
    # listed here are what a step needs BEFORE or AROUND `nix develop`,
    # which cannot come from the devshell it is about to enter.
    extraPackages = [
      # `just` is the entry point of every build/test step
      # (`nix develop --command just build-cli`). It comes from the devshell
      # in practice; kept here so a step that shells out to `just` before
      # entering the devshell still resolves, and so `ix shell` debugging of
      # a stuck lane can drive the same recipes.
      pkgs.just
      # `git describe` in the nix-build lane needs annotated tags, and the
      # bats suites shell out to git constantly. git itself is in
      # module.nix's baseUserland; git-lfs is in platform.nix. `openssh` is
      # neither, and `nix` reaches for `ssh` whenever a flake input or a
      # store URI is ssh-shaped.
      pkgs.openssh
      # `ps`/`pstree` parity: the activation suites inspect process trees.
      # flox-cli-tests carries its own copies, but an operator debugging a
      # hung lane over `ix shell` has no PATH but this one.
      pkgs.procps
      # EVIDENCE-DRIVEN, not a guess. GitHub's ubuntu images ship util-linux;
      # neither module.nix's base userland nor platform.nix does. The
      # cli-unit lane's `manifest_builds_can_depend_on_nef` runs a generated
      # build.bash that pipes through `rev`, and on the first real ix run it
      # failed with "rev: command not found" -> make Error 127 - one failing
      # test out of 1702, entirely an image-parity gap.
      #
      # indexable-inc/flox's `.github/actions/ix-setup` carries a symlink
      # shim that backfills the same binaries out of the guest closure, from
      # when its pool ran the platform default template and could not use
      # this file. This entry is the proper fix; once the pool pins the
      # `flox` attr the shim is redundant and should be deleted.
      pkgs.util-linux
    ];

    # Delta over platform.nix's pins. platform.nix targets baml-class
    # 16-thread lanes; flox's suites are tuned for GitHub's
    # `ubuntu-22.04-8core`, so every parallelism knob drops to 8.
    #
    # These are IMAGE-level defaults and the workflow sets the same values
    # in its own `env:` block, which wins (workflow env overrides unit env).
    # Both on purpose: the workflow copy makes the number deterministic per
    # commit and visible in the diff; this copy keeps a lane that forgets
    # the env block from silently self-sizing off ~100 vCPUs.
    jobEnvironment = {
      CARGO_BUILD_JOBS = "8";
      NEXTEST_TEST_THREADS = "8";
      RUST_TEST_THREADS = "8";
      # `cargo test` honours neither of the two above for its own harness
      # when invoked as `cargo test -- --test-threads`; the bats suites read
      # this one to size their `parallel` fan-out.
      FLOX_TEST_JOBS = "8";
      # Set in setup_suite.bash and in ci.yml's `env:` already; pinned here
      # too so an `ix shell` debugging session behaves like CI.
      FLOX_DISABLE_METRICS = "true";
      # flox's Rust workspace is deeply generic in places; the upstream CI
      # image ships a 8 MiB default stack and rustc has been seen to blow
      # it. Cheap insurance, no cost when unused.
      RUST_MIN_STACK = "67108864";
      # NixOS sources /etc/set-environment from BOTH global shell rc files
      # (bash via the compiled-in SYS_BASHRC /etc/bashrc; zsh via
      # /etc/zshenv, which even NO_GLOBAL_RCS cannot suppress) unless this
      # marker is already set. Runner jobs never pass through a login
      # shell, so without it every test-spawned shell re-runs the session
      # bootstrap: ~20 session variables leak into activation env-diff
      # assertions and PATH is REPLACED wholesale (the generated
      # `export PATH=…` line carries no `$PATH` suffix). Set in ci.yml's
      # workflow `env:` today; pinned here too as the durable home once
      # the pool runs on this template.
      __NIXOS_SET_ENVIRONMENT_DONE = "1";
      # Companion to the marker above: NixOS's /etc/bashrc also sources
      # /etc/profile when __ETC_PROFILE_DONE is unset, and /etc/profile
      # EXPORTS the guard — so even a `bash --noprofile --rcfile` gains one
      # new exported variable via the compiled-in SYS_BASHRC, which leaks
      # into activation env-diff assertions. Bash-only (zsh's guard is
      # unexported). Preset, the re-export is value-identical and cancels.
      __ETC_PROFILE_DONE = "1";
    };
  };
}
