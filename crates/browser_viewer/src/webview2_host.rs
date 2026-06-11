//! WebView2 in composition mode, driven entirely through async callbacks
//! (no blocking `wait_for_async_operation`, which re-enters GPUI's message
//! loop and triggers `RefCell already borrowed` violations).
//!
//! Flow:
//!
//! 1. `initialize(parent, visual, url, on_done)` registers a
//!    `CreateCoreWebView2EnvironmentCompletedHandler` and dispatches
//!    `CreateCoreWebView2Environment`, then returns immediately.
//! 2. When the env handler fires (on the Win32 message loop), we cast the
//!    environment to `ICoreWebView2Environment3` and call
//!    `CreateCoreWebView2CompositionController`, again returning
//!    immediately after registering the next handler.
//! 3. When the controller handler fires, we set the visual target, bounds,
//!    and visibility on the controller, navigate to the URL, and hand the
//!    finished controller back via `on_done`.
//!
//! `on_done` runs on the GPUI UI thread (same thread that initiated the
//! chain) so it can safely park the controller in a thread-local for
//! lifetime management.

use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use gpui_windows::HostedVisual;
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler,
    CallDevToolsProtocolMethodCompletedHandler, CapturePreviewCompletedHandler,
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DocumentTitleChangedEventHandler,
    HistoryChangedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, COREWEBVIEW2_MOUSE_EVENT_KIND,
        COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS, CreateCoreWebView2Environment, ICoreWebView2,
        ICoreWebView2CompositionController, ICoreWebView2Controller, ICoreWebView2Environment3,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler, SourceChangedEventHandler,
    WebMessageReceivedEventHandler, take_pwstr,
};
use windows::{
    Win32::Foundation::{HWND, POINT, RECT},
    core::{HSTRING, Interface, PCWSTR, PWSTR},
};

/// What the caller gets when initialization completes successfully. The
/// composition controller drives input + visual target binding; the regular
/// controller (same COM object cast to a different interface) exposes
/// `SetBounds`, `SetIsVisible`, and `CoreWebView2()`.
/// One observable change on the WebView, delivered to the host via the
/// `events_tx` passed to [`initialize`].
#[derive(Debug, Clone)]
pub(crate) enum NavigationEvent {
    /// Page `<title>` changed, including via JS `document.title = ...`.
    TitleChanged(String),
    /// The displayed URL changed — happens on navigation and on
    /// `pushState`/`replaceState` from SPAs.
    SourceChanged(String),
    /// Either `CanGoBack` or `CanGoForward` changed.
    HistoryChanged {
        can_go_back: bool,
        can_go_forward: bool,
    },
    /// Top-level navigation kicked off.
    NavigationStarting,
    /// Top-level navigation finished, successfully or not.
    NavigationCompleted { is_success: bool },
    /// Phase 4: a Design-mode JSON message arrived from the injected
    /// script. The raw JSON string is passed through unparsed — the
    /// BrowserItem layer owns the shape so this module doesn't need
    /// to know the protocol.
    DesignModeMessage(String),
}

/// Tokens returned by `add_*` handlers, kept so we can `remove_*` on drop.
#[derive(Default)]
struct EventTokens {
    title: Option<i64>,
    source: Option<i64>,
    history: Option<i64>,
    navigation_starting: Option<i64>,
    navigation_completed: Option<i64>,
    web_message: Option<i64>,
}

pub(crate) struct WebView2Session {
    pub composition_controller: ICoreWebView2CompositionController,
    pub controller: ICoreWebView2Controller,
    /// The WebView interface — exposes navigation methods (GoBack,
    /// GoForward, Reload, Navigate, ExecuteScript) and event registration.
    pub webview: ICoreWebView2,
    /// Held so the visual stays attached to the DComp tree for the lifetime
    /// of the session. Dropping the session removes the visual.
    visual: HostedVisual,
    /// Event-handler tokens; removed on drop before `controller.Close()`.
    event_tokens: EventTokens,
}

impl WebView2Session {
    /// Position and size the WebView atomically. `x`/`y` are in DIPs from
    /// the parent HWND's client-area origin. This updates three things:
    ///
    /// 1. The DComp visual's offset (where the page is actually rendered).
    /// 2. The controller's `Bounds` to a rect at the same position. The
    ///    bounds rect's *size* defines the WebView2 viewport; its
    ///    *position* is what popup menus and dialogs use to anchor
    ///    themselves to the host window. Setting it equal to the visual's
    ///    offset keeps right-click menus, autofill popups, etc. aligned
    ///    with the content.
    /// 3. Calls `NotifyParentWindowPositionChanged` so WebView2
    ///    recomputes screen-space positions for any open popups.
    ///
    /// All changes are committed to the DComp tree before returning.
    pub fn set_rect(&self, x: f32, y: f32, width: i32, height: i32) -> Result<()> {
        unsafe {
            self.visual
                .visual()
                .SetOffsetX2(x)
                .map_err(|err| anyhow!("SetOffsetX2: {err}"))?;
            self.visual
                .visual()
                .SetOffsetY2(y)
                .map_err(|err| anyhow!("SetOffsetY2: {err}"))?;
        }
        let rect = windows::Win32::Foundation::RECT {
            left: x as i32,
            top: y as i32,
            right: x as i32 + width,
            bottom: y as i32 + height,
        };
        unsafe {
            self.controller
                .SetBounds(rect)
                .map_err(|err| anyhow!("controller.SetBounds: {err}"))?;
            self.controller
                .NotifyParentWindowPositionChanged()
                .map_err(|err| anyhow!("NotifyParentWindowPositionChanged: {err}"))?;
        }
        self.visual.commit()?;
        Ok(())
    }

    /// Forward a mouse event into the WebView. Coordinates are in browser-
    /// local space (origin at the visual's top-left). `mouse_data` carries
    /// the wheel delta for wheel events (signed WHEEL_DELTA units), 0
    /// otherwise.
    /// Phase 4.E: capture a PNG screenshot of the page via
    /// `CapturePreview`. The PNG bytes land in `on_done` on the GPUI
    /// foreground thread. Implemented over `IStream` backed by
    /// `HGLOBAL` (Windows-managed memory buffer): we allocate the
    /// stream, hand it to WebView2, and on completion `Seek` it back
    /// to start + `Read` the full payload.
    pub fn capture_preview_png(
        &self,
        on_done: Box<dyn FnOnce(Result<Vec<u8>>) + 'static>,
    ) -> Result<()> {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
        use windows::Win32::System::Com::{IStream, STREAM_SEEK_SET};
        // Stream takes ownership of the HGLOBAL — pass null + true
        // for `fdeleteonrelease` so Windows frees it when the stream
        // refcount hits zero.
        let stream: IStream = unsafe {
            CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true)
                .map_err(|e| anyhow!("CreateStreamOnHGlobal: {e}"))?
        };
        let stream_for_handler = stream.clone();
        let mut done_slot: Option<Box<dyn FnOnce(Result<Vec<u8>>) + 'static>> = Some(on_done);
        let handler = CapturePreviewCompletedHandler::create(Box::new(move |hr| {
            let on_done = done_slot
                .take()
                .expect("CapturePreviewCompletedHandler called twice");
            if let Err(err) = hr {
                on_done(Err(anyhow!("CapturePreview failed: {err}")));
                return Ok(());
            }
            let result = read_stream_to_vec(&stream_for_handler);
            on_done(result);
            Ok(())
        }));
        unsafe {
            self.webview
                .CapturePreview(
                    COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG,
                    &stream,
                    &handler,
                )
                .map_err(|err| anyhow!("CapturePreview: {err}"))?;
        }
        // Pre-seek not needed before CapturePreview; the seek happens
        // in the handler before reading.
        let _ = STREAM_SEEK_SET;
        Ok(())
    }

    /// Post a string message to the page via `PostWebMessageAsString`.
    /// The design-mode script listens for `"activate"`, `"deactivate"`,
    /// and `"clear_selection"`.
    pub fn post_message_string(&self, msg: &str) -> Result<()> {
        let msg_h = HSTRING::from(msg);
        unsafe {
            self.webview
                .PostWebMessageAsString(PCWSTR(msg_h.as_ptr()))
                .map_err(|err| anyhow!("PostWebMessageAsString: {err}"))?;
        }
        Ok(())
    }

    /// Send a `Input.dispatchKeyEvent` over the WebView2 CDP channel.
    /// Phase 3 uses CDP for keyboard because
    /// `ICoreWebView2CompositionController` has no public
    /// `SendKeyboardInput` equivalent (verified across every released
    /// SDK including 1.0.4015-prerelease). CDP dispatches at the
    /// renderer level so it bypasses Win32 focus entirely — pages
    /// receive the key event regardless of which HWND currently has
    /// Win32 focus, which is exactly what we need given gpui_windows'
    /// `translate_accelerator` keeps Zed's HWND as the focus owner.
    ///
    /// `event_type` is one of `"keyDown"`, `"keyUp"`, `"char"`,
    /// `"rawKeyDown"`. `text` is set on keyDown for printable keys so
    /// the renderer fires `input` events on form controls — non-text
    /// keys pass `None`.
    ///
    /// Invoke an arbitrary Chrome DevTools Protocol method and deliver the
    /// raw JSON response string to `on_done` on the GPUI thread (same
    /// contract as [`Self::capture_preview_png`]).
    pub fn call_devtools_protocol(
        &self,
        method: &str,
        params_json: &str,
        on_done: Box<dyn FnOnce(Result<String>) + 'static>,
    ) -> Result<()> {
        let method_h = HSTRING::from(method);
        let params_h = HSTRING::from(params_json);
        let mut done_slot: Option<Box<dyn FnOnce(Result<String>) + 'static>> = Some(on_done);
        let handler =
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |hr, result_json| {
                let on_done = done_slot
                    .take()
                    .expect("CallDevToolsProtocolMethodCompletedHandler called twice");
                if let Err(err) = hr {
                    on_done(Err(anyhow!("CDP call failed: {err}")));
                    return Ok(());
                }
                on_done(Ok(result_json));
                Ok(())
            }));
        unsafe {
            self.webview
                .CallDevToolsProtocolMethod(
                    PCWSTR(method_h.as_ptr()),
                    PCWSTR(params_h.as_ptr()),
                    &handler,
                )
                .map_err(|err| anyhow!("CallDevToolsProtocolMethod: {err}"))?;
        }
        Ok(())
    }

    /// The completion handler is a no-op; we fire-and-forget. Any CDP
    /// error returns asynchronously and only matters for diagnosis.
    pub fn dispatch_key_event(
        &self,
        event_type: &str,
        key: &str,
        code: &str,
        modifiers: i32,
        windows_virtual_key_code: i32,
        text: Option<&str>,
    ) -> Result<()> {
        let params = build_dispatch_key_event_json(
            event_type,
            key,
            code,
            modifiers,
            windows_virtual_key_code,
            text,
        );
        let method = HSTRING::from("Input.dispatchKeyEvent");
        let params_h = HSTRING::from(params);
        let handler =
            CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_hr, _result| Ok(())));
        unsafe {
            self.webview
                .CallDevToolsProtocolMethod(
                    PCWSTR(method.as_ptr()),
                    PCWSTR(params_h.as_ptr()),
                    &handler,
                )
                .map_err(|err| anyhow!("CallDevToolsProtocolMethod: {err}"))?;
        }
        Ok(())
    }

    /// Reorder this session's WebView2 underlay to the front of the
    /// underlay group, so it occludes other browser tabs' underlays
    /// sharing the same pane. Called when the tab becomes active (or when
    /// a freshly-opened tab's session becomes ready). We reorder rather
    /// than toggling `SetIsVisible`, because hiding/showing the controller
    /// caused a one-frame desktop flash on reactivation.
    pub fn bring_underlay_to_front(&self) -> Result<()> {
        self.visual.bring_underlay_to_front()
    }

    pub fn send_mouse_input(
        &self,
        kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
        virtual_keys: COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
        mouse_data: u32,
        x: i32,
        y: i32,
    ) -> Result<()> {
        unsafe {
            self.composition_controller
                .SendMouseInput(kind, virtual_keys, mouse_data, POINT { x, y })
                .map_err(|err| anyhow!("SendMouseInput: {err}"))?;
        }
        Ok(())
    }
}

impl crate::automation::platform::AutomationSession for WebView2Session {
    fn platform(&self) -> crate::automation::platform::BrowserPlatform {
        crate::automation::platform::BrowserPlatform::WebView2
    }

    fn call_cdp(
        &self,
        method: &str,
        params_json: &str,
        on_done: crate::automation::platform::JsonCallback,
    ) -> Result<()> {
        self.call_devtools_protocol(
            method,
            params_json,
            Box::new(move |result| {
                on_done(result.and_then(crate::automation::cdp::parse_cdp_response));
            }),
        )
    }

    fn evaluate_expression(
        &self,
        expression: &str,
        await_promise: bool,
        on_done: crate::automation::platform::JsonCallback,
    ) -> Result<()> {
        let params = serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": await_promise,
        })
        .to_string();
        self.call_devtools_protocol(
            "Runtime.evaluate",
            &params,
            Box::new(move |result| {
                on_done(result.and_then(crate::automation::cdp::parse_cdp_response));
            }),
        )
    }

    fn invoke_on_element(
        &self,
        handle: crate::automation::session::ElementHandle,
        function_declaration: &str,
        arguments: Option<&[serde_json::Value]>,
        on_done: crate::automation::platform::JsonCallback,
    ) -> Result<()> {
        match handle {
            crate::automation::session::ElementHandle::CdpBackendNodeId(id) => {
                crate::automation::action::invoke_on_backend_node(
                    self,
                    id,
                    function_declaration,
                    arguments,
                    on_done,
                )
            }
            other => Err(crate::automation::platform::unsupported_capability(
                crate::automation::platform::BrowserPlatform::WebView2,
                &format!("element handle {other:?}"),
            )),
        }
    }

    fn dispatch_key_event(
        &self,
        event: crate::automation::platform::KeyDispatch,
        on_done: crate::automation::platform::UnitCallback,
    ) -> Result<()> {
        let modifiers = i32::try_from(event.modifiers)
            .map_err(|_| anyhow!("key modifiers out of range: {}", event.modifiers))?;
        let windows_virtual_key_code =
            i32::try_from(event.windows_virtual_key_code).map_err(|_| {
                anyhow!(
                    "windows virtual key code out of range: {}",
                    event.windows_virtual_key_code
                )
            })?;
        let result = WebView2Session::dispatch_key_event(
            self,
            &event.event_type,
            &event.key,
            &event.code,
            modifiers,
            windows_virtual_key_code,
            event.text.as_deref(),
        );
        on_done(result);
        Ok(())
    }

    fn dispatch_mouse_event(
        &self,
        event: crate::automation::platform::MouseDispatch,
        on_done: crate::automation::platform::JsonCallback,
    ) -> Result<()> {
        let params = serde_json::json!({
            "type": event.event_type,
            "x": event.x,
            "y": event.y,
            "button": event.button,
            "buttons": event.buttons,
            "clickCount": event.click_count,
            "modifiers": event.modifiers,
        })
        .to_string();
        self.call_devtools_protocol(
            "Input.dispatchMouseEvent",
            &params,
            Box::new(move |result| {
                on_done(result.and_then(crate::automation::cdp::parse_cdp_response));
            }),
        )
    }
}

impl Drop for WebView2Session {
    fn drop(&mut self) {
        unsafe {
            // Remove event handlers before closing so the underlying closures
            // (which hold a clone of the events channel sender) get dropped,
            // closing the channel and signalling the drain task to exit.
            if let Some(t) = self.event_tokens.title.take() {
                let _ = self.webview.remove_DocumentTitleChanged(t);
            }
            if let Some(t) = self.event_tokens.source.take() {
                let _ = self.webview.remove_SourceChanged(t);
            }
            if let Some(t) = self.event_tokens.history.take() {
                let _ = self.webview.remove_HistoryChanged(t);
            }
            if let Some(t) = self.event_tokens.navigation_starting.take() {
                let _ = self.webview.remove_NavigationStarting(t);
            }
            if let Some(t) = self.event_tokens.navigation_completed.take() {
                let _ = self.webview.remove_NavigationCompleted(t);
            }
            if let Some(t) = self.event_tokens.web_message.take() {
                let _ = self.webview.remove_WebMessageReceived(t);
            }
            if let Err(err) = self.controller.Close() {
                log::warn!("WebView2Session: controller.Close() failed: {err}");
            }
        }
    }
}

/// Register the five navigation-state events on `webview`, each pushing into
/// `events_tx`. Returns tokens for removal on drop.
fn register_navigation_events(
    webview: &ICoreWebView2,
    events_tx: mpsc::UnboundedSender<NavigationEvent>,
) -> Result<EventTokens> {
    let mut tokens = EventTokens::default();

    // --- Title --------------------------------------------------------
    let tx = events_tx.clone();
    let handler = DocumentTitleChangedEventHandler::create(Box::new(move |sender, _| {
        if let Some(webview) = sender {
            let mut title = PWSTR::null();
            unsafe {
                if webview.DocumentTitle(&mut title).is_ok() && !title.is_null() {
                    let title = take_pwstr(title);
                    let _ = tx.unbounded_send(NavigationEvent::TitleChanged(title));
                }
            }
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        webview
            .add_DocumentTitleChanged(&handler, &mut token)
            .map_err(|e| anyhow!("add_DocumentTitleChanged: {e}"))?;
    }
    tokens.title = Some(token);

    // --- Source (URL) -------------------------------------------------
    let tx = events_tx.clone();
    let handler = SourceChangedEventHandler::create(Box::new(move |sender, _| {
        if let Some(webview) = sender {
            let mut uri = PWSTR::null();
            unsafe {
                if webview.Source(&mut uri).is_ok() && !uri.is_null() {
                    let uri = take_pwstr(uri);
                    let _ = tx.unbounded_send(NavigationEvent::SourceChanged(uri));
                }
            }
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        webview
            .add_SourceChanged(&handler, &mut token)
            .map_err(|e| anyhow!("add_SourceChanged: {e}"))?;
    }
    tokens.source = Some(token);

    // --- History (can_go_back / can_go_forward) -----------------------
    let tx = events_tx.clone();
    let handler = HistoryChangedEventHandler::create(Box::new(move |sender, _| {
        if let Some(webview) = sender {
            let mut back = windows::core::BOOL(0);
            let mut fwd = windows::core::BOOL(0);
            unsafe {
                let _ = webview.CanGoBack(&mut back);
                let _ = webview.CanGoForward(&mut fwd);
            }
            let _ = tx.unbounded_send(NavigationEvent::HistoryChanged {
                can_go_back: back.as_bool(),
                can_go_forward: fwd.as_bool(),
            });
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        webview
            .add_HistoryChanged(&handler, &mut token)
            .map_err(|e| anyhow!("add_HistoryChanged: {e}"))?;
    }
    tokens.history = Some(token);

    // --- Navigation starting -----------------------------------------
    let tx = events_tx.clone();
    let handler = NavigationStartingEventHandler::create(Box::new(move |_, _| {
        let _ = tx.unbounded_send(NavigationEvent::NavigationStarting);
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        webview
            .add_NavigationStarting(&handler, &mut token)
            .map_err(|e| anyhow!("add_NavigationStarting: {e}"))?;
    }
    tokens.navigation_starting = Some(token);

    // --- Navigation completed ----------------------------------------
    let tx = events_tx.clone();
    let handler = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
        let is_success = args
            .as_ref()
            .and_then(|a| {
                let mut success = windows::core::BOOL(0);
                unsafe { a.IsSuccess(&mut success).ok() }?;
                Some(success.as_bool())
            })
            .unwrap_or(false);
        let _ = tx.unbounded_send(NavigationEvent::NavigationCompleted { is_success });
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        webview
            .add_NavigationCompleted(&handler, &mut token)
            .map_err(|e| anyhow!("add_NavigationCompleted: {e}"))?;
    }
    tokens.navigation_completed = Some(token);

    // --- Design-mode JS messages (Phase 4) ----------------------------
    // The injected `design_mode_script` posts JSON strings via
    // `chrome.webview.postMessage`. We pull the raw string out via
    // `TryGetWebMessageAsString` (works for any string the page sends,
    // including JSON.stringify output) and forward it to the host
    // unparsed — BrowserItem owns the protocol shape.
    let tx = events_tx;
    let handler = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
        if let Some(args) = args {
            let mut msg = PWSTR::null();
            unsafe {
                if args.TryGetWebMessageAsString(&mut msg).is_ok() && !msg.is_null() {
                    let s = take_pwstr(msg);
                    let _ = tx.unbounded_send(NavigationEvent::DesignModeMessage(s));
                }
            }
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        webview
            .add_WebMessageReceived(&handler, &mut token)
            .map_err(|e| anyhow!("add_WebMessageReceived: {e}"))?;
    }
    tokens.web_message = Some(token);

    Ok(tokens)
}

/// Pull every byte out of an `IStream` into a `Vec<u8>`. Seeks to the
/// start first (CapturePreview leaves the cursor at end-of-data), then
/// reads in 64KB chunks until `Read` reports zero bytes.
fn read_stream_to_vec(stream: &windows::Win32::System::Com::IStream) -> Result<Vec<u8>> {
    use windows::Win32::System::Com::STREAM_SEEK_SET;
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut buf = vec![0u8; 64 * 1024];
    unsafe {
        stream
            .Seek(0, STREAM_SEEK_SET, None)
            .map_err(|e| anyhow!("IStream.Seek: {e}"))?;
        loop {
            let mut bytes_read: u32 = 0;
            // IStream::Read returns S_OK or S_FALSE (at EOF). Both
            // pass through `ok()`; we only stop when bytes_read is 0.
            stream
                .Read(
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    Some(&mut bytes_read),
                )
                .ok()
                .map_err(|e| anyhow!("IStream.Read: {e}"))?;
            if bytes_read == 0 {
                break;
            }
            out.extend_from_slice(&buf[..bytes_read as usize]);
        }
    }
    Ok(out)
}

/// Build the CDP `Input.dispatchKeyEvent` JSON payload by hand to avoid
/// pulling `serde_json` for one call site. Strings are minimally escaped
/// (`"` and `\`) — the inputs are short, ASCII, controlled by us; we
/// don't need full JSON-escape coverage.
fn build_dispatch_key_event_json(
    event_type: &str,
    key: &str,
    code: &str,
    modifiers: i32,
    windows_virtual_key_code: i32,
    text: Option<&str>,
) -> String {
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    }

    let mut json = String::with_capacity(160);
    json.push_str("{\"type\":\"");
    json.push_str(event_type);
    json.push_str("\",\"key\":\"");
    json.push_str(&esc(key));
    json.push_str("\",\"code\":\"");
    json.push_str(&esc(code));
    json.push_str("\",\"modifiers\":");
    json.push_str(&modifiers.to_string());
    json.push_str(",\"windowsVirtualKeyCode\":");
    json.push_str(&windows_virtual_key_code.to_string());
    if let Some(t) = text {
        json.push_str(",\"text\":\"");
        json.push_str(&esc(t));
        json.push_str("\",\"unmodifiedText\":\"");
        json.push_str(&esc(t));
        json.push('"');
    }
    json.push('}');
    json
}

/// Initialize WebView2 against `parent` (for input parenting/IME), targeting
/// `visual` for rendering. Sets the controller's bounds to `bounds`,
/// navigates to `url`, then calls `on_done` with the constructed session.
///
/// Returns immediately after dispatching the first async COM call. `on_done`
/// fires later via the Win32 message loop.
pub(crate) fn initialize(
    parent: HWND,
    visual: HostedVisual,
    bounds: RECT,
    url: String,
    events_tx: mpsc::UnboundedSender<NavigationEvent>,
    on_done: Box<dyn FnOnce(Result<WebView2Session>) + 'static>,
) -> Result<()> {
    // The env handler runs after `CreateCoreWebView2Environment` resolves.
    // It owns the visual + parent + bounds + url + on_done, threading them
    // forward into the controller handler.
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result, env| {
            if let Err(err) = result {
                on_done(Err(anyhow!("WebView2 env creation failed: {err}")));
                return Ok(());
            }
            let env = match env {
                Some(env) => env,
                None => {
                    on_done(Err(anyhow!("WebView2 env was null")));
                    return Ok(());
                }
            };

            // The composition-controller API lives on
            // ICoreWebView2Environment3, not the base environment.
            let env3 = match env.cast::<ICoreWebView2Environment3>() {
                Ok(e) => e,
                Err(err) => {
                    on_done(Err(anyhow!(
                        "WebView2: env doesn't implement ICoreWebView2Environment3: {err}"
                    )));
                    return Ok(());
                }
            };

            let ctrl_handler = CreateCoreWebView2CompositionControllerCompletedHandler::create(
                Box::new(move |result, comp_ctrl| {
                    if let Err(err) = result {
                        on_done(Err(anyhow!(
                            "WebView2 composition controller creation failed: {err}"
                        )));
                        return Ok(());
                    }
                    let comp_ctrl = match comp_ctrl {
                        Some(c) => c,
                        None => {
                            on_done(Err(anyhow!("WebView2 composition controller was null")));
                            return Ok(());
                        }
                    };

                    let outcome = (|| -> Result<WebView2Session> {
                        // The same COM object also implements
                        // ICoreWebView2Controller, which is what carries
                        // SetBounds / SetIsVisible / CoreWebView2.
                        let controller: ICoreWebView2Controller =
                            comp_ctrl.cast().map_err(|err| {
                                anyhow!("cast to ICoreWebView2Controller failed: {err}")
                            })?;

                        unsafe {
                            comp_ctrl
                                .SetRootVisualTarget(visual.visual())
                                .map_err(|err| anyhow!("SetRootVisualTarget: {err}"))?;
                            controller
                                .SetBounds(bounds)
                                .map_err(|err| anyhow!("SetBounds: {err}"))?;
                            controller
                                .SetIsVisible(true)
                                .map_err(|err| anyhow!("SetIsVisible: {err}"))?;
                        }

                        visual
                            .commit()
                            .map_err(|err| anyhow!("DComp commit: {err}"))?;

                        let webview = unsafe {
                            controller
                                .CoreWebView2()
                                .map_err(|err| anyhow!("CoreWebView2: {err}"))?
                        };

                        // Wire navigation events before the first navigate
                        // so even the initial load fires title/source events.
                        let event_tokens = register_navigation_events(&webview, events_tx)?;

                        // Phase 4: inject the design-mode script before
                        // the first navigation. `AddScriptToExecuteOnDocumentCreated`
                        // runs the script on every navigation, including
                        // the initial one. The script is idempotent —
                        // re-evaluating it on the same document just emits
                        // a `ready` message and exits.
                        let script_h = HSTRING::from(crate::design_mode_script::SCRIPT);
                        let install_handler =
                            AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
                                |_hr, _id| Ok(()),
                            ));
                        unsafe {
                            if let Err(err) = webview.AddScriptToExecuteOnDocumentCreated(
                                PCWSTR(script_h.as_ptr()),
                                &install_handler,
                            ) {
                                log::warn!(
                                    "browser_viewer: AddScriptToExecuteOnDocumentCreated failed: {err}"
                                );
                            }
                        }

                        // CP10: inject the console/network instrumentation
                        // at document-start too, so browser_console_messages
                        // / browser_network_requests capture from page load.
                        let instr_h = HSTRING::from(crate::automation::instrumentation::SCRIPT);
                        let instr_handler =
                            AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(
                                |_hr, _id| Ok(()),
                            ));
                        unsafe {
                            if let Err(err) = webview.AddScriptToExecuteOnDocumentCreated(
                                PCWSTR(instr_h.as_ptr()),
                                &instr_handler,
                            ) {
                                log::warn!(
                                    "browser_viewer: automation instrumentation inject failed: {err}"
                                );
                            }
                        }

                        let url_h = HSTRING::from(&url);
                        unsafe {
                            webview
                                .Navigate(PCWSTR(url_h.as_ptr()))
                                .map_err(|err| anyhow!("Navigate: {err}"))?;
                        }

                        Ok(WebView2Session {
                            composition_controller: comp_ctrl,
                            controller,
                            webview,
                            visual,
                            event_tokens,
                        })
                    })();

                    on_done(outcome);
                    Ok(())
                }),
            );

            unsafe {
                if let Err(err) =
                    env3.CreateCoreWebView2CompositionController(parent, &ctrl_handler)
                {
                    log::error!("CreateCoreWebView2CompositionController failed: {err}");
                }
            }
            Ok(())
        },
    ));

    unsafe {
        CreateCoreWebView2Environment(&env_handler)
            .map_err(|err| anyhow!("CreateCoreWebView2Environment dispatch failed: {err}"))?;
    }
    Ok(())
}
