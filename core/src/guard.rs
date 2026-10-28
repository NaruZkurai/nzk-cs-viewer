//! guard — the **single write-scope guard** every engine write funnels through.
//!
//! The engine's write tools were confined to the *repository* but not to the
//! *indexed root(s)*. An MCP server bound to a Unity project as
//! `--repo <project> --root "Assets/NZK toolkit v5"` could therefore create or
//! overwrite any path under that project — `Library/`, `Temp/`,
//! `ProjectSettings/`, `Logs/`, `Packages/` — Unity-managed state where a stray
//! write is corrupting and hard to diagnose later. The default `excludes` in
//! [`crate::config`] prune *discovery*; they were never a write guard.
//!
//! [`WriteScope`] closes that hole at one place. It refuses, naming both the
//! offending path and the allowed scope:
//!
//! 1. any destination outside the repository (unconditional);
//! 2. any destination inside an engine/Unity-managed directory, even when that
//!    directory sits inside the indexed root (unconditional);
//! 3. any destination outside the configured indexed root(s) unless the caller
//!    passes an explicit `allow_outside_root: true` opt-in.
//!
//! Denial is the default. The opt-in is a single, loudly-named argument; there
//! is no configuration knob and no silent widening.

use std::path::{Path, PathBuf};

use crate::config::looks_like_unity;
use crate::index::{abspath, Index};

/// Engine/Unity-managed directory names that are never a write destination.
///
/// Matched case-insensitively against every path component under the
/// repository. `Packages` is Unity-managed and is denied only for a Unity
/// project ([`looks_like_unity`]); the rest are denied unconditionally.
pub const MANAGED_DIRS: &[&str] = &[
    "Library",
    "Temp",
    "Obj",
    "obj",
    "Logs",
    ".git",
    "node_modules",
    "target",
    "bin",
    "ProjectSettings",
    "Packages",
];

/// The one managed directory that is only managed for a Unity project.
const UNITY_ONLY_MANAGED: &str = "Packages";

/// The default root the `csgraph` CLI uses when no `--root` is given.
const DEFAULT_ROOT: &str = "_sealed_src";

/// True when `name` is an engine/Unity-managed directory component.
pub fn is_managed_dir(name: &str, unity: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    for d in MANAGED_DIRS {
        if *d == UNITY_ONLY_MANAGED {
            if unity && lower == UNITY_ONLY_MANAGED.to_ascii_lowercase() {
                return true;
            }
            continue;
        }
        if d.to_ascii_lowercase() == lower {
            return true;
        }
    }
    false
}

/// The first engine/Unity-managed component in `path`, if any. Used by write
/// paths that have no repository handle and can therefore only enforce rule 2.
pub fn first_managed_dir(path: &Path, unity: bool) -> Option<String> {
    for comp in path.components() {
        let name = comp.as_os_str().to_string_lossy().into_owned();
        if is_managed_dir(&name, unity) {
            return Some(name);
        }
    }
    None
}

/// A resolved write scope: one repository plus its configured indexed root(s).
#[derive(Clone, Debug)]
pub struct WriteScope {
    repo: PathBuf,
    roots: Vec<PathBuf>,
    /// The configured root spellings, for error text.
    root_names: Vec<String>,
    unity: bool,
}

impl WriteScope {
    /// Build a scope from a repository and its configured root spellings.
    ///
    /// Root entries may be relative (resolved against `repo`) or absolute; a
    /// root that names a file scopes to its parent, mirroring
    /// [`Index::cache_dir`]. An empty root list scopes to the repository
    /// itself, so a scope is never "no writable location at all".
    pub fn new(repo: &Path, roots: &[String], unity: bool) -> WriteScope {
        let repo = resolve_best_effort(&normalize(repo));
        let mut resolved: Vec<PathBuf> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for raw in roots {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let joined = {
                let p = Path::new(raw);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    repo.join(p)
                }
            };
            let mut abs = resolve_best_effort(&normalize(&joined));
            if abs.is_file() {
                if let Some(parent) = abs.parent() {
                    abs = parent.to_path_buf();
                }
            }
            names.push(raw.to_string());
            resolved.push(abs);
        }
        if resolved.is_empty() {
            names.push("<repo>".to_string());
            resolved.push(repo.clone());
        }
        WriteScope { repo, roots: resolved, root_names: names, unity }
    }

    /// The scope of one [`Index`]: its repository, the roots of the last build
    /// (falling back to the configured roots before a build), and whether the
    /// repository looks like a Unity project.
    pub fn for_index(index: &Index) -> WriteScope {
        let mut roots: Vec<String> = index.roots().to_vec();
        if roots.is_empty() {
            roots = index.config().roots.clone();
        }
        WriteScope::new(index.repo_path(), &roots, looks_like_unity(index.repo_path()))
    }

    /// The scope the *process* was started with (`--repo DIR [--root REL]...`),
    /// or `None` when the process has no bound repository.
    ///
    /// `reorg::Reorg::write_tree` is the one write chokepoint that carries no
    /// [`Index`]; the stdio MCP server that binds it is started from the command
    /// line, so its `--repo`/`--root` are the authoritative scope there. This is
    /// a fallback only — a caller that has a real `Index` must pass a scope
    /// derived from it (see [`Reorg::set_write_scope`](crate::reorg::Reorg::set_write_scope)).
    pub fn from_process_args() -> Option<WriteScope> {
        scope_from_args(&std::env::args().collect::<Vec<String>>())
    }

    /// The repository this scope is confined to.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The configured indexed root spellings, for messages.
    pub fn root_names(&self) -> &[String] {
        &self.root_names
    }

    /// True when `name` is managed in this scope.
    pub fn is_managed(&self, name: &str) -> bool {
        is_managed_dir(name, self.unity)
    }

    /// Rule 1+2+3, denial by default.
    pub fn check(&self, path: &Path) -> Result<PathBuf, String> {
        self.check_optin(path, false)
    }

    /// Rule 1+2 always; rule 3 unless `allow_outside_root`.
    ///
    /// `path` may be relative (resolved against the repository) or absolute and
    /// need not exist. Returns the resolved absolute path on success.
    pub fn check_optin(&self, path: &Path, allow_outside_root: bool) -> Result<PathBuf, String> {
        let raw = path.display().to_string();
        let abs = if path.is_absolute() {
            resolve_best_effort(&normalize(path))
        } else {
            resolve_best_effort(&normalize(&self.repo.join(path)))
        };
        if !abs.starts_with(&self.repo) {
            let why = format!("it is outside the repository {}", self.repo.display());
            return Err(self.refuse(&raw, &abs, &why));
        }
        if let Ok(rel) = abs.strip_prefix(&self.repo) {
            for comp in rel.components() {
                let name = comp.as_os_str().to_string_lossy().into_owned();
                if self.is_managed(&name) {
                    let why =
                        format!("it is inside the engine-managed directory {name:?}");
                    return Err(self.refuse(&raw, &abs, &why));
                }
            }
        }
        if !allow_outside_root && !self.roots.iter().any(|r| abs.starts_with(r)) {
            let why = "it is outside the indexed root(s); pass allow_outside_root=true \
                       to opt in explicitly";
            return Err(self.refuse(&raw, &abs, why));
        }
        Ok(abs)
    }

    fn refuse(&self, raw: &str, abs: &Path, reason: &str) -> String {
        format!(
            "write-scope refusal: path {raw:?} resolves to {}, {reason}; writes are confined \
             to the indexed root(s) [{}] inside the repository {}",
            abs.display(),
            self.root_names.join(", "),
            self.repo.display()
        )
    }
}

/// Parse `--repo DIR [--repo DIR]... [--root REL]...` (the first `--repo` wins)
/// into a scope. `None` when there is no `--repo`.
pub fn scope_from_args(args: &[String]) -> Option<WriteScope> {
    let mut repo: Option<String> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--repo" {
            if repo.is_none() {
                repo = args.get(i + 1).cloned();
            }
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--repo=") {
            if repo.is_none() {
                repo = Some(v.to_string());
            }
        } else if a == "--root" {
            if let Some(v) = args.get(i + 1) {
                roots.push(v.clone());
            }
            i += 2;
            continue;
        } else if let Some(v) = a.strip_prefix("--root=") {
            roots.push(v.to_string());
        }
        i += 1;
    }
    let repo = repo?;
    if repo.is_empty() {
        return None;
    }
    if roots.is_empty() {
        roots.push(DEFAULT_ROOT.to_string());
    }
    let repo = PathBuf::from(repo);
    let unity = looks_like_unity(&repo);
    Some(WriteScope::new(&repo, &roots, unity))
}

/// `os.path.abspath` + lexical `.`/`..` collapse, without touching the disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// Canonicalise the longest existing prefix of `abs` and re-append the
/// non-existent tail. A path that does not exist yet (a `create` destination)
/// is still checked against every symlinked ancestor that *does* exist, so a
/// symlink cannot be used to escape the scope.
fn resolve_best_effort(abs: &Path) -> PathBuf {
    let abs = abspath(abs);
    let mut anc = abs.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = anc.canonicalize() {
            let mut out = canon;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        match (anc.parent().map(Path::to_path_buf), anc.file_name()) {
            (Some(parent), Some(name)) if parent != anc => {
                tail.push(name.to_os_string());
                anc = parent;
            }
            _ => return abs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csgraph-guard-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A Unity-shaped repo: root "Assets/NZK toolkit v5", plus the managed
    /// directories Unity itself owns.
    fn unity_repo(tag: &str) -> PathBuf {
        let dir = scratch(tag);
        for d in [
            "Assets/NZK toolkit v5",
            "Assets/Other",
            "Library",
            "Temp",
            "Logs",
            "ProjectSettings",
            "Packages",
            "obj",
        ] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        dir
    }

    fn unity_scope(dir: &Path) -> WriteScope {
        WriteScope::new(dir, &["Assets/NZK toolkit v5".to_string()], true)
    }

    #[test]
    fn refuses_managed_directories_even_inside_the_root() {
        let dir = unity_repo("managed");
        let scope = unity_scope(&dir);
        for rel in [
            "Library/foo.txt",
            "ProjectSettings/ProjectSettings.asset",
            "Temp/scratch.cs",
            "Logs/Editor.log",
            "obj/Debug/x.cs",
            "Packages/manifest.json",
            ".git/config",
            "Assets/NZK toolkit v5/Library/nested.txt",
        ] {
            let err = scope.check(&dir.join(rel)).unwrap_err();
            assert!(err.starts_with("write-scope refusal:"), "{rel}: {err}");
            assert!(err.contains(rel), "refusal must name the offending path: {rel}: {err}");
            assert!(
                err.contains("engine-managed directory"),
                "refusal must name the reason: {rel}: {err}"
            );
            assert!(
                err.contains("Assets/NZK toolkit v5"),
                "refusal must name the allowed scope: {rel}: {err}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_outside_the_indexed_root_unless_opted_in() {
        let dir = unity_repo("root");
        let scope = unity_scope(&dir);
        let other = dir.join("Assets/Other/file.cs");
        let err = scope.check(&other).unwrap_err();
        assert!(err.contains("outside the indexed root(s)"), "{err}");
        assert!(err.contains("allow_outside_root=true"), "{err}");
        assert!(err.contains("Assets/Other/file.cs"), "{err}");
        // the explicit opt-in widens the root, nothing else
        assert!(scope.check_optin(&other, true).is_ok(), "opt-in must allow it");
        // and the opt-in never widens past a managed directory
        let lib = dir.join("Library/foo.txt");
        let err = scope.check_optin(&lib, true).unwrap_err();
        assert!(err.contains("engine-managed directory"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_outside_the_repository_unconditionally() {
        let dir = unity_repo("repo");
        let scope = unity_scope(&dir);
        for raw in ["/etc/passwd", "../outside.cs"] {
            let err = scope.check(Path::new(raw)).unwrap_err();
            assert!(err.contains("outside the repository"), "{raw}: {err}");
            assert!(scope.check_optin(Path::new(raw), true).is_err(),
                "the opt-in must never widen past the repository: {raw}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn allows_ordinary_writes_inside_the_root() {
        let dir = unity_repo("allow");
        let scope = unity_scope(&dir);
        let ok = scope.check(&dir.join("Assets/NZK toolkit v5/Test.cs")).unwrap();
        assert_eq!(ok, dir.canonicalize().unwrap().join("Assets/NZK toolkit v5/Test.cs"));
        // a not-yet-existing nested destination is allowed too
        assert!(scope.check(&dir.join("Assets/NZK toolkit v5/sub/New.cs")).is_ok());
        // a relative path resolves against the repository
        assert!(scope.check(Path::new("Assets/NZK toolkit v5/Test.cs")).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn packages_is_managed_only_for_unity_projects() {
        let dir = scratch("packages");
        fs::create_dir_all(dir.join("Packages")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        let plain = WriteScope::new(&dir, &[".".to_string()], false);
        assert!(plain.check(&dir.join("Packages/manifest.json")).is_ok(),
            "a non-Unity repo may own a Packages directory");
        let unity = WriteScope::new(&dir, &[".".to_string()], true);
        let err = unity.check(&dir.join("Packages/manifest.json")).unwrap_err();
        assert!(err.contains("engine-managed directory \"Packages\""), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_from_args_reads_the_stdio_server_binding() {
        let dir = unity_repo("args");
        let args: Vec<String> = vec![
            "csgraph".into(),
            "--mcp".into(),
            "--repo".into(),
            dir.to_string_lossy().into_owned(),
            "--root".into(),
            "Assets/NZK toolkit v5".into(),
        ];
        let scope = scope_from_args(&args).expect("a --repo must yield a scope");
        assert!(scope.check(&dir.join("Assets/NZK toolkit v5/A.cs")).is_ok());
        let err = scope.check(&dir.join("Library/foo.txt")).unwrap_err();
        assert!(err.contains("engine-managed directory"), "{err}");
        let err = scope.check(&dir.join("Assets/Other/B.cs")).unwrap_err();
        assert!(err.contains("outside the indexed root(s)"), "{err}");
        assert!(scope_from_args(&["csgraph".into(), "--mcp".into()]).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn for_index_uses_the_build_roots() {
        let dir = unity_repo("index");
        fs::write(
            dir.join("Assets/NZK toolkit v5/A.cs"),
            "namespace N { public class A { } }\n",
        )
        .unwrap();
        fs::write(dir.join("Assets/Other/B.cs"), "namespace N { public class B { } }\n").unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&["Assets/NZK toolkit v5".to_string()], &[], &crate::config::Config::default());
        assert!(ix.indexed, "fixture must index");
        let scope = WriteScope::for_index(&ix);
        assert!(scope.check(&dir.join("Assets/NZK toolkit v5/A.cs")).is_ok());
        let err = scope.check(&dir.join("Assets/Other/B.cs")).unwrap_err();
        assert!(err.contains("outside the indexed root(s)"), "{err}");
        let err = scope.check(&dir.join("Library/foo.txt")).unwrap_err();
        assert!(err.contains("engine-managed directory"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
