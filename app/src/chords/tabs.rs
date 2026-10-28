//! The document-tab model behind `window.CsTabs` (`shell.js:910-1871`).
//!
//! This is the *model* half of the reference's tab layer: identity, parameter
//! normalisation, the tab stack, the split-pane flag, and the persisted
//! session. Everything the reference does to the DOM — rendering the tab bar,
//! creating/destroying the Monaco split editor, fetching `/api/source`,
//! scheduling the viewer save — is deliberately **not** here; [`crate::app`]
//! and the renderer own that.
//!
//! ## What is ported literally
//!
//! * `keyFor` (`shell.js:964-968`) — the document identity is the node id or
//!   the file path, *independent of mode/line*, so re-opening the same document
//!   at another line activates the existing tab.
//! * `normalize` (`shell.js:978-995`) — only the two shapes `openSource()`
//!   understands become a tab; anything else is not one. `node` keeps
//!   `mode`/`match`; `file` keeps `line`/`mode`/`context`/`to`/`match`, with
//!   `line` defaulting to 1.
//! * `defaultLabel` / `shortNode` / `basename` (`shell.js:948-1006`).
//! * `MAX_TABS = 24` and `trimTabs` (`shell.js:911`, `:1301-1312`): the oldest
//!   tab that is neither active nor the split document is evicted first, and the
//!   active plus split pair are never evicted.
//! * `cycle` (`shell.js:1293-1299`): fewer than two tabs is a no-op, and the
//!   step wraps in both directions. `delta == 0` counts as **forward**, matching
//!   the reference's `dir < 0 ? -1 : 1`.
//! * `closeTab`/`closeActive`/`closeOthers` (`shell.js:1316-1355`) and
//!   `toggleSplit` (`shell.js:1475-1490`).
//! * `sanParams`/`session`/`restoreSession` (`shell.js:1653-1726`).
//!
//! ## Intentional divergences (DOM-free model)
//!
//! * A tab has no `snap`, so `session()`'s `mode` is `params.mode` (the
//!   reference prefers the last rendered `state.source.srcMode`). The field is
//!   still emitted, with the same `""` fallback.
//! * `restoreSession` is applied directly; [`Tabs::try_restore_session`] is the
//!   additive boolean form that the reference's public wrapper returns.
//! * Closing the last tab also clears the split flag, mirroring `closeAll`
//!   (`shell.js:1357-1370`); the reference leaves the CSS class to the DOM path.
//! * JS slices strings in UTF-16 units; the port truncates by `char`, which can
//!   differ only for astral-plane text in an over-long persisted row.

use serde_json::{json, Map, Value};

/// `shell.js:911` — the hard cap on simultaneously open documents.
pub const MAX_TABS: usize = 24;

/// `shell.js:966-967` — a row's path/node is truncated before it is persisted.
const MAX_NODE_CHARS: usize = 400;
const MAX_FILE_CHARS: usize = 1024;
const MAX_LABEL_CHARS: usize = 120;

/// One open document. `key` is the identity from [`Tabs::key_for`].
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    /// `node:<id>` / `file:<path>` / `doc:unknown` (`shell.js:964-968`).
    pub key: String,
    /// The normalised `openSource()` parameters for this tab.
    pub params: Map<String, Value>,
    /// The tab-bar caption (`shell.js:1002-1006`, `:1597-1607`).
    pub label: String,
}

/// The tab stack: identity, order, the active document and the split pane.
///
/// The active tab is tracked by key rather than index so eviction and closing
/// cannot leave it dangling.
#[derive(Clone, Debug)]
pub struct Tabs {
    tabs: Vec<Tab>,
    active: Option<String>,
    split_key: Option<String>,
    split_on: bool,
    split_frac: f64,
}

impl Default for Tabs {
    fn default() -> Self {
        Self::new()
    }
}

impl Tabs {
    /// An empty stack, split off, divider centred (`shell.js:915-918`).
    pub fn new() -> Self {
        Tabs {
            tabs: Vec::new(),
            active: None,
            split_key: None,
            split_on: false,
            split_frac: 0.5,
        }
    }

    // ------------------------------------------------------------- identity

    /// `shell.js:964-968` — the document identity of an `openSource()` call,
    /// independent of mode/line. Empty strings are not identities (JS truthiness).
    pub fn key_for(params: &Value) -> String {
        let obj = params.as_object();
        if let Some(node) = obj.and_then(|p| p.get("node")).and_then(Value::as_str) {
            if !node.is_empty() {
                return format!("node:{node}");
            }
        }
        if let Some(file) = obj.and_then(|p| p.get("file")).and_then(Value::as_str) {
            if !file.is_empty() {
                return format!("file:{file}");
            }
        }
        "doc:unknown".to_string()
    }

    /// `shell.js:978-995` — accept only the shapes `openSource()` understands;
    /// anything else is not a tab. Returns the normalised parameter object.
    pub fn normalize(params: &Value) -> Option<Map<String, Value>> {
        let p = params.as_object()?;
        if let Some(node) = p.get("node").and_then(Value::as_str) {
            if !node.is_empty() {
                let mut out = Map::new();
                out.insert("node".to_string(), Value::String(node.to_string()));
                if let Some(mode) = p.get("mode").and_then(Value::as_str) {
                    out.insert("mode".to_string(), Value::String(mode.to_string()));
                }
                if let Some(m) = p.get("match").and_then(Value::as_str) {
                    out.insert("match".to_string(), Value::String(m.to_string()));
                }
                return Some(out);
            }
        }
        if let Some(file) = p.get("file").and_then(Value::as_str) {
            if !file.is_empty() {
                let mut out = Map::new();
                out.insert("file".to_string(), Value::String(file.to_string()));
                out.insert(
                    "line".to_string(),
                    Value::from(num_or(p.get("line"), 1)),
                );
                if let Some(mode) = p.get("mode").and_then(Value::as_str) {
                    out.insert("mode".to_string(), Value::String(mode.to_string()));
                }
                if p.get("context").is_some_and(Value::is_number) {
                    out.insert("context".to_string(), p["context"].clone());
                }
                if p.get("to").is_some_and(Value::is_number) {
                    out.insert("to".to_string(), p["to"].clone());
                }
                if let Some(m) = p.get("match").and_then(Value::as_str) {
                    out.insert("match".to_string(), Value::String(m.to_string()));
                }
                return Some(out);
            }
        }
        None
    }

    /// `shell.js:1535-1568` — normalise, derive the key, and open/activate it.
    /// A `None` result means "not a document" and the caller should fall through
    /// to the untabbed path, exactly like the reference's wrapper.
    pub fn open_params(&mut self, params: &Value) -> Option<String> {
        let normalised = Self::normalize(params)?;
        let key = Self::key_for(&Value::Object(normalised.clone()));
        Some(self.open(&key, Value::Object(normalised)))
    }

    // --------------------------------------------------------------- state

    /// The number of open tabs (`shell.js:1823`).
    pub fn count(&self) -> usize {
        self.tabs.len()
    }

    /// Whether any document is open.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// The active document's key, if any (`shell.js:1824`).
    pub fn active_key(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Read-only view of the stack, in tab-bar order.
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// The normalised parameters stored for `key`.
    pub fn params(&self, key: &str) -> Option<&Map<String, Value>> {
        self.by_key(key).map(|t| &t.params)
    }

    /// The caption a tab bar would paint for `key`.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.by_key(key).map(|t| t.label.as_str())
    }

    /// The key the split pane is showing (`shell.js:1840`).
    pub fn split_key(&self) -> Option<&str> {
        self.split_key.as_deref()
    }

    /// The divider position, clamped to `0.15..=0.85` (`shell.js:1518`).
    pub fn split_frac(&self) -> f64 {
        self.split_frac
    }

    /// Write the divider position from a drag (`shell.js:1516-1521`):
    /// `splitFrac = clamp((ev.clientX - r.left) / r.width, 0.15, 0.85)`.
    ///
    /// Clamped again here, at the only writer the pointer path reaches, so a
    /// drag can never move the divider outside the reference band even if a
    /// caller computes the fraction itself. A non-finite input is ignored (the
    /// position is unchanged) rather than written as `NaN`.
    pub fn set_split_frac(&mut self, frac: f64) {
        if frac.is_finite() {
            self.split_frac = frac.clamp(0.15, 0.85);
        }
    }

    /// `shell.js:1535-1568` — open or re-activate a document.
    ///
    /// A tab with the same `key` is re-activated and its parameters are merged
    /// (later keys win, so a re-open at another line updates what is displayed).
    /// A new tab is pushed, becomes active, and then the stack is trimmed to
    /// [`MAX_TABS`]. Returns the key (the reference returns nothing).
    pub fn open(&mut self, key: &str, params: Value) -> String {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.key == key) {
            if let Some(p) = params.as_object() {
                for (k, v) in p {
                    tab.params.insert(k.clone(), v.clone());
                }
            }
            self.active = Some(tab.key.clone());
            return tab.key.clone();
        }
        let obj = params.as_object().cloned().unwrap_or_default();
        let label = default_label(&obj);
        self.tabs.push(Tab {
            key: key.to_string(),
            params: obj,
            label,
        });
        self.active = Some(key.to_string());
        self.trim_tabs();
        key.to_string()
    }

    /// `shell.js:1301-1312` — evict the oldest tab that is neither the active
    /// document nor the split document until the cap holds.
    fn trim_tabs(&mut self) {
        let mut guard = self.tabs.len() * 2 + 4;
        while self.tabs.len() > MAX_TABS && guard > 0 {
            guard -= 1;
            let active = self.active.as_deref();
            let split = self.split_key.as_deref();
            let victim = self
                .tabs
                .iter()
                .position(|t| Some(t.key.as_str()) != active && Some(t.key.as_str()) != split);
            match victim {
                Some(i) => {
                    self.tabs.remove(i);
                }
                None => break,
            }
        }
    }

    // ------------------------------------------------------------- commands

    /// `shell.js:1293-1299` — step the tab stack. Fewer than two tabs does
    /// nothing; the step wraps, and `delta == 0` steps **forward**.
    /// Returns the newly active key.
    pub fn cycle(&mut self, delta: isize) -> Option<String> {
        if self.tabs.len() < 2 {
            return None;
        }
        let len = self.tabs.len() as isize;
        let current = self
            .active
            .as_deref()
            .and_then(|k| self.tabs.iter().position(|t| t.key == k))
            .map(|i| i as isize)
            .unwrap_or(0);
        let step = if delta < 0 { -1 } else { 1 };
        let next = (current + step + len) % len;
        let key = self.tabs[next as usize].key.clone();
        self.active = Some(key.clone());
        Some(key)
    }

    /// `shell.js:1340-1343` + `:1316-1338` — close the active document.
    ///
    /// The focus moves to the next tab at the closed index (falling back to the
    /// new last one), the split document is re-pointed at another tab when it
    /// was the one closed, and closing the last tab switches the split off.
    /// Returns the closed key.
    pub fn close_active(&mut self) -> Option<String> {
        let key = self.active.clone()?;
        let idx = self.tabs.iter().position(|t| t.key == key)?;
        // `wasActive` is true by construction: this only closes the active tab.
        self.tabs.remove(idx);
        if self.split_key.as_deref() == Some(key.as_str()) {
            self.split_key = self.first_other_key(None);
        }
        if self.tabs.is_empty() {
            self.active = None;
            self.split_key = None;
            self.split_on = false;
        } else {
            let next = idx.min(self.tabs.len() - 1);
            self.active = Some(self.tabs[next].key.clone());
        }
        Some(key)
    }

    /// `shell.js:1345-1355` — close every document except the active one.
    /// Returns the closed keys, oldest first. With no active tab this is a
    /// no-op, matching the reference's `false`.
    pub fn close_others(&mut self) -> Vec<String> {
        let Some(active) = self.active.clone() else {
            return Vec::new();
        };
        let mut closed = Vec::new();
        self.tabs.retain(|t| {
            if t.key == active {
                true
            } else {
                closed.push(t.key.clone());
                false
            }
        });
        if self.split_key.as_deref() != Some(active.as_str()) {
            self.split_key = None;
        }
        if self.split_on && self.split_key.is_none() {
            self.split_on = false;
        }
        closed
    }

    /// `shell.js:1475-1490` — toggle the split pane. It refuses while fewer
    /// than two documents are open (the reference also toasts) and returns the
    /// resulting split state.
    pub fn toggle_split(&mut self) -> bool {
        if self.split_on {
            self.split_on = false;
            return false;
        }
        if self.tabs.len() < 2 {
            return false;
        }
        let target = self
            .split_key
            .clone()
            .filter(|k| self.tabs.iter().any(|t| &t.key == k))
            .or_else(|| {
                self.active
                    .as_deref()
                    .and_then(|a| self.tabs.iter().find(|t| t.key != a))
                    .map(|t| t.key.clone())
            })
            .or_else(|| self.tabs.first().map(|t| t.key.clone()));
        self.split_key = target;
        self.split_on = self.split_key.is_some();
        self.split_on
    }

    /// `shell.js:1374-1376`,`:1839` — whether the split pane is showing.
    pub fn split(&self) -> bool {
        self.split_on
    }

    // ------------------------------------------------------------- session

    /// `shell.js:1667-1685` — the persisted shape: `{v, tabs, active, split}`.
    pub fn session(&self) -> Value {
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .take(MAX_TABS)
            .map(|t| {
                json!({
                    "key": t.key,
                    "params": Value::Object(t.params.clone()),
                    // No rendered snapshot in the DOM-free model: fall back to
                    // `params.mode` exactly as the reference does for a tab that
                    // has never rendered.
                    "mode": t.params.get("mode").and_then(Value::as_str).unwrap_or(""),
                    "label": t.label,
                })
            })
            .collect();
        json!({
            "v": 1,
            "tabs": tabs,
            "active": self.active,
            "split": {
                "on": self.split_on,
                "key": self.split_key,
                "w": (self.split_frac * 1000.0).round() / 1000.0,
            },
        })
    }

    /// `shell.js:1688-1726` — rebuild the stack from a persisted session.
    /// Every malformed row is skipped; an empty or unusable session is ignored
    /// (the reference returns `false` there).
    pub fn restore_session(&mut self, session: Value) {
        let _ = self.try_restore_session(&session);
    }

    /// The boolean form of [`Tabs::restore_session`] (`shell.js:1851-1853`).
    pub fn try_restore_session(&mut self, session: &Value) -> bool {
        let Some(sess) = session.as_object() else {
            return false;
        };
        let Some(list) = sess.get("tabs").and_then(Value::as_array) else {
            return false;
        };
        if list.is_empty() {
            return false;
        }
        let mut restored: Vec<Tab> = Vec::new();
        for row in list {
            if restored.len() >= MAX_TABS {
                break;
            }
            let Some(row) = row.as_object() else {
                continue;
            };
            // `sanParams(row.params || row)` — a null/absent `params` falls back
            // to the row itself; a present non-object is rejected by sanParams.
            let raw: Value = match row.get("params") {
                None | Some(Value::Null) => Value::Object(row.clone()),
                Some(p) => p.clone(),
            };
            let Some(params) = san_params(&raw) else {
                continue;
            };
            let key = Self::key_for(&Value::Object(params.clone()));
            if restored.iter().any(|t| t.key == key) {
                continue;
            }
            let mut tab = Tab {
                label: default_label(&params),
                key,
                params,
            };
            if let Some(mode) = row.get("mode").and_then(Value::as_str) {
                if matches!(mode, "raw" | "doc" | "effective") {
                    tab.params
                        .insert("mode".to_string(), Value::String(mode.to_string()));
                }
            }
            if let Some(label) = row.get("label").and_then(Value::as_str) {
                tab.label = label.chars().take(MAX_LABEL_CHARS).collect();
            }
            restored.push(tab);
        }
        if restored.is_empty() {
            return false;
        }

        self.tabs = restored;
        self.active = sess
            .get("active")
            .and_then(Value::as_str)
            .and_then(|want| self.by_key(want).map(|t| t.key.clone()))
            .or_else(|| self.tabs.first().map(|t| t.key.clone()));

        let sp = sess.get("split").and_then(Value::as_object);
        // `wantSplit` falls back to "any other tab" even when the session says
        // the split was off, so a multi-tab restore shows the split again.
        let want_split = sp
            .filter(|s| s.get("on").and_then(Value::as_bool).unwrap_or(false))
            .and_then(|s| s.get("key"))
            .and_then(Value::as_str)
            .and_then(|k| self.tabs.iter().any(|t| t.key == k).then(|| k.to_string()))
            .or_else(|| {
                self.active
                    .as_deref()
                    .and_then(|a| self.tabs.iter().find(|t| t.key != a))
                    .map(|t| t.key.clone())
            });
        if let Some(w) = sp.and_then(|s| s.get("w")).and_then(Value::as_f64) {
            if w.is_finite() {
                self.split_frac = w.clamp(0.15, 0.85);
            }
        }
        self.split_key = want_split;
        self.split_on = self.split_key.is_some();
        true
    }

    // ------------------------------------------------------------- helpers

    fn by_key(&self, key: &str) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.key == key)
    }

    /// `shell.js:1025-1028` — the first tab that is not `except`.
    fn first_other_key(&self, except: Option<&str>) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| Some(t.key.as_str()) != except)
            .map(|t| t.key.clone())
    }
}

/// `shell.js:959-961` — a positive finite number, floored, or a default.
fn num_or(v: Option<&Value>, dflt: i64) -> i64 {
    match v.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n > 0.0 => n.floor() as i64,
        _ => dflt,
    }
}

/// `shell.js:948-952` — the component after the last `/`.
fn basename(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// `shell.js:954-957` — the last two dotted segments.
fn short_node(id: &str) -> String {
    let bits: Vec<&str> = id.split('.').collect();
    if bits.len() > 2 {
        bits[bits.len() - 2..].join(".")
    } else {
        bits.last().copied().unwrap_or(id).to_string()
    }
}

/// `shell.js:1002-1006` — the caption before a document has rendered.
fn default_label(params: &Map<String, Value>) -> String {
    if let Some(node) = params.get("node").and_then(Value::as_str) {
        return short_node(node);
    }
    if let Some(file) = params.get("file").and_then(Value::as_str) {
        let line = num_or(params.get("line"), 0);
        let suffix = if line > 0 { format!(":{line}") } else { String::new() };
        return format!("{}{suffix}", basename(file));
    }
    "document".to_string()
}

/// `shell.js:1653-1665` — the persistence sanitiser. Only a non-empty `node`
/// or `file` qualifies; `mode` is restricted to the three understood values.
fn san_params(v: &Value) -> Option<Map<String, Value>> {
    let p = v.as_object()?;
    let valid_mode = |m: &str| matches!(m, "raw" | "doc" | "effective");
    let mut out = Map::new();
    if let Some(node) = p.get("node").and_then(Value::as_str) {
        if !node.is_empty() {
            out.insert(
                "node".to_string(),
                Value::String(node.chars().take(MAX_NODE_CHARS).collect()),
            );
            if let Some(mode) = p.get("mode").and_then(Value::as_str) {
                if valid_mode(mode) {
                    out.insert("mode".to_string(), Value::String(mode.to_string()));
                }
            }
            return Some(out);
        }
    }
    if let Some(file) = p.get("file").and_then(Value::as_str) {
        if !file.is_empty() {
            out.insert(
                "file".to_string(),
                Value::String(file.chars().take(MAX_FILE_CHARS).collect()),
            );
            out.insert("line".to_string(), Value::from(num_or(p.get("line"), 1)));
            if let Some(ctx) = p.get("context").and_then(Value::as_f64) {
                if ctx.is_finite() {
                    out.insert("context".to_string(), Value::from(floor_min1(ctx)));
                }
            }
            if let Some(to) = p.get("to").and_then(Value::as_f64) {
                if to.is_finite() {
                    out.insert("to".to_string(), Value::from(floor_min1(to)));
                }
            }
            if let Some(mode) = p.get("mode").and_then(Value::as_str) {
                if valid_mode(mode) {
                    out.insert("mode".to_string(), Value::String(mode.to_string()));
                }
            }
            return Some(out);
        }
    }
    None
}

/// `Math.max(1, Math.floor(v))` (`shell.js:1660-1661`).
fn floor_min1(v: f64) -> i64 {
    let f = v.floor() as i64;
    f.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_file(t: &mut Tabs, file: &str, line: i64) -> String {
        t.open_params(&json!({ "file": file, "line": line }))
            .expect("a file is a document")
    }

    /// `shell.js:964-968`: identity ignores mode and line.
    #[test]
    fn key_for_prefers_node_then_file() {
        assert_eq!(Tabs::key_for(&json!({ "node": "A.B.C", "mode": "doc" })), "node:A.B.C");
        assert_eq!(Tabs::key_for(&json!({ "node": "A.B.C", "line": 40 })), "node:A.B.C");
        assert_eq!(Tabs::key_for(&json!({ "file": "src/x.cs", "line": 9 })), "file:src/x.cs");
        assert_eq!(Tabs::key_for(&json!({ "file": "src/x.cs", "line": 12 })), "file:src/x.cs");
        assert_eq!(Tabs::key_for(&json!({})), "doc:unknown");
        // JS truthiness: an empty node/file is not an identity.
        assert_eq!(Tabs::key_for(&json!({ "node": "" })), "doc:unknown");
        assert_eq!(Tabs::key_for(&json!({ "node": "", "file": "a.cs" })), "file:a.cs");
    }

    /// `shell.js:978-995`: only the two understood shapes become tabs.
    #[test]
    fn normalize_accepts_node_and_file_shapes_only() {
        assert_eq!(
            Tabs::normalize(&json!({ "node": "N", "mode": "effective", "match": "m", "line": 5 })),
            Some(
                [("node", json!("N")), ("mode", json!("effective")), ("match", json!("m"))]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect()
            )
        );
        let file = Tabs::normalize(&json!({ "file": "a/b.cs", "to": 12, "context": 7, "mode": "raw" }))
            .expect("a file is normalisable");
        assert_eq!(file.get("line"), Some(&json!(1)));
        assert_eq!(file.get("to"), Some(&json!(12)));
        assert_eq!(file.get("context"), Some(&json!(7)));
        assert_eq!(file.get("mode"), Some(&json!("raw")));
        assert_eq!(Tabs::normalize(&json!({ "file": "" })), None);
        assert_eq!(Tabs::normalize(&json!({ "line": 3 })), None);
        assert_eq!(Tabs::normalize(&json!("nope")), None);
    }

    /// `shell.js:1002-1006`: labels are the last two dotted segments, or
    /// `basename:line` for a file document.
    #[test]
    fn default_labels() {
        assert_eq!(default_label(&Tabs::normalize(&json!({ "node": "A.B.C.D" })).unwrap()), "C.D");
        assert_eq!(default_label(&Tabs::normalize(&json!({ "node": "D" })).unwrap()), "D");
        assert_eq!(
            default_label(&Tabs::normalize(&json!({ "file": "src/deep/x.cs", "line": 42 })).unwrap()),
            "x.cs:42"
        );
        assert_eq!(default_label(&Map::new()), "document");
    }

    /// `shell.js:1535-1568`: re-opening the same document re-activates one tab
    /// and merges the newer parameters.
    #[test]
    fn open_reuses_the_key_and_merges() {
        let mut t = Tabs::new();
        assert_eq!(open_file(&mut t, "a.cs", 1), "file:a.cs");
        assert_eq!(open_file(&mut t, "a.cs", 40), "file:a.cs");
        assert_eq!(t.count(), 1);
        assert_eq!(t.params("file:a.cs").unwrap().get("line"), Some(&json!(40)));
        assert_eq!(t.active_key(), Some("file:a.cs"));
    }

    /// `shell.js:1303-1311` + `:911`: the cap is 24, evicting neither the
    /// active tab nor the split document.
    #[test]
    fn max_tabs_evicts_the_oldest_non_active() {
        let mut t = Tabs::new();
        for i in 0..MAX_TABS {
            open_file(&mut t, &format!("f{i}.cs"), 1);
        }
        t.cycle(-1); // active is now the second-to-last... just pin it by key.
        let active = t.active_key().unwrap().to_string();
        open_file(&mut t, "new.cs", 1);
        assert_eq!(t.count(), MAX_TABS);
        assert_eq!(t.tabs()[0].key, "file:f1.cs", "the oldest non-active is evicted");
        assert_eq!(t.active_key(), Some("file:new.cs"));
        assert!(t.tabs().iter().any(|tab| tab.key == active));
    }

    /// `shell.js:1293-1299`: a single tab never cycles; the step wraps both ways
    /// and `delta == 0` goes forward.
    #[test]
    fn cycle_wraps_and_needs_two_tabs() {
        let mut t = Tabs::new();
        open_file(&mut t, "a.cs", 1);
        assert_eq!(t.cycle(1), None);
        open_file(&mut t, "b.cs", 1);
        assert_eq!(t.active_key(), Some("file:b.cs"));
        assert_eq!(t.cycle(1).as_deref(), Some("file:a.cs"));
        assert_eq!(t.cycle(-1).as_deref(), Some("file:b.cs"));
        assert_eq!(t.cycle(0).as_deref(), Some("file:a.cs"), "zero counts as forward");
        assert_eq!(t.cycle(-1).as_deref(), Some("file:b.cs"));
    }

    /// `shell.js:1316-1338`: closing picks the tab at the closed index, or the
    /// new last tab, and clears the split on the final close.
    #[test]
    fn close_active_selects_the_next_tab() {
        let mut t = Tabs::new();
        open_file(&mut t, "a.cs", 1);
        open_file(&mut t, "b.cs", 1);
        open_file(&mut t, "c.cs", 1);
        assert_eq!(t.close_active().as_deref(), Some("file:c.cs"));
        assert_eq!(t.active_key(), Some("file:b.cs"));
        assert_eq!(t.count(), 2);
        assert_eq!(t.close_active().as_deref(), Some("file:b.cs"));
        assert_eq!(t.active_key(), Some("file:a.cs"));
        assert_eq!(t.close_active().as_deref(), Some("file:a.cs"));
        assert_eq!(t.active_key(), None);
        assert!(!t.split());
        assert_eq!(t.close_active(), None);
    }

    /// `shell.js:1322-1325`: when the split document is closed the split
    /// re-points at another tab instead of going dark.
    #[test]
    fn closing_the_split_tab_repoints_it() {
        let mut t = Tabs::new();
        open_file(&mut t, "a.cs", 1);
        open_file(&mut t, "b.cs", 1);
        assert!(t.toggle_split());
        assert_eq!(t.split_key(), Some("file:a.cs"), "the other tab, not the active one");
        open_file(&mut t, "a.cs", 1);
        t.close_active();
        assert!(t.split());
        assert_eq!(t.split_key(), Some("file:b.cs"));
    }

    /// `shell.js:1345-1355`: close-others keeps exactly the active tab and
    /// reports the closed keys.
    #[test]
    fn close_others_keeps_only_the_active() {
        let mut t = Tabs::new();
        open_file(&mut t, "a.cs", 1);
        open_file(&mut t, "b.cs", 1);
        open_file(&mut t, "c.cs", 1);
        assert_eq!(t.close_others(), vec!["file:a.cs".to_string(), "file:b.cs".to_string()]);
        assert_eq!(t.count(), 1);
        assert_eq!(t.active_key(), Some("file:c.cs"));
        assert!(!t.split());
        assert!(Tabs::new().close_others().is_empty());
    }

    /// `shell.js:1475-1490`: the split needs two documents; it refuses while
    /// off with one tab, then toggles off.
    #[test]
    fn toggle_split_needs_two_documents() {
        let mut t = Tabs::new();
        assert!(!t.toggle_split());
        open_file(&mut t, "a.cs", 1);
        assert!(!t.toggle_split());
        assert!(!t.split());
        open_file(&mut t, "b.cs", 1);
        assert!(t.toggle_split());
        assert!(t.split());
        assert_eq!(t.split_key(), Some("file:a.cs"));
        assert!(!t.toggle_split());
        assert!(!t.split());
    }

    /// `shell.js:1516-1521`: the drag writes the divider fraction clamped to the
    /// reference band `0.15..=0.85`; a non-finite fraction is ignored.
    ///
    /// ADVERSARIAL: fails if the clamp is removed (a drag could drive the source
    /// body to 0/100% and the split body out of the pane) or if `NaN` is stored.
    #[test]
    fn set_split_frac_clamps_to_the_reference_band() {
        let mut t = Tabs::new();
        assert_eq!(t.split_frac(), 0.5, "shell.js:915-918 starts centred");
        t.set_split_frac(0.5);
        assert_eq!(t.split_frac(), 0.5);
        t.set_split_frac(0.0);
        assert_eq!(t.split_frac(), 0.15);
        t.set_split_frac(-3.0);
        assert_eq!(t.split_frac(), 0.15);
        t.set_split_frac(1.0);
        assert_eq!(t.split_frac(), 0.85);
        t.set_split_frac(9.0);
        assert_eq!(t.split_frac(), 0.85);
        t.set_split_frac(0.7);
        assert_eq!(t.split_frac(), 0.7);
        t.set_split_frac(f64::NAN);
        assert_eq!(t.split_frac(), 0.7, "NaN must not overwrite the position");
        t.set_split_frac(f64::INFINITY);
        assert_eq!(t.split_frac(), 0.7, "infinity must not overwrite the position");
        assert_eq!(
            t.session()["split"]["w"],
            json!(0.7),
            "the clamped fraction is what the session persists (shell.js:1682)"
        );
    }

    /// `shell.js:1667-1685`: the session round-trips through `restore_session`,
    /// including active, split key and divider fraction.
    #[test]
    fn session_round_trips() {
        let mut t = Tabs::new();
        open_file(&mut t, "a.cs", 4);
        open_file(&mut t, "b.cs", 1);
        t.toggle_split();
        let s = t.session();
        assert_eq!(s["v"], json!(1));
        assert_eq!(s["tabs"].as_array().unwrap().len(), 2);
        assert_eq!(s["active"], json!("file:b.cs"));
        assert_eq!(s["split"]["key"], json!("file:a.cs"));
        assert_eq!(s["split"]["w"], json!(0.5));

        let mut r = Tabs::new();
        assert!(r.try_restore_session(&s));
        assert_eq!(r.count(), 2);
        assert_eq!(r.active_key(), Some("file:b.cs"));
        assert_eq!(r.split_key(), Some("file:a.cs"));
        assert_eq!(r.label("file:a.cs"), Some("a.cs:4"));
    }

    /// `shell.js:1688-1708`: malformed rows are skipped, duplicates collapse,
    /// and an unusable session leaves the stack untouched.
    #[test]
    fn restore_skips_bad_rows() {
        let mut t = Tabs::new();
        open_file(&mut t, "keep.cs", 1);
        assert!(!t.try_restore_session(&json!({ "tabs": [] })));
        assert!(!t.try_restore_session(&json!({ "tabs": [{ "nope": 1 }] })));
        assert_eq!(t.count(), 1, "a rejected session must not clear the stack");

        let ok = t.try_restore_session(&json!({
            "tabs": [
                1,
                { "params": { "file": "x.cs", "line": 3 }, "label": "X" },
                { "params": { "file": "x.cs", "line": 9 } },
                { "params": { "node": "N.M", "mode": "doc" }, "mode": "effective" }
            ],
            "active": "file:x.cs"
        }));
        assert!(ok);
        assert_eq!(t.count(), 2, "the scalar row is skipped and the duplicate collapses");
        assert_eq!(t.active_key(), Some("file:x.cs"));
        assert_eq!(t.label("file:x.cs"), Some("X"));
        assert_eq!(t.params("file:x.cs").unwrap().get("line"), Some(&json!(3)));
        assert_eq!(t.params("node:N.M").unwrap().get("mode"), Some(&json!("effective")));
    }

    /// `shell.js:1653-1664`: the persisted sanitiser clamps modes, truncates
    /// paths, and floors positive-only numbers.
    #[test]
    fn san_params_clamps_the_persisted_row() {
        let p = san_params(&json!({ "file": "a.cs", "line": 0, "to": 9.9, "context": 0, "mode": "bogus" }))
            .unwrap();
        assert_eq!(p.get("line"), Some(&json!(1)), "line defaults to 1");
        assert_eq!(p.get("to"), Some(&json!(9)));
        assert_eq!(p.get("context"), Some(&json!(1)), "context clamps up to 1");
        assert_eq!(p.get("mode"), None, "bogus modes are dropped");
        let n = san_params(&json!({ "node": "n", "mode": "raw" })).unwrap();
        assert_eq!(n.get("mode"), Some(&json!("raw")));
        assert_eq!(san_params(&json!({ "file": "" })), None);
    }
}
