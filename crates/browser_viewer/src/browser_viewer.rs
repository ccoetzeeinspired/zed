//! In-editor embedded browser.
//!
//! Phase 1.A: a new browser tab can be opened via the `browser: new tab`
//! action; the tab hosts a WebView2 view inside Zed's window via
//! DirectComposition. See `plans/browser-viewer.md`.
//!
//! Gated to Windows; on other platforms `init` is a no-op so the workspace
//! cross-compiles cleanly.

use gpui::{App, actions};
use ui::SharedString;
use workspace::Workspace;

pub mod browser_view;

#[cfg(target_os = "windows")]
mod webview2_host;

pub use browser_view::{BrowserItem, BrowserView, open_new_tab};

actions!(
    browser,
    [
        /// Open a new browser tab navigating to the default URL.
        NewTab
    ]
);

/// Default URL the `NewTab` action opens. Real configurable homepage lands
/// in Phase 1.B settings; for now this proves end-to-end navigation.
const DEFAULT_NEW_TAB_URL: &str = "https://example.com";

/// Register the browser-viewer feature with the application.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "windows")]
    {
        cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
            workspace.register_action(
                |workspace, _: &NewTab, window, cx| {
                    open_new_tab(workspace, SharedString::new(DEFAULT_NEW_TAB_URL), window, cx);
                },
            );
        })
        .detach();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cx;
        log::info!("browser_viewer: skipping init (not supported on this platform)");
    }
}
