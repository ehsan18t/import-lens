use crate::cache::key::CacheIdentity;
use crate::ipc::protocol::{ImportRequest, ImportRuntime};
use crate::pipeline::bundler_aliases;
use oxc_resolver::{
    AliasValue, ModuleType, PathUtil, ResolveOptions, Resolver, TsConfig, TsconfigDiscovery,
    TsconfigOptions, TsconfigReferences,
};
use serde_json::Value;
use std::{
    cell::OnceCell,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, RwLock},
};

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub package_root: PathBuf,
    pub package_json: Value,
    pub entry_path: PathBuf,
    pub is_cjs: bool,
    pub side_effects: SideEffectsMode,
}

/// Declares [`SideEffectsMode`]'s arms and the [`SideEffectsKind`] naming each from the same line,
/// so an arm cannot be added without a kind, and a kind cannot be added without joining
/// [`SideEffectsKind::ALL`].
///
/// `every_side_effects_form_answers_with_what_rolldown_retained` quantifies over that list, so a
/// new declaration form cannot be handled without a row pinning it against what Rolldown retained.
macro_rules! side_effects_modes {
    ($(
        $(#[$attribute:meta])*
        $variant:ident $({ $($field:ident : $field_type:ty),* $(,)? })? => $kind:ident,
    )+) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum SideEffectsMode {
            $($(#[$attribute])* $variant $({ $($field: $field_type),* })?,)+
        }

        /// One per arm of [`SideEffectsMode`], carrying no data: the set of answers the daemon can
        /// give about a `sideEffects` declaration, enumerable by a test.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum SideEffectsKind {
            $($kind,)+
        }

        impl SideEffectsKind {
            /// Every kind, emitted alongside the arms themselves.
            pub const ALL: &'static [Self] = &[$(Self::$kind,)+];
        }

        impl SideEffectsMode {
            pub fn kind(&self) -> SideEffectsKind {
                match self {
                    $(Self::$variant { .. } => SideEffectsKind::$kind,)+
                }
            }
        }
    };
}

side_effects_modes! {
    False => False,
    True => True,
    /// The glob form: an array of patterns, or the single-pattern string §7.4 names as its equal.
    /// It carries the answer (whether the measured entry is declared effectful), not the
    /// patterns, so nothing downstream can read them a second way.
    Array { entry_matches: bool } => Array,
    Missing => Missing,
    Unknown => Unknown,
}

impl SideEffectsMode {
    /// **Whether the measured entry is one the package declares effectful**: a property of the
    /// import, not of the package.
    ///
    /// A package declaring `"sideEffects": ["**/*.css"]` is not side-effectful for a JavaScript
    /// import, so the array arm answers with the matcher, as the boolean arms answer with the
    /// boolean. This is the whole answer; callers must not OR in "is an array".
    pub fn has_side_effects(&self) -> bool {
        match self {
            Self::False => false,
            Self::True | Self::Missing | Self::Unknown => true,
            Self::Array { entry_matches } => *entry_matches,
        }
    }
}

#[derive(Debug, Clone)]
struct PackageManifest {
    root: PathBuf,
    json: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedModulePath {
    pub path: PathBuf,
    pub is_cjs: bool,
}

pub fn resolve_package_entry(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<ResolvedPackage, String> {
    validate_package_name(&request.package_name)?;

    let manifest = find_package_manifest(active_document_path, request)?;
    let entry_resolver = EntryResolver::for_package(&manifest.root, request.runtime);
    let resolution = resolve_with_oxc(entry_resolver.get(), active_document_path, request);
    let (entry_path, is_cjs) = match resolution {
        Ok(resolved) => {
            let entry_path = resolved.entry_path;
            if subpath_for_request(request).is_none() {
                validate_declared_entry_resolution(
                    entry_resolver.get(),
                    &manifest,
                    request.runtime,
                )?;
            }
            let is_cjs = resolved_entry_is_commonjs(&manifest, &entry_path, resolved.is_cjs);
            (entry_path, is_cjs)
        }
        Err(error) => resolve_legacy_fallback(&manifest, request, &error)?,
    };

    Ok(ResolvedPackage {
        side_effects: side_effects_mode(&manifest.json, &manifest.root, &entry_path),
        package_root: manifest.root,
        package_json: manifest.json,
        entry_path,
        is_cjs,
    })
}

pub fn resolved_from_cache_identity(identity: &CacheIdentity) -> Option<ResolvedPackage> {
    let package_root = PathBuf::from(identity.package_root.as_ref()?);
    let entry_path = PathBuf::from(identity.entry_path.as_ref()?);
    let package_json_path = package_root.join("package.json");
    let package_json: Value =
        serde_json::from_str(&fs::read_to_string(&package_json_path).ok()?).ok()?;
    let manifest = PackageManifest {
        root: package_root.clone(),
        json: package_json.clone(),
    };
    let is_cjs = resolved_entry_is_commonjs(&manifest, &entry_path, false);
    let side_effects = side_effects_mode(&package_json, &package_root, &entry_path);

    Some(ResolvedPackage {
        package_root,
        package_json,
        entry_path,
        is_cjs,
        side_effects,
    })
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    entry_path: PathBuf,
    is_cjs: bool,
}

/// The resolver one package-entry resolution runs through.
///
/// An installed package resolves through the shared set, whose memoized filesystem facts
/// [`invalidate_shared_resolvers`] lifts when `node_modules` changes. A workspace package (a link
/// whose real root sits outside `node_modules`) is edited and built in place, where no watcher
/// reports it, so a memoized fact about it (its manifest, or a miss on an unbuilt `dist/` file)
/// would stand for the daemon's life. It resolves through a resolver that lives for this one
/// resolution.
enum EntryResolver {
    Shared(Arc<ResolverSet>, ImportRuntime),
    Fresh(Box<Resolver>),
}

impl EntryResolver {
    fn for_package(package_root: &Path, runtime: ImportRuntime) -> Self {
        let installed = fs::canonicalize(package_root).is_ok_and(|real_root| {
            real_root
                .components()
                .any(|component| component.as_os_str() == "node_modules")
        });
        if installed {
            Self::Shared(shared_resolvers(), runtime)
        } else {
            Self::Fresh(Box::new(Resolver::new(resolve_options(runtime))))
        }
    }

    fn get(&self) -> &Resolver {
        match self {
            Self::Shared(resolvers, runtime) => resolvers.resolver(*runtime),
            Self::Fresh(resolver) => resolver,
        }
    }
}

fn resolve_with_oxc(
    resolver: &Resolver,
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<ResolvedEntry, String> {
    let directory = active_document_path
        .parent()
        .ok_or_else(|| "active document path has no parent directory".to_owned())?;

    let resolved = resolve_module_path(resolver, directory, &request.specifier)?;

    Ok(ResolvedEntry {
        entry_path: resolved.path,
        is_cjs: resolved.is_cjs,
    })
}

fn resolve_legacy_fallback(
    manifest: &PackageManifest,
    request: &ImportRequest,
    resolution_error: &str,
) -> Result<(PathBuf, bool), String> {
    let subpath = subpath_for_request(request);
    if manifest.json.get("exports").is_some() {
        if let Some(sub) = subpath {
            let exports_key = format!("./{sub}");
            return Err(format!(
                "subpath '{}' is not defined in the exports map of {}",
                exports_key, request.package_name
            ));
        }

        if let Some(main) = manifest.json.get("main").and_then(Value::as_str) {
            return resolve_file_candidate(&manifest.root.join(main))
                .map(|path| classify_resolved_entry(manifest, path, false));
        }

        if let Some(subpaths) = subpath_only_exports(&manifest.json) {
            return Err(no_root_entry_message(
                &request.package_name,
                "its exports map has no \".\"",
                &subpaths,
            ));
        }

        return Err(format!(
            "failed to resolve package entry with oxc_resolver: {resolution_error}"
        ));
    }

    if let Some(sub) = subpath {
        return resolve_file_candidate(&manifest.root.join(sub))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    // The resolver's own main-field order for this runtime, so the fallback cannot pick an entry
    // the resolver would have ranked lower.
    if let Some(target) = profile_entry_fields(request.runtime)
        .iter()
        .find_map(|field| manifest.json.get(*field).and_then(Value::as_str))
    {
        return resolve_file_candidate(&manifest.root.join(target))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    // The CommonJS default, right for the many packages that ship an `index.js` and declare
    // nothing. For a package that declares no entry field at all, a failure says plainly that
    // nothing at the root is importable and names the subpaths (`@next/font`: `./google`,
    // `./local`), instead of listing probed candidates, which reads as a resolver malfunction.
    resolve_file_candidate(&manifest.root.join("index.js"))
        .map(|path| classify_resolved_entry(manifest, path, false))
        .map_err(|probed| {
            if !crate::pipeline::native_binary::manifest_declares_no_js_entry(&manifest.json) {
                return probed;
            }
            no_root_entry_message(
                &request.package_name,
                "it declares no main, module, browser or exports, and has no index.js",
                &importable_subpaths(&manifest.root),
            )
        })
}

/// The one message for a package with nothing importable at its root. `pipeline::analyze` keys the
/// `no_root_entry` stage on its opening words, so both shapes must come through here.
fn no_root_entry_message(package_name: &str, reason: &str, subpaths: &[String]) -> String {
    let hint = if subpaths.is_empty() {
        String::new()
    } else {
        format!("; importable subpaths include {}", subpaths.join(", "))
    };
    format!("package '{package_name}' {NO_ROOT_ENTRY_PHRASE} ({reason}){hint}")
}

pub(crate) const NO_ROOT_ENTRY_PHRASE: &str = "has no importable root entry";

/// The subpaths of an `exports` map that maps subpaths only (every key starts with `./`, none is
/// `"."`), first eight in declaration order. `None` for a map with a root entry or a conditions
/// object, whose failure to resolve is something else.
fn subpath_only_exports(manifest: &Value) -> Option<Vec<String>> {
    let exports = manifest.get("exports")?.as_object()?;
    let subpath_only = !exports.is_empty()
        && exports
            .keys()
            .all(|key| key.starts_with("./") && key != ".");
    subpath_only.then(|| exports.keys().take(8).cloned().collect())
}

/// Immediate subdirectories that are themselves importable, so a "no entry" message can say where
/// the code actually is instead of only what is missing.
///
/// Deliberately shallow and bounded: it runs on a failure path to improve one sentence. Sorted, so
/// the message does not reorder itself between runs.
fn importable_subpaths(package_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(package_root) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            ["index.js", "index.mjs", "index.cjs"]
                .iter()
                .any(|name| entry.path().join(name).is_file())
        })
        .filter_map(|entry| entry.file_name().to_str().map(|name| format!("./{name}")))
        .take(8)
        .collect();
    found.sort();
    found
}

fn validate_declared_entry_resolution(
    resolver: &Resolver,
    manifest: &PackageManifest,
    runtime: ImportRuntime,
) -> Result<(), String> {
    if manifest.json.get("exports").is_some() {
        return Ok(());
    }

    let declared_entries = profile_entry_fields(runtime)
        .iter()
        .filter_map(|field| {
            manifest
                .json
                .get(*field)
                .and_then(Value::as_str)
                .map(|target| (*field, target))
        })
        .collect::<Vec<_>>();
    if declared_entries.is_empty() {
        return Ok(());
    }

    for (_, target) in &declared_entries {
        if resolve_manifest_target(resolver, &manifest.root, target).is_ok() {
            return Ok(());
        }
    }

    let (_, first_target) = declared_entries[0];
    resolve_file_candidate(&manifest.root.join(first_target)).map(|_| ())
}

/// The top-level entry fields, in preference order, for a runtime. The single source of the
/// resolver's `main_fields` and of the legacy fallback's search order.
fn profile_entry_fields(runtime: ImportRuntime) -> &'static [&'static str] {
    match runtime {
        ImportRuntime::Component | ImportRuntime::Client => &["browser", "module", "main"],
        ImportRuntime::Server => &["module", "main"],
    }
}

fn resolve_manifest_target(
    resolver: &Resolver,
    package_root: &Path,
    target: &str,
) -> Result<ResolvedModulePath, String> {
    let specifier =
        if target.starts_with("./") || target.starts_with("../") || Path::new(target).is_absolute()
        {
            target.to_owned()
        } else {
            format!("./{target}")
        };

    resolve_module_path(resolver, package_root, &specifier)
}

fn find_package_manifest(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<PackageManifest, String> {
    let package_root = find_package_root(active_document_path, &request.package_name)?;
    let package_json_path = package_root.join("package.json");
    let json = serde_json::from_str::<Value>(&fs::read_to_string(&package_json_path).map_err(
        |error| {
            format!(
                "failed to read package manifest {}: {error}",
                package_json_path.display()
            )
        },
    )?)
    .map_err(|error| {
        format!(
            "failed to parse package manifest {}: {error}",
            package_json_path.display()
        )
    })?;

    if !json.get("version").is_some_and(Value::is_string) {
        return Err(format!(
            "package manifest {} is missing a string version",
            package_json_path.display()
        ));
    }

    Ok(PackageManifest {
        root: package_root,
        json,
    })
}

/// Whether a specifier resolves, through the project's `tsconfig.json` / `jsconfig.json` `paths` /
/// `baseUrl` or a bundler config's alias table, to a real file **outside `node_modules`**: first-party source, and therefore a **path
/// alias** rather than a package.
///
/// **Request-scoped, on both counts.** Not longer: each alias `Resolver` carries an `oxc_resolver`
/// filesystem cache that negative-caches a miss, and an import written before the file it points
/// at must stop being a floor once the file exists, with no restart and no invalidation message
/// (nothing watches first-party source). Not shorter: [`ResolverSet::alias_resolvers`] builds one
/// `Resolver` and a cold JSONC parse per reachable config, which per specifier cost a 20-alias
/// component ~20 ms of the 50 ms NFR-002 budget. One probe per response builds the set once,
/// lazily, so a document whose imports are all installed pays nothing.
///
/// **The question is about the workspace's alias table, not the importing document.** The same
/// specifier means the same thing from a `.ts`, `.vue`, `.svelte` or `.astro` file. So the config
/// is located by [`find_workspace_config`] and handed to oxc explicitly
/// (`TsconfigDiscovery::Manual`); see [`alias_resolve_options`] for why `Auto` cannot be used.
///
/// The nearest config alone is not the alias table either: the create-vue / create-astro scaffold
/// has a root `tsconfig.json` of nothing but `references`, with `paths` in `tsconfig.app.json`.
/// Which project owns the document is unknowable once `include` is discarded, so no project is
/// chosen: [`ResolverSet::alias_resolvers`] collects every reachable table (the nearest config, its
/// `references` transitively, and the `extends` each folds in), and one hit from any is positive
/// evidence.
///
/// The discriminator is **positive evidence**, never its absence, so the errors are asymmetric: a
/// specifier that resolves to nothing (a typo, a stale import, an uninstalled dependency) is a
/// floor that refuses a verdict, never a silent pass (ADR-0006). This tells apart the two kinds of
/// "no `node_modules/<name>/package.json`", which must never be conflated:
///
/// * **a package that is not installed**: its bytes are missing, so the total is a floor (SRS
///   FR-024a, bullet 4). Whether `package.json` declares it is not the discriminator: the same
///   bytes are missing either way.
/// * **a path alias** (`@app/components`, `~lib/foo`, a bare `components/Button` under a
///   `baseUrl`) pointing at first-party source, which Import Lens does not measure ([ADR-0004]). It
///   is not a gap and flags nothing.
///
/// **The target need not sit inside the workspace root** (unlike the config, see
/// [`find_workspace_config`]): an existing file outside `node_modules` is first-party wherever it
/// sits, and opening one package of a monorepo with a `"@shared/*": ["../shared/*"]` alias is
/// ordinary. The `node_modules` test is the only bound the target needs.
///
/// Residual limits (docs/known-issues.md A1 to A3). All but the last land on floor:
///
/// * an alias a Vite / webpack / Rollup config computes in a way a static read cannot follow (a
///   variable or a helper of its own, see [`bundler_aliases`]);
/// * an alias whose target file does not exist (the pattern matching is not evidence; the file is);
/// * a `references` graph wider than [`MAX_REACHABLE_ALIAS_CONFIGS`], whose tail is not asked;
/// * because every reachable table is asked, an alias defined only in `tsconfig.node.json` also
///   resolves for a document governed by `tsconfig.app.json`. It errs toward "flag nothing" and
///   cannot invent a number: the target still exists outside `node_modules`.
pub struct FirstPartySourceProbe<'a> {
    workspace_root: &'a Path,
    active_document_path: &'a Path,
    /// The workspace's alias tables, one resolver each: built on the first specifier that needs
    /// them, reused by every later one, dropped with the probe. `None` means the project has no
    /// config to read.
    alias_resolvers: OnceCell<Option<Vec<Resolver>>>,
}

impl<'a> FirstPartySourceProbe<'a> {
    pub fn new(workspace_root: &'a Path, active_document_path: &'a Path) -> Self {
        Self {
            workspace_root,
            active_document_path,
            alias_resolvers: OnceCell::new(),
        }
    }

    /// Whether `specifier` maps, through **any** alias table this workspace reaches, to a file that
    /// exists outside `node_modules`.
    pub fn resolves_to_first_party_source(&self, specifier: &str) -> bool {
        let Some(directory) = self.active_document_path.parent() else {
            return false;
        };
        let Some(alias_resolvers) = self.alias_resolvers().as_ref() else {
            // No `tsconfig.json` / `jsconfig.json` between the document and the workspace root:
            // no positive evidence is possible, so the specifier is not an alias.
            return false;
        };

        // Any reachable table that maps the specifier to first-party source settles it; no
        // project is chosen.
        alias_resolvers.iter().any(|resolver| {
            resolver
                .resolve(directory, specifier)
                .is_ok_and(|resolution| is_first_party_source(&resolution.full_path()))
        })
    }

    /// One `Resolver` per reachable config, built at most once per probe, so each specifier costs
    /// one warm `resolve`.
    fn alias_resolvers(&self) -> &Option<Vec<Resolver>> {
        self.alias_resolvers.get_or_init(|| {
            shared_resolvers().alias_resolvers(self.workspace_root, self.active_document_path)
        })
    }
}

/// The positive evidence itself: a file that exists and is not inside `node_modules` (where it
/// would be a package whose bytes the total owes).
fn is_first_party_source(path: &Path) -> bool {
    path.is_file()
        && !path
            .components()
            .any(|component| component.as_os_str() == "node_modules")
}

/// The config files whose `paths` / `baseUrl` make up the workspace's alias table, nearest first.
///
/// A JavaScript project declares its aliases only in `jsconfig.json`, and `oxc_resolver`'s own
/// discovery looks for `tsconfig.json` alone, so the config is named explicitly.
const ALIAS_CONFIG_FILE_NAMES: [&str; 2] = ["tsconfig.json", "jsconfig.json"];

/// The nearest `tsconfig.json` / `jsconfig.json` at or above the document, **bounded at the
/// workspace root**.
///
/// Unbounded, the walk reaches `C:\Users\<you>\tsconfig.json` and lets a config outside the project
/// decide what is first-party. A document outside the workspace root finds nothing, and its
/// specifiers land on floor (the direction ADR-0006 demands).
fn find_workspace_config(workspace_root: &Path, active_document_path: &Path) -> Option<PathBuf> {
    active_document_path
        .ancestors()
        .skip(1)
        .take_while(|directory| directory.starts_with(workspace_root))
        .find_map(|directory| {
            ALIAS_CONFIG_FILE_NAMES
                .iter()
                .map(|name| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
}

/// A cap on the `references` graph, so a config that references a hundred projects cannot turn one
/// unresolvable specifier into a hundred resolver builds. Real scaffolds have two or three.
///
/// A truncation and a residual limit (SRS FR-024a, known-issues A2): an alias defined only in the
/// 25th reachable project reads as a floor.
const MAX_REACHABLE_ALIAS_CONFIGS: usize = 24;

/// Every config whose `paths` table the workspace can reach from `config_file`: the config itself,
/// and every project in its `references`, transitively.
///
/// The `extends` chain needs no walk: `oxc_resolver` folds an extended config's
/// `compilerOptions.paths` into the extending config. A referenced project is a separate program
/// with its own alias table that nothing merges, so the daemon collects them all and asks each.
fn reachable_alias_configs(config_file: &Path) -> Vec<PathBuf> {
    let mut discovered = vec![config_file.to_path_buf()];
    let mut next = 0;

    while next < discovered.len() && discovered.len() < MAX_REACHABLE_ALIAS_CONFIGS {
        let current = discovered[next].clone();
        next += 1;

        for referenced in referenced_alias_configs(&current) {
            if !discovered.contains(&referenced) {
                discovered.push(referenced);
                if discovered.len() >= MAX_REACHABLE_ALIAS_CONFIGS {
                    break;
                }
            }
        }
    }

    discovered
}

/// The configs named in one config's `references`, as absolute config file paths, **each checked
/// on its own so one bad entry costs only its own table**. Do not use `resolve_tsconfig` with
/// `TsconfigReferences::Auto`: it fails if any referenced project cannot load, so one stale entry
/// (a deleted `tsconfig.node.json`) would make every alias in the workspace a floor.
///
/// The parse is oxc's own ([`TsConfig::parse`], JSONC-aware), so there is one source of truth
/// about what a tsconfig means. `references` are never inherited through `extends` (oxc's
/// `extend_tsconfig` does not copy them), so the config's own text is the whole list.
///
/// A config that cannot be read or parsed yields nothing: the tables that did load are still
/// evidence.
fn referenced_alias_configs(config_file: &Path) -> Vec<PathBuf> {
    let Some(directory) = config_file.parent() else {
        return Vec::new();
    };
    let Ok(source) = fs::read_to_string(config_file) else {
        return Vec::new();
    };
    let Ok(tsconfig) = TsConfig::parse(true, config_file, config_file, source) else {
        return Vec::new();
    };

    tsconfig
        .references
        .iter()
        .filter_map(|reference| referenced_config_file(directory, &reference.path))
        .collect()
}

/// What config a single `references` entry names, or `None` if it names nothing that exists.
///
/// The three spellings are `oxc_resolver`'s own (`Cache::get_tsconfig`) and TypeScript's: a path
/// to a **file** is that file; a path to a **directory** implies its `tsconfig.json`; anything
/// else gets `.json` appended.
fn referenced_config_file(directory: &Path, reference: &Path) -> Option<PathBuf> {
    let candidate = directory.normalize_with(reference);
    if candidate.is_file() {
        return Some(candidate);
    }
    if candidate.is_dir() {
        let implied = candidate.join("tsconfig.json");
        return implied.is_file().then_some(implied);
    }

    let with_extension = append_extension(&candidate, "json");
    with_extension.is_file().then_some(with_extension)
}

pub fn find_package_root(
    active_document_path: &Path,
    package_name: &str,
) -> Result<PathBuf, String> {
    validate_package_name(package_name)?;

    let mut current = active_document_path
        .parent()
        .ok_or_else(|| "active document path has no parent directory".to_owned())?
        .to_path_buf();
    let mut checked_paths: Vec<PathBuf> = Vec::new();

    loop {
        let package_root = current.join("node_modules").join(package_name);
        let package_json_path = package_root.join("package.json");

        if package_json_path.exists() {
            return Ok(package_root);
        }
        // Record only after the existence check so the common success path
        // allocates nothing; format the diagnostic lazily on failure below.
        checked_paths.push(package_json_path);

        if !current.pop() {
            break;
        }
    }

    let details = checked_paths
        .iter()
        .map(|path| format!("checked: {}", path.display()))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "package manifest not found for {package_name}; {details}"
    ))
}

fn validate_package_name(package_name: &str) -> Result<(), String> {
    let parts = package_name.split('/').collect::<Vec<_>>();
    let is_valid = if package_name.starts_with('@') {
        parts.len() == 2
            && parts[0].len() > 1
            && is_safe_package_segment(parts[0])
            && is_safe_package_segment(parts[1])
    } else {
        parts.len() == 1 && is_safe_package_segment(parts[0])
    };

    if is_valid {
        return Ok(());
    }

    Err(format!("unsafe package name: {package_name}"))
}

fn is_safe_package_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.contains('\\')
        && !segment.contains(':')
}

fn entry_matches_manifest_esm_field(manifest: &PackageManifest, entry_path: &Path) -> bool {
    ["module", "browser"]
        .iter()
        .filter_map(|field| manifest.json.get(field).and_then(Value::as_str))
        .filter_map(|relative| resolve_manifest_file_candidate(manifest, relative).ok())
        .any(|candidate| candidate == entry_path)
}

fn classify_resolved_entry(
    manifest: &PackageManifest,
    entry_path: PathBuf,
    resolver_is_cjs: bool,
) -> (PathBuf, bool) {
    let is_cjs = resolved_entry_is_commonjs(manifest, &entry_path, resolver_is_cjs);
    (entry_path, is_cjs)
}

fn resolved_entry_is_commonjs(
    manifest: &PackageManifest,
    entry_path: &Path,
    resolver_is_cjs: bool,
) -> bool {
    let Some(extension) = path_extension(entry_path) else {
        return resolver_is_cjs;
    };

    if matches!(extension, "cjs" | "cts") {
        return true;
    }
    if matches!(extension, "mjs" | "mts") {
        return false;
    }
    if matches!(extension, "ts" | "tsx" | "jsx" | "json") {
        return false;
    }
    if entry_matches_manifest_esm_field(manifest, entry_path) {
        return false;
    }
    if entry_matches_exports_condition(manifest, entry_path, &["browser", "import"]) {
        return false;
    }
    if entry_matches_exports_condition(manifest, entry_path, &["require"]) {
        return true;
    }

    match package_type(&manifest.json) {
        Some("module") => return false,
        Some("commonjs") => return true,
        _ => {}
    }

    resolver_is_cjs || entry_matches_manifest_main_field(manifest, entry_path) || extension == "js"
}

fn path_extension(path: &Path) -> Option<&str> {
    path.extension().and_then(|extension| extension.to_str())
}

fn entry_matches_manifest_main_field(manifest: &PackageManifest, entry_path: &Path) -> bool {
    manifest
        .json
        .get("main")
        .and_then(Value::as_str)
        .and_then(|relative| resolve_manifest_file_candidate(manifest, relative).ok())
        .is_some_and(|candidate| candidate == entry_path)
}

fn package_type(package_json: &Value) -> Option<&str> {
    package_json.get("type").and_then(Value::as_str)
}

fn entry_matches_exports_condition(
    manifest: &PackageManifest,
    entry_path: &Path,
    conditions: &[&str],
) -> bool {
    manifest.json.get("exports").is_some_and(|exports| {
        exports_condition_points_to_entry(exports, manifest, entry_path, conditions)
    })
}

fn exports_condition_points_to_entry(
    value: &Value,
    manifest: &PackageManifest,
    entry_path: &Path,
    conditions: &[&str],
) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };

    map.iter().any(|(key, child)| {
        if conditions.contains(&key.as_str())
            && exports_target_points_to_entry(child, manifest, entry_path)
        {
            return true;
        }

        exports_condition_points_to_entry(child, manifest, entry_path, conditions)
    })
}

fn exports_target_points_to_entry(
    value: &Value,
    manifest: &PackageManifest,
    entry_path: &Path,
) -> bool {
    match value {
        Value::String(target) => resolve_manifest_file_candidate(manifest, target)
            .is_ok_and(|candidate| candidate == entry_path),
        Value::Array(items) => items
            .iter()
            .any(|item| exports_target_points_to_entry(item, manifest, entry_path)),
        Value::Object(map) => map
            .values()
            .any(|item| exports_target_points_to_entry(item, manifest, entry_path)),
        _ => false,
    }
}

fn resolve_manifest_file_candidate(
    manifest: &PackageManifest,
    relative: &str,
) -> Result<PathBuf, String> {
    let candidate = resolve_file_candidate(&manifest.root.join(relative))?;
    normalize_existing_path(&candidate)
}

fn resolve_file_candidate(candidate: &Path) -> Result<PathBuf, String> {
    let candidates = [
        candidate.to_path_buf(),
        append_extension(candidate, "js"),
        append_extension(candidate, "mjs"),
        append_extension(candidate, "cjs"),
        candidate.join("index.js"),
        candidate.join("index.mjs"),
        candidate.join("index.cjs"),
    ];

    let found_path = candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            let details = candidates
                .iter()
                .map(|path| format!("candidate: {}", path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "package entry not found near {}; {details}",
                candidate.display()
            )
        })?;

    Ok(found_path)
}

/// The runtime resolvers for installed packages, sharing one `oxc_resolver` FS cache across
/// requests (Component and Client use identical options, so they share a resolver; Server has its
/// own).
pub struct ResolverSet {
    browser: Resolver,
    server: Resolver,
}

impl ResolverSet {
    fn new() -> Self {
        let browser = Resolver::new(resolve_options(ImportRuntime::Component));
        // clone_with_options shares the same Arc<Cache>, so all runtimes reuse
        // one set of memoized (option-independent) filesystem facts.
        let server = browser.clone_with_options(resolve_options(ImportRuntime::Server));
        Self { browser, server }
    }

    pub fn resolver(&self, runtime: ImportRuntime) -> &Resolver {
        match runtime {
            ImportRuntime::Component | ImportRuntime::Client => &self.browser,
            ImportRuntime::Server => &self.server,
        }
    }

    /// The resolvers that read the workspace's alias tables (one per config reachable from the
    /// nearest one), used only by [`FirstPartySourceProbe::resolves_to_first_party_source`].
    ///
    /// Never use them to find package entries: a `paths` entry can shadow a real package name, and
    /// a measurement must be of what the package manager installed.
    ///
    /// Built fresh once per request ([`FirstPartySourceProbe`] owns them for one response): longer
    /// would cache a miss, per specifier would cost a resolver and a JSONC parse per config each.
    ///
    /// **The `references` walk is not memoized either.** A memo would go stale on an edit no
    /// watcher reports (a referenced config outside the workspace folder, or one created after the
    /// walk dropped it as missing), and the walk is cheap beside the resolvers built from it: about
    /// 0.13 ms for three configs against about 2 ms for the probe (release build, Windows).
    ///
    /// Each alias resolver holds its own oxc FS cache: oxc memoizes a manually configured tsconfig
    /// in one slot (the cache entry for `/`) whatever the config path, so two configs sharing a
    /// cache would answer with whichever loaded first.
    ///
    /// A Vite, webpack or Rollup config is an alias table too ([`bundler_aliases`]), one resolver
    /// each, read on the same per-request terms.
    fn alias_resolvers(
        &self,
        workspace_root: &Path,
        active_document_path: &Path,
    ) -> Option<Vec<Resolver>> {
        let mut resolvers = find_workspace_config(workspace_root, active_document_path)
            .map(|config_file| {
                reachable_alias_configs(&config_file)
                    .iter()
                    .map(|config| Resolver::new(alias_resolve_options(config)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        resolvers.extend(
            bundler_aliases::alias_tables(workspace_root, active_document_path)
                .into_iter()
                .map(|aliases| Resolver::new(bundler_alias_resolve_options(aliases))),
        );
        (!resolvers.is_empty()).then_some(resolvers)
    }
}

/// Resolution options for ONE alias table: that config, handed over **explicitly**.
///
/// `TsconfigDiscovery::Manual`, not `Auto`: `Auto` only applies a config that claims the importing
/// document through `files` / `include` / `exclude`, and TypeScript's default `include` claims no
/// `.vue`, `.svelte` or `.astro` file. `Manual` applies `paths` as a property of the project.
///
/// `TsconfigReferences::Disabled` buys **immunity to a broken reference**: under `Auto`, oxc also
/// loads every project in `references` and fails the whole load if one cannot be read, so a config
/// with a good `paths` table that lists a deleted `tsconfig.node.json` would resolve nothing.
/// [`reachable_alias_configs`] walks the references itself, so nothing is lost. An `extends` chain
/// still folds in automatically.
///
/// `.vue`, `.svelte` and `.astro` join the extensions because aliases in those projects routinely
/// point at a component file (`@app/Button` → `src/Button.vue`). This only widens what counts as
/// first-party source, and this resolver never picks a package entry, so no measurement can reach
/// these extensions.
fn alias_resolve_options(config_file: &Path) -> ResolveOptions {
    ResolveOptions {
        tsconfig: Some(TsconfigDiscovery::Manual(TsconfigOptions {
            config_file: config_file.to_path_buf(),
            references: TsconfigReferences::Disabled,
        })),
        extensions: alias_target_extensions(),
        ..resolve_options(ImportRuntime::Component)
    }
}

/// Resolution options for ONE bundler config's alias table. Its keys match the way webpack and
/// `@rollup/plugin-alias` match them, which is oxc's own alias rule: the key itself or the key
/// followed by `/`, and a trailing `$` for an exact match only.
fn bundler_alias_resolve_options(aliases: Vec<(String, PathBuf)>) -> ResolveOptions {
    ResolveOptions {
        alias: aliases
            .into_iter()
            .map(|(key, target)| {
                (
                    key,
                    vec![AliasValue::Path(target.to_string_lossy().into_owned())],
                )
            })
            .collect(),
        extensions: alias_target_extensions(),
        ..resolve_options(ImportRuntime::Component)
    }
}

fn alias_target_extensions() -> Vec<String> {
    let mut extensions = module_extensions();
    extensions.extend([".vue", ".svelte", ".astro"].map(str::to_owned));
    extensions
}

static SHARED_RESOLVERS: OnceLock<RwLock<Arc<ResolverSet>>> = OnceLock::new();

fn resolver_slot() -> &'static RwLock<Arc<ResolverSet>> {
    SHARED_RESOLVERS.get_or_init(|| RwLock::new(Arc::new(ResolverSet::new())))
}

pub fn shared_resolvers() -> Arc<ResolverSet> {
    resolver_slot()
        .read()
        .map(|guard| Arc::clone(&guard))
        .unwrap_or_else(|_| Arc::new(ResolverSet::new()))
}

/// Publishes a fresh `ResolverSet` (empty cache, empty alias-config-graph memo). In-flight
/// resolutions keep their `Arc` snapshot and finish against the old cache, so this is safe while
/// background resolutions run, unlike oxc's in-place `clear_cache`, which is documented as unsafe
/// against concurrent resolution.
///
/// Called on a `node_modules` change and on a `tsconfig.json` / `jsconfig.json` edit
/// (`service::invalidate_workspace_config_paths`), so an added `paths` entry takes effect.
pub fn invalidate_shared_resolvers() {
    if let Ok(mut guard) = resolver_slot().write() {
        *guard = Arc::new(ResolverSet::new());
    }
}

// Shared with the engine so its resolution configuration cannot drift from the direct resolver's.
pub(crate) fn resolve_options(runtime: ImportRuntime) -> ResolveOptions {
    match runtime {
        ImportRuntime::Component | ImportRuntime::Client => ResolveOptions {
            alias_fields: vec![vec!["browser".to_owned()]],
            condition_names: vec![
                "browser".to_owned(),
                "module".to_owned(),
                "import".to_owned(),
                "default".to_owned(),
            ],
            extensions: module_extensions(),
            extension_alias: extension_aliases(),
            main_fields: main_fields(runtime),
            module_type: true,
            node_path: false,
            ..ResolveOptions::default()
        },
        ImportRuntime::Server => ResolveOptions {
            alias_fields: Vec::new(),
            condition_names: vec![
                "node".to_owned(),
                "server".to_owned(),
                "module".to_owned(),
                "import".to_owned(),
                "default".to_owned(),
            ],
            extensions: module_extensions(),
            extension_alias: extension_aliases(),
            main_fields: main_fields(runtime),
            module_type: true,
            node_path: false,
            ..ResolveOptions::default()
        },
    }
}

/// How a bare stylesheet `@import "pkg"` names a package: the `style` condition and `style` main
/// field, the profile Vite and postcss-import share. Never the JavaScript profile: it has no `.css`
/// extension and would answer `pkg/base` with `pkg/base.js`.
pub(crate) fn stylesheet_resolve_options() -> ResolveOptions {
    ResolveOptions {
        condition_names: vec!["style".to_owned(), "default".to_owned()],
        extensions: vec![".css".to_owned()],
        main_fields: vec!["style".to_owned()],
        node_path: false,
        ..ResolveOptions::default()
    }
}

fn main_fields(runtime: ImportRuntime) -> Vec<String> {
    profile_entry_fields(runtime)
        .iter()
        .map(|field| (*field).to_owned())
        .collect()
}

fn module_extensions() -> Vec<String> {
    [
        ".js", ".mjs", ".cjs", ".jsx", ".ts", ".tsx", ".mts", ".cts", ".json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn extension_aliases() -> Vec<(String, Vec<String>)> {
    [
        (".js", [".ts", ".tsx", ".js"].as_slice()),
        (".mjs", [".mts", ".mjs"].as_slice()),
        (".cjs", [".cts", ".cjs"].as_slice()),
        (".jsx", [".tsx", ".jsx"].as_slice()),
    ]
    .into_iter()
    .map(|(extension, aliases)| {
        (
            extension.to_owned(),
            aliases.iter().map(|alias| (*alias).to_owned()).collect(),
        )
    })
    .collect()
}

pub(crate) fn resolve_module_path(
    resolver: &Resolver,
    from_path: &Path,
    specifier: &str,
) -> Result<ResolvedModulePath, String> {
    let resolution = resolver
        .resolve(from_path, specifier)
        .map_err(|error| error.to_string())?;

    if resolution.query().is_some() || resolution.fragment().is_some() {
        return Err(format!(
            "resolved module '{specifier}' contains unsupported query or fragment"
        ));
    }

    let full_path = resolution.full_path().to_path_buf();
    if !full_path.is_file() {
        return Err(format!(
            "resolved module '{specifier}' is not a file: {}",
            full_path.display()
        ));
    }

    let is_cjs = resolution.module_type() == Some(ModuleType::CommonJs)
        || path_looks_cjs(&full_path.to_string_lossy());
    Ok(ResolvedModulePath {
        path: normalize_existing_path(&full_path)?,
        is_cjs,
    })
}

pub(crate) fn append_extension(candidate: &Path, extension: &str) -> PathBuf {
    let mut path = candidate.as_os_str().to_owned();
    path.push(".");
    path.push(extension);
    PathBuf::from(path)
}

pub(crate) fn normalize_existing_path(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve path {}: {error}", path.display()))
}

fn side_effects_mode(
    package_json: &Value,
    package_root: &Path,
    entry_path: &Path,
) -> SideEffectsMode {
    match package_json.get("sideEffects") {
        Some(Value::Bool(false)) => SideEffectsMode::False,
        Some(Value::Bool(true)) => SideEffectsMode::True,
        Some(Value::Array(patterns)) => side_effects_array_mode(patterns, package_root, entry_path),
        // A string is a single glob, a first-class form in the spec (§7.4), read like an array.
        Some(pattern @ Value::String(_)) => {
            side_effects_array_mode(std::slice::from_ref(pattern), package_root, entry_path)
        }
        Some(_) => SideEffectsMode::Unknown,
        None => SideEffectsMode::Missing,
    }
}

/// The glob form, read exactly as the pattern list Rolldown gets, then matched: `.any()` over the
/// patterns is the answer for every list, including degenerate ones ([ADR-0002]: where we read the
/// metadata upstream reads, our answer is upstream's):
///
/// * **an empty array** is `SideEffects::Array(vec![])` upstream, and `check_side_effects_for`
///   answers `pats.iter().any(…)`, i.e. `false`: it means what `"sideEffects": false` means.
/// * **a non-string element** is dropped by `oxc_resolver` (`filter_map(JsonValue::as_str)`), the
///   parser Rolldown builds `SideEffects` from: `["index.js", 42]` is `["index.js"]`, and `[42]`
///   is `[]`.
///
/// Answering `Unknown` (side-effectful) for either would contradict the build the size came from.
/// `Unknown` is only for an entry path that cannot be canonicalized.
fn side_effects_array_mode(
    patterns: &[Value],
    package_root: &Path,
    entry_path: &Path,
) -> SideEffectsMode {
    let Some(entry) = normalized_side_effect_path(package_root, entry_path) else {
        return SideEffectsMode::Unknown;
    };

    SideEffectsMode::Array {
        entry_matches: patterns
            .iter()
            .filter_map(Value::as_str)
            .any(|pattern| side_effects_pattern_matches(pattern, &entry)),
    }
}

/// The entry's path **relative to its package root**: the string a `sideEffects` glob is matched
/// against, and the same string Rolldown derives
/// (`resolved_id.id.relative_path(package_json.realpath().parent())`). Both sides must agree on the
/// path, not merely the matcher.
///
/// **Both paths are canonicalized and the root is stripped.** Never derive it by scanning for a
/// `node_modules` component: a workspace-linked package's real path has none (pnpm/npm/yarn link
/// `node_modules/<name>` onto `packages/<name>`). Canonicalizing both sides makes the strip survive
/// a junction, a pnpm store link, and a Windows `\\?\` spelling on one side only.
///
/// An entry that canonicalizes outside its root (a `dist/` that is a junction or symlink out of the
/// package) gets the `../`-led path Rolldown's own `relative_path` computes, so the glob is matched
/// against what the build matched, not against a path the build never saw. `None` means a path
/// could not be canonicalized, the one case for [`SideEffectsMode::Unknown`].
fn normalized_side_effect_path(package_root: &Path, entry_path: &Path) -> Option<String> {
    let root = fs::canonicalize(package_root).ok()?;
    let entry = fs::canonicalize(entry_path).ok()?;
    let relative = match entry.strip_prefix(&root) {
        Ok(inside) => inside.to_path_buf(),
        Err(_) => rolldown_common::ModuleId::new(entry.to_string_lossy().into_owned())
            .relative_path(&root),
    };

    let joined = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");

    (!joined.is_empty()).then_some(joined)
}

/// **The matcher is Rolldown's own** (`fast_glob::glob_match`, which `rolldown_utils` and
/// `rolldown_common` match `sideEffects` with), and so is the pattern normalisation around it. Two
/// glob engines reading one array can disagree, and Rolldown owns retention (FR-021); [ADR-0002]:
/// where upstream vendors a component, use that component.
///
/// The normalisation copies `rolldown_common::side_effects::glob_match_with_normalized_pattern`
/// (`pub(crate)` there): a pattern with no separator (`fx.js`) or a `./` prefix matches at any
/// depth, which is what makes `["*.css"]` mean what bundlers take it to mean. Copy it; do not
/// improve on it.
///
/// `path` is the package-relative path, forward-slashed by [`normalized_side_effect_path`].
fn side_effects_pattern_matches(pattern: &str, path: &str) -> bool {
    let trimmed = pattern.trim_start_matches("./");
    let mut normalized = if trimmed.len() != pattern.len() || !trimmed.contains('/') {
        format!("**/{trimmed}")
    } else {
        trimmed.to_owned()
    };
    // `sideEffects` is a positive allowlist, so `!lib/fx.js` names a file; unescaped, fast_glob
    // would read the `!` as a negation and match every other path.
    if normalized.starts_with('!') {
        normalized.insert(0, '\\');
    }

    fast_glob::glob_match(
        normalized.as_bytes(),
        path.trim_start_matches("./").as_bytes(),
    )
}

fn subpath_for_request(request: &ImportRequest) -> Option<&str> {
    request
        .specifier
        .strip_prefix(&request.package_name)
        .and_then(|value| value.strip_prefix('/'))
}

fn path_looks_cjs(path: &str) -> bool {
    path.ends_with(".cjs") || path.ends_with(".cts")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ConfigFixture {
        root: PathBuf,
    }

    impl ConfigFixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "il-config-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::remove_dir_all(&root).ok();
            fs::create_dir_all(&root).expect("fixture root");
            Self { root }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
            fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for ConfigFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    /// The fallback reached when oxc cannot resolve a package with no `exports` map searches the
    /// entry fields in the resolver's own order for the runtime: a browser runtime prefers
    /// `browser`, a server runtime never reads it.
    #[test]
    fn the_legacy_fallback_searches_entry_fields_in_the_resolvers_order() {
        let fixture = ConfigFixture::new("legacy-fallback-order");
        fixture.write("browser.js", "export const side = 'browser';\n");
        fixture.write("module.js", "export const side = 'module';\n");
        fixture.write("main.js", "exports.side = 'main';\n");
        let manifest = |json: Value| PackageManifest {
            root: fixture.root.clone(),
            json,
        };
        let request = |runtime| ImportRequest {
            specifier: "pkg".to_owned(),
            package_name: "pkg".to_owned(),
            version: "1.0.0".to_owned(),
            named: Vec::new(),
            import_kind: crate::ipc::protocol::ImportKind::Namespace,
            runtime,
        };
        let entry_name = |manifest: &PackageManifest, runtime| {
            let (entry, _) = resolve_legacy_fallback(manifest, &request(runtime), "oxc failed")
                .expect("the fallback resolves a declared entry");
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .expect("entry file name")
        };

        let all_three = manifest(serde_json::json!({
            "name": "pkg",
            "version": "1.0.0",
            "browser": "./browser.js",
            "module": "./module.js",
            "main": "./main.js",
        }));
        assert_eq!(entry_name(&all_three, ImportRuntime::Client), "browser.js");
        assert_eq!(
            entry_name(&all_three, ImportRuntime::Component),
            "browser.js"
        );
        assert_eq!(entry_name(&all_three, ImportRuntime::Server), "module.js");

        let browser_and_main = manifest(serde_json::json!({
            "name": "pkg",
            "version": "1.0.0",
            "browser": "./browser.js",
            "main": "./main.js",
        }));
        assert_eq!(
            entry_name(&browser_and_main, ImportRuntime::Server),
            "main.js",
            "a server import never reads `browser`"
        );
    }

    /// One probe per ask, as two requests would, so
    /// `creating_the_alias_target_lifts_the_floor_without_an_invalidation` fails if anyone
    /// memoizes the resolvers across requests.
    fn resolves_to_first_party_source(
        workspace_root: &Path,
        active_document_path: &Path,
        specifier: &str,
    ) -> bool {
        FirstPartySourceProbe::new(workspace_root, active_document_path)
            .resolves_to_first_party_source(specifier)
    }

    /// The normalisation around `fast_glob` (the half of the matcher that is ours), pinned against
    /// the shapes `sideEffects` is really written in. Dropping either the no-separator or the `./`
    /// rule stops a package-root pattern matching a package-root file.
    #[test]
    fn a_side_effect_pattern_is_matched_the_way_rolldown_matches_it() {
        // The everyday declaration says nothing about a JavaScript entry.
        assert!(!side_effects_pattern_matches("**/*.css", "dist/index.js"));
        assert!(side_effects_pattern_matches("**/*.css", "dist/styles.css"));

        // `**/` matches ZERO directories: a package-root stylesheet matches too.
        assert!(side_effects_pattern_matches("**/*.css", "styles.css"));

        // No separator, and `./`-prefixed: both are depth-independent (the shape webpack's docs
        // use).
        assert!(side_effects_pattern_matches("fx.js", "fx.js"));
        assert!(side_effects_pattern_matches("fx.js", "lib/deep/fx.js"));
        assert!(side_effects_pattern_matches("./fx.js", "fx.js"));
        assert!(side_effects_pattern_matches("*.css", "styles.css"));

        // A pattern that DOES carry a separator is anchored at the package root.
        assert!(side_effects_pattern_matches("dist/*.js", "dist/index.js"));
        assert!(!side_effects_pattern_matches("dist/*.js", "src/index.js"));
        // `*` does not cross a separator.
        assert!(!side_effects_pattern_matches(
            "dist/*.js",
            "dist/deep/index.js"
        ));

        // Braces are the matcher's own, not a hand-rolled expansion pass.
        assert!(side_effects_pattern_matches("**/*.{css,scss}", "a/b.scss"));
        assert!(!side_effects_pattern_matches("**/*.{css,scss}", "a/b.js"));
    }

    /// The alias-table walk must stop at the workspace root: a stray `paths` entry in a home
    /// directory `tsconfig.json` would silently bless a missing dependency as an alias.
    #[test]
    fn the_alias_config_search_stops_at_the_workspace_root() {
        let fixture = ConfigFixture::new("bounded");
        // A tsconfig ABOVE the workspace. Nothing in the project put it there.
        fixture.write(
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"@app/*":["elsewhere/*"]}}}"#,
        );
        let workspace_root = fixture.root.join("workspace");
        fs::create_dir_all(workspace_root.join("src")).expect("workspace src");

        assert_eq!(
            find_workspace_config(
                &workspace_root,
                &workspace_root.join("src").join("index.ts")
            ),
            None,
            "a config outside the workspace must never supply the workspace's alias table"
        );
    }

    /// A JavaScript project declares its aliases only in `jsconfig.json`, which `oxc_resolver`'s
    /// own discovery does not look for.
    #[test]
    fn the_alias_config_search_finds_a_jsconfig() {
        let fixture = ConfigFixture::new("jsconfig");
        let config = fixture.write(
            "jsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );
        fs::create_dir_all(fixture.root.join("src")).expect("src");

        assert_eq!(
            find_workspace_config(&fixture.root, &fixture.root.join("src").join("index.js")),
            Some(config),
        );
    }

    /// The nearest config wins, exactly as TypeScript resolves one: a monorepo package's own
    /// tsconfig, not the repo root's.
    #[test]
    fn the_alias_config_search_prefers_the_nearest_config() {
        let fixture = ConfigFixture::new("nearest");
        fixture.write("tsconfig.json", r#"{"compilerOptions":{"baseUrl":"."}}"#);
        let nested = fixture.write(
            "packages/app/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );

        assert_eq!(
            find_workspace_config(
                &fixture.root,
                &fixture
                    .root
                    .join("packages")
                    .join("app")
                    .join("src")
                    .join("index.ts"),
            ),
            Some(nested),
        );
    }

    /// With no config there is no positive evidence, so an uninstalled bare specifier is a floor.
    #[test]
    fn a_specifier_is_not_first_party_without_a_config() {
        let fixture = ConfigFixture::new("no-config");
        fixture.write("src/components.ts", "export const Button = 1;\n");

        assert!(!resolves_to_first_party_source(
            &fixture.root,
            &fixture.root.join("src").join("index.ts"),
            "@app/components",
        ));
    }

    /// `reachable_alias_configs` includes every project in `references`: the solution-style
    /// scaffold keeps its aliases in a referenced project, and its root config has none.
    #[test]
    fn the_reachable_configs_include_every_referenced_project() {
        let fixture = ConfigFixture::new("references");
        let root = fixture.write(
            "tsconfig.json",
            r#"{"files":[],"references":[{"path":"./tsconfig.node.json"},{"path":"./tsconfig.app.json"}]}"#,
        );
        let node = fixture.write("tsconfig.node.json", r#"{"include":["vite.config.*"]}"#);
        let app = fixture.write(
            "tsconfig.app.json",
            r#"{"include":["src/**/*"],"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );

        let mut reachable = reachable_alias_configs(&root);
        reachable.sort();
        let mut expected = vec![root, node, app];
        expected.sort();

        assert_eq!(reachable, expected);
    }

    /// A JavaScript-only Vite project has no `jsconfig.json`; its Vite config is its alias table.
    #[test]
    fn a_javascript_vite_project_declares_its_aliases_in_the_vite_config_alone() {
        let fixture = ConfigFixture::new("vite-alias");
        fixture.write("src/components/Button.vue", "<template />\n");
        fixture.write(
            "vite.config.js",
            r#"import { fileURLToPath, URL } from "node:url";
            export default { resolve: { alias: { components: fileURLToPath(new URL("./src/components", import.meta.url)) } } };"#,
        );
        let document = fixture.root.join("src").join("main.js");

        assert!(
            resolves_to_first_party_source(&fixture.root, &document, "components/Button"),
            "the only alias table is the Vite config, and its target exists outside node_modules"
        );
        assert!(
            !resolves_to_first_party_source(&fixture.root, &document, "components/Missing"),
            "an aliased path to no file is still no evidence"
        );
        assert!(
            !resolves_to_first_party_source(&fixture.root, &document, "lodash"),
            "a specifier no table maps is a package that is not installed"
        );
    }

    /// **An alias target above the workspace root is first-party source.** A monorepo opened at
    /// `packages/web`, whose `paths` reach `../shared`, is ordinary, and the sibling's source ships
    /// no package bytes. Unlike the config search, the target is bounded only by the
    /// `node_modules` test.
    #[test]
    fn an_alias_target_above_the_workspace_root_is_first_party_source() {
        let fixture = ConfigFixture::new("monorepo-alias");
        fixture.write("packages/shared/ui.ts", "export const Button = 1;\n");
        fixture.write("packages/web/src/local.ts", "export const local = 1;\n");
        fixture.write(
            "packages/web/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@shared/*":["../shared/*"],"@app/*":["src/*"]}}}"#,
        );

        let workspace_root = fixture.root.join("packages").join("web");
        let document = workspace_root.join("src").join("index.ts");

        assert!(
            resolves_to_first_party_source(&workspace_root, &document, "@shared/ui"),
            "the alias target exists and is not inside node_modules: it is the user's own source, \
             wherever it sits, and a total that omits it omits nothing"
        );
        assert!(
            resolves_to_first_party_source(&workspace_root, &document, "@app/local"),
            "test setup: the same config's in-workspace alias must still resolve"
        );
        assert!(
            !resolves_to_first_party_source(&workspace_root, &document, "@shared/missing"),
            "and the bound that matters still holds: a target that does not exist is no evidence"
        );
    }

    /// **The floor is not sticky.** An import written before the file it points at is a floor, and
    /// creating that file must lift it on the next request with no restart and no invalidation
    /// message. `oxc_resolver` negative-caches a missing path, so the alias resolvers must not
    /// outlive a request.
    #[test]
    fn creating_the_alias_target_lifts_the_floor_without_an_invalidation() {
        let fixture = ConfigFixture::new("sticky-floor");
        fixture.write(
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );
        fixture.write("src/index.ts", "export const app = 1;\n");
        let document = fixture.root.join("src").join("index.ts");

        // The developer writes the import before the component exists: a floor, and the daemon
        // has now looked at a path that does not exist.
        assert!(
            !resolves_to_first_party_source(&fixture.root, &document, "@app/components"),
            "test setup: the alias target does not exist yet, so there is no positive evidence"
        );

        // They create it. No restart, no `invalidate_shared_resolvers`, no watcher event.
        fixture.write("src/components.ts", "export const Button = 1;\n");

        assert!(
            resolves_to_first_party_source(&fixture.root, &document, "@app/components"),
            "creating the alias target must lift the floor. It did not: the miss was cached in the \
             memoized resolver's filesystem cache, so the file stayed a floor for the daemon's life \
             - never cached, never persisted, and refused a verdict by `importlens check`"
        );
    }

    /// **The `references` graph is not sticky either.** A referenced project that did not exist at
    /// the first walk is dropped from it, and creating it fires no watcher event when it sits
    /// outside the workspace folder, so the next request must walk again.
    #[test]
    fn creating_a_referenced_project_lifts_the_floor_without_an_invalidation() {
        let fixture = ConfigFixture::new("sticky-references");
        fixture.write(
            "tsconfig.json",
            r#"{"files":[],"references":[{"path":"./tsconfig.app.json"}]}"#,
        );
        fixture.write(
            "src/components.ts",
            "export const Button = 1;
",
        );
        let document = fixture.root.join("src").join("index.ts");

        assert!(
            !resolves_to_first_party_source(&fixture.root, &document, "@app/components"),
            "test setup: the project declaring the alias does not exist yet"
        );

        fixture.write(
            "tsconfig.app.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );

        assert!(
            resolves_to_first_party_source(&fixture.root, &document, "@app/components"),
            "the referenced project now exists and maps the alias to first-party source"
        );
    }

    /// Guards `TsconfigReferences::Disabled` (see [`alias_resolve_options`]): under `Auto`, one
    /// unreadable reference fails the whole load, so a config with a good `paths` table would
    /// resolve nothing.
    #[test]
    fn a_dangling_reference_does_not_silence_the_config_that_declares_it() {
        let fixture = ConfigFixture::new("dangling-self");
        // A real alias table, and a `references` entry pointing at a deleted project.
        fixture.write(
            "tsconfig.json",
            r#"{"references":[{"path":"./tsconfig.deleted.json"}],"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );
        fixture.write("src/components.ts", "export const Button = 1;\n");

        assert!(
            resolves_to_first_party_source(
                &fixture.root,
                &fixture.root.join("src").join("index.ts"),
                "@app/components",
            ),
            "a stale `references` entry must cost that project's table and nothing else. Loading \
             the references with this config would fail the whole load, and every alias in the \
             workspace would become a floor"
        );
    }

    /// A dangling reference must not silence its siblings either: `referenced_alias_configs` checks
    /// each entry on its own, so the `tsconfig.app.json` beside a deleted one is still asked.
    #[test]
    fn a_dangling_reference_does_not_silence_its_siblings() {
        let fixture = ConfigFixture::new("dangling-sibling");
        let root = fixture.write(
            "tsconfig.json",
            r#"{"files":[],"references":[{"path":"./tsconfig.deleted.json"},{"path":"./tsconfig.app.json"}]}"#,
        );
        let app = fixture.write(
            "tsconfig.app.json",
            r#"{"include":["src/**/*"],"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );
        fixture.write("src/components.ts", "export const Button = 1;\n");

        assert_eq!(
            reachable_alias_configs(&root),
            vec![root.clone(), app],
            "the reference that does not exist is skipped; the one beside it is still enumerated"
        );
        assert!(
            resolves_to_first_party_source(
                &fixture.root,
                &fixture.root.join("src").join("index.ts"),
                "@app/components",
            ),
            "one stale `references` entry must not take every alias table in the workspace down \
             with it"
        );
    }
}
