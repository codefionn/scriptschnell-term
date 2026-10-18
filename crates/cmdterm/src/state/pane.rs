//! The pane tree of a tab: panes split side by side or stacked, nested to
//! any depth. Each pane shows one panel. Panes are addressed by their
//! position in depth-first order, first child before second.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    SideBySide,
    Stacked,
}

/// A pane, or two subtrees sharing an area along `axis`. The live model
/// stores panel ids in the panes; the saved state stores panel indices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayout<T = usize> {
    Pane {
        panel: T,
    },
    Split {
        axis: SplitAxis,
        first: Box<PaneLayout<T>>,
        second: Box<PaneLayout<T>>,
        /// Thousandths of the split's room the first half takes. 0 shares
        /// the room by the panes each half lays out along `axis`.
        #[serde(default, skip_serializing_if = "is_even")]
        share: u16,
    },
}

fn is_even(share: &u16) -> bool {
    *share == 0
}

/// Least share either half of a resized split keeps, in thousandths.
pub const LEAST_SHARE: u16 = 100;

/// The share a split's first half takes, in thousandths, with an even
/// split (0) resolved by the panes each half lays out along `axis`.
fn effective_share<T: Copy + PartialEq>(
    share: u16,
    first: &PaneLayout<T>,
    second: &PaneLayout<T>,
    axis: SplitAxis,
) -> u16 {
    match share {
        0 => {
            let (a, b) = (first.span(axis), second.span(axis));
            (1000 * a / (a + b)) as u16
        }
        own => own,
    }
}

/// A fraction of a split as a stored share, kept in the resizable range.
fn keep_share(fraction: f32) -> u16 {
    ((fraction * 1000.0).round() as i32)
        .clamp(i32::from(LEAST_SHARE), 1000 - i32::from(LEAST_SHARE)) as u16
}

impl<T: Copy + PartialEq> PaneLayout<T> {
    pub fn pane(panel: T) -> Self {
        Self::Pane { panel }
    }

    /// Number of panes.
    pub fn len(&self) -> usize {
        match self {
            Self::Pane { .. } => 1,
            Self::Split { first, second, .. } => first.len() + second.len(),
        }
    }

    /// A tree always has at least one pane.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The panel shown in pane `index`.
    pub fn get(&self, index: usize) -> Option<T> {
        match self {
            Self::Pane { panel } => (index == 0).then_some(*panel),
            Self::Split { first, second, .. } => {
                let count = first.len();
                if index < count {
                    first.get(index)
                } else {
                    second.get(index - count)
                }
            }
        }
    }

    /// Shows `panel` in pane `index`. Returns false when there is no such pane.
    pub fn set(&mut self, index: usize, panel: T) -> bool {
        match self.slot(index) {
            Some(slot) => {
                *slot = panel;
                true
            }
            None => false,
        }
    }

    fn slot(&mut self, index: usize) -> Option<&mut T> {
        match self {
            Self::Pane { panel } => (index == 0).then_some(panel),
            Self::Split { first, second, .. } => {
                let count = first.len();
                if index < count {
                    first.slot(index)
                } else {
                    second.slot(index - count)
                }
            }
        }
    }

    /// The first pane showing `panel`.
    pub fn position(&self, panel: T) -> Option<usize> {
        let mut found = None;
        let mut index = 0;
        self.for_each(&mut |shown| {
            if found.is_none() && shown == panel {
                found = Some(index);
            }
            index += 1;
        });
        found
    }

    /// Calls `f` with every pane's panel in pane order.
    pub fn for_each(&self, f: &mut impl FnMut(T)) {
        match self {
            Self::Pane { panel } => f(*panel),
            Self::Split { first, second, .. } => {
                first.for_each(f);
                second.for_each(f);
            }
        }
    }

    /// Splits pane `index` along `axis`. The old pane becomes the first half
    /// and `panel` the second, at index `index + 1`.
    pub fn split(&mut self, index: usize, axis: SplitAxis, panel: T) -> bool {
        self.split_with(index, axis, panel, false)
    }

    /// Splits pane `index` along `axis` with `panel` in the first half when
    /// `before` is set, at index `index`, else in the second, at `index + 1`.
    pub fn split_with(&mut self, index: usize, axis: SplitAxis, panel: T, before: bool) -> bool {
        self.split_tree(index, axis, Self::pane(panel), before)
    }

    /// Splits pane `index` along `axis` and puts the panes of `tree` in the
    /// new half: the first when `before` is set, else the second.
    pub fn split_tree(&mut self, index: usize, axis: SplitAxis, tree: Self, before: bool) -> bool {
        match self {
            Self::Pane { panel: shown } if index == 0 => {
                let (old, new) = (Box::new(Self::pane(*shown)), Box::new(tree));
                let (first, second) = if before { (new, old) } else { (old, new) };
                *self = Self::Split {
                    axis,
                    first,
                    second,
                    share: 0,
                };
                true
            }
            Self::Pane { .. } => false,
            Self::Split { first, second, .. } => {
                let count = first.len();
                if index < count {
                    first.split_tree(index, axis, tree, before)
                } else {
                    second.split_tree(index - count, axis, tree, before)
                }
            }
        }
    }

    /// Shares the room of every split along `axis` evenly by its panes, as
    /// after a tab's panes join another tab, so none of them gets a sliver.
    pub fn even_out(&mut self, axis: SplitAxis) {
        if let Self::Split {
            axis: own,
            first,
            second,
            share,
        } = self
        {
            if *own == axis {
                *share = 0;
            }
            first.even_out(axis);
            second.even_out(axis);
        }
    }

    /// Removes pane `index`; its sibling takes the space of their split.
    /// Returns the panel it showed. The last pane cannot be removed.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        let count = first.len();
        let (target, other, index) = if index < count {
            (first, second, index)
        } else {
            (second, first, index - count)
        };
        if let Self::Pane { panel } = **target {
            if index != 0 {
                return None;
            }
            let rest = std::mem::replace(&mut **other, Self::pane(panel));
            *self = rest;
            return Some(panel);
        }
        target.remove(index)
    }

    /// The same tree with `f` applied to every pane. Panes for which `f`
    /// returns `None` are removed; `None` when no pane is left.
    pub fn filter_map<U: Copy + PartialEq>(
        &self,
        f: &mut impl FnMut(T) -> Option<U>,
    ) -> Option<PaneLayout<U>> {
        match self {
            Self::Pane { panel } => f(*panel).map(PaneLayout::pane),
            Self::Split {
                axis,
                first,
                second,
                share,
            } => match (first.filter_map(f), second.filter_map(f)) {
                (Some(first), Some(second)) => Some(PaneLayout::Split {
                    axis: *axis,
                    first: Box::new(first),
                    second: Box::new(second),
                    share: *share,
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Moves the edge of the innermost split holding pane `index` by
    /// `delta` thousandths, toward the second half for a positive one, so
    /// the pane grows when it is in the first half. Returns false for a lone
    /// pane or when the share is already at its limit.
    pub fn resize(&mut self, index: usize, delta: i32) -> bool {
        let Self::Split {
            axis,
            first,
            second,
            share,
        } = self
        else {
            return false;
        };
        let count = first.len();
        let (inner, in_first) = if index < count {
            (first.as_mut(), true)
        } else {
            (second.as_mut(), false)
        };
        let local = if in_first { index } else { index - count };
        if inner.resize(local, delta) {
            return true;
        }
        let current = match *share {
            0 => {
                let (a, b) = (first.span(*axis), second.span(*axis));
                (1000 * a / (a + b)) as i32
            }
            own => i32::from(own),
        };
        let delta = if in_first { delta } else { -delta };
        let least = i32::from(LEAST_SHARE);
        let next = (current + delta).clamp(least, 1000 - least);
        if next == current {
            return false;
        }
        *share = next as u16;
        true
    }

    /// Gives the split between pane `boundary` and the pane after it
    /// `share` thousandths of its room for its first half. Returns false
    /// when there is no such split.
    pub fn set_share(&mut self, boundary: usize, share: u16) -> bool {
        let Self::Split {
            axis,
            first,
            second,
            share: own,
        } = self
        else {
            return false;
        };
        let axis = *axis;
        let count = first.len();
        if boundary + 1 == count {
            let least = LEAST_SHARE;
            let before = f32::from(effective_share(*own, first, second, axis)) / 1000.0;
            *own = share.clamp(least, 1000 - least);
            let after = f32::from(*own) / 1000.0;
            // The dividers beside this one stay where they are: a half
            // split along the same axis gets its share back in proportion,
            // so only the two panes at the dragged line change size.
            if let Self::Split {
                axis: inner,
                first: a,
                second: b,
                share: inner_share,
            } = first.as_mut()
                && *inner == axis
                && after > 0.0
            {
                let q = f32::from(effective_share(*inner_share, a, b, axis)) / 1000.0;
                *inner_share = keep_share(q * before / after);
            }
            if let Self::Split {
                axis: inner,
                first: a,
                second: b,
                share: inner_share,
            } = second.as_mut()
                && *inner == axis
                && after < 1.0
            {
                let p = f32::from(effective_share(*inner_share, a, b, axis)) / 1000.0;
                *inner_share = keep_share((before + p * (1.0 - before) - after) / (1.0 - after));
            }
            true
        } else if boundary + 1 < count {
            first.set_share(boundary, share)
        } else {
            second.set_share(boundary - count, share)
        }
    }

    /// Panes laid out along `axis` in this subtree, counting nested splits on
    /// the same axis, so three panes in a row get a third of the width each.
    pub fn span(&self, axis: SplitAxis) -> usize {
        match self {
            Self::Split {
                axis: own,
                first,
                second,
                ..
            } if *own == axis => first.span(axis) + second.span(axis),
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneLayout, SplitAxis};

    #[test]
    fn evening_out_resets_the_shares_along_its_axis() {
        let mut layout = PaneLayout::pane(1u64);
        assert!(layout.split(0, SplitAxis::SideBySide, 2));
        assert!(layout.set_share(0, 300));
        let mut moved = PaneLayout::pane(3u64);
        assert!(moved.split(0, SplitAxis::SideBySide, 4));
        assert!(moved.set_share(0, 800));
        assert!(layout.split_tree(0, SplitAxis::SideBySide, moved, false));
        layout.even_out(SplitAxis::SideBySide);
        let mut shares = Vec::new();
        fn collect(layout: &PaneLayout<u64>, out: &mut Vec<u16>) {
            if let PaneLayout::Split {
                first,
                second,
                share,
                ..
            } = layout
            {
                out.push(*share);
                collect(first, out);
                collect(second, out);
            }
        }
        collect(&layout, &mut shares);
        assert!(shares.iter().all(|share| *share == 0), "{shares:?}");
        assert_eq!(panels(&layout), vec![1, 3, 4, 2]);
    }

    #[test]
    fn dragging_a_line_keeps_the_next_line_in_place() {
        // Three panes in a row: 1 | (2 | 3), the inner split even.
        let mut layout = PaneLayout::pane(1u64);
        assert!(layout.split(0, SplitAxis::SideBySide, 2));
        assert!(layout.split(1, SplitAxis::SideBySide, 3));
        // Lines at a third and two thirds. Move the first to a fifth.
        assert!(layout.set_share(0, 200));
        let PaneLayout::Split { share, second, .. } = &layout else {
            panic!("not a split");
        };
        assert_eq!(*share, 200);
        let PaneLayout::Split { share: inner, .. } = second.as_ref() else {
            panic!("not a split");
        };
        // The second line stays at 667 of the room: 200 + p * 800.
        let line = 200.0 + f32::from(*inner) / 1000.0 * 800.0;
        assert!(
            (line - 667.0).abs() < 2.0,
            "{inner} puts the line at {line}"
        );
    }

    fn panels(layout: &PaneLayout<u64>) -> Vec<u64> {
        let mut out = Vec::new();
        layout.for_each(&mut |panel| out.push(panel));
        out
    }

    #[test]
    fn splits_nest_and_keep_pane_order() {
        let mut layout = PaneLayout::pane(1);
        assert!(layout.split(0, SplitAxis::SideBySide, 2));
        assert!(layout.split(0, SplitAxis::Stacked, 3));
        assert_eq!(panels(&layout), [1, 3, 2]);
        assert_eq!(layout.len(), 3);
        assert_eq!(layout.get(1), Some(3));
        assert_eq!(layout.position(2), Some(2));
        assert!(!layout.split(3, SplitAxis::Stacked, 4));
        assert_eq!(layout.span(SplitAxis::SideBySide), 2);
        assert!(layout.split(2, SplitAxis::SideBySide, 4));
        assert_eq!(layout.span(SplitAxis::SideBySide), 3);
        assert!(layout.split_with(0, SplitAxis::Stacked, 5, true));
        assert_eq!(panels(&layout), [5, 1, 3, 2, 4]);
        let mut other = PaneLayout::pane(6);
        other.split(0, SplitAxis::Stacked, 7);
        assert!(layout.split_tree(4, SplitAxis::SideBySide, other, false));
        assert_eq!(panels(&layout), [5, 1, 3, 2, 4, 6, 7]);
    }

    #[test]
    fn removing_a_pane_hands_its_space_to_the_sibling() {
        let mut layout = PaneLayout::pane(1);
        layout.split(0, SplitAxis::SideBySide, 2);
        layout.split(1, SplitAxis::Stacked, 3);
        assert_eq!(layout.remove(0), Some(1));
        assert_eq!(
            layout,
            PaneLayout::Split {
                axis: SplitAxis::Stacked,
                first: Box::new(PaneLayout::pane(2)),
                second: Box::new(PaneLayout::pane(3)),
                share: 0,
            }
        );
        assert_eq!(layout.remove(1), Some(3));
        assert_eq!(layout, PaneLayout::pane(2));
        assert_eq!(layout.remove(0), None);
    }

    #[test]
    fn filter_map_drops_missing_panes() {
        let mut layout = PaneLayout::pane(0usize);
        layout.split(0, SplitAxis::SideBySide, 1);
        layout.split(1, SplitAxis::Stacked, 7);
        let ids = layout.filter_map(&mut |index| (index < 2).then_some(index as u64 + 10));
        assert_eq!(
            ids,
            Some(PaneLayout::Split {
                axis: SplitAxis::SideBySide,
                first: Box::new(PaneLayout::pane(10)),
                second: Box::new(PaneLayout::pane(11)),
                share: 0,
            })
        );
        assert_eq!(layout.filter_map(&mut |_| None::<u64>), None);
    }

    #[test]
    fn resizing_moves_the_innermost_edge_and_stops_at_the_limit() {
        let mut layout = PaneLayout::pane(1);
        layout.split(0, SplitAxis::SideBySide, 2);
        layout.split(1, SplitAxis::Stacked, 3);
        // Pane 0 grows the outer split from its even share.
        assert!(layout.resize(0, 50));
        assert!(matches!(layout, PaneLayout::Split { share: 550, .. }));
        // Pane 2 sits in the second half of the inner split: growing it
        // moves that edge up.
        assert!(layout.resize(2, 50));
        let PaneLayout::Split { second, .. } = &layout else {
            unreachable!()
        };
        assert!(matches!(**second, PaneLayout::Split { share: 450, .. }));
        for _ in 0..20 {
            layout.resize(0, 50);
        }
        assert!(matches!(layout, PaneLayout::Split { share: 900, .. }));
        assert!(!layout.resize(0, 50));
        assert!(!PaneLayout::pane(1).resize(0, 50));
        // The edge after pane 1 belongs to the inner split.
        assert!(layout.set_share(1, 300));
        let PaneLayout::Split { second, .. } = &layout else {
            unreachable!()
        };
        assert!(matches!(**second, PaneLayout::Split { share: 300, .. }));
        assert!(layout.set_share(0, 10));
        assert!(matches!(layout, PaneLayout::Split { share: 100, .. }));
        assert!(!layout.set_share(2, 500));
        let saved = serde_json::to_string(&layout).unwrap();
        let back: PaneLayout = serde_json::from_str(&saved).unwrap();
        assert_eq!(back, layout);
    }
}
