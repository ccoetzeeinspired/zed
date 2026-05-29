# Agent Browser Cursor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an agent-controllable cursor for the embedded Zed browser so Codex can resolve, preview, and click selected or described page elements inside the active browser tab.

**Architecture:** Extend the existing `browser_viewer` WebView2 composition browser with a typed automation protocol, page-side element resolver, GPUI cursor overlay, and native WebView2 click execution. Keep agent integration as a second stage after local browser actions prove the cursor loop works.

**Tech Stack:** Rust, GPUI, WebView2 composition controller, DirectComposition underlay, injected JavaScript via `AddScriptToExecuteOnDocumentCreated`, Zed workspace actions.

---

## File Structure

- Create `crates/browser_viewer/src/browser_protocol.rs`: typed Rust protocol structs for browser automation messages and resolved targets.
- Create `crates/browser_viewer/src/browser_automation_script.rs`: injected JavaScript string combining existing design-mode behavior with target resolution commands.
- Create `crates/browser_viewer/src/agent_cursor.rs`: Rust state and overlay element helpers for the agent cursor.
- Modify `crates/browser_viewer/src/browser_viewer.rs`: expose new modules and actions.
- Modify `crates/browser_viewer/src/browser_view.rs`: handle automation messages, preview cursor state, render overlay, execute native clicks.
- Modify `crates/browser_viewer/src/webview2_host.rs`: inject the generalized automation script instead of the old design-only script.
- Modify `crates/zed_actions/src/lib.rs`: add agent-facing browser command action types.
- Modify `crates/agent_ui/src/agent_panel.rs`: later-stage bridge from agent action to active browser tab.

## Task 1: Protocol Types

**Files:**
- Create: `crates/browser_viewer/src/browser_protocol.rs`
- Modify: `crates/browser_viewer/src/browser_viewer.rs`

- [x] **Step 1: Add protocol structs**

Create `browser_protocol.rs` with:

```rust
use serde::{Deserialize, Serialize};

use crate::design::{ElementRect, ElementSource};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserElementQuery {
    Selected,
    Selector { selector: String },
    TextExact { text: String },
    TextContains { text: String },
    RoleAndName { role: String, name: String },
    Point { x: f32, y: f32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTargetConfidence {
    Exact,
    Strong,
    Ambiguous,
    Weak,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserResolvedElement {
    pub selector: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub accessible_name: Option<String>,
    pub rect: ElementRect,
    #[serde(default)]
    pub source: Option<ElementSource>,
    pub confidence: BrowserTargetConfidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserAutomationInbound {
    AgentTargetResolved {
        request_id: String,
        target: BrowserResolvedElement,
    },
    AgentTargetNotFound {
        request_id: String,
        reason: String,
    },
    AgentTargetAmbiguous {
        request_id: String,
        candidates: Vec<BrowserResolvedElement>,
    },
}

impl BrowserAutomationInbound {
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}
```

- [x] **Step 2: Export the module**

In `browser_viewer.rs`, add:

```rust
pub mod browser_protocol;
```

- [x] **Step 3: Run check**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
cargo check -p browser_viewer
```

Expected: any failures should be limited to missing serde traits on reused design structs. If so, derive `Serialize` where needed on `ElementRect` and `ElementSource`.

## Task 2: Automation Script

**Files:**
- Create: `crates/browser_viewer/src/browser_automation_script.rs`
- Modify: `crates/browser_viewer/src/browser_viewer.rs`
- Modify: `crates/browser_viewer/src/webview2_host.rs`

- [ ] **Step 1: Copy existing design script behavior**

Create `browser_automation_script.rs` with a `SCRIPT` constant. Start from the current `design_mode_script.rs` and preserve:

- `ready`
- `element_selected`
- `page_scrolled`
- host messages `activate`, `deactivate`, `clear_selection`
- React source detection
- selector generation

- [x] **Step 2: Add host message handling**

Inside the script's `chrome.webview.addEventListener('message', ...)`, parse JSON messages. If parsing fails, keep the existing string command behavior.

Support:

```javascript
if (msg.kind === 'find_element') {
  resolveAgentTarget(msg.requestId, msg.query);
} else if (msg.kind === 'clear_agent_cursor') {
  post({ kind: 'agent_target_not_found', requestId: msg.requestId, reason: 'cleared' });
}
```

- [x] **Step 3: Implement resolver helpers**

Add JS helpers:

```javascript
function visible(el) {
  const rect = el.getBoundingClientRect();
  const style = getComputedStyle(el);
  return rect.width > 0 && rect.height > 0 && style.visibility !== 'hidden' && style.display !== 'none';
}

function norm(s) {
  return (s || '').replace(/\s+/g, ' ').trim();
}

function elementText(el) {
  return norm(el.innerText || el.textContent || el.getAttribute('aria-label') || '');
}

function roleOf(el) {
  const explicit = el.getAttribute('role');
  if (explicit) return explicit;
  const tag = el.tagName.toLowerCase();
  if (tag === 'button') return 'button';
  if (tag === 'a' && el.hasAttribute('href')) return 'link';
  if (tag === 'input') {
    const type = (el.getAttribute('type') || 'text').toLowerCase();
    if (type === 'submit' || type === 'button') return 'button';
    return 'textbox';
  }
  return null;
}

function accessibleName(el) {
  return norm(el.getAttribute('aria-label') || el.value || elementText(el));
}
```

- [x] **Step 4: Implement target serialization**

Add:

```javascript
function serializeTarget(el, confidence) {
  const rect = el.getBoundingClientRect();
  return {
    selector: cssPath(el),
    tag: el.tagName.toLowerCase(),
    text: elementText(el).slice(0, 500),
    role: roleOf(el),
    accessibleName: accessibleName(el).slice(0, 500),
    rect: { x: rect.left, y: rect.top, w: rect.width, h: rect.height },
    source: detectReactSource(el),
    confidence
  };
}
```

- [ ] **Step 5: Swap script injection**

In `webview2_host.rs`, replace:

```rust
let script_h = HSTRING::from(crate::design_mode_script::SCRIPT);
```

with:

```rust
let script_h = HSTRING::from(crate::browser_automation_script::SCRIPT);
```

In `browser_viewer.rs`, add the Windows module:

```rust
#[cfg(target_os = "windows")]
mod browser_automation_script;
```

- [x] **Step 6: Run check**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
cargo check -p browser_viewer
```

Expected: pass.

## Task 3: Cursor State and Overlay

**Files:**
- Create: `crates/browser_viewer/src/agent_cursor.rs`
- Modify: `crates/browser_viewer/src/browser_viewer.rs`
- Modify: `crates/browser_viewer/src/browser_view.rs`

- [x] **Step 1: Add cursor state types**

Create `agent_cursor.rs`:

```rust
use crate::browser_protocol::BrowserResolvedElement;

#[derive(Debug, Clone)]
pub enum AgentCursorStatus {
    Preview,
    Clicking,
    Clicked,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct AgentCursorState {
    pub request_id: String,
    pub target: BrowserResolvedElement,
    pub status: AgentCursorStatus,
    pub label: String,
    pub ambiguity: Vec<BrowserResolvedElement>,
}

impl AgentCursorState {
    pub fn preview(request_id: String, target: BrowserResolvedElement) -> Self {
        let label = target
            .accessible_name
            .as_ref()
            .or(target.text.as_ref())
            .map(|text| {
                let tag = target.tag.as_deref().unwrap_or("element");
                format!("{tag} \"{}\"", text.chars().take(48).collect::<String>())
            })
            .unwrap_or_else(|| target.tag.clone().unwrap_or_else(|| "element".to_string()));

        Self {
            request_id,
            target,
            status: AgentCursorStatus::Preview,
            label,
            ambiguity: Vec::new(),
        }
    }
}
```

- [x] **Step 2: Export module**

In `browser_viewer.rs`, add:

```rust
pub mod agent_cursor;
```

- [x] **Step 3: Add state to BrowserItem**

In `BrowserItem`, add:

```rust
pub agent_cursor: Option<crate::agent_cursor::AgentCursorState>,
```

Initialize with `None`.

- [x] **Step 4: Clear on navigation**

In `apply_navigation_event`, when handling `NavigationStarting`, also set:

```rust
item.agent_cursor = None;
```

- [x] **Step 5: Render overlay**

In `BrowserView::render`, after drawing overlay handling, render an `AgentCursorOverlayElement` when `agent_cursor.is_some()`.

The element should paint:

- green or accent outline for preview
- warning color for weak/ambiguous
- small marker at rect center
- compact label near the top-left of the rect

- [x] **Step 6: Run check**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
cargo check -p browser_viewer
```

Expected: pass.

## Task 4: Preview Actions

**Files:**
- Modify: `crates/browser_viewer/src/browser_viewer.rs`
- Modify: `crates/browser_viewer/src/browser_view.rs`

- [x] **Step 1: Add actions**

In `browser_viewer.rs`, add actions:

```rust
PreviewSelectedElement,
ClickPreviewedElement,
ClearAgentCursor
```

For text/selector commands, if action structs with fields are needed, define them manually with `gpui::Action` like nearby Zed action patterns.

- [x] **Step 2: Add request helper**

In `BrowserView`, add a helper that serializes `BrowserElementQuery` and posts a JSON `find_element` message to WebView2.

- [x] **Step 3: Handle automation inbound messages**

In `apply_navigation_event`, keep design messages working. If `DesignInbound::parse` fails, try `BrowserAutomationInbound::parse`.

On `AgentTargetResolved`, set `item.agent_cursor = Some(AgentCursorState::preview(...))`.

On not found, set failed state or log and notify.

On ambiguous, choose the first candidate for preview and keep all candidates in `ambiguity`.

- [x] **Step 4: Preview selected element**

Implement `PreviewSelectedElement` by converting current `design_selection` into a `BrowserResolvedElement` and setting `agent_cursor`.

- [x] **Step 5: Clear action**

Implement `ClearAgentCursor` by setting `agent_cursor = None`.

- [x] **Step 6: Run check**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
cargo check -p browser_viewer
```

Expected: pass.

## Task 5: Native Click Previewed Target

**Files:**
- Modify: `crates/browser_viewer/src/browser_view.rs`

- [x] **Step 1: Implement click helper**

Add `click_agent_cursor_target` to `BrowserView`.

It should:

- read `agent_cursor`
- read `last_bounds`
- compute center of target rect
- clamp inside viewport
- call `send_mouse_input` for move/down/up
- clear cursor or mark failure

- [x] **Step 2: Wire action**

Register `ClickPreviewedElement` in `BrowserView::render` and call the helper.

- [x] **Step 3: Preserve focus**

After click, focus the `BrowserView` root only if the click target is the page. Do not focus the URL editor.

- [x] **Step 4: Run check**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
cargo check -p browser_viewer
```

Expected: pass.

## Task 6: Build and Manual Verification

**Files:**
- No planned source edits unless verification reveals defects.

- [x] **Step 1: Build Zed**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
$env:CARGO_INCREMENTAL='0'
$env:CARGO_PROFILE_DEV_DEBUG='0'
cargo build -p zed
```

Expected: build succeeds.

- [x] **Step 2: Launch Zed**

Run:

```powershell
Start-Process -FilePath 'D:\ccoetzeeinspired-zed-target\debug\zed.exe' -ArgumentList 'D:\ccoetzeeinspired-zed' -WorkingDirectory 'D:\ccoetzeeinspired-zed'
```

Expected: Zed opens.

- [ ] **Step 3: Manual browser checks**

Verify:

- `browser::NewTab` opens an embedded browser tab.
- URL bar accepts input and navigates.
- Navigate to `file:///D:/ccoetzeeinspired-zed/docs/superpowers/browser-agent-cursor-test.html`.
- Target text `Sign in` previews the `Sign in` button.
- Clicking the previewed target changes page output to `clicked: Sign in`.
- Target selector `css:[data-testid="create-account-button"]` previews the `Create Account` button.
- Clicking the previewed target changes page output to `clicked: Create Account`.
- Design mode still selects elements.
- Drawing mode still draws.
- Preview selected element displays overlay.
- Click previewed selected element triggers page click behavior.
- Overlay clears after click.
- Keyboard focus returns to normal editor/browser behavior.

## Task 7: Agent Bridge

**Files:**
- Modify: `crates/zed_actions/src/lib.rs`
- Modify: `crates/agent_ui/src/agent_panel.rs`
- Possibly modify: `crates/browser_viewer/src/browser_view.rs`

- [x] **Step 1: Add agent actions**

Add structs under `zed_actions::agent`:

```rust
#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct BrowserResolveElement {
    pub query_kind: SharedString,
    pub query: SharedString,
}

#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct BrowserClickResolvedElement {
    pub request_id: SharedString,
}

#[derive(Clone, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct BrowserClearAgentCursor;
```

- [x] **Step 2: Route active browser item**

In `agent_panel.rs`, register actions that locate the active workspace item and dispatch to browser actions only if the item is a `BrowserView`.

- [ ] **Step 3: Report errors to active thread**

If no active browser tab exists, send a short message to the active agent thread:

```text
No active browser tab is available. Open one with browser::NewTab first.
```

- [x] **Step 4: Run check**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
cargo check -p zed
```

Expected: pass.

## Task 8: Final Verification

- [ ] **Step 1: Full build**

Run:

```powershell
$env:CARGO_TARGET_DIR='D:\ccoetzeeinspired-zed-target'
$env:CARGO_INCREMENTAL='0'
$env:CARGO_PROFILE_DEV_DEBUG='0'
cargo build -p zed
```

Expected: pass.

- [ ] **Step 2: Inspect git diff**

Run:

```powershell
git diff --stat
git diff -- crates/browser_viewer crates/agent_ui crates/zed_actions docs/superpowers
```

Expected: changes are scoped to the browser cursor feature and docs.

- [ ] **Step 3: Runtime smoke**

Launch Zed and verify:

- open embedded browser
- navigate local URL
- preview target by selector/text
- click target
- design mode regression
- drawing regression
- no focus trap after switching tabs

- [ ] **Step 4: Commit**

Run:

```powershell
git add crates/browser_viewer crates/agent_ui crates/zed_actions docs/superpowers
git commit -m "feat: add agent browser cursor"
```
