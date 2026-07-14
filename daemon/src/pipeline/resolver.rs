use crate::cache::key::CacheIdentity;
use crate::ipc::protocol::{ImportRequest, ImportRuntime};
use oxc_resolver::{
    ModuleType, PathUtil, ResolveOptions, Resolver, TsConfig, TsconfigDiscovery, TsconfigOptions,
    TsconfigReferences,
};
use serde_json::Value;
use std::{
    cell::OnceCell,
    collections::HashMap,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffectsMode {
    False,
    True,
    Array(SideEffectsPatterns),
    Missing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideEffectsPatterns {
    patterns: Vec<String>,
    entry_matches: bool,
}

impl SideEffectsMode {
    pub fn has_side_effects(&self) -> bool {
        match self {
            Self::False => false,
            Self::True | Self::Missing | Self::Unknown => true,
            Self::Array(patterns) => patterns.entry_matches,
        }
    }

    pub fn matching_paths<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
        let Self::Array(patterns) = self else {
            return Vec::new();
        };

        let mut matched_paths = Vec::new();
        for path in paths {
            let Some(normalized) = normalized_side_effect_path(path) else {
                continue;
            };
            if patterns
                .patterns
                .iter()
                .any(|pattern| side_effects_pattern_matches(pattern, &normalized))
            {
                let path = path.to_path_buf();
                if !matched_paths.contains(&path) {
                    matched_paths.push(path);
                }
            }
        }

        matched_paths
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Self::Array(_))
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
    let resolution = resolve_with_oxc(active_document_path, request);
    let (entry_path, is_cjs) = match resolution {
        Ok(resolved) => {
            let entry_path = resolved.entry_path;
            if subpath_for_request(request).is_none() {
                validate_declared_entry_resolution(&manifest, request.runtime)?;
            }
            let is_cjs = resolved_entry_is_commonjs(&manifest, &entry_path, resolved.is_cjs);
            (entry_path, is_cjs)
        }
        Err(error) => resolve_legacy_fallback(&manifest, request, &error)?,
    };

    Ok(ResolvedPackage {
        package_root: manifest.root,
        side_effects: side_effects_mode(&manifest.json, &entry_path),
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
    let side_effects = side_effects_mode(&package_json, &entry_path);

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

fn resolve_with_oxc(
    active_document_path: &Path,
    request: &ImportRequest,
) -> Result<ResolvedEntry, String> {
    let directory = active_document_path
        .parent()
        .ok_or_else(|| "active document path has no parent directory".to_owned())?;

    let resolvers = shared_resolvers();
    let resolved = resolve_module_path(
        resolvers.resolver(request.runtime),
        directory,
        &request.specifier,
    )?;

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

        return Err(format!(
            "failed to resolve package entry with oxc_resolver: {resolution_error}"
        ));
    }

    if let Some(sub) = subpath {
        return resolve_file_candidate(&manifest.root.join(sub))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    if let Some(module) = manifest.json.get("module").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(module))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    if let Some(browser) = manifest.json.get("browser").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(browser))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    if let Some(main) = manifest.json.get("main").and_then(Value::as_str) {
        return resolve_file_candidate(&manifest.root.join(main))
            .map(|path| classify_resolved_entry(manifest, path, false));
    }

    resolve_file_candidate(&manifest.root.join("index.js"))
        .map(|path| classify_resolved_entry(manifest, path, false))
}

fn validate_declared_entry_resolution(
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

    let resolvers = shared_resolvers();
    let resolver = resolvers.resolver(runtime);
    for (_, target) in &declared_entries {
        if resolve_manifest_target(resolver, &manifest.root, target).is_ok() {
            return Ok(());
        }
    }

    let (_, first_target) = declared_entries[0];
    resolve_file_candidate(&manifest.root.join(first_target)).map(|_| ())
}

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

/// Whether a specifier resolves — through the project's `tsconfig.json` / `jsconfig.json` `paths` /
/// `baseUrl` — to a real file **outside `node_modules`**: first-party source, and therefore a **path
/// alias** rather than a package.
///
/// **It is a REQUEST-scoped object, and both halves of that are load-bearing.**
///
/// *Scoped to a request*, because the alias resolvers it holds must not outlive one. Each `Resolver`
/// carries an `oxc_resolver` filesystem cache that negative-caches a miss, and the miss here is the
/// one answer that must never be cached: an import written *before* the file it points at is
/// correctly a floor, and creating that file has to lift it — with no daemon restart and no
/// invalidation message, because nothing watches first-party source and nothing can send one. A
/// memoized resolver made that floor **sticky for the daemon's life**
/// ([`ResolverSet::alias_config_graphs`], and the test that goes red without this). The probe is
/// built by the response that needs it and dropped with it, so no filesystem fact survives the query.
///
/// *Scoped to a REQUEST*, not to a specifier, because [`ResolverSet::alias_resolvers`] builds one
/// `Resolver` per reachable config, each with its own cold JSONC parse. Asking that question once per
/// specifier made the cost `O(aliased imports × reachable configs)`: on the create-vue shape (three
/// reachable configs) a 20-alias page component spent ~20 ms of the 50 ms NFR-002 interactive budget
/// inside the daemon alone, on every debounced keystroke. One probe per response builds the set once
/// and reuses it across the whole specifier loop — `O(reachable configs)` — and a warm resolver
/// answers the next specifier in tens of microseconds. The set is still built **lazily**: a document
/// whose every import is installed never asks, and pays nothing.
///
/// **The question is about the WORKSPACE'S ALIAS TABLE, not about the importing document.**
/// "Does this specifier map to first-party source?" is a property of the project's `paths` /
/// `baseUrl` and of the file those point at. Which document happens to contain the import decides
/// nothing — the same specifier in the same project means the same thing from a `.ts`, a `.vue`, a
/// `.svelte` or an `.astro` file.
///
/// It was keyed on the document, and that broke three of the six languages the extension activates
/// on. The first implementation used `resolve_file`, which drives `TsconfigDiscovery::Auto`:
/// oxc walks up to the nearest tsconfig **that claims the document through `files` / `include` /
/// `exclude`**, and returns `None` when none does. TypeScript's default `include` claims no `.vue`,
/// `.svelte` or `.astro` file — so for every Vue, Svelte and Astro user, **every file using a path
/// alias stayed a permanent floor**: never cached, never persisted, and refused a verdict by
/// `importlens check`. That is the exact regression the alias fix is named for, surviving inside
/// the fix.
///
/// So the config is located by [`find_workspace_config`] and handed to oxc explicitly
/// (`TsconfigDiscovery::Manual`), which applies its `paths` regardless of what `include` claims.
///
/// **And the nearest config alone is not the alias table**, in the literal create-vue / create-astro
/// scaffold: a root `tsconfig.json` that is nothing but `references`, with the real `paths` in a
/// referenced `tsconfig.app.json`. A resolver built on the nearest config and asked to *choose* a
/// project out of that list answered with whichever it picked — and the one that owns the `paths` is
/// not knowable from the document once `include` is discarded.
///
/// The answer is to stop asking oxc to CHOOSE a project at all, because the question is
/// document-independent: *does this specifier map, through **any** `paths` table the workspace
/// reaches, to a first-party file that exists?* So [`ResolverSet::alias_resolvers`] collects every
/// reachable table — the nearest config, everything in its `references` (transitively), and the
/// `extends` chain each of those folds in — and the specifier is tried against **each**, with
/// `TsconfigReferences::Disabled` so that one deleted `references` entry cannot silence the good
/// `paths` table of the config that lists it (the reason is written out at [`alias_resolve_options`],
/// and it is *not* the list-order story an earlier revision told here — that one was measured false
/// and retracted). One hit is positive evidence. The answer cannot depend on which document asks, nor
/// on the order the references happen to be listed in.
///
/// The discriminator stays **positive evidence**, never the absence of it: an alias is recognized
/// because it *resolves to a real file outside `node_modules`*, which is the thing that actually
/// makes its zero a fact. So the two errors are not symmetric. A specifier that resolves to nothing
/// — a typo, a stale import, a genuinely uninstalled dependency — is a **floor**, which refuses a
/// verdict; it can never be a silent pass on a total that is missing a whole package. That is the
/// direction ADR-0006 demands to fail in.
///
/// This is the one fact that tells the two kinds of "no `node_modules/<name>/package.json`" apart,
/// and they must never be conflated:
///
/// * **a package that is not installed.** Its bytes belong in the file's total and are missing from
///   it, so the total is a floor (SRS FR-024a, bullet 4). Whether the project *declared* it changes
///   nothing: `import _ from "lodash"` omits exactly the same bytes whether or not `package.json`
///   mentions lodash, so declaration is **not** the discriminator — an earlier attempt made it one
///   and had to narrow FR-024a to fit, blessing a typo'd import as an alias.
/// * **a specifier that is not a package at all** — a tsconfig / bundler **path alias**
///   (`@app/components`, `~lib/foo`, a bare `components/Button` under a `baseUrl`) pointing at
///   first-party source. Import Lens measures third-party imports ([ADR-0004]), so first-party code
///   contributes nothing to any total it reports, exactly like a relative import. It is not a gap,
///   and it must flag nothing.
///
/// Treating the second as the first made **every file that uses path aliases a permanent floor**.
/// Aliases are ordinary in real TypeScript projects.
///
/// **The target does NOT have to sit inside the workspace root.** A previous revision required that,
/// mirroring the bound [`find_workspace_config`] holds on the *config*, and the two are not the same
/// rule. A target that **exists** and is **not** inside `node_modules` is first-party source wherever
/// it sits — the project's own tsconfig says so — and it contributes no package bytes to any total
/// Import Lens reports, so it must flag nothing. With the bound, opening **one package of a
/// monorepo** (an ordinary way to open one) made every file using a cross-package alias
/// (`"@shared/*": ["../shared/*"]`) a permanent floor. The `node_modules` test is what stops a real
/// package being mistaken for source, and it is the only bound the target needs.
///
/// **No filesystem fact here outlives the request, and that is deliberate** (see [`ResolverSet`]):
/// the resolvers die with the probe, so an alias whose target did not exist *yet* — the import
/// written before the file it points at — stops being a floor on the next request, with no daemon
/// restart and no invalidation message. A memoized miss is a cached negative that nothing can lift,
/// which is the same defect class as the config the daemon read exactly once.
///
/// The residual limits, stated rather than papered over. All but the last land on **floor** —
/// conservative, never a wrong number:
///
/// * an alias declared **only** in a Vite / webpack / Rollup config, which the daemon does not read;
/// * an alias whose target file does not exist (the *pattern* matching is not evidence; the file is);
/// * a `references` graph wider than [`MAX_REACHABLE_ALIAS_CONFIGS`], whose tail is not asked;
/// * and the one that does **not** point at floor: because *every* reachable table is asked, an alias
///   defined only in `tsconfig.node.json` also resolves for a document governed by
///   `tsconfig.app.json`. That is inherent to a document-independent answer (nothing tells the daemon
///   which project owns a document once `include` is discarded — see [`alias_resolve_options`]), and
///   it errs toward "flag nothing" for a specifier that really is first-party source *somewhere* in
///   the workspace. It can never invent a number: the specifier still resolves to a file that exists
///   outside `node_modules`, which weighs nothing in either project.
pub struct FirstPartySourceProbe<'a> {
    workspace_root: &'a Path,
    active_document_path: &'a Path,
    /// The workspace's alias tables, one resolver each — built on the FIRST specifier that needs
    /// them (a document whose every import is installed builds none), reused by every specifier
    /// after it, and dropped with the probe. `Some(_)` holding an empty answer is impossible; `None`
    /// means the project has no config to read.
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
            // No `tsconfig.json` / `jsconfig.json` anywhere between the document and the workspace
            // root: the project has no alias table the daemon can read, so there is no positive
            // evidence to be had and the specifier is not an alias.
            return false;
        };

        // ANY reachable table that maps the specifier to first-party source settles it. There is no
        // "the" project to choose, and choosing one is what broke the create-vue scaffold.
        alias_resolvers.iter().any(|resolver| {
            resolver
                .resolve(directory, specifier)
                .is_ok_and(|resolution| is_first_party_source(&resolution.full_path()))
        })
    }

    /// One `Resolver` per reachable config, built at most once per probe — which is what keeps the
    /// per-specifier cost at one warm `resolve` instead of a resolver build and a JSONC parse per
    /// config.
    fn alias_resolvers(&self) -> &Option<Vec<Resolver>> {
        self.alias_resolvers.get_or_init(|| {
            shared_resolvers().alias_resolvers(self.workspace_root, self.active_document_path)
        })
    }
}

/// The positive evidence itself: a file that **exists** and is not inside `node_modules` — where it
/// would be a package, whose bytes this file's total owes.
fn is_first_party_source(path: &Path) -> bool {
    path.is_file()
        && !path
            .components()
            .any(|component| component.as_os_str() == "node_modules")
}

/// The config files whose `paths` / `baseUrl` make up the workspace's alias table, nearest first.
///
/// `jsconfig.json` is here because a JavaScript project declares its aliases in it and in nothing
/// else — and `oxc_resolver`'s own discovery looks for `tsconfig.json` alone, which is why the
/// previous implementation could not see one at all. Naming the config explicitly is what lets us
/// read either.
const ALIAS_CONFIG_FILE_NAMES: [&str; 2] = ["tsconfig.json", "jsconfig.json"];

/// The nearest `tsconfig.json` / `jsconfig.json` at or above the document, **bounded at the
/// workspace root**.
///
/// The bound is not cosmetic: an unbounded walk reaches `C:\Users\<you>\tsconfig.json` and would
/// let a config from outside the project decide whether one of its imports is first-party.
///
/// A document that is not under the workspace root finds nothing and its specifiers land on floor —
/// conservative, and the direction ADR-0006 demands to fail in.
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
/// **It is a truncation, and it is a residual limit** (stated in SRS FR-024a, not papered over): the
/// tail of a wider graph is not asked, so an alias defined only in the 25th reachable project reads
/// as a floor. Conservative, and the direction to fail in.
const MAX_REACHABLE_ALIAS_CONFIGS: usize = 24;

/// Every config whose `paths` table the workspace can reach from `config_file`: the config itself,
/// and every project in its `references`, transitively.
///
/// The `extends` chain is NOT enumerated here, and does not need to be — `oxc_resolver` folds an
/// extended config's `compilerOptions.paths` into the extending config when it loads one, so a
/// resolver built on `config_file` already sees them.
///
/// `references` are different in kind: a referenced project is a *separate* program with its own
/// alias table, not a base whose settings are inherited. Nothing merges them, and the one that owns
/// the `paths` is not knowable from the document (see [`resolves_to_first_party_source`]) — so the
/// daemon collects them all and asks each.
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

/// The configs named in one config's `references`, as absolute paths to the config *files* — **each
/// one checked on its own, so one bad entry costs only its own table.**
///
/// This used to hand the whole config to `oxc_resolver`'s `resolve_tsconfig` with
/// `TsconfigReferences::Auto` and read `references_resolved`. That call loads **every** referenced
/// project and fails if *any* of them cannot be loaded, so a single stale entry — a `references`
/// pointing at a `tsconfig.node.json` somebody deleted, which is not exotic — returned `Err`, the
/// walk enumerated **nothing**, and every alias in the workspace became a floor. A bad reference must
/// cost that project's table and no other.
///
/// The parse is still oxc's own ([`TsConfig::parse`]): a `tsconfig.json` is JSONC (the create-vue
/// scaffold ships comments in one), and a second parser here would be a second source of truth about
/// what a tsconfig means. It is used *without* loading the references, which is exactly the part that
/// could fail. `references` are never inherited through `extends` — oxc's `extend_tsconfig` copies
/// `files` / `include` / `exclude` / `compilerOptions` and not `references` — so the config's own
/// text is the whole list.
///
/// A config that cannot be read or parsed yields nothing rather than failing the lookup: the tables
/// that *did* load are still evidence, and a specifier none of them maps is a floor.
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
/// The three spellings are `oxc_resolver`'s own (`Cache::get_tsconfig`), and TypeScript's: a path to
/// a **file** is that file; a path to a **directory** implies its `tsconfig.json`; anything else gets
/// `.json` appended.
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

/// A cap on the alias-config-graph memo, which is otherwise keyed by every distinct nearest-config
/// path the daemon has ever been asked about — unbounded, in a monorepo with a `tsconfig.json` per
/// package. Overflow **clears** the map rather than evicting one entry: the map holds path lists, not
/// measurements, so the whole cost of being wrong is one re-walk of the `references` graph, and an
/// LRU would be more machinery than the thing it protects.
const MAX_MEMOIZED_ALIAS_CONFIG_GRAPHS: usize = 64;

/// The three runtime resolvers share one `oxc_resolver` FS cache (Component and
/// Client use identical options, so they share a resolver; Server has its own).
/// Building a fresh resolver per request threw that cache away every time.
pub struct ResolverSet {
    browser: Resolver,
    server: Resolver,
    /// The `references` graph reachable from a nearest `tsconfig.json` / `jsconfig.json`: config
    /// paths only, memoized per nearest-config path.
    ///
    /// **What is memoized is the WALK, not the filesystem.** The walk parses every reachable config
    /// to enumerate its `references`, and it holds nothing but paths; the resolvers built from those
    /// paths are built **fresh for every request** and thrown away with it
    /// ([`ResolverSet::alias_resolvers`], [`FirstPartySourceProbe`]) — because an `oxc_resolver`
    /// that outlives a request memoizes the filesystem, and a memoized **miss** is a cached negative
    /// that nothing lifts. An alias whose target did not exist when the daemon first looked would
    /// stay a floor for the daemon's life, even after the developer created the file. That is the
    /// same defect as the config the daemon read exactly once, one level down.
    ///
    /// (Each alias resolver must in any case hold its OWN oxc FS cache rather than sharing one: oxc
    /// memoizes a **manually configured** tsconfig on the cache entry for `/` — one slot, whatever
    /// the config path — so two configs sharing a cache would silently answer with whichever loaded
    /// first. That is precisely the shape here.)
    ///
    /// The map dies with the `ResolverSet` on [`invalidate_shared_resolvers`], which is what makes a
    /// `tsconfig.json` edit take effect (FR-027a).
    alias_config_graphs: RwLock<HashMap<PathBuf, Arc<Vec<PathBuf>>>>,
}

impl ResolverSet {
    fn new() -> Self {
        let browser = Resolver::new(resolve_options(ImportRuntime::Component));
        // clone_with_options shares the same Arc<Cache>, so all runtimes reuse
        // one set of memoized (option-independent) filesystem facts.
        let server = browser.clone_with_options(resolve_options(ImportRuntime::Server));
        Self {
            browser,
            server,
            alias_config_graphs: RwLock::new(HashMap::new()),
        }
    }

    pub fn resolver(&self, runtime: ImportRuntime) -> &Resolver {
        match runtime {
            ImportRuntime::Component | ImportRuntime::Client => &self.browser,
            ImportRuntime::Server => &self.server,
        }
    }

    /// The resolvers that read the workspace's alias tables — one per config reachable from the
    /// nearest one — used by [`resolves_to_first_party_source`] and by nothing else.
    ///
    /// They are deliberately NOT the resolver that finds package entries. A tsconfig `paths` entry
    /// can shadow a real package name, and a *measurement* must be of what the package manager
    /// actually installed — the bytes that ship — not of whatever the editor's alias table points
    /// at. The alias tables answer one question only: "is this specifier first-party?"
    ///
    /// **Built fresh for every request, on purpose — and exactly once per request.** See
    /// [`ResolverSet::alias_config_graphs`]: a resolver that survives a request caches the
    /// filesystem, and the miss it caches is the one answer that must never be cached. But a
    /// resolver per *specifier* is not the price of that: building the set costs a `Resolver` and a
    /// cold JSONC parse per reachable config, and paying it per specifier put a 20-alias component
    /// at ~20 ms of a 50 ms warm budget. [`FirstPartySourceProbe`] owns the set for the life of one
    /// response, which is the shortest lifetime that is not per-specifier.
    fn alias_resolvers(
        &self,
        workspace_root: &Path,
        active_document_path: &Path,
    ) -> Option<Vec<Resolver>> {
        let config_file = find_workspace_config(workspace_root, active_document_path)?;

        Some(
            self.alias_config_graph(&config_file)
                .iter()
                .map(|config| Resolver::new(alias_resolve_options(config)))
                .collect(),
        )
    }

    /// The memoized `references` walk for one nearest-config path.
    fn alias_config_graph(&self, config_file: &Path) -> Arc<Vec<PathBuf>> {
        if let Ok(memo) = self.alias_config_graphs.read()
            && let Some(configs) = memo.get(config_file)
        {
            return Arc::clone(configs);
        }

        let configs = Arc::new(reachable_alias_configs(config_file));
        if let Ok(mut memo) = self.alias_config_graphs.write() {
            if memo.len() >= MAX_MEMOIZED_ALIAS_CONFIG_GRAPHS {
                memo.clear();
            }
            return Arc::clone(
                memo.entry(config_file.to_path_buf())
                    .or_insert_with(|| Arc::clone(&configs)),
            );
        }
        configs
    }
}

/// Resolution options for ONE alias table: that config, handed over **explicitly**.
///
/// `TsconfigDiscovery::Manual` is half the fix. `Auto` only applies a config that CLAIMS the
/// importing document through `files` / `include` / `exclude`, and TypeScript's default `include`
/// claims no `.vue`, `.svelte` or `.astro` file — so an alias resolved fine from a `.ts` document
/// and resolved to nothing from the other three. `Manual` applies the `paths` table as a property
/// of the project, which is what it is.
///
/// `TsconfigReferences::Disabled` is the other half, and **its old justification was measured false,
/// so here is the one that holds.** The old one said `Auto` would make oxc pick ONE referenced
/// project by `references` **list order**, killing the create-vue scaffold. That was true of the
/// design where oxc chose the project; it is not true of this one, because [`reachable_alias_configs`]
/// walks the `references` graph itself and every table gets its own resolver. Flipping this single
/// word to `Auto` leaves the whole suite green — the alias matrix included — so *that* claim detects
/// nothing and has been retracted.
///
/// What `Disabled` really buys is **immunity to a broken reference**. Under `Auto`, loading a config
/// also loads every project in its `references`, and oxc fails the whole load if any one of them
/// cannot be read: a config that owns a perfectly good `paths` table and happens to list a
/// `tsconfig.node.json` somebody deleted would resolve **nothing at all**, and every alias in it
/// would become a floor. `Disabled` drops the references before they are loaded, so that config's own
/// table still answers. The sibling half of the same hazard is fixed in
/// [`referenced_alias_configs`], and both have a test that goes red without them.
///
/// Nobody picks a project; every table is asked. An `extends` chain still folds in automatically,
/// which is why it needs no walk of its own.
///
/// The extension list adds `.vue`, `.svelte` and `.astro` to the module extensions, because an
/// alias in those projects routinely points AT a component file (`@app/Button` → `src/Button.vue`).
/// It only ever widens what counts as *first-party source* — the file still has to exist and still
/// has to sit outside `node_modules` — and this resolver never picks a package entry, so no
/// measurement can reach these extensions.
fn alias_resolve_options(config_file: &Path) -> ResolveOptions {
    let mut extensions = module_extensions();
    extensions.extend([".vue", ".svelte", ".astro"].map(str::to_owned));

    ResolveOptions {
        tsconfig: Some(TsconfigDiscovery::Manual(TsconfigOptions {
            config_file: config_file.to_path_buf(),
            references: TsconfigReferences::Disabled,
        })),
        extensions,
        ..resolve_options(ImportRuntime::Component)
    }
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

/// Publishes a fresh `ResolverSet` (empty cache, empty alias-config-graph memo). In-flight resolutions
/// keep their `Arc` snapshot and finish against the old cache, so this is safe to call while
/// background prewarm/report resolutions run — unlike oxc's in-place `clear_cache`, which is
/// documented as unsafe against concurrent resolution.
///
/// It is what a `tsconfig.json` / `jsconfig.json` edit rides, as well as a `node_modules` change.
/// Without that, the alias table the daemon loaded at startup was the alias table it used until it
/// died: a developer who followed the documented remedy — add the missing `paths` entry — saw the
/// file stay a floor forever, because the config had been memoized in the resolver's FS cache
/// (`service::invalidate_workspace_config_paths`).
pub fn invalidate_shared_resolvers() {
    if let Ok(mut guard) = resolver_slot().write() {
        *guard = Arc::new(ResolverSet::new());
    }
}

// Shared with the candidate engine so its resolution configuration cannot
// drift from the direct resolver's.
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
            main_fields: vec!["browser".to_owned(), "module".to_owned(), "main".to_owned()],
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
            main_fields: vec!["module".to_owned(), "main".to_owned()],
            module_type: true,
            node_path: false,
            ..ResolveOptions::default()
        },
    }
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

fn side_effects_mode(package_json: &Value, entry_path: &Path) -> SideEffectsMode {
    match package_json.get("sideEffects") {
        Some(Value::Bool(false)) => SideEffectsMode::False,
        Some(Value::Bool(true)) => SideEffectsMode::True,
        Some(Value::Array(patterns)) => side_effects_array_mode(patterns, entry_path),
        // A string is a single glob and is a first-class form in the spec (§7.4), not
        // an invalid value. Landing it in `Unknown` forced the package
        // unconditionally side-effectful and, worse, suppressed the conservative glob
        // diagnostic — while the size suffered the identical undercount an array form
        // does.
        Some(pattern @ Value::String(_)) => {
            side_effects_array_mode(std::slice::from_ref(pattern), entry_path)
        }
        Some(_) => SideEffectsMode::Unknown,
        None => SideEffectsMode::Missing,
    }
}

fn side_effects_array_mode(patterns: &[Value], entry_path: &Path) -> SideEffectsMode {
    let Some(entry) = normalized_side_effect_path(entry_path) else {
        return SideEffectsMode::Unknown;
    };

    let mut side_effect_patterns = Vec::new();

    for pattern in patterns {
        let Some(pattern) = pattern.as_str() else {
            return SideEffectsMode::Unknown;
        };
        side_effect_patterns.push(pattern.to_owned());
    }

    if side_effect_patterns.is_empty() {
        return SideEffectsMode::Unknown;
    }

    SideEffectsMode::Array(SideEffectsPatterns {
        entry_matches: side_effect_patterns
            .iter()
            .any(|pattern| side_effects_pattern_matches(pattern, &entry)),
        patterns: side_effect_patterns,
    })
}

fn normalized_side_effect_path(path: &Path) -> Option<String> {
    path.file_name()?;
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    let node_modules_index = components
        .iter()
        .rposition(|component| *component == "node_modules")?;
    let package_start = node_modules_index + 1;
    let relative_start = if components
        .get(package_start)
        .is_some_and(|name| name.starts_with('@'))
    {
        package_start + 2
    } else {
        package_start + 1
    };

    Some(components.get(relative_start..)?.join("/"))
}

fn side_effects_pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = normalize_side_effect_pattern(pattern);
    let expanded_patterns = expand_brace_patterns(&pattern);

    expanded_patterns.into_iter().any(|pattern| {
        if pattern.contains('/') {
            path_components_match(
                &pattern.split('/').collect::<Vec<_>>(),
                &path.split('/').collect::<Vec<_>>(),
            )
        } else {
            path.split('/')
                .any(|segment| segment_pattern_matches(&pattern, segment))
        }
    })
}

fn normalize_side_effect_pattern(pattern: &str) -> String {
    pattern.trim().trim_start_matches("./").replace('\\', "/")
}

fn expand_brace_patterns(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close_offset) = pattern[open + 1..].find('}') else {
        return vec![pattern.to_owned()];
    };
    let close = open + 1 + close_offset;
    let before = &pattern[..open];
    let after = &pattern[close + 1..];

    pattern[open + 1..close]
        .split(',')
        .flat_map(|choice| expand_brace_patterns(&format!("{before}{choice}{after}")))
        .collect()
}

fn path_components_match(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }

    if pattern[0] == "**" {
        return path_components_match(&pattern[1..], path)
            || (!path.is_empty() && path_components_match(pattern, &path[1..]));
    }

    !path.is_empty()
        && segment_pattern_matches(pattern[0], path[0])
        && path_components_match(&pattern[1..], &path[1..])
}

fn segment_pattern_matches(pattern: &str, segment: &str) -> bool {
    let pattern = pattern.as_bytes();
    let segment = segment.as_bytes();
    let mut pattern_index = 0;
    let mut segment_index = 0;
    let mut star_index = None;
    let mut star_segment_index = 0;

    while segment_index < segment.len() {
        if pattern
            .get(pattern_index)
            .is_some_and(|byte| *byte == b'?' || *byte == segment[segment_index])
        {
            pattern_index += 1;
            segment_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_segment_index = segment_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_segment_index += 1;
            segment_index = star_segment_index;
        } else {
            return false;
        }
    }

    while pattern.get(pattern_index) == Some(&b'*') {
        pattern_index += 1;
    }

    pattern_index == pattern.len()
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

    /// One probe, one specifier — the shape a *single* request has when it asks about *one* import.
    /// A test that asks twice therefore asks through two probes, exactly as two requests would, which
    /// is what makes `creating_the_alias_target_lifts_the_floor_without_an_invalidation` a real
    /// guard: nothing it does can carry a filesystem fact from the first ask to the second, unless
    /// somebody memoizes the resolvers where they must not be memoized.
    fn resolves_to_first_party_source(
        workspace_root: &Path,
        active_document_path: &Path,
        specifier: &str,
    ) -> bool {
        FirstPartySourceProbe::new(workspace_root, active_document_path)
            .resolves_to_first_party_source(specifier)
    }

    /// **The Minor, and it is not cosmetic.** The walk that looks for the workspace's alias table
    /// must stop at the workspace root. Unbounded, it reaches `C:\Users\<you>\tsconfig.json` — a
    /// config from outside the project, deciding whether one of its imports is first-party. A stray
    /// `paths` entry in a home directory would silently bless a missing dependency as an alias,
    /// which is a total short a whole package, cached and passed by `importlens check`.
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

    /// A JavaScript project declares its aliases in `jsconfig.json` and in nothing else.
    /// `oxc_resolver`'s own discovery looks for `tsconfig.json` alone — which is why naming the
    /// config explicitly is what lets the daemon read one at all.
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

    /// A project with no config at all has no alias table the daemon can read, so there is no
    /// positive evidence to be had and every bare specifier that is not installed is a floor.
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

    /// Every `paths` table the nearest config REACHES is asked, including the ones in its
    /// `references` — and `reachable_alias_configs` is what finds them. The solution-style scaffold
    /// keeps its aliases in a referenced project, and the root config that points at it has none of
    /// its own, so a walk that stops at the root config can only ever answer "not an alias".
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

    /// **An alias target above the workspace root IS first-party source.** A monorepo opened at one
    /// package — `packages/web`, whose `paths` reach `../shared` — is an ordinary way to open one,
    /// and the sibling package's source is the user's own code: it ships no npm-package bytes, so it
    /// must flag nothing.
    ///
    /// A previous revision required the target to sit inside the workspace root, mirroring the bound
    /// [`find_workspace_config`] holds on the *config*. The two are not the same rule, and this one
    /// made **every file using a cross-package alias a permanent floor** — never cached, never
    /// persisted, and refused a verdict by `importlens check`. The `node_modules` test is what stops
    /// a real package being mistaken for source, and it is the only bound the target needs.
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

    /// **The floor is not sticky.** An import written before the file it points at is correctly a
    /// floor — and creating that file must lift it, on the next request, with no daemon restart and
    /// no invalidation message. Nothing watches first-party source, so nothing can send one.
    ///
    /// It did not: the alias resolvers were memoized per config, and `oxc_resolver` negative-caches a
    /// missing path in its FS cache. The daemon's *first* answer for a specifier was its answer
    /// forever — a cached negative that nothing invalidates, the same defect as the config the daemon
    /// read exactly once. The resolvers are therefore built per query now, and only the `references`
    /// walk is memoized.
    #[test]
    fn creating_the_alias_target_lifts_the_floor_without_an_invalidation() {
        let fixture = ConfigFixture::new("sticky-floor");
        fixture.write(
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        );
        fixture.write("src/index.ts", "export const app = 1;\n");
        let document = fixture.root.join("src").join("index.ts");

        // The developer writes the import before the component exists. It is a floor, and the daemon
        // has now looked at (and, before the fix, memoized) a path that does not exist.
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

    /// **`TsconfigReferences::Disabled` is load-bearing, and this is what goes red without it.**
    ///
    /// The original justification for `Disabled` — that `Auto` would make oxc pick a referenced
    /// project by list order — stopped being true when the daemon started walking the `references`
    /// graph itself, and flipping the word to `Auto` leaves the rest of the suite green.
    ///
    /// What `Disabled` actually buys: under `Auto`, oxc loads a config's `references` when it loads
    /// the config, and **fails the whole load if any one of them cannot be read**. A config that owns
    /// a perfectly good `paths` table and lists one project that was deleted would then resolve
    /// nothing at all, and every alias in it would become a floor. `Disabled` drops the references
    /// before they are loaded, so the config's own table still answers.
    #[test]
    fn a_dangling_reference_does_not_silence_the_config_that_declares_it() {
        let fixture = ConfigFixture::new("dangling-self");
        // A real alias table, and a `references` entry pointing at a project that is not there —
        // a `tsconfig.node.json` somebody deleted, which is not exotic.
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

    /// **And a dangling reference must not silence its SIBLINGS either.**
    ///
    /// `referenced_alias_configs` used to ask oxc to resolve the root config with its references, and
    /// read `references_resolved`. One unloadable entry made that call `Err`, so **no** reference was
    /// enumerated — the `tsconfig.app.json` beside it, which owns the only `paths` table in a
    /// solution-style scaffold, was never asked, and every alias in the workspace became a floor.
    /// Each entry is checked on its own now.
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
