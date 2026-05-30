//! Awaitable automation commands for MCP / IPC (return results, not fire-and-forget logs).

use std::sync::Arc;
use std::time::Instant;

use super::action::{DEFAULT_ACTION_TIMEOUT, RETRY_INTERVAL, SessionAttempt};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{AsyncApp, Entity};

use crate::automation::action::{element_for_action, try_click_backend_node, try_type_backend_node};
use crate::automation::cdp::CdpSession;
use crate::automation::navigate::{WaitForOptions, normalize_navigate_url, wait_for_result};
use crate::automation::snapshot::{PageSnapshot, snapshot_from_ax_tree};
use crate::browser_view::BrowserView;

fn resolve_ref_on_browser(
    browser: &Entity<BrowserView>,
    ref_id: &str,
    cx: &gpui::App,
) -> Result<crate::automation::ElementRef> {
    let element = browser
        .read(cx)
        .item()
        .read(cx)
        .resolve_automation_ref(ref_id)
        .ok_or_else(|| {
            anyhow!(
                "ref {ref_id} not found or stale — run browser_snapshot first"
            )
        })?;
    element_for_action(element)
}

async fn run_with_actionability_wait_result(
    cx: &mut AsyncApp,
    browser: Entity<BrowserView>,
    label: &str,
    ref_id: &str,
    attempt: SessionAttempt,
) -> Result<()> {
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
            return Err(anyhow!(
                "browser automation {label} on {ref_id}: no live WebView2 session"
            ));
        }

        match rx.await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => {
                last_error = err.to_string();
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "browser automation {label} on {ref_id} failed after {:?}: {last_error}",
                        DEFAULT_ACTION_TIMEOUT
                    ));
                }
                cx.background_executor().timer(RETRY_INTERVAL).await;
            }
            Err(_) => {
                return Err(anyhow!(
                    "browser automation {label} on {ref_id}: channel dropped"
                ));
            }
        }
    }
}

pub async fn snapshot(browser: Entity<BrowserView>, cx: &mut AsyncApp) -> Result<PageSnapshot> {
    let browser_for_gen = browser.clone();
    let page_generation = cx.update(|app| {
        browser_for_gen
            .read(app)
            .item()
            .read(app)
            .automation_page_generation()
    });
    let (tx, rx) = oneshot::channel::<Result<PageSnapshot>>();
    let mut tx_slot = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_webview_session(cx, |session| {
            let cdp = CdpSession::new(session);
            cdp.fetch_full_ax_tree(Box::new(move |tree_result| {
                if let Some(tx) = tx_slot.take() {
                    let snapshot = tree_result
                        .and_then(|tree| snapshot_from_ax_tree(tree, page_generation));
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

    let snapshot = rx
        .await
        .map_err(|_| anyhow!("browser automation snapshot channel dropped"))??;

    browser.update(cx, |view, cx| {
        view.store_automation_snapshot(cx, snapshot.clone());
    });

    Ok(snapshot)
}

pub async fn click(
    browser: Entity<BrowserView>,
    ref_id: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    let ref_id = ref_id.to_string();
    let attempt = Arc::new(
        move |session: &crate::webview2_host::WebView2Session,
              on_done: Box<dyn FnOnce(Result<()>) + 'static>| {
            try_click_backend_node(session, backend_node_id, on_done)
        },
    );
    run_with_actionability_wait_result(cx, browser, "click", &ref_id, attempt).await
}

pub async fn type_text(
    browser: Entity<BrowserView>,
    ref_id: &str,
    text: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    let ref_id = ref_id.to_string();
    let text = text.to_string();
    let attempt = Arc::new(
        move |session: &crate::webview2_host::WebView2Session,
              on_done: Box<dyn FnOnce(Result<()>) + 'static>| {
            try_type_backend_node(session, backend_node_id, &text, on_done)
        },
    );
    run_with_actionability_wait_result(cx, browser, "type", &ref_id, attempt).await
}

pub async fn navigate(
    browser: Entity<BrowserView>,
    url: &str,
    cx: &mut AsyncApp,
) -> Result<String> {
    let target = normalize_navigate_url(url)?;
    browser.update(cx, |view, cx| {
        view.automation_navigate(target.clone(), cx);
    });
    wait_for_result(
        browser.clone(),
        WaitForOptions {
            wait_load: true,
            text: None,
            timeout: crate::automation::DEFAULT_NAV_TIMEOUT,
        },
        cx,
    )
    .await?;
    Ok(target)
}

pub async fn wait_for(
    browser: Entity<BrowserView>,
    options: WaitForOptions,
    cx: &mut AsyncApp,
) -> Result<()> {
    wait_for_result(browser, options, cx).await
}
