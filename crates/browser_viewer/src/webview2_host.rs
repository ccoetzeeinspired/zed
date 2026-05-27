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
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DocumentTitleChangedEventHandler,
    HistoryChangedEventHandler,
    Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_MOUSE_EVENT_KIND, COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS,
        CreateCoreWebView2Environment, ICoreWebView2, ICoreWebView2CompositionController,
        ICoreWebView2Controller, ICoreWebView2Environment3,
    },
    NavigationCompletedEventHandler, NavigationStartingEventHandler, SourceChangedEventHandler,
    take_pwstr,
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
    HistoryChanged { can_go_back: bool, can_go_forward: bool },
    /// Top-level navigation kicked off.
    NavigationStarting,
    /// Top-level navigation finished, successfully or not.
    NavigationCompleted { is_success: bool },
}

/// Tokens returned by `add_*` handlers, kept so we can `remove_*` on drop.
#[derive(Default)]
struct EventTokens {
    title: Option<i64>,
    source: Option<i64>,
    history: Option<i64>,
    navigation_starting: Option<i64>,
    navigation_completed: Option<i64>,
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
    let tx = events_tx;
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

    Ok(tokens)
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
    let env_handler =
        CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, env| {
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

            let ctrl_handler =
                CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(
                    move |result, comp_ctrl| {
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

                            visual.commit().map_err(|err| anyhow!("DComp commit: {err}"))?;

                            let webview = unsafe {
                                controller
                                    .CoreWebView2()
                                    .map_err(|err| anyhow!("CoreWebView2: {err}"))?
                            };

                            // Wire navigation events before the first navigate
                            // so even the initial load fires title/source events.
                            let event_tokens =
                                register_navigation_events(&webview, events_tx)?;

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
                    },
                ));

            unsafe {
                if let Err(err) =
                    env3.CreateCoreWebView2CompositionController(parent, &ctrl_handler)
                {
                    log::error!("CreateCoreWebView2CompositionController failed: {err}");
                }
            }
            Ok(())
        }));

    unsafe {
        CreateCoreWebView2Environment(&env_handler)
            .map_err(|err| anyhow!("CreateCoreWebView2Environment dispatch failed: {err}"))?;
    }
    Ok(())
}
