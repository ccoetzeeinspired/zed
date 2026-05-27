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
use futures::channel::oneshot;
use gpui::{
    App, Bounds, Context, Element, ElementId, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Render,
    SharedString, Style, Window, div, relative, size,
};
use ui::prelude::*;
use workspace::{
    Workspace,
    item::{Item, ItemBufferKind, ItemEvent},
};

#[cfg(target_os = "windows")]
mod windows_imports {
    pub use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    pub use windows::Win32::Foundation::HWND;
}

#[cfg(target_os = "windows")]
use windows_imports::*;

#[cfg(target_os = "windows")]
use crate::webview2_host::{WebView2Session, initialize};

/// Backing model for one browser tab.
pub struct BrowserItem {
    url: SharedString,
    title: SharedString,
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
}

impl EventEmitter<()> for BrowserItem {}

/// The Zed tab view.
pub struct BrowserView {
    item: Entity<BrowserItem>,
    focus_handle: FocusHandle,
}

impl BrowserView {
    pub fn new(url: SharedString, cx: &mut Context<Self>) -> Self {
        let item = cx.new(|_| BrowserItem::new(url));
        Self {
            item,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn item(&self) -> &Entity<BrowserItem> {
        &self.item
    }
}

impl EventEmitter<()> for BrowserView {}

impl Focusable for BrowserView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.item.clone();
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(BrowserViewportElement::new(item))
    }
}

impl Item for BrowserView {
    type Event = ();

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

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
    if bounds_changed && item.session.is_some() {
        if let Some(session) = &item.session {
            let offset_x = f32::from(bounds.origin.x);
            let offset_y = f32::from(bounds.origin.y);
            let width = f32::from(bounds.size.width) as i32;
            let height = f32::from(bounds.size.height) as i32;
            if let Err(err) = session.set_position(offset_x, offset_y) {
                log::warn!("BrowserItem: set_position failed: {err:?}");
            }
            if let Err(err) = session.set_size(width, height) {
                log::warn!("BrowserItem: set_size failed: {err:?}");
            }
            if let Err(err) = session.commit() {
                log::warn!("BrowserItem: commit failed: {err:?}");
            }
        }
        item.last_bounds = Some(bounds);
    } else if bounds_changed {
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

    let offset_x = f32::from(bounds.origin.x);
    let offset_y = f32::from(bounds.origin.y);
    let width = f32::from(bounds.size.width) as i32;
    let height = f32::from(bounds.size.height) as i32;

    unsafe {
        visual
            .visual()
            .SetOffsetX2(offset_x)
            .map_err(|err| anyhow!("initial SetOffsetX2: {err}"))?;
        visual
            .visual()
            .SetOffsetY2(offset_y)
            .map_err(|err| anyhow!("initial SetOffsetY2: {err}"))?;
    }

    let rect = windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };

    let (tx, rx) = oneshot::channel();
    let url = item.url.to_string();
    initialize(
        hwnd,
        visual,
        rect,
        url,
        Box::new(move |result| {
            // Sender is dropped here if the channel is gone; that means the
            // tab was closed before init finished, which is fine.
            let _ = tx.send(result);
        }),
    )
    .context("initialize")?;

    item.init_started = true;
    item.last_bounds = Some(bounds);

    cx.spawn(async move |this, cx| {
        let Ok(result) = rx.await else {
            return;
        };
        let _ = this.update(cx, |item, cx| match result {
            Ok(session) => {
                log::info!("browser_viewer: session ready for {}", item.url);
                item.session = Some(session);
                cx.notify();
            }
            Err(err) => {
                log::error!(
                    "browser_viewer: WebView2 init failed for {}: {err:?}",
                    item.url
                );
                item.init_started = false;
                cx.notify();
            }
        });
    })
    .detach();

    Ok(())
}

/// Open a new browser tab in the active pane of the given workspace.
pub fn open_new_tab(
    workspace: &mut Workspace,
    url: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let view = cx.new(|cx| BrowserView::new(url, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}
