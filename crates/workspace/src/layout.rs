//! FORK: dynamic workspace layout (Stage 2 — see plans/agent-in-center.md).
//!
//! Replaces the hard-coded `match bottom_dock_layout { … }` render topology
//! with a declarative, recursive [`LayoutNode`] tree. The three dock entities
//! and the center pane group are still the region *contents*; the tree only
//! decides where each renders and how they nest.
//!
//! Stage 2 is **behavior-neutral**: [`default_layout_node`] builds a tree that
//! reproduces today's layout for every `BottomDockLayout` variant, so nothing
//! visibly changes. Later stages let the tree diverge (free placement) and
//! persist it.

use gpui::{AnyElement, Axis, div, prelude::*};

use crate::BottomDockLayout;
use crate::dock::DockPosition;

/// A leaf region of the workspace layout: the center pane group, or one of the
/// three panel docks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutRegion {
    Center,
    Dock(DockPosition),
}

/// A node in the workspace layout tree: either a leaf region or a split that
/// arranges child nodes along an axis.
#[derive(Clone, Debug)]
pub enum LayoutNode {
    Leaf(LayoutRegion),
    Split {
        axis: Axis,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    fn leaf(region: LayoutRegion) -> Self {
        LayoutNode::Leaf(region)
    }

    fn split(axis: Axis, children: Vec<LayoutNode>) -> Self {
        LayoutNode::Split { axis, children }
    }

    /// All regions currently present in this subtree (depth-first).
    pub fn regions(&self) -> Vec<LayoutRegion> {
        let mut out = Vec::new();
        self.collect_regions(&mut out);
        out
    }

    fn collect_regions(&self, out: &mut Vec<LayoutRegion>) {
        match self {
            LayoutNode::Leaf(region) => out.push(*region),
            LayoutNode::Split { children, .. } => {
                for child in children {
                    child.collect_regions(out);
                }
            }
        }
    }

    /// Remove `region` from this subtree, returning the resulting node (or
    /// `None` if the subtree was solely that region). Splits left with a single
    /// child collapse into that child.
    fn without_region(self, region: LayoutRegion) -> Option<LayoutNode> {
        match self {
            LayoutNode::Leaf(r) if r == region => None,
            leaf @ LayoutNode::Leaf(_) => Some(leaf),
            LayoutNode::Split { axis, children } => {
                let kept: Vec<LayoutNode> = children
                    .into_iter()
                    .filter_map(|c| c.without_region(region))
                    .collect();
                match kept.len() {
                    0 => None,
                    1 => Some(kept.into_iter().next().unwrap()),
                    _ => Some(LayoutNode::Split { axis, children: kept }),
                }
            }
        }
    }
}

/// The four edges a region can be moved toward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveDirection {
    Left,
    Right,
    Up,
    Down,
}

impl MoveDirection {
    fn axis(self) -> Axis {
        match self {
            MoveDirection::Left | MoveDirection::Right => Axis::Horizontal,
            MoveDirection::Up | MoveDirection::Down => Axis::Vertical,
        }
    }

    /// True if the region should be inserted *before* the rest along the axis
    /// (Left/Up), false if *after* (Right/Down).
    fn inserts_first(self) -> bool {
        matches!(self, MoveDirection::Left | MoveDirection::Up)
    }
}

/// Move `region` to the given edge of the whole workspace: pull it out of the
/// tree, then wrap the remainder in a split along the move axis with `region`
/// on the leading (Left/Up) or trailing (Right/Down) side. Returns the new root.
///
/// Worked example — the user's "agent in the middle" goal. Default Contained
/// tree is `H[Left, V[Center, Bottom], Right]` with Left=file-tree,
/// Center=editor, Right=agent dock. "Move Center (editor) Right":
///   1. extract Center → remainder `H[Left, Bottom, Right]` (Bottom collapses up)
///   2. wrap trailing → `H[ H[Left, Bottom, Right], Center ]`
///   3. normalize merges the nested H → `H[Left, Bottom, Right, Center]`
/// With the terminal closed that reads left-to-right as
/// `[file-tree | agent | editor]` — exactly the requested layout, in one move.
pub fn move_region(root: LayoutNode, region: LayoutRegion, direction: MoveDirection) -> LayoutNode {
    let Some(remainder) = root.clone().without_region(region) else {
        // Region was the only thing in the tree; nothing to move against.
        return root;
    };
    let moved = LayoutNode::leaf(region);
    let children = if direction.inserts_first() {
        vec![moved, remainder]
    } else {
        vec![remainder, moved]
    };
    normalize(LayoutNode::split(direction.axis(), children))
}

/// Flatten redundant structure: a split with one child becomes that child, and
/// a split whose child is a split on the same axis is merged into the parent.
/// Keeps the tree canonical so repeated moves don't nest endlessly.
pub fn normalize(node: LayoutNode) -> LayoutNode {
    match node {
        leaf @ LayoutNode::Leaf(_) => leaf,
        LayoutNode::Split { axis, children } => {
            let mut flat: Vec<LayoutNode> = Vec::new();
            for child in children {
                match normalize(child) {
                    LayoutNode::Split {
                        axis: child_axis,
                        children: grandchildren,
                    } if child_axis == axis => flat.extend(grandchildren),
                    other => flat.push(other),
                }
            }
            if flat.len() == 1 {
                flat.into_iter().next().unwrap()
            } else {
                LayoutNode::Split { axis, children: flat }
            }
        }
    }
}

/// The region elements, each rendered once by `Workspace::render` and consumed
/// (via `take`) as the tree is assembled. A region appears at most once in any
/// tree, so taking is safe.
pub struct RenderedRegions {
    pub center: Option<AnyElement>,
    pub left: Option<AnyElement>,
    pub right: Option<AnyElement>,
    pub bottom: Option<AnyElement>,
}

impl RenderedRegions {
    fn take_dock(&mut self, position: DockPosition) -> Option<AnyElement> {
        match position {
            DockPosition::Left => self.left.take(),
            DockPosition::Right => self.right.take(),
            DockPosition::Bottom => self.bottom.take(),
        }
    }
}

/// Build the layout tree that reproduces today's hard-coded topology for the
/// given `bottom_dock_layout`. Mirrors the four arms of the original
/// `Workspace::render` match exactly:
///
/// - `Contained`:   row[ Left, col[ Center, Bottom ], Right ]
/// - `Full`:        col[ row[ Left, Center, Right ], Bottom ]
/// - `LeftAligned`: row[ col[ row[ Left, Center ], Bottom ], Right ]
/// - `RightAligned`:row[ Left, col[ row[ Center, Right ], Bottom ] ]
pub fn default_layout_node(bottom_dock_layout: BottomDockLayout) -> LayoutNode {
    use Axis::{Horizontal, Vertical};
    use LayoutRegion::{Center, Dock};

    let left = || LayoutNode::leaf(Dock(DockPosition::Left));
    let right = || LayoutNode::leaf(Dock(DockPosition::Right));
    let bottom = || LayoutNode::leaf(Dock(DockPosition::Bottom));
    let center = || LayoutNode::leaf(Center);

    match bottom_dock_layout {
        BottomDockLayout::Contained => LayoutNode::split(
            Horizontal,
            vec![
                left(),
                LayoutNode::split(Vertical, vec![center(), bottom()]),
                right(),
            ],
        ),
        BottomDockLayout::Full => LayoutNode::split(
            Vertical,
            vec![
                LayoutNode::split(Horizontal, vec![left(), center(), right()]),
                bottom(),
            ],
        ),
        BottomDockLayout::LeftAligned => LayoutNode::split(
            Horizontal,
            vec![
                LayoutNode::split(
                    Vertical,
                    vec![
                        LayoutNode::split(Horizontal, vec![left(), center()]),
                        bottom(),
                    ],
                ),
                right(),
            ],
        ),
        BottomDockLayout::RightAligned => LayoutNode::split(
            Horizontal,
            vec![
                left(),
                LayoutNode::split(
                    Vertical,
                    vec![
                        LayoutNode::split(Horizontal, vec![center(), right()]),
                        bottom(),
                    ],
                ),
            ],
        ),
    }
}

/// Recursively assemble the layout tree into an element, consuming the
/// pre-rendered regions. `parent_axis` is the axis of the enclosing split (used
/// to decide how the center leaf wraps); `is_root` selects the root sizing
/// (`h_full`) vs nested sizing (`flex_1` + `overflow_hidden`).
pub fn assemble_layout(
    node: &LayoutNode,
    regions: &mut RenderedRegions,
    parent_axis: Option<Axis>,
    is_root: bool,
) -> AnyElement {
    match node {
        LayoutNode::Leaf(LayoutRegion::Center) => {
            let center = regions
                .center
                .take()
                .unwrap_or_else(|| div().into_any_element());
            // The pre-rendered center is an `h_flex().flex_1()` row. When it
            // sits directly in a horizontal split it needs a flex-column
            // wrapper so it fills height (matches the original "center column");
            // inside a vertical split the split container already is that
            // column, so the row is added directly.
            match parent_axis {
                Some(Axis::Vertical) => center,
                _ => div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(center)
                    .into_any_element(),
            }
        }
        LayoutNode::Leaf(LayoutRegion::Dock(position)) => regions
            .take_dock(*position)
            .unwrap_or_else(|| div().into_any_element()),
        LayoutNode::Split { axis, children } => {
            let mut container = div().flex();
            container = match axis {
                Axis::Horizontal => container.flex_row(),
                Axis::Vertical => container.flex_col(),
            };
            container = if is_root {
                container.h_full()
            } else {
                container.flex_1().overflow_hidden()
            };
            for child in children {
                container = container.child(assemble_layout(child, regions, Some(*axis), false));
            }
            container.into_any_element()
        }
    }
}
