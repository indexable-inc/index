{
  lib,
  root,
  # `findDuplicates` from lib/util/lists.nix, threaded in by callers (this file
  # is the package-registry bootstrap, so it cannot reach the assembled `ix`).
  findDuplicates,
}: let
  relativePath = path: lib.removePrefix "${toString root}/" (toString path);

  childDirs = dir: let
    entries = builtins.readDir dir;
  in
    map (name: dir + "/${name}") (
      lib.filter (name: entries.${name} == "directory") (builtins.attrNames entries)
    );

  dirsWithFile = fileName: dir: let
    entries = builtins.readDir dir;
    here = lib.optional ((entries.${fileName} or null) == "regular") dir;
  in
    here ++ lib.concatMap (child: dirsWithFile fileName child) (childDirs dir);

  packageDirs = dirsWithFile "package.nix" root;
  defaultPackageDirs = dirsWithFile "default.nix" root;
  packageDirsWithoutMetadata =
    lib.filter (
      dir: !(builtins.pathExists (dir + "/package.nix"))
    )
    defaultPackageDirs;

  allowedMetadataKeys = [
    "cross"
    "flake"
    "id"
    "inRustWorkspace"
    "isolatedFeatures"
    "mirror"
    "overlay"
    "packageSet"
    "passthruTests"
    "path"
    "pyExtension"
    "updateScript"
    "workspaceIfdRoots"
  ];

  assertKnownKeys = label: allowedKeys: value: let
    unknownKeys = lib.subtractLists allowedKeys (builtins.attrNames value);
  in
    assert lib.assertMsg (
      unknownKeys == []
    ) "${label}: unsupported keys: ${lib.concatStringsSep ", " unknownKeys}"; value;

  # Optional, system-scoped target descriptor: null/false disables it, true takes
  # the package id as the selector, and an attrset overrides the named selector
  # key and/or `systems`. packageSet selects by `attrPath`, flake/overlay by
  # `attrName`.
  normalizeTarget = {
    name,
    key,
    default,
    extraKeys ? [],
  }: label: id: value:
    if value == null || value == false
    then null
    else if value == true
    then {
      ${key} = default id;
      systems = null;
    }
    else
      assertKnownKeys "${label}: ${name}" (
        [
          key
          "systems"
        ]
        ++ extraKeys
      )
      value
      // {
        ${key} = value.${key} or (default id);
        systems = value.systems or null;
      };

  normalizePackageSet = normalizeTarget {
    name = "packageSet";
    key = "attrPath";
    default = id: [id];
  };

  normalizeFlake = normalizeTarget {
    name = "flake";
    key = "attrName";
    default = lib.id;
  };

  normalizeOverlay = normalizeTarget {
    name = "overlay";
    key = "attrName";
    default = lib.id;
    extraKeys = ["build"];
  };

  normalizeCross = label: id: value: let
    # Apple silicon only: the fleet has no Intel-Mac users, so the second
    # triple would double cross-build cost for artifacts nobody pulls. A
    # package that needs it opts in via `cross.targets`.
    defaultTargets = ["aarch64-apple-darwin"];
    normalized =
      if value == null || value == false
      then null
      else if value == true
      then {}
      else
        assertKnownKeys "${label}: cross" [
          "attrName"
          "exposeNativeDarwin"
          "systems"
          "targets"
        ]
        value;
  in
    if normalized == null
    then null
    else
      normalized
      // {
        attrName = normalized.attrName or id;
        exposeNativeDarwin = normalized.exposeNativeDarwin or true;
        systems = normalized.systems or null;
        targets = normalized.targets or defaultTargets;
      };

  normalizePassthruTests = label: id: value:
    if value == null || value == false
    then null
    else if value == true
    then {
      prefix = "rust-${id}";
    }
    else
      assertKnownKeys "${label}: passthruTests" ["prefix"] value
      // {
        prefix = value.prefix or "rust-${id}";
      };

  # Opt-in standalone mirror repo (packages/mirror + the mirror-sync
  # workflow): `repo` is the GitHub `owner/name` the generated tree is
  # snapshot-synced into; `description`/`topics` seed the repo when CI creates
  # it. Absent/false = no mirror.
  normalizeMirror = label: value:
    if value == null || value == false
    then null
    else
      assertKnownKeys "${label}: mirror" [
        "description"
        "repo"
        "topics"
      ]
      value
      // {
        repo = value.repo or (throw "${label}: mirror.repo is required");
        description = value.description or null;
        topics = value.topics or [];
      };

  normalizeRustWorkspace = label: value:
    if value == null || value == false
    then null
    else if value == true
    then {
      systems = null;
    }
    else
      assertKnownKeys "${label}: inRustWorkspace" ["systems"] value
      // {
        systems = value.systems or null;
      };

  importMetadata = dir: let
    metadataFile = dir + "/package.nix";
    imported = import metadataFile;
    raw =
      if builtins.isFunction imported
      then imported {inherit lib;}
      else imported;
    label = "packages/${relativePath dir}/package.nix";
    id = raw.id or (throw "packages/${relativePath dir}/package.nix: missing required `id`");
  in
    assertKnownKeys label allowedMetadataKeys raw
    // {
      inherit id;
      path = raw.path or dir;
      metadataPath = metadataFile;
      relativePath = relativePath dir;
      packageSet = normalizePackageSet label id (raw.packageSet or null);
      flake = normalizeFlake label id (raw.flake or null);
      overlay = normalizeOverlay label id (raw.overlay or null);
      inRustWorkspace = normalizeRustWorkspace label (raw.inRustWorkspace or null);
      cross = normalizeCross label id (raw.cross or null);
      mirror = normalizeMirror label (raw.mirror or null);
      # Opt-in marker for a pyo3 extension-module cdylib crate: the shared
      # Rust workspace injects the darwin `-undefined dynamic_lookup` link
      # args for marked crates (lib/rust/workspace.nix, replacing per-crate
      # build.rs copies), and `unibind.lib.build` requires the mark. Boolean;
      # must ride `inRustWorkspace` (asserted below).
      pyExtension = raw.pyExtension or false;
      # `isolatedFeatures = true` gives the crate its own `-p` cargo
      # invocation in the shared unit graph instead of riding the
      # `--workspace` resolve, so its dependency features resolve from its
      # own manifest alone. Needed by crates whose linked artifact cannot
      # tolerate workspace-unified features (e.g. a Node addon that must not
      # inherit unibind-runtime's `py` feature from the Python consumers).
      isolatedFeatures = raw.isolatedFeatures or false;
      passthruTests = normalizePassthruTests label id (raw.passthruTests or null);
      # `updateScript = true` marks a package that exposes a
      # `passthru.updateScript` (e.g. a pinned prebuilt binary that tracks an
      # upstream "latest" pointer). The generated `update` app runs every
      # flagged package's updater; see lib/per-system.nix.
      updateScript = raw.updateScript or false;
      # Only declared workspace providers are inspected for IFD roots. Probing
      # every package's passthru forces unrelated system closures during job
      # name discovery. The actual roots remain owned by the package producer.
      workspaceIfdRoots = let
        declared = raw.workspaceIfdRoots or false;
      in
        assert lib.assertMsg (builtins.isBool declared)
        "${label}: workspaceIfdRoots must be a boolean"; declared;
    };

  entries = map importMetadata packageDirs;
  ids = map (entry: entry.id) entries;
  duplicateIds = findDuplicates ids;
  byId = lib.genAttrs' entries (entry: lib.nameValuePair entry.id entry);

  enabledForSystem = system: value:
    value != null && ((value.systems or null) == null || builtins.elem system value.systems);

  packageSetEntriesFor = system: lib.filter (entry: enabledForSystem system entry.packageSet) entries;

  flakeEntriesFor = system: lib.filter (entry: enabledForSystem system entry.flake) entries;

  overlayEntriesFor = system: lib.filter (entry: enabledForSystem system entry.overlay) entries;

  crossEntriesFor = system: lib.filter (entry: enabledForSystem system entry.cross) entries;

  # Packages that expose a `passthru.updateScript`, restricted to those actually
  # built for `system` (the flake package-set path is where `updateScript` is
  # bound). Drives the generated `update` aggregator.
  updateScriptEntriesFor = system: lib.filter (entry: entry.updateScript) (packageSetEntriesFor system);

  passthruTestEntriesFor = system:
    lib.filter (
      entry:
        entry.passthruTests
        != null
        && (
          if entry.packageSet != null
          then enabledForSystem system entry.packageSet
          else enabledForSystem system entry.inRustWorkspace
        )
    )
    entries;

  mirrorEntries = lib.filter (entry: entry.mirror != null) entries;

  pyExtensionEntries = lib.filter (entry: entry.pyExtension) entries;
  pyExtensionWithoutWorkspace = map (entry: entry.id) (
    lib.filter (entry: entry.inRustWorkspace == null) pyExtensionEntries
  );

  isolatedFeatureEntries = lib.filter (entry: entry.isolatedFeatures) entries;
  isolatedWithoutWorkspace = map (entry: entry.id) (
    lib.filter (entry: entry.inRustWorkspace == null) isolatedFeatureEntries
  );

  rustWorkspaceEntries = lib.filter (entry: entry.inRustWorkspace != null) entries;

  rustWorkspaceEntriesFor = system: lib.filter (entry: enabledForSystem system entry.inRustWorkspace) rustWorkspaceEntries;
in
  assert lib.assertMsg (
    duplicateIds == []
  ) "packages/registry.nix: duplicate package ids: ${lib.concatStringsSep ", " duplicateIds}";
  assert lib.assertMsg (
    pyExtensionWithoutWorkspace == []
  ) "packages/registry.nix: pyExtension requires inRustWorkspace: ${lib.concatStringsSep ", " pyExtensionWithoutWorkspace}";
  assert lib.assertMsg (
    isolatedWithoutWorkspace == []
  ) "packages/registry.nix: isolatedFeatures requires inRustWorkspace: ${lib.concatStringsSep ", " isolatedWithoutWorkspace}"; {
    inherit
      entries
      byId
      packageDirsWithoutMetadata
      packageSetEntriesFor
      flakeEntriesFor
      overlayEntriesFor
      crossEntriesFor
      mirrorEntries
      pyExtensionEntries
      isolatedFeatureEntries
      updateScriptEntriesFor
      passthruTestEntriesFor
      rustWorkspaceEntries
      rustWorkspaceEntriesFor
      ;
  }
