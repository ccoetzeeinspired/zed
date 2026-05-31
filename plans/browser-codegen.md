# Agent → Runnable Scripts: Record + Codegen (Plan & Testing Strategy)

**Status:** **CP15 v1 shipped + verified (2026-05-31).** Recorder + codegen +
`browser_record`/`browser_codegen` tools land on `feat/cp15-codegen` (commits
`3ed12d5` + `de8e118`). The record→codegen→runnable-Playwright claim is **proven**:
the canonical TrueLens login→dashboard flow plus a 5-flow / 5-site campaign now
run **6/6 green, no-flake** under real Playwright (see §4.6 + §4.6.1). The campaign
surfaced one residual — locator collisions — and both its forms are fixed
(`exact: true` + `.nth(i)`).
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

#### 4.6 Results — RUN + PROVEN (2026-05-31)

The canonical case was executed end-to-end. The agent drove
`https://truelens.co.za/` (login → dashboard) through the MCP tools with
`browser_record` on, then `browser_codegen` emitted this spec **verbatim**
(zero hand-editing):

```ts
import { test, expect } from '@playwright/test';

test('recorded flow', async ({ page }) => {
  await page.goto('https://truelens.co.za/');
  await page.waitForLoadState();
  await page.getByRole('textbox', { name: 'Email' }).fill('c@admin.com');
  await page.getByRole('textbox', { name: 'Password' }).fill('12');
  await page.getByRole('button', { name: 'Sign In' }).click();
  await expect(page.getByRole('heading', { name: 'Dashboard' })).toBeVisible();
  await page.waitForLoadState();
});
```

Run under **real Playwright 1.60 Chromium, headless**:

| Metric | Result |
|---|---|
| Actions recorded | 5 (navigate, type, type, click, verify) |
| Manual edits to run | **0** |
| Pass under real Playwright | **1/1**, then **3/3** re-runs — **no flake** (~1.4 s) |
| Locator collisions | 0 — every `getByRole(role,{name})` resolved uniquely |
| LLM reconstruction | none — locators recorded from `RefRegistry` at action time |

So every ephemeral `eN` ref became a durable accessibility-first locator, and the
run's `verify_*` became the spec's own `expect(...)` assertion. **The claim
holds for a well-structured flow.**

**One honest residual observed (timing, §3).** The post-click `waitForLoadState()`
emitted *after* the assertion rather than right after the click: the `click`
command returns before the async nav to `/dashboard` settles, so the recorder
captured the click's post-action URL as still `/` and only saw the URL change at
the next (verify) action. **Harmless** here — Playwright's web-first
`expect().toBeVisible()` auto-waits — but a real artifact. Fix ideas for v2: read
`page_generation`/`is_loading` after consequential actions before capturing the
URL, or emit a defensive `waitForLoadState()` after every `click`. Left as a
documented residual; the experiment did not require it.

**Repro harness (for the next session):**
- MCP stdio client: `C:\Users\darks\AppData\Local\Temp\mcp-client.mjs` — accepts
  a JSON array (or a file path) of `{name, arguments}` calls; does the
  initialize handshake then runs each `tools/call`.
- Playwright runner scaffold: `D:\src\zed\target\cp15-pw\` (config + `tests/`);
  `npx playwright test --project=chromium`. The emitted spec also lands at
  `D:\src\zed\target\cp15-truelens-login.spec.ts`.
- Browser tab must be opened by hand (`browser: new tab`) — the agent can't
  trigger that GUI action headlessly.

**Not yet exercised (next):** the `captureStorageState:true` seeded variant
(skip login, prove auth reuse) and a search→results flow with autocomplete
adaptivity. v1 deliberately omits fallback-selector capture (id/data-testid) —
add it only if a real flow surfaces a role+name collision.

#### 4.6.1 Verification campaign — 5 flows × 5 sites (2026-05-31)

After the canonical case, a self-driven campaign ran 5 multi-navigation flows
end-to-end (record → codegen → **real** Playwright headless), no human eyeballing,
verified against ground truth + each spec's pass/fail:

| Flow | Site | Shape |
|---|---|---|
| 1 | example.com → iana.org | link nav (2-hop) |
| 2 | the-internet.herokuapp.com | Form Auth login (3-hop) + assert |
| 3 | quotes.toscrape.com | tag filter → pagination |
| 4 | saucedemo.com | login → add-to-cart → cart |
| 5 | en.wikipedia.org | search+submit → article |

**Initial result: 2/6 pass** (flows 1 + the TrueLens login). All 4 failures were
the **same residual the plan predicted (§3 hostile DOM / §4.4 durability):
locator collisions** (Playwright strict-mode), in two distinct forms — which the
campaign cleanly separated:

1. **Substring over-match (flows 2, 5).** Playwright's default `getByRole` `name`
   is a case-insensitive **substring**, so `'Secure Area'` also matched
   `'Welcome to the Secure Area…'` and `'Web scraping'` matched `'Methods to
   prevent web scraping'`. **Fix: emit `{ name, exact: true }`** — more faithful
   (our recorded name *is* the full accessible name) and kills the whole class.
   (`getByText` assertions stay substring on purpose — page text often carries
   trailing noise like a `×` close glyph.)
2. **Genuine multiplicity (flows 3, 4).** 4 identical `'inspirational'` links; 6
   identical `'Add to cart'` buttons. `exact` can't help. **Fix: `.nth(i)`** —
   `snapshot.rs` computes each ref's `dup_index`/`dup_count` among same-`(role,
   name)` elements in DOM/AX order (matches Playwright `getByRole` ordering);
   codegen appends `.nth(i)` only when `dup_count > 1`.

**After both fixes (commit `de8e118`): full suite 6/6 pass, 3× re-run no-flake
(~7.5 s for all 6).** Both fixes are unit-tested (43 automation tests green incl.
new nth-codegen + duplicate-index snapshot tests) and were confirmed under real
Playwright. (Verification note: the `.nth` values were confirmed via the live
Playwright run with the tool's computed indices; a full re-drive through the
rebuilt binary just needs a browser tab reopened — a GUI step the agent can't
trigger.)

**Net learning:** role+name codegen reproduces clean flows verbatim; the *only*
thing that broke on real sites was locator ambiguity, and both its forms have
deterministic, now-implemented fixes. Fallback-selector capture (id/data-testid —
saucedemo exposes perfect `data-test` attrs) remains a v2 nicety for the rare case
where even `.nth(i)` is too positional (DOM reorders between record and replay).

#### 4.6.2 Stress campaign — real complex sites (2026-05-31)

Per the user ("stress it, proper workflows, no practice/generic sites"), a second
campaign ran 3 deep journeys on real heavy sites, driven through the **rebuilt
binary** (so these double as the real-tool confirmation of `exact:true` + `.nth`),
then verified by a 3-agent Workflow (each ran its spec 3× under real Playwright +
adversarially audited it):

| Flow | Site | Journey | Result |
|---|---|---|---|
| A | theguardian.com | home → Science section → Environment section → assert heading | 3/3 ✅ |
| B | github.com | microsoft/playwright → Issues tab → Labels → assert search box | 3/3 ✅ |
| C | developer.mozilla.org | reveal search → type+Enter → article → sidebar "Array" → assert | 3/3 ✅ |

**All passed now — but the audit surfaced real fragilities the happy-path runs
hid. These are the substantive findings:**

1. **`exact:true` vs dynamic accessible names — the key tension (GitHub, CRITICAL).**
   GitHub's Issues tab name is `"Issues 143"` — the live open-issue **count is
   baked into the accessible name**. Our `exact:true` (which *fixed* the
   substring-collision class in §4.6.1) makes this *more* brittle: the day the
   count moves off 143, the locator stops matching. So `exact:true` is
   double-edged. **v2 fix:** detect volatile name components (trailing/embedded
   counts, dates) at record time and emit a stable form — strip the count and use
   a prefix/regex (`{ name: /^Issues/ }`) or anchor links by `href`.
2. **Cross-origin consent iframes are invisible to the snapshot (all 3, esp.
   Guardian).** `getFullAXTree` returns only the main frame, so a Sourcepoint/CMP
   consent dialog (a cross-origin iframe) never appears as refs — the live agent
   can't click it and codegen can't record a dismissal. The specs passed only
   because the headless profile/geo didn't raise a *blocking* overlay; on a cold
   EU profile the consent iframe could intercept the nav clicks. **v2:** (a) walk
   child frames in the snapshot (CDP per-frame AX), and/or (b) emit
   `page.addLocatorHandler` for known CMPs, and/or (c) seed `storageState` with
   prior consent.
3. **`.nth(i)` is positional, not semantic (Guardian, MDN).** Disambiguation by
   DOM index reproduces the exact element now, but silently resolves to a
   *different* element if the site reorders/AB-tests its header/footer or changes
   search-result ranking. Inherent to index-based selection. **v2:** prefer a
   captured `href`/`data-testid` fallback over `.nth(i)` when available; keep
   `.nth(i)` as the last resort.
4. **Timing residual confirmed.** The trailing post-assertion `waitForLoadState()`
   is frequently a no-op, and the real waits rely on Playwright auto-waiting
   rather than the emitted ones. Harmless here; the §4.5 "attach the wait to the
   navigating click" fix would make the emitted waits meaningful.

**Takeaway:** the engine reproduces real deep-nav/SPA/search journeys verbatim
(9/9 across both campaigns once `exact`+`.nth` landed). The remaining work is all
**locator *durability over time*** (dynamic names, consent frames, positional
indices) — not reproduction *now*. A v2 "smart locator" pass (volatile-name
normalization + href/test-id fallback + frame-aware snapshot + CMP handling) is
the clear next investment.

#### 4.6.3 Pass 3 — durable-selector capture + a new site (2026-05-31)

Implemented (commit `4d6f378`) a **universal** durable-selector layer: at record
time, for the acted element, the page computes the most stable selector that
*uniquely* identifies it (`querySelectorAll().length===1`): `data-testid` → other
test-id attrs → `id` → link `href` → `name`. Codegen priority: `getByTestId` >
unique `getByRole(name)` > unique structural selector (replaces positional `.nth`)
> `.nth(i)`. **Deliberately conservative (Rule: universal, not scenario-fitted):**
a unique accessible name is *not* overridden by an id/href (those can be
framework-generated/volatile — React `:r1:`, Ember ids, session hrefs); the
in-page uniqueness check declines a non-unique href.

Re-drove Guardian/GitHub/MDN through the rebuilt binary + a **new site:
crates.io** (Ember SPA, chosen to test the safety property). 2× Playwright:

| Flow | Result | Note |
|---|---|---|
| crates.io (NEW) | 2/2 ✅ | **Safety confirmed** — used accessible names for the search box + `serde` link, did NOT grab volatile Ember ids. (`serde v1.0.228` heading shows the dynamic-name caveat in a *heading*.) |
| MDN | 2/2 ✅ | duplicate "Array" links share `/Array` href → non-unique → correctly kept `.nth(0)` |
| Guardian | 1/2 ⚠️ flaky | one pass, one 30 s timeout — **cold-profile consent overlay/ad-load intermittently blocks the nav click** (finding #2 reconfirmed, now observed firing) |
| GitHub | 0/2 ❌ | deep nav reproduced (both clicks → `/labels`); only the *assertion* failed |

**Two findings:**
- **AX role-attribution divergence (GitHub, NEW).** The labels search element
  snapshotted as a `search` *landmark* named "Search all labels" this session
  (last campaign: `textbox`, which passed). Playwright's Chromium doesn't expose
  a `search`-role element with that name → assertion not found. The same logical
  element surfaces under different roles across snapshots/engines — so a single
  `getByRole` for an assertion is fragile. **Motivates v3: emit resilient
  `locator.or(...)` chains** (role+name OR test-id/css OR `getByText`) for
  assertions, robust to role/name attribution differences.
- **Consent flakiness is real & intermittent (Guardian).** Confirmed by an
  actual 30 s timeout on a cold run — not just theoretical. Reinforces the
  frame-aware-snapshot / `addLocatorHandler` / `storageState`-consent work.

**Honest status of this pass:** the durable-selector change is *safe + correct*
(validated; no regressions; new-site crates.io green) and adds durability headroom
(test-id, unique-href disambiguation) — but it did not by itself raise the pass
rate on these 4, because their failures are consent flakiness and role-attribution
divergence, which it doesn't target. Those define the next passes: **v3 resilient
`.or()` assertion locators**, then **consent/iframe handling**.

#### 4.6.4 Pass 4 — resilient `.or()` assertion locators + a new site (2026-05-31)

Implemented (commit `fb3401e`) **resilient visibility-assertion locators**. When a
`verify_element_visible`/`_list_visible` relies on role+name (no durable
structural selector), codegen emits:
`getByRole(role,{name,exact}).or(getByLabel(name)).or(getByPlaceholder(name)).or(getByText(name)).first()`.
Principle (universal, Rule-#2-clean — no external heuristics, no site
assumptions): an accessible name comes from one of a few standard sources (text /
`<label>` / `placeholder`), so OR the standard Playwright accessors **derived from
the element's own recorded name**; `.first()` keeps it single (an over-matching
branch can't strict-mode or regress a passing assertion).

Verified on the rebuilt binary; **also proved the automation layer can open its own
tab via `browser_tabs new` (MCP) — no GUI step needed**, so passes are now fully
headless-driveable. Re-drove Guardian/GitHub/MDN/crates.io + a **new site:
npmjs.com**. 2× Playwright:

| Flow | Result | Note |
|---|---|---|
| **GitHub** | **2/2 ✅ (was 0/2)** | **Pass-4 fix confirmed** — the `getByPlaceholder` branch matches the `search`-landmark element whose `getByRole('search')` didn't reproduce |
| MDN | 2/2 ✅ | — |
| crates.io | 2/2 ✅ | — |
| Guardian | 1/2 ⚠️ | consent-overlay flakiness (unchanged; not targeted this pass) |
| **npmjs (NEW)** | 0/2 ❌ | **site-side bot detection** — npmjs serves Playwright-headless a "Performing security verification" interstitial; the real page never loads (heading count 0). Our embedded WebView2 (real, non-headless Chromium) passed it fine. |

**New finding — headless bot-walls (npmjs).** Some real sites block Playwright
headless (Cloudflare-style challenge) and serve a verification interstitial, so a
recorded flow can't reproduce *headless* regardless of locator quality. This is
the §3 "different engine" residual in its strongest form. Not a codegen defect and
**not ours to "fix"** (evasion isn't a universal codegen concern, Rule #2);
mitigations are run-environment, not code: run Playwright **headed**, with a real
user-data-dir/profile, or seed `storageState`. Worth surfacing to the user as a
capability of the *runner*, not the generator.

**Cumulative real-site scorecard (deterministic reproduction, excluding
site-side bot-walls + consent flakiness):** GitHub, MDN, crates.io, TrueLens,
example→iana, the-internet, quotes, wikipedia = **green**. Open levers:
**consent/iframe handling** (Guardian) and **headed/profile runner mode** (npmjs).

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
