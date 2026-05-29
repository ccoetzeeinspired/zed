.rules

# Fork context (read this first)

This is a personal fork of `zed-industries/zed`, hosted at
`https://github.com/ccoetzeeinspired/zed`. The notes below describe how this
fork differs from upstream, why those differences exist, and the workflow for
keeping it in sync. These notes live only on the `pdf-viewer` and
`claude-only` branches so that `main` stays a byte-for-byte mirror of
upstream and never produces sync conflicts on this file.

## Specs and plans (under `plans/`)

Design documents for in-flight or upcoming features live under `plans/`.
Read the relevant plan before starting implementation work on the
corresponding feature.

| Plan                                            | Status        | Branch (future)  |
|-------------------------------------------------|---------------|------------------|
| [`browser-viewer.md`](plans/browser-viewer.md)  | Draft, ready  | `browser-viewer` |

## Remotes

| Remote     | URL                                                   | Role                                    |
|------------|-------------------------------------------------------|-----------------------------------------|
| `origin`   | `https://github.com/ccoetzeeinspired/zed.git`         | Personal fork — push here.              |
| `upstream` | `https://github.com/zed-industries/zed.git`           | Source repo — read-only, never push.    |

## Branches

- **`main`** — tracks `origin/main`, which mirrors `upstream/main`. No local
  modifications. Only used as a rebase base when syncing.
- **`pdf-viewer`** — carries the PDF viewer crate, the msvc_spectre_libs
  build stub, and this CLAUDE.md addition.
- **`claude-only`** — built on top of `pdf-viewer`; gates the agent panel
  to claude-acp and vendors the ACP bridge.
- **`browser-viewer`** — built on top of `claude-only`; adds the in-editor
  WebView2 browser tab with composition-mode rendering, CDP keyboard,
  design-mode element picker + drawing overlay + screenshot bundle,
  and the GPUI scene `Cutout` primitive (see below).

## What this fork adds, and why

### `crates/pdf_viewer/` — in-editor PDF viewer

A v1 "preview now, then fork" native PDF viewer. Upstream Zed has no PDF
support; this fills the gap without dragging in a heavy native PDF dependency.

Approach: pages are rasterized to PNGs via poppler's `pdftoppm` into a
per-file temp cache (`%TEMP%\zed-pdf-viewer\<hash>\`), and rendered as a
vertically scrolling column of `img()` elements. Cache key is
`(path, size, mtime)`, so re-opening is instant.

- `PdfItem` claims `*.pdf` paths via `project::ProjectItem::try_open`. It
  extension-checks the absolute path (not the worktree-relative path) because
  a standalone-opened PDF becomes its own worktree root with an empty
  relative path.
- `PdfView` zoom uses `max_w(relative(zoom))`: 1.0 = fit-to-width, >1.0
  overflows horizontally. Clamped 0.2–6.0, ×1.1 step.
- `pdftoppm` is located by walking `%LOCALAPPDATA%\Microsoft\WinGet\Packages\oschwartz10612.Poppler*\<ver>\Library\bin\`
  first, then falling back to PATH.
- Keybindings (`assets/keymaps/default-windows.json`): `ctrl-=` / `ctrl-+`
  zoom in, `ctrl--` zoom out, `ctrl-0` reset.
- Wired in via `pdf_viewer::init(cx)` in `crates/zed/src/main.rs` and the
  test harness in `crates/zed/src/zed.rs`.

Future direction noted in the module docs: swap the rasterizer for an
in-process `pdfium` renderer with lazy per-page rendering.

### Agent panel — gated to Claude Code only

This fork's `claude-only` branch restricts the in-Zed agent sidebar so it
can only ever route to the Claude Agent (`claude-acp`) external agent,
which in turn uses the user's Claude Code subscription auth. The native
"Zed Agent" path and any other LLM-provider routing are hidden from the
user-facing flow.

The deliberate choice was to **gate, not delete**, the native agent. The
`Agent::NativeAgent` enum variant and its ~50 references across
`agent_panel.rs`, `agent_configuration.rs`, `agent_connection_store.rs`,
and `manage_profiles_modal.rs` remain in the tree. Reasons:

- **Why:** those files are heavily churned upstream (the 103-commit
  sync we did during fork setup touched several of them). Literally
  ripping out `NativeAgent` would turn every sync into a multi-hour
  merge job, forever, with zero user-visible benefit over gating.
- **How to apply:** when you see `Agent::NativeAgent` referenced in a
  diff or upstream change, leave it alone — it's unreachable from the
  panel UI but still load-bearing for collab workspaces and other code
  paths. Only the *user-facing surfaces* (picker entries, default
  selection) are touched.

The actual gating diff is small and lives in `agent_panel.rs`. Search for
`FORK: claude-acp only` to find it. Two changes:

- Constructor defaults `selected_agent` to `Custom { id: "claude-acp" }`
  instead of `Agent::default()` (which is `NativeAgent`).
- The new-thread picker menu drops the "Zed Agent" and
  "New From Summary" entries; both led to native-agent threads.

Side effects deliberately not gated:

- Inline assist (Cmd-K) and terminal-assist still use
  `crates/language_models/`. They're not part of the agent panel, so
  they bill via whatever provider the user has configured for those
  features (Anthropic API key, Zed Pro, etc.). If you want to gate
  those too, that's a future task — much bigger blast radius.

### `vendor/claude-agent-acp/` — patched ACP bridge

To make Claude Code slash commands work in the panel, this fork vendors
the ACP bridge (`@agentclientprotocol/claude-agent-acp`) under
`vendor/claude-agent-acp/` so we can iterate on it locally instead of
relying on the npx-fetched npm package.

Setup procedure (one-time per fresh clone):

```powershell
cd D:\src\zed\vendor\claude-agent-acp
npm install
npm run build              # produces dist/index.js (gitignored)
```

The user's global Zed settings (`%APPDATA%\Zed\settings.json`) must
point `claude-acp` at the local build:

```jsonc
"agent_servers": {
  "claude-acp": {
    "type": "custom",
    "command": "node",
    "args": ["D:/src/zed/vendor/claude-agent-acp/dist/index.js"],
    "env": { "ANTHROPIC_API_KEY": "" }
  }
}
```

- **Why `ANTHROPIC_API_KEY: ""`:** signals the bridge/SDK to use OAuth
  (Claude Code login) instead of API-key auth — matches what the
  upstream registry path does.
- **How to refresh after editing `src/`:** re-run `npm run build`, then
  restart Zed (the bridge subprocess is spawned per session).

Files under `vendor/claude-agent-acp/`:

- `src/` — TypeScript source (modify here).
- `dist/` — Build output (gitignored, locally generated by `npm run build`).
- `node_modules/` — Dependencies (gitignored, locally installed by `npm install`).
- `package.json`, `tsconfig.json`, etc. — Build config (committed).

The vendored source is a snapshot of upstream
`agentclientprotocol/claude-agent-acp` at **v0.39.0** (which bundles
`@anthropic-ai/claude-agent-sdk` 0.3.156 — this is what surfaces Opus 4.8
in the panel, via the `default` model alias). It carries two fork patches:

- **Ultracode effort tier** (`src/acp-agent.ts`, tagged `// FORK:`).
  `ultracode` is *not* an SDK effort level (those are
  low/medium/high/xhigh/max) — it's a session-scoped `Settings.ultracode`
  flag meaning "xhigh effort + standing dynamic-workflow orchestration".
  The patch adds a synthetic **"Ultracode"** option at the top of the
  Effort dropdown on xhigh-capable models, plus an `effortFlagSettings()`
  helper that routes the selection: choosing it calls
  `applyFlagSettings({ ultracode: true })`; choosing any real level clears
  the flag. All three effort-apply sites go through the helper. Caveat:
  the vendored upstream effort tests assert exact `applyFlagSettings`
  payloads and will fail under this patch — the fork doesn't run the
  vendored test suite (build is `tsc`-only).
- **Workflow visibility** (`src/acp-agent.ts`, tagged `// FORK:`). The
  bridge swallows the SDK's `system` workflow-progress subtypes by
  default, so an ultracode / dynamic-workflow run is invisible in the
  panel. This patch forwards `task_started` (opens a tool-call keyed by
  `task_id`) and `task_notification` (resolves it with the task summary)
  as ACP tool-call updates, so each subagent surfaces as it runs.
  `task_progress` / `task_updated` stay swallowed to avoid per-tick
  thread spam.

Any future fork-specific patches go in `src/acp-agent.ts` and similar;
use a `// FORK:` comment marker so they're easy to find on rebases.

**Re-vendoring to a newer upstream release:** re-clone the matching git
tag (`git clone --branch vX.Y.Z …`) and copy its `src/` + `package.json`
+ `tsconfig.json` over the vendored tree, then `npm install && npm run
build` and re-apply the `// FORK:` patches above. The npm package ships
only `dist/`, so the TypeScript `src/` must come from the GitHub tag.
Do **not** bump only the SDK under an older bridge — the bridge code is
written against a specific SDK's model API, and a mismatch silently
mangles the model list (learned the hard way going 0.3.146→0.3.156 under
the 0.37.0 bridge: the dropdown collapsed to bare `default/sonnet/haiku`).

### `crates/browser_viewer/` — in-editor WebView2 browser tab (Windows only)

The motivating feature: render real Chromium pages inside a Zed tab via
WebView2 in composition mode, with GPUI overlays (modals, popovers,
freehand drawing) painting *above* the page, and a "design mode" that
captures element + screenshot + scribbles + prompt and dispatches
them into the claude-acp agent panel as a multimodal prompt
(phase 4.F — shipped; see the Design mode surface below).

Full design lives in `plans/browser-viewer.md`. **Read it first**
before touching any of the surfaces below — it documents the why for
every non-obvious choice.

#### Architecture cheat sheet (the load-bearing bits)

1. **Composition mode, never child-HWND.** WebView2 is created via
   `ICoreWebView2Environment3.CreateCoreWebView2CompositionController`
   and bound to a fresh `IDCompositionVisual` via
   `SetRootVisualTarget`. Zed already uses DirectComposition for its
   own swap chain, and a child HWND draws *behind* DComp's swap-chain
   visual on the same window — not what we want. Composition mode
   gives us proper inter-visual compositing.

2. **DComp tree (`crates/gpui_windows/src/directx_renderer.rs` +
   `dcomp_registry.rs`).** Restructured so the IDCompositionTarget root
   is a `comp_container` visual with two kinds of children:
   - `comp_visual` (GPUI's swap-chain holder), added with
     `AddVisual(_, false, NULL)` → END of list → **FRONT** of z-order.
   - "Underlay" visuals from external crates, added via
     `gpui_windows::create_underlay_visual_for_hwnd` with
     `AddVisual(_, true, NULL)` → BEGINNING → **BACK** of z-order.
   - The old `create_child_visual_for_hwnd` still exists for visuals
     that should sit *above* GPUI's swap chain (currently unused).
   - `IDCompositionVisual::AddVisual` semantics with `referenceVisual = NULL`:
     `insertAbove = TRUE` → beginning of list (painted first = back);
     `insertAbove = FALSE` → end of list (painted last = front). The
     docs phrasing is confusing; this fork uses the exact opposite
     convention from what "above" linguistically suggests.

3. **Cutout primitive (`crates/gpui/src/scene.rs` + `window.rs`).** A
   GPUI scene primitive added by this fork. `Window::paint_cutout(bounds)`
   inserts a `Cutout` at the calling element's z-position. The Windows
   D3D11 renderer handles the `PrimitiveBatch::Cutouts` arm by calling
   `ID3D11DeviceContext1::ClearView(rtv, [0,0,0,0], &rects)` — the
   *only* GPUI primitive that can decrease destination alpha
   (everything else uses src-over blend which can't). `ClearView`
   bypasses blend state, depth-stencil, raster state, scissor, and the
   pipeline state in general, writing raw `[0, 0, 0, 0]` into the RTV
   pixels. Other renderers (Metal, wgpu) treat the batch as a no-op —
   browser_viewer is Windows-only anyway.

4. **`BrowserViewportElement::paint` emits the cutout** when its
   session is live. Paint order in a browser tab: workspace bg paints
   opaque → cutout punches alpha=0 in viewport rect → WebView2
   underlay shows through that hole → modals/popovers/drawing strokes
   painted afterwards stay opaque on top. This is the "all GPUI on top
   of the page" effect.

5. **CDP for page keyboard** (`crates/browser_viewer/src/browser_view.rs` +
   `webview2_host.rs`). `ICoreWebView2CompositionController` has *no*
   `SendKeyEvent` method in any released or prerelease SDK including
   1.0.4015-prerelease (verified during Phase 3 SDK research).
   Microsoft has never shipped keyboard injection for composition
   mode. We use Chrome DevTools Protocol via
   `webview.CallDevToolsProtocolMethod("Input.dispatchKeyEvent", json, handler)`
   instead — works on our current `webview2-com 0.38`, survives all
   future SDK upgrades, dispatches at the renderer level so it bypasses
   Win32 focus entirely. Covers ASCII / arrows / F-keys / modifiers /
   hold-to-repeat. Does *not* cover IME (CJK) or Win32 dead-key
   accents — explicit non-goals for the dogfood target.

6. **Win32 SetFocus trick** (`forward_mouse_event` in `browser_view.rs`).
   WebView2 in composition mode creates internal HWNDs and steals
   Win32 keyboard focus on every mouse-down. From then on,
   `GetMessageW` routes WM_KEYDOWN to the WebView2 HWND, and
   `gpui_windows::platform.rs::translate_accelerator` never sees the
   keys — so global shortcuts like Ctrl+Shift+P / Ctrl+P stop reaching
   Zed. After each forwarded mouse event we call
   `SetFocus(zed_hwnd)` to yank Win32 focus back. Side-effect-neutral
   because the page receives its own keystrokes via the CDP path, not
   via the OS message pump.

7. **Design-mode JS bus** (`design_mode_script.rs` + `design.rs`). A
   script is injected via `AddScriptToExecuteOnDocumentCreated` on
   every navigation. It handles hover highlighting + element selection
   + scroll-tracking; communicates with the host via JSON over
   `chrome.webview.postMessage`. The host registers
   `add_WebMessageReceived` and forwards messages to BrowserItem as
   `NavigationEvent::DesignModeMessage(raw_json)`, which is parsed
   into typed `DesignInbound` variants. The script intercepts
   pointerdown / mousedown / click / touchstart / contextmenu in the
   capture phase, calling `preventDefault + stopImmediatePropagation`
   on all of them — many sites navigate on pointerdown well before
   click fires, so only capturing click leaks navigation.

#### Build / sync conflict surface this fork now owns

Because Phase 4's Cutout primitive lives in `crates/gpui/`, the
following upstream-tracked files are now part of the fork-vs-upstream
diff and **will cause merge conflicts on upstream rebases:**

- `crates/gpui/src/scene.rs` — new `Cutout` struct,
  `Primitive::Cutout` variant, `PrimitiveKind::Cutout`,
  `PrimitiveBatch::Cutouts`, plus `Scene::cutouts` field and
  matching updates to `clear/finish/insert_primitive/batches()`.
- `crates/gpui/src/window.rs` — new `Window::paint_cutout` method.
- `crates/gpui_windows/src/directx_renderer.rs` — `comp_container`
  field, `set_swap_chain` restructure, `draw_cutouts` method.
- `crates/gpui_windows/src/dcomp_registry.rs` —
  `create_underlay_visual_for_hwnd` + `HostedVisual::bring_underlay_to_front`
  (active-tab underlay reorder, Task #28).
- `crates/gpui_windows/src/window.rs` — none directly, but the
  background-appearance code may collide.
- `crates/gpui_macos/src/metal_renderer.rs` and
  `crates/gpui_wgpu/src/wgpu_renderer.rs` — no-op `Cutouts` arm in
  the batch match.
- `assets/keymaps/default-windows.json` — `BrowserView` context with
  `browser::OpenDevTools` / `browser::FocusAddressBar`.
- `crates/zed/src/main.rs` and `Cargo.toml` — `browser_viewer` init
  + workspace member entry.
- `crates/zed_actions/src/lib.rs` — `agent::SendDesignBundleToAgent`
  action (the Phase 4.F dispatch payload). Leaf crate, low conflict
  risk; re-add inside `mod agent` next to `ReviewBranchDiff`.
- `crates/agent_ui/src/agent_panel.rs` — a `register_action` arm for
  `SendDesignBundleToAgent` plus the `build_design_bundle_blocks`
  helper. **This file churns heavily upstream.** The arm sits next to
  the `ReviewBranchDiff` / `ResolveConflictsWithAgent` git arms in the
  `init` `register_action` chain, and the helper next to
  `build_conflict_resolution_prompt`. Both tagged
  `// FORK: browser_viewer 4.F` — re-add on conflict.
- `crates/browser_viewer/{Cargo.toml, src/browser_view.rs, src/drawing.rs}`
  — new `zed_actions` / `base64` / `image` (png-only) deps, the dispatch
  in `on_design_submit`, and the `annotate_screenshot` compositor.
  Fork-owned crate, so these won't conflict.

For rebases, search the diff for `FORK:` markers — most non-obvious
changes are tagged. Where there's a clean re-insertion slot in an
upstream-reordered list, just put the line back; for substantive
collisions, prefer the fork's behaviour and re-read this section.

#### User-facing surfaces

- `browser: new tab` action — opens the configured homepage in a
  tab. Default URL settable via `browser.homepage` in settings.
- Address bar: URL editor, back/forward/reload, Ctrl+L to focus +
  select-all. Search fallback uses `browser.search_url` with
  `{query}` placeholder.
- DevTools: Ctrl+Shift+I / F12 when a browser tab has focus.
- Design mode: crosshair icon in address bar (or `browser: toggle
  design mode`). Click an element → floating "Describe the change"
  panel anchored next to it. Submit → composites the selected-element
  outline + freehand strokes onto the page screenshot, then dispatches
  a `zed_actions::agent::SendDesignBundleToAgent` action carrying the
  typed prompt, element context (selector, page URL, source file:line,
  outerHTML) and the annotated PNG (base64, capped ≤2 MB). `agent_ui`
  handles it: builds an ACP prompt (text + image + embedded-HTML
  resource) and sends it into the claude-acp panel — continuing the
  active thread if one is open, else opening a fresh one (so iterative
  tweaks accumulate context). Set `ZED_BROWSER_DESIGN_DEBUG_BUNDLE=1`
  to also dump the bundle to `%TEMP%\zed-browser-design\<unix-ms>\`
  for debugging. (Phase 4.F — the loop-closing feature; shipped.)
- Drawing mode: pencil icon → freehand strokes over the page; eraser
  icon clears.

#### Known limits / parked work

- IME (CJK / Arabic) and dead-key accents — CDP doesn't cover these.
- Multi-browser-tab in same pane — **fixed & verified (Task #28).**
  Inactive browser tabs keep painting (no hide → no reactivation flash);
  the active tab reorders its WebView2 underlay to the front of the
  underlay group on activation and on new-tab session-ready, so only it
  shows through its cutout. See `HostedVisual::bring_underlay_to_front`
  (gpui_windows) + `WebView2Session::bring_underlay_to_front`, driven
  from `BrowserView::render` / `deactivated` via the `is_visible` flag.
  Verified with two tabs (localhost + google).
- ACP submission pipeline (Task #29) — **done & verified (Phase 4.F).**
  Submit dispatches the bundle into the claude-acp panel and Claude
  edits the targeted source (verified end-to-end against a Next.js dev
  app). A required fix landed alongside: the design panel now calls
  `.occlude()` so Submit/Cancel clicks don't leak through to the page
  and re-select the next element (the panel sits over the WebView2
  mouse-forwarding region; without occlude, GPUI fired both the button
  click and the viewport's forward handler). Remaining polish: confirm
  the element-outline lands exactly on the selected element on hi-DPI
  pages; exercise the pencil-drawing composite path; and validate
  reuse-active-thread vs new-thread in the wild. A per-browser-tab
  "owned thread" model is a possible future refinement (deferred —
  needs async thread-handle capture).

### `stubs/msvc_spectre_libs/` — build workaround

A local no-op crate that replaces the crates.io `msvc_spectre_libs` via
`[patch.crates-io]` in the workspace `Cargo.toml`. The upstream crate's
`build.rs` panics (when its `error` feature is enabled, as `microsoft/pet`
does) unless the VS "Spectre-mitigated libs" component is installed. This
stub does nothing and lets the linker use the normal CRT — fine for a
personal build, not appropriate to upstream.

## Build notes

- Target directory is the default in-tree `D:\src\zed\target\`.
  Built binary lands at `D:\src\zed\target\debug\zed.exe` (debug) or
  `D:\src\zed\target\release\zed.exe` (release).
- An older out-of-tree target dir at `D:\zt\` exists from a previous
  shell session that had `CARGO_TARGET_DIR=D:\zt` set. **Don't run
  binaries from `D:\zt\` — they're stale.** Safe to delete the whole
  `D:\zt\` tree to reclaim ~16 GB. No `CARGO_TARGET_DIR` is set, so
  cargo uses the in-tree default `target/`.
- **Always run cargo from `D:\src\zed` (the project dir) — never from
  a parent like `D:\` with `--manifest-path`.** Cargo resolves both
  `.cargo/config.toml` and `rust-toolchain.toml` from the *current
  directory*, not the manifest path. Running from elsewhere silently
  drops both, with two non-obvious failure modes: (1) you get the
  default toolchain instead of the pinned `1.95.0`, surfacing as a
  spurious `error[E0658]: use of unstable library feature 'cfg_select'`
  in `crates/client`; (2) you lose the `target-feature=+crt-static`
  from `.cargo/config.toml`'s `[target.'cfg(target_os = "windows")']`
  block, surfacing as `LNK1169: one or more multiply defined symbols`
  at the final link (the static-CRT `webrtc_sys`/livekit objects clash
  with the dynamic CRT). That crt-static flag is load-bearing for
  linking on Windows.
- Runtime dependency: poppler's `pdftoppm.exe`. Install via
  `winget install oschwartz10612.Poppler`, or ensure it's on PATH.

### Build with `-j 4` on this machine

Always build with `cargo build -j 4` (not bare `cargo build`).

- **Why:** the machine has 8C/16T and 32 GB RAM. Cargo defaults to one
  `rustc` per logical core (16), and several Zed crates
  (`language_model`, `editor`, `theme`, `wasmtime-wasi`) peak at 4–8 GB
  per `rustc` instance. 16 parallel rustcs at that footprint blow past
  available RAM and trigger `rustc-LLVM ERROR: out of memory`, which
  manifests as cascading "invalid metadata" / "only metadata stub
  found" errors in unrelated crates. Closing memory hogs (Chrome,
  Slack) helps, but `-j 4` is the reliable fix: ~24 GB peak, fits in
  available RAM with headroom.
- **How to apply:** every cargo invocation in the sync workflow and
  during day-to-day development. From-scratch debug builds at `-j 4`
  finish in ~5 minutes on this hardware.

### After a toolchain bump, `cargo clean` first

If `rust-toolchain.toml` changes between syncs (upstream bumps the
pinned Rust version), the next build will fail with errors like:

```
error[E0786]: found invalid metadata files for crate `gpui`
error: only metadata stub found for `dylib` dependency `std` ...
```

- **Why:** stale `.rmeta` files in `D:\zt\` were written by the old
  compiler and can't be read by the new one. These same errors can
  *also* be caused by mid-build OOM (see `-j 4` note above); the
  differentiator is whether the build output earlier shows `rustup`
  installing components.
- **How to apply:** run `cargo clean` once after a toolchain bump,
  then build normally. Costs the ~5 min from-scratch build time.

## Sync workflow — pulling upstream changes into this fork

Run this whenever you want to incorporate new upstream Zed commits.
The branch chain is `main` → `pdf-viewer` → `claude-only` →
`browser-viewer`; each rebases onto its predecessor.

```powershell
# 1. Update local main from upstream
git fetch upstream
git checkout main
git merge --ff-only upstream/main
git push origin main                  # keep the fork's main current too

# 2. Rebase pdf-viewer onto the new main
git checkout pdf-viewer
git rebase main
#    ...resolve conflicts if any...
cargo build -j 4                      # verify (see Build notes for why -j 4)
git push --force-with-lease origin pdf-viewer

# 3. Rebase claude-only onto the new pdf-viewer
git checkout claude-only
git rebase pdf-viewer
cargo build -j 4
git push --force-with-lease origin claude-only

# 4. Rebase browser-viewer onto the new claude-only
git checkout browser-viewer
git rebase claude-only
cargo build -j 4
git push --force-with-lease origin browser-viewer
```

### Conflict hot spots

Files this fork modifies in code paths upstream churns frequently:

- **`Cargo.toml`** — `members = [...]` insertion and
  `[workspace.dependencies]` entry sit in alphabetically-sorted lists.
  Re-insert in the right alphabetical slot if upstream reorders.
- **`Cargo.lock`** — usually easiest to take upstream's version
  (`git checkout --theirs Cargo.lock`) then re-run `cargo build` to
  regenerate with our deps included.
- **`crates/zed/src/main.rs`** and **`crates/zed/src/zed.rs`** —
  `pdf_viewer::init(cx)` and `browser_viewer::init(cx)` sit in init
  lists that get reordered. Re-add after upstream's version of the
  list.
- **`crates/gpui/src/scene.rs`** — fork adds a new `Cutout`
  primitive (struct + enum variants + Scene field + BatchIterator
  updates). Touches several distinct spots in the file; upstream may
  add other primitives in the same locations. See
  "Build / sync conflict surface" in the browser_viewer section.
- **`crates/gpui/src/window.rs`** — fork adds `Window::paint_cutout`
  near `paint_quad`.
- **`crates/gpui_windows/src/directx_renderer.rs`** — fork's
  `comp_container` field on `DirectComposition`, the `set_swap_chain`
  restructure, and the new `draw_cutouts` method.
- **`crates/gpui_macos/src/metal_renderer.rs`** and
  **`crates/gpui_wgpu/src/wgpu_renderer.rs`** — no-op `Cutouts` arm
  in the `match batch` block; if upstream adds variants there, just
  put ours back next to `Surfaces`.
- **`agent_panel.rs`** / **`agent_configuration.rs`** etc. (on
  `claude-only` branch) — the claude-acp gating points. See "Agent
  panel — gated to Claude Code only" section.
- **`crates/settings_content/src/settings_content.rs`** and
  **`crates/settings/src/vscode_import.rs`** — `browser` field added
  alongside other settings. Re-add in the right position.

`assets/keymaps/default-windows.json` has fork-only additions
(`BrowserView` context block; pdf_viewer keys). Conflicts when
upstream reorders sibling rules — search for `FORK:` markers.

Everything under `crates/pdf_viewer/`, `crates/browser_viewer/`,
`vendor/claude-agent-acp/`, and `stubs/` won't conflict — upstream
doesn't touch them.

### Rebase vs. merge

We rebase, not merge. Reasons:

- **Why:** keeps `pdf-viewer` as a clean, linear set of "PDF viewer"
  commits on top of current upstream. Easier to inspect, easier to
  eventually open as an upstream PR if desired.
- **How to apply:** always rebase `pdf-viewer` onto `main`; never merge
  `main` into `pdf-viewer`. Push with `--force-with-lease`, never plain
  `--force`.

## What NOT to do

- **Never push to `upstream`** — you don't have write access, but the
  attempt will still surprise you. Push only to `origin`.
- **Never commit fork-specific changes to `main`.** `main` exists solely to
  mirror upstream. All fork changes go on `pdf-viewer` (or other feature
  branches off `main`).
- **Don't try to upstream the `msvc_spectre_libs` stub.** It's a personal
  build workaround, not a fix appropriate for the source repo.
