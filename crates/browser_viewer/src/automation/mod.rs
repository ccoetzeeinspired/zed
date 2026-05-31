//! Playwright-shaped browser automation for the embedded WebView2 tab.
//!
//! Agent tools reach this layer via the `zed-browser-mcp` adapter (CP5).
//! All actions use CDP / DOM — never Sikuli-style coordinate clicking.
//!
//! See `plans/browser-automation.md`.

mod action;
mod cdp;
mod commands;
mod ipc;
mod keys;
mod navigate;
mod session;
mod snapshot;
mod tabs;
mod target;

pub use action::{element_for_action, try_click_backend_node, try_type_backend_node};
pub use cdp::{CdpSession, parse_cdp_response};
pub use ipc::{DEFAULT_IPC_PORT, init as init_automation_ipc};
pub use navigate::{WaitForOptions, normalize_navigate_url, wait_for, DEFAULT_NAV_TIMEOUT};
pub use session::{AutomationSessionState, ElementRef, RefRegistry};
pub use snapshot::{PageSnapshot, snapshot_from_ax_tree};
pub use target::{
    resolve_automation_target, resolve_automation_target_global,
    resolve_automation_workspace_global,
};

use std::sync::Arc;
use std::time::Instant;

use action::{DEFAULT_ACTION_TIMEOUT, RETRY_INTERVAL, SessionAttempt};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{App, Entity};

use crate::browser_view::BrowserView;

/// Run the CP0 smoke test: `Accessibility.enable` on the given browser tab.
pub fn run_smoke_test(
    browser: Entity<BrowserView>,
    _window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    let (tx, rx) = oneshot::channel::<Result<()>>();
    let mut tx_slot = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_webview_session(cx, |session| {
            let cdp = CdpSession::new(session);
            cdp.enable_accessibility(Box::new(move |result| {
                if let Some(tx) = tx_slot.take() {
                    let _ = tx.send(result);
                }
            }))
            .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "No live WebView2 session on the target browser tab. Wait for the page to finish loading."
        ));
    }

    cx.spawn(async move |_| {
        match rx.await {
            Ok(Ok(())) => log::info!("browser automation: Accessibility.enable OK"),
            Ok(Err(err)) => log::error!("browser automation smoke test failed: {err:#}"),
            Err(_) => log::warn!("browser automation smoke test channel dropped"),
        }
    })
    .detach();

    Ok(())
}

/// CP1: capture an accessibility snapshot and refresh the ref registry.
pub fn run_snapshot(
    browser: Entity<BrowserView>,
    _window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    let page_generation = browser.read(cx).item().read(cx).automation_page_generation();
    let (tx, rx) = oneshot::channel::<Result<PageSnapshot>>();
    let mut tx_slot = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_webview_session(cx, |session| {
            let cdp = CdpSession::new(session);
            cdp.fetch_full_ax_tree(Box::new(move |tree_result| {
                if let Some(tx) = tx_slot.take() {
                    let snapshot = tree_result.and_then(|tree| {
                        snapshot_from_ax_tree(tree, page_generation)
                    });
                    let _ = tx.send(snapshot);
                }
            }))
            .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "No live WebView2 session on the target browser tab. Wait for the page to finish loading."
        ));
    }

    let browser = browser.clone();
    cx.spawn(async move |cx| {
        match rx.await {
            Ok(Ok(snapshot)) => {
                browser.update(cx, |view, cx| {
                    view.store_automation_snapshot(cx, snapshot);
                });
            }
            Ok(Err(err)) => log::error!("browser automation snapshot failed: {err:#}"),
            Err(_) => log::warn!("browser automation snapshot channel dropped"),
        }
    })
    .detach();

    Ok(())
}

fn resolve_ref_on_browser(
    browser: &Entity<BrowserView>,
    ref_id: &str,
    cx: &App,
) -> Result<ElementRef> {
    let element = browser
        .read(cx)
        .item()
        .read(cx)
        .resolve_automation_ref(ref_id)
        .ok_or_else(|| {
            anyhow!(
                "ref {ref_id} not found or stale — run browser: automation snapshot first"
            )
        })?;
    element_for_action(element)
}

async fn run_with_actionability_wait(
    cx: &mut gpui::AsyncApp,
    browser: Entity<BrowserView>,
    label: &str,
    ref_id: &str,
    attempt: SessionAttempt,
) {
    let deadline = Instant::now() + DEFAULT_ACTION_TIMEOUT;
    let mut last_error = String::from("action timed out");

    loop {
        let (tx, rx) = oneshot::channel::<Result<()>>();
        let mut tx_slot = Some(tx);
        let attempt = attempt.clone();
        let kicked_off = browser.update(cx, |view, cx| {
            view.with_webview_session(cx, |session| {
                attempt(session, Box::new(move |result| {
                    if let Some(tx) = tx_slot.take() {
                        let _ = tx.send(result);
                    }
                }))
                .is_ok()
            })
            .unwrap_or(false)
        });

        if !kicked_off {
            log::error!(
                "browser automation {label} on {ref_id}: no live WebView2 session"
            );
            return;
        }

        match rx.await {
            Ok(Ok(())) => {
                log::info!("browser automation {label} on {ref_id} OK");
                return;
            }
            Ok(Err(err)) => {
                last_error = err.to_string();
                if Instant::now() >= deadline {
                    log::error!(
                        "browser automation {label} on {ref_id} failed after {:?}: {last_error}",
                        DEFAULT_ACTION_TIMEOUT
                    );
                    return;
                }
                cx.background_executor().timer(RETRY_INTERVAL).await;
            }
            Err(_) => {
                log::warn!("browser automation {label} on {ref_id}: channel dropped");
                return;
            }
        }
    }
}

/// CP2: click the element identified by snapshot ref (`eN`).
pub fn run_click(
    browser: Entity<BrowserView>,
    ref_id: &str,
    _window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    let element = resolve_ref_on_browser(&browser, ref_id, cx)?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    let ref_id = ref_id.to_string();
    let browser = browser.clone();
    let attempt = Arc::new(
        move |session: &crate::webview2_host::WebView2Session,
              on_done: Box<dyn FnOnce(Result<()>) + 'static>| {
            try_click_backend_node(session, backend_node_id, on_done)
        },
    );

    cx.spawn(async move |cx| {
        run_with_actionability_wait(cx, browser, "click", &ref_id, attempt).await;
    })
    .detach();

    Ok(())
}

/// CP3: type text into the element identified by snapshot ref (`eN`).
pub fn run_type(
    browser: Entity<BrowserView>,
    ref_id: &str,
    text: &str,
    _window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    let element = resolve_ref_on_browser(&browser, ref_id, cx)?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    let ref_id = ref_id.to_string();
    let text = text.to_string();
    let browser = browser.clone();
    let attempt = Arc::new(
        move |session: &crate::webview2_host::WebView2Session,
              on_done: Box<dyn FnOnce(Result<()>) + 'static>| {
            try_type_backend_node(session, backend_node_id, &text, false, on_done)
        },
    );

    cx.spawn(async move |cx| {
        run_with_actionability_wait(cx, browser, "type", &ref_id, attempt).await;
    })
    .detach();

    Ok(())
}

/// Dev helper: ref from `ZED_BROWSER_AUTOMATION_REF`, default `e21` (Sign In).
pub fn dev_automation_ref() -> String {
    std::env::var("ZED_BROWSER_AUTOMATION_REF").unwrap_or_else(|_| "e21".to_string())
}

/// Dev helper: type ref default `e14` (email field on dogfood login page).
pub fn dev_automation_type_ref() -> String {
    std::env::var("ZED_BROWSER_AUTOMATION_TYPE_REF").unwrap_or_else(|_| "e14".to_string())
}

/// Dev helper: text from `ZED_BROWSER_AUTOMATION_TEXT`, default sample email.
pub fn dev_automation_text() -> String {
    std::env::var("ZED_BROWSER_AUTOMATION_TEXT").unwrap_or_else(|_| String::new())
}

/// TrueLens login page refs from a typical snapshot (re-snapshot if navigation changes refs).
pub const DEV_EMAIL_REF: &str = "e14";
pub const DEV_PASSWORD_REF: &str = "e18";
pub const DEV_SIGN_IN_REF: &str = "e21";

/// CP4: navigate the active browser tab to `url`.
pub fn run_navigate(
    browser: Entity<BrowserView>,
    url: &str,
    _window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    let target = normalize_navigate_url(url)?;
    browser.update(cx, |view, cx| {
        view.automation_navigate(target.clone(), cx);
    });
    log::info!("browser automation navigate → {target}");
    Ok(())
}

/// CP4: wait for load and/or page text on the active browser tab.
pub fn run_wait_for(
    browser: Entity<BrowserView>,
    options: WaitForOptions,
    _window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    let browser = browser.clone();
    cx.spawn(async move |cx| {
        wait_for(browser, options, cx).await;
    })
    .detach();
    Ok(())
}

/// Dev: navigate to `browser.homepage` from settings.
pub fn run_navigate_homepage(
    browser: Entity<BrowserView>,
    homepage: &str,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Result<()> {
    run_navigate(browser, homepage, window, cx)
}
