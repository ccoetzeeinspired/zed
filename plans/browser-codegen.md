# Agent → Runnable Scripts: Record + Codegen (Plan & Testing Strategy)

**Status:** Proposal / candidate checkpoint (**CP15**). Nothing implemented yet.
**Prereq:** CP0–CP13 shipped (full Playwright MCP parity, 51 tools) — see
[`browser-automation.md`](browser-automation.md).
**Branch:** all work lands in `cccl-main` (see `CLAUDE.md` → Branches).
**Last updated:** 2026-05-31

---

## 0. Orientation for a fresh session (read this first)

You are picking up a personal Zed fork that has a **bidirectional
browser-agent automation framework**: the claude-acp agent drives an *embedded*
WebView2 browser tab through an MCP server, using CDP + the accessibility tree.
This document is about the **next** phase — turning an agent's interactive run
into a runnable Playwright **script** (codegen) and the broader testing story.
Before designing that, here's the lay of the land.

### What exists today (CP0–CP13, all user-verified)

A **51-tool** Playwright-shaped MCP surface (`zed-browser` context server) that
drives the embedded tab. Tool families:

- **Navigation:** `browser_navigate`, `browser_navigate_back`, `browser_wait_for`
- **Inspection:** `browser_snapshot` (AX tree → YAML + `ref=eN`), `browser_evaluate`,
  `browser_take_screenshot`
- **Element interaction:** `browser_click` (button/double/modifiers), `browser_type`
  (+`submit`, +`slowly`/`slowlyDelayMs`), `browser_fill_form`, `browser_select_option`,
  `browser_hover`, `browser_press_key`, `browser_scroll`, `browser_file_upload`,
  `browser_drag`, `browser_drop`
- **Coordinate ("vision") mouse:** `browser_mouse_move_xy`/`_click_xy`/`_down`/
  `_up`/`_drag_xy`/`_wheel`
- **Dialogs:** `browser_handle_dialog`
- **Tabs:** `browser_tabs` (list/select/new/close), `browser_close`
- **Observation:** `browser_console_messages`, `browser_network_requests`,
  `browser_network_request`
- **Emulation/output:** `browser_resize`, `browser_pdf_save`
- **Storage:** `browser_cookie_*` (get/set/list/delete/clear), `browser_localstorage_*`,
  `browser_sessionstorage_*`, `browser_storage_state`, `browser_set_storage_state`
- **Assertions:** `browser_verify_element_visible`/`_list_visible`/`_text_visible`/`_value`
- **Sync:** `browser_wait_for`

### Architecture (how a tool call flows)

```
claude-acp agent → vendor/zed-browser-mcp (stdio MCP, Node)
                 → loopback TCP 127.0.0.1:19382 (one JSON line per req/resp)
                 → crates/browser_viewer/src/automation/ipc.rs::dispatch_request
                 → commands.rs (awaitable handlers)
                 → cdp.rs / action.rs (CDP over WebView2 CallDevToolsProtocolMethod)
                 → the active BrowserView tab
```

Everything is **CDP / DOM on resolved accessibility refs** — never coordinate
clicking (except the explicit `mouse_*_xy` vision tools). Refs (`e14`) come from
`browser_snapshot`; they reset on navigation.

### File map (the load-bearing bits)

```
crates/browser_viewer/src/
  automation/
    mod.rs            — module wiring + dev actions
    cdp.rs            — CdpSession; call_method, evaluate_expression,
                        call_with_domain_enabled, set_file_input_files, parse_cdp_response
    target.rs         — resolve active tab / workspace globally
    session.rs        — RefRegistry + ElementRef { ref_id, role, name, backend_dom_node_id, ax_node_id }
    snapshot.rs       — AX tree → YAML + ref registry (MAX_REFS default 2000, env override)
    action.rs         — click/type/scroll/drop/etc. CDP scripts + invoke_on_backend_node
    commands.rs       — all awaitable MCP/IPC command handlers (the 51 tools' logic)
    keys.rs           — Playwright key spec → CDP Input.dispatchKeyEvent fields
    tabs.rs           — browser_tabs / close over workspace items
    instrumentation.rs— doc-start JS that buffers console + fetch/XHR (CP10)
    ipc.rs            — TCP listener + GPUI dispatch loop (THE chokepoint — see CP15)
  webview2_host.rs    — WebView2Session: call_devtools_protocol, dispatch_key_event,
                        capture_preview_png; injects design-mode + instrumentation scripts
  browser_view.rs     — BrowserView/BrowserItem; automation_navigate/go_back; url editor

vendor/zed-browser-mcp/src/index.ts — the 51 MCP tools (stdio → TCP)
plans/browser-automation.md          — the authoritative spec + full CP log
plans/browser-codegen.md             — THIS doc (CP15 proposal)
```

### How to build / run / verify (reproduce the loop)

```powershell
# From D:\src\zed (always; never a parent — toolchain/crt-static come from cwd).
# Kill Zed first — a running zed.exe locks the binary (cargo exits 0 but the
# copy-up fails with "Access is denied"; verify the exe timestamp moved).
Stop-Process -Name zed -Force -ErrorAction SilentlyContinue

cargo test -p browser_viewer automation:: -j 4   # unit tests (use -j 4, see CLAUDE.md)
cargo build -p zed -j 4                            # build (~30-60s incremental)
# rebuild the MCP adapter after TS edits:
cd vendor\zed-browser-mcp; npm run build; cd ..\..
Start-Process D:\src\zed\target\debug\zed.exe      # launch; then open a browser tab
```

Opening a browser tab (`Ctrl+Shift+P` → `browser: new tab`) is a **GUI action
the agent cannot trigger headlessly** — ask the user. Once a tab is open, drive
the MCP server directly via a small stdio client (the pattern used all session):
spawn `node vendor/zed-browser-mcp/dist/index.js`, send `initialize` +
`notifications/initialized`, then `tools/call`. (A reusable harness lived at
`/tmp/mcp-client.mjs` — re-create it; it does the handshake then runs a JSON
array of `{name, arguments}` calls.)

### Verification ethos (non-negotiable — the user enforced this)

- **Per-action visual spot-check.** The user verifies *each* action on-screen
  before the next; do **not** batch then claim verified. Inject *visible*
  fixtures that write results on-page, do ONE action, say exactly what to look
  for, wait for yes/no.
- **Ground truth, not AX inference.** Never call an action "worked" from the AX
  snapshot — confirm via the page's own `location.href`, an `evaluate`
  read-back, a results-only element, the browser's own DevTools, or the user's
  eyes. (Two misreads this session were caught exactly this way.)
- **Fix anomalies found during a checkpoint** — even unrelated ones — so the
  foundation stays solid (e.g. CP7 surfaced and fixed: post-nav `evaluate`
  stale-context race, address-bar desync, the 500-ref snapshot cap).

### Key learnings baked in (don't re-discover)

- **Enter needs `text:"\r"`** in the CDP keyDown or Chromium skips `keypress`
  and form-submit silently no-ops (`automation/keys.rs`).
- **`navigate` pre-sets `is_loading`** so wait-for-load can't return before
  `NavigationStarting` fires (else `evaluate` hits the old page context).
- **handle_dialog** is a **JS override** (arm-then-trigger), not native
  `ScriptDialogOpening`; **drop** is synthetic (no real files); **console/network**
  are doc-start JS instrumentation — all chosen for composition-mode reliability
  over uncertain WebView2/CDP bindings.
- **`browser_drop` ≠ files** (use `file_upload`); **`resize`** sets a viewport
  override with no auto-reset; **`pdf_save`** is paper-layout (≈A4), not viewport.

### Divergences (intentionally not built; analogs exist)

`run_code_unsafe` (→ `browser_evaluate`), `generate_locator` (→ snapshot refs),
`annotate` (→ design mode), tracing/video/`resume`, `get_config`. Deferred:
**CP14** network request mocking (`route`/`unroute`/offline) — GitHub issue #13.

### GitHub / process

- Repo `ccoetzeeinspired/zed`. Default + protected branch: **`cccl-main`**
  (PR-only; admin can bypass). Integration branch: `cccl-staging`. `main` is the
  upstream mirror. Don't base work on the legacy chain (`pdf-viewer` …
  `browser-automation`) — branch off `cccl-main`.
- Project board #1 ("Browser Automation — Playwright Parity"): CP7–CP13 + the
  four CP7 follow-ups are **Done**; CP14 (#13) is **Todo**.

---

## 1. The problem this checkpoint targets

The user's long-standing frustration (stated 2026-05-31): an AI agent can drive
a browser interactively and *complete* a flow ("do a–e: log in, search, add to
cart, log out"), but historically that has **not** translated into a runnable
script (e.g. Playwright) that reproduces a–e with little-to-no manual editing.
The agent's "knowledge" of how it did it doesn't become a durable artifact.

Why interactive success ≠ runnable script — four root causes:

1. **Ephemeral handles vs durable locators.** Interactive automation acts on
   throwaway refs (`e14`) valid only for one snapshot of one page state. A script
   needs *stable* locators. When an LLM is asked afterward to "write the script,"
   it **reconstructs locators from memory** — lossy and guess-prone. This is the
   #1 cause of non-reproducing scripts.
2. **Preconditions / state.** The interactive run started already authenticated
   / past the cookie banner / popup. The cold script hits the login wall (and the
   exact popups we hit this session) and dies early.
3. **Adaptivity.** Mid-run the agent *reacts* — "a popup appeared, dismiss it";
   "Enter didn't submit, click the button" (both happened this session). Those
   decisions aren't in the linear a–e, so a flat script lacks them.
4. **Timing / flake.** The agent's actionability waits + retries paper over
   timing; a naive emitted script has no waits and flakes.

## 2. Why our framework changes the odds (structurally)

1. **Semantic refs = durable-locator currency.** Our snapshot/refs are derived
   from **role + accessible name** and `RefRegistry`/`ElementRef` stores
   `{role, name}` for every ref. `e14` *is* `{role:"textbox", name:"Email"}`,
   which maps ~1:1 to `page.getByRole('textbox', {name:'Email'})` — Playwright's
   recommended, durable, accessibility-first locator. The agent's automation
   path **already contains** the durable-locator info; it's in the registry at
   the moment of every action. (Coordinate/Sikuli automation has none of this —
   it translates to nothing.)
2. **We own the layer → RECORD, not RECALL.** Every action passes through one
   chokepoint (`ipc.rs::dispatch_request`). We can log each action *with its
   resolved role+name* as it happens and **codegen the script as a byproduct of
   the run** — deterministic, no LLM reconstruction. This is the crux: the
   reason "AI + playwright-mcp" struggled isn't that its actions weren't semantic
   (they were) — it's that nobody captured them; the model re-derived the script
   from memory. Owning the layer removes that.
3. **Preconditions are solved, and proven.** CP12 `storage_state` is exactly
   Playwright's auth-reuse mechanism. This session we captured a real TrueLens
   session → cleared → restored → `/dashboard` loaded authenticated with no
   re-login. A generated script can seed `storageState` and skip the login flow
   entirely.
4. **Same primitives → mechanical mapping.** We deliberately mirror Playwright's
   tool shapes, so the action→API translation is near 1:1 (see §4.3 table). Some
   of our runtime *divergences* even map to Playwright's **native** mechanism in
   script form (dialog → `page.on('dialog')`, storage → `storageState`), so
   codegen can emit *more* idiomatic Playwright than our runtime uses.

## 3. Honest residuals (calibration — where "no edits" holds vs not)

These don't vanish:

- **Adaptivity.** A *recorded* run captures what actually happened (including a
  popup-dismiss it did) — far more faithful than recall. But it won't
  auto-handle a popup that appears only *sometimes*; conditionals are a
  human/LLM touch-up. (Mitigation: Playwright's `page.addLocatorHandler` for
  recurring overlays — codegen could optionally emit it; advanced/future.)
- **Timing.** Codegen can auto-insert `expect(...).toBeVisible()` / waits after
  navigations (we have the data), removing most flake; messy SPAs may still need
  a hand-tuned wait.
- **Hostile DOM.** role+name is gold on decent sites; a site with no
  roles/names/test-ids and churning dynamic IDs yields brittle selectors even
  from Playwright's own codegen. No framework fixes that. (Mitigation: capture a
  fallback selector — id/data-testid/CSS — at record time; §4.4.)
- **Different engine.** The generated script runs under *real* Playwright
  Chromium, not our WebView2. That's the point (portable scripts), and behavior
  is usually identical, but note it.

**Calibrated expectation:** for a well-structured flow on a reasonable site, a
recorded a–e should reproduce as a script with little-to-no editing. For
messy/adaptive flows, expect a strong ~80% draft needing the conditional/timing
bits hand-finished — still categorically better than "the script doesn't run."

---

## 4. CP15 — Record + Codegen (the design sketch)

**Goal:** an opt-in recorder that turns an agent's interactive automation run
into a runnable Playwright `.spec.ts` reproducing the flow with minimal edits.

### 4.1 Where to record

In **Rust, at `ipc.rs::dispatch_request`** — the single chokepoint every tool
call passes through, and the only place with authoritative access to the
`RefRegistry` (ref → role+name) without re-parsing snapshot YAML.

- A process-global recording buffer behind a `Mutex` (dispatch already
  serializes via `AUTOMATION_SERIAL`, so contention is a non-issue):
  ```rust
  struct RecordedAction { kind: String, role: Option<String>, name: Option<String>,
      ref_fallback: Option<String>, args: serde_json::Value, url: String, t_ms: u64 }
  static RECORDING: Mutex<Option<Vec<RecordedAction>>> = Mutex::new(None);
  ```
- On each *successful* ref-bearing action, resolve `ref_id → ElementRef` (same
  path commands use) and record `{role, name}` — **not** the ephemeral ref.
- Capture the page URL after each action (`browser.read().item().url()`); a URL
  change since the previous action ⇒ codegen inserts a `waitForLoadState` /
  `expect(page).toHaveURL(...)`.
- **Codegen** (action-log → script text) can live in Rust (co-located with the
  data) and return the script string; **Node writes the file** (mirrors
  `pdf_save` / `storage_state`: Rust returns data, Node does fs). Codegen in TS
  is also fine — but then Node must rebuild the ref→role/name map from snapshot
  YAML; prefer Rust to avoid drift.

### 4.2 Tool surface (CP15 additions)

- `browser_record` `{action: "start"|"stop"|"status", captureStorageState?: bool}`
  — `start` clears+enables the buffer (optionally auto-captures `storage_state`
  so the script can seed auth); `stop` disables and returns the action count.
- `browser_codegen` `{filename?, format?: "playwright-ts"}` — generate the script
  from the current buffer; Node writes it (default a temp `.spec.ts`), returns
  the path + the script text. (Could also auto-emit on `record stop`.)

Keep it opt-in start/stop so the agent scopes exactly the a–e it wants captured.

### 4.3 Action → Playwright mapping (the core of durability)

| Recorded action | Emitted Playwright |
|---|---|
| navigate(url) | `await page.goto('url'); await page.waitForLoadState();` |
| navigate_back | `await page.goBack();` |
| click(role,name, opts) | `await page.getByRole('role', { name: 'name' }).click({ button, modifiers, clickCount });` |
| type(role,name, text) | `await page.getByRole(...).fill('text');` |
| type submit:true | `…fill('text'); await page.getByRole(...).press('Enter');` |
| type slowly(delay) | `await page.getByRole(...).pressSequentially('text', { delay });` |
| fill_form([{role,name,value,kind}]) | one `.fill` / `.check` / `.selectOption` per field |
| select_option(role,name, values) | `await page.getByRole(...).selectOption([…]);` |
| press_key(key) | `await page.keyboard.press('key');` |
| hover(role,name) | `await page.getByRole(...).hover();` |
| scroll(ref) / scroll(dx,dy) | `…scrollIntoViewIfNeeded()` / `await page.mouse.wheel(dx,dy);` |
| file_upload(role,name, paths) | `await page.getByRole(...).setInputFiles([…]);` |
| drag(srcRole/name → dstRole/name) | `await page.getByRole(src).dragTo(page.getByRole(dst));` |
| mouse_*_xy | `await page.mouse.move/click/down/up/wheel(x,y,…);` |
| wait_for(text) | `await expect(page.getByText('text')).toBeVisible();` |
| wait_for(textGone) | `await expect(page.getByText('text')).toHaveCount(0);` |
| handle_dialog(accept,text) | top-of-test `page.on('dialog', d => accept ? d.accept(text) : d.dismiss());` |
| verify_element_visible(role,name) | `await expect(page.getByRole(...)).toBeVisible();` |
| verify_text_visible(text) | `await expect(page.getByText('text')).toBeVisible();` |
| verify_value(role,name, value) | `await expect(page.getByRole(...)).toHaveValue('value');` |
| verify_list_visible(role,name) | `await expect(page.getByRole('list')).toBeVisible();` + item count |
| (run start) storage_state captured | `test.use({ storageState: 'state.json' });` (skip the login flow) |

### 4.4 Locator strategy (durability rules)

- **Primary:** `getByRole(role, { name })` from the recorded role+name.
- **Disambiguation:** if `(role, name)` matched >1 element at record time (check
  the snapshot/DOM match count), emit `.nth(i)` or `.first()` and flag it.
- **Fallback capture:** at record time, also read `id` / `data-testid` / a short
  CSS path for the target element (one tiny DOM read) and store it as
  `ref_fallback`. Codegen prefers role+name but emits a commented fallback
  locator so a human can swap it on a name collision / nameless element.
- **Text/headings:** for non-interactive assertions, `getByText` is fine.

### 4.5 Adaptivity & preconditions in the emitted script

- Popups the agent dismissed *during* recording appear as real steps (faithful).
- For recurring overlays, optionally emit `await page.addLocatorHandler(...)` —
  Playwright's native auto-dismiss. Advanced; behind a flag.
- Auth precondition: prefer `storageState` seeding (proven) over replaying the
  login keystrokes — more robust and faster.

### 4.6 Verification plan (this *is* the testing experiment the user wants)

1. Agent performs a–e via the MCP tools with `browser_record` on.
2. `browser_codegen` emits `flow.spec.ts`.
3. Run it under **real** Playwright headless: `npx playwright test flow.spec.ts`
   against the same site.
4. **Measure:** does it reproduce a–e? Metrics — steps reproduced, pass/fail,
   flake over N runs, manual edits required, locator-collision count.
5. **Self-verifying:** emit the run's `verify_*` calls as `expect(...)` so the
   script asserts its own success.
6. **Canonical first case:** the TrueLens login → `/dashboard` flow (known; and
   `storage_state` seeding already proven this session). Then a search→results
   flow (Takealot) which also exercises autocomplete adaptivity.

### 4.7 Effort / risk

- **Recorder:** small (one hook at the dispatch chokepoint + a Mutex buffer).
- **Codegen:** the bulk, but well-bounded — a finite, known tool set maps to a
  finite Playwright API (table above). Pure string templating.
- **Harness:** reuses the existing build/launch/MCP-client loop + a Playwright
  install for step 3.
- **Risk:** locator durability on hostile sites; adaptive flows; engine diff
  (WebView2 vs Chromium). All discussed in §3; none block the happy path.

### 4.8 Open decisions for the implementing session

- Codegen in Rust (co-located, no drift) vs Node (easier templating)? — lean
  Rust returns text, Node writes file.
- Auto-emit on `record stop` vs explicit `browser_codegen`?
- Emit `storageState` seeding by default, or replay login? — default seed.
- Fallback-selector capture: worth the extra DOM read per action? — yes, cheap
  and high-value on name collisions.
- Output: a single `.spec.ts`, or a Playwright project scaffold (config +
  fixtures)? — start with a single spec; scaffold later.

---

## 5. Broader testing strategy (beyond codegen)

- **Self-asserting flows:** `verify_*` during a run double as the script's
  assertions — the agent both *does* and *checks*, and the codegen carries both.
- **Auth fixtures:** `storage_state` files become reusable Playwright
  `storageState` fixtures — capture once (via the agent), reuse across a suite.
- **Regression sweeps:** a library of recorded `.spec.ts` flows, re-run in CI
  under real Playwright; the agent regenerates/repairs them when the UI changes
  (the agent *re-records* rather than a human re-writing selectors — closing the
  loop the user has been chasing).
- **Console/network gates (CP10):** assert "no console errors" / "API returned
  200" inside generated specs using the same observation data.

## 6. Non-goals (for codegen)

- Not a general Playwright codegen for arbitrary external Chromium — it codegens
  *our* agent's runs against the embedded tab.
- Not auto-resolving genuine non-determinism (A/B content, random IDs) — those
  remain human/LLM touch-ups; the doc just makes them visible, not magic.
