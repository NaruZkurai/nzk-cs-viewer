//! The arrange/spacing model behind `arrange.key(ev)` (`arrange.js:289-424`).
//!
//! This is the pure decision layer of the reference's `CSArrange` factory: it
//! owns the module-level `gap` (`arrange.js:41`) and reproduces the two commands
//! that change the selection's layout — `arrange(axis)` (`:289`) and
//! `spacing(dir)` (`:302`). The canvas wiring (`place`'s use of `posOf` /
//! `chipSizeFor`, the marquee gestures, the render loop) stays in `crate::app`;
//! this module only decides the *order* in which the ids would be placed and
//! how the spacing value moves.
//!
//! ## What the caller must supply
//!
//! The reference derives both orders itself: `sortByStep(list)` (`:292`) sorts
//! the selection into depth-first step order, and `orderAlong(list, axis)`
//! (`:293`, `:307`) sorts it by where the chips currently sit along the axis.
//! The frozen interface collapses those two sources into one closure,
//! `order_key: Fn(&str) -> Option<usize>`, so the two are split as:
//!
//! * `order_key(id)` is the id's rank in the caller's **`orderAlong(axis)`
//!   ordering** — its current position along the axis (x for `h`, y for `v`,
//!   x + y for `d`). `None` means "no position" and sorts after every known
//!   rank, exactly like `sortByStep`'s `Infinity` for an id missing from the
//!   step index (`arrange.js:160-161`). When every id is keyless there is no
//!   detectable axis, and [`Arrangement::spacing`] returns `None` — the
//!   reference's `detectAxis` returning `null` (`:305-306`).
//! * For [`Arrangement::arrange`], `ids` must already be in **step order**
//!   (`sortByStep`). The reference's `step` is therefore the input slice, and
//!   the flip test is `sameOrder(orderAlong(list, axis), step)` (`:293`).
//!
//! Because `order_key` already encodes which axis it was built for, `axis` is
//! carried for API parity and for the caller's toast; it does not change the
//! returned order — `place`'s axis only decides the geometry, not the order
//! (`arrange.js:218-232`).
//!
//! ## Spacing formula
//!
//! `gap = clamp(dir > 0 ? gap * 1.25 + 0.5 : (gap - 0.5) / 1.25, MIN, MAX)`
//! (`arrange.js:308`), with `ARRANGE_GAP_DEFAULT = 16`, `ARRANGE_GAP_MIN = 2`,
//! `ARRANGE_GAP_MAX = 400` (`arrange.js:23`).

use super::Axis;

/// `arrange.js:23` — `ARRANGE_GAP_DEFAULT`, the initial `gap` (`:41`).
pub const ARRANGE_GAP_DEFAULT: f64 = 16.0;
/// `arrange.js:23` — `ARRANGE_GAP_MIN`, the tightest the spacing may become.
pub const ARRANGE_GAP_MIN: f64 = 2.0;
/// `arrange.js:23` — `ARRANGE_GAP_MAX`, the widest the spacing may become.
pub const ARRANGE_GAP_MAX: f64 = 400.0;

/// `arrange.js:25` — `clamp(v, lo, hi)`.
fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// The reference module's mutable `gap`, owned by the command layer.
///
/// The reference keeps `gap` at `createArrange` scope (`arrange.js:41`) and
/// mutates it only in `spacing`; one `Arrangement` is one such scope.
#[derive(Clone, Copy, Debug)]
pub struct Arrangement {
    /// The current spacing in world units (`arrange.js:41`, `:308`).
    pub gap: f64,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self::new()
    }
}

impl Arrangement {
    /// A fresh scope with the reference's initial `gap` (16).
    pub fn new() -> Self {
        Self {
            gap: ARRANGE_GAP_DEFAULT,
        }
    }

    /// The current spacing.
    pub fn gap(&self) -> f64 {
        self.gap
    }

    /// `orderAlong(list, axis)` (`arrange.js:255-271`): a stable sort of `ids`
    /// by the caller's along-axis rank, unkeyed ids last. The reference's
    /// `||`-chained tie-breaks are the caller's job to encode in the rank, so
    /// equal ranks keep the incoming order instead of inventing one.
    fn order_along(ids: &[String], order_key: &dyn Fn(&str) -> Option<usize>) -> Vec<String> {
        let mut keyed: Vec<(usize, usize, &String)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (order_key(id.as_str()).unwrap_or(usize::MAX), i, id))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        keyed.into_iter().map(|(_, _, id)| id.clone()).collect()
    }

    /// `arrange(axis)` (`arrange.js:289-299`): the order the selection would be
    /// placed in, or `None` for the reference's toast-and-return.
    ///
    /// `ids` is the caller's `sortByStep` order. If the selection already runs
    /// in step order along the axis (`sameOrder(orderAlong, step)`) the order
    /// is reversed — the reference's flip — otherwise it is the step order.
    /// `None` when fewer than two ids are selected (`:291`).
    pub fn arrange(
        &mut self,
        ids: &[String],
        axis: Axis,
        order_key: &dyn Fn(&str) -> Option<usize>,
    ) -> Option<Vec<String>> {
        if ids.len() < 2 {
            return None;
        }
        // `place`'s axis decides geometry only; `order_key` already encodes the
        // along-axis ranking, so the returned order is axis-independent.
        let _ = axis;
        let along = Self::order_along(ids, order_key);
        let in_step_order = along == ids;
        let mut order = ids.to_vec();
        if in_step_order {
            order.reverse();
        }
        Some(order)
    }

    /// `spacing(dir)` (`arrange.js:302-312`): tighten (`dir < 0`) or widen
    /// (`dir > 0`) `gap`, then return the along-axis order that would be placed.
    ///
    /// `None` when fewer than two ids are selected (`:304`), or when no id
    /// carries a position along the axis — the reference's `detectAxis`
    /// returning `null` (`:305-306`), which the caller signals with a keyless
    /// `order_key`.
    pub fn spacing(
        &mut self,
        ids: &[String],
        dir: i32,
        order_key: &dyn Fn(&str) -> Option<usize>,
    ) -> Option<Vec<String>> {
        if ids.len() < 2 {
            return None;
        }
        if ids.iter().all(|id| order_key(id.as_str()).is_none()) {
            return None;
        }
        // `arrange.js:308`, verbatim.
        self.gap = clamp(
            if dir > 0 {
                self.gap * 1.25 + 0.5
            } else {
                (self.gap - 0.5) / 1.25
            },
            ARRANGE_GAP_MIN,
            ARRANGE_GAP_MAX,
        );
        Some(Self::order_along(ids, order_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `arrange.js:23`: the three constants, and `:41`/`arrange.js:16` for the
    /// initial `gap`.
    #[test]
    fn constants_and_initial_gap() {
        assert_eq!(ARRANGE_GAP_DEFAULT, 16.0);
        assert_eq!(ARRANGE_GAP_MIN, 2.0);
        assert_eq!(ARRANGE_GAP_MAX, 400.0);
        let a = Arrangement::new();
        assert_eq!(a.gap(), 16.0);
        assert_eq!(a.gap, ARRANGE_GAP_DEFAULT);
        assert_eq!(Arrangement::default().gap(), 16.0);
    }

    /// `arrange.js:291`: fewer than two ids toasts and returns.
    #[test]
    fn arrange_needs_two_ids() {
        let mut a = Arrangement::new();
        let none = |_: &str| None;
        assert_eq!(a.arrange(&[], Axis::Vertical, &none), None);
        assert_eq!(a.arrange(&ids(&["a"]), Axis::Vertical, &none), None);
    }

    /// `arrange.js:292-295`: `inStepOrder` false -> the step order is placed
    /// unchanged (the selection is not yet running in step order along the axis).
    #[test]
    fn arrange_keeps_step_order_when_not_aligned() {
        let mut a = Arrangement::new();
        // Step order is a,b,c; along the axis they currently read c,b,a.
        let rank = |id: &str| match id {
            "a" => Some(2),
            "b" => Some(1),
            "c" => Some(0),
            _ => None,
        };
        let out = a.arrange(&ids(&["a", "b", "c"]), Axis::Vertical, &rank);
        assert_eq!(out, Some(ids(&["a", "b", "c"])));
    }

    /// `arrange.js:293-294`: already in step order along the axis -> flip.
    #[test]
    fn arrange_flips_when_already_in_step_order() {
        let mut a = Arrangement::new();
        // Step order a,b,c and along-axis order a,b,c agree.
        let rank = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            "c" => Some(2),
            _ => None,
        };
        let out = a.arrange(&ids(&["a", "b", "c"]), Axis::Horizontal, &rank);
        assert_eq!(out, Some(ids(&["c", "b", "a"])));
        // After that placement the selection reads c,b,a along the axis, so a
        // second press is no longer aligned and lands back on step order — the
        // reference's flip-back (`arrange.js:283-287`).
        let flipped_rank = |id: &str| match id {
            "a" => Some(2),
            "b" => Some(1),
            "c" => Some(0),
            _ => None,
        };
        let out2 = a.arrange(&ids(&["a", "b", "c"]), Axis::Horizontal, &flipped_rank);
        assert_eq!(out2, Some(ids(&["a", "b", "c"])));
    }

    /// `arrange.js:187-203`/`:305-306`: no detectable axis -> spacing returns
    /// without touching `gap` or placing anything.
    #[test]
    fn spacing_stops_without_a_detectable_axis() {
        let mut a = Arrangement::new();
        let none = |_: &str| None;
        assert_eq!(a.spacing(&ids(&["a", "b"]), 1, &none), None);
        assert_eq!(a.gap(), 16.0);
    }

    /// `arrange.js:304`: fewer than two ids toasts and returns.
    #[test]
    fn spacing_needs_two_ids() {
        let mut a = Arrangement::new();
        let rank = |_: &str| Some(0);
        assert_eq!(a.spacing(&[], 1, &rank), None);
        assert_eq!(a.spacing(&ids(&["a"]), 1, &rank), None);
        assert_eq!(a.gap(), 16.0);
    }

    /// `arrange.js:308`: the exact widen/tighten formula, unclamped.
    #[test]
    fn spacing_formula_matches_reference() {
        let rank = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            _ => None,
        };
        let mut a = Arrangement::new();
        // widen: 16 * 1.25 + 0.5 = 20.5
        let out = a.spacing(&ids(&["b", "a"]), 1, &rank);
        assert_eq!(out, Some(ids(&["a", "b"])));
        assert_eq!(a.gap(), 20.5);
        // tighten: (20.5 - 0.5) / 1.25 = 16
        a.spacing(&ids(&["a", "b"]), -1, &rank);
        assert_eq!(a.gap(), 16.0);
    }

    /// `arrange.js:308`: `clamp` holds the floor and the ceiling.
    #[test]
    fn spacing_clamps_to_min_and_max() {
        let rank = |id: &str| match id {
            "a" => Some(0),
            "b" => Some(1),
            _ => None,
        };
        let list = ids(&["a", "b"]);
        let mut a = Arrangement::new();
        // (2 - 0.5) / 1.25 = 1.2 -> clamped up to 2.
        a.gap = 2.0;
        a.spacing(&list, -1, &rank);
        assert_eq!(a.gap(), ARRANGE_GAP_MIN);
        // 400 * 1.25 + 0.5 = 500.5 -> clamped down to 400.
        a.gap = 400.0;
        a.spacing(&list, 1, &rank);
        assert_eq!(a.gap(), ARRANGE_GAP_MAX);
        // Repeated tightening converges to the floor and stays there.
        let mut b = Arrangement::new();
        for _ in 0..64 {
            b.spacing(&list, -1, &rank);
        }
        assert_eq!(b.gap(), ARRANGE_GAP_MIN);
    }

    /// `arrange.js:255-271`/`:160-161`: unkeyed ids sort after known ranks and
    /// equal ranks keep the incoming order (the caller owns the tie-breaks).
    #[test]
    fn order_along_puts_unkeyed_last_and_is_stable() {
        let mut a = Arrangement::new();
        let rank = |id: &str| match id {
            "a" => Some(1),
            "b" => None,
            "c" => Some(0),
            "d" => Some(1),
            _ => None,
        };
        // spacing returns `orderAlong(list, axis)` (`arrange.js:307`).
        let out = a.spacing(&ids(&["a", "b", "c", "d"]), 1, &rank);
        assert_eq!(out, Some(ids(&["c", "a", "d", "b"])));
        // arrange compares the same along-axis order against the step order and
        // places the step order when they disagree (`arrange.js:293-295`).
        let out2 = a.arrange(&ids(&["b", "a", "d", "c"]), Axis::Diagonal, &rank);
        assert_eq!(out2, Some(ids(&["b", "a", "d", "c"])));
    }
}
