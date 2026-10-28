//! Per-document-window engine isolation.
//!
//! The v2 reference gave every document window its own webview and therefore
//! its own JS thread and engine round-trip; a pathological document could only
//! burn its own window's CPU. The native fork previously synthesized a document
//! **synchronously on the winit main thread** using the app's single live
//! [`Index`], so opening a heavy document froze every window (and two document
//! windows shared one engine index).
//!
//! This module gives each document window:
//!
//! * its **own** `csgraph_core::index::Index` — never the app's main index and
//!   never another window's, so two document windows cannot disturb each
//!   other's engine state; and
//! * a **dedicated worker thread** that builds that index and synthesizes the
//!   document, so no document-window engine work ever runs on the main thread.
//!
//! Honest limitation: one process still has one winit event loop and one wgpu
//! device. That loop is the architecture boundary and remains shared — but it
//! only *renders*; the CPU-heavy engine work is what is isolated here.
//!
//! **Why a process-wide build lock:** `Index::build` writes a shared on-disk
//! cache whose temporary file has a fixed name, so two concurrent builds could
//! corrupt each other. [`BUILD_LOCK`] serialises only engine *construction*;
//! document synthesis (`Index::doc_source`) runs under the lock's absence and
//! stays concurrent per window.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use csgraph_core::index::Index;

use crate::app::source_pane::{document_for, Document};
use crate::windows::DocTarget;

/// Serialises engine **construction** across document windows.
///
/// See the module doc: `Index::build` writes a shared cache with a fixed
/// temporary-file name, so concurrent builds are not safe. Document synthesis
/// is *not* under this lock.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

/// The outcome of a document window's engine work.
pub enum DocLoad {
    /// The window's own engine synthesized this document.
    Ready(Document),
    /// The engine (or the target) refused; the text is the honest reason.
    Failed(String),
}

/// One document window's private engine worker.
///
/// Created by [`DocEngine::spawn`], drained by the main thread with
/// [`DocEngine::try_take`] — never blocked on. Dropping the engine (window
/// closed) only flips the cancel flag; the main thread must never wait for a
/// heavy index build to finish.
pub struct DocEngine {
    target: DocTarget,
    rx: Receiver<DocLoad>,
    cancel: Arc<AtomicBool>,
    /// The worker handle. Kept so the thread is detached deliberately, never
    /// joined from the main thread.
    worker: Option<JoinHandle<()>>,
    started: Instant,
    /// Set once a `DocLoad` has been handed out, so a second `try_take` cannot
    /// see a stale/disconnected channel as a fresh result.
    taken: bool,
}

impl DocEngine {
    /// Start loading `target` for a document window.
    ///
    /// A node target spawns a named worker thread that builds its **own**
    /// [`Index`] (under [`BUILD_LOCK`]) and synthesizes the document there. A
    /// file or empty target spawns nothing: the honest failure is pre-resolved
    /// into the channel, because there is nothing the engine could build.
    pub fn spawn(repo: &Path, roots: &[String], target: DocTarget) -> Self {
        let (tx, rx) = channel();
        let started = Instant::now();
        let cancel = Arc::new(AtomicBool::new(false));

        if target.node.is_none() {
            let msg = match target.file.as_deref() {
                Some(file) => format!(
                    "file document windows are not available in the native viewer: {file}"
                ),
                None => "document window has no node or file target".to_string(),
            };
            let _ = tx.send(DocLoad::Failed(msg));
            return Self {
                target,
                rx,
                cancel,
                worker: None,
                started,
                taken: false,
            };
        }

        let repo = repo.to_path_buf();
        let roots = roots.to_vec();
        let worker_target = target.clone();
        let worker_cancel = Arc::clone(&cancel);
        // Fallback sender: if the OS refuses a thread we must still answer the
        // window honestly rather than leave it loading forever.
        let fallback_tx = tx.clone();
        let worker = std::thread::Builder::new()
            .name(format!("doc-engine {}", target.key()))
            .spawn(move || {
                // Engine construction: exclusive against other windows' builds.
                let mut index = {
                    let _guard = BUILD_LOCK.lock().unwrap_or_else(|p| p.into_inner());
                    let cfg = crate::model::load_config(&repo);
                    let mut index = Index::new(repo.clone());
                    index.build(&roots, &cfg.excludes, &cfg);
                    index
                };
                // The window may have closed while the index was building; the
                // result is unwanted then, and dropping it is the cancellation.
                if worker_cancel.load(Ordering::SeqCst) {
                    return;
                }
                let load = match worker_target.node.as_deref() {
                    Some(node) => match document_for(&mut index, node) {
                        Ok(doc) => DocLoad::Ready(doc),
                        Err(e) => DocLoad::Failed(e),
                    },
                    // Unreachable: `spawn` returns before this for non-node
                    // targets. Kept honest rather than inventing a document.
                    None => DocLoad::Failed(
                        "document window has no node or file target".to_string(),
                    ),
                };
                // The receiver is gone if the window closed after the cancel
                // check; a send error is the normal "nobody wants it" case.
                let _ = tx.send(load);
            });
        let worker = match worker {
            Ok(h) => Some(h),
            Err(e) => {
                let _ = fallback_tx.send(DocLoad::Failed(format!(
                    "could not start a document-engine worker thread: {e}"
                )));
                None
            }
        };
        Self {
            target,
            rx,
            cancel,
            worker,
            started,
            taken: false,
        }
    }

    /// The target this engine was spawned for.
    pub fn target(&self) -> &DocTarget {
        &self.target
    }

    /// How long the window has been loading (for the pending paint).
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Non-blocking drain: `Some` at most once.
    ///
    /// `None` means "still loading" (or already taken) — the main thread never
    /// blocks on the worker. A worker that died without sending is reported as
    /// an honest failure rather than an endless load.
    pub fn try_take(&mut self) -> Option<DocLoad> {
        if self.taken {
            return None;
        }
        match self.rx.try_recv() {
            Ok(load) => {
                self.taken = true;
                Some(load)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.taken = true;
                Some(DocLoad::Failed(
                    "document-engine worker ended without producing a document".to_string(),
                ))
            }
        }
    }

    /// Ask the worker to abandon its result. The window still owns the engine;
    /// this is the cancel-on-close signal, not a join.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Test seam: the same plumbing with a deterministic loader instead of a
    /// real engine build, so the async behaviour is provable without a corpus.
    #[cfg(test)]
    pub(crate) fn spawn_with_loader<F: FnOnce() -> DocLoad + Send + 'static>(
        target: DocTarget,
        loader: F,
    ) -> Self {
        let (tx, rx) = channel();
        let started = Instant::now();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = std::thread::Builder::new()
            .name(format!("doc-engine {} (test)", target.key()))
            .spawn(move || {
                let load = loader();
                let _ = tx.send(load);
            })
            .ok();
        Self {
            target,
            rx,
            cancel,
            worker,
            started,
            taken: false,
        }
    }
}

impl Drop for DocEngine {
    /// Window closed: signal the worker (so it skips synthesis) and let the
    /// handle detach. Deliberately **no join** — a dropped window must never
    /// block the winit main thread on an index build.
    fn drop(&mut self) {
        self.cancel();
        let _ = self.worker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real synthesized-document payload, same shape the app's document-window
    /// test uses. `node_ref` is both the payload's node and the identity.
    fn fixture_document(node_ref: &str) -> Document {
        let payload = json!({
            "ok": true, "total": 6, "start": 1, "end": 6, "sections": 0,
            "definesKnown": false, "parts": [{"file": "a.cs", "line": 1}],
            "node": {"id": node_ref, "full": node_ref, "label": node_ref,
                     "kind": "method", "file": "a.cs", "line": 1, "endLine": 4},
            "lines": [
                {"n": 0, "ctx": "part", "text": "// part", "file": "a.cs"},
                {"n": 1, "ctx": "head", "text": "namespace N{", "file": "a.cs", "orig": 1},
                {"n": 2, "ctx": "body", "text": "  void M(){", "file": "a.cs", "orig": 2},
                {"n": 3, "ctx": "body", "text": "  }", "file": "a.cs", "orig": 3},
                {"n": 4, "ctx": "tail", "text": "}", "file": "a.cs", "orig": 4}
            ]
        });
        Document::from_value(node_ref, &payload)
            .expect("the fixture is a real synthesized-document payload")
    }

    fn wait_for(engine: &mut DocEngine, limit: Duration) -> Option<DocLoad> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(load) = engine.try_take() {
                return Some(load);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.try_take()
    }

    /// Unique throwaway corpus dir; the process id plus an atomic counter keeps
    /// parallel tests from sharing (and deleting) one another's root.
    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ncv-doc-engine-{}-{tag}-{n}",
            std::process::id()
        ))
    }

    /// A real per-window `Index` builds on the worker thread and produces the
    /// document — the engine work is genuinely off the main thread and uses its
    /// own index, not the app's.
    #[test]
    fn real_engine_loads_a_document_on_a_worker_thread() {
        let dir = unique_dir("real");
        std::fs::create_dir_all(&dir).expect("temp corpus dir");
        std::fs::write(
            dir.join("A.cs"),
            "namespace N{ class C{ void M(){} } }\n",
        )
        .expect("write corpus file");

        // Discover the engine's real node id for `M` with a throwaway index,
        // exactly how the app's index sees the corpus.
        let mut probe = Index::new(dir.clone());
        probe.build(
            &[".".to_string()],
            &[],
            &csgraph_core::config::Config::default(),
        );
        let node_id = probe
            .nodes()
            .iter()
            .find_map(|n| {
                let full = n.get("full").and_then(|v| v.as_str()).unwrap_or("");
                if full.ends_with('M') || full.ends_with(".M") {
                    n.get("id").and_then(|v| v.as_str()).map(str::to_string)
                } else {
                    None
                }
            })
            .expect("the engine indexed N.C.M");
        drop(probe);

        let target = DocTarget::for_node(node_id, "doc");
        let mut engine = DocEngine::spawn(&dir, &[".".to_string()], target);
        match wait_for(&mut engine, Duration::from_secs(10)) {
            Some(DocLoad::Ready(doc)) => {
                assert!(!doc.lines.is_empty(), "the worker returned a real document");
                let text = doc
                    .lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("M("), "document text: {text}");
            }
            Some(DocLoad::Failed(e)) => panic!("engine load failed: {e}"),
            None => panic!("document engine never produced a result"),
        }
        drop(engine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `spawn` returns immediately; the loader runs on another thread; the
    /// result arrives asynchronously (the pending paint really is pending).
    #[test]
    fn spawn_returns_immediately_and_loads_off_the_main_thread() {
        let (tx, rx) = channel();
        let main_thread = std::thread::current().id();
        let target = DocTarget::for_node("N.C.M", "doc");
        let spawned = Instant::now();
        let mut engine = DocEngine::spawn_with_loader(target, move || {
            let _ = tx.send(std::thread::current().id());
            std::thread::sleep(Duration::from_millis(150));
            DocLoad::Ready(fixture_document("N.C.M"))
        });
        let first = engine.try_take();
        let spawn_elapsed = spawned.elapsed();
        assert!(first.is_none(), "the load must not be ready immediately");
        assert!(
            spawn_elapsed < Duration::from_millis(50),
            "spawn + first poll must not block: {spawn_elapsed:?}"
        );
        let loader_thread = rx.recv_timeout(Duration::from_secs(5)).expect("loader ran");
        assert_ne!(loader_thread, main_thread, "load ran on the main thread");
        match wait_for(&mut engine, Duration::from_secs(5)) {
            Some(DocLoad::Ready(doc)) => assert_eq!(doc.node_ref, "N.C.M"),
            Some(DocLoad::Failed(e)) => panic!("loader failed: {e}"),
            None => panic!("loader result never arrived"),
        }
        assert!(
            engine.elapsed() >= Duration::from_millis(100),
            "elapsed tracks the real wait: {:?}",
            engine.elapsed()
        );
    }

    /// Two document windows have independent engines and own distinct document
    /// state — one window's result cannot be the other's.
    #[test]
    fn two_document_windows_load_independently() {
        let mut a = DocEngine::spawn_with_loader(
            DocTarget::for_node("N.C.M", "doc"),
            || DocLoad::Ready(fixture_document("N.C.M")),
        );
        let mut b = DocEngine::spawn_with_loader(
            DocTarget::for_node("N.D.N", "raw"),
            || DocLoad::Ready(fixture_document("N.D.N")),
        );
        let da = match wait_for(&mut a, Duration::from_secs(5)) {
            Some(DocLoad::Ready(d)) => d,
            _ => panic!("window A did not load"),
        };
        let db = match wait_for(&mut b, Duration::from_secs(5)) {
            Some(DocLoad::Ready(d)) => d,
            _ => panic!("window B did not load"),
        };
        assert_ne!(da.node_ref, db.node_ref, "windows must not share a document");
        assert_eq!(da.node_ref, "N.C.M");
        assert_eq!(db.node_ref, "N.D.N");
    }

    /// A target with no node cannot be built, so it fails honestly and fast —
    /// proving `spawn` did not try to index the (nonexistent) repo.
    #[test]
    fn file_target_and_empty_target_fail_honestly_without_building_an_index() {
        let missing = std::env::temp_dir().join("ncv-doc-engine-does-not-exist");
        let file_target = DocTarget {
            node: None,
            file: Some("src/a.cs".to_string()),
            line: 3,
            mode: "raw".to_string(),
        };
        let started = Instant::now();
        let mut engine = DocEngine::spawn(&missing, &[".".to_string()], file_target);
        match engine.try_take() {
            Some(DocLoad::Failed(e)) => {
                assert!(e.contains("file document"), "honest file reason: {e}")
            }
            _ => panic!("a file target must fail without building an index"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));

        let empty = DocTarget {
            node: None,
            file: None,
            line: 0,
            mode: "doc".to_string(),
        };
        let mut engine = DocEngine::spawn(&missing, &[], empty);
        match engine.try_take() {
            Some(DocLoad::Failed(e)) => assert!(e.contains("no node or file target"), "{e}"),
            _ => panic!("an empty target must fail without building an index"),
        }
    }

    /// Closing a window whose load is still pending never blocks the caller:
    /// `Drop` only cancels, it does not join the worker.
    #[test]
    fn dropping_a_pending_load_does_not_block() {
        let engine = DocEngine::spawn_with_loader(DocTarget::for_node("N.C.M", "doc"), || {
            std::thread::sleep(Duration::from_millis(300));
            DocLoad::Ready(fixture_document("N.C.M"))
        });
        let dropped = Instant::now();
        drop(engine);
        assert!(
            dropped.elapsed() < Duration::from_millis(50),
            "dropping a pending load blocked: {:?}",
            dropped.elapsed()
        );
    }
}
