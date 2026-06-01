//! Phase 1.A: a real Zed tab that hosts a WebView2 via composition mode.
//!
//! `BrowserItem` is the model — owns the WebView2 session, the URL, the
//! title. `BrowserView` is the GPUI view — implements `workspace::Item` so
//! it can be added to a pane, and renders a custom `BrowserViewportElement`
//! whose `Element::prepaint` receives the absolute window-relative
//! `Bounds<Pixels>` of the tab. That bounds drives the WebView2 visual's
//! position and the controller's `SetBounds`, so the rendered page stays
//! aligned with the GPUI element through window resize, sidebar toggle, etc.
//!
//! WebView2 init is async (env -> composition controller -> navigate). We
//! kick it off on the first `prepaint` that has a non-empty rect; a
//! `futures::channel::oneshot` carries the resulting `WebView2Session`
//! back into the entity on the GPUI foreground task, where we
//! `cx.notify()` for a re-render.

use anyhow::{Context as _, anyhow};
use editor::Editor;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{
    Anchor, AnchoredPositionMode, AnyElement, App, AppContext as _, Bounds, Context, Div, Element,
    ElementId, Entity, EntityId, EventEmitter, FocusHandle, Focusable, GlobalElementId,
    InspectorElementId, IntoElement, KeyDownEvent, KeyUpEvent, Keystroke, LayoutId, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels, Point,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, Style, Task, WeakEntity, Window, anchored,
    deferred, div, point, px, relative, size,
};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
    time::Duration,
};
use ui::Tooltip;
use ui::prelude::*;
use workspace::{
    Workspace,
    browser_agent::{BrowserAgentRequest, register_active_browser},
    item::{Item, ItemBufferKind, ItemEvent},
};

#[cfg(target_os = "windows")]
mod windows_imports {
    pub use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    pub use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_MOUSE_EVENT_KIND, COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
        COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOUBLE_CLICK,
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
        COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP, COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
        COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_CONTROL,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_LEFT_BUTTON,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_MIDDLE_BUTTON,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_RIGHT_BUTTON,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_SHIFT,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON1,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON2,
    };
    pub use windows::Win32::Foundation::HWND;
    pub use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
}

#[cfg(target_os = "windows")]
use windows_imports::*;

#[cfg(target_os = "windows")]
use crate::webview2_host::{NavigationEvent, WebView2Session, initialize};

/// One notch on a mouse wheel; matches Win32 `WHEEL_DELTA`.
#[cfg(target_os = "windows")]
const WHEEL_DELTA: f32 = 120.0;

#[cfg(target_os = "windows")]
const AGENT_CURSOR_STEPS: usize = 12;
#[cfg(target_os = "windows")]
const AGENT_CURSOR_STEP_MS: u64 = 32;
#[cfg(target_os = "windows")]
const AGENT_CURSOR_POST_CLICK_DELAY_MS: u64 = 80;
#[cfg(target_os = "windows")]
const AGENT_SCROLL_STEP_MS: u64 = 70;
#[cfg(target_os = "windows")]
const AGENT_TEXT_RESULT_POLL_MS: u64 = 50;
#[cfg(target_os = "windows")]
const AGENT_TEXT_RESULT_MAX_POLLS: usize = 80;
#[cfg(target_os = "windows")]
const AGENT_FIND_RESULT_POLL_MS: u64 = 50;
#[cfg(target_os = "windows")]
const AGENT_FIND_RESULT_MAX_POLLS: usize = 80;
#[cfg(target_os = "windows")]
const AGENT_PAGE_STATE_POLL_MS: u64 = 50;
#[cfg(target_os = "windows")]
const AGENT_PAGE_STATE_MAX_POLLS: usize = 80;
#[cfg(target_os = "windows")]
const AGENT_SNAPSHOT_RESULT_POLL_MS: u64 = 50;
#[cfg(target_os = "windows")]
const AGENT_SNAPSHOT_RESULT_MAX_POLLS: usize = 80;
#[cfg(target_os = "windows")]
const AGENT_ACTIONABILITY_RESULT_POLL_MS: u64 = 50;
#[cfg(target_os = "windows")]
const AGENT_ACTIONABILITY_RESULT_MAX_POLLS: usize = 80;
#[cfg(target_os = "windows")]
const AGENT_TRACE_MAX_ENTRIES: usize = 50;

#[cfg(target_os = "windows")]
struct AgentBrowserOperation {
    request_id: String,
    target: crate::browser_protocol::BrowserResolvedElement,
    label: String,
    text: Option<String>,
    response_tx: oneshot::Sender<anyhow::Result<serde_json::Value>>,
}

/// Backing model for one browser tab.
pub struct BrowserItem {
    /// Currently displayed URL. Starts as the URL the tab was opened with and
    /// updates on every navigation (including `pushState`).
    url: SharedString,
    /// Page `<title>`, falling back to the URL until the first
    /// `DocumentTitleChanged` arrives.
    title: SharedString,
    /// True between the first `NavigationStarting` and the corresponding
    /// `NavigationCompleted`. The address bar (Phase 1.B) will turn the
    /// reload button into a stop button while this is set.
    is_loading: bool,
    /// Browser-history state from the most recent `HistoryChanged`.
    /// Drives the address bar's back/forward button enabled state.
    can_go_back: bool,
    can_go_forward: bool,
    #[cfg(target_os = "windows")]
    session: Option<WebView2Session>,
    /// True between kicking off WebView2 init and the session landing in
    /// `session`. Prevents re-triggering init on every prepaint.
    init_started: bool,
    last_bounds: Option<Bounds<Pixels>>,
    /// Tracks whether the WebView is currently shown. Mirrors the last
    /// value passed to `controller.SetIsVisible`. Drives Phase 1.E
    /// tab-switch hide/show so an inactive browser tab's contents don't
    /// bleed through behind the active tab in the same pane.
    is_visible: bool,
    /// Phase 4: whether design-mode is armed on this tab. The injected
    /// script tracks the same flag client-side; this mirror lets the
    /// host render the indicator and decide whether to forward
    /// `clear_selection` / dispatch the bundle.
    pub design_mode_enabled: bool,
    /// The element selected by the last design-mode click. Cleared on
    /// deactivate, on navigation, and on explicit clear-selection.
    pub design_selection: Option<crate::design::ElementSelection>,
    /// User-entered prompt text in the "Describe the change" input.
    /// Persisted on the item so it survives Render() recreations of
    /// the input widget.
    pub design_prompt: String,
    /// Phase 4.D: freehand drawing overlay state — see `drawing.rs`.
    /// Independent toggle from design mode so the user can draw without
    /// having picked an element, or pick an element and add scribbles
    /// pointing at it before submitting.
    pub drawing_mode_enabled: bool,
    pub drawing: crate::drawing::DrawingCanvas,
    /// Agent-controlled cursor preview target. Rendered by GPUI and clicked
    /// through native WebView2 mouse input.
    pub agent_cursor: Option<crate::agent_cursor::AgentCursorState>,
    #[cfg(target_os = "windows")]
    agent_operation_queue: VecDeque<AgentBrowserOperation>,
    #[cfg(target_os = "windows")]
    agent_operation_running: bool,
    #[cfg(target_os = "windows")]
    agent_text_results: HashMap<String, crate::browser_protocol::AgentTypeTextOutcome>,
    #[cfg(target_os = "windows")]
    agent_find_results: HashMap<String, serde_json::Value>,
    #[cfg(target_os = "windows")]
    agent_snapshot_results: HashMap<String, crate::browser_protocol::AgentVisibleElementsSnapshot>,
    #[cfg(target_os = "windows")]
    agent_snapshots: HashMap<String, crate::browser_protocol::BrowserAgentSnapshot>,
    #[cfg(target_os = "windows")]
    latest_snapshot: Option<crate::browser_protocol::BrowserAgentSnapshot>,
    #[cfg(target_os = "windows")]
    agent_actionability_results:
        HashMap<String, crate::browser_protocol::BrowserActionabilityOutcome>,
    #[cfg(target_os = "windows")]
    agent_page_results: HashMap<String, crate::browser_protocol::AgentPageState>,
    #[cfg(target_os = "windows")]
    agent_trace: VecDeque<crate::browser_protocol::BrowserAgentTraceEntry>,
    #[cfg(target_os = "windows")]
    agent_trace_sequence: u64,
    #[cfg(target_os = "windows")]
    agent_console_events: VecDeque<crate::browser_protocol::BrowserConsoleEventSummary>,
    #[cfg(target_os = "windows")]
    agent_network_events: VecDeque<crate::browser_protocol::BrowserNetworkEventSummary>,
    #[cfg(target_os = "windows")]
    agent_diagnostic_sequence: u64,
    /// Phase 4.C: user drag offset for the "Describe the change" panel,
    /// added to its element-anchored base position so it can be moved off
    /// whatever it covers. Reset on new selection / panel close.
    design_panel_offset: Point<Pixels>,
    /// Active panel drag: (mouse position at drag start, panel offset at
    /// drag start). `Some` while the panel header is held.
    design_drag: Option<(Point<Pixels>, Point<Pixels>)>,
}

impl BrowserItem {
    fn new(url: SharedString) -> Self {
        Self {
            title: url.clone(),
            url,
            is_loading: false,
            can_go_back: false,
            can_go_forward: false,
            #[cfg(target_os = "windows")]
            session: None,
            init_started: false,
            last_bounds: None,
            is_visible: true,
            design_mode_enabled: false,
            design_selection: None,
            design_prompt: String::new(),
            drawing_mode_enabled: false,
            drawing: crate::drawing::DrawingCanvas::default(),
            agent_cursor: None,
            #[cfg(target_os = "windows")]
            agent_operation_queue: VecDeque::new(),
            #[cfg(target_os = "windows")]
            agent_operation_running: false,
            #[cfg(target_os = "windows")]
            agent_text_results: HashMap::default(),
            #[cfg(target_os = "windows")]
            agent_find_results: HashMap::default(),
            #[cfg(target_os = "windows")]
            agent_snapshot_results: HashMap::default(),
            #[cfg(target_os = "windows")]
            agent_snapshots: HashMap::default(),
            #[cfg(target_os = "windows")]
            latest_snapshot: None,
            #[cfg(target_os = "windows")]
            agent_actionability_results: HashMap::default(),
            #[cfg(target_os = "windows")]
            agent_page_results: HashMap::default(),
            #[cfg(target_os = "windows")]
            agent_trace: VecDeque::new(),
            #[cfg(target_os = "windows")]
            agent_trace_sequence: 0,
            #[cfg(target_os = "windows")]
            agent_console_events: VecDeque::new(),
            #[cfg(target_os = "windows")]
            agent_network_events: VecDeque::new(),
            #[cfg(target_os = "windows")]
            agent_diagnostic_sequence: 0,
            design_panel_offset: point(px(0.), px(0.)),
            design_drag: None,
        }
    }

    pub fn url(&self) -> &SharedString {
        &self.url
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    pub fn is_loading(&self) -> bool {
        self.is_loading
    }

    pub fn can_go_back(&self) -> bool {
        self.can_go_back
    }

    pub fn can_go_forward(&self) -> bool {
        self.can_go_forward
    }
}

fn browser_target_context(target: &crate::browser_protocol::BrowserResolvedElement) -> String {
    let label = target
        .accessible_name
        .as_deref()
        .or(target.text.as_deref())
        .map(|text| text.chars().take(120).collect::<String>())
        .unwrap_or_else(|| target.selector.clone());
    let tag = target.tag.as_deref().unwrap_or("element");
    format!(
        "{} \"{}\"\n  Selector: {}\n  Bounds: x={}, y={}, width={}, height={}",
        tag, label, target.selector, target.rect.x, target.rect.y, target.rect.w, target.rect.h
    )
}

impl EventEmitter<()> for BrowserItem {}

/// Events emitted by `BrowserView`. Tracking these separately from
/// `BrowserItem`'s notifications lets us translate them into the
/// workspace's `ItemEvent` vocabulary.
pub enum BrowserViewEvent {
    /// Tab title or icon should be re-fetched.
    UpdateTab,
}

/// The Zed tab view.
pub struct BrowserView {
    item: Entity<BrowserItem>,
    focus_handle: FocusHandle,
    url_editor: Entity<Editor>,
    agent_target_editor: Entity<Editor>,
    /// Phase 4.C: editor for the "Describe the change" floating input.
    /// Always lives — we render it only when the item has a selection
    /// and design mode is on, but keeping the entity persistent avoids
    /// dropping in-flight text on a notify-driven re-render.
    design_prompt_editor: Entity<Editor>,
    /// Weak ref to the owning workspace, set in `added_to_workspace`.
    /// Used to query `has_active_modal()` so the browser visual can hide
    /// while a modal (command palette, file finder, etc.) is open —
    /// otherwise it draws on top of every GPUI-rendered overlay because
    /// WebView2's DComp visual sits above the Zed swap chain in the
    /// composition tree.
    workspace: Option<WeakEntity<Workspace>>,
    /// Selector the prompt editor was last focused for. Lets us re-focus on
    /// each new element selection so the panel's Esc / Ctrl+Enter shortcuts
    /// fire and those keys don't leak to the page.
    design_prompt_focused_for: Option<SharedString>,
}

impl BrowserView {
    pub fn new(url: SharedString, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let item = cx.new(|_| BrowserItem::new(url.clone()));

        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(url.as_ref(), window, cx);
            editor
        });

        let agent_target_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Target text or css:selector", window, cx);
            editor
        });

        let design_prompt_editor = cx.new(|cx| {
            // Gutter-less, soft-wrapping, auto-growing prompt input —
            // configured like Zed's agent chat editor so it reads as a
            // text box, not a code editor.
            let mut editor = Editor::auto_height(3, 8, window, cx);
            editor.set_placeholder_text("Describe the change you want…", window, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_soft_wrap();
            editor
        });

        // Re-emit + re-render on every BrowserItem change. The tab strip
        // picks up the UpdateTab event; the render pass syncs the URL
        // editor from the model when needed (it has &mut Window).
        cx.observe(&item, move |_view, _item, cx| {
            cx.emit(BrowserViewEvent::UpdateTab);
            cx.notify();
        })
        .detach();

        #[cfg(target_os = "windows")]
        {
            Self::register_as_active_browser(cx);
        }

        Self {
            item,
            focus_handle: cx.focus_handle(),
            url_editor,
            agent_target_editor,
            design_prompt_editor,
            workspace: None,
            design_prompt_focused_for: None,
        }
    }

    pub fn item(&self) -> &Entity<BrowserItem> {
        &self.item
    }

    #[cfg(target_os = "windows")]
    fn register_as_active_browser(cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        register_active_browser(Rc::new(move |request, cx| {
            weak.update(cx, |view, cx| view.handle_browser_agent_request(request, cx))
                .unwrap_or_else(|_| {
                    Task::ready(Err(anyhow!(
                        "Active Zed browser tab is no longer available"
                    )))
                })
        }));
    }

    #[cfg(target_os = "windows")]
    fn navigate_to(&self, target: String, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                let url_h = windows::core::HSTRING::from(&target);
                unsafe {
                    if let Err(err) = session
                        .webview
                        .Navigate(windows::core::PCWSTR(url_h.as_ptr()))
                    {
                        log::warn!("BrowserView::navigate_to({target}): {err}");
                    }
                }
            }
        });
    }

    #[cfg(target_os = "windows")]
    fn go_back(&self, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                unsafe {
                    let _ = session.webview.GoBack();
                }
            }
        });
    }

    #[cfg(target_os = "windows")]
    fn go_forward(&self, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                unsafe {
                    let _ = session.webview.GoForward();
                }
            }
        });
    }

    #[cfg(target_os = "windows")]
    fn reload_page(&self, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                unsafe {
                    let _ = session.webview.Reload();
                }
            }
        });
    }

    #[cfg(target_os = "windows")]
    fn stop_loading(&self, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                unsafe {
                    let _ = session.webview.Stop();
                }
            }
        });
    }

    fn on_toggle_drawing_mode(
        &mut self,
        _: &crate::ToggleDrawingMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item.update(cx, |item, cx| {
            item.drawing_mode_enabled = !item.drawing_mode_enabled;
            cx.notify();
        });
    }

    fn on_clear_drawing(
        &mut self,
        _: &crate::ClearDrawing,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item.update(cx, |item, cx| {
            item.drawing.clear();
            cx.notify();
        });
    }

    fn on_preview_selected_element(
        &mut self,
        _: &crate::PreviewSelectedElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item.update(cx, |item, cx| {
            let Some(selection) = item.design_selection.clone() else {
                item.agent_cursor = Some(crate::agent_cursor::AgentCursorState {
                    request_id: "selected".to_string(),
                    target: crate::browser_protocol::BrowserResolvedElement {
                        selector: "selected".to_string(),
                        tag: Some("missing".to_string()),
                        text: Some("No selected browser element".to_string()),
                        role: None,
                        accessible_name: Some("No selected browser element".to_string()),
                        rect: crate::design::ElementRect {
                            x: 0.,
                            y: 0.,
                            w: 0.,
                            h: 0.,
                        },
                        source: None,
                        confidence: crate::browser_protocol::BrowserTargetConfidence::Weak,
                    },
                    status: crate::agent_cursor::AgentCursorStatus::Failed(
                        "No selected browser element".to_string(),
                    ),
                    label: "No selected browser element".to_string(),
                    ambiguity: Vec::new(),
                    pointer_position: None,
                });
                cx.notify();
                return;
            };

            let target = crate::browser_protocol::BrowserResolvedElement {
                selector: selection.selector,
                tag: selection.tag,
                text: Some(selection.outer_html.chars().take(120).collect()),
                role: None,
                accessible_name: None,
                rect: selection.rect,
                source: selection.source,
                confidence: crate::browser_protocol::BrowserTargetConfidence::Exact,
            };
            item.agent_cursor = Some(crate::agent_cursor::AgentCursorState::preview(
                "selected".to_string(),
                target,
            ));
            cx.notify();
        });
    }

    fn on_clear_agent_cursor(
        &mut self,
        _: &crate::ClearAgentCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item.update(cx, |item, cx| {
            item.agent_cursor = None;
            cx.notify();
        });
    }

    #[cfg(target_os = "windows")]
    fn preview_agent_target_from_editor(&self, cx: &mut Context<Self>) {
        let input = self.agent_target_editor.read(cx).text(cx);
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return;
        }
        let query = if let Some(selector) = trimmed
            .strip_prefix("css:")
            .or_else(|| trimmed.strip_prefix("selector:"))
        {
            crate::browser_protocol::BrowserElementQuery::Selector {
                selector: selector.trim().to_string(),
            }
        } else {
            crate::browser_protocol::BrowserElementQuery::TextContains {
                text: trimmed.to_string(),
            }
        };
        self.post_find_element("toolbar".to_string(), query, cx)
            .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn on_click_previewed_element(
        &mut self,
        _: &crate::ClickPreviewedElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        click_agent_cursor_target(self, cx, window);
    }

    #[cfg(not(target_os = "windows"))]
    fn on_click_previewed_element(
        &mut self,
        _: &crate::ClickPreviewedElement,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn on_toggle_design_mode(
        &mut self,
        _: &crate::ToggleDesignMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "windows")]
        self.item.update(cx, |item, cx| {
            item.design_mode_enabled = !item.design_mode_enabled;
            let msg = if item.design_mode_enabled {
                "activate"
            } else {
                item.design_selection = None;
                item.design_prompt.clear();
                "deactivate"
            };
            if let Some(session) = &item.session {
                if let Err(err) = session.post_message_string(msg) {
                    log::warn!("browser_viewer: design-mode toggle post failed: {err}");
                }
            }
            cx.notify();
        });
        #[cfg(not(target_os = "windows"))]
        let _ = cx;
    }

    fn on_focus_address_bar(
        &mut self,
        _: &crate::FocusAddressBar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor_focus = self.url_editor.focus_handle(cx);
        window.focus(&editor_focus, cx);
        // Select all so typing replaces the current URL — matches real
        // browser Ctrl+L behavior.
        self.url_editor.update(cx, |editor, cx| {
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
    }

    fn on_open_devtools(
        &mut self,
        _: &crate::OpenDevTools,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "windows")]
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                unsafe {
                    if let Err(err) = session.webview.OpenDevToolsWindow() {
                        log::warn!("browser_viewer: OpenDevToolsWindow failed: {err}");
                    }
                }
            }
        });
        #[cfg(not(target_os = "windows"))]
        let _ = cx;
    }

    #[cfg(target_os = "windows")]
    fn on_agent_resolve_element(
        &mut self,
        action: &zed_actions::agent::BrowserResolveElement,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = match browser_query_from_parts(&action.query_kind, &action.query) {
            Ok(query) => query,
            Err(reason) => {
                self.item.update(cx, |item, cx| {
                    item.agent_cursor = Some(crate::agent_cursor::AgentCursorState {
                        request_id: "agent".to_string(),
                        target: crate::browser_protocol::BrowserResolvedElement {
                            selector: "invalid-query".to_string(),
                            tag: Some("missing".to_string()),
                            text: Some(reason.clone()),
                            role: None,
                            accessible_name: Some(reason.clone()),
                            rect: crate::design::ElementRect {
                                x: 0.,
                                y: 0.,
                                w: 0.,
                                h: 0.,
                            },
                            source: None,
                            confidence: crate::browser_protocol::BrowserTargetConfidence::Weak,
                        },
                        status: crate::agent_cursor::AgentCursorStatus::Failed(reason.clone()),
                        label: reason,
                        ambiguity: Vec::new(),
                        pointer_position: None,
                    });
                    cx.notify();
                });
                return;
            }
        };
        self.post_find_element("agent".to_string(), query, cx)
            .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn on_agent_click_resolved_element(
        &mut self,
        action: &zed_actions::agent::BrowserClickResolvedElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request_id = action.request_id.as_ref().trim();
        if !request_id.is_empty() {
            let request_matches = self
                .item
                .read(cx)
                .agent_cursor
                .as_ref()
                .is_some_and(|cursor| cursor.request_id == request_id);
            if !request_matches {
                self.item.update(cx, |item, cx| {
                    item.agent_cursor = Some(failed_agent_cursor(
                        "agent",
                        "No matching browser target preview",
                    ));
                    cx.notify();
                });
                return;
            }
        }
        click_agent_cursor_target(self, cx, window);
    }

    #[cfg(target_os = "windows")]
    fn handle_browser_agent_request(
        &mut self,
        request: BrowserAgentRequest,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        match request {
            BrowserAgentRequest::CurrentPage => {
                Task::ready(Ok(self.browser_agent_current_page(cx)))
            }
            BrowserAgentRequest::Open { url } => {
                self.navigate_to(url.clone(), cx);
                Task::ready(Ok(serde_json::json!({
                    "ok": true,
                    "url": url,
                    "message": "Opened URL in the active Zed embedded browser tab"
                })))
            }
            BrowserAgentRequest::Navigate { url } => {
                self.navigate_to(url.clone(), cx);
                Task::ready(Ok(serde_json::json!({
                    "ok": true,
                    "url": url,
                    "message": "Navigation requested in the active Zed embedded browser tab"
                })))
            }
            BrowserAgentRequest::Snapshot { request_id } => {
                self.post_visible_elements_snapshot(request_id, cx)
            }
            BrowserAgentRequest::Screenshot { request_id } => {
                self.post_browser_screenshot(request_id, cx)
            }
            BrowserAgentRequest::ClickRef {
                snapshot_id,
                element_ref,
            } => self.enqueue_snapshot_ref_operation(snapshot_id, element_ref, None, cx),
            BrowserAgentRequest::FillRef {
                snapshot_id,
                element_ref,
                text,
                submit: _,
            } => self.enqueue_snapshot_ref_operation(snapshot_id, element_ref, Some(text), cx),
            BrowserAgentRequest::ScrollToRef {
                request_id,
                snapshot_id,
                element_ref,
                align,
            } => self.post_scroll_to_snapshot_ref(request_id, snapshot_id, element_ref, align, cx),
            BrowserAgentRequest::Scroll {
                request_id,
                delta_x,
                delta_y,
                steps,
                x,
                y,
            } => self.post_agent_scroll(request_id, delta_x, delta_y, steps, x, y, cx),
            BrowserAgentRequest::FindElement {
                request_id,
                query_kind,
                query,
            } => {
                let query = match browser_query_from_parts(&query_kind, &query) {
                    Ok(query) => query,
                    Err(err) => return Task::ready(Err(anyhow!(err))),
                };
                self.post_find_element(request_id, query, cx)
            }
            BrowserAgentRequest::ClickElement { request_id } => {
                self.enqueue_agent_cursor_operation(&request_id, None, cx)
            }
            BrowserAgentRequest::TypeText { request_id, text } => {
                self.enqueue_agent_cursor_operation(&request_id, Some(text), cx)
            }
            BrowserAgentRequest::Trace => {
                let item = self.item.read(cx);
                Task::ready(Ok(serde_json::json!({
                    "ok": true,
                    "entries": item.agent_trace.iter().cloned().collect::<Vec<_>>(),
                    "console": item.agent_console_events.iter().cloned().collect::<Vec<_>>(),
                    "network": item.agent_network_events.iter().cloned().collect::<Vec<_>>(),
                })))
            }
            BrowserAgentRequest::Console { level, limit } => {
                Task::ready(Ok(self.browser_agent_console(level, limit, cx)))
            }
            BrowserAgentRequest::Network {
                failures_only,
                limit,
            } => Task::ready(Ok(self.browser_agent_network(failures_only, limit, cx))),
            BrowserAgentRequest::Expect {
                kind,
                value,
                selector,
                timeout_ms,
            } => self.post_browser_expect(kind, value, selector, timeout_ms, cx),
            BrowserAgentRequest::ClearCursor => {
                self.item.update(cx, |item, cx| {
                    item.agent_cursor = None;
                    item.agent_operation_queue.clear();
                    item.agent_operation_running = false;
                    cx.notify();
                });
                Task::ready(Ok(serde_json::json!({
                    "ok": true,
                    "message": "Browser cursor preview cleared"
                })))
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn browser_agent_current_page(&self, cx: &App) -> serde_json::Value {
        let item = self.item.read(cx);
        let cursor = item.agent_cursor.as_ref().map(|cursor| {
            serde_json::json!({
                "requestId": cursor.request_id,
                "selector": cursor.target.selector,
                "tag": cursor.target.tag,
                "text": cursor.target.text,
                "role": cursor.target.role,
                "accessibleName": cursor.target.accessible_name,
                "bounds": {
                    "x": cursor.target.rect.x,
                    "y": cursor.target.rect.y,
                    "width": cursor.target.rect.w,
                    "height": cursor.target.rect.h,
                },
                "ambiguityCount": cursor.ambiguity.len(),
            })
        });
        let drawing_overlay = browser_drawing_overlay_json(&item);
        serde_json::json!({
            "url": item.url(),
            "title": item.title(),
            "hasSession": item.session.is_some(),
            "viewportReady": item.last_bounds.is_some(),
            "agentBusy": item.agent_operation_running || !item.agent_operation_queue.is_empty(),
            "agentQueueLength": item.agent_operation_queue.len(),
            "previewedTarget": cursor,
            "drawingOverlay": drawing_overlay,
        })
    }

    #[cfg(target_os = "windows")]
    fn browser_agent_console(
        &self,
        level: Option<String>,
        limit: usize,
        cx: &App,
    ) -> serde_json::Value {
        let item = self.item.read(cx);
        let normalized_level = level.as_deref().map(str::to_ascii_lowercase);
        let mut events = item
            .agent_console_events
            .iter()
            .filter(|event| {
                normalized_level
                    .as_deref()
                    .map(|level| event.level.eq_ignore_ascii_case(level))
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        events.reverse();
        events.truncate(limit.min(100));
        events.reverse();
        serde_json::json!({
            "ok": true,
            "events": events,
        })
    }

    #[cfg(target_os = "windows")]
    fn browser_agent_network(
        &self,
        failures_only: bool,
        limit: usize,
        cx: &App,
    ) -> serde_json::Value {
        let item = self.item.read(cx);
        let mut events = item
            .agent_network_events
            .iter()
            .filter(|event| {
                if !failures_only {
                    return true;
                }
                event.error_text.is_some()
                    || event.status.map(|status| status >= 400).unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        events.reverse();
        events.truncate(limit.min(100));
        events.reverse();
        serde_json::json!({
            "ok": true,
            "events": events,
        })
    }

    #[cfg(target_os = "windows")]
    fn post_browser_expect(
        &self,
        kind: String,
        value: Option<String>,
        selector: Option<String>,
        timeout_ms: u64,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let item_entity = self.item.clone();
        let timeout_ms = timeout_ms.clamp(100, 10_000);
        cx.spawn(async move |_, cx| {
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
            let result = loop {
                let script = browser_expect_script(&kind, value.as_deref(), selector.as_deref());
                let receiver = item_entity.update(cx, |item, _| {
                    item.session
                        .as_ref()
                        .ok_or_else(|| anyhow!("Browser session is not ready"))
                        .and_then(|session| session.execute_script(&script))
                })?;
                let raw = receiver
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("Browser expect response was dropped")))?;
                let decoded: String = serde_json::from_str(&raw).unwrap_or(raw);
                let result: crate::browser_protocol::BrowserExpectResult =
                    serde_json::from_str(&decoded)?;
                let done = result.ok || std::time::Instant::now() >= deadline;
                if done {
                    break result;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            };
            let (console, network) = item_entity.update(cx, |item, _| {
                push_agent_trace(
                    item,
                    crate::browser_protocol::BrowserAgentTraceEntry {
                        sequence: 0,
                        tool: "browser.expect".to_string(),
                        request_id: None,
                        snapshot_id: None,
                        element_ref: None,
                        selector: selector.clone(),
                        ok: result.ok,
                        reason: result.reason.clone(),
                    },
                );
                (
                    item.agent_console_events.iter().cloned().collect::<Vec<_>>(),
                    item.agent_network_events.iter().cloned().collect::<Vec<_>>(),
                )
            });
            Ok(serde_json::json!({
                "ok": result.ok,
                "kind": result.kind,
                "value": result.value,
                "selector": result.selector,
                "observed": result.observed,
                "reason": result.reason,
                "console": console,
                "network": network,
            }))
        })
    }

    #[cfg(target_os = "windows")]
    fn enqueue_agent_cursor_operation(
        &mut self,
        request_id: &str,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let request_id = request_id.trim();
        let (response_tx, response_rx) = oneshot::channel();
        let operation = {
            let item = self.item.read(cx);
            let Some(cursor) = item.agent_cursor.as_ref() else {
                return Task::ready(Err(anyhow!("No browser target preview")));
            };
            if !request_id.is_empty() && cursor.request_id != request_id {
                return Task::ready(Err(anyhow!("No matching browser target preview")));
            }
            AgentBrowserOperation {
                request_id: cursor.request_id.clone(),
                target: cursor.target.clone(),
                label: cursor.label.clone(),
                text,
                response_tx,
            }
        };

        self.item.update(cx, |item, _| {
            item.agent_operation_queue.push_back(operation);
        });
        self.start_next_agent_operation(cx);
        cx.background_executor().spawn(async move {
            response_rx
                .await
                .unwrap_or_else(|_| Err(anyhow!("Browser operation response was dropped")))
        })
    }

    #[cfg(target_os = "windows")]
    fn enqueue_snapshot_ref_operation(
        &mut self,
        snapshot_id: String,
        element_ref: String,
        text: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let tool = if text.is_some() {
            "browser.fill"
        } else {
            "browser.click"
        };
        let request_id = format!("{}:{}", snapshot_id, element_ref);
        let requires_editable = text.is_some();
        let target = {
            let mut failure = None;
            let target = self.item.update(cx, |item, _| {
                let Some(snapshot) = item.latest_snapshot.as_ref() else {
                    failure = Some("No browser snapshot is available".to_string());
                    return None;
                };
                match find_snapshot_ref(snapshot, &snapshot_id, &element_ref) {
                    Ok(target) => Some(target),
                    Err(err) => {
                        failure = Some(err.to_string());
                        None
                    }
                }
            });
            if let Some(target) = target {
                target
            } else {
                let reason = failure.unwrap_or_else(|| "No element ref in snapshot".to_string());
                self.item.update(cx, |item, _| {
                    push_agent_trace(
                        item,
                        crate::browser_protocol::BrowserAgentTraceEntry {
                            sequence: 0,
                            tool: tool.to_string(),
                            request_id: Some(request_id.clone()),
                            snapshot_id: Some(snapshot_id.clone()),
                            element_ref: Some(element_ref.clone()),
                            selector: None,
                            ok: false,
                            reason: Some(reason.clone()),
                        },
                    );
                });
                return Task::ready(Err(anyhow!(reason)));
            }
        };
        let item_entity = self.item.clone();
        let tool = tool.to_string();
        cx.spawn(async move |this, cx| {
            let actionability_request = serde_json::json!({
                "kind": "actionability",
                "requestId": request_id.clone(),
                "selector": target.selector.clone(),
                "requiresEditable": requires_editable,
            })
            .to_string();
            let post_result = this.update(cx, |_, cx| {
                item_entity.update(cx, |item, _| {
                    item.agent_actionability_results.remove(&request_id);
                    item.session
                        .as_ref()
                        .ok_or_else(|| anyhow!("Browser session is not ready"))
                        .and_then(|session| {
                            session.post_automation_message_string(&actionability_request)
                        })
                })
            })?;
            if let Err(err) = post_result {
                this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        push_agent_trace(
                            item,
                            crate::browser_protocol::BrowserAgentTraceEntry {
                                sequence: 0,
                                tool: tool.clone(),
                                request_id: Some(request_id.clone()),
                                snapshot_id: Some(snapshot_id.clone()),
                                element_ref: Some(element_ref.clone()),
                                selector: Some(target.selector.clone()),
                                ok: false,
                                reason: Some(err.to_string()),
                            },
                        );
                    })
                })?;
                return Ok(serde_json::json!({
                    "ok": false,
                    "requestId": request_id,
                    "selector": target.selector,
                    "reason": err.to_string(),
                }));
            }

            let mut outcome = None;
            for _ in 0..AGENT_ACTIONABILITY_RESULT_MAX_POLLS {
                cx.background_executor()
                    .timer(Duration::from_millis(AGENT_ACTIONABILITY_RESULT_POLL_MS))
                    .await;
                let result = this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        item.agent_actionability_results.remove(&request_id)
                    })
                })?;
                if result.is_some() {
                    outcome = result;
                    break;
                }
            }

            let Some(outcome) = outcome else {
                this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        push_agent_trace(
                            item,
                            crate::browser_protocol::BrowserAgentTraceEntry {
                                sequence: 0,
                                tool: tool.clone(),
                                request_id: Some(request_id.clone()),
                                snapshot_id: Some(snapshot_id.clone()),
                                element_ref: Some(element_ref.clone()),
                                selector: Some(target.selector.clone()),
                                ok: false,
                                reason: Some(
                                    "Timed out waiting for browser actionability check".to_string(),
                                ),
                            },
                        );
                    })
                })?;
                return Ok(serde_json::json!({
                    "ok": false,
                    "requestId": request_id,
                    "selector": target.selector,
                    "reason": "Timed out waiting for browser actionability check",
                }));
            };
            if !outcome.ok {
                this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        push_agent_trace(
                            item,
                            crate::browser_protocol::BrowserAgentTraceEntry {
                                sequence: 0,
                                tool: tool.clone(),
                                request_id: Some(request_id.clone()),
                                snapshot_id: Some(snapshot_id.clone()),
                                element_ref: Some(element_ref.clone()),
                                selector: Some(outcome.selector.clone()),
                                ok: false,
                                reason: outcome.reason.clone(),
                            },
                        );
                    })
                })?;
                return Ok(actionability_failure_response(&request_id, &outcome));
            }

            let operation_task = this.update(cx, |this, cx| {
                item_entity.update(cx, |item, cx| {
                    item.agent_cursor = Some(crate::agent_cursor::AgentCursorState::preview(
                        request_id.clone(),
                        target.clone(),
                    ));
                    cx.notify();
                });
                this.enqueue_agent_cursor_operation(&request_id, text, cx)
            })?;
            let response = operation_task.await;
            this.update(cx, |_, cx| {
                item_entity.update(cx, |item, _| {
                    let (ok, reason) = match &response {
                        Ok(value) => (
                            value.get("ok").and_then(|ok| ok.as_bool()).unwrap_or(false),
                            value
                                .get("reason")
                                .and_then(|reason| reason.as_str())
                                .or_else(|| {
                                    value
                                        .get("snapshotReason")
                                        .and_then(|reason| reason.as_str())
                                })
                                .map(ToOwned::to_owned),
                        ),
                        Err(err) => (false, Some(err.to_string())),
                    };
                    push_agent_trace(
                        item,
                        crate::browser_protocol::BrowserAgentTraceEntry {
                            sequence: 0,
                            tool,
                            request_id: Some(request_id),
                            snapshot_id: Some(snapshot_id),
                            element_ref: Some(element_ref),
                            selector: Some(target.selector),
                            ok,
                            reason,
                        },
                    );
                })
            })?;
            response
        })
    }

    #[cfg(target_os = "windows")]
    fn start_next_agent_operation(&mut self, cx: &mut Context<Self>) {
        let item_entity = self.item.clone();
        let mut plan = None;

        self.item.update(cx, |item, cx| {
            if item.agent_operation_running {
                return;
            }

            let Some(operation) = item.agent_operation_queue.pop_front() else {
                return;
            };

            item.agent_operation_running = true;

            if item.session.is_none() {
                item.agent_cursor = Some(failed_agent_cursor(
                    &operation.request_id,
                    "Browser session is not ready",
                ));
                let _ = operation.response_tx.send(Ok(serde_json::json!({
                    "ok": false,
                    "requestId": operation.request_id,
                    "reason": "Browser session is not ready",
                })));
                item.agent_operation_running = false;
                cx.notify();
                return;
            }

            let Some(bounds) = item.last_bounds else {
                item.agent_cursor = Some(failed_agent_cursor(
                    &operation.request_id,
                    "Browser viewport is not ready",
                ));
                let _ = operation.response_tx.send(Ok(serde_json::json!({
                    "ok": false,
                    "requestId": operation.request_id,
                    "reason": "Browser viewport is not ready",
                })));
                item.agent_operation_running = false;
                cx.notify();
                return;
            };

            let rect = &operation.target.rect;
            let max_x = f32::from(bounds.size.width).max(1.) - 1.;
            let max_y = f32::from(bounds.size.height).max(1.) - 1.;
            let target_x = (rect.x + rect.w / 2.).clamp(0., max_x);
            let target_y = (rect.y + rect.h / 2.).clamp(0., max_y);
            let (start_x, start_y) = item
                .agent_cursor
                .as_ref()
                .and_then(|cursor| cursor.pointer_position)
                .unwrap_or((24., 24.));

            let mut cursor = crate::agent_cursor::AgentCursorState::preview(
                operation.request_id.clone(),
                operation.target.clone(),
            );
            cursor.label = operation.label.clone();
            cursor.status = crate::agent_cursor::AgentCursorStatus::Clicking;
            cursor.pointer_position = Some((start_x, start_y));
            item.agent_cursor = Some(cursor);

            plan = Some((operation, start_x, start_y, target_x, target_y));
            cx.notify();
        });

        let Some((operation, start_x, start_y, target_x, target_y)) = plan else {
            return;
        };

        cx.spawn(async move |this, cx| {
            for step in 1..=AGENT_CURSOR_STEPS {
                let t = step as f32 / AGENT_CURSOR_STEPS as f32;
                let eased = 1. - (1. - t) * (1. - t);
                let x = start_x + (target_x - start_x) * eased;
                let y = start_y + (target_y - start_y) * eased;

                this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, cx| {
                        let mut cursor = crate::agent_cursor::AgentCursorState::preview(
                            operation.request_id.clone(),
                            operation.target.clone(),
                        );
                        cursor.label = operation.label.clone();
                        cursor.status = crate::agent_cursor::AgentCursorStatus::Clicking;
                        cursor.pointer_position = Some((x, y));
                        item.agent_cursor = Some(cursor);

                        if let Some(session) = item.session.as_ref() {
                            let _ = session.dispatch_mouse_event("mouseMoved", x, y, None);
                        }
                        cx.notify();
                    });
                })?;

                cx.background_executor()
                    .timer(Duration::from_millis(AGENT_CURSOR_STEP_MS))
                    .await;
            }

            let click_result = this.update(cx, |_, cx| {
                item_entity.update(cx, |item, cx| {
                    let result = if let Some(session) = item.session.as_ref() {
                        session
                            .dispatch_mouse_event("mousePressed", target_x, target_y, Some("left"))
                            .and_then(|_| {
                                session.dispatch_mouse_event(
                                    "mouseReleased",
                                    target_x,
                                    target_y,
                                    Some("left"),
                                )
                            })
                    } else {
                        Err(anyhow!("Browser session is not ready"))
                    };

                    if let Err(err) = &result {
                        item.agent_cursor =
                            Some(failed_agent_cursor(&operation.request_id, err.to_string()));
                        item.agent_operation_running = false;
                    }
                    cx.notify();
                    result
                })
            })?;

            let mut type_outcome = None;
            if click_result.is_ok()
                && let Some(text) = operation.text.as_ref()
            {
                cx.background_executor()
                    .timer(Duration::from_millis(AGENT_CURSOR_POST_CLICK_DELAY_MS))
                    .await;
                let type_request = serde_json::json!({
                    "kind": "type_text",
                    "requestId": operation.request_id.clone(),
                    "selector": operation.target.selector.clone(),
                    "text": text,
                })
                .to_string();
                let post_result = this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        item.agent_text_results.remove(&operation.request_id);
                        item.session
                            .as_ref()
                            .ok_or_else(|| anyhow!("Browser session is not ready"))
                            .and_then(|session| {
                                session.post_automation_message_string(&type_request)
                            })
                    })
                })?;
                if let Err(err) = post_result {
                    log::warn!("browser_viewer: agent text operation dispatch failed: {err}");
                } else {
                    for _ in 0..AGENT_TEXT_RESULT_MAX_POLLS {
                        cx.background_executor()
                            .timer(Duration::from_millis(AGENT_TEXT_RESULT_POLL_MS))
                            .await;
                        let outcome = this.update(cx, |_, cx| {
                            item_entity.update(cx, |item, _| {
                                item.agent_text_results.remove(&operation.request_id)
                            })
                        })?;
                        if let Some(outcome) = outcome {
                            if !outcome.ok {
                                log::warn!(
                                    "browser_viewer: agent text verification failed for {}: {:?}",
                                    operation.target.selector,
                                    outcome.reason
                                );
                            }
                            type_outcome = Some(outcome);
                            break;
                        }
                    }
                }
            }

            let mut page_state = None;
            if click_result.is_ok() {
                let page_state_request = serde_json::json!({
                    "kind": "page_state",
                    "requestId": operation.request_id.clone(),
                })
                .to_string();
                let post_result = this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        item.agent_page_results.remove(&operation.request_id);
                        item.session
                            .as_ref()
                            .ok_or_else(|| anyhow!("Browser session is not ready"))
                            .and_then(|session| {
                                session.post_automation_message_string(&page_state_request)
                            })
                    })
                })?;
                if post_result.is_ok() {
                    for _ in 0..AGENT_PAGE_STATE_MAX_POLLS {
                        cx.background_executor()
                            .timer(Duration::from_millis(AGENT_PAGE_STATE_POLL_MS))
                            .await;
                        let state = this.update(cx, |_, cx| {
                            item_entity.update(cx, |item, _| {
                                item.agent_page_results.remove(&operation.request_id)
                            })
                        })?;
                        if let Some(state) = state {
                            page_state = Some(state);
                            break;
                        }
                    }
                }
            }

            let mut action_snapshot = None;
            let mut snapshot_reason = None;
            if click_result.is_ok() {
                let snapshot_request_id = format!("{}:after", operation.request_id);
                let snapshot_request = serde_json::json!({
                    "kind": "snapshot",
                    "requestId": snapshot_request_id.clone(),
                })
                .to_string();
                let post_result = this.update(cx, |_, cx| {
                    item_entity.update(cx, |item, _| {
                        item.agent_snapshots.remove(&snapshot_request_id);
                        item.session
                            .as_ref()
                            .ok_or_else(|| anyhow!("Browser session is not ready"))
                            .and_then(|session| {
                                session.post_automation_message_string(&snapshot_request)
                            })
                    })
                })?;

                match post_result {
                    Ok(()) => {
                        for _ in 0..AGENT_SNAPSHOT_RESULT_MAX_POLLS {
                            cx.background_executor()
                                .timer(Duration::from_millis(AGENT_SNAPSHOT_RESULT_POLL_MS))
                                .await;
                            let snapshot = this.update(cx, |_, cx| {
                                item_entity.update(cx, |item, _| {
                                    item.agent_snapshots.remove(&snapshot_request_id)
                                })
                            })?;
                            if let Some(snapshot) = snapshot {
                                if let Some(reason) = snapshot.reason.clone() {
                                    snapshot_reason = Some(reason);
                                }
                                action_snapshot = Some(snapshot);
                                break;
                            }
                        }
                        if action_snapshot.is_none() {
                            snapshot_reason =
                                Some("Timed out waiting for post-action snapshot".to_string());
                        }
                    }
                    Err(err) => {
                        snapshot_reason = Some(err.to_string());
                    }
                }
            }

            let response = this.update(cx, |this, cx| {
                item_entity.update(cx, |item, cx| {
                    if click_result.is_ok() {
                        if let Some(cursor) = item.agent_cursor.as_mut()
                            && cursor.request_id == operation.request_id
                        {
                            cursor.status = crate::agent_cursor::AgentCursorStatus::Clicked;
                        }
                    }
                    item.agent_operation_running = false;
                    cx.notify();
                });
                this.start_next_agent_operation(cx);
                item_entity.update(cx, |item, _| {
                    let click_ok = click_result.is_ok();
                    let click_reason = click_result.as_ref().err().map(ToString::to_string);
                    let type_expected = operation.text.clone();
                    let type_value = type_outcome
                        .as_ref()
                        .and_then(|outcome| outcome.value.clone());
                    let type_reason = type_outcome
                        .as_ref()
                        .and_then(|outcome| outcome.reason.clone());
                    let type_ok = match (&type_expected, &type_outcome) {
                        (Some(_), Some(outcome)) => outcome.ok,
                        (Some(_), None) => false,
                        (None, _) => true,
                    };
                    let raw_page_messages = page_state
                        .as_ref()
                        .map(|state| state.messages.clone())
                        .unwrap_or_default();
                    let page_messages = blocking_page_messages(raw_page_messages);
                    let page_ok = page_messages.is_empty();
                    let type_failure_reason = if type_expected.is_some() && type_outcome.is_none() {
                        Some("Timed out waiting for text verification".to_string())
                    } else {
                        type_reason.clone()
                    };
                    let page_reason = if page_ok {
                        None
                    } else {
                        Some(format!(
                            "Page reported validation or error messages: {}",
                            page_messages.join(" | ")
                        ))
                    };
                    let reason = click_reason.or(type_failure_reason).or(page_reason);
                    let ok = click_ok && type_ok && page_ok;
                    serde_json::json!({
                        "ok": ok,
                        "requestId": operation.request_id,
                        "selector": operation.target.selector,
                        "tag": operation.target.tag,
                        "bounds": {
                            "x": operation.target.rect.x,
                            "y": operation.target.rect.y,
                            "width": operation.target.rect.w,
                            "height": operation.target.rect.h,
                        },
                        "typed": type_expected.as_ref().map(|expected| serde_json::json!({
                            "expected": expected,
                            "observed": type_value,
                            "ok": type_ok,
                            "reason": type_reason,
                        })),
                        "reason": reason,
                        "url": item.url(),
                        "title": item.title(),
                        "page": page_state.as_ref().map(|state| serde_json::json!({
                            "url": state.url.clone(),
                            "title": state.title.clone(),
                            "activeSelector": state.active_selector.clone(),
                            "activeValue": state.active_value.clone(),
                            "messages": page_messages.clone(),
                            "ok": page_ok,
                        })),
                        "snapshot": action_snapshot.as_ref().map(browser_agent_snapshot_json),
                        "snapshotReason": snapshot_reason,
                    })
                })
            })?;
            let _ = operation.response_tx.send(Ok(response));

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    #[cfg(target_os = "windows")]
    fn on_agent_clear_cursor(
        &mut self,
        _: &zed_actions::agent::BrowserClearAgentCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.item.update(cx, |item, cx| {
            item.agent_cursor = None;
            cx.notify();
        });
    }

    #[cfg(target_os = "windows")]
    fn post_agent_scroll(
        &mut self,
        request_id: String,
        delta_x: f64,
        delta_y: f64,
        steps: u32,
        x: Option<f64>,
        y: Option<f64>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let steps = steps.clamp(1, 60);
        let item_entity = self.item.clone();
        let plan = self.item.update(cx, |item, cx| {
            if item.session.is_none() {
                item.agent_cursor = Some(failed_agent_cursor(&request_id, "Browser is not ready"));
                cx.notify();
                return Err(anyhow!("Browser is not ready"));
            }
            let Some(bounds) = item.last_bounds else {
                item.agent_cursor = Some(failed_agent_cursor(
                    &request_id,
                    "Browser viewport is not ready",
                ));
                cx.notify();
                return Err(anyhow!("Browser viewport is not ready"));
            };

            let max_x = f32::from(bounds.size.width).max(1.) - 1.;
            let max_y = f32::from(bounds.size.height).max(1.) - 1.;
            let local_x = x.map(|x| x as f32).unwrap_or(max_x / 2.).clamp(0., max_x);
            let local_y = y.map(|y| y as f32).unwrap_or(max_y / 2.).clamp(0., max_y);
            let mut cursor = crate::agent_cursor::AgentCursorState::preview(
                request_id.clone(),
                crate::browser_protocol::BrowserResolvedElement {
                    selector: "browser-scroll-origin".to_string(),
                    tag: Some("viewport".to_string()),
                    text: Some("Scroll".to_string()),
                    role: None,
                    accessible_name: Some("Scroll".to_string()),
                    rect: crate::design::ElementRect {
                        x: local_x,
                        y: local_y,
                        w: 1.,
                        h: 1.,
                    },
                    source: None,
                    confidence: crate::browser_protocol::BrowserTargetConfidence::Strong,
                },
            );
            cursor.label = "Scroll".to_string();
            cursor.pointer_position = Some((local_x, local_y));
            item.agent_cursor = Some(cursor);
            cx.notify();

            Ok((local_x as i32, local_y as i32))
        });

        let (local_x, local_y) = match plan {
            Ok(plan) => plan,
            Err(err) => {
                return Task::ready(Ok(serde_json::json!({
                    "ok": false,
                    "requestId": request_id,
                    "reason": err.to_string(),
                })));
            }
        };

        cx.spawn(async move |this, cx| {
            let step_delta_x = delta_x / f64::from(steps);
            let step_delta_y = delta_y / f64::from(steps);
            let mut reason = None;

            for _ in 0..steps {
                if step_delta_y.abs() >= 1.0 {
                    let data = browser_scroll_wheel_data(step_delta_y);
                    if let Err(err) = this.update(cx, |_, cx| {
                        item_entity.update(cx, |item, _| {
                            let Some(session) = item.session.as_ref() else {
                                return Err(anyhow!("Browser is not ready"));
                            };
                            session.send_mouse_input(
                                COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
                                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                                data,
                                local_x,
                                local_y,
                            )
                        })
                    })? {
                        reason = Some(err.to_string());
                        break;
                    }
                }

                if step_delta_x.abs() >= 1.0 {
                    let data = browser_scroll_wheel_data(step_delta_x);
                    if let Err(err) = this.update(cx, |_, cx| {
                        item_entity.update(cx, |item, _| {
                            let Some(session) = item.session.as_ref() else {
                                return Err(anyhow!("Browser is not ready"));
                            };
                            session.send_mouse_input(
                                COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
                                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                                data,
                                local_x,
                                local_y,
                            )
                        })
                    })? {
                        reason = Some(err.to_string());
                        break;
                    }
                }

                cx.background_executor()
                    .timer(Duration::from_millis(AGENT_SCROLL_STEP_MS))
                    .await;
            }

            let ok = reason.is_none();
            this.update(cx, |this, cx| {
                item_entity.update(cx, |item, _| {
                    push_agent_trace(
                        item,
                        crate::browser_protocol::BrowserAgentTraceEntry {
                            sequence: 0,
                            tool: "browser.scroll".to_string(),
                            request_id: Some(request_id.clone()),
                            snapshot_id: None,
                            element_ref: None,
                            selector: Some("browser-scroll-origin".to_string()),
                            ok,
                            reason: reason.clone(),
                        },
                    );
                });

                Ok(serde_json::json!({
                    "ok": ok,
                    "requestId": request_id,
                    "deltaX": delta_x,
                    "deltaY": delta_y,
                    "steps": steps,
                    "x": local_x,
                    "y": local_y,
                    "reason": reason,
                    "page": this.browser_agent_current_page(cx),
                }))
            })?
        })
    }

    #[cfg(target_os = "windows")]
    fn post_scroll_to_snapshot_ref(
        &self,
        request_id: String,
        snapshot_id: String,
        element_ref: String,
        align: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let align = normalize_scroll_align(&align);
        let item_entity = self.item.clone();
        let target = {
            let mut failure = None;
            let target = self.item.update(cx, |item, _| {
                let Some(snapshot) = item.latest_snapshot.as_ref() else {
                    failure = Some("No browser snapshot is available".to_string());
                    return None;
                };
                match find_snapshot_ref_for_scroll(snapshot, &snapshot_id, &element_ref) {
                    Ok(target) => Some(target),
                    Err(err) => {
                        failure = Some(err.to_string());
                        None
                    }
                }
            });
            if let Some(target) = target {
                target
            } else {
                let reason = failure.unwrap_or_else(|| "No element ref in snapshot".to_string());
                return Task::ready(Ok(serde_json::json!({
                    "ok": false,
                    "requestId": request_id,
                    "snapshotId": snapshot_id,
                    "ref": element_ref,
                    "reason": reason,
                })));
            }
        };

        let script = scroll_into_view_script(&target.selector, &align);
        let receiver = self.item.update(cx, |item, cx| {
            let mut cursor =
                crate::agent_cursor::AgentCursorState::preview(request_id.clone(), target.clone());
            cursor.label = format!("Scroll to {}", cursor.label);
            item.agent_cursor = Some(cursor);
            cx.notify();
            item.session
                .as_ref()
                .ok_or_else(|| anyhow!("Browser session is not ready"))
                .and_then(|session| session.execute_script(&script))
        });

        let receiver = match receiver {
            Ok(receiver) => receiver,
            Err(err) => {
                return Task::ready(Ok(serde_json::json!({
                    "ok": false,
                    "requestId": request_id,
                    "snapshotId": snapshot_id,
                    "ref": element_ref,
                    "selector": target.selector,
                    "reason": err.to_string(),
                })));
            }
        };

        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("Browser scroll_to response was dropped")));
            let (ok, reason) = match result {
                Ok(raw) => match parse_scroll_into_view_result(&raw) {
                    Ok(value) => (
                        value.get("ok").and_then(|ok| ok.as_bool()).unwrap_or(false),
                        value
                            .get("reason")
                            .and_then(|reason| reason.as_str())
                            .map(ToOwned::to_owned),
                    ),
                    Err(err) => (false, Some(err.to_string())),
                },
                Err(err) => (false, Some(err.to_string())),
            };

            this.update(cx, |this, cx| {
                item_entity.update(cx, |item, cx| {
                    push_agent_trace(
                        item,
                        crate::browser_protocol::BrowserAgentTraceEntry {
                            sequence: 0,
                            tool: "browser.scroll_to".to_string(),
                            request_id: Some(request_id.clone()),
                            snapshot_id: Some(snapshot_id.clone()),
                            element_ref: Some(element_ref.clone()),
                            selector: Some(target.selector.clone()),
                            ok,
                            reason: reason.clone(),
                        },
                    );
                    if let Some(cursor) = item.agent_cursor.as_mut()
                        && cursor.request_id == request_id
                    {
                        cursor.status = if ok {
                            crate::agent_cursor::AgentCursorStatus::Clicked
                        } else {
                            crate::agent_cursor::AgentCursorStatus::Failed(
                                reason
                                    .clone()
                                    .unwrap_or_else(|| "Scroll target failed".to_string()),
                            )
                        };
                    }
                    cx.notify();
                });

                Ok(serde_json::json!({
                    "ok": ok,
                    "requestId": request_id,
                    "snapshotId": snapshot_id,
                    "ref": element_ref,
                    "selector": target.selector,
                    "text": target.text,
                    "align": align,
                    "bounds": {
                        "x": target.rect.x,
                        "y": target.rect.y,
                        "width": target.rect.w,
                        "height": target.rect.h,
                    },
                    "reason": reason,
                    "page": this.browser_agent_current_page(cx),
                }))
            })?
        })
    }

    #[cfg(target_os = "windows")]
    fn post_find_element(
        &self,
        request_id: String,
        query: crate::browser_protocol::BrowserElementQuery,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let request_id_for_payload = request_id.clone();
        let Ok(payload) = serde_json::to_string(&serde_json::json!({
            "kind": "find_element",
            "requestId": request_id_for_payload,
            "query": query,
        })) else {
            return Task::ready(Err(anyhow!("Failed to encode browser find request")));
        };
        let item_entity = self.item.clone();
        let request_id_for_update = request_id.clone();
        self.item.update(cx, |item, cx| {
            item.agent_find_results.remove(&request_id_for_update);
            if let Some(session) = item.session.as_ref() {
                if let Err(err) = session.post_automation_message_string(&payload) {
                    item.agent_cursor = Some(failed_agent_cursor("agent", err.to_string()));
                    item.agent_find_results.insert(
                        request_id_for_update.clone(),
                        serde_json::json!({
                            "ok": false,
                            "requestId": request_id_for_update.clone(),
                            "reason": err.to_string(),
                        }),
                    );
                }
            } else {
                item.agent_cursor = Some(failed_agent_cursor("agent", "Browser is not ready"));
                item.agent_find_results.insert(
                    request_id_for_update.clone(),
                    serde_json::json!({
                        "ok": false,
                        "requestId": request_id_for_update.clone(),
                        "reason": "Browser is not ready",
                    }),
                );
            }
            cx.notify();
        });

        cx.spawn(async move |_, cx| {
            for _ in 0..AGENT_FIND_RESULT_MAX_POLLS {
                cx.background_executor()
                    .timer(Duration::from_millis(AGENT_FIND_RESULT_POLL_MS))
                    .await;
                let result =
                    item_entity.update(cx, |item, _| item.agent_find_results.remove(&request_id));
                if let Some(result) = result {
                    return Ok(result);
                }
            }
            Ok(serde_json::json!({
                "ok": false,
                "requestId": request_id,
                "reason": "Timed out waiting for browser target resolution",
            }))
        })
    }

    #[cfg(target_os = "windows")]
    fn post_visible_elements_snapshot(
        &self,
        request_id: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let item_entity = self.item.clone();
        let snapshot_id = format!("snapshot-{request_id}");
        let script = direct_snapshot_script(&snapshot_id);
        let receiver = self.item.update(cx, |item, _| {
            item.agent_snapshots.remove(&request_id);
            item.session
                .as_ref()
                .ok_or_else(|| anyhow!("Browser session is not ready"))
                .and_then(|session| session.execute_script(&script))
        });

        let receiver = match receiver {
            Ok(receiver) => receiver,
            Err(err) => {
                let drawing_overlay = browser_drawing_overlay_json(&self.item.read(cx));
                return Task::ready(Ok(snapshot_tool_response(
                    &request_id,
                    crate::browser_protocol::BrowserAgentSnapshot {
                        snapshot_id: format!("{request_id}-failed"),
                        page_revision: 0,
                        url: None,
                        title: None,
                        root: Vec::new(),
                        reason: Some(err.to_string()),
                    },
                    drawing_overlay,
                )));
            }
        };

        cx.spawn(async move |_, cx| {
            let timeout = cx.background_executor().timer(Duration::from_millis(
                AGENT_SNAPSHOT_RESULT_POLL_MS * AGENT_SNAPSHOT_RESULT_MAX_POLLS as u64,
            ));
            futures::pin_mut!(receiver);
            futures::pin_mut!(timeout);
            let snapshot = match futures::future::select(receiver, timeout).await {
                futures::future::Either::Left((result, _)) => match result {
                    Ok(Ok(result)) => match parse_executed_snapshot_result(&result) {
                        Ok(snapshot) => snapshot,
                        Err(err) => crate::browser_protocol::BrowserAgentSnapshot {
                            snapshot_id: format!("{request_id}-failed"),
                            page_revision: 0,
                            url: None,
                            title: None,
                            root: Vec::new(),
                            reason: Some(err.to_string()),
                        },
                    },
                    Ok(Err(err)) => crate::browser_protocol::BrowserAgentSnapshot {
                        snapshot_id: format!("{request_id}-failed"),
                        page_revision: 0,
                        url: None,
                        title: None,
                        root: Vec::new(),
                        reason: Some(err.to_string()),
                    },
                    Err(_) => crate::browser_protocol::BrowserAgentSnapshot {
                        snapshot_id: format!("{request_id}-failed"),
                        page_revision: 0,
                        url: None,
                        title: None,
                        root: Vec::new(),
                        reason: Some("Browser snapshot execution response was dropped".to_string()),
                    },
                },
                futures::future::Either::Right(((), _)) => {
                    crate::browser_protocol::BrowserAgentSnapshot {
                        snapshot_id: format!("{request_id}-failed"),
                        page_revision: 0,
                        url: None,
                        title: None,
                        root: Vec::new(),
                        reason: Some(
                            "Timed out waiting for browser snapshot execution".to_string(),
                        ),
                    }
                }
            };
            let drawing_overlay = item_entity.update(cx, |item, _| {
                if snapshot.reason.is_none() {
                    item.latest_snapshot = Some(snapshot.clone());
                }
                push_agent_trace(
                    item,
                    crate::browser_protocol::BrowserAgentTraceEntry {
                        sequence: 0,
                        tool: "browser.snapshot".to_string(),
                        request_id: Some(request_id.clone()),
                        snapshot_id: Some(snapshot.snapshot_id.clone()),
                        element_ref: None,
                        selector: None,
                        ok: snapshot.reason.is_none(),
                        reason: snapshot.reason.clone(),
                    },
                );
                browser_drawing_overlay_json(item)
            });
            Ok(snapshot_tool_response(&request_id, snapshot, drawing_overlay))
        })
    }

    #[cfg(target_os = "windows")]
    fn post_browser_screenshot(
        &self,
        request_id: String,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<serde_json::Value>> {
        let path = browser_screenshot_path(&request_id);
        let (capture_tx, capture_rx) = oneshot::channel();
        let mut capture_tx = Some(capture_tx);
        let dispatch_result = self.item.update(cx, |item, _| {
            item.session
                .as_ref()
                .ok_or_else(|| anyhow!("Browser session is not ready"))
                .and_then(|session| {
                    session.capture_preview_png(Box::new(move |result| {
                        if let Some(tx) = capture_tx.take() {
                            let _ = tx.send(result);
                        }
                    }))
                })
        });

        if let Err(err) = dispatch_result {
            return Task::ready(Ok(serde_json::json!({
                "ok": false,
                "requestId": request_id,
                "reason": err.to_string(),
            })));
        }

        let item_entity = self.item.clone();
        cx.spawn(async move |this, cx| {
            let result = capture_rx.await.unwrap_or_else(|_| {
                Err(anyhow!("Browser screenshot capture response was dropped"))
            });
            let response = match result {
                Ok(bytes) => match std::fs::write(&path, &bytes) {
                    Ok(()) => this.update(cx, |this, cx| {
                        item_entity.update(cx, |item, _| {
                            push_agent_trace(
                                item,
                                crate::browser_protocol::BrowserAgentTraceEntry {
                                    sequence: 0,
                                    tool: "browser.screenshot".to_string(),
                                    request_id: Some(request_id.clone()),
                                    snapshot_id: None,
                                    element_ref: None,
                                    selector: None,
                                    ok: true,
                                    reason: None,
                                },
                            );
                        });
                        Ok::<serde_json::Value, anyhow::Error>(serde_json::json!({
                            "ok": true,
                            "requestId": request_id,
                            "path": path.to_string_lossy(),
                            "bytes": bytes.len(),
                            "page": this.browser_agent_current_page(cx),
                        }))
                    })??,
                    Err(err) => serde_json::json!({
                        "ok": false,
                        "requestId": request_id,
                        "path": path.to_string_lossy(),
                        "reason": format!("Failed to write browser screenshot: {err}"),
                    }),
                },
                Err(err) => serde_json::json!({
                    "ok": false,
                    "requestId": request_id,
                    "reason": err.to_string(),
                }),
            };
            Ok(response)
        })
    }

    /// Phase 3: forward GPUI key events that GPUI didn't bind to actions
    /// into WebView2 via CDP `Input.dispatchKeyEvent`. On focus only
    /// happens when the user has actually clicked the page (mouse_down
    /// focuses BrowserView), so editor / address-bar typing is
    /// unaffected — that focus path routes through the editor's own
    /// key handlers first.
    #[cfg(target_os = "windows")]
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_key("keyDown", &event.keystroke, cx);
    }

    #[cfg(target_os = "windows")]
    fn on_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // On key-up suppress `text` so the renderer doesn't double-fire
        // `input` — only the keyDown carries the text payload.
        let mut ks = event.keystroke.clone();
        ks.key_char = None;
        self.dispatch_key("keyUp", &ks, cx);
    }

    #[cfg(target_os = "windows")]
    fn dispatch_key(&self, event_type: &str, ks: &Keystroke, cx: &mut Context<Self>) {
        let Some((key, code, vk, text)) = keystroke_to_cdp(ks) else {
            return;
        };
        let modifiers = cdp_modifiers_mask(&ks.modifiers);
        let text_for_up = if event_type == "keyUp" { None } else { text };
        self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                if let Err(err) = session.dispatch_key_event(
                    event_type,
                    &key,
                    &code,
                    modifiers,
                    vk,
                    text_for_up.as_deref(),
                ) {
                    log::debug!("dispatch_key_event({event_type}, {key}) failed: {err:?}");
                }
            }
        });
    }

    fn on_submit_url(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent_target_editor.focus_handle(cx).is_focused(window) {
            #[cfg(target_os = "windows")]
            self.preview_agent_target_from_editor(cx);
            return;
        }

        // When the URL editor isn't focused, Enter came from the page
        // viewport (we focus BrowserView on click). GPUI counts the
        // action as consumed once dispatched here, so `on_key_down`
        // won't fire — forward Enter to the page via CDP ourselves so
        // form submits / search submits / chat-send work.
        if !self.url_editor.focus_handle(cx).is_focused(window) {
            #[cfg(target_os = "windows")]
            {
                let ks = Keystroke {
                    modifiers: Modifiers::default(),
                    key: "enter".to_string(),
                    key_char: None,
                };
                self.dispatch_key("keyDown", &ks, cx);
                self.dispatch_key("keyUp", &ks, cx);
            }
            return;
        }
        use settings::Settings as _;
        let input = self.url_editor.read(cx).text(cx);
        let search_url = crate::BrowserSettings::get_global(cx).search_url.clone();
        let target = parse_address_bar_input(&input, &search_url);
        #[cfg(target_os = "windows")]
        self.navigate_to(target, cx);
        #[cfg(not(target_os = "windows"))]
        let _ = target;
    }

    /// Phase 4.D: capture mouse drag into `BrowserItem.drawing`.
    /// Coordinates land in window-space (same frame the WebView2 visual
    /// uses); the paint element draws them directly without translation.
    fn attach_drawing_handlers(&self, root: Div, cx: &mut Context<Self>) -> Div {
        let stroke_color = gpui::hsla(0.36, 1.0, 0.5, 1.0);
        let stroke_width = px(3.0);
        root.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                this.item.update(cx, |item, cx| {
                    item.drawing.begin(ev.position, stroke_color, stroke_width);
                    cx.notify();
                });
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            if ev.pressed_button != Some(MouseButton::Left) {
                return;
            }
            this.item.update(cx, |item, cx| {
                if item.drawing.current.is_some() {
                    item.drawing.extend(ev.position);
                    cx.notify();
                }
            });
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _: &MouseUpEvent, _, cx| {
                this.item.update(cx, |item, cx| {
                    item.drawing.finish();
                    cx.notify();
                });
            }),
        )
    }

    #[cfg(target_os = "windows")]
    fn attach_mouse_handlers(&self, root: Div, cx: &mut Context<Self>) -> Div {
        root.on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                // Phase 3: focus the BrowserView so page key events route to
                // our `on_key_down` handler (which forwards to WebView2 via
                // CDP `Input.dispatchKeyEvent`). Ctrl+Shift+P / Ctrl+P still
                // work because (a) Workspace-context bindings match anywhere
                // in the context chain including under BrowserView, and (b)
                // the post-forward `SetFocus(zed_hwnd)` in `forward_mouse_event`
                // keeps Win32 focus on Zed so `translate_accelerator` still
                // delivers WM_KEYDOWN to gpui_windows.
                window.focus(&this.focus_handle, cx);
                let kind = if ev.click_count >= 2 {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK
                } else {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN
                };
                let vk = virtual_keys(&ev.modifiers, Some(MouseButton::Left));
                forward_mouse_event(this, cx, window, ev.position, kind, vk, 0);
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
                    vk,
                    0,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                let kind = if ev.click_count >= 2 {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOUBLE_CLICK
                } else {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN
                };
                let vk = virtual_keys(&ev.modifiers, Some(MouseButton::Middle));
                forward_mouse_event(this, cx, window, ev.position, kind, vk, 0);
            }),
        )
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
                    vk,
                    0,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                let kind = if ev.click_count >= 2 {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOUBLE_CLICK
                } else {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN
                };
                let vk = virtual_keys(&ev.modifiers, Some(MouseButton::Right));
                forward_mouse_event(this, cx, window, ev.position, kind, vk, 0);
            }),
        )
        .on_mouse_up(
            MouseButton::Right,
            cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP,
                    vk,
                    0,
                );
            }),
        )
        // X1/X2 "navigate back/forward" mouse buttons. Forward to WebView2
        // as X_BUTTON events so the page can handle them, then call
        // `cx.stop_propagation()` to prevent the enclosing Pane
        // (`workspace::pane`) from also handling them via
        // `workspace.go_back()` / `go_forward()`. Without the propagation
        // stop the Pane's handlers fire too, switching workspace tabs and
        // stealing focus from the BrowserView — visible to the user as the
        // browser becoming unresponsive.
        .on_mouse_down(
            MouseButton::Navigate(NavigationDirection::Back),
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, Some(ev.button));
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN,
                    vk,
                    1,
                );
                cx.stop_propagation();
            }),
        )
        .on_mouse_up(
            MouseButton::Navigate(NavigationDirection::Back),
            cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
                    vk,
                    1,
                );
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(
            MouseButton::Navigate(NavigationDirection::Forward),
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, Some(ev.button));
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN,
                    vk,
                    2,
                );
                cx.stop_propagation();
            }),
        )
        .on_mouse_up(
            MouseButton::Navigate(NavigationDirection::Forward),
            cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
                    vk,
                    2,
                );
                cx.stop_propagation();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
            let vk = virtual_keys(&ev.modifiers, ev.pressed_button);
            forward_mouse_event(
                this,
                cx,
                window,
                ev.position,
                COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                vk,
                0,
            );
        }))
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, cx| {
            let (delta_x, delta_y) = match ev.delta {
                ScrollDelta::Lines(p) => (p.x * WHEEL_DELTA, p.y * WHEEL_DELTA),
                ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            };
            let vk = virtual_keys(&ev.modifiers, None);
            // WebView2 wheel mouse_data convention matches Win32
            // WM_MOUSEWHEEL: positive = wheel rotated forward / scroll page
            // content UP. GPUI ScrollDelta also follows the convention
            // "positive y = scroll up", so pass through directly without
            // negating.
            if delta_y.abs() >= 1.0 {
                let data = (delta_y as i32) as u32;
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL,
                    vk,
                    data,
                );
            }
            if delta_x.abs() >= 1.0 {
                let data = (delta_x as i32) as u32;
                forward_mouse_event(
                    this,
                    cx,
                    window,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL,
                    vk,
                    data,
                );
            }
        }))
    }
}

#[cfg(target_os = "windows")]
fn virtual_keys(
    modifiers: &Modifiers,
    pressed_button: Option<MouseButton>,
) -> COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS {
    let mut bits: i32 = 0;
    if modifiers.shift {
        bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_SHIFT.0;
    }
    if modifiers.control {
        bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_CONTROL.0;
    }
    match pressed_button {
        Some(MouseButton::Left) => bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_LEFT_BUTTON.0,
        Some(MouseButton::Middle) => bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_MIDDLE_BUTTON.0,
        Some(MouseButton::Right) => bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_RIGHT_BUTTON.0,
        Some(MouseButton::Navigate(NavigationDirection::Back)) => {
            bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON1.0
        }
        Some(MouseButton::Navigate(NavigationDirection::Forward)) => {
            bits |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON2.0
        }
        _ => {}
    }
    COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(bits)
}

#[cfg(target_os = "windows")]
fn forward_mouse_event(
    view: &mut BrowserView,
    cx: &mut Context<BrowserView>,
    window: &mut Window,
    position: Point<Pixels>,
    kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
    vk: COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
    mouse_data: u32,
) {
    view.item.update(cx, |item, _| {
        // Don't forward to the page while the design panel is being
        // dragged — avoids page hover flicker under the moving panel.
        if item.design_drag.is_some() {
            return;
        }
        let Some(session) = item.session.as_ref() else {
            return;
        };
        let Some(bounds) = item.last_bounds else {
            return;
        };
        let local_x = (f32::from(position.x) - f32::from(bounds.origin.x)) as i32;
        let local_y = (f32::from(position.y) - f32::from(bounds.origin.y)) as i32;
        if let Err(err) = session.send_mouse_input(kind, vk, mouse_data, local_x, local_y) {
            log::debug!("send_mouse_input failed: {err}");
        }
    });

    // WebView2, even in composition mode, creates internal child HWNDs
    // and shifts Win32 keyboard focus to one of them on mouse-down. From
    // then on, `GetMessageW` delivers WM_KEYDOWN to the WebView2 HWND
    // (which knows nothing about gpui_windows' WM_GPUI_KEYDOWN
    // accelerator translation), so global shortcuts like Ctrl+Shift+P
    // and Ctrl+P stop reaching Zed. Re-assert Win32 focus on the host
    // HWND so the message pump keeps routing keystrokes to GPUI.
    // Side-effect-neutral until Phase 3: the page wouldn't receive
    // keystrokes either way, since we don't forward via
    // `SendKeyboardInput`.
    if let Some(hwnd) = hwnd_from_window(window) {
        unsafe {
            let _ = SetFocus(Some(hwnd));
        }
    }
}

#[cfg(target_os = "windows")]
fn browser_scroll_wheel_data(logical_delta: f64) -> u32 {
    let wheel_delta = if logical_delta > 0.0 {
        -logical_delta.round().max(1.0)
    } else if logical_delta < 0.0 {
        (-logical_delta).round().max(1.0)
    } else {
        0.0
    };

    (wheel_delta as i32) as u32
}

#[cfg(target_os = "windows")]
fn click_agent_cursor_target(
    view: &mut BrowserView,
    cx: &mut Context<BrowserView>,
    window: &mut Window,
) {
    let _ = click_agent_cursor_target_inner(view, cx);

    if let Some(hwnd) = hwnd_from_window(window) {
        unsafe {
            let _ = SetFocus(Some(hwnd));
        }
    }
}

#[cfg(target_os = "windows")]
fn click_agent_cursor_target_inner(
    view: &mut BrowserView,
    cx: &mut Context<BrowserView>,
) -> Result<(), String> {
    let mut click_plan = None;
    let item = view.item.clone();
    view.item.update(cx, |item, cx| {
        let Some(cursor) = item.agent_cursor.as_mut() else {
            click_plan = Some(Err("No browser target preview".to_string()));
            return;
        };
        if item.session.is_none() {
            cursor.status = crate::agent_cursor::AgentCursorStatus::Failed(
                "Browser session is not ready".into(),
            );
            cx.notify();
            click_plan = Some(Err("Browser session is not ready".to_string()));
            return;
        };
        let Some(bounds) = item.last_bounds else {
            cursor.status = crate::agent_cursor::AgentCursorStatus::Failed(
                "Browser viewport is not ready".into(),
            );
            cx.notify();
            click_plan = Some(Err("Browser viewport is not ready".to_string()));
            return;
        };

        cursor.status = crate::agent_cursor::AgentCursorStatus::Clicking;
        let rect = &cursor.target.rect;
        let max_x = f32::from(bounds.size.width).max(1.) - 1.;
        let max_y = f32::from(bounds.size.height).max(1.) - 1.;
        let target_x = (rect.x + rect.w / 2.).clamp(0., max_x);
        let target_y = (rect.y + rect.h / 2.).clamp(0., max_y);
        let (start_x, start_y) = cursor.pointer_position.unwrap_or((24., 24.));
        cursor.pointer_position = Some((start_x, start_y));
        click_plan = Some(Ok((start_x, start_y, target_x, target_y)));
        cx.notify();
    });

    let (start_x, start_y, target_x, target_y) =
        click_plan.unwrap_or_else(|| Err("No browser target preview".to_string()))?;

    cx.spawn(async move |this, cx| {
        for step in 1..=AGENT_CURSOR_STEPS {
            let t = step as f32 / AGENT_CURSOR_STEPS as f32;
            let eased = 1. - (1. - t) * (1. - t);
            let x = start_x + (target_x - start_x) * eased;
            let y = start_y + (target_y - start_y) * eased;
            this.update(cx, |_, cx| {
                item.update(cx, |item, cx| {
                    let Some(cursor) = item.agent_cursor.as_mut() else {
                        return;
                    };
                    cursor.pointer_position = Some((x, y));
                    if let Some(session) = item.session.as_ref() {
                        let _ = session.dispatch_mouse_event("mouseMoved", x, y, None);
                    }
                    cx.notify();
                });
            })?;
            cx.background_executor()
                .timer(Duration::from_millis(AGENT_CURSOR_STEP_MS))
                .await;
        }

        this.update(cx, |_, cx| {
            item.update(cx, |item, cx| {
                let Some(cursor) = item.agent_cursor.as_mut() else {
                    return;
                };
                let Some(session) = item.session.as_ref() else {
                    cursor.status = crate::agent_cursor::AgentCursorStatus::Failed(
                        "Browser session is not ready".into(),
                    );
                    cx.notify();
                    return;
                };
                let result = session
                    .dispatch_mouse_event("mousePressed", target_x, target_y, Some("left"))
                    .and_then(|_| {
                        session.dispatch_mouse_event(
                            "mouseReleased",
                            target_x,
                            target_y,
                            Some("left"),
                        )
                    });
                match result {
                    Ok(()) => {
                        cursor.status = crate::agent_cursor::AgentCursorStatus::Clicked;
                    }
                    Err(err) => {
                        cursor.status =
                            crate::agent_cursor::AgentCursorStatus::Failed(err.to_string());
                    }
                }
                cx.notify();
            });
        })?;
        anyhow::Ok(())
    })
    .detach();

    Ok(())
}

#[cfg(target_os = "windows")]
fn browser_query_from_parts(
    query_kind: &str,
    query: &str,
) -> Result<crate::browser_protocol::BrowserElementQuery, String> {
    let kind = query_kind.trim().to_ascii_lowercase();
    let query = query.trim().to_string();
    match kind.as_str() {
        "selected" => Ok(crate::browser_protocol::BrowserElementQuery::Selected),
        "selector" => {
            if query.is_empty() {
                Err("Selector query cannot be empty".to_string())
            } else {
                Ok(crate::browser_protocol::BrowserElementQuery::Selector { selector: query })
            }
        }
        "text_exact" => {
            if query.is_empty() {
                Err("Text query cannot be empty".to_string())
            } else {
                Ok(crate::browser_protocol::BrowserElementQuery::TextExact { text: query })
            }
        }
        "text_contains" | "text" => {
            if query.is_empty() {
                Err("Text query cannot be empty".to_string())
            } else {
                Ok(crate::browser_protocol::BrowserElementQuery::TextContains { text: query })
            }
        }
        "role_and_name" => {
            let Some((role, name)) = query.split_once('|') else {
                return Err("Role query must use role|name".to_string());
            };
            Ok(crate::browser_protocol::BrowserElementQuery::RoleAndName {
                role: role.trim().to_string(),
                name: name.trim().to_string(),
            })
        }
        "point" => {
            let parts = query
                .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            if parts.len() != 2 {
                return Err("Point query must use x,y".to_string());
            }
            let x = parts[0]
                .parse::<f32>()
                .map_err(|_| "Point x must be a number".to_string())?;
            let y = parts[1]
                .parse::<f32>()
                .map_err(|_| "Point y must be a number".to_string())?;
            Ok(crate::browser_protocol::BrowserElementQuery::Point { x, y })
        }
        other => Err(format!("Unsupported browser query kind: {other}")),
    }
}

#[cfg(target_os = "windows")]
fn find_snapshot_ref(
    snapshot: &crate::browser_protocol::BrowserAgentSnapshot,
    snapshot_id: &str,
    element_ref: &str,
) -> anyhow::Result<crate::browser_protocol::BrowserResolvedElement> {
    if snapshot.snapshot_id != snapshot_id {
        anyhow::bail!("Snapshot ref is stale");
    }

    fn visit<'a>(
        nodes: &'a [crate::browser_protocol::BrowserSnapshotNode],
        element_ref: &str,
    ) -> Option<&'a crate::browser_protocol::BrowserSnapshotNode> {
        for node in nodes {
            if node.node_ref.as_deref() == Some(element_ref) {
                return Some(node);
            }
            if let Some(found) = visit(&node.children, element_ref) {
                return Some(found);
            }
        }
        None
    }

    let Some(node) = visit(&snapshot.root, element_ref) else {
        anyhow::bail!("No element ref in snapshot");
    };
    let Some(selector) = node.selector.clone() else {
        anyhow::bail!("Snapshot ref is not actionable");
    };
    let Some(rect) = node.rect.clone() else {
        anyhow::bail!("Snapshot ref has no bounds");
    };

    Ok(crate::browser_protocol::BrowserResolvedElement {
        selector,
        tag: None,
        text: node.text.clone(),
        role: node.role.clone(),
        accessible_name: node.name.clone(),
        rect,
        source: None,
        confidence: crate::browser_protocol::BrowserTargetConfidence::Exact,
    })
}

#[cfg(target_os = "windows")]
fn find_snapshot_ref_for_scroll(
    snapshot: &crate::browser_protocol::BrowserAgentSnapshot,
    snapshot_id: &str,
    element_ref: &str,
) -> anyhow::Result<crate::browser_protocol::BrowserResolvedElement> {
    if snapshot.snapshot_id != snapshot_id {
        anyhow::bail!("Snapshot ref is stale");
    }

    let Some(node) = find_snapshot_node(&snapshot.root, element_ref) else {
        anyhow::bail!("No element ref in snapshot");
    };
    let Some(selector) = node.selector.clone() else {
        anyhow::bail!("Snapshot ref has no selector");
    };
    let Some(rect) = node.rect.clone() else {
        anyhow::bail!("Snapshot ref has no bounds");
    };

    Ok(crate::browser_protocol::BrowserResolvedElement {
        selector,
        tag: None,
        text: node.text.clone(),
        role: node.role.clone(),
        accessible_name: node.name.clone(),
        rect,
        source: None,
        confidence: crate::browser_protocol::BrowserTargetConfidence::Exact,
    })
}

#[cfg(target_os = "windows")]
fn find_snapshot_node<'a>(
    nodes: &'a [crate::browser_protocol::BrowserSnapshotNode],
    element_ref: &str,
) -> Option<&'a crate::browser_protocol::BrowserSnapshotNode> {
    for node in nodes {
        if node.node_ref.as_deref() == Some(element_ref) {
            return Some(node);
        }
        if let Some(found) = find_snapshot_node(&node.children, element_ref) {
            return Some(found);
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn normalize_scroll_align(align: &str) -> String {
    match align {
        "start" | "center" | "end" | "nearest" => align.to_string(),
        _ => "center".to_string(),
    }
}

#[cfg(target_os = "windows")]
fn scroll_into_view_script(selector: &str, align: &str) -> String {
    let selector = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let align = serde_json::to_string(&normalize_scroll_align(align))
        .unwrap_or_else(|_| "\"center\"".to_string());
    format!(
        r#"
(() => {{
    const selector = {selector};
    const align = {align};
    const el = document.querySelector(selector);
    if (!el) {{
        return {{ ok: false, reason: 'Scroll target selector no longer matched', selector }};
    }}
    el.scrollIntoView({{ block: align, inline: 'nearest', behavior: 'instant' }});
    const rect = el.getBoundingClientRect();
    return {{
        ok: true,
        selector,
        rect: {{ x: rect.left, y: rect.top, w: rect.width, h: rect.height }},
        scrollX: window.scrollX,
        scrollY: window.scrollY,
    }};
}})()
"#
    )
}

#[cfg(target_os = "windows")]
fn parse_scroll_into_view_result(result: &str) -> anyhow::Result<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(result) {
        Ok(value) if value.is_object() => Ok(value),
        Ok(serde_json::Value::String(encoded)) => serde_json::from_str(&encoded)
            .map_err(|err| anyhow!("Failed to decode browser scroll_to result: {err}")),
        Ok(_) => anyhow::bail!("Browser scroll_to returned non-object JSON"),
        Err(err) => anyhow::bail!("Failed to parse browser scroll_to result: {err}"),
    }
}

#[cfg(target_os = "windows")]
fn actionability_failure_response(
    request_id: &str,
    outcome: &crate::browser_protocol::BrowserActionabilityOutcome,
) -> serde_json::Value {
    serde_json::json!({
        "ok": false,
        "requestId": request_id,
        "selector": outcome.selector,
        "checks": outcome.checks,
        "reason": outcome
            .reason
            .clone()
            .unwrap_or_else(|| "Element is not actionable".to_string()),
    })
}

#[cfg(target_os = "windows")]
fn browser_agent_snapshot_json(
    snapshot: &crate::browser_protocol::BrowserAgentSnapshot,
) -> serde_json::Value {
    serde_json::json!({
        "snapshotId": snapshot.snapshot_id.clone(),
        "pageRevision": snapshot.page_revision,
        "url": snapshot.url.clone(),
        "title": snapshot.title.clone(),
        "root": snapshot.root.clone(),
        "reason": snapshot.reason.clone(),
    })
}

#[cfg(target_os = "windows")]
fn browser_drawing_overlay_json(item: &BrowserItem) -> serde_json::Value {
    let stroke_count = item.drawing.strokes.len() + usize::from(item.drawing.current.is_some());
    let point_count: usize = item
        .drawing
        .strokes
        .iter()
        .map(|stroke| stroke.points.len())
        .sum::<usize>()
        + item
            .drawing
            .current
            .as_ref()
            .map_or(0, |stroke| stroke.points.len());
    let viewport = item.last_bounds.map(|bounds| {
        serde_json::json!({
            "x": f32::from(bounds.origin.x),
            "y": f32::from(bounds.origin.y),
            "width": f32::from(bounds.size.width),
            "height": f32::from(bounds.size.height),
        })
    });
    let drawing_svg = item
        .last_bounds
        .filter(|_| !item.drawing.is_empty())
        .map(|bounds| item.drawing.to_svg(bounds.origin, bounds.size));

    serde_json::json!({
        "hasDrawing": !item.drawing.is_empty(),
        "drawingModeEnabled": item.drawing_mode_enabled,
        "strokeCount": stroke_count,
        "pointCount": point_count,
        "viewport": viewport,
        "svg": drawing_svg,
        "note": if item.drawing.is_empty() {
            "No GPUI drawing overlay strokes are currently recorded"
        } else {
            "GPUI drawing overlay is not part of the page DOM; use this SVG or browser.screenshot for visual context"
        },
    })
}

#[cfg(target_os = "windows")]
fn snapshot_tool_response(
    request_id: &str,
    snapshot: crate::browser_protocol::BrowserAgentSnapshot,
    drawing_overlay: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "ok": snapshot.reason.is_none(),
        "requestId": request_id,
        "snapshotId": snapshot.snapshot_id,
        "pageRevision": snapshot.page_revision,
        "url": snapshot.url,
        "title": snapshot.title,
        "root": snapshot.root,
        "reason": snapshot.reason,
        "drawingOverlay": drawing_overlay,
    })
}

#[cfg(target_os = "windows")]
fn parse_executed_snapshot_result(
    result: &str,
) -> anyhow::Result<crate::browser_protocol::BrowserAgentSnapshot> {
    match serde_json::from_str::<crate::browser_protocol::BrowserAgentSnapshot>(result) {
        Ok(snapshot) => Ok(snapshot),
        Err(object_err) => {
            let encoded = serde_json::from_str::<String>(result)
                .map_err(|_| anyhow!("Failed to parse browser snapshot result: {object_err}"))?;
            serde_json::from_str(&encoded)
                .map_err(|string_err| anyhow!("Failed to decode browser snapshot: {string_err}"))
        }
    }
}

#[cfg(target_os = "windows")]
fn direct_snapshot_script(snapshot_id: &str) -> String {
    let snapshot_id = serde_json::to_string(snapshot_id).unwrap_or_else(|_| "\"snapshot\"".into());
    format!(
        r#"
(() => {{
    const snapshotId = {snapshot_id};
    function cssPath(el) {{
        if (!(el instanceof Element)) return '';
        const path = [];
        while (el && el.nodeType === Node.ELEMENT_NODE && path.length < 8) {{
            let sel = el.nodeName.toLowerCase();
            if (el.id) {{ sel += '#' + CSS.escape(el.id); path.unshift(sel); break; }}
            let sibs = Array.from(el.parentElement ? el.parentElement.children : []);
            const same = sibs.filter(sib => sib.nodeName === el.nodeName);
            if (same.length > 1) sel += `:nth-of-type(${{same.indexOf(el) + 1}})`;
            path.unshift(sel);
            el = el.parentElement;
        }}
        return path.join(' > ');
    }}
    function visible(el) {{
        if (!(el instanceof Element)) return false;
        const style = getComputedStyle(el);
        if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') return false;
        const rect = el.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0;
    }}
    function textOf(el) {{
        if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {{
            return el.value || el.getAttribute('placeholder') || '';
        }}
        return (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
    }}
    function nameOf(el) {{
        return (el.getAttribute('aria-label') || el.getAttribute('title') || textOf(el) || '').slice(0, 220);
    }}
    function roleOf(el) {{
        const explicit = el.getAttribute('role');
        if (explicit) return explicit;
        const tag = el.tagName.toUpperCase();
        const type = (el.getAttribute('type') || '').toLowerCase();
        if (tag === 'BUTTON') return 'button';
        if (tag === 'A' && el.hasAttribute('href')) return 'link';
        if (tag === 'INPUT' && ['button', 'submit', 'reset'].includes(type)) return 'button';
        if (tag === 'INPUT' || tag === 'TEXTAREA') return 'textbox';
        if (tag === 'SELECT') return 'combobox';
        if (/^H[1-6]$/.test(tag)) return 'heading';
        if (tag === 'FORM') return 'form';
        if (tag === 'MAIN') return 'main';
        if (tag === 'LABEL') return 'label';
        return null;
    }}
    function interactive(el) {{
        return el.matches('button, a[href], input, textarea, select, [role], [tabindex], summary, label');
    }}
    let refCounter = 1;
    function nodeFor(el, depth) {{
        if (!visible(el) || depth > 7) return null;
        const role = roleOf(el);
        const isInteractive = interactive(el);
        const children = [];
        for (const child of Array.from(el.children || [])) {{
            const childNode = nodeFor(child, depth + 1);
            if (childNode) children.push(childNode);
        }}
        const text = isInteractive ? null : textOf(el).slice(0, 220);
        if (!role && !isInteractive && !text && children.length === 0) return null;
        if (!role && !isInteractive && children.length === 1 && !text) return children[0];
        const rect = el.getBoundingClientRect();
        const hasSnapshotTarget = isInteractive || Boolean(text);
        const node = {{
            role: role || (text ? 'text' : null),
            name: isInteractive ? nameOf(el) : null,
            text,
            selector: hasSnapshotTarget ? cssPath(el) : null,
            rect: hasSnapshotTarget ? {{ x: rect.left, y: rect.top, w: rect.width, h: rect.height }} : null,
            children,
            disabled: Boolean(el.disabled || el.getAttribute('aria-disabled') === 'true'),
            editable: el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el.isContentEditable,
        }};
        if (hasSnapshotTarget) node.ref = `e${{refCounter++}}`;
        return node;
    }}
    const root = document.querySelector('main') || document.body || document.documentElement;
    const rootNode = root ? nodeFor(root, 0) : null;
    return {{
        snapshotId,
        pageRevision: Math.floor(Date.now() / 1000),
        url: location.href,
        title: document.title || '',
        root: rootNode ? [rootNode] : [],
    }};
}})()
"#
    )
}

#[cfg(target_os = "windows")]
fn push_agent_trace(
    item: &mut BrowserItem,
    mut entry: crate::browser_protocol::BrowserAgentTraceEntry,
) {
    item.agent_trace_sequence += 1;
    entry.sequence = item.agent_trace_sequence;
    item.agent_trace.push_back(entry);
    while item.agent_trace.len() > AGENT_TRACE_MAX_ENTRIES {
        item.agent_trace.pop_front();
    }
}

#[cfg(target_os = "windows")]
const AGENT_DIAGNOSTIC_MAX_ENTRIES: usize = 100;

#[cfg(target_os = "windows")]
fn next_agent_diagnostic_sequence(item: &mut BrowserItem) -> u64 {
    item.agent_diagnostic_sequence += 1;
    item.agent_diagnostic_sequence
}

#[cfg(target_os = "windows")]
fn push_console_event(
    item: &mut BrowserItem,
    mut event: crate::browser_protocol::BrowserConsoleEventSummary,
) {
    event.sequence = next_agent_diagnostic_sequence(item);
    item.agent_console_events.push_back(event);
    while item.agent_console_events.len() > AGENT_DIAGNOSTIC_MAX_ENTRIES {
        item.agent_console_events.pop_front();
    }
}

#[cfg(target_os = "windows")]
fn push_network_event(
    item: &mut BrowserItem,
    mut event: crate::browser_protocol::BrowserNetworkEventSummary,
) {
    event.sequence = next_agent_diagnostic_sequence(item);
    item.agent_network_events.push_back(event);
    while item.agent_network_events.len() > AGENT_DIAGNOSTIC_MAX_ENTRIES {
        item.agent_network_events.pop_front();
    }
}

#[cfg(target_os = "windows")]
fn browser_expect_script(kind: &str, value: Option<&str>, selector: Option<&str>) -> String {
    let kind_json = serde_json::to_string(kind).unwrap();
    let value_json = serde_json::to_string(&value).unwrap();
    let selector_json = serde_json::to_string(&selector).unwrap();
    format!(
        r#"(function() {{
            const kind = {kind_json};
            const value = {value_json};
            const selector = {selector_json};
            const norm = text => String(text || '').replace(/\s+/g, ' ').trim();
            const visible = el => {{
                if (!el) return false;
                const style = getComputedStyle(el);
                const rect = el.getBoundingClientRect();
                return style.visibility !== 'hidden' &&
                    style.display !== 'none' &&
                    Number(style.opacity || '1') > 0 &&
                    rect.width > 0 &&
                    rect.height > 0;
            }};
            if (kind === 'url_contains') {{
                const observed = location.href;
                const ok = value ? observed.includes(value) : false;
                return JSON.stringify({{ ok, kind, value, selector, observed,
                    reason: ok ? null : 'URL did not contain expected value' }});
            }}
            const root = selector ? document.querySelector(selector) : document.body;
            if (!root) {{
                return JSON.stringify({{ ok: false, kind, value, selector, observed: null,
                    reason: 'Selector did not match an element' }});
            }}
            if (kind === 'visible') {{
                const ok = visible(root);
                return JSON.stringify({{ ok, kind, value, selector,
                    observed: ok ? 'visible' : 'not visible',
                    reason: ok ? null : 'Element is not visible' }});
            }}
            if (kind === 'text_contains') {{
                const observed = norm(root.innerText || root.textContent || '');
                const ok = value ? observed.includes(value) : false;
                return JSON.stringify({{ ok, kind, value, selector,
                    observed: observed.slice(0, 500),
                    reason: ok ? null : 'Text did not contain expected value' }});
            }}
            return JSON.stringify({{ ok: false, kind, value, selector, observed: null,
                reason: 'Unsupported expectation kind' }});
        }})()"#
    )
}

#[cfg(target_os = "windows")]
fn blocking_page_messages(messages: Vec<String>) -> Vec<String> {
    let mut filtered = Vec::new();
    for message in messages {
        let message = message.trim();
        if message.is_empty() || !is_blocking_page_message(message) {
            continue;
        }

        let message = if message.len() > 300 {
            format!("{}...", &message[..300])
        } else {
            message.to_string()
        };

        if !filtered.contains(&message) {
            filtered.push(message);
        }

        if filtered.len() == 8 {
            break;
        }
    }
    filtered
}

#[cfg(target_os = "windows")]
fn browser_screenshot_path(request_id: &str) -> std::path::PathBuf {
    let mut slug = request_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "capture" } else { slug };
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("zed-browser-{slug}-{millis}.png"))
}

#[cfg(target_os = "windows")]
fn is_blocking_page_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();

    if lower == "0 items in cart 0" || lower.contains("price is") || lower.contains("list price") {
        return false;
    }

    [
        "error",
        "failed",
        "invalid",
        "required",
        "please include",
        "please fill",
        "try again",
        "incorrect",
        "wrong",
        "does not match",
        "missing",
        "unable to",
        "could not",
        "cannot",
        "can't",
        "expired",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(target_os = "windows")]
fn failed_agent_cursor(
    request_id: impl Into<String>,
    reason: impl Into<String>,
) -> crate::agent_cursor::AgentCursorState {
    let reason = reason.into();
    crate::agent_cursor::AgentCursorState {
        request_id: request_id.into(),
        target: crate::browser_protocol::BrowserResolvedElement {
            selector: "browser-command-failed".to_string(),
            tag: Some("missing".to_string()),
            text: Some(reason.clone()),
            role: None,
            accessible_name: Some(reason.clone()),
            rect: crate::design::ElementRect {
                x: 0.,
                y: 0.,
                w: 0.,
                h: 0.,
            },
            source: None,
            confidence: crate::browser_protocol::BrowserTargetConfidence::Weak,
        },
        status: crate::agent_cursor::AgentCursorStatus::Failed(reason.clone()),
        label: reason,
        ambiguity: Vec::new(),
        pointer_position: None,
    }
}

impl EventEmitter<BrowserViewEvent> for BrowserView {}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Multi-tab z-order: when several browser tabs share a pane, all
        // their WebView2 underlays stay attached (we don't hide inactive
        // ones — that caused a one-frame desktop flash on reactivation).
        // `Item::deactivated` marks this tab not-fronted (`is_visible =
        // false`); on (re)activation Render runs here and reorders this
        // tab's underlay to the front of the underlay group, so the active
        // tab's page — not a sibling's — shows through GPUI's transparent
        // regions. Gated on `is_visible` so we reorder only on the
        // activation transition, not every frame.
        #[cfg(target_os = "windows")]
        {
            Self::register_as_active_browser(cx);
            self.item.update(cx, |item, _| {
                if item.is_visible {
                    return;
                }
                match item.session.as_ref() {
                    Some(session) => match session.bring_underlay_to_front() {
                        Ok(()) => item.is_visible = true,
                        // Leave is_visible false so the next Render retries.
                        Err(err) => log::warn!(
                            "BrowserItem: reorder underlay to front on activate failed: {err:?}"
                        ),
                    },
                    // No session yet; mark fronted so the session-ready
                    // callback fronts it once init completes.
                    None => item.is_visible = true,
                }
            });
        }

        // FORK: focus the prompt editor on each new element selection so the
        // panel's Esc / Ctrl+Enter shortcuts fire and those keys don't leak
        // to the page (selecting an element otherwise leaves focus on the
        // browser root, which forwards keystrokes to the page over CDP).
        #[cfg(target_os = "windows")]
        {
            let current_sel = self
                .item
                .read(cx)
                .design_selection
                .as_ref()
                .map(|s| SharedString::from(s.selector.clone()));
            if current_sel != self.design_prompt_focused_for {
                if current_sel.is_some() {
                    let handle = self.design_prompt_editor.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                }
                self.design_prompt_focused_for = current_sel;
            }
        }

        // Sync the address bar editor text from the model when (a) the model
        // URL has changed and (b) the user isn't typing into the input.
        let model_url = self.item.read(cx).url.clone();
        let editor_focused = self.url_editor.focus_handle(cx).is_focused(window);
        if !editor_focused {
            let editor_text = self.url_editor.read(cx).text(cx);
            if editor_text != model_url.as_ref() {
                self.url_editor.update(cx, |editor, cx| {
                    editor.set_text(model_url.as_ref(), window, cx);
                });
            }
        }

        let can_back = self.item.read(cx).can_go_back;
        let can_fwd = self.item.read(cx).can_go_forward;
        let is_loading = self.item.read(cx).is_loading;
        let item = self.item.clone();

        let drawing_on = self.item.read(cx).drawing_mode_enabled;
        let has_strokes_any = !self.item.read(cx).drawing.is_empty();

        // Phase 4 architectural fix: viewport intentionally has NO
        // background fill. The WebView2 underlay sits in the DComp
        // tree under GPUI's swap chain; GPUI's alpha-premultiplied
        // swap chain clears to transparent (when the window's
        // background appearance is non-Opaque), so wherever GPUI
        // doesn't paint a pixel the page shows through. Painting a
        // bg here would block the page.
        let mut viewport = div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(BrowserViewportElement::new(item.clone()));

        #[cfg(target_os = "windows")]
        {
            if !drawing_on {
                viewport = self.attach_mouse_handlers(viewport, cx);
            }
        }

        // Always paint the strokes layer (read-only) so previously
        // drawn marks stay visible while design-mode picker is active
        // or drawing mode is off. The interactive capture layer only
        // attaches when drawing mode is on.
        if has_strokes_any || drawing_on {
            viewport = viewport.child(
                div()
                    .absolute()
                    .inset_0()
                    .child(DrawingPaintElement::new(item)),
            );
        }
        if let Some(overlay) = self.render_agent_cursor_overlay(cx) {
            viewport = viewport.child(deferred(overlay).with_priority(3));
        }
        if drawing_on {
            viewport = self.attach_drawing_handlers(viewport, cx);
        }

        let root = v_flex()
            .track_focus(&self.focus_handle)
            .key_context("BrowserView")
            .on_action(cx.listener(Self::on_submit_url))
            .on_action(cx.listener(Self::on_open_devtools))
            .on_action(cx.listener(Self::on_focus_address_bar))
            .on_action(cx.listener(Self::on_toggle_design_mode))
            .on_action(cx.listener(Self::on_toggle_drawing_mode))
            .on_action(cx.listener(Self::on_clear_drawing))
            .on_action(cx.listener(Self::on_preview_selected_element))
            .on_action(cx.listener(Self::on_click_previewed_element))
            .on_action(cx.listener(Self::on_clear_agent_cursor));

        #[cfg(target_os = "windows")]
        let root = root
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_action(cx.listener(Self::on_agent_resolve_element))
            .on_action(cx.listener(Self::on_agent_click_resolved_element))
            .on_action(cx.listener(Self::on_agent_clear_cursor));

        let mut tree = root
            .size_full()
            .child(self.render_address_bar(can_back, can_fwd, is_loading, cx))
            .child(viewport);

        #[cfg(target_os = "windows")]
        if let Some(panel) = self.render_design_panel(cx) {
            tree = tree.child(panel);
        }

        // While the design panel is being dragged, lay a transparent
        // window-covering catcher on top so mouse-move/up are reliably
        // captured anywhere (the panel header only starts the drag). This
        // is the standard modal-backdrop pattern; GPUI does not otherwise
        // route the release back to the element that began the drag.
        #[cfg(target_os = "windows")]
        if self.item.read(cx).design_drag.is_some() {
            let win = window.viewport_size();
            tree = tree.child(
                deferred(
                    anchored()
                        .position_mode(AnchoredPositionMode::Window)
                        .position(point(px(0.), px(0.)))
                        .child(
                            div()
                                .occlude()
                                .w(win.width)
                                .h(win.height)
                                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                                    this.item.update(cx, |item, cx| {
                                        if let Some((start_mouse, start_offset)) = item.design_drag
                                        {
                                            item.design_panel_offset = point(
                                                start_offset.x + (ev.position.x - start_mouse.x),
                                                start_offset.y + (ev.position.y - start_mouse.y),
                                            );
                                            cx.notify();
                                        }
                                    });
                                }))
                                .on_mouse_up(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                        this.item.update(cx, |item, cx| {
                                            item.design_drag = None;
                                            cx.notify();
                                        });
                                    }),
                                )
                                // Release OUTSIDE the window: GPUI's on_mouse_up
                                // is hitbox-gated and won't fire off-window, but
                                // Win32 SetCapture still delivers the up — catch
                                // it here so the drag can't get stuck (which
                                // would leave the catcher occluding the window).
                                .on_mouse_up_out(
                                    MouseButton::Left,
                                    cx.listener(|this, _: &MouseUpEvent, _, cx| {
                                        this.item.update(cx, |item, cx| {
                                            item.design_drag = None;
                                            cx.notify();
                                        });
                                    }),
                                ),
                        ),
                )
                .with_priority(2),
            );
        }

        tree
    }
}

impl BrowserView {
    fn render_agent_cursor_overlay(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let cursor = self.item.read(cx).agent_cursor.clone()?;
        let rect = cursor.target.rect;
        if rect.w <= 0. || rect.h <= 0. {
            return None;
        }

        let colors = cx.theme().colors();
        let border = colors.border_focused;
        let target_x = rect.x + rect.w / 2.;
        let target_y = rect.y + rect.h / 2.;
        let (pointer_x, pointer_y) = cursor.pointer_position.unwrap_or((target_x, target_y));
        let marker_x = pointer_x - 7.;
        let marker_y = pointer_y - 7.;
        let target_marker_x = target_x - 9.;
        let target_marker_y = target_y - 9.;

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .child(
                    div()
                        .absolute()
                        .left(px(target_marker_x))
                        .top(px(target_marker_y))
                        .w(px(18.))
                        .h(px(18.))
                        .border_2()
                        .border_color(border)
                        .rounded_full()
                        .bg(border.opacity(0.12)),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(marker_x))
                        .top(px(marker_y))
                        .w(px(14.))
                        .h(px(14.))
                        .rounded_full()
                        .border_2()
                        .border_color(colors.elevated_surface_background)
                        .bg(border),
                )
                .child(
                    div()
                        .absolute()
                        .left(px((target_x + 12.).min(rect.x + rect.w)))
                        .top(px((target_y - 26.).max(0.)))
                        .px_1p5()
                        .py_0p5()
                        .rounded_md()
                        .bg(colors.elevated_surface_background)
                        .border_1()
                        .border_color(border)
                        .child(
                            Label::new(cursor.label)
                                .size(LabelSize::XSmall)
                                .color(Color::Default),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Phase 4.C: float a "Describe the change" panel near the selected
    /// page element. Returns `None` when there's nothing to show
    /// (design mode off, no selection, or no recorded viewport bounds).
    ///
    /// Positioning math: the JS gives us the element rect in CSS pixels
    /// relative to the page viewport. The page viewport's top-left in
    /// Zed window coords is `item.last_bounds.origin` (we track this
    /// every prepaint, so it stays accurate through resize / scroll /
    /// sidebar toggle). Add the two together and we anchor in window
    /// space — `Anchored::position` then handles overflow-flipping if
    /// the panel would fall off the right or bottom edge of the window.
    #[cfg(target_os = "windows")]
    fn render_design_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let item = self.item.read(cx);
        if !item.design_mode_enabled {
            return None;
        }
        let selection = item.design_selection.as_ref()?;
        let viewport_bounds = item.last_bounds?;
        let viewport_origin = viewport_bounds.origin;
        let offset = item.design_panel_offset;
        let has_strokes = !item.drawing.is_empty();
        let theme = cx.theme().colors().clone();

        // Base anchor: just below the selected element, plus the user's
        // drag offset. `snap_to_window` keeps it on-screen.
        let anchor_x = px(f32::from(viewport_origin.x) + selection.rect.x) + offset.x;
        let anchor_y =
            px(f32::from(viewport_origin.y) + selection.rect.y + selection.rect.h + 8.) + offset.y;

        // Target chip: prefer source file:line, then the element tag,
        // then the raw selector. Full selector shown on hover.
        let target_label = selection
            .source
            .as_ref()
            .and_then(|s| {
                if let Some(file) = s.file_name.as_ref() {
                    Some(match s.line_number {
                        Some(line) => format!("{file}:{line}"),
                        None => file.clone(),
                    })
                } else {
                    s.component.clone()
                }
            })
            .or_else(|| selection.tag.as_ref().map(|t| format!("<{t}>")))
            .unwrap_or_else(|| selection.selector.clone());
        let full_selector = SharedString::new(selection.selector.clone());
        let selector_for_key = selection.selector.clone();
        let selector_for_submit = selection.selector.clone();

        let prompt_empty = self
            .design_prompt_editor
            .read(cx)
            .text(cx)
            .trim()
            .is_empty();
        let attach_text = if has_strokes {
            "Sends an annotated screenshot (with your drawing) + element context"
        } else {
            "Sends a screenshot + element context"
        };

        let panel = v_flex()
            .occlude()
            .w(px(380.))
            .bg(theme.elevated_surface_background)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_lg()
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                if ks.key == "enter" && (ks.modifiers.platform || ks.modifiers.control) {
                    this.on_design_submit(&selector_for_key, window, cx);
                    cx.stop_propagation();
                } else if ks.key == "escape" {
                    this.on_design_cancel(window, cx);
                    cx.stop_propagation();
                }
            }))
            // Header — also the drag handle.
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border_variant)
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_1()
                            .items_center()
                            // Drag handle = the title area ONLY, so the X
                            // button stays clickable. (If the header itself
                            // were the handle, clicking the X would start a
                            // micro-drag whose catcher overlay eats the click.)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                    this.item.update(cx, |item, cx| {
                                        item.design_drag =
                                            Some((ev.position, item.design_panel_offset));
                                        cx.notify();
                                    });
                                    cx.stop_propagation();
                                }),
                            )
                            .child(
                                Icon::new(IconName::Crosshair)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(Label::new("Describe the change").size(LabelSize::Small)),
                    )
                    .child(
                        IconButton::new("design-close", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Cancel (Esc)"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_design_cancel(window, cx);
                            })),
                    ),
            )
            // Target chip.
            .child(
                div().px_2().pt_2().child(
                    h_flex()
                        .id("design-target")
                        .px_1p5()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.element_background)
                        .tooltip(Tooltip::text(full_selector))
                        .child(
                            Label::new(target_label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                ),
            )
            // Prompt input.
            .child(
                div()
                    .m_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.editor_background)
                    .border_1()
                    .border_color(theme.border_variant)
                    .child(self.design_prompt_editor.clone()),
            )
            // Footer — what gets sent + actions.
            .child(
                h_flex()
                    .px_2()
                    .pb_2()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        Label::new(attach_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Label::new("Ctrl+Enter")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Button::new("design-cancel", "Cancel")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.on_design_cancel(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("design-submit", "Submit")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .disabled(prompt_empty)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.on_design_submit(&selector_for_submit, window, cx);
                                    })),
                            ),
                    ),
            );

        // `anchored` positions at paint time but paints in document
        // order — `deferred` bumps it onto the late-paint pass with
        // priority so it sits over the viewport + drawing overlay.
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopLeft)
                    .position_mode(AnchoredPositionMode::Window)
                    .position(point(anchor_x, anchor_y))
                    .snap_to_window()
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    #[cfg(target_os = "windows")]
    fn on_design_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.design_prompt_editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        self.item.update(cx, |item, cx| {
            item.design_selection = None;
            item.design_prompt.clear();
            item.design_panel_offset = point(px(0.), px(0.));
            item.design_drag = None;
            if let Some(session) = &item.session {
                let _ = session.post_message_string("clear_selection");
            }
            cx.notify();
        });
        // Move focus off the prompt editor — it stops rendering now that the
        // selection is cleared, and an orphaned focus handle would drive a
        // continuous re-render (visible as toolbar flicker). The browser root
        // is always present, so focus it.
        window.focus(&self.focus_handle, cx);
    }

    #[cfg(target_os = "windows")]
    fn on_design_submit(&mut self, selector: &str, window: &mut Window, cx: &mut Context<Self>) {
        use base64::Engine as _;

        let prompt_text = self.design_prompt_editor.read(cx).text(cx);
        let selector_owned = selector.to_string();

        // Snapshot everything the bundle needs *now*, before the
        // async screenshot completes — `BrowserItem.design_selection`
        // may be cleared by the user before the callback fires.
        let (
            outer_html,
            source,
            drawing_snapshot,
            element_rect,
            viewport_origin,
            viewport_size,
            page_url,
        ) = {
            let item = self.item.read(cx);
            let Some(sel) = item.design_selection.as_ref() else {
                log::warn!("browser_viewer: submit fired without a selection");
                return;
            };
            let bounds = item.last_bounds.unwrap_or_default();
            (
                sel.outer_html.clone(),
                sel.source.clone(),
                item.drawing.clone(),
                (sel.rect.x, sel.rect.y, sel.rect.w, sel.rect.h),
                bounds.origin,
                bounds.size,
                item.url.to_string(),
            )
        };
        let has_drawing = !drawing_snapshot.is_empty();
        // "file:line" hint when the page script detected a React source.
        let source_hint = source.as_ref().and_then(|s| {
            if let Some(file) = s.file_name.as_ref() {
                let loc = match s.line_number {
                    Some(line) => format!("{file}:{line}"),
                    None => file.clone(),
                };
                Some(match s.component.as_ref() {
                    Some(component) => format!("{loc} ({component})"),
                    None => loc,
                })
            } else {
                // React 19 has no file:line — fall back to the component name.
                s.component.clone()
            }
        });

        // Capture is async — the PNG arrives on the GPUI foreground
        // thread via this oneshot. We then composite the design
        // annotations onto it, base64-encode it, and dispatch a
        // `SendDesignBundleToAgent` action that the agent panel turns
        // into a claude-acp prompt (4.F). The `write_bundle` to %TEMP%
        // remains a debug-only fallback gated on an env var.
        let (tx, rx) = futures::channel::oneshot::channel::<anyhow::Result<Vec<u8>>>();
        let mut tx_slot = Some(tx);
        let dispatch = move |result: anyhow::Result<Vec<u8>>| {
            if let Some(tx) = tx_slot.take() {
                let _ = tx.send(result);
            }
        };
        let dispatch_box: Box<dyn FnOnce(anyhow::Result<Vec<u8>>) + 'static> = Box::new(dispatch);

        let kicked_off = self.item.update(cx, |item, _| {
            if let Some(session) = item.session.as_ref() {
                if let Err(err) = session.capture_preview_png(dispatch_box) {
                    log::warn!("browser_viewer: capture_preview_png dispatch failed: {err}");
                    return false;
                }
                return true;
            }
            false
        });
        if !kicked_off {
            log::warn!("browser_viewer: submit dropped — no live session");
            return;
        }

        cx.spawn_in(window, async move |view, cx| {
            let png = match rx.await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(err)) => {
                    log::warn!("browser_viewer: capture_preview err: {err:?}");
                    return;
                }
                Err(_) => {
                    log::debug!("browser_viewer: capture_preview channel dropped");
                    return;
                }
            };

            // Composite the element outline + freehand strokes onto the
            // capture, off the UI thread.
            let annotated = {
                let png = png.clone();
                let drawing = drawing_snapshot.clone();
                cx.background_spawn(async move {
                    crate::drawing::annotate_screenshot(
                        &png,
                        Some(element_rect),
                        &drawing,
                        viewport_origin,
                        viewport_size,
                    )
                })
                .await
            };
            let annotated_png = match annotated {
                Ok(bytes) => bytes,
                Err(err) => {
                    log::warn!("browser_viewer: annotate failed, sending raw capture: {err:?}");
                    png
                }
            };

            // Debug-only fallback: persist the bundle to %TEMP%.
            if std::env::var_os("ZED_BROWSER_DESIGN_DEBUG_BUNDLE").is_some() {
                let bundle = crate::bundle::DesignBundle {
                    prompt: prompt_text.clone(),
                    selector: selector_owned.clone(),
                    outer_html: outer_html.clone(),
                    source: source.clone(),
                    drawing_svg: drawing_snapshot.to_svg(viewport_origin, viewport_size),
                    page_url: page_url.clone(),
                    screenshot_png: annotated_png.clone(),
                };
                match crate::bundle::write_bundle(&bundle) {
                    Ok(dir) => log::info!(
                        "browser_viewer: [fork-debug] design bundle written to {}",
                        dir.display()
                    ),
                    Err(err) => {
                        log::warn!("browser_viewer: [fork-debug] failed to persist bundle: {err:?}")
                    }
                }
            }

            let annotated_png_base64 =
                base64::engine::general_purpose::STANDARD.encode(&annotated_png);
            log::debug!(
                "browser_viewer: dispatching design bundle to agent — \
                 selector={}, png={} bytes ({} b64 chars), has_drawing={}, source={:?}",
                selector_owned,
                annotated_png.len(),
                annotated_png_base64.len(),
                has_drawing,
                source_hint,
            );

            let action = zed_actions::agent::SendDesignBundleToAgent {
                prompt: prompt_text.into(),
                selector: selector_owned.into(),
                page_url: page_url.into(),
                outer_html: outer_html.into(),
                source_hint: source_hint.map(Into::into),
                annotated_png_base64: annotated_png_base64.into(),
                drawing_svg: drawing_snapshot
                    .to_svg(viewport_origin, viewport_size)
                    .into(),
                has_drawing,
            };

            let _ = view.update_in(cx, |this, window, cx| {
                window.dispatch_action(Box::new(action), cx);
                // The drawing has been consumed into the dispatched image.
                this.item.update(cx, |item, cx| {
                    item.drawing.clear();
                    cx.notify();
                });
            });
        })
        .detach();

        // Clear selection + prompt now so the user sees an immediate
        // response. The async write proceeds in the background.
        self.on_design_cancel(window, cx);
    }

    fn render_address_bar(
        &self,
        can_back: bool,
        can_fwd: bool,
        is_loading: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let design_on = self.item.read(cx).design_mode_enabled;
        let drawing_on = self.item.read(cx).drawing_mode_enabled;
        let has_strokes = !self.item.read(cx).drawing.is_empty();
        let has_selection = self.item.read(cx).design_selection.is_some();
        let has_agent_cursor = self.item.read(cx).agent_cursor.is_some();
        h_flex()
            .h_8()
            .flex_none()
            .px_2()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().tab_bar_background)
            .child(
                IconButton::new("browser-back", IconName::ArrowLeft)
                    .icon_size(IconSize::Small)
                    .disabled(!can_back)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        #[cfg(target_os = "windows")]
                        this.go_back(cx);
                        #[cfg(not(target_os = "windows"))]
                        let _ = (this, cx);
                    })),
            )
            .child(
                IconButton::new("browser-forward", IconName::ArrowRight)
                    .icon_size(IconSize::Small)
                    .disabled(!can_fwd)
                    .tooltip(Tooltip::text("Forward"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        #[cfg(target_os = "windows")]
                        this.go_forward(cx);
                        #[cfg(not(target_os = "windows"))]
                        let _ = (this, cx);
                    })),
            )
            .child(
                IconButton::new(
                    "browser-reload",
                    if is_loading {
                        IconName::Close
                    } else {
                        IconName::ArrowCircle
                    },
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(if is_loading { "Stop" } else { "Reload" }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    #[cfg(target_os = "windows")]
                    {
                        if is_loading {
                            this.stop_loading(cx);
                        } else {
                            this.reload_page(cx);
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    let _ = (this, cx);
                })),
            )
            .child(
                div()
                    .flex_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.url_editor.clone()),
            )
            .child(
                div()
                    .w(px(190.))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.agent_target_editor.clone()),
            )
            .child(
                IconButton::new("browser-preview-target-query", IconName::Check)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Preview target text or css:selector"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        #[cfg(target_os = "windows")]
                        this.preview_agent_target_from_editor(cx);
                        #[cfg(not(target_os = "windows"))]
                        let _ = (this, cx);
                    })),
            )
            .child(
                IconButton::new("browser-design-mode", IconName::Crosshair)
                    .icon_size(IconSize::Small)
                    .toggle_state(design_on)
                    .tooltip(Tooltip::text(if design_on {
                        "Design Mode: ON (click to disable)"
                    } else {
                        "Design Mode: OFF (click to enable element picker)"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_design_mode(&crate::ToggleDesignMode, window, cx);
                    })),
            )
            .child(
                IconButton::new("browser-drawing-mode", IconName::Pencil)
                    .icon_size(IconSize::Small)
                    .toggle_state(drawing_on)
                    .tooltip(Tooltip::text(if drawing_on {
                        "Drawing Mode: ON (click to disable)"
                    } else {
                        "Drawing Mode: OFF (click to draw on the page)"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_drawing_mode(&crate::ToggleDrawingMode, window, cx);
                    })),
            )
            .when(has_strokes, |b| {
                b.child(
                    IconButton::new("browser-drawing-clear", IconName::Eraser)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Clear strokes"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_clear_drawing(&crate::ClearDrawing, window, cx);
                        })),
                )
            })
            .child(
                IconButton::new("browser-preview-selected", IconName::Crosshair)
                    .icon_size(IconSize::Small)
                    .disabled(!has_selection)
                    .tooltip(Tooltip::text("Preview selected element for agent click"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_preview_selected_element(
                            &crate::PreviewSelectedElement,
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                IconButton::new("browser-click-previewed", IconName::PlayFilled)
                    .icon_size(IconSize::Small)
                    .disabled(!has_agent_cursor)
                    .tooltip(Tooltip::text("Click previewed browser target"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_click_previewed_element(&crate::ClickPreviewedElement, window, cx);
                    })),
            )
    }
}

impl Item for BrowserView {
    type Event = BrowserViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            BrowserViewEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }

    fn for_each_project_item(
        &self,
        _cx: &App,
        _f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.item.read(cx).title().clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ToolWeb))
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.item.read(cx).url().clone())
    }

    fn agent_browser_context(&self, cx: &App) -> Option<SharedString> {
        let item = self.item.read(cx);
        let mut context = format!(
            "Zed embedded browser:\n- URL: {}\n- Title: {}",
            item.url(),
            item.title()
        );

        if let Some(selection) = item.design_selection.as_ref() {
            let label = selection
                .tag
                .as_deref()
                .map(|tag| format!("<{tag}>"))
                .unwrap_or_else(|| "selected element".to_string());
            context.push_str(&format!(
                "\n- Selected element: {}\n  Selector: {}\n  Bounds: x={}, y={}, width={}, height={}",
                label,
                selection.selector,
                selection.rect.x,
                selection.rect.y,
                selection.rect.w,
                selection.rect.h
            ));
        }

        if let Some(cursor) = item.agent_cursor.as_ref() {
            context.push_str(&format!(
                "\n- Previewed target: {}\n  Request id: {}",
                browser_target_context(&cursor.target),
                cursor.request_id
            ));
            if !cursor.ambiguity.is_empty() {
                context.push_str(&format!(
                    "\n  Ambiguous candidates: {}",
                    cursor.ambiguity.len() + 1
                ));
            }
        }

        context.push_str(
            "\nAvailable ACP tools: browser.current_page, browser.open, browser.navigate, browser.snapshot, browser.screenshot, browser.click, browser.fill, browser.scroll_to, browser.scroll, browser.trace, browser.find_element, browser.click_element, browser.type_text, browser.clear_cursor. Use browser.open when the user asks to open @browser or when no active tab exists. Default loop: browser.open or browser.navigate, then browser.snapshot, then use refs from browser.snapshot with browser.click, browser.fill, or browser.scroll_to. Use browser.current_page or browser.snapshot drawingOverlay.hasDrawing / drawingOverlay.svg to inspect GPUI freehand drawing annotations; the page DOM root will not contain those strokes. Use browser.scroll_to when the snapshot already contains the target section or text, such as Reviews or Specifications; use browser.scroll only for manual relative movement. Use browser.screenshot when a popup, modal, blank/empty snapshot, drawing overlay, or actionability failure does not match what the user can see. Snapshot refs are short-lived; re-snapshot after navigation, click, fill, scroll, or any visible UI change. Use browser.find_element only as a repair path when snapshot output is insufficient; after ambiguous candidates, choose a candidate or re-snapshot instead of repeating broad probes. If an exact in-page filter value is unavailable, use the closest visible site control. Use these against the Zed embedded browser tab; do not use external Chrome, iab, the Codex Desktop Browser plugin, or external web search for this context.",
        );
        Some(context.into())
    }

    /// Capture a weak ref to the workspace + subscribe so we re-render
    /// across modal open/close transitions. Workspace re-emits the
    /// modal-open event from its `ModalLayer`; there is no `ModalClosed`
    /// event, but `ModalLayer::hide_modal` calls `cx.notify()`, which the
    /// `cx.observe(&workspace)` subscription picks up — so the same
    /// re-render path handles both transitions.
    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = workspace.weak_handle();
        self.workspace = Some(weak.clone());
        if let Some(entity) = weak.upgrade() {
            cx.subscribe(&entity, |_, _, event: &workspace::Event, cx| {
                if matches!(event, workspace::Event::ModalOpened) {
                    cx.notify();
                }
            })
            .detach();
            cx.observe(&entity, |_, _, cx| {
                cx.notify();
            })
            .detach();
        }
    }

    /// Called by `workspace::pane` when this item stops being the active
    /// item in its pane. We do NOT hide the WebView2 (`SetIsVisible(false)`)
    /// — that produced a one-frame desktop flash on reactivation (the
    /// cutout was emitted before the WebView resumed painting). Instead we
    /// mark this tab not-fronted; when it next becomes active, `Render`
    /// reorders its underlay to the front of the underlay group so it
    /// occludes any other browser tab sharing the pane. Inactive tabs keep
    /// painting (cheap — Chromium throttles offscreen rendering) but sit
    /// behind the active tab's underlay, so they no longer leak through its
    /// cutout (the multi-tab bug from Task #28).
    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            item.is_visible = false;
        });
    }
}

/// Custom element whose `prepaint` snapshots the window-relative bounds and
/// drives the underlying [`BrowserItem`]'s WebView2 visual position. Paints
/// nothing of its own — WebView2 paints directly into the DComp visual.
struct BrowserViewportElement {
    item: Entity<BrowserItem>,
}

impl BrowserViewportElement {
    fn new(item: Entity<BrowserItem>) -> Self {
        Self { item }
    }
}

impl IntoElement for BrowserViewportElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for BrowserViewportElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: size(relative(1.).into(), relative(1.).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        #[cfg(target_os = "windows")]
        {
            let hwnd = hwnd_from_window(window);
            self.item.update(cx, |item, cx| {
                drive_session(item, bounds, hwnd, cx);
            });
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (bounds, window, cx);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Phase 4 architectural fix: insert a Cutout into the scene at
        // this element's z-position. The Windows renderer wipes the
        // swap-chain pixels in `bounds` to alpha = 0 via
        // ID3D11DeviceContext1::ClearView, exposing the WebView2
        // underlay visual in the DComp tree. Modals, popovers, the
        // floating Describe panel, drawing strokes — anything painted
        // later — render on top of the transparent region and stay
        // visible.
        //
        // Only emit the cutout when our session is live, so before
        // first navigation the viewport area shows the workspace
        // background (a fine "loading" placeholder) rather than the
        // window's compositor background.
        #[cfg(target_os = "windows")]
        {
            let has_session = self.item.read(cx).session.is_some();
            if has_session {
                window.paint_cutout(bounds);
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (bounds, window, cx);
        }
    }
}

/// Renders all committed strokes plus the currently-in-progress one as
/// GPUI `paint_path` calls. Owns no input — the overlay div above it
/// handles mouse capture and pushes points into
/// `BrowserItem.drawing`.
struct DrawingPaintElement {
    item: Entity<BrowserItem>,
}

impl DrawingPaintElement {
    fn new(item: Entity<BrowserItem>) -> Self {
        Self { item }
    }
}

impl IntoElement for DrawingPaintElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for DrawingPaintElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: size(relative(1.).into(), relative(1.).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let canvas = self.item.read(cx).drawing.clone();
        let stroke_color = gpui::hsla(0.36, 1.0, 0.5, 1.0); // bright green
        for stroke in canvas.strokes.iter().chain(canvas.current.as_ref()) {
            if stroke.points.len() < 2 {
                continue;
            }
            let mut builder = gpui::PathBuilder::stroke(stroke.width);
            builder.move_to(stroke.points[0]);
            for p in stroke.points.iter().skip(1) {
                builder.line_to(*p);
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, stroke_color);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn hwnd_from_window(window: &mut Window) -> Option<HWND> {
    let raw = window.window_handle().ok()?.as_raw();
    if let RawWindowHandle::Win32(win32) = raw {
        Some(HWND(win32.hwnd.get() as *mut std::ffi::c_void))
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn drive_session(
    item: &mut BrowserItem,
    bounds: Bounds<Pixels>,
    hwnd: Option<HWND>,
    cx: &mut Context<BrowserItem>,
) {
    // Skip zero-area frames; happens transiently while the tab is hidden
    // or before first layout.
    if bounds.size.width <= Pixels::ZERO || bounds.size.height <= Pixels::ZERO {
        return;
    }

    // Reposition the visual whenever the element's rect changes.
    let bounds_changed = item.last_bounds != Some(bounds);
    if bounds_changed {
        if let Some(session) = &item.session {
            let x = f32::from(bounds.origin.x);
            let y = f32::from(bounds.origin.y);
            let width = f32::from(bounds.size.width) as i32;
            let height = f32::from(bounds.size.height) as i32;
            if let Err(err) = session.set_rect(x, y, width, height) {
                log::warn!("BrowserItem: set_rect failed: {err:?}");
            }
        }
        item.last_bounds = Some(bounds);
    }

    // Kick off the WebView2 init exactly once, on the first prepaint that
    // has a real HWND and a non-empty rect.
    if !item.init_started && item.session.is_none() {
        let Some(hwnd) = hwnd else {
            return;
        };
        if let Err(err) = start_session(item, bounds, hwnd, cx) {
            log::error!("BrowserItem: failed to dispatch WebView2 init: {err:?}");
        }
    }
}

#[cfg(target_os = "windows")]
fn start_session(
    item: &mut BrowserItem,
    bounds: Bounds<Pixels>,
    hwnd: HWND,
    cx: &mut Context<BrowserItem>,
) -> anyhow::Result<()> {
    // Underlay: the WebView2 visual sits *below* GPUI's swap chain in
    // the DComp tree. Combined with the window's transparent
    // background appearance (set below) and a non-opaque viewport div,
    // GPUI overlays (address bar, floating Describe panel, drawing
    // strokes) render on top of the page — fixing the Phase 4 z-order
    // problem documented in plans/browser-viewer.md §AC-P2-2.
    let visual = gpui_windows::create_underlay_visual_for_hwnd(hwnd)
        .context("gpui_windows::create_underlay_visual_for_hwnd")?;

    let x = f32::from(bounds.origin.x);
    let y = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width) as i32;
    let height = f32::from(bounds.size.height) as i32;

    // Position the visual where the BrowserView element lives in the
    // window. The subsequent `initialize` call configures controller
    // bounds + popup-position notification on the same coords.
    unsafe {
        visual
            .visual()
            .SetOffsetX2(x)
            .map_err(|err| anyhow!("initial SetOffsetX2: {err}"))?;
        visual
            .visual()
            .SetOffsetY2(y)
            .map_err(|err| anyhow!("initial SetOffsetY2: {err}"))?;
    }

    // Initial controller bounds are placed at the visual's screen-space
    // origin so popups (right-click menu, autofill, alert dialogs) land
    // anchored to the page, not at the parent window's (0, 0).
    let rect = windows::Win32::Foundation::RECT {
        left: x as i32,
        top: y as i32,
        right: x as i32 + width,
        bottom: y as i32 + height,
    };

    let (ready_tx, ready_rx) = oneshot::channel();
    let (events_tx, events_rx) = mpsc::unbounded::<NavigationEvent>();
    let url = item.url.to_string();
    initialize(
        hwnd,
        visual,
        rect,
        url,
        events_tx,
        Box::new(move |result| {
            // Sender is dropped here if the channel is gone; that means the
            // tab was closed before init finished, which is fine.
            let _ = ready_tx.send(result);
        }),
    )
    .context("initialize")?;

    item.init_started = true;
    item.last_bounds = Some(bounds);

    // Single foreground task that:
    // 1. Waits for the WebView2Session to be ready.
    // 2. Stores it on the entity.
    // 3. Drains navigation events for the rest of the tab's lifetime,
    //    updating BrowserItem state on each event. When the session is
    //    dropped (tab closed), its Drop removes the event handlers,
    //    closing the channel and ending this task.
    cx.spawn(async move |this, cx| {
        let Ok(result) = ready_rx.await else {
            return;
        };
        let session_stored = match this.update(cx, |item, cx| {
            match result {
                Ok(session) => {
                    log::info!("browser_viewer: session ready for {}", item.url);
                    // Front the new tab's underlay only if it's STILL the
                    // active tab — the user can switch away during the async
                    // WebView2 init, and fronting an inactive tab would
                    // occlude the active one (the Task #28 z-order class). An
                    // inactive tab re-fronts itself via Render on reactivation.
                    if item.is_visible {
                        if let Err(err) = session.bring_underlay_to_front() {
                            log::warn!("browser_viewer: initial underlay front failed: {err:?}");
                        }
                    }
                    item.session = Some(session);
                    cx.notify();
                    true
                }
                Err(err) => {
                    log::error!(
                        "browser_viewer: WebView2 init failed for {}: {err:?}",
                        item.url
                    );
                    item.init_started = false;
                    cx.notify();
                    false
                }
            }
        }) {
            Ok(stored) => stored,
            Err(_) => return,
        };
        if !session_stored {
            return;
        }

        // Drain navigation events for the rest of the session's life.
        let mut events_rx = events_rx;
        while let Some(event) = events_rx.next().await {
            if this
                .update(cx, |item, cx| {
                    apply_navigation_event(item, event);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();

    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_navigation_event(item: &mut BrowserItem, event: NavigationEvent) {
    match event {
        NavigationEvent::TitleChanged(title) => {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                item.title = SharedString::new(trimmed);
            }
        }
        NavigationEvent::SourceChanged(url) => {
            item.url = SharedString::new(url);
            // If we haven't received a title yet, fall back to the URL so
            // the tab label stays meaningful.
            if item.title.as_ref() == item.url.as_ref() || item.title.is_empty() {
                item.title = item.url.clone();
            }
        }
        NavigationEvent::HistoryChanged {
            can_go_back,
            can_go_forward,
        } => {
            item.can_go_back = can_go_back;
            item.can_go_forward = can_go_forward;
        }
        NavigationEvent::NavigationStarting => {
            item.is_loading = true;
            // Stale selection from the previous document is no longer
            // meaningful — clear so the floating "Describe" input
            // disappears for the duration of the load. The script
            // re-injects on every navigation and the user picks anew.
            item.design_selection = None;
            item.agent_cursor = None;
        }
        NavigationEvent::NavigationCompleted { is_success } => {
            item.is_loading = false;
            if !is_success {
                log::debug!("browser_viewer: navigation completed without success");
            }
        }
        NavigationEvent::DesignModeMessage(raw) => {
            use crate::design::DesignInbound;
            if let Some(parsed) = crate::browser_protocol::BrowserAutomationInbound::parse(&raw) {
                apply_browser_automation_event(item, parsed);
                return;
            }
            let Some(parsed) = DesignInbound::parse(&raw) else {
                log::debug!("browser_viewer: dropping unparseable browser msg: {raw}");
                return;
            };
            match parsed {
                DesignInbound::Ready => {
                    // Script installed. If we already have design mode
                    // enabled on the host (e.g. user toggled it before
                    // first paint), push activate so the script catches up.
                    if item.design_mode_enabled
                        && let Some(session) = &item.session
                    {
                        let _ = session.post_message_string("activate");
                    }
                }
                DesignInbound::ElementSelected(sel) => {
                    log::info!(
                        "browser_viewer: element selected — {} ({} chars HTML)",
                        sel.selector,
                        sel.outer_html.len()
                    );
                    item.design_selection = Some(sel);
                    // Fresh selection re-anchors the panel at the new element.
                    item.design_panel_offset = point(px(0.), px(0.));
                    item.design_drag = None;
                }
                DesignInbound::PageScrolled(scroll) => {
                    // Update the selection's rect so the host can
                    // re-anchor the floating input. Selector +
                    // outerHTML stay stable.
                    if let Some(sel) = item.design_selection.as_mut() {
                        sel.rect = scroll.rect;
                    }
                }
            }
        }
        NavigationEvent::ConsoleEvent(event) => {
            push_console_event(item, event);
        }
        NavigationEvent::NetworkEvent(event) => {
            push_network_event(item, event);
        }
    }
}

#[cfg(target_os = "windows")]
fn apply_browser_automation_event(
    item: &mut BrowserItem,
    event: crate::browser_protocol::BrowserAutomationInbound,
) {
    match event {
        crate::browser_protocol::BrowserAutomationInbound::AgentTargetResolved {
            request_id,
            target,
        } => {
            item.agent_cursor = Some(crate::agent_cursor::AgentCursorState::preview(
                request_id.clone(),
                target.clone(),
            ));
            item.agent_find_results.insert(
                request_id.clone(),
                serde_json::json!({
                    "ok": true,
                    "requestId": request_id.clone(),
                    "selector": target.selector.clone(),
                    "tag": target.tag.clone(),
                    "text": target.text.clone(),
                    "role": target.role.clone(),
                    "accessibleName": target.accessible_name.clone(),
                    "bounds": {
                        "x": target.rect.x,
                        "y": target.rect.y,
                        "width": target.rect.w,
                        "height": target.rect.h,
                    },
                    "confidence": target.confidence.clone(),
                }),
            );
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentTargetNotFound {
            request_id,
            reason,
        } => {
            item.agent_find_results.insert(
                request_id.clone(),
                serde_json::json!({
                    "ok": false,
                    "requestId": request_id.clone(),
                    "reason": reason.clone(),
                }),
            );
            item.agent_cursor = Some(crate::agent_cursor::AgentCursorState {
                request_id: "agent".to_string(),
                target: crate::browser_protocol::BrowserResolvedElement {
                    selector: "not-found".to_string(),
                    tag: Some("missing".to_string()),
                    text: Some(reason.clone()),
                    role: None,
                    accessible_name: Some(reason.clone()),
                    rect: crate::design::ElementRect {
                        x: 0.,
                        y: 0.,
                        w: 0.,
                        h: 0.,
                    },
                    source: None,
                    confidence: crate::browser_protocol::BrowserTargetConfidence::Weak,
                },
                status: crate::agent_cursor::AgentCursorStatus::Failed(reason.clone()),
                label: reason,
                ambiguity: Vec::new(),
                pointer_position: None,
            });
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentTargetAmbiguous {
            request_id,
            candidates,
        } => {
            item.agent_find_results.insert(
                request_id.clone(),
                serde_json::json!({
                    "ok": false,
                    "requestId": request_id.clone(),
                    "reason": "Multiple visible elements matched",
                    "candidates": candidates.clone(),
                }),
            );
            if let Some(first) = candidates.first().cloned() {
                let mut cursor =
                    crate::agent_cursor::AgentCursorState::preview("agent".to_string(), first);
                cursor.ambiguity = candidates;
                item.agent_cursor = Some(cursor);
            }
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentTypeTextResult {
            request_id,
            outcome,
        } => {
            item.agent_text_results.insert(request_id, outcome);
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentPageStateResult {
            request_id,
            state,
        } => {
            item.agent_page_results.insert(request_id, state);
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentVisibleElementsResult {
            request_id,
            snapshot,
        } => {
            item.agent_snapshot_results.insert(request_id, snapshot);
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentSnapshotResult {
            request_id,
            snapshot,
        } => {
            item.latest_snapshot = Some(snapshot.clone());
            item.agent_snapshots.insert(request_id, snapshot);
        }
        crate::browser_protocol::BrowserAutomationInbound::AgentActionabilityResult {
            request_id,
            outcome,
        } => {
            item.agent_actionability_results.insert(request_id, outcome);
        }
    }
}

/// Open a new browser tab in the active pane of the given workspace.
pub fn open_new_tab(
    workspace: &mut Workspace,
    url: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let view = cx.new(|cx| BrowserView::new(url, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

/// Best-effort parsing of address-bar input: if it parses as a URL with a
/// scheme, use as-is; if it looks like a host (contains a `.` and no spaces),
/// prepend `https://`; otherwise treat as a search query against
/// `search_url_template` (with `{query}` substituted).
fn parse_address_bar_input(input: &str, search_url_template: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "about:blank".to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    const KNOWN_SCHEMES: &[&str] = &[
        "http://",
        "https://",
        "file://",
        "about:",
        "data:",
        "javascript:",
    ];
    if KNOWN_SCHEMES.iter().any(|p| lower.starts_with(p)) {
        return trimmed.to_string();
    }
    // localhost:port is a common dev-server case.
    if trimmed.starts_with("localhost") || trimmed.starts_with("127.0.0.1") {
        return format!("http://{trimmed}");
    }
    // Bare host or host/path — must contain a dot, no spaces, and start
    // with an alphanumeric.
    let looks_like_host = trimmed.contains('.')
        && !trimmed.contains(' ')
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if looks_like_host {
        return format!("https://{trimmed}");
    }
    // Fall through to search.
    let encoded = url_encode_query(trimmed);
    if search_url_template.contains("{query}") {
        search_url_template.replace("{query}", &encoded)
    } else {
        // Template without `{query}` — append as a query parameter so the
        // user still gets a search rather than navigating to the bare URL.
        format!("{search_url_template}{encoded}")
    }
}

/// Tiny URL form-encoder for the search-query fallback. Replaces spaces
/// with `+` and percent-encodes characters outside the unreserved set.
fn url_encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                let hi = byte >> 4;
                let lo = byte & 0xF;
                out.push(if hi < 10 {
                    (b'0' + hi) as char
                } else {
                    (b'A' + hi - 10) as char
                });
                out.push(if lo < 10 {
                    (b'0' + lo) as char
                } else {
                    (b'A' + lo - 10) as char
                });
            }
        }
    }
    out
}

/// CDP modifier bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8. Matches the
/// values `Input.dispatchKeyEvent.modifiers` expects.
#[cfg(target_os = "windows")]
fn cdp_modifiers_mask(m: &Modifiers) -> i32 {
    let mut bits = 0;
    if m.alt {
        bits |= 1;
    }
    if m.control {
        bits |= 2;
    }
    if m.platform {
        bits |= 4;
    }
    if m.shift {
        bits |= 8;
    }
    bits
}

/// Translate a GPUI `Keystroke` into the four fields CDP
/// `Input.dispatchKeyEvent` needs: (`KeyboardEvent.key`,
/// `KeyboardEvent.code`, `windowsVirtualKeyCode`, `text`).
///
/// `text` is `Some` only for printable single-character keystrokes
/// without Ctrl/Alt modifiers — that's what tells the renderer to
/// fire an `input` event on form controls. Special keys (Tab,
/// Enter, arrows, F1-F12, ...) intentionally pass `None`.
///
/// The `KeyboardEvent.code` value is the physical key code from the
/// US-QWERTY layout — for non-US layouts this may not match the
/// user's actual keycap, but pages that care use `key` (the logical
/// value) anyway. Acceptable for Phase 3.
#[cfg(target_os = "windows")]
fn keystroke_to_cdp(ks: &Keystroke) -> Option<(String, String, i32, Option<String>)> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    let m = &ks.modifiers;
    let key_str = ks.key.as_str();

    // Special-key table: GPUI's `key` string → (KeyboardEvent.key,
    // KeyboardEvent.code, VK_*). Drives non-printable keys; printable
    // keys fall through to the text path below.
    let special: Option<(&str, &str, i32)> = match key_str {
        "enter" => Some(("Enter", "Enter", VK_RETURN.0 as i32)),
        "tab" => Some(("Tab", "Tab", VK_TAB.0 as i32)),
        "escape" => Some(("Escape", "Escape", VK_ESCAPE.0 as i32)),
        "backspace" => Some(("Backspace", "Backspace", VK_BACK.0 as i32)),
        "delete" => Some(("Delete", "Delete", VK_DELETE.0 as i32)),
        "insert" => Some(("Insert", "Insert", VK_INSERT.0 as i32)),
        "space" => Some((" ", "Space", VK_SPACE.0 as i32)),
        "up" => Some(("ArrowUp", "ArrowUp", VK_UP.0 as i32)),
        "down" => Some(("ArrowDown", "ArrowDown", VK_DOWN.0 as i32)),
        "left" => Some(("ArrowLeft", "ArrowLeft", VK_LEFT.0 as i32)),
        "right" => Some(("ArrowRight", "ArrowRight", VK_RIGHT.0 as i32)),
        "home" => Some(("Home", "Home", VK_HOME.0 as i32)),
        "end" => Some(("End", "End", VK_END.0 as i32)),
        "pageup" => Some(("PageUp", "PageUp", VK_PRIOR.0 as i32)),
        "pagedown" => Some(("PageDown", "PageDown", VK_NEXT.0 as i32)),
        "f1" => Some(("F1", "F1", VK_F1.0 as i32)),
        "f2" => Some(("F2", "F2", VK_F2.0 as i32)),
        "f3" => Some(("F3", "F3", VK_F3.0 as i32)),
        "f4" => Some(("F4", "F4", VK_F4.0 as i32)),
        "f5" => Some(("F5", "F5", VK_F5.0 as i32)),
        "f6" => Some(("F6", "F6", VK_F6.0 as i32)),
        "f7" => Some(("F7", "F7", VK_F7.0 as i32)),
        "f8" => Some(("F8", "F8", VK_F8.0 as i32)),
        "f9" => Some(("F9", "F9", VK_F9.0 as i32)),
        "f10" => Some(("F10", "F10", VK_F10.0 as i32)),
        "f11" => Some(("F11", "F11", VK_F11.0 as i32)),
        "f12" => Some(("F12", "F12", VK_F12.0 as i32)),
        _ => None,
    };
    if let Some((key, code, vk)) = special {
        return Some((key.to_string(), code.to_string(), vk, None));
    }

    // Printable single-character paths: letters, digits, punctuation.
    let lower = key_str.to_ascii_lowercase();
    let mut chars = lower.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        // Multi-char key string we don't recognize — skip.
        return None;
    }

    let (code, vk): (String, i32) = match first {
        'a'..='z' => {
            let upper = first.to_ascii_uppercase();
            (format!("Key{upper}"), upper as i32)
        }
        '0'..='9' => (format!("Digit{first}"), first as i32),
        '`' => ("Backquote".to_string(), VK_OEM_3.0 as i32),
        '-' => ("Minus".to_string(), VK_OEM_MINUS.0 as i32),
        '=' => ("Equal".to_string(), VK_OEM_PLUS.0 as i32),
        '[' => ("BracketLeft".to_string(), VK_OEM_4.0 as i32),
        ']' => ("BracketRight".to_string(), VK_OEM_6.0 as i32),
        '\\' => ("Backslash".to_string(), VK_OEM_5.0 as i32),
        ';' => ("Semicolon".to_string(), VK_OEM_1.0 as i32),
        '\'' => ("Quote".to_string(), VK_OEM_7.0 as i32),
        ',' => ("Comma".to_string(), VK_OEM_COMMA.0 as i32),
        '.' => ("Period".to_string(), VK_OEM_PERIOD.0 as i32),
        '/' => ("Slash".to_string(), VK_OEM_2.0 as i32),
        _ => return None,
    };

    // KeyboardEvent.key is the logical typed value: lowercase when
    // unshifted, uppercase/shifted symbol when shift is held. We use
    // GPUI's `key_char` when available (already reflects shift + dead
    // keys), falling back to the raw `key` letter.
    let key_for_event = ks
        .key_char
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| lower.clone());

    // `text` is what makes the renderer fire `input` events on inputs.
    // Only set it for keystrokes that produce a typeable character —
    // Ctrl-combos and Alt-combos suppress the text so a binding like
    // Ctrl+S doesn't also dump "s" into a focused text field.
    let typeable = !(m.control || m.alt) && !key_for_event.is_empty();
    let text = if typeable {
        Some(key_for_event.clone())
    } else {
        None
    };

    Some((key_for_event, code, vk, text))
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn browser_query_from_parts_accepts_comma_point() {
        let query =
            browser_query_from_parts("point", "12.5, 34").expect("point query should parse");

        match query {
            crate::browser_protocol::BrowserElementQuery::Point { x, y } => {
                assert_eq!(x, 12.5);
                assert_eq!(y, 34.);
            }
            other => panic!("expected point query, got {other:?}"),
        }
    }

    #[test]
    fn browser_query_from_parts_accepts_whitespace_point() {
        let query = browser_query_from_parts("point", "12.5 34").expect("point query should parse");

        match query {
            crate::browser_protocol::BrowserElementQuery::Point { x, y } => {
                assert_eq!(x, 12.5);
                assert_eq!(y, 34.);
            }
            other => panic!("expected point query, got {other:?}"),
        }
    }

    #[test]
    fn browser_query_from_parts_rejects_bad_point() {
        let error = browser_query_from_parts("point", "12.5")
            .expect_err("missing y coordinate should be rejected");

        assert_eq!(error, "Point query must use x,y");
    }

    #[test]
    fn blocking_page_messages_ignores_normal_page_chrome() {
        let messages = vec![
            "0 items in cart 0".to_string(),
            "Samsung Galaxy Fit3 Silver R 1,099 Price is 1099 rand R 1,299".to_string(),
        ];

        assert!(blocking_page_messages(messages).is_empty());
    }

    #[test]
    fn blocking_page_messages_keeps_real_validation_errors() {
        let messages = vec![
            "0 items in cart 0".to_string(),
            "Please include an '@' in the email address.".to_string(),
            "An unexpected error occurred".to_string(),
        ];

        assert_eq!(
            blocking_page_messages(messages),
            vec![
                "Please include an '@' in the email address.".to_string(),
                "An unexpected error occurred".to_string()
            ]
        );
    }

    #[test]
    fn find_snapshot_ref_rejects_wrong_snapshot_id() {
        let snapshot = crate::browser_protocol::BrowserAgentSnapshot {
            snapshot_id: "snap-1".to_string(),
            page_revision: 1,
            url: None,
            title: None,
            root: vec![crate::browser_protocol::BrowserSnapshotNode {
                node_ref: Some("e1".to_string()),
                role: Some("button".to_string()),
                name: Some("Submit".to_string()),
                text: None,
                selector: Some("button".to_string()),
                rect: Some(crate::design::ElementRect {
                    x: 0.,
                    y: 0.,
                    w: 100.,
                    h: 30.,
                }),
                children: Vec::new(),
                disabled: false,
                editable: false,
            }],
            reason: None,
        };

        let err = find_snapshot_ref(&snapshot, "snap-2", "e1")
            .expect_err("wrong snapshot id should fail");

        assert_eq!(err.to_string(), "Snapshot ref is stale");
    }

    #[test]
    fn find_snapshot_ref_returns_selector_and_rect() {
        let snapshot = crate::browser_protocol::BrowserAgentSnapshot {
            snapshot_id: "snap-1".to_string(),
            page_revision: 1,
            url: None,
            title: None,
            root: vec![crate::browser_protocol::BrowserSnapshotNode {
                node_ref: Some("e1".to_string()),
                role: Some("button".to_string()),
                name: Some("Submit".to_string()),
                text: None,
                selector: Some("button".to_string()),
                rect: Some(crate::design::ElementRect {
                    x: 0.,
                    y: 0.,
                    w: 100.,
                    h: 30.,
                }),
                children: Vec::new(),
                disabled: false,
                editable: false,
            }],
            reason: None,
        };

        let target = find_snapshot_ref(&snapshot, "snap-1", "e1").expect("ref should resolve");

        assert_eq!(target.selector, "button");
        assert_eq!(target.accessible_name.as_deref(), Some("Submit"));
    }

    #[test]
    fn find_snapshot_ref_for_scroll_accepts_text_node() {
        let snapshot = crate::browser_protocol::BrowserAgentSnapshot {
            snapshot_id: "snap-1".to_string(),
            page_revision: 1,
            url: None,
            title: None,
            root: vec![crate::browser_protocol::BrowserSnapshotNode {
                node_ref: Some("e9".to_string()),
                role: Some("text".to_string()),
                name: None,
                text: Some("Reviews 4.8 25 Reviews".to_string()),
                selector: Some("main > section:nth-of-type(5)".to_string()),
                rect: Some(crate::design::ElementRect {
                    x: 20.,
                    y: 1820.,
                    w: 600.,
                    h: 240.,
                }),
                children: Vec::new(),
                disabled: false,
                editable: false,
            }],
            reason: None,
        };

        let target =
            find_snapshot_ref_for_scroll(&snapshot, "snap-1", "e9").expect("text ref scrolls");

        assert_eq!(target.selector, "main > section:nth-of-type(5)");
        assert_eq!(target.text.as_deref(), Some("Reviews 4.8 25 Reviews"));
        assert_eq!(target.rect.y, 1820.);
    }

    #[test]
    fn actionability_failure_response_is_not_ok() {
        let outcome = crate::browser_protocol::BrowserActionabilityOutcome {
            ok: false,
            selector: "button".to_string(),
            checks: crate::browser_protocol::BrowserActionabilityChecks {
                attached: true,
                visible: true,
                stable: true,
                enabled: false,
                editable: false,
                receives_events: true,
            },
            reason: Some("Element is disabled".to_string()),
        };

        let response = actionability_failure_response("act-1", &outcome);

        assert_eq!(response["ok"], false);
        assert_eq!(response["requestId"], "act-1");
        assert_eq!(response["reason"], "Element is disabled");
    }

    #[test]
    fn browser_agent_snapshot_json_keeps_refs_for_next_action() {
        let snapshot = crate::browser_protocol::BrowserAgentSnapshot {
            snapshot_id: "snap-2".to_string(),
            page_revision: 7,
            url: Some("https://example.test/login".to_string()),
            title: Some("Login".to_string()),
            root: vec![crate::browser_protocol::BrowserSnapshotNode {
                node_ref: Some("e2".to_string()),
                role: Some("textbox".to_string()),
                name: Some("Email".to_string()),
                text: None,
                selector: Some("input#email".to_string()),
                rect: Some(crate::design::ElementRect {
                    x: 10.,
                    y: 20.,
                    w: 200.,
                    h: 40.,
                }),
                children: Vec::new(),
                disabled: false,
                editable: true,
            }],
            reason: None,
        };

        let json = browser_agent_snapshot_json(&snapshot);

        assert_eq!(json["snapshotId"], "snap-2");
        assert_eq!(json["pageRevision"], 7);
        assert_eq!(json["root"][0]["ref"], "e2");
        assert_eq!(json["root"][0]["editable"], true);
    }

    #[test]
    fn browser_drawing_overlay_json_exposes_visible_freehand_strokes() {
        let mut item = BrowserItem::new("https://example.test".into());
        item.last_bounds = Some(Bounds {
            origin: point(px(100.), px(200.)),
            size: size(px(400.), px(300.)),
        });
        item.drawing_mode_enabled = true;
        item.drawing.begin(
            point(px(110.), px(220.)),
            gpui::hsla(0.36, 1.0, 0.5, 1.0),
            px(3.),
        );
        item.drawing.extend(point(px(130.), px(240.)));
        item.drawing.finish();

        let overlay = browser_drawing_overlay_json(&item);

        assert_eq!(overlay["hasDrawing"], true);
        assert_eq!(overlay["drawingModeEnabled"], true);
        assert_eq!(overlay["strokeCount"], 1);
        assert_eq!(overlay["pointCount"], 2);
        assert_eq!(overlay["viewport"]["x"], 100.0);
        assert!(
            overlay["svg"]
                .as_str()
                .expect("drawing svg should be exposed")
                .contains("M10.0 20.0 L30.0 40.0")
        );
    }

    #[test]
    fn snapshot_tool_response_includes_drawing_overlay_context() {
        let snapshot = crate::browser_protocol::BrowserAgentSnapshot {
            snapshot_id: "snapshot-test".to_string(),
            page_revision: 1,
            url: Some("https://example.test".to_string()),
            title: Some("Example".to_string()),
            root: Vec::new(),
            reason: None,
        };
        let drawing_overlay = serde_json::json!({
            "hasDrawing": true,
            "svg": "<svg><path d=\"M1 2 L3 4\"/></svg>",
        });

        let response = snapshot_tool_response("req-1", snapshot, drawing_overlay);

        assert_eq!(response["ok"], true);
        assert_eq!(response["drawingOverlay"]["hasDrawing"], true);
        assert!(
            response["drawingOverlay"]["svg"]
                .as_str()
                .unwrap()
                .contains("M1 2 L3 4")
        );
    }

    #[test]
    fn browser_scroll_wheel_data_uses_intuitive_page_direction() {
        assert_eq!(browser_scroll_wheel_data(120.0), (-120_i32) as u32);
        assert_eq!(browser_scroll_wheel_data(-120.0), 120_u32);
        assert_eq!(browser_scroll_wheel_data(0.0), 0_u32);
    }

    #[test]
    fn direct_snapshot_script_assigns_refs_to_text_nodes() {
        let script = direct_snapshot_script("snapshot-test");

        assert!(script.contains("const hasSnapshotTarget = isInteractive || Boolean(text);"));
        assert!(script.contains("selector: hasSnapshotTarget ? cssPath(el) : null"));
        assert!(script.contains("if (hasSnapshotTarget) node.ref"));
    }

    #[test]
    fn scroll_into_view_script_uses_center_alignment_by_default() {
        let script = scroll_into_view_script("main > section:nth-of-type(5)", "sideways");

        assert!(script.contains("scrollIntoView"));
        assert!(script.contains("\"center\""));
        assert!(script.contains("main > section"));
    }

    #[test]
    fn browser_screenshot_path_sanitizes_request_id() {
        let path = browser_screenshot_path("popup recovery:/\\*?");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("path should have utf8 filename");

        assert!(file_name.starts_with("zed-browser-popup-recovery-"));
        assert!(file_name.ends_with(".png"));
        assert!(!file_name.contains(':'));
        assert!(!file_name.contains('/'));
        assert!(!file_name.contains('\\'));
    }

    #[test]
    fn parse_executed_snapshot_result_accepts_object_json() {
        let result = r#"{
            "snapshotId": "snapshot-test",
            "pageRevision": 9,
            "url": "file:///test.html",
            "title": "Test",
            "root": []
        }"#;

        let snapshot = parse_executed_snapshot_result(result).expect("object result should parse");

        assert_eq!(snapshot.snapshot_id, "snapshot-test");
        assert_eq!(snapshot.page_revision, 9);
    }

    #[test]
    fn parse_executed_snapshot_result_accepts_json_string_result() {
        let inner = serde_json::json!({
            "snapshotId": "snapshot-string",
            "pageRevision": 10,
            "url": "file:///test.html",
            "title": "Test",
            "root": []
        })
        .to_string();
        let result = serde_json::to_string(&inner).expect("string result should encode");

        let snapshot = parse_executed_snapshot_result(&result).expect("string result should parse");

        assert_eq!(snapshot.snapshot_id, "snapshot-string");
        assert_eq!(snapshot.page_revision, 10);
    }

    #[test]
    fn direct_snapshot_script_embeds_snapshot_id_and_returns_tree() {
        let script = direct_snapshot_script("snapshot-test");

        assert!(script.contains("const snapshotId = \"snapshot-test\";"));
        assert!(script.contains("return {"));
        assert!(script.contains("pageRevision: Math.floor(Date.now() / 1000)"));
        assert!(script.contains("root: rootNode ? [rootNode] : []"));
        assert!(script.contains("node.ref"));
    }

    #[test]
    fn push_agent_trace_bounds_recent_entries() {
        let mut item = BrowserItem::new(SharedString::from("https://example.test"));

        for index in 0..(AGENT_TRACE_MAX_ENTRIES + 2) {
            push_agent_trace(
                &mut item,
                crate::browser_protocol::BrowserAgentTraceEntry {
                    sequence: 0,
                    tool: "browser.snapshot".to_string(),
                    request_id: Some(format!("req-{index}")),
                    snapshot_id: None,
                    element_ref: None,
                    selector: None,
                    ok: true,
                    reason: None,
                },
            );
        }

        assert_eq!(item.agent_trace.len(), AGENT_TRACE_MAX_ENTRIES);
        assert_eq!(
            item.agent_trace
                .front()
                .and_then(|entry| entry.request_id.as_deref()),
            Some("req-2")
        );
        assert_eq!(
            item.agent_trace.back().map(|entry| entry.sequence),
            Some((AGENT_TRACE_MAX_ENTRIES + 2) as u64)
        );
    }
}
