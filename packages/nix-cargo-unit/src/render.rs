use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::hash;
use crate::model::{Unit, UnitGraph, UnitMode};
use crate::shell;
use color_eyre::eyre::{Result, WrapErr as _, eyre};
use serde::Deserialize;
use url::Url;

// An options bag mirroring independent CLI switches, so the bool count is
// the CLI surface, not a state machine (same precedent as model.rs).
#[allow(clippy::struct_excessive_bools)]
pub struct RenderOptions {
    pub workspace_root: PathBuf,
    pub vendor_root: Option<PathBuf>,
    pub cargo_lock_sources: CargoLockSources,
    pub content_addressed: bool,
    pub toolchain_id: Option<String>,
    pub deny_unused_crate_dependencies: bool,
    pub deny_panics: bool,
    /// When false, lib-only units compile with `-Zembed-metadata=no`: the
    /// rlib carries a metadata stub and dependents compile against the
    /// sibling .rmeta (extern-path points there), while links still resolve
    /// the rlib through the `-L dependency=` search paths every consumer
    /// already passes. See policy.nix `compiler.embedMetadata` for the
    /// measurements that justify exposing this.
    pub embed_metadata: bool,
    /// Extra rustc flags for metadata-emitting lib compiles, under the Rustc
    /// driver only. The clippy driver is a different binary (the pinned
    /// llm-clippy toolchain) that may not know fork-only flags, and no other
    /// driver emits metadata dependents consume, so this scope is exactly the
    /// compiles whose .rmeta the flags exist to stabilize. Order-preserving
    /// into argv. The policy layer owns toolchain-compatibility validation
    /// (lib/rust/resolve.nix pairs these with the fork toolchain); the
    /// renderer transcribes them into the template's `rmetaStabilityArgs`
    /// binding, which additionally keys them on the IMPORTING toolchain's
    /// fork marker: cargo-unit re-imports one rendered file for the clippy
    /// graph with the pinned llm-clippy toolchain, and that import's
    /// parallel Rustc dependency units must compile without fork-only flags.
    pub rmeta_stability_flags: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CargoLockSources {
    packages: Vec<CargoLockPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoLock {
    #[serde(default)]
    package: Vec<CargoLockPackageEntry>,
}

#[derive(Debug, Deserialize)]
struct CargoLockPackageEntry {
    name: String,
    version: String,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoManifest {
    package: Option<CargoManifestPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoManifestPackage {
    links: Option<String>,
    #[serde(default)]
    authors: Option<toml::Value>,
    #[serde(default)]
    description: Option<toml::Value>,
    #[serde(default)]
    homepage: Option<toml::Value>,
    #[serde(default)]
    repository: Option<toml::Value>,
    #[serde(default)]
    license: Option<toml::Value>,
    #[serde(default, rename = "license-file")]
    license_file: Option<toml::Value>,
    #[serde(default, rename = "rust-version")]
    rust_version: Option<toml::Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CargoLockPackage {
    name: String,
    version: String,
    source: String,
    /// Dependency strings exactly as Cargo.lock spells them: `name`,
    /// `name version`, or `name version (source)` when disambiguation is
    /// needed. Used to compute the vendored universe a build script's
    /// `cargo metadata` needs (see `metadata_universe_for_unit`).
    dependencies: Vec<String>,
}

impl CargoLockSources {
    pub fn from_path(path: &Path) -> Result<Self> {
        let lock = fs::read_to_string(path)
            .wrap_err_with(|| format!("reading Cargo.lock source map from {}", path.display()))?;
        Self::parse(&lock)
            .wrap_err_with(|| format!("parsing Cargo.lock source map from {}", path.display()))
    }

    fn parse(lock: &str) -> Result<Self> {
        let lock: CargoLock = toml::from_str(lock)?;
        let packages = lock
            .package
            .into_iter()
            .filter_map(|package| {
                let CargoLockPackageEntry {
                    name,
                    version,
                    source,
                    dependencies,
                } = package;
                source.map(|source| CargoLockPackage {
                    name,
                    version,
                    source,
                    dependencies,
                })
            })
            .collect();

        Ok(Self { packages })
    }

    fn source_for_unit(&self, unit: &Unit) -> Result<String> {
        self.package_for_unit(unit)
            .map(|package| package.source.clone())
    }

    fn package_for_unit(&self, unit: &Unit) -> Result<&CargoLockPackage> {
        let unit_name = unit.package_name();
        let unit_version = unit.package_version();
        let unit_source = external_source_from_pkg_id(&unit.pkg_id).ok_or_else(|| {
            eyre!(
                "external unit {} {} has package id without a registry, sparse, or git source: {}",
                unit_name,
                unit_version,
                unit.pkg_id
            )
        })?;

        let matches: Vec<_> = self
            .packages
            .iter()
            .filter(|package| {
                package.name == unit_name.as_ref()
                    && package.version == unit_version
                    && cargo_lock_source_matches_pkg_id(&unit_source, &package.source)
            })
            .collect();

        match matches.as_slice() {
            [package] => Ok(package),
            [] => Err(eyre!(
                "external unit {} {} has no matching Cargo.lock source for package id {}",
                unit_name,
                unit_version,
                unit.pkg_id
            )),
            packages => Err(eyre!(
                "external unit {} {} matches multiple Cargo.lock sources: {}",
                unit_name,
                unit_version,
                packages
                    .iter()
                    .map(|package| package.source.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// The vendored universe a build script's own `cargo metadata` needs: the
    /// lockfile dependency closure of the unit's package, excluding the package
    /// itself (the run derivation's writable manifest copy plays that role).
    ///
    /// Cargo.lock omits dev-dependencies of non-workspace packages, so this
    /// universe matches the manifest only after the run phase strips its
    /// dev-dependency tables -- exactly the dependency set cargo resolves when
    /// it builds the crate as a dependency rather than a workspace root.
    fn metadata_universe_for_unit(&self, unit: &Unit) -> Result<Vec<&CargoLockPackage>> {
        let root = self.package_for_unit(unit)?;
        let mut seen = BTreeSet::new();
        let mut universe = Vec::new();
        let mut queue: VecDeque<&CargoLockPackage> = VecDeque::from([root]);
        seen.insert((
            root.source.as_str(),
            root.name.as_str(),
            root.version.as_str(),
        ));

        while let Some(package) = queue.pop_front() {
            for dependency in &package.dependencies {
                for resolved in self.resolve_lock_dependency(package, dependency)? {
                    if seen.insert((
                        resolved.source.as_str(),
                        resolved.name.as_str(),
                        resolved.version.as_str(),
                    )) {
                        universe.push(resolved);
                        queue.push_back(resolved);
                    }
                }
            }
        }

        universe.sort_by(|a, b| {
            (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source))
        });
        Ok(universe)
    }

    /// Resolve one Cargo.lock dependency string (`name`, `name version`, or
    /// `name version (source)`) to its package entries. Ambiguity is fine --
    /// cargo only adds version/source qualifiers when a bare name is ambiguous,
    /// and including every match merely widens the vendored universe.
    fn resolve_lock_dependency(
        &self,
        dependent: &CargoLockPackage,
        dependency: &str,
    ) -> Result<Vec<&CargoLockPackage>> {
        let mut parts = dependency.split_whitespace();
        let name = parts.next().ok_or_else(|| {
            eyre!(
                "Cargo.lock package {} {} has an empty dependency string",
                dependent.name,
                dependent.version
            )
        })?;
        let version = parts.next();
        let source = parts
            .next()
            .map(|source| source.trim_start_matches('(').trim_end_matches(')'));

        let matches: Vec<_> = self
            .packages
            .iter()
            .filter(|package| {
                package.name == name
                    && version.is_none_or(|version| package.version == version)
                    && source.is_none_or(|source| package.source == source)
            })
            .collect();

        if matches.is_empty() {
            // A dependency of an external package always has an external
            // source of its own (registry crates cannot depend on path
            // packages), so every dependency string must resolve here.
            return Err(eyre!(
                "Cargo.lock dependency `{}` of {} {} matches no vendorable package entry",
                dependency,
                dependent.name,
                dependent.version
            ));
        }
        Ok(matches)
    }
}

fn cargo_lock_source_matches_pkg_id(pkg_id_source: &str, cargo_lock_source: &str) -> bool {
    if pkg_id_source == cargo_lock_source {
        return true;
    }

    pkg_id_source.starts_with("git+")
        && cargo_lock_source
            .rsplit_once('#')
            .is_some_and(|(source_without_rev, _)| source_without_rev == pkg_id_source)
}

struct PreparedGraph {
    hashes: Vec<String>,
    names: Vec<String>,
    source_refs: Vec<String>,
    source_entries: BTreeMap<String, SourceEntry>,
    transitive_unit_deps: Vec<BTreeSet<usize>>,
    build_script_runs: BTreeMap<usize, BuildScriptRun>,
}

struct BuildScriptRun {
    compile_index: usize,
    dependency_runs: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceEntry {
    name: String,
    base: SourceBase,
    root: PathBuf,
    relative: String,
    include_relatives: Vec<String>,
    excluded_relatives: Vec<String>,
    source_key: String,
    /// Placeholder this source's store path is rewritten to, see
    /// [`source_remap_prefix`].
    remap_prefix: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceBase {
    Workspace,
    WorkspaceClosure,
    VendorPackage,
    VendorClosure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceScope {
    Package,
    Closure,
}

struct ScopedSourceRoot {
    root: PathBuf,
    scope: SourceScope,
    relative: String,
    include_relatives: Vec<String>,
}

impl SourceBase {
    const fn label(self) -> &'static str {
        match self {
            Self::Workspace | Self::WorkspaceClosure => "workspace",
            Self::VendorPackage | Self::VendorClosure => "vendor",
        }
    }

    const fn audit_label(self) -> &'static str {
        match self {
            Self::Workspace | Self::WorkspaceClosure => "workspace",
            Self::VendorPackage => "vendor-package",
            Self::VendorClosure => "vendor-closure",
        }
    }

    const fn scope(self) -> SourceScope {
        match self {
            Self::Workspace | Self::VendorPackage => SourceScope::Package,
            Self::WorkspaceClosure | Self::VendorClosure => SourceScope::Closure,
        }
    }
}

impl SourceScope {
    const fn audit_label(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Closure => "closure",
        }
    }
}

impl SourceEntry {
    /// Package directory relative to the scoped source root, as a Cargo manifest
    /// dir. The workspace root collapses to `"."` so callers never emit an empty
    /// path component.
    const fn package_root(&self) -> &str {
        if self.relative.is_empty() {
            "."
        } else {
            self.relative.as_str()
        }
    }

    fn nix_expr(&self) -> String {
        match self.base {
            SourceBase::Workspace if !self.excluded_relatives.is_empty() => format!(
                "scopedWorkspaceSourceExcluding {} {} {}",
                nix_attr(&self.name),
                nix_attr(&self.relative),
                nix_string_list(&self.excluded_relatives)
            ),
            SourceBase::WorkspaceClosure if !self.excluded_relatives.is_empty() => format!(
                "scopedWorkspaceClosureSourceExcluding {} {} {}",
                nix_attr(&self.name),
                nix_string_list(&self.include_relatives),
                nix_string_list(&self.excluded_relatives)
            ),
            SourceBase::Workspace => format!(
                "scopedWorkspaceSource {} {}",
                nix_attr(&self.name),
                nix_attr(&self.relative)
            ),
            SourceBase::WorkspaceClosure => format!(
                "scopedWorkspaceClosureSource {} {}",
                nix_attr(&self.name),
                nix_string_list(&self.include_relatives)
            ),
            SourceBase::VendorPackage => format!("vendorSources.{}", nix_attr(&self.source_key)),
            SourceBase::VendorClosure => format!(
                "scopedVendorClosureSource {} {}",
                nix_attr(&self.name),
                nix_string_list(&self.include_relatives)
            ),
        }
    }
}

const UNITS_TEMPLATE: &str = include_str!("../templates/units.nix.in");

pub fn render_units_nix(graph: &UnitGraph, options: &RenderOptions) -> Result<String> {
    graph.ensure_supported()?;
    let prepared = prepare_graph(graph, options)?;
    let slots = [
        ("source_entries", render_source_entries(&prepared)),
        (
            "source_audit_entries",
            render_source_audit_entries(&prepared),
        ),
        (
            "unit_entries",
            render_unit_entries(graph, options, &prepared)?,
        ),
        (
            "clippy_unit_entries",
            render_clippy_unit_entries(graph, options, &prepared)?,
        ),
        (
            "clippy_unit_names_by_package",
            render_clippy_unit_names_by_package(graph, &prepared),
        ),
        (
            "panic_object_unit_entries",
            render_panic_object_unit_entries(graph, options, &prepared)?,
        ),
        (
            "policy_check_entries",
            render_policy_check_entries(graph, options, &prepared)?,
        ),
        (
            "unused_crate_dependencies_by_package",
            render_unused_crate_dependencies_by_package(graph, options, &prepared),
        ),
        ("roots", render_roots(graph, &prepared)),
        ("checked_roots", render_checked_roots(graph, &prepared)),
        (
            "package_entries",
            render_root_entries(graph, &prepared, |_| true),
        ),
        (
            "binary_entries",
            render_root_entries(graph, &prepared, Unit::is_bin),
        ),
        (
            "library_entries",
            render_root_entries(graph, &prepared, Unit::is_library),
        ),
        (
            "benchmark_entries",
            render_benchmark_entries(graph, &prepared),
        ),
        ("test_entries", render_test_entries(graph, &prepared)),
        ("doctest_entries", render_doctest_entries(graph, &prepared)),
        (
            "test_target_entries",
            render_test_target_entries(graph, &prepared),
        ),
        (
            "test_name_unit_entries",
            render_test_name_unit_entries(graph, options, &prepared)?,
        ),
        ("rmeta_stability_args", render_rmeta_stability_args(options)),
        (
            "doctest_target_entries",
            render_doctest_target_entries(graph, &prepared, options)?,
        ),
        (
            "benchmark_target_entries",
            render_benchmark_target_entries(graph, &prepared),
        ),
        ("target_set_entries", render_target_sets(graph, &prepared)),
        ("default_entry", render_default_entry(graph, &prepared)),
    ];

    Ok(fill_template(UNITS_TEMPLATE, &slots))
}

/// The rmeta-stability flags as nix string literals for the template's
/// `rmetaStabilityArgs` list. The flags are fixed `-Z...` spellings from
/// resolve.nix (no quotes or backslashes to escape), asserted rather than
/// escaped so a future flag with metacharacters fails the render loudly
/// instead of producing a subtly wrong nix literal.
fn render_rmeta_stability_args(options: &RenderOptions) -> String {
    options
        .rmeta_stability_flags
        .iter()
        .map(|flag| {
            assert!(
                !flag.contains('"') && !flag.contains('\\') && !flag.contains('$'),
                "rmeta stability flag {flag:?} needs escaping the renderer does not implement"
            );
            format!("\"{flag}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// The template's only syntax is `{{ slot }}` substitution -- no loops or
// conditionals -- so a single pass is the whole engine. Spliced values are
// never rescanned, matching template-engine semantics.
fn fill_template(template: &str, slots: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .expect("template placeholder is unterminated");
        let key = after[..end].trim();
        let (_, value) = slots
            .iter()
            .find(|(slot, _)| *slot == key)
            .unwrap_or_else(|| panic!("template placeholder {key} has no rendered slot"));
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn render_unit_entries(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
) -> Result<String> {
    let mut entries = String::new();
    for (run_index, build_script_run) in &prepared.build_script_runs {
        write!(
            entries,
            "    {} = mkUnit {};\n\n",
            prepared.unit_attr(*run_index),
            render_build_script_run(graph, options, prepared, *run_index, build_script_run)?
        )?;
    }

    for (index, unit) in graph.units.iter().enumerate() {
        if unit.is_run_custom_build() {
            continue;
        }

        write!(
            entries,
            "    {} = mkUnit {};\n\n",
            prepared.unit_attr(index),
            render_rustc_unit(graph, options, prepared, index)?
        )?;
    }

    Ok(entries)
}

fn render_clippy_unit_entries(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
) -> Result<String> {
    let mut entries = String::new();
    for (index, unit) in graph.units.iter().enumerate() {
        if !is_clippy_unit_candidate(unit) {
            continue;
        }
        write!(
            entries,
            "    {} = mkUnit {};\n\n",
            prepared.unit_attr(index),
            render_clippy_unit(graph, options, prepared, index)?
        )?;
    }
    Ok(entries)
}

// Clippy only lints workspace-owned code. Vendored crates (registry, sparse,
// git) compile under `--cap-lints warn` for a reason: we don't want a churning
// upstream to break the workspace lint gate. The run-custom-build unit
// executes `build.rs` (no source to lint), but the custom-build compile unit
// IS workspace Rust and the old `cargo clippy` workspace gate covered it, so
// it must keep getting clippy here.
fn is_clippy_unit_candidate(unit: &Unit) -> bool {
    !unit.is_run_custom_build() && !unit.is_external()
}

fn render_panic_object_unit_entries(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
) -> Result<String> {
    let mut entries = String::new();
    if !options.deny_panics {
        return Ok(entries);
    }
    for (index, unit) in graph.units.iter().enumerate() {
        if !is_panic_freedom_candidate(unit) {
            continue;
        }
        write!(
            entries,
            "    {} = mkUnit {};\n\n",
            prepared.unit_attr(index),
            render_panic_object_unit(graph, options, prepared, index)?
        )?;
    }
    Ok(entries)
}

fn render_policy_check_entries(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
) -> Result<String> {
    // Unused-crate-dependency checks are emitted per package
    // (`unusedCrateDependenciesByPackage`), not as one workspace aggregate, so a
    // single crate edit rebuilds only its own check. `_options` stays for the
    // panic-freedom gate below.
    let mut entries = String::new();
    if options.deny_panics
        && let Some(check) = render_panic_freedom_check(graph, prepared)
    {
        writeln!(entries, "    panicFreedom = {check};")?;
    }

    Ok(entries)
}

// Production units the panic-freedom scan inspects: workspace lib and bin
// compiles, each contributing a panic-objects derivation whose monomorphized
// objects the scan reads. Test and bench bodies legitimately panic (`assert!`,
// `unwrap`, explicit `panic!`), so they are excluded; a library generic that
// only a test instantiates is consequently not covered, which is correct because
// it is not on a production code path. Build-script runs have no compile of their
// own, external crates compile under `--cap-lints warn` and are not the
// workspace's invariant to hold, and proc-macro / custom-build compile units are
// host build tooling rather than the shipped surface.
fn is_panic_freedom_candidate(unit: &Unit) -> bool {
    !unit.is_external()
        && !unit.is_proc_macro()
        && !unit.is_run_custom_build()
        && !unit.is_custom_build_compile()
        && !unit.is_test()
        && !unit.is_benchmark()
}

// Every workspace crate name that can appear as a mangled symbol prefix, used to
// scope findings. A library generic monomorphized inside a bin object keeps the
// library's crate token, so the scan needs the whole set, not just the unit
// under inspection.
fn workspace_crate_names(graph: &UnitGraph) -> Vec<String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for unit in &graph.units {
        if !unit.is_external() && !unit.is_run_custom_build() {
            names.insert(unit.target.name.clone());
        }
    }
    names.into_iter().collect()
}

// Renders one panic-freedom derivation per candidate unit, joined under a single
// aggregate so a touched unit only re-scans itself. Each scan runs
// `nix-cargo-unit scan-panics` over the unit's panic-objects derivation, scoped
// to the whole workspace crate set, and fails if any workspace function
// references a panic sink. The scanner is the `cargoUnit` package, asserted
// non-null so enabling the policy without wiring the scanner fails loudly rather
// than silently passing.
fn render_panic_freedom_check(graph: &UnitGraph, prepared: &PreparedGraph) -> Option<String> {
    let mut scans = String::new();
    for (index, unit) in graph.units.iter().enumerate() {
        if !is_panic_freedom_candidate(unit) {
            continue;
        }
        let _ = writeln!(
            scans,
            "          (scanUnit {} panicObjectUnits.{})",
            nix_attr(&prepared.names[index]),
            prepared.unit_attr(index),
        );
    }

    if scans.is_empty() {
        return None;
    }

    let mut scan_args: Vec<String> = Vec::new();
    for name in workspace_crate_names(graph) {
        scan_args.push("--crate-name".to_string());
        scan_args.push(name);
    }
    let scan_args_nix = nix_string_list(&scan_args);

    Some(format!(
        r#"assert pkgs.lib.assertMsg (cargoUnit != null) "cargo-unit panic-freedom needs the cargoUnit scanner package; pass it through buildWorkspace.";
      let
        scanUnit = drvName: objects:
          pkgs.runCommand "cargo-unit-panic-freedom-${{drvName}}"
            {{ nativeBuildInputs = [ cargoUnit ]; }}
            ''
              set -euo pipefail
              nix-cargo-unit scan-panics ${{pkgs.lib.escapeShellArgs {scan_args_nix}}} "${{objects}}"
              mkdir -p "$out"
            '';
      in
      pkgs.symlinkJoin {{
        name = "cargo-unit-panic-freedom";
        paths = [
{scans}        ];
      }}"#
    ))
}

fn render_source_entries(prepared: &PreparedGraph) -> String {
    let mut entries = String::new();
    for (key, source) in &prepared.source_entries {
        let _ = writeln!(entries, "    {} = {};", nix_attr(key), source.nix_expr());
    }
    entries
}

fn render_source_audit_entries(prepared: &PreparedGraph) -> String {
    let mut entries = String::new();
    for (key, source) in &prepared.source_entries {
        let exclusions = if source.excluded_relatives.is_empty() {
            String::new()
        } else {
            format!(
                " excludedRelatives = {};",
                nix_string_list(&source.excluded_relatives)
            )
        };
        let _ = writeln!(
            entries,
            "    {} = {{ base = {}; scope = {}; relative = {}; includeRelatives = {}; sourceKey = {}; remapPrefix = {};{} }};",
            nix_attr(key),
            nix_attr(source.base.audit_label()),
            nix_attr(source.base.scope().audit_label()),
            nix_attr(&source.relative),
            nix_string_list(&source.include_relatives),
            nix_attr(&source.source_key),
            nix_attr(&source.remap_prefix),
            exclusions,
        );
    }
    entries
}

fn render_default_entry(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    graph
        .roots
        .first()
        .map(|first_root| {
            format!(
                "  default = withPolicyChecks {};\n",
                prepared.unit_ref(*first_root)
            )
        })
        .unwrap_or_default()
}

impl PreparedGraph {
    fn unit_attr(&self, index: usize) -> String {
        nix_attr(&self.names[index])
    }

    fn unit_ref(&self, index: usize) -> String {
        format!("units.{}", self.unit_attr(index))
    }

    fn source_ref(&self, index: usize) -> String {
        format!("sources.{}", nix_attr(&self.source_refs[index]))
    }

    fn source_entry(&self, index: usize) -> Result<&SourceEntry> {
        let key = self
            .source_refs
            .get(index)
            .ok_or_else(|| eyre!("unit index {index} has no scoped source entry"))?;
        self.source_entries
            .get(key)
            .ok_or_else(|| eyre!("unit index {index} references missing scoped source {key}"))
    }
}

fn workspace_package_roots(graph: &UnitGraph) -> BTreeSet<PathBuf> {
    graph
        .units
        .iter()
        .filter(|unit| !unit.is_external())
        .filter_map(|unit| local_package_root_from_pkg_id(&unit.pkg_id))
        .collect()
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct LocalSourceCacheKey {
    package_id: String,
    package_name: String,
    package_version: String,
    root: PathBuf,
    has_build_script: bool,
    input_class: SourceInputClass,
}

/// Cache only within this render: the next invocation re-reads source inputs.
/// Production and test sources may differ only under an explicit package
/// contract. Include that class in both this cache and the emitted source key.
fn prepare_sources(
    graph: &UnitGraph,
    options: &RenderOptions,
) -> Result<(Vec<String>, BTreeMap<String, SourceEntry>)> {
    let workspace_packages = workspace_package_roots(graph);
    // A build script may read arbitrary package-relative files. Keep its
    // package intact instead of inferring those inputs from Rust includes.
    let build_script_packages: BTreeSet<&str> = graph
        .units
        .iter()
        .filter(|unit| unit.is_custom_build_compile())
        .map(|unit| unit.pkg_id.as_str())
        .collect();
    let mut source_refs = Vec::with_capacity(graph.units.len());
    let mut source_entries = BTreeMap::new();
    let mut local_cache: BTreeMap<LocalSourceCacheKey, SourceEntry> =
        BTreeMap::new();
    for unit in &graph.units {
        let has_build_script = build_script_packages.contains(unit.pkg_id.as_str());
        let local_key = (!unit.is_external()).then(|| LocalSourceCacheKey {
            package_id: unit.pkg_id.clone(),
            package_name: unit.package_name().into_owned(),
            package_version: unit.package_version().to_owned(),
            root: local_package_root_from_pkg_id(&unit.pkg_id)
                .unwrap_or_else(|| crate_root_for_unit(unit)),
            has_build_script,
            input_class: source_input_class(unit),
        });
        let source = if let Some(cached) = local_key.as_ref().and_then(|key| local_cache.get(key)) {
            cached.clone()
        } else {
            let source =
                source_entry_for_unit(unit, options, &workspace_packages, has_build_script)?;
            if let Some(key) = local_key {
                local_cache.insert(key, source.clone());
            }
            source
        };
        let key = source.name.clone();
        source_refs.push(key.clone());
        source_entries.entry(key).or_insert(source);
    }

    Ok((source_refs, source_entries))
}

fn prepare_graph(graph: &UnitGraph, options: &RenderOptions) -> Result<PreparedGraph> {
    let mut hashes = vec![None; graph.units.len()];
    for index in 0..graph.units.len() {
        compute_hash(graph, options, index, &mut hashes);
    }
    let hashes: Vec<String> = hashes.into_iter().map(Option::unwrap).collect();

    let names: Vec<String> = graph
        .units
        .iter()
        .enumerate()
        .map(|(index, unit)| {
            if unit.is_run_custom_build() {
                format!(
                    "{}-build-script-run-{}-{}",
                    unit.package_name(),
                    unit.package_version(),
                    hashes[index]
                )
            } else {
                format!(
                    "{}-{}-{}",
                    unit.target.name,
                    unit.package_version(),
                    hashes[index]
                )
            }
        })
        .collect();

    let transitive_unit_deps: Vec<BTreeSet<usize>> = (0..graph.units.len())
        .map(|index| {
            let mut deps = BTreeSet::new();
            collect_transitive_unit_deps(graph, index, &mut deps);
            deps
        })
        .collect();

    let (source_refs, source_entries) = prepare_sources(graph, options)?;

    let mut build_script_runs = BTreeMap::new();
    for (index, unit) in graph.units.iter().enumerate() {
        if !unit.is_run_custom_build() {
            continue;
        }

        let compile_index = unit
            .dependencies
            .iter()
            .map(|dep| dep.index)
            .find(|dep_index| {
                graph
                    .units
                    .get(*dep_index)
                    .is_some_and(Unit::is_custom_build_compile)
            })
            .ok_or_else(|| eyre!("build script run unit {index} has no compile dependency"))?;

        let dependency_runs = unit
            .dependencies
            .iter()
            .map(|dep| dep.index)
            .filter(|dep_index| {
                *dep_index != compile_index
                    && graph
                        .units
                        .get(*dep_index)
                        .is_some_and(Unit::is_run_custom_build)
            })
            .collect();

        build_script_runs.insert(
            index,
            BuildScriptRun {
                compile_index,
                dependency_runs,
            },
        );
    }

    Ok(PreparedGraph {
        hashes,
        names,
        source_refs,
        source_entries,
        transitive_unit_deps,
        build_script_runs,
    })
}

// Infallible: `render_units_nix` validates the graph up front, so all indexes
// are in bounds.
fn compute_hash(
    graph: &UnitGraph,
    options: &RenderOptions,
    index: usize,
    hashes: &mut [Option<String>],
) -> String {
    if let Some(hash) = &hashes[index] {
        return hash.clone();
    }

    let unit = &graph.units[index];
    let mut dependency_hashes = Vec::new();
    for dependency in &unit.dependencies {
        // Include run-custom-build deps: `merge_identity_hash` dedupes units
        // over every dependency, so a name hash that skipped runs collided
        // two merged units differing only in a build-script-run variant
        // (indexable-inc/index#4093). The unit graph is a DAG, so recursing
        // through runs terminates.
        dependency_hashes.push(format!(
            "{}:{}:{}:{}",
            dependency.extern_crate_name,
            dependency.public,
            dependency.noprelude,
            compute_hash(graph, options, dependency.index, hashes)
        ));
    }

    let hash = unit.identity_hash(&dependency_hashes, options.toolchain_id.as_deref());
    hashes[index] = Some(hash.clone());
    hash
}

fn collect_transitive_unit_deps(graph: &UnitGraph, index: usize, deps: &mut BTreeSet<usize>) {
    for dependency in &graph.units[index].dependencies {
        if graph.units[dependency.index].is_run_custom_build() {
            continue;
        }
        if deps.insert(dependency.index) {
            collect_transitive_unit_deps(graph, dependency.index, deps);
        }
    }
}

// `Rustc` produces the build artifacts (rlib, bin, test binary).
// `Clippy` runs the same compilation through `clippy-driver` so lints
// fire per unit, emitting metadata only — no link step, no binary output.
// `ObjectEmit` runs `rustc` with the same flags but emits relocatable objects
// only (`--emit obj`, no link), giving the panic-freedom scan the monomorphized
// objects a linked binary would otherwise hide behind resolved branches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Driver {
    Rustc,
    Clippy,
    ObjectEmit,
    // Compiles a `--test` unit with `-Zdump-test-names` (the ix rustc fork's
    // flag for rust-lang/rust#50297): rustc serializes the #[test]/#[bench]
    // descriptors the harness collected as JSON and stops before analysis,
    // codegen, and linking, so discovery never pays for the test binary's
    // codegen or link.
    TestNames,
}

impl Driver {
    const fn binary(self) -> &'static str {
        match self {
            Self::Rustc | Self::ObjectEmit | Self::TestNames => "rustc",
            Self::Clippy => "clippy-driver",
        }
    }
}

// CA-derivation attributes Nix needs to content-address a unit's output.
// Opt-in: callers pass `--content-addressed`, otherwise units stay input
// addressed.
//
// Why content-address: it buys "early cutoff" — when a crate is rebuilt but its
// output is byte-identical, Nix reuses the existing path and dependents are not
// re-derived. How often that fires is bounded by rustc itself: a crate's rmeta
// is oversensitive (a comment / private-body / formatting change still changes
// it, forcing reverse-dependency rebuilds), and link targets (bins/cdylibs)
// always relink when any transitive rlib changed because rlibs aren't byte-stable
// under `-C incremental`. Upstream is working to narrow exactly this — see the
// "Relink, don't Rebuild" rustc project goal
// (rust-lang/rust-project-goals 2025h2) — after which CA early cutoff will fire
// on more non-interface changes. (Build-script outputs are deliberately kept
// input-addressed in render_build_script_run_derivation: their OUT_DIR can hold
// arbitrary non-reproducible content, which floating CA cannot tolerate without
// wedging the store — see the note there, briansmith/ring#715 + NixOS/nix#15649.)
//
// The CA hash is blake3, per ADR 0003 (docs/adr/0003-content-identity-two-families.md):
// a nix CA output is family-1 content, so it carries the same flat 32-byte
// unkeyed blake3 address as CAS objects, jj `FileId`s and NARs. Family 1 has an
// outside referee, which is the property being bought here: `nix hash file
// --type blake3` and `b3sum` agree byte for byte, so a disagreement is an ix bug
// rather than a question about our own pipeline.
//
// The constraint boundary. Only the address ix computes for content it produces
// itself moves. Everything an outside format fixes stays sha256: narinfo
// `NarHash` (the signing fingerprint embeds it, so changing the algorithm voids
// every existing signature), fixed-output `outputHash` pins that quote an
// upstream digest, and nix's own text-hashed store objects (`store-dir-config.cc`
// asserts SHA256 there).
//
// This was blocked until the fork taught the reference-bearing `source:` store
// path scheme to accept blake3 (`makeFixedOutputPath`,
// index/views/nix/src/libstore/store-dir-config.cc): a CA output naming other
// store paths, which every rust unit does, previously threw for any algorithm
// but sha256. Needs the `blake3-hashes` experimental feature, which is why the
// flake requests it.
fn append_content_addressing(attrs: &mut Attrs, content_addressed: bool) {
    if !content_addressed {
        return;
    }
    attrs.bool("__contentAddressed", true);
    attrs.string("outputHashMode", "recursive");
    attrs.string("outputHashAlgo", "blake3");
}

// The per-unit derivation fields that vary by role; everything else (version,
// source, dependency rlibs, `dontStrip`, content addressing) is shared.
struct UnitDerivation<'a> {
    pname: String,
    native_build_inputs: &'a str,
    driver: Driver,
    install_phase: String,
    // `Some` only for a unit that compiles the package's own code, so
    // `packageBuildEnv.<package>` can be merged onto its env in the template.
    // Clippy compiles the same source, including env! expressions, and needs
    // the same package-scoped environment and lint configuration.
    // Panic-object checks currently have no package tag.
    package_name: Option<String>,
}

// Shared scaffold for the build, clippy, and panic-object derivations. They
// compile the same unit source against the same dependency rlibs and differ only
// in name, the driver/emit, the extra native inputs, and how the result is
// installed.
fn render_unit_derivation(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
    index: usize,
    spec: UnitDerivation<'_>,
) -> Result<String> {
    let UnitDerivation {
        pname,
        native_build_inputs,
        driver,
        install_phase,
        package_name,
    } = spec;
    let mut attrs = Attrs::new();

    attrs.string("pname", &pname);
    if let Some(package_name) = &package_name {
        attrs.string("packageName", package_name);
    }
    attrs.string("version", graph.units[index].package_version());
    attrs.expr("src", &prepared.source_ref(index));
    attrs.expr("nativeBuildInputs", native_build_inputs);
    attrs.expr(
        "buildInputs",
        &render_build_inputs(graph, prepared, index, unit_build_script_run(graph, index)),
    );
    attrs.bool("dontStrip", true);
    // Content-address libraries and binaries -- that is where CA's "early
    // cutoff" pays off: a byte-identical rebuild lets dependents skip rebuilding.
    // Tests and benchmarks are leaf derivations that nothing consumes, so CA
    // buys them no early cutoff; it only adds floating-realisation fragility. A
    // stale or GC'd test realisation on a shared store resurfaces as a silent
    // "build of resolved derivation '<...>-cargo-unit-test-manifest' failed: N
    // dependencies failed" with no builder error (NixOS/nix#15649) -- exactly
    // the failures seen on the shared CI store. Keep tests and benches
    // input-addressed; they still benefit from upstream CA on their inputs.
    let unit = &graph.units[index];
    let content_addressed = options.content_addressed && !unit.is_test() && !unit.is_benchmark();
    append_content_addressing(&mut attrs, content_addressed);
    emit_build_phase(
        &mut attrs,
        &render_driver_build_phase(graph, options, prepared, index, driver)?,
    );
    attrs.multiline("installPhase", &install_phase);
    if driver == Driver::Rustc {
        attrs.multiline(
            "postFixup",
            &render_split_debuginfo_sidecar_post_fixup(&graph.units[index]),
        );
    }

    Ok(attrs.render())
}

fn render_rustc_unit(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
    index: usize,
) -> Result<String> {
    let unit = &graph.units[index];
    let native_build_inputs = if collects_unused_crate_dependencies(unit, options) {
        "[ rustToolchain pkgs.jq ] ++ extraNativeBuildInputs"
    } else {
        "[ rustToolchain ] ++ extraNativeBuildInputs"
    };
    render_unit_derivation(
        graph,
        options,
        prepared,
        index,
        UnitDerivation {
            pname: unit.target.name.clone(),
            native_build_inputs,
            driver: Driver::Rustc,
            install_phase: render_install_phase(unit, options, &prepared.hashes[index]),
            package_name: Some(unit.package_name().into_owned()),
        },
    )
}

fn render_clippy_unit(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
    index: usize,
) -> Result<String> {
    // `clippy-driver` rides the rustToolchain (which carries the matching
    // rustc); the clippy package only needs to be on PATH for the driver
    // binary. Callers append it via `extraClippyNativeBuildInputs`. Clippy units
    // link metadata-only against the build units' rlibs, so a touched source
    // file only invalidates that unit's clippy plus its downstream.
    render_unit_derivation(
        graph,
        options,
        prepared,
        index,
        UnitDerivation {
            pname: format!("{}-clippy", graph.units[index].target.name),
            native_build_inputs: "[ rustToolchain ] ++ extraNativeBuildInputs ++ extraClippyNativeBuildInputs",
            driver: Driver::Clippy,
            install_phase: "mkdir -p $out\n".to_string(),
            package_name: Some(graph.units[index].package_name().into_owned()),
        },
    )
}

// Recompiles the unit with `--emit obj` so the panic-freedom scan has the
// relocatable objects (including monomorphized generics) that the linked build
// unit hides.
fn render_panic_object_unit(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
    index: usize,
) -> Result<String> {
    render_unit_derivation(
        graph,
        options,
        prepared,
        index,
        UnitDerivation {
            pname: format!("{}-panic-objects", graph.units[index].target.name),
            native_build_inputs: "[ rustToolchain ] ++ extraNativeBuildInputs",
            driver: Driver::ObjectEmit,
            // Content-addressed like the build unit, so it hits the same
            // killed-build orphan `cp` failure (#2247); guard it identically.
            install_phase: format!(
                "{ORPHAN_OUTPUT_PRECHECK}mkdir -p $out\nfind build -maxdepth 1 -name '*.o' -exec cp {{}} \"$out/\" ';'\n"
            ),
            package_name: None,
        },
    )
}

fn unit_build_script_run(graph: &UnitGraph, index: usize) -> Option<usize> {
    let unit = &graph.units[index];
    unit.dependencies
        .iter()
        .map(|dep| dep.index)
        .find(|dep_index| {
            graph.units.get(*dep_index).is_some_and(|dep_unit| {
                dep_unit.is_run_custom_build() && dep_unit.pkg_id == unit.pkg_id
            })
        })
}

fn render_build_inputs(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    index: usize,
    build_script_run: Option<usize>,
) -> String {
    let mut refs: Vec<String> = graph.units[index]
        .dependencies
        .iter()
        .filter_map(|dep| {
            let dep_unit = &graph.units[dep.index];
            (!dep_unit.is_run_custom_build())
                .then(|| format!("units.{}", nix_attr(&prepared.names[dep.index])))
        })
        .collect();

    if let Some(run_index) = build_script_run {
        refs.push(format!("units.{}", nix_attr(&prepared.names[run_index])));
    }

    if refs.is_empty() {
        "[]".to_string()
    } else {
        format!("[ {} ]", refs.join(" "))
    }
}

// Cargo sets `CARGO_BIN_EXE_<name>` for integration tests and benchmarks.
// The unit graph exposes same-package build-mode bin targets as dependencies;
// test-harness bin units and integration-test binaries are runnable outputs too,
// but they are not the executable Cargo points this variable at.
fn same_package_bins(graph: &UnitGraph, index: usize) -> Vec<(String, usize)> {
    let unit = &graph.units[index];
    if !needs_bin_exe_env(unit) {
        return Vec::new();
    }
    unit.dependencies
        .iter()
        .filter_map(|dependency| {
            let candidate = graph.units.get(dependency.index)?;
            is_bin_exe_candidate(unit, candidate)
                .then(|| (candidate.target.name.clone(), dependency.index))
        })
        .collect()
}

fn needs_bin_exe_env(unit: &Unit) -> bool {
    matches!(unit.mode, UnitMode::Test)
        && unit
            .target
            .kind
            .iter()
            .any(|kind| matches!(kind.as_str(), "test" | "bench"))
}

fn is_bin_exe_candidate(unit: &Unit, candidate: &Unit) -> bool {
    candidate.pkg_id == unit.pkg_id
        && candidate.mode == UnitMode::Build
        && candidate.target.has_kind("bin")
        && candidate.platform == unit.platform
}

fn render_driver_build_phase(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
    index: usize,
    driver: Driver,
) -> Result<String> {
    let unit = &graph.units[index];
    let source = prepared.source_entry(index)?;
    let mut script = String::new();

    let collect_unused_deps =
        driver == Driver::Rustc && collects_unused_crate_dependencies(unit, options);

    script.push_str("mkdir -p build\n");
    script.push_str("build_script_flags=()\n");
    script.push_str("rustc_env=()\n");
    script.push_str("rustc_args=()\n\n");
    script.push_str(&cargo_package_exports(unit)?);
    writeln!(
        script,
        "export CARGO_MANIFEST_DIR={}",
        shell::double_quote(&source_path_expr(source, &crate_root_for_unit(unit))?)
    )?;
    append_bin_exe_env(&mut script, graph, prepared, index)?;

    if let Some(run_index) = unit_build_script_run(graph, index) {
        let run_ref = format!("${{units.{}}}", nix_attr(&prepared.names[run_index]));
        append_build_script_flag_reader(&mut script, &run_ref, unit);
    }
    append_direct_dependency_metadata_exports(&mut script, graph, prepared, index)?;

    push_rustc_args(&mut script, unit, &prepared.hashes[index], driver);
    append_path_remap_args(&mut script, source);
    append_target_linker_arg(&mut script, unit);
    append_extra_rustc_args(&mut script, unit);

    for dep_index in &prepared.transitive_unit_deps[index] {
        let dep = &graph.units[*dep_index];
        if dep.is_bin() {
            continue;
        }
        writeln!(
            script,
            "rustc_args+=( -L \"dependency=${{units.{}}}/lib\" )",
            nix_attr(&prepared.names[*dep_index])
        )?;
    }

    if unit.is_proc_macro() {
        script.push_str("rustc_args+=( --extern proc_macro )\n");
    }
    if collect_unused_deps {
        script.push_str("rustc_args+=( --error-format=json --json=unused-externs-silent )\n");
        push_arg(&mut script, "-W");
        push_arg(&mut script, "unused-crate-dependencies");
    }

    append_dependency_externs(&mut script, graph, prepared, unit, options)?;
    append_dylib_rpath_args(&mut script, graph, prepared, index, driver)?;

    let source_path = source_path_expr(source, Path::new(&unit.target.src_path))?;
    writeln!(
        script,
        "rustc_args+=( {} )",
        shell::double_quote(&source_path)
    )?;

    match driver {
        Driver::Rustc => {
            if uses_explicit_output_path(unit, driver) {
                writeln!(
                    script,
                    "rustc_args+=( -o {} )",
                    shell::quote(&format!("build/{}", unit.target.name))
                )?;
            } else {
                script.push_str("rustc_args+=( --out-dir build )\n");
                if unit.is_proc_macro() {
                    script.push_str("rustc_args+=( --emit dep-info,link )\n");
                } else {
                    script.push_str("rustc_args+=( --emit dep-info,metadata,link )\n");
                    if !options.embed_metadata && thin_metadata_eligible(unit) {
                        script.push_str("rustc_args+=( -Zembed-metadata=no )\n");
                    }
                    if !options.rmeta_stability_flags.is_empty() {
                        // Via the template binding, not literal argv: the
                        // binding empties itself under a non-fork importing
                        // toolchain (the clippy graph), where these flags
                        // would hard-error.
                        script.push_str(
                            "rustc_args+=( ${pkgs.lib.escapeShellArgs rmetaStabilityArgs} )\n",
                        );
                    }
                }
            }
        }
        Driver::Clippy => {
            // Clippy only needs MIR. Skip codegen and linking entirely.
            script.push_str("rustc_args+=( --out-dir build )\n");
            script.push_str("rustc_args+=( --emit dep-info,metadata )\n");
        }
        Driver::ObjectEmit => {
            // Emit relocatable objects only, no link. `--out-dir` keeps every
            // codegen unit's object; a single `obj=<file>` path is rejected for
            // codegen-units > 1, so the directory form is the portable one.
            script.push_str("rustc_args+=( --out-dir build )\n");
            script.push_str("rustc_args+=( --emit obj )\n");
        }
        Driver::TestNames => {
            // The dump is the unit's only product: rustc writes the JSON and
            // stops before codegen, so there is no artifact and no `-o`. The
            // flag exists only in the ix rustc fork; a toolchain without it
            // fails loudly here ("unknown unstable option"), which is the
            // guard against selecting `testDiscovery = "dump-test-names"`
            // with an upstream toolchain.
            script.push_str("rustc_args+=( --out-dir build )\n");
            script.push_str("rustc_args+=( -Zdump-test-names=build/test-names.json )\n");
        }
    }

    script.push_str("rustc_args+=( \"''${build_script_flags[@]}\" )\n");
    if driver == Driver::Clippy {
        // Inject the workspace's clippy lint policy (-D/-W/-A clippy::...)
        // at the rustc-args end so they override any earlier defaults.
        script.push_str("rustc_args+=( ${pkgs.lib.escapeShellArgs extraClippyLintArgs} )\n");
    }
    append_driver_invocation(&mut script, driver, collect_unused_deps);

    Ok(script)
}

// One `--extern` per dependency normally; two when the workspace compiles
// with `-Zembed-metadata=no` (policy.compiler.embedMetadata = false). Under
// that flag a dependency rlib carries only a metadata stub, and rustc does
// not fall back to `-L` search for a crate pinned by `--extern`. Cargo's
// -Zno-embed-metadata scheme passes the same crate name twice, once for the
// rlib (link input) and once for the sibling .rmeta (full metadata); mirror
// that whenever the sibling exists. A dependency whose extern-path is not an
// rlib (proc-macro .so, rmeta-only fallback) has no sibling to add and keeps
// a single --extern.
//
// A `dylib` dependency is the third shape, and it is one `--extern` per
// published artifact rather than per dependency. A dylib unit emits a shared
// library and (usually) an rlib from one rustc invocation, and only rustc can
// decide which of them a given consumer links: that follows from the
// consumer's own crate type and `-C prefer-dynamic`, not from anything visible
// here. Cargo does the same -- `cargo build -v` over a
// `crate-type = ["rlib", "dylib"]` dependency emits two `--extern` flags for
// the one crate name, `libfoo.rlib` and `libfoo.so`. Naming only the rlib (all
// a single `extern-path` can express) silently absorbs the dependency into
// every consumer statically, which for a crate holding process-global state
// means one process carrying several copies of it. That is the whole reason a
// dylib is being built at all.
//
// The dylib branch ignores `embed_metadata` because `thin_metadata_eligible`
// refuses a dylib unit outright: its rlib always carries full metadata, so
// there is no stub to repair with a sibling .rmeta.
//
// A dependency without a dylib crate type keeps the exact `--extern` lines it
// had before this existed, so a graph with no dylib in it renders byte-for-byte
// what it used to. The `extern-path` fallback inside the dylib branch covers
// hand-built `extraUnits` injections: only units this renderer installs are
// guaranteed to publish `extern-paths`.
fn append_dependency_externs(
    script: &mut String,
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    unit: &Unit,
    options: &RenderOptions,
) -> Result<()> {
    for dependency in &unit.dependencies {
        let dep_unit = &graph.units[dependency.index];
        if dep_unit.is_run_custom_build() || dep_unit.is_bin() {
            continue;
        }
        if dep_unit.target.has_crate_type("dylib") {
            let dep_name = nix_attr(&prepared.names[dependency.index]);
            let crate_name = &dependency.extern_crate_name;
            writeln!(
                script,
                "if [ -f \"${{units.{dep_name}}}/nix-support/extern-paths\" ]; then\n  while IFS= read -r extern_artifact; do\n    rustc_args+=( --extern \"{crate_name}=$extern_artifact\" )\n  done < \"${{units.{dep_name}}}/nix-support/extern-paths\"\nelse\n  rustc_args+=( --extern \"{crate_name}=$(cat ${{units.{dep_name}}}/nix-support/extern-path)\" )\nfi",
            )?;
        } else if options.embed_metadata {
            writeln!(
                script,
                "rustc_args+=( --extern \"{}=$(cat ${{units.{}}}/nix-support/extern-path)\" )",
                dependency.extern_crate_name,
                nix_attr(&prepared.names[dependency.index])
            )?;
        } else {
            writeln!(
                script,
                "extern_path=\"$(cat ${{units.{}}}/nix-support/extern-path)\"",
                nix_attr(&prepared.names[dependency.index])
            )?;
            writeln!(
                script,
                "rustc_args+=( --extern \"{}=$extern_path\" )",
                dependency.extern_crate_name
            )?;
            writeln!(
                script,
                "if [ \"''${{extern_path%.rlib}}\" != \"$extern_path\" ] && [ -f \"''${{extern_path%.rlib}}.rmeta\" ]; then\n  rustc_args+=( --extern \"{}=''${{extern_path%.rlib}}.rmeta\" )\nfi",
                dependency.extern_crate_name
            )?;
        }
    }
    Ok(())
}

// Point a linking unit at the store paths of the shared libraries it will ask
// for at run time.
//
// rustc records a dependency dylib in the linked artifact by SONAME (ELF) or
// `@rpath/<name>` install name (Mach-O), never by the absolute path it was
// given, so a binary linked against `libfoo-<hash>.so` looks for that bare name
// through the dynamic loader's search path and finds nothing. Cargo has no
// answer to copy here -- it leaves the artifacts in one `target/` directory and
// relies on `cargo run` setting `LD_LIBRARY_PATH` -- but under one derivation
// per unit each dylib has its own store path, and the graph is the only place
// that knows which.
//
// Transitive rather than direct: a dylib that itself links a dylib puts that
// second one in the consumer's `DT_NEEDED` too, so the rpath list has to cover
// the closure, matching the `-L dependency=` loop above.
//
// This runs only for units that actually link a dylib. Nothing else in the
// graph gains a flag, so a workspace with no dylib crate type renders exactly
// the bytes it did before.
//
// The Rust standard library is deliberately not on this list. libstd only turns
// dynamic under `-C prefer-dynamic`, which is a caller's flag arriving through
// `extraRustcArgs`, so the caller that asks for it also supplies
// `-C link-arg=-Wl,-rpath,<toolchain>/lib/rustlib/<target>/lib` through
// `extraLinkRustcArgsForPlatform`. Adding it unconditionally here would put the
// whole Rust toolchain in the runtime closure of packages that never asked.
fn append_dylib_rpath_args(
    script: &mut String,
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    index: usize,
    driver: Driver,
) -> Result<()> {
    // Only the rustc driver links. Clippy stops at metadata, the panic scan
    // emits bare objects, and the test-name dump stops before codegen, so an
    // rpath in any of them would be a flag that changes a unit's script and
    // nothing else.
    if driver != Driver::Rustc || !unit_links(&graph.units[index]) {
        return Ok(());
    }
    for dep_index in &prepared.transitive_unit_deps[index] {
        if !graph.units[*dep_index].target.has_crate_type("dylib") {
            continue;
        }
        writeln!(
            script,
            "rustc_args+=( -C \"link-arg=-Wl,-rpath,${{units.{}}}/lib\" )",
            nix_attr(&prepared.names[*dep_index])
        )?;
    }
    Ok(())
}

fn append_bin_exe_env(
    script: &mut String,
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    index: usize,
) -> Result<()> {
    for (bin_name, bin_index) in same_package_bins(graph, index) {
        let env_name = format!("CARGO_BIN_EXE_{bin_name}");
        writeln!(
            script,
            "rustc_env+=( {} )",
            shell::quote(&format!(
                "{env_name}=${{units.{}}}/bin/{bin_name}",
                nix_attr(&prepared.names[bin_index])
            ))
        )?;
    }

    Ok(())
}

fn append_driver_invocation(script: &mut String, driver: Driver, collect_unused_deps: bool) {
    let binary = driver.binary();
    if collect_unused_deps {
        // `collect_unused_deps` is only ever true for the rustc driver; the
        // diagnostics-capture path is rustc-specific and reuses the rustc
        // unused-extern JSON shape.
        debug_assert_eq!(driver, Driver::Rustc);
        script.push_str("rustc_diagnostics=build/rustc-diagnostics.jsonl\n");
        script.push_str("set +e\n");
        script.push_str("set -x\n");
        let _ = writeln!(
            script,
            "env \"''${{rustc_env[@]}}\" {binary} \"''${{rustc_args[@]}}\" 2> \"$rustc_diagnostics\""
        );
        script.push_str("rustc_status=$?\n");
        script.push_str("set +x\n");
        script.push_str("set -e\n");
        script.push_str("cat \"$rustc_diagnostics\" >&2\n");
        script.push_str("if [ \"$rustc_status\" -ne 0 ]; then\n");
        script.push_str("  exit \"$rustc_status\"\n");
        script.push_str("fi\n");
        script.push_str(
            r#"jq -r 'select(."$message_type" == "unused_extern") | .unused_extern_names[]' "$rustc_diagnostics" | sort -u > build/unused-crate-dependencies
"#,
        );
    } else {
        // Echo the exact driver invocation, then turn xtrace back off so the
        // trailing stdenv fixupPhase (compressManPages et al.) does not stream
        // its own `set -x` trace into the build log. Under `set -e` a failed
        // driver aborts before the `set +x`, which is fine: the build stops with
        // the compiler error and no fixup runs. Mirrors the scoped trace in the
        // rustc-diagnostics branch above. Without this, every successful rust
        // derivation floods CI logs (and GitHub truncates the step).
        script.push_str("set -x\n");
        let _ = writeln!(
            script,
            "env \"''${{rustc_env[@]}}\" {binary} \"''${{rustc_args[@]}}\""
        );
        script.push_str("set +x\n");
    }
}

fn collects_unused_crate_dependencies(unit: &Unit, options: &RenderOptions) -> bool {
    options.deny_unused_crate_dependencies && !unit.is_external()
}

// A root (bin) or test unit gets an explicit `-o build/<name>` path (see the
// `Driver::Rustc` match arm below), which alone fixes the output filename.
// `-C extra-filename` only matters for `--out-dir`-style outputs (libs, rlibs,
// proc-macros) where multiple units can otherwise collide; combining both
// flags is what makes rustc warn "ignoring -C extra-filename flag due to -o
// flag" on every such build (index#1975).
fn uses_explicit_output_path(unit: &Unit, driver: Driver) -> bool {
    driver == Driver::Rustc && (unit.is_bin() || unit.is_test())
}

fn push_rustc_args(script: &mut String, unit: &Unit, hash: &str, driver: Driver) {
    push_arg(script, "--crate-name");
    push_arg(script, &unit.target.name.replace('-', "_"));
    push_arg(script, "--edition");
    push_arg(script, &unit.target.edition);

    for crate_type in &unit.target.crate_types {
        push_arg(script, "--crate-type");
        push_arg(script, crate_type);
    }
    if unit.is_proc_macro() {
        push_arg(script, "-C");
        push_arg(script, "prefer-dynamic");
    }

    push_codegen(script, "opt-level", &unit.profile.opt_level);
    push_codegen(script, "debuginfo", unit.profile.debuginfo.rustc_value());
    if let Some(lto) = lto_for_unit(unit) {
        push_codegen(script, "lto", lto);
    }
    if let Some(codegen_units) = unit.profile.codegen_units {
        push_codegen(script, "codegen-units", &codegen_units.to_string());
    }
    push_codegen(
        script,
        "debug-assertions",
        if unit.profile.debug_assertions {
            "yes"
        } else {
            "no"
        },
    );
    push_codegen(
        script,
        "overflow-checks",
        if unit.profile.overflow_checks {
            "yes"
        } else {
            "no"
        },
    );
    push_arg(script, "-C");
    push_arg(
        script,
        &format!("panic={}", unit.profile.panic.rustc_value()),
    );
    if let Some(strip) = unit.profile.strip.rustc_value() {
        push_arg(script, "-C");
        push_arg(script, &format!("strip={strip}"));
    }
    if let Some(split_debuginfo) = &unit.profile.split_debuginfo {
        push_arg(script, "-C");
        push_arg(script, &format!("split-debuginfo={split_debuginfo}"));
    }
    if unit.profile.rpath {
        push_arg(script, "-C");
        push_arg(script, "rpath=yes");
    }
    push_codegen(script, "metadata", hash);
    // rustc warns "ignoring -C extra-filename flag due to -o flag" when both
    // are given; the explicit `-o build/<name>` path fully determines a bin/test
    // unit's output name, so extra-filename (which only disambiguates
    // `--out-dir`-style outputs) would be a no-op there anyway.
    if !uses_explicit_output_path(unit, driver) {
        push_codegen(script, "extra-filename", &format!("-{hash}"));
    }

    for rustflag in &unit.profile.rustflags {
        push_arg(script, rustflag);
    }
    for rustflag in &unit.lint_rustflags {
        push_arg(script, rustflag);
    }
    for arg in &unit.check_cfg_args {
        push_arg(script, arg);
    }
    for feature in &unit.features {
        push_arg(script, "--cfg");
        push_arg(script, &format!("feature=\"{feature}\""));
    }
    if unit.uses_test_harness() {
        push_arg(script, "--test");
    } else if unit.mode == UnitMode::Test {
        push_arg(script, "--cfg");
        push_arg(script, "test");
    }
    if let Some(platform) = &unit.platform {
        push_arg(script, "--target");
        push_arg(script, platform);
    }
    if unit.is_external() {
        push_arg(script, "--cap-lints");
        push_arg(script, "warn");
    }
}

// `-Zembed-metadata=no` is only sound for a unit that both emits a separate
// full .rmeta (so dependents keep complete metadata to compile against) and is
// consumed as an rlib. A proc-macro's dylib IS its metadata carrier and emits
// no .rmeta in this engine; a `dylib` crate type is consumed directly by
// dependents, so stripping its metadata would break them. Restricting the flag
// to pure lib/rlib units keeps every other consumer contract intact. Bin,
// test, and benchmark units fall out automatically via their `bin` crate type.
fn thin_metadata_eligible(unit: &Unit) -> bool {
    !unit.is_proc_macro()
        && !unit.target.crate_types.is_empty()
        && unit
            .target
            .crate_types
            .iter()
            .all(|crate_type| matches!(crate_type.as_str(), "lib" | "rlib"))
}

fn lto_for_unit(unit: &Unit) -> Option<&'static str> {
    let allowed = unit
        .target
        .crate_types
        .iter()
        .all(|crate_type| matches!(crate_type.as_str(), "bin" | "cdylib" | "staticlib"));
    allowed.then(|| unit.profile.lto.rustc_value()).flatten()
}

fn push_codegen(script: &mut String, key: &str, value: &str) {
    push_arg(script, "-C");
    push_arg(script, &format!("{key}={value}"));
}

fn push_arg(script: &mut String, value: &str) {
    let _ = writeln!(script, "rustc_args+=( {} )", shell::quote(value));
}

/// Rewrites the store paths rustc would otherwise bake into this unit's output.
///
/// Two prefixes, because half a remap leaves half the closure:
///
/// - `$src`, the unit's own scoped source, which supplies the `file!()` of
///   every panic location in this crate (see [`source_remap_prefix`]).
/// - the toolchain's `rust-src`, which supplies them for std. Upstream
///   compiles std against a virtual `/rustc/<commit>` prefix, but rustc
///   translates that straight back to the local `rust-src` directory when the
///   component is installed (`-Ztranslate-remapped-path-to-local-path`, on by
///   default), so a single `unwrap` on a std generic monomorphized here pins
///   the entire ~1.9 GiB toolchain into the binary's closure. The directory is
///   absent from toolchains built without `rust-src`, where the mapping simply
///   never matches.
///
/// Emitted for every driver and every unit rather than left to
/// `extraRustcArgs`, so that a crate baking a location and the crate linking
/// it agree, and so that a caller who has never heard of this gets the small
/// closure anyway.
///
/// The cost is that panic messages and backtraces name `/build/<crate>-<ver>`
/// and `/rustc` instead of a real store path. Nothing resolves those paths;
/// `include_str!`, `include!` and `env!("CARGO_MANIFEST_DIR")` all read the
/// real filesystem and are untouched by `--remap-path-prefix`, so a crate that
/// deliberately embeds a store path for a data file or helper binary keeps
/// both the path and its reference. Coverage does read them back, and
/// `normalize_source_path` in the units template maps them to workspace
/// relative paths using the `remapPrefix` recorded in `sourceAudit`.
fn append_path_remap_args(script: &mut String, source: &SourceEntry) {
    let _ = writeln!(
        script,
        "rustc_args+=( --remap-path-prefix \"$src={}\" )",
        source.remap_prefix
    );
    script.push_str(
        "rustc_args+=( --remap-path-prefix \"${rustToolchain}/lib/rustlib/src/rust=/rustc\" )\n",
    );
}

fn append_target_linker_arg(script: &mut String, unit: &Unit) {
    let Some(platform) = &unit.platform else {
        return;
    };
    let env_name = cargo_target_linker_env_name(platform);
    let _ = writeln!(script, "if [ \"''${{{env_name}+x}}\" = x ]; then");
    let _ = writeln!(script, "  rustc_args+=( -C \"linker=''${{{env_name}}}\" )");
    script.push_str("fi\n");
}

fn cargo_target_linker_env_name(target: &str) -> String {
    format!("CARGO_TARGET_{}_LINKER", cargo_env_segment(target))
}

// Cargo's environment-variable normalization for a name segment: ASCII letters
// uppercased, digits and `_` kept, every other byte mapped to `_`. Shared by
// CARGO_FEATURE_* and the target portion of CARGO_TARGET_<triple>_LINKER.
fn cargo_env_segment(value: &str) -> String {
    let mut segment = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z' => segment.push(char::from(byte.to_ascii_uppercase())),
            b'A'..=b'Z' | b'0'..=b'9' | b'_' => segment.push(char::from(byte)),
            _ => segment.push('_'),
        }
    }
    segment
}

fn append_extra_rustc_args(script: &mut String, unit: &Unit) {
    let package_name = nix_attr(&unit.package_name());
    let platform = unit
        .platform
        .as_ref()
        .map_or_else(|| "null".to_string(), |platform| nix_attr(platform));
    if !unit.is_custom_build_compile() {
        let _ = writeln!(
            script,
            "${{renderExtraRustcArgs {package_name} {platform}}}"
        );
    }
    if unit_links(unit) {
        let _ = writeln!(script, "${{renderExtraLinkRustcArgs {platform}}}");
    }
}

fn unit_links(unit: &Unit) -> bool {
    !unit.is_custom_build_compile()
        && (unit.is_bin()
            || unit.is_test()
            || unit.is_benchmark()
            || unit.is_proc_macro()
            || unit.target.has_crate_type("cdylib")
            || unit.target.has_crate_type("dylib")
            || unit.target.has_crate_type("staticlib"))
}

fn append_build_script_flag_reader(script: &mut String, run_ref: &str, unit: &Unit) {
    let quoted_run_ref = format!("\"{run_ref}\"");
    let snippets = [
        ("rustc-cfg", "--cfg"),
        ("rustc-link-lib", "-l"),
        ("rustc-link-search", "-L"),
    ];

    script.push('\n');
    for (file, flag) in snippets {
        let flag_arg = shell::quote(flag);
        let _ = writeln!(
            script,
            "if [ -f {quoted_run_ref}/{file} ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && build_script_flags+=( {flag_arg} \"$line\" )\n  done < {quoted_run_ref}/{file}\nfi",
        );
    }
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/rustc-cdylib-link-arg ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && build_script_flags+=( -C \"link-arg=$line\" )\n  done < {quoted_run_ref}/rustc-cdylib-link-arg\nfi",
    );
    append_link_arg_reader(script, &quoted_run_ref, "rustc-link-arg");
    if unit.is_benchmark() {
        append_link_arg_reader(script, &quoted_run_ref, "rustc-link-arg-benches");
    } else if unit.is_test() {
        append_link_arg_reader(script, &quoted_run_ref, "rustc-link-arg-tests");
    } else if unit.is_bin() {
        append_link_arg_reader(script, &quoted_run_ref, "rustc-link-arg-bins");
    }
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/rustc-env ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && export \"$line\"\n  done < {quoted_run_ref}/rustc-env\nfi",
    );
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/cargo-metadata ]; then\n  cp {quoted_run_ref}/cargo-metadata build/cargo-metadata\nfi",
    );
    // Generated source paths enter crate metadata, so mktemp entropy makes
    // separately realized content-addressed dependencies byte-incompatible.
    let _ = writeln!(
        script,
        "compile_out_dir=$NIX_BUILD_TOP/cargo-unit-build-script-out\nmkdir -p \"$compile_out_dir\"\nif [ -d {quoted_run_ref}/out-dir ]; then\n  cp -R {quoted_run_ref}/out-dir/. \"$compile_out_dir\"/\nfi\nrustc_args+=( --remap-path-prefix \"$compile_out_dir=/cargo-unit-build-script-out\" )\nrustc_env+=( OUT_DIR=\"$compile_out_dir\" )\n"
    );
}

fn append_link_arg_reader(script: &mut String, quoted_run_ref: &str, file: &str) {
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/{file} ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && build_script_flags+=( -C \"link-arg=$line\" )\n  done < {quoted_run_ref}/{file}\nfi",
    );
}

// A killed content-addressed build on a store without a build sandbox (the
// darwin default) can leave its resolved output path behind in /nix/store as an
// invalid directory owned by the `_nixbld` user that ran it. Before fork rev
// 403b19c5 nix did not remove that orphan before a later rebuild of the same
// drv (the daemon now clears it itself under the build locks, #4112), so a
// fresh builder (a
// different `_nixbld` uid) hits the pre-existing dir at `$out` and the artifact
// `cp` below fails with a bare `cp: ... Permission denied` one phase after rustc
// succeeded, which reads like a linker/OOM failure (#2247). Detect the orphan
// first and fail with the path, its owner, and the recovery recipe. A valid
// sealed store path is always root-owned and never pre-exists for a fresh build,
// so an existing non-writable `$out` here is unambiguously a killed-build
// leftover; we only detect and report it, never delete a store path from a
// builder. This precheck firing on a store whose daemon carries the #4112 fix
// means something new is wrong. A nix builder puts GNU coreutils `stat` first
// on PATH even on darwin, so query the owner with GNU `-c '%U'` first; BSD
// `stat -f '%Su'` is only the fallback (GNU stat would misparse `-f` as
// `--file-system`).
const ORPHAN_OUTPUT_PRECHECK: &str = "\
if [ -e \"$out\" ] && [ ! -w \"$out\" ]; then
  echo >&2 \"error: refusing to install over a pre-existing, non-writable output path:\"
  echo >&2 \"  $out\"
  echo >&2 \"  owner: $(stat -c '%U' \"$out\" 2>/dev/null || stat -f '%Su' \"$out\" 2>/dev/null || echo '?')\"
  echo >&2 \"This is an invalid orphan left by a killed content-addressed build (see index#2247).\"
  echo >&2 \"Nix does not clear it before rebuilding, so this build cannot write its output.\"
  echo >&2 \"Recover on the host (a valid store path is root-owned; an invalid one is not):\"
  echo >&2 \"  nix-store --check-validity \\\"$out\\\"   # nonzero exit => invalid => safe to remove\"
  echo >&2 \"  sudo rm -rf \\\"$out\\\"\"
  exit 1
fi
";

fn render_install_phase(unit: &Unit, options: &RenderOptions, hash: &str) -> String {
    let unused_crate_dependencies_install = if collects_unused_crate_dependencies(unit, options) {
        "\
if [ -s build/unused-crate-dependencies ]; then
  cp build/unused-crate-dependencies $out/nix-support/unused-crate-dependencies
fi
"
    } else {
        ""
    };

    if unit.is_bin() || unit.is_test() {
        format!(
            "\
{ORPHAN_OUTPUT_PRECHECK}mkdir -p $out/bin $out/nix-support
cp {} $out/bin/{}
chmod 755 $out/bin/{}
if [ -f build/cargo-metadata ]; then
  cp build/cargo-metadata $out/nix-support/cargo-metadata
fi
{unused_crate_dependencies_install}
",
            shell::quote(&format!("build/{}", unit.target.name)),
            shell::quote(&unit.target.name),
            shell::quote(&unit.target.name)
        )
    } else {
        let lib_name = unit.target.name.replace('-', "_");
        // Build byproducts are excluded from $out/lib because each one either
        // breaks content-addressed early cutoff or bloats the closure, and
        // none has a consumer there:
        //
        // - `*.d` (dep-info): rustc does NOT apply `--remap-path-prefix` to
        //   dep-info, so the .d names every source file by its raw
        //   /nix/store/...-cargo-unit-source-* path. Measured on
        //   vcfs-0.1.0 (nightly-2026-05-27): the rlib and rmeta contain zero
        //   store-path strings and the installed .d was the output's only
        //   external reference, pinning the whole scoped source into the
        //   unit's runtime closure. Worse, any source byte edit changes $src
        //   and therefore the .d bytes, so a CA rebuild could never converge
        //   to the same output even when the compiled artifacts are
        //   byte-identical, which defeats early cutoff for every dependent.
        // - `rustc-diagnostics.jsonl`: rendered warnings carry line numbers,
        //   so unrelated line shifts churn the bytes. It exists for the jq
        //   unused-extern extraction inside this build only.
        // - `unused-crate-dependencies`: consumed from $out/nix-support
        //   (installed below); the lib copy was incidental duplication.
        //
        // A dylib unit publishes every linkable artifact it produced, one per
        // line, because a consumer has to pass all of them to rustc (see
        // `append_dependency_externs`). `extern-path` below stays the single
        // preferred artifact so existing readers are unaffected.
        //
        // The missing-shared-library check is not paranoia: this file is what
        // makes dynamic linking happen at all, and a dylib unit that quietly
        // produced no shared library would fall back to the rlib and link
        // statically -- succeeding, and losing the single-copy property the
        // dylib exists for. Fail here instead, where the cause is on screen.
        let dylib_extern_paths_install = if unit.target.has_crate_type("dylib") {
            format!(
                "\
shared_library=\"\"
for artifact in \\
  \"$out/lib/lib{lib_name}-{hash}.rlib\" \\
  \"$out/lib/lib{lib_name}-{hash}.so\" \\
  \"$out/lib/lib{lib_name}-{hash}.dylib\" \\
  \"$out/lib/{lib_name}-{hash}.dll\"; do
  if [ -f \"$artifact\" ]; then
    printf '%s\\n' \"$artifact\" >> $out/nix-support/extern-paths
    case \"$artifact\" in
      *.rlib) ;;
      *) shared_library=\"$artifact\" ;;
    esac
  fi
done
if [ -z \"$shared_library\" ]; then
  echo >&2 \"error: unit {lib_name} declares crate-type dylib but produced no shared library in:\"
  ls >&2 \"$out/lib\"
  exit 1
fi
"
            )
        } else {
            String::new()
        };
        format!(
            "\
{ORPHAN_OUTPUT_PRECHECK}mkdir -p $out/lib $out/nix-support
for build_artifact in build/*; do
  case \"$build_artifact\" in
    *.dwo|*.dwp) continue ;;
    *.d|*/rustc-diagnostics.jsonl|*/unused-crate-dependencies) continue ;;
  esac
  cp -R \"$build_artifact\" \"$out/lib/\"
done
if [ -f build/cargo-metadata ]; then
  cp build/cargo-metadata $out/nix-support/cargo-metadata
fi
extern_path=\"\"
for artifact in \\
  \"$out/lib/lib{lib_name}-{hash}.rlib\" \\
  \"$out/lib/lib{lib_name}-{hash}.so\" \\
  \"$out/lib/lib{lib_name}-{hash}.dylib\" \\
  \"$out/lib/{lib_name}-{hash}.dll\" \\
  \"$out/lib/lib{lib_name}-{hash}.rmeta\"; do
  if [ -f \"$artifact\" ]; then
    extern_path=\"$artifact\"
    break
  fi
done
[ -n \"$extern_path\" ] && printf '%s\\n' \"$extern_path\" > $out/nix-support/extern-path
{dylib_extern_paths_install}{unused_crate_dependencies_install}
"
        )
    }
}

fn render_split_debuginfo_sidecar_post_fixup(unit: &Unit) -> String {
    let output_dir = if unit.is_bin() || unit.is_test() {
        "$out/bin"
    } else {
        "$out/lib"
    };
    format!(
        "\
for split_debuginfo_sidecar in build/*.dwo build/*.dwp; do
  [ -e \"$split_debuginfo_sidecar\" ] || continue
  cp \"$split_debuginfo_sidecar\" \"{output_dir}/\"
done
"
    )
}

// Build-dependency package names whose presence marks a build script as one
// that shells out to `cargo metadata` at run time (cbindgen does, from
// `Builder::generate`). Only these runs get the offline cargo config below:
// wiring the vendored registry into *every* build-script run would make each
// one depend on its package's whole vendored dependency closure, re-running
// expensive build scripts (ring, aws-lc-sys) on every unrelated lockfile bump.
const CARGO_INVOKING_BUILD_DEPS: &[&str] = &["cbindgen"];

fn build_script_invokes_cargo(graph: &UnitGraph, compile_index: usize) -> bool {
    let mut seen = BTreeSet::from([compile_index]);
    let mut queue = VecDeque::from([compile_index]);
    while let Some(index) = queue.pop_front() {
        let unit = &graph.units[index];
        if CARGO_INVOKING_BUILD_DEPS.contains(&unit.package_name().as_ref()) {
            return true;
        }
        for dependency in &unit.dependencies {
            if seen.insert(dependency.index) {
                queue.push_back(dependency.index);
            }
        }
    }
    false
}

// The `?rev=`/`?tag=`/`?branch=` selector and trailing commit of a Cargo.lock
// git source string, split out so the offline config can emit the matching
// `[source."<git>"]` replacement block (same shape lib/rust/vendor.nix emits
// for the aggregate vendor dir).
fn parse_git_lock_source(source: &str) -> Result<(String, Option<(String, String)>)> {
    let rest = source
        .strip_prefix("git+")
        .ok_or_else(|| eyre!("git source string `{source}` does not start with git+"))?;
    let rest = rest.split_once('#').map_or(rest, |(head, _)| head);
    match rest.split_once('?') {
        None => Ok((rest.to_string(), None)),
        Some((url, selector)) => {
            let (kind, reference) = selector.split_once('=').ok_or_else(|| {
                eyre!("git source string `{source}` has a selector without a value")
            })?;
            if !matches!(kind, "rev" | "tag" | "branch") {
                return Err(eyre!("git source string `{source}` has selector `{kind}`"));
            }
            Ok((
                url.to_string(),
                Some((kind.to_string(), reference.to_string())),
            ))
        }
    }
}

// The Nix expression for the cargo config an offline `cargo metadata` run
// reads: crates-io (and any git source in the universe) replaced with a
// `linkFarm` holding exactly the package's lockfile dependency closure. The
// linkFarm is scoped to that closure -- not the whole-workspace vendor dir --
// so the run derivation only rebuilds when its own dependencies change.
fn render_metadata_cargo_config(run_unit: &Unit, universe: &[&CargoLockPackage]) -> Result<String> {
    let mut farm_entries = String::new();
    for package in universe {
        write!(
            farm_entries,
            "
        {{ name = {}; path = vendorSources.{}; }}",
            nix_attr(&format!("{}-{}", package.name, package.version)),
            nix_attr(&format!(
                "{}#{}@{}",
                package.source, package.name, package.version
            ))
        )?;
    }

    let mut git_blocks = String::new();
    let mut git_sources = BTreeSet::new();
    for package in universe {
        if package.source.starts_with("git+") && git_sources.insert(package.source.as_str()) {
            let (url, selector) = parse_git_lock_source(&package.source)?;
            write!(
                git_blocks,
                "
    [source.{}]
    git = {}
",
                nix_attr(&package.source),
                nix_attr(&url)
            )?;
            if let Some((kind, reference)) = selector {
                writeln!(git_blocks, "    {kind} = {}", nix_attr(&reference))?;
            }
            git_blocks.push_str("    replace-with = \"vendored-sources\"\n");
        } else if !package.source.starts_with("registry+") && !package.source.starts_with("sparse+")
        {
            return Err(eyre!(
                "cannot vendor {} {} from `{}` for the {} build script's cargo metadata run",
                package.name,
                package.version,
                package.source,
                run_unit.package_name()
            ));
        }
    }

    Ok(format!(
        r#"pkgs.writeText {config_name} ''
             [source.crates-io]
             replace-with = "vendored-sources"
{git_blocks}
             [source.vendored-sources]
             directory = "${{pkgs.linkFarm {farm_name} [{farm_entries} ]}}"
           ''"#,
        config_name = nix_attr(&format!(
            "{}-{}-metadata-cargo-config.toml",
            run_unit.package_name(),
            run_unit.package_version()
        )),
        farm_name = nix_attr(&format!(
            "{}-{}-metadata-vendor",
            run_unit.package_name(),
            run_unit.package_version()
        )),
    ))
}

fn render_build_script_run(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
    run_index: usize,
    build_script_run: &BuildScriptRun,
) -> Result<String> {
    let run_unit = &graph.units[run_index];
    let compile_unit = &graph.units[build_script_run.compile_index];
    let mut attrs = Attrs::new();

    // A vendored crate's build script can itself shell out to `cargo metadata`
    // (cbindgen does). That invocation treats the crate as a standalone
    // workspace root, so without help it reaches for crates.io from inside the
    // sandbox and dies on DNS. Give exactly those runs what a normal offline
    // cargo build gets: a vendored-registry cargo config scoped to the
    // package's lockfile dependency closure (the run phase also strips the
    // manifest's dev-dependencies, which a workspace lockfile never vendors).
    let offline_cargo_metadata =
        run_unit.is_external() && build_script_invokes_cargo(graph, build_script_run.compile_index);
    if offline_cargo_metadata {
        let universe = options
            .cargo_lock_sources
            .metadata_universe_for_unit(run_unit)?;
        attrs.expr(
            "cargoOfflineConfig",
            &render_metadata_cargo_config(run_unit, &universe)?,
        );
    }

    attrs.string(
        "pname",
        &format!("{}-build-script-output", run_unit.package_name()),
    );
    // Tag so `packageBuildEnv.<package>` reaches the build script's process env:
    // this is the unit that runs build.rs, and a `cargo:rustc-env` it emits in
    // turn flows to this package's compile units.
    attrs.string("packageName", &run_unit.package_name());
    attrs.string("version", run_unit.package_version());
    attrs.expr("src", &prepared.source_ref(run_index));
    attrs.expr(
        "nativeBuildInputs",
        "[ rustToolchain ] ++ extraNativeBuildInputs",
    );

    let mut inputs = vec![format!(
        "units.{}",
        nix_attr(&prepared.names[build_script_run.compile_index])
    )];
    inputs.extend(
        build_script_run
            .dependency_runs
            .iter()
            .map(|index| format!("units.{}", nix_attr(&prepared.names[*index]))),
    );
    attrs.expr("buildInputs", &format!("[ {} ]", inputs.join(" ")));
    // Keep `dontStrip` so the default fixup's `patchelf` never runs on these `.o`
    // relocatables (it errors with "wrong ELF type"); the installPhase below does
    // a targeted `strip --strip-debug` instead.
    attrs.bool("dontStrip", true);
    // Keep build-script outputs INPUT-ADDRESSED (do NOT content-address them),
    // even though crate rlibs are CA. A build script may persist *arbitrary*
    // files in OUT_DIR — compiled `.o`/`.a`, but also generated Rust sources, C
    // headers, bindings, and codegen metadata — and we cannot prove all of it is
    // byte-reproducible. Under floating content addressing any non-reproducible
    // byte makes the output hash vary between builds; after a GC drops one copy,
    // the next build's mismatching realisation makes Nix abort ("Trying to
    // register a realisation ... but we already have another one locally",
    // NixOS/nix#15649) and wedges the store until manual surgery — observed as
    // "build of resolved derivation '<crate>' failed" (rcgen -> ring), triggered
    // by the CI host's periodic nix-gc.
    //
    // Input addressing sidesteps this entirely: the path derives from inputs, so
    // there is exactly one path per derivation (dedup by construction), with no
    // realisation and no reproducibility requirement. The early cutoff CA would
    // buy here is marginal anyway — a build-script output only changes when its
    // build.rs inputs change — so input addressing is strictly the safer trade.
    // (Crate rlibs stay CA via render_unit_derivation: rustc output is
    // deterministic, which is where early cutoff actually pays off.)
    //
    // We still strip debug info below as a best-effort *size* win: C-toolchain
    // crates (ring, aws-lc-sys, lmdb) bake the absolute build path into DWARF
    // `.debug_str` (briansmith/ring#715), bloating `.o`/`.a` with data nothing
    // debugs. Because the output is input-addressed, stripping is no longer
    // load-bearing for correctness, so it is best-effort: a strip failure or a
    // missing `strip` only costs store space. Use `--strip-debug`, never
    // `--strip-all` — the crate links against these objects' symbols.
    attrs.multiline(
        "buildPhase",
        &render_build_script_run_phase(
            graph,
            prepared,
            run_index,
            run_unit,
            compile_unit,
            build_script_run,
            offline_cargo_metadata,
        )?,
    );
    // Best-effort: strip debug info from the build script's compiled `.o`/`.a` to
    // shrink the (input-addressed) output. Skip cleanly if `strip` is absent and
    // tolerate per-file failures — this is size-only, never correctness (see the
    // input-addressing note above), so it must never fail the build.
    attrs.multiline(
        "installPhase",
        "if [ -d \"$out/out-dir\" ] && command -v strip >/dev/null 2>&1; then\n  \
         find \"$out/out-dir\" -type f \\( -name '*.o' -o -name '*.a' \\) -exec strip --strip-debug {} + || true\n\
         fi\n",
    );

    Ok(attrs.render())
}

#[allow(clippy::too_many_lines)]
fn render_build_script_run_phase(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    run_index: usize,
    run_unit: &Unit,
    compile_unit: &Unit,
    build_script_run: &BuildScriptRun,
    offline_cargo_metadata: bool,
) -> Result<String> {
    let mut script = String::new();
    let source = prepared.source_entry(run_index)?;
    // Taken from `build_script_run` rather than passed alongside it: the caller
    // was handing over both the struct and one of its own fields, which is the
    // 8th argument that tripped `clippy::too_many_arguments` (limit 7). One
    // source of truth, and no way for the two to disagree.
    let compile_ref = format!(
        "${{units.{}}}",
        nix_attr(&prepared.names[build_script_run.compile_index])
    );
    let manifest_dir = source_path_expr(source, &crate_root_for_unit(run_unit))?;

    script.push_str("mkdir -p $out\n");
    writeln!(
        script,
        "build_script_manifest_dir_source={}",
        shell::double_quote(&manifest_dir)
    )?;
    writeln!(
        script,
        "build_script_manifest_dir=$out/{}",
        shell::quote(source.package_root())
    )?;
    script.push_str("mkdir -p \"$build_script_manifest_dir\"\n");
    // Includes and symlinks can declare inputs outside the package directory.
    // Preserve their workspace-relative layout when the build script reads
    // those same inputs at runtime through CARGO_MANIFEST_DIR.
    if source.base.scope() == SourceScope::Closure {
        script.push_str("cp -RL \"$src\"/. \"$out\"/\n");
    } else {
        script.push_str(
            "cp -RL \"$build_script_manifest_dir_source\"/. \"$build_script_manifest_dir\"/\n",
        );
    }
    script.push_str("chmod -R u+w \"$build_script_manifest_dir\"\n");
    // Cargo nests OUT_DIR several levels deep
    // (`target/<profile>/build/<pkg>-<hash>/out`), so a build script can write
    // relative to its ancestors. The `v8`/`rusty_v8` crate takes a download
    // lock at `OUT_DIR/../../v8.fslock`; a bare `mktemp -d` gives a path shallow
    // enough that its grandparent is `/`, so that write hits PermissionDenied.
    // Nest two levels under the temp dir so `OUT_DIR/../..` lands back inside
    // the (writable) mktemp dir.
    script.push_str("build_script_out_dir=$(mktemp -d)/build/out\n");
    script.push_str("mkdir -p \"$build_script_out_dir\"\n");
    script.push_str("build_script_env=()\n");
    script.push_str("export OUT_DIR=$build_script_out_dir\n");
    ensure_source_contains_unit(source, run_unit)?;
    script.push_str("export CARGO_MANIFEST_DIR=$build_script_manifest_dir\n");
    if offline_cargo_metadata {
        // A build script that shells out to `cargo metadata` (cbindgen) runs it
        // against this crate's manifest as if it were a standalone workspace.
        // Point crates-io at a vendored linkFarm (built above and handed in as
        // `$cargoOfflineConfig`) and force `--offline`, so the resolve never
        // touches the network.
        //
        // Two edits to the writable manifest copy make the resolve match the
        // vendored universe, which is the crate's dependency closure *as the
        // workspace resolved it*:
        //   * strip the dev-dependency tables — the workspace Cargo.lock never
        //     records a non-member's dev-deps, so they are absent from the
        //     universe, and cbindgen only reads the crate's public-API
        //     dependencies (never its dev-deps) to generate the C header; and
        //   * delete any Cargo.lock the crate shipped to crates.io. That lock
        //     pins versions the crate published against (e.g. dashu-int 0.4.1),
        //     which need not be the versions the workspace resolved and vendored
        //     (0.4.3). Removing it lets cargo re-resolve against the vendored
        //     dir, where the workspace-chosen versions are the only candidates
        //     and satisfy the manifest's own requirements.
        script.push_str(
            "cargo_offline_home=$(mktemp -d)/cargo-home\n\
             mkdir -p \"$cargo_offline_home\"\n\
             cp \"$cargoOfflineConfig\" \"$cargo_offline_home/config.toml\"\n\
             export CARGO_HOME=\"$cargo_offline_home\"\n\
             export CARGO_NET_OFFLINE=true\n\
             rm -f \"$build_script_manifest_dir/Cargo.lock\"\n\
             awk '/^\\[/ { drop = ($0 ~ /dev[_-]dependencies/) } !drop { print }' \\\n  \
             \"$build_script_manifest_dir/Cargo.toml\" > \"$build_script_manifest_dir/Cargo.toml.offline\"\n\
             mv \"$build_script_manifest_dir/Cargo.toml.offline\" \"$build_script_manifest_dir/Cargo.toml\"\n",
        );
    }
    script.push_str("export RUSTC=\"$(type -p rustc)\"\n");
    script.push_str("export RUSTDOC=\"$(type -p rustdoc)\"\n");
    script.push_str("HOST_TRIPLE=\"$($RUSTC -vV | sed -n 's/^host: //p')\"\n");
    script.push_str("export HOST=\"$HOST_TRIPLE\"\n");
    if let Some(platform) = &run_unit.platform {
        writeln!(script, "export TARGET={}", shell::quote(platform))?;
    } else {
        script.push_str("export TARGET=\"$HOST_TRIPLE\"\n");
    }
    writeln!(
        script,
        "export PROFILE={}",
        shell::quote(&run_unit.profile.name)
    )?;
    writeln!(
        script,
        "export OPT_LEVEL={}",
        shell::quote(&run_unit.profile.opt_level)
    )?;
    let package_name = nix_attr(&run_unit.package_name());
    let platform = run_unit
        .platform
        .as_ref()
        .map_or_else(|| "null".to_string(), |platform| nix_attr(platform));
    writeln!(
        script,
        "cargo_encoded_rustflags=( {} )",
        run_unit
            .profile
            .rustflags
            .iter()
            .map(|flag| shell::quote(flag))
            .collect::<Vec<_>>()
            .join(" ")
    )?;
    writeln!(
        script,
        "${{renderCargoEncodedRustflags {package_name} {platform}}}"
    )?;
    script.push_str(
        "export CARGO_ENCODED_RUSTFLAGS=\"$(IFS=$'\\x1f'; printf '%s' \"''${cargo_encoded_rustflags[*]}\")\"\n",
    );
    writeln!(
        script,
        "export DEBUG={}",
        shell::quote(if run_unit.profile.debuginfo.is_enabled() {
            "true"
        } else {
            "false"
        })
    )?;
    script.push_str(concat!("export NUM_JOBS=''", "${NIX_BUILD_CORES:-1}\n"));
    script.push_str(&cargo_package_exports(run_unit)?);
    script.push_str(&cargo_manifest_links_export(run_unit)?);
    append_cargo_feature_exports(&mut script, run_unit);
    append_cargo_cfg_exports(&mut script);
    append_dependency_metadata_exports(&mut script, graph, prepared, build_script_run)?;
    script.push_str("cd \"$CARGO_MANIFEST_DIR\"\n");
    script.push_str("build_script_stdout=$(mktemp)\n");
    script.push_str("build_script_stderr=$(mktemp)\n");
    script.push_str("set +e\n");
    writeln!(
        script,
        "env \"''${{build_script_env[@]}}\" {}/bin/{} > \"$build_script_stdout\" 2> \"$build_script_stderr\"",
        compile_ref,
        shell::quote(&compile_unit.target.name)
    )?;
    script.push_str("build_script_status=$?\n");
    script.push_str("set -e\n");
    script.push_str("cat \"$build_script_stderr\" >&2\n");
    script.push_str("if [ \"$build_script_status\" -ne 0 ]; then\n");
    script.push_str("  cat \"$build_script_stdout\" >&2\n");
    script.push_str("  exit \"$build_script_status\"\n");
    script.push_str("fi\n");
    script.push_str("cp -R \"$build_script_out_dir\" \"$out/out-dir\"\n");
    script.push_str(
        r#"if [ -d "$out/out-dir" ]; then
  while IFS= read -r -d "" build_script_output_file; do
    if grep -Iq -- "$build_script_out_dir" "$build_script_output_file"; then
      substituteInPlace "$build_script_output_file" --replace-fail "$build_script_out_dir" "$out/out-dir"
    fi
  done < <(find "$out/out-dir" -type f -print0)
fi
"#,
    );
    script.push_str(
        r#"
while IFS= read -r line; do
  line="''${line//$build_script_out_dir/$out/out-dir}"
  case "$line" in
    cargo::*)
      normalized="cargo:''${line#cargo::}"
      ;;
    *)
      normalized="$line"
      ;;
  esac

  case "$normalized" in
    cargo:rustc-cfg=*)
      printf '%s\n' "''${normalized#cargo:rustc-cfg=}" >> $out/rustc-cfg
      ;;
    cargo:rustc-link-lib=*)
      printf '%s\n' "''${normalized#cargo:rustc-link-lib=}" >> $out/rustc-link-lib
      ;;
    cargo:rustc-link-search=*)
      printf '%s\n' "''${normalized#cargo:rustc-link-search=}" >> $out/rustc-link-search
      ;;
    cargo:rustc-env=*)
      printf '%s\n' "''${normalized#cargo:rustc-env=}" >> $out/rustc-env
      ;;
    cargo:rustc-cdylib-link-arg=*)
      printf '%s\n' "''${normalized#cargo:rustc-cdylib-link-arg=}" >> $out/rustc-cdylib-link-arg
      ;;
    cargo:rustc-link-arg=*)
      printf '%s\n' "''${normalized#cargo:rustc-link-arg=}" >> $out/rustc-link-arg
      ;;
    cargo:rustc-link-arg-benches=*)
      printf '%s\n' "''${normalized#cargo:rustc-link-arg-benches=}" >> $out/rustc-link-arg-benches
      ;;
    cargo:rustc-link-arg-bins=*)
      printf '%s\n' "''${normalized#cargo:rustc-link-arg-bins=}" >> $out/rustc-link-arg-bins
      ;;
    cargo:rustc-link-arg-tests=*)
      printf '%s\n' "''${normalized#cargo:rustc-link-arg-tests=}" >> $out/rustc-link-arg-tests
      ;;
    cargo:warning=*)
      printf '%s\n' "build script warning: ''${normalized#cargo:warning=}" >&2
      ;;
    cargo:rerun-if-changed=*|cargo:rerun-if-env-changed=*)
      ;;
    cargo:*)
      printf '%s\n' "''${normalized#cargo:}" >> $out/cargo-metadata
      ;;
  esac
done < "$build_script_stdout"
"#,
    );

    Ok(script)
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DependencyPolicyKey {
    pkg_id: String,
    package_name: String,
    package_version: String,
    extern_crate_name: String,
}

// The shell helper every per-package unused-crate-dependency check shares: a
// dependency is unused only when *every* unit of the package that declares it
// reports it unused, so a dependency exercised by one target but not another is
// not flagged.
const UNUSED_CRATE_DEPENDENCIES_HELPER: &str = "      failures=0\n      check_unused() {\n        package=\"$1\"\n        dependency=\"$2\"\n        shift 2\n        unit_count=\"$#\"\n        unused_count=0\n\n        for unit in \"$@\"; do\n          report=\"$unit/nix-support/unused-crate-dependencies\"\n          if [ -f \"$report\" ] && grep -Fxq \"$dependency\" \"$report\"; then\n            unused_count=$((unused_count + 1))\n          fi\n        done\n\n        if [ \"$unused_count\" -eq \"$unit_count\" ]; then\n          printf 'unused dependency in %s: %s\\n' \"$package\" \"$dependency\" >&2\n          failures=1\n        fi\n      }\n\n";

// Per-package unused-crate-dependency checks. Each package's check references
// only that package's own units, so editing one crate rebuilds only its own
// check rather than a single whole-workspace aggregate that fans out to every
// crate. Returns a Nix attrset keyed by cargo package name.
fn render_unused_crate_dependencies_by_package(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
) -> String {
    // package name -> (dependency -> units of that package declaring it)
    let mut by_package: BTreeMap<String, BTreeMap<DependencyPolicyKey, BTreeSet<usize>>> =
        BTreeMap::new();

    for (index, unit) in graph.units.iter().enumerate() {
        if unit.is_run_custom_build() || !collects_unused_crate_dependencies(unit, options) {
            continue;
        }
        for dependency in &unit.dependencies {
            let dep_unit = &graph.units[dependency.index];
            if dep_unit.is_run_custom_build() || dep_unit.is_bin() {
                continue;
            }
            by_package
                .entry(unit.package_name().to_string())
                .or_default()
                .entry(DependencyPolicyKey {
                    pkg_id: unit.pkg_id.clone(),
                    package_name: unit.package_name().to_string(),
                    package_version: unit.package_version().to_string(),
                    extern_crate_name: dependency.extern_crate_name.clone(),
                })
                .or_default()
                .insert(index);
        }
    }

    if by_package.is_empty() {
        return "{ }".to_string();
    }

    let mut out = String::from("{\n");
    for (package_name, dependency_units) in by_package {
        let mut script = String::new();
        let _ = writeln!(
            script,
            "pkgs.runCommand {} {{ nativeBuildInputs = [ pkgs.gnugrep ]; }} ''",
            nix_attr(&format!("{package_name}-unused-crate-dependencies"))
        );
        script.push_str(UNUSED_CRATE_DEPENDENCIES_HELPER);
        for (dependency, unit_indexes) in dependency_units {
            let unit_refs = unit_indexes
                .iter()
                .map(|index| format!("\"${{units.{}}}\"", nix_attr(&prepared.names[*index])))
                .collect::<Vec<_>>()
                .join(" ");
            let package = format!("{} {}", dependency.package_name, dependency.package_version);
            let _ = writeln!(
                script,
                "      check_unused {} {} {unit_refs}",
                shell::quote(&package),
                shell::quote(&dependency.extern_crate_name),
            );
        }
        script.push_str("\n      if [ \"$failures\" -ne 0 ]; then\n        exit 1\n      fi\n      mkdir -p \"$out\"\n    ''");
        let _ = writeln!(out, "      {} = {script};", nix_attr(&package_name));
    }
    out.push_str("    }");
    out
}

// Maps each cargo package name to the attr names of its per-unit clippy
// derivations, so the template can join just that package's clippy units into
// one per-crate gate instead of one whole-workspace aggregate.
fn render_clippy_unit_names_by_package(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    let mut by_package: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (index, unit) in graph.units.iter().enumerate() {
        if !is_clippy_unit_candidate(unit) {
            continue;
        }
        by_package
            .entry(unit.package_name().to_string())
            .or_default()
            .push(prepared.names[index].clone());
    }
    if by_package.is_empty() {
        return "{ }".to_string();
    }
    let mut out = String::from("{\n");
    for (package_name, names) in by_package {
        let _ = writeln!(
            out,
            "      {} = {};",
            nix_attr(&package_name),
            nix_string_list(&names)
        );
    }
    out.push_str("    }");
    out
}

fn cargo_package_exports(unit: &Unit) -> Result<String> {
    let mut script = String::new();
    let package_name = unit.package_name();
    let version = unit.package_version();
    let manifest = optional_cargo_manifest_package(unit)?;
    // Build scripts observe Cargo's split version fields, including the empty
    // prerelease string. ring uses CARGO_PKG_VERSION_PRE in its links invariant.
    let version_without_build_metadata = version.split_once('+').map_or(version, |(base, _)| base);
    let (version_core, version_pre) = version_without_build_metadata
        .split_once('-')
        .unwrap_or((version_without_build_metadata, ""));
    let mut version_parts = version_core.split('.');
    let major = version_parts.next().unwrap_or("0");
    let minor = version_parts.next().unwrap_or("0");
    let patch = version_parts.next().unwrap_or("0");

    let metadata = manifest
        .as_ref()
        .map(cargo_manifest_package_metadata)
        .unwrap_or_default();

    // CARGO_CRATE_NAME is the rust identifier rustc receives via `--crate-name`.
    // Cargo normalizes the target's name (`-` → `_`); crates like rmcp read this
    // via `env!()` at compile time.
    let crate_name = unit.target.name.replace('-', "_");
    let is_bin = unit.target.has_kind("bin");

    for (name, value) in [
        ("CARGO_CRATE_NAME", crate_name.as_str()),
        ("CARGO_PKG_NAME", package_name.as_ref()),
        ("CARGO_PKG_VERSION", version),
        ("CARGO_PKG_VERSION_MAJOR", major),
        ("CARGO_PKG_VERSION_MINOR", minor),
        ("CARGO_PKG_VERSION_PATCH", patch),
        ("CARGO_PKG_VERSION_PRE", version_pre),
        ("CARGO_PKG_AUTHORS", metadata.authors.as_str()),
        ("CARGO_PKG_DESCRIPTION", metadata.description.as_str()),
        ("CARGO_PKG_HOMEPAGE", metadata.homepage.as_str()),
        ("CARGO_PKG_REPOSITORY", metadata.repository.as_str()),
        ("CARGO_PKG_LICENSE", metadata.license.as_str()),
        ("CARGO_PKG_LICENSE_FILE", metadata.license_file.as_str()),
        ("CARGO_PKG_RUST_VERSION", metadata.rust_version.as_str()),
    ] {
        let _ = writeln!(script, "export {name}={}", shell_env_value(value));
    }

    if is_bin {
        let _ = writeln!(
            script,
            "export CARGO_BIN_NAME={}",
            shell_env_value(&unit.target.name)
        );
    }

    Ok(script)
}

fn cargo_manifest_links_export(unit: &Unit) -> Result<String> {
    // Cargo injects package.links for build.rs. nix-cargo-unit runs build scripts
    // outside Cargo, and crates like ring panic when CARGO_MANIFEST_LINKS is absent.
    Ok(cargo_manifest_package(unit)?
        .and_then(|package| package.links)
        .map(|links| format!("export CARGO_MANIFEST_LINKS={}\n", shell_env_value(&links)))
        .unwrap_or_default())
}

fn shell_env_value(value: &str) -> String {
    nix_indented_string_fragment(&shell_double_quote_literal(value))
}

fn shell_double_quote_literal(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`")
    )
}

fn nix_indented_string_fragment(value: &str) -> String {
    value.replace("''", "'''").replace("${", "''${")
}

fn nix_indented_string(value: &str) -> String {
    format!("''\n{value}''")
}

#[derive(Default)]
struct CargoManifestPackageMetadata {
    authors: String,
    description: String,
    homepage: String,
    repository: String,
    license: String,
    license_file: String,
    rust_version: String,
}

fn cargo_manifest_package_metadata(package: &CargoManifestPackage) -> CargoManifestPackageMetadata {
    CargoManifestPackageMetadata {
        authors: manifest_string_list(package.authors.as_ref()).join(":"),
        description: manifest_string(package.description.as_ref()),
        homepage: manifest_string(package.homepage.as_ref()),
        repository: manifest_string(package.repository.as_ref()),
        license: manifest_string(package.license.as_ref()),
        license_file: manifest_string(package.license_file.as_ref()),
        rust_version: manifest_string(package.rust_version.as_ref()),
    }
}

fn manifest_string(value: Option<&toml::Value>) -> String {
    value
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn manifest_string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn optional_cargo_manifest_package(unit: &Unit) -> Result<Option<CargoManifestPackage>> {
    let manifest_path = crate_root_for_unit(unit).join("Cargo.toml");
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err)
                .wrap_err_with(|| format!("reading package manifest {}", manifest_path.display()));
        }
    };

    cargo_manifest_package_from_str(&manifest)
        .wrap_err_with(|| format!("parsing package manifest {}", manifest_path.display()))
}

fn cargo_manifest_package(unit: &Unit) -> Result<Option<CargoManifestPackage>> {
    let manifest_path = crate_root_for_unit(unit).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .wrap_err_with(|| format!("reading package manifest {}", manifest_path.display()))?;

    cargo_manifest_package_from_str(&manifest)
        .wrap_err_with(|| format!("parsing package manifest {}", manifest_path.display()))
}

fn cargo_manifest_package_from_str(manifest: &str) -> Result<Option<CargoManifestPackage>> {
    let manifest: CargoManifest = toml::from_str(manifest)?;
    Ok(manifest.package)
}

#[cfg(test)]
fn cargo_manifest_package_links(manifest: &str) -> Result<Option<String>> {
    Ok(cargo_manifest_package_from_str(manifest)?.and_then(|package| package.links))
}

fn append_cargo_feature_exports(script: &mut String, unit: &Unit) {
    for feature in &unit.features {
        let _ = writeln!(script, "export {}=1", cargo_feature_env_name(feature));
    }
}

fn cargo_feature_env_name(feature: &str) -> String {
    format!("CARGO_FEATURE_{}", cargo_env_segment(feature))
}

fn append_cargo_cfg_exports(script: &mut String) {
    // Cargo normally exports CARGO_CFG_* before build.rs. Direct build-script
    // execution has to synthesize them or target-sensitive crates like libm fail.
    script.push_str(
        r#"cargo_cfg_output=$(mktemp)
"$RUSTC" --print cfg --target "$TARGET" > "$cargo_cfg_output"
while IFS= read -r cargo_cfg_line; do
  case "$cargo_cfg_line" in
    *=*)
      cargo_cfg_key="''${cargo_cfg_line%%=*}"
      cargo_cfg_value="''${cargo_cfg_line#*=}"
      cargo_cfg_value="''${cargo_cfg_value%\"}"
      cargo_cfg_value="''${cargo_cfg_value#\"}"
      ;;
    *)
      cargo_cfg_key="$cargo_cfg_line"
      cargo_cfg_value=""
      ;;
  esac

  cargo_cfg_env="CARGO_CFG_$(printf '%s' "$cargo_cfg_key" | tr '[:lower:]-' '[:upper:]_')"
  if [ "''${!cargo_cfg_env+x}" = x ] && [ -n "$cargo_cfg_value" ]; then
    export "$cargo_cfg_env=''${!cargo_cfg_env},$cargo_cfg_value"
  else
    export "$cargo_cfg_env=$cargo_cfg_value"
  fi
done < "$cargo_cfg_output"
"#,
    );
}

fn append_direct_dependency_metadata_exports(
    script: &mut String,
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    index: usize,
) -> Result<()> {
    for dep in &graph.units[index].dependencies {
        let dep_unit = &graph.units[dep.index];
        if dep_unit.is_run_custom_build() {
            continue;
        }
        let Some(links) =
            optional_cargo_manifest_package(dep_unit)?.and_then(|package| package.links)
        else {
            continue;
        };

        let dep_ref = format!("${{units.{}}}", nix_attr(&prepared.names[dep.index]));
        let env_prefix = cargo_links_env_prefix(&links);
        let _ = writeln!(
            script,
            r#"# Cargo exposes dependency build-script metadata through DEP_<links>_*.
if [ -f "{dep_ref}/nix-support/cargo-metadata" ]; then
  while IFS= read -r cargo_metadata_line; do
    case "$cargo_metadata_line" in
      *=*)
        cargo_metadata_key="''${{cargo_metadata_line%%=*}}"
        cargo_metadata_value="''${{cargo_metadata_line#*=}}"
        rustc_env+=( "DEP_{env_prefix}_$cargo_metadata_key=$cargo_metadata_value" )
        ;;
    esac
  done < "{dep_ref}/nix-support/cargo-metadata"
fi"#,
        );
    }

    Ok(())
}

fn append_dependency_metadata_exports(
    script: &mut String,
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    build_script_run: &BuildScriptRun,
) -> Result<()> {
    for dep_run_index in &build_script_run.dependency_runs {
        let dep_run_unit = &graph.units[*dep_run_index];
        let Some(links) = cargo_manifest_package(dep_run_unit)?.and_then(|package| package.links)
        else {
            continue;
        };
        let dep_run_ref = format!("${{units.{}}}", nix_attr(&prepared.names[*dep_run_index]));
        let env_prefix = cargo_links_env_prefix(&links);
        let _ = writeln!(
            script,
            r#"# Cargo exposes metadata from build-script dependencies through DEP_<links>_*.
# aws-lc-rs uses these variables to find the aws-lc-sys headers and link outputs.
if [ -f "{dep_run_ref}/cargo-metadata" ]; then
  while IFS= read -r cargo_metadata_line; do
    case "$cargo_metadata_line" in
      *=*)
        cargo_metadata_key="''${{cargo_metadata_line%%=*}}"
        cargo_metadata_value="''${{cargo_metadata_line#*=}}"
        build_script_env+=( "DEP_{env_prefix}_$cargo_metadata_key=$cargo_metadata_value" )
        cargo_metadata_env="DEP_{env_prefix}_$(printf '%s' "$cargo_metadata_key" | tr '[:lower:]' '[:upper:]' | sed 's/[^[:alnum:]_]/_/g')"
        export "$cargo_metadata_env=$cargo_metadata_value"
        ;;
    esac
  done < "{dep_run_ref}/cargo-metadata"
fi"#
        );
    }
    Ok(())
}

fn cargo_links_env_prefix(links: &str) -> String {
    links
        .chars()
        .map(|ch| match ch {
            'a'..='z' => ch.to_ascii_uppercase(),
            '-' => '_',
            _ => ch,
        })
        .collect()
}

fn crate_root_for_unit(unit: &Unit) -> PathBuf {
    let source = Path::new(&unit.target.src_path);
    if let Some(manifest_root) = nearest_manifest_root(source) {
        return manifest_root;
    }

    if source.file_name().is_some_and(|name| name == "build.rs") {
        return source.parent().unwrap_or(source).to_path_buf();
    }

    let raw = unit.target.src_path.as_str();
    if let Some((root, _)) = raw.split_once("/src/") {
        return PathBuf::from(root);
    }

    source.parent().unwrap_or(source).to_path_buf()
}

fn nearest_manifest_root(source: &Path) -> Option<PathBuf> {
    let mut dir = source.parent()?;
    loop {
        // Cargo sets CARGO_MANIFEST_DIR to the package root even when the
        // build script entrypoint is nested, as aws-lc-sys does with builder/main.rs.
        if dir.join("Cargo.toml").is_file() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// A test-only input contract is never inferred from a filename or Rust text.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SourceInputClass {
    Complete,
    Production,
}

fn source_input_class(unit: &Unit) -> SourceInputClass {
    // Check-mode library units do not set cfg(test); test harness units do.
    // Unknown modes and user flags that can enable test compilation keep all
    // inputs. Build-script packages are handled separately at the boundary.
    let opaque_test_flags = unit
        .profile
        .rustflags
        .iter()
        .chain(&unit.lint_rustflags)
        .chain(&unit.check_cfg_args)
        .any(|flag| {
            flag == "test"
                || flag.starts_with("--test")
                || flag.starts_with("--cfg")
                || flag.starts_with('@')
        });
    if unit.is_library()
        && !unit.is_test()
        && !unit.is_benchmark()
        && matches!(unit.mode, UnitMode::Build | UnitMode::Check)
        && !opaque_test_flags
    {
        SourceInputClass::Production
    } else {
        SourceInputClass::Complete
    }
}

/// Opt-in contract: these paths are read only by test targets or cfg(test) modules.
/// Package owners must audit procedural macros and other implicit file reads.
/// Known includes, path attributes and uncertain source syntax still retain
/// inputs through the existing conservative source-closure scanner.
fn test_source_exclusions(
    unit: &Unit,
    package_root: &Path,
    has_build_script: bool,
) -> Result<BTreeSet<PathBuf>> {
    let manifest_path = package_root.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(BTreeSet::new());
    }
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(manifest_path)?)?;
    let setting = ["package", "metadata", "ix", "inputs", "test-only"]
        .iter()
        .try_fold(&manifest, |value, key| value.get(*key));
    let Some(setting) = setting else {
        return Ok(BTreeSet::new());
    };
    let paths = setting.as_array().ok_or_else(|| {
        eyre!("package.metadata.ix.inputs.test-only must be an array of relative paths")
    })?;
    let mut excluded = BTreeSet::new();
    for value in paths {
        let relative = Path::new(value.as_str().ok_or_else(|| {
            eyre!("package.metadata.ix.inputs.test-only entries must be strings")
        })?);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(eyre!(
                "test-only input must be a package-relative path: {}",
                relative.display()
            ));
        }
        let path = package_root.join(relative);
        let mut ancestor = path.as_path();
        while ancestor != package_root {
            if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
                return Err(eyre!(
                    "test-only input cannot traverse a symlink: {}",
                    path.display()
                ));
            }
            ancestor = ancestor
                .parent()
                .ok_or_else(|| eyre!("test-only input has no parent"))?;
        }
        if !(path.is_file() || path.is_dir()) || !excluded.insert(path) {
            return Err(eyre!(
                "test-only inputs must be unique regular files or directories"
            ));
        }
    }
    // Explicit/custom and implicit build scripts can read arbitrary files.
    let package_build = manifest
        .get("package")
        .and_then(|package| package.get("build"));
    let manifest_build_script = match package_build {
        Some(toml::Value::Boolean(false)) => false,
        Some(_) => true,
        None => package_root.join("build.rs").exists(),
    };
    if has_build_script
        || manifest_build_script
        || source_input_class(unit) == SourceInputClass::Complete
    {
        excluded.clear();
    }
    Ok(excluded)
}

fn source_entry_for_unit(
    unit: &Unit,
    options: &RenderOptions,
    workspace_packages: &BTreeSet<PathBuf>,
    has_build_script: bool,
) -> Result<SourceEntry> {
    if unit.is_external() {
        let vendor_root = options.vendor_root.as_ref().ok_or_else(|| {
            eyre!(
                "external unit {} {} needs --vendor-root to scope its vendored source",
                unit.package_name(),
                unit.package_version()
            )
        })?;
        let scoped = vendored_source_root_for_unit(unit, vendor_root)?;

        let base = match scoped.scope {
            SourceScope::Package => SourceBase::VendorPackage,
            SourceScope::Closure => SourceBase::VendorClosure,
        };
        let source_key = vendor_source_key(unit, &options.cargo_lock_sources)?;

        return Ok(SourceEntry {
            name: source_name(base, unit, &source_key, &scoped.relative),
            base,
            root: scoped.root,
            relative: scoped.relative,
            include_relatives: scoped.include_relatives,
            excluded_relatives: Vec::new(),
            source_key,
            remap_prefix: source_remap_prefix(unit),
        });
    }

    let (scoped, excluded_relatives) = local_source_root_for_unit(
        unit,
        &options.workspace_root,
        workspace_packages,
        has_build_script,
    )?;

    let mut source_key = local_source_key(unit);
    let package_root =
        local_package_root_from_pkg_id(&unit.pkg_id).unwrap_or_else(|| crate_root_for_unit(unit));
    if !test_source_exclusions(unit, &package_root, has_build_script)?.is_empty() {
        source_key.push_str("#production");
    }

    Ok(SourceEntry {
        name: source_name(SourceBase::Workspace, unit, &source_key, &scoped.relative),
        base: match scoped.scope {
            SourceScope::Package => SourceBase::Workspace,
            SourceScope::Closure => SourceBase::WorkspaceClosure,
        },
        root: scoped.root,
        relative: scoped.relative,
        include_relatives: scoped.include_relatives,
        excluded_relatives,
        source_key,
        remap_prefix: source_remap_prefix(unit),
    })
}

fn nested_manifest_roots(package_root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut roots = BTreeSet::new();
    let mut pending = vec![package_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if !directory.is_dir() {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(package_root)?;
            if relative.components().next().is_some_and(|part| {
                matches!(
                    part.as_os_str().to_str(),
                    Some("src" | "tests" | "examples" | "benches")
                )
            }) {
                continue;
            }
            if path.join("Cargo.toml").is_file() {
                roots.insert(path);
            } else {
                pending.push(path);
            }
        }
    }
    Ok(roots)
}

/// Opt-in boundary: callers must audit build tools and procedural macros for
/// reads into nested packages before declaring this source split. Textual
/// include detection preserves known references; it is not such an audit.
fn nested_package_filter_enabled(package_root: &Path) -> Result<bool> {
    let path = package_root.join("Cargo.toml");
    if !path.is_file() {
        return Ok(false);
    }
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(path)?)?;
    let setting = [
        "package",
        "metadata",
        "ix",
        "inputs",
        "exclude-nested-packages",
    ]
    .iter()
    .try_fold(&manifest, |value, key| value.get(*key));
    match setting {
        None => Ok(false),
        Some(toml::Value::Boolean(enabled)) => Ok(*enabled),
        Some(_) => Err(eyre!(
            "package.metadata.ix.inputs.exclude-nested-packages must be a boolean"
        )),
    }
}

fn manifest_needs_nested_inputs(package_root: &Path, excluded: &BTreeSet<PathBuf>) -> Result<bool> {
    if excluded.is_empty() {
        return Ok(false);
    }
    let manifest_path = package_root.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(true);
    }
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(manifest_path)?)?;
    match manifest
        .get("package")
        .and_then(|package| package.get("build"))
    {
        Some(toml::Value::Boolean(false)) => {}
        Some(_) => return Ok(true),
        None if package_root.join("build.rs").exists() => return Ok(true),
        None => {}
    }
    for name in ["lib", "bin", "example", "test", "bench"] {
        let Some(value) = manifest.get(name) else {
            continue;
        };
        let targets = value
            .as_array()
            .map_or_else(|| std::slice::from_ref(value), Vec::as_slice);
        for target in targets {
            if let Some(path) = target.get("path").and_then(toml::Value::as_str) {
                let path = normalize_path(&package_root.join(path));
                if excluded.iter().any(|boundary| path.starts_with(boundary)) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn local_source_root_for_unit(
    unit: &Unit,
    workspace_root: &Path,
    workspace_packages: &BTreeSet<PathBuf>,
    has_build_script: bool,
) -> Result<(ScopedSourceRoot, Vec<String>)> {
    let package_root =
        local_package_root_from_pkg_id(&unit.pkg_id).unwrap_or_else(|| crate_root_for_unit(unit));
    relative_path_string(&package_root, workspace_root).map_err(|_| {
        eyre!(
            "local unit {} {} source root {} is outside workspace root {}",
            unit.package_name(),
            unit.package_version(),
            package_root.display(),
            workspace_root.display()
        )
    })?;

    let package_relative = relative_path_string(&package_root, workspace_root)?;
    // Manifest boundaries make the source identity independent of which
    // nested packages happen to be selected in this unit graph.
    let mut known_packages = BTreeSet::new();
    if nested_package_filter_enabled(&package_root)? {
        known_packages = nested_manifest_roots(&package_root)?;
        known_packages.extend(workspace_packages.iter().cloned());
    }
    let mut excluded: BTreeSet<PathBuf> = known_packages
        .iter()
        .filter(|path| {
            !has_build_script && *path != &package_root && path.starts_with(&package_root)
        })
        .filter(|path| {
            // Nested fixture/module packages in a conventional Rust target
            // directory remain inputs of that target.
            let first = path
                .strip_prefix(&package_root)
                .unwrap()
                .components()
                .next();
            !first.is_some_and(|part| {
                matches!(
                    part.as_os_str().to_str(),
                    Some("src" | "tests" | "examples" | "benches")
                )
            })
        })
        .cloned()
        .collect();
    // Only outermost boundaries matter. A selected grandchild must not
    // change the filter of a parent whose child is already excluded.
    let boundaries = excluded.clone();
    excluded.retain(|path| {
        !path
            .ancestors()
            .skip(1)
            .any(|parent| boundaries.contains(parent))
    });
    if manifest_needs_nested_inputs(&package_root, &excluded)? {
        excluded.clear();
    }
    excluded.extend(test_source_exclusions(
        unit,
        &package_root,
        has_build_script,
    )?);
    let include_relatives =
        source_closure_with_exclusions(&package_root, workspace_root, &mut excluded)?;
    let excluded_relatives: Vec<String> = excluded
        .iter()
        .map(|path| relative_path_string(path, workspace_root))
        .collect::<Result<_>>()?;

    if include_relatives.len() > 1 || include_relatives.first() != Some(&package_relative) {
        return Ok((
            ScopedSourceRoot {
                root: workspace_root.to_path_buf(),
                scope: SourceScope::Closure,
                relative: package_relative,
                include_relatives,
            },
            excluded_relatives,
        ));
    }

    Ok((
        ScopedSourceRoot {
            root: package_root,
            scope: SourceScope::Package,
            relative: package_relative.clone(),
            include_relatives: vec![package_relative],
        },
        excluded_relatives,
    ))
}

fn vendored_source_root_for_unit(unit: &Unit, vendor_root: &Path) -> Result<ScopedSourceRoot> {
    let source = Path::new(&unit.target.src_path);
    let relative = source.strip_prefix(vendor_root).map_err(|_| {
        eyre!(
            "external unit {} {} source path {} is outside vendor root {}",
            unit.package_name(),
            unit.package_version(),
            source.display(),
            vendor_root.display()
        )
    })?;

    let crate_root = match relative.components().next() {
        Some(Component::Normal(component)) => vendor_root.join(component),
        _ => Err(eyre!(
            "external unit {} {} source path {} does not contain a vendored crate directory under {}",
            unit.package_name(),
            unit.package_version(),
            source.display(),
            vendor_root.display()
        ))?,
    };

    let crate_relative = relative_path_string(&crate_root, vendor_root)?;
    let include_relatives = source_closure_relatives(&crate_root, vendor_root)?;

    if include_relatives.len() > 1 || include_relatives.first() != Some(&crate_relative) {
        return Ok(ScopedSourceRoot {
            root: vendor_root.to_path_buf(),
            scope: SourceScope::Closure,
            relative: crate_relative,
            include_relatives,
        });
    }

    Ok(ScopedSourceRoot {
        root: crate_root,
        scope: SourceScope::Package,
        relative: crate_relative.clone(),
        include_relatives: vec![crate_relative],
    })
}

fn relative_path_string(path: &Path, root: &Path) -> Result<String> {
    let relative = path.strip_prefix(root)?;
    Ok(relative.to_string_lossy().into_owned())
}

fn source_name(base: SourceBase, unit: &Unit, source_key: &str, relative: &str) -> String {
    let package_name = unit.package_name();
    let hash = hash::short(&format!(
        "{}\0{}\0{}\0{}\0{}",
        base.label(),
        package_name,
        unit.package_version(),
        source_key,
        relative
    ));
    format!(
        "cargo-unit-source-{}-{}-{hash}",
        store_name_component(package_name.as_ref()),
        store_name_component(unit.package_version())
    )
}

/// Build-time placeholder that stands in for a unit's source store path.
///
/// `file!()` expands to the absolute path of the source file, which here is a
/// store path, so every `panic!`, `unwrap`, `assert` and `#[track_caller]`
/// location bakes one into the compiled artifact as ordinary string data that
/// no `strip` removes. Nix's reference scanner looks for the 32-character hash
/// part of a store path anywhere in an output, so each of those strings pins
/// the whole vendored crate into the binary's runtime closure. Remapping to a
/// hash-free placeholder is what keeps a 23 MB executable from dragging its
/// entire build-time closure behind it (indexable-inc/index#4249).
///
/// Derived from the package name and version rather than the source store name
/// so it stays readable in a backtrace and, more importantly, so it is a pure
/// function of the graph: relocating a source (a rename, a filter change, a
/// different but identical checkout) no longer perturbs the bytes of the rlib
/// that quotes it, which is exactly the early cutoff content addressing is for.
/// Every unit sharing a source entry shares this prefix, because
/// [`source_name`] keys on the same package identity.
fn source_remap_prefix(unit: &Unit) -> String {
    format!(
        "/build/{}-{}",
        store_name_component(unit.package_name().as_ref()),
        store_name_component(unit.package_version())
    )
}

fn store_name_component(value: &str) -> String {
    let component: String = value
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '-' | '.' | '_' => ch,
            _ => '-',
        })
        .collect();

    if component.is_empty() {
        "unknown".to_string()
    } else {
        component
    }
}

fn local_source_key(unit: &Unit) -> String {
    format!("path#{}@{}", unit.package_name(), unit.package_version())
}

fn vendor_source_key(unit: &Unit, cargo_lock_sources: &CargoLockSources) -> Result<String> {
    let source = cargo_lock_sources.source_for_unit(unit)?;
    Ok(format!(
        "{}#{}@{}",
        source,
        unit.package_name(),
        unit.package_version()
    ))
}

fn external_source_from_pkg_id(pkg_id: &str) -> Option<String> {
    if pkg_id.starts_with("registry+")
        || pkg_id.starts_with("git+")
        || pkg_id.starts_with("sparse+")
    {
        let (source, _) = pkg_id.rsplit_once('#')?;
        return Some(source.to_string());
    }

    let (_, rest) = pkg_id.split_once(" (")?;
    let source = rest.strip_suffix(')')?;
    if source.starts_with("registry+")
        || source.starts_with("git+")
        || source.starts_with("sparse+")
    {
        Some(source.to_string())
    } else {
        None
    }
}

fn local_package_root_from_pkg_id(pkg_id: &str) -> Option<PathBuf> {
    if let Some(rest) = pkg_id.strip_prefix("path+file://") {
        let (path, _) = rest.split_once('#')?;
        return file_url_path(path);
    }

    let (_, rest) = pkg_id.split_once("(path+file://")?;
    let (path, _) = rest.split_once(')')?;
    file_url_path(path)
}

// `Url::to_file_path` percent-decodes the path from the package id.
fn file_url_path(path: &str) -> Option<PathBuf> {
    Url::parse(&format!("file://{path}"))
        .ok()?
        .to_file_path()
        .ok()
}

fn source_closure_relatives(root: &Path, source_boundary: &Path) -> Result<Vec<String>> {
    source_closure_with_exclusions(root, source_boundary, &mut BTreeSet::new())
}

fn source_closure_with_exclusions(
    root: &Path,
    source_boundary: &Path,
    excluded: &mut BTreeSet<PathBuf>,
) -> Result<Vec<String>> {
    let source_boundary = normalize_path(source_boundary);
    let mut included_roots = BTreeSet::from([normalize_path(root)]);
    let mut queue = VecDeque::from([normalize_path(root)]);

    while let Some(scan_root) = queue.pop_front() {
        collect_source_closure_roots(
            &scan_root,
            &source_boundary,
            &mut included_roots,
            &mut queue,
            excluded,
        )?;
    }

    included_roots
        .iter()
        .map(|path| relative_path_string(path, &source_boundary))
        .collect()
}

fn collect_source_closure_roots(
    root: &Path,
    source_boundary: &Path,
    included_roots: &mut BTreeSet<PathBuf>,
    queue: &mut VecDeque<PathBuf>,
    excluded: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if excluded.contains(root) || !root.exists() || !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if excluded.contains(&path) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let target = fs::read_link(&path)?;
            let target = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or(root).join(target)
            };

            let target = normalize_path(&target);
            if !target.starts_with(source_boundary) {
                return Err(eyre!(
                    "source symlink {} points outside source boundary {} to {}",
                    path.display(),
                    source_boundary.display(),
                    target.display()
                ));
            }

            retain_referenced_packages(&target, excluded, queue);
            if !path_is_covered_by_roots(&target, included_roots) {
                if target.is_dir() {
                    queue.push_back(target.clone());
                }
                included_roots.insert(target);
            }
        } else if file_type.is_dir() {
            collect_source_closure_roots(&path, source_boundary, included_roots, queue, excluded)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            scan_rust_includes_into_closure(
                &path,
                source_boundary,
                included_roots,
                queue,
                excluded,
            )?;
        }
    }

    Ok(())
}

#[derive(Default)]
struct SourceScopeSyntax {
    preserve_nested: bool,
    includes: Vec<ScannedIncludePath>,
}

fn contains_path_token(stream: proc_macro2::TokenStream) -> bool {
    stream.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(name) => name.to_string().trim_start_matches("r#") == "path",
        proc_macro2::TokenTree::Group(group) => contains_path_token(group.stream()),
        _ => false,
    })
}

/// Read macro and attribute token boundaries without interpreting Rust expressions.
/// Comments and literal contents cannot masquerade as a module-path attribute.
fn source_scope_syntax(source: &str) -> Option<SourceScopeSyntax> {
    use proc_macro2::{Delimiter, TokenTree};
    let mut pending = vec![source.parse::<proc_macro2::TokenStream>().ok()?];
    let mut result = SourceScopeSyntax::default();
    while let Some(stream) = pending.pop() {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                pending.push(group.stream());
            }
            if matches!(token, TokenTree::Punct(mark) if mark.as_char() == '#') {
                let next = index
                    + 1
                    + usize::from(
                        matches!(tokens.get(index + 1), Some(TokenTree::Punct(mark)) if mark.as_char() == '!'),
                    );
                match tokens.get(next) {
                    Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket => {
                        result.preserve_nested |= contains_path_token(group.stream());
                    }
                    _ => result.preserve_nested = true,
                }
            }
            let TokenTree::Ident(name) = token else {
                continue;
            };
            if !matches!(
                name.to_string().trim_start_matches("r#"),
                "include" | "include_str" | "include_bytes"
            ) || !matches!(tokens.get(index + 1), Some(TokenTree::Punct(mark)) if mark.as_char() == '!')
            {
                continue;
            }
            let Some(TokenTree::Group(group)) = tokens.get(index + 2) else {
                result.preserve_nested = true;
                continue;
            };
            let args: Vec<_> = group.stream().into_iter().collect();
            let literal = match args.as_slice() {
                [TokenTree::Literal(value)] => Some(value),
                [TokenTree::Literal(value), TokenTree::Punct(comma)] if comma.as_char() == ',' => {
                    Some(value)
                }
                _ => None,
            };
            match literal.and_then(|value| parse_rust_string_literal(&value.to_string())) {
                Some(path) if !path.is_empty() => result.includes.push(ScannedIncludePath {
                    path,
                    require_existing_file: false,
                }),
                _ => result.preserve_nested = true,
            }
        }
    }
    Some(result)
}

/// Extend `included_roots` / `queue` with any directories reached through
/// `include!`, `include_bytes!`, or `include_str!` macros in `file` whose
/// argument is a plain or `r"…"` string literal, and through the wrapper
/// macros described on [`extract_include_macro_paths`]. Paths are resolved
/// relative to the source file's directory and normalized; matches outside
/// `source_boundary` are dropped on the assumption they come from build
/// scripts via `OUT_DIR` or similar (rustc will surface a clear error if
/// the file is genuinely missing). Non-literal arguments such as
/// `concat!(env!("OUT_DIR"), "/x")` cannot be resolved statically and are
/// skipped on purpose.
fn scan_rust_includes_into_closure(
    file: &Path,
    source_boundary: &Path,
    included_roots: &mut BTreeSet<PathBuf>,
    queue: &mut VecDeque<PathBuf>,
    excluded: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let source = match fs::read_to_string(file) {
        Ok(source) => source,
        Err(_) if excluded.is_empty() => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut candidates = extract_include_macro_paths(&source);
    if !excluded.is_empty() {
        match source_scope_syntax(&source) {
            Some(syntax) => {
                candidates.extend(syntax.includes);
                if syntax.preserve_nested || source.contains("CARGO_MANIFEST_DIR") {
                    queue.extend(std::mem::take(excluded));
                }
            }
            // Tokenization uncertainty cannot justify omitting an input.
            None => queue.extend(std::mem::take(excluded)),
        }
    }

    let file_dir = file.parent().unwrap_or(file);
    for candidate in candidates {
        let resolved = normalize_path(&file_dir.join(&candidate.path));
        if !resolved.starts_with(source_boundary) {
            continue;
        }
        // A wrapper-macro literal is only evidence of a source input once it
        // names a file that is really there; see `ScannedIncludePath`.
        if candidate.require_existing_file && !resolved.is_file() {
            continue;
        }
        let Some(include_root) = include_closure_root(&resolved, source_boundary) else {
            continue;
        };
        // Closure widening uses the parent directory, but only the actual
        // referenced path can require a previously excluded input.
        retain_referenced_packages(&resolved, excluded, queue);
        if !path_is_covered_by_roots(&include_root, included_roots) {
            queue.push_back(include_root.clone());
            included_roots.insert(include_root);
        }
    }
    Ok(())
}

/// A read into a nested package preserves the original package tree. Keeping
/// all nested inputs in this case also avoids changing its identity when a
/// different target selection exposes additional descendant packages.
fn retain_referenced_packages(
    target: &Path,
    excluded: &mut BTreeSet<PathBuf>,
    queue: &mut VecDeque<PathBuf>,
) {
    if excluded
        .iter()
        .any(|path| target.starts_with(path) || path.starts_with(target))
    {
        queue.extend(std::mem::take(excluded));
    }
}

fn include_closure_root(resolved: &Path, source_boundary: &Path) -> Option<PathBuf> {
    let include_root = if resolved.is_dir() {
        resolved
    } else {
        resolved.parent().unwrap_or(resolved)
    };

    if include_root == source_boundary {
        // A boundary file such as vendorDir/README.md must stay a file;
        // promoting it to vendorDir walks every vendored crate symlink.
        if resolved == source_boundary {
            return None;
        }
        return Some(resolved.to_path_buf());
    }

    Some(include_root.to_path_buf())
}

/// A path lifted out of a macro invocation by
/// [`extract_include_macro_paths`], with how much the caller may trust it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ScannedIncludePath {
    /// The literal as written, to be resolved against the scanned file's
    /// directory.
    path: String,
    /// Whether the path must name a file that is really on disk before it
    /// widens the source closure. An `include*!` argument does not: it is a
    /// path by construction, and a missing one is rustc's error to report.
    /// A path lifted out of any other macro does, because its existence is
    /// the only evidence that the literal is a path at all.
    require_existing_file: bool,
}

/// Lift path arguments out of macro calls. The scan is intentionally
/// textual: it does not parse Rust, so false positives inside comments and
/// string literals are possible but harmless (an extra non-existent
/// directory in the closure is filtered out at the Nix layer).
///
/// For `include!`, `include_bytes!`, and `include_str!`, plain `"…"` and
/// `r"…"` literals are resolved as files. `concat!` arguments with a
/// leading literal directory are resolved to that directory; the computed
/// filename stays dynamic, but the source closure still contains the data
/// tree. Other computed arguments such as `env!`, identifiers, and raw
/// strings with `#` delimiters are skipped.
///
/// Any OTHER macro contributes its leading string literal too, but only
/// when that literal names a file (so not a `concat!` directory prefix,
/// which this scan meets again as a nested call) that climbs out of the
/// package with `../`, and then only if that file exists
/// (`require_existing_file`). This is what
/// carries a WRAPPER macro - a `macro_rules!` that forwards a literal path
/// into `include_str!`, so the include marker never appears at the call
/// site. Marker-anchored scanning cannot see those, and the failure is
/// silent at render time and loud only inside the build sandbox, as a
/// missing file rustc blames on the crate. The two bounds are what keep the
/// widening honest: a non-escaping literal is already inside the package
/// root, so following it would gain nothing, and requiring the file to
/// exist is what stops a plain string such as `"../"` or `format!("../{}")`
/// from dragging a parent directory into a unit's source input.
fn extract_include_macro_paths(source: &str) -> Vec<ScannedIncludePath> {
    const INCLUDE_MACROS: &[&str] = &["include", "include_bytes", "include_str"];
    let mut paths = Vec::new();
    let mut cursor = 0;
    while let Some(found) = source[cursor..].find('!') {
        let bang = cursor + found;
        cursor = bang + 1;
        // The macro's name, which also rules out `!=` and unary `!`.
        let Some(name) = macro_name_ending_at(source, bang) else {
            continue;
        };
        let Some(tail) = source[cursor..].trim_start().strip_prefix('(') else {
            continue;
        };
        let tail = tail.trim_start();
        let is_include_macro = INCLUDE_MACROS.contains(&name);
        if let Some(literal) = parse_rust_string_literal(tail) {
            if literal.is_empty() {
                continue;
            }
            if is_include_macro {
                paths.push(ScannedIncludePath {
                    path: literal,
                    require_existing_file: false,
                });
            } else if literal.starts_with("../") && !literal.ends_with('/') {
                paths.push(ScannedIncludePath {
                    path: literal,
                    require_existing_file: true,
                });
            }
            continue;
        }

        if !is_include_macro {
            continue;
        }
        let Some(concat_tail) = tail.strip_prefix("concat!") else {
            continue;
        };
        let Some(concat_body) = concat_tail.trim_start().strip_prefix('(') else {
            continue;
        };
        if let Some(literal) = parse_rust_string_literal(concat_body.trim_start())
            && let Some(directory) = literal.strip_suffix('/')
            && !directory.is_empty()
        {
            paths.push(ScannedIncludePath {
                path: directory.to_string(),
                require_existing_file: false,
            });
        }
    }
    paths
}

/// The macro name immediately before the `!` at `bang`, or `None` when no
/// identifier is there (unary `!`, `!=`). A qualified call such as
/// `crate::cite!` yields the last segment, which is the name the match
/// against the include macros needs.
fn macro_name_ending_at(source: &str, bang: usize) -> Option<&str> {
    let head = &source[..bang];
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .last()
        .map(|(index, _)| index)?;
    Some(&head[start..])
}

fn parse_rust_string_literal(source: &str) -> Option<String> {
    let (source, is_raw) = match source.strip_prefix('r') {
        Some(after_r) if after_r.starts_with('"') => (after_r, true),
        _ => (source, false),
    };
    let body = source.strip_prefix('"')?;
    let mut chars = body.chars();
    let mut literal = String::new();
    while let Some(c) = chars.next() {
        if c == '"' {
            return Some(literal);
        }
        if c == '\\' && !is_raw {
            match chars.next() {
                Some('n') => literal.push('\n'),
                Some('t') => literal.push('\t'),
                Some('r') => literal.push('\r'),
                Some('"') => literal.push('"'),
                Some('\'') => literal.push('\''),
                Some('\\') => literal.push('\\'),
                // Unsupported Rust escapes cannot justify omitting a source input.
                _ => return None,
            }
        } else {
            literal.push(c);
        }
    }

    None
}

fn path_is_covered_by_roots(path: &Path, roots: &BTreeSet<PathBuf>) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    normalized
}

fn source_path_expr(source: &SourceEntry, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(&source.root).map_err(|_| {
        eyre!(
            "unit source path {} is outside scoped source root {}",
            path.display(),
            source.root.display()
        )
    })?;
    let relative = relative.to_string_lossy();
    if relative.is_empty() {
        Ok("$src".to_string())
    } else {
        Ok(format!("$src/{relative}"))
    }
}

fn ensure_source_contains_unit(source: &SourceEntry, unit: &Unit) -> Result<()> {
    let path = Path::new(&unit.target.src_path);
    source_path_expr(source, path).map(|_| ())
}

fn render_roots(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    render_unit_refs(&graph.roots, prepared)
}

fn render_unit_refs(roots: &[usize], prepared: &PreparedGraph) -> String {
    roots
        .iter()
        .map(|index| format!("units.{}", nix_attr(&prepared.names[*index])))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_root_entries_for(
    roots: &[usize],
    units: &[Unit],
    prepared: &PreparedGraph,
    include: impl Fn(&Unit) -> bool,
) -> String {
    let mut entries = String::new();
    let mut seen = BTreeSet::new();
    for index in roots {
        let unit = &units[*index];
        if !include(unit) || !seen.insert(unit.target.name.clone()) {
            continue;
        }
        let _ = writeln!(
            entries,
            "    {} = units.{};",
            nix_attr(&unit.target.name),
            nix_attr(&prepared.names[*index])
        );
    }
    entries
}

fn render_root_entries(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    include: impl Fn(&Unit) -> bool,
) -> String {
    render_root_entries_for(&graph.roots, &graph.units, prepared, include)
}

fn render_checked_roots(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    graph
        .roots
        .iter()
        .map(|index| {
            format!(
                "withPolicyChecks units.{}",
                nix_attr(&prepared.names[*index])
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_target_sets(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    let root_sets = if graph.root_sets.is_empty() {
        vec![graph.roots.clone()]
    } else {
        graph.root_sets.clone()
    };
    let test_keys = compute_test_keys(graph, prepared);
    let benchmark_keys = compute_benchmark_keys(graph, prepared);
    let doctest_keys = compute_doctest_keys(graph, prepared);

    root_sets
        .iter()
        .map(|roots| {
            format!(
                "    {{\n      roots = [ {} ];\n      binaries = {{\n{}      }};\n      libraries = {{\n{}      }};\n      benchmarks = {{\n{}      }};\n      tests = {{\n{}      }};\n      doctests = {{\n{}      }};\n    }}",
                render_unit_refs(roots, prepared),
                render_root_entries_for(roots, &graph.units, prepared, Unit::is_bin),
                render_root_entries_for(roots, &graph.units, prepared, Unit::is_library),
                render_benchmark_entries_for(roots, &graph.units, prepared, &benchmark_keys),
                render_test_entries_for(roots, &graph.units, prepared, &test_keys),
                render_doctest_entries_for(roots, &graph.units, &doctest_keys),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Globally stable key per root runnable unit. Picks `target.name` when no
/// other matching root unit shares it, falling back to the unit-specific name.
fn compute_root_keys(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    include: impl Fn(&Unit) -> bool,
) -> BTreeMap<usize, String> {
    let mut all_roots: BTreeSet<usize> = graph.roots.iter().copied().collect();
    for set in &graph.root_sets {
        all_roots.extend(set.iter().copied());
    }
    let roots: Vec<usize> = all_roots
        .into_iter()
        .filter(|index| include(&graph.units[*index]))
        .collect();

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for index in &roots {
        *counts
            .entry(graph.units[*index].target.name.as_str())
            .or_insert(0) += 1;
    }

    let mut keys = BTreeMap::new();
    for index in roots {
        let unit = &graph.units[index];
        let key = if counts[unit.target.name.as_str()] == 1 {
            unit.target.name.clone()
        } else {
            prepared.names[index].clone()
        };
        keys.insert(index, key);
    }
    keys
}

fn compute_test_keys(graph: &UnitGraph, prepared: &PreparedGraph) -> BTreeMap<usize, String> {
    compute_root_keys(graph, prepared, Unit::is_test)
}

fn compute_benchmark_keys(graph: &UnitGraph, prepared: &PreparedGraph) -> BTreeMap<usize, String> {
    compute_root_keys(graph, prepared, Unit::is_benchmark)
}

fn test_binary_expr(unit: &Unit, prepared: &PreparedGraph, index: usize) -> String {
    let unit_ref = format!("${{units.{}}}", nix_attr(&prepared.names[index]));
    format!("{unit_ref}/bin/{}", unit.target.name)
}

fn append_doctest_case_filter(script: &mut String) {
    // rustdoc splits each --test-args value on whitespace before it reaches
    // libtest. TEST_NAME contains spaces, so the Nix template derives a
    // whitespace-free substring unique across this target's manifest. The
    // one-test assertion remains the fail-closed backstop for format changes.
    script.push_str(
        "rustdoc_args+=( --test-args \"$DOCTEST_FILTER --include-ignored --nocapture\" )\n",
    );
}

// The rustdoc/doctest mirror of the dual-extern rule in the compile phase:
// under `-Zembed-metadata=no` the crate's rlib is a metadata stub, so the
// sibling .rmeta is passed as a second `--extern` for the same name (cargo's
// -Zno-embed-metadata scheme). See the compile-phase comment for why `-L`
// search cannot substitute for this.
fn append_rustdoc_extern(
    script: &mut String,
    options: &RenderOptions,
    extern_name: &str,
    unit_ref: &str,
) -> Result<()> {
    if options.embed_metadata {
        writeln!(
            script,
            "rustdoc_args+=( --extern \"{extern_name}=$(cat {unit_ref}/nix-support/extern-path)\" )",
        )?;
    } else {
        writeln!(
            script,
            "extern_path=\"$(cat {unit_ref}/nix-support/extern-path)\"",
        )?;
        writeln!(
            script,
            "rustdoc_args+=( --extern \"{extern_name}=$extern_path\" )",
        )?;
        writeln!(
            script,
            "if [ \"''${{extern_path%.rlib}}\" != \"$extern_path\" ] && [ -f \"''${{extern_path%.rlib}}.rmeta\" ]; then\n  rustdoc_args+=( --extern \"{extern_name}=''${{extern_path%.rlib}}.rmeta\" )\nfi",
        )?;
    }
    Ok(())
}

fn render_doctest_command(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    options: &RenderOptions,
    index: usize,
    mode: DoctestCommandMode,
) -> Result<String> {
    let unit = &graph.units[index];
    let source = prepared.source_entry(index)?;
    let package_root = source_path_expr(source, &crate_root_for_unit(unit))?;
    let source_path = source_path_expr(source, Path::new(&unit.target.src_path))?;
    let unit_ref = format!("${{units.{}}}", nix_attr(&prepared.names[index]));
    let mut script = String::new();

    script.push_str("set -euo pipefail\n");
    writeln!(script, "export src=\"${{{}}}\"", prepared.source_ref(index))?;
    writeln!(
        script,
        "export CARGO_MANIFEST_DIR={}",
        shell::double_quote(&package_root)
    )?;
    script.push_str("cd \"$CARGO_MANIFEST_DIR\"\n");
    script.push_str(&cargo_package_exports(unit)?);
    script.push_str("build_script_rustdoc_args=()\n");
    script.push_str("doctest_build_args=()\n");
    script.push_str("doctest_runtime_library_paths=()\n");
    script.push_str("rustdoc_args=( --test )\n");
    match mode {
        DoctestCommandMode::List => {
            script.push_str("rustdoc_args+=( -Z unstable-options --output-format doctest )\n");
        }
        DoctestCommandMode::RunAll => {}
        DoctestCommandMode::RunCase => append_doctest_case_filter(&mut script),
    }
    push_rustdoc_arg(&mut script, "--crate-name");
    push_rustdoc_arg(&mut script, &unit.target.name.replace('-', "_"));
    push_rustdoc_arg(&mut script, "--edition");
    push_rustdoc_arg(&mut script, &unit.target.edition);
    for rustflag in &unit.lint_rustflags {
        push_rustdoc_arg(&mut script, rustflag);
    }
    for arg in &unit.check_cfg_args {
        push_rustdoc_arg(&mut script, arg);
    }
    for feature in &unit.features {
        push_rustdoc_arg(&mut script, "--cfg");
        push_rustdoc_arg(&mut script, &format!("feature=\"{feature}\""));
    }
    if let Some(platform) = &unit.platform {
        push_rustdoc_arg(&mut script, "--target");
        push_rustdoc_arg(&mut script, platform);
    }
    append_doctest_link_args(&mut script, unit, mode);
    append_doctest_builder_args(&mut script, graph, prepared, index, mode);
    for dep_index in &prepared.transitive_unit_deps[index] {
        let dep = &graph.units[*dep_index];
        if dep.is_bin() {
            continue;
        }
        writeln!(
            script,
            "rustdoc_args+=( -L \"dependency=${{units.{}}}/lib\" )",
            nix_attr(&prepared.names[*dep_index])
        )?;
    }
    writeln!(script, "rustdoc_args+=( -L \"dependency={unit_ref}/lib\" )")?;
    append_rustdoc_extern(
        &mut script,
        options,
        &unit.target.name.replace('-', "_"),
        &unit_ref,
    )?;
    for dependency in &unit.dependencies {
        let dep_unit = &graph.units[dependency.index];
        if dep_unit.is_run_custom_build() || dep_unit.is_bin() {
            continue;
        }
        append_rustdoc_extern(
            &mut script,
            options,
            &dependency.extern_crate_name,
            &format!("${{units.{}}}", nix_attr(&prepared.names[dependency.index])),
        )?;
    }
    writeln!(
        script,
        "rustdoc_args+=( {} )",
        shell::double_quote(&source_path)
    )?;
    script.push_str("set -x\n");
    match mode {
        DoctestCommandMode::RunCase => {
            script.push_str("doctest_log=$(mktemp)\n");
            script.push_str("rustdoc \"''${rustdoc_args[@]}\" 2>&1 | tee \"$doctest_log\"\n");
            script.push_str("if ! grep -q '^running 1 test$' \"$doctest_log\"; then\n");
            script.push_str(
                "  echo \"rustdoc filter did not run exactly one doctest: $TEST_NAME\" >&2\n",
            );
            script.push_str("  exit 1\n");
            script.push_str("fi\n");
        }
        DoctestCommandMode::List | DoctestCommandMode::RunAll => {
            script.push_str("rustdoc \"''${rustdoc_args[@]}\"\n");
        }
    }
    // Scope xtrace to the rustdoc invocation so the stdenv fixupPhase that runs
    // afterward does not stream its own trace into the log. See the rustc branch.
    script.push_str("set +x\n");
    Ok(script)
}

#[derive(Clone, Copy)]
enum DoctestCommandMode {
    List,
    RunAll,
    RunCase,
}

/// The run modes LINK each doctest binary, exactly like a test unit's final
/// link, so they carry the same explicit linker surface: the per-platform
/// link args (mold, native search paths, rpaths) via the template's
/// `renderDoctestLinkArgs`, and the cargo target-linker environment override
/// when the unit has an explicit `--target`. List mode never links and stays
/// argv-identical, so the shared doctest manifest does not re-key when only
/// link policy moves. Explicit rather than toolchain-conditional: the
/// doctest link used to succeed only because the upstream rust-overlay
/// aggregate satisfied it implicitly, and that implicit environment
/// dependency was the defect regardless of which toolchain hides it.
fn append_doctest_link_args(script: &mut String, unit: &Unit, mode: DoctestCommandMode) {
    if matches!(mode, DoctestCommandMode::List) {
        return;
    }
    let platform = unit
        .platform
        .as_ref()
        .map_or_else(|| "null".to_string(), |platform| nix_attr(platform));
    let _ = writeln!(script, "${{renderDoctestLinkArgs {platform}}}");
    if let Some(platform) = &unit.platform {
        let env_name = cargo_target_linker_env_name(platform);
        let _ = writeln!(script, "if [ \"''${{{env_name}+x}}\" = x ]; then");
        let _ = writeln!(
            script,
            "  rustdoc_args+=( -C \"linker=''${{{env_name}}}\" )"
        );
        script.push_str("fi\n");
    }
}

fn push_rustdoc_arg(script: &mut String, value: &str) {
    let _ = writeln!(script, "rustdoc_args+=( {} )", shell::quote(value));
}

fn append_doctest_builder_args(
    script: &mut String,
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    index: usize,
    mode: DoctestCommandMode,
) {
    let unit = &graph.units[index];
    for rustflag in &unit.profile.rustflags {
        push_doctest_build_arg(script, rustflag);
    }
    if let Some(run_index) = unit_build_script_run(graph, index) {
        let run_ref = format!("${{units.{}}}", nix_attr(&prepared.names[run_index]));
        append_doctest_build_script_flag_reader(script, &run_ref);
    }

    script.push_str("rustdoc_args+=( \"''${build_script_rustdoc_args[@]}\" )\n");
    if !matches!(mode, DoctestCommandMode::List) {
        script.push_str("if [ \"''${#doctest_build_args[@]}\" -gt 0 ]; then\n");
        script.push_str("  rustdoc_args+=( -Z unstable-options )\n");
        script.push_str("fi\n");
    }
    script.push_str("for doctest_build_arg in \"''${doctest_build_args[@]}\"; do\n");
    script.push_str("  rustdoc_args+=( --doctest-build-arg \"$doctest_build_arg\" )\n");
    script.push_str("done\n");
    script.push_str("if [ \"''${#doctest_runtime_library_paths[@]}\" -gt 0 ]; then\n");
    script.push_str(
        "  doctest_runtime_library_path=$(IFS=:; printf '%s' \"''${doctest_runtime_library_paths[*]}\")\n",
    );
    script.push_str("  doctest_runtime_library_path_host=$(rustc -vV | sed -n 's/^host: //p')\n");
    script.push_str("  case \"$doctest_runtime_library_path_host\" in\n");
    script.push_str(
        "    *apple-darwin*)\n      doctest_runtime_library_path_var=DYLD_FALLBACK_LIBRARY_PATH\n      doctest_runtime_library_path_default=\"$HOME/lib:/usr/local/lib:/usr/lib\"\n      ;;\n",
    );
    script.push_str(
        "    *)\n      doctest_runtime_library_path_var=LD_LIBRARY_PATH\n      doctest_runtime_library_path_default=\n      ;;\n",
    );
    script.push_str("  esac\n");
    script.push_str(
        "  doctest_runtime_library_path_current=\"''${!doctest_runtime_library_path_var-}\"\n",
    );
    script.push_str("  if [ -n \"$doctest_runtime_library_path_current\" ]; then\n");
    script.push_str(
        "    export \"$doctest_runtime_library_path_var=$doctest_runtime_library_path:$doctest_runtime_library_path_current\"\n",
    );
    script.push_str("  else\n");
    script.push_str("    if [ -n \"$doctest_runtime_library_path_default\" ]; then\n");
    script.push_str(
        "      export \"$doctest_runtime_library_path_var=$doctest_runtime_library_path:$doctest_runtime_library_path_default\"\n",
    );
    script.push_str("    else\n");
    script.push_str(
        "      export \"$doctest_runtime_library_path_var=$doctest_runtime_library_path\"\n",
    );
    script.push_str("    fi\n");
    script.push_str("  fi\n");
    script.push_str("fi\n");
}

fn push_doctest_build_arg(script: &mut String, value: &str) {
    let _ = writeln!(script, "doctest_build_args+=( {} )", shell::quote(value));
}

fn append_doctest_build_script_flag_reader(script: &mut String, run_ref: &str) {
    let quoted_run_ref = format!("\"{run_ref}\"");

    script.push('\n');
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/rustc-cfg ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && build_script_rustdoc_args+=( '--cfg' \"$line\" )\n  done < {quoted_run_ref}/rustc-cfg\nfi",
    );
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/rustc-link-search ]; then\n  while IFS= read -r line; do\n    if [ -n \"$line\" ]; then\n      build_script_rustdoc_args+=( '-L' \"$line\" )\n      link_search_path=\"$line\"\n      case \"$link_search_path\" in\n        *=*) link_search_path=\"''${{link_search_path#*=}}\" ;;\n      esac\n      case \"$link_search_path\" in\n        {quoted_run_ref}/out-dir|{quoted_run_ref}/out-dir/*) doctest_runtime_library_paths+=( \"$link_search_path\" ) ;;\n      esac\n    fi\n  done < {quoted_run_ref}/rustc-link-search\nfi",
    );
    append_doctest_link_arg_reader(script, &quoted_run_ref, "rustc-link-arg");
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/rustc-env ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && export \"$line\"\n  done < {quoted_run_ref}/rustc-env\nfi",
    );
    let _ = writeln!(script, "export OUT_DIR={quoted_run_ref}/out-dir\n");
}

fn append_doctest_link_arg_reader(script: &mut String, quoted_run_ref: &str, file: &str) {
    let _ = writeln!(
        script,
        "if [ -f {quoted_run_ref}/{file} ]; then\n  while IFS= read -r line; do\n    [ -n \"$line\" ] && doctest_build_args+=( -C \"link-arg=$line\" )\n  done < {quoted_run_ref}/{file}\nfi",
    );
}

fn render_benchmark_entries_for(
    roots: &[usize],
    units: &[Unit],
    prepared: &PreparedGraph,
    keys: &BTreeMap<usize, String>,
) -> String {
    let mut entries = String::new();
    let mut seen = BTreeSet::new();
    for index in roots {
        let unit = &units[*index];
        if !unit.is_benchmark() {
            continue;
        }
        let key = keys
            .get(index)
            .expect("compute_benchmark_keys covers every root benchmark unit")
            .clone();
        if !seen.insert(key.clone()) {
            continue;
        }
        let _ = writeln!(
            entries,
            "    {} = units.{};",
            nix_attr(&key),
            nix_attr(&prepared.names[*index])
        );
    }

    entries
}

fn render_benchmark_entries(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    let keys = compute_benchmark_keys(graph, prepared);
    render_benchmark_entries_for(&graph.roots, &graph.units, prepared, &keys)
}

fn render_test_entries_for(
    roots: &[usize],
    units: &[Unit],
    prepared: &PreparedGraph,
    keys: &BTreeMap<usize, String>,
) -> String {
    let mut entries = String::new();
    let mut seen = BTreeSet::new();
    for index in roots {
        let unit = &units[*index];
        if !unit.is_test() {
            continue;
        }
        let key = keys
            .get(index)
            .expect("compute_test_keys covers every root test unit")
            .clone();
        if !seen.insert(key.clone()) {
            continue;
        }
        let binary = test_binary_expr(unit, prepared, *index);
        let _ = writeln!(
            entries,
            "    {} = mkTestEntry {{ name = {}; binary = \"{binary}\"; packageName = {}; edition = {}; }};",
            nix_attr(&key),
            nix_attr(&key),
            nix_attr(&unit.package_name()),
            nix_attr(&unit.target.edition),
        );
    }

    entries
}

fn render_test_entries(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    let keys = compute_test_keys(graph, prepared);
    render_test_entries_for(&graph.roots, &graph.units, prepared, &keys)
}

fn compute_doctest_keys(graph: &UnitGraph, prepared: &PreparedGraph) -> BTreeMap<usize, String> {
    compute_root_keys(graph, prepared, Unit::has_doctests)
}

fn render_doctest_entries_for(
    roots: &[usize],
    units: &[Unit],
    keys: &BTreeMap<usize, String>,
) -> String {
    let mut entries = String::new();
    let mut seen = BTreeSet::new();
    for index in roots {
        let unit = &units[*index];
        if !unit.has_doctests() {
            continue;
        }
        let key = keys
            .get(index)
            .expect("compute_doctest_keys covers every root doctest unit")
            .clone();
        if !seen.insert(key.clone()) {
            continue;
        }
        let _ = writeln!(
            entries,
            "    {} = mkDoctestEntry (builtins.head (builtins.filter (target: target.name == {}) doctestTargets));",
            nix_attr(&key),
            nix_attr(&key),
        );
    }

    entries
}

fn render_doctest_entries(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    let keys = compute_doctest_keys(graph, prepared);
    render_doctest_entries_for(
        &keys.keys().copied().collect::<Vec<_>>(),
        &graph.units,
        &keys,
    )
}
/// Emit one indented record per unique target, ordered by key. Targets are
/// deduplicated upstream into `by_key`, so this only joins them for the
/// template.
fn join_target_entries(by_key: BTreeMap<String, String>) -> String {
    let mut entries = String::new();
    for target in by_key.into_values() {
        let _ = writeln!(entries, "    {target}");
    }
    entries
}

/// The cargo target kind a runnable binary tests, normalized to the kind
/// vocabulary cargo-nextest keys binary ids by: every library kind collapses
/// to `lib` (a unit-test binary of an rlib/dylib target is still the
/// package's lib unittest binary), proc-macro stays distinct, and
/// test/bench/example/bin pass through. An unrecognized kind passes through
/// verbatim so a new cargo target kind reads as itself downstream instead of
/// being silently misfiled as a library.
fn runnable_target_kind(unit: &Unit) -> &str {
    if unit.target.has_kind("test") {
        "test"
    } else if unit.target.has_kind("bench") {
        "bench"
    } else if unit.target.has_kind("example") {
        "example"
    } else if unit.target.has_kind("bin") {
        "bin"
    } else if unit.is_proc_macro() {
        "proc-macro"
    } else if unit.target.has_library_kind() {
        "lib"
    } else {
        unit.target.kind.first().map_or("lib", String::as_str)
    }
}

/// One `{ name; binary; ... }` per unique runnable binary target across every
/// root set. Tests and benchmarks share the same record shape; the only
/// difference is which root units `keys` selects. The template feeds these into
/// a single manifest derivation so enumeration is one IFD instead of one per
/// binary.
fn render_runnable_target_entries(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    keys: &BTreeMap<usize, String>,
    with_test_names: bool,
) -> String {
    let mut by_key: BTreeMap<String, String> = BTreeMap::new();
    for (&index, key) in keys {
        if by_key.contains_key(key) {
            continue;
        }
        let unit = &graph.units[index];
        let source = prepared
            .source_entry(index)
            .expect("prepared graph has source entries for every runnable target");
        // `names` points at the target's `-Zdump-test-names` JSON, rendered
        // only for test targets that use the libtest harness: a
        // `harness = false` target has no #[test] collection to dump, so the
        // manifest's dump mode falls back to executing that one binary,
        // exactly like the "binary" mode. Benchmarks never carry it (the
        // benchmark plan runs binaries by design). The attribute is lazy;
        // graphs that never select the dump mode never build the unit.
        let names = if with_test_names && unit.uses_test_harness() {
            format!(
                " names = \"${{testNameUnits.{}}}/test-names.json\";",
                nix_attr(key)
            )
        } else {
            String::new()
        };
        by_key.insert(
            key.clone(),
            format!(
                "{{ name = {}; targetName = {}; binary = \"{}\"; packageName = {}; packageVersion = {}; packageRoot = {}; sourceStoreName = {}; kind = {}; edition = {};{names} }}",
                nix_attr(key),
                // `name` is the workspace-global attr key (unit-specific when
                // target names collide across packages); `targetName` is the
                // raw cargo target name, which nextest binary ids are built
                // from (they are already package-scoped).
                nix_attr(&unit.target.name),
                test_binary_expr(unit, prepared, index),
                nix_attr(&unit.package_name()),
                nix_attr(unit.package_version()),
                nix_attr(source.package_root()),
                nix_attr(&source.name),
                nix_attr(runnable_target_kind(unit)),
                nix_attr(&unit.target.edition),
            ),
        );
    }
    join_target_entries(by_key)
}

fn render_test_target_entries(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    render_runnable_target_entries(graph, prepared, &compute_test_keys(graph, prepared), true)
}

/// One `-Zdump-test-names` unit per harness test target, keyed by the same
/// target key `testTargets` uses. Each is the test unit's exact compile
/// (same source, deps, features, env, per-package build env — env matters,
/// `env!()` reads happen during expansion) with the dump flag appended and no
/// codegen, link, or binary install: `$out/test-names.json` is the product.
/// Rendered unconditionally (the text is cheap and the attrset lazy); only
/// `testDiscovery = "dump-test-names"` ever builds one.
fn render_test_name_unit_entries(
    graph: &UnitGraph,
    options: &RenderOptions,
    prepared: &PreparedGraph,
) -> Result<String> {
    let keys = compute_test_keys(graph, prepared);
    let mut by_key: BTreeMap<String, String> = BTreeMap::new();
    for (&index, key) in &keys {
        if by_key.contains_key(key) {
            continue;
        }
        let unit = &graph.units[index];
        if !unit.uses_test_harness() {
            continue;
        }
        let rendered = render_unit_derivation(
            graph,
            options,
            prepared,
            index,
            UnitDerivation {
                pname: format!("{}-test-names", unit.target.name),
                native_build_inputs: "[ rustToolchain ] ++ extraNativeBuildInputs",
                driver: Driver::TestNames,
                install_phase: "mkdir -p $out\ncp build/test-names.json \"$out/test-names.json\"\n"
                    .to_string(),
                package_name: Some(unit.package_name().into_owned()),
            },
        )?;
        by_key.insert(
            key.clone(),
            format!("{} = mkUnit {rendered};", nix_attr(key)),
        );
    }
    Ok(join_target_entries(by_key))
}

fn render_doctest_target_entries(
    graph: &UnitGraph,
    prepared: &PreparedGraph,
    options: &RenderOptions,
) -> Result<String> {
    let keys = compute_doctest_keys(graph, prepared);
    let mut by_key: BTreeMap<String, String> = BTreeMap::new();
    for (&index, key) in &keys {
        if by_key.contains_key(key) {
            continue;
        }
        let unit = &graph.units[index];
        let source = prepared
            .source_entry(index)
            .expect("prepared graph has source entries for every doctest target");
        by_key.insert(
            key.clone(),
            format!(
                "{{ name = {}; packageName = {}; packageVersion = {}; packageRoot = {}; sourceStoreName = {}; listCommand = {}; allCommand = {}; runCommand = {}; }}",
                nix_attr(key),
                nix_attr(&unit.package_name()),
                nix_attr(unit.package_version()),
                nix_attr(source.package_root()),
                nix_attr(&source.name),
                nix_indented_string(&render_doctest_command(
                    graph,
                    prepared,
                    options,
                    index,
                    DoctestCommandMode::List,
                )?),
                nix_indented_string(&render_doctest_command(
                    graph,
                    prepared,
                    options,
                    index,
                    DoctestCommandMode::RunAll,
                )?),
                nix_indented_string(&render_doctest_command(
                    graph,
                    prepared,
                    options,
                    index,
                    DoctestCommandMode::RunCase,
                )?),
            ),
        );
    }
    Ok(join_target_entries(by_key))
}

/// One benchmark record per unique benchmark target across every root set. The
/// template feeds these into benchmark plans and previous-vs-next Tango
/// comparisons without another Cargo metadata pass.
fn render_benchmark_target_entries(graph: &UnitGraph, prepared: &PreparedGraph) -> String {
    render_runnable_target_entries(
        graph,
        prepared,
        &compute_benchmark_keys(graph, prepared),
        false,
    )
}

/// The kernel's per-string ceiling on anything passed through `execve`:
/// `MAX_ARG_STRLEN` is a fixed `32 * PAGE_SIZE`, i.e. 131072 bytes on every
/// platform this builds on. It is not `ARG_MAX` (the much larger total budget)
/// and it is not tunable -- `ulimit` does not move it.
///
/// A derivation attribute becomes an environment variable, so a unit whose
/// build script exceeds this cannot be built AT ALL: `execve` of the builder
/// returns `E2BIG` and Nix reports "executing '...bash': Argument list too
/// long" with no build log, because the build never started.
const MAX_ARG_STRLEN: usize = 32 * 4096;

/// Where a unit's build script stops travelling as an environment variable and
/// starts travelling as a file. Deliberately below [`MAX_ARG_STRLEN`]: a script
/// that renders just under the ceiling today would otherwise become
/// unbuildable on the next flag added to it, and that failure arrives as an
/// unexplained `E2BIG` in a deploy rather than as a test failure.
const BUILD_PHASE_INLINE_LIMIT: usize = 100 * 1024;

// The invariant the whole mechanism rests on, enforced where it cannot rot: a
// threshold at or above the ceiling would reintroduce the E2BIG deploy failure
// this module exists to prevent.
const _: () = assert!(
    BUILD_PHASE_INLINE_LIMIT < MAX_ARG_STRLEN,
    "the inline limit must leave headroom under the execve per-string ceiling"
);

/// The attribute the oversized build script is parked in. Nix writes any
/// attribute named in `passAsFile` into a file inside the build and exports
/// `<name>Path` instead, so the script itself never reaches `execve`.
const BUILD_SCRIPT_ATTR: &str = "cargoUnitBuildScript";

/// Emit a unit's build script, via a file when it is too large to be an
/// environment variable.
///
/// # Why this is size-gated rather than unconditional
///
/// `buildPhase` feeds every unit's derivation hash. Routing every unit through
/// `passAsFile` would move every hash in the Rust world at once, discarding the
/// entire build cache to fix a problem that a handful of units have. The
/// oversized units change shape (and hash) either way, since they cannot build
/// in their current shape at all.
///
/// The cost of the gate is that a unit changes shape when it crosses the
/// threshold. That is deterministic and asserted by the tests below, and the
/// alternative -- a silent `E2BIG` at deploy time -- is strictly worse.
fn emit_build_phase(attrs: &mut Attrs, script: &str) {
    if script.len() <= BUILD_PHASE_INLINE_LIMIT {
        attrs.multiline("buildPhase", script);
        return;
    }

    attrs.multiline(BUILD_SCRIPT_ATTR, script);
    attrs.expr(
        "passAsFile",
        &nix_string_list(&[BUILD_SCRIPT_ATTR.to_string()]),
    );
    // No `${...}` here: inside a Nix `''` string that would be interpolation,
    // and the exported variable is a plain shell name.
    attrs.multiline(
        "buildPhase",
        &format!("    source \"${BUILD_SCRIPT_ATTR}Path\"\n"),
    );
}

fn nix_attr(value: &str) -> String {
    serde_json::to_string(value).expect("serialize Nix string")
}

fn nix_string_list(values: &[String]) -> String {
    format!(
        "[ {} ]",
        values
            .iter()
            .map(|value| nix_attr(value))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

struct Attrs {
    values: Vec<(String, String)>,
}

impl Attrs {
    const fn new() -> Self {
        Self { values: Vec::new() }
    }

    fn string(&mut self, name: &str, value: &str) {
        self.values
            .push((name.to_string(), format!("{};", nix_attr(value))));
    }

    fn bool(&mut self, name: &str, value: bool) {
        self.values.push((
            name.to_string(),
            format!("{};", if value { "true" } else { "false" }),
        ));
    }

    fn expr(&mut self, name: &str, value: &str) {
        self.values.push((name.to_string(), format!("{value};")));
    }

    fn multiline(&mut self, name: &str, value: &str) {
        self.values
            .push((name.to_string(), format!("''\n{value}  '';")));
    }

    fn render(self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        for (name, value) in self.values {
            let _ = writeln!(out, "      {name} = {value}");
        }
        out.push_str("    }");
        out
    }
}

#[cfg(test)]
mod build_phase_size_tests {
    use super::{
        Attrs, BUILD_PHASE_INLINE_LIMIT, BUILD_SCRIPT_ATTR, MAX_ARG_STRLEN, emit_build_phase,
    };

    // The threshold-below-ceiling invariant is a `const` assertion beside the
    // constants themselves, so it cannot compile in a broken state and needs
    // no runtime test here.

    /// An ordinary unit keeps its script inline, so its derivation hash is
    /// unchanged by this mechanism existing. That is what keeps the cached
    /// world from rebuilding.
    #[test]
    fn a_small_script_stays_an_ordinary_inline_build_phase() {
        let mut attrs = Attrs::new();
        emit_build_phase(&mut attrs, "  cargo build\n");
        let rendered = attrs.render();

        assert!(rendered.contains("buildPhase"));
        assert!(rendered.contains("cargo build"));
        assert!(
            !rendered.contains("passAsFile"),
            "a small script must not be routed through a file: {rendered}"
        );
        assert!(
            !rendered.contains(BUILD_SCRIPT_ATTR),
            "a small script must not be parked in the script attribute"
        );
    }

    /// The real defect, reproduced: `codex_core` rendered a 135899-byte script,
    /// which `execve` refuses with E2BIG before the builder starts. Over the
    /// threshold the script must travel as a FILE, and the surviving
    /// `buildPhase` must be short enough that it can never trip the limit.
    #[test]
    fn an_oversized_script_travels_as_a_file_and_leaves_a_short_build_phase() {
        let script = format!("  {}\n", "x".repeat(135_899));
        assert!(script.len() > BUILD_PHASE_INLINE_LIMIT);

        let mut attrs = Attrs::new();
        emit_build_phase(&mut attrs, &script);

        // Assert on the attribute VALUES, not on the rendered blob: the blob
        // necessarily contains the script once, and what the kernel limits is
        // each individual value.
        let by_name: std::collections::HashMap<&str, &str> = attrs
            .values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        assert!(
            by_name.contains_key(BUILD_SCRIPT_ATTR),
            "the script must be parked in its own attribute"
        );
        assert_eq!(
            by_name.get("passAsFile").copied(),
            Some(format!("[ \"{BUILD_SCRIPT_ATTR}\" ];").as_str()),
            "the parked attribute must be named in passAsFile or Nix will still \
             export it as an environment variable"
        );

        let build_phase = by_name.get("buildPhase").expect("buildPhase is emitted");
        assert!(
            build_phase.contains(&format!("${BUILD_SCRIPT_ATTR}Path")),
            "buildPhase must source the file Nix wrote: {build_phase}"
        );
        assert!(
            build_phase.len() < 200,
            "the surviving buildPhase must be constant-sized, got {} bytes",
            build_phase.len()
        );
        // No `${` anywhere in it: inside a Nix `''` string that is
        // interpolation, and the variable is a plain shell name.
        assert!(
            !build_phase.contains("${"),
            "buildPhase must not contain Nix interpolation: {build_phase}"
        );
    }

    /// The invariant the deploy actually needs: after emission, NO single
    /// attribute value can exceed the kernel's per-string ceiling. This is the
    /// assertion that would have caught `codex_core` before it reached a deploy.
    #[test]
    fn no_emitted_attribute_can_exceed_the_kernel_ceiling() {
        for size in [1_usize, BUILD_PHASE_INLINE_LIMIT, 135_899, 512 * 1024] {
            let mut attrs = Attrs::new();
            emit_build_phase(&mut attrs, &"y".repeat(size));

            for (name, value) in &attrs.values {
                if name == BUILD_SCRIPT_ATTR {
                    // Parked attributes are written to a file by Nix and never
                    // reach execve, which is the entire mechanism.
                    continue;
                }
                assert!(
                    value.len() < MAX_ARG_STRLEN,
                    "attribute {name} is {} bytes at script size {size}, over the \
                     {MAX_ARG_STRLEN}-byte execve ceiling",
                    value.len()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_lock_sources(packages: &[(&str, &str, &str)]) -> CargoLockSources {
        CargoLockSources {
            packages: packages
                .iter()
                .map(|(name, version, source)| CargoLockPackage {
                    name: (*name).to_string(),
                    version: (*version).to_string(),
                    source: (*source).to_string(),
                    dependencies: Vec::new(),
                })
                .collect(),
        }
    }

    fn single_library_graph(pkg_id: &str, name: &str, src_path: &str, edition: &str) -> UnitGraph {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [{
                "pkg_id": pkg_id,
                "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": name,
                    "src_path": src_path,
                    "edition": edition
                },
                "profile": { "name": "release", "opt_level": "3" },
                "mode": "build",
                "dependencies": []
            }],
            "roots": [0]
        }))
        .unwrap()
    }

    fn render_error(graph: &UnitGraph) -> String {
        render_units_nix(
            graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap_err()
        .to_string()
    }

    /// A binary depending on a library, where the library's crate types are the
    /// argument: `["lib"]` for the ordinary case, `["rlib", "dylib"]` for the
    /// one under test.
    fn bin_over_library_graph(library_crate_types: &str) -> UnitGraph {
        serde_json::from_str(&format!(
            r#"{{
              "version": 1,
              "units": [
                {{
                  "pkg_id": "path+file:///workspace#app@0.1.0",
                  "target": {{
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "app",
                    "src_path": "/workspace/app/src/main.rs",
                    "edition": "2024"
                  }},
                  "profile": {{ "name": "release", "opt_level": "3" }},
                  "features": [],
                  "mode": "build",
                  "dependencies": [{{ "index": 1, "extern_crate_name": "engine" }}]
                }},
                {{
                  "pkg_id": "path+file:///workspace#engine@0.1.0",
                  "target": {{
                    "kind": ["lib"],
                    "crate_types": {library_crate_types},
                    "name": "engine",
                    "src_path": "/workspace/engine/src/lib.rs",
                    "edition": "2024"
                  }},
                  "profile": {{ "name": "release", "opt_level": "3" }},
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                }}
              ],
              "roots": [0]
            }}"#
        ))
        .unwrap()
    }

    fn render_for_test(graph: &UnitGraph) -> String {
        render_units_nix(
            graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: Some("rustc-test".to_string()),
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap()
    }

    /// Cargo passes rustc two `--extern` flags for one crate name when the
    /// dependency offers both an rlib and a dylib, because the choice belongs
    /// to rustc. Naming only the rlib links the dependency statically into
    /// every consumer, which for a crate holding process-global state is
    /// several copies of that state in one process.
    #[test]
    fn dylib_dependency_is_offered_to_rustc_in_every_form() {
        let rendered = render_for_test(&bin_over_library_graph(r#"["rlib", "dylib"]"#));

        assert!(
            rendered.contains("nix-support/extern-paths"),
            "a consumer of a dylib unit must read every published artifact:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"rustc_args+=( --extern "engine=$extern_artifact" )"#),
            "the extern-paths loop must emit one --extern per artifact:\n{rendered}"
        );
        assert!(
            rendered.contains(r#"printf '%s\n' "$artifact" >> $out/nix-support/extern-paths"#),
            "a dylib unit must publish extern-paths at install time:\n{rendered}"
        );
        assert!(
            rendered.contains("declares crate-type dylib but produced no shared library"),
            "a dylib unit that produced no shared library must fail loudly:\n{rendered}"
        );
    }

    /// rustc records a dependency dylib by SONAME or `@rpath/<name>`, so a
    /// linked artifact needs the store path of every dylib below it. Only
    /// linking units get this: an rlib consumer links nothing.
    #[test]
    fn linking_units_get_an_rpath_to_each_dylib_dependency() {
        let rendered = render_for_test(&bin_over_library_graph(r#"["rlib", "dylib"]"#));
        let rpaths = rendered
            .matches(r#"rustc_args+=( -C "link-arg=-Wl,-rpath,"#)
            .count();

        assert_eq!(
            rpaths, 1,
            "exactly the one linking unit (the bin) should carry a dylib rpath:\n{rendered}"
        );
    }

    /// The whole change is gated on a `dylib` crate type, so a graph without
    /// one has to render exactly what it rendered before. Otherwise every
    /// existing unit in the repo moves for a feature it does not use.
    #[test]
    fn a_graph_without_a_dylib_renders_the_plain_extern_path() {
        let rendered = render_for_test(&bin_over_library_graph(r#"["lib"]"#));

        assert!(
            rendered.contains(r#"rustc_args+=( --extern "engine=$(cat ${units."engine-0.1.0-"#),
            "a plain rlib dependency must keep the single extern-path form:\n{rendered}"
        );
        assert!(
            !rendered.contains("extern-paths"),
            "nothing about extern-paths may appear without a dylib:\n{rendered}"
        );
        assert!(
            !rendered.contains("-Wl,-rpath,"),
            "no rpath may appear without a dylib:\n{rendered}"
        );
    }

    #[test]
    fn renders_one_derivation_per_build_unit() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: Some("rustc-test".to_string()),
                deny_unused_crate_dependencies: true,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // The generated `units` / `libraries` sets are merged with the
        // additive prebuilt-injection seam (extraUnits / extraLibraries). Both
        // default to `{}`, so an unconfigured graph is behaviorally identical.
        assert!(rendered.contains("units = (rec"));
        assert!(rendered.contains("}) // extraUnits;"));
        assert!(rendered.contains("// extraLibraries;"));
        assert!(rendered.contains("--crate-name"));
        assert!(rendered.contains("sources = {"));
        assert!(rendered.contains("scopedWorkspaceSource \"cargo-unit-source-hello-0.1.0-"));
        assert!(rendered.contains("\"\""));
        assert!(rendered.contains("src = sources."));
        assert!(rendered.contains("\"$src/src/main.rs\""));
        assert!(rendered.contains("default = withPolicyChecks units."));
        assert!(rendered.contains("policyChecks"));
        assert!(rendered.contains("extraRustcArgs"));
        assert!(rendered.contains("packageRustcArgs ? {}"));
        assert!(rendered.contains("tests ="));
        assert!(rendered.contains("--json=unused-externs-silent"));
        assert!(rendered.contains("withPolicyChecks"));
        // Per-unit clippy: the same local unit gets a sibling clippy-driver
        // derivation in `clippyUnits`, grouped per crate by `clippyByPackage`.
        assert!(rendered.contains("clippyUnits = rec"));
        assert!(rendered.contains("pname = \"hello-clippy\""));
        assert!(rendered.contains("env \"''${rustc_env[@]}\" clippy-driver"));
        assert!(rendered.contains("extraClippyLintArgs"));
        assert!(rendered.contains("clippyByPackage ="));
        assert!(rendered.contains("clippyUnitNamesByPackage ="));
    }

    #[test]
    fn installs_split_debuginfo_sidecars_after_fixup() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": {
                    "name": "release",
                    "opt_level": "3",
                    "split_debuginfo": "packed"
                  },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello-cli",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": {
                    "name": "release",
                    "opt_level": "3",
                    "split_debuginfo": "packed"
                  },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0, 1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: Some("rustc-test".to_string()),
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("rustc_args+=( 'split-debuginfo=packed' )"));
        assert_eq!(
            rendered
                .matches("for split_debuginfo_sidecar in build/*.dwo build/*.dwp; do")
                .count(),
            2
        );
        assert!(rendered.contains("cp \"$split_debuginfo_sidecar\" \"$out/bin/\""));
        assert!(rendered.contains("cp \"$split_debuginfo_sidecar\" \"$out/lib/\""));
        assert!(rendered.contains("case \"$build_artifact\" in\n    *.dwo|*.dwp) continue ;;"));
        // The unremapped dep-info .d (raw $src store paths; would pin the
        // source into the closure and defeat CA early cutoff) and the two
        // in-build report files never reach $out/lib.
        assert!(
            rendered
                .contains("*.d|*/rustc-diagnostics.jsonl|*/unused-crate-dependencies) continue ;;")
        );
        assert!(!rendered.contains("cp -R build/* $out/lib/"));
    }

    #[test]
    fn omits_extra_filename_when_output_path_is_explicit() {
        // Bin (and test) units get an explicit `-o build/<name>` path, which by
        // itself fully determines the output filename. Also emitting
        // `-C extra-filename` there is what makes rustc warn "ignoring -C
        // extra-filename flag due to -o flag" on every such build (index#1975).
        // Lib units use `--out-dir` instead, so they still need extra-filename
        // to disambiguate same-named outputs.
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello-cli",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0, 1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: Some("rustc-test".to_string()),
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // The bin unit's rustc (link) invocation carries the explicit `-o`
        // path immediately after the source-path arg, with no
        // `extra-filename` anywhere in between -- that adjacency is exactly
        // what makes rustc emit "ignoring -C extra-filename flag due to -o
        // flag".
        let bin_rustc_invocation = rendered
            .split("rustc_args+=( \"$src/src/main.rs\" )")
            .nth(1)
            .and_then(|rest| rest.split("env \"''${rustc_env[@]}\" rustc").next())
            .expect("bin unit's rustc build phase");
        assert!(bin_rustc_invocation.contains("rustc_args+=( -o 'build/hello-cli' )"));
        assert!(!bin_rustc_invocation.contains("extra-filename"));

        // Sibling clippy-driver units still use `--out-dir` (never `-o`), so
        // they keep extra-filename for both the lib and the bin target; only
        // the bin's *rustc* link step drops it. Three occurrences total: the
        // lib's rustc build, the lib's clippy build, and the bin's clippy
        // build.
        assert_eq!(rendered.matches("'extra-filename=").count(), 3);
    }

    #[test]
    fn install_phase_guards_against_stale_orphan_output() {
        // Every rustc build unit (both the lib and the bin here) must open its
        // installPhase with the orphan-output pre-check before any mkdir/cp, so a
        // killed content-addressed build's leftover output at `$out` fails loud
        // with the recovery recipe instead of a bare `cp: Permission denied`
        // (index#2247).
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello-cli",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0, 1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: true,
                toolchain_id: Some("rustc-test".to_string()),
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // The guard fires on a pre-existing, non-writable `$out` (the killed-build
        // orphan) and names the recovery recipe. One occurrence per build unit
        // (the lib and the bin); it must land before the artifact `cp`.
        assert_eq!(
            rendered
                .matches("refusing to install over a pre-existing, non-writable output path")
                .count(),
            2
        );
        assert!(rendered.contains("nix-store --check-validity"));
        // The guard runs first: it is prepended directly to the installPhase
        // mkdir in both the lib and the bin branches, so it must be immediately
        // adjacent to (and hence before) the mkdir that precedes the artifact cp.
        assert!(rendered.contains("exit 1\nfi\nmkdir -p $out/lib $out/nix-support"));
        assert!(rendered.contains("exit 1\nfi\nmkdir -p $out/bin $out/nix-support"));
    }

    #[test]
    fn deny_panics_scans_local_units_and_skips_externals() {
        // One workspace library plus one external dependency. The panic-freedom
        // check must scan the local unit's compiled artifact and leave the
        // vendored crate alone, mirroring how clippy and unused-dependency
        // policy stay scoped to workspace-owned code.
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": [{ "index": 1, "extern_crate_name": "serde" }]
                },
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "serde",
                    "src_path": "/vendor/serde/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let options = |deny_panics| RenderOptions {
            workspace_root: PathBuf::from("/workspace"),
            vendor_root: Some(PathBuf::from("/vendor")),
            cargo_lock_sources: cargo_lock_sources(&[(
                "serde",
                "1.0.0",
                "registry+https://github.com/rust-lang/crates.io-index",
            )]),
            content_addressed: false,
            toolchain_id: None,
            deny_unused_crate_dependencies: false,
            deny_panics,
            embed_metadata: true,
            rmeta_stability_flags: vec![],
        };

        let rendered = render_units_nix(&graph, &options(true)).unwrap();
        assert!(rendered.contains("panicFreedom ="));
        assert!(rendered.contains("name = \"cargo-unit-panic-freedom\";"));
        assert!(rendered.contains("nix-cargo-unit scan-panics ${pkgs.lib.escapeShellArgs"));
        // The workspace crate set is baked into the scan args.
        assert!(rendered.contains("\"--crate-name\" \"hello\""));
        // Fail closed when the scanner package is not wired through.
        assert!(rendered.contains("assertMsg (cargoUnit != null)"));
        // The local library gets a panic-objects derivation and is scanned; the
        // vendored crate gets neither.
        assert!(rendered.contains("pname = \"hello-panic-objects\""));
        assert!(rendered.contains("(scanUnit \"hello-0.1.0-"));
        assert!(rendered.contains("panicObjectUnits.\"hello-0.1.0-"));
        assert!(!rendered.contains("serde-panic-objects"));
        assert!(!rendered.contains("(scanUnit \"serde-1.0.0-"));

        // Off by default: no flag, no panic-freedom policy entry and no
        // panic-objects derivations.
        let unchecked = render_units_nix(&graph, &options(false)).unwrap();
        assert!(!unchecked.contains("panicFreedom"));
        assert!(!unchecked.contains("panic-objects"));
    }

    #[test]
    fn deny_panics_skips_test_units() {
        // Test bodies legitimately panic, so a test target must not become a
        // panic-objects candidate even though it is workspace-owned.
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["test"],
                    "crate_types": ["bin"],
                    "name": "helloit",
                    "src_path": "/workspace/tests/it.rs",
                    "edition": "2024",
                    "test": true
                  },
                  "profile": { "name": "test", "opt_level": "0" },
                  "features": [],
                  "mode": "test",
                  "dependencies": []
                }
              ],
              "roots": [0, 1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: true,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("pname = \"hello-panic-objects\""));
        assert!(!rendered.contains("helloit-panic-objects"));
        assert!(!rendered.contains("(scanUnit \"helloit-"));
    }

    #[test]
    fn external_only_graph_emits_no_clippy_units() {
        // Vendored crates compile under `--cap-lints warn` and aren't owned by
        // the workspace, so they should never produce per-unit clippy
        // derivations. A graph with only external units must skip clippy
        // entirely and keep `policyChecks` free of a `clippy` entry.
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "serde",
                    "src_path": "/vendor/serde/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[(
                    "serde",
                    "1.0.0",
                    "registry+https://github.com/rust-lang/crates.io-index",
                )]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // `clippyUnits = rec { };` rendered empty proves no per-unit clippy
        // derivations were emitted; the driver invocation only appears inside a
        // rendered clippy unit's build phase, so it's the load-bearing tell.
        assert!(rendered.contains("clippyUnits = rec {\n  };"));
        assert!(!rendered.contains("env \"''${rustc_env[@]}\" clippy-driver"));
        assert!(!rendered.contains("pname = \"serde-clippy\""));
    }

    #[test]
    fn exposes_test_roots_as_runnable_checks() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024",
                    "test": true
                  },
                  "profile": { "name": "test", "opt_level": "0" },
                  "features": [],
                  "mode": "test",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("tests = {"));
        assert!(rendered.contains("\"hello\" = mkTestEntry { name = \"hello\";"));
        assert!(rendered.contains("packageName = \"hello\";"));
        assert!(rendered.contains("/bin/hello\";"));
        assert!(rendered.contains("testRunPrelude ? \"\""));
        assert!(rendered.contains("testArgsByPackage ? {}"));
        assert!(rendered.contains("packageTestInputs ? {}"));
        assert!(rendered.contains("packageTestEnv ? {}"));
        // Per-package build env: the param exists, mkUnit merges it keyed by the
        // unit's `packageName` tag, and strips that tag before `mkDerivation`.
        assert!(rendered.contains("packageBuildEnv ? {}"));
        assert!(rendered.contains("packageBuildEnv.${attrs.packageName or \"\"} or {}"));
        assert!(rendered.contains("builtins.removeAttrs attrs [ \"packageName\" ]"));
        assert!(rendered.contains("mkTestEntry ="));
        assert!(rendered.contains("edition = \"2024\";"));
        assert!(rendered.contains("inherit name binary packageName edition testArgs;"));
        assert!(rendered.contains("RUST_TEST_THREADS"));
        assert!(rendered.contains("mkTestCases ="));
        assert!(rendered.contains("testTargets = ["));
        assert!(rendered.contains("testTargetNamesByPackage ="));
        assert!(rendered.contains("pkgs.lib.groupBy (target: target.packageName) testTargets"));
        assert!(rendered.contains("doctestTargetNamesByPackage ="));
        assert!(rendered.contains("{ name = \"hello\"; targetName = \"hello\"; binary ="));
        assert!(!rendered.contains("units.\\\""));
        assert!(rendered.contains("packageName = \"hello\";"));
        assert!(rendered.contains("packageRoot = \".\";"));
        assert!(rendered.contains("sourceStoreName = \"cargo-unit-source-hello-0.1.0-"));
        // testTargets records carry the raw cargo target name, kind, and
        // edition so the workspace nextest export can synthesize
        // binaries/cargo metadata without re-deriving any of them from unit
        // keys.
        assert!(rendered.contains("; kind = \"lib\"; edition = \"2024\";"));
        assert!(rendered.contains("sourcePackageRoot ="));
        assert!(rendered.contains("test_cwd="));
        assert!(rendered.contains("cd \"$test_cwd\""));
        assert!(rendered.contains("testPlan = mkTestPlan \"cargo-unit-test-plan\";"));
        assert!(rendered.contains("coverageReport = mkCoverageReport {};"));
        assert!(rendered.contains("makeCoverageReport = mkCoverageReport;"));
        assert!(rendered.contains("writableTestCwd ? true"));
        assert!(rendered.contains(".cargo-unit-writable-cwd-ready"));
        assert!(rendered.contains("llvm-profdata"));
        assert!(rendered.contains("llvm-cov"));
        assert!(!rendered.contains("fallbackLlvmCov"));
        assert!(rendered.contains("source-roots.tsv"));
        assert!(rendered.contains("testManifestDrv ="));
        assert!(rendered.contains("cargo-unit-test-manifest"));
        assert!(rendered.contains("failed to list tests for"));
    }

    #[test]
    fn dump_test_names_discovery_renders_parallel_units() {
        // Two root test units: one on the libtest harness, one `harness =
        // false` (target.test = false). Only the harness unit gets a
        // `-Zdump-test-names` twin and a `names` pointer; the harness-less
        // one must fall back to executing its binary even in dump mode.
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024",
                    "test": true
                  },
                  "profile": { "name": "test", "opt_level": "0" },
                  "features": [],
                  "mode": "test",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["test"],
                    "crate_types": ["bin"],
                    "name": "nohar",
                    "src_path": "/workspace/tests/nohar.rs",
                    "edition": "2024",
                    "test": false
                  },
                  "profile": { "name": "test", "opt_level": "0" },
                  "features": [],
                  "mode": "test",
                  "dependencies": []
                }
              ],
              "roots": [0, 1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // The template gate and both manifest builders are present.
        assert!(rendered.contains("testDiscovery ? \"binary\""));
        assert!(rendered.contains("binaryTestManifestDrv ="));
        assert!(rendered.contains("dumpTestNamesManifestDrv ="));
        assert!(rendered.contains("testManifestDrv ="));

        // The harness target gets a dump twin: same compile, dump flag, JSON
        // install, no codegen output, and its target record points at it.
        assert!(rendered.contains("testNameUnits = {"));
        assert!(rendered.contains("\"hello\" = mkUnit {"));
        assert!(rendered.contains("pname = \"hello-test-names\";"));
        assert!(rendered.contains("rustc_args+=( -Zdump-test-names=build/test-names.json )"));
        assert!(rendered.contains("cp build/test-names.json \"$out/test-names.json\""));
        assert!(rendered.contains(" names = \"${testNameUnits.\"hello\"}/test-names.json\"; }"));

        // The harness-less target gets neither.
        assert!(!rendered.contains("\"nohar\" = mkUnit"));
        assert!(!rendered.contains("pname = \"nohar-test-names\";"));
        assert!(!rendered.contains("testNameUnits.\"nohar\""));

        // The jq projection reproduces terse `--list` bytes: name, kind, and
        // the ignored filter.
        assert!(rendered.contains("jq -r '.tests[] | \"\\(.name): \\(.kind)\"'"));
        assert!(rendered.contains("jq -r '.tests[] | select(.ignore) | \"\\(.name): \\(.kind)\"'"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn bin_exe_env_uses_build_bins_for_integration_tests() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("src")).unwrap();
        fs::create_dir_all(workspace.path().join("tests")).unwrap();
        fs::write(
            workspace.path().join("Cargo.toml"),
            r#"[package]
name = "dag-runner"
version = "0.1.0"
"#,
        )
        .unwrap();
        let main_rs = workspace.path().join("src/main.rs");
        let integration_rs = workspace.path().join("tests/integration.rs");
        fs::write(&main_rs, "fn main() {}\n").unwrap();
        fs::write(&integration_rs, "#[test]\nfn runs() {}\n").unwrap();
        let main_rs = main_rs.display().to_string();
        let integration_rs = integration_rs.display().to_string();
        let pkg_id = format!(
            "path+file://{}#dag-runner@0.1.0",
            workspace.path().display()
        );
        let bin_target = serde_json::json!({
          "kind": ["bin"],
          "crate_types": ["bin"],
          "name": "dag-runner",
          "src_path": main_rs,
          "edition": "2024"
        });

        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
          "version": 1,
          "units": [
            {
              "pkg_id": pkg_id,
              "target": bin_target,
              "profile": { "name": "release", "opt_level": "3", "rustflags": ["-C", "target-feature=+sse2"] },
              "features": [],
              "mode": "build",
              "dependencies": []
            },
            {
              "pkg_id": pkg_id,
              "target": bin_target,
              "profile": { "name": "test", "opt_level": "0" },
              "features": [],
              "mode": "test",
              "dependencies": []
            },
            {
              "pkg_id": pkg_id,
              "target": {
                "kind": ["test"],
                "crate_types": ["bin"],
                "name": "integration",
                "src_path": integration_rs,
                "edition": "2024"
              },
              "profile": { "name": "test", "opt_level": "0" },
              "features": [],
              "mode": "test",
              "dependencies": [
                { "index": 0, "extern_crate_name": "dag_runner" }
              ]
            },
            {
              "pkg_id": pkg_id,
              "target": bin_target,
              "profile": { "name": "dev", "opt_level": "0" },
              "features": ["extra"],
              "mode": "build",
              "dependencies": []
            }
          ],
          "roots": [0, 1, 2]
        }))
        .unwrap();

        assert!(same_package_bins(&graph, 1).is_empty());
        assert_eq!(
            same_package_bins(&graph, 2),
            vec![("dag-runner".to_string(), 0)]
        );

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.path().to_path_buf(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // The build unit (rustc), its sibling clippy unit, and the
        // `-Zdump-test-names` discovery twin each set CARGO_BIN_EXE_<name>
        // for integration tests: clippy needs the same compilation env as
        // rustc, and the dump unit is the same compile up to its early stop,
        // so `env!("CARGO_BIN_EXE_...")` must resolve during its expansion
        // too. The count rises with the number of unit kinds that compile
        // the integration test target.
        assert_eq!(rendered.matches("CARGO_BIN_EXE_dag-runner=").count(), 3);
        assert!(rendered.contains("rustc_env+=( 'CARGO_BIN_EXE_dag-runner=${units."));
        assert!(!rendered.contains("export CARGO_BIN_EXE_dag-runner"));
        assert!(rendered.contains("env \"''${rustc_env[@]}\" rustc \"''${rustc_args[@]}\""));
        assert!(
            rendered.contains("env \"''${rustc_env[@]}\" clippy-driver \"''${rustc_args[@]}\"")
        );
    }

    #[test]
    fn exposes_benchmark_roots_as_tango_comparison_inputs() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "bench", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bench"],
                    "crate_types": ["bin"],
                    "name": "greeting",
                    "src_path": "/workspace/benches/greeting.rs",
                    "edition": "2024",
                    "test": false
                  },
                  "profile": { "name": "bench", "opt_level": "3" },
                  "features": [],
                  "mode": "test",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "hello" }
                  ]
                }
              ],
              "roots": [1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("benchmarks = {"));
        assert!(rendered.contains("\"greeting\" = units."));
        assert!(rendered.contains("benchmarkTargets = ["));
        assert!(rendered.contains("{ name = \"greeting\"; targetName = \"greeting\"; binary ="));
        assert!(rendered.contains("; kind = \"bench\"; edition = \"2024\";"));
        assert!(
            rendered.contains("benchmarkPlan = mkBenchmarkPlan \"cargo-unit-benchmark-plan\";")
        );
        assert!(rendered.contains("compareTangoBenchmarks = mkTangoBenchmarkComparison;"));
        assert!(rendered.contains("targetSets = ["));
        assert!(!rendered.contains("\"greeting\" = mkTestEntry"));
        assert!(!rendered.contains("rustc_args+=( '--test' )"));
        assert!(rendered.contains("rustc_args+=( '--cfg' )"));
        assert!(rendered.contains("rustc_args+=( 'test' )"));
    }

    #[test]
    fn exposes_root_sets_for_namespaced_target_outputs() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": [],
                  "platform": "x86_64-unknown-linux-musl"
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": [],
                  "platform": "aarch64-apple-darwin"
                }
              ],
              "roots": [0, 1],
              "root_sets": [[0], [1]]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("targetSets = ["));
        assert_eq!(rendered.matches("binaries = {").count(), 3);
        assert_eq!(rendered.matches("\"hello\" = units.").count(), 4);
    }

    #[test]
    fn scopes_doctests_to_each_root_set() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace/alpha#alpha@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "alpha",
                    "src_path": "/workspace/alpha/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace/beta#beta@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "beta",
                    "src_path": "/workspace/beta/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "features": [],
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0, 1],
              "root_sets": [[0], [1]]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert_eq!(rendered.matches("\"alpha\" = mkDoctestEntry").count(), 2);
        assert_eq!(rendered.matches("\"beta\" = mkDoctestEntry").count(), 2);
    }

    fn write_doctest_contract_sources(workspace: &Path) -> [String; 4] {
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            r#"[package]
name = "native"
version = "0.1.0"
"#,
        )
        .unwrap();
        let build_rs = workspace.join("build.rs");
        let leaf_rs = workspace.join("src/leaf.rs");
        let lib_rs = workspace.join("src/lib.rs");
        let middle_rs = workspace.join("src/middle.rs");
        fs::write(&build_rs, "fn main() {}\n").unwrap();
        fs::write(&leaf_rs, "pub fn leaf() {}\n").unwrap();
        fs::write(&lib_rs, "pub fn native() {}\n").unwrap();
        fs::write(&middle_rs, "pub fn middle() {}\n").unwrap();

        [build_rs, leaf_rs, lib_rs, middle_rs].map(|path| path.to_string_lossy().into_owned())
    }

    fn doctest_contract_graph(workspace: &Path) -> UnitGraph {
        let [build_rs, leaf_rs, lib_rs, middle_rs] = write_doctest_contract_sources(workspace);
        let pkg_id = format!("path+file://{}#native@0.1.0", workspace.display());

        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
          "version": 1,
          "units": [
            {
              "pkg_id": pkg_id,
              "target": {
                "kind": ["custom-build"],
                "crate_types": ["bin"],
                "name": "build-script-build",
                "src_path": build_rs,
                "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "features": [],
              "mode": "build",
              "dependencies": []
            },
            {
              "pkg_id": pkg_id,
              "target": {
                "kind": ["custom-build"],
                "crate_types": ["bin"],
                "name": "build-script-build",
                "src_path": build_rs,
                "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "features": [],
              "mode": "run-custom-build",
              "dependencies": [
                { "index": 0, "extern_crate_name": "build_script_build" }
              ]
            },
            {
              "pkg_id": pkg_id,
              "target": {
                "kind": ["lib"],
                "crate_types": ["lib"],
                "name": "leaf",
                "src_path": leaf_rs,
                "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "features": [],
              "mode": "build",
              "dependencies": []
            },
            {
              "pkg_id": pkg_id,
              "target": {
                "kind": ["lib"],
                "crate_types": ["lib"],
                "name": "middle",
                "src_path": middle_rs,
                "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "features": [],
              "mode": "build",
              "dependencies": [
                { "index": 2, "extern_crate_name": "leaf" }
              ]
            },
            {
              "pkg_id": pkg_id,
              "target": {
                "kind": ["lib"],
                "crate_types": ["lib"],
                "name": "native",
                "src_path": lib_rs,
                "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3", "rustflags": ["-C", "target-feature=+sse2"] },
              "lint_rustflags": ["--deny=warnings", "--warn=unexpected_cfgs"],
              "check_cfg_args": ["--check-cfg", "cfg(docsrs,test)"],
              "features": [],
              "mode": "build",
              "dependencies": [
                { "index": 1, "extern_crate_name": "build_script_build" },
                { "index": 3, "extern_crate_name": "middle" }
              ]
            }
          ],
          "roots": [4]
        }))
        .unwrap();

        graph
    }

    #[test]
    fn doctest_commands_match_cargo_rustdoc_contract() {
        let workspace = tempfile::tempdir().unwrap();
        let graph = doctest_contract_graph(workspace.path());

        let options = RenderOptions {
            workspace_root: workspace.path().to_path_buf(),
            vendor_root: None,
            cargo_lock_sources: CargoLockSources::default(),
            content_addressed: false,
            toolchain_id: None,
            deny_unused_crate_dependencies: false,
            deny_panics: false,
            embed_metadata: true,
            rmeta_stability_flags: vec![],
        };
        let rendered = render_units_nix(&graph, &options).unwrap();
        let prepared = prepare_graph(&graph, &options).unwrap();
        let doctest_index = *compute_doctest_keys(&graph, &prepared)
            .keys()
            .next()
            .unwrap();
        let run_case = render_doctest_command(
            &graph,
            &prepared,
            &options,
            doctest_index,
            DoctestCommandMode::RunCase,
        )
        .unwrap();

        assert_doctest_rendered_contract(&rendered);
        assert_eq!(run_case.matches("--test-args").count(), 1);
        assert!(run_case.contains("--test-args \"$DOCTEST_FILTER --include-ignored --nocapture\""));
        assert!(!run_case.contains("--test-args \"$TEST_NAME\""));
    }

    fn assert_doctest_rendered_contract(rendered: &str) {
        assert_eq!(rendered.matches("-Z unstable-options").count(), 3);
        assert!(rendered.contains("build_script_rustdoc_args=()"));
        assert!(rendered.contains("doctest_build_args=()"));
        assert!(rendered.contains("doctest_runtime_library_paths=()"));
        assert!(rendered.contains("rustdoc_args+=( \"''${build_script_rustdoc_args[@]}\" )"));
        assert!(rendered.contains("doctest_build_args+=( '-C' )"));
        assert!(rendered.contains("rustdoc_args+=( '--deny=warnings' )"));
        assert!(rendered.contains("rustdoc_args+=( '--warn=unexpected_cfgs' )"));
        assert!(rendered.contains("rustdoc_args+=( '--check-cfg' )"));
        assert!(rendered.contains("rustdoc_args+=( 'cfg(docsrs,test)' )"));
        assert!(rendered.contains("rustdoc_args+=( --doctest-build-arg \"$doctest_build_arg\" )"));
        assert!(rendered.contains("case \"$link_search_path\" in"));
        assert!(rendered.contains("/out-dir|\"${units."));
        assert!(
            rendered.contains(
                "/out-dir/*) doctest_runtime_library_paths+=( \"$link_search_path\" ) ;;"
            )
        );
        assert!(!rendered.contains(
            "[ -n \"$link_search_path\" ] && doctest_runtime_library_paths+=( \"$link_search_path\" )"
        ));
        assert!(
            rendered
                .contains("doctest_runtime_library_path_host=$(rustc -vV | sed -n 's/^host: //p')")
        );
        assert!(rendered.contains("doctest_runtime_library_path_var=DYLD_FALLBACK_LIBRARY_PATH"));
        assert!(rendered.contains(
            "doctest_runtime_library_path_default=\"$HOME/lib:/usr/local/lib:/usr/lib\""
        ));
        assert!(rendered.contains("doctest_runtime_library_path_var=LD_LIBRARY_PATH"));
        assert!(rendered.contains(
            "doctest_runtime_library_path_current=\"''${!doctest_runtime_library_path_var-}\""
        ));
        assert!(
            rendered.contains(
                "export \"$doctest_runtime_library_path_var=$doctest_runtime_library_path"
            )
        );
        assert!(!rendered.contains("done < \"${units.native-run}/rustc-link-lib"));
        assert!(!rendered.contains(
            "doctest_build_args+=( -C \"link-arg=$line\" )\n  done < \"${units.native-run}/rustc-cdylib-link-arg"
        ));
        assert!(!rendered.contains("rustdoc_args+=( \"''${build_script_flags[@]}\" )"));
        assert!(rendered.contains("done < \"${units."));
        assert!(rendered.contains("/rustc-env"));
        assert!(rendered.contains("compile_out_dir=$NIX_BUILD_TOP/cargo-unit-build-script-out"));
        assert!(rendered.contains(
            "rustc_args+=( --remap-path-prefix \"$compile_out_dir=/cargo-unit-build-script-out\" )"
        ));
        assert!(rendered.contains("rustdoc_args+=( -L \"dependency=${units.\"middle-0.1.0-"));
        assert!(rendered.contains("rustdoc_args+=( -L \"dependency=${units.\"leaf-0.1.0-"));
        assert!(rendered.contains("/out-dir/. \"$compile_out_dir\"/"));
        assert!(rendered.contains("rustc_env+=( OUT_DIR=\"$compile_out_dir\" )"));
        assert!(!rendered.contains("--test-args \"$TEST_NAME\""));
        assert!(rendered.contains("^running 1 test$"));
    }

    #[test]
    fn aggregates_unused_crate_dependency_reports_by_package() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "serde",
                    "src_path": "/vendor/serde/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "serde" }
                  ]
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "serde" }
                  ]
                }
              ],
              "roots": [1, 2]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[(
                    "serde",
                    "1.0.0",
                    "registry+https://github.com/rust-lang/crates.io-index",
                )]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: true,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert_eq!(
            rendered
                .matches("check_unused 'hello 0.1.0' 'serde'")
                .count(),
            1
        );
        assert!(rendered.contains("$out/nix-support/unused-crate-dependencies"));
    }

    fn nested_package_fixture() -> (tempfile::TempDir, UnitGraph, BTreeSet<PathBuf>) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("parent");
        for relative in ["src", "child/src", "tests/fixture/src"] {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"parent\"\nversion = \"0.1.0\"\n[package.metadata.ix.inputs]\nexclude-nested-packages = true\n",
        )
        .unwrap();
        fs::write(
            root.join("child/Cargo.toml"),
            "[package]\nname = \"child\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn parent() {}\n").unwrap();
        fs::write(root.join("child/src/lib.rs"), "pub fn child() {}\n").unwrap();
        let graph = single_library_graph(
            &format!("path+file://{}#parent@0.1.0", root.display()),
            "parent",
            &root.join("src/lib.rs").to_string_lossy(),
            "2024",
        );
        let packages =
            BTreeSet::from([root.clone(), root.join("child"), root.join("tests/fixture")]);
        (temp, graph, packages)
    }

    struct TestInputFixture {
        temp: tempfile::TempDir,
        graph: UnitGraph,
        options: RenderOptions,
    }

    fn test_input_fixture() -> TestInputFixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("fixture");
        fs::create_dir_all(root.join("src/tests")).unwrap();
        fs::create_dir_all(root.join("migrations")).unwrap();
        fs::write(root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n[package.metadata.ix.inputs]\ntest-only = [\"src/tests.rs\", \"src/tests/data.rs\"]\n").unwrap();
        fs::write(root.join("src/lib.rs"),
            "#[cfg(test)] mod tests;\npub const SQL: &str = include_str!(\"../migrations/one.sql\");\n").unwrap();
        fs::write(
            root.join("src/tests.rs"),
            "mod data;\n#[test] fn database() {}\n",
        )
        .unwrap();
        fs::write(root.join("src/tests/data.rs"), "pub const ROW: u8 = 1;\n").unwrap();
        fs::write(root.join("migrations/one.sql"), "SELECT 1;\n").unwrap();
        let mut graph = single_library_graph(
            &format!("path+file://{}#fixture@0.1.0", root.display()),
            "fixture",
            &root.join("src/lib.rs").to_string_lossy(),
            "2024",
        );
        let mut test = graph.units[0].clone();
        test.mode = UnitMode::Test;
        graph.units.push(test);
        let options = RenderOptions {
            workspace_root: temp.path().to_path_buf(),
            vendor_root: None,
            cargo_lock_sources: CargoLockSources::default(),
            content_addressed: true,
            toolchain_id: None,
            deny_unused_crate_dependencies: false,
            deny_panics: false,
            embed_metadata: true,
            rmeta_stability_flags: vec![],
        };
        TestInputFixture {
            temp,
            graph,
            options,
        }
    }

    // Observe the bytes selected by the emitted source boundary, rather than
    // treating equal source-entry names as evidence of equal file contents.
    fn selected_source_bytes(entry: &SourceEntry, workspace: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut pending = vec![entry.root.clone()];
        let mut files = BTreeMap::new();
        while let Some(path) = pending.pop() {
            let relative = relative_path_string(&path, workspace).unwrap();
            if entry.excluded_relatives.iter().any(|excluded| {
                relative == *excluded || relative.starts_with(&format!("{excluded}/"))
            }) {
                continue;
            }
            if path.is_dir() {
                pending.extend(
                    fs::read_dir(path)
                        .unwrap()
                        .map(|child| child.unwrap().path()),
                );
            } else {
                files.insert(relative, fs::read(path).unwrap());
            }
        }
        files
    }

    #[test]
    fn test_only_edits_preserve_production_inputs_but_change_test_inputs() {
        let TestInputFixture {
            temp,
            graph,
            options,
        } = test_input_fixture();
        let (refs, entries) = prepare_sources(&graph, &options).unwrap();
        assert_ne!(refs[0], refs[1]);
        assert_eq!(entries.len(), 2);
        let production = selected_source_bytes(&entries[&refs[0]], temp.path());
        let tests = selected_source_bytes(&entries[&refs[1]], temp.path());
        assert_eq!(
            entries[&refs[0]].excluded_relatives,
            ["fixture/src/tests/data.rs", "fixture/src/tests.rs"]
        );
        assert!(entries[&refs[1]].excluded_relatives.is_empty());
        let rendered = render_units_nix(&graph, &options).unwrap();
        assert!(rendered.contains("scopedWorkspaceSourceExcluding"));
        fs::write(
            temp.path().join("fixture/src/tests/data.rs"),
            "pub const ROW: u8 = 2;\n",
        )
        .unwrap();
        let (after_refs, after) = prepare_sources(&graph, &options).unwrap();
        assert_eq!(refs, after_refs);
        assert_eq!(
            production,
            selected_source_bytes(&after[&refs[0]], temp.path())
        );
        assert_ne!(tests, selected_source_bytes(&after[&refs[1]], temp.path()));
        fs::write(
            temp.path().join("fixture/migrations/one.sql"),
            "SELECT 2;\n",
        )
        .unwrap();
        let (sql_refs, sql_entries) = prepare_sources(&graph, &options).unwrap();
        assert_ne!(
            production,
            selected_source_bytes(&sql_entries[&sql_refs[0]], temp.path())
        );
        fs::write(
            temp.path().join("fixture/migrations/one.sql"),
            "SELECT 1;\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("fixture/src/lib.rs"),
            "pub fn changed() {}\n",
        )
        .unwrap();
        let (code_refs, code_entries) = prepare_sources(&graph, &options).unwrap();
        assert_ne!(
            production,
            selected_source_bytes(&code_entries[&code_refs[0]], temp.path())
        );
    }

    #[test]
    fn test_source_classes_are_independent_of_unit_order_and_render_history() {
        let TestInputFixture {
            temp,
            mut graph,
            options,
        } = test_input_fixture();
        let (refs, entries) = prepare_sources(&graph, &options).unwrap();
        graph.units.reverse();
        let (reversed, reversed_entries) = prepare_sources(&graph, &options).unwrap();
        assert_eq!(refs, reversed.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(entries, reversed_entries);
        fs::write(
            temp.path().join("fixture/src/lib.rs"),
            "include!(\"tests/data.rs\");\n",
        )
        .unwrap();
        let (_, changed) = prepare_sources(&graph, &options).unwrap();
        assert!(
            changed
                .values()
                .all(|source| source.excluded_relatives.is_empty())
        );
    }

    #[test]
    fn test_input_contract_retains_build_script_and_uncertain_compile_inputs() {
        let TestInputFixture {
            temp,
            graph,
            options,
        } = test_input_fixture();
        let packages = workspace_package_roots(&graph);
        for source in [
            "include!(concat!(\"tests/\", \"data.rs\"));\n",
            "#[path = \"tests/data.rs\"] mod fixture;\n",
            "const ROOT: &str = env!(\"CARGO_MANIFEST_DIR\");\n",
        ] {
            fs::write(temp.path().join("fixture/src/lib.rs"), source).unwrap();
            let entry = source_entry_for_unit(&graph.units[0], &options, &packages, false).unwrap();
            assert!(entry.excluded_relatives.is_empty(), "{source}");
        }
        fs::write(
            temp.path().join("fixture/src/lib.rs"),
            "#[cfg(test)] mod tests;\n",
        )
        .unwrap();
        assert!(
            !source_entry_for_unit(&graph.units[0], &options, &packages, false)
                .unwrap()
                .excluded_relatives
                .is_empty()
        );
        let entry = source_entry_for_unit(&graph.units[0], &options, &packages, true).unwrap();
        assert!(entry.excluded_relatives.is_empty());
        fs::write(temp.path().join("fixture/build.rs"), "fn main() {}\n").unwrap();
        assert!(
            source_entry_for_unit(&graph.units[0], &options, &packages, false)
                .unwrap()
                .excluded_relatives
                .is_empty()
        );
        for mode in [
            UnitMode::Test,
            UnitMode::Doc,
            UnitMode::Other("future".into()),
        ] {
            let mut unit = graph.units[0].clone();
            unit.mode = mode;
            assert_eq!(source_input_class(&unit), SourceInputClass::Complete);
        }
        let mut flagged = graph.units[0].clone();
        flagged.profile.rustflags = vec!["--cfg=test".into()];
        assert_eq!(source_input_class(&flagged), SourceInputClass::Complete);
    }

    #[cfg(unix)]
    #[test]
    fn test_input_contract_retains_referenced_symlinks_and_rejects_symlink_declarations() {
        let TestInputFixture {
            temp,
            graph,
            options,
        } = test_input_fixture();
        let root = temp.path().join("fixture");
        std::os::unix::fs::symlink("tests/data.rs", root.join("src/alias.rs")).unwrap();
        let (_, entries) = prepare_sources(&graph, &options).unwrap();
        assert!(
            entries
                .values()
                .all(|entry| entry.excluded_relatives.is_empty())
        );
        fs::remove_file(root.join("src/tests/data.rs")).unwrap();
        std::os::unix::fs::symlink("../lib.rs", root.join("src/tests/data.rs")).unwrap();
        assert!(test_source_exclusions(&graph.units[0], &root, false).is_err());
    }

    #[test]
    fn test_directory_reference_restores_unrelated_sibling_inputs() {
        let TestInputFixture {
            temp,
            graph,
            options,
        } = test_input_fixture();
        let root = temp.path().join("fixture");
        fs::write(
            root.join("Cargo.toml"),
            "[package.metadata.ix.inputs]\ntest-only = [\"src/tests\"]\n",
        )
        .unwrap();
        fs::write(
            root.join("src/tests/sibling.rs"),
            "pub const SIBLING: u8 = 2;\n",
        )
        .unwrap();
        fs::write(root.join("src/constant.rs"), "pub const VALUE: u8 = 1;\n").unwrap();
        fs::write(root.join("src/lib.rs"), "include!(\"constant.rs\");\n").unwrap();
        let (refs, entries) = prepare_sources(&graph, &options).unwrap();
        assert_eq!(entries[&refs[0]].excluded_relatives, ["fixture/src/tests"]);
        assert!(entries[&refs[1]].excluded_relatives.is_empty());
        fs::write(root.join("src/lib.rs"), "include!(\"tests/data.rs\");\n").unwrap();
        let (refs, entries) = prepare_sources(&graph, &options).unwrap();
        assert!(entries[&refs[0]].excluded_relatives.is_empty());
        let selected = selected_source_bytes(&entries[&refs[0]], temp.path());
        assert!(selected.contains_key("fixture/src/tests/sibling.rs"));
        assert!(selected.contains_key("fixture/src/tests/data.rs"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                "/outside-source-boundary",
                root.join("src/tests/escape.rs"),
            )
            .unwrap();
            assert!(prepare_sources(&graph, &options).is_err());
        }
    }

    #[test]
    fn test_input_contract_rejects_untyped_missing_or_escaping_paths() {
        let TestInputFixture { temp, graph, .. } = test_input_fixture();
        let root = temp.path().join("fixture");
        for setting in [
            "true",
            "[42]",
            "[\"../outside\"]",
            "[\"/absolute\"]",
            "[\"src/missing.rs\"]",
            "[\".\"]",
            "[\"src/tests.rs\", \"src/tests.rs\"]",
        ] {
            fs::write(
                root.join("Cargo.toml"),
                format!("[package.metadata.ix.inputs]\ntest-only = {setting}\n"),
            )
            .unwrap();
            assert!(
                test_source_exclusions(&graph.units[0], &root, false).is_err(),
                "{setting}"
            );
        }
    }

    #[test]
    fn nested_package_edits_leave_parent_scope_and_target_fixtures_unchanged() {
        let (temp, graph, packages) = nested_package_fixture();
        let (before, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert_eq!(excluded, ["parent/child"]);
        assert_eq!(before.scope, SourceScope::Package);
        fs::write(
            temp.path().join("parent/child/src/lib.rs"),
            "pub fn changed_child() {}\n",
        )
        .unwrap();
        let (after, after_excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert_eq!(before.root, after.root);
        assert_eq!(before.include_relatives, after.include_relatives);
        assert_eq!(excluded, after_excluded);
    }

    #[test]
    fn nested_package_boundary_requires_a_typed_explicit_declaration() {
        let (temp, graph, packages) = nested_package_fixture();
        let manifest = temp.path().join("parent/Cargo.toml");
        for extra in [
            "",
            "\n[package.metadata.ix.inputs]\nexclude-nested-packages = false\n",
        ] {
            fs::write(&manifest, format!("[package]\nname = \"parent\"\n{extra}")).unwrap();
            let (_, excluded) =
                local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
            assert!(excluded.is_empty());
        }
        fs::write(
            &manifest,
            "[package.metadata.ix.inputs]\nexclude-nested-packages = \"true\"\n",
        )
        .unwrap();
        assert!(
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).is_err()
        );
    }

    #[test]
    fn repeated_package_targets_share_scope_without_caching_across_renders() {
        let (temp, mut graph, _) = nested_package_fixture();
        let mut test = graph.units[0].clone();
        test.target.kind = vec!["test".to_owned()];
        graph.units.push(test);
        let options = RenderOptions {
            workspace_root: temp.path().to_path_buf(),
            vendor_root: None,
            cargo_lock_sources: CargoLockSources::default(),
            content_addressed: true,
            toolchain_id: None,
            deny_unused_crate_dependencies: false,
            deny_panics: false,
            embed_metadata: true,
            rmeta_stability_flags: vec![],
        };
        let (refs, entries) = prepare_sources(&graph, &options).unwrap();
        assert_eq!(refs[0], refs[1]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[&refs[0]].excluded_relatives, ["parent/child"]);
        let packages = workspace_package_roots(&graph);
        for unit in &graph.units {
            let uncached = source_entry_for_unit(unit, &options, &packages, false).unwrap();
            assert_eq!(entries[&uncached.name], uncached);
        }
        fs::write(
            temp.path().join("parent/src/lib.rs"),
            "include!(\"../child/src/lib.rs\");\n",
        )
        .unwrap();
        let (_, after) = prepare_sources(&graph, &options).unwrap();
        assert!(after[&refs[0]].excluded_relatives.is_empty());

        // An unparseable package id uses each target's name as its package
        // identity, even when both targets share the same manifest root.
        graph.units[0].pkg_id = "unparseable".to_owned();
        graph.units[1].pkg_id = "unparseable".to_owned();
        graph.units[1].target.name = "other_target".to_owned();
        let (fallback_refs, fallback_entries) = prepare_sources(&graph, &options).unwrap();
        assert_ne!(fallback_refs[0], fallback_refs[1]);
        for unit in &graph.units {
            let uncached = source_entry_for_unit(unit, &options, &packages, false).unwrap();
            assert_eq!(fallback_entries[&uncached.name], uncached);
        }
    }

    #[test]
    fn ordinary_path_fields_and_comments_do_not_declare_module_inputs() {
        let (temp, graph, packages) = nested_package_fixture();
        fs::write(
            temp.path().join("parent/src/lib.rs"),
            r#"// A path is ordinary data, not a module attribute.
#[doc = "path and include_str ! are documentation"]
struct Entry { path: String }
const DATA: &str = include_str /* whitespace */ ! ("lib.rs");
"#,
        )
        .unwrap();
        let (_, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert_eq!(excluded, ["parent/child"]);
    }

    #[test]
    fn uncertain_tokens_or_commented_module_attributes_keep_nested_inputs() {
        for source in [
            "#[path /* comment */ = \"../child/src/lib.rs\"] mod child;",
            "#[cfg_attr(feature = \"child\", path /* ] */ = \"../child/src/lib.rs\")] mod child;",
            "#[r#path = \"../child/src/lib.rs\"] mod child;",
            "const DATA: &str = r#include_str ! (concat!(\"../child/\", NAME));",
            "fn incomplete(",
            "// include_str!(\"lib.rs\")\nconst DATA: &str = include_str ! (concat!(\"../child/\", NAME));",
        ] {
            let (temp, graph, packages) = nested_package_fixture();
            fs::write(temp.path().join("parent/src/lib.rs"), source).unwrap();
            let (_, excluded) =
                local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
            assert!(excluded.is_empty(), "{source}");
        }
    }

    #[test]
    fn unsupported_string_escapes_keep_nested_inputs() {
        for source in [
            r#"const DATA: &str = include_str!("\x2e\x2e/child/src/lib.rs");"#,
            r#"const DATA: &str = include_str!("\u{2e}\u{2e}/child/src/lib.rs");"#,
            r#"const DATA: &str = include_str!("..\
                /child/src/lib.rs");"#,
        ] {
            let (temp, graph, packages) = nested_package_fixture();
            fs::write(temp.path().join("parent/src/lib.rs"), source).unwrap();
            let (_, excluded) =
                local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
            assert!(excluded.is_empty(), "{source}");
        }
    }

    #[test]
    fn nested_package_selection_does_not_change_parent_source_scope() {
        let (temp, graph, mut packages) = nested_package_fixture();
        let grandchild = temp.path().join("parent/child/grandchild");
        fs::create_dir_all(&grandchild).unwrap();
        fs::write(grandchild.join("Cargo.toml"), "[package]\n").unwrap();
        packages.insert(grandchild);
        let (selected, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        let (unselected, unselected_excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &BTreeSet::new(), false)
                .unwrap();
        assert_eq!(selected.root, unselected.root);
        assert_eq!(selected.include_relatives, unselected.include_relatives);
        assert_eq!(excluded, unselected_excluded);
        assert_eq!(excluded, ["parent/child"]);
    }

    #[test]
    fn explicit_nested_include_retains_package_and_its_external_inputs() {
        let (temp, graph, packages) = nested_package_fixture();
        let parent = temp.path().join("parent");
        fs::create_dir_all(temp.path().join("shared")).unwrap();
        fs::write(temp.path().join("shared/data.txt"), "data").unwrap();
        fs::write(
            parent.join("src/lib.rs"),
            "include!(\"../child/src/lib.rs\");\n",
        )
        .unwrap();
        fs::write(
            parent.join("child/src/lib.rs"),
            "const DATA: &str = include_str!(\"../../../shared/data.txt\");\n",
        )
        .unwrap();
        let (scope, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert!(excluded.is_empty());
        assert_eq!(scope.scope, SourceScope::Closure);
        assert!(scope.include_relatives.contains(&"shared".to_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_nested_symlink_retains_its_target_package() {
        let (temp, graph, packages) = nested_package_fixture();
        std::os::unix::fs::symlink(
            "../child/src/lib.rs",
            temp.path().join("parent/src/linked.rs"),
        )
        .unwrap();
        let (_, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert!(excluded.is_empty());
    }

    #[test]
    fn manifest_targets_and_unselected_build_scripts_preserve_nested_inputs() {
        let (temp, graph, packages) = nested_package_fixture();
        let root = temp.path().join("parent");
        for manifest in [
            "[package]\nname = \"parent\"\n[lib]\npath = \"child/src/lib.rs\"\n",
            "[package]\nname = \"parent\"\n[[bin]]\nname = \"child\"\npath = \"child/src/lib.rs\"\n",
            "[package]\nname = \"parent\"\nbuild = \"scripts/generate.rs\"\n",
        ] {
            fs::write(
                root.join("Cargo.toml"),
                format!(
                    "{manifest}\n[package.metadata.ix.inputs]\nexclude-nested-packages = true\n"
                ),
            )
            .unwrap();
            let (_, excluded) =
                local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
            assert!(excluded.is_empty(), "{manifest}: {excluded:?}");
        }
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"parent\"\n[package.metadata.ix.inputs]\nexclude-nested-packages = true\n").unwrap();
        fs::write(root.join("build.rs"), "fn main() {}\n").unwrap();
        let (_, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert!(excluded.is_empty());
    }

    #[test]
    fn unresolved_builtin_include_syntax_preserves_nested_inputs() {
        let (temp, graph, packages) = nested_package_fixture();
        for source in [
            r#"const DATA: &str = include_str ! ("../child/data");"#,
            r#"const DATA: &str = include_str /* input */ ! ("../child/data");"#,
            r#"const DATA: &str = include_str!["../child/data"];"#,
            r#"const DATA: &str = include_str!{"../child/data"};"#,
            r#"const DATA: &str = include_str!(concat!("../child/", "data"));"#,
            r#"#[path /* input */ = "../child/src/lib.rs"] mod child;"#,
            r#"#[cfg_attr(test, path /* input */ = "../child/src/lib.rs")] mod child;"#,
        ] {
            fs::write(temp.path().join("parent/src/lib.rs"), source).unwrap();
            let (_, excluded) =
                local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
            assert!(excluded.is_empty(), "{source}: {excluded:?}");
        }
    }

    #[test]
    fn dynamic_manifest_paths_and_build_scripts_preserve_nested_inputs() {
        let (temp, graph, packages) = nested_package_fixture();
        let (_, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, true).unwrap();
        assert!(excluded.is_empty());
        fs::write(
            temp.path().join("parent/src/lib.rs"),
            "const ROOT: &str = env!(\"CARGO_MANIFEST_DIR\");\n",
        )
        .unwrap();
        let (_, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert!(excluded.is_empty());
        fs::write(
            temp.path().join("parent/src/lib.rs"),
            "#[path = \"../child/src/lib.rs\"] mod child;\n",
        )
        .unwrap();
        let (_, excluded) =
            local_source_root_for_unit(&graph.units[0], temp.path(), &packages, false).unwrap();
        assert!(excluded.is_empty());
    }

    #[test]
    fn scopes_local_and_vendor_sources_per_package() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.15",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "itoa",
                    "src_path": "/vendor/itoa-1.0.15/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace/crates/core#scope-core@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "scope_core",
                    "src_path": "/workspace/crates/core/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "itoa" }
                  ]
                },
                {
                  "pkg_id": "path+file:///workspace/crates/cli#scope-cli@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "scope_cli",
                    "src_path": "/workspace/crates/cli/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 1, "extern_crate_name": "scope_core" }
                  ]
                }
              ],
              "roots": [2]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[(
                    "itoa",
                    "1.0.15",
                    "registry+https://github.com/rust-lang/crates.io-index",
                )]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("scopedWorkspaceSource \"cargo-unit-source-scope-core-0.1.0-"));
        assert!(rendered.contains("\"crates/core\""));
        assert!(rendered.contains("scopedWorkspaceSource \"cargo-unit-source-scope-cli-0.1.0-"));
        assert!(rendered.contains("\"crates/cli\""));
        assert!(rendered.contains(
            "vendorSources.\"registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.15\""
        ));
        assert!(rendered.contains("sourceAudit = {"));
        assert!(rendered.contains("base = \"vendor-package\";"));
        assert!(rendered.contains("\"$src/src/lib.rs\""));
        assert!(rendered.contains("\"$src/src/main.rs\""));
        assert!(!rendered.contains("${src}/crates/core"));
        assert!(!rendered.contains("${src}/crates/cli"));
        assert!(!rendered.contains("${vendorDir}/itoa-1.0.15"));
    }

    #[test]
    fn every_compile_unit_remaps_its_source_and_the_toolchain_out_of_its_output() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.15",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "itoa",
                    "src_path": "/vendor/itoa-1.0.15/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace/crates/cli#scope-cli@0.1.0",
                  "target": {
                    "kind": ["custom-build"],
                    "crate_types": ["bin"],
                    "name": "build-script-build",
                    "src_path": "/workspace/crates/cli/build.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace/crates/cli#scope-cli@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "scope_cli",
                    "src_path": "/workspace/crates/cli/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "itoa" }
                  ]
                }
              ],
              "roots": [1, 2]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[(
                    "itoa",
                    "1.0.15",
                    "registry+https://github.com/rust-lang/crates.io-index",
                )]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        // A vendored crate, a workspace bin and a build script all bake
        // `file!()` locations, so all three must lose their source store path.
        let source_remap =
            |prefix: &str| format!("rustc_args+=( --remap-path-prefix \"$src={prefix}\" )");
        assert!(rendered.contains(&source_remap("/build/itoa-1.0.15")));
        assert!(rendered.contains(&source_remap("/build/scope-cli-0.1.0")));
        // `rustc_args=()` opens exactly one rustc or clippy invocation, so
        // matching counts is what says no driver was left un-remapped.
        assert_eq!(
            rendered
                .matches(
                    "rustc_args+=( --remap-path-prefix \"${rustToolchain}/lib/rustlib/src/rust=/rustc\" )"
                )
                .count(),
            rendered.matches("rustc_args=()").count()
        );
        assert!(rendered.contains("remapPrefix = \"/build/itoa-1.0.15\";"));
        assert!(rendered.contains("remapPrefix = \"/build/scope-cli-0.1.0\";"));
    }

    #[test]
    fn no_embed_metadata_thins_lib_rlibs_and_adds_the_rmeta_extern() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace/crates/lib#scope-lib@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "scope_lib",
                    "src_path": "/workspace/crates/lib/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#demo-macro@1.0.0",
                  "target": {
                    "kind": ["proc-macro"],
                    "crate_types": ["proc-macro"],
                    "name": "demo_macro",
                    "src_path": "/vendor/demo-macro-1.0.0/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace/crates/cli#scope-cli@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "scope_cli",
                    "src_path": "/workspace/crates/cli/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "scope_lib" },
                    { "index": 1, "extern_crate_name": "demo_macro" }
                  ]
                }
              ],
              "roots": [2]
            }"#,
        )
        .unwrap();

        let options = |embed_metadata| RenderOptions {
            workspace_root: PathBuf::from("/workspace"),
            vendor_root: Some(PathBuf::from("/vendor")),
            cargo_lock_sources: cargo_lock_sources(&[(
                "demo-macro",
                "1.0.0",
                "registry+https://github.com/rust-lang/crates.io-index",
            )]),
            content_addressed: false,
            toolchain_id: None,
            deny_unused_crate_dependencies: false,
            deny_panics: false,
            embed_metadata,
            rmeta_stability_flags: vec![],
        };

        let thinned = render_units_nix(&graph, &options(false)).unwrap();
        // Exactly the lib unit compiles thin: the proc-macro's dylib is its
        // metadata carrier and the bin emits no metadata at all.
        assert_eq!(thinned.matches("-Zembed-metadata=no").count(), 1);
        // Every dependent passes the dependency twice, rlib (from
        // extern-path) plus sibling .rmeta when one exists: rustc does not
        // fall back to -L search for an --extern-pinned crate, and the thin
        // rlib alone is a metadata stub. The sibling guard is generated for
        // both consumers of the bin unit (scope_lib and demo_macro; the
        // proc-macro's .so has no .rmeta sibling so the guard is a no-op).
        assert!(thinned.contains("rustc_args+=( --extern \"scope_lib=$extern_path\" )"));
        assert!(
            thinned.contains("rustc_args+=( --extern \"scope_lib=''${extern_path%.rlib}.rmeta\" )")
        );
        assert!(thinned.contains("rustc_args+=( --extern \"demo_macro=$extern_path\" )"));
        // extern-path priority is unchanged: the rlib stays the recorded
        // artifact, thin or not.
        assert!(thinned.contains(".rlib\" \\"));

        let embedded = render_units_nix(&graph, &options(true)).unwrap();
        assert!(!embedded.contains("-Zembed-metadata=no"));
        assert!(!embedded.contains(".rmeta\" )"));
    }

    #[test]
    fn rmeta_stability_flags_reach_exactly_the_metadata_emitting_rustc_compiles() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace/crates/lib#scope-lib@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "scope_lib",
                    "src_path": "/workspace/crates/lib/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#demo-macro@1.0.0",
                  "target": {
                    "kind": ["proc-macro"],
                    "crate_types": ["proc-macro"],
                    "name": "demo_macro",
                    "src_path": "/vendor/demo-macro-1.0.0/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace/crates/cli#scope-cli@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "scope_cli",
                    "src_path": "/workspace/crates/cli/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "scope_lib" },
                    { "index": 1, "extern_crate_name": "demo_macro" }
                  ]
                }
              ],
              "roots": [2]
            }"#,
        )
        .unwrap();

        let options = |flags: &[&str]| RenderOptions {
            workspace_root: PathBuf::from("/workspace"),
            vendor_root: Some(PathBuf::from("/vendor")),
            cargo_lock_sources: cargo_lock_sources(&[(
                "demo-macro",
                "1.0.0",
                "registry+https://github.com/rust-lang/crates.io-index",
            )]),
            content_addressed: false,
            toolchain_id: None,
            deny_unused_crate_dependencies: false,
            deny_panics: false,
            embed_metadata: false,
            rmeta_stability_flags: flags.iter().map(|flag| (*flag).to_string()).collect(),
        };

        let rendered = render_units_nix(
            &graph,
            &options(&["-Zrmeta-content-svh", "-Zrmeta-strip-spans=all"]),
        )
        .unwrap();
        // The flags land once, in the import-gated template binding: applied
        // only when the IMPORTING toolchain records a fork rev, so the
        // clippy-graph import (pinned llm-clippy toolchain, no fork rev)
        // compiles its parallel dependency units without them. Argv order
        // inside the binding is the policy layer's order.
        assert!(rendered.contains(
            "rmetaStabilityArgs = pkgs.lib.optionals (rustToolchain ? forkRev) [ \"-Zrmeta-content-svh\" \"-Zrmeta-strip-spans=all\" ];"
        ));
        assert_eq!(rendered.matches("-Zrmeta-content-svh").count(), 1);
        assert_eq!(rendered.matches("-Zrmeta-strip-spans=all").count(), 1);
        // Exactly one unit in this graph emits consumable metadata under the
        // Rustc driver: the lib. The proc-macro (dylib is its metadata
        // carrier, no separate .rmeta here) and the bin (no metadata at all)
        // must not reference the binding, and neither must any clippy
        // invocation, whose driver never receives the flags at all.
        assert_eq!(
            rendered
                .matches("rustc_args+=( ${pkgs.lib.escapeShellArgs rmetaStabilityArgs} )")
                .count(),
            1
        );

        let bare = render_units_nix(&graph, &options(&[])).unwrap();
        // `-Zrmeta`, not `rmeta-`: the template's rmetaStabilityArgs binding
        // and its comment exist in every render (empty list when no flags);
        // what must not exist without flags is any actual flag or any unit
        // referencing the binding.
        assert!(!bare.contains("-Zrmeta"));
        assert!(!bare.contains("escapeShellArgs rmetaStabilityArgs"));
    }

    #[test]
    fn vendor_sources_are_keyed_by_full_package_identity() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.15",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "itoa",
                    "src_path": "/vendor/crates-io-itoa/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "sparse+https://example.invalid/index/#itoa@1.0.15",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "itoa",
                    "src_path": "/vendor/example-itoa/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0, 1]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[
                    (
                        "itoa",
                        "1.0.15",
                        "registry+https://github.com/rust-lang/crates.io-index",
                    ),
                    ("itoa", "1.0.15", "sparse+https://example.invalid/index/"),
                ]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains(
            "vendorSources.\"registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.15\""
        ));
        assert!(
            rendered
                .contains("vendorSources.\"sparse+https://example.invalid/index/#itoa@1.0.15\"")
        );
    }

    #[test]
    fn git_vendor_sources_use_locked_source_identity() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "git+https://github.com/shepmaster/snafu.git#snafu@0.9.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "snafu",
                    "src_path": "/vendor/snafu/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let locked_source =
            "git+https://github.com/shepmaster/snafu.git#1f8e75f56390c421a198871916100c6316d23d4f";
        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[("snafu", "0.9.0", locked_source)]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains(&format!("vendorSources.\"{locked_source}#snafu@0.9.0\"")));
        assert!(
            !rendered.contains(
                "vendorSources.\"git+https://github.com/shepmaster/snafu.git#snafu@0.9.0\""
            )
        );
    }

    #[test]
    fn git_vendor_sources_match_unit_graph_version_only_fragments() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "git+https://github.com/rust-netlink/rtnetlink?rev=eb685374ba7f7a1201754f6b2b40c491d3d50cb3#0.20.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "rtnetlink",
                    "src_path": "/vendor/rtnetlink/src/lib.rs",
                    "edition": "2021"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let locked_source = "git+https://github.com/rust-netlink/rtnetlink?rev=eb685374ba7f7a1201754f6b2b40c491d3d50cb3#eb685374ba7f7a1201754f6b2b40c491d3d50cb3";
        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(PathBuf::from("/vendor")),
                cargo_lock_sources: cargo_lock_sources(&[("rtnetlink", "0.20.0", locked_source)]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains(&format!(
            "vendorSources.\"{locked_source}#rtnetlink@0.20.0\""
        )));
    }

    #[cfg(unix)]
    #[test]
    fn builds_filtered_source_closure_when_package_symlinks_escape_root() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-symlink-source-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        fs::create_dir_all(workspace.join("internal")).unwrap();
        fs::create_dir_all(workspace.join("sibling/src")).unwrap();
        fs::write(
            workspace.join("internal/Cargo.toml"),
            r#"[package]
name = "internal"
version = "0.1.0"
"#,
        )
        .unwrap();
        fs::write(workspace.join("internal/lib.rs"), "pub fn marker() {}\n").unwrap();
        std::os::unix::fs::symlink("../sibling/src", workspace.join("internal/src")).unwrap();
        let src_path = workspace.join("internal/lib.rs");
        let pkg_id = format!(
            "path+file://{}#internal@0.1.0",
            workspace.join("internal").display()
        );
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "name": "internal",
                        "src_path": src_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": []
                }
            ],
            "roots": [0]
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(
            rendered.contains("scopedWorkspaceClosureSource \"cargo-unit-source-internal-0.1.0-")
        );
        assert!(rendered.contains("[ \"internal\" \"sibling/src\" ]"));
        assert!(rendered.contains("export CARGO_MANIFEST_DIR=\"$src/internal\""));
        assert!(rendered.contains("\"$src/internal/lib.rs\""));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn extends_source_closure_through_include_macros() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-include-source-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        fs::create_dir_all(workspace.join("regex-lite/tests")).unwrap();
        fs::create_dir_all(workspace.join("testdata")).unwrap();
        fs::write(
            workspace.join("regex-lite/Cargo.toml"),
            r#"[package]
name = "regex-lite"
version = "0.1.0"
"#,
        )
        .unwrap();
        // The test entry point mirrors the regex-lite shape: an integration
        // test under `tests/` that reads sibling testdata via `include_bytes!`
        // with a parent-relative path. The walker must add the testdata dir
        // to the rustc source closure, otherwise the build sandbox can't see it.
        fs::write(
            workspace.join("regex-lite/tests/lib.rs"),
            r#"const ANCHORED: &[u8] = include_bytes!("../../testdata/anchored.toml");
const CRLF: &str = include_str!("../../testdata/crlf.toml");
"#,
        )
        .unwrap();
        fs::write(
            workspace.join("testdata/anchored.toml"),
            "name = 'anchored'\n",
        )
        .unwrap();
        fs::write(workspace.join("testdata/crlf.toml"), "name = 'crlf'\n").unwrap();
        let src_path = workspace.join("regex-lite/tests/lib.rs");
        let pkg_id = format!(
            "path+file://{}#regex-lite@0.1.0",
            workspace.join("regex-lite").display()
        );
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["test"],
                        "crate_types": ["bin"],
                        "name": "integration",
                        "src_path": src_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3", "test": true },
                    "mode": "build",
                    "dependencies": []
                }
            ],
            "roots": [0]
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("[ \"regex-lite\" \"testdata\" ]"));
        assert!(rendered.contains("sourcePackageRelative ="));
        assert!(rendered.contains("test_source_root="));
        assert!(rendered.contains("test_package_relative="));
        assert!(rendered.contains("test_cwd=\"$test_root/$test_package_relative\""));
        assert!(rendered.contains("cp -R \"$test_source_root\"/. \"$test_root\"/"));
        fs::remove_dir_all(workspace).unwrap();
    }

    /// The cas-gc-eval shape (ix#10171): a crate whose build-time citations
    /// read files out of OTHER workspace crates through a `macro_rules!`
    /// wrapper around `include_str!`. The include marker never appears at
    /// the call site, so before this the cited crate's directory was absent
    /// from the unit's source and the build failed inside the sandbox with
    /// "couldn't read .../ship.rs: No such file or directory".
    #[test]
    fn extends_source_closure_through_wrapper_macros_that_escape_the_package() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-wrapper-include-source-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        fs::create_dir_all(workspace.join("evaluator/src")).unwrap();
        fs::create_dir_all(workspace.join("cited/src")).unwrap();
        // A real directory holding no cited file, so that following the
        // citation below would be visible in the rendered closure.
        fs::create_dir_all(workspace.join("uncited/src")).unwrap();
        fs::write(
            workspace.join("evaluator/Cargo.toml"),
            r#"[package]
name = "evaluator"
version = "0.1.0"
"#,
        )
        .unwrap();
        fs::write(
            workspace.join("cited/src/ship.rs"),
            "// only the local sidecar did\n",
        )
        .unwrap();
        fs::write(
            workspace.join("evaluator/src/lib.rs"),
            r#"macro_rules! cite {
    ($path:literal, $anchor:literal) => {{
        const SOURCE: &str = include_str!($path);
        SOURCE
    }};
}

// The cited file is real, and its directory must reach the sandbox.
pub const SHIP: &str = cite!("../../cited/src/ship.rs", "only the local sidecar did");
// A literal that names no file must not widen anything, and it points at
// its own directory so that following it would show up in the closure.
pub const GONE: &str = cite!("../../uncited/src/absent.rs", "anchor");
// A plain relative-path constant is not a macro argument, so the parent
// directory it names must not be dragged into the source either.
pub const UP: &str = "../../";
"#,
        )
        .unwrap();
        let src_path = workspace.join("evaluator/src/lib.rs");
        let pkg_id = format!(
            "path+file://{}#evaluator@0.1.0",
            workspace.join("evaluator").display()
        );
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "name": "evaluator",
                        "src_path": src_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3", "test": false },
                    "mode": "build",
                    "dependencies": []
                }
            ],
            "roots": [0]
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(
            rendered.contains("[ \"cited/src\" \"evaluator\" ]"),
            "the cited crate's directory must join the unit source closure: {rendered}"
        );
        assert!(
            !rendered.contains("uncited"),
            "a wrapper-macro literal that names no file must not widen the closure: {rendered}"
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn include_macro_does_not_promote_vendor_root_to_source_closure() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-vendor-include-boundary-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let vendor = workspace.join("vendor");
        let real = workspace.join("real");
        let clap = real.join("clap-4.6.1");
        let derive_arbitrary = real.join("derive_arbitrary-1.4.2");
        fs::create_dir_all(clap.join("src")).unwrap();
        fs::create_dir_all(derive_arbitrary.join("src")).unwrap();
        fs::create_dir_all(&vendor).unwrap();
        fs::write(
            clap.join("Cargo.toml"),
            r#"[package]
name = "clap"
version = "4.6.1"
"#,
        )
        .unwrap();
        fs::write(
            clap.join("src/lib.rs"),
            r#"#![doc = include_str!("../../README.md")]
"#,
        )
        .unwrap();
        fs::write(derive_arbitrary.join("src/lib.rs"), "pub fn marker() {}\n").unwrap();
        fs::write(vendor.join("README.md"), "vendor readme\n").unwrap();
        std::os::unix::fs::symlink(&clap, vendor.join("clap-4.6.1")).unwrap();
        std::os::unix::fs::symlink(&derive_arbitrary, vendor.join("derive_arbitrary-1.4.2"))
            .unwrap();
        let src_path = vendor.join("clap-4.6.1/src/lib.rs");
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": "registry+https://github.com/rust-lang/crates.io-index#clap@4.6.1",
                    "target": {
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "name": "clap",
                        "src_path": src_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": []
                }
            ],
            "roots": [0]
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: Some(vendor),
                cargo_lock_sources: cargo_lock_sources(&[(
                    "clap",
                    "4.6.1",
                    "registry+https://github.com/rust-lang/crates.io-index",
                )]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("[ \"README.md\" \"clap-4.6.1\" ]"));
        assert!(!rendered.contains("derive_arbitrary-1.4.2"));
        fs::remove_dir_all(workspace).unwrap();
    }

    /// The two ways a scanned path can be spelled, flattened so the
    /// expectations below read as the source does.
    fn scanned(path: &str, require_existing_file: bool) -> ScannedIncludePath {
        ScannedIncludePath {
            path: path.to_string(),
            require_existing_file,
        }
    }

    #[test]
    fn extracts_literal_include_macro_paths_in_order() {
        let source = r#"
            const A: &[u8] = include_bytes!("data/anchored.toml");
            const B: &str = include_str!("../../shared/template.txt");
            // The filename stays dynamic, but the directory is static. The
            // nested `concat!` is a macro call the scan meets again on its
            // own; its literal is a directory prefix, so it contributes
            // nothing a second time:
            include_bytes!(concat!("../../testdata/", CASE, ".toml"));
            // OUT_DIR paths are not source paths and are skipped:
            include!(concat!(env!("OUT_DIR"), "/generated.rs"));
            // Raw strings without `#` are still literals:
            const C: &[u8] = include_bytes!(r"raw/data.bin");
            // Wrong macro name with the same suffix: not an include macro, and
            // the literal does not escape the package, so nothing is lifted:
            let _ = my_include_bytes!("not_a_real_macro");
            // Comments and intra-string occurrences are over-matched but harmless:
            // include_str!("from_a_comment.txt")
        "#;
        let mut paths = extract_include_macro_paths(source);
        paths.sort();
        assert_eq!(
            paths,
            vec![
                scanned("../../shared/template.txt", false),
                scanned("../../testdata", false),
                scanned("data/anchored.toml", false),
                scanned("from_a_comment.txt", false),
                scanned("raw/data.bin", false),
            ]
        );
    }

    /// The cas-gc-eval shape: the include marker is inside a `macro_rules!`
    /// body, so every call site spells only the wrapper's name and a path
    /// that climbs out of the package.
    #[test]
    fn extracts_parent_escaping_paths_from_wrapper_macros() {
        let source = r#"
            citation: cite!(
                "../../../control/postgres/backup/src/ship.rs",
                "only the local sidecar did",
            ),
            // Qualified calls name the same macro:
            let _ = crate::cite!("../../../storage/cas/src/lib.rs", "anchor");
            // Inside the package already, so following it would gain nothing:
            let _ = cite!("sibling.rs", "anchor");
            // Not a path, and never resolves to a file, but the shape is the
            // one that would drag a parent directory in if it were followed:
            const UP_TO_CRATES: &str = "../../../";
            let _ = assert!(!is_crate_relative("../../../"));
            let _ = format!("../{name}/index.js");
            // Unary `!` and `!=` are not macro calls:
            if !(a != b) && c != d { }
        "#;
        let mut paths = extract_include_macro_paths(source);
        paths.sort();
        assert_eq!(
            paths,
            vec![
                scanned("../../../control/postgres/backup/src/ship.rs", true),
                scanned("../../../storage/cas/src/lib.rs", true),
                scanned("../{name}/index.js", true),
            ],
            "a wrapper-macro literal is lifted only when it escapes the package; \
             `require_existing_file` is what then drops `../{{name}}/index.js`"
        );
    }

    #[test]
    fn rejects_sources_without_a_resolvable_owner() {
        let cases = [
            (
                "path+file:///repo/crates/alpha#alpha@0.1.0",
                "alpha",
                "/repo/crates/alpha/src/lib.rs",
                "2024",
                "outside workspace root",
            ),
            (
                "registry+https://github.com/rust-lang/crates.io-index#itoa@1.0.15",
                "itoa",
                "/vendor/itoa-1.0.15/src/lib.rs",
                "2021",
                "needs --vendor-root",
            ),
        ];

        for (pkg_id, name, source, edition, expected) in cases {
            let graph = single_library_graph(pkg_id, name, source, edition);
            let error = render_error(&graph);
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn content_addressed_is_explicitly_opt_in() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "hello 0.1.0 (path+file:///workspace)",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "dev", "opt_level": "0" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: true,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("__contentAddressed = true"));
        assert!(rendered.contains("outputHashMode = \"recursive\""));
        // Pins the CA hash algorithm, which was previously unasserted. ADR 0003
        // makes a nix CA output family-1 content, so this must stay blake3; a
        // silent revert to sha256 would put the renderer back outside the ADR.
        assert!(
            rendered.contains("outputHashAlgo = \"blake3\""),
            "content-addressed units must address their output with blake3"
        );
    }

    #[test]
    fn tests_stay_input_addressed_even_with_content_addressing() {
        // Tests are leaf derivations nothing consumes, so they get no CA early
        // cutoff, only floating-realisation fragility. They must stay
        // input-addressed even when content_addressed is on (libraries remain
        // CA -- see content_addressed_is_explicitly_opt_in).
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "hello 0.1.0 (path+file:///workspace)",
                  "target": {
                    "kind": ["test"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024",
                    "test": true
                  },
                  "profile": { "name": "test", "opt_level": "0" },
                  "mode": "test",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: true,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(
            !rendered.contains("__contentAddressed = true"),
            "test units must be input-addressed even with content_addressed = true"
        );
    }

    #[test]
    fn target_linker_environment_is_forwarded_to_rustc() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "platform": "x86_64-apple-darwin",
                  "dependencies": []
                }
              ],
              "roots": [0]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(
            rendered
                .contains("if [ \"''${CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER+x}\" = x ]; then")
        );
        assert!(rendered.contains(
            "rustc_args+=( -C \"linker=''${CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER}\" )"
        ));
        assert!(rendered.contains("--target"));
        assert!(rendered.contains("x86_64-apple-darwin"));
    }

    #[test]
    fn extra_rustc_args_are_requested_for_the_unit_platform() {
        let graph: UnitGraph = serde_json::from_str(
            r#"{
              "version": 1,
              "units": [
                {
                  "pkg_id": "path+file:///workspace#host@0.1.0",
                  "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "host",
                    "src_path": "/workspace/src/lib.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                },
                {
                  "pkg_id": "path+file:///workspace#hello@0.1.0",
                  "target": {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "hello",
                    "src_path": "/workspace/src/main.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "platform": "x86_64-apple-darwin",
                  "dependencies": [
                    { "index": 0, "extern_crate_name": "host" }
                  ]
                },
                {
                  "pkg_id": "path+file:///workspace#build-helper@0.1.0",
                  "target": {
                    "kind": ["custom-build"],
                    "crate_types": ["bin"],
                    "name": "build-script-build",
                    "src_path": "/workspace/build.rs",
                    "edition": "2024"
                  },
                  "profile": { "name": "release", "opt_level": "3" },
                  "mode": "build",
                  "dependencies": []
                }
              ],
              "roots": [1, 2]
            }"#,
        )
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("extraRustcArgsForPlatform ? _platform: []"));
        assert!(rendered.contains("${renderExtraRustcArgs \"host\" null}"));
        assert!(rendered.contains("${renderExtraRustcArgs \"hello\" \"x86_64-apple-darwin\"}"));
        assert!(rendered.contains("${renderExtraLinkRustcArgs \"x86_64-apple-darwin\"}"));
        assert!(!rendered.contains("${renderExtraLinkRustcArgs null}"));
        assert!(!rendered.contains("${renderExtraRustcArgs \"build-helper\" null}"));
    }

    #[test]
    fn empty_shell_env_values_do_not_close_generated_nix_strings() {
        assert_eq!(shell_env_value(""), "\"\"");
        assert_eq!(
            shell_env_value("compiler's ${api}"),
            r#""compiler's \''${api}""#
        );
    }

    #[test]
    fn cargo_manifest_links_reads_dotted_package_keys() {
        let links = cargo_manifest_package_links(
            r#"
package.name = "native"
package.version = "0.1.0"
package.links = "native_ffi"
"#,
        )
        .unwrap();

        assert_eq!(links.as_deref(), Some("native_ffi"));
    }

    #[test]
    fn cargo_manifest_links_rejects_non_string_values() {
        let err = cargo_manifest_package_links(
            r#"
[package]
name = "native"
version = "0.1.0"
links = 5
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("invalid type"),
            "expected TOML type error, got: {err}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn build_script_runs_receive_cargo_target_cfg_and_feature_environment() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-render-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            r#"[package]
name = "native"
version = "0.1.0-alpha.1"
authors = ["Native Team", "Build Crew"]
description = "Native FFI fixtures"
homepage = "https://example.com/native"
repository = "https://example.com/native.git"
license = "MIT"
license-file = "LICENSE"
rust-version = "1.85"
links = "native_ffi"
"#,
        )
        .unwrap();
        let build_rs = workspace.join("build.rs");
        fs::write(&build_rs, "fn main() {}\n").unwrap();
        let build_rs_path = build_rs.to_string_lossy();
        let pkg_id = format!("path+file://{}#native@0.1.0-alpha.1", workspace.display());
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-build",
                        "src_path": build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3", "rustflags": ["-C", "target-cpu=native"] },
                    "features": ["arch", "simd-support"],
                    "mode": "build",
                    "dependencies": []
                },
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-build",
                        "src_path": build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3", "rustflags": ["-C", "target-cpu=native"] },
                    "features": ["arch", "simd-support"],
                    "mode": "run-custom-build",
                    "platform": "x86_64-unknown-linux-gnu",
                    "dependencies": [
                        { "index": 0, "extern_crate_name": "build_script_build" }
                    ]
                }
            ],
            "roots": []
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("export TARGET='x86_64-unknown-linux-gnu'"));
        assert!(rendered.contains("export CARGO_PKG_VERSION_PRE=\"alpha.1\""));
        assert!(rendered.contains("export CARGO_PKG_AUTHORS=\"Native Team:Build Crew\""));
        assert!(rendered.contains("export CARGO_PKG_DESCRIPTION=\"Native FFI fixtures\""));
        assert!(rendered.contains("export CARGO_PKG_HOMEPAGE=\"https://example.com/native\""));
        assert!(
            rendered.contains("export CARGO_PKG_REPOSITORY=\"https://example.com/native.git\"")
        );
        assert!(rendered.contains("export CARGO_PKG_LICENSE=\"MIT\""));
        assert!(rendered.contains("export CARGO_PKG_LICENSE_FILE=\"LICENSE\""));
        assert!(rendered.contains("export CARGO_PKG_RUST_VERSION=\"1.85\""));
        assert!(rendered.contains("export CARGO_MANIFEST_LINKS=\"native_ffi\""));
        assert!(rendered.contains("export RUSTDOC=\"$(type -p rustdoc)\""));
        assert!(rendered.contains("export CARGO_FEATURE_ARCH=1"));
        assert!(rendered.contains("export CARGO_FEATURE_SIMD_SUPPORT=1"));
        assert!(rendered.contains("cargo_encoded_rustflags=( '-C' 'target-cpu=native' )"));
        assert!(
            rendered
                .contains("${renderCargoEncodedRustflags \"native\" \"x86_64-unknown-linux-gnu\"}")
        );
        assert!(rendered.contains("export CARGO_ENCODED_RUSTFLAGS="));
        assert!(rendered.contains("\"$RUSTC\" --print cfg --target \"$TARGET\""));
        assert!(rendered.contains("cargo_cfg_env=\"CARGO_CFG_$(printf '%s' \"$cargo_cfg_key\""));
        assert!(
            rendered.contains("export \"$cargo_cfg_env=''${!cargo_cfg_env},$cargo_cfg_value\"")
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn build_script_manifest_dir_uses_package_root_for_nested_entrypoints() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-nested-build-script-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let package_root = workspace.join("crates/nested-native");
        fs::create_dir_all(package_root.join("builder")).unwrap();
        fs::write(
            package_root.join("Cargo.toml"),
            r#"[package]
name = "nested-native"
version = "0.1.0"
links = "nested_native"
"#,
        )
        .unwrap();
        let build_rs = package_root.join("builder").join("main.rs");
        fs::write(&build_rs, "fn main() {}\n").unwrap();
        let build_rs_path = build_rs.to_string_lossy();
        let pkg_id = format!("path+file://{}#nested-native@0.1.0", package_root.display());
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-main",
                        "src_path": build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": []
                },
                {
                    "pkg_id": pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-main",
                        "src_path": build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "run-custom-build",
                    "dependencies": [
                        { "index": 0, "extern_crate_name": "build_script_main" }
                    ]
                }
            ],
            "roots": []
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("build_script_manifest_dir_source=\"$src\""));
        assert!(rendered.contains("build_script_manifest_dir=$out/'crates/nested-native'"));
        assert!(rendered.contains(
            "cp -RL \"$build_script_manifest_dir_source\"/. \"$build_script_manifest_dir\"/"
        ));
        assert!(rendered.contains("chmod -R u+w \"$build_script_manifest_dir\""));
        assert!(rendered.contains("export CARGO_MANIFEST_DIR=$build_script_manifest_dir"));
        assert!(!rendered.contains("build_script_manifest_dir_source=\"$src/builder\""));
        assert!(rendered.contains("export CARGO_MANIFEST_LINKS=\"nested_native\""));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn build_script_runs_receive_dependency_metadata_environment() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-dependency-metadata-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let sys_root = workspace.join("native-sys");
        let app_root = workspace.join("app");
        fs::create_dir_all(sys_root.join("src")).unwrap();
        fs::create_dir_all(app_root.join("src")).unwrap();
        fs::write(
            sys_root.join("Cargo.toml"),
            r#"[package]
name = "native-sys"
version = "0.1.0"
links = "native-ffi"
"#,
        )
        .unwrap();
        fs::write(
            app_root.join("Cargo.toml"),
            r#"[package]
name = "app"
version = "0.1.0"
"#,
        )
        .unwrap();
        let sys_build_rs = sys_root.join("build.rs");
        let app_build_rs = app_root.join("build.rs");
        let sys_lib_rs = sys_root.join("src").join("lib.rs");
        let app_lib_rs = app_root.join("src").join("lib.rs");
        fs::write(&sys_build_rs, "fn main() {}\n").unwrap();
        fs::write(&app_build_rs, "fn main() {}\n").unwrap();
        fs::write(&sys_lib_rs, "pub fn native() {}\n").unwrap();
        fs::write(&app_lib_rs, "pub fn app() {}\n").unwrap();
        let sys_build_rs_path = sys_build_rs.to_string_lossy();
        let app_build_rs_path = app_build_rs.to_string_lossy();
        let sys_lib_rs_path = sys_lib_rs.to_string_lossy();
        let app_lib_rs_path = app_lib_rs.to_string_lossy();
        let sys_pkg_id = format!("path+file://{}#native-sys@0.1.0", sys_root.display());
        let app_pkg_id = format!("path+file://{}#app@0.1.0", app_root.display());
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [
                {
                    "pkg_id": sys_pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-build",
                        "src_path": sys_build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": []
                },
                {
                    "pkg_id": sys_pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-build",
                        "src_path": sys_build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "run-custom-build",
                    "dependencies": [
                        { "index": 0, "extern_crate_name": "build_script_build" }
                    ]
                },
                {
                    "pkg_id": app_pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-build",
                        "src_path": app_build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": []
                },
                {
                    "pkg_id": app_pkg_id,
                    "target": {
                        "kind": ["custom-build"],
                        "crate_types": ["bin"],
                        "name": "build-script-build",
                        "src_path": app_build_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "run-custom-build",
                    "dependencies": [
                        { "index": 2, "extern_crate_name": "build_script_build" },
                        { "index": 1, "extern_crate_name": "native_sys" }
                    ]
                },
                {
                    "pkg_id": sys_pkg_id,
                    "target": {
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "name": "native_sys",
                        "src_path": sys_lib_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": [
                        { "index": 1, "extern_crate_name": "build_script_build" }
                    ]
                },
                {
                    "pkg_id": app_pkg_id,
                    "target": {
                        "kind": ["lib"],
                        "crate_types": ["lib"],
                        "name": "app",
                        "src_path": app_lib_rs_path,
                        "edition": "2024"
                    },
                    "profile": { "name": "release", "opt_level": "3" },
                    "mode": "build",
                    "dependencies": [
                        { "index": 4, "extern_crate_name": "native_sys" }
                    ]
                }
            ],
            "roots": []
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains(
            "cargo_metadata_env=\"DEP_NATIVE_FFI_$(printf '%s' \"$cargo_metadata_key\" | tr '[:lower:]' '[:upper:]' | sed 's/[^[:alnum:]_]/_/g')\""
        ));
        assert!(rendered.contains(
            "build_script_env+=( \"DEP_NATIVE_FFI_$cargo_metadata_key=$cargo_metadata_value\" )"
        ));
        assert!(rendered.contains("export \"$cargo_metadata_env=$cargo_metadata_value\""));
        assert!(rendered.contains("env \"''${build_script_env[@]}\""));
        assert!(rendered.contains("/cargo-metadata build/cargo-metadata"));
        assert!(rendered.contains("cp build/cargo-metadata $out/nix-support/cargo-metadata"));
        assert!(rendered.contains("/nix-support/cargo-metadata\" ]; then"));
        assert!(rendered.contains(
            "rustc_env+=( \"DEP_NATIVE_FFI_$cargo_metadata_key=$cargo_metadata_value\" )"
        ));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn unit_graph_lint_flags_render_as_rustc_args() {
        let workspace = std::env::temp_dir().join(format!(
            "nix-cargo-unit-lint-flags-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/lib.rs"), "pub fn marker() {}\n").unwrap();

        let src_path = workspace.join("src/lib.rs");
        let pkg_id = format!("path+file://{}#linted@0.1.0", workspace.display());
        let graph: UnitGraph = serde_json::from_value(serde_json::json!({
            "version": 1,
            "units": [{
                "pkg_id": pkg_id,
                "target": {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "linted",
                    "src_path": src_path,
                    "edition": "2024"
                },
                "profile": { "name": "dev", "opt_level": "0" },
                "lint_rustflags": [
                    "--deny=clippy::all",
                    "--forbid=unsafe_code",
                    "--warn=unexpected_cfgs",
                    "--warn=clippy::pedantic",
                    "--check-cfg",
                    "cfg(ix_test)"
                ],
                "check_cfg_args": [
                    "--check-cfg",
                    "cfg(docsrs,test)",
                    "--check-cfg",
                    "cfg(feature, values(\"alpha\", \"beta\"))"
                ],
                "mode": "build",
                "dependencies": []
            }],
            "roots": [0]
        }))
        .unwrap();

        let rendered = render_units_nix(
            &graph,
            &RenderOptions {
                workspace_root: workspace.clone(),
                vendor_root: None,
                cargo_lock_sources: CargoLockSources::default(),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap();

        assert!(rendered.contains("rustc_args+=( '--deny=clippy::all' )"));
        assert!(rendered.contains("rustc_args+=( '--forbid=unsafe_code' )"));
        assert!(rendered.contains("rustc_args+=( '--warn=clippy::pedantic' )"));
        assert!(rendered.contains("rustc_args+=( '--warn=unexpected_cfgs' )"));
        assert!(rendered.contains("rustc_args+=( '--check-cfg' )"));
        assert!(rendered.contains("rustc_args+=( 'cfg(docsrs,test)' )"));
        assert!(rendered.contains("rustc_args+=( 'cfg(feature, values(\"alpha\", \"beta\"))' )"));
        assert!(rendered.contains("rustc_args+=( 'cfg(ix_test)' )"));
        fs::remove_dir_all(workspace).unwrap();
    }

    fn cargo_lock_sources_with_deps(packages: &[(&str, &str, &str, &[&str])]) -> CargoLockSources {
        CargoLockSources {
            packages: packages
                .iter()
                .map(|(name, version, source, dependencies)| CargoLockPackage {
                    name: (*name).to_string(),
                    version: (*version).to_string(),
                    source: (*source).to_string(),
                    dependencies: dependencies.iter().map(|d| (*d).to_string()).collect(),
                })
                .collect(),
        }
    }

    // A vendored crate whose build script shells out to `cargo metadata`
    // (detected by a cbindgen build-dependency) must run offline against a
    // vendored registry, or it dies reaching crates.io from the sandbox
    // (issue #4142, hegeltest-c 0.30.1). A build script with no such dependency
    // is left untouched so it does not gain a needless dependency-closure input.
    fn write_vendored_crate(vendor: &Path, name: &str, version: &str) {
        let dir = vendor.join(format!("{name}-{version}"));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        fs::write(dir.join("src/lib.rs"), "").unwrap();
        fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
    }

    fn cargo_metadata_build_script_graph(vendor: &Path, with_cbindgen: bool) -> UnitGraph {
        let registry = "registry+https://github.com/rust-lang/crates.io-index";
        let build_deps = if with_cbindgen {
            serde_json::json!([{ "index": 0, "extern_crate_name": "cbindgen" }])
        } else {
            serde_json::json!([])
        };
        let cbindgen_lib = vendor.join("cbindgen-0.29.4/src/lib.rs");
        let mycrate_build = vendor.join("mycrate-1.0.0/build.rs");
        let mycrate_lib = vendor.join("mycrate-1.0.0/src/lib.rs");
        serde_json::from_value(serde_json::json!({
          "version": 1,
          "units": [
            {
              "pkg_id": format!("{registry}#cbindgen@0.29.4"),
              "target": {
                "kind": ["lib"], "crate_types": ["lib"], "name": "cbindgen",
                "src_path": cbindgen_lib, "edition": "2021"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "mode": "build", "dependencies": []
            },
            {
              "pkg_id": format!("{registry}#mycrate@1.0.0"),
              "target": {
                "kind": ["custom-build"], "crate_types": ["bin"],
                "name": "build-script-build",
                "src_path": mycrate_build, "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "mode": "build", "dependencies": build_deps
            },
            {
              "pkg_id": format!("{registry}#mycrate@1.0.0"),
              "target": {
                "kind": ["custom-build"], "crate_types": ["bin"],
                "name": "build-script-build",
                "src_path": mycrate_build, "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "mode": "run-custom-build",
              "dependencies": [{ "index": 1, "extern_crate_name": "build_script_build" }]
            },
            {
              "pkg_id": format!("{registry}#mycrate@1.0.0"),
              "target": {
                "kind": ["lib"], "crate_types": ["lib"], "name": "mycrate",
                "src_path": mycrate_lib, "edition": "2024"
              },
              "profile": { "name": "release", "opt_level": "3" },
              "mode": "build",
              "dependencies": [{ "index": 2, "extern_crate_name": "build_script_build" }]
            }
          ],
          "roots": [3]
        }))
        .unwrap()
    }

    fn render_cargo_metadata_build_script(with_cbindgen: bool) -> String {
        let registry = "registry+https://github.com/rust-lang/crates.io-index";
        let vendor = tempfile::tempdir().unwrap();
        write_vendored_crate(vendor.path(), "cbindgen", "0.29.4");
        write_vendored_crate(vendor.path(), "mycrate", "1.0.0");
        render_units_nix(
            &cargo_metadata_build_script_graph(vendor.path(), with_cbindgen),
            &RenderOptions {
                workspace_root: PathBuf::from("/workspace"),
                vendor_root: Some(vendor.path().to_path_buf()),
                cargo_lock_sources: cargo_lock_sources_with_deps(&[
                    ("mycrate", "1.0.0", registry, &["cbindgen"]),
                    ("cbindgen", "0.29.4", registry, &[]),
                ]),
                content_addressed: false,
                toolchain_id: None,
                deny_unused_crate_dependencies: false,
                deny_panics: false,
                embed_metadata: true,
                rmeta_stability_flags: vec![],
            },
        )
        .unwrap()
    }

    #[test]
    fn cargo_metadata_build_script_runs_offline_against_vendored_registry() {
        let rendered = render_cargo_metadata_build_script(true);
        assert!(rendered.contains("cargoOfflineConfig ="));
        assert!(rendered.contains("[source.crates-io]"));
        assert!(rendered.contains("replace-with = \"vendored-sources\""));
        // The offline vendor dir is scoped to the crate's own dependency
        // closure, not the whole workspace: only cbindgen appears.
        assert!(rendered.contains(
            "vendorSources.\"registry+https://github.com/rust-lang/crates.io-index#cbindgen@0.29.4\""
        ));
        assert!(rendered.contains("export CARGO_NET_OFFLINE=true"));
        // Dev-dependencies are stripped: the workspace lock never records a
        // non-member's dev-deps, so they cannot be in the vendored universe.
        assert!(rendered.contains("dev[_-]dependencies"));
    }

    #[test]
    fn build_script_without_cargo_metadata_stays_online_free() {
        let rendered = render_cargo_metadata_build_script(false);
        assert!(!rendered.contains("cargoOfflineConfig ="));
        assert!(!rendered.contains("export CARGO_NET_OFFLINE=true"));
    }
}
