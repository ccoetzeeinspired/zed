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

use crate::automation::commands;
use crate::automation::navigate::{WaitForOptions, DEFAULT_NAV_TIMEOUT};
use crate::automation::target::resolve_automation_target_global;

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
            commands::click(browser, &ref_id, cx).await?;
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
            commands::type_text(browser.clone(), &ref_id, text, submit, cx).await?;
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
        "navigate" => {
            let url = request
                .params
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("navigate requires params.url"))?;
            let final_url = commands::navigate(browser, url, cx).await?;
            Ok(json!({ "url": final_url }))
        }
        "wait_for" => {
            if let Some(secs) = request.params.get("time").and_then(|v| v.as_f64()) {
                if secs > 0.0 {
                    cx.background_executor()
                        .timer(Duration::from_secs_f64(secs))
                        .await;
                }
                return Ok(json!({ "waited_seconds": secs }));
            }
            let text = request
                .params
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            commands::wait_for(
                browser,
                WaitForOptions {
                    wait_load: request
                        .params
                        .get("wait_load")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    text,
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

fn ref_from_params(params: &Value) -> Result<String> {
    params
        .get("ref")
        .or_else(|| params.get("target"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("missing ref/target — run browser_snapshot first"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_response_serializes_ok() {
        let line = ipc_ok_response(json!("1"), json!({ "yaml": "- button" }));
        assert!(line.contains("\"ok\":true"));
    }
}
