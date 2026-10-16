//! `compile_check` — the differential Roslyn check, in Rust.
//!
//! This is a line-for-line port of the frozen `tools/csgraph/compile_check.py`.
//! The structural checks in `reorg::verify` prove the declaration set and the
//! source text survive; they cannot prove the result still COMPILES. This
//! module answers that by DIFFERENCE: compile the original corpus, compile the
//! reorganised corpus, compare the diagnostics.
//!
//! A checkout usually cannot resolve every reference (Unity and VRChat
//! assemblies may not be installed), so "does it compile with zero errors" is
//! not a usable question. "Does the reorganisation introduce a single
//! diagnostic the original did not have" is. Unity's bundled Roslyn is used
//! when present; otherwise the SAME offline .NET-SDK Roslyn that `parse-check`
//! drives compiles both sides, so the differential is meaningful with no Unity
//! install. Only when neither compiler exists is the check `skipped` — never a
//! false `ok`.
//!
//! Faithfulness notes (all mirrored deliberately):
//!
//! * `parse` drops file/line/column and keeps `(kind, code, message)`.
//! * `profile` is the counted multiset keyed by that tuple.
//! * `differential` sorts `new`/`worse`/`fixed`/`fewer` exactly the way Python
//!   sorts its tuples, and `resolvedDiagnostics` is `(fixed + fewer)[:20]` —
//!   concatenation, not a global sort.
//! * toolchain discovery tries `CSVIEWER_UNITY_DATA`, then the same glob
//!   patterns as Python (including the quirk that `_default_vrc_dirs` callers
//!   use `glob.glob` *without* `recursive=True`, so `**` there matches exactly
//!   one path segment, while the reference sweep under a VRChat package root
//!   *does* recurse).
//! * a child that cannot start, or times out, degrades to the same `skipped`
//!   result Python produces; nothing here panics.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

/// `_DIAG_RE` (exported for parity; the Python module defines it but uses
/// `_MSG_RE` for parsing).
pub const DIAG_RE: &str = r"error CS(\d+)|warning CS(\d+)";

/// A diagnostic with file/line/column stripped: `(kind, code, message)`.
pub type Diag = (String, String, String);

/// Assemblies the NZK toolkit needs. Each entry is a GROUP of candidate
/// relative paths for the SAME assembly, in preference order: Unity's layout
/// drifts between versions and flavours, so the first candidate that exists
/// under the resolved data dir is used and the rest are ignored (never both,
/// which would duplicate the identity). The group's first name is the canonical
/// name used in reports. Missing groups are simply not passed.
///
/// Observed on `/500gb/Unity/Hub/Editor/2022.3.22f1/Editor/Data`:
/// `Managed/UnityEditor/` does not exist — `UnityEditor.CoreModule.dll` and
/// `UnityEditor.dll` ship under `Managed/UnityEngine/`.
pub const UNITY_REFS: &[&[&str]] = &[
    &["UnityReferenceAssemblies/unity-4.8-api/mscorlib.dll"],
    &["UnityReferenceAssemblies/unity-4.8-api/System.dll"],
    &["UnityReferenceAssemblies/unity-4.8-api/System.Core.dll"],
    &["UnityReferenceAssemblies/unity-4.8-api/Facades/netstandard.dll"],
    &["UnityReferenceAssemblies/unity-4.8-api/Facades/System.Runtime.dll"],
    &["Managed/UnityEngine/UnityEngine.dll"],
    &["Managed/UnityEngine/UnityEngine.CoreModule.dll"],
    &["Managed/UnityEngine/UnityEngine.AnimationModule.dll"],
    &["Managed/UnityEngine/UnityEngine.PhysicsModule.dll"],
    &["Managed/UnityEngine/UnityEngine.IMGUIModule.dll"],
    &["Managed/UnityEngine/UnityEngine.AudioModule.dll"],
    &["Managed/UnityEngine/UnityEngine.ParticleSystemModule.dll"],
    &["Managed/UnityEngine/UnityEngine.TextRenderingModule.dll"],
    &["Managed/UnityEngine/UnityEngine.UIModule.dll"],
    &["Managed/UnityEngine/UnityEngine.JSONSerializeModule.dll"],
    &["Managed/UnityEngine/UnityEngine.ClothModule.dll"],
    &[
        // 2022.3 actual first; the older `Managed/UnityEditor/` layout second.
        "Managed/UnityEngine/UnityEditor.CoreModule.dll",
        "Managed/UnityEditor/UnityEditor.CoreModule.dll",
    ],
];

/// The first existing path for a `UNITY_REFS` group, relative to `data`.
pub fn unity_ref_path(data: &str, group: &[&str]) -> Option<String> {
    group
        .iter()
        .map(|rel| join_str(data, rel))
        .find(|p| Path::new(p).is_file())
}

/// `DEFAULT_DEFINES`.
pub const DEFAULT_DEFINES: &[&str] = &["UNITY_EDITOR", "UNITY_2022_3_OR_NEWER"];

/// "could not be found" codes. An INCREASE in one of these is reported but does
/// not fail the check; a diagnostic the original never produced at all always
/// does.
pub const REFERENCE_CODES: &[&str] = &["CS0234", "CS0246"];

/// Env override for extra reference-assembly directories, `:`-separated (the
/// `PATH` convention). `--refs DIR` on the command line is appended after it.
/// An entry may be a directory (every non-hidden `*.dll` below it is passed to
/// csc) or a single `*.dll`. This is the plug-in path for an off-box Unity
/// install's `Editor/Data/Managed` (plus `UnityReferenceAssemblies`) or a
/// prebuilt reference folder: point it at the folder and the differential
/// compiles against the real Unity assemblies without any code change.
pub const REFS_ENV: &str = "CSVIEWER_REFS";

/// Core-library basenames. When a caller-supplied reference folder contributes
/// one of these, the SDK's own reference pack is dropped for that compile: two
/// core-library sets in one csc invocation collide (`CS0433`, `System.Object`
/// exists in both `mscorlib` and `System.Runtime`) and the point of `--refs` is
/// to SUBSTITUTE Unity's reference assemblies, not to merge them.
pub const CORE_LIB_BASENAMES: &[&str] = &[
    "mscorlib.dll",
    "netstandard.dll",
    "system.runtime.dll",
    "system.private.corelib.dll",
];

fn basename_lower(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// True when the extra references bring their own core library, so the SDK
/// reference pack must not be passed alongside them.
fn refs_supply_core(extra_refs: &[String]) -> bool {
    extra_refs.iter().any(|r| {
        let base = basename_lower(r.strip_prefix("-r:").unwrap_or(r));
        CORE_LIB_BASENAMES.contains(&base.as_str())
    })
}

fn msg_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?P<file>.+?)\((?P<line>\d+),(?P<col>\d+)\):\s*(?P<kind>error|warning) (?P<code>CS\d+): (?P<msg>.*)$")
            .expect("compile_check::_MSG_RE is a fixed valid pattern")
    })
}

fn safe_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[^A-Za-z0-9._-]+")
            .expect("compile_check safe-name pattern is a fixed valid pattern")
    })
}

// ---------------------------------------------------------------------------
// toolchain discovery
// ---------------------------------------------------------------------------

fn home_dir() -> Option<String> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string_lossy().into_owned())
}

/// `os.path.expanduser` for the `~`/`~/...` forms the patterns use.
fn expand_user(pat: &str) -> String {
    if pat == "~" {
        if let Some(h) = home_dir() {
            return h;
        }
    } else if let Some(rest) = pat.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return format!("{}/{}", h.trim_end_matches('/'), rest);
        }
    }
    pat.to_string()
}

fn join_str(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `fnmatch` for the one wildcard the patterns use: `*` (not `/`).
fn wildcard_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let mut dp = vec![vec![false; n.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=n.len() {
            dp[i][j] = if p[i - 1] == '*' {
                dp[i - 1][j] || dp[i][j - 1]
            } else {
                dp[i - 1][j - 1] && p[i - 1] == n[j - 1]
            };
        }
    }
    dp[p.len()][n.len()]
}

/// Non-recursive `glob.glob(pat)`: `*` stays within one path segment and
/// hidden entries are not matched; only existing paths are returned, sorted
/// the way Python's `glob` sorts (the caller may reverse it).
fn glob_paths(pat: &str) -> Vec<String> {
    let expanded = expand_user(pat);
    let absolute = expanded.starts_with('/');
    let comps: Vec<&str> = expanded.split('/').filter(|c| !c.is_empty()).collect();
    let mut bases: Vec<String> = vec![if absolute { "/".to_string() } else { ".".to_string() }];
    for comp in comps {
        let mut next: Vec<String> = Vec::new();
        if comp.contains('*') {
            for base in &bases {
                let mut names: Vec<String> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(base) {
                    for entry in rd.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.starts_with('.') {
                            continue;
                        }
                        if wildcard_match(comp, &name) {
                            names.push(name);
                        }
                    }
                }
                names.sort();
                for name in names {
                    next.push(join_str(base, &name));
                }
            }
        } else {
            for base in &bases {
                next.push(join_str(base, comp));
            }
        }
        bases = next;
    }
    let mut out: Vec<String> = bases
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .collect();
    out.sort();
    out
}

/// `glob.glob(folder/**/*.dll, recursive=True)` — hidden entries are pruned.
fn glob_recursive_dlls(folder: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let walker = walkdir::WalkDir::new(folder)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'));
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            if name.ends_with(".dll") {
                out.push(entry.path().to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

/// `find_unity_data()`.
pub fn find_unity_data() -> Option<String> {
    if let Ok(env) = std::env::var("CSVIEWER_UNITY_DATA") {
        if !env.is_empty() && Path::new(&env).is_dir() {
            return Some(env);
        }
    }
    let patterns = [
        "~/Unity/Hub/Editor/*/Editor/Data",
        // `~` follows $HOME, which is not necessarily the desktop user when
        // this runs from a service or a packaged app - check the real homes too.
        "/home/*/Unity/Hub/Editor/*/Editor/Data",
        "/opt/Unity/Editor/Data",
        "/opt/unity/Editor/Data",
        "/usr/share/unity3d/Editor/Data",
        // Unity Hub installs on the data mounts this box uses (found by bounded
        // search; `/500gb` is the live 2022.3.22f1 install). `glob_paths` is
        // single-segment, so `/run/media/*/*/...` covers <user>/<uuid> mounts.
        "/500gb/Unity/Hub/Editor/*/Editor/Data",
        "/2tb/Unity/Hub/Editor/*/Editor/Data",
        "/4tb/Unity/Hub/Editor/*/Editor/Data",
        "/10tb/Unity/Hub/Editor/*/Editor/Data",
        "/nzk/Unity/Hub/Editor/*/Editor/Data",
        "/run/media/*/*/Unity/Hub/Editor/*/Editor/Data",
    ];
    for pat in patterns {
        let mut matches = glob_paths(pat);
        matches.sort();
        matches.reverse();
        for path in matches {
            if Path::new(&path)
                .join("DotNetSdkRoslyn/csc.dll")
                .is_file()
            {
                return Some(path);
            }
        }
    }
    None
}

/// `_default_vrc_dirs()` — note Python calls `glob.glob` here *without*
/// `recursive=True`, so `**` matches exactly one path segment.
pub fn default_vrc_dirs() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for pat in [
        "~/Unity/*/Assets/**/Packages/com.vrchat.*",
        "/nzk/unity/*/Packages/com.vrchat.*",
        "/2tb/@/nzk/unity/*/Packages/com.vrchat.*",
    ] {
        out.extend(glob_paths(pat));
    }
    out
}

/// `toolchain(data=None, vrc_packages=None)` -> `(prefix, refs)`.
pub fn toolchain(data: Option<&str>, vrc_packages: Option<&str>) -> (Option<Vec<String>>, Vec<String>) {
    let data = match data {
        Some(d) if !d.is_empty() => Some(d.to_string()),
        _ => find_unity_data(),
    };
    let data = match data {
        Some(d) => d,
        None => return (None, Vec::new()),
    };
    let net = join_str(&data, "NetCoreRuntime/dotnet");
    let csc = join_str(&data, "DotNetSdkRoslyn/csc.dll");
    if !(Path::new(&net).is_file() && Path::new(&csc).is_file()) {
        return (None, Vec::new());
    }
    let mut refs: Vec<String> = Vec::new();
    for group in UNITY_REFS {
        if let Some(p) = unity_ref_path(&data, group) {
            refs.push(format!("-r:{p}"));
        }
    }
    let mut folders: Vec<String> = Vec::new();
    if let Some(v) = vrc_packages {
        if !v.is_empty() {
            folders.push(v.to_string());
        }
    }
    folders.extend(default_vrc_dirs());
    for folder in folders {
        for dll in glob_recursive_dlls(&folder) {
            refs.push(format!("-r:{dll}"));
        }
    }
    (Some(vec![net, csc]), refs)
}

// ---------------------------------------------------------------------------
// plug-in reference folders (`--refs DIR` / `CSVIEWER_REFS`)
// ---------------------------------------------------------------------------

/// What [`collect_refs`] found: the directories scanned, the assemblies actually
/// passed to csc, and every requested path that was not usable. Reported in the
/// `compile-diff` document so the toolchain says exactly what it loaded.
#[derive(Clone, Debug, Default)]
pub struct RefInventory {
    pub dirs: Vec<String>,
    pub loaded: Vec<String>,
    pub missing_paths: Vec<String>,
    pub notes: Vec<String>,
}

impl RefInventory {
    pub fn to_json(&self) -> Value {
        json!({
            "pluginRefDirs": self.dirs,
            "pluginRefsLoaded": self.loaded,
            "pluginRefsLoadedCount": self.loaded.len(),
            "pluginRefsMissingPaths": self.missing_paths,
            "notes": self.notes,
        })
    }
}

/// Expand `--refs` / `CSVIEWER_REFS` entries into csc `-r:` arguments.
///
/// A directory contributes every non-hidden `*.dll` beneath it (sorted); a
/// `*.dll` file is taken as-is; anything else is recorded in `missing_paths`
/// rather than being fatal, so a partly-populated reference folder still
/// compiles and the report says exactly what was not found.
pub fn collect_refs(entries: &[String]) -> (Vec<String>, RefInventory) {
    let mut inv = RefInventory::default();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for raw in entries {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let p = Path::new(s);
        if p.is_dir() {
            inv.dirs.push(s.to_string());
            let walker = walkdir::WalkDir::new(p)
                .into_iter()
                .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'));
            let mut dlls: Vec<String> = Vec::new();
            for entry in walker.filter_map(Result::ok) {
                if entry.file_type().is_file()
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .ends_with(".dll")
                {
                    dlls.push(entry.path().to_string_lossy().into_owned());
                }
            }
            dlls.sort();
            if dlls.is_empty() {
                inv.notes
                    .push(format!("no .dll found under reference dir {s}"));
            }
            for d in dlls {
                if seen.insert(d.clone()) {
                    inv.loaded.push(d);
                } else {
                    inv.notes.push(format!("duplicate reference ignored: {d}"));
                }
            }
        } else if p.is_file() && s.to_lowercase().ends_with(".dll") {
            let path = s.to_string();
            if seen.insert(path.clone()) {
                inv.loaded.push(path);
            } else {
                inv.notes.push(format!("duplicate reference ignored: {path}"));
            }
        } else {
            inv.missing_paths.push(s.to_string());
            inv.notes
                .push(format!("reference path is missing or not a .dll: {s}"));
        }
    }
    let extra: Vec<String> = inv.loaded.iter().map(|d| format!("-r:{d}")).collect();
    (extra, inv)
}

/// Read `CSVIEWER_REFS` (`:`-separated) into reference entries.
pub fn refs_from_env() -> Vec<String> {
    match std::env::var(REFS_ENV) {
        Ok(v) => v
            .split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The toolchain's own reference view: which canonical Unity assemblies exist
/// under the resolved Unity data dir, which references the compiler itself
/// supplies (Unity's list or the SDK ref pack), which plug-in references were
/// loaded, and which canonical assemblies are missing from all of them.
/// Additive evidence in the `compile-diff` document; it never changes the
/// differential verdict.
pub fn references_report(data: Option<&str>, inv: &RefInventory) -> Value {
    let resolved_data: Option<String> = match data {
        Some(d) if !d.is_empty() => Some(d.to_string()),
        _ => find_unity_data(),
    };
    let mut present: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut compiler_label = String::new();
    let mut compiler_refs: Vec<String> = Vec::new();
    let mut compiler_refs_root = String::new();
    let mut core_replaced = false;
    match resolve_compiler(data) {
        Ok(Compiler::Unity { refs, .. }) => {
            compiler_label = "unity-roslyn".to_string();
            compiler_refs = refs
                .iter()
                .map(|r| r.strip_prefix("-r:").unwrap_or(r).to_string())
                .collect();
            compiler_refs_root = resolved_data.clone().unwrap_or_default();
        }
        Ok(Compiler::Sdk { tc, sdk }) => {
            compiler_label = format!("sdk-roslyn {sdk}");
            core_replaced = refs_supply_core(&inv.loaded);
            if core_replaced {
                compiler_refs_root =
                    "none (SDK ref pack replaced by plugin core libs)".to_string();
            } else {
                compiler_refs = tc
                    .refs
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                compiler_refs_root = tc
                    .refs
                    .first()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
        }
        Err(e) => compiler_label = format!("none: {e}"),
    }
    for r in &compiler_refs {
        present.insert(basename_lower(r));
    }

    let mut data_refs: Vec<Value> = Vec::new();
    if let Some(d) = &resolved_data {
        for group in UNITY_REFS {
            let name = basename_lower(group[0]);
            if let Some(p) = unity_ref_path(d, group) {
                present.insert(name.clone());
                data_refs.push(json!({"name": name, "path": p}));
            }
        }
    }
    for r in &inv.loaded {
        present.insert(basename_lower(r));
    }
    let missing: Vec<String> = UNITY_REFS
        .iter()
        .map(|g| basename_lower(g[0]))
        .filter(|b| !present.contains(b))
        .collect();
    let mut compiler_ref_basenames: Vec<String> = compiler_refs
        .iter()
        .map(|r| basename_lower(r))
        .collect::<std::collections::BTreeSet<String>>()
        .into_iter()
        .collect();
    compiler_ref_basenames.sort();
    json!({
        "compiler": compiler_label,
        "coreLibReplacedByPluginRefs": core_replaced,
        "compilerRefsRoot": compiler_refs_root,
        "compilerRefCount": compiler_refs.len(),
        "compilerRefBasenames": compiler_ref_basenames,
        "unityData": resolved_data,
        "unityDataRefsPresent": data_refs,
        "unityDataRefsPresentCount": data_refs.len(),
        "pluginRefDirs": inv.dirs,
        "pluginRefsLoaded": inv.loaded,
        "pluginRefsLoadedCount": inv.loaded.len(),
        "pluginRefsMissingPaths": inv.missing_paths,
        "notes": inv.notes,
        "canonicalRefs": UNITY_REFS.iter().map(|g| basename_lower(g[0])).collect::<Vec<String>>(),
        "missingCanonicalRefs": missing,
    })
}

// ---------------------------------------------------------------------------
// compiling
// ---------------------------------------------------------------------------

/// The raw csc result, mirroring `compile_files`' dict.
#[derive(Clone, Debug, Default)]
pub struct CompileResult {
    pub ok: bool,
    pub skipped: Option<String>,
    pub returncode: Option<i64>,
    pub log: String,
    pub diagnostics: Vec<Diag>,
    pub cmd: Vec<String>,
    /// Which compiler actually ran: `unity-roslyn`, `sdk-roslyn <sdk>`, or
    /// `None` when the check was skipped. Not part of the differential JSON
    /// (that shape is a frozen caller contract); surfaced by the CLI wrapper.
    pub toolchain: Option<String>,
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn exit_code(status: std::process::ExitStatus) -> i64 {
    if let Some(c) = status.code() {
        return c as i64;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return -(sig as i64);
        }
    }
    -1
}

/// Python's `OSError.__str__` shape, so a child that cannot start degrades to
/// the same `skipped` reason: `[Errno 2] No such file or directory: '/path'`.
fn python_oserror(err: &std::io::Error, filename: &str) -> String {
    match err.raw_os_error() {
        Some(code) => {
            let mut msg = err.to_string();
            if let Some(idx) = msg.find(" (os error") {
                msg.truncate(idx);
            }
            format!("[Errno {code}] {msg}: '{filename}'")
        }
        None => err.to_string(),
    }
}

/// Run `command`, merging stdout and stderr into one stream exactly the way
/// `subprocess.run(..., stderr=STDOUT)` does, with a timeout. Both streams are
/// pointed at duplicated handles of one file so writes interleave.
fn run_merged(command: &mut Command, timeout: u64) -> Result<(i64, String), String> {
    let log_path = std::env::temp_dir().join(format!(
        "csgraph-csc-{}-{}.log",
        std::process::id(),
        now_nanos()
    ));
    let file = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let file2 = file.try_clone().map_err(|e| e.to_string())?;
    command.stdout(Stdio::from(file)).stderr(Stdio::from(file2));

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(python_oserror(
                &e,
                &command.get_program().to_string_lossy(),
            ));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&log_path);
                    // Mirror `subprocess.TimeoutExpired.__str__`.
                    return Err(format!(
                        "Command '{}' timed out after {} seconds",
                        command_debug(command),
                        timeout
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&log_path);
                return Err(e.to_string());
            }
        }
    };
    let bytes = std::fs::read(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    Ok((exit_code(status), String::from_utf8_lossy(&bytes).into_owned()))
}

/// `repr` of a `Command`'s argv, close enough to Python's list repr for a
/// timeout message: `['a', 'b']`.
fn command_debug(command: &Command) -> String {
    let args: Vec<String> = command
        .get_args()
        .map(|a| format!("'{}'", a.to_string_lossy()))
        .collect();
    format!(
        "['{}', {}]",
        command.get_program().to_string_lossy(),
        args.join(", ")
    )
}

/// Which Roslyn compiler the differential drives.
///
/// Unity's bundled Roslyn is tried first exactly as the frozen Python did. On a
/// machine without Unity the check is NOT degraded to `skipped` any more: the
/// same offline .NET-SDK Roslyn that `parse-check` already uses compiles the
/// corpus instead, and the check stays differential. An explicit `data` pin is
/// never escaped — a caller who names a Unity data dir gets `skipped` when it
/// is absent (that is how tests force the no-toolchain path).
enum Compiler {
    Unity { cmd: Vec<String>, refs: Vec<String> },
    Sdk { tc: RoslynToolchain, sdk: String },
}

/// Resolve the compiler for `compile_files`. `data` is the explicit Unity data
/// pin (from `--data`); `None` allows the SDK fallback.
fn resolve_compiler(data: Option<&str>) -> Result<Compiler, String> {
    if let Some(d) = data {
        let (prefix, refs) = toolchain(Some(d), None);
        return match prefix {
            Some(cmd) => Ok(Compiler::Unity { cmd, refs }),
            None => Err("no Unity/Roslyn toolchain found".to_string()),
        };
    }
    let (prefix, refs) = toolchain(None, None);
    if let Some(cmd) = prefix {
        return Ok(Compiler::Unity { cmd, refs });
    }
    match find_roslyn_toolchain(None) {
        Ok(tc) => {
            let sdk = tc
                .sdk
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            Ok(Compiler::Sdk { tc, sdk })
        }
        Err(e) => Err(format!(
            "no Unity/Roslyn toolchain found and no .NET SDK Roslyn either: {e}"
        )),
    }
}

impl Compiler {
    fn name(&self) -> String {
        match self {
            Compiler::Unity { .. } => "unity-roslyn".to_string(),
            Compiler::Sdk { sdk, .. } => format!("sdk-roslyn {sdk}"),
        }
    }

    /// The full argv. Both branches compile a library offline with no config
    /// file and no implicit references (`-nostdlib+`); every reference is
    /// passed explicitly, which is how the SDK path stays NuGet-free.
    fn build_cmd(
        &self,
        files: &[String],
        out_dll: &str,
        defines: &[String],
        extra_refs: &[String],
    ) -> Vec<String> {
        let mut cmd: Vec<String> = match self {
            Compiler::Unity { cmd, .. } => cmd.clone(),
            Compiler::Sdk { tc, .. } => vec![
                tc.dotnet.to_string_lossy().into_owned(),
                "exec".to_string(),
                tc.csc.to_string_lossy().into_owned(),
            ],
        };
        for a in [
            "-nologo",
            "-target:library",
            "-langversion:9",
            "-nullable:disable",
            "-unsafe+",
        ] {
            cmd.push(a.to_string());
        }
        cmd.push(format!("-out:{out_dll}"));
        cmd.push("-nostdlib+".to_string());
        cmd.push("-noconfig".to_string());
        // `-define:` with no symbols is a csc command-line error (CS2006); omit
        // the switch instead of emitting an empty one.
        if !defines.is_empty() {
            cmd.push(format!("-define:{}", defines.join(",")));
        }
        match self {
            Compiler::Unity { refs, .. } => cmd.extend(refs.iter().cloned()),
            Compiler::Sdk { tc, .. } => {
                // A caller-supplied reference folder that carries its own core
                // library (Unity's `mscorlib.dll`, a netstandard set) REPLACES
                // the SDK ref pack. Passing both makes every `System.Object`
                // ambiguous (CS0433) and the compile then measures the clash,
                // not the code.
                if !refs_supply_core(extra_refs) {
                    for r in &tc.refs {
                        cmd.push(format!("-r:{}", r.display()));
                    }
                }
            }
        }
        cmd.extend(extra_refs.iter().cloned());
        cmd.extend(files.iter().cloned());
        cmd
    }
}

/// `compile_files(files, out_dll, ...)`.
pub fn compile_files(
    files: &[String],
    out_dll: &str,
    data: Option<&str>,
    defines: &[String],
    extra_refs: &[String],
    timeout: u64,
) -> CompileResult {
    let compiler = match resolve_compiler(data) {
        Ok(c) => c,
        Err(reason) => {
            return CompileResult {
                skipped: Some(reason),
                ..Default::default()
            }
        }
    };
    compile_files_with(&compiler, files, out_dll, defines, extra_refs, timeout)
}

/// [`compile_files`] against an already-resolved compiler (so a differential
/// resolves the toolchain once and can sanity-check it once).
fn compile_files_with(
    compiler: &Compiler,
    files: &[String],
    out_dll: &str,
    defines: &[String],
    extra_refs: &[String],
    timeout: u64,
) -> CompileResult {
    let cmd = compiler.build_cmd(files, out_dll, defines, extra_refs);
    let toolchain = compiler.name();

    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    match run_merged(&mut command, timeout) {
        Ok((code, log)) => {
            let diagnostics = parse(&log);
            CompileResult {
                ok: code == 0,
                skipped: None,
                returncode: Some(code),
                log,
                diagnostics,
                cmd,
                toolchain: Some(toolchain),
            }
        }
        Err(e) => CompileResult {
            skipped: Some(format!("csc failed to run: {e}")),
            cmd,
            ..Default::default()
        },
    }
}

/// `Compiler` bytes that must exist before the toolchain is trusted: a Roslyn
/// `csc.dll` that is a real, non-empty assembly. A stub `--data` tree used to be
/// accepted on the mere presence of the two paths, and a no-op process exits 0
/// with no diagnostics — a false clean pass (verifier finding 2).
fn compiler_files_plausible(compiler: &Compiler) -> Result<(), String> {
    match compiler {
        Compiler::Unity { cmd, .. } => {
            // Unity's toolchain is `[net-runtime, csc.dll, ...]`.
            let csc = cmd.get(1).map(String::as_str).unwrap_or("");
            let meta = std::fs::metadata(csc)
                .map_err(|e| format!("Toolchain csc.dll is unreadable ({csc}): {e}"))?;
            if meta.len() < 4096 {
                return Err(format!(
                    "Toolchain csc.dll is empty or truncated ({csc}, {} bytes)",
                    meta.len()
                ));
            }
            let head = std::fs::read(csc).map_err(|e| format!("Toolchain csc.dll unreadable: {e}"))?;
            if !head.starts_with(b"MZ") {
                return Err(format!(
                    "Toolchain csc.dll is not a PE assembly (no MZ header): {csc}"
                ));
            }
            Ok(())
        }
        Compiler::Sdk { tc, .. } => {
            let meta = std::fs::metadata(&tc.csc)
                .map_err(|e| format!("SDK csc.dll is unreadable ({}): {e}", tc.csc.display()))?;
            if meta.len() < 4096 {
                return Err(format!(
                    "SDK csc.dll is empty or truncated ({} bytes)",
                    meta.len()
                ));
            }
            Ok(())
        }
    }
}

/// A compiler that runs but never reports a diagnostic is not a working Roslyn:
/// the differential would otherwise compute an empty profile on both sides and
/// report `ok:true` (verifier finding 2). Compile one known-broken file and
/// require a real `CS` error, so a stubbed `--data` (or any no-op executable)
/// degrades to `skipped` instead.
fn sanity_check_compiler(compiler: &Compiler) -> Result<(), String> {
    compiler_files_plausible(compiler)?;
    let dir = make_temp_dir().map_err(|e| format!("cannot create sanity dir: {e}"))?;
    let src = dir.join("Sanity.cs");
    let out = dir.join("sanity.dll").to_string_lossy().into_owned();
    let body = "namespace __csgraph { public class __Sanity { \
                public UndefinedType M() { return null; } } }\n";
    if let Err(e) = std::fs::write(&src, body) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("cannot write sanity probe: {e}"));
    }
    let files = vec![src.to_string_lossy().into_owned()];
    let cmd = compiler.build_cmd(&files, &out, &[], &[]);
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    let res = run_merged(&mut command, 15);
    let _ = std::fs::remove_dir_all(&dir);
    match res {
        Ok((_, log)) => {
            let has_cs_error = parse(&log).iter().any(|(kind, code, _)| {
                kind == "error" && code.starts_with("CS")
            });
            if has_cs_error {
                Ok(())
            } else {
                Err(format!(
                    "toolchain did not report the expected CS diagnostic (not a working Roslyn): {}",
                    tail_trim(&log)
                ))
            }
        }
        Err(e) => Err(format!("toolchain sanity check failed to run: {e}")),
    }
}

// ---------------------------------------------------------------------------
// diagnostic parse / profile
// ---------------------------------------------------------------------------

/// `parse(log)` — `[(kind, code, message)]` with positions stripped.
pub fn parse(log: &str) -> Vec<Diag> {
    let mut out: Vec<Diag> = Vec::new();
    for line in log.split('\n') {
        let line = line.trim();
        let caps = match msg_re().captures(line) {
            Some(c) => c,
            None => continue,
        };
        let kind = caps.name("kind").map(|m| m.as_str()).unwrap_or("");
        let code = caps.name("code").map(|m| m.as_str()).unwrap_or("");
        let msg = caps.name("msg").map(|m| m.as_str().trim()).unwrap_or("");
        out.push((kind.to_string(), code.to_string(), msg.to_string()));
    }
    out
}

/// `parse` PLUS positions: `[{kind, code, file, line, col, message}]`.
///
/// Additive evidence for the `compile-diff` document. The differential itself
/// still keys on the position-free `(kind, code, message)` tuples — that shape
/// is the frozen caller contract — so this only ever ADDS keys.
pub fn parse_full(log: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for line in log.split('\n') {
        let line = line.trim();
        let caps = match msg_re().captures(line) {
            Some(c) => c,
            None => continue,
        };
        let g = |n: &str| caps.name(n).map(|m| m.as_str()).unwrap_or("");
        out.push(json!({
            "kind": g("kind"),
            "code": g("code"),
            "file": g("file"),
            "line": g("line").parse::<i64>().unwrap_or(0),
            "col": g("col").parse::<i64>().unwrap_or(0),
            "message": g("msg").trim(),
        }));
    }
    out
}

/// `profile(diags)` — counted multiset keyed by `(kind, code, message)`.
pub fn profile(diags: &[Diag]) -> BTreeMap<Diag, i64> {    let mut out: BTreeMap<Diag, i64> = BTreeMap::new();
    for d in diags {
        *out.entry(d.clone()).or_insert(0) += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// materialisation
// ---------------------------------------------------------------------------

/// `_materialize(texts, dest)`; `pairs` must already be in Python's sorted
/// order (the caller sorts).
fn materialize(pairs: &[(String, String)], dest: &Path) {
    if std::fs::create_dir_all(dest).is_err() {
        return;
    }
    for (i, (name, text)) in pairs.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let safe = safe_re()
            .replace_all(name, "_")
            .trim_matches('_')
            .to_string();
        let mut safe = if safe.is_empty() {
            "src".to_string()
        } else {
            safe
        };
        if !safe.ends_with(".cs") {
            safe.push_str(".cs");
        }
        let _ = std::fs::write(dest.join(format!("{:04}_{}", i, safe)), text);
    }
}

/// `_cs_files(root)`.
fn cs_files(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file()
            && entry.file_name().to_string_lossy().ends_with(".cs")
        {
            out.push(entry.path().to_string_lossy().into_owned());
        }
    }
    out.sort();
    out
}

fn make_temp_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0..1000u32 {
        let cand = base.join(format!(
            "csviewer-compile-{}-{}-{}",
            std::process::id(),
            now_nanos(),
            attempt
        ));
        match std::fs::create_dir(&cand) {
            Ok(()) => return Ok(cand),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a temporary directory",
    ))
}

// ---------------------------------------------------------------------------
// differential
// ---------------------------------------------------------------------------

/// `differential(original_texts, emitted, ...)`.
///
/// `original_texts` is the indexed display-path -> text map; `emitted` is the
/// reorganisation's `(path, text)` list. The JSON shape is a frozen caller
/// contract; [`differential_ex`] additionally returns the compiler that ran.
pub fn differential(
    original_texts: &HashMap<String, String>,
    emitted: &[(String, String)],
    data: Option<&str>,
    defines: &[String],
    extra_refs: &[String],
    timeout: u64,
    workdir: Option<&str>,
) -> Value {
    differential_ex(
        original_texts,
        emitted,
        data,
        defines,
        extra_refs,
        timeout,
        workdir,
    )
    .0
}

/// [`differential`] plus the compiler that produced it (`None` when skipped).
pub fn differential_ex(
    original_texts: &HashMap<String, String>,
    emitted: &[(String, String)],
    data: Option<&str>,
    defines: &[String],
    extra_refs: &[String],
    timeout: u64,
    workdir: Option<&str>,
) -> (Value, Option<String>) {
    if emitted.is_empty() {
        return (json!({"ok": false, "error": "nothing emitted"}), None);
    }
    let tmp: PathBuf = match workdir {
        Some(w) => PathBuf::from(w),
        None => match make_temp_dir() {
            Ok(d) => d,
            Err(e) => {
                return (
                    json!({"ok": false, "skipped": format!("csc failed to run: {e}")}),
                    None,
                )
            }
        },
    };
    let keep = workdir.is_some();
    let base_dir = tmp.join("baseline");
    let cand_dir = tmp.join("candidate");

    let mut orig: Vec<(String, String)> = original_texts
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    orig.sort();
    let mut emit: Vec<(String, String)> = emitted.to_vec();
    emit.sort();
    materialize(&orig, &base_dir);
    materialize(&emit, &cand_dir);

    let base_files = cs_files(&base_dir);
    let cand_files = cs_files(&cand_dir);
    if base_files.is_empty() || cand_files.is_empty() {
        if !keep {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        return (json!({"ok": false, "skipped": "no compilable sources"}), None);
    }

    let base_dll = tmp.join("baseline.dll").to_string_lossy().into_owned();
    let cand_dll = tmp.join("candidate.dll").to_string_lossy().into_owned();
    // Resolve the compiler once and prove it is a working Roslyn before trusting
    // any result from it (verifier finding 2: a stub `--data` with an empty
    // csc.dll and a no-op dotnet used to read as a clean unity-roslyn pass).
    let compiler = match resolve_compiler(data) {
        Ok(c) => c,
        Err(reason) => {
            if !keep {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            return (json!({"ok": false, "skipped": reason}), None);
        }
    };
    if let Err(reason) = sanity_check_compiler(&compiler) {
        if !keep {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        return (json!({"ok": false, "skipped": reason}), None);
    }
    let base = compile_files_with(&compiler, &base_files, &base_dll, defines, extra_refs, timeout);
    let cand = compile_files_with(&compiler, &cand_files, &cand_dll, defines, extra_refs, timeout);
    if !keep {
        let _ = std::fs::remove_dir_all(&tmp);
    }

    if base.skipped.is_some() || cand.skipped.is_some() {
        let reason = cand.skipped.clone().or_else(|| base.skipped.clone());
        return (json!({"ok": false, "skipped": reason}), None);
    }
    // A non-zero compiler exit with no parseable diagnostic is a toolchain that
    // did not actually answer, not an empty (clean) compile.
    for (side, res) in [("baseline", &base), ("candidate", &cand)] {
        if res.returncode != Some(0) && res.diagnostics.is_empty() {
            return (
                json!({
                    "ok": false,
                    "skipped": format!(
                        "{side} compiler exited {:?} without a parseable CS diagnostic: {}",
                        res.returncode,
                        tail_trim(&res.log)
                    ),
                }),
                None,
            );
        }
    }
    let toolchain = cand.toolchain.clone().or_else(|| base.toolchain.clone());

    let bp = profile(&base.diagnostics);
    let cp = profile(&cand.diagnostics);

    let mut new: Vec<(Diag, i64)> = cp
        .iter()
        .filter(|(k, _)| !bp.contains_key(*k))
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    new.sort();
    let mut worse: Vec<(Diag, i64, i64)> = cp
        .iter()
        .filter(|(k, v)| bp.get(*k).map_or(false, |b| **v > *b))
        .map(|(k, v)| (k.clone(), bp[k], *v))
        .collect();
    worse.sort();
    let mut fixed: Vec<(Diag, i64, i64)> = bp
        .iter()
        .filter(|(k, _)| !cp.contains_key(*k))
        .map(|(k, v)| (k.clone(), *v, 0))
        .collect();
    fixed.sort();
    let mut fewer: Vec<(Diag, i64, i64)> = cp
        .iter()
        .filter(|(k, v)| bp.get(*k).map_or(false, |b| **v < *b))
        .map(|(k, v)| (k.clone(), bp[k], *v))
        .collect();
    fewer.sort();

    let structural: Vec<(Diag, i64, i64)> = worse
        .iter()
        .filter(|w| !REFERENCE_CODES.contains(&w.0 .1.as_str()))
        .cloned()
        .collect();

    let errors = |p: &BTreeMap<Diag, i64>, kind: &str| -> i64 {
        p.iter()
            .filter(|(k, _)| k.0 == kind)
            .map(|(_, v)| *v)
            .sum()
    };

    (
        json!({
            "ok": new.is_empty() && structural.is_empty(),
            "skipped": Value::Null,
            "baseline": {
                "returncode": base.returncode,
                "errors": errors(&bp, "error"),
                "warnings": errors(&bp, "warning"),
            },
            "candidate": {
                "returncode": cand.returncode,
                "errors": errors(&cp, "error"),
                "warnings": errors(&cp, "warning"),
            },
            // Additive machine-readable per-diagnostic evidence (file/line/col).
            // Existing keys above are unchanged for caller compatibility.
            "baselineDiagnostics": parse_full(&base.log),
            "candidateDiagnostics": parse_full(&cand.log),
            // a diagnostic the original did not have, or the same one more often
            "newDiagnostics": new.iter().map(|(k, c)| json!({
                "kind": k.0, "code": k.1, "message": k.2, "count": c,
            })).collect::<Vec<Value>>(),
            "worseDiagnostics": worse.iter().map(|(k, b, c)| json!({
                "kind": k.0, "code": k.1, "message": k.2,
                "baseline": b, "candidate": c,
            })).collect::<Vec<Value>>(),
            "structuralDiagnostics": structural.iter().map(|(k, b, c)| json!({
                "kind": k.0, "code": k.1, "message": k.2,
                "baseline": b, "candidate": c,
            })).collect::<Vec<Value>>(),
            // fewer diagnostics is not a failure, but it is worth reporting
            "resolvedDiagnostics": fixed.iter().chain(fewer.iter()).take(20).map(|(k, b, c)| json!({
                "code": k.1, "message": k.2, "baseline": b, "candidate": c,
            })).collect::<Vec<Value>>(),
        }),
        toolchain,
    )
}

// ---------------------------------------------------------------------------
// local dotnet-Roslyn parse gate (`csgraph parse-check`)
// ---------------------------------------------------------------------------
//
// The differential check above prefers Unity's bundled Roslyn, but on a
// machine without Unity it falls back to the .NET SDK's own Roslyn (see
// `resolve_compiler`) instead of degrading to `skipped`. This is the local,
// offline parse gate: it builds a tiny driver from `core/roslyn/ParseCheck.cs`
// with the SDK's own `csc.dll` (no NuGet, no network), then runs it to collect
// real `CSharpSyntaxTree` parse diagnostics. A prebuilt driver bundle staged by
// the AppImage is used as-is when present. If anything is missing the result
// is `skipped` with a concrete reason — never a false `ok`.

/// Driver source, embedded so the CLI needs no files shipped beside it.
pub const ROSLYN_DRIVER_SRC: &str = include_str!("../roslyn/ParseCheck.cs");

/// Env override for the `dotnet` executable. Pointing it at a missing path is
/// how callers (and the unit tests) force the `skipped` path.
pub const DOTNET_ENV: &str = "CSGRAPH_DOTNET";

/// A discovered local .NET SDK able to build and run the Roslyn driver.
#[derive(Clone, Debug)]
pub struct RoslynToolchain {
    pub dotnet: PathBuf,
    pub sdk: PathBuf,
    pub csc: PathBuf,
    pub bincore: PathBuf,
    pub runtimeconfig: PathBuf,
    pub refs: Vec<PathBuf>,
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `dotnet` from the explicit override, then `CSGRAPH_DOTNET`, then PATH.
fn find_dotnet(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(raw) = explicit {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "{DOTNET_ENV} points at a missing dotnet executable: {raw}"
        ));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("dotnet");
            if cand.is_file() {
                return Ok(cand);
            }
        }
    }
    for cand in [
        "/usr/bin/dotnet",
        "/usr/local/bin/dotnet",
        "/usr/share/dotnet/dotnet",
    ] {
        if Path::new(cand).is_file() {
            return Ok(PathBuf::from(cand));
        }
    }
    Err(format!(
        "dotnet not found on PATH (set {DOTNET_ENV} to override)"
    ))
}

/// Locate a local .NET SDK with Roslyn plus its reference pack.
///
/// `override_exe` forces the `dotnet` path (and therefore the SDK root);
/// `None` uses `CSGRAPH_DOTNET` and then PATH.
pub fn find_roslyn_toolchain(override_exe: Option<&str>) -> Result<RoslynToolchain, String> {
    let explicit = match override_exe {
        Some(s) => Some(s.to_string()),
        None => env_string(DOTNET_ENV),
    };
    let dotnet = find_dotnet(explicit.as_deref())?;
    let dotnet = std::fs::canonicalize(&dotnet).unwrap_or(dotnet);
    let root = dotnet
        .parent()
        .ok_or_else(|| format!("dotnet has no parent directory: {}", dotnet.display()))?
        .to_path_buf();

    // newest SDK install that actually carries Roslyn
    let mut sdk_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root.join("sdk")) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.join("Roslyn/bincore/csc.dll").is_file() {
                sdk_dirs.push(p);
            }
        }
    }
    sdk_dirs.sort();
    let sdk = sdk_dirs.pop().ok_or_else(|| {
        format!(
            "no .NET SDK with Roslyn/bincore/csc.dll under {}",
            root.join("sdk").display()
        )
    })?;
    let bincore = sdk.join("Roslyn/bincore");
    let csc = bincore.join("csc.dll");
    let runtimeconfig = bincore.join("csc.runtimeconfig.json");
    if !runtimeconfig.is_file() {
        return Err(format!(
            "Roslyn runtimeconfig missing: {}",
            runtimeconfig.display()
        ));
    }

    // newest reference pack (ref/netX.Y/*.dll) for compiling the driver
    let mut ref_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root.join("packs/Microsoft.NETCore.App.Ref")) {
        for entry in rd.flatten() {
            if let Ok(rd2) = std::fs::read_dir(entry.path().join("ref")) {
                for v in rd2.flatten() {
                    if v.path().is_dir() {
                        ref_dirs.push(v.path());
                    }
                }
            }
        }
    }
    ref_dirs.sort();
    let ref_dir = ref_dirs.pop().ok_or_else(|| {
        format!(
            "no Microsoft.NETCore.App.Ref reference pack under {}",
            root.join("packs").display()
        )
    })?;
    let mut refs: Vec<PathBuf> = std::fs::read_dir(&ref_dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "dll").unwrap_or(false))
        .collect();
    refs.sort();
    if refs.is_empty() {
        return Err(format!("reference pack is empty: {}", ref_dir.display()));
    }
    Ok(RoslynToolchain {
        dotnet,
        sdk,
        csc,
        bincore,
        runtimeconfig,
        refs,
    })
}

fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Where the compiled driver is cached: the cargo target tree when we can find
/// it (gitignored), otherwise the temp dir. `CSGRAPH_ROSLYN_BUILD` overrides.
fn roslyn_build_root() -> PathBuf {
    if let Some(v) = env_string("CSGRAPH_ROSLYN_BUILD") {
        return PathBuf::from(v);
    }
    if let Some(v) = env_string("CARGO_TARGET_DIR") {
        return PathBuf::from(v).join("csgraph-roslyn");
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.as_path();
        while let Some(parent) = p.parent() {
            if parent.file_name().map(|n| n == "target").unwrap_or(false) {
                return parent.join("csgraph-roslyn");
            }
            p = parent;
        }
    }
    std::env::temp_dir().join("csgraph-roslyn")
}

fn build_driver_into(tc: &RoslynToolchain, dir: &Path, timeout: u64) -> Result<(), String> {
    let src = dir.join("ParseCheck.cs");
    std::fs::write(&src, ROSLYN_DRIVER_SRC).map_err(|e| e.to_string())?;
    let rsp = dir.join("refs.rsp");
    let mut body = String::new();
    for r in &tc.refs {
        body.push_str(&format!("-r:\"{}\"\n", r.display()));
    }
    std::fs::write(&rsp, body).map_err(|e| e.to_string())?;
    let out = dir.join("ParseCheck.dll");
    // Runtime dependencies sit next to the driver: no deps.json, no NuGet.
    for name in ["Microsoft.CodeAnalysis.dll", "Microsoft.CodeAnalysis.CSharp.dll"] {
        let from = tc.bincore.join(name);
        if !from.is_file() {
            return Err(format!("missing Roslyn assembly: {}", from.display()));
        }
        std::fs::copy(&from, dir.join(name)).map_err(|e| e.to_string())?;
    }
    std::fs::copy(&tc.runtimeconfig, dir.join("ParseCheck.runtimeconfig.json"))
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(&tc.dotnet);
    command
        .arg("exec")
        .arg(&tc.csc)
        .arg("-nologo")
        .arg("-noconfig")
        .arg(format!("@{}", rsp.display()))
        .arg(format!(
            "-r:{}",
            tc.bincore.join("Microsoft.CodeAnalysis.dll").display()
        ))
        .arg(format!(
            "-r:{}",
            tc.bincore.join("Microsoft.CodeAnalysis.CSharp.dll").display()
        ))
        .arg("-langversion:latest")
        .arg("-nullable:disable")
        .arg("-target:exe")
        .arg(format!("-out:{}", out.display()))
        .arg(&src);
    match run_merged(&mut command, timeout) {
        Ok((0, _)) => Ok(()),
        Ok((code, log)) => Err(format!(
            "roslyn driver build failed (csc exit {code}): {}",
            tail_trim(&log)
        )),
        Err(e) => Err(format!("roslyn driver build could not run: {e}")),
    }
}

/// Build the driver on demand (content-addressed so concurrent callers share a
/// cache entry) and return the `ParseCheck.dll` path.
pub fn ensure_roslyn_driver(tc: &RoslynToolchain, timeout: u64) -> Result<PathBuf, String> {
    let mut stamp = ROSLYN_DRIVER_SRC.to_string();
    stamp.push('\n');
    stamp.push_str(&tc.dotnet.to_string_lossy());
    stamp.push('\n');
    stamp.push_str(&tc.csc.to_string_lossy());
    stamp.push('\n');
    for r in &tc.refs {
        stamp.push_str(&r.to_string_lossy());
        stamp.push('\n');
    }
    let root = roslyn_build_root();
    let cache = root.join(short_hash(&stamp));
    let driver = cache.join("ParseCheck.dll");
    if driver.is_file() {
        return Ok(driver);
    }
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let tmp = root.join(format!("build-{}-{}", std::process::id(), now_nanos()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    if let Err(e) = build_driver_into(tc, &tmp, timeout) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if !tmp.join("ParseCheck.dll").is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("csc reported success but produced no ParseCheck.dll".to_string());
    }
    match std::fs::rename(&tmp, &cache) {
        Ok(()) => Ok(driver),
        Err(_) => {
            // another process won the race — use the cache if it is complete
            let _ = std::fs::remove_dir_all(&tmp);
            if driver.is_file() {
                Ok(driver)
            } else {
                Err(format!(
                    "cannot populate roslyn driver cache at {}",
                    cache.display()
                ))
            }
        }
    }
}

fn tail_trim(log: &str) -> String {
    let t = log.trim();
    let n = t.chars().count();
    if n > 400 {
        format!("…{}", t.chars().skip(n - 400).collect::<String>())
    } else {
        t.to_string()
    }
}

/// Parse the driver's stdout into `(roslynVersion, filesParsed, diagnostics)`.
///
/// Noise (a .NET host banner, a crash trace) is not JSON and is rejected here,
/// surfacing upstream as a `skipped` reason rather than a false result.
pub fn parse_driver_output(raw: &str) -> Result<(String, usize, Vec<Value>), String> {
    for line in raw.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(obj) = v.as_object() else { continue };
        if !obj.contains_key("diagnostics") {
            continue;
        }
        let version = obj
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let files = obj.get("files").and_then(Value::as_u64).unwrap_or(0) as usize;
        let empty: Vec<Value> = Vec::new();
        let raw_diags = obj
            .get("diagnostics")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let mut out: Vec<Value> = Vec::with_capacity(raw_diags.len());
        for d in raw_diags {
            out.push(json!({
                "severity": d.get("severity").and_then(Value::as_str).unwrap_or("error"),
                "id": d.get("id").and_then(Value::as_str).unwrap_or(""),
                "file": d.get("file").and_then(Value::as_str).unwrap_or(""),
                "line": d.get("line").and_then(Value::as_i64).unwrap_or(0),
                "col": d.get("col").and_then(Value::as_i64).unwrap_or(0),
                "message": d.get("message").and_then(Value::as_str).unwrap_or(""),
            }));
        }
        return Ok((version, files, out));
    }
    let shown = if raw.trim().is_empty() {
        "<empty stdout>".to_string()
    } else {
        tail_trim(raw)
    };
    Err(format!("roslyn driver produced no JSON report: {shown}"))
}

fn skipped_report(files: usize, reason: String) -> Value {
    json!({
        "ok": false,
        "toolchain": Value::Null,
        "files": files,
        "diagnostics": [],
        "count": 0,
        "skipped": reason,
    })
}

// ---------------------------------------------------------------------------
// prebuilt driver bundle (AppImage) + apply-path policy
// ---------------------------------------------------------------------------

/// Env override for a directory holding a PREBUILT Roslyn driver:
/// `ParseCheck.dll`, `ParseCheck.runtimeconfig.json`,
/// `Microsoft.CodeAnalysis.dll`, `Microsoft.CodeAnalysis.CSharp.dll`. The
/// AppImage stages exactly this directory so a shipped build can gate without a
/// .NET SDK on the host.
pub const ROSLYN_DRIVER_ENV: &str = "CSGRAPH_ROSLYN_DRIVER";

/// The files a prebuilt driver directory must contain to be usable.
pub const DRIVER_BUNDLE_FILES: &[&str] = &[
    "ParseCheck.dll",
    "ParseCheck.runtimeconfig.json",
    "Microsoft.CodeAnalysis.dll",
    "Microsoft.CodeAnalysis.CSharp.dll",
];

fn valid_driver_dir(dir: &Path) -> bool {
    DRIVER_BUNDLE_FILES
        .iter()
        .all(|n| dir.join(n).is_file())
}

/// The app home (`usr/`'s parent) of a packaged build, if we are running from
/// one: `CSVIEWER_HOME`, then `APPDIR`, then the `<...>/usr/bin/csviewer`
/// layout derived from the current executable.
fn app_home() -> Option<PathBuf> {
    if let Some(v) = env_string("CSVIEWER_HOME") {
        return Some(PathBuf::from(v));
    }
    if let Some(v) = env_string("APPDIR") {
        return Some(PathBuf::from(v));
    }
    let exe = std::env::current_exe().ok()?;
    let usr = exe.parent()?.parent()?;
    if usr.file_name().map(|n| n == "usr").unwrap_or(false) {
        usr.parent().map(PathBuf::from)
    } else {
        None
    }
}

/// Candidate prebuilt-driver directories, most explicit first.
fn driver_bundle_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(h) = app_home() {
        out.push(h.join("usr/share/csviewer/roslyn"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("roslyn"));
        }
    }
    out
}

/// A dotnet host shipped beside the bundle (`usr/lib/csviewer-dotnet/dotnet`),
/// or inside the bundle itself.
fn bundled_dotnet_host(bundle: &Path) -> Option<PathBuf> {
    let local = bundle.join("dotnet");
    if local.is_file() {
        return Some(local);
    }
    // usr/share/csviewer/roslyn -> usr/lib/csviewer-dotnet/dotnet
    let usr = bundle.parent()?.parent()?.parent()?;
    let host = usr.join("lib/csviewer-dotnet/dotnet");
    if host.is_file() {
        return Some(host);
    }
    None
}

fn bundle_dotnet(bundle: &Path) -> Result<PathBuf, String> {
    if let Some(v) = env_string(DOTNET_ENV) {
        let p = PathBuf::from(&v);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "{DOTNET_ENV} points at a missing dotnet executable: {v}"
        ));
    }
    if let Some(host) = bundled_dotnet_host(bundle) {
        return Ok(host);
    }
    find_dotnet(None).map_err(|e| {
        format!(
            "bundled Roslyn driver at {} needs a dotnet host: {e} \
             (set {DOTNET_ENV} or ship usr/lib/csviewer-dotnet)",
            bundle.display()
        )
    })
}

/// The prebuilt `(dotnet, ParseCheck.dll)` pair when a shipped bundle exists.
/// `Ok(None)` means "no bundle, build the driver from source as before".
fn prebuilt_driver(override_exe: Option<&str>) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let mut dirs: Vec<(PathBuf, bool)> = Vec::new();
    if let Some(v) = env_string(ROSLYN_DRIVER_ENV) {
        dirs.push((PathBuf::from(v), true));
    }
    for d in driver_bundle_dirs() {
        dirs.push((d, false));
    }
    for (dir, required) in dirs {
        if !valid_driver_dir(&dir) {
            if required {
                return Err(format!(
                    "{ROSLYN_DRIVER_ENV} is not a complete Roslyn driver bundle \
                     (need {}): {}",
                    DRIVER_BUNDLE_FILES.join(", "),
                    dir.display()
                ));
            }
            continue;
        }
        let dotnet = match override_exe {
            Some(p) => {
                let pb = PathBuf::from(p);
                if !pb.is_file() {
                    return Err(format!(
                        "{DOTNET_ENV} points at a missing dotnet executable: {p}"
                    ));
                }
                pb
            }
            None => bundle_dotnet(&dir)?,
        };
        return Ok(Some((dotnet, dir.join("ParseCheck.dll"))));
    }
    Ok(None)
}

/// How the apply path must treat a validity-gate result.
#[derive(Clone, Debug)]
pub struct ValidityPolicy {
    /// `"roslyn"` when the gate really ran, else `"skipped:<reason>"`.
    pub validated: String,
    /// True when no real gate ran, so the write would land unverified.
    pub unverified: bool,
    /// A ready-to-return refusal when `require_validated` is set and no gate ran.
    pub refusal: Option<Value>,
}

/// Map a gate report (`parse_check`/`parse_check_with` shape) onto the
/// apply-path policy: a `skipped` gate is NEVER a pass. With
/// `require_validated:true` the write must be refused; otherwise it proceeds
/// but the payload must carry `unverified:true`.
pub fn validity_policy(gate: &Value, require_validated: bool) -> ValidityPolicy {
    let skipped = gate.get("skipped").cloned().unwrap_or(Value::Null);
    if skipped.is_null() {
        return ValidityPolicy {
            validated: "roslyn".to_string(),
            unverified: false,
            refusal: None,
        };
    }
    let reason = skipped
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| skipped.to_string());
    let validated = format!("skipped:{reason}");
    let refusal = if require_validated {
        Some(json!({
            "ok": false,
            "validated": validated,
            "unverified": true,
            "error": format!(
                "refusing to write unverified C#: the validity gate was skipped \
                 ({reason}); pass `require_validated:false` to write anyway"
            ),
        }))
    } else {
        None
    };
    ValidityPolicy {
        validated,
        unverified: true,
        refusal,
    }
}

// ---------------------------------------------------------------------------
// `csgraph compile-diff` — explicit baseline/candidate compile differential
// ---------------------------------------------------------------------------

fn collect_cs(root: &Path) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter() {
        // A walk error (unreadable subdirectory) must not silently shrink the
        // compared tree: skip nothing.
        let entry = entry.map_err(|e| format!("cannot walk {}: {e}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".cs") && !name.ends_with(".cs.nzk") {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        let bytes = std::fs::read(entry.path())
            .map_err(|e| format!("cannot read {}: {e}", entry.path().display()))?;
        // Lossy-decode, never drop: the rest of the engine reads sources as
        // bytes and decodes this way. A non-UTF-8 (Latin-1/UTF-16) file is still
        // compiled — its invalid bytes become U+FFFD — so the gate can never
        // report a clean pass over a file it did not read (verifier finding 1).
        out.push((rel, String::from_utf8_lossy(&bytes).into_owned()));
    }
    out.sort();
    Ok(out)
}

/// Print a `skipped` document and return exit 3 — the honest outcome when an
/// input cannot be read at all. Never a pass.
fn cli_skipped(reason: String) -> Result<i32, String> {
    let doc = json!({"ok": false, "skipped": reason, "toolchain": Value::Null});
    println!(
        "{}",
        serde_json::to_string(&doc).map_err(|e| e.to_string())?
    );
    Ok(3)
}

/// `csgraph compile-diff --baseline DIR... --candidate DIR...` — run the
/// differential Roslyn check on explicit trees. This is the file-level surface
/// the `probe_compile.py` gate uses, and it is the same `differential_ex` the
/// reorg/index compile arm calls.
///
/// `--refs DIR` (repeatable, also `CSVIEWER_REFS`) plugs in extra reference
/// assemblies without a code change: a directory contributes every non-hidden
/// `*.dll` below it, a plain `*.dll` is taken as-is. A folder that carries its
/// own core library replaces the SDK ref pack (see `refs_supply_core`). The
/// printed document always carries a `references` object naming exactly which
/// references were loaded and which canonical Unity assemblies are missing.
///
/// Exit codes match `parse-check`: `0` no new diagnostics, `1` new/worse
/// diagnostics, `3` skipped (never a pass), `2` usage error.
///
/// [`compile_diff_report`] is the in-memory half: the same report `--baseline`/
/// `--candidate` trees produce, built from two source sets the caller already
/// holds. The MCP `compile_diff` tool uses it so a workspace can be compiled
/// against Unity reference assemblies with no Unity process, no Editor focus and
/// no Unity licence — only the local Roslyn that `parse-check` already drives.
pub fn compile_diff_report(
    original: &HashMap<String, String>,
    emitted: &[(String, String)],
    data: Option<&str>,
    defines: &[String],
    refs: &[String],
    timeout: u64,
) -> Value {
    let timeout = timeout.clamp(1, 90);
    // Plug-in reference assemblies: explicit then `CSVIEWER_REFS`.
    let mut ref_entries: Vec<String> = refs.to_vec();
    ref_entries.extend(refs_from_env());
    let (extra_refs, inv) = collect_refs(&ref_entries);
    let (mut doc, toolchain) = differential_ex(
        original,
        emitted,
        data,
        defines,
        &extra_refs,
        timeout,
        None,
    );
    if let Value::Object(ref mut m) = doc {
        m.insert("toolchain".to_string(), json!(toolchain));
        m.insert(
            "references".to_string(),
            references_report(data, &inv),
        );
    }
    doc
}

pub fn cli_compile_diff(args: &[String]) -> Result<i32, String> {
    let mut baseline: Vec<PathBuf> = Vec::new();
    let mut candidate: Vec<PathBuf> = Vec::new();
    let mut defines: Vec<String> = DEFAULT_DEFINES.iter().map(|s| (*s).to_string()).collect();
    let mut data: Option<String> = None;
    let mut refs: Vec<String> = Vec::new();
    let mut timeout: u64 = 25;
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", args[*i - 1]))
        };
        match a {
            "--baseline" => baseline.push(PathBuf::from(take(&mut i)?)),
            "--candidate" => candidate.push(PathBuf::from(take(&mut i)?)),
            "--define" => defines.push(take(&mut i)?),
            "--data" => data = Some(take(&mut i)?),
            "--refs" => refs.push(take(&mut i)?),
            "--timeout" => {
                let raw = take(&mut i)?;
                timeout = raw
                    .parse::<u64>()
                    .map_err(|_| format!("--timeout needs a positive integer, got {raw:?}"))?;
            }
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 1;
    }
    if baseline.is_empty() {
        return Err("--baseline DIR is required".to_string());
    }
    if candidate.is_empty() {
        return Err("--candidate DIR is required".to_string());
    }
    let mut original: HashMap<String, String> = HashMap::new();
    for (n, dir) in baseline.iter().enumerate() {
        if !dir.is_dir() {
            return Err(format!("--baseline is not a directory: {}", dir.display()));
        }
        let pairs = match collect_cs(dir) {
            Ok(p) => p,
            Err(e) => return cli_skipped(e),
        };
        for (rel, text) in pairs {
            original.insert(format!("{n}/{rel}"), text);
        }
    }
    let mut emitted: Vec<(String, String)> = Vec::new();
    for (n, dir) in candidate.iter().enumerate() {
        if !dir.is_dir() {
            return Err(format!("--candidate is not a directory: {}", dir.display()));
        }
        let pairs = match collect_cs(dir) {
            Ok(p) => p,
            Err(e) => return cli_skipped(e),
        };
        for (rel, text) in pairs {
            emitted.push((format!("{n}/{rel}"), text));
        }
    }
    let doc = compile_diff_report(&original, &emitted, data.as_deref(), &defines, &refs, timeout);
    println!(
        "{}",
        serde_json::to_string(&doc).map_err(|e| e.to_string())?
    );
    let skipped = !doc["skipped"].is_null();
    let ok = doc["ok"].as_bool().unwrap_or(false);
    Ok(if skipped {
        3
    } else if ok {
        0
    } else {
        1
    })
}


/// Run the local Roslyn parse gate over `files`.
///
/// A prebuilt driver bundle ships in the AppImage (`CSGRAPH_ROSLYN_DRIVER` /
/// `CSVIEWER_HOME`); when it is absent the driver is built from the embedded
/// source with the local SDK's Roslyn, exactly as before. `toolchain_override`
/// forces the `dotnet` executable; `None` reads `CSGRAPH_DOTNET`. A missing
/// toolchain, an unbuildable driver, or an empty file list yields `ok:false`
/// plus a concrete non-null `skipped` reason — never a false `ok:true`.
pub fn parse_check_with(files: &[String], toolchain_override: Option<&str>, timeout: u64) -> Value {
    let n = files.len();
    if n == 0 {
        return skipped_report(0, "no C# files to check".to_string());
    }
    let (dotnet, driver, bundled) = match prebuilt_driver(toolchain_override) {
        Ok(Some((dotnet, driver))) => (dotnet, driver, true),
        Ok(None) => {
            let tc = match find_roslyn_toolchain(toolchain_override) {
                Ok(t) => t,
                Err(e) => return skipped_report(n, e),
            };
            let driver = match ensure_roslyn_driver(&tc, timeout.min(90)) {
                Ok(d) => d,
                Err(e) => return skipped_report(n, e),
            };
            (tc.dotnet.clone(), driver, false)
        }
        Err(e) => return skipped_report(n, e),
    };
    let existing: Vec<String> = files
        .iter()
        .filter(|f| Path::new(f.as_str()).is_file())
        .cloned()
        .collect();
    if existing.is_empty() {
        return skipped_report(n, "none of the requested files exist".to_string());
    }
    let mut command = Command::new(&dotnet);
    command.arg(&driver).args(&existing);
    let (code, log) = match run_merged(&mut command, timeout) {
        Ok(v) => v,
        Err(e) => return skipped_report(n, format!("roslyn driver failed to run: {e}")),
    };
    if code != 0 {
        return skipped_report(n, format!("roslyn driver exited {code}: {}", tail_trim(&log)));
    }
    match parse_driver_output(&log) {
        Ok((version, parsed, diagnostics)) => {
            if parsed == 0 {
                // nothing was actually read: do not report a clean pass
                return skipped_report(n, "roslyn driver parsed no files".to_string());
            }
            let count = diagnostics.len();
            let label = if bundled { "bundled-roslyn" } else { "dotnet-roslyn" };
            json!({
                "ok": count == 0,
                "toolchain": format!("{label} {version}"),
                "files": parsed,
                "diagnostics": diagnostics,
                "count": count,
                "skipped": Value::Null,
            })
        }
        Err(e) => skipped_report(n, e),
    }
}

/// `csgraph parse-check` entry point (reads `CSGRAPH_DOTNET`).
pub fn parse_check(files: &[String], timeout: u64) -> Value {
    parse_check_with(files, None, timeout)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Ground truth for `parse`/`profile`, generated with the frozen Python
    // module (`tools/csgraph/compile_check.py`) over the synthetic log below:
    //
    //   CC = load('.../compile_check.py')
    //   CC.parse(LOG); CC.profile(CC.parse(LOG))
    const LOG: &str = r#"Assets/Foo.cs(12,5): error CS0103: The name 'Bar' does not exist in the current context
  Assets/Baz.cs(3,1): warning CS0168: The variable 'x' is declared but never used   
Assets/Qux (1).cs(1,2): error CS0246: The type or namespace name 'Newton' could not be found (are you missing a using directive or an assembly reference?)
Assets/Qux (1).cs(9,2): error CS0246: The type or namespace name 'Newton' could not be found (are you missing a using directive or an assembly reference?)
Assets/Weird (1).cs(4,4): warning CS0219: The variable 'y' is assigned but its value is never used
Assets/Uni.cs(7,3): error CS1002: ; expected  
error CS9999: no location at all
warning CS0000: also no location
Assets/Uni.cs(8,3): error CS1002:    ;   expected
some unrelated compiler noise
Assets/What.cs(2,2):ERROR CS1234: wrong case
Assets/What.cs(2,2): error cs1234: lowercase code
Assets/Ünïcode.cs(5,5): warning CS0168: Le caractère « ü » est inutilisé
"#;

    const PARSE_JSON: &str = r#"[
      ["error","CS0103","The name 'Bar' does not exist in the current context"],
      ["warning","CS0168","The variable 'x' is declared but never used"],
      ["error","CS0246","The type or namespace name 'Newton' could not be found (are you missing a using directive or an assembly reference?)"],
      ["error","CS0246","The type or namespace name 'Newton' could not be found (are you missing a using directive or an assembly reference?)"],
      ["warning","CS0219","The variable 'y' is assigned but its value is never used"],
      ["error","CS1002","; expected"],
      ["error","CS1002",";   expected"],
      ["warning","CS0168","Le caractère « ü » est inutilisé"]
    ]"#;

    const PROFILE_JSON: &str = r#"[
      [["error","CS0103","The name 'Bar' does not exist in the current context"],1],
      [["warning","CS0168","The variable 'x' is declared but never used"],1],
      [["error","CS0246","The type or namespace name 'Newton' could not be found (are you missing a using directive or an assembly reference?)"],2],
      [["warning","CS0219","The variable 'y' is assigned but its value is never used"],1],
      [["error","CS1002","; expected"],1],
      [["error","CS1002",";   expected"],1],
      [["warning","CS0168","Le caractère « ü » est inutilisé"],1]
    ]"#;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cc-test-{}-{}-{}",
            std::process::id(),
            now_nanos(),
            name
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parse_matches_python_ground_truth() {
        let want: Value = serde_json::from_str(PARSE_JSON).unwrap();
        let got: Vec<Value> = parse(LOG)
            .into_iter()
            .map(|(kind, code, msg)| json!([kind, code, msg]))
            .collect();
        assert_eq!(Value::Array(got), want);
        assert!(parse("").is_empty());
    }

    #[test]
    fn parse_full_keeps_positions_and_skips_noise() {
        let got = parse_full(LOG);
        assert_eq!(got.len(), 8, "{got:?}");
        assert_eq!(
            got[0],
            json!({
                "kind": "error", "code": "CS0103", "file": "Assets/Foo.cs",
                "line": 12, "col": 5,
                "message": "The name 'Bar' does not exist in the current context"
            })
        );
        assert_eq!(got[2]["file"], json!("Assets/Qux (1).cs"));
        assert_eq!(got[2]["line"], json!(1));
        assert_eq!(got[2]["col"], json!(2));
        // location-less compiler lines never fabricate a file/line
        assert!(got.iter().all(|d| !d["file"].as_str().unwrap().is_empty()));
        assert!(parse_full("").is_empty());
    }

    #[test]
    fn profile_matches_python_ground_truth() {        let raw: Vec<Value> = serde_json::from_str(PROFILE_JSON).unwrap();
        let mut want: BTreeMap<Diag, i64> = BTreeMap::new();
        for entry in raw {
            let arr = entry.as_array().unwrap();
            let key = arr[0].as_array().unwrap();
            let d = (
                key[0].as_str().unwrap().to_string(),
                key[1].as_str().unwrap().to_string(),
                key[2].as_str().unwrap().to_string(),
            );
            want.insert(d, arr[1].as_i64().unwrap());
        }
        assert_eq!(profile(&parse(LOG)), want);
        assert!(profile(&[]).is_empty());
    }

    #[test]
    fn materialize_matches_python_names() {
        let d = tmp("mat");
        let pairs = vec![
            ("a b/c.cs".to_string(), "x".to_string()),
            ("___.cs".to_string(), "y".to_string()),
            ("empty".to_string(), String::new()),
        ];
        materialize(&pairs, &d);
        let mut names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // Python: re.sub -> "a_b_c.cs" keeps ".cs"; "___.cs" strips to ".cs"
        // (still ends with .cs, so no "src"); empty text is skipped entirely.
        assert_eq!(names, vec!["0000_a_b_c.cs".to_string(), "0001_.cs".to_string()]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn glob_paths_treats_double_star_as_one_segment() {
        let root = tmp("glob");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("a/.hid/x")).unwrap();
        std::fs::write(root.join("a/top.dll"), b"").unwrap();
        std::fs::write(root.join("a/.dot.dll"), b"").unwrap();
        std::fs::write(root.join("a/b/g.dll"), b"").unwrap();
        std::fs::write(root.join("a/.hid/x/f.dll"), b"").unwrap();
        let got = glob_paths(&format!("{}/a/**/*.dll", root.display()));
        // Python (glob, recursive=False): ['<root>/a/b/g.dll'] only.
        assert_eq!(
            got,
            vec![root.join("a/b/g.dll").to_string_lossy().into_owned()]
        );
        assert!(glob_paths(&format!("{}/nope/*", root.display())).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn toolchain_discovers_fake_unity_data() {
        let data = tmp("unity");
        for rel in [
            "NetCoreRuntime",
            "DotNetSdkRoslyn",
            "UnityReferenceAssemblies/unity-4.8-api",
        ] {
            std::fs::create_dir_all(data.join(rel)).unwrap();
        }
        std::fs::write(data.join("NetCoreRuntime/dotnet"), b"").unwrap();
        std::fs::write(data.join("DotNetSdkRoslyn/csc.dll"), b"").unwrap();
        std::fs::write(
            data.join("UnityReferenceAssemblies/unity-4.8-api/mscorlib.dll"),
            b"",
        )
        .unwrap();
        let vrc = tmp("vrc");
        std::fs::create_dir_all(vrc.join("sub")).unwrap();
        std::fs::write(vrc.join("A.dll"), b"").unwrap();
        std::fs::write(vrc.join("sub/B.dll"), b"").unwrap();
        std::fs::write(vrc.join(".hidden.dll"), b"").unwrap();

        let (prefix, refs) = toolchain(
            Some(&data.to_string_lossy()),
            Some(&vrc.to_string_lossy()),
        );
        let prefix = prefix.expect("fake toolchain discovered");
        assert_eq!(prefix[0], data.join("NetCoreRuntime/dotnet").to_string_lossy());
        assert_eq!(prefix[1], data.join("DotNetSdkRoslyn/csc.dll").to_string_lossy());
        let joined = refs.join("\n");
        assert!(joined.contains(&format!(
            "-r:{}",
            data.join("UnityReferenceAssemblies/unity-4.8-api/mscorlib.dll")
                .display()
        )));
        assert!(joined.contains(&format!("-r:{}", vrc.join("A.dll").display())));
        assert!(joined.contains(&format!("-r:{}", vrc.join("sub/B.dll").display())));
        assert!(!joined.contains(".hidden.dll"));

        std::fs::remove_dir_all(&data).ok();
        std::fs::remove_dir_all(&vrc).ok();
    }

    #[test]
    fn toolchain_missing_returns_none() {
        let (prefix, refs) = toolchain(Some("/nonexistent-csviewer-data"), None);
        assert!(prefix.is_none());
        assert!(refs.is_empty());
    }

    #[test]
    fn differential_shape_edges_match_python() {
        // emitted == {} -> "nothing emitted" (no `skipped` key at all)
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(
            differential(&none, &[], None, &[], &[], 5, None),
            json!({"ok": false, "error": "nothing emitted"})
        );

        // emitted non-empty but the baseline has no materialisable text.
        let emitted = vec![("B.cs".to_string(), "class B {}".to_string())];
        assert_eq!(
            differential(&none, &emitted, None, &[], &[], 5, None),
            json!({"ok": false, "skipped": "no compilable sources"})
        );

        // A forced-missing toolchain degrades to the same `skipped` shape.
        let mut orig: HashMap<String, String> = HashMap::new();
        orig.insert("A.cs".to_string(), "class A {}".to_string());
        assert_eq!(
            differential(&orig, &emitted, Some("/nonexistent-csviewer-data"), &[], &[], 5, None),
            json!({"ok": false, "skipped": "no Unity/Roslyn toolchain found"})
        );
    }

    #[test]
    fn index_reorg_compile_arm_calls_the_module() {
        // An empty source gives the emitter nothing compilable, so the arm
        // resolves to `nothing emitted` / `no compilable sources` without ever
        // spawning Unity's compiler -- yet it must still have gone through
        // `compile_check` (the old stub returned a hard-coded `reason`).
        let root = tmp("idx");
        let src = root.join("_sealed_src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Empty.cs"), "").unwrap();

        let roots = vec!["_sealed_src".to_string()];
        let cfg = crate::config::Config {
            roots: roots.clone(),
            excludes: Vec::new(),
            defines: Vec::new(),
            player: false,
            ..Default::default()
        };
        let mut index = crate::index::Index::new(root.clone());
        index.build(&roots, &[], &cfg);
        let res = index.reorg(Some(&json!({"group": "single"})), "compile", None, false);
        assert!(res.get("plan").is_some(), "compile arm must attach the plan: {res}");
        assert!(res.get("report").is_some(), "compile arm must attach the report: {res}");
        assert!(
            res.get("reason").is_none(),
            "compile arm still looks like the hard-coded stub: {res}"
        );
        assert_eq!(res.get("ok"), Some(&Value::Bool(false)), "{res}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn python_oserror_matches_python_text() {
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            python_oserror(&e, "/x/dotnet"),
            "[Errno 2] No such file or directory: '/x/dotnet'"
        );
        let e = std::io::Error::from_raw_os_error(13);
        assert_eq!(
            python_oserror(&e, "/x/dotnet"),
            "[Errno 13] Permission denied: '/x/dotnet'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_merged_merges_streams_and_reports_exit_code() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo out; echo err 1>&2; exit 3");
        let (code, log) = run_merged(&mut cmd, 10).unwrap();
        assert_eq!(code, 3);
        assert!(log.contains("out"), "{log:?}");
        assert!(log.contains("err"), "{log:?}");
    }

    #[cfg(unix)]
    #[test]
    fn run_merged_timeout_matches_python() {
        // Ground truth from CPython:
        //   subprocess.run(['/bin/sleep','5'], timeout=1)
        //   -> "Command '['/bin/sleep', '5']' timed out after 1 seconds"
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5");
        let err = run_merged(&mut cmd, 1).unwrap_err();
        assert_eq!(err, "Command '['/bin/sleep', '5']' timed out after 1 seconds");
    }

    // -----------------------------------------------------------------------
    // local dotnet-Roslyn parse gate
    // -----------------------------------------------------------------------

    /// Captured shape of the driver's stdout (Roslyn 4.12.0.0).
    const DRIVER_JSON: &str = r#"{"version":"4.12.0.0","diagnostics":[{"severity":"error","id":"CS1026","file":"/tmp/x/Broken.cs","line":1,"col":49,"message":") expected"},{"severity":"error","id":"CS1513","file":"/tmp/x/Broken.cs","line":1,"col":50,"message":"} expected"}],"files":1,"count":2}"#;

    #[test]
    fn parse_driver_output_maps_diagnostics() {
        let (version, files, diags) =
            parse_driver_output(DRIVER_JSON).expect("driver JSON parses");
        assert_eq!(version, "4.12.0.0");
        assert_eq!(files, 1);
        assert_eq!(
            diags,
            vec![
                json!({"severity":"error","id":"CS1026","file":"/tmp/x/Broken.cs","line":1,"col":49,"message":") expected"}),
                json!({"severity":"error","id":"CS1513","file":"/tmp/x/Broken.cs","line":1,"col":50,"message":"} expected"}),
            ]
        );
    }

    #[test]
    fn parse_driver_output_tolerates_host_noise_around_the_json() {
        let noisy = format!("You must install .NET to run this application.\n{DRIVER_JSON}\n");
        let (_, files, diags) = parse_driver_output(&noisy).unwrap();
        assert_eq!(files, 1);
        assert_eq!(diags.len(), 2);
    }

    #[test]
    fn parse_driver_output_rejects_non_json() {
        assert!(parse_driver_output("").is_err());
        assert!(parse_driver_output("not json at all").is_err());
        // JSON that is not the driver report (no diagnostics key) is rejected.
        assert!(parse_driver_output(r#"{"version":"4.12.0.0"}"#).is_err());
    }

    #[test]
    fn parse_driver_output_surfaces_zero_parsed_files() {
        // `parse_check_with` turns this into `skipped`, never a clean pass.
        let (_, files, diags) =
            parse_driver_output(r#"{"version":"4.12.0.0","diagnostics":[],"files":0,"count":0}"#)
                .unwrap();
        assert_eq!(files, 0);
        assert!(diags.is_empty());
    }

    #[test]
    fn parse_check_skips_when_toolchain_forced_absent() {
        // The override is passed directly, so no test mutates the process env
        // (tests run concurrently in one process).
        let files = vec!["/tmp/definitely-not-a-real-cs-file.cs".to_string()];
        let res = parse_check_with(&files, Some("/nonexistent-dotnet-for-csgraph"), 5);
        assert_eq!(res["ok"], json!(false), "{res}");
        assert_eq!(res["diagnostics"], json!([]), "{res}");
        assert!(res["toolchain"].is_null(), "{res}");
        let reason = res["skipped"].as_str().unwrap_or("");
        assert!(
            reason.contains("/nonexistent-dotnet-for-csgraph"),
            "reason must name the override: {reason:?}"
        );
    }

    #[test]
    fn parse_check_skips_on_empty_file_list() {
        let res = parse_check_with(&[], None, 5);
        assert_eq!(res["ok"], json!(false), "{res}");
        assert_eq!(res["skipped"], json!("no C# files to check"), "{res}");
    }

    #[test]
    fn parse_check_skips_when_files_are_missing() {
        let files = vec!["/tmp/definitely-not-a-real-cs-file.cs".to_string()];
        // Only meaningful when a real toolchain is present; otherwise the
        // toolchain reason wins and both are still `skipped`.
        let res = parse_check_with(&files, None, 5);
        assert_eq!(res["ok"], json!(false), "must never be a false ok: {res}");
        assert!(res["skipped"].is_string(), "{res}");
    }

    /// Bounded integration check: builds/runs the driver against one clean and
    /// one broken file. Degrades to a no-op when no local .NET SDK exists, so
    /// the suite stays green on a machine without one.
    #[test]
    fn parse_check_reports_real_roslyn_diagnostics() {
        if find_roslyn_toolchain(None).is_err() {
            return;
        }
        let dir = tmp("roslyn");
        let clean = dir.join("Clean.cs");
        let broken = dir.join("Broken.cs");
        std::fs::write(&clean, "namespace N { class C { int X() { return 1; } } }\n").unwrap();
        std::fs::write(&broken, "namespace N { class C { int X( { return 1; } } }\n").unwrap();

        let ok = parse_check(&[clean.to_string_lossy().into_owned()], 25);
        assert_eq!(ok["ok"], json!(true), "clean file must pass: {ok}");
        assert!(ok["skipped"].is_null(), "{ok}");
        assert_eq!(ok["count"], json!(0), "{ok}");
        assert!(
            ok["toolchain"]
                .as_str()
                .unwrap_or("")
                .split(' ')
                .next()
                .map(|t| t.ends_with("roslyn"))
                .unwrap_or(false),
            "{ok}"
        );

        let bad = parse_check(&[broken.to_string_lossy().into_owned()], 25);
        assert_eq!(bad["ok"], json!(false), "broken file must fail: {bad}");
        assert!(bad["skipped"].is_null(), "{bad}");
        assert!(bad["count"].as_i64().unwrap_or(0) >= 1, "{bad}");
        let first = &bad["diagnostics"][0];
        assert_eq!(first["severity"], json!("error"), "{bad}");
        assert_eq!(first["file"], json!(broken.to_string_lossy()), "{bad}");
        assert!(first["id"].as_str().unwrap_or("").starts_with("CS"), "{bad}");
        assert!(first["line"].as_i64().unwrap_or(0) >= 1, "{bad}");

        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // compile-level differential through the SDK Roslyn
    // -----------------------------------------------------------------------

    const VALID_A: &str = "namespace N { public class A { public int X() { return 1; } } }\n";
    const CLEAN_EDIT: &str =
        "namespace N { public class A { public int X() { return 2; } } }\n";
    const BAD_B: &str =
        "namespace N { public class B { public UndefinedType M() { return null; } } }\n";
    const BROKEN_EDIT: &str = "namespace N { public class A { public int X() { return 1; } } }\n\
namespace N { public class B { public UndefinedType M() { return null; } } }\n";

    fn has_compiler() -> bool {
        resolve_compiler(None).is_ok()
    }

    #[test]
    fn validity_policy_never_treats_skipped_as_a_pass() {
        let real = json!({"ok": true, "skipped": null});
        let p = validity_policy(&real, true);
        assert_eq!(p.validated, "roslyn");
        assert!(!p.unverified);
        assert!(p.refusal.is_none());

        let skipped = json!({"ok": false, "skipped": "no Unity/Roslyn toolchain found"});
        let p = validity_policy(&skipped, false);
        assert_eq!(p.validated, "skipped:no Unity/Roslyn toolchain found");
        assert!(p.unverified, "a skipped gate must be reported unverified");
        assert!(p.refusal.is_none(), "without require_validated the write proceeds");

        let p = validity_policy(&skipped, true);
        assert!(p.unverified);
        let refusal = p.refusal.expect("require_validated must refuse");
        assert_eq!(refusal["ok"], json!(false));
        assert_eq!(refusal["unverified"], json!(true));
        assert_eq!(
            refusal["validated"],
            json!("skipped:no Unity/Roslyn toolchain found")
        );
        assert!(refusal["error"].as_str().unwrap_or("").contains("unverified"));
    }

    #[test]
    fn driver_bundle_requires_every_assembly() {
        let dir = tmp("bundle");
        assert!(!valid_driver_dir(&dir));
        for (i, n) in DRIVER_BUNDLE_FILES.iter().enumerate() {
            std::fs::write(dir.join(n), b"").unwrap();
            assert_eq!(valid_driver_dir(&dir), i + 1 == DRIVER_BUNDLE_FILES.len(), "{n}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Bounded integration check: the SDK differential must catch a NEW semantic
    /// diagnostic (`UndefinedType` -> CS0246) and pass a clean edit, while an
    /// explicit missing toolchain stays `skipped`. Degrades to a no-op when the
    /// box has neither Unity nor a .NET SDK.
    #[test]
    fn differential_reports_new_semantic_diagnostics() {
        if !has_compiler() {
            return;
        }
        let mut base: HashMap<String, String> = HashMap::new();
        base.insert("A.cs".to_string(), VALID_A.to_string());

        // clean edit: candidate still valid -> no new diagnostics
        let clean = vec![("A.cs".to_string(), CLEAN_EDIT.to_string())];
        let res = differential(&base, &clean, None, &DEFAULT_DEFINES.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(), &[], 30, None);
        assert_eq!(res["ok"], json!(true), "clean edit must pass: {res}");
        assert!(res["skipped"].is_null(), "SDK fallback must not skip: {res}");
        assert_eq!(res["newDiagnostics"], json!([]), "{res}");

        // semantic break: candidate introduces an undefined type
        let broken = vec![("A.cs".to_string(), BROKEN_EDIT.to_string())];
        let res = differential(&base, &broken, None, &DEFAULT_DEFINES.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(), &[], 30, None);
        assert_eq!(res["ok"], json!(false), "new diagnostic must fail: {res}");
        assert!(res["skipped"].is_null(), "{res}");
        let codes: Vec<String> = res["newDiagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["code"].as_str().map(str::to_string))
            .collect();
        assert!(codes.contains(&"CS0246".to_string()), "missing CS0246: {res}");

        // explicit data pin that does not exist -> skipped, never a false ok
        let res = differential(
            &base,
            &clean,
            Some("/nonexistent-csviewer-data"),
            &[],
            &[],
            5,
            None,
        );
        assert_eq!(res["ok"], json!(false), "{res}");
        assert_eq!(res["skipped"], json!("no Unity/Roslyn toolchain found"), "{res}");
    }

    /// `cli_compile_diff` exit codes: 0 clean, 1 new diagnostic, 3 skipped.
    #[test]
    fn cli_compile_diff_exit_codes_are_meaningful() {
        if !has_compiler() {
            return;
        }
        let root = tmp("cli-diff");
        let b = root.join("baseline");
        let c = root.join("candidate-clean");
        let x = root.join("candidate-bad");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&x).unwrap();
        std::fs::write(b.join("A.cs"), VALID_A).unwrap();
        std::fs::write(c.join("A.cs"), CLEAN_EDIT).unwrap();
        std::fs::write(x.join("A.cs"), VALID_A).unwrap();
        std::fs::write(x.join("B.cs"), BAD_B).unwrap();

        let ok = cli_compile_diff(&[
            "--baseline".into(),
            b.to_string_lossy().into_owned(),
            "--candidate".into(),
            c.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(ok, 0, "clean diff must exit 0");
        let bad = cli_compile_diff(&[
            "--baseline".into(),
            b.to_string_lossy().into_owned(),
            "--candidate".into(),
            x.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(bad, 1, "new diagnostic must exit 1");

        let skipped = cli_compile_diff(&[
            "--baseline".into(),
            b.to_string_lossy().into_owned(),
            "--candidate".into(),
            c.to_string_lossy().into_owned(),
            "--data".into(),
            "/nonexistent-csviewer-data".into(),
        ])
        .unwrap();
        assert_eq!(skipped, 3, "skipped must exit 3, never 0");

        // A non-UTF-8 (Latin-1) source must be compiled, not dropped: the
        // verifier's hard false pass (finding 1).
        let l1 = root.join("candidate-latin1");
        std::fs::create_dir_all(&l1).unwrap();
        std::fs::write(l1.join("A.cs"), VALID_A).unwrap();
        std::fs::write(
            l1.join("B.cs"),
            b"// caf\xe9\nnamespace N { public class B { public UndefinedType M() { return null; } } }\n",
        )
        .unwrap();
        let latin1 = cli_compile_diff(&[
            "--baseline".into(),
            b.to_string_lossy().into_owned(),
            "--candidate".into(),
            l1.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(latin1, 1, "a non-UTF-8 source must not be reported clean");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn collect_cs_keeps_non_utf8_sources() {
        let dir = tmp("nonutf8");
        std::fs::write(dir.join("A.cs"), b"class A {}\n").unwrap();
        std::fs::write(dir.join("B.cs"), b"// caf\xe9\nclass B {}\n").unwrap();
        let got = collect_cs(&dir).expect("collect must not fail");
        assert_eq!(got.len(), 2, "no source file may be dropped: {got:?}");
        let b = got
            .iter()
            .find(|(n, _)| n == "B.cs")
            .expect("B.cs must be collected")
            .1
            .clone();
        assert!(
            b.contains("caf\u{fffd}"),
            "non-UTF-8 bytes must be lossily decoded, got {b:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------------
    // plug-in reference folders (`--refs` / `CSVIEWER_REFS`)
    // -----------------------------------------------------------------------

    #[test]
    fn collect_refs_expands_dirs_and_reports_missing() {
        let root = tmp("refs");
        let dir = root.join("plugins");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("A.dll"), b"x").unwrap();
        std::fs::write(dir.join("sub/B.dll"), b"x").unwrap();
        std::fs::write(dir.join(".hidden.dll"), b"x").unwrap();
        std::fs::write(dir.join("notdll.txt"), b"x").unwrap();
        let a = dir.join("A.dll").to_string_lossy().into_owned();
        let entries = vec![
            dir.to_string_lossy().into_owned(),
            "/nonexistent-refs-dir".to_string(),
            a.clone(),
        ];
        let (extra, inv) = collect_refs(&entries);
        assert_eq!(inv.dirs, vec![dir.to_string_lossy().into_owned()]);
        assert_eq!(inv.loaded.len(), 2, "hidden dll must be pruned: {:?}", inv.loaded);
        assert!(inv.loaded.iter().any(|p| p.ends_with("sub/B.dll")));
        assert!(!inv.loaded.iter().any(|p| p.ends_with(".hidden.dll")));
        assert!(extra.iter().all(|r| r.starts_with("-r:")), "{extra:?}");
        assert_eq!(extra.len(), 2);
        assert_eq!(inv.missing_paths, vec!["/nonexistent-refs-dir".to_string()]);
        // the explicit file is already loaded through the dir: not duplicated
        assert_eq!(
            inv.notes.iter().filter(|n| n.contains("duplicate")).count(),
            1,
            "{:?}",
            inv.notes
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unity_refs_tolerate_both_editor_coremodule_layouts() {
        let group = UNITY_REFS.last().unwrap();

        // 2022.3 (the live /500gb install): Managed/UnityEngine/.
        let modern = tmp("unity-modern");
        std::fs::create_dir_all(modern.join("Managed/UnityEngine")).unwrap();
        std::fs::write(
            modern.join("Managed/UnityEngine/UnityEditor.CoreModule.dll"),
            b"x",
        )
        .unwrap();
        let p = unity_ref_path(&modern.to_string_lossy(), group).expect("modern layout resolves");
        assert!(
            p.ends_with("Managed/UnityEngine/UnityEditor.CoreModule.dll"),
            "{p}"
        );

        // older layout: Managed/UnityEditor/.
        let old = tmp("unity-old");
        std::fs::create_dir_all(old.join("Managed/UnityEditor")).unwrap();
        std::fs::write(
            old.join("Managed/UnityEditor/UnityEditor.CoreModule.dll"),
            b"x",
        )
        .unwrap();
        let p = unity_ref_path(&old.to_string_lossy(), group).expect("old layout resolves");
        assert!(
            p.ends_with("Managed/UnityEditor/UnityEditor.CoreModule.dll"),
            "{p}"
        );
        assert!(unity_ref_path("/nonexistent-unity", group).is_none());

        std::fs::remove_dir_all(&modern).ok();
        std::fs::remove_dir_all(&old).ok();
    }

    #[test]
    fn refs_supply_core_decides_whether_the_sdk_refpack_is_dropped() {        assert!(refs_supply_core(&["-r:/x/mscorlib.dll".to_string()]));
        assert!(refs_supply_core(&["-r:/x/System.Runtime.dll".to_string()]));
        assert!(!refs_supply_core(&["-r:/x/VRC.SDKBase.dll".to_string()]));
        assert!(!refs_supply_core(&[]));
    }

    #[test]
    fn references_report_names_what_is_loaded_and_missing() {
        let inv = RefInventory {
            dirs: vec!["/tmp/x".to_string()],
            loaded: vec!["/tmp/x/UnityEngine.CoreModule.dll".to_string()],
            ..Default::default()
        };
        let r = references_report(Some("/nonexistent-csviewer-data"), &inv);
        let missing: Vec<String> = r["missingCanonicalRefs"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert!(
            !missing.contains(&"unityengine.coremodule.dll".to_string()),
            "a loaded reference must not be reported missing: {r}"
        );
        assert!(
            missing.contains(&"unityeditor.coremodule.dll".to_string()),
            "an absent reference must be reported missing: {r}"
        );
        assert_eq!(r["pluginRefsLoadedCount"], json!(1));
        assert_eq!(r["unityData"], json!("/nonexistent-csviewer-data"));
        let loader = inv.to_json();
        assert_eq!(loader["pluginRefsLoadedCount"], json!(1));
    }

    #[test]
    fn index_reorg_compile_arm_reports_toolchain_and_references() {
        // The API/MCP compile arm must carry the same provenance the CLI does:
        // a `toolchain` string and a `references` inventory, added on top of the
        // pre-existing `skipped`/`ok`/`plan`/`report` keys.
        let root = tmp("idx-tc");
        let src = root.join("_sealed_src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("A.cs"), VALID_A).unwrap();

        let roots = vec!["_sealed_src".to_string()];
        let cfg = crate::config::Config {
            roots: roots.clone(),
            excludes: Vec::new(),
            defines: Vec::new(),
            player: false,
            ..Default::default()
        };
        let mut index = crate::index::Index::new(root.clone());
        index.build(&roots, &[], &cfg);
        let res = index.reorg(Some(&json!({"group": "single"})), "compile", None, false);

        assert!(res.get("plan").is_some(), "compile arm must attach the plan: {res}");
        assert!(res.get("report").is_some(), "compile arm must attach the report: {res}");
        let references = res.get("references").expect("references provenance: {res}");
        assert!(
            references.get("missingCanonicalRefs").map(Value::is_array).unwrap_or(false),
            "{res}"
        );
        assert!(
            references.get("pluginRefsLoaded").map(Value::is_array).unwrap_or(false),
            "{res}"
        );
        assert!(res.get("baselineDiagnostics").map(Value::is_array).unwrap_or(false), "{res}");
        assert!(res.get("candidateDiagnostics").map(Value::is_array).unwrap_or(false), "{res}");

        if !has_compiler() {
            // A box with neither Unity nor a .NET SDK must still be honest.
            assert!(res["skipped"].is_string(), "{res}");
            assert!(res["toolchain"].is_null(), "{res}");
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        assert!(res["skipped"].is_null(), "a working toolchain must not skip: {res}");
        let tc = res["toolchain"].as_str().unwrap_or("");
        assert!(tc.ends_with("roslyn"), "{res}");
        if find_unity_data().is_some() {
            assert_eq!(tc, "unity-roslyn", "{res}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// The `--refs` CLI surface is accepted, non-fatal on a missing path, and
    /// reported in the document without changing a clean edit's verdict.
    #[test]
    fn cli_compile_diff_accepts_refs_and_reports_them() {
        if !has_compiler() {
            return;
        }
        let root = tmp("cli-refs");
        let b = root.join("baseline");
        let c = root.join("candidate");
        let refs = root.join("refs-empty");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::write(b.join("A.cs"), VALID_A).unwrap();
        std::fs::write(c.join("A.cs"), CLEAN_EDIT).unwrap();

        let rc = cli_compile_diff(&[
            "--baseline".into(),
            b.to_string_lossy().into_owned(),
            "--candidate".into(),
            c.to_string_lossy().into_owned(),
            "--refs".into(),
            refs.to_string_lossy().into_owned(),
            "--refs".into(),
            (root.join("no-such-refs")).to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(rc, 0, "a plug-in ref dir must not change a clean verdict");
        std::fs::remove_dir_all(&root).ok();
    }

    /// Verifier finding 2: a `--data` tree with the two paths present but no
    /// working compiler must be refused, never read as a clean unity-roslyn pass.
    #[test]
    fn differential_refuses_a_stub_unity_toolchain() {
        let data = tmp("stub-unity");
        std::fs::create_dir_all(data.join("NetCoreRuntime")).unwrap();
        std::fs::create_dir_all(data.join("DotNetSdkRoslyn")).unwrap();
        std::fs::write(data.join("NetCoreRuntime/dotnet"), b"").unwrap();
        std::fs::write(data.join("DotNetSdkRoslyn/csc.dll"), b"").unwrap();

        let mut orig: HashMap<String, String> = HashMap::new();
        orig.insert("A.cs".to_string(), "namespace N { class A {} }\n".to_string());
        let emitted = vec![(
            "A.cs".to_string(),
            "namespace N { class A {} }\n".to_string(),
        )];
        let res = differential(
            &orig,
            &emitted,
            Some(&data.to_string_lossy()),
            &[],
            &[],
            5,
            None,
        );
        assert_eq!(res["ok"], json!(false), "a stub toolchain must not pass: {res}");
        let reason = res["skipped"].as_str().unwrap_or("");
        assert!(
            reason.contains("csc.dll"),
            "the skip reason must name the bad compiler: {res}"
        );
        std::fs::remove_dir_all(&data).ok();
    }
}
