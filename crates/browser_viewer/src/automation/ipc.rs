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
use crate::automation::tabs;
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

    match request.method.as_str() {
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
    }
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
