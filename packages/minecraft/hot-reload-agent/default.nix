{
  jdk25,
  lib,
  stdenv,
}: let
  fs = lib.fileset;
in
  stdenv.mkDerivation {
    pname = "minecraft-hot-reload-agent";
    version = "0.1.0";
    src = fs.toSource {
      root = ./.;
      fileset = fs.unions [
        ./MANIFEST.MF
        ./src
      ];
    };

    strictDeps = true;
    nativeBuildInputs = [jdk25];

    # The tree ships no tests for the agent; the jar is plain javac + jar
    # packaging of a single class.
    doCheck = false;

    buildPhase = ''
      # shell
      runHook preBuild

      mkdir -p classes
      javac --release 21 -d classes $(find src -name '*.java' -print)
      # `jar` stamps every entry with the current wall-clock mtime, so this
      # input-addressed derivation's NAR content varies per build. That trips
      # "hash mismatch importing path" whenever two machines cache different
      # variants of the same store path. Pin every entry to the zip-epoch
      # minimum (DOS time is 2s-resolution from 1980) for a bit-identical jar.
      jar --create --file minecraft-hot-reload-agent.jar --manifest MANIFEST.MF \
        --date "1980-01-01T00:00:02Z" -C classes .

      runHook postBuild
    '';

    installPhase = ''
      # shell
      runHook preInstall

      install -Dm0644 minecraft-hot-reload-agent.jar \
        "$out/share/minecraft-hot-reload-agent/minecraft-hot-reload-agent.jar"

      runHook postInstall
    '';

    meta = {
      description = "Minecraft development hot-reload Java agent";
      license = lib.licenses.mit;
    };
  }
