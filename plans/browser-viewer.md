# Browser Viewer — Plan & Specification

**Status:** Draft, ready to implement
**Branch (future):** `browser-viewer` (off `claude-only`)
**Platform target (phase 1):** Windows only
**Owner:** ccoetzeeinspired
**Last updated:** 2026-05-27

---

## 1. Executive Summary

This document specifies a new feature for this Zed fork: an **embedded, fully
interactive browser pane** that opens as a Zed editor tab and renders web
content using Microsoft's WebView2 (Chromium/Edge) component, integrated into
GPUI's render tree on Windows.

The motivating use case is the "Cursor design-mode" workflow seen in modern
AI-augmented IDEs: navigate to a running dev server (`localhost:3001`) in a
Zed tab next to the source files, click on a UI element, draw on it, type a
description of the desired change, and have it routed to the Claude Agent
with full context (screenshot, selected element, source location). This
collapses the inner loop of UI development to within a single window.

A previous note in the fork's CLAUDE.md observed that Zed has **no native
mechanism for embedding web content** — GPUI is a custom GPU-rendered UI
framework with no DOM, JS engine, or webview. This spec addresses that
constraint head-on by integrating WebView2 via Windows' composition
infrastructure (DirectComposition), without forking GPUI's core. The
upstream-shaped extension to GPUI is one new "host visual slot" primitive;
all browser-specific code lives in a new `browser_viewer` crate that
follows the `pdf_viewer` pattern.

The work is scoped into six phases, each producing a working artifact:

1. **Phase 0** — Spike. WebView2 hosted as a child HWND, navigates to a
   hardcoded URL inside Zed's main window. Proves embedding is possible.
2. **Phase 1** — Tabs and navigation. Browser opens as a real Zed tab via
   command palette, with address bar, back/forward/reload.
3. **Phase 2** — Composition mode. WebView2 draws into Zed's render
   surface via DirectComposition instead of a child HWND.
4. **Phase 3** — Input bridging and UX. Keyboard, mouse, IME, scrolling,
   focus, DevTools, and browser-style shortcuts all work correctly.
5. **Phase 4** — Design mode. Element selection, freehand drawing
   overlay, and integration with claude-acp.
6. **Phase 5** — Production polish. Error handling, runtime detection,
   settings, performance tuning.

The estimated total effort is 10–14 weeks of focused work for a single
developer, with the design-mode feature (the primary motivation) usable
at the end of Phase 4.

---

## 2. Goals

- **G1.** A user can open a URL inside a Zed tab and interact with the page
  as they would in a normal browser window.
- **G2.** Multiple browser tabs coexist with file tabs in the same Zed pane,
  using Zed's existing tab infrastructure.
- **G3.** Page rendering is at native Chromium speed — 60fps for typical
  pages, hardware-accelerated video and WebGL, no per-frame screenshot
  bottleneck.
- **G4.** A "Design Mode" toggle in the browser tab lets the user click an
  element, draw freehand annotations on top of it, type a description, and
  send the bundled context to the Claude Agent for code modification.
- **G5.** Sessions, cookies, and localStorage persist across Zed restarts,
  so authenticated apps work without re-login every session.
- **G6.** DevTools (Chrome's element inspector, console, network panel) are
  reachable from the browser pane.
- **G7.** All work lives in a single new crate (`browser_viewer`) plus a
  minimal GPUI extension; nothing else in Zed needs structural changes.

## 3. Non-Goals

- **NG1.** Cross-platform support (macOS, Linux) in the initial release.
  Phase 1–5 are Windows-only. A future spec will cover CEF OSR for
  cross-platform; the architecture is designed to make that transition
  feasible without re-architecting Zed-side code.
- **NG2.** Replacing the user's default browser. Browser tabs in Zed are
  for development workflows; users may still prefer Chrome/Firefox for
  general browsing.
- **NG3.** Bookmark management, history sync, password manager. Out of
  scope. Use a real browser for those.
- **NG4.** Extension support (Chrome Web Store extensions). WebView2 does
  not support these the way Chrome does, and we don't want to build a
  parallel extension story.
- **NG5.** Replacing the Simple Browser tab via the existing `markdown
  preview` flow or similar. This is a new, parallel UI surface.
- **NG6.** Native ad-blocking, fingerprinting protection. Out of scope.
- **NG7.** Bundling WebView2 Runtime itself. We rely on the system-installed
  runtime (present on Windows 10/11 by default since 2022) and prompt the
  user to install it if missing.

## 4. Glossary

- **WebView2** — Microsoft's official embedded Chromium component for
  Windows. Built on the Edge browser, exposes a COM API. Installed
  separately as the "WebView2 Runtime" but pre-installed on modern
  Windows.
- **OSR (Off-Screen Rendering)** — A pattern where a web engine renders to
  a buffer/texture rather than its own window, allowing the host
  application to composite the result into its own scene. CEF supports
  OSR directly; WebView2 supports an analogous pattern via composition
  mode.
- **DirectComposition (DComp)** — A Windows graphics API that composes
  multiple sources (windows, swap chains, surfaces) into a single visual
  tree drawn by the desktop window manager. The mechanism that lets
  WebView2 draw "into" another application's window without being a
  separate HWND.
- **GPUI** — Zed's custom Rust UI framework. GPU-rendered, no DOM, no
  retained widget tree. Each frame is laid out and drawn imperatively.
- **`Item`** — A trait in `crates/workspace/` that anything appearing as
  a tab in a Zed pane must implement (editor buffers, image viewers, PDF
  viewer, terminal, etc.). The browser pane will implement this.
- **`ProjectItem`** — A trait in `crates/project/` for items associated
  with a project path. Maps URIs/paths to items. Browser tabs will be
  registered as project items keyed off the `http://` and `https://`
  URI schemes (or a special `browser:` scheme — see Open Questions).
- **claude-acp** — The Agent Client Protocol bridge to Claude Code, used
  by the agent panel in this fork. Already vendored under
  `vendor/claude-agent-acp/`.
- **`webview2-com` crate** — The currently-maintained Rust bindings to
  the WebView2 COM API. Selected over `webview2-rs` (older, higher-level
  but less complete) and `webview2` (deprecated).

---

## 5. System Architecture

### 5.1 Component diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                       Zed Main Process                          │
│                                                                 │
│  ┌─────────────┐    ┌──────────────────┐    ┌────────────────┐  │
│  │   Editor    │    │ browser_viewer   │    │   Agent UI     │  │
│  │    Tabs     │    │     (crate)      │    │  (claude-acp)  │  │
│  └──────┬──────┘    └────────┬─────────┘    └────────┬───────┘  │
│         │                    │                       │          │
│         │             ┌──────▼──────┐                │          │
│         │             │ BrowserView │                │          │
│         │             │  (per tab)  │                │          │
│         │             └──────┬──────┘                │          │
│         │                    │ owns                  │          │
│         │             ┌──────▼─────────┐             │          │
│         │             │ WebView2Host   │             │          │
│         │             │  (COM client)  │             │          │
│         │             └────────┬───────┘             │          │
│         │                      │                     │          │
│         ▼                      ▼                     ▼          │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │            GPUI Window (DirectComposition tree)           │  │
│  │  ┌─────────────┐  ┌────────────────────┐  ┌────────────┐  │  │
│  │  │ Editor view │  │  WebView2 visual   │  │ Sidebar    │  │  │
│  │  │ (GPUI draw) │  │  (DComp child)     │  │ (GPUI)     │  │  │
│  │  └─────────────┘  └────────────────────┘  └────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ COM RPC
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│            WebView2 Runtime Browser Process Tree                │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
│  │ msedgewebview│  │  Renderer    │  │  GPU         │   ...     │
│  │   (browser)  │  │  process(es) │  │  process     │           │
│  └──────────────┘  └──────────────┘  └──────────────┘           │
└─────────────────────────────────────────────────────────────────┘
```

### 5.2 Process model

Zed remains a single host process. Opening a browser tab triggers the
WebView2 SDK to spawn its multi-process browser tree as a child of Zed.
Subsequent tabs share the same WebView2 environment (one browser process
tree), but each tab has its own renderer process per Chromium's site
isolation rules.

Process expectations per browser tab (steady state):

- Shared by all tabs: 1 `msedgewebview2.exe` (browser controller),
  1 GPU process, 1 utility process. ~200MB combined.
- Per tab: 1 renderer process (~80MB baseline, grows with page).
- Per tab on heavy sites: additional out-of-process iframes (~30MB
  each).

These are inherited from Chromium and not configurable. Closing all
browser tabs lets the WebView2 process tree exit; the next `BrowserView`
spawns a new tree.

### 5.3 Data flow — page render

1. `BrowserView::new(url)` calls `WebView2Host::create_async(url)`.
2. `WebView2Host` initializes a `CoreWebView2Environment` (shared
   singleton across the Zed process) and a `CoreWebView2Controller`
   for this tab.
3. The controller is configured for composition mode:
   `CoreWebView2ControllerOptions.IsInPrivateModeEnabled = false;`
   then `environment.CreateCoreWebView2CompositionControllerAsync(...)`.
4. The resulting `IDCompositionVisual` is handed to a new GPUI
   primitive `HostedVisual` (see §6.2), which slots it into the GPUI
   window's composition tree at the browser tab's rect.
5. WebView2's renderer process(es) generate frames. WebView2's GPU
   process pushes those frames into the shared composition visual.
6. Windows' Desktop Window Manager (DWM) composes the GPUI swap chain
   and the WebView2 visual into the final on-screen image. **No
   per-frame copy through Zed code.**

### 5.4 Data flow — input

1. GPUI dispatches events normally — mouse, keyboard, scroll — to the
   focused element.
2. When focus is in a `BrowserView`, the view's event handlers
   translate GPUI events into WebView2 input calls:
   - Mouse: `controller.SendMouseInput(VirtualMouseEventKind, ...)`
   - Keyboard: `controller.SendKeyEvent(...)` (or rely on
     WebView2's `AcceleratorKeyPressed` callback for shortcuts)
   - Scroll: `controller.SendMouseInput(MouseWheel, ...)`
3. WebView2 routes inputs through its renderer, page receives DOM
   events as a real browser would.
4. Page-initiated focus changes propagate back via WebView2's
   `LostFocus` / `GotFocus` events, which the host translates to
   GPUI focus state.

### 5.5 Data flow — design mode

1. User clicks the "Design Mode" toggle in the browser tab toolbar.
2. `BrowserView` calls `controller.AddScriptToExecuteOnDocumentCreated`
   to register the element-selection script (see §6.4).
3. On the next reload (or via `ExecuteScriptAsync` to apply
   immediately), the page gains element-hover outlines.
4. User clicks an element. The injected script calls
   `window.chrome.webview.postMessage({ kind: 'element_selected', ... })`.
5. WebView2 raises `WebMessageReceived` on the host; `BrowserView`
   updates state with the selected element's CSS selector, outerHTML,
   bounding rect, and (best-effort) source location.
6. Zed renders a "Describe the change" text input as a GPUI overlay,
   positioned above the browser visual via the composition layering.
7. Optionally, the user draws freehand on the browser surface. A new
   GPUI canvas primitive (see §6.5) captures strokes into a vector
   path overlay; the page underneath continues to receive other events.
8. On submit:
   - Capture screenshot via `controller.CapturePreviewAsync()`.
   - Bundle `{ screenshot.png, drawing.svg, selector, outerHTML,
     source_hint, prompt }` into an ACP `prompt` request with image
     content blocks.
   - Send to the active claude-acp session (the agent panel).
9. Claude responds with code modifications referencing the source file.

---

## 6. Technical Design

### 6.1 WebView2 embedding model — composition only

**Decision (revised in Phase 0).** We always use composition mode.
The child-HWND fallback originally planned for Phase 0 doesn't work
inside Zed: GPUI renders via DirectComposition, and child HWNDs are
drawn behind the parent's DComp swap chain, making them invisible.
There is no cheap intermediate; composition is the entry-level
requirement.

**Composition mode.** `ICoreWebView2Environment3.CreateCoreWebView2-
CompositionController(parent_hwnd, callback)` returns a
`ICoreWebView2CompositionController` whose `SetRootVisualTarget`
accepts an `IDCompositionVisual` we own. We add that visual as a
child of GPUI's root composition visual so it draws on top of Zed's
swap chain content. The `parent_hwnd` is still required — it's the
logical parent for input/IME routing — but the rendering output goes
to the visual.

The same COM object also implements `ICoreWebView2Controller`, which
exposes `SetBounds`, `SetIsVisible`, and `CoreWebView2().Navigate(...)`.
We `.cast()` between the two interfaces as needed.

**Input has no automatic path.** Composition mode does not forward
OS input to WebView2; the host must call `SendMouseInput` /
`SendKeyEvent` / `SendPointerInput` explicitly. Input bridging is
therefore part of Phase 1 (basic mouse) and Phase 3 (full keyboard,
IME, scroll, focus), not Phase 0.

### 6.2 GPUI extension — the `dcomp_registry` module (shipped)

GPUI owns the `IDCompositionTarget` and root `IDCompositionVisual`
for each Zed window. To insert externally-owned visuals (WebView2,
future video preview, etc.), Phase 0 added a small Windows-only
module in `crates/gpui_windows/src/dcomp_registry.rs`:

```rust
// Public API
pub fn create_child_visual_for_hwnd(hwnd: HWND) -> Result<HostedVisual>;

pub struct HostedVisual { /* … */ }
impl HostedVisual {
    pub fn visual(&self) -> &IDCompositionVisual;
    pub fn commit(&self) -> Result<()>;
}
// Drop removes the visual from the root and commits.
```

Internally a thread-local HWND → `Weak<DirectComposition>` map is
populated by `DirectXRenderer::new` when it creates a `DirectComposi-
tion`. The renderer's storage of `DirectComposition` was changed to
`Arc<DirectComposition>` so callers can hold weak references without
extending the renderer's lifetime.

**Why thread-local, not a global static.** COM interfaces in
`DirectComposition` are STA-bound to the GPUI UI thread. All
registration (`DirectXRenderer::new`) and lookup
(`browser_viewer::spike`) happen on that thread. A thread-local
avoids needing `unsafe impl Send/Sync` on COM types that genuinely
aren't safe to share across threads.

**Why HWND-keyed, not method-on-Window.** The cleanest API would be
a method on `gpui::Window`, but exposing it requires extending the
cross-platform `PlatformWindow` trait (touches non-Windows
implementations) or downcasting via `Any`. The registry approach
adds zero surface to gpui core; everything lives in `gpui_windows`.
If a future feature needs broader cross-platform abstractions,
revisit then.

**Lifetime.** `HostedVisual` holds an `Arc<DirectComposition>` so
the renderer's DComp survives at least as long as any hosted visual.
The visual is removed from the root and the tree is re-committed on
drop, so the owner can simply drop the handle to clean up.

### 6.3 Input bridging

WebView2 in composition mode does not receive input from the OS
directly — the host is responsible for forwarding every relevant
event. Mouse is straightforward via `SendMouseInput`; **keyboard is
not symmetric**, which we learned the hard way and is documented
here so future sessions don't repeat the experiment.

#### Mouse (shipped in Phase 1.D)

| GPUI event             | WebView2 call                                  |
|------------------------|------------------------------------------------|
| MouseDown / MouseUp    | `SendMouseInput(LeftButtonDown/Up, ...)`       |
| MouseMove              | `SendMouseInput(Move, x, y)`                   |
| ScrollWheel            | `SendMouseInput(Wheel/HorizontalWheel, delta)` |
| Navigate (X1/X2)       | `SendMouseInput(X_BUTTON_DOWN/UP, ...)` + `cx.stop_propagation()` so the Pane's history handler doesn't double-fire |

#### Keyboard — CDP `Input.dispatchKeyEvent` (shipped in Phase 3)

**Background.** `ICoreWebView2CompositionController` has no
`SendKeyEvent`. We verified this across every released SDK including
the latest prerelease at the time of writing (1.0.4015-prerelease) —
the keyboard equivalent of `SendMouseInput` has never shipped, is not
on the public roadmap, and the MicrosoftEdge/WebView2Feedback issue
tracking it has no ETA. Bumping `webview2-com` will not unblock this.

We also briefly tried (Phase 1.D) calling
`controller.MoveFocus(PROGRAMMATIC)` on viewport click hoping
WebView2's internal HWND subclass would then route keyboard messages
to the page. Result: page keyboard still didn't work, and address-bar
typing broke intermittently. Reverted.

**Decision: CDP `Input.dispatchKeyEvent` via
`ICoreWebView2.CallDevToolsProtocolMethod`.** Available since
WebView2 SDK 1.0.* (well before our 0.38 binding), survives all future
SDK upgrades, dispatches at the renderer level so Win32 focus stays on
Zed's HWND, and is the path Microsoft themselves point developers to
in WebView2Feedback threads about composition-mode keyboard.

Other options we considered and ruled out:

1. **Host-side HWND subclass + JS injection** — synthesize
   `KeyboardEvent` dispatches via `ExecuteScript`. More moving parts
   (Win32 subclass + per-frame focus inspection + JS), bypasses Chrome
   form handling.
2. **`SendInput` Win32 simulation** — affects system focus, plays
   badly with multi-window Zed, most brittle of all.
3. **Newer SDK with `ICoreWebView2KeyboardInputController`** — does
   not exist; eliminated by SDK research.
4. **Synthetic `WM_KEYDOWN`/`WM_CHAR` to WebView2 child HWND** — used
   by Tauri/wry as a fallback. Works but fragile across Edge updates
   (child window class names can change).

**What CDP covers.** Letters, digits, modifier combos, Shift-symbols,
arrows, Home/End/PgUp/PgDn, Tab, Enter, Backspace, Esc, Delete, Insert,
F1–F24. Form `input` events fire (so React onChange, contentEditable,
etc. work). Hold-to-repeat works because Windows fires repeated
WM_KEYDOWN and we forward each.

**What CDP misses.** IME composition (CJK, Arabic), Win32 dead-key
sequences, autofill heuristics that watch for OS-level keystrokes, OS
accessibility input (UI Automation, speech-to-text). For US-English
ASCII workflows — the documented Phase 3 target — none of these block
dogfooding. IME stays on the roadmap as a separate axis.

**Implementation summary** (see `crates/browser_viewer/src/browser_view.rs`):

- `BrowserView::on_key_down` / `on_key_up` listeners on the focus-rooted
  `v_flex` forward into `WebView2Session::dispatch_key_event`.
- `keystroke_to_cdp(&Keystroke)` maps GPUI's `key` strings to
  `(KeyboardEvent.key, KeyboardEvent.code, windowsVirtualKeyCode, text)`.
  `text` is set only on `keyDown` for printable keys without
  Ctrl/Alt, so form `input` events fire and Ctrl-combos don't leak
  characters.
- `WebView2Session::dispatch_key_event` builds a CDP JSON payload
  inline (no `serde_json` dependency) and fire-and-forgets via
  `CallDevToolsProtocolMethod` with a no-op completion handler.

**Page-focused Enter is special.** GPUI's keymap dispatch counts an
`on_action` listener as "consumed" once it's invoked. `menu::Confirm`
is bound to the URL editor's Enter; the same listener fires from the
page viewport when BrowserView is focused. To avoid Enter being
swallowed into a no-op (which would prevent form submission),
`on_submit_url` checks `url_editor.focus_handle.is_focused(window)` —
if false, it manually forwards `keyDown`/`keyUp` for Enter via CDP and
returns.

**Phase 3 also added Ctrl+L** (`browser::FocusAddressBar`) — focuses
the URL editor and selects all, matching real-browser convention. F12
and Ctrl+Shift+I open WebView2 DevTools (Phase 2's DevTools opener).

### 6.4 Design mode JS protocol

A script registered via `AddScriptToExecuteOnDocumentCreated` injects
into every navigation. The host enables/disables it by maintaining a
"design mode" flag and re-injecting on toggle.

The injected script (sketch):

```js
(() => {
  if (window.__zedDesignMode) return; // idempotent
  window.__zedDesignMode = true;

  const HIGHLIGHT_STYLE = '2px solid rgb(0, 255, 136)';
  const overlay = document.createElement('div');
  Object.assign(overlay.style, {
    position: 'fixed', pointerEvents: 'none', zIndex: 2147483647,
    border: HIGHLIGHT_STYLE, boxSizing: 'border-box',
    transition: 'all 60ms ease-out',
  });
  document.body.appendChild(overlay);

  let armed = false;
  let hovered = null;

  function activate() { armed = true; }
  function deactivate() {
    armed = false; hovered = null;
    overlay.style.display = 'none';
  }

  document.addEventListener('mousemove', e => {
    if (!armed) return;
    const el = e.target;
    if (el === hovered || el === overlay) return;
    hovered = el;
    const r = el.getBoundingClientRect();
    Object.assign(overlay.style, {
      display: 'block',
      left: r.left + 'px', top: r.top + 'px',
      width: r.width + 'px', height: r.height + 'px',
    });
  }, true);

  document.addEventListener('click', e => {
    if (!armed) return;
    e.preventDefault(); e.stopPropagation();
    const el = e.target;
    const rect = el.getBoundingClientRect();
    window.chrome.webview.postMessage({
      kind: 'element_selected',
      selector: cssPath(el),
      outerHTML: el.outerHTML.slice(0, 8000),
      rect: { x: rect.x, y: rect.y, w: rect.width, h: rect.height },
      source: detectReactSource(el),
    });
  }, true);

  // Host messages: 'activate', 'deactivate', 'clear'
  window.chrome.webview.addEventListener('message', e => {
    const msg = e.data;
    if (msg === 'activate') activate();
    else if (msg === 'deactivate') deactivate();
  });

  // (cssPath and detectReactSource helpers omitted for brevity)
})();
```

Source detection cascade (`detectReactSource`):

1. Walk DOM upward looking for `__reactFiber$*` property — the React
   internal fiber. From the fiber, walk to `_debugSource` which has
   `{ fileName, lineNumber, columnNumber }`. This works for React in
   dev mode (production strips `_debugSource`).
2. Look for `data-source-file` / `data-source-line` attributes — many
   build-time tools (e.g., `vite-plugin-react-click-to-component`)
   inject these.
3. Look for `data-component`, `data-testid`, `id` — coarser hints
   the LLM can grep with.
4. Fall through to "no source hint" — send the outerHTML and the page
   URL, let Claude do the search.

### 6.5 Freehand drawing overlay

GPUI has no built-in stroke-capture primitive. New element in
`browser_viewer`:

```rust
// crates/browser_viewer/src/drawing.rs
pub struct DrawingCanvas {
    strokes: Vec<Stroke>,
    current_stroke: Option<Vec<Point<Pixels>>>,
    stroke_color: Hsla,
    stroke_width: Pixels,
}

pub struct Stroke {
    points: Vec<Point<Pixels>>,
    color: Hsla,
    width: Pixels,
}
```

The canvas captures `mouse_down → mouse_move → mouse_up` sequences
into stroke vectors. Rendering uses GPUI's existing path drawing
(`Path::new().move_to().line_to()...`). On submit, strokes serialize
to SVG, which goes into the ACP message as either a separate image
block or an overlay merged onto the screenshot.

When the drawing canvas is active, it is positioned in GPUI's
z-order *above* the browser visual. Mouse events on the canvas don't
forward to WebView2 (drawing pre-empts the page). When the user
exits draw mode, the canvas hides and pointer events flow through
to the browser again.

### 6.6 Session and storage

Each Zed installation gets a single WebView2 user data folder at:

```
%LOCALAPPDATA%\Zed\WebView2Data\
```

This is passed to `CoreWebView2EnvironmentOptions.UserDataFolder`.
All tabs in a Zed process share this — cookies, localStorage, cache,
service workers. Closing Zed and reopening preserves logins.

Future enhancement (out of scope for v1): per-project profiles so
that work for `Project A` doesn't share cookies with `Project B`.

### 6.7 Crate layout

```
crates/browser_viewer/
├── Cargo.toml
└── src/
    ├── browser_viewer.rs       # init(), public exports, ProjectItem registration
    ├── browser_item.rs         # BrowserItem (ProjectItem)
    ├── browser_view.rs         # BrowserView (Item, Render)
    ├── webview2_host.rs        # COM wrapping: env, controller, composition
    ├── input.rs                # GPUI event → WebView2 input translation
    ├── design_mode.rs          # injected JS, message routing, source detection
    ├── drawing.rs              # DrawingCanvas (stroke capture)
    └── address_bar.rs          # URL bar, nav buttons, find-on-page
```

Plus a single new file in `crates/gpui_windows/`:

```
crates/gpui_windows/
└── src/
    └── composition.rs          # HostedVisualHandle + WindowExt impl
```

### 6.8 Wiring into Zed

Mirror the `pdf_viewer` pattern in `crates/zed/src/`:

```rust
// crates/zed/src/main.rs (additions)
#[cfg(target_os = "windows")]
browser_viewer::init(cx);
```

And in `Cargo.toml` workspace members and `[workspace.dependencies]`,
plus `crates/zed/Cargo.toml` dependency (Windows-only `cfg`).

Settings under a new `browser` key (gated behind a feature flag for
the unstable preview period):

```jsonc
"browser": {
  "enabled": true,
  "default_search_url": "https://www.google.com/search?q={query}",
  "homepage": "about:blank",
  "design_mode_default_source_detector": "react"
}
```

---

## 7. Functional Requirements

### Core browser

| ID    | Requirement                                                                                                                                                  | Phase |
|-------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| FR-1  | A user can open a URL in a new Zed tab via a command-palette action `browser: open URL...`. The action prompts for a URL and accepts `http://` and `https://`. | 1     |
| FR-2  | A user can open multiple browser tabs in the same Zed window. Each tab has independent navigation history.                                                   | 1     |
| FR-3  | Each browser tab displays an address bar with the current URL, plus back, forward, and reload buttons.                                                       | 1     |
| FR-4  | Typing a URL into the address bar and pressing Enter navigates to that URL.                                                                                  | 1     |
| FR-5  | Typing a non-URL string and pressing Enter performs a search via the configured default search engine.                                                       | 1     |
| FR-6  | The browser tab title reflects the page's `<title>` and updates on navigation.                                                                               | 1     |
| FR-7  | The browser tab icon reflects the page favicon, falling back to a generic globe icon.                                                                        | 1     |
| FR-8  | Page rendering is hardware-accelerated and reaches at least 60fps on a representative dev-server page (Next.js or Vite default).                             | 2     |
| FR-9  | Click, scroll, hover, and keyboard interactions with the page work as in a normal Chrome window.                                                             | 3     |
| FR-10 | Standard browser keyboard shortcuts work: `Ctrl+L` (focus address bar), `Ctrl+R` (reload), `Ctrl+W` (close tab), `Ctrl+T` (new tab), `Ctrl+F` (find on page). | 3     |
| FR-11 | DevTools open in a separate window via `Ctrl+Shift+I` or a button in the address bar.                                                                        | 3     |
| FR-12 | Cookies and localStorage persist across Zed restarts.                                                                                                        | 1     |

### Design mode

| ID    | Requirement                                                                                                                                                                       | Phase |
|-------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| FR-20 | A "Design Mode" toggle button in the browser tab toolbar enables and disables design mode for that tab.                                                                           | 4     |
| FR-21 | While design mode is active, hovering an element in the page outlines it in a clearly-visible green border.                                                                       | 4     |
| FR-22 | Clicking an element in design mode selects it, freezes the highlight, and displays a "Describe the change" text input positioned next to the element (or below if space-limited). | 4     |
| FR-23 | While design mode is active and an element is selected, the user can draw freehand strokes on top of the page. Strokes are stored as vector paths.                                | 4     |
| FR-24 | Submitting the text input sends the bundle (screenshot, drawing SVG, element selector, outerHTML, source hint, prompt) to the active claude-acp session.                          | 4     |
| FR-25 | If no claude-acp session is active in the agent panel, submission creates a new one.                                                                                              | 4     |
| FR-26 | The source-detection cascade tries React `_debugSource`, then `data-source-*` attributes, then `data-component`/`data-testid`, then falls through to no hint.                     | 4     |
| FR-27 | Exiting design mode (toggle off, or Escape) removes the overlay and restores normal page interaction.                                                                             | 4     |

### Runtime and installation

| ID    | Requirement                                                                                                                                                                | Phase |
|-------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------|
| FR-30 | If the WebView2 Runtime is not installed, the first attempt to open a browser tab shows an error explaining the dependency and linking to Microsoft's installer page.      | 5     |
| FR-31 | If WebView2 initialization fails for any other reason, an error tab opens displaying the failure cause; the rest of Zed remains functional.                                | 5     |
| FR-32 | On non-Windows platforms (Phase 1 scope), the `browser_viewer` crate compiles as a no-op stub; calling any of its public API logs a "not supported on this platform" warning. | 1     |

---

## 8. Non-Functional Requirements

| ID     | Requirement                                                                                                                                            |
|--------|--------------------------------------------------------------------------------------------------------------------------------------------------------|
| NFR-1  | First browser tab opens within 800ms of the action being dispatched, on a machine with WebView2 Runtime already initialized.                           |
| NFR-2  | Subsequent browser tabs (with the WebView2 environment already up) open within 200ms.                                                                  |
| NFR-3  | Browser rendering maintains ≥55fps when scrolling typical dev-server pages on the reference hardware (Ryzen 7 9800X3D + Radeon RX 9070 XT).            |
| NFR-4  | Closing the last browser tab releases the WebView2 process tree within 5 seconds (no permanent process leak).                                          |
| NFR-5  | Memory overhead with a single browser tab open is ≤ 350MB above baseline Zed.                                                                          |
| NFR-6  | The Zed installer size does not grow more than 5MB on account of this feature (we rely on system-installed WebView2 Runtime, not bundled).             |
| NFR-7  | The `browser_viewer` crate compiles cleanly under `cargo clippy -- -D warnings`.                                                                       |
| NFR-8  | All new code follows `.rules` (no `unwrap()`, no silent error swallowing, no `mod.rs`, etc.).                                                           |
| NFR-9  | The feature is gated behind a settings flag `browser.enabled` and a Cargo `#[cfg(target_os = "windows")]` to make the upstream merge surface explicit. |
| NFR-10 | DevTools, when opened, do not block the host (they run in a separate window managed by WebView2).                                                      |

---

## 9. Implementation Phases & Acceptance Criteria

### Phase 0 — Spike (shipped 2026-05-27)

**Objective.** Prove WebView2 can render inside Zed's main window.
End state: a hardcoded action opens a WebView2 displaying
`https://example.com`, drawn into Zed's render tree.

**Implementation note — what we actually shipped.**

The original spec assumed Phase 0 would use child-HWND embedding,
with composition mode deferred to Phase 2. That assumption was wrong
on Zed specifically: GPUI renders via DirectComposition, and a child
HWND inside a DComp-parented window draws *behind* the parent's swap
chain — invisible. Phase 0 had to go straight to composition mode.

Two key adjustments were made during the spike and are now load-
bearing for everything that follows:

1. **Composition controller, not HWND controller.** WebView2 exposes
   `ICoreWebView2CompositionController` for OSR-style hosts. We hand
   it an `IDCompositionVisual` via `SetRootVisualTarget`; the
   underlying COM object also implements `ICoreWebView2Controller`,
   which we cast to in order to call `SetBounds`, `SetIsVisible`,
   and `CoreWebView2().Navigate(...)`.
2. **Async callbacks, not `wait_for_async_operation`.** The blocking
   helper pumps Windows messages while waiting for the COM
   completion, re-entering GPUI's window handlers and triggering
   `RefCell already borrowed` violations. We use `::create()` to
   register the handlers and let GPUI's normal message loop fire
   them; the chain is `env created → composition controller created
   → SetRootVisualTarget + Navigate`.

**GPUI extension.** A small `pub(crate)` API was added to
`crates/gpui_windows/src/dcomp_registry.rs`: a thread-local
HWND → `Arc<DirectComposition>` registry, populated when the
renderer constructs its DComp, plus a public
`create_child_visual_for_hwnd(hwnd) -> HostedVisual` that adds a
child visual to the window's root visual. `HostedVisual` owns a
strong reference and removes the visual on drop. Thread-local
because COM interfaces are STA-bound to the GPUI UI thread.

**Tasks (as shipped).**

- Added `webview2-com 0.38`, `windows` (workspace), `raw-window-
  handle`, `gpui_windows` deps to `crates/browser_viewer/` under
  `#[cfg(target_os = "windows")]` gating.
- Created `crates/browser_viewer/` with three modules:
  `browser_viewer.rs` (entry point), `webview2_host.rs` (async COM
  chain), `spike.rs` (action + HWND extraction + thread-local
  session storage).
- Made `crates/gpui_windows/src/directx_renderer.rs`
  `DirectComposition` accessible via the registry and wrapped its
  storage in `Arc`.
- Wired `browser_viewer::init(cx)` into `crates/zed/src/main.rs`.

**Acceptance criteria (revised).**

- [x] AC-P0-1: Running the action renders `example.com` inside
  Zed's window via DirectComposition. **PASSED.**
- [~] AC-P0-2: ~~Clicking links on the page navigates correctly.~~
  **MOVED to Phase 1.** Composition mode does not auto-route OS
  input to WebView2; the host must call `SendMouseInput` /
  `SendKeyEvent` explicitly. The right surface to wire this is the
  Phase 1 `BrowserView` GPUI element, not a Win32 subclass hook
  that would be thrown away in Phase 1.
- [x] AC-P0-3: Closing Zed does not leak `msedgewebview2.exe`
  processes. **PASSED** (informal check; to be revisited in
  Phase 5 with a proper soak test).
- [x] AC-P0-4: All new code is gated to Windows; cross-compile to
  Linux accepts the crate manifest (verified by inspection;
  blocked from full check by missing Linux std toolchain on the
  dev machine — non-blocking for this fork).

### Phase 1 — Tabs + Navigation + basic input (3 weeks)

**Objective.** Browser tabs are real Zed tabs, openable from the
command palette, navigable like a normal browser, with mouse clicks
working. Composition mode (from Phase 0) carries forward; the spike
action is replaced by a proper `BrowserView` GPUI element.

**Tasks.**

- Implement `BrowserItem` (ProjectItem) and `BrowserView` (Item).
- The `BrowserView` GPUI element calls
  `gpui_windows::create_child_visual_for_hwnd` on first render,
  positions the visual to its on-screen rect each layout pass, and
  drives `controller.SetBounds(rect_in_local_coords)` on resize.
- Hook URL navigation to `Workspace::open_path` via a custom
  `BrowserPath` that wraps a URL.
- Build the address bar component (URL input + back/forward/reload
  buttons + Design Mode toggle placeholder).
- Wire `webview2.NavigationStarting`, `NavigationCompleted`,
  `DocumentTitleChanged`, `FaviconChanged`, `HistoryChanged` events.
- Implement search-vs-URL heuristic for the address bar input.
- Multi-tab: each `BrowserView` owns its own controller; switching
  tabs hides/shows visuals via `HostedVisual` drop / re-create or
  `IDCompositionVisual::SetOffsetX/Y` off-screen.
- **Basic mouse input forwarding** (inherits AC-P0-2):
  `BrowserView.on_mouse_down/up/move/scroll` translate GPUI
  coordinates into browser-local coordinates and call
  `composition_controller.SendMouseInput(...)`. Full keyboard +
  IME deferred to Phase 3.
- Add settings stub: `browser.enabled`, `browser.default_search_url`,
  `browser.homepage`.
- Remove the Phase 0 spike action (`browser: open spike url`); the
  spike module is deleted.

**Acceptance criteria.**

- [ ] AC-P1-1: `Ctrl+Shift+P → browser: open URL` opens a tab that
  navigates to the given URL.
- [ ] AC-P1-2: Typing into the address bar and pressing Enter
  navigates. Plain text triggers a search.
- [ ] AC-P1-3: Back / Forward / Reload buttons function correctly,
  reflect history state (disabled when no history available).
- [ ] AC-P1-4: Tab title and favicon update on navigation.
- [ ] AC-P1-5: Opening a second browser tab in the same window works;
  switching between them preserves each tab's state.
- [ ] AC-P1-6: Closing a tab cleans up the controller; no zombie
  processes.
- [ ] AC-P1-7: After Zed restart, cookies set in the previous session
  are still present (verified by visiting a cookie-test page).
- [ ] AC-P1-8 (inherited from P0): clicking a link on a page navigates
  correctly. Mouse down + up are forwarded to WebView2 via
  `SendMouseInput`.
- [ ] AC-P1-9: The browser visual stays aligned with its `BrowserView`
  element through window resize, sidebar toggle, and tab switching.

### Phase 2 — Composition mode + GPU integration (shipped during Phase 0/1)

**Status: completed early.** The composition-mode work originally
scoped as a separate 2-3 week phase landed during Phase 0 (when the
child-HWND spike failed against Zed's DComp swap chain and we pivoted
straight to composition mode) and Phase 1 (when `dcomp_registry`
formalised the host-extension API in `gpui_windows`).

**What shipped (cross-referenced to original tasks).**

- `crates/gpui_windows/src/dcomp_registry.rs` exposes
  `create_child_visual_for_hwnd` returning a `HostedVisual` — the
  spec's planned `HostedVisualHandle` / `WindowExt::add_hosted_visual`
  pair, just shaped around our thread-local HWND→Weak<DirectComposition>
  registry instead of a window-method.
- `WebView2Host` uses
  `CreateCoreWebView2CompositionController` via the async
  callback chain in `webview2_host::initialize` — no
  `wait_for_async_operation` to avoid re-entering GPUI's message pump.
- `SetRootVisualTarget` binds the composition controller to the
  hosted `IDCompositionVisual`; positioning + bounds are pushed every
  GPUI prepaint via `WebView2Session::set_rect`.

**Acceptance criteria (all verified during Phase 1 dogfooding).**

- [x] AC-P2-1: Composition mode means no child HWND for the page —
  WebView2 draws directly into a DComp visual under Zed's swap chain.
- [x] AC-P2-2: GPUI overlays render *above* the page (proper fix
  shipped in Phase 4 — modal-hide workaround removed). Architecture:
    1. DComp tree restructured so a new `comp_container` is the root
       with two children — `comp_visual` (GPUI's swap-chain holder,
       front-most) and a per-browser-tab WebView2 underlay (behind).
       New API: `gpui_windows::create_underlay_visual_for_hwnd`.
    2. New GPUI scene primitive `Cutout` — inserted via
       `Window::paint_cutout(bounds)`. The Windows D3D11 renderer
       handles the `PrimitiveBatch::Cutouts` arm by calling
       `ID3D11DeviceContext1::ClearView` with `[0, 0, 0, 0]`, which
       writes raw alpha=0 pixels into the swap-chain RTV bypassing
       blend state. Mac/wgpu renderers no-op the batch.
    3. `BrowserViewportElement::paint` emits a Cutout for the viewport
       region at its z-position. Workspace bg paints first → cutout
       punches alpha=0 → underlay shows through → modals/popovers/
       drawing strokes painted afterwards remain opaque on top.
  The original Phase 1.E "hide WebView on modal open" workaround was
  deleted; modals now naturally render above the page via z-order.
- [x] AC-P2-3: Resize tracks smoothly. `prepaint` runs every layout
  pass; `set_rect` updates DComp offsets + controller bounds +
  notifies parent-window position changes atomically.
- [x] AC-P2-4: Sidebar / agent-panel dock toggles reflow cleanly
  (verified left/right toggles in Phase 2 close-out, 2026-05-28).
- [x] AC-P2-5: WebGL hardware acceleration intact. Aquarium demo
  with 30 000 fish > 100 FPS on the dev workstation.

### Phase 3 — Input + UX polish (2 weeks)

**Objective.** All input and shortcuts feel like a real browser.

**Tasks.**

- Full input bridging per §6.3 table.
- Focus management: pressing Tab into a browser tab focuses the page;
  pressing Tab out moves to the next GPUI element.
- IME support for English and a CJK locale (Japanese or Chinese as
  the test bed).
- Find-on-page UI (`Ctrl+F`) — a Zed-native input bar that calls
  WebView2's find API.
- DevTools button + `Ctrl+Shift+I` shortcut → opens DevTools in a
  separate window via `controller.CoreWebView2.OpenDevToolsWindow()`.
- Right-click context menu: use WebView2's built-in initially; future
  enhancement to integrate with Zed's menu system.
- Drag-and-drop into the browser (file uploads) works.
- Copy/paste between browser and Zed editor tabs.

**Acceptance criteria.**

- [ ] AC-P3-1: All shortcuts from FR-10 work as specified.
- [ ] AC-P3-2: IME composition works in a CJK locale (visual smoke
  test with Japanese hiragana input on Google Translate).
- [ ] AC-P3-3: Pasting from Zed into a page text field works; copying
  from a page and pasting into a Zed editor tab works.
- [ ] AC-P3-4: Pressing `Tab` cycles focus between page elements
  while focus is in the browser; another `Tab` at the end of the
  page moves focus to the next GPUI element.
- [ ] AC-P3-5: DevTools opens in a separate window and reflects the
  active tab's page.
- [ ] AC-P3-6: File-upload `<input type="file">` works (drag-drop
  and click-to-pick).

### Phase 4 — Design Mode (2 weeks)

**Status: COMPLETE (2026-05-29).** The select → describe → annotated
screenshot + element context → claude-acp edit loop works end to end, with
a redesigned draggable panel and React-19 source detection. Closed out after
an adversarial review pass (9 findings fixed). Remaining items are
polish/hardening under Phase 5 + the deferred list in CLAUDE.md.

**Objective.** The motivating feature lands. Element selection,
freehand drawing, and submission to claude-acp.

**Tasks.**

- Implement the injected design-mode script per §6.4.
- Implement message-passing host side: `WebMessageReceived` handler
  routes `element_selected` to the view.
- Build the "Describe the change" text input UI, positioned near the
  selected element (anchored within the browser visual's coordinate
  space, repositioned on scroll).
- Implement `DrawingCanvas` per §6.5.
- Implement screenshot capture via
  `controller.CapturePreviewAsync(PreviewKindPng, stream)`.
- Implement the submission pipeline: bundle artifacts and call into
  the agent panel via `claude-acp` ACP `prompt` with image content
  blocks. **DONE (Phase 4.F).** `on_design_submit` composites the
  selected-element outline + freehand strokes onto the screenshot,
  base64-encodes it (≤2 MB), and dispatches
  `zed_actions::agent::SendDesignBundleToAgent`; `agent_ui` builds the
  ACP prompt (text + image + embedded-HTML resource) and sends it into
  the active claude-acp thread, or opens a new one if none is live.
  The drawing rides as a burned-in annotation on the screenshot rather
  than a standalone SVG, so Claude sees the scribble in context.
- React `_debugSource` detection.
- `data-source-*` and `data-component`/`data-testid` fallbacks.
- Visual indicator (e.g., a green status pill) showing design mode
  is active, mirroring the affordance from the Cursor screenshot.

**Acceptance criteria.**

- [x] AC-P4-1: Toggling Design Mode shows hover outlines on page
  elements within 100ms. (verified)
- [x] AC-P4-2: Clicking an element shows the "Describe the change"
  input next to it. (verified — redesigned as a modern, draggable card)
- [x] AC-P4-3: Drawing strokes appears on top of the page without
  affecting the page's own pointer events when drawing mode is on.
  (verified end to end, composited onto the dispatched screenshot)
- [x] AC-P4-4: Submitting sends an ACP message to the agent panel
  containing: screenshot (PNG ≤ 2MB), drawing, element selector,
  outerHTML excerpt, source hint, prompt text. **Done & verified
  (Phase 4.F)** — the drawing is composited onto the screenshot rather
  than sent as standalone SVG. Verified end-to-end against a Next.js
  dev app: the agent received the request text + selector + embedded
  outerHTML resource + screenshot and edited the correct source file.
  `ZED_BROWSER_DESIGN_DEBUG_BUNDLE=1` dumps the annotated PNG to
  `%TEMP%` for inspection. (`source_hint` requires React DevTools
  `_debugSource`; absent on prod-ish builds — that's AC-P4-5's path.)
- [~] AC-P4-5: Source identity of the clicked component. **Reframed for
  React 19:** React 19 removed fiber `_debugSource`, so exact file:line is
  no longer available at runtime on modern stacks. `detectReactSource` now
  surfaces the nearest **component name** (verified on a React 19 / Next.js
  dev app) and still reads `_debugSource` (React < 19) + `data-source-*`
  attributes for file:line when present. Claude greps the component name to
  locate the source — confirmed sufficient in practice.
- [~] AC-P4-6: Escape. The describe panel's Esc clears the current
  selection + overlay, closes the panel, and refocuses the browser root. It
  does NOT fully toggle design mode off (`ToggleDesignMode` has no
  keybinding; exit is via the crosshair button). Two-level Esc
  (clear selection → exit design mode) is deferred polish.

### Phase 5 — Production polish (1–2 weeks)

**Objective.** Ship-ready behavior on edge cases and errors.

**Tasks.**

- Detect missing WebView2 Runtime; show a helpful error tab with the
  Microsoft install link.
- Handle initialization failures gracefully (corrupted user data
  folder, sandbox policy issues).
- Settings UI (or just documentation): default search engine,
  homepage, design-mode source detector preference.
- Performance profiling: measure tab-open latency, sustained
  scrolling FPS, memory steady state. Tune.
- Telemetry (local logs only, no remote): tab-open count, design-
  mode invocations, error rates.
- Update `CLAUDE.md` with usage instructions and architecture notes.
- README in `crates/browser_viewer/` summarizing the feature.

**Acceptance criteria.**

- [ ] AC-P5-1: On a clean Windows 10 VM without WebView2 Runtime,
  the first browser-open action shows a clear error pointing at the
  installer URL.
- [ ] AC-P5-2: Tab-open latency meets NFR-1 / NFR-2 on the reference
  hardware, measured with three trials each.
- [ ] AC-P5-3: Sustained scroll FPS ≥ 55 on the reference page set
  (Next.js docs, Vite docs, MDN).
- [ ] AC-P5-4: Closing 10 browser tabs in sequence brings WebView2
  process count back to 1 (the persistent browser controller) or 0
  (idle) within 10 seconds.
- [ ] AC-P5-5: `cargo clippy -p browser_viewer -- -D warnings` passes.
- [ ] AC-P5-6: CLAUDE.md updated with usage + architecture.

---

## 10. Risks & Mitigations

| Risk                                                                                                                       | Likelihood | Impact | Mitigation                                                                                                                                              |
|----------------------------------------------------------------------------------------------------------------------------|------------|--------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| GPUI composition tree exposes private state, requiring deeper changes than `HostedVisualHandle` to wire WebView2 visuals.  | Medium     | High   | Spike in Phase 0 directly exercises the integration. If it requires reshaping GPUI's window lifecycle, descope to child-HWND-only and accept z-order limitations until a cleaner GPUI extension can be designed. |
| WebView2 composition mode has unresolved DPI scaling issues on multi-monitor / fractional DPI setups.                      | Medium     | Medium | Test on multi-monitor early in Phase 2. Workaround: scale the visual transform manually based on `IDCompositionVisual.SetTransform`.                    |
| Input forwarding produces double-input or focus loops when GPUI and WebView2 disagree about focus state.                   | High       | Medium | Phase 3 dedicates time to focus state machine. Build an explicit "browser has focus" flag in `BrowserView`; only forward inputs when set; on focus loss, send `LostFocus` to WebView2 explicitly.                              |
| React source detection fails for production builds, modern frameworks (Solid, Svelte), or apps using SSR streaming.        | High       | Low    | Detection is best-effort. Fall through to "no source hint" gracefully. Document framework support in CLAUDE.md.                                          |
| The `webview2-com` crate has gaps in its API coverage requiring contributions upstream or direct COM raw calls.            | Medium     | Medium | We're already a Rust shop; raw COM via `windows` crate is acceptable. Document any patches needed if `webview2-com` requires forking.                   |
| WebView2 Runtime updates change behavior of `CapturePreviewAsync` or other APIs we depend on.                              | Low        | Medium | Lock minimum WebView2 version in initialization; show a warning if user's installed runtime is older than tested.                                       |
| Memory grows unboundedly with long-running design-mode sessions due to retained screenshots.                               | Medium     | Low    | Cap the per-session screenshot cache to ≤ 5 images; flush on tab close.                                                                                  |
| Upstream Zed adds its own browser-pane feature with conflicting names/architecture.                                        | Low        | Medium | Keep all code under `browser_viewer` namespace, distinct from any upstream `browser` or `web_view`. Periodic check of upstream PR list.                  |
| The design-mode workflow turns out to be less useful in practice than expected.                                            | Medium     | Low    | Phase boundaries deliver value: even Phase 1 (browser tabs) is useful standalone. Re-evaluate after Phase 3 whether to invest in Phase 4.                |

---

## 11. Open Questions

The following decisions are deferred to implementation start. Each
should be resolved before the corresponding phase begins.

- **OQ-1 (Phase 1).** Should browser tabs use a custom `browser:` URI
  scheme, or should `http://` / `https://` paths be intercepted
  globally? Custom scheme is less invasive but requires explicit
  user action. Global interception is more "browser-like" but risks
  conflicting with Markdown/Anchor link handling. **Recommendation:**
  custom scheme via command palette only in Phase 1; add a "open
  link in Zed browser" context-menu in Phase 3.
- **OQ-2 (Phase 1).** Should tabs persist across Zed restarts (like
  Chrome's "restore tabs")? **Recommendation:** yes, treat browser
  tabs like any other Zed pane item — workspace state already
  handles this if `BrowserItem` is restorable.
- **OQ-3 (Phase 4).** Default source detector — React only, or
  multi-framework from day one? **Recommendation:** ship React first
  since that's the user's stack (per the screenshots); add Vue/Svelte
  in a follow-up if needed.
- **OQ-4 (Phase 4).** Where does the "Describe the change" input
  send the prompt — always claude-acp, or to whichever agent is
  active in the panel? **Recommendation:** always claude-acp, since
  this fork has gated the panel to claude-acp anyway.
- **OQ-5 (Phase 4).** Should the design-mode draw layer include
  text annotations (e.g., draggable labels) in addition to freehand
  strokes? **Recommendation:** strokes only in v1; text in v2 if
  asked.
- **OQ-6 (Phase 5).** Are per-project browser profiles needed
  (separate cookies/storage per Zed workspace), or is a single
  global profile sufficient? **Recommendation:** single global
  profile in v1; per-project in v2 if friction shows up.

---

## 12. References

- [WebView2 documentation](https://learn.microsoft.com/en-us/microsoft-edge/webview2/) — Microsoft's primary docs for the SDK.
- [WebView2 composition](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution#composition) — composition-mode-specific guidance.
- [`webview2-com` crate](https://crates.io/crates/webview2-com) — Rust COM bindings, primary dependency.
- [`windows` crate](https://crates.io/crates/windows) — official Microsoft Rust bindings for Windows APIs (DComp, COM, HWND interop).
- [DirectComposition overview](https://learn.microsoft.com/en-us/windows/win32/directcomp/directcomposition-portal) — the Windows compositor we'll be integrating with.
- [Avalonia.WebView source](https://github.com/AvaloniaUI/Avalonia.WebView) — reference implementation of "custom UI + embedded browser" in another framework.
- [CEF OSR docs](https://bitbucket.org/chromiumembedded/cef/wiki/GeneralUsage.md) — for the eventual cross-platform port (not used in v1, but informs the architecture so the transition is clean).
- This fork's `crates/pdf_viewer/` — pattern reference for "external content rendered in a Zed tab."
