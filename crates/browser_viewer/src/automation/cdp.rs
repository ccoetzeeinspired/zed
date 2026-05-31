//! CDP session helpers for embedded browser automation.

use anyhow::{Context as _, Result, anyhow};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler,
    Microsoft::Web::WebView2::Win32::ICoreWebView2,
};
use windows::core::{HSTRING, PCWSTR};

use crate::webview2_host::WebView2Session;

/// Thin wrapper around [`WebView2Session`] CDP calls used by the automation
/// layer. All methods take a completion callback because WebView2 CDP is
/// async on the Win32 message loop.
pub struct CdpSession<'a> {
    session: &'a WebView2Session,
}

impl<'a> CdpSession<'a> {
    pub fn new(session: &'a WebView2Session) -> Self {
        Self { session }
    }

    /// Enable the Accessibility domain (required before snapshots in CP1).
    pub fn enable_accessibility(
        &self,
        on_done: Box<dyn FnOnce(Result<()>) + 'static>,
    ) -> Result<()> {
        self.call_method(
            "Accessibility.enable",
            "{}",
            Box::new(move |result: Result<Value>| {
                on_done(result.map(|_| ()))
            }),
        )
    }

    pub fn get_full_ax_tree(
        &self,
        on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
    ) -> Result<()> {
        self.call_method("Accessibility.getFullAXTree", "{}", on_done)
    }

    /// Enable accessibility, then fetch the full AX tree in one chained call.
    pub fn fetch_full_ax_tree(
        &self,
        on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
    ) -> Result<()> {
        let webview = self.session.webview.clone();
        let completion: Completion<Value> = Rc::new(RefCell::new(Some(on_done)));
        let enable_completion = completion.clone();
        self.call_method(
            "Accessibility.enable",
            "{}",
            Box::new(move |enable_result| match enable_result {
                Err(err) => finish(&enable_completion, Err(err)),
                Ok(_) => {
                    let tree_completion = completion.clone();
                    let _ = call_devtools_on_webview(
                        &webview,
                        "Accessibility.getFullAXTree",
                        "{}",
                        Box::new(move |tree_raw| {
                            finish(
                                &tree_completion,
                                tree_raw.and_then(parse_cdp_response),
                            );
                        }),
                    );
                }
            }),
        )
    }

    pub fn call_method(
        &self,
        method: &str,
        params_json: &str,
        on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
    ) -> Result<()> {
        self.session.call_devtools_protocol(
            method,
            params_json,
            Box::new(move |result| {
                let result = result.and_then(parse_cdp_response);
                on_done(result);
            }),
        )
    }

    /// CP8: set the files on a `<input type=file>` by backend node id
    /// (`DOM.enable` then `DOM.setFileInputFiles`). Paths must exist on disk.
    pub fn set_file_input_files(
        &self,
        backend_node_id: i32,
        files: &[String],
        on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
    ) -> Result<()> {
        let webview = self.session.webview.clone();
        let params = serde_json::json!({
            "files": files,
            "backendNodeId": backend_node_id,
        })
        .to_string();
        let completion: Completion<Value> = Rc::new(RefCell::new(Some(on_done)));
        let enable_completion = completion.clone();
        self.call_method(
            "DOM.enable",
            "{}",
            Box::new(move |enable_result| match enable_result {
                Err(err) => finish(&enable_completion, Err(err)),
                Ok(_) => {
                    let set_completion = completion.clone();
                    let _ = call_devtools_on_webview(
                        &webview,
                        "DOM.setFileInputFiles",
                        &params,
                        Box::new(move |raw| {
                            finish(&set_completion, raw.and_then(parse_cdp_response));
                        }),
                    );
                }
            }),
        )
    }

    /// Enable a CDP domain (`enable_method`, e.g. `"Network.enable"`) then call
    /// `method` with `params_json`. Used by the cookie tools.
    pub fn call_with_domain_enabled(
        &self,
        enable_method: &str,
        method: &str,
        params_json: &str,
        on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
    ) -> Result<()> {
        let webview = self.session.webview.clone();
        let method = method.to_string();
        let params = params_json.to_string();
        let completion: Completion<Value> = Rc::new(RefCell::new(Some(on_done)));
        let enable_completion = completion.clone();
        self.call_method(
            enable_method,
            "{}",
            Box::new(move |enable_result| match enable_result {
                Err(err) => finish(&enable_completion, Err(err)),
                Ok(_) => {
                    let call_completion = completion.clone();
                    let _ = call_devtools_on_webview(
                        &webview,
                        &method,
                        &params,
                        Box::new(move |raw| {
                            finish(&call_completion, raw.and_then(parse_cdp_response));
                        }),
                    );
                }
            }),
        )
    }

    /// Evaluate a JS expression in the page main world (`Runtime.evaluate`).
    pub fn evaluate_expression(
        &self,
        expression: &str,
        on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": false,
        })
        .to_string();
        self.call_method("Runtime.evaluate", &params, on_done)
    }
}

/// Parse a CDP response from WebView2.
///
/// WebView2's `CallDevToolsProtocolMethod` returns the **method result object**
/// directly (see Microsoft docs: "return object as a JSON string"), not the
/// full CDP wire envelope `{ "id", "result" }`. Some callers/tests may still
/// pass the envelope form — both are accepted.
pub fn parse_cdp_response(raw: String) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let value: Value =
        serde_json::from_str(trimmed).with_context(|| format!("invalid CDP JSON: {raw}"))?;
    if let Some(error) = value.get("error") {
        return Err(anyhow!("CDP error: {error}"));
    }
    if let Some(result) = value.get("result") {
        return Ok(result.clone());
    }
    Ok(value)
}

pub(crate) type Completion<T> = Rc<RefCell<Option<Box<dyn FnOnce(Result<T>) + 'static>>>>;

pub(crate) fn finish<T>(completion: &Completion<T>, result: Result<T>) {
    if let Some(done) = completion.borrow_mut().take() {
        done(result);
    }
}

pub(crate) fn call_devtools_on_webview(
    webview: &ICoreWebView2,
    method: &str,
    params_json: &str,
    on_done: Box<dyn FnOnce(Result<String>) + 'static>,
) -> Result<()> {
    let method_h = HSTRING::from(method);
    let params_h = HSTRING::from(params_json);
    let completion: Completion<String> = Rc::new(RefCell::new(Some(on_done)));
    let handler_completion = completion.clone();
    let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
        move |hr, result_json| {
            let result = if let Err(err) = hr {
                Err(anyhow!("CDP call failed: {err}"))
            } else {
                Ok(result_json)
            };
            finish(&handler_completion, result);
            Ok(())
        },
    ));
    match unsafe {
        webview.CallDevToolsProtocolMethod(
            PCWSTR(method_h.as_ptr()),
            PCWSTR(params_h.as_ptr()),
            &handler,
        )
    } {
        Ok(()) => Ok(()),
        Err(err) => {
            finish(
                &completion,
                Err(anyhow!("CallDevToolsProtocolMethod: {err}")),
            );
            Err(anyhow!("CallDevToolsProtocolMethod: {err}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cdp_response_extracts_result() {
        let raw = r#"{"id":1,"result":{"enabled":true}}"#;
        let result = parse_cdp_response(raw.to_string()).unwrap();
        assert_eq!(result["enabled"], true);
    }

    #[test]
    fn parse_cdp_response_surfaces_error() {
        let raw = r#"{"id":1,"error":{"code":-32000,"message":"fail"}}"#;
        assert!(parse_cdp_response(raw.to_string()).is_err());
    }

    #[test]
    fn parse_cdp_response_accepts_webview2_direct_empty_result() {
        let result = parse_cdp_response("{}".to_string()).unwrap();
        assert!(result.as_object().is_some_and(|o| o.is_empty()));
    }

    #[test]
    fn parse_cdp_response_accepts_webview2_direct_result_object() {
        let raw = r#"{"root":{"nodeId":1,"role":{"value":"RootWebArea"}}}"#;
        let result = parse_cdp_response(raw.to_string()).unwrap();
        assert!(result.get("root").is_some());
    }
}
