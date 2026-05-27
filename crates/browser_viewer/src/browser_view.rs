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
    App, AppContext as _, Bounds, Context, Div, Element, ElementId, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    NavigationDirection, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, SharedString,
    Style, Window, div, relative, size,
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
        COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN, COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
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
}

#[cfg(target_os = "windows")]
use windows_imports::*;

#[cfg(target_os = "windows")]
use crate::webview2_host::{NavigationEvent, WebView2Session, initialize};

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
    /// True between kicking off WebView2 init and the session landing in
    /// `session`. Prevents re-triggering init on every prepaint.
    init_started: bool,
    last_bounds: Option<Bounds<Pixels>>,
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
}

impl BrowserView {
    pub fn new(url: SharedString, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let item = cx.new(|_| BrowserItem::new(url.clone()));

        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(url.as_ref(), window, cx);
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
        }
    }

    pub fn item(&self) -> &Entity<BrowserItem> {
        &self.item
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

    fn on_submit_url(
        &mut self,
        _: &menu::Confirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.url_editor.read(cx).text(cx);
        let target = parse_address_bar_input(&input);
        #[cfg(target_os = "windows")]
        self.navigate_to(target, cx);
        #[cfg(not(target_os = "windows"))]
        let _ = target;
    }

    #[cfg(target_os = "windows")]
    fn attach_mouse_handlers(&self, root: Div, cx: &mut Context<Self>) -> Div {
        root.on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                let kind = if ev.click_count >= 2 {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOUBLE_CLICK
                } else {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN
                };
                let vk = virtual_keys(&ev.modifiers, Some(MouseButton::Left));
                forward_mouse_event(this, cx, ev.position, kind, vk, 0);
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
                    vk,
                    0,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let kind = if ev.click_count >= 2 {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOUBLE_CLICK
                } else {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN
                };
                let vk = virtual_keys(&ev.modifiers, Some(MouseButton::Middle));
                forward_mouse_event(this, cx, ev.position, kind, vk, 0);
            }),
        )
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
                    vk,
                    0,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let kind = if ev.click_count >= 2 {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOUBLE_CLICK
                } else {
                    COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN
                };
                let vk = virtual_keys(&ev.modifiers, Some(MouseButton::Right));
                forward_mouse_event(this, cx, ev.position, kind, vk, 0);
            }),
        )
        .on_mouse_up(
            MouseButton::Right,
            cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
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
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, Some(ev.button));
                forward_mouse_event(
                    this,
                    cx,
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
            cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
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
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, Some(ev.button));
                forward_mouse_event(
                    this,
                    cx,
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
            cx.listener(|this, ev: &MouseUpEvent, _, cx| {
                let vk = virtual_keys(&ev.modifiers, None);
                forward_mouse_event(
                    this,
                    cx,
                    ev.position,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
                    vk,
                    2,
                );
                cx.stop_propagation();
            }),
        )
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            let vk = virtual_keys(&ev.modifiers, ev.pressed_button);
            forward_mouse_event(
                this,
                cx,
                ev.position,
                COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                vk,
                0,
            );
        }))
        .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
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
    position: Point<Pixels>,
    kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
    vk: COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
    mouse_data: u32,
) {
    view.item.update(cx, |item, _| {
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
}

impl EventEmitter<BrowserViewEvent> for BrowserView {}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let mut viewport = div()
            .flex_1()
            .min_h_0()
            .bg(cx.theme().colors().editor_background)
            .child(BrowserViewportElement::new(item));

        #[cfg(target_os = "windows")]
        {
            viewport = self.attach_mouse_handlers(viewport, cx);
        }

        v_flex()
            .track_focus(&self.focus_handle)
            .key_context("BrowserView")
            .on_action(cx.listener(Self::on_submit_url))
            .size_full()
            .child(self.render_address_bar(can_back, can_fwd, is_loading, cx))
            .child(viewport)
    }
}

impl BrowserView {
    fn render_address_bar(
        &self,
        can_back: bool,
        can_fwd: bool,
        is_loading: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
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
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        // The WebView2 visual draws itself via the DComp tree; nothing to do.
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
    let visual = gpui_windows::create_child_visual_for_hwnd(hwnd)
        .context("gpui_windows::create_child_visual_for_hwnd")?;

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
        }
        NavigationEvent::NavigationCompleted { is_success } => {
            item.is_loading = false;
            if !is_success {
                log::debug!("browser_viewer: navigation completed without success");
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
/// prepend `https://`; otherwise treat as a Google search query.
fn parse_address_bar_input(input: &str) -> String {
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
    format!("https://www.google.com/search?q={encoded}")
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
