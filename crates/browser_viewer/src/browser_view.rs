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
    Render, ScrollDelta, ScrollWheelEvent, SharedString, Size, Style, WeakEntity, Window, anchored,
    deferred, div, point, px, relative, size,
};
use ui::Tooltip;
use ui::prelude::*;
use workspace::{
    Workspace,
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
#[cfg(target_os = "macos")]
use crate::wkwebview_host::WKWebViewSession;
#[cfg(target_os = "macos")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// One notch on a mouse wheel; matches Win32 `WHEEL_DELTA`.
#[cfg(target_os = "windows")]
const WHEEL_DELTA: f32 = 120.0;

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
    #[cfg(target_os = "macos")]
    session: Option<WKWebViewSession>,
    /// True between kicking off WebView2 init and the session landing in
    /// `session`. Prevents re-triggering init on every prepaint.
    init_started: bool,
    last_bounds: Option<Bounds<Pixels>>,
    /// Tracks whether the WebView is currently shown. Mirrors the last
    /// value passed to `controller.SetIsVisible`. Drives Phase 1.E
    /// tab-switch hide/show so an inactive browser tab's contents don't
    /// bleed through behind the active tab in the same pane.
    is_visible: bool,
    #[cfg(target_os = "macos")]
    automation_viewport_override: Option<Size<Pixels>>,
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
    /// Invalidates automation ref handles on navigation (see `automation`).
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    automation_state: crate::automation::AutomationSessionState,
    /// Phase 4.C: user drag offset for the "Describe the change" panel,
    /// added to its element-anchored base position so it can be moved off
    /// whatever it covers. Reset on new selection / panel close.
    design_panel_offset: Point<Pixels>,
    /// Active panel drag: (mouse position at drag start, panel offset at
    /// drag start). `Some` while the panel header is held.
    design_drag: Option<(Point<Pixels>, Point<Pixels>)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeSurfaceVisibilityUpdate {
    Show,
    Hide,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UrlEditorSync {
    Sync,
    PreserveUserInput,
}

fn native_surface_visibility_update(
    is_active_item: bool,
    is_visible: bool,
) -> NativeSurfaceVisibilityUpdate {
    match (is_active_item, is_visible) {
        (true, false) => NativeSurfaceVisibilityUpdate::Show,
        (false, true) => NativeSurfaceVisibilityUpdate::Hide,
        _ => NativeSurfaceVisibilityUpdate::Unchanged,
    }
}

fn should_paint_native_browser_cutout(has_session: bool, is_visible: bool) -> bool {
    has_session && is_visible
}

fn url_editor_sync_policy(editor_focused: bool, force_sync: bool) -> UrlEditorSync {
    if force_sync || !editor_focused {
        UrlEditorSync::Sync
    } else {
        UrlEditorSync::PreserveUserInput
    }
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
            #[cfg(target_os = "macos")]
            session: None,
            init_started: false,
            last_bounds: None,
            is_visible: true,
            #[cfg(target_os = "macos")]
            automation_viewport_override: None,
            design_mode_enabled: false,
            design_selection: None,
            design_prompt: String::new(),
            drawing_mode_enabled: false,
            drawing: crate::drawing::DrawingCanvas::default(),
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            automation_state: crate::automation::AutomationSessionState::new(),
            design_panel_offset: point(px(0.), px(0.)),
            design_drag: None,
        }
    }

    pub fn url(&self) -> &SharedString {
        &self.url
    }

    #[cfg(target_os = "macos")]
    pub fn set_automation_viewport_override(&mut self, width: Pixels, height: Pixels) {
        self.automation_viewport_override = Some(size(width, height));
        self.last_bounds = None;
    }

    #[cfg(target_os = "macos")]
    pub fn clear_automation_viewport_override(&mut self) {
        self.automation_viewport_override = None;
        self.last_bounds = None;
    }

    #[cfg(target_os = "macos")]
    pub fn automation_viewport_override(&self) -> Option<Size<Pixels>> {
        self.automation_viewport_override
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

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn automation_page_generation(&self) -> u64 {
        self.automation_state.page_generation()
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn resolve_automation_ref(&self, ref_id: &str) -> Option<crate::automation::ElementRef> {
        self.automation_state.resolve_ref(ref_id)
    }
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
    force_url_editor_sync: bool,
}

impl BrowserView {
    pub fn new(url: SharedString, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let item = cx.new(|_| BrowserItem::new(url.clone()));

        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(url.as_ref(), window, cx);
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

        Self {
            item,
            focus_handle: cx.focus_handle(),
            url_editor,
            design_prompt_editor,
            workspace: None,
            design_prompt_focused_for: None,
            force_url_editor_sync: false,
        }
    }

    pub fn item(&self) -> &Entity<BrowserItem> {
        &self.item
    }

    /// Run `f` against the live WebView2 session when initialization has finished.
    #[cfg(target_os = "windows")]
    pub fn with_webview_session<R>(
        &self,
        cx: &App,
        f: impl FnOnce(&crate::webview2_host::WebView2Session) -> R,
    ) -> Option<R> {
        self.item.read(cx).session.as_ref().map(f)
    }

    /// Run `f` against the platform automation session when one is live.
    #[cfg(target_os = "windows")]
    pub fn with_automation_session<R>(
        &self,
        cx: &App,
        f: impl FnOnce(&crate::webview2_host::WebView2Session) -> R,
    ) -> Option<R> {
        self.item.read(cx).session.as_ref().map(f)
    }

    /// Run `f` against the platform automation session when one is live.
    #[cfg(target_os = "macos")]
    pub fn with_automation_session<R>(
        &self,
        cx: &App,
        f: impl FnOnce(&dyn crate::automation::AutomationSession) -> R,
    ) -> Option<R> {
        self.item
            .read(cx)
            .session
            .as_ref()
            .map(|session| f(session))
    }

    #[cfg(target_os = "macos")]
    pub fn with_wkwebview_session<R>(
        &self,
        cx: &App,
        f: impl FnOnce(&crate::wkwebview_host::WKWebViewSession) -> R,
    ) -> Option<R> {
        self.item.read(cx).session.as_ref().map(f)
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn store_automation_snapshot(
        &self,
        cx: &mut App,
        snapshot: crate::automation::PageSnapshot,
    ) {
        let ref_count = snapshot.ref_count;
        let page_generation = snapshot.page_generation;
        let yaml = snapshot.yaml.clone();
        let registry = snapshot.registry;
        self.item.update(cx, |item, _| {
            item.automation_state.replace_registry(registry);
        });
        log::info!(
            "browser automation snapshot: {ref_count} refs (page gen {page_generation})\n{yaml}"
        );
        if std::env::var("ZED_BROWSER_AUTOMATION_DEBUG").as_deref() == Ok("1") {
            let dir = std::env::temp_dir().join("zed-browser-automation");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("snapshot-{page_generation}.yaml"));
            let _ = std::fs::write(path, yaml);
        }
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    pub fn automation_navigate(&mut self, target: String, cx: &mut Context<Self>) {
        // Pre-set is_loading so a following wait-for-load can't return before
        // the NavigationStarting event fires (otherwise the first poll sees the
        // PREVIOUS load already complete and `evaluate` hits the stale context).
        self.item.update(cx, |item, _| item.is_loading = true);
        self.navigate_to(target, cx);
    }

    #[cfg(target_os = "macos")]
    pub fn automation_update_page_state(
        &mut self,
        url: Option<String>,
        title: Option<String>,
        is_loading: bool,
        cx: &mut Context<Self>,
    ) {
        let force_url_sync = url.is_some();
        self.item.update(cx, |item, _| {
            item.is_loading = is_loading;
            if let Some(url) = url {
                item.url = SharedString::new(url);
            }
            if let Some(title) = title {
                item.title = SharedString::new(title);
            }
        });
        if force_url_sync {
            self.force_url_editor_sync = true;
            cx.notify();
        }
    }

    /// CP7: navigate back in history. Returns `false` (no-op) if there is no
    /// back entry. Pre-sets `is_loading` so a following wait-for-load doesn't
    /// race the `NavigationStarting` event.
    #[cfg(target_os = "windows")]
    pub fn automation_go_back(&self, cx: &mut Context<Self>) -> bool {
        if !self.item.read(cx).can_go_back {
            return false;
        }
        self.item.update(cx, |item, _| item.is_loading = true);
        self.go_back(cx);
        true
    }

    #[cfg(target_os = "macos")]
    pub fn automation_go_back(&self, cx: &mut Context<Self>) -> bool {
        let can_go_back = self
            .item
            .read(cx)
            .session
            .as_ref()
            .map(WKWebViewSession::can_go_back)
            .unwrap_or(false);
        if !can_go_back {
            return false;
        }
        self.item.update(cx, |item, _| item.is_loading = true);
        self.go_back(cx);
        true
    }

    #[cfg(target_os = "windows")]
    fn navigate_to(&mut self, target: String, cx: &mut Context<Self>) {
        self.force_url_editor_sync = true;
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
        #[cfg(target_os = "macos")]
        self.navigate_to(target, cx);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let _ = (target, cx);
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
        #[cfg(target_os = "macos")]
        {
            let is_active_item = self.is_active_pane_item(cx);
            self.item.update(cx, |item, _| {
                match native_surface_visibility_update(is_active_item, item.is_visible) {
                    NativeSurfaceVisibilityUpdate::Show => {
                        if let Some(session) = item.session.as_ref()
                            && let Err(err) = session.set_visible(true)
                        {
                            log::warn!("BrowserItem: show WKWebView on activate failed: {err:?}");
                            return;
                        }
                        item.is_visible = true;
                    }
                    NativeSurfaceVisibilityUpdate::Hide => {
                        if let Some(session) = item.session.as_ref() {
                            session.release_keyboard_focus();
                            if let Err(err) = session.set_visible(false) {
                                log::warn!(
                                    "BrowserItem: hide inactive WKWebView on render failed: {err:?}"
                                );
                                return;
                            }
                        }
                        item.is_visible = false;
                    }
                    NativeSurfaceVisibilityUpdate::Unchanged => {}
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
        #[cfg(target_os = "macos")]
        if editor_focused || !self.focus_handle.is_focused(window) {
            self.item.update(cx, |item, _| {
                if let Some(session) = item.session.as_ref() {
                    session.release_keyboard_focus();
                }
            });
        }
        if url_editor_sync_policy(editor_focused, self.force_url_editor_sync) == UrlEditorSync::Sync
        {
            let editor_text = self.url_editor.read(cx).text(cx);
            if editor_text != model_url.as_ref() {
                self.url_editor.update(cx, |editor, cx| {
                    editor.set_text(model_url.as_ref(), window, cx);
                });
            }
            self.force_url_editor_sync = false;
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
            .on_action(cx.listener(Self::on_clear_drawing));

        #[cfg(target_os = "windows")]
        let root = root
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up));

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
        self.hide_native_browser_surface(cx);
    }

    fn on_removed(&self, cx: &mut Context<Self>) {
        self.hide_native_browser_surface(cx);
    }

    fn workspace_deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.hide_native_browser_surface(cx);
    }
}

impl BrowserView {
    #[cfg(target_os = "macos")]
    fn is_active_pane_item(&self, cx: &Context<Self>) -> bool {
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return true;
        };

        workspace
            .read(cx)
            .pane_for_item_id(cx.entity_id())
            .and_then(|pane| pane.read(cx).active_item())
            .is_some_and(|active_item| active_item.item_id() == cx.entity_id())
    }

    #[cfg(target_os = "macos")]
    fn hide_native_browser_surface(&self, cx: &mut Context<Self>) {
        self.item.update(cx, |item, _| {
            item.is_visible = false;
            if let Some(session) = item.session.as_ref() {
                session.release_keyboard_focus();
                if let Err(err) = session.set_visible(false) {
                    log::warn!("BrowserItem: hide WKWebView on deactivate failed: {err:?}");
                }
            }
        });
    }

    #[cfg(not(target_os = "macos"))]
    fn hide_native_browser_surface(&self, cx: &mut Context<Self>) {
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
        #[cfg(target_os = "macos")]
        {
            let native_view = ns_view_from_window(window);
            self.item.update(cx, |item, cx| {
                drive_session(item, bounds, native_view, cx);
            });
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let item = self.item.read(cx);
            if should_paint_native_browser_cutout(item.session.is_some(), item.is_visible) {
                window.paint_cutout(bounds);
            }
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

#[cfg(target_os = "macos")]
fn ns_view_from_window(window: &mut Window) -> Option<crate::wkwebview_host::NativeView> {
    let raw = window.window_handle().ok()?.as_raw();
    if let RawWindowHandle::AppKit(appkit) = raw {
        Some(appkit.ns_view.as_ptr().cast())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn drive_session(
    item: &mut BrowserItem,
    bounds: Bounds<Pixels>,
    native_view: Option<crate::wkwebview_host::NativeView>,
    cx: &mut Context<BrowserItem>,
) {
    let session_bounds = macos_session_bounds(item, bounds);
    if session_bounds.size.width <= Pixels::ZERO || session_bounds.size.height <= Pixels::ZERO {
        return;
    }

    let bounds_changed = item.last_bounds != Some(session_bounds);
    if bounds_changed {
        if let Some(session) = &item.session
            && let Err(err) = session.set_rect(session_bounds)
        {
            log::warn!("BrowserItem: WKWebView set_rect failed: {err:?}");
        }
        item.last_bounds = Some(session_bounds);
    }

    if !item.init_started && item.session.is_none() {
        let Some(native_view) = native_view else {
            return;
        };
        if let Err(err) = start_session(item, session_bounds, native_view, cx) {
            log::error!("BrowserItem: failed to initialize WKWebView: {err:?}");
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_session_bounds(item: &BrowserItem, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let Some(override_size) = item.automation_viewport_override() else {
        return bounds;
    };
    Bounds::new(
        bounds.origin,
        size(
            min_pixels(override_size.width, bounds.size.width),
            min_pixels(override_size.height, bounds.size.height),
        ),
    )
}

#[cfg(target_os = "macos")]
fn min_pixels(left: Pixels, right: Pixels) -> Pixels {
    if left <= right { left } else { right }
}

#[cfg(target_os = "macos")]
fn start_session(
    item: &mut BrowserItem,
    bounds: Bounds<Pixels>,
    native_view: crate::wkwebview_host::NativeView,
    cx: &mut Context<BrowserItem>,
) -> anyhow::Result<()> {
    let url = item.url.to_string();
    let session = WKWebViewSession::initialize(native_view, bounds, &url)?;
    session.set_visible(item.is_visible)?;
    item.session = Some(session);
    item.init_started = true;
    item.last_bounds = Some(bounds);
    cx.notify();
    Ok(())
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

#[cfg(target_os = "macos")]
impl BrowserView {
    fn navigate_to(&mut self, target: String, cx: &mut Context<Self>) {
        self.force_url_editor_sync = true;
        self.item.update(cx, |item, cx| {
            item.url = SharedString::new(target.clone());
            item.title = item.url.clone();
            item.is_loading = true;
            item.clear_automation_viewport_override();
            item.automation_state.bump_page_generation();
            if let Some(session) = item.session.as_ref() {
                match session.navigate(&target) {
                    Ok(()) => item.is_loading = false,
                    Err(err) => {
                        item.is_loading = false;
                        log::warn!("BrowserView::navigate_to({target}): {err}");
                    }
                }
            }
            cx.notify();
        });
    }

    fn go_back(&self, cx: &mut Context<Self>) {
        self.item.update(cx, |item, cx| {
            let can_go_back = item
                .session
                .as_ref()
                .is_some_and(WKWebViewSession::can_go_back);
            if !can_go_back {
                return;
            }

            item.is_loading = true;
            item.clear_automation_viewport_override();
            item.automation_state.bump_page_generation();

            if let Some(session) = item.session.as_ref() {
                match session.go_back() {
                    Ok(()) => item.is_loading = false,
                    Err(err) => {
                        item.is_loading = false;
                        log::warn!("BrowserView::go_back: {err}");
                    }
                }
            } else {
                item.is_loading = false;
            }
            cx.notify();
        });
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
            item.automation_state.bump_page_generation();
            // Stale selection from the previous document is no longer
            // meaningful — clear so the floating "Describe" input
            // disappears for the duration of the load. The script
            // re-injects on every navigation and the user picks anew.
            item.design_selection = None;
        }
        NavigationEvent::NavigationCompleted { is_success } => {
            item.is_loading = false;
            if !is_success {
                log::debug!("browser_viewer: navigation completed without success");
            }
        }
        NavigationEvent::DesignModeMessage(raw) => {
            use crate::design::DesignInbound;
            let Some(parsed) = DesignInbound::parse(&raw) else {
                log::debug!("browser_viewer: dropping unparseable design msg: {raw}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_browser_surface_visibility_tracks_active_item() {
        assert_eq!(
            native_surface_visibility_update(true, false),
            NativeSurfaceVisibilityUpdate::Show
        );
        assert_eq!(
            native_surface_visibility_update(false, true),
            NativeSurfaceVisibilityUpdate::Hide
        );
        assert_eq!(
            native_surface_visibility_update(true, true),
            NativeSurfaceVisibilityUpdate::Unchanged
        );
        assert_eq!(
            native_surface_visibility_update(false, false),
            NativeSurfaceVisibilityUpdate::Unchanged
        );
    }

    #[test]
    fn native_browser_cutout_requires_visible_session() {
        assert!(should_paint_native_browser_cutout(true, true));
        assert!(!should_paint_native_browser_cutout(true, false));
        assert!(!should_paint_native_browser_cutout(false, true));
        assert!(!should_paint_native_browser_cutout(false, false));
    }

    #[test]
    fn focused_url_editor_syncs_after_host_driven_navigation() {
        assert_eq!(
            url_editor_sync_policy(true, false),
            UrlEditorSync::PreserveUserInput
        );
        assert_eq!(url_editor_sync_policy(true, true), UrlEditorSync::Sync);
        assert_eq!(url_editor_sync_policy(false, false), UrlEditorSync::Sync);
    }
}
