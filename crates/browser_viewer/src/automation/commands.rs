//! Awaitable automation commands for MCP / IPC (return results, not fire-and-forget logs).

use std::sync::Arc;
use std::time::Instant;

use super::action::{DEFAULT_ACTION_TIMEOUT, RETRY_INTERVAL, SessionAttempt};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{AsyncApp, Entity};
use serde_json::Value;

use crate::automation::action::{
    element_for_action, invoke_on_backend_node, try_bounding_rect_backend_node,
    try_click_backend_node, try_focus_backend_node, try_hover_point_backend_node,
    try_scroll_into_view_backend_node, try_select_option_backend_node,
    try_set_checked_backend_node, try_type_backend_node,
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

/// Click an element. `button` is "left"/"right"/"middle"; with the default
/// (left, no double-click, no modifiers) this uses the proven DOM `.click()`
/// path (actionability auto-wait + form.requestSubmit). Any non-default option
/// routes through CDP `Input.dispatchMouseEvent` at the element centre so
/// right-click context menus, double-click, and modifier-clicks behave like a
/// real pointer.
pub async fn click(
    browser: Entity<BrowserView>,
    ref_id: &str,
    button: &str,
    double: bool,
    modifiers: &[String],
    cx: &mut AsyncApp,
) -> Result<()> {
    if button == "left" && !double && modifiers.is_empty() {
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
        return run_with_actionability_wait_result(cx, browser, "click", &ref_id, attempt).await;
    }

    // Coordinate / button / modifier click via CDP mouse events.
    let mods = modifier_mask(modifiers)?;
    let (btn, buttons) = button_codes(button)?;
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    let point_raw = run_one_shot(&browser, cx, &format!("click-point {ref_id}"), move |session, done| {
        try_hover_point_backend_node(session, backend_node_id, done)
    })
    .await?;
    let point = unwrap_cdp_value(point_raw);
    let x = point.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = point.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let clicks = if double { 2 } else { 1 };
    for cc in 1..=clicks {
        dispatch_mouse(&browser, cx, "mousePressed", x, y, btn, buttons, cc, mods).await?;
        dispatch_mouse(&browser, cx, "mouseReleased", x, y, btn, 0, cc, mods).await?;
    }
    Ok(())
}

/// CDP modifier bitmask (Alt=1, Ctrl=2, Meta=4, Shift=8) from modifier names.
fn modifier_mask(modifiers: &[String]) -> Result<i64> {
    let mut mask = 0;
    for m in modifiers {
        mask |= match m.to_ascii_lowercase().as_str() {
            "alt" | "option" => 1,
            "control" | "ctrl" => 2,
            "meta" | "cmd" | "command" => 4,
            "shift" => 8,
            other => return Err(anyhow!("unknown modifier {other:?}")),
        };
    }
    Ok(mask)
}

/// CDP mouse button name + `buttons` bitmask (left=1, right=2, middle=4).
fn button_codes(button: &str) -> Result<(&'static str, i64)> {
    Ok(match button {
        "left" => ("left", 1),
        "right" => ("right", 2),
        "middle" => ("middle", 4),
        other => return Err(anyhow!("unknown button {other:?}")),
    })
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_mouse(
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
    kind: &str,
    x: f64,
    y: f64,
    button: &str,
    buttons: i64,
    click_count: i64,
    modifiers: i64,
) -> Result<()> {
    let params = serde_json::json!({
        "type": kind, "x": x, "y": y, "button": button,
        "buttons": buttons, "clickCount": click_count, "modifiers": modifiers,
    })
    .to_string();
    run_one_shot(browser, cx, "mouse", move |session, done| {
        CdpSession::new(session).call_method("Input.dispatchMouseEvent", &params, done)
    })
    .await?;
    Ok(())
}

pub async fn type_text(
    browser: Entity<BrowserView>,
    ref_id: &str,
    text: &str,
    keep_focus_for_submit: bool,
    slowly: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    if slowly {
        return type_slowly(browser, ref_id, text, cx).await;
    }
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

/// Type character-by-character via real CDP key events (focus, then keyDown/
/// keyUp per char). More faithful than the bulk value-setter for frameworks
/// that key off keystrokes.
async fn type_slowly(
    browser: Entity<BrowserView>,
    ref_id: &str,
    text: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    run_one_shot(&browser, cx, &format!("focus {ref_id}"), move |session, done| {
        try_focus_backend_node(session, backend_node_id, done)
    })
    .await?;

    let chars: Vec<String> = text.chars().map(|c| c.to_string()).collect();
    let dispatched = browser.update(cx, |view, cx| {
        view.with_webview_session(cx, |session| {
            for ch in &chars {
                let (key, code, vk, text_payload) =
                    match crate::automation::keys::parse_key(ch) {
                        Ok(k) => (k.key, k.code, k.windows_virtual_key_code, k.text),
                        // Char the key table doesn't map (e.g. space, unicode):
                        // send it as raw text so the renderer still inserts it.
                        Err(_) => (ch.clone(), String::new(), 0, Some(ch.clone())),
                    };
                let _ = session.dispatch_key_event("keyDown", &key, &code, 0, vk, text_payload.as_deref());
                let _ = session.dispatch_key_event("keyUp", &key, &code, 0, vk, None);
            }
            true
        })
        .unwrap_or(false)
    });
    if !dispatched {
        return Err(anyhow!("browser automation type (slowly) {ref_id}: no live WebView2 session"));
    }
    Ok(())
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

/// Build a `Page.captureScreenshot` clip (page coords + scale) from a measured
/// element rect. Errors if the element has no layout box.
fn clip_from_rect(rect: &Value) -> Result<Value> {
    let num = |k: &str| rect.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let (width, height) = (num("width"), num("height"));
    if width <= 0.0 || height <= 0.0 {
        return Err(anyhow!("element has no layout box to screenshot"));
    }
    Ok(serde_json::json!({
        "x": num("x"),
        "y": num("y"),
        "width": width,
        "height": height,
        "scale": 1.0,
    }))
}

/// CP6: screenshot the page via CDP `Page.captureScreenshot`. Returns
/// `{ data: <base64>, mimeType, bytes }`.
///
/// - `full_page` → `captureBeyondViewport` (whole scrollable page).
/// - `ref_id` → clip to that element's page-coordinate box (implies
///   beyond-viewport so off-screen elements still capture).
/// - `format` is `"png"` or `"jpeg"`; `quality` applies to jpeg only.
pub async fn screenshot(
    browser: Entity<BrowserView>,
    full_page: bool,
    format: String,
    quality: Option<i64>,
    ref_id: Option<String>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let mut params = serde_json::Map::new();
    params.insert("format".into(), Value::from(format.clone()));
    let mut beyond_viewport = full_page;

    if let Some(ref_id) = ref_id {
        let browser_ref = browser.clone();
        let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, &ref_id, app))?;
        let backend_node_id = element.backend_dom_node_id.expect("checked above");
        let rect_raw = run_one_shot(&browser, cx, &format!("rect {ref_id}"), move |session, done| {
            try_bounding_rect_backend_node(session, backend_node_id, done)
        })
        .await?;
        params.insert("clip".into(), clip_from_rect(&unwrap_cdp_value(rect_raw))?);
        // A page-coordinate clip only resolves correctly beyond the viewport.
        beyond_viewport = true;
    }

    params.insert("captureBeyondViewport".into(), Value::Bool(beyond_viewport));
    if format == "jpeg" {
        params.insert("quality".into(), Value::from(quality.unwrap_or(80)));
    }
    let params_str = Value::Object(params).to_string();

    let raw = run_one_shot(&browser, cx, "screenshot", move |session, done| {
        CdpSession::new(session).call_method("Page.captureScreenshot", &params_str, done)
    })
    .await?;

    let data = raw
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))?;
    let mime = if format == "jpeg" {
        "image/jpeg"
    } else {
        "image/png"
    };
    Ok(serde_json::json!({ "data": data, "mimeType": mime, "bytes": data.len() }))
}

/// CP6: evaluate JS in the page. `function` is a JS function expression
/// (`() => …` or `el => …`); with `ref_id` it's called with the element as both
/// `this` and the first argument. Returns `{ result: <json value> }`.
pub async fn evaluate(
    browser: Entity<BrowserView>,
    function: String,
    ref_id: Option<String>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    if let Some(ref_id) = ref_id {
        let browser_ref = browser.clone();
        let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, &ref_id, app))?;
        let backend_node_id = element.backend_dom_node_id.expect("checked above");
        // Bind the element as `this` and pass it as the first arg too, so both
        // `function() { this… }` and `el => el…` styles work.
        let decl = format!("function() {{ return ({function}).call(this, this); }}");
        let raw = run_one_shot(&browser, cx, &format!("evaluate {ref_id}"), move |session, done| {
            invoke_on_backend_node(session, backend_node_id, &decl, None, done)
        })
        .await?;
        return Ok(serde_json::json!({ "result": unwrap_cdp_value(raw) }));
    }

    // Page-level evaluate: run the function and (await any promise it returns).
    let expr = format!("({function})()");
    let params = serde_json::json!({
        "expression": expr,
        "returnByValue": true,
        "awaitPromise": true,
    })
    .to_string();
    // Use the raw CDP string so we can surface `exceptionDetails` (which
    // `parse_cdp_response` would otherwise drop when it extracts `result`).
    let raw = run_one_shot(&browser, cx, "evaluate", move |session, done| {
        session.call_devtools_protocol(
            "Runtime.evaluate",
            &params,
            Box::new(move |s| {
                done(s.and_then(|s| {
                    serde_json::from_str::<Value>(s.trim())
                        .map_err(|e| anyhow!("evaluate response parse: {e}"))
                }));
            }),
        )
    })
    .await?;

    if let Some(details) = raw.get("exceptionDetails") {
        let msg = details
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|v| v.as_str())
            .or_else(|| details.get("text").and_then(|v| v.as_str()))
            .unwrap_or("evaluate threw");
        return Err(anyhow!("{msg}"));
    }
    let result = raw
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(serde_json::json!({ "result": result }))
}

/// CP6: select `<option>`(s) in a `<select>` by value / label / text.
pub async fn select_option(
    browser: Entity<BrowserView>,
    ref_id: &str,
    values: Vec<String>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    let args: Vec<Value> = values.iter().map(|v| Value::from(v.as_str())).collect();
    let ref_label = ref_id.to_string();
    let raw = run_one_shot(&browser, cx, &format!("select {ref_label}"), move |session, done| {
        try_select_option_backend_node(session, backend_node_id, &args, done)
    })
    .await?;
    let result = unwrap_cdp_value(raw);
    if result.get("matched").and_then(|v| v.as_i64()).unwrap_or(0) == 0 {
        return Err(anyhow!("no <option> in {ref_label} matched {values:?}"));
    }
    Ok(result)
}

/// CP6: hover the mouse over an element (scrolls it into view, then dispatches a
/// CDP `mouseMoved` at its centre so CSS `:hover` menus / tooltips trigger).
pub async fn hover(browser: Entity<BrowserView>, ref_id: &str, cx: &mut AsyncApp) -> Result<Value> {
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");

    let point_raw = run_one_shot(&browser, cx, &format!("hover-point {ref_id}"), move |session, done| {
        try_hover_point_backend_node(session, backend_node_id, done)
    })
    .await?;
    let point = unwrap_cdp_value(point_raw);
    let x = point.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = point.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let params = serde_json::json!({ "type": "mouseMoved", "x": x, "y": y, "buttons": 0 }).to_string();
    run_one_shot(&browser, cx, &format!("hover {ref_id}"), move |session, done| {
        CdpSession::new(session).call_method("Input.dispatchMouseEvent", &params, done)
    })
    .await?;
    Ok(serde_json::json!({ "x": x, "y": y }))
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
            text_gone: None,
            timeout: crate::automation::DEFAULT_NAV_TIMEOUT,
        },
        cx,
    )
    .await?;
    Ok(target)
}

/// CP7: navigate back in history (WebView2 `GoBack`), then wait for load.
/// Returns the resulting URL. Errors if there is no back entry.
pub async fn navigate_back(browser: Entity<BrowserView>, cx: &mut AsyncApp) -> Result<String> {
    let went_back = browser.update(cx, |view, cx| view.automation_go_back(cx));
    if !went_back {
        return Err(anyhow!("no back history on this tab"));
    }
    wait_for_result(
        browser.clone(),
        WaitForOptions {
            wait_load: true,
            text: None,
            text_gone: None,
            timeout: crate::automation::DEFAULT_NAV_TIMEOUT,
        },
        cx,
    )
    .await?;
    let url = cx.update(|app| browser.read(app).item().read(app).url().to_string());
    Ok(url)
}

/// A single field for `fill_form`. `kind` is "textbox" (default) / "checkbox" /
/// "radio" / "combobox" / "select"; `value` is the text, option, or boolean.
pub struct FormField {
    pub ref_id: String,
    pub value: String,
    pub kind: Option<String>,
}

/// CP7: fill several fields in one call. Routes each field by kind: checkbox/
/// radio → set checked; combobox/select → select option; else → type text.
pub async fn fill_form(
    browser: Entity<BrowserView>,
    fields: Vec<FormField>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let mut filled = 0u64;
    for field in &fields {
        match field.kind.as_deref() {
            Some("checkbox") | Some("radio") => {
                let checked = !matches!(
                    field.value.to_ascii_lowercase().as_str(),
                    "" | "false" | "0" | "off" | "no" | "unchecked"
                );
                set_checked(browser.clone(), &field.ref_id, checked, cx).await?;
            }
            Some("combobox") | Some("select") => {
                select_option(browser.clone(), &field.ref_id, vec![field.value.clone()], cx).await?;
            }
            _ => {
                type_text(browser.clone(), &field.ref_id, &field.value, false, false, cx).await?;
            }
        }
        filled += 1;
    }
    Ok(serde_json::json!({ "filled": filled }))
}

/// Set a checkbox/radio checked state (used by `fill_form`).
async fn set_checked(
    browser: Entity<BrowserView>,
    ref_id: &str,
    checked: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    let browser_ref = browser.clone();
    let element = cx.update(|app| resolve_ref_on_browser(&browser_ref, ref_id, app))?;
    let backend_node_id = element.backend_dom_node_id.expect("checked above");
    run_one_shot(&browser, cx, &format!("check {ref_id}"), move |session, done| {
        try_set_checked_backend_node(session, backend_node_id, checked, done)
    })
    .await?;
    Ok(())
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

    #[test]
    fn clip_from_rect_builds_scaled_clip() {
        let rect = json!({ "x": 10.0, "y": 20.0, "width": 300.0, "height": 150.0 });
        let clip = clip_from_rect(&rect).unwrap();
        assert_eq!(clip["x"], 10.0);
        assert_eq!(clip["width"], 300.0);
        assert_eq!(clip["scale"], 1.0);
    }

    #[test]
    fn clip_from_rect_rejects_zero_size() {
        let rect = json!({ "x": 0.0, "y": 0.0, "width": 0.0, "height": 0.0 });
        assert!(clip_from_rect(&rect).is_err());
    }
}
