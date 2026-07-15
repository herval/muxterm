use std::collections::HashMap;

use egui::{Pos2, Rect, Vec2};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct PaneId(pub u64);

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SplitAxis {
    /// Vertical divider, panes side by side (cmd+d).
    SideBySide,
    /// Horizontal divider, panes stacked (cmd+shift+d).
    Stacked,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Binary split tree of a tab. Leaves are panes.
#[derive(Debug)]
pub enum Node {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

pub enum Removal {
    NotFound,
    Removed { focus_hint: PaneId },
    BecameEmpty,
}

impl Node {
    /// Replace `Leaf(target)` with an evenly-split node holding the old pane
    /// first and `new_pane` second. Returns false if `target` is not in the
    /// tree.
    pub fn split(
        &mut self,
        target: PaneId,
        axis: SplitAxis,
        new_pane: PaneId,
    ) -> bool {
        self.split_with(target, axis, new_pane, 0.5)
    }

    /// Like `split`, but the new split gets `ratio` (the fraction kept by the
    /// old pane, `first`) instead of an even 0.5 - workspace templates use
    /// this to size the panes they lay out.
    pub fn split_with(
        &mut self,
        target: PaneId,
        axis: SplitAxis,
        new_pane: PaneId,
        ratio: f32,
    ) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    axis,
                    ratio,
                    first: Box::new(Node::Leaf(target)),
                    second: Box::new(Node::Leaf(new_pane)),
                };
                true
            },
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                first.split_with(target, axis, new_pane, ratio)
                    || second.split_with(target, axis, new_pane, ratio)
            },
        }
    }

    /// Remove `Leaf(target)`, promoting its sibling subtree in its place.
    pub fn remove(&mut self, target: PaneId) -> Removal {
        match self {
            Node::Leaf(id) => {
                if *id == target {
                    Removal::BecameEmpty
                } else {
                    Removal::NotFound
                }
            },
            Node::Split { first, second, .. } => {
                let target_is_first =
                    matches!(first.as_ref(), Node::Leaf(id) if *id == target);
                let target_is_second =
                    matches!(second.as_ref(), Node::Leaf(id) if *id == target);
                if target_is_first || target_is_second {
                    let sibling = if target_is_first { second } else { first };
                    let sibling = std::mem::replace(
                        sibling.as_mut(),
                        Node::Leaf(PaneId(u64::MAX)),
                    );
                    let focus_hint = sibling.first_leaf();
                    *self = sibling;
                    return Removal::Removed { focus_hint };
                }
                match first.remove(target) {
                    Removal::NotFound => second.remove(target),
                    r => r,
                }
            },
        }
    }

    pub fn first_leaf(&self) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { first, .. } => first.first_leaf(),
        }
    }

    pub fn leaves(&self) -> Vec<PaneId> {
        fn walk(node: &Node, out: &mut Vec<PaneId>) {
            match node {
                Node::Leaf(id) => out.push(*id),
                Node::Split { first, second, .. } => {
                    walk(first, out);
                    walk(second, out);
                },
            }
        }
        let mut out = Vec::new();
        walk(self, &mut out);
        out
    }
}

/// Split `rect` into (first, divider, second) along `axis`.
pub fn split_rect(
    rect: Rect,
    axis: SplitAxis,
    ratio: f32,
    gap: f32,
) -> (Rect, Rect, Rect) {
    match axis {
        SplitAxis::SideBySide => {
            let usable = (rect.width() - gap).max(0.0);
            let first_w = usable * ratio;
            let first = Rect::from_min_size(
                rect.min,
                Vec2::new(first_w, rect.height()),
            );
            let divider = Rect::from_min_size(
                Pos2::new(rect.min.x + first_w, rect.min.y),
                Vec2::new(gap, rect.height()),
            );
            let second = Rect::from_min_max(
                Pos2::new(rect.min.x + first_w + gap, rect.min.y),
                rect.max,
            );
            (first, divider, second)
        },
        SplitAxis::Stacked => {
            let usable = (rect.height() - gap).max(0.0);
            let first_h = usable * ratio;
            let first = Rect::from_min_size(
                rect.min,
                Vec2::new(rect.width(), first_h),
            );
            let divider = Rect::from_min_size(
                Pos2::new(rect.min.x, rect.min.y + first_h),
                Vec2::new(rect.width(), gap),
            );
            let second = Rect::from_min_max(
                Pos2::new(rect.min.x, rect.min.y + first_h + gap),
                rect.max,
            );
            (first, divider, second)
        },
    }
}

/// Pick the pane whose near edge is closest beyond `from`'s edge in `dir`,
/// requiring overlap on the perpendicular axis; ties break on larger overlap.
pub fn neighbor(
    rects: &HashMap<PaneId, Rect>,
    from: PaneId,
    dir: Dir,
) -> Option<PaneId> {
    let cur = *rects.get(&from)?;
    let overlap = |a_min: f32, a_max: f32, b_min: f32, b_max: f32| {
        (a_max.min(b_max) - a_min.max(b_min)).max(0.0)
    };

    let mut best: Option<(PaneId, f32, f32)> = None;
    for (&id, &r) in rects {
        if id == from {
            continue;
        }
        let (dist, ov) = match dir {
            Dir::Left => (
                cur.min.x - r.max.x,
                overlap(cur.min.y, cur.max.y, r.min.y, r.max.y),
            ),
            Dir::Right => (
                r.min.x - cur.max.x,
                overlap(cur.min.y, cur.max.y, r.min.y, r.max.y),
            ),
            Dir::Up => (
                cur.min.y - r.max.y,
                overlap(cur.min.x, cur.max.x, r.min.x, r.max.x),
            ),
            Dir::Down => (
                r.min.y - cur.max.y,
                overlap(cur.min.x, cur.max.x, r.min.x, r.max.x),
            ),
        };
        if dist < -1.0 || ov <= 0.0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, best_dist, best_ov)) => {
                dist < best_dist - 0.5
                    || ((dist - best_dist).abs() <= 0.5 && ov > best_ov)
            },
        };
        if better {
            best = Some((id, dist, ov));
        }
    }
    best.map(|(id, _, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    #[test]
    fn split_and_leaves() {
        let mut tree = Node::Leaf(PaneId(1));
        assert!(tree.split(PaneId(1), SplitAxis::SideBySide, PaneId(2)));
        assert!(tree.split(PaneId(2), SplitAxis::Stacked, PaneId(3)));
        assert!(!tree.split(PaneId(99), SplitAxis::Stacked, PaneId(4)));
        assert_eq!(tree.leaves(), vec![PaneId(1), PaneId(2), PaneId(3)]);
    }

    #[test]
    fn split_with_sets_ratio() {
        let mut tree = Node::Leaf(PaneId(1));
        assert!(tree.split_with(PaneId(1), SplitAxis::SideBySide, PaneId(2), 0.7));
        match &tree {
            Node::Split { ratio, .. } => assert_eq!(*ratio, 0.7),
            _ => panic!("expected a split"),
        }
        assert!(!tree.split_with(PaneId(99), SplitAxis::Stacked, PaneId(3), 0.3));
    }

    #[test]
    fn remove_promotes_sibling() {
        let mut tree = Node::Leaf(PaneId(1));
        tree.split(PaneId(1), SplitAxis::SideBySide, PaneId(2));
        tree.split(PaneId(2), SplitAxis::Stacked, PaneId(3));

        match tree.remove(PaneId(2)) {
            Removal::Removed { focus_hint } => {
                assert_eq!(focus_hint, PaneId(3))
            },
            _ => panic!("expected Removed"),
        }
        assert_eq!(tree.leaves(), vec![PaneId(1), PaneId(3)]);

        match tree.remove(PaneId(1)) {
            Removal::Removed { focus_hint } => {
                assert_eq!(focus_hint, PaneId(3))
            },
            _ => panic!("expected Removed"),
        }
        assert_eq!(tree.leaves(), vec![PaneId(3)]);

        assert!(matches!(tree.remove(PaneId(3)), Removal::BecameEmpty));
    }

    #[test]
    fn remove_missing_is_not_found() {
        let mut tree = Node::Leaf(PaneId(1));
        tree.split(PaneId(1), SplitAxis::SideBySide, PaneId(2));
        assert!(matches!(tree.remove(PaneId(42)), Removal::NotFound));
        assert_eq!(tree.leaves(), vec![PaneId(1), PaneId(2)]);
    }

    #[test]
    fn split_rect_side_by_side() {
        let (a, div, b) =
            split_rect(rect(0.0, 0.0, 104.0, 50.0), SplitAxis::SideBySide, 0.5, 4.0);
        assert_eq!(a, rect(0.0, 0.0, 50.0, 50.0));
        assert_eq!(div, rect(50.0, 0.0, 4.0, 50.0));
        assert_eq!(b, rect(54.0, 0.0, 50.0, 50.0));
    }

    #[test]
    fn neighbor_directional() {
        // +---+---+
        // | 1 | 2 |
        // +---+---+
        // |   3   |
        // +-------+
        let mut rects = HashMap::new();
        rects.insert(PaneId(1), rect(0.0, 0.0, 50.0, 50.0));
        rects.insert(PaneId(2), rect(54.0, 0.0, 50.0, 50.0));
        rects.insert(PaneId(3), rect(0.0, 54.0, 104.0, 50.0));

        assert_eq!(neighbor(&rects, PaneId(1), Dir::Right), Some(PaneId(2)));
        assert_eq!(neighbor(&rects, PaneId(2), Dir::Left), Some(PaneId(1)));
        assert_eq!(neighbor(&rects, PaneId(1), Dir::Down), Some(PaneId(3)));
        // 1 and 2 tie going up from 3; either is acceptable.
        let up = neighbor(&rects, PaneId(3), Dir::Up);
        assert!(up == Some(PaneId(1)) || up == Some(PaneId(2)));
        assert_eq!(neighbor(&rects, PaneId(1), Dir::Left), None);
        assert_eq!(neighbor(&rects, PaneId(1), Dir::Up), None);
        assert_eq!(neighbor(&rects, PaneId(2), Dir::Down), Some(PaneId(3)));
    }
}
