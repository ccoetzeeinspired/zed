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
//!
//! Stage 3b adds **generic flex resize**: every [`LayoutNode::Split`] carries a
//! shared per-child flex vector, and on the custom (relocated) layout path the
//! split is rendered by [`resize::RegionAxisElement`], which sizes its children
//! by proportion of their flex weights and draws a draggable handle between
//! sibling regions. The default path keeps the plain flex `div`s untouched.

use gpui::{AnyElement, Axis, div, prelude::*};
use parking_lot::Mutex;
use std::sync::Arc;

use crate::BottomDockLayout;
use crate::dock::DockPosition;

/// Basis offset for the [`resize::RegionAxisElement`] ids so they can never
/// collide with the center pane group's own `PaneAxisElement` basis values.
const REGION_BASIS_OFFSET: usize = 1_000_000;

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
        /// Per-child flex weights (mirrors `PaneAxis.flexes`). A present child's
        /// main-axis size is `container * flex[i] / sum(flex of present
        /// children)`, so the weights are relative proportions; closed docks are
        /// simply omitted from that sum. Shared via `Arc<Mutex<_>>` so the
        /// render-time [`resize::RegionAxisElement`] mutates the *same* vector the
        /// tree holds (a `clone()` of the tree shares the `Arc`), letting a drag
        /// survive to the next frame with just a `cx.notify()`. Length always
        /// equals `children.len()` and is reset to equal on any topology change
        /// (see [`LayoutNode::split`]). Serialization is deferred to Stage 5.
        flexes: Arc<Mutex<Vec<f32>>>,
    },
}

impl LayoutNode {
    fn leaf(region: LayoutRegion) -> Self {
        LayoutNode::Leaf(region)
    }

    /// Build a split, seeding its flex vector to equal weights. All split
    /// construction routes through here, so any topology change (move, collapse,
    /// merge) resets weights to equal — predictable and keeps `flexes.len()`
    /// in lockstep with `children.len()`.
    fn split(axis: Axis, children: Vec<LayoutNode>) -> Self {
        let flexes = Arc::new(Mutex::new(vec![1.; children.len()]));
        LayoutNode::Split {
            axis,
            children,
            flexes,
        }
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
            LayoutNode::Split { axis, children, .. } => {
                let kept: Vec<LayoutNode> = children
                    .into_iter()
                    .filter_map(|c| c.without_region(region))
                    .collect();
                match kept.len() {
                    0 => None,
                    1 => Some(kept.into_iter().next().unwrap()),
                    // Rebuild via `split` so the surviving children get a fresh,
                    // correctly-sized flex vector.
                    _ => Some(LayoutNode::split(axis, kept)),
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
        LayoutNode::Split { axis, children, .. } => {
            let mut flat: Vec<LayoutNode> = Vec::new();
            for child in children {
                match normalize(child) {
                    LayoutNode::Split {
                        axis: child_axis,
                        children: grandchildren,
                        ..
                    } if child_axis == axis => flat.extend(grandchildren),
                    other => flat.push(other),
                }
            }
            if flat.len() == 1 {
                flat.into_iter().next().unwrap()
            } else {
                // Rebuild via `split` so the merged child set gets a fresh,
                // correctly-sized flex vector.
                LayoutNode::split(axis, flat)
            }
        }
    }
}

/// The region elements, each rendered once by `Workspace::render` and consumed
/// (via `take`) as the tree is assembled. A region appears at most once in any
/// tree, so taking is safe. A dock that is closed/absent is `None` and is
/// dropped from the assembled layout entirely.
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
/// pre-rendered regions.
///
/// `interactive` selects the render strategy:
/// - `false` (default / Contained path): emit plain flex `div`s exactly as
///   Stage 2 did — every child `flex_1`, no resize handles. Pixel-identical to
///   the original layout (acceptance criterion #3).
/// - `true` (custom / relocated path): emit a [`resize::RegionAxisElement`] per
///   split so siblings are sized by the split's flex vector and a draggable
///   handle sits between each pair; absent (closed-dock) regions are dropped.
pub fn assemble_layout(
    node: &LayoutNode,
    regions: &mut RenderedRegions,
    interactive: bool,
) -> AnyElement {
    if interactive {
        let mut basis = 0usize;
        assemble_interactive(node, regions, &mut basis)
            .unwrap_or_else(|| div().into_any_element())
    } else {
        assemble_default(node, regions, None, true)
    }
}

/// Stage 2 assembler — unchanged. `parent_axis` is the axis of the enclosing
/// split (decides how the center leaf wraps); `is_root` selects root sizing
/// (`h_full`) vs nested sizing (`flex_1` + `overflow_hidden`).
fn assemble_default(
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
        LayoutNode::Split { axis, children, .. } => {
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
                container =
                    container.child(assemble_default(child, regions, Some(*axis), false));
            }
            container.into_any_element()
        }
    }
}

/// Stage 3b interactive assembler. Returns `None` when a subtree has nothing to
/// show (e.g. a split all of whose docks are closed) so it contributes no flex
/// slot. `basis` is a depth-first pre-order counter giving each split a stable
/// element id across frames (stable while topology is unchanged — i.e. during a
/// drag, and across dock open/close, which don't change the tree).
fn assemble_interactive(
    node: &LayoutNode,
    regions: &mut RenderedRegions,
    basis: &mut usize,
) -> Option<AnyElement> {
    match node {
        LayoutNode::Leaf(LayoutRegion::Center) => {
            let center = regions.center.take()?;
            // The parent `RegionAxisElement` sizes this leaf via `layout_as_root`;
            // fill that slot rather than relying on flex.
            Some(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .overflow_hidden()
                    .child(center)
                    .into_any_element(),
            )
        }
        // The dock element (from `render_dock_for_layout`) is already `size_full`;
        // a closed dock is `None` and drops out of the split.
        LayoutNode::Leaf(LayoutRegion::Dock(position)) => regions.take_dock(*position),
        LayoutNode::Split {
            axis,
            children,
            flexes,
        } => {
            let my_basis = REGION_BASIS_OFFSET + *basis;
            *basis += 1;
            // Assemble children in tree order, keeping `None` for absent ones so
            // each stays aligned with its weight in `flexes`.
            let child_opts: Vec<Option<AnyElement>> = children
                .iter()
                .map(|child| assemble_interactive(child, regions, basis))
                .collect();
            let present = child_opts.iter().filter(|c| c.is_some()).count();
            match present {
                0 => None,
                // A lone present child needs no axis/handle — render it directly.
                1 => child_opts.into_iter().flatten().next(),
                _ => Some(
                    resize::region_axis(my_basis, *axis, flexes.clone(), child_opts)
                        .into_any_element(),
                ),
            }
        }
    }
}

/// FORK Stage 3b: a resizable axis element for the top-level workspace layout
/// tree. Inspired by `pane_group::element::PaneAxisElement` but for whole
/// regions (docks + center): lays its present children out proportionally from a
/// shared flex vector (`size = container * flex[i] / sum(present flexes)`) and
/// draws a draggable handle between each adjacent pair. None of the pane-specific
/// cruft (active-pane overlays, `bounding_boxes`, workspace serialization) is
/// carried over — region flex state is in-memory only until Stage 5.
mod resize {
    use std::cell::RefCell;
    use std::mem;
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui::{
        Along, AnyElement, App, Axis, Bounds, CursorStyle, Element, ElementId, GlobalElementId,
        Hitbox, HitboxBehavior, IntoElement, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
        Point, Size, Style, Window, px, relative, size,
    };
    use parking_lot::Mutex;
    use ui::prelude::*;

    const HANDLE_HITBOX_SIZE: f32 = 4.0;
    const DIVIDER_SIZE: f32 = 1.0;
    const HORIZONTAL_MIN_SIZE: f32 = 80.;
    const VERTICAL_MIN_SIZE: f32 = 100.;

    /// Construct a `RegionAxisElement`. `children` is aligned 1:1 with the
    /// split's tree children (and with `flexes`); `None` entries are absent
    /// regions (closed docks) and are skipped during layout.
    pub(super) fn region_axis(
        basis: usize,
        axis: Axis,
        flexes: Arc<Mutex<Vec<f32>>>,
        children: Vec<Option<AnyElement>>,
    ) -> RegionAxisElement {
        RegionAxisElement {
            basis,
            axis,
            flexes,
            children,
        }
    }

    pub(super) struct RegionAxisElement {
        basis: usize,
        axis: Axis,
        flexes: Arc<Mutex<Vec<f32>>>,
        children: Vec<Option<AnyElement>>,
    }

    pub(super) struct RegionAxisLayout {
        /// Full index (into `flexes`) of the left child of the boundary currently
        /// being dragged, if any.
        dragged_handle: Rc<RefCell<Option<usize>>>,
        children: Vec<RegionChildLayout>,
    }

    struct RegionChildLayout {
        element: AnyElement,
        /// Trailing-edge resize handle to the next *present* sibling, if any.
        handle: Option<RegionHandleLayout>,
    }

    struct RegionHandleLayout {
        hitbox: Hitbox,
        divider_bounds: Bounds<Pixels>,
        /// Full indices into `flexes` of the two regions this boundary resizes.
        a: usize,
        b: usize,
        /// Origin of region `a` (captured at prepaint; drag is computed from the
        /// absolute mouse position relative to it).
        a_origin: Point<Pixels>,
        /// Sum of the flex weights of the present children this frame, so a drag
        /// can map pixels back into the shared (full-length) flex vector.
        present_total: f32,
    }

    impl RegionAxisElement {
        /// Convert a drag (absolute mouse position vs. region `a`'s origin) into
        /// updated flexes for the two adjacent regions: zero-sum, clamped so
        /// neither falls below the per-axis minimum.
        #[allow(clippy::too_many_arguments)]
        fn compute_resize(
            flexes: &Arc<Mutex<Vec<f32>>>,
            e: &MouseMoveEvent,
            a: usize,
            b: usize,
            a_origin: Point<Pixels>,
            axis: Axis,
            container_size: Size<Pixels>,
            present_total: f32,
            window: &mut Window,
            cx: &mut App,
        ) {
            let main = container_size.along(axis);
            if main <= px(0.) || present_total <= 0. {
                return;
            }
            let min = match axis {
                Axis::Horizontal => px(HORIZONTAL_MIN_SIZE),
                Axis::Vertical => px(VERTICAL_MIN_SIZE),
            };
            let mut flexes = flexes.lock();
            if a >= flexes.len() || b >= flexes.len() {
                return;
            }

            let a_size = main * (flexes[a] / present_total);
            let b_size = main * (flexes[b] / present_total);
            let combined = a_size + b_size;
            if combined <= min * 2. {
                return;
            }

            let raw = (e.position - a_origin).along(axis);
            let upper = combined - min;
            let target_a = if raw < min {
                min
            } else if raw > upper {
                upper
            } else {
                raw
            };

            let new_flex_a = (target_a / main) * present_total;
            let delta = new_flex_a - flexes[a];
            flexes[a] = new_flex_a;
            flexes[b] -= delta;

            cx.stop_propagation();
            window.refresh();
        }

        /// Build the hitbox + 1px divider straddling `region_bounds`' trailing
        /// edge along `axis`.
        fn layout_handle(
            axis: Axis,
            region_bounds: Bounds<Pixels>,
            window: &mut Window,
        ) -> (Hitbox, Bounds<Pixels>) {
            let handle_bounds = Bounds {
                origin: region_bounds.origin.apply_along(axis, |origin| {
                    origin + region_bounds.size.along(axis) - px(HANDLE_HITBOX_SIZE / 2.)
                }),
                size: region_bounds
                    .size
                    .apply_along(axis, |_| px(HANDLE_HITBOX_SIZE)),
            };
            let divider_bounds = Bounds {
                origin: region_bounds
                    .origin
                    .apply_along(axis, |origin| origin + region_bounds.size.along(axis)),
                size: region_bounds.size.apply_along(axis, |_| px(DIVIDER_SIZE)),
            };
            (
                window.insert_hitbox(handle_bounds, HitboxBehavior::BlockMouse),
                divider_bounds,
            )
        }
    }

    impl IntoElement for RegionAxisElement {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for RegionAxisElement {
        type RequestLayoutState = ();
        type PrepaintState = RegionAxisLayout;

        fn id(&self) -> Option<ElementId> {
            Some(self.basis.into())
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _global_id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (gpui::LayoutId, Self::RequestLayoutState) {
            let style = Style {
                flex_grow: 1.,
                flex_shrink: 1.,
                flex_basis: relative(0.).into(),
                size: size(relative(1.).into(), relative(1.).into()),
                ..Style::default()
            };
            (window.request_layout(style, None, cx), ())
        }

        fn prepaint(
            &mut self,
            global_id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            bounds: Bounds<Pixels>,
            _state: &mut Self::RequestLayoutState,
            window: &mut Window,
            cx: &mut App,
        ) -> RegionAxisLayout {
            let dragged_handle = window.with_element_state::<Rc<RefCell<Option<usize>>>, _>(
                global_id.unwrap(),
                |state, _cx| {
                    let state = state.unwrap_or_else(|| Rc::new(RefCell::new(None)));
                    (state.clone(), state)
                },
            );

            let flexes = self.flexes.lock().clone();
            let main = bounds.size.along(self.axis);

            // Sum the flex weights of the present children (closed docks omitted),
            // so each present child occupies `flex[i] / present_total` of `main`.
            let present_total = {
                let total: f32 = self
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.is_some())
                    .map(|(i, _)| flexes.get(i).copied().unwrap_or(1.0))
                    .sum();
                if total > 0. { total } else { 1.0 }
            };

            let mut origin = bounds.origin;
            // (full_index, bounds) per present child, in render order.
            let mut placed: Vec<(usize, Bounds<Pixels>)> = Vec::new();
            let mut layout = RegionAxisLayout {
                dragged_handle,
                children: Vec::new(),
            };

            for (i, child_opt) in mem::take(&mut self.children).into_iter().enumerate() {
                let Some(mut child) = child_opt else {
                    continue;
                };
                let flex = flexes.get(i).copied().unwrap_or(1.0);
                let child_main = main * (flex / present_total);
                let child_size = bounds
                    .size
                    .apply_along(self.axis, |_| child_main)
                    .map(|d| d.round());
                let child_bounds = Bounds {
                    origin,
                    size: child_size,
                };

                child.layout_as_root(child_size.into(), window, cx);
                child.prepaint_at(origin, window, cx);

                origin = origin.apply_along(self.axis, |val| val + child_size.along(self.axis));

                placed.push((i, child_bounds));
                layout.children.push(RegionChildLayout {
                    element: child,
                    handle: None,
                });
            }

            // A handle on each present child except the last, resizing it against
            // the next present sibling.
            for order in 0..placed.len().saturating_sub(1) {
                let (a, a_bounds) = placed[order];
                let (b, _) = placed[order + 1];
                let (hitbox, divider_bounds) = Self::layout_handle(self.axis, a_bounds, window);
                layout.children[order].handle = Some(RegionHandleLayout {
                    hitbox,
                    divider_bounds,
                    a,
                    b,
                    a_origin: a_bounds.origin,
                    present_total,
                });
            }

            layout
        }

        fn paint(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&gpui::InspectorElementId>,
            bounds: Bounds<Pixels>,
            _: &mut Self::RequestLayoutState,
            layout: &mut Self::PrepaintState,
            window: &mut Window,
            cx: &mut App,
        ) {
            for child in &mut layout.children {
                child.element.paint(window, cx);
            }

            let container_size = bounds.size;
            let axis = self.axis;

            for child in layout.children.iter_mut() {
                let Some(handle) = child.handle.as_ref() else {
                    continue;
                };
                let a = handle.a;
                let b = handle.b;
                let a_origin = handle.a_origin;
                let present_total = handle.present_total;

                let cursor_style = match axis {
                    Axis::Vertical => CursorStyle::ResizeRow,
                    Axis::Horizontal => CursorStyle::ResizeColumn,
                };
                if layout
                    .dragged_handle
                    .borrow()
                    .is_some_and(|dragged_a| dragged_a == a)
                {
                    window.set_window_cursor_style(cursor_style);
                } else {
                    window.set_cursor_style(cursor_style, &handle.hitbox);
                }

                window.paint_quad(gpui::fill(
                    handle.divider_bounds,
                    cx.theme().colors().pane_group_border,
                ));

                window.on_mouse_event({
                    let dragged_handle = layout.dragged_handle.clone();
                    let flexes = self.flexes.clone();
                    let handle_hitbox = handle.hitbox.clone();
                    move |e: &MouseDownEvent, phase, window, cx| {
                        if phase.bubble() && handle_hitbox.is_hovered(window) {
                            dragged_handle.replace(Some(a));
                            // Double-click resets the split to equal weights.
                            if e.click_count >= 2 {
                                let mut borrow = flexes.lock();
                                *borrow = vec![1.; borrow.len()];
                                window.refresh();
                            }
                            cx.stop_propagation();
                        }
                    }
                });

                window.on_mouse_event({
                    let dragged_handle = layout.dragged_handle.clone();
                    let flexes = self.flexes.clone();
                    move |e: &MouseMoveEvent, phase, window, cx| {
                        if phase.bubble() && *dragged_handle.borrow() == Some(a) {
                            Self::compute_resize(
                                &flexes,
                                e,
                                a,
                                b,
                                a_origin,
                                axis,
                                container_size,
                                present_total,
                                window,
                                cx,
                            );
                        }
                    }
                });
            }

            window.on_mouse_event({
                let dragged_handle = layout.dragged_handle.clone();
                move |_: &MouseUpEvent, phase, _window, _cx| {
                    if phase.bubble() {
                        dragged_handle.replace(None);
                    }
                }
            });
        }
    }
}
