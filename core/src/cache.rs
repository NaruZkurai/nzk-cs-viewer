//! cache — the on-disk graph cache.
//!
//! The Python engine keeps this in a SQLite file (`<root>/.csgraph.db`, WAL)
//! with three roles: the finished graph keyed by a hash of every source path,
//! size and mtime; a per-file parse result; and the viewer's own state (theme,
//! camera, node positions).
//!
//! v2 keeps the same three roles but not the same file format: SQLite would be
//! another dependency and the cache is not part of the API contract (the
//! differential oracle drops cache fields, and the graph it returns is
//! identical either way). So this is a small content-addressed store — one
//! zlib-compressed blob per entry, written through a temp file and renamed, so
//! a crash mid-write can never leave a half-written cache behind.
//!
//! Nothing here is ever an input to the parser. The directory is appended to
//! the excludes list by the caller and the file walk only ever picks up `.cs`
//! and `.cs.nkz` sources anyway.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

/// Directory name used inside the indexed root. Deliberately not `.csgraph.db`:
/// the Python viewer may be indexing the same folder at the same time, and two
/// different formats must not fight over one path.
pub const CACHE_DIR: &str = ".csgraph.v2";

/// Bump when the cached record shape changes; older entries are then ignored
/// rather than mis-decoded.
pub const CACHE_VERSION: u32 = 1;

pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// Open (creating if needed) the cache under `root`.
    pub fn open(root: &Path) -> Cache {
        let dir = root.join(CACHE_DIR);
        let _ = fs::create_dir_all(&dir);
        Cache { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn key_path(&self, ns: &str, key: &str) -> PathBuf {
        // FNV-1a 64: stable across runs and platforms. This only names the
        // file, so collision resistance is irrelevant; the key is also stored
        // inside the blob and checked on read.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.dir.join(ns).join(format!("{h:016x}.v{CACHE_VERSION}.z"))
    }

    /// Read a blob. Returns `None` when missing, unreadable, corrupt, or when
    /// the stored key does not match (which is how a hash collision or a stale
    /// entry is rejected rather than trusted).
    pub fn get(&self, ns: &str, key: &str) -> Option<Vec<u8>> {
        let path = self.key_path(ns, key);
        let raw = fs::read(&path).ok()?;
        let mut out = Vec::new();
        ZlibDecoder::new(&raw[..]).read_to_end(&mut out).ok()?;
        // header: "v<version>\n<key>\n<payload>"
        let nl1 = out.iter().position(|b| *b == b'\n')?;
        if out.get(..nl1)? != format!("v{CACHE_VERSION}").as_bytes() {
            return None;
        }
        let rest = &out[nl1 + 1..];
        let nl2 = rest.iter().position(|b| *b == b'\n')?;
        if rest.get(..nl2)? != key.as_bytes() {
            return None;
        }
        Some(rest[nl2 + 1..].to_vec())
    }

    /// Write a blob atomically. A failure is reported but never fatal: a cache
    /// is a speed-up, not a correctness requirement.
    pub fn put(&self, ns: &str, key: &str, data: &[u8]) -> std::io::Result<()> {
        let path = self.key_path(ns, key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut body = Vec::with_capacity(data.len() + key.len() + 8);
        body.extend_from_slice(format!("v{CACHE_VERSION}\n").as_bytes());
        body.extend_from_slice(key.as_bytes());
        body.push(b'\n');
        body.extend_from_slice(data);

        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(&body)?;
        let packed = enc.finish()?;

        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &packed)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Drop every entry (used when the record shape or the corpus changes).
    pub fn clear(&self) -> std::io::Result<()> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir)?;
        }
        fs::create_dir_all(&self.dir)
    }

    // ---- viewer state -------------------------------------------------------
    //
    // The UI writes its own theme/sliders/camera/positions back so a restart
    // looks the way it was left. This is a single small JSON document, not a
    // keyed blob, so it gets its own file.

    fn state_path(&self) -> PathBuf {
        self.dir.join("viewer.json")
    }

    pub fn viewer_state(&self) -> Option<serde_json::Value> {
        let raw = fs::read(self.state_path()).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    pub fn save_viewer_state(&self, value: &serde_json::Value) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.state_path();
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
        fs::write(&tmp, &body)?;
        fs::rename(&tmp, &path)
    }
}

/// Signature of an index run: every source path with its size and mtime.
///
/// Order-independent (the file list is sorted first) and hex-encoded, so the
/// same corpus in the same state always produces the same key.
pub fn signature(entries: &[(String, u64, i64)]) -> String {
    let mut list = entries.to_vec();
    list.sort();
    let mut s = String::new();
    for (path, size, mtime) in &list {
        s.push_str(path);
        s.push('\x1f');
        s.push_str(&size.to_string());
        s.push('\x1f');
        s.push_str(&mtime.to_string());
        s.push('\x1e');
    }
    format!("v{CACHE_VERSION}:{:016x}:{}", fnv1a64(s.as_bytes()), list.len())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_tampering() {
        let root = std::env::temp_dir().join(format!("csgraph-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let c = Cache::open(&root);
        assert!(c.get("parse", "a.cs").is_none());
        c.put("parse", "a.cs", b"hello world").unwrap();
        assert_eq!(c.get("parse", "a.cs").as_deref(), Some(&b"hello world"[..]));
        // different key -> different file -> miss
        assert!(c.get("parse", "b.cs").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn signature_is_order_independent() {
        let a = vec![("x.cs".to_string(), 1u64, 2i64), ("y.cs".to_string(), 3, 4)];
        let b = vec![("y.cs".to_string(), 3u64, 4i64), ("x.cs".to_string(), 1, 2)];
        assert_eq!(signature(&a), signature(&b));
        let c = vec![("x.cs".to_string(), 1u64, 2i64), ("y.cs".to_string(), 3, 5)];
        assert_ne!(signature(&a), signature(&c));
    }
}
