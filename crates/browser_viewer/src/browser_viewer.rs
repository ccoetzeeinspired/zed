//! In-editor embedded browser.
//!
//! Phase 1.A: a new browser tab can be opened via the `browser: new tab`
//! action; the tab hosts a WebView2 view inside Zed's window via
//! DirectComposition. See `plans/browser-viewer.md`.
//!
//! Gated to Windows; on other platforms `init` is a no-op so the workspace
//! cross-compiles cleanly.

use gpui::{App, actions};
use settings::Settings as _;
use std::rc::Rc;
use ui::SharedString;
use workspace::Workspace;

pub mod agent_cursor;
pub mod browser_protocol;
pub mod browser_settings;
pub mod browser_view;
pub mod bundle;
pub mod design;
pub mod drawing;

#[cfg(target_os = "windows")]
mod design_mode_script;
#[cfg(target_os = "windows")]
mod webview2_host;

pub use browser_settings::BrowserSettings;
pub use browser_view::{BrowserItem, BrowserView, open_new_tab};

actions!(
    browser,
    [
        /// Open a new browser tab navigating to the configured homepage.
        NewTab,
        /// Open WebView2 DevTools on the active browser tab.
        OpenDevTools,
        /// Focus the address bar (Ctrl+L convention from real browsers).
        FocusAddressBar,
        /// Toggle design mode on the active browser tab.
        ToggleDesignMode,
        /// Toggle drawing mode — freehand strokes over the page.
        ToggleDrawingMode,
        /// Clear all drawn strokes on the active browser tab.
        ClearDrawing,
        /// Preview the current design-mode selection as an agent click target.
        PreviewSelectedElement,
        /// Click the currently previewed agent cursor target.
        ClickPreviewedElement,
        /// Clear the visible agent cursor target.
        ClearAgentCursor
    ]
);

/// Register the browser-viewer feature with the application.
pub fn init(cx: &mut App) {
    BrowserSettings::register(cx);
    #[cfg(target_os = "windows")]
    {
        cx.observe_new(|workspace: &mut Workspace, window, cx| {
            workspace.register_action(|workspace, _: &NewTab, window, cx| {
                let homepage = BrowserSettings::get_global(cx).homepage.clone();
                open_new_tab(workspace, SharedString::new(homepage), window, cx);
            });
            let Some(window) = window else {
                return;
            };
            let workspace_handle = cx.entity().downgrade();
            let window_handle = window.window_handle();
            workspace::browser_agent::register_browser_opener(Rc::new(move |url, cx| {
                let result = window_handle
                    .update(cx, |_, window, cx| {
                        workspace_handle.update(cx, |workspace, cx| {
                            open_new_tab(workspace, SharedString::new(url.clone()), window, cx);
                            serde_json::json!({
                                "opened": true,
                                "url": url,
                                "surface": "zed-embedded-browser"
                            })
                        })
                    })
                    .map_err(|err| anyhow::anyhow!(err))
                    .and_then(|result| result);
                gpui::Task::ready(result)
            }));
        })
        .detach();
    }
    #[cfg(not(target_os = "windows"))]
    {
        log::info!("browser_viewer: skipping init (not supported on this platform)");
    }
}
