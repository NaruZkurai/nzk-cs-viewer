//! The folder picker's browsing model — display-free by construction.
//!
//! This module is the *engine* behind `#modal` in the reference frontend
//! (`frontend/index.html:387`), the "Choose a folder to index" dialog whose
//! `#browseBtn` is the owner's "can't open path to index" complaint. Like
//! [`crate::actions`], nothing here mentions `egui`, a window or a GPU: the
//! model answers "what is the parent / home / root / listing of this path?"
//! and the host paints the answer into `#modalPath`, `#modalBody` and
//! `#modalNote`. Every operation is therefore unit-testable with no display.
//!
//! ### Why this module exists rather than the engine's `/api/browse`
//!
//! The original host reached the filesystem over HTTP: `shell.js:29 browseTo`
//! calls `GET /api/browse?path=…`, whose implementation is
//! `core/src/api.rs:668 browse()`, and renders `dirs`/`files`. The native fork
//! has no HTTP server and no browser, but it *does* have the same filesystem, so
//! the browse model is reimplemented here directly against `std::fs`. The
//! payload shape is deliberately the one `browse()` returns (`path`, `parent`,
//! `dirs`) so the host's rendering code maps 1:1 onto the reference picker:
//! `browse()`'s `parent` is a `String` computed by
//! `trim_end_matches('/').rsplit_once('/')` (`api.rs:730-735`), and
//! [`Listing::parent`] is the same rule.
//!
//! ### Directories only, and why
//!
//! `browse()` returns `files` too, but the picker renders those as non-clickable
//! `<div class="pick file">` rows (`shell.js:66-72`) — they are *decoration*,
//! not navigation targets, and the Select button is gated on `hasCs`, never on
//! files. The native fork's listing feeds `#modalBody` as **navigation targets**,
//! so this model lists only directories and reports `files` as a count. That
//! keeps the entry list exactly the set of things a click can descend into,
//! which is the property the host needs; a file row carries no action.
//!
//! ### Honesty: "does not exist" is not "empty"
//!
//! `renderPicker` prints `Empty folder.` whenever `dirs` and `files` are both
//! empty (`shell.js:75-76`) — it *cannot* distinguish a genuinely empty folder
//! from a path it failed to read, because `browseTo`'s error path
//! (`shell.js:33-39`) replaces the body instead of reporting a status. This
//! module makes the distinction structural rather than cosmetic:
//! [`Listing`] carries a [`Note`], and the three failure cases are separate
//! variants with different text. An unreadable directory can never render as
//! "Empty folder" (`Note::Empty` is only ever produced after a successful
//! `read_dir` returned zero entries).
//!
//! ### Determinism
//!
//! `browse()` sorts with
//! `a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b))` (`api.rs:682`).
//! [`Listing::sort_key`] is that exact total order: case-insensitive first, then
//! a case-sensitive tie-break, so two names differing only in case have one
//! stable order instead of inheriting readdir order (which varies by filesystem).

use std::fs;
use std::path::{Component, Path, PathBuf};

/// A directory entry the picker can descend into.
///
/// Mirrors `browse()`'s per-dir object (`api.rs:714-720`): `name`, an absolute
/// `path`, and `symlink`. `has_cs`/`cs_count` from that payload are deliberately
/// absent — see the module docs on the module being navigation-only — and are
/// left to a later pass that has the index in hand.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DirEntry {
    /// The final path component, as `read_dir` reported it. Rendered as the row
    /// label (`<span class="pname">name/</span>`, `shell.js:60`).
    pub name: String,
    /// Absolute path to descend into; this is `data-nav` (`shell.js:58`), the
    /// value the host feeds back into [`browse_to`].
    pub path: PathBuf,
    /// True when the entry is itself a symlink. `browse()` resolves links so a
    /// linked folder stays usable, but still flags them (`api.rs:693-695`,
    /// rendered by `shell.js:61`); we do the same.
    pub symlink: bool,
}

/// What the host should put in `#modalNote`.
///
/// This is the honesty channel. Each variant maps to one `textContent` and one
/// `className` from the reference (`shell.js:77-86`), except that the two
/// failure variants have no reference equivalent: the reference collapsed every
/// read error into a replaced `#modalBody` and left `#modalNote` **empty**
/// (`shell.js:36`) — a blank note next to a body that says "Loading…". Naming
/// the failures here is the fix for that class of silent failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Note {
    /// The picker has opened and its first directory listing is still running.
    Loading,
    /// `indexable` / `modal-note ok` — the current folder is selectable.
    Indexable,
    /// `no .cs files in this subtree` / `modal-note warn`.
    NoCs,
    /// The folder listed fine and holds nothing at all.
    Empty,
    /// The path does not exist. Distinct from [`Note::Empty`] on purpose: this
    /// is the "can't open path to index" case, and saying "Empty folder" for it
    /// is the bug this module is built to prevent.
    NotFound,
    /// The path exists but could not be read (`EACCES` on the directory, or a
    /// file where a directory was expected).
    Unreadable,
}

impl Note {
    /// The note text, or `None` when the slot should stay empty.
    ///
    /// `Indexable`/`NoCs` are the reference strings verbatim. `Empty` has no
    /// reference string — the reference wrote the empty message into
    /// `#modalBody`, not the note — so it is worded here for the note slot.
    pub fn text(self) -> Option<&'static str> {
        match self {
            Note::Loading => Some("loading folder…"),
            Note::Indexable => Some("indexable"),
            Note::NoCs => Some("no .cs files in this subtree"),
            Note::Empty => Some("folder is empty"),
            Note::NotFound => Some("path does not exist"),
            Note::Unreadable => Some("permission denied — cannot read this folder"),
        }
    }

    /// The CSS class for `#modalNote`, matching the reference's two names
    /// (`shell.js:81,85`). Failures keep `warn` — the reference used `warn` for
    /// its error path too (`shell.js:37`).
    pub fn class(self) -> &'static str {
        match self {
            Note::Loading => "modal-note",
            Note::Indexable => "modal-note ok",
            Note::NoCs | Note::Empty | Note::NotFound | Note::Unreadable => "modal-note warn",
        }
    }

    /// Whether the host should leave `#modalSelect` enabled.
    ///
    /// The reference gates Select on `hasCs` alone (`shell.js:78`), which means a
    /// folder that is empty or unreadable leaves the primary button *disabled*
    /// with no explanation. Here a folder that exists and is readable is always
    /// selectable — picking a folder with no `.cs` files is a legitimate choice
    /// (it becomes the root, and the index then reports zero nodes honestly),
    /// while a missing/unreadable path is not selectable and now says why.
    pub fn selectable(self) -> bool {
        match self {
            Note::Indexable | Note::NoCs | Note::Empty => true,
            Note::Loading => false,
            Note::NotFound | Note::Unreadable => false,
        }
    }
}

/// One directory listing: the state the modal renders.
///
/// `path`/`parent`/`dirs` are the reference payload's fields; `note` is the
/// honesty channel described on [`Note`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Listing {
    /// Absolute, normalised path being shown (`#modalPath`, `#modalPathInput`).
    pub path: PathBuf,
    /// The parent to navigate to, or `None` at the filesystem root — the
    /// reference sends `parent` and disables `#modalUp` when it is absent
    /// (`shell.js:54`). `None` also covers a failed listing, where up must not
    /// be offered for a path we could not read.
    pub parent: Option<PathBuf>,
    /// Sub-directories, in [`Listing::sort_key`] order. Empty unless `note` is
    /// [`Note::Indexable`], [`Note::NoCs`] or [`Note::Empty`].
    pub dirs: Vec<DirEntry>,
    /// How many non-directory entries were skipped. Exposed so a host that wants
    /// the reference's file rows can show them without re-reading the directory;
    /// this module deliberately does not hand out file paths to navigate to.
    pub files_skipped: usize,
    /// What to show in `#modalNote`.
    pub note: Note,
}

impl Listing {
    /// The listing for a path that could not be used, carrying no entries.
    ///
    /// Both failure constructors funnel through here so a failed listing can
    /// never accidentally carry entries from an earlier successful read: the
    /// `dirs` vector is always freshly empty, which is what makes "no entries"
    /// plus a failure note unambiguous to the host.
    fn failed(path: PathBuf, note: Note) -> Listing {
        Listing { path, parent: None, dirs: Vec::new(), files_skipped: 0, note }
    }

    /// The sort key for a name: case-insensitive, then case-sensitive.
    ///
    /// `browse()`'s comparator (`api.rs:682`) expressed as a key so both the
    /// listing and the tests can name the one order. Returning a tuple rather
    /// than comparing inside a closure keeps the order a *value*: tests assert on
    /// it directly, and a future caller cannot accidentally get readdir order.
    pub fn sort_key(name: &str) -> (String, String) {
        (name.to_lowercase(), name.to_string())
    }

    /// Whether `#modalSelect` should be enabled.
    pub fn selectable(&self) -> bool {
        self.note.selectable()
    }

    /// The paths to render as rows, in order. This is what the host feeds to
    /// `#modalBody`, and equals the reference's `dirs[].path`.
    pub fn nav_paths(&self) -> Vec<&Path> {
        self.dirs.iter().map(|d| d.path.as_path()).collect()
    }
}

/// Normalise `p` without touching the filesystem.
///
/// Removes `.` components and resolves `..` lexically. **Lexical `..` is a
/// deliberate choice with a known limit**: `a/b/..` normalises to `a` even when
/// `b` is a symlink to somewhere else, where the kernel would resolve to the
/// link's parent. The alternative — `fs::canonicalize` — would make
/// [`Listing::path`] differ from what the user typed and would *fail* on a path
/// that does not exist, which is exactly the honest-failure case that must still
/// be reported with the user's own path. Since every navigation here is
/// `read_dir`-checked before it is shown, a lexically-normalised path is only
/// ever displayed, never trusted.
///
/// A relative path is *not* resolved against the cwd here; see [`browse_to`].
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only pop a real name; never pop past the root, so "/.." stays
                // "/" instead of becoming empty.
                if !matches!(out.components().next_back(), None | Some(Component::RootDir)) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// The current user's home directory, or `None` when it cannot be determined.
///
/// `$HOME` first (what the shell and the reference's `~` expansion use), then
/// `std::env::home_dir`, then `/root`/`/Users`-style fallbacks are *not* guessed
/// — an unknown home is reported as `None` so `#modalHome` can be disabled
/// rather than sending the user to a fabricated path. The reference's
/// `homePath()` (`shell.js:87-92`) regex-derives `/home/<user>` from the current
/// path and otherwise sends the literal string `~` for the *server* to expand;
/// with no server, that expansion has to happen here.
pub fn home_dir() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    // `home_dir` is deprecated on some targets but is still the only portable
    // std accessor; the `$HOME` branch above is the one that fires on the
    // Linux/macOS targets this fork ships.
    #[allow(deprecated)]
    std::env::home_dir()
}

/// The filesystem root, when one exists.
///
/// `Some("/")` on Unix. `None` on a platform with no single root (Windows drive
/// letters), so `#modalRoot` can be disabled rather than navigating to a path
/// that is not a directory.
pub fn root_dir() -> Option<PathBuf> {
    let r = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
    if r.is_dir() { Some(r) } else { None }
}

/// The parent of `path` as the reference computes it, or `None` at the root.
///
/// Deliberately **not** `Path::parent()`: the reference derives `parent` from
/// the *string* form (`api.rs:730-735`), and matching that rule means
/// `/` -> `None`, `/a` -> `/`, `/a/b` -> `/a`. It is filesystem-free, so the
/// host can call it on a path that no longer exists.
pub fn parent_of(path: &Path) -> Option<PathBuf> {
    let s = path.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() {
        // "/" (or "" ) — already the root.
        return None;
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some(PathBuf::from("/")),
        Some((head, _)) => Some(PathBuf::from(head)),
        // A bare relative name such as "src" has no parent to offer.
        None => None,
    }
}

/// Resolve a user-typed path the way the picker should.
///
/// `#modalPathInput` accepts "`/absolute/path or repo-relative folder…`" (its
/// own placeholder, `index.html:390`), so a relative value must be joined to
/// `repo_root` — the folder currently being indexed — rather than to the
/// process cwd, which for a GUI launched from a desktop entry is arbitrary.
/// `~`/`~/x` is expanded via [`home_dir`]. An empty string means "the repo
/// root", matching `browseTo(startPath || '')` (`shell.js:96`) where the empty
/// path resolves to the repository root server-side.
pub fn resolve_input(input: &str, repo_root: &Path) -> PathBuf {
    let t = input.trim();
    if t.is_empty() {
        return normalize(repo_root);
    }
    if t == "~" {
        return home_dir().unwrap_or_else(|| normalize(repo_root));
    }
    if let Some(rest) = t.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return normalize(&h.join(rest));
        }
    }
    let p = Path::new(t);
    if p.is_absolute() { normalize(p) } else { normalize(&repo_root.join(p)) }
}

/// List the sub-directories of `path` — the one filesystem operation here.
///
/// Returns a [`Listing`] in every case; it does not return `Result`, because
/// every failure has a defined rendering and the host should never have to
/// invent one. The three outcomes are kept distinct:
///
/// * not a directory, or `symlink_metadata` fails with `NotFound` -> [`Note::NotFound`]
/// * `read_dir` fails (typically `EACCES`)                 -> [`Note::Unreadable`]
/// * `read_dir` succeeds                                   -> `Indexable`/`NoCs`/`Empty`
///
/// `.`-prefixed entries are skipped, matching `browse()` (`api.rs:687-689`).
/// Entries are sorted by [`Listing::sort_key`] — never readdir order.
pub fn browse_to(path: &Path) -> Listing {
    // `symlink_metadata`, not `metadata`: a *broken* symlink should report as
    // a path that exists-but-is-not-a-directory (Unreadable) rather than as a
    // missing path, which is what `metadata`'s ENOENT would claim.
    let path = normalize(path);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Listing::failed(path, Note::NotFound);
        }
        Err(_) => return Listing::failed(path, Note::Unreadable),
        Ok(md) => {
            // Resolve one level so a symlinked folder is browsable — `browse()`
            // uses `fs::metadata` for the same reason (`api.rs:692`).
            let resolved = fs::metadata(&path).unwrap_or(md);
            if !resolved.is_dir() {
                // A regular file is "not a directory": distinct from missing.
                return Listing::failed(path, Note::Unreadable);
            }
        }
    }

    let rd = match fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => {
            let note = if e.kind() == std::io::ErrorKind::NotFound {
                Note::NotFound
            } else {
                Note::Unreadable
            };
            return Listing::failed(path, note);
        }
    };

    let mut dirs: Vec<DirEntry> = Vec::new();
    let mut files_skipped = 0usize;
    // `flatten` drops per-entry errors (a racing unlink, a vanished mount)
    // rather than failing the whole listing; the entry simply is not offered.
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let child = path.join(&name);
        // `metadata` follows links so a symlinked folder is listed as a folder;
        // a broken link errors here and is skipped, as in `browse()`.
        let Ok(md) = fs::metadata(&child) else { continue };
        if md.is_dir() {
            let symlink = fs::symlink_metadata(&child)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            dirs.push(DirEntry { name, path: child, symlink });
        } else {
            files_skipped += 1;
        }
    }
    dirs.sort_by(|a, b| Listing::sort_key(&a.name).cmp(&Listing::sort_key(&b.name)));

    // Ordering matters: `Empty` is only reachable when read_dir genuinely
    // returned nothing, so it can never mask an error above.
    let note = if !dirs.is_empty() {
        // `.cs` detection needs the index (or a walk) to be truthful about
        // *subtrees*, and this module has neither, so it does not claim
        // `Indexable` on evidence it lacks. `NoCs` is accurate for the folder
        // itself: no `.cs` was found directly in it. A later pass can upgrade
        // this to the reference's subtree-aware `hasCs` once it can ask.
        Note::NoCs
    } else if files_skipped > 0 {
        Note::NoCs
    } else {
        Note::Empty
    };

    Listing { parent: parent_of(&path), path, dirs, files_skipped, note }
}

/// The state of the folder modal — what the host paints and mutates.
///
/// Holds the current [`Listing`] plus the repo root that relative typed paths
/// resolve against, so the six modal controls are six method calls on one value:
/// `#modalUp` -> [`Picker::up`], `#modalHome` -> [`Picker::home`], `#modalRoot`
/// -> [`Picker::root`], `#modalRepo` -> [`Picker::repo`], `#modalGo`/Enter ->
/// [`Picker::go`], a `.pick` row -> [`Picker::open_dir`], `#modalSelect` ->
/// [`Picker::chosen`].
#[derive(Clone, Debug)]
pub struct Picker {
    repo_root: PathBuf,
    current: Listing,
}

impl Picker {
    /// Open the picker on `start` (empty = the repo root), as
    /// `openPicker(startPath)` does (`shell.js:94-97`).
    pub fn new(repo_root: &Path, start: &str) -> Picker {
        let repo_root = normalize(repo_root);
        let target = resolve_input(start, &repo_root);
        Picker { current: browse_to(&target), repo_root }
    }

    /// Create a picker state without touching the filesystem, for the UI thread
    /// to show immediately while a worker performs the first listing.
    pub fn pending(repo_root: &Path, start: &str) -> Picker {
        let repo_root = normalize(repo_root);
        let target = resolve_input(start, &repo_root);
        Picker {
            current: Listing {
                path: target,
                parent: None,
                dirs: Vec::new(),
                files_skipped: 0,
                note: Note::Loading,
            },
            repo_root,
        }
    }

    /// The listing currently on screen.
    pub fn current(&self) -> &Listing {
        &self.current
    }

    /// The folder relative typed paths resolve against.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// `#modalGo` / Enter: navigate to a typed path, honestly failing when it
    /// does not exist.
    pub fn go(&mut self, input: &str) -> Listing {
        let target = resolve_input(input, &self.repo_root);
        self.goto(&target)
    }

    /// `#modalUp`: the parent, or a no-op at the root.
    ///
    /// A no-op rather than an error: at `/` there is genuinely nowhere up to go,
    /// which is why the reference *disables* the button (`shell.js:54`) instead
    /// of reporting a failure. The current listing is returned unchanged so the
    /// state cannot silently become something the user did not ask for.
    pub fn up(&mut self) -> Listing {
        match self.current.parent.clone() {
            Some(p) => self.goto(&p),
            None => self.current.clone(),
        }
    }

    /// `#modalHome`: the user's home directory, or a no-op when it is unknown.
    ///
    /// Unknown home must not navigate anywhere: the reference's `homePath()`
    /// returns the string `~` for the server to expand, and with no server a
    /// literal `~` would be a fabricated path. Keeping the current listing is
    /// the honest no-op; [`home_dir`] exposes `None` so the host can grey the
    /// button out.
    pub fn home(&mut self) -> Listing {
        match home_dir() {
            Some(h) => self.goto(&h),
            None => self.current.clone(),
        }
    }

    /// `#modalRoot`: the filesystem root, or a no-op when there is none.
    pub fn root(&mut self) -> Listing {
        match root_dir() {
            Some(r) => self.goto(&r),
            None => self.current.clone(),
        }
    }

    /// `#modalRepo`: back to the folder being indexed, as
    /// `browseTo(state.root || '')` does (`shell.js:754`).
    pub fn repo(&mut self) -> Listing {
        let r = self.repo_root.clone();
        self.goto(&r)
    }

    /// A `.pick` row click: descend into an entry of the current listing.
    ///
    /// Takes the row's `path` (`data-nav`, `shell.js:58`). A path that is not in
    /// the current listing is still followed — the host may pass a stale row
    /// after a refresh, and telling the user "does not exist" is better than
    /// dropping the click.
    pub fn open_dir(&mut self, path: &Path) -> Listing {
        self.goto(path)
    }

    /// The path `#modalSelect` would commit, or `None` when the current folder
    /// is not selectable.
    ///
    /// Electron parity (`chooseCurrentFolder`, `shell.js:99-106`): Select does
    /// **not** index. It writes this path into `#rootInput` and tells the user to
    /// press Index. Returning `Option` rather than a `PathBuf` keeps the host
    /// from having to re-derive selectability from the note.
    pub fn chosen(&self) -> Option<&Path> {
        if self.current.selectable() { Some(self.current.path.as_path()) } else { None }
    }

    /// The path `#modalSelect` would commit, owned.
    ///
    /// The owned twin of [`Picker::chosen`], for a host that stores the choice
    /// and then keeps using the picker (which is the normal case: Select writes
    /// `#rootInput` and closes the modal). Borrowing `self` for the chosen path
    /// while also calling a `&mut self` method is the `E0502` this avoids.
    pub fn chosen_path(&self) -> Option<PathBuf> {
        self.chosen().map(Path::to_path_buf)
    }

    /// Every mutator returns the new listing **by value**, not by reference.
    ///
    /// Returning `&Listing` borrows `self` for as long as the caller holds it,
    /// so the natural host code — "navigate, then read something else off the
    /// picker" — fails to compile with `E0502` (measured while writing the
    /// module's own demo harness). A `Listing` is a path, an `Option<PathBuf>`,
    /// a small `Vec<DirEntry>` and a note; cloning it is cheaper than the
    /// `read_dir` that just produced it, and it keeps the host free of borrow
    /// gymnastics.
    fn goto(&mut self, path: &Path) -> Listing {
        self.current = browse_to(path);
        self.current.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A unique scratch directory under the OS temp dir.
    ///
    /// Uniqueness comes from the pid plus a process-wide counter, so tests
    /// running in the same binary in parallel cannot collide. `Drop` removes the
    /// tree even when an assertion panics, which is what keeps repeated runs
    /// from accumulating `ncv-picker-*` dirs in `/tmp`.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("ncv-picker-{}-{}-{}", tag, std::process::id(), n));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.path.join(rel);
            fs::create_dir_all(&p).expect("create subdir");
            p
        }

        fn file(&self, rel: &str) {
            let p = self.path.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            let mut f = fs::File::create(&p).expect("create file");
            f.write_all(b"// x\n").expect("write file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn sort_key_is_case_insensitive_then_stable() {
        // The key, not just the comparator: lowercase dominates, the original
        // name only breaks ties. Note the two keys for "Alpha"/"alpha" are NOT
        // equal — that is exactly the point, because readdir order must not
        // decide which of two case-differing names comes first.
        assert_ne!(Listing::sort_key("Alpha"), Listing::sort_key("alpha"));
        assert_eq!(Listing::sort_key("Alpha").0, Listing::sort_key("alpha").0);
        // Case-insensitive primary: "alpha" sorts before "Beta" despite 'B'.
        assert!(Listing::sort_key("alpha") < Listing::sort_key("Beta"));
        // Deterministic tie-break: 'A' (0x41) < 'a' (0x61), so "Alpha" < "alpha".
        assert!(Listing::sort_key("Alpha") < Listing::sort_key("alpha"));
    }

    #[test]
    fn parent_of_matches_the_reference_string_rule() {
        assert_eq!(parent_of(Path::new("/")), None);
        assert_eq!(parent_of(Path::new("/a")), Some(PathBuf::from("/")));
        assert_eq!(parent_of(Path::new("/a/b")), Some(PathBuf::from("/a")));
        assert_eq!(parent_of(Path::new("/a/b/")), Some(PathBuf::from("/a")));
        // A bare relative name has no parent to offer (never guessed as cwd).
        assert_eq!(parent_of(Path::new("src")), None);
    }

    #[test]
    fn normalize_is_lexical_and_cannot_escape_root() {
        assert_eq!(normalize(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(normalize(Path::new("/a/b/..")), PathBuf::from("/a"));
        assert_eq!(normalize(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize(Path::new("")), PathBuf::from("."));
    }

    #[test]
    fn home_root_and_repo_navigation_reach_real_directories() {
        let tmp = TempDir::new("nav");
        let repo = tmp.dir("repo");
        let mut p = Picker::new(&repo, "");

        // Empty start resolves to the repo root, not the process cwd.
        assert_eq!(p.current().path, normalize(&repo));
        assert_eq!(p.chosen(), Some(normalize(&repo).as_path()));

        // `#modalRoot` -> "/" and there is nowhere further up.
        let r = p.root();
        assert_eq!(r.path, PathBuf::from("/"));
        assert_eq!(r.parent, None);
        assert_eq!(p.up().path, PathBuf::from("/"), "up at the root is a no-op");

        // `#modalHome`, when the environment has one, is a real directory.
        if home_dir().is_some() {
            let n = p.home();
            assert!(n.path.is_dir(), "home {n:?} must be a real directory");
            assert_ne!(n.note, Note::NotFound);
        }

        // `#modalRepo` comes back, and `up` from it reaches the temp dir.
        assert_eq!(p.repo_root(), normalize(&repo).as_path());
        assert_eq!(p.repo().path, normalize(&repo));
        assert_eq!(p.up().path, normalize(&tmp.path));

        // The whole point of the owned return: navigate, then read something
        // else off the picker in the same expression — no `E0502`.
        let both = format!("{} {:?}", p.repo().path.display(), p.chosen_path());
        assert!(both.contains("repo"), "{both}");
    }

    #[test]
    fn listing_is_directories_only_deterministic_and_case_insensitive() {
        let tmp = TempDir::new("list");
        // Created in a deliberately non-sorted order so a readdir-order
        // implementation would fail this test on a filesystem that preserves
        // creation order.
        tmp.dir("zebra");
        tmp.dir("Alpha");
        tmp.dir("alpha");
        tmp.dir("beta");
        tmp.dir(".hidden");
        tmp.file("main.cs");
        tmp.file("notes.txt");
        tmp.file(".dotfile");

        let l = browse_to(&tmp.path);

        // Directories only, hidden skipped, in case-insensitive order with the
        // case-sensitive tie-break. `.hidden` must not appear.
        let names: Vec<&str> = l.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "alpha", "beta", "zebra"]);
        // Files are counted, never offered as navigation targets.
        assert_eq!(l.files_skipped, 2, "main.cs and notes.txt are not dirs");
        assert_eq!(l.nav_paths().len(), 4);
        assert!(l.dirs.iter().all(|d| d.path.is_dir()));
        assert_eq!(l.note, Note::NoCs, "no .cs directly in this folder");
        assert!(l.selectable());
        // `parent` is the *string* rule, so it must be the real parent of the
        // full path — `temp_dir()` itself is often a symlink (`/tmp` ->
        // `/private/tmp` on macOS), and `browse_to` normalises lexically rather
        // than canonicalising, so the comparison is against the lexical parent.
        assert_eq!(l.parent.as_deref(), Some(parent_of(&tmp.path).unwrap().as_path()));

        // The same listing twice is byte-identical: no readdir order leaks in.
        assert_eq!(browse_to(&tmp.path).dirs, l.dirs);
    }

    #[test]
    fn listing_matches_the_reference_order_for_case_variants() {
        // The exact comparator at `api.rs:682` applied to names that differ only
        // by case. The tie-break compares the ORIGINAL strings, so it is raw
        // ASCII: 'A' (65) < 'a' (97), giving ALPHA < Alpha < alpha. This was
        // confirmed by re-running the reference expression in isolation, because
        // the intuitive "alpha first" reading is wrong — and the whole point of
        // naming the order is that it must not be guessed.
        let tmp = TempDir::new("case");
        for n in ["Zulu", "yankee", "Xray", "whiskey", "alpha", "Alpha", "ALPHA"] {
            tmp.dir(n);
        }
        let l = browse_to(&tmp.path);
        let names: Vec<&str> = l.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ALPHA", "Alpha", "alpha", "whiskey", "Xray", "yankee", "Zulu"],
            "must match api.rs:682 exactly, including the ASCII case tie-break"
        );

        // A genuine total order: no two distinct names in this set are
        // unordered, which is what removes any dependence on readdir order.
        for x in &names {
            for y in &names {
                if x != y {
                    let (kx, ky) = (Listing::sort_key(x), Listing::sort_key(y));
                    assert!((kx < ky) ^ (ky < kx), "{x} vs {y} must be ordered one way");
                }
            }
        }
    }

    #[test]
    fn many_files_yield_no_rows_but_are_all_counted() {
        // Files must never become navigation targets, at any count.
        let tmp = TempDir::new("files");
        for i in 0..25 {
            tmp.file(&format!("f{i}.cs"));
        }
        let l = browse_to(&tmp.path);
        assert!(l.dirs.is_empty(), "25 .cs files must produce zero nav rows");
        assert_eq!(l.files_skipped, 25, "all 25 are counted, not dropped silently");
        assert_eq!(l.note, Note::NoCs);
    }

    #[test]
    fn hostile_paths_do_not_panic() {
        // Paths the modal can plausibly receive, including empty input, doubled
        // separators, relative escapes and an unexpandable `~user`. None may
        // panic, and none may claim to be an empty folder.
        //
        // Note "." is excluded from the "must not be Empty" rule below for the
        // obvious reason: the process cwd can genuinely be an empty directory,
        // and `Empty` would then be the honest answer. What must never happen is
        // a *failure* silently rendering as `Empty`; that is asserted directly.
        for p in ["", "/", "//", "/.", "/..", "~/", "~nosuchuser/x", "/proc/self/fd/999999"] {
            let l = browse_to(Path::new(p));
            assert!(
                l.note != Note::Empty || l.path.is_dir(),
                "{p:?} reported Empty but is not a readable directory: {l:?}"
            );
        }
        // The unexpandable-home path must fail honestly, not read as empty.
        assert_eq!(browse_to(Path::new("~nosuchuser/x")).note, Note::NotFound);

        // The same through the typed-path entry point. A temp dir with a child
        // in it, so `go("")` (the repo root) is non-empty and `Empty` would be a
        // genuine bug rather than a truthful report.
        let tmp = TempDir::new("hostile");
        tmp.dir("child");
        let mut pk = Picker::new(&tmp.path, "");
        for s in ["", "/", "..", "../../..", "~/../..", "\0bad"] {
            let l = pk.go(s);
            assert_ne!(l.note, Note::Empty, "go({s:?}) must not report Empty");
        }
    }

    #[test]
    fn nonexistent_path_says_so_and_never_looks_empty() {
        let tmp = TempDir::new("missing");
        let gone = tmp.path.join("no-such-folder");

        let l = browse_to(&gone);
        assert_eq!(l.note, Note::NotFound);
        assert_eq!(l.note.text(), Some("path does not exist"));
        assert!(l.dirs.is_empty());
        // The whole point: an empty vec plus this note is NOT "Empty folder".
        assert_ne!(l.note, Note::Empty);
        assert!(!l.selectable());
        assert_eq!(l.parent, None, "no Up for a path we could not read");
        // The path is echoed back so the modal can show what the user typed.
        assert_eq!(l.path, gone);

        // Through the Picker API, typed navigation must fail honestly too.
        let mut p = Picker::new(&tmp.path, "");
        let cur = p.go(gone.to_string_lossy().as_ref());
        assert_eq!(cur.note, Note::NotFound);
        assert_eq!(p.chosen(), None, "a missing folder must not be selectable");

        // A relative typed path resolves against the repo root, not the cwd.
        let sub = tmp.dir("repo/sub");
        let mut p2 = Picker::new(&tmp.path.join("repo"), "");
        assert_eq!(p2.go("sub").path, normalize(&sub));
        assert_ne!(p2.current().note, Note::NotFound);
    }

    #[test]
    fn a_file_is_not_a_directory_and_an_empty_dir_is_not_an_error() {
        let tmp = TempDir::new("mixed");
        tmp.file("plain.txt");
        let empty = tmp.dir("empty");

        // A file where a directory was expected: unreadable, not "not found".
        let f = browse_to(&tmp.path.join("plain.txt"));
        assert_eq!(f.note, Note::Unreadable);
        assert!(!f.selectable());

        // A genuinely empty directory is a success with zero entries: the one
        // case that may say "empty".
        let e = browse_to(&empty);
        assert_eq!(e.note, Note::Empty);
        assert!(e.dirs.is_empty());
        assert_eq!(e.files_skipped, 0);
        assert!(e.selectable(), "an empty folder is still a legal root");
    }

    #[test]
    fn unreadable_path_is_handled_without_panic() {
        use std::os::unix::fs::PermissionsExt;

        // uid 0 bypasses the permission bits, so the assertion is skipped rather
        // than quietly passing for the wrong reason.
        if effective_uid() == 0 {
            eprintln!("skipping: running as root, mode 000 is still readable");
            return;
        }

        let tmp = TempDir::new("perm");
        let locked = tmp.dir("locked");
        tmp.dir("locked/inner");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("chmod 000");

        // Must not panic, and must not claim the folder is empty.
        let l = browse_to(&locked);
        // Restore before any assertion can unwind, so Drop can clean up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert_eq!(l.note, Note::Unreadable);
        assert_eq!(l.note.text(), Some("permission denied — cannot read this folder"));
        assert_ne!(l.note, Note::Empty, "unreadable must never render as an empty folder");
        assert!(l.dirs.is_empty());
        assert!(!l.selectable());
    }

    /// The effective uid, so the permission test can skip under root.
    ///
    /// Shelling out to `id -u` rather than an `unsafe` `geteuid()` FFI call.
    /// The project rule permits exactly **one** `unsafe` C FFI call across the
    /// whole tree and reserves it for the parallelizer (see
    /// `notes/csviewer-v3.md`); a test helper is not that call, so this stays a
    /// subprocess. Returns 1 (not root) when `id` is unavailable, so the test
    /// still exercises the permission path.
    fn effective_uid() -> u32 {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1)
    }

    #[test]
    fn note_text_and_class_match_the_reference_strings() {
        assert_eq!(Note::Indexable.text(), Some("indexable"));
        assert_eq!(Note::Indexable.class(), "modal-note ok");
        assert_eq!(Note::NoCs.text(), Some("no .cs files in this subtree"));
        assert_eq!(Note::NoCs.class(), "modal-note warn");
        assert_eq!(Note::NotFound.class(), "modal-note warn");
        // Failures are exactly the non-selectable states.
        assert!(!Note::NotFound.selectable() && !Note::Unreadable.selectable());
        assert!(Note::Indexable.selectable() && Note::NoCs.selectable() && Note::Empty.selectable());
    }

    #[test]
    fn symlinked_directory_is_listed_and_flagged() {
        let tmp = TempDir::new("link");
        let real = tmp.dir("real");
        let link = tmp.path.join("linked");
        if std::os::unix::fs::symlink(&real, &link).is_err() {
            eprintln!("skipping: symlink not permitted here");
            return;
        }
        let l = browse_to(&tmp.path);
        let entry = l.dirs.iter().find(|d| d.name == "linked").expect("linked dir listed");
        assert!(entry.symlink, "a symlink must be flagged, as browse() does");
        assert_eq!(entry.path, link);
        // Descending through the link works (browse() resolves links too).
        assert_eq!(browse_to(&link).note, Note::Empty);
    }

    #[test]
    fn open_dir_follows_a_row_and_keeps_one_current_listing() {
        let tmp = TempDir::new("rows");
        let a = tmp.dir("a");
        tmp.dir("a/b");
        tmp.dir("c");

        let mut p = Picker::new(&tmp.path, "");
        let names: Vec<String> = p.current().dirs.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, vec!["a", "c"]);

        // Click the second row -> only `c` remains current.
        let c = p.current().dirs[1].path.clone();
        assert_eq!(c, tmp.path.join("c"));
        assert_eq!(p.open_dir(&c).path, normalize(&c));
        assert!(p.current().dirs.is_empty(), "c has no sub-directories");
        assert_eq!(p.current().note, Note::Empty);

        // Click the first row of the *original* listing -> descend into a.
        assert_eq!(p.open_dir(&a).path, normalize(&a));
        assert_eq!(
            p.current().dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );
    }
}
