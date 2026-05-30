//! Awaitable automation commands for MCP / IPC (return results, not fire-and-forget logs).

use std::sync::Arc;
use std::time::Instant;

use super::action::{DEFAULT_ACTION_TIMEOUT, RETRY_INTERVAL, SessionAttempt};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{AsyncApp, Entity};
use serde_json::Value;

use crate::automation::action::{
    element_for_action, try_click_backend_node, try_scroll_into_view_backend_node,
    try_type_backend_node,
};
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
    keep_focus_for_submit: bool,
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
            try_type_backend_node(session, backend_node_id, &text, keep_focus_for_submit, on_done)
        },
    );
    run_with_actionability_wait_result(cx, browser, "type", &ref_id, attempt).await
}

/// CP6: press a key (Playwright-style spec) on the focused element / page.
///
/// Fire-and-forget via CDP `Input.dispatchKeyEvent` (keyDown + keyUp), the
/// same path the human-typing handler uses — no actionability retry, since
/// there's no target ref to resolve. The caller should ensure the intended
/// element is focused first (e.g. `browser_click` it, or `browser_type` which
/// focuses on its way in).
pub async fn press_key(
    browser: Entity<BrowserView>,
    key: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let press = crate::automation::keys::parse_key(key)?;
    let dispatched = browser.update(cx, |view, cx| {
        view.with_webview_session(cx, |session| {
            // keyDown carries the text payload (so form controls fire `input`);
            // keyUp clears it so the renderer doesn't double-fire.
            let down = session.dispatch_key_event(
                "keyDown",
                &press.key,
                &press.code,
                press.modifiers,
                press.windows_virtual_key_code,
                press.text.as_deref(),
            );
            let up = session.dispatch_key_event(
                "keyUp",
                &press.key,
                &press.code,
                press.modifiers,
                press.windows_virtual_key_code,
                None,
            );
            down.and(up).is_ok()
        })
        .unwrap_or(false)
    });
    if !dispatched {
        return Err(anyhow!(
            "browser automation press_key {key:?}: no live WebView2 session"
        ));
    }
    Ok(())
}

/// CP6: scroll the page. With `ref_id`, scroll that element into view (handles
/// inner scroll containers); otherwise scroll the viewport by `(dx, dy)` pixels
/// (positive dy = down, positive dx = right). Returns `{ x, y, maxY }`.
pub async fn scroll(
    browser: Entity<BrowserView>,
    ref_id: Option<String>,
    dx: f64,
    dy: f64,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let raw = if let Some(ref_id) = ref_id {
        let browser_ref = browser.clone();
        let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, &ref_id, app))?;
        let backend_node_id = element.backend_dom_node_id.expect("checked above");
        run_one_shot(&browser, cx, &format!("scroll-into-view {ref_id}"), move |session, done| {
            try_scroll_into_view_backend_node(session, backend_node_id, done)
        })
        .await?
    } else {
        // `behavior:'instant'` so the position read-back below is synchronous
        // even if the page sets `scroll-behavior: smooth`.
        let expr = format!(
            "(()=>{{window.scrollBy({{left:{dx},top:{dy},behavior:'instant'}});\
             const e=document.scrollingElement||document.documentElement;\
             return {{x:window.scrollX,y:window.scrollY,maxY:Math.max(0,e.scrollHeight-e.clientHeight)}};}})()"
        );
        run_one_shot(&browser, cx, "scroll", move |session, done| {
            CdpSession::new(session).evaluate_expression(&expr, done)
        })
        .await?
    };
    Ok(unwrap_cdp_value(raw))
}

/// Run a single CDP call that yields a `Value`, awaiting the result. The
/// closure kicks the call off inside the live WebView2 session.
async fn run_one_shot<F>(
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
    label: &str,
    kick: F,
) -> Result<Value>
where
    F: FnOnce(&crate::webview2_host::WebView2Session, Box<dyn FnOnce(Result<Value>) + 'static>) -> Result<()>
        + 'static,
{
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx_slot = Some(tx);
    let mut kick = Some(kick);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_webview_session(cx, |session| {
            let kick = kick.take().expect("kick called once");
            kick(
                session,
                Box::new(move |res| {
                    if let Some(tx) = tx_slot.take() {
                        let _ = tx.send(res);
                    }
                }),
            )
            .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!("browser automation {label}: no live WebView2 session"));
    }
    rx.await
        .map_err(|_| anyhow!("browser automation {label} channel dropped"))?
}

/// CDP `returnByValue` wraps results as `{ type, value }`. Unwrap to the inner
/// value when present.
fn unwrap_cdp_value(v: Value) -> Value {
    v.get("value").cloned().unwrap_or(v)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unwrap_cdp_value_extracts_inner_value() {
        let wrapped = json!({ "type": "object", "value": { "x": 0, "y": 800, "maxY": 4200 } });
        let inner = unwrap_cdp_value(wrapped);
        assert_eq!(inner["y"], 800);
        assert_eq!(inner["maxY"], 4200);
        assert!(inner.get("type").is_none());
    }

    #[test]
    fn unwrap_cdp_value_passes_through_unwrapped() {
        let plain = json!({ "x": 1, "y": 2 });
        assert_eq!(unwrap_cdp_value(plain.clone()), plain);
    }
}
