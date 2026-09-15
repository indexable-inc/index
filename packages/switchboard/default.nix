# Switchboard: cross-platform chatrooms (ENG-7479). N platform frontends lower
# messages into ONE canonical IR, a router applies room policy (forwarding
# rules, provenance-based loop prevention, guest disclosure), and fans out to
# N backends; humans, external guests (email), and AI agents are all
# first-class room members.
#
# Packaging follows packages/agent/distiller: pure-Python source copied into a
# pinned interpreter via toPythonModule, a makeWrapper entrypoint, and
# sandbox-run passthru tests -- hermetic pytest over the router/adapters (fake
# transports, no network), an import smoke test, and the strict type gate
# (zuban --strict + ruff ANN) via ix.buildPyStrictCheck.
{
  ix,
  lib,
  pkgs,
}: let
  switchboardSource = builtins.path {
    name = "ix-switchboard-python-source";
    path = ./src;
  };
  # pydantic: the IR models (parse at the boundary, per repo policy). httpx:
  # the Slack adapter's HTTP client (the async client the repo already ships
  # elsewhere); email rides the stdlib smtplib/imaplib, no extra deps.
  pythonDeps = ps: [
    ps.pydantic
    ps.httpx
  ];
  switchboardModule = pkgs.python3.pkgs.toPythonModule (
    pkgs.runCommand "ix-switchboard-python-module"
    {
      strictDeps = true;
      propagatedBuildInputs = pythonDeps pkgs.python3.pkgs;
      meta.description = "Cross-platform chatroom routing: canonical IR, router, slack/email adapters";
    }
    ''
      site="$out/${pkgs.python3.sitePackages}/switchboard"
      mkdir -p "$site"
      cp -r ${switchboardSource}/switchboard/. "$site/"
    ''
  );
  switchboardPython = pkgs.python3.withPackages (ps: pythonDeps ps ++ [switchboardModule]);

  # Strict type + annotation gate (zuban --strict + ruff ANN), the same policy
  # buildUvApplication enforces; the sources resolve pydantic and httpx.
  pyStrict = ix.buildPyStrictCheck pkgs {
    pname = "switchboard";
    pythonSrc = switchboardSource;
    pythonPackages = pythonDeps;
    pythonVersion = pkgs.python3.pythonVersion;
  };

  testPython = pkgs.python3.withPackages (
    ps:
      pythonDeps ps
      ++ [
        switchboardModule
        ps.pytest
      ]
  );
  testsSource = builtins.path {
    name = "ix-switchboard-tests";
    path = ./tests;
  };
  # Hermetic by construction: the e2e tests run rooms over in-memory adapters,
  # the Slack tests over httpx.MockTransport, the email tests over fake
  # SMTP/IMAP transports -- no network, no credentials, sandbox-safe.
  unitTests =
    pkgs.runCommand "ix-switchboard-pytest"
    {
      nativeBuildInputs = [testPython];
      strictDeps = true;
    }
    ''
      export HOME=$TMPDIR
      ${lib.getExe testPython} -m pytest ${testsSource} -q -p no:cacheprovider >stdout 2>stderr || {
        echo "switchboard unit tests failed:" >&2
        cat stdout stderr >&2
        exit 1
      }
      cat stdout
      mkdir -p "$out"
    '';
  importTest =
    pkgs.runCommand "ix-switchboard-import"
    {
      nativeBuildInputs = [switchboardPython];
      strictDeps = true;
    }
    ''
      ${lib.getExe switchboardPython} -c '
      import switchboard
      import switchboard.adapter, switchboard.agent, switchboard.email
      import switchboard.ir, switchboard.memory, switchboard.router, switchboard.slack
      assert switchboard.Router is not None
      print("switchboard-ok", switchboard.__version__)
      ' >stdout 2>stderr || {
        echo "switchboard import test failed:" >&2
        cat stdout stderr >&2
        exit 1
      }
      grep -q '^switchboard-ok' stdout
      mkdir -p "$out"
    '';

  package =
    pkgs.runCommand "ix-switchboard"
    {
      nativeBuildInputs = [pkgs.makeWrapper];
      strictDeps = true;
      meta = {
        description = "Cross-platform chatrooms: slack + email + AI agents over one canonical IR";
        mainProgram = "switchboard";
      };
    }
    ''
      mkdir -p $out/bin
      makeWrapper ${lib.getExe switchboardPython} $out/bin/switchboard \
        --add-flags "-m switchboard"
    '';
in
  package.overrideAttrs (old: {
    passthru =
      (old.passthru or {})
      // {
        python = switchboardPython;
        # The bare importable package and its source tree, for embedding into
        # other interpreters (e.g. bundling into packages/mcp) without
        # duplicating the recipe.
        pythonModule = switchboardModule;
        pythonSource = switchboardSource;
        tests =
          (old.passthru.tests or {})
          // {
            pytest = unitTests;
            import = importTest;
            typecheck = pyStrict;
          };
      };
  })
