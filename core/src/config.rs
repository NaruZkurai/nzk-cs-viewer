//! config — indexer policy, viewer settings, and conditional-compilation symbols.
//!
//! Port of the config half of the Python `server.py`:
//! `DEFAULT_CONFIG`, `load_config`, `settings_path`, `load_settings`,
//! `save_settings`, `_nearby_csproj_defines`, `_looks_like_unity`,
//! `resolve_defines`.
//!
//! The `#if` symbols matter for correctness, not convenience: `known=false`
//! tells the parser that nothing said how to evaluate `#if`, so it keeps every
//! branch instead of guessing. The detection order (explicit config, then a
//! nearby generated `*.csproj`, then the Unity editor defaults) is exactly the
//! Python's.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Indexer policy (`csgraph.config.json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub roots: Vec<String>,
    pub excludes: Vec<String>,
    pub host: String,
    pub port: u16,
    pub defines: Vec<String>,
    pub player: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: vec!["_sealed_src".to_string()],
            excludes: [
                ".git", "obj", "bin", "tmp", "node_modules", "_bak",
                "_bak_nzk_consolidate", "_bak_v6_originals", "_superseded", "testing",
                // `copy_path`'s durable journal/backup directory (R2). Never a
                // source file: it is created only for the duration of an apply.
                ".csgraph-copy",
                // Build artifacts, package caches, and vendored/third-party
                // trees. Discovery is recursive, so a root that reaches one of
                // these (the vrcCS repo has a full CPython tree under
                // `cs viewer builder/cache/` and an 8.8 GB cargo `target/`)
                // would otherwise spend minutes walking it. These names are
                // matched as path components, so they prune the tree wherever
                // it appears; a directory that merely contains one of these
                // substrings (`repo_build_Core`, `cs viewer builds`) is kept.
                "cache", "target", "build", "dist", "vendor", "vendored",
                "__pycache__", ".venv", "venv", ".cache", ".cargo", ".rustup",
                "Library",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            host: "127.0.0.1".to_string(),
            port: 8765,
            defines: Vec::new(),
            player: false,
        }
    }
}

/// Viewer settings (`~/.config/csgraph/settings.json`), per-key defaulted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(rename = "uiTheme")]
    pub ui_theme: String,
    /// Publish this viewer into the cross-user instance registry so a csgraph
    /// MCP server running as another user can auto-detect it. Default **on**
    /// (`true`), as the owner asked ("same as last image boot default on");
    /// `false` makes this viewer invisible to discovery.
    #[serde(rename = "viewerDiscovery", default = "default_viewer_discovery")]
    pub viewer_discovery: bool,
}

fn default_viewer_discovery() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings { ui_theme: "nzk-dark+".to_string(), viewer_discovery: true }
    }
}

pub const UI_THEMES: [&str; 2] = ["nzk-dark+", "dark-v1"];

/// `CSGRAPH_SETTINGS` overrides the location; otherwise the XDG config path.
pub fn settings_path() -> PathBuf {
    if let Ok(p) = std::env::var("CSGRAPH_SETTINGS") {
        if !p.is_empty() {
            return PathBuf::from(expanduser(&p));
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{home}/.config")
        });
    Path::new(&base).join("csgraph").join("settings.json")
}

/// Only `~` is expanded, matching `os.path.expanduser` for the common case.
fn expanduser(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// Read the settings file, falling back to defaults per key.
///
/// An unreadable or malformed file is not an error: the viewer must still open.
pub fn load_settings(path: Option<&Path>) -> Settings {
    let path = path.map(PathBuf::from).unwrap_or_else(settings_path);
    let mut out = Settings::default();
    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => {
                if let Some(Value::String(t)) = map.get("uiTheme") {
                    out.ui_theme = t.clone();
                }
                if let Some(Value::Bool(b)) = map.get("viewerDiscovery") {
                    out.viewer_discovery = *b;
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("[csgraph] settings {} unreadable ({}); using defaults",
                                path.display(), e),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[csgraph] settings {} unreadable ({}); using defaults",
                            path.display(), e),
    }
    if !UI_THEMES.contains(&out.ui_theme.as_str()) {
        out.ui_theme = Settings::default().ui_theme;
    }
    out
}

/// Merge known keys into the settings file (atomic replace).
pub fn save_settings(patch: &Value, path: Option<&Path>) -> Option<Settings> {
    let path = path.map(PathBuf::from).unwrap_or_else(settings_path);
    let mut cfg = load_settings(Some(&path));
    if let Value::Object(map) = patch {
        if let Some(Value::String(t)) = map.get("uiTheme") {
            if UI_THEMES.contains(&t.as_str()) {
                cfg.ui_theme = t.clone();       // never let a bad value clobber a good one
            }
        }
        if let Some(Value::Bool(b)) = map.get("viewerDiscovery") {
            cfg.viewer_discovery = *b;
        }
    }
    if !UI_THEMES.contains(&cfg.ui_theme.as_str()) {
        cfg.ui_theme = Settings::default().ui_theme;
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let body = match serde_json::to_string_pretty(&cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[csgraph] settings save failed: {e}");
            return None;
        }
    };
    if let Err(e) = fs::write(&tmp, body + "\n").and_then(|_| fs::rename(&tmp, &path)) {
        eprintln!("[csgraph] settings save failed: {e}");
        return None;
    }
    Some(cfg)
}

/// Load `csgraph.config.json`, starting from the defaults.
///
/// Matches the Python's leniency for the array keys (`roots`/`excludes`/
/// `defines`): a value of the wrong JSON type is ignored rather than fatal,
/// because a broken config should never stop the viewer from opening.
pub fn load_config(path: &Path) -> Config {
    let mut cfg = Config::default();
    let Ok(text) = fs::read_to_string(path) else {
        return cfg;
    };
    let user: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[csgraph] config {} unreadable ({}); using defaults", path.display(), e);
            return cfg;
        }
    };
    let Value::Object(map) = user else { return cfg };
    if let Some(Value::Array(a)) = map.get("roots") {
        cfg.roots = a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    }
    if let Some(Value::Array(a)) = map.get("excludes") {
        cfg.excludes = a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
    }
    if let Some(Value::String(s)) = map.get("host") {
        cfg.host = s.clone();
    }
    if let Some(Value::Number(n)) = map.get("port") {
        if let Some(p) = n.as_u64() {
            cfg.port = p as u16;
        }
    }
    match map.get("defines") {
        Some(Value::Array(a)) => {
            cfg.defines = a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
        }
        _ => cfg.defines = Vec::new(),
    }
    if let Some(Value::Bool(b)) = map.get("player") {
        cfg.player = *b;
    }
    cfg
}

// ---------------------------------------------------------------------------
// conditional-compilation symbols
// ---------------------------------------------------------------------------

/// Union of every `<DefineConstants>` in `*.csproj` around `repo`.
///
/// Unity writes the effective editor symbol list into the generated project
/// files, so reading one is far more faithful than guessing. Looked for in the
/// indexed folder, its parents, and each parent's `root/`.
pub fn nearby_csproj_defines(repo: &Path, levels: usize) -> HashSet<String> {
    let mut out = HashSet::new();
    let base = absolutise(repo);
    let mut dirs = vec![base.clone()];
    let mut up = base;
    for _ in 0..levels {
        let Some(parent) = up.parent().map(Path::to_path_buf) else { break };
        if parent == up {
            break;
        }
        up = parent;
        dirs.push(up.clone());
    }
    let mut cand: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for d in dirs {
        for sub in [d.clone(), d.join("root")] {
            if seen.insert(sub.clone()) && sub.is_dir() {
                cand.push(sub);
            }
        }
    }
    for d in cand {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        for nm in names {
            if !nm.ends_with(".csproj") {
                continue;
            }
            let Ok(txt) = fs::read_to_string(d.join(&nm)) else { continue };
            for cap in define_re().captures_iter(&txt) {
                if let Some(m) = cap.get(1) {
                    for sym in m.as_str().split(';') {
                        let sym = sym.trim();
                        if !sym.is_empty() {
                            out.insert(sym.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

fn define_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"<DefineConstants>([^<]*)</DefineConstants>").unwrap())
}

/// True when `repo` sits inside (or next to) a Unity project tree.
pub fn looks_like_unity(repo: &Path) -> bool {
    let base = absolutise(repo);
    let mut d = base.clone();
    for _ in 0..4 {
        if d.join("ProjectSettings").is_dir() || d.join("Assets").is_dir() {
            return true;
        }
        match d.parent().map(Path::to_path_buf) {
            Some(p) if p != d => d = p,
            _ => break,
        }
    }
    let flat = base.to_string_lossy().replace('\\', "/");
    flat.split('/').any(|part| part == "Assets")
}

fn absolutise(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| {
        std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf())
    })
}

/// `(symbols, known)` for the index at `repo` (and the roots it indexes).
///
/// `known == false` means nothing told us how to evaluate `#if`, so the parser
/// keeps every branch instead of guessing.
pub fn resolve_defines(repo: &Path, cfg: &Config, roots: Option<&[String]>) -> (HashSet<String>, bool) {
    let mut syms: HashSet<String> = HashSet::new();
    for s in &cfg.defines {
        let s = s.trim();
        if !s.is_empty() {
            syms.insert(s.to_string());
        }
    }
    let mut probes = vec![absolutise(repo)];
    for r in roots.unwrap_or(&[]) {
        if r.is_empty() {
            continue;
        }
        let p = if Path::new(r).is_absolute() {
            PathBuf::from(r)
        } else {
            repo.join(r)
        };
        probes.push(absolutise(&p));
    }
    let mut found: HashSet<String> = HashSet::new();
    let mut unity = false;
    for p in &probes {
        found.extend(nearby_csproj_defines(p, 3));
        unity = looks_like_unity(p) || unity;
    }
    if !found.is_empty() {
        syms.extend(found.iter().cloned());
    } else if unity {
        syms.extend(crate::model::unity_defines().into_iter().map(str::to_string));
    }
    let known = !found.is_empty() || unity || !cfg.defines.is_empty();
    if cfg.player {
        syms.retain(|s| !s.starts_with("UNITY_EDITOR") && s != "UNITY_EDITOR_ONLY_COMPILATION");
    }
    syms.remove("");
    (syms, known)
}

// ---------------------------------------------------------------------------
// solution / project model (.sln + .csproj)
// ---------------------------------------------------------------------------
//
// There is no MSBuild and no Roslyn here: this is a bounded, best-effort reader
// of the two files that tell a human (and the viewer) which C# files belong to
// which project and which `#if` symbols that project defines. It never claims
// to be a build. When a file cannot be read or does not look like MSBuild, a
// diagnostic is recorded and the rest of the tree is still parsed.

/// One parsed `.csproj`.
///
/// `id` is the repo-relative project path (forward slashes) — stable across
/// runs, readable in a URL, and unique for a real tree. `files` holds the
/// source set (SDK default globs plus explicit `<Compile Include>`), as
/// repo-relative display paths, sorted. `defines` is the sorted union of every
/// `<DefineConstants>` property in the file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub targets: Vec<String>,
    pub defines: Vec<String>,
    pub files: Vec<String>,
    pub sdk_style: bool,
}

/// The project model the viewer exposes: every `.sln` found near the indexed
/// roots plus every `.csproj` those solutions reference (and any `.csproj`
/// sitting next to the roots).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectModel {
    pub solutions: Vec<String>,
    pub projects: Vec<Project>,
    pub diagnostics: Vec<String>,
}

impl ProjectModel {
    /// The model without per-file lists — the small form `/api/projects` sends
    /// by default. `files` is the expensive field on a large tree.
    pub fn compact(&self) -> Value {
        json_projects(self, false)
    }

    /// The model with every source file listed (`/api/projects?files=1`).
    pub fn full(&self) -> Value {
        json_projects(self, true)
    }
}

fn json_projects(model: &ProjectModel, with_files: bool) -> Value {
    let projects: Vec<Value> = model
        .projects
        .iter()
        .map(|p| {
            let mut o = serde_json::Map::new();
            o.insert("id".to_string(), Value::String(p.id.clone()));
            o.insert("name".to_string(), Value::String(p.name.clone()));
            o.insert("path".to_string(), Value::String(p.path.clone()));
            o.insert(
                "targets".to_string(),
                Value::Array(p.targets.iter().cloned().map(Value::String).collect()),
            );
            o.insert(
                "defines".to_string(),
                Value::Array(p.defines.iter().cloned().map(Value::String).collect()),
            );
            o.insert("fileCount".to_string(), Value::from(p.files.len()));
            o.insert("sdkStyle".to_string(), Value::Bool(p.sdk_style));
            if with_files {
                o.insert(
                    "files".to_string(),
                    Value::Array(p.files.iter().cloned().map(Value::String).collect()),
                );
            }
            Value::Object(o)
        })
        .collect();
    Value::Object(
        [
            ("solutions", Value::Array(model.solutions.iter().cloned().map(Value::String).collect())),
            ("projects", Value::Array(projects)),
            (
                "diagnostics",
                Value::Array(model.diagnostics.iter().cloned().map(Value::String).collect()),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    )
}

/// `symbol -> [project id, ...]` — which project(s) contributed each define.
///
/// The union the engine already used is unchanged ([`resolve_defines`] still
/// calls [`nearby_csproj_defines`]); this only makes the contribution visible.
pub fn define_sources(model: &ProjectModel) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in &model.projects {
        for d in &p.defines {
            let ids = out.entry(d.clone()).or_default();
            if !ids.contains(&p.id) {
                ids.push(p.id.clone());
            }
        }
    }
    for ids in out.values_mut() {
        ids.sort();
    }
    out
}

/// `display file path -> [project id, ...]` — the node-attribution lookup.
///
/// Display paths match the indexer's (`display_path`, repo-relative when the
/// file is inside the repo), so a node's `file` can be attributed directly.
pub fn project_file_map(model: &ProjectModel) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in &model.projects {
        for f in &p.files {
            let ids = out.entry(f.clone()).or_default();
            if !ids.contains(&p.id) {
                ids.push(p.id.clone());
            }
        }
    }
    for ids in out.values_mut() {
        ids.sort();
    }
    out
}

/// Fields read out of one `.csproj`, before paths are resolved.
#[derive(Clone, Debug, Default)]
struct CsprojFields {
    name: String,
    targets: Vec<String>,
    defines: Vec<String>,
    sdk_style: bool,
    default_items: bool,
    includes: Vec<String>,
    removes: Vec<String>,
}

/// Read one `.csproj`, returning its raw fields or a diagnostic reason.
fn read_csproj(path: &Path) -> Result<CsprojFields, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_csproj(&text)
}

/// Parse `.csproj` text. `Err` is a file that is not an MSBuild project at all
/// (no `<Project>` root); a syntactically odd but recognisable project still
/// parses into whatever fields could be found.
fn parse_csproj(text: &str) -> Result<CsprojFields, String> {
    if !text.contains("<Project") {
        return Err("not an MSBuild project (no <Project> root)".to_string());
    }
    let pick = |tag: &str| -> String {
        xml_prop_all(text, tag)
            .into_iter()
            .find(|v| !v.is_empty() && !v.contains('$'))
            .unwrap_or_default()
    };
    let mut name = pick("AssemblyName");
    if name.is_empty() {
        name = pick("ProjectName");
    }
    if name.is_empty() {
        name = pick("RootNamespace");
    }
    let mut targets: Vec<String> = Vec::new();
    for v in xml_prop_all(text, "TargetFramework") {
        push_unique(&mut targets, &v);
    }
    for v in xml_prop_all(text, "TargetFrameworks") {
        push_unique(&mut targets, &v);
    }
    targets.sort();
    let mut defines: Vec<String> = Vec::new();
    for v in xml_prop_all(text, "DefineConstants") {
        for sym in v.split(';') {
            let sym = sym.trim();
            // `$(DefineConstants)` is a property reference, not a symbol.
            if !sym.is_empty() && !sym.contains('$') {
                push_unique(&mut defines, sym);
            }
        }
    }
    defines.sort();
    let sdk_style = regex_match(text, r#"(?is)<Project\b[^>]*\bSdk\s*="#)
        || text.contains("Microsoft.NET.Sdk")
        || text.contains("Sdk.props");
    let default_items = !xml_prop_all(text, "EnableDefaultCompileItems")
        .iter()
        .any(|v| v.trim().eq_ignore_ascii_case("false"));
    let includes = xml_attr_all(text, "Compile", "Include");
    let removes = xml_attr_all(text, "Compile", "Remove");
    Ok(CsprojFields {
        name,
        targets,
        defines,
        sdk_style,
        default_items,
        includes,
        removes,
    })
}

/// Parse a `.sln` and return the project paths it names (as written).
///
/// The classic format is one line per project:
/// `Project("{<type-guid>}") = "Name", "relative\path.csproj", "{<guid>}"`.
/// Anything that is not a quoted `.csproj` (solution folders, web projects) is
/// skipped. A malformed line is simply ignored — a broken solution must not
/// stop the viewer from parsing the projects next to it.
pub fn parse_sln(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("Project(") {
            continue;
        }
        let parts: Vec<&str> = line.split('"').collect();
        // ["Project(", typeGuid, ") = ", name, ", ", path, ", ", guid, ...]
        if parts.len() < 6 {
            continue;
        }
        let raw = parts[5].trim();
        if !raw.to_ascii_lowercase().ends_with(".csproj") {
            continue;
        }
        let norm = raw.replace('\\', "/");
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    out
}

/// Directories that can hold a solution or project file: the indexed repo, its
/// parents (plus each parent's `root/`), and the configured roots. Bounded —
/// never a recursive scan of the machine.
fn project_candidate_dirs(repo: &Path, roots: &[String], levels: usize) -> Vec<PathBuf> {
    let base = absolutise(repo);
    let mut probes: Vec<PathBuf> = Vec::new();
    let mut up = base.clone();
    for i in 0..=levels {
        probes.push(up.clone());
        probes.push(up.join("root"));
        let Some(parent) = up.parent().map(Path::to_path_buf) else { break };
        if parent == up || i == levels {
            break;
        }
        up = parent;
    }
    for r in roots {
        if r.is_empty() {
            continue;
        }
        let p = if Path::new(r).is_absolute() {
            PathBuf::from(r)
        } else {
            repo.join(r)
        };
        probes.push(absolutise(&p));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for p in probes {
        let key = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.insert(key) && p.is_dir() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Discover every solution/project near `repo` and build the model.
///
/// `levels` bounds how far up the tree the parent search goes, exactly like
/// [`nearby_csproj_defines`]. Missing or malformed files land in
/// `diagnostics`; they never abort discovery.
pub fn discover_projects(repo: &Path, roots: &[String], levels: usize) -> ProjectModel {
    let mut model = ProjectModel::default();
    let mut projects: Vec<Project> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // Deterministic scan list: the flat candidate dirs (repo, parents, roots),
    // then a bounded recursive walk of the repo and each configured root so a
    // project in a subdirectory is still found.
    let mut scans: Vec<(PathBuf, bool)> = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    for dir in project_candidate_dirs(repo, roots, levels) {
        let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen_dirs.insert(key) {
            scans.push((dir, false));
        }
    }
    let mut recursive: Vec<PathBuf> = vec![absolutise(repo)];
    for r in roots {
        if r.is_empty() {
            continue;
        }
        let p = if Path::new(r).is_absolute() {
            PathBuf::from(r)
        } else {
            repo.join(r)
        };
        recursive.push(absolutise(&p));
    }
    let mut seen_recursive: HashSet<PathBuf> = HashSet::new();
    for dir in recursive {
        let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen_recursive.insert(key) {
            scans.push((dir, true));
        }
    }

    // The scope is known before any project is built: a `.csproj` outside the
    // indexed repo/roots is dropped from the model at the end anyway, so reading
    // it (and, for an SDK-style project, walking its whole directory for the
    // default `**/*.cs` glob) is pure waste. On the real Unity layout the repo
    // sits under `<unity>/Assets/<pkg>` and the parent holds every
    // Unity-generated `.csproj`; building those cost seconds of `stat`/`read`
    // work that the next step then threw away.
    let scopes = project_scope_dirs(repo, roots);

    let debug_timing = std::env::var_os("CSGRAPH_BUILD_TIMING").is_some();
    for (dir, deep) in scans {
        let t_scan = std::time::Instant::now();
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in WalkDir::new(&dir)
            .follow_links(false)
            .max_depth(if deep { 8 } else { 1 })
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_type().is_dir() || !glob_dir_excluded(e))
            .take(50_000)
            .flatten()
        {
            if entry.file_type().is_file() {
                paths.push(entry.into_path());
            }
        }
        paths.sort();
        let n_paths = paths.len();
        for path in paths {
            scan_project_file(&path, repo, &scopes, &mut model, &mut projects, &mut seen);
        }
        if debug_timing {
            eprintln!(
                "[timing]   projects.scan deep={deep} dir={} files={n_paths} {}ms",
                dir.display(),
                t_scan.elapsed().as_millis()
            );
        }
    }

    projects.sort_by(|a, b| a.path.cmp(&b.path));
    projects.dedup_by(|a, b| a.path == b.path);

    // Keep the final filter even though scope is applied before building: it is
    // the authoritative guard that no caller (or a `.sln`-referenced file) can
    // leak a foreign project into `/api/projects` or the viewer's Filters list.
    projects.retain(|p| project_path_in_scope(repo, &p.path, &scopes));
    for p in &mut projects {
        p.files.retain(|f| project_path_in_scope(repo, f, &scopes));
    }

    model.projects = projects;
    model.solutions.retain(|s| project_path_in_scope(repo, s, &scopes));
    model.solutions.sort();
    model.diagnostics.sort();
    model.diagnostics.dedup();
    model
}

/// Absolute directories a discovered project is allowed to live under: the
/// indexed repo itself plus every configured root (relative roots join the
/// repo). Canonicalised so a symlinked repo path (`~/nem` -> `/run/media/...`)
/// and the canonical paths a `.sln`/`.csproj` reports compare equal.
fn project_scope_dirs(repo: &Path, roots: &[String]) -> Vec<PathBuf> {
    let mut out = vec![fs::canonicalize(repo).unwrap_or_else(|_| absolutise(repo))];
    for r in roots {
        if r.is_empty() {
            continue;
        }
        let p = if Path::new(r).is_absolute() {
            PathBuf::from(r)
        } else {
            repo.join(r)
        };
        let p = fs::canonicalize(&p).unwrap_or_else(|_| absolutise(&p));
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// `true` when the repo-relative (or absolute) display path `path` resolves
/// inside one of `scopes`. Unresolvable paths are out of scope: an external,
/// missing or stale project must not be advertised as part of this corpus.
fn project_path_in_scope(repo: &Path, path: &str, scopes: &[PathBuf]) -> bool {
    if path.is_empty() {
        return false;
    }
    let p = Path::new(path);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo.join(p)
    };
    let p = fs::canonicalize(&p).unwrap_or_else(|_| absolutise(&p));
    scopes.iter().any(|scope| p.starts_with(scope))
}

/// Read a `.sln` or `.csproj` into the model (or record why it could not).
///
/// `scopes` is the corpus this model is for: a `.csproj` outside it is never
/// read or expanded. Its only possible fate later is being dropped, so building
/// it first (and walking a Unity project root for an SDK-style glob, or stat-ing
/// thousands of explicit `<Compile Include>` entries) is work no answer can use.
fn scan_project_file(
    path: &Path,
    repo: &Path,
    scopes: &[PathBuf],
    model: &mut ProjectModel,
    projects: &mut Vec<Project>,
    seen: &mut HashSet<PathBuf>,
) {
    let Some(file_name) = path.file_name().map(|s| s.to_string_lossy().to_ascii_lowercase()) else {
        return;
    };
    if file_name.ends_with(".sln") {
        let rel = crate::parser::display_path(path, repo);
        match fs::read_to_string(path) {
            Ok(txt) => {
                if !model.solutions.contains(&rel) {
                    model.solutions.push(rel);
                }
                for pref in parse_sln(&txt) {
                    let child = path
                        .parent()
                        .map(|d| d.join(&pref))
                        .unwrap_or_else(|| PathBuf::from(&pref));
                    let child_rel = crate::parser::display_path(&child, repo);
                    match fs::canonicalize(&child) {
                        Ok(c) => {
                            if !scopes.iter().any(|scope| c.starts_with(scope)) {
                                continue;
                            }
                            if seen.insert(c.clone()) {
                                match read_csproj(&c) {
                                    Ok(fields) => projects.push(build_project(
                                        fields,
                                        &c,
                                        repo,
                                        &mut model.diagnostics,
                                    )),
                                    Err(e) => {
                                        model.diagnostics.push(format!("{child_rel}: {e}"))
                                    }
                                }
                            }
                        }
                        Err(_) => model
                            .diagnostics
                            .push(format!("{child_rel}: project file not found")),
                    }
                }
            }
            Err(e) => model.diagnostics.push(format!("{rel}: unreadable solution ({e})")),
        }
    } else if file_name.ends_with(".csproj") {
        if let Ok(c) = fs::canonicalize(path) {
            if !scopes.iter().any(|scope| c.starts_with(scope)) {
                return;
            }
            if seen.insert(c.clone()) {
                match read_csproj(&c) {
                    Ok(fields) => {
                        projects.push(build_project(fields, &c, repo, &mut model.diagnostics))
                    }
                    Err(e) => model
                        .diagnostics
                        .push(format!("{}: {e}", crate::parser::display_path(path, repo))),
                }
            }
        }
    }
}

/// Turn parsed fields into a [`Project`], resolving the source-file set.
fn build_project(
    mut fields: CsprojFields,
    csproj: &Path,
    repo: &Path,
    diagnostics: &mut Vec<String>,
) -> Project {
    let project_dir = csproj.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    if fields.name.is_empty() {
        fields.name = csproj
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
    }
    let path = crate::parser::display_path(csproj, repo);
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();

    // SDK-style projects include `**/*.cs` by default; `bin`/`obj` and hidden
    // dirs are never compile inputs.
    if fields.sdk_style && fields.default_items {
        for entry in WalkDir::new(&project_dir)
            .follow_links(false)
            .max_depth(32)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_type().is_dir() || !glob_dir_excluded(e))
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) == Some("cs") {
                files.insert(entry.path().to_path_buf());
            }
        }
    }

    // Explicit `<Compile Include>` items are always part of the set.
    for inc in &fields.includes {
        let inc = inc.trim();
        if inc.is_empty() || inc.contains('$') || inc.contains('*') {
            continue; // wildcards are covered by the globs above
        }
        let p = if Path::new(inc).is_absolute() {
            PathBuf::from(inc)
        } else {
            project_dir.join(inc.replace('\\', "/"))
        };
        if p.is_file() {
            files.insert(p);
        }
    }

    // `<Compile Remove>` / `Exclude` patterns, best-effort: match the
    // project-relative path or the file name.
    if !fields.removes.is_empty() {
        let removes: Vec<String> = fields
            .removes
            .iter()
            .flat_map(|v| v.split(';'))
            .map(|s| s.trim().replace('\\', "/"))
            .filter(|s| !s.is_empty())
            .collect();
        files.retain(|p| {
            let rel = p
                .strip_prefix(&project_dir)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"));
            let base = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            !removes
                .iter()
                .any(|pat| crate::parser::fnmatch(&rel, pat) || crate::parser::fnmatch(&base, pat))
        });
    }

    let mut out_files: BTreeSet<String> = BTreeSet::new();
    for p in &files {
        out_files.insert(crate::parser::display_path(p, repo));
    }
    let _ = diagnostics;
    Project {
        id: path.clone(),
        name: fields.name,
        path,
        targets: fields.targets,
        defines: fields.defines,
        files: out_files.into_iter().collect(),
        sdk_style: fields.sdk_style,
    }
}

/// True when a directory is a build output or vendored dir that SDK globs skip.
fn glob_dir_excluded(e: &walkdir::DirEntry) -> bool {
    let name = e.file_name().to_string_lossy();
    name.starts_with('.')
        || matches!(name.as_ref(), "bin" | "obj" | "node_modules" | "target" | "Library")
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    let s = s.trim();
    if !s.is_empty() && !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

fn regex_match(text: &str, pat: &str) -> bool {
    regex::Regex::new(pat).map(|re| re.is_match(text)).unwrap_or(false)
}

/// Every `<Tag>value</Tag>` (namespace prefix optional), trimmed.
fn xml_prop_all(text: &str, tag: &str) -> Vec<String> {
    let pat = format!(
        r"(?is)<(?:[A-Za-z_][\w.-]*:)?{tag}(?:\s[^>]*)?>(.*?)</(?:[A-Za-z_][\w.-]*:)?{tag}\s*>"
    );
    let Ok(re) = regex::Regex::new(&pat) else { return Vec::new() };
    re.captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .collect()
}

/// Every `attr="..."` on `<Elem ...>` (namespace prefix optional).
fn xml_attr_all(text: &str, elem: &str, attr: &str) -> Vec<String> {
    let pat = format!(
        r#"(?is)<(?:[A-Za-z_][\w.-]*:)?{elem}\b[^>]*?\b{attr}\s*=\s*"([^"]*)""#
    );
    let Ok(re) = regex::Regex::new(&pat) else { return Vec::new() };
    re.captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_python() {
        let c = Config::default();
        assert_eq!(c.roots, vec!["_sealed_src"]);
        assert_eq!(c.port, 8765);
        assert!(c.excludes.contains(&"testing".to_string()));
        assert!(!c.player);
        assert_eq!(Settings::default().ui_theme, "nzk-dark+");
    }

    #[test]
    fn player_build_drops_the_editor_symbols() {
        let dir = std::env::temp_dir().join(format!("csgraph-cfg-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.defines = vec!["UNITY_EDITOR".to_string(), "KEEP_ME".to_string(),
                           "UNITY_EDITOR_ONLY_COMPILATION".to_string()];
        cfg.player = false;
        let (syms, known) = resolve_defines(&dir, &cfg, None);
        assert!(known);
        assert!(syms.contains("UNITY_EDITOR"));
        assert!(syms.contains("KEEP_ME"));
        cfg.player = true;
        let (syms, _) = resolve_defines(&dir, &cfg, None);
        assert!(!syms.contains("UNITY_EDITOR"));
        assert!(!syms.contains("UNITY_EDITOR_ONLY_COMPILATION"));
        assert!(syms.contains("KEEP_ME"));
        let _ = fs::remove_dir_all(&dir);
    }

    // -- .sln / .csproj model ------------------------------------------------

    /// A private repo dir whose parent is also private, so the bounded
    /// parent-dir search cannot pick up a sibling test's files.
    fn proj_repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("csgraph-proj-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        repo
    }

    fn write(path: &Path, text: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn sdk_csproj(assembly: &str, defines: &str) -> String {
        format!(
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n\
             <PropertyGroup>\n\
               <AssemblyName>{assembly}</AssemblyName>\n\
               <TargetFramework>net472</TargetFramework>\n\
               <DefineConstants>{defines}</DefineConstants>\n\
             </PropertyGroup>\n\
             </Project>\n"
        )
    }

    #[test]
    fn sln_lists_multiple_projects() {
        let repo = proj_repo("sln-multi");
        write(&repo.join("Demo.sln"), "Microsoft Visual Studio Solution File, Format Version 12.00\n\
            Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"App\", \"App\\App.csproj\", \"{11111111-1111-1111-1111-111111111111}\"\n\
            Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Lib\", \"Lib/Lib.csproj\", \"{22222222-2222-2222-2222-222222222222}\"\n\
            Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"SolutionItems\", \"SolutionItems\", \"{33333333-3333-3333-3333-333333333333}\"\n\
            EndProject\n");
        write(&repo.join("App/App.csproj"), &sdk_csproj("App", "TRACE;APP"));
        write(&repo.join("Lib/Lib.csproj"), &sdk_csproj("Lib", "TRACE;LIB"));
        fs::create_dir_all(repo.join("App")).unwrap();
        fs::create_dir_all(repo.join("Lib")).unwrap();

        let m = discover_projects(&repo, &[], 1);
        let names: Vec<&str> = m.projects.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"App"), "projects: {names:?}");
        assert!(names.contains(&"Lib"), "projects: {names:?}");
        assert_eq!(m.projects.len(), 2, "solution folders must not become projects");
        assert!(m.solutions.iter().any(|s| s.ends_with("Demo.sln")));
        // ids/paths are repo-relative with forward slashes
        let app = m.projects.iter().find(|p| p.name == "App").unwrap();
        assert_eq!(app.path, "App/App.csproj");
        assert_eq!(app.id, app.path);
        assert!(app.sdk_style);
        assert_eq!(app.targets, vec!["net472"]);
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn explicit_compile_include_is_honored_without_globs() {
        let repo = proj_repo("explicit");
        write(&repo.join("Shared/Shared.cs"), "namespace N { class Shared {} }\n");
        write(&repo.join("App/Program.cs"), "namespace N { class Program {} }\n");
        write(
            &repo.join("App/App.csproj"),
            "<Project ToolsVersion=\"4.0\">\n\
             <PropertyGroup><AssemblyName>App</AssemblyName></PropertyGroup>\n\
             <ItemGroup><Compile Include=\"..\\Shared\\Shared.cs\" /></ItemGroup>\n\
             </Project>\n",
        );

        let m = discover_projects(&repo, &[], 1);
        assert_eq!(m.projects.len(), 1, "diagnostics: {:?}", m.diagnostics);
        let p = &m.projects[0];
        assert!(!p.sdk_style);
        assert_eq!(p.files, vec!["Shared/Shared.cs".to_string()]);
        assert!(
            !p.files.iter().any(|f| f.ends_with("Program.cs")),
            "a non-SDK project must not glob its directory"
        );
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn sdk_default_globs_skip_bin_and_obj() {
        let repo = proj_repo("globs");
        write(&repo.join("App/App.csproj"), &sdk_csproj("App", "TRACE"));
        write(&repo.join("App/Program.cs"), "namespace N { class Program {} }\n");
        write(&repo.join("App/Sub/Deep.cs"), "namespace N { class Deep {} }\n");
        write(&repo.join("App/bin/Skip.cs"), "namespace N { class Skip {} }\n");
        write(&repo.join("App/obj/Skip2.cs"), "namespace N { class Skip2 {} }\n");

        let m = discover_projects(&repo, &[], 1);
        assert_eq!(m.projects.len(), 1, "diagnostics: {:?}", m.diagnostics);
        let files = &m.projects[0].files;
        assert!(files.contains(&"App/Program.cs".to_string()), "{files:?}");
        assert!(files.contains(&"App/Sub/Deep.cs".to_string()), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("/bin/") || f.contains("/obj/")), "{files:?}");
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn define_constants_union_and_attribution() {
        let repo = proj_repo("defines");
        write(&repo.join("App/App.csproj"), &sdk_csproj("App", "$(DefineConstants);TRACE;APP_ONLY"));
        write(&repo.join("Lib/Lib.csproj"), &sdk_csproj("Lib", "TRACE;LIB_ONLY"));
        write(&repo.join("App/Program.cs"), "namespace N { class Program {} }\n");
        write(&repo.join("Lib/Lib.cs"), "namespace N { class Lib {} }\n");

        let m = discover_projects(&repo, &[], 1);
        let app = m.projects.iter().find(|p| p.name == "App").unwrap();
        let lib = m.projects.iter().find(|p| p.name == "Lib").unwrap();
        // The property reference is not a symbol.
        assert_eq!(app.defines, vec!["APP_ONLY".to_string(), "TRACE".to_string()]);
        assert_eq!(lib.defines, vec!["LIB_ONLY".to_string(), "TRACE".to_string()]);

        let sources = define_sources(&m);
        assert_eq!(
            sources.get("TRACE").unwrap(),
            &vec![app.id.clone(), lib.id.clone()]
        );
        assert_eq!(sources.get("APP_ONLY").unwrap(), &vec![app.id.clone()]);
        assert!(sources.get("$(DefineConstants)").is_none());

        // Node attribution: file -> owning project id(s).
        let fmap = project_file_map(&m);
        assert_eq!(fmap.get("App/Program.cs").unwrap(), &vec![app.id.clone()]);
        assert_eq!(fmap.get("Lib/Lib.cs").unwrap(), &vec![lib.id.clone()]);

        // The existing union behaviour is untouched (flat csproj in one dir,
        // exactly where `nearby_csproj_defines` already looked).
        write(&repo.join("flat/F1.csproj"), &sdk_csproj("F1", "TRACE;FLAT_A"));
        write(&repo.join("flat/F2.csproj"), &sdk_csproj("F2", "TRACE;FLAT_B"));
        let cfg = Config { defines: vec![], ..Config::default() };
        let (syms, known) = resolve_defines(&repo.join("flat"), &cfg, None);
        assert!(known);
        for s in ["TRACE", "FLAT_A", "FLAT_B"] {
            assert!(syms.contains(s), "union missing {s}: {syms:?}");
        }
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn malformed_and_missing_files_are_diagnostics_not_panics() {
        let repo = proj_repo("broken");
        write(
            &repo.join("Broken.sln"),
            "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Gone\", \"Gone\\Gone.csproj\", \"{44444444-4444-4444-4444-444444444444}\"\n",
        );
        write(&repo.join("Ugly.csproj"), "this is not an msbuild file at all\n");
        write(&repo.join("Half.csproj"), "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><AssemblyName>Half\n");

        assert!(parse_csproj("nope").is_err());
        let m = discover_projects(&repo, &[], 1);
        assert!(
            m.diagnostics.iter().any(|d| d.contains("Gone.csproj") && d.contains("not found")),
            "missing project must be a diagnostic: {:?}",
            m.diagnostics
        );
        assert!(
            m.diagnostics.iter().any(|d| d.contains("Ugly.csproj") && d.contains("not an MSBuild")),
            "malformed project must be a diagnostic: {:?}",
            m.diagnostics
        );
        // The unclosed-but-recognisable project still parses; it just has no name.
        assert!(m.projects.iter().any(|p| p.path == "Half.csproj"));
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn csproj_without_assembly_name_falls_back_to_the_stem() {
        let repo = proj_repo("no-asm");
        write(
            &repo.join("Widget/Widget.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n\
             <PropertyGroup><TargetFramework>netstandard2.0</TargetFramework></PropertyGroup>\n\
             </Project>\n",
        );
        write(&repo.join("Widget/Widget.cs"), "namespace N { class Widget {} }\n");
        let m = discover_projects(&repo, &[], 1);
        let p = m.projects.iter().find(|p| p.path == "Widget/Widget.csproj").unwrap();
        assert_eq!(p.name, "Widget");
        assert_eq!(p.files, vec!["Widget/Widget.cs".to_string()]);
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    /// A project outside the indexed repo/roots is NOT part of this corpus.
    ///
    /// The real shape: the indexed folder is a Unity package under
    /// `<unity>/Assets/<pkg>`, and `<unity>/` (the repo's parent) holds a pile
    /// of Unity-generated `.csproj` files. Discovery probes the parent by
    /// design, but the model must not advertise those foreign projects in
    /// `/api/projects` or the viewer's Filters list.
    #[test]
    fn projects_outside_the_indexed_scope_are_dropped() {
        let repo = proj_repo("scope");
        let parent = repo.parent().unwrap().to_path_buf();
        // Inside the corpus.
        write(&repo.join("Inside/Inside.csproj"), &sdk_csproj("Inside", "TRACE"));
        write(&repo.join("Inside/Inside.cs"), "namespace N { class Inside {} }\n");
        // Same discovery scan, but outside the indexed folder.
        write(&parent.join("Outside.csproj"), &sdk_csproj("Outside", "TRACE"));
        write(&parent.join("Outside.cs"), "namespace N { class Outside {} }\n");

        let m = discover_projects(&repo, &[], 3);
        let names: Vec<&str> = m.projects.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"Inside"), "in-scope project must stay: {names:?}");
        assert!(
            !names.contains(&"Outside"),
            "a parent-directory project leaked into the indexed corpus: {names:?}"
        );

        // A configured root is also in scope.
        let m = discover_projects(&parent, &[repo.to_string_lossy().into_owned()], 3);
        assert!(
            m.projects.iter().any(|p| p.name == "Inside"),
            "a project under an explicit root must stay: {:?}",
            m.projects.iter().map(|p| &p.name).collect::<Vec<_>>()
        );

        // Explicit `<Compile Include>` entries outside the scope are dropped
        // from the project's file list too, so `?files=1` cannot advertise a
        // file the index never parsed.
        write(
            &repo.join("Inside/Inside.csproj"),
            "<Project ToolsVersion=\"4.0\">\n\
             <PropertyGroup><AssemblyName>Inside</AssemblyName></PropertyGroup>\n\
             <ItemGroup><Compile Include=\"..\\..\\Outside.cs\" /></ItemGroup>\n\
             </Project>\n",
        );
        let m = discover_projects(&repo, &[], 3);
        let inside = m.projects.iter().find(|p| p.name == "Inside").unwrap();
        assert!(
            !inside.files.iter().any(|f| f.ends_with("Outside.cs")),
            "out-of-scope source file leaked into the project: {:?}",
            inside.files
        );

        let _ = fs::remove_dir_all(parent);
    }
}
