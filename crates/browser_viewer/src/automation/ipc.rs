//! Loopback IPC bridge between `zed-browser-mcp` (stdio MCP) and GPUI automation.
//!
//! Uses TCP on `127.0.0.1` (default port 19382). The plan also allows named pipes;
//! TCP keeps the Node client trivial and avoids message-mode pipe edge cases.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use std::sync::Mutex;

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use futures::channel::mpsc as async_mpsc;
use gpui::{App, AsyncApp, Global};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::BrowserSettings;
use crate::automation::commands;
use crate::automation::navigate::{WaitForOptions, DEFAULT_NAV_TIMEOUT};
use crate::automation::recorder::{self, FormFieldRec, RecordedAction, Target};
use crate::automation::tabs;
use crate::browser_view::BrowserView;
use gpui::Entity;
use crate::automation::target::{
    resolve_automation_target_global, resolve_automation_workspace_global,
};
use gpui::SharedString;
use settings::Settings as _;

/// Default loopback port for MCP ↔ Zed automation.
pub const DEFAULT_IPC_PORT: u16 = 19382;

const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

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

struct GlobalBrowserAutomationIpc(async_mpsc::UnboundedSender<PendingIpcRequest>);

impl Global for GlobalBrowserAutomationIpc {}

/// Serializes MCP IPC automation with the browser tab's own GPUI updates.
static AUTOMATION_SERIAL: Mutex<()> = Mutex::new(());

fn ipc_port() -> u16 {
    std::env::var("ZED_BROWSER_AUTOMATION_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
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
    let _guard = AUTOMATION_SERIAL
        .lock()
        .map_err(|_| anyhow!("browser automation IPC lock poisoned"))?;

    log::info!(
        "browser automation IPC: {} {:?}",
        request.method,
        request.params
    );

    // `tabs` operates on the workspace, not a single page — and `list`/`new`
    // must work even with zero browser tabs open, so it resolves its own
    // target ahead of the page-level browser resolution below.
    if request.method.as_str() == "tabs" {
        return dispatch_tabs(request.params, cx).await;
    }
    // `close` acts on the active tab via the workspace (window + workspace), not
    // a page-level CDP target.
    if request.method.as_str() == "close" {
        let (window, workspace) = cx
            .update(|app| resolve_automation_workspace_global(app))
            .ok_or_else(|| anyhow!("No Zed workspace window found"))?;
        return tabs::close_active(workspace, window, cx).await;
    }

    let browser = cx
        .update(|app| resolve_automation_target_global(app))
        .ok_or_else(|| {
            anyhow!("No active Zed browser tab. Open one with browser: new tab first.")
        })?;

    // CP15: `record` start/stop/status + `codegen` are meta-controls — handle
    // them before the recordable-action path so they aren't themselves recorded.
    if request.method.as_str() == "record" {
        return dispatch_record(&request.params, &browser, cx).await;
    }
    if request.method.as_str() == "codegen" {
        let (script, storage_state) = recorder::codegen();
        return Ok(json!({ "script": script, "storageState": storage_state }));
    }

    // CP15: resolve ref→role+name *before* the action runs (refs invalidate on
    // navigation), then commit it to the recording buffer only on success.
    let pending = if recorder::is_recording() {
        capture_pending_action(request.method.as_str(), &request.params, &browser, cx).await
    } else {
        None
    };
    // Most arms move `browser` by value; keep a handle for the post-action URL.
    let browser_for_commit = browser.clone();

    let result = match request.method.as_str() {
        "snapshot" => {
            let snapshot = commands::snapshot(browser, cx).await?;
            Ok(json!({
                "yaml": snapshot.yaml,
                "ref_count": snapshot.ref_count,
            }))
        }
        "click" => {
            let ref_id = ref_from_params(&request.params)?;
            let button = request
                .params
                .get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left")
                .to_string();
            let double = request
                .params
                .get("doubleClick")
                .or_else(|| request.params.get("double"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let modifiers = parse_string_list(request.params.get("modifiers")).unwrap_or_default();
            commands::click(browser, &ref_id, &button, double, &modifiers, cx).await?;
            Ok(json!({ "ref": ref_id }))
        }
        "type" => {
            let ref_id = ref_from_params(&request.params)?;
            let text = request
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("type requires params.text"))?;
            let submit = request.params.get("submit").and_then(|v| v.as_bool()) == Some(true);
            let slowly = request.params.get("slowly").and_then(|v| v.as_bool()) == Some(true);
            let slowly_delay_ms = request
                .params
                .get("slowlyDelayMs")
                .or_else(|| request.params.get("slowly_delay_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            commands::type_text(browser.clone(), &ref_id, text, submit, slowly, slowly_delay_ms, cx).await?;
            if submit {
                commands::press_key(browser, "Enter", cx).await?;
            }
            Ok(json!({ "ref": ref_id, "chars": text.len() }))
        }
        "press_key" => {
            let key = request
                .params
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("press_key requires params.key"))?;
            commands::press_key(browser, key, cx).await?;
            Ok(json!({ "key": key }))
        }
        "scroll" => {
            let ref_id = request
                .params
                .get("ref")
                .or_else(|| request.params.get("target"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let dx = request.params.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let dy = request.params.get("dy").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let pos = commands::scroll(browser, ref_id, dx, dy, cx).await?;
            Ok(pos)
        }
        "screenshot" => {
            let full_page = request
                .params
                .get("full_page")
                .or_else(|| request.params.get("fullPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let format = request
                .params
                .get("type")
                .or_else(|| request.params.get("format"))
                .and_then(|v| v.as_str())
                .unwrap_or("png")
                .to_string();
            let quality = request.params.get("quality").and_then(|v| v.as_i64());
            let ref_id = request
                .params
                .get("ref")
                .or_else(|| request.params.get("target"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            commands::screenshot(browser, full_page, format, quality, ref_id, cx).await
        }
        "evaluate" => {
            let function = request
                .params
                .get("function")
                .or_else(|| request.params.get("expression"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("evaluate requires params.function"))?
                .to_string();
            let ref_id = request
                .params
                .get("ref")
                .or_else(|| request.params.get("target"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            commands::evaluate(browser, function, ref_id, cx).await
        }
        "select_option" => {
            let ref_id = ref_from_params(&request.params)?;
            let values = parse_string_list(request.params.get("values"))
                .ok_or_else(|| anyhow!("select_option requires params.values (string or array)"))?;
            commands::select_option(browser, &ref_id, values, cx).await
        }
        "hover" => {
            let ref_id = ref_from_params(&request.params)?;
            commands::hover(browser, &ref_id, cx).await
        }
        "file_upload" => {
            let ref_id = ref_from_params(&request.params)?;
            let files = parse_string_list(
                request.params.get("paths").or_else(|| request.params.get("files")),
            )
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("file_upload requires params.paths (a path or array of paths)"))?;
            commands::file_upload(browser, &ref_id, files, cx).await
        }
        "drag" => {
            let start = request
                .params
                .get("startRef")
                .or_else(|| request.params.get("start"))
                .or_else(|| request.params.get("from"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("drag requires params.startRef"))?
                .to_string();
            let end = request
                .params
                .get("endRef")
                .or_else(|| request.params.get("end"))
                .or_else(|| request.params.get("to"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("drag requires params.endRef"))?
                .to_string();
            commands::drag(browser, &start, &end, cx).await
        }
        "drop" => {
            let ref_id = ref_from_params(&request.params)?;
            let data = request.params.get("data").and_then(|v| v.as_str()).map(str::to_string);
            let mime = request.params.get("mime").and_then(|v| v.as_str()).map(str::to_string);
            commands::drop(browser, &ref_id, data, mime, cx).await
        }
        "handle_dialog" => {
            let accept = request
                .params
                .get("accept")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let prompt_text = request
                .params
                .get("promptText")
                .or_else(|| request.params.get("prompt_text"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            commands::handle_dialog(browser, accept, prompt_text, cx).await
        }
        "mouse_move_xy" => {
            let (x, y) = xy_from_params(&request.params)?;
            commands::mouse_move_xy(browser, x, y, cx).await
        }
        "mouse_click_xy" => {
            let (x, y) = xy_from_params(&request.params)?;
            let button = button_from_params(&request.params);
            let double = request
                .params
                .get("doubleClick")
                .or_else(|| request.params.get("double"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            commands::mouse_click_xy(browser, x, y, &button, double, cx).await
        }
        "mouse_down" => {
            let (x, y) = xy_from_params(&request.params)?;
            commands::mouse_button(browser, x, y, &button_from_params(&request.params), true, cx).await
        }
        "mouse_up" => {
            let (x, y) = xy_from_params(&request.params)?;
            commands::mouse_button(browser, x, y, &button_from_params(&request.params), false, cx).await
        }
        "mouse_drag_xy" => {
            let sx = num_param(&request.params, &["startX", "x1", "fromX"])?;
            let sy = num_param(&request.params, &["startY", "y1", "fromY"])?;
            let ex = num_param(&request.params, &["endX", "x2", "toX"])?;
            let ey = num_param(&request.params, &["endY", "y2", "toY"])?;
            commands::mouse_drag_xy(browser, sx, sy, ex, ey, &button_from_params(&request.params), cx).await
        }
        "mouse_wheel" => {
            let x = request.params.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = request.params.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let dx = request.params.get("deltaX").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let dy = request.params.get("deltaY").and_then(|v| v.as_f64()).unwrap_or(0.0);
            commands::mouse_wheel(browser, x, y, dx, dy, cx).await
        }
        "console_messages" => {
            let level = request.params.get("level").and_then(|v| v.as_str()).map(str::to_string);
            let clear = request.params.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
            commands::console_messages(browser, level, clear, cx).await
        }
        "network_requests" => {
            let clear = request.params.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
            commands::network_requests(browser, clear, cx).await
        }
        "network_request" => {
            let id = request
                .params
                .get("id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| anyhow!("network_request requires params.id"))?;
            commands::network_request(browser, id, cx).await
        }
        "resize" => {
            let width = num_param(&request.params, &["width", "w"])? as i64;
            let height = num_param(&request.params, &["height", "h"])? as i64;
            commands::resize(browser, width, height, cx).await
        }
        "pdf_save" => {
            let landscape = request.params.get("landscape").and_then(|v| v.as_bool()).unwrap_or(false);
            let print_background = request
                .params
                .get("printBackground")
                .or_else(|| request.params.get("print_background"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            commands::pdf_save(browser, landscape, print_background, cx).await
        }
        "cookie_list" => commands::cookie_list(browser, cx).await,
        "cookie_get" => {
            let name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("cookie_get requires params.name"))?;
            commands::cookie_get(browser, name, cx).await
        }
        "cookie_set" => {
            let cookie = request
                .params
                .get("cookie")
                .cloned()
                .ok_or_else(|| anyhow!("cookie_set requires params.cookie (object with name+value)"))?;
            commands::cookie_set(browser, cookie, cx).await
        }
        "cookie_delete" => {
            let name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("cookie_delete requires params.name"))?;
            commands::cookie_delete(browser, name, cx).await
        }
        "cookie_clear" => commands::cookie_clear(browser, cx).await,
        "storage_list" => {
            let store = store_from_params(&request.params);
            commands::storage_list(browser, &store, cx).await
        }
        "storage_get" => {
            let store = store_from_params(&request.params);
            let key = key_from_params(&request.params)?;
            commands::storage_get(browser, &store, &key, cx).await
        }
        "storage_set" => {
            let store = store_from_params(&request.params);
            let key = key_from_params(&request.params)?;
            let value = request.params.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string();
            commands::storage_set(browser, &store, &key, &value, cx).await
        }
        "storage_delete" => {
            let store = store_from_params(&request.params);
            let key = key_from_params(&request.params)?;
            commands::storage_delete(browser, &store, &key, cx).await
        }
        "storage_clear" => {
            let store = store_from_params(&request.params);
            commands::storage_clear(browser, &store, cx).await
        }
        "storage_state" => commands::storage_state(browser, cx).await,
        "set_storage_state" => {
            let state = request
                .params
                .get("state")
                .cloned()
                .ok_or_else(|| anyhow!("set_storage_state requires params.state"))?;
            commands::set_storage_state(browser, state, cx).await
        }
        "verify_element_visible" => {
            let ref_id = ref_from_params(&request.params)?;
            commands::verify_element_visible(browser, &ref_id, cx).await
        }
        "verify_list_visible" => {
            let ref_id = ref_from_params(&request.params)?;
            commands::verify_list_visible(browser, &ref_id, cx).await
        }
        "verify_text_visible" => {
            let text = request
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("verify_text_visible requires params.text"))?;
            commands::verify_text_visible(browser, text, cx).await
        }
        "verify_value" => {
            let ref_id = ref_from_params(&request.params)?;
            let value = request
                .params
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("verify_value requires params.value"))?;
            commands::verify_value(browser, &ref_id, value, cx).await
        }
        "navigate" => {
            let url = request
                .params
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("navigate requires params.url"))?;
            let final_url = commands::navigate(browser, url, cx).await?;
            Ok(json!({ "url": final_url }))
        }
        "navigate_back" => {
            let url = commands::navigate_back(browser, cx).await?;
            Ok(json!({ "url": url }))
        }
        "fill_form" => {
            let fields = parse_form_fields(request.params.get("fields"))?;
            commands::fill_form(browser, fields, cx).await
        }
        "wait_for" => {
            let text = request
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let text_gone = request
                .params
                .get("textGone")
                .or_else(|| request.params.get("text_gone"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // `time` is only a plain sleep when no text criterion is given.
            if text.is_none() && text_gone.is_none() {
                if let Some(secs) = request.params.get("time").and_then(|v| v.as_f64()) {
                    if secs > 0.0 {
                        cx.background_executor()
                            .timer(Duration::from_secs_f64(secs))
                            .await;
                    }
                    return Ok(json!({ "waited_seconds": secs }));
                }
            }
            let wait_load = request
                .params
                .get("wait_load")
                .and_then(|v| v.as_bool())
                // When waiting on text(Gone), don't also block on a load event.
                .unwrap_or(text.is_none() && text_gone.is_none());
            commands::wait_for(
                browser,
                WaitForOptions {
                    wait_load,
                    text,
                    text_gone,
                    timeout: DEFAULT_NAV_TIMEOUT,
                },
                cx,
            )
            .await?;
            Ok(json!({ "ready": true }))
        }
        "ping" => Ok(json!({ "status": "ok" })),
        other => Err(anyhow!("unknown IPC method {other:?}")),
    };

    // CP15: commit the recorded action with its post-action URL on success.
    if result.is_ok() {
        if let Some(action) = pending {
            let url = cx.update(|app| {
                browser_for_commit
                    .read(app)
                    .item()
                    .read(app)
                    .url()
                    .to_string()
            });
            recorder::push(action, url);
        }
    }

    result
}

/// Handle `browser_record` (start / stop / status).
async fn dispatch_record(
    params: &Value,
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let action = params
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("status");
    match action {
        "start" => {
            let capture_ss = params
                .get("captureStorageState")
                .or_else(|| params.get("capture_storage_state"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let storage_state = if capture_ss {
                Some(commands::storage_state(browser.clone(), cx).await?)
            } else {
                None
            };
            let start_url =
                cx.update(|app| browser.read(app).item().read(app).url().to_string());
            recorder::start(start_url, storage_state);
            Ok(json!({ "recording": true, "storageStateCaptured": capture_ss }))
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

/// Resolve a snapshot ref to its durable `{role, name}` target (for recording).
fn resolve_target(
    browser: &Entity<BrowserView>,
    ref_id: &str,
    cx: &mut AsyncApp,
) -> Option<Target> {
    cx.update(|app| {
        browser
            .read(app)
            .item()
            .read(app)
            .resolve_automation_ref(ref_id)
            .map(|e| {
                // Only emit .nth(i) when the role+name was actually ambiguous.
                let index = (e.dup_count > 1).then_some(e.dup_index);
                Target::new(e.role, e.name)
                    .with_index(index)
                    .with_frame(e.frame_selector)
            })
    })
}

/// Resolve a ref to a full [`Target`]: role+name+dup-index (sync) plus the most
/// durable unique selector for the element (async CDP read). Best-effort.
async fn target_for(
    browser: &Entity<BrowserView>,
    ref_id: &str,
    cx: &mut AsyncApp,
) -> Option<Target> {
    let base = resolve_target(browser, ref_id, cx)?;
    let durable = commands::durable_selector(browser.clone(), ref_id, cx)
        .await
        .map(|(is_testid, value)| {
            if is_testid {
                recorder::DurableLoc::TestId(value)
            } else {
                recorder::DurableLoc::Css(value)
            }
        });
    Some(base.with_durable(durable))
}

/// Build a [`RecordedAction`] for a recordable method, resolving any refs to
/// role+name (+ durable selector) *before* the action runs. Returns `None` for
/// non-recordable methods (snapshot, evaluate, screenshot, storage, tabs, …).
async fn capture_pending_action(
    method: &str,
    params: &Value,
    browser: &Entity<BrowserView>,
    cx: &mut AsyncApp,
) -> Option<RecordedAction> {
    match method {
        "navigate" => {
            let url = params.get("url").and_then(|v| v.as_str())?.to_string();
            Some(RecordedAction::Navigate { url })
        }
        "navigate_back" => Some(RecordedAction::NavigateBack),
        "click" => {
            let ref_id = ref_str(params)?;
            let target = target_for(browser, &ref_id, cx).await?;
            let button = params
                .get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left")
                .to_string();
            let double = params
                .get("doubleClick")
                .or_else(|| params.get("double"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let modifiers = parse_string_list(params.get("modifiers")).unwrap_or_default();
            Some(RecordedAction::Click {
                target,
                button,
                double,
                modifiers,
            })
        }
        "type" => {
            let ref_id = ref_str(params)?;
            let target = target_for(browser, &ref_id, cx).await?;
            let text = params.get("text").and_then(|v| v.as_str())?.to_string();
            let submit = params.get("submit").and_then(|v| v.as_bool()) == Some(true);
            let slowly = params.get("slowly").and_then(|v| v.as_bool()) == Some(true);
            let delay_ms = params
                .get("slowlyDelayMs")
                .or_else(|| params.get("slowly_delay_ms"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(RecordedAction::Type {
                target,
                text,
                submit,
                slowly,
                delay_ms,
            })
        }
        "press_key" => {
            let key = params.get("key").and_then(|v| v.as_str())?.to_string();
            Some(RecordedAction::PressKey { key })
        }
        "hover" => {
            let ref_id = ref_str(params)?;
            Some(RecordedAction::Hover {
                target: target_for(browser, &ref_id, cx).await?,
            })
        }
        "scroll" => {
            let ref_id = params
                .get("ref")
                .or_else(|| params.get("target"))
                .and_then(|v| v.as_str());
            if let Some(ref_id) = ref_id {
                Some(RecordedAction::ScrollTo {
                    target: target_for(browser, ref_id, cx).await?,
                })
            } else {
                let dx = params.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let dy = params.get("dy").and_then(|v| v.as_f64()).unwrap_or(0.0);
                Some(RecordedAction::ScrollBy { dx, dy })
            }
        }
        "select_option" => {
            let ref_id = ref_str(params)?;
            let values = parse_string_list(params.get("values"))?;
            Some(RecordedAction::SelectOption {
                target: target_for(browser, &ref_id, cx).await?,
                values,
            })
        }
        "file_upload" => {
            let ref_id = ref_str(params)?;
            let paths =
                parse_string_list(params.get("paths").or_else(|| params.get("files")))?;
            Some(RecordedAction::FileUpload {
                target: target_for(browser, &ref_id, cx).await?,
                paths,
            })
        }
        "drag" => {
            let start = params
                .get("startRef")
                .or_else(|| params.get("start"))
                .or_else(|| params.get("from"))
                .and_then(|v| v.as_str())?;
            let end = params
                .get("endRef")
                .or_else(|| params.get("end"))
                .or_else(|| params.get("to"))
                .and_then(|v| v.as_str())?;
            Some(RecordedAction::Drag {
                from: target_for(browser, start, cx).await?,
                to: target_for(browser, end, cx).await?,
            })
        }
        "fill_form" => {
            let arr = params.get("fields").and_then(|v| v.as_array())?;
            let mut fields = Vec::with_capacity(arr.len());
            for item in arr {
                let ref_id = item
                    .get("ref")
                    .or_else(|| item.get("target"))
                    .and_then(|v| v.as_str())?;
                let value = match item.get("value") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Bool(b)) => b.to_string(),
                    Some(Value::Number(n)) => n.to_string(),
                    _ => String::new(),
                };
                let kind = item.get("type").and_then(|v| v.as_str()).map(str::to_string);
                fields.push(FormFieldRec {
                    target: target_for(browser, ref_id, cx).await?,
                    value,
                    kind,
                });
            }
            Some(RecordedAction::FillForm { fields })
        }
        "handle_dialog" => {
            let accept = params.get("accept").and_then(|v| v.as_bool()).unwrap_or(true);
            let prompt_text = params
                .get("promptText")
                .or_else(|| params.get("prompt_text"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(RecordedAction::HandleDialog {
                accept,
                prompt_text,
            })
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
                .and_then(|v| v.as_bool())
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
            dx: params.get("deltaX").and_then(|v| v.as_f64()).unwrap_or(0.0),
            dy: params.get("deltaY").and_then(|v| v.as_f64()).unwrap_or(0.0),
        }),
        "wait_for" => {
            let text = params.get("text").and_then(|v| v.as_str());
            let text_gone = params
                .get("textGone")
                .or_else(|| params.get("text_gone"))
                .and_then(|v| v.as_str());
            if let Some(text) = text {
                Some(RecordedAction::WaitForText {
                    text: text.to_string(),
                })
            } else {
                text_gone.map(|t| RecordedAction::WaitForTextGone { text: t.to_string() })
            }
        }
        "verify_element_visible" => Some(RecordedAction::VerifyElementVisible {
            target: target_for(browser, &ref_str(params)?, cx).await?,
        }),
        "verify_list_visible" => Some(RecordedAction::VerifyListVisible {
            target: target_for(browser, &ref_str(params)?, cx).await?,
        }),
        "verify_text_visible" => {
            let text = params.get("text").and_then(|v| v.as_str())?.to_string();
            Some(RecordedAction::VerifyTextVisible { text })
        }
        "verify_value" => {
            let ref_id = ref_str(params)?;
            let value = params.get("value").and_then(|v| v.as_str())?.to_string();
            Some(RecordedAction::VerifyValue {
                target: target_for(browser, &ref_id, cx).await?,
                value,
            })
        }
        // Not recorded: snapshot, evaluate, screenshot, drop (no PW analog),
        // console/network, resize, pdf_save, cookies/storage, set_storage_state.
        _ => None,
    }
}

/// Ref/target string from params (no error — recording is best-effort).
fn ref_str(params: &Value) -> Option<String> {
    params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// First present numeric param among `keys` (no error — for recording).
fn num_opt(params: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| params.get(*k).and_then(|v| v.as_f64()))
}

async fn dispatch_tabs(params: Value, cx: &mut AsyncApp) -> Result<Value> {
    let (window, workspace) = cx
        .update(|app| resolve_automation_workspace_global(app))
        .ok_or_else(|| anyhow!("No Zed workspace window found"))?;

    let action = params
        .get("action")
        .and_then(|v| v.as_str())
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
            let url = match params.get("url").and_then(|v| v.as_str()) {
                Some(url) => SharedString::new(url.to_string()),
                None => cx.update(|app| SharedString::new(BrowserSettings::get_global(app).homepage.clone())),
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
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .ok_or_else(|| anyhow!("this tabs action requires params.index (a tab number from `list`)"))
}

fn ref_from_params(params: &Value) -> Result<String> {
    params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing ref/target — run browser_snapshot first"))
}

/// Web-storage kind from params (`store` = "local"/"session"; default local).
fn store_from_params(params: &Value) -> String {
    params
        .get("store")
        .and_then(|v| v.as_str())
        .unwrap_or("local")
        .to_string()
}

/// Required `key` param (web storage).
fn key_from_params(params: &Value) -> Result<String> {
    params
        .get("key")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing params.key"))
}

/// Required numeric param under any of `keys`.
fn num_param(params: &Value, keys: &[&str]) -> Result<f64> {
    keys.iter()
        .find_map(|k| params.get(*k).and_then(|v| v.as_f64()))
        .ok_or_else(|| anyhow!("missing numeric param (one of {keys:?})"))
}

/// Required `x` / `y` coordinate pair.
fn xy_from_params(params: &Value) -> Result<(f64, f64)> {
    Ok((num_param(params, &["x"])?, num_param(params, &["y"])?))
}

/// Mouse button name from params (default left).
fn button_from_params(params: &Value) -> String {
    params
        .get("button")
        .and_then(|v| v.as_str())
        .unwrap_or("left")
        .to_string()
}

/// Parse `fill_form` `fields`: `[{ ref|target, value, type? }, …]`.
fn parse_form_fields(value: Option<&Value>) -> Result<Vec<commands::FormField>> {
    let arr = value
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("fill_form requires params.fields (array)"))?;
    let mut fields = Vec::with_capacity(arr.len());
    for item in arr {
        let ref_id = item
            .get("ref")
            .or_else(|| item.get("target"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("each fill_form field needs a ref"))?
            .to_string();
        let value = match item.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let kind = item.get("type").and_then(|v| v.as_str()).map(str::to_string);
        fields.push(commands::FormField { ref_id, value, kind });
    }
    Ok(fields)
}

/// Accept either a single string or an array of strings (for `select_option`).
fn parse_string_list(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_response_serializes_ok() {
        let line = ipc_ok_response(json!("1"), json!({ "yaml": "- button" }));
        assert!(line.contains("\"ok\":true"));
    }

    #[test]
    fn parse_string_list_accepts_string_and_array() {
        assert_eq!(parse_string_list(Some(&json!("M"))), Some(vec!["M".to_string()]));
        assert_eq!(
            parse_string_list(Some(&json!(["S", "L"]))),
            Some(vec!["S".to_string(), "L".to_string()])
        );
        assert_eq!(parse_string_list(Some(&json!(42))), None);
        assert_eq!(parse_string_list(None), None);
    }
}
