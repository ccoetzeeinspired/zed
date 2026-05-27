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
use gpui_windows::HostedVisual;
use webview2_com::{
    CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
    Microsoft::Web::WebView2::Win32::{
        CreateCoreWebView2Environment, ICoreWebView2CompositionController,
        ICoreWebView2Controller, ICoreWebView2Environment3,
    },
};
use windows::{
    Win32::Foundation::{HWND, RECT},
    core::{HSTRING, Interface, PCWSTR},
};

/// What the caller gets when initialization completes successfully. The
/// composition controller drives input + visual target binding; the regular
/// controller (same COM object cast to a different interface) exposes
/// `SetBounds`, `SetIsVisible`, and `CoreWebView2()`.
pub(crate) struct WebView2Session {
    // Held alive for COM ref-counting; later phases will use it for input
    // dispatch (SendMouseInput, SendKeyEvent) and design-mode JS injection.
    #[allow(dead_code)]
    pub composition_controller: ICoreWebView2CompositionController,
    pub controller: ICoreWebView2Controller,
    /// Held so the visual stays attached to the DComp tree for the lifetime
    /// of the session. Dropping the session removes the visual.
    visual: HostedVisual,
}

impl WebView2Session {
    /// Move the WebView's visual to the given DIP offset from the parent
    /// window's client-area origin. Caller must subsequently call
    /// [`commit`](Self::commit) for the change to become visible.
    pub fn set_position(&self, offset_x: f32, offset_y: f32) -> Result<()> {
        unsafe {
            self.visual
                .visual()
                .SetOffsetX2(offset_x)
                .map_err(|err| anyhow!("SetOffsetX2: {err}"))?;
            self.visual
                .visual()
                .SetOffsetY2(offset_y)
                .map_err(|err| anyhow!("SetOffsetY2: {err}"))?;
        }
        Ok(())
    }

    /// Resize the WebView's rendering viewport. Coordinates are in the
    /// visual's local space (origin at the visual's offset).
    pub fn set_size(&self, width: i32, height: i32) -> Result<()> {
        let rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe {
            self.controller
                .SetBounds(rect)
                .map_err(|err| anyhow!("controller.SetBounds: {err}"))?;
        }
        Ok(())
    }

    /// Commit pending visual transform changes to the DComp tree.
    pub fn commit(&self) -> Result<()> {
        self.visual.commit()
    }
}

impl Drop for WebView2Session {
    fn drop(&mut self) {
        unsafe {
            if let Err(err) = self.controller.Close() {
                log::warn!("WebView2Session: controller.Close() failed: {err}");
            }
        }
    }
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
                            let url_h = HSTRING::from(&url);
                            unsafe {
                                webview
                                    .Navigate(PCWSTR(url_h.as_ptr()))
                                    .map_err(|err| anyhow!("Navigate: {err}"))?;
                            }

                            Ok(WebView2Session {
                                composition_controller: comp_ctrl,
                                controller,
                                visual,
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
