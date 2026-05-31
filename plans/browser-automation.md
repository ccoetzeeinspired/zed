# Browser Automation — Status & Specification

**Status:** CP0–CP8 shipped and verified (19-tool surface; CP8 = input
interactions: file_upload, drag, drop, handle_dialog); CP9+ roadmap to **full**
Playwright MCP parity in §9  
**Branch:** `browser-automation` (off `browser-viewer`)  
**Platform:** Windows only (WebView2 / CDP)  
**Last updated:** 2026-05-31

---

## 1. What this is

Agent-driven control of the **embedded Zed browser tab** (WebView2), not a
separate browser process and not coordinate-based clicking. The claude-acp
panel drives the tab through MCP tools registered in Zed `context_servers`.

Typical agent loop:

```text
browser_navigate → browser_wait_for → browser_snapshot → browser_type / browser_click → …
```

**Goal: Playwright MCP parity.** The agent-facing contract tracks Microsoft's
[`@playwright/mcp`](https://github.com/microsoft/playwright-mcp) beat for beat —
same tool names, same accessibility-snapshot-driven model, same opaque `ref=eN`
handles, same arg schemas and auto-wait semantics. An agent that can drive
`@playwright/mcp` should drive `zed-browser` with no relearning. The only
difference is the target: the embedded WebView2 tab via
`CallDevToolsProtocolMethod`, not a spawned Chromium. When in doubt on a tool's
shape or behavior, match Playwright MCP rather than inventing our own.

**Principles**

- CDP / DOM on resolved element refs (`e1`, `e2`, … from snapshots).
- No visible automation cursor; human browsing unchanged.
- All automation code lives under `crates/browser_viewer/src/automation/`.
- Design mode (human pick → design bundle → agent) stays separate.

**Integration:** Rust automation core in `browser_viewer`, stdio MCP adapter
at `vendor/zed-browser-mcp/`, loopback TCP IPC to Zed, forwarded to claude-acp
via existing ACP `mcpServers` plumbing from configured `context_servers`.

---

## 2. Architecture

```text
┌─────────────────────────────────────────────────────────────┐
│  claude-acp agent panel                                      │
│    tool calls → vendor/zed-browser-mcp (stdio MCP)          │
└───────────────────────────┬─────────────────────────────────┘
                            │ MCP JSON-RPC (stdio)
                            ▼
┌─────────────────────────────────────────────────────────────┐
│  zed-browser-mcp (Node)                                      │
│    maps tools → loopback TCP JSON lines                      │
└───────────────────────────┬─────────────────────────────────┘
                            │ 127.0.0.1:19382 (default)
                            ▼
┌─────────────────────────────────────────────────────────────┐
│  browser_viewer::automation (Rust, GPUI foreground)          │
│    target resolution → CdpSession → snapshot / click / type    │
└───────────────────────────┬─────────────────────────────────┘
                            │ CallDevToolsProtocolMethod
                            ▼
┌─────────────────────────────────────────────────────────────┐
│  WebView2Session (active BrowserView tab)                    │
└─────────────────────────────────────────────────────────────┘
```

**Tab targeting:** Prefer the workspace active item when it is a `BrowserView`;
otherwise the most recently activated browser tab across open workspaces
(`resolve_automation_target_global`).

**IPC protocol:** One JSON line per request/response.

```json
{"id":"…","method":"snapshot","params":{}}
{"id":"…","ok":true,"result":{"yaml":"…","ref_count":42}}
```

Methods: `ping`, `snapshot`, `click`, `type`, `navigate`, `wait_for`.  
Override port with env `ZED_BROWSER_AUTOMATION_PORT`.

---

## 3. Shipped tool surface (Tier 1 + Tier 2)

| MCP tool | Rust IPC method | Notes |
|----------|-----------------|-------|
| `browser_navigate` | `navigate` | `ICoreWebView2.Navigate` + wait for load |
| `browser_snapshot` | `snapshot` | CDP AX tree → YAML + `ref=eN` registry |
| `browser_click` | `click` | Ref → actionability → `Runtime.callFunctionOn` |
| `browser_type` | `type` | Native input setter + React `_valueTracker` sync |
| `browser_wait_for` | `wait_for` | Load, text in title/body, or sleep seconds |
| `browser_press_key` | `press_key` | Playwright-style key spec → CDP `Input.dispatchKeyEvent` (keyDown+keyUp); also backs `type`'s `submit:true` |
| `browser_scroll` | `scroll` | `ref` → `scrollIntoView` (handles inner scrollers); else `window.scrollBy(dx,dy)`. Returns `{x,y,maxY}`. Fork extension (Playwright MCP has no scroll tool). |
| `browser_tabs` | `tabs` | `action` = list / select / new / close (+ `index`, `url`). Operates on the **workspace** (`items_of_type::<BrowserView>`, `activate_item`, `close_item_by_id`), not CDP. Returns `{tabs:[{index,title,url,active}],count}`. |
| `browser_take_screenshot` | `screenshot` | CDP `Page.captureScreenshot`. `fullPage` (captureBeyondViewport), `ref` (element clip in page coords), `type` png/jpeg + `quality`. Returns an MCP image content block. |
| `browser_evaluate` | `evaluate` | `Runtime.evaluate` `(fn)()` (awaits promises, surfaces `exceptionDetails`); with `ref`, calls `fn` with the element as `this`+arg0. Returns `{result}`. |
| `browser_select_option` | `select_option` | Set `<select>` option(s) by value/label/text (+ `input`/`change`). Returns `{matched,value}`. |
| `browser_hover` | `hover` | Scroll element to centre, dispatch CDP `Input.dispatchMouseEvent` `mouseMoved` → real CSS `:hover`. |
| `browser_navigate_back` | `navigate_back` | WebView2 `GoBack` + wait for load (CP7). |
| `browser_fill_form` | `fill_form` | Batch fields by kind: text / checkbox-radio / select (CP7). |
| `browser_close` | `close` | Close the active browser tab (workspace `close_item_by_id`) (CP7). |
| `browser_file_upload` | `file_upload` | CDP `DOM.setFileInputFiles` (paths on disk) (CP8). |
| `browser_drag` | `drag` | Mouse drag press→move→release between two element centres (CP8). |
| `browser_drop` | `drop` | Synthetic HTML5 drop (data/MIME; not files — use file_upload) (CP8). |
| `browser_handle_dialog` | `handle_dialog` | JS-override of alert/confirm/prompt via `evaluate`; arm-then-trigger (CP8). |

**CP6–CP8 complete** — 19 tools shipped (Tier 1 + Tier 2 parity fills + input interactions).

**Key-dispatch gotcha (learned in CP6):** Enter must carry `text:"\r"` in the
keyDown, or Chromium never fires the `keypress`/`char` event — `keydown` alone
fires (so arrow-key nav and suggestion-select work) but **implicit form
submission / SPA Enter handlers do not**. Symptom: Enter "does nothing" while
arrows work. Matches Playwright's US layout (Enter→`\r`). See
`automation/keys.rs`. The human-typing path (`browser_view::keystroke_to_cdp`)
still maps Enter to `None` — same latent gap if SPA Enter-submit is ever needed
there.

---

## 4. Checkpoint status

| CP | Scope | Status |
|----|--------|--------|
| **CP0** | CDP transport, `Accessibility.enable` smoke test | Done |
| **CP1** | Snapshot + ref registry, invalidation on navigation | Done |
| **CP2** | Click + actionability auto-wait (5s) | Done |
| **CP3** | Type into inputs (incl. React controlled fields) | Done |
| **CP4** | Navigate + wait-for load/text | Done |
| **CP5** | MCP adapter + `context_servers` + end-to-end agent | Done |
| **CP6** | Tier 2 breadth (press_key, scroll, tabs, screenshot, evaluate, select_option, hover) | Done |
| **CP7** | Parity fills (click options, wait_for textGone, type slowly, navigate_back, fill_form, close) | Done |
| **CP8** | Input interactions (file_upload, drag, drop, handle_dialog) | Done |

### Verification log (2026-05-30)

- **Dev palette:** TrueLens login via `browser: automation *` actions (email,
  password, Sign In) after React input fix.
- **Agent panel:** Full MCP flow on TrueLens — navigate, wait, snapshot, type
  credentials, click Sign In → landed on `/dashboard` with admin user table
  visible. Agent summarized sidebar + Active Users table from post-login
  snapshot.

### Verification log (CP6, `browser_press_key`)

- **Unit:** `automation::keys` — 9 tests (key specs, modifier chords, Enter
  text payload).
- **Runtime (via MCP server, real pages):**
  - Google — `z`/`e`/`d` as three standalone `browser_press_key` calls →
    `q=zed`; Enter submitted the search form (real URL nav). Confirms char +
    Enter dispatch.
  - Takealot — typing opened the autocomplete (input events fired), `ArrowDown`
    moved the highlight. Plain Enter initially did **nothing** → root-caused to
    the missing `text:"\r"` (no `keypress`). After the fix, type "mechanical
    keyboard" + Enter navigated to the results page (filters + product grid),
    **visually confirmed by the user**.

### Verification log (CP6, `browser_scroll`)

- **Unit:** `automation::commands` — CDP `returnByValue` unwrap (`{type,value}` →
  inner).
- **Runtime (Takealot results page, ~6000px):** viewport delta `0→1500→3000`;
  `dy:100000` clamped to `y=maxY=5954`; `dy:-100000` → `y=0`; element-into-view
  `ref` (Brand filter) from top → `y=369`; top-nav `ref` from bottom → `y=0`.
  All confirmed by the deterministic `{x,y,maxY}` return (page `window.scrollX/Y`).

### Verification log (CP6, `browser_tabs`)

- **Unit:** `automation::tabs` — `build_json` indexing / count / active marking.
- **Runtime (via MCP server):** from a single TrueLens tab — `list` (1, active);
  `new https://www.google.com/` → 2 tabs, new one active; `list` (Google title
  resolved, active `[1]`); `select 0` → active flips to TrueLens; `close 1` →
  back to 1 tab. Tab focus + close **visually confirmed by the user** (workspace
  z-order / WebView2 underlay correct).

### Verification log (CP6, `browser_take_screenshot`)

- **Unit:** `automation::commands` — `clip_from_rect` (builds scaled clip;
  rejects zero-size).
- **Runtime (Takealot homepage, decoded PNGs inspected):** viewport (326 KB,
  valid PNG sig, above-the-fold render); `fullPage` (3.16 MB, full scrollable
  page incl. footer — footer legibly confirmed via a bottom-of-page viewport
  capture); element `ref=e101` (1.7 KB, just the search box). Confirms CDP
  `Page.captureScreenshot` works in composition-mode WebView2 — no
  `CapturePreview` fallback needed.

### Verification log (CP6, `browser_evaluate` / `browser_select_option` / `browser_hover`)

- **Unit:** `automation::ipc` — `parse_string_list` (string | array | reject).
- **Runtime (via MCP server, Amazon.co.za + injected control `<select>`):**
  - `evaluate` page-level `() => ({math:6*7,…})` → `42` + real title/url/ua;
    element-level `el => …` on the select ref → `"SELECT with 3 options"`.
  - `select_option ["Large"]` → `{matched:1, value:"large"}`, independently
    cross-checked via `evaluate` reading `.value` → `"large"`.
  - `hover` the select, then `evaluate document.querySelectorAll(":hover")` →
    chain ends at `SELECT#__zed_test_sel` (real CSS `:hover`, not just an event).

**Evidence method (adopted this run):** verify against ground truth — return
values, an independent `evaluate` read-back, or a *viewport-resolution*
screenshot of the region (full-page PNGs downscale too far to read fine text).
Don't infer success from the AX snapshot alone.

### Verification log (CP7 — user-confirmed, step-by-step on naledi.co.za)

Each step driven through the MCP server and **visually confirmed by the user**
against on-page fixtures (no AX-only inference):

- `navigate_back` — naledi → example.com → `GoBack` → back to naledi (page
  watched changing each time).
- `type slowly` — text appeared in a visible input via per-char key events
  (instant in practice; see follow-up on a configurable delay).
- `fill_form` — text box, checkbox, and `<select>` all changed together
  (`FORM-FILLED` / ticked / "Large").
- click options — on-page click-log captured: right → `auxclick`+`contextmenu`
  (button=2); double → `click(detail1)`,`click(detail2)`,`dblclick`; shift →
  `click shift=true`.
- `wait_for textGone` — a banner scheduled to vanish at ~3s; the call blocked
  the full ~3s and returned as it disappeared.
- `browser_close` — opened a 2nd tab (watched it appear + activate), closed it,
  back to one tab.

### Verification log (CP8 — user-confirmed, step-by-step on example.com)

Driven through the MCP server against a visible on-page panel (file input +
SRC/TGT/DROP buttons + live readouts); each action **visually confirmed by the
user** plus a tool-side `evaluate` read-back:

- `file_upload` — uploaded `zed-upload-test.txt`; filename showed on the input;
  `input.files` = {count 1, name, size 38}.
- `drag` — SRC→TGT; recorder logged `SRC pointerdown/mousedown` then
  `TGT pointerup/mouseup` (press on source, release on target).
- `drop` — synthetic drop delivered `DROPPED_PAYLOAD_42` to the target's `drop`
  listener.
- `handle_dialog` — armed accept+text → `confirm()=true`, `prompt()="…"`; armed
  dismiss → `confirm()=false`, `prompt()=null`; **no dialog box popped** (JS
  override, by design).

### Follow-ups discovered during CP7 verification — ALL FIXED + user-verified

Per the "fix anomalies each CP" practice, all four were fixed in the CP7 batch
(issues #9–#12) and re-verified on-screen:

1. **Address bar (`url_editor`) stale after navigation** — ✅ **fixed.** Root
   cause was the navigation race in #2 below: `item.url` (and the render-time
   editor sync) lagged because `navigate` returned before `NavigationStarting`.
   Fixing #2 fixed this; the address bar now tracks the page. Verified
   (truelens → example.com → bar updated).
2. **Post-navigation `evaluate` execution-context race** — ✅ **fixed.**
   `automation_navigate` now pre-sets `is_loading=true` (like `automation_go_back`)
   so wait-for-load can't return before the nav starts. Verified: immediate
   `location.href` after navigate returns the new URL, not stale.
3. **Snapshot ~500-ref cap** — ✅ **fixed.** Raised default to 2000
   (`ZED_BROWSER_AUTOMATION_MAX_REFS` override). Verified: naledi snapshot now
   1711 refs and an appended end-of-body marker is reachable (`e1711`).
4. **`type slowly` configurable delay** — ✅ **done.** Added `slowlyDelayMs`
   (per-character delay). Verified at 150ms/char (watched typing letter by
   letter). Use to pace human-like input vs anti-bot/legacy-site handling.

### Known fixes during CP5 dogfood

1. **React controlled inputs** — plain `element.value = …` updated the DOM but
   not React state; sign-in submitted empty fields. Fixed in `action.rs`
   (native setter + `InputEvent` + blur).
2. **GPUI re-entrant borrow on MCP click/type** — MCP path called
   `browser.read()` inside `browser.update()` when resolving refs; dev actions
   did not. Fixed in `commands.rs` (resolve refs via `cx.update(|app| …)` only).
3. **IPC serialization** — global mutex around MCP dispatch to avoid interleaving
   with navigation event handlers on the same tab.

---

## 5. File layout

```text
crates/browser_viewer/src/
  automation/
    mod.rs          — public API, dev actions wiring
    cdp.rs          — CdpSession, parse_cdp_response
    target.rs       — per-workspace + global tab resolution
    session.rs      — RefRegistry, page generation on navigation
    snapshot.rs     — AX tree → YAML (iterative walk)
    action.rs       — click/type CDP scripts, actionability
    navigate.rs     — URL normalize, wait-for
    commands.rs     — awaitable MCP/IPC command handlers
    ipc.rs          — TCP listener + GPUI dispatch loop
  webview2_host.rs  — call_devtools_protocol
  browser_viewer.rs — dev actions + init_automation_ipc

vendor/zed-browser-mcp/
  src/index.ts      — stdio MCP server (Tier 1 tools)
  src/ipc.ts        — TCP client to Zed
  README.md           — setup + settings snippet

docs/superpowers/browser-automation-test.html  — local test page (optional)
plans/browser-automation.md                    — this document
```

---

## 6. Setup (one-time + settings)

### Build MCP adapter

```powershell
cd D:\src\zed\vendor\zed-browser-mcp
npm install
npm run build    # writes dist/ (gitignored)
```

### Zed settings (`%APPDATA%\Zed\settings.json`)

```jsonc
"context_servers": {
  "zed-browser": {
    "command": "node",
    "args": ["D:/src/zed/vendor/zed-browser-mcp/dist/index.js"]
  }
},
"browser": {
  "homepage": "https://truelens.co.za/",
  "automation_credentials": { "email": "…", "password": "…" },
  "automation_dev_wait_text": "TrueLens"
}
```

Enable `zed-browser` in the agent profile (or `enable_all_context_servers`).
Restart Zed after rebuilding `target/debug/zed.exe`.

### Agent prompt pattern

Open a browser tab first (`browser: new tab`). Then ask the agent to use
**zed-browser** MCP: navigate → wait → **snapshot** (refs reset after navigation)
→ type/click using refs from that snapshot.

Example:

```text
Use zed-browser MCP: navigate to https://truelens.co.za/, wait for the login
page, browser_snapshot, type my credentials into the email and password fields,
click Sign In.
```

### Dev palette actions (still available)

Registered under `browser:` for manual testing without MCP:

- `browser: automation smoke test` / `snapshot` / `navigate homepage`
- `browser: automation wait for load` / `wait for text`
- `browser: automation type email` / `type password` / `click sign in`

---

## 7. Build & test

From `D:\src\zed`:

```powershell
cargo build -p browser_viewer -j 4
cargo build -p zed -j 4
cargo test -p browser_viewer automation -j 4
```

Zed logs IPC calls as `browser automation IPC: <method> …`. Set
`ZED_BROWSER_AUTOMATION_DEBUG=1` to dump snapshots under
`%TEMP%\zed-browser-automation\`.

---

## 8. CP6 — Tier 2 (done)

Priority order from dogfood:

1. ~~**`browser_press_key`**~~ — **Done.** Key spec → CDP, backs `submit:true`.
   Verified on Google + Takealot.
2. ~~**`browser_scroll`**~~ — **Done.** Viewport delta + element-into-view,
   returns `{x,y,maxY}`. Verified on Takealot results.
3. ~~**`browser_tabs`**~~ — **Done.** list / select / new / close over workspace
   browser tabs. Verified end-to-end.
4. ~~**`browser_take_screenshot`**~~ — **Done.** CDP `Page.captureScreenshot`;
   viewport / full-page / element. Verified (PNGs inspected).
5. ~~**`browser_select_option`**, **`browser_evaluate`**, **`browser_hover`**~~ —
   **Done.** Verified end-to-end. **CP6 complete.**

Deferred / non-goals:

- Cross-platform (macOS/Linux).
- IME and dead-key input via agent.
- ACP host-side tool execution without MCP (no upstream hook today).

> Note: coordinate/Sikuli automation was previously a non-goal. As of the
> full-parity decision (§9) it is **in scope** (CP9, vision tools) — ref-based
> stays primary, coordinate is the fallback.

---

## 9. Full-parity roadmap (CP7+)

**Goal:** literal Playwright MCP parity across every tool that maps onto an
embedded WebView2 tab, with a small set of **documented divergences** for tools
bound to Playwright's own runtime/infrastructure (which an embedded browser
cannot and should not replicate). Decisions taken with the user: include the
**vision** (coordinate) tools; treat infra-specific tools as divergences with
analogs; do read-only **network** observation now and defer request mocking.

### 9.1 Parity matrix (every Playwright MCP tool → our disposition)

Legend: ✅ shipped · 🔧 enhance existing · ➕ new · ⛔ divergence (analog noted).

| Playwright tool | Status | Where |
|-----------------|--------|-------|
| `browser_navigate` | ✅ | CP4 |
| `browser_snapshot` | ✅ | CP1 |
| `browser_click` | ✅ doubleClick / button / modifiers | CP7 |
| `browser_type` | ✅ `slowly` | CP7 |
| `browser_wait_for` | ✅ `textGone` | CP7 |
| `browser_press_key` | ✅ | CP6 |
| `browser_select_option` | ✅ | CP6 |
| `browser_hover` | ✅ | CP6 |
| `browser_evaluate` | ✅ | CP6 |
| `browser_take_screenshot` | ✅ | CP6 |
| `browser_scroll` (fork ext.) | ✅ | CP6 |
| `browser_tabs` | ✅ | CP6 |
| `browser_navigate_back` | ✅ `GoBack` | CP7 |
| `browser_fill_form` | ✅ batch type over refs | CP7 |
| `browser_close` | ✅ close active tab/page | CP7 |
| `browser_file_upload` | ✅ CDP `DOM.setFileInputFiles` | CP8 |
| `browser_drag` | ✅ CDP `Input` mouse drag (press→move→release) | CP8 |
| `browser_drop` | ✅ synthetic HTML5 drop (data/MIME; not files — use file_upload) | CP8 |
| `browser_handle_dialog` | ✅ JS-override (`evaluate`) — arm-then-trigger; not native `ScriptDialogOpening` | CP8 |
| `browser_mouse_click_xy` / `_move_xy` / `_down` / `_up` / `_drag_xy` / `_wheel` | ➕ CDP `Input.dispatchMouseEvent` (vision) | CP9 |
| `browser_console_messages` | ➕ `GetDevToolsProtocolEventReceiver` buffer | CP10 |
| `browser_network_requests` / `browser_network_request` | ➕ `Network.*` event buffer (read-only) | CP10 |
| `browser_resize` | ➕ CDP `Emulation.setDeviceMetricsOverride` | CP11 |
| `browser_pdf_save` | ➕ CDP `Page.printToPDF` | CP11 |
| `browser_cookie_*` (get/set/list/delete/clear) | ➕ CDP `Network.*Cookies` | CP12 |
| `browser_localstorage_*` / `browser_sessionstorage_*` | ➕ `Runtime.evaluate` over storage APIs | CP12 |
| `browser_storage_state` / `browser_set_storage_state` | ➕ compose cookies + storage to/from JSON | CP12 |
| `browser_verify_element_visible` / `_list_visible` / `_text_visible` / `_value` | ➕ assert over AX snapshot + DOM | CP13 |
| `browser_route` / `_unroute` / `_route_list` / `network_state_set` | ➕ CDP `Fetch` interception | CP14 (deferred) |
| `browser_run_code_unsafe` | ⛔ no Playwright runtime → use `browser_evaluate` | — |
| `browser_generate_locator` | ⛔ Playwright codegen → use `browser_snapshot` refs | — |
| `browser_annotate` | ⛔ Playwright Dashboard → fork **design mode** overlay | — |
| `browser_start/stop_tracing`, `start/stop_video`, `video_chapter`, `resume` | ⛔ Playwright trace/video infra — not applicable to embedded WebView2 | — |
| `browser_highlight` / `browser_hide_highlight` | ⛔ (optional later via CDP `Overlay`) | — |
| `browser_get_config` | ⛔ no equivalent config surface | — |

### 9.2 Checkpoints

- **CP7 — parity fills (no new infra). DONE + user-verified.** `browser_click`
  doubleClick/button/modifiers; `browser_wait_for` `textGone`; `browser_type`
  `slowly`; `browser_navigate_back`; `browser_fill_form`; `browser_close`.
  Mostly DOM / existing methods.
- **CP8 — input interactions. DONE + tool-verified.** `browser_file_upload`
  (`DOM.setFileInputFiles`), `browser_drag` (mouse press→move→release),
  `browser_drop` (synthetic HTML5 drop — data/MIME, not files),
  `browser_handle_dialog`. **Design note:** handle_dialog uses a JS-override
  installed via `evaluate` (overrides `alert`/`confirm`/`prompt` + records +
  returns per policy), *not* WebView2's native `ScriptDialogOpening` — chosen
  for reliability in composition mode (no native binding / default-dialog
  suppression). Semantics: arm `browser_handle_dialog` *before* the action that
  triggers the dialog; re-arm after navigation. Covers alert/confirm/prompt
  (not native chrome dialogs / beforeunload / file chooser).
- **CP9 — vision / coordinate tools.** `browser_mouse_*_xy` via
  `Input.dispatchMouseEvent` (the hover path already proves this works). Flip the
  coordinate non-goal in §8.
- **CP10 — observation infra (console + network, read-only).** New per-session
  buffers fed by `GetDevToolsProtocolEventReceiver` (CDP *events*, vs the
  request/response calls used today): `Runtime.consoleAPICalled`/`Log.entryAdded`
  → `browser_console_messages`; `Network.requestWillBeSent`/`responseReceived` →
  `browser_network_requests` + `browser_network_request`. Biggest new-infra CP
  (enable domains, ring buffer, lifecycle on navigation/tab-close).
- **CP11 — emulation + PDF.** `browser_resize`
  (`Emulation.setDeviceMetricsOverride`, for responsive testing);
  `browser_pdf_save` (`Page.printToPDF`) — composes with the fork's PDF viewer.
- **CP12 — storage.** Cookies via `Network.getCookies`/`setCookie`/
  `deleteCookies`/`clearBrowserCookies`; local/session storage via
  `Runtime.evaluate`; `storage_state`/`set_storage_state` compose both to/from a
  JSON file. (Unlocks auth/session reuse — the biggest real-capability add.)
- **CP13 — testing assertions.** `browser_verify_*` evaluated against the AX
  snapshot + DOM. (`generate_locator` stays a divergence.)
- **CP14 — network mocking (deferred).** `browser_route`/`_unroute`/`_route_list`/
  `network_state_set` via CDP `Fetch` domain interception.

### 9.3 Divergences (won't replicate; analogs provided)

`browser_run_code_unsafe` (we run page JS via `browser_evaluate`, not the
Playwright API); `browser_generate_locator` (use snapshot refs);
`browser_annotate` (fork design mode); tracing / video / `resume` /
`browser_get_config` (Playwright-infrastructure-specific). These are recorded so
"not present" is a deliberate, explained choice rather than a gap.

### 9.4 Verification standard (applies to every CP)

Per the CP6 evidence method: confirm against **ground truth** — tool return
values, an independent `browser_evaluate` read-back, or a *viewport-resolution*
screenshot of the affected region. Never infer success from the AX snapshot
alone. Each new tool ships with a unit test for its pure logic plus a runtime
check through the real MCP server.

---

## 10. Sync notes

`browser-automation` rebases onto `browser-viewer`. Fork-owned paths
(`crates/browser_viewer/src/automation/`, `vendor/zed-browser-mcp/`,
`plans/browser-automation.md`) should not conflict with upstream. Touch points
on shared files: `browser_viewer.rs` init, `settings_content.rs` browser
fields, `default-windows.json` dev action keymap entries — search `automation`
on rebase.
