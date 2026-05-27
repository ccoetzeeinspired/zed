//! In-editor embedded browser, Phase 0 spike.
//!
//! Phase 0 goal: prove that WebView2 can be created and embedded inside Zed's
//! main window via a child HWND. Subsequent phases replace the child HWND with
//! a DirectComposition-hosted visual, add real tab/project-item integration,
//! input bridging, and design mode. See `plans/browser-viewer.md`.
//!
//! The whole crate is gated to Windows; on other platforms `init` is a no-op
//! stub so the workspace cross-compiles cleanly.

use gpui::App;

#[cfg(target_os = "windows")]
mod webview2_host;

#[cfg(target_os = "windows")]
mod spike;

/// Register the browser-viewer feature with the application.
///
/// On Windows this wires up the `browser: open spike URL` action for the
/// Phase 0 acceptance test; on other platforms this is a no-op.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "windows")]
    {
        spike::init(cx);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = cx;
        log::info!("browser_viewer: skipping init (not supported on this platform)");
    }
}
