//! registry — find the live cs viewer instance(s), across OS users and roots.
//!
//! The viewer and an MCP server are separate processes and, on this box, run as
//! different users (the viewer as `naruzkurai`, the harness/MCP server as
//! `root`). The only cross-process seam for viewer state is the file
//! `<cache_root>/.csgraph.v2/viewer.json` ([`crate::cache`]); the problem is
//! finding the right `<cache_root>` without guessing.
//!
//! So the app registers a small record for itself:
//!
//! ```text
//! $XDG_RUNTIME_DIR/csviewer/instances/<pid>.json
//! ```
//!
//! and removes it on a clean exit. A record that no longer matches a live
//! process (dead pid, different uid, non-viewer cmdline) is reported with
//! `stale: true` and never counted as a live instance. When the registry has no
//! live record, discovery falls back to a bounded `/proc/<pid>/cmdline` scan.
//!
//! `$XDG_RUNTIME_DIR` is per-user, but it lives under `/run/user/<uid>` which is
//! world-readable (and root can read every user's), so scanning `/run/user/*`
//! makes the registry cross-user. Discovery never writes to another user's
//! directory, never signals a process, and never kills anything: it is a stat +
//! read of `/proc` and of the registry files.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Environment override for the registry directory (tests, sandboxes).
pub const INSTANCE_DIR_ENV: &str = "CSVIEWER_INSTANCE_DIR";
/// Subdirectory of `$XDG_RUNTIME_DIR` that holds the records.
pub const REGISTRY_SUBDIR: &str = "csviewer/instances";
/// The cache directory name used by [`crate::cache`].
pub const CACHE_DIR: &str = ".csgraph.v2";
/// The viewer-state file name inside the cache directory.
pub const STATE_FILE: &str = "viewer.json";
/// A record older than this is labelled stale even when the pid still exists.
pub const STALE_AFTER_MS: i64 = 5_000;
/// Upper bound on records read / processes probed (bounded work, always).
const MAX_RECORDS: usize = 64;
const MAX_SCANNED: usize = 16;

/// Wall clock in milliseconds since the epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The uid of this process, via `/proc/self`.
pub fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return fs::metadata("/proc/self").ok().map(|m| m.uid());
    }
    #[allow(unreachable_code)]
    None
}

/// The user name this process runs as (best effort; never an error).
pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "?".to_string())
}

/// Where this process writes its own record.
pub fn write_dir() -> PathBuf {
    if let Ok(d) = std::env::var(INSTANCE_DIR_ENV) {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        if !x.is_empty() {
            return PathBuf::from(x).join(REGISTRY_SUBDIR);
        }
    }
    std::env::temp_dir().join(REGISTRY_SUBDIR)
}

/// Every directory that may hold records (read side).
///
/// Ordered and de-duplicated; `/run/user/<uid>` entries are sorted so the
/// answer is deterministic.
pub fn instance_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    {
        let mut push = |p: PathBuf| {
            if !p.as_os_str().is_empty() && !out.contains(&p) {
                out.push(p);
            }
        };
        if let Ok(d) = std::env::var(INSTANCE_DIR_ENV) {
            if !d.is_empty() {
                push(PathBuf::from(d));
            }
        }
        if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
            if !x.is_empty() {
                push(PathBuf::from(x).join(REGISTRY_SUBDIR));
            }
        }
        if let Ok(rd) = fs::read_dir("/run/user") {
            let mut paths: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path().join(REGISTRY_SUBDIR))
                .collect();
            paths.sort();
            for p in paths {
                push(p);
            }
        }
        push(std::env::temp_dir().join(REGISTRY_SUBDIR));
    }
    out
}

/// Mirror of [`crate::index::Index::cache_dir`]`(None)`: the first root that
/// exists, else the repo. The app writes its record with this same answer, so
/// the record's `statePath` and the engine's actual state file agree.
pub fn cache_root_for(repo: &Path, roots: &[String]) -> PathBuf {
    for r in roots {
        if r.is_empty() {
            continue;
        }
        let p = if Path::new(r).is_absolute() {
            PathBuf::from(r)
        } else {
            repo.join(r)
        };
        let p = crate::index::abspath(&p);
        if p.is_dir() {
            return p;
        }
        if p.is_file() {
            return p.parent().map(Path::to_path_buf).unwrap_or(p);
        }
    }
    crate::index::abspath(repo)
}

/// The `viewer.json` a cache root would hold.
pub fn state_path_for(cache_root: &Path) -> PathBuf {
    cache_root.join(CACHE_DIR).join(STATE_FILE)
}

/// Write one instance record atomically (temp + rename). Returns its path.
pub fn register(record: &Value, dir: &Path) -> Option<PathBuf> {
    let pid = record.get("pid").and_then(Value::as_u64)?;
    if dir.as_os_str().is_empty() {
        return None;
    }
    fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("{pid}.json"));
    let body = serde_json::to_vec_pretty(record).ok()?;
    let tmp = dir.join(format!("{pid}.json.tmp"));
    fs::write(&tmp, &body).ok()?;
    fs::rename(&tmp, &path).ok()?;
    Some(path)
}

/// Remove this process's record on a clean exit (best effort).
pub fn unregister(path: &Path) {
    if path.as_os_str().is_empty() {
        return;
    }
    let _ = fs::remove_file(path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent); // only succeeds when empty
    }
}

/// `/proc/<pid>/cmdline` as one space-joined string, or `None` when the pid is
/// gone (or unreadable). Never signals the process.
pub fn proc_cmdline(pid: u32) -> Option<String> {
    let args = proc_args(pid)?;
    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

/// `/proc/<pid>/cmdline` as its exact argument vector (NUL-separated, so paths
/// with spaces survive). Never signals the process.
pub fn proc_args(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let parts: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Parent pid from `/proc/<pid>/stat`, when readable.
pub fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `<pid> (<comm>) <state> <ppid> ...`; comm may contain spaces, so split
    // after the closing parenthesis.
    let close = stat.rfind(')')?;
    let rest = &stat[close + 1..];
    let mut it = rest.split_whitespace();
    let _state = it.next()?;
    it.next()?.parse::<u32>().ok()
}

/// True for an AppImage/AppRun launcher wrapper around the real viewer binary.
pub fn is_wrapper_args(args: &[String]) -> bool {
    let Some(prog) = args.first() else { return false };
    let base = Path::new(prog)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    base.ends_with(".appimage") || base == "apprun"
}

/// The uid that owns `/proc/<pid>`, when readable.
pub fn proc_uid(pid: u32) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid());
    }
    #[allow(unreachable_code)]
    None
}

/// Explicit allowlist of program names that are a cs viewer.
///
/// * `csviewer` — the v2 shipping name.
/// * `csviewer-*` — versioned/suffixed builds and the AppImage itself
///   (`csviewer-Latest-x86_64.AppImage`), which is the v2 distribution shape.
/// * `native-csviewer` — **the v3 fork's shipping name** (task-36). It starts
///   with `native-`, so a prefix check on `csviewer` missed it and a live v3
///   window was reported stale under the name we actually ship.
///
/// Deliberately *not* a substring match: `nativecsviewer`, `my-native-csviewer`
/// and unrelated `viewer` binaries are rejected. A **renamed** binary is not
/// recognised either — a rename cannot be inferred from argv, and widening the
/// predicate to guess would create false positives; the supported names are the
/// ones above.
fn viewer_prog_name(base: &str) -> bool {
    base == "native-csviewer" || base == "csviewer" || base.starts_with("csviewer-")
}

/// Lowercased basename of an argv element.
fn arg_basename(arg: &str) -> String {
    Path::new(arg)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// True for a process that is a cs viewer: the **program** (argv[0], basename)
/// is on the explicit [`viewer_prog_name`] allowlist (`csviewer`,
/// `csviewer-*`, or the v3 shipping name `native-csviewer`), or an AppRun
/// wrapper whose arguments name one. Matching the program rather than any
/// argument stops an unrelated shell command that merely mentions "csviewer"
/// from being mistaken for a viewer.
///
/// `.AppImage` mount: the mount path's basename is `csviewer` (v2) or
/// `native-csviewer` (v3), so both are handled by the allowlist; an `AppRun`
/// launcher is handled by the wrapper branch, which applies the *same*
/// allowlist to each argument instead of a substring test.
pub fn is_viewer_args(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--mcp" || a == "--selftest") {
        return false;
    }
    let Some(prog) = args.first() else { return false };
    let base = arg_basename(prog);
    if viewer_prog_name(&base) {
        return true;
    }
    if base == "apprun" {
        return args
            .iter()
            .skip(1)
            .any(|a| viewer_prog_name(&arg_basename(a)));
    }
    false
}

/// One discovered instance. `alive` is "the pid exists, is owned by the
/// recorded uid and its cmdline is still a cs viewer"; `stale` is its negation.
#[derive(Clone, Debug)]
pub struct Instance {
    pub pid: u32,
    pub uid: Option<u32>,
    pub user: String,
    pub repo: String,
    pub roots: Vec<String>,
    pub cache_dir: String,
    pub state_path: String,
    pub mode: String,
    pub port: Option<u16>,
    pub started_ms: i64,
    pub alive: bool,
    pub stale: bool,
    pub cmdline: String,
    pub record: String,
    /// `registry` for a record, `proc-scan` for the `/proc` fallback.
    pub source: &'static str,
}

impl Instance {
    pub fn to_json(&self) -> Value {
        json!({
            "pid": self.pid,
            "uid": self.uid,
            "user": self.user,
            "repo": if self.repo.is_empty() { Value::Null } else { json!(self.repo) },
            "roots": self.roots,
            "cacheDir": if self.cache_dir.is_empty() { Value::Null } else { json!(self.cache_dir) },
            "statePath": if self.state_path.is_empty() { Value::Null } else { json!(self.state_path) },
            "mode": self.mode,
            "port": self.port,
            "startedMs": if self.started_ms > 0 { json!(self.started_ms) } else { Value::Null },
            "alive": self.alive,
            "stale": self.stale,
            "cmdline": self.cmdline,
            "record": if self.record.is_empty() { Value::Null } else { json!(self.record) },
            "source": self.source,
        })
    }

    /// True when `sel` names this instance: an exact pid, or a substring of the
    /// user/repo/root/cache/state path.
    pub fn matches(&self, sel: &str) -> bool {
        let s = sel.trim();
        if s.is_empty() {
            return false;
        }
        if self.pid.to_string() == s {
            return true;
        }
        let hay = [
            self.user.as_str(),
            self.repo.as_str(),
            self.cache_dir.as_str(),
            self.state_path.as_str(),
        ];
        if hay.iter().any(|h| !h.is_empty() && h.contains(s)) {
            return true;
        }
        self.roots.iter().any(|r| !r.is_empty() && r.contains(s))
    }
}

fn num(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Judge a record against the live system.
fn judge(pid: u32, uid: Option<u32>) -> (bool, String) {
    let Some(args) = proc_args(pid) else {
        return (false, String::new());
    };
    let cmd = args.join(" ");
    if !is_viewer_args(&args) {
        return (false, cmd);
    }
    if let (Some(want), Some(have)) = (uid, proc_uid(pid)) {
        if want != have {
            return (false, cmd);
        }
    }
    (true, cmd)
}

fn instance_from_record(record_path: &Path, v: &Value) -> Option<Instance> {
    let pid = num(v, "pid")? as u32;
    if pid == 0 {
        return None;
    }
    let uid = num(v, "uid").map(|u| u as u32);
    let (alive, cmdline) = judge(pid, uid);
    let cache_dir = str_of(v, "cacheDir");
    let mut state_path = str_of(v, "statePath");
    if state_path.is_empty() && !cache_dir.is_empty() {
        state_path = state_path_for(Path::new(&cache_dir))
            .to_string_lossy()
            .into_owned();
    }
    let started_ms = v.get("startedMs").and_then(Value::as_i64).unwrap_or(0);
    Some(Instance {
        pid,
        uid,
        user: str_of(v, "user"),
        repo: str_of(v, "repo"),
        roots: v
            .get("roots")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        cache_dir,
        state_path,
        mode: {
            let m = str_of(v, "mode");
            if m.is_empty() { "gui".to_string() } else { m }
        },
        port: num(v, "port").map(|p| p as u16),
        started_ms,
        alive,
        stale: !alive,
        cmdline: if cmdline.is_empty() {
            str_of(v, "cmdline")
        } else {
            cmdline
        },
        record: record_path.to_string_lossy().into_owned(),
        source: "registry",
    })
}

/// Read every `*.json` record under `dirs` (bounded, sorted, best effort).
pub fn read_records(dirs: &[PathBuf]) -> Vec<(PathBuf, Value)> {
    let mut out: Vec<(PathBuf, Value)> = Vec::new();
    for d in dirs {
        let Ok(rd) = fs::read_dir(d) else { continue };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect();
        files.sort();
        for f in files {
            if out.len() >= MAX_RECORDS {
                return out;
            }
            let Ok(raw) = fs::read(&f) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            out.push((f, v));
        }
    }
    out
}

/// Parse one `/proc/<pid>/cmdline` into a viewer instance, best effort. An
/// argument-less GUI launch (the AppImage's `launch`) yields `cacheDir: null` —
/// honestly unknown rather than guessed at.
fn instance_from_proc(pid: u32, args: &[String], cmd: &str, repo_hint: &str) -> Instance {
    let mut repo_arg = String::new();
    let mut roots: Vec<String> = Vec::new();
    let mut port: Option<u16> = None;
    let mut mode = "gui".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" if i + 1 < args.len() => {
                repo_arg = args[i + 1].clone();
                i += 2;
            }
            "--root" if i + 1 < args.len() => {
                roots.push(args[i + 1].clone());
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                port = args[i + 1].parse::<u16>().ok();
                i += 2;
            }
            "--serve" => {
                mode = "serve".to_string();
                i += 1;
            }
            _ => i += 1,
        }
    }
    if repo_arg.is_empty() {
        repo_arg = repo_hint.to_string();
    }
    let (repo, cache_dir, state_path) = if repo_arg.is_empty() {
        (String::new(), String::new(), String::new())
    } else {
        let repo_path = crate::index::abspath(Path::new(&repo_arg));
        let cache_root = cache_root_for(&repo_path, &roots);
        (
            repo_path.to_string_lossy().into_owned(),
            cache_root.to_string_lossy().into_owned(),
            state_path_for(&cache_root).to_string_lossy().into_owned(),
        )
    };
    Instance {
        pid,
        uid: proc_uid(pid),
        user: user_of_uid(proc_uid(pid)),
        repo,
        roots,
        cache_dir,
        state_path,
        mode,
        port,
        started_ms: 0,
        alive: true,
        stale: false,
        cmdline: cmd.to_string(),
        record: String::new(),
        source: "proc-scan",
    }
}

/// Best-effort user name for a uid (reads `/etc/passwd`; no libc).
fn user_of_uid(uid: Option<u32>) -> String {
    let Some(uid) = uid else {
        return String::new();
    };
    let Ok(passwd) = fs::read_to_string("/etc/passwd") else {
        return String::new();
    };
    for line in passwd.lines() {
        let mut it = line.split(':');
        let name = it.next().unwrap_or("");
        let _pass = it.next();
        if let Some(id) = it.next() {
            if id.parse::<u32>() == Ok(uid) {
                return name.to_string();
            }
        }
    }
    String::new()
}

/// Bounded `/proc` scan for live viewer processes not already in the registry.
///
/// An AppImage starts an `AppRun`/`.AppImage` wrapper *and* the extracted
/// binary as two viewer-looking processes. When both share a uid and parent,
/// the wrapper is dropped so one viewer is one instance.
fn proc_scan(repo_hint: &str, skip: &[u32]) -> Vec<Instance> {
    struct Cand {
        pid: u32,
        uid: Option<u32>,
        ppid: Option<u32>,
        args: Vec<String>,
        cmd: String,
    }
    let mut cands: Vec<Cand> = Vec::new();
    let Ok(rd) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .collect();
    pids.sort_unstable();
    for pid in pids {
        if pid == std::process::id() || skip.contains(&pid) || cands.len() >= MAX_SCANNED * 2 {
            continue;
        }
        let Some(args) = proc_args(pid) else { continue };
        let cmd = args.join(" ");
        if !is_viewer_args(&args) {
            continue;
        }
        cands.push(Cand {
            pid,
            uid: proc_uid(pid),
            ppid: proc_ppid(pid),
            args,
            cmd,
        });
    }

    let mut out: Vec<Instance> = Vec::new();
    for c in &cands {
        if is_wrapper_args(&c.args)
            && cands.iter().any(|o| {
                o.pid != c.pid
                    && o.uid == c.uid
                    && o.ppid == c.ppid
                    && !is_wrapper_args(&o.args)
            })
        {
            continue; // the real binary is also present: one viewer, not two
        }
        out.push(instance_from_proc(c.pid, &c.args, &c.cmd, repo_hint));
        if out.len() >= MAX_SCANNED {
            break;
        }
    }
    out
}

/// Discovery runs the bounded `/proc` fallback unless this env var is truthy.
/// (`CSVIEWER_NO_PROC_SCAN=1`; sandboxes and documentation fixtures use it.)
pub const NO_PROC_SCAN_ENV: &str = "CSVIEWER_NO_PROC_SCAN";

fn proc_scan_enabled() -> bool {
    match std::env::var(NO_PROC_SCAN_ENV) {
        Ok(v) => !(v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")),
        Err(_) => true,
    }
}

/// Discover instances and say which path produced them.
///
/// The registry records and the `/proc` fallback are merged (bounded, registry
/// first for the same pid); `discovery` is `"registry"` when a live record was
/// present, `"proc-scan"` when only the fallback found live viewers, `"none"`
/// when nothing live was found at all.
pub fn discover(repo_hint: &str) -> (Vec<Instance>, &'static str) {
    discover_full(&instance_dirs(), repo_hint)
}

/// [`discover`] against an explicit set of registry directories, including the
/// bounded `/proc` fallback.
pub fn discover_full(dirs: &[PathBuf], repo_hint: &str) -> (Vec<Instance>, &'static str) {
    let mut out = discover_registry(dirs, repo_hint);
    let registry_live = out.iter().any(|i| i.alive);
    if registry_live || !proc_scan_enabled() {
        return (out, if registry_live { "registry" } else { "none" });
    }

    let known: Vec<u32> = out.iter().map(|i| i.pid).collect();
    let scanned = proc_scan(repo_hint, &known);
    let scanned_live = scanned.iter().any(|i| i.alive);
    out.extend(scanned);
    out.sort_by_key(|i| i.pid);

    let discovery = if registry_live {
        "registry"
    } else if scanned_live {
        "proc-scan"
    } else {
        "none"
    };
    (out, discovery)
}

/// Registry-only discovery: read the records under `dirs`, judge each against
/// `/proc`, and return them sorted by pid. Never performs the `/proc` scan, so
/// the answer is deterministic and testable.
pub fn discover_registry(dirs: &[PathBuf], _repo_hint: &str) -> Vec<Instance> {
    let mut registry: Vec<Instance> = read_records(dirs)
        .iter()
        .filter_map(|(p, v)| instance_from_record(p, v))
        .collect();
    registry.sort_by_key(|i| i.pid);
    registry
}

/// Read the viewer state file and its mtime in ms.
pub fn read_state_file(path: &Path) -> Option<(Value, i64)> {
    let raw = fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((value, mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csgraph-registry-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn record_round_trips_and_stale_pid_is_not_alive() {
        let dir = tmpdir("round");
        let me = std::process::id();
        // A record for this very process, but claiming to be a viewer: our own
        // cmdline is not `csviewer`, so it must be judged stale (never trusted).
        let rec = json!({
            "pid": me, "uid": current_uid().unwrap_or(0), "user": "t",
            "repo": "/w", "roots": ["r"], "cacheDir": "/w/r",
            "statePath": "/w/r/.csgraph.v2/viewer.json",
            "mode": "serve", "port": 1234, "startedMs": now_ms(),
        });
        let path = register(&rec, &dir).unwrap();
        assert!(path.exists());
        let instances = discover_registry(std::slice::from_ref(&dir), "/w");
        assert_eq!(instances.len(), 1);
        assert!(!instances[0].alive, "a non-viewer pid must never be trusted");
        assert!(instances[0].stale);
        assert_eq!(instances[0].state_path, "/w/r/.csgraph.v2/viewer.json");

        // A dead-pid record is stale too.
        let dead = json!({"pid": 999_999_999u64, "user": "t"});
        register(&dead, &dir).unwrap();
        let instances = discover_registry(std::slice::from_ref(&dir), "");
        assert_eq!(instances.len(), 2);
        assert!(instances.iter().all(|i| i.stale));

        unregister(&path);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_root_prefers_an_existing_root_else_repo() {
        let base = tmpdir("cacheroot");
        let repo = base.join("repo");
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let got = cache_root_for(&repo, &["missing".into(), root.to_string_lossy().into_owned()]);
        assert_eq!(got, root);
        let got = cache_root_for(&repo, &["missing".into()]);
        assert_eq!(got, crate::index::abspath(&repo));
        assert_eq!(
            state_path_for(&root),
            root.join(".csgraph.v2").join("viewer.json")
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn viewer_program_is_recognised_and_mcp_is_not() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_viewer_args(&args(&[
            "/tmp/.mount_csviewINDcFD/usr/bin/csviewer",
            "launch"
        ])));
        assert!(is_viewer_args(&args(&[
            "csviewer",
            "--serve",
            "--port",
            "8799"
        ])));
        assert!(is_viewer_args(&args(&[
            "/2tb/.../csviewer-Latest-x86_64.AppImage",
            "launch"
        ])));
        assert!(!is_viewer_args(&args(&[
            "target/debug/csgraph",
            "--mcp",
            "--repo",
            "/w"
        ])));
        assert!(!is_viewer_args(&args(&["csviewer", "--selftest"])));
        assert!(!is_viewer_args(&args(&["/usr/bin/python3", "csviewer.py"])));
        // A shell command that merely *mentions* csviewer is not a viewer.
        assert!(!is_viewer_args(&args(&[
            "bash",
            "-c",
            "cd native-csviewer && cargo fetch"
        ])));
        assert!(is_wrapper_args(&args(&[
            "/x/csviewer-Latest-x86_64.AppImage",
            "launch"
        ])));
        assert!(!is_wrapper_args(&args(&["/x/csviewer", "launch"])));
    }

    /// task-36: the v3 binary ships as `native-csviewer`; a live window under
    /// that name must be recognised, without turning the predicate into a
    /// substring machine.
    #[test]
    fn shipping_name_native_csviewer_is_recognised_and_near_misses_are_not() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // true: the shipping names, by bare name and at a real path.
        assert!(is_viewer_args(&args(&["csviewer"])));
        assert!(is_viewer_args(&args(&["csviewer-v3"])));
        assert!(is_viewer_args(&args(&["native-csviewer"])));
        assert!(is_viewer_args(&args(&[
            "/2tb/@/nzk/git/vrcCS/cs viewer builder/native-csviewer/target/release/native-csviewer",
            "--repo",
            "/w",
            "--root",
            "."
        ])));
        assert!(is_viewer_args(&args(&["target/debug/native-csviewer", "--snap-demo", "/tmp/x.json"])));
        // AppImage mount path (v2 shape) and an AppRun launcher around a viewer.
        assert!(is_viewer_args(&args(&["/tmp/.mount_csviewINDcFD/usr/bin/csviewer", "launch"])));
        assert!(is_viewer_args(&args(&[
            "AppRun",
            "/tmp/.mount_csviewerX/usr/bin/csviewer",
            "launch"
        ])));
        assert!(is_viewer_args(&args(&[
            "AppRun",
            "/tmp/.mount_nativecsviewerX/usr/bin/native-csviewer",
            "launch"
        ])));

        // false: near misses that a substring/prefix shortcut would accept.
        assert!(!is_viewer_args(&args(&["nativecsviewer"])));
        assert!(!is_viewer_args(&args(&["my-native-csviewer"])));
        assert!(!is_viewer_args(&args(&["/usr/bin/viewer", "--repo", "/w"])));
        assert!(!is_viewer_args(&args(&["native-csviewer "])));   // trailing space
        assert!(!is_viewer_args(&args(&["/opt/native-csviewer-helper"])));
        assert!(!is_viewer_args(&args(&["AppRun", "/tmp/mount/usr/bin/notviewer"])));
        // A shell command that merely *mentions* the v3 name stays false.
        assert!(!is_viewer_args(&args(&["bash", "-c", "pgrep native-csviewer"])));
        // #38: a renamed binary is deliberately NOT recognised.
        assert!(!is_viewer_args(&args(&["ncv", "--repo", "/w"])));
        // The MCP/selftest entry points are never viewers, whatever the name.
        assert!(!is_viewer_args(&args(&["native-csviewer", "--mcp"])));
        assert!(!is_viewer_args(&args(&["native-csviewer", "--selftest"])));
    }

    #[test]
    fn instance_selection_matches_pid_user_and_path() {
        let inst = Instance {
            pid: 471876,
            uid: Some(1000),
            user: "naruzkurai".into(),
            repo: "/2tb/@/nzk/git/vrcCS".into(),
            roots: vec!["_sealed_src".into()],
            cache_dir: "/2tb/@/nzk/git/vrcCS/_sealed_src".into(),
            state_path: "/2tb/@/nzk/git/vrcCS/_sealed_src/.csgraph.v2/viewer.json".into(),
            mode: "gui".into(),
            port: None,
            started_ms: 0,
            alive: true,
            stale: false,
            cmdline: String::new(),
            record: String::new(),
            source: "proc-scan",
        };
        assert!(inst.matches("471876"));
        assert!(inst.matches("naruzkurai"));
        assert!(inst.matches("_sealed_src"));
        assert!(inst.matches("vrcCS"));
        assert!(!inst.matches("999"));
        assert!(!inst.matches(""));
    }

    #[test]
    fn proc_instance_parses_serve_arguments() {
        let argv: Vec<String> = [
            "/x/csviewer",
            "--serve",
            "--port",
            "8799",
            "--repo",
            "/w",
            "--root",
            "_sealed_src",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let inst = instance_from_proc(471876, &argv, &argv.join(" "), "/hint");
        assert_eq!(inst.mode, "serve");
        assert_eq!(inst.port, Some(8799));
        assert_eq!(inst.repo, "/w");
        assert_eq!(inst.roots, vec!["_sealed_src".to_string()]);
        assert!(inst.state_path.ends_with(".csgraph.v2/viewer.json"));
        // An argument-less GUI launch keeps the cache dir honestly unknown.
        let gui: Vec<String> = vec!["/x/csviewer".into(), "launch".into()];
        let inst = instance_from_proc(1, &gui, "/x/csviewer launch", "");
        assert_eq!(inst.mode, "gui");
        assert_eq!(inst.cache_dir, "");
        assert_eq!(inst.state_path, "");
    }

    #[test]
    fn empty_dirs_are_honestly_empty() {
        let dir = tmpdir("empty");
        let instances = discover_registry(std::slice::from_ref(&dir), "/w");
        assert!(instances.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
