//! Phase 0 spike: a temporary action that pops up an embedded WebView2
//! inside Zed's main window via DirectComposition. Not the production
//! entry point; replaced in Phase 1 by a real `ProjectItem`-registered
//! browser tab.

use std::cell::RefCell;

use gpui::{App, actions};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, RECT};
use workspace::Workspace;

use crate::webview2_host::{WebView2Session, initialize};

actions!(
    browser,
    [
        /// Phase 0 spike: open example.com inside Zed's main window at a
        /// fixed rect via DirectComposition. Not a production action —
        /// removed in Phase 1.
        OpenSpikeUrl
    ]
);

const SPIKE_URL: &str = "https://example.com";
const SPIKE_RECT: RECT = RECT {
    left: 0,
    top: 0,
    right: 800,
    bottom: 600,
};
const SPIKE_OFFSET_X: f32 = 100.0;
const SPIKE_OFFSET_Y: f32 = 100.0;

thread_local! {
    /// Holds the WebView2 sessions created by the spike action. STA-bound;
    /// dropped when the GPUI UI thread ends (Zed shutdown).
    static ALIVE_SESSIONS: RefCell<Vec<WebView2Session>> =
        RefCell::new(Vec::new());
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_workspace, _: &OpenSpikeUrl, window, _cx| {
            if let Err(err) = open_spike(window) {
                log::error!("browser_viewer spike failed: {err:?}");
            }
        });
    })
    .detach();
}

fn open_spike(window: &mut gpui::Window) -> anyhow::Result<()> {
    let handle = window
        .window_handle()
        .map_err(|err| anyhow::anyhow!("could not get window handle: {err}"))?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        anyhow::bail!("expected Win32 window handle");
    };
    let hwnd = HWND(win32.hwnd.get() as *mut std::ffi::c_void);

    log::info!(
        "browser_viewer spike: attaching WebView2 to HWND {:?}",
        hwnd.0
    );

    // Carve out a DComp visual under Zed's render tree at our target offset.
    let visual = gpui_windows::create_child_visual_for_hwnd(hwnd)?;
    unsafe {
        visual
            .visual()
            .SetOffsetX2(SPIKE_OFFSET_X)
            .map_err(|err| anyhow::anyhow!("SetOffsetX2: {err}"))?;
        visual
            .visual()
            .SetOffsetY2(SPIKE_OFFSET_Y)
            .map_err(|err| anyhow::anyhow!("SetOffsetY2: {err}"))?;
    }

    initialize(
        hwnd,
        visual,
        SPIKE_RECT,
        SPIKE_URL.to_string(),
        Box::new(move |result| match result {
            Ok(session) => {
                log::info!(
                    "browser_viewer spike: WebView2 ready, navigated to {SPIKE_URL}"
                );
                ALIVE_SESSIONS.with(|s| s.borrow_mut().push(session));
            }
            Err(err) => {
                log::error!("browser_viewer spike: initialization failed: {err:?}");
            }
        }),
    )?;

    log::info!("browser_viewer spike: WebView2 init dispatched");
    Ok(())
}
