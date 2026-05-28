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
use ui::SharedString;
use workspace::Workspace;

pub mod browser_settings;
pub mod browser_view;

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
        OpenDevTools
    ]
);

/// Register the browser-viewer feature with the application.
pub fn init(cx: &mut App) {
    BrowserSettings::register(cx);
    #[cfg(target_os = "windows")]
    {
        cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
            workspace.register_action(|workspace, _: &NewTab, window, cx| {
                let homepage = BrowserSettings::get_global(cx).homepage.clone();
                open_new_tab(workspace, SharedString::new(homepage), window, cx);
            });
        })
        .detach();
    }
    #[cfg(not(target_os = "windows"))]
    {
        log::info!("browser_viewer: skipping init (not supported on this platform)");
    }
}
