# Plan: Dynamic workspace layout (free regions + agent in the center)

Status: **Approved scope: full arbitrary-layout rewrite.** Branch: `browser-viewer`.
This is a multi-stage epic; each stage ships compiling, runnable, live-verified.

## Goal

Let the user place the **four main regions** — center editor/browser pane group,
and the three panel docks (file tree, thread list, agent conversation, terminal,
etc.) — in **any arrangement**: any region to any edge, regions swapped, the
agent conversation in the literal center, the editor pushed to a side. Driving
need: the agent is the primary surface; the layout must adapt to the user's feel
day to day.

## Two independent capabilities (built together, useful separately)

1. **Agent (and any view) in the center** — host the agent conversation as a
   center-pane `Item` tab, the same way this fork already hosts the browser and
   PDF viewers. Self-contained; ships first (Stage 1).
2. **Free region placement** — a declarative layout engine that replaces Zed's
   hard-coded `[left | center+bottom | right]` topology with an arrangeable
   tree. The big rewrite (Stages 2–5).

## Architecture

### Capability 1 — Agent conversation as a center Item
`ConversationView` (`crates/agent_ui/src/conversation_view.rs:527`) already holds
all its state and renders pure GPUI; `AgentPanel` owns it in
`retained_threads: HashMap<ThreadId, Entity<ConversationView>>`. Add
`ConversationItem` (new file `crates/agent_ui/src/conversation_item.rs`) wrapping
the **same** `Entity<ConversationView>`, implementing `Item` (template:
`BrowserView` at `browser_view.rs:1543`). Open via
`workspace.add_item_to_active_pane()` (`workspace.rs:4510`). Same entity ⇒ dock
view and center tab stay in sync via GPUI broadcast. `AgentPanel` stays intact
(owner, thread-list, dispatch target) ⇒ nothing existing breaks. No Cutout.

### Capability 2 — The layout engine (`WorkspaceLayout`)
Replace the fixed `Workspace { center, left_dock, right_dock, bottom_dock }`
*render topology* (not the fields) with a declarative, recursive tree:

```
enum Slot { Center, Dock(DockId) }            // DockId = the 3 existing dock entities
enum LayoutNode {
    Leaf(Slot),
    Split { axis: Axis, children: Vec<(LayoutNode, /*flex*/ f32)> },
}
struct WorkspaceLayout { root: LayoutNode }    // lives on Workspace; default == today
```

- **Keep the 3 dock entities + center PaneGroup** as the region contents. The
  layout tree only decides *where each renders and how big*. Panels still live in
  docks; the center still splits internally via its own `PaneGroup`.
- **Render** (`workspace.rs` Render impl, ~8412): walk `WorkspaceLayout.root`,
  emitting a flex container per `Split` and rendering the dock/center per `Leaf`.
  This replaces the `BottomDockLayout` match. Isolated in a new
  `workspace::layout` module; the Render impl calls one entry point.
- **Resize**: generic — a drag handle between two `Split` siblings adjusts their
  flexes (reuse the `PaneAxis` flex/bounding-box approach from `pane_group.rs`).
  Position-agnostic; replaces `resize_left_dock/right/bottom`.
- **Dock position semantics**: `DockPosition`/`position_is_valid` decouples from
  "screen edge" → the layout tree owns placement. Reconciled in Stage 2.
- **Persistence**: serialize `WorkspaceLayout` (new column/table); migrate old
  workspaces (no layout ⇒ build the default tree from existing dock positions).
- **Drag-to-rearrange UX**: drag a region header to an edge/center drop-zone of
  another region → re-parent the node in the tree. Stage 4.

## Stages (each: compiles, runs, live-verified, committed)

- **Stage 1 — Agent in center.** `ConversationItem` + `agent: open in center`
  action + header button. Ships the headline feature alone. Low risk, isolated.
- **Stage 2 — Layout engine, behavior-neutral.** Introduce `WorkspaceLayout` +
  the recursive renderer + generic resize, with the **default tree reproducing
  today's layout exactly**. Goal: zero visible change, engine in place. Riskiest
  stage (touches the Render impl) — verify pixel-parity with the screenshot tool.
- **Stage 3 — Region placement control.** Actions/commands to move a region to
  any edge or into the center axis (e.g., "Move dock to top", "Swap regions").
  Position-aware resize. Verify each arrangement live.
- **Stage 4 — Drag-to-rearrange.** Drop-zone overlays + drag preview; mutate the
  tree on drop. The polished UX.
- **Stage 5 — Persistence + migration.** Serialize the layout tree; migrate old
  workspaces; restore on restart. (Agent-in-center `SerializableItem` here too.)

## Conflict-surface strategy (this is the expensive part)

`workspace.rs` Render (8412–8852) is the most upstream-churned, load-bearing code
in the tree; a free-layout rewrite there causes **semantic** (not textual) merge
conflicts every sync that `// FORK:` markers can't resolve.

- Put the layout tree, renderer, and resize in a **new module**
  `crates/workspace/src/layout.rs`; the Render impl calls a single
  `self.render_layout(window, cx)` entry point so the inline diff in the churned
  render fn is ~1 line.
- Stage 1 is fully isolated (new agent_ui file + 1-line action/button).
- Accept that Stages 2–5 are a **permanent, growing fork diff** in `workspace.rs`
  / `pane_group.rs` / `persistence.rs`. Document every touch point here and in
  CLAUDE.md's conflict-surface section so rebases are mechanical.

## Risks & mitigations

- **Broken editor mid-rewrite** → strict staging; default tree == today's layout
  in Stage 2; live-verify every stage before committing.
- **Resize/axis assumptions** in `pane_group.rs` (center assumes h_flex context)
  → Stage 2 makes split direction read from the node, not ambient context.
- **Persistence migration** → versioned; absent layout falls back to default tree
  built from current dock positions.
- **Design-bundle dispatch (Phase 4.F)** still targets the dock `AgentPanel`,
  which stays alive → unaffected; re-verify in Stage 1.
- **Rebase tax** → isolation module + meticulous CLAUDE.md notes.

## Acceptance criteria (whole epic)

1. Agent conversation opens as a center tab, synced with the dock view; closing
   the tab keeps the thread.
2. Any of the 4 regions can be placed at any edge or center, resized, and the
   arrangement persists across restart.
3. The default (fresh / migrated) layout is identical to today's.
4. Design-mode → agent dispatch still works.
5. Each stage builds (`-j 4` from `D:\src\zed`) and is verified live before "done."

## Stage 3b — generic flex resize (drag handles for relocated regions)

Status: in progress (resuming after Stage 3a; HEAD f0f1d22). Recorded here because the
detailed design previously lived only in a session memory note, never on disk.

The gap: Stage 3a's `MoveRegion*` relocate regions, but their boundaries can't be
dragged. `layout.rs` emits each `Split` as a bare `div().flex()` whose children are all
`.flex_1()` (locked 1/N) with **no handle between siblings** (`layout.rs:279-294`). The
only working resize is each dock's own edge handle, which still measures against the
dock's *original* screen edge (`workspace.rs:8672-8682`; `resize_*_dock` at `2470-2499`) —
hence "funky" once a dock is relocated. NOTE: `LayoutNode::Split` currently carries only
`{ axis, children: Vec<LayoutNode> }` — there is **no** flex/weight field yet (the
"per-Split flex vector" is what this stage adds), and there is no `Slot` type
(`LayoutRegion { Center, Dock(DockPosition) }`).

Approach: mirror the center pane group's `PaneAxisElement` for the top-level region
splits. Reference anchors (`crates/workspace/src/pane_group.rs`): `HANDLE_HITBOX_SIZE = 4.0`
(`:31`), `HORIZONTAL_MIN_SIZE = 80.` / `VERTICAL_MIN_SIZE = 100.` (`:32-33`),
`PaneAxis.flexes: Arc<Mutex<Vec<f32>>>` (`:602`, default `vec![1.; n]`), `PaneAxisElement`
(`:1115`), `flex_changes` (`:1170`), `layout_handle` (`:787`).

Design:
- **Per-Split flex vector.** Add `flexes: Arc<Mutex<Vec<f32>>>` to `LayoutNode::Split`
  (mirrors `PaneAxis.flexes`; default `vec![1.; n]`). `Arc<Mutex>` so the render-time
  element mutates the same vector `Workspace.custom_layout` holds — `custom_layout.clone()`
  shares the Arc, so a drag survives to the next frame with just `cx.notify()`.
  `move_region` resets a split's flexes to equal whenever its child count changes.
  Serialize/Deserialize deferred to Stage 5 (manual impl reading the Vec out of the mutex).
- **`RegionAxisElement`** (new, in `layout.rs`), modeled on `PaneAxisElement`:
  `request_layout` → flex container; `prepaint` reads flexes, sizes each child
  `flex/total * main_axis`, inserts a `HANDLE_HITBOX_SIZE` (4px) `Hitbox` per interior gap;
  `paint` draws the 1px divider, sets the resize cursor, and wires MouseDown/Move/Up.
  Reuse `flex_changes`' zero-sum + per-axis-min-clamp math: `delta/main_size` shifted
  between the two adjacent children, clamped so neither drops below the axis min.
- **Gate to the custom path only.** `assemble_layout` keeps its current bare-`div` output
  for the default (`custom_layout == None`) path, so Stage 2's verified pixel-parity is
  untouched (acceptance #3). The interactive `RegionAxisElement` + handles are emitted only
  when `custom_layout.is_some()` (render branch `workspace.rs:8689-8696`).
- **Suppress the dock's built-in resize handle in custom layouts.** `render_dock`
  (`workspace.rs:2425-2468`) bakes the dock's own edge handle; pass a flag to omit it in the
  custom path so the generic handle is the sole resize affordance. Default path keeps it.
- **Dock sizing in custom layouts** is driven by the flex weights (the dock renders at its
  flex-allocated bounds, ignoring its stored px width); the default path keeps dock px
  sizing. The center pane group keeps its own independent `PaneAxis` flexes — Stage 3b
  governs only the top-level region splits.

Touch points: `crates/workspace/src/layout.rs` (Split.flexes, `RegionAxisElement`, flex
math, interactive assemble branch) and `crates/workspace/src/workspace.rs`
(`render_layout_node` interactive flag at `:2315-2345`, `render_dock` handle suppression).

Verify live: relocate the agent (Stage 3a action), drag the agent↔editor boundary both
horizontally and vertically, confirm the neighbor is zero-sum and the min-size clamp
holds; confirm the default layout is unchanged and its dock handles still work.

## Stage 4 — drag-to-rearrange (region drag/drop)

Status: in progress (after 3b at `aea77cb`). The polished mouse alternative to the
Stage 3a `MoveRegion*` palette commands: drag a whole region onto another region's
edge/center to re-parent it in the tree. Structural template = pane **tab** drag-to-split
(`pane.rs`: `DraggedTab`, `handle_drag_move` quadrant hit-test at ~3732, `handle_tab_drop`
deferred split/move at ~3788, half-pane overlay at ~4527). GPUI DnD API:
`on_drag<T,W>`/`on_drop<T>`/`on_drag_move<T>` + `DragMoveEvent<T>{event,bounds}` +
`can_drop`; overlays that must catch the drop need `.occlude()`.

**Grab affordance (user choice): Alt + drag the region body** — no chrome. Gated on Alt so
it doesn't hijack normal interaction. KNOWN RISK to verify live: Alt+click is editor
multi-cursor, so the drag source must be **non-occluding** (Alt+click still reaches the
editor); only the *drop-zone* overlays occlude (to catch the mouse-up cleanly — the
`.occlude()` lesson from browser_viewer 4.F).

Split into two commits:
- **4a — layout tree drop ops (pure, unit-tested).** In `layout.rs`: `DropZone{Left,Right,
  Top,Bottom,Center}` (`axis()`/`inserts_before()`); `LayoutNode::replace_leaf` +
  `swap_leaves`; `pub drop_region(root, moved, target, zone)` and `swap_regions(root,a,b)`.
  Edge zones = detach `moved` via `without_region`, replace the `target` leaf with a 2-child
  `split(zone.axis(), …)`, then `normalize` (this resets the affected split's flexes to
  equal — accepted v1, same as 3a). Center = positional swap via `swap_leaves`, which
  mutates leaves in place and **preserves flex weights**. No-ops: `moved==target`, target
  absent, `moved` is the whole tree. `#[cfg(test)]` tests assert topology (a `shape()`
  stringifier) + the `flexes.len()==children.len()` invariant. Verified with
  `cargo test -p workspace`.
- **4b — drag/drop UI.** `RegionBounds = Arc<Mutex<Vec<(LayoutRegion, Bounds<Pixels>)>>>`
  written in `RegionAxisElement::prepaint` (Stage 3b dropped `bounding_boxes`; this re-adds
  per-region rects, keyed by region, for drop hit-testing), threaded through
  `assemble_layout`/`assemble_interactive`/`region_axis`, cleared+passed from
  `render_layout_node`. `DraggedRegion(LayoutRegion)` payload. `Workspace`: `alt_held`
  (via `on_modifiers_changed`), `region_bounds`, `drag_drop_target: Option<(LayoutRegion,
  DropZone)>`. Source: Alt-gated non-occluding `on_drag` wrapper per region (subtle tint
  for discoverability). Drop: an occluding full-size layer active during a `DraggedRegion`
  drag with `on_drag_move::<DraggedRegion>` (hit-test cursor vs `region_bounds` → region →
  5-zone sub-divide via `drop_target_size` band → `DropZone`) + `on_drop::<DraggedRegion>`
  → `drop_region_on(moved,target,zone)`; a `deferred` highlight quad over the candidate
  sub-rect. Single lone-leaf region falls back to `workspace.bounds`.

Then Stage 5 (persistence + migration; serialize the tree incl. per-Split flex vectors).
