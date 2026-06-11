//! Loopback IPC bridge between `zed-browser-mcp` and macOS WKWebView tabs.
//!
//! This intentionally exposes the same transport as Windows while only enabling
//! commands that have native WKWebView support in this slice.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::{mpsc as async_mpsc, oneshot};
use gpui::{App, AsyncApp, Entity, Global, SharedString, px};
use serde::Deserialize;
use serde_json::{Value, json};
use settings::Settings as _;

use crate::BrowserSettings;
use crate::automation::recorder::{self, DurableLoc, FormFieldRec, RecordedAction, Target};
use crate::automation::session::DurableSelector;
use crate::automation::target::{
    resolve_automation_target_global, resolve_automation_workspace_global,
};
use crate::automation::{
    ElementHandle, macos_click_script, macos_console_messages_script, macos_drop_script,
    macos_element_center_script, macos_element_evaluate_script, macos_element_value_script,
    macos_element_visible_script, macos_handle_dialog_script, macos_hover_script,
    macos_list_visible_script, macos_mouse_click_script, macos_mouse_drag_script,
    macos_mouse_event_script, macos_mouse_wheel_script, macos_network_request_script,
    macos_network_requests_script, macos_page_contains_text_script, macos_page_state_script,
    macos_press_key_script, macos_scroll_by_script, macos_scroll_into_view_script,
    macos_select_option_script, macos_set_checked_script, macos_set_storage_state_script,
    macos_snapshot_script, macos_storage_clear_script, macos_storage_delete_script,
    macos_storage_get_script, macos_storage_list_script, macos_storage_set_script,
    macos_storage_state_script, macos_type_script, snapshot_from_ax_tree, tabs,
};
use crate::browser_view::BrowserView;

/// Default loopback port for MCP ↔ Zed automation.
pub const DEFAULT_IPC_PORT: u16 = 19382;

const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const NO_ACTIVE_BROWSER_MESSAGE: &str =
    "No active Zed browser tab. Open an embedded browser tab first.";

#[derive(Debug, Deserialize)]
struct IpcRequest {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

struct PendingIpcRequest {
    request: IpcRequest,
    response_tx: std::sync::mpsc::SyncSender<String>,
}

#[derive(Debug, Clone)]
struct MacosFormField {
    ref_id: String,
    value: String,
    kind: Option<String>,
}

struct GlobalBrowserAutomationIpc(async_mpsc::UnboundedSender<PendingIpcRequest>);

impl Global for GlobalBrowserAutomationIpc {}

fn ipc_port() -> u16 {
    std::env::var("ZED_BROWSER_AUTOMATION_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_IPC_PORT)
}

fn ipc_ok_response(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({ "id": id, "ok": true, "result": result }))
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"response serialization failed\"}".into())
}

fn ipc_error_response(id: Value, error: &str) -> String {
    serde_json::to_string(&json!({ "id": id, "ok": false, "error": error }))
        .unwrap_or_else(|_| format!("{{\"ok\":false,\"error\":{error:?}}}"))
}

/// Start the automation IPC listener (loopback TCP + GPUI dispatch loop).
pub fn init(cx: &mut App) {
    let (ipc_tx, mut ipc_rx) = async_mpsc::unbounded::<PendingIpcRequest>();
    cx.set_global(GlobalBrowserAutomationIpc(ipc_tx.clone()));

    thread::Builder::new()
        .name("BrowserAutomationIpc".into())
        .spawn(move || tcp_server_loop(ipc_tx))
        .ok();

    cx.spawn(async move |mut cx| {
        while let Some(pending) = ipc_rx.next().await {
            let id = pending.request.id.clone();
            let response_line = match dispatch_request(pending.request, &mut cx).await {
                Ok(result) => ipc_ok_response(id, result),
                Err(err) => ipc_error_response(id, &err.to_string()),
            };
            let _ = pending.response_tx.send(response_line);
        }
    })
    .detach();
}

fn tcp_server_loop(ipc_tx: async_mpsc::UnboundedSender<PendingIpcRequest>) {
    let addr = format!("127.0.0.1:{}", ipc_port());
    let listener = loop {
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                log::info!("browser automation IPC listening on {addr}");
                break listener;
            }
            Err(err) => {
                log::error!("browser automation IPC bind {addr} failed: {err}; retrying in 2s");
                thread::sleep(Duration::from_secs(2));
            }
        }
    };

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let ipc_tx = ipc_tx.clone();
                thread::Builder::new()
                    .name("BrowserAutomationIpcClient".into())
                    .spawn(move || {
                        if let Err(err) = handle_tcp_client(stream, ipc_tx) {
                            log::error!("browser automation IPC client: {err:#}");
                        }
                    })
                    .ok();
            }
            Err(err) => log::error!("browser automation IPC accept failed: {err}"),
        }
    }
}

fn handle_tcp_client(
    stream: TcpStream,
    ipc_tx: async_mpsc::UnboundedSender<PendingIpcRequest>,
) -> Result<()> {
    stream.set_read_timeout(Some(IPC_REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(IPC_REQUEST_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Err(anyhow!("empty IPC request"));
    }

    let request: IpcRequest = serde_json::from_str(line)?;
    let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
    ipc_tx
        .unbounded_send(PendingIpcRequest {
            request,
            response_tx,
        })
        .map_err(|_| anyhow!("browser automation IPC dispatcher stopped"))?;

    let response = response_rx
        .recv_timeout(IPC_REQUEST_TIMEOUT)
        .map_err(|_| anyhow!("browser automation IPC timed out after {IPC_REQUEST_TIMEOUT:?}"))?;
    writer.write_all(response.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

async fn dispatch_request(request: IpcRequest, cx: &mut AsyncApp) -> Result<Value> {
    log::info!(
        "browser automation IPC: {} {:?}",
        request.method,
        request.params
    );

    if request.method.as_str() == "codegen" {
        let (script, storage_state) = recorder::codegen();
        return Ok(json!({ "script": script, "storageState": storage_state }));
    }

    if request.method.as_str() == "record" {
        let browser = active_browser(cx)?;
        return dispatch_record(&request.params, &browser, cx).await;
    }

    let pending = if recorder::is_recording() {
        match active_browser(cx) {
            Ok(browser) => {
                capture_pending_action(request.method.as_str(), &request.params, &browser, cx).await
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let browser_for_commit = active_browser(cx).ok();

    let result = match request.method.as_str() {
        "ping" => Ok(json!({ "status": "ok" })),
        "tabs" => dispatch_tabs(request.params, cx).await,
        "close" => {
            let (window, workspace) = cx
                .update(|app| resolve_automation_workspace_global(app))
                .ok_or_else(|| anyhow!("No Zed workspace window found"))?;
            tabs::close_active(workspace, window, cx).await
        }
        "navigate" => {
            let url = navigate_url_from_params(&request.params)?;
            let browser = active_browser(cx)?;
            let previous_url =
                cx.update(|app| browser.read(app).item().read(app).url().to_string());
            let final_url = url.clone();
            cx.update(|app| {
                browser.update(app, |view, cx| {
                    view.automation_navigate(url, cx);
                });
            });
            wait_for_navigation_macos(browser.clone(), previous_url, cx).await?;
            let observed_url =
                evaluate_expression_on_browser(&browser, "(() => location.href)()".to_string(), cx)
                    .await
                    .ok()
                    .map(cdp_result_value)
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or(final_url);
            Ok(json!({ "url": observed_url }))
        }
        "navigate_back" => dispatch_navigate_back(cx).await,
        "snapshot" => dispatch_snapshot(cx).await,
        "click" => dispatch_click(request.params, cx).await,
        "hover" => dispatch_hover(request.params, cx).await,
        "drag" => dispatch_drag(request.params, cx).await,
        "drop" => dispatch_drop(request.params, cx).await,
        "type" => dispatch_type(request.params, cx).await,
        "select_option" => dispatch_select_option(request.params, cx).await,
        "fill_form" => dispatch_fill_form(request.params, cx).await,
        "press_key" => dispatch_press_key(request.params, cx).await,
        "scroll" => dispatch_scroll(request.params, cx).await,
        "mouse_move_xy" => dispatch_mouse_move_xy(request.params, cx).await,
        "mouse_click_xy" => dispatch_mouse_click_xy(request.params, cx).await,
        "mouse_down" => dispatch_mouse_button(request.params, true, cx).await,
        "mouse_up" => dispatch_mouse_button(request.params, false, cx).await,
        "mouse_drag_xy" => dispatch_mouse_drag_xy(request.params, cx).await,
        "mouse_wheel" => dispatch_mouse_wheel(request.params, cx).await,
        "screenshot" => dispatch_screenshot(request.params, cx).await,
        "evaluate" => dispatch_evaluate(request.params, cx).await,
        "storage_list" => dispatch_storage_list(request.params, cx).await,
        "storage_get" => dispatch_storage_get(request.params, cx).await,
        "storage_set" => dispatch_storage_set(request.params, cx).await,
        "storage_delete" => dispatch_storage_delete(request.params, cx).await,
        "storage_clear" => dispatch_storage_clear(request.params, cx).await,
        "storage_state" => dispatch_storage_state(cx).await,
        "set_storage_state" => dispatch_set_storage_state(request.params, cx).await,
        "handle_dialog" => dispatch_handle_dialog(request.params, cx).await,
        "console_messages" => dispatch_console_messages(request.params, cx).await,
        "network_requests" => dispatch_network_requests(request.params, cx).await,
        "network_request" => dispatch_network_request(request.params, cx).await,
        "cookie_list" => dispatch_cookie_list(cx).await,
        "cookie_get" => dispatch_cookie_get(request.params, cx).await,
        "cookie_set" => dispatch_cookie_set(request.params, cx).await,
        "cookie_delete" => dispatch_cookie_delete(request.params, cx).await,
        "cookie_clear" => dispatch_cookie_clear(cx).await,
        "file_upload" => unsupported_native_gap(
            "file_upload",
            "requires native WKUIDelegate file input handling",
        ),
        "resize" => dispatch_resize(request.params, cx).await,
        "pdf_save" => dispatch_pdf_save(request.params, cx).await,
        "verify_text_visible" => dispatch_verify_text_visible(request.params, cx).await,
        "verify_element_visible" => dispatch_verify_element_visible(request.params, cx).await,
        "verify_list_visible" => dispatch_verify_list_visible(request.params, cx).await,
        "verify_value" => dispatch_verify_value(request.params, cx).await,
        "wait_for" => dispatch_wait_for(request.params, cx).await,
        other => Err(anyhow!(
            "browser automation method {other:?} is not implemented for macOS WKWebView yet"
        )),
    };

    if result.is_ok() {
        if should_release_browser_keyboard_focus(request.method.as_str())
            && let Some(browser) = browser_for_commit.as_ref()
        {
            release_browser_keyboard_focus(browser, cx);
        }

        if let (Some(action), Some(browser)) = (pending, browser_for_commit) {
            let url = result
                .as_ref()
                .ok()
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    cx.update(|app| browser.read(app).item().read(app).url().to_string())
                });
            recorder::push(action, url);
        }
    }

    result
}

fn should_release_browser_keyboard_focus(method: &str) -> bool {
    matches!(
        method,
        "click"
            | "type"
            | "select_option"
            | "fill_form"
            | "press_key"
            | "mouse_click_xy"
            | "mouse_down"
            | "mouse_up"
            | "mouse_drag_xy"
    )
}

fn release_browser_keyboard_focus(browser: &Entity<BrowserView>, cx: &mut AsyncApp) {
    cx.update(|app| {
        browser.update(app, |view, app| {
            view.with_wkwebview_session(app, |session| {
                session.release_keyboard_focus();
            });
        });
    });
}

async fn dispatch_snapshot(cx: &mut AsyncApp) -> Result<Value> {
    let browser = active_browser(cx)?;
    let page_generation = browser.update(cx, |view, cx| {
        view.item().read(cx).automation_page_generation()
    });

    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_automation_session(cx, |session| {
            session
                .evaluate_expression(
                    macos_snapshot_script(),
                    true,
                    Box::new(move |result| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(result);
                        }
                    }),
                )
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation snapshot: no live WKWebView session"
        ));
    }

    let raw = rx
        .await
        .map_err(|_| anyhow!("browser automation snapshot channel dropped"))??;
    let tree = cdp_result_value(raw);
    let snapshot = snapshot_from_ax_tree(tree, page_generation)?;
    browser.update(cx, |view, cx| {
        view.store_automation_snapshot(cx, snapshot.clone());
    });
    Ok(json!({
        "yaml": snapshot.yaml,
        "ref_count": snapshot.ref_count,
    }))
}

async fn dispatch_navigate_back(cx: &mut AsyncApp) -> Result<Value> {
    let browser = active_browser(cx)?;
    let went_back = browser.update(cx, |view, cx| view.automation_go_back(cx));
    if !went_back {
        return Err(anyhow!("no back history on this tab"));
    }
    wait_for_macos(browser.clone(), true, None, None, cx).await?;
    let raw =
        evaluate_expression_on_browser(&browser, "(() => location.href)()".to_string(), cx).await?;
    let url = cdp_result_value(raw)
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| cx.update(|app| browser.read(app).item().read(app).url().to_string()));
    Ok(json!({ "url": url }))
}

async fn dispatch_evaluate(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let function = params
        .get("function")
        .or_else(|| params.get("expression"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("evaluate requires params.function"))?;
    let browser = active_browser(cx)?;
    let expression = if let Some(ref_id) = ref_id.as_deref() {
        let element = browser
            .update(cx, |view, cx| {
                view.item().read(cx).resolve_automation_ref(ref_id)
            })
            .ok_or_else(|| {
                anyhow!("ref {ref_id} not found or stale; run browser_snapshot first")
            })?;
        match element.element_handle {
            Some(ElementHandle::WkDomToken(token)) => {
                macos_element_evaluate_script(function, &token)
            }
            Some(ElementHandle::CdpBackendNodeId(_)) | None => {
                return Err(anyhow!(
                    "ref {ref_id} does not have a macOS WKWebView token; run browser_snapshot again"
                ));
            }
        }
    } else {
        format!("({function})()")
    };

    let raw = evaluate_expression_on_browser(&browser, expression, cx).await?;
    Ok(json!({ "result": cdp_result_value(raw) }))
}

async fn dispatch_click(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let button = params
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left");
    let double = params
        .get("doubleClick")
        .or_else(|| params.get("double"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let modifiers = parse_string_list(params.get("modifiers"));
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_click_script(&token, button, double, &modifiers),
        cx,
    )
    .await?;
    Ok(json!({ "ref": ref_id }))
}

async fn dispatch_hover(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let modifiers = parse_string_list(params.get("modifiers"));
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    let point = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_hover_script(&token, &modifiers), cx)
            .await?,
    );
    Ok(json!({ "ref": ref_id, "point": point }))
}

async fn dispatch_drag(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let start = params
        .get("startRef")
        .or_else(|| params.get("start"))
        .or_else(|| params.get("from"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("drag requires params.startRef"))?;
    let end = params
        .get("endRef")
        .or_else(|| params.get("end"))
        .or_else(|| params.get("to"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("drag requires params.endRef"))?;
    let browser = active_browser(cx)?;
    let start_token = wk_token_for_ref(&browser, start, cx)?;
    let end_token = wk_token_for_ref(&browser, end, cx)?;
    let start_point = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_element_center_script(&start_token), cx)
            .await?,
    );
    let end_point = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_element_center_script(&end_token), cx)
            .await?,
    );
    let (sx, sy) = point_from_value(&start_point, start)?;
    let (ex, ey) = point_from_value(&end_point, end)?;
    evaluate_expression_on_browser(
        &browser,
        macos_mouse_drag_script(sx, sy, ex, ey, "left"),
        cx,
    )
    .await?;
    Ok(json!({ "from": [sx, sy], "to": [ex, ey] }))
}

async fn dispatch_drop(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let data = params
        .get("data")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mime = params
        .get("mime")
        .and_then(Value::as_str)
        .map(str::to_string);
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_drop_script(&token, data.as_deref(), mime.as_deref()),
        cx,
    )
    .await?;
    Ok(json!({ "dropped": true }))
}

async fn dispatch_type(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("type requires params.text"))?;
    let submit = params.get("submit").and_then(Value::as_bool) == Some(true);
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    evaluate_expression_on_browser(&browser, macos_type_script(&token, text, submit), cx).await?;
    Ok(json!({ "ref": ref_id, "chars": text.len() }))
}

async fn dispatch_select_option(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let values = parse_string_or_array(params.get("values"))
        .ok_or_else(|| anyhow!("select_option requires params.values (string or array)"))?;
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    let result = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_select_option_script(&token, &values), cx)
            .await?,
    );
    if result.get("matched").and_then(Value::as_i64).unwrap_or(0) == 0 {
        return Err(anyhow!("no <option> in {ref_id} matched {values:?}"));
    }
    Ok(result)
}

async fn dispatch_fill_form(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let fields = parse_form_fields(params.get("fields"))?;
    let browser = active_browser(cx)?;
    let mut filled = 0u64;

    for field in fields {
        let token = wk_token_for_ref(&browser, &field.ref_id, cx)?;
        match field.kind.as_deref() {
            Some("checkbox") | Some("radio") => {
                let checked = !matches!(
                    field.value.to_ascii_lowercase().as_str(),
                    "" | "false" | "0" | "off" | "no" | "unchecked"
                );
                evaluate_expression_on_browser(
                    &browser,
                    macos_set_checked_script(&token, checked),
                    cx,
                )
                .await?;
            }
            Some("combobox") | Some("select") => {
                let result = cdp_result_value(
                    evaluate_expression_on_browser(
                        &browser,
                        macos_select_option_script(&token, std::slice::from_ref(&field.value)),
                        cx,
                    )
                    .await?,
                );
                if result.get("matched").and_then(Value::as_i64).unwrap_or(0) == 0 {
                    return Err(anyhow!(
                        "no <option> in {} matched {:?}",
                        field.ref_id,
                        field.value
                    ));
                }
            }
            _ => {
                evaluate_expression_on_browser(
                    &browser,
                    macos_type_script(&token, &field.value, false),
                    cx,
                )
                .await?;
            }
        }
        filled += 1;
    }

    Ok(json!({ "filled": filled }))
}

async fn dispatch_press_key(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("press_key requires params.key"))?;
    let press = crate::automation::keys::parse_key(key)?;
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_press_key_script(
            &press.key,
            &press.code,
            press.text.as_deref(),
            press.modifiers,
        ),
        cx,
    )
    .await?;
    Ok(json!({ "key": key }))
}

async fn dispatch_scroll(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let dx = params.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = params.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
    let browser = active_browser(cx)?;
    let expression = if let Some(ref_id) = ref_id {
        let token = wk_token_for_ref(&browser, &ref_id, cx)?;
        macos_scroll_into_view_script(&token)
    } else {
        macos_scroll_by_script(dx, dy)
    };
    Ok(cdp_result_value(
        evaluate_expression_on_browser(&browser, expression, cx).await?,
    ))
}

async fn dispatch_mouse_move_xy(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let (x, y) = xy_from_params(&params)?;
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_mouse_event_script("mousemove", x, y, "left", 0, 0),
        cx,
    )
    .await?;
    Ok(json!({ "x": x, "y": y }))
}

async fn dispatch_mouse_click_xy(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let (x, y) = xy_from_params(&params)?;
    let button = button_from_params(&params);
    let double = params
        .get("doubleClick")
        .or_else(|| params.get("double"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_mouse_click_script(x, y, &button, double),
        cx,
    )
    .await?;
    Ok(json!({ "x": x, "y": y, "button": button, "double": double }))
}

async fn dispatch_mouse_button(params: Value, down: bool, cx: &mut AsyncApp) -> Result<Value> {
    let (x, y) = xy_from_params(&params)?;
    let button = button_from_params(&params);
    let buttons = if down { mouse_buttons_mask(&button) } else { 0 };
    let event_type = if down { "mousedown" } else { "mouseup" };
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_mouse_event_script(event_type, x, y, &button, buttons, 1),
        cx,
    )
    .await?;
    Ok(json!({ "x": x, "y": y, "button": button, "down": down }))
}

async fn dispatch_mouse_drag_xy(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let sx = num_param(&params, &["startX", "x1", "fromX"])?;
    let sy = num_param(&params, &["startY", "y1", "fromY"])?;
    let ex = num_param(&params, &["endX", "x2", "toX"])?;
    let ey = num_param(&params, &["endY", "y2", "toY"])?;
    let button = button_from_params(&params);
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(
        &browser,
        macos_mouse_drag_script(sx, sy, ex, ey, &button),
        cx,
    )
    .await?;
    Ok(json!({ "from": [sx, sy], "to": [ex, ey], "button": button }))
}

async fn dispatch_mouse_wheel(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    let dx = params.get("deltaX").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = params.get("deltaY").and_then(Value::as_f64).unwrap_or(0.0);
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(&browser, macos_mouse_wheel_script(x, y, dx, dy), cx).await?;
    Ok(json!({ "x": x, "y": y, "deltaX": dx, "deltaY": dy }))
}

async fn dispatch_screenshot(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let format = params
        .get("type")
        .or_else(|| params.get("format"))
        .and_then(Value::as_str)
        .unwrap_or("png")
        .to_string();
    let quality = params.get("quality").and_then(Value::as_i64);
    let ref_id = params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let browser = active_browser(cx)?;

    if let Some(ref_id) = ref_id {
        let token = wk_token_for_ref(&browser, &ref_id, cx)?;
        evaluate_expression_on_browser(&browser, macos_scroll_into_view_script(&token), cx).await?;
    }

    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_wkwebview_session(cx, |session| {
            session
                .capture_screenshot(
                    &format,
                    quality,
                    Box::new(move |result| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(result);
                        }
                    }),
                )
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation screenshot: no live WKWebView session"
        ));
    }
    rx.await
        .map_err(|_| anyhow!("browser automation screenshot channel dropped"))?
}

async fn dispatch_resize(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let (width, height) = resize_dimensions_from_params(&params)?;
    let browser = active_browser(cx)?;
    browser.update(cx, |view, cx| {
        let item = view.item().clone();
        item.update(cx, |item, cx| {
            item.set_automation_viewport_override(px(width as f32), px(height as f32));
            cx.notify();
        });
    });
    Ok(json!({ "width": width, "height": height }))
}

async fn dispatch_pdf_save(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let (landscape, print_background) = pdf_options_from_params(&params);
    validate_macos_pdf_options(landscape, print_background)?;
    let browser = active_browser(cx)?;
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_wkwebview_session(cx, |session| {
            session
                .create_pdf(
                    landscape,
                    print_background,
                    Box::new(move |result| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(result);
                        }
                    }),
                )
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation pdf_save: no live WKWebView session"
        ));
    }
    rx.await
        .map_err(|_| anyhow!("browser automation pdf_save channel dropped"))?
}

async fn dispatch_storage_list(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let store = store_from_params(&params);
    let browser = active_browser(cx)?;
    let items = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_storage_list_script(&store), cx).await?,
    );
    Ok(json!({ "store": store, "items": items }))
}

async fn dispatch_storage_get(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let store = store_from_params(&params);
    let key = key_from_params(&params)?;
    let browser = active_browser(cx)?;
    let value = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_storage_get_script(&store, &key), cx)
            .await?,
    );
    Ok(json!({ "store": store, "key": key, "value": value }))
}

async fn dispatch_storage_set(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let store = store_from_params(&params);
    let key = key_from_params(&params)?;
    let value = params
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(&browser, macos_storage_set_script(&store, &key, &value), cx)
        .await?;
    Ok(json!({ "store": store, "key": key }))
}

async fn dispatch_storage_delete(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let store = store_from_params(&params);
    let key = key_from_params(&params)?;
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(&browser, macos_storage_delete_script(&store, &key), cx).await?;
    Ok(json!({ "store": store, "key": key, "deleted": true }))
}

async fn dispatch_storage_clear(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let store = store_from_params(&params);
    let browser = active_browser(cx)?;
    evaluate_expression_on_browser(&browser, macos_storage_clear_script(&store), cx).await?;
    Ok(json!({ "store": store, "cleared": true }))
}

async fn dispatch_storage_state(cx: &mut AsyncApp) -> Result<Value> {
    let browser = active_browser(cx)?;
    let mut state = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_storage_state_script(), cx).await?,
    );
    let cookies = get_cookies_on_browser(&browser, cx)
        .await?
        .get("cookies")
        .cloned()
        .unwrap_or_else(|| json!([]));
    if let Some(obj) = state.as_object_mut() {
        obj.insert("cookies".to_string(), cookies);
    }
    Ok(state)
}

async fn dispatch_set_storage_state(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let state = params
        .get("state")
        .ok_or_else(|| anyhow!("set_storage_state requires params.state"))?;
    let browser = active_browser(cx)?;
    let mut cookies = 0usize;
    for cookie in state
        .get("cookies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        set_cookie_on_browser(&browser, cookie.clone(), cx).await?;
        cookies += 1;
    }
    let mut result = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_set_storage_state_script(state), cx).await?,
    );
    if let Some(obj) = result.as_object_mut() {
        obj.insert("cookies".to_string(), json!(cookies));
    }
    Ok(result)
}

async fn dispatch_handle_dialog(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let accept = params
        .get("accept")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let prompt_text = params
        .get("promptText")
        .or_else(|| params.get("prompt_text"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let browser = active_browser(cx)?;
    let last = cdp_result_value(
        evaluate_expression_on_browser(
            &browser,
            macos_handle_dialog_script(accept, prompt_text.as_deref()),
            cx,
        )
        .await?,
    );
    Ok(json!({ "armed": true, "accept": accept, "last": last }))
}

async fn dispatch_console_messages(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let level = params.get("level").and_then(Value::as_str);
    let clear = params
        .get("clear")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let browser = active_browser(cx)?;
    let messages = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_console_messages_script(level, clear), cx)
            .await?,
    );
    let count = messages.as_array().map(Vec::len).unwrap_or(0);
    Ok(json!({ "messages": messages, "count": count }))
}

async fn dispatch_network_requests(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let clear = params
        .get("clear")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let browser = active_browser(cx)?;
    let requests = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_network_requests_script(clear), cx).await?,
    );
    let count = requests.as_array().map(Vec::len).unwrap_or(0);
    Ok(json!({ "requests": requests, "count": count }))
}

async fn dispatch_network_request(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let id = params
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("network_request requires params.id"))?;
    let browser = active_browser(cx)?;
    let request = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_network_request_script(id), cx).await?,
    );
    Ok(json!({ "request": request }))
}

async fn dispatch_cookie_list(cx: &mut AsyncApp) -> Result<Value> {
    let browser = active_browser(cx)?;
    get_cookies_on_browser(&browser, cx).await
}

async fn dispatch_cookie_get(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("cookie_get requires params.name"))?;
    let browser = active_browser(cx)?;
    let list = get_cookies_on_browser(&browser, cx).await?;
    let cookie = list
        .get("cookies")
        .and_then(Value::as_array)
        .and_then(|cookies| {
            cookies
                .iter()
                .find(|cookie| cookie.get("name").and_then(Value::as_str) == Some(name))
                .cloned()
        })
        .unwrap_or(Value::Null);
    Ok(json!({ "cookie": cookie }))
}

async fn dispatch_cookie_set(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let cookie = params
        .get("cookie")
        .cloned()
        .ok_or_else(|| anyhow!("cookie_set requires params.cookie (object with name+value)"))?;
    let browser = active_browser(cx)?;
    let cookie = normalize_cookie_for_wk(cookie, || {
        cx.update(|app| browser.read(app).item().read(app).url().to_string())
    })?;
    set_cookie_on_browser(&browser, cookie, cx).await
}

async fn dispatch_cookie_delete(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("cookie_delete requires params.name"))?
        .to_string();
    let browser = active_browser(cx)?;
    delete_cookies_on_browser(&browser, name, cx).await
}

async fn dispatch_cookie_clear(cx: &mut AsyncApp) -> Result<Value> {
    let browser = active_browser(cx)?;
    clear_cookies_on_browser(&browser, cx).await
}

async fn dispatch_verify_text_visible(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("verify_text_visible requires params.text"))?;
    let browser = active_browser(cx)?;
    let visible =
        evaluate_bool_on_browser(&browser, macos_page_contains_text_script(text), cx).await?;
    if !visible {
        return Err(anyhow!("text {text:?} is not visible on the page"));
    }
    Ok(json!({ "ok": true, "text": text }))
}

async fn dispatch_verify_element_visible(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    let visible =
        evaluate_bool_on_browser(&browser, macos_element_visible_script(&token), cx).await?;
    if !visible {
        return Err(anyhow!("element {ref_id} is not visible"));
    }
    Ok(json!({ "ok": true, "ref": ref_id }))
}

async fn dispatch_verify_list_visible(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    let result = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_list_visible_script(&token), cx).await?,
    );
    let visible = result
        .get("visible")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let items = result.get("items").and_then(Value::as_i64).unwrap_or(0);
    if !visible {
        return Err(anyhow!("list {ref_id} is not visible"));
    }
    if items == 0 {
        return Err(anyhow!("list {ref_id} has no visible items"));
    }
    Ok(json!({ "ok": true, "ref": ref_id, "items": items }))
}

async fn dispatch_verify_value(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let ref_id = ref_from_params(&params)?;
    let expected = params
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("verify_value requires params.value"))?;
    let browser = active_browser(cx)?;
    let token = wk_token_for_ref(&browser, &ref_id, cx)?;
    let actual = cdp_result_value(
        evaluate_expression_on_browser(&browser, macos_element_value_script(&token), cx).await?,
    )
    .as_str()
    .unwrap_or("")
    .to_string();
    if actual != expected {
        return Err(anyhow!(
            "value mismatch on {ref_id}: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(json!({ "ok": true, "ref": ref_id, "value": actual }))
}

async fn dispatch_wait_for(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string);
    let text_gone = params
        .get("textGone")
        .or_else(|| params.get("text_gone"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if text.is_none() && text_gone.is_none() {
        if let Some(secs) = params.get("time").and_then(Value::as_f64) {
            if secs > 0.0 {
                cx.background_executor()
                    .timer(Duration::from_secs_f64(secs))
                    .await;
            }
            return Ok(json!({ "waited_seconds": secs }));
        }
    }

    let browser = active_browser(cx)?;
    let wait_load = params
        .get("wait_load")
        .and_then(Value::as_bool)
        .unwrap_or(text.is_none() && text_gone.is_none());
    wait_for_macos(browser, wait_load, text, text_gone, cx).await?;
    Ok(json!({ "ready": true }))
}

async fn dispatch_record(
    params: &Value,
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    match action {
        "start" => {
            let capture_storage_state = params
                .get("captureStorageState")
                .or_else(|| params.get("capture_storage_state"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let storage_state = if capture_storage_state {
                Some(dispatch_storage_state(cx).await?)
            } else {
                None
            };
            let start_url = cx.update(|app| browser.read(app).item().read(app).url().to_string());
            recorder::start(start_url, storage_state);
            Ok(json!({ "recording": true, "storageStateCaptured": capture_storage_state }))
        }
        "stop" => {
            let count = recorder::stop();
            Ok(json!({ "recording": false, "actions": count }))
        }
        "status" => Ok(recorder::status_json()),
        other => Err(anyhow!(
            "unknown record action {other:?}; use start | stop | status"
        )),
    }
}

fn recording_target(
    browser: &Entity<BrowserView>,
    ref_id: &str,
    cx: &mut AsyncApp,
) -> Option<Target> {
    let element = browser.update(cx, |view, cx| {
        view.item().read(cx).resolve_automation_ref(ref_id)
    })?;
    let index = if element.dup_count > 1 {
        Some(element.dup_index)
    } else {
        None
    };
    let durable = match element.durable_selector {
        Some(DurableSelector::TestId(value)) => Some(DurableLoc::TestId(value)),
        Some(DurableSelector::Css(value)) => Some(DurableLoc::Css(value)),
        None => None,
    };
    Some(
        Target::new(element.role, element.name)
            .with_index(index)
            .with_durable(durable)
            .with_frame(element.frame_selector),
    )
}

async fn capture_pending_action(
    method: &str,
    params: &Value,
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
) -> Option<RecordedAction> {
    match method {
        "navigate" => Some(RecordedAction::Navigate {
            url: params.get("url").and_then(Value::as_str)?.to_string(),
        }),
        "navigate_back" => Some(RecordedAction::NavigateBack),
        "click" => {
            let ref_id = ref_str(params)?;
            Some(RecordedAction::Click {
                target: recording_target(browser, &ref_id, cx)?,
                button: params
                    .get("button")
                    .and_then(Value::as_str)
                    .unwrap_or("left")
                    .to_string(),
                double: params
                    .get("doubleClick")
                    .or_else(|| params.get("double"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                modifiers: parse_string_list(params.get("modifiers")),
            })
        }
        "hover" => {
            let ref_id = ref_str(params)?;
            Some(RecordedAction::Hover {
                target: recording_target(browser, &ref_id, cx)?,
            })
        }
        "drag" => {
            let start = params
                .get("startRef")
                .or_else(|| params.get("start"))
                .or_else(|| params.get("from"))
                .and_then(Value::as_str)?;
            let end = params
                .get("endRef")
                .or_else(|| params.get("end"))
                .or_else(|| params.get("to"))
                .and_then(Value::as_str)?;
            Some(RecordedAction::Drag {
                from: recording_target(browser, start, cx)?,
                to: recording_target(browser, end, cx)?,
            })
        }
        "type" => {
            let ref_id = ref_str(params)?;
            Some(RecordedAction::Type {
                target: recording_target(browser, &ref_id, cx)?,
                text: params.get("text").and_then(Value::as_str)?.to_string(),
                submit: params.get("submit").and_then(Value::as_bool) == Some(true),
                slowly: params.get("slowly").and_then(Value::as_bool) == Some(true),
                delay_ms: params
                    .get("slowlyDelayMs")
                    .or_else(|| params.get("slowly_delay_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            })
        }
        "select_option" => {
            let ref_id = ref_str(params)?;
            Some(RecordedAction::SelectOption {
                target: recording_target(browser, &ref_id, cx)?,
                values: parse_string_or_array(params.get("values"))?,
            })
        }
        "fill_form" => {
            let fields = parse_form_fields(params.get("fields")).ok()?;
            let mut recorded = Vec::with_capacity(fields.len());
            for field in fields {
                recorded.push(FormFieldRec {
                    target: recording_target(browser, &field.ref_id, cx)?,
                    value: field.value,
                    kind: field.kind,
                });
            }
            Some(RecordedAction::FillForm { fields: recorded })
        }
        "press_key" => Some(RecordedAction::PressKey {
            key: params.get("key").and_then(Value::as_str)?.to_string(),
        }),
        "scroll" => {
            if let Some(ref_id) = ref_str(params) {
                Some(RecordedAction::ScrollTo {
                    target: recording_target(browser, &ref_id, cx)?,
                })
            } else {
                Some(RecordedAction::ScrollBy {
                    dx: params.get("dx").and_then(Value::as_f64).unwrap_or(0.0),
                    dy: params.get("dy").and_then(Value::as_f64).unwrap_or(0.0),
                })
            }
        }
        "mouse_move_xy" => Some(RecordedAction::MouseMoveXy {
            x: num_opt(params, &["x"])?,
            y: num_opt(params, &["y"])?,
        }),
        "mouse_click_xy" => Some(RecordedAction::MouseClickXy {
            x: num_opt(params, &["x"])?,
            y: num_opt(params, &["y"])?,
            button: button_from_params(params),
            double: params
                .get("doubleClick")
                .or_else(|| params.get("double"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "mouse_down" => Some(RecordedAction::MouseDownXy {
            x: num_opt(params, &["x"])?,
            y: num_opt(params, &["y"])?,
            button: button_from_params(params),
        }),
        "mouse_up" => Some(RecordedAction::MouseUpXy {
            x: num_opt(params, &["x"])?,
            y: num_opt(params, &["y"])?,
            button: button_from_params(params),
        }),
        "mouse_drag_xy" => Some(RecordedAction::MouseDragXy {
            sx: num_opt(params, &["startX", "x1", "fromX"])?,
            sy: num_opt(params, &["startY", "y1", "fromY"])?,
            ex: num_opt(params, &["endX", "x2", "toX"])?,
            ey: num_opt(params, &["endY", "y2", "toY"])?,
        }),
        "mouse_wheel" => Some(RecordedAction::MouseWheel {
            dx: params.get("deltaX").and_then(Value::as_f64).unwrap_or(0.0),
            dy: params.get("deltaY").and_then(Value::as_f64).unwrap_or(0.0),
        }),
        "wait_for" => {
            let text = params.get("text").and_then(Value::as_str);
            let text_gone = params
                .get("textGone")
                .or_else(|| params.get("text_gone"))
                .and_then(Value::as_str);
            if let Some(text) = text {
                Some(RecordedAction::WaitForText {
                    text: text.to_string(),
                })
            } else {
                text_gone.map(|text| RecordedAction::WaitForTextGone {
                    text: text.to_string(),
                })
            }
        }
        "handle_dialog" => {
            let accept = params
                .get("accept")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let prompt_text = params
                .get("promptText")
                .or_else(|| params.get("prompt_text"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(RecordedAction::HandleDialog {
                accept,
                prompt_text,
            })
        }
        "verify_text_visible" => Some(RecordedAction::VerifyTextVisible {
            text: params.get("text").and_then(Value::as_str)?.to_string(),
        }),
        "verify_element_visible" => Some(RecordedAction::VerifyElementVisible {
            target: recording_target(browser, &ref_str(params)?, cx)?,
        }),
        "verify_list_visible" => Some(RecordedAction::VerifyListVisible {
            target: recording_target(browser, &ref_str(params)?, cx)?,
        }),
        "verify_value" => {
            let ref_id = ref_str(params)?;
            Some(RecordedAction::VerifyValue {
                target: recording_target(browser, &ref_id, cx)?,
                value: params.get("value").and_then(Value::as_str)?.to_string(),
            })
        }
        _ => None,
    }
}

async fn evaluate_expression_on_browser(
    browser: &Entity<BrowserView>,
    expression: String,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_automation_session(cx, |session| {
            session
                .evaluate_expression(
                    &expression,
                    true,
                    Box::new(move |result| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(result);
                        }
                    }),
                )
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation evaluate: no live WKWebView session"
        ));
    }

    rx.await
        .map_err(|_| anyhow!("browser automation evaluate channel dropped"))?
}

async fn get_cookies_on_browser(browser: &Entity<BrowserView>, cx: &mut AsyncApp) -> Result<Value> {
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_wkwebview_session(cx, |session| {
            session
                .get_cookies(Box::new(move |result| {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(result);
                    }
                }))
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation cookies: no live WKWebView session"
        ));
    }

    rx.await
        .map_err(|_| anyhow!("browser automation cookies channel dropped"))?
}

async fn set_cookie_on_browser(
    browser: &Entity<BrowserView>,
    cookie: Value,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_wkwebview_session(cx, |session| {
            session
                .set_cookie(
                    cookie,
                    Box::new(move |result| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(result);
                        }
                    }),
                )
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation cookie_set: no live WKWebView session"
        ));
    }

    rx.await
        .map_err(|_| anyhow!("browser automation cookie_set channel dropped"))?
}

async fn delete_cookies_on_browser(
    browser: &Entity<BrowserView>,
    name: String,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_wkwebview_session(cx, |session| {
            session
                .delete_cookies_named(
                    name,
                    Box::new(move |result| {
                        if let Some(tx) = tx.take() {
                            let _ = tx.send(result);
                        }
                    }),
                )
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation cookie_delete: no live WKWebView session"
        ));
    }

    rx.await
        .map_err(|_| anyhow!("browser automation cookie_delete channel dropped"))?
}

async fn clear_cookies_on_browser(
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (tx, rx) = oneshot::channel::<Result<Value>>();
    let mut tx = Some(tx);
    let kicked_off = browser.update(cx, |view, cx| {
        view.with_wkwebview_session(cx, |session| {
            session
                .clear_cookies(Box::new(move |result| {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(result);
                    }
                }))
                .is_ok()
        })
        .unwrap_or(false)
    });
    if !kicked_off {
        return Err(anyhow!(
            "browser automation cookie_clear: no live WKWebView session"
        ));
    }

    rx.await
        .map_err(|_| anyhow!("browser automation cookie_clear channel dropped"))?
}

async fn evaluate_bool_on_browser(
    browser: &Entity<BrowserView>,
    expression: String,
    cx: &mut AsyncApp,
) -> Result<bool> {
    cdp_result_value(evaluate_expression_on_browser(browser, expression, cx).await?)
        .as_bool()
        .ok_or_else(|| anyhow!("browser automation evaluate did not return a boolean"))
}

async fn wait_for_macos(
    browser: Entity<BrowserView>,
    wait_load: bool,
    text: Option<String>,
    text_gone: Option<String>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        let mut loading = browser.update(cx, |view, cx| view.item().read(cx).is_loading());
        let mut title = browser.update(cx, |view, cx| view.item().read(cx).title().to_string());
        let mut url = browser.update(cx, |view, cx| view.item().read(cx).url().to_string());

        let page_state = cdp_result_value(
            evaluate_expression_on_browser(&browser, macos_page_state_script().to_string(), cx)
                .await
                .unwrap_or(Value::Null),
        );
        let ready_state = page_state
            .get("readyState")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(value) = page_state.get("title").and_then(Value::as_str) {
            title = value.to_string();
        }
        if let Some(value) = page_state.get("url").and_then(Value::as_str) {
            url = value.to_string();
        }
        if ready_state == "complete" {
            loading = false;
        }
        let load_ready =
            !wait_load || ready_state == "complete" || (ready_state.is_empty() && !loading);

        let text_ready = match text.as_deref() {
            Some(needle) if title.contains(needle) => true,
            Some(needle) if load_ready => {
                evaluate_bool_on_browser(&browser, macos_page_contains_text_script(needle), cx)
                    .await
                    .unwrap_or(false)
            }
            Some(_) => false,
            None => true,
        };

        let gone_ready = match text_gone.as_deref() {
            Some(needle) if title.contains(needle) => false,
            Some(needle) if load_ready => {
                !evaluate_bool_on_browser(&browser, macos_page_contains_text_script(needle), cx)
                    .await
                    .unwrap_or(true)
            }
            Some(_) => false,
            None => true,
        };

        if load_ready && text_ready && gone_ready {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "browser automation wait_for timed out after {:?} (loading={}, title={:?}, url={})",
                DEFAULT_WAIT_TIMEOUT,
                loading,
                title,
                url
            ));
        }

        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

async fn wait_for_navigation_macos(
    browser: Entity<BrowserView>,
    previous_url: String,
    cx: &mut AsyncApp,
) -> Result<()> {
    let started = Instant::now();
    let deadline = Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        let item_state = browser.update(cx, |view, cx| {
            let item = view.item().read(cx);
            (
                item.is_loading(),
                item.title().to_string(),
                item.url().to_string(),
            )
        });
        let page_state = cdp_result_value(
            evaluate_expression_on_browser(&browser, macos_page_state_script().to_string(), cx)
                .await
                .unwrap_or(Value::Null),
        );
        let ready_state = page_state
            .get("readyState")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = page_state
            .get("url")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(item_state.2.as_str());
        let title = page_state
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(item_state.1.as_str());
        let loading = if ready_state == "complete" {
            false
        } else {
            item_state.0
        };
        let url_changed = !previous_url.is_empty() && url != previous_url;
        let ready_enough = ready_state == "complete"
            || (!loading && !url.is_empty())
            || (url_changed && started.elapsed() >= Duration::from_millis(1500));

        if ready_enough
            && (previous_url.is_empty()
                || url_changed
                || started.elapsed() >= Duration::from_millis(750))
        {
            browser.update(cx, |view, cx| {
                view.automation_update_page_state(
                    (!url.is_empty()).then(|| url.to_string()),
                    (!title.is_empty()).then(|| title.to_string()),
                    loading,
                    cx,
                );
            });
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "browser automation navigate timed out after {:?} (previous_url={previous_url:?}, readyState={ready_state:?}, url={url:?})",
                DEFAULT_WAIT_TIMEOUT
            ));
        }

        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

fn active_browser(cx: &mut AsyncApp) -> Result<Entity<BrowserView>> {
    cx.update(|app| resolve_automation_target_global(app))
        .ok_or_else(|| anyhow!(NO_ACTIVE_BROWSER_MESSAGE))
}

fn ref_from_params(params: &Value) -> Result<String> {
    params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing ref/target; run browser_snapshot first"))
}

fn navigate_url_from_params(params: &Value) -> Result<String> {
    params
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("navigate requires params.url"))
}

fn ref_str(params: &Value) -> Option<String> {
    params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn store_from_params(params: &Value) -> String {
    params
        .get("store")
        .and_then(Value::as_str)
        .unwrap_or("local")
        .to_string()
}

fn key_from_params(params: &Value) -> Result<String> {
    params
        .get("key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing params.key"))
}

fn xy_from_params(params: &Value) -> Result<(f64, f64)> {
    Ok((num_param(params, &["x"])?, num_param(params, &["y"])?))
}

fn point_from_value(value: &Value, ref_id: &str) -> Result<(f64, f64)> {
    let x = value
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("element {ref_id} did not return a numeric x coordinate"))?;
    let y = value
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("element {ref_id} did not return a numeric y coordinate"))?;
    Ok((x, y))
}

fn num_param(params: &Value, keys: &[&str]) -> Result<f64> {
    num_opt(params, keys).ok_or_else(|| anyhow!("missing numeric param among {keys:?}"))
}

fn num_opt(params: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| params.get(*key).and_then(Value::as_f64))
}

fn button_from_params(params: &Value) -> String {
    params
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
        .to_string()
}

fn mouse_buttons_mask(button: &str) -> i32 {
    match button {
        "middle" => 4,
        "right" => 2,
        _ => 1,
    }
}

fn wk_token_for_ref(
    browser: &Entity<BrowserView>,
    ref_id: &str,
    cx: &mut AsyncApp,
) -> Result<String> {
    let element = browser
        .update(cx, |view, cx| {
            view.item().read(cx).resolve_automation_ref(ref_id)
        })
        .ok_or_else(|| anyhow!("ref {ref_id} not found or stale; run browser_snapshot first"))?;
    match element.element_handle {
        Some(ElementHandle::WkDomToken(token)) => Ok(token),
        Some(ElementHandle::CdpBackendNodeId(_)) | None => Err(anyhow!(
            "ref {ref_id} does not have a macOS WKWebView token; run browser_snapshot again"
        )),
    }
}

fn parse_string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_string_or_array(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(value) => Some(vec![value.clone()]),
        Value::Array(values) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    }
}

fn parse_form_fields(value: Option<&Value>) -> Result<Vec<MacosFormField>> {
    let fields = value
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("fill_form requires params.fields (array)"))?;
    let mut parsed = Vec::with_capacity(fields.len());
    for field in fields {
        let ref_id = field
            .get("ref")
            .or_else(|| field.get("target"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("each fill_form field needs a ref"))?
            .to_string();
        let value = match field.get("value") {
            Some(Value::String(value)) => value.clone(),
            Some(Value::Bool(value)) => value.to_string(),
            Some(Value::Number(value)) => value.to_string(),
            _ => String::new(),
        };
        let kind = field
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        parsed.push(MacosFormField {
            ref_id,
            value,
            kind,
        });
    }
    Ok(parsed)
}

fn cdp_result_value(value: Value) -> Value {
    value
        .get("result")
        .and_then(|result| result.get("value"))
        .cloned()
        .or_else(|| value.get("value").cloned())
        .unwrap_or(value)
}

async fn dispatch_tabs(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let (window, workspace) = cx
        .update(|app| resolve_automation_workspace_global(app))
        .ok_or_else(|| anyhow!("No Zed workspace window found"))?;

    let action = params
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or("list");

    match action {
        "list" => tabs::list(&workspace, cx),
        "select" => {
            let index = tab_index(&params)?;
            tabs::select(workspace, window, index, cx).await
        }
        "close" => {
            let index = tab_index(&params)?;
            tabs::close(workspace, window, index, cx).await
        }
        "new" => {
            let url = match params.get("url").and_then(|value| value.as_str()) {
                Some(url) => SharedString::new(url.to_string()),
                None => cx.update(|app| {
                    SharedString::new(BrowserSettings::get_global(app).homepage.clone())
                }),
            };
            tabs::new_tab(workspace, window, url, cx).await
        }
        other => Err(anyhow!(
            "unknown tabs action {other:?}; use list | select | new | close"
        )),
    }
}

fn tab_index(params: &Value) -> Result<usize> {
    params
        .get("index")
        .and_then(|value| value.as_u64())
        .map(|index| index as usize)
        .ok_or_else(|| anyhow!("this tabs action requires params.index (a tab number from `list`)"))
}

fn unsupported_native_gap(method: &str, reason: &str) -> Result<Value> {
    Err(anyhow!(
        "browser automation method {method:?} is recognized but not implemented for macOS WKWebView yet: {reason}"
    ))
}

fn normalize_cookie_for_wk(
    mut cookie: Value,
    current_url: impl FnOnce() -> String,
) -> Result<Value> {
    if !cookie.is_object() {
        return Err(anyhow!("cookie_set requires params.cookie to be an object"));
    }
    if cookie.get("url").is_none() && cookie.get("domain").is_none() {
        if let Some(obj) = cookie.as_object_mut() {
            obj.insert("url".to_string(), Value::from(current_url()));
        }
    }
    Ok(cookie)
}

fn resize_dimensions_from_params(params: &Value) -> Result<(i64, i64)> {
    let width = params
        .get("width")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("resize requires params.width"))?;
    let height = params
        .get("height")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("resize requires params.height"))?;
    if width <= 0 || height <= 0 {
        return Err(anyhow!("resize width and height must be positive"));
    }
    Ok((width, height))
}

fn pdf_options_from_params(params: &Value) -> (bool, bool) {
    let landscape = params
        .get("landscape")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let print_background = params
        .get("printBackground")
        .or_else(|| params.get("print_background"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    (landscape, print_background)
}

fn validate_macos_pdf_options(landscape: bool, print_background: bool) -> Result<()> {
    if landscape {
        return Err(anyhow!(
            "browser automation method {:?} is recognized but not implemented for macOS WKWebView yet: {}",
            "pdf_save",
            "WKWebView PDF generation is source-wired for default orientation; landscape output needs native print-configuration support",
        ));
    }
    if !print_background {
        return Err(anyhow!(
            "browser automation method {:?} is recognized but not implemented for macOS WKWebView yet: {}",
            "pdf_save",
            "WKWebView PDF generation does not expose a printBackground toggle in this native slice",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_response_serializes_ok() {
        let line = ipc_ok_response(json!("1"), json!({ "status": "ok" }));
        assert!(line.contains("\"id\":\"1\""));
        assert!(line.contains("\"ok\":true"));
    }

    #[test]
    fn ipc_error_response_includes_method_limitation() {
        let line = ipc_error_response(
            json!(7),
            "browser automation method \"screenshot\" is not implemented for macOS WKWebView yet",
        );
        assert!(line.contains("\"ok\":false"));
        assert!(line.contains("screenshot"));
        assert!(line.contains("macOS WKWebView"));
    }

    #[test]
    fn tab_index_requires_numeric_index() {
        assert_eq!(tab_index(&json!({ "index": 2 })).unwrap(), 2);
        assert!(tab_index(&json!({ "index": "2" })).is_err());
        assert!(tab_index(&json!({})).is_err());
    }

    #[test]
    fn point_from_value_requires_numeric_coordinates() {
        assert_eq!(
            point_from_value(&json!({ "x": 12.5, "y": 44.0 }), "wk-1").unwrap(),
            (12.5, 44.0)
        );
        assert!(point_from_value(&json!({ "x": 12.5 }), "wk-1").is_err());
        assert!(point_from_value(&json!({ "x": "12", "y": 44.0 }), "wk-1").is_err());
    }

    #[test]
    fn unsupported_native_gap_names_recognized_method_and_reason() {
        let err = unsupported_native_gap("file_upload", "requires native WKUIDelegate")
            .unwrap_err()
            .to_string();
        assert!(err.contains("file_upload"));
        assert!(err.contains("recognized"));
        assert!(err.contains("WKUIDelegate"));
    }

    #[test]
    fn input_automation_methods_release_wk_keyboard_focus() {
        for method in [
            "click",
            "type",
            "select_option",
            "fill_form",
            "press_key",
            "mouse_click_xy",
            "mouse_down",
            "mouse_up",
            "mouse_drag_xy",
        ] {
            assert!(should_release_browser_keyboard_focus(method), "{method}");
        }

        for method in ["snapshot", "screenshot", "evaluate", "scroll", "tabs"] {
            assert!(!should_release_browser_keyboard_focus(method), "{method}");
        }
    }

    #[test]
    fn macos_e2e_regression_tool_surface_has_expected_focus_policy() {
        let workflow = [
            ("tabs", false),
            ("navigate", false),
            ("snapshot", false),
            ("scroll", false),
            ("screenshot", false),
            ("click", true),
            ("type", true),
            ("fill_form", true),
            ("press_key", true),
            ("select_option", true),
        ];

        for (method, releases_keyboard_focus) in workflow {
            assert_eq!(
                should_release_browser_keyboard_focus(method),
                releases_keyboard_focus,
                "{method}"
            );
        }
    }

    #[test]
    fn macos_navigation_requires_string_url_for_url_bar_entry() {
        assert_eq!(
            navigate_url_from_params(&json!({ "url": "https://example.com/books?q=rust" }))
                .unwrap(),
            "https://example.com/books?q=rust"
        );
        assert!(navigate_url_from_params(&json!({ "url": 42 })).is_err());
        assert!(navigate_url_from_params(&json!({})).is_err());
    }

    #[test]
    fn macos_fill_form_regression_parses_visible_field_payloads() {
        let fields = parse_form_fields(Some(&json!([
            { "ref": "button-search", "value": "books" },
            { "target": "available-online", "value": true, "type": "checkbox" },
            { "ref": "format", "value": 2, "type": "select" }
        ])))
        .unwrap();

        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].ref_id, "button-search");
        assert_eq!(fields[0].value, "books");
        assert_eq!(fields[0].kind, None);
        assert_eq!(fields[1].ref_id, "available-online");
        assert_eq!(fields[1].value, "true");
        assert_eq!(fields[1].kind.as_deref(), Some("checkbox"));
        assert_eq!(fields[2].ref_id, "format");
        assert_eq!(fields[2].value, "2");
        assert_eq!(fields[2].kind.as_deref(), Some("select"));
    }

    #[test]
    fn macos_fill_form_regression_rejects_missing_refs() {
        let err = parse_form_fields(Some(&json!([{ "value": "books" }])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("each fill_form field needs a ref"));
    }

    #[test]
    fn macos_select_option_regression_accepts_string_or_array_values() {
        assert_eq!(
            parse_string_or_array(Some(&json!("Books"))).unwrap(),
            vec!["Books".to_string()]
        );
        assert_eq!(
            parse_string_or_array(Some(&json!(["Books", "Ebooks"]))).unwrap(),
            vec!["Books".to_string(), "Ebooks".to_string()]
        );
        assert!(parse_string_or_array(Some(&json!(false))).is_none());
    }

    #[test]
    fn resize_dimensions_from_params_requires_positive_dimensions() {
        assert_eq!(
            resize_dimensions_from_params(&json!({ "width": 320, "height": 640 })).unwrap(),
            (320, 640)
        );
        assert!(resize_dimensions_from_params(&json!({ "width": 0, "height": 640 })).is_err());
        assert!(resize_dimensions_from_params(&json!({ "width": 320 })).is_err());
    }

    #[test]
    fn normalize_cookie_for_wk_defaults_to_current_url_without_domain() {
        let cookie = normalize_cookie_for_wk(json!({ "name": "sid", "value": "abc" }), || {
            "https://example.com/path".to_string()
        })
        .unwrap();
        assert_eq!(cookie["url"], "https://example.com/path");
    }

    #[test]
    fn normalize_cookie_for_wk_preserves_explicit_domain_or_url() {
        let cookie = normalize_cookie_for_wk(
            json!({ "name": "sid", "value": "abc", "domain": "example.com" }),
            || "https://other.example/".to_string(),
        )
        .unwrap();
        assert_eq!(cookie["domain"], "example.com");
        assert!(cookie.get("url").is_none());

        let cookie = normalize_cookie_for_wk(
            json!({ "name": "sid", "value": "abc", "url": "https://example.com/" }),
            || "https://other.example/".to_string(),
        )
        .unwrap();
        assert_eq!(cookie["url"], "https://example.com/");
    }

    #[test]
    fn normalize_cookie_for_wk_rejects_non_object_cookie() {
        assert!(
            normalize_cookie_for_wk(json!("sid=abc"), || "https://example.com/".to_string())
                .is_err()
        );
    }

    #[test]
    fn pdf_options_from_params_defaults_like_mcp() {
        assert_eq!(pdf_options_from_params(&json!({})), (false, true));
    }

    #[test]
    fn pdf_options_from_params_accepts_camel_and_snake_background() {
        assert_eq!(
            pdf_options_from_params(&json!({
                "landscape": true,
                "printBackground": false
            })),
            (true, false)
        );
        assert_eq!(
            pdf_options_from_params(&json!({
                "print_background": false
            })),
            (false, false)
        );
    }

    #[test]
    fn validate_macos_pdf_options_accepts_default_mcp_options() {
        validate_macos_pdf_options(false, true).unwrap();
    }

    #[test]
    fn validate_macos_pdf_options_rejects_unimplemented_print_options() {
        let err = validate_macos_pdf_options(true, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("landscape"));

        let err = validate_macos_pdf_options(false, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("printBackground"));
    }

    #[test]
    fn cdp_result_value_extracts_nested_runtime_value() {
        assert_eq!(
            cdp_result_value(json!({ "result": { "type": "number", "value": 42 } })),
            json!(42)
        );
        assert_eq!(
            cdp_result_value(json!({ "type": "string", "value": "ready" })),
            json!("ready")
        );
        let plain = json!({ "arbitrary": true });
        assert_eq!(cdp_result_value(plain.clone()), plain);
    }
}
