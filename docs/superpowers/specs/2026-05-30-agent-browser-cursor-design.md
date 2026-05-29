# Agent Browser Cursor Design

## Goal

Add an agent-controllable cursor to the embedded Zed browser so Codex can inspect, preview, and click elements inside the active browser tab. The experience should feel like a browser-native agent loop: the agent understands the page, identifies an element, shows what it is about to do, then executes the click through the real WebView2 browser surface.

## Base Branch

This work starts from `browser-viewer` on the `browser-agent-cursor` branch.

The browser foundation is the existing WebView2 composition implementation:

- `crates/browser_viewer/src/browser_viewer.rs`
- `crates/browser_viewer/src/browser_view.rs`
- `crates/browser_viewer/src/webview2_host.rs`
- `crates/browser_viewer/src/design_mode_script.rs`
- `crates/browser_viewer/src/design.rs`
- `crates/browser_viewer/src/drawing.rs`

The agent handoff foundation is:

- `crates/zed_actions/src/lib.rs`
- `crates/agent_ui/src/agent_panel.rs`
- `crates/agent_ui/src/conversation_item.rs`

## Non-Negotiables

- Keep the existing embedded WebView2 composition browser.
- Do not introduce a detached browser window.
- Do not introduce a second browser engine.
- Do not implement click behavior as screenshot-only automation.
- Prefer native WebView2 mouse input over DOM `.click()`.
- Show the target before the first real click.
- Preserve design mode, drawing mode, screenshot capture, and design bundle handoff.
- Target the active `BrowserView`, not a hidden global browser.
- Report clear errors when there is no active browser tab or no matching target.

## Primary User Flows

### Flow 1: Click a Described Element

The user asks the agent to click a named element:

```text
click the Sign in button in the browser
```

The browser resolves a visible target, previews it with a cursor marker and outline, asks for confirmation, then performs a native click.

### Flow 2: Click the Selected Browser Element

The user selects an element with design mode, then asks:

```text
click this
```

The browser uses the current `design_selection`, previews the same target through the agent cursor overlay, then clicks after confirmation.

### Flow 3: Navigate and Click

The user asks:

```text
open localhost:3000 and click Create Account
```

The browser opens or reuses the active browser tab, navigates to the URL, waits for navigation state to settle, resolves the target, previews it, then clicks after confirmation.

### Flow 4: Precision Selector Command

The user or agent uses a precise selector:

```text
click selector button[data-testid="submit"]
```

The browser resolves the selector and clicks the target after preview/confirmation.

## Target Query Model

V1 supports these query types:

```rust
pub enum BrowserElementQuery {
    Selected,
    Selector(String),
    TextExact(String),
    TextContains(String),
    RoleAndName { role: String, name: String },
    Point { x: f32, y: f32 },
}
```

`Selected` maps to the current design-mode selected element when available.

`Selector` uses `document.querySelectorAll` and filters to visible elements.

`TextExact` compares normalized visible text.

`TextContains` performs normalized substring matching.

`RoleAndName` uses explicit `role`, native element semantics, `aria-label`, `aria-labelledby`, button/input values, and visible text.

`Point` resolves `document.elementFromPoint`.

## Target Result Model

Resolved targets use this shape:

```rust
pub struct BrowserResolvedElement {
    pub selector: String,
    pub tag: Option<String>,
    pub text: Option<String>,
    pub role: Option<String>,
    pub accessible_name: Option<String>,
    pub rect: ElementRect,
    pub source: Option<ElementSource>,
    pub confidence: BrowserTargetConfidence,
}

pub enum BrowserTargetConfidence {
    Exact,
    Strong,
    Ambiguous,
    Weak,
}
```

If multiple candidates match, the resolver returns an ambiguous result with candidates rather than silently clicking. V1 may preview the best candidate, but the confirmation text must mention ambiguity.

## Browser Automation Protocol

The current `design_mode_script.rs` should be generalized. The script should keep design selection behavior but add an agent cursor protocol.

Proposed files:

- `crates/browser_viewer/src/browser_protocol.rs`
- `crates/browser_viewer/src/browser_automation_script.rs`
- `crates/browser_viewer/src/agent_cursor.rs`

Host-to-page messages:

```json
{ "kind": "find_element", "requestId": "uuid", "query": { "type": "text_contains", "text": "Sign in" } }
{ "kind": "preview_element", "requestId": "uuid", "selector": "button:nth-of-type(1)" }
{ "kind": "clear_agent_cursor", "requestId": "uuid" }
```

Page-to-host messages:

```json
{ "kind": "agent_target_resolved", "requestId": "uuid", "target": { } }
{ "kind": "agent_target_not_found", "requestId": "uuid", "reason": "No visible element matched text" }
{ "kind": "agent_target_ambiguous", "requestId": "uuid", "candidates": [ ] }
```

The JS layer is responsible for DOM querying and accessible-name heuristics. Rust is responsible for storing state, rendering the overlay, and executing native input.

## Browser Item State

Add cursor state to `BrowserItem`:

```rust
pub agent_cursor: Option<AgentCursorState>
```

Cursor state:

```rust
pub struct AgentCursorState {
    pub request_id: String,
    pub target: BrowserResolvedElement,
    pub status: AgentCursorStatus,
    pub label: String,
    pub ambiguity: Vec<BrowserResolvedElement>,
}

pub enum AgentCursorStatus {
    Preview,
    Clicking,
    Clicked,
    Failed(String),
}
```

State clears on:

- navigation start
- explicit cancel
- Escape
- successful click
- browser tab close
- new target preview

## Visible Cursor UX

The cursor overlay is rendered by GPUI, not by the page.

It should include:

- target outline
- small cursor marker at click point
- compact label, such as `button "Sign in"` or `h1 "Launch Checklist"`
- warning styling when target confidence is `Ambiguous` or `Weak`

The overlay must sit above the WebView2 underlay, like the current drawing/design panel overlays. It must not block unrelated Zed UI or create permanent focus capture.

## Native Click Execution

Clicks should use `WebView2Session::send_mouse_input` in `crates/browser_viewer/src/webview2_host.rs`.

Click algorithm:

1. Read the active `BrowserItem.agent_cursor`.
2. Read the browser viewport bounds from `BrowserItem.last_bounds`.
3. Compute the target center from `BrowserResolvedElement.rect`.
4. Clamp the point inside the viewport.
5. Convert viewport CSS pixels to WebView2 local input coordinates.
6. Send mouse move.
7. Send left button down.
8. Send left button up.
9. Clear or mark cursor state according to the click result.

DOM `.click()` is not the primary implementation. It can be added later as an explicit fallback/debug action.

## Zed Actions

Add browser-side actions in `browser_viewer` for local testing and command palette access:

```rust
PreviewElementByText
PreviewElementBySelector
PreviewSelectedElement
ClickPreviewedElement
ClearAgentCursor
```

Add agent-facing actions in `zed_actions::agent`:

```rust
pub struct BrowserResolveElement {
    pub query_kind: SharedString,
    pub query: SharedString,
}

pub struct BrowserClickResolvedElement {
    pub request_id: SharedString,
}

pub struct BrowserClearAgentCursor;
```

The first implementation slice can use browser-local actions before full agent tool wiring. The action boundaries must not prevent later ACP/tool integration.

## Agent Integration

The agent path should route browser commands to the active browser tab.

Resolution strategy:

1. Locate the active workspace item.
2. Check whether it is a `BrowserView`.
3. If yes, dispatch the browser command to that view.
4. If no active browser tab exists, return a clear message to the agent panel.

V1 can expose explicit actions first. After local behavior is reliable, expose a proper tool contract:

```text
browser.open(url)
browser.find_element(query)
browser.preview_click(target)
browser.click(target)
browser.current_page()
```

## Safety Model

V1 defaults to preview then confirm.

Immediate click is allowed only when:

- the user explicitly asks for it in the current turn, or
- a future trusted-local-browser setting is enabled, or
- a debug command invokes it directly.

Risk levels are reserved for future work:

- Low: localhost, file URLs, development servers
- Medium: ordinary web pages
- High: auth, payments, destructive actions

The V1 implementation should not hard-code a safety system, but it should keep command phases separate so confirmation and policy can be inserted cleanly.

## Acceptance Criteria

- The `browser-agent-cursor` branch builds.
- Browser opens as a real editor tab through `browser::NewTab`.
- URL bar navigation still works.
- Design mode still selects elements.
- Drawing mode still works.
- Existing design bundle submission still works.
- Agent cursor can resolve the current selected design element.
- Agent cursor can resolve an element by CSS selector.
- Agent cursor can resolve an element by visible text.
- Browser shows a visible target cursor/outline overlay.
- Confirmed click triggers actual page behavior through native WebView2 input.
- Overlay clears after click, cancel, and navigation.
- Browser does not trap keyboard focus after preview or click.
- Multiple browser tabs do not click the wrong tab.
- Commands report a clear error when no browser tab is active.

## First Implementation Slice

The first shippable slice is:

1. Add protocol structs.
2. Add JS resolver for selector and text.
3. Add `BrowserView` action to preview a target by selector.
4. Add `BrowserView` action to preview a target by text.
5. Render the target overlay.
6. Add `BrowserView` action to click the previewed target.
7. Reuse selected design element through the same preview/click path.
8. Verify build and manual browser behavior.

Agent chat/tool integration follows after the browser cursor loop works locally.

## Verification Plan

Build:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
$env:CARGO_INCREMENTAL='0'
$env:CARGO_PROFILE_DEV_DEBUG='0'
cargo build -p zed
```

Manual browser verification:

1. Launch `D:\ccoetzeeinspired-zed-target\debug\zed.exe D:\ccoetzeeinspired-zed`.
2. Run `browser::NewTab`.
3. Navigate to a local app with visible buttons.
4. Preview by selector.
5. Preview by visible text.
6. Confirm click.
7. Verify the page receives the click.
8. Toggle design mode and select an element.
9. Preview/click selected element.
10. Confirm drawing mode and design bundle still behave.

Regression checks:

- URL bar typing still works.
- Editor typing still works after closing or switching browser tabs.
- Inactive browser tab is not targeted.
- No stale overlay appears after navigation.
