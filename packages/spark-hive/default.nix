# Apache Spark 3.5, the official complete (hadoop3 + Hive) binary distribution,
# packaged self-contained for NixOS and pinned to JDK 17.
#
# Why not nixpkgs `spark`: that package uses the lean `bin-without-hadoop`
# tarball and injects an external hadoop classpath at runtime. It ships no
# `hive-exec`, only `hive-storage-api`. Apache Gluten's
# `HiveTableScanExecTransformer` companion object eagerly `classForName`s Hive's
# ORC and Parquet input-format classes during query planning (no config gate),
# so on the lean build Gluten fails to initialize for any query. This
# distribution bundles hadoop and the full Hive jar set (hive-exec, hive-common,
# hive-metastore, hive-serde, spark-hive, ...), so Gluten and other
# Hive-dependent Spark features work with no external classpath.
#
# JDK 17: Spark 3.5 supports 8/11/17 and Gluten 1.6 supports 8/17; 17 is the
# overlap the spark service module standardizes on. The bin wrappers `--set`
# JAVA_HOME, so this is the JVM Spark actually runs on regardless of the
# caller's environment.
#
# Bump: edit pins.json and run `nix run .#update`, then check the tarball
# against its published `.sha512` on archive.apache.org.
{
  ix,
  lib,
  stdenv,
  fetchurl,
  makeWrapper,
  bash,
  coreutils,
  python3,
  procps,
  tzdata,
  # Writer for `passthru.updateScript` (flake-package path only); null on the
  # overlay path.
  updateScriptWriter ? null,
  # Pinned, not a parameter: a `jdk ? jdk17_headless` arg collides with the
  # `pkgs.jdk` callPackage auto-fills (currently openjdk 21, which Spark 3.5 does
  # not support), silently overriding the default. Spark 3.5 and Gluten 1.6 both
  # support JDK 17.
  jdk17_headless,
  pysparkPython ? python3,
}: let
  # Version + URL and SRI hash live in the sibling pins.json, never inline
  # (repo policy). Bump the version/url in pins.json, then `nix run .#update`
  # re-pins the hash.
  pin = ix.pins.loadPin ./pins.json "spark";
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/spark-hive: updateScript";
    pname = "spark-hive";
    relPath = "packages/spark-hive/pins.json";
  };
in
  stdenv.mkDerivation (finalAttrs: {
    pname = "spark-hive";
    inherit (pin) version;

    src = fetchurl {inherit (pin) url hash;};

    nativeBuildInputs = [makeWrapper];
    strictDeps = true;

    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out"
      mv * "$out/"
      # The distribution's launchers carry `#!/usr/bin/env bash`. Rewrite them to a
      # store bash so they do not depend on an FHS `env`/PATH at runtime (systemd
      # units run with a minimal PATH).
      patchShebangs "$out/bin" "$out/sbin"
      # `find-spark-home` is sourced, not executed, so leave it unwrapped. Every
      # other launcher gets JAVA_HOME pinned plus bash/coreutils/pyspark-Python and
      # `ps` (used by `load-spark-env.sh`) on PATH, so the units are self-sufficient
      # even when Spark's scripts re-exec each other.
      for n in $(find "$out/bin" -type f -executable ! -name "find-spark-home"); do
        wrapProgram "$n" \
          --set JAVA_HOME "${jdk17_headless}" \
          --set TZDIR "${tzdata}/share/zoneinfo" \
          --prefix PATH : "${
        lib.makeBinPath [
          bash
          coreutils
          pysparkPython
          procps
        ]
      }"
      done
      runHook postInstall
    '';

    # Velox (via Gluten) calls `discover_tz_dir`, which needs the IANA tz database;
    # NixOS has none at the FHS `/usr/share/zoneinfo`, so the wrappers point TZDIR
    # at the tzdata store path. Executors inherit it from the worker's environment.
    passthru =
      {
        jdk = jdk17_headless;
      }
      // lib.optionalAttrs (updateScript != null) {inherit updateScript;};

    meta = {
      description = "Apache Spark ${finalAttrs.version}, official hadoop3 + Hive distribution, JDK 17, packaged for NixOS";
      homepage = "https://spark.apache.org/";
      license = lib.licenses.asl20;
      sourceProvenance = [lib.sourceTypes.binaryBytecode];
      platforms = ["x86_64-linux"];
      mainProgram = "spark-submit";
    };
  })
