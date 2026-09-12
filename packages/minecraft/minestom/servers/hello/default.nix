{
  ix,
  lib,
  pkgs ? ix.pkgs,
}: let
  fs = lib.fileset;
  minestomRoot = ix.paths.packagesRoot + "/minecraft/minestom";
  src = fs.toSource {
    root = minestomRoot;
    fileset = minestomRoot;
  };
in
  ix.buildGradleFatJar pkgs {
    pname = "minestom-hello";
    version = "0.1.0";
    inherit src;
    gradleBuildTask = ":servers:hello:jar";
    jarPath = "servers/hello/build/libs/minestom-hello-0.1.0.jar";
    mavenSnapshotMetadata = [
      {
        group = "net.minestom";
        # astlog-ignore: pname-with-version (Maven artifact coordinate data, not a derivation)
        name = "minestom";
        version = "master-SNAPSHOT";
        src = minestomRoot + "/gradle/snapshot-metadata.xml";
      }
    ];
    verificationMetadata = minestomRoot + "/gradle/verification-metadata.xml";
  }
