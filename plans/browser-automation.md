# Browser Automation — Status & Specification

**Status:** CP0–CP5 shipped and dogfooded; CP6 (Tier 2 tools) next  
**Branch:** `browser-automation` (off `browser-viewer`)  
**Platform:** Windows only (WebView2 / CDP)  
**Last updated:** 2026-05-30

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

## 3. Shipped tool surface (Tier 1)

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

Tier 2 remaining (CP6, not started): `browser_select_option`,
`browser_take_screenshot`, `browser_hover`, `browser_evaluate`.

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
| **CP6** | Tier 2 breadth | In progress — `browser_press_key`, `browser_scroll`, `browser_tabs` done |

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

## 8. What's next (CP6)

Priority order from dogfood:

1. ~~**`browser_press_key`**~~ — **Done.** Key spec → CDP, backs `submit:true`.
   Verified on Google + Takealot.
2. ~~**`browser_scroll`**~~ — **Done.** Viewport delta + element-into-view,
   returns `{x,y,maxY}`. Verified on Takealot results.
3. ~~**`browser_tabs`**~~ — **Done.** list / select / new / close over workspace
   browser tabs. Verified end-to-end.
4. **`browser_take_screenshot`** — PNG for agent context (separate from snapshot).
5. **`browser_select_option`**, **`browser_evaluate`**, **`browser_hover`**.

Deferred / non-goals:

- Cross-platform (macOS/Linux).
- Sikuli / coordinate automation.
- IME and dead-key input via agent.
- ACP host-side tool execution without MCP (no upstream hook today).

---

## 9. Sync notes

`browser-automation` rebases onto `browser-viewer`. Fork-owned paths
(`crates/browser_viewer/src/automation/`, `vendor/zed-browser-mcp/`,
`plans/browser-automation.md`) should not conflict with upstream. Touch points
on shared files: `browser_viewer.rs` init, `settings_content.rs` browser
fields, `default-windows.json` dev action keymap entries — search `automation`
on rebase.
