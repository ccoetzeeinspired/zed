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

Tier 2 (CP6, not started): `browser_press_key`, `browser_scroll`,
`browser_select_option`, `browser_tabs`, `browser_take_screenshot`,
`browser_hover`, `browser_evaluate`.

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
| **CP6** | Tier 2 breadth | Not started |

### Verification log (2026-05-30)

- **Dev palette:** TrueLens login via `browser: automation *` actions (email,
  password, Sign In) after React input fix.
- **Agent panel:** Full MCP flow on TrueLens — navigate, wait, snapshot, type
  credentials, click Sign In → landed on `/dashboard` with admin user table
  visible. Agent summarized sidebar + Active Users table from post-login
  snapshot.

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

1. **`browser_press_key`** — Enter after type (`submit: true`), Tab, Escape.
2. **`browser_scroll`** — viewport / element scroll for long pages.
3. **`browser_tabs`** — list/switch when multiple browser tabs exist.
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
