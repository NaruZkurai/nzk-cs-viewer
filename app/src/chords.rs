//! The four chords that were blocked on missing models (user priority 4-7).
//!
//! This module owns the *decision*: given the live modifier state and the key
//! the window delivered, which chord (if any) does the reference claim? It does
//! **not** perform the action — [`crate::app`] dispatches the result — so the
//! whole mapping is testable without a window, exactly like
//! [`crate::app::bare_letter_action`] and `is_history_back_chord`.
//!
//! ## Where each chord comes from
//!
//! * `Ctrl+Tab` / `Ctrl+Shift+Tab`, `Alt+W`, `Alt+Shift+W`, `Alt+S` —
//!   `shell.js:410-431`, the `window.CsTabs` block. It sits *after* the
//!   "typing" guard and the shortcuts-panel guard, and *before*
//!   `arrange.key(ev)`.
//! * `Ctrl+Shift+V` / `H` / `D`, `Shift+D`, `Shift+=` / `Shift+-` —
//!   `arrange.js:416-424` (`arrange()` at `:289`, `spacing()` at `:302`).
//! * `Alt+Right` — `nav.js:439-441`, the `forward()` half of the pane stack.
//!   `back()` is `Alt+Left` (`:440`) and is already bound.
//! * `/` — `shell.js:452-455`: leave find mode, focus and select the search box.
//!
//! ## Precedence is the point
//!
//! The reference's checks are ordered, and two orderings are observable:
//!
//! * `Ctrl+Shift+D` is an **arrange**, and plain `Shift+D` is *also* an arrange
//!   (`arrange.js:423`), even though the canvas binds a bare `d`. The arrange
//!   test runs before the bare-letter chain, so shift must be consumed here
//!   rather than leaking to [`crate::app`]'s letter table.
//! * `Alt+Right` requires `!shift`; `Alt+Shift+W` is a tab close and must not be
//!   mistaken for `Alt+W`.

pub mod arrange;
pub mod tabs;

/// The axis `arrange()` lays a selection out along (`arrange.js:290-296`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    Vertical,
    Horizontal,
    Diagonal,
}

impl Axis {
    /// The word `arrange()` puts in its toast (`arrange.js:295`).
    pub fn label(self) -> &'static str {
        match self {
            Axis::Vertical => "vertically",
            Axis::Horizontal => "horizontally",
            Axis::Diagonal => "diagonally",
        }
    }

    /// The single-character axis `arrange()` was called with (`arrange.js:418`).
    pub fn from_char(c: char) -> Option<Axis> {
        match c {
            'v' | 'V' => Some(Axis::Vertical),
            'h' | 'H' => Some(Axis::Horizontal),
            'd' | 'D' => Some(Axis::Diagonal),
            _ => None,
        }
    }
}

/// One chord the reference recognises.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chord {
    /// `Ctrl+Tab` / `Ctrl+Shift+Tab` — step the document-tab stack.
    TabCycle(isize),
    /// `Alt+W` or `Alt+Shift+W` — close the active tab, or every other tab.
    TabClose { others: bool },
    /// `Alt+S` — split the pane, or rejoin it.
    TabToggleSplit,
    /// `Alt+Right` — step forward through the pane's document stack.
    ForwardHistory,
    /// `Ctrl+Shift+V|H|D` or `Shift+D` — arrange the selection along an axis.
    Arrange(Axis),
    /// `Shift+=` / `Shift+-` — widen or tighten the current spacing.
    Spacing(i32),
    /// `/` — focus and select the search box.
    FocusSearch,
}

/// Decide which chord a key event is.
///
/// `key` is a normalised token: a single character (case as delivered) for
/// character keys, or `"Tab"` / `"ArrowRight"` for the two named keys the
/// reference matches. `alt`/`ctrl`/`shift`/`meta` are the live modifiers.
///
/// The four-way modifier tests are copied literally from the reference; in
/// particular `Ctrl` and `Meta` are **not** interchangeable here, because
/// `shell.js:414` spells `Alt+W` as `!ev.ctrlKey && !ev.metaKey` while
/// `arrange.js:417` treats `ctrlKey || metaKey` as one modifier.
pub fn resolve(alt: bool, ctrl: bool, shift: bool, meta: bool, key: &str) -> Option<Chord> {
    let chars: Vec<char> = key.chars().collect();
    let one = if chars.len() == 1 { Some(chars[0]) } else { None };

    // nav.js:439-441 — the pane stack's forward step. `Alt+Right` with no other
    // modifier; `Alt+Shift+Right` is not a chord.
    if alt && !ctrl && !meta && !shift && key == "ArrowRight" {
        return Some(Chord::ForwardHistory);
    }

    // shell.js:410-431 — the `window.CsTabs` block.
    if ctrl && !alt && !meta && key == "Tab" {
        return Some(Chord::TabCycle(if shift { -1 } else { 1 }));
    }
    if alt && !ctrl && !meta && one.is_some_and(|c| c == 'w' || c == 'W') {
        return Some(Chord::TabClose { others: shift });
    }
    if alt && !ctrl && !meta && !shift && one.is_some_and(|c| c == 's' || c == 'S') {
        return Some(Chord::TabToggleSplit);
    }

    // arrange.js:416-424 — `arrange.key(ev)`.
    if (ctrl || meta) && shift {
        if let Some(axis) = one.and_then(Axis::from_char) {
            return Some(Chord::Arrange(axis));
        }
    }
    if shift {
        if one.is_some_and(|c| c == '+' || c == '=') {
            return Some(Chord::Spacing(1));
        }
        if one.is_some_and(|c| c == '-' || c == '_') {
            return Some(Chord::Spacing(-1));
        }
        // A bare `Shift+D` is the diagonal arrange even with no Ctrl — checked
        // after the `+`/`-` spacing cases, matching the reference's order.
        if one.is_some_and(|c| c == 'd' || c == 'D') {
            return Some(Chord::Arrange(Axis::Diagonal));
        }
    }

    // shell.js:452-455 — `/` returns to graph mode and focuses the search box.
    //
    // Unlike the `c` branch above it (`shell.js:448`, which spells out
    // `!ctrlKey && !metaKey && !altKey`), the reference gives `/` **no** modifier
    // test at all: it is a bare `else if (ev.key === '/')` in the chain. So the
    // port must not invent one — `Ctrl+/` and `Alt+/` focus the box too. (A
    // shifted `/` arrives as `?` on a US layout and so never matches, which is
    // the reference's behaviour as well.)
    if one == Some('/') {
        return Some(Chord::FocusSearch);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(alt: bool, ctrl: bool, shift: bool, key: &str) -> Option<Chord> {
        resolve(alt, ctrl, shift, false, key)
    }

    /// `shell.js:414-415`: `Ctrl+Tab` cycles forward, `Ctrl+Shift+Tab` back.
    #[test]
    fn ctrl_tab_cycles_and_shift_reverses() {
        assert_eq!(ch(false, true, false, "Tab"), Some(Chord::TabCycle(1)));
        assert_eq!(ch(false, true, true, "Tab"), Some(Chord::TabCycle(-1)));
    }

    /// `shell.js:414` requires `!altKey && !metaKey`; `Ctrl+Alt+Tab` is nothing.
    #[test]
    fn tab_cycle_rejects_alt_and_meta() {
        assert_eq!(resolve(true, true, false, false, "Tab"), None);
        assert_eq!(resolve(false, true, false, true, "Tab"), None);
    }

    /// `shell.js:416-424`: `Alt+W` closes active, `Alt+Shift+W` closes others,
    /// and `Alt+S` toggles the split. The letter is accepted in either case
    /// because the reference tests both.
    #[test]
    fn alt_tab_management_chords() {
        assert_eq!(ch(true, false, false, "w"), Some(Chord::TabClose { others: false }));
        assert_eq!(ch(true, false, false, "W"), Some(Chord::TabClose { others: false }));
        assert_eq!(ch(true, false, true, "W"), Some(Chord::TabClose { others: true }));
        assert_eq!(ch(true, false, false, "s"), Some(Chord::TabToggleSplit));
        // `Alt+Shift+S` is not the split chord — only `Alt+S` is.
        assert_eq!(ch(true, false, true, "S"), None);
    }

    /// `nav.js:441`: `Alt+Right` needs all three other modifiers absent.
    #[test]
    fn alt_right_is_forward_only_without_shift() {
        assert_eq!(ch(true, false, false, "ArrowRight"), Some(Chord::ForwardHistory));
        assert_eq!(ch(true, false, true, "ArrowRight"), None);
        assert_eq!(ch(false, false, false, "ArrowRight"), None);
    }

    /// `arrange.js:418-420`: `Ctrl+Shift+V|H|D`.
    #[test]
    fn ctrl_shift_letters_arrange() {
        assert_eq!(ch(false, true, true, "v"), Some(Chord::Arrange(Axis::Vertical)));
        assert_eq!(ch(false, true, true, "h"), Some(Chord::Arrange(Axis::Horizontal)));
        assert_eq!(ch(false, true, true, "d"), Some(Chord::Arrange(Axis::Diagonal)));
        // The reference's `(ctrlKey || metaKey)` treats Cmd as Ctrl.
        assert_eq!(resolve(false, false, true, true, "v"), Some(Chord::Arrange(Axis::Vertical)));
    }

    /// `arrange.js:423`: a bare `Shift+D` is the diagonal arrange. This is the
    /// divergence that makes the chord table worth testing — without it, `D`
    /// would fall through to the bare-letter canvas table.
    #[test]
    fn bare_shift_d_is_diagonal() {
        assert_eq!(ch(false, false, true, "d"), Some(Chord::Arrange(Axis::Diagonal)));
        assert_eq!(ch(false, false, true, "D"), Some(Chord::Arrange(Axis::Diagonal)));
    }

    /// `arrange.js:421-422`: `Shift+=` widens, `Shift+-` tightens.
    #[test]
    fn shift_plus_minus_change_spacing() {
        assert_eq!(ch(false, false, true, "="), Some(Chord::Spacing(1)));
        assert_eq!(ch(false, false, true, "+"), Some(Chord::Spacing(1)));
        assert_eq!(ch(false, false, true, "-"), Some(Chord::Spacing(-1)));
        assert_eq!(ch(false, false, true, "_"), Some(Chord::Spacing(-1)));
    }

    /// `shell.js:452`: `/` focuses the search box. The branch is **unguarded** in
    /// the reference, so `Ctrl+/` and `Alt+/` reach it as well; a shifted `/`
    /// arrives as `?` and so never reaches this arm with `key == "/"`.
    #[test]
    fn slash_focuses_search() {
        assert_eq!(ch(false, false, false, "/"), Some(Chord::FocusSearch));
        assert_eq!(ch(false, true, false, "/"), Some(Chord::FocusSearch));
        assert_eq!(ch(true, false, false, "/"), Some(Chord::FocusSearch));
        assert_eq!(ch(false, false, false, "?"), None);
    }
}
