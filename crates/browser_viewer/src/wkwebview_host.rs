//! macOS WKWebView host and automation session.

use std::cell::Cell;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::rc::Rc;
use std::slice;

use anyhow::{Result, anyhow};
use base64::Engine as _;
use block::ConcreteBlock;
use cocoa::{
    appkit::{NSView, NSViewHeightSizable, NSViewWidthSizable, NSWindowOrderingMode},
    base::{NO, YES, id, nil},
    foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL},
};
use gpui::{Bounds, Pixels};
use objc::{class, msg_send, sel, sel_impl};
use serde_json::Value;

use crate::automation::{
    BrowserPlatform, ElementHandle,
    platform::{
        AutomationSession, JsonCallback, KeyDispatch, MouseDispatch, UnitCallback,
        unsupported_capability,
    },
};

#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

pub type NativeView = id;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WKPageState {
    pub url: Option<String>,
    pub title: Option<String>,
    pub is_loading: bool,
}

pub struct WKWebViewSession {
    parent: NativeView,
    host: NativeView,
    container: id,
    webview: id,
}

impl WKWebViewSession {
    pub fn initialize(parent: NativeView, bounds: Bounds<Pixels>, url: &str) -> Result<Self> {
        let config: id = unsafe { msg_send![class!(WKWebViewConfiguration), new] };
        if config == nil {
            return Err(anyhow!("WKWebViewConfiguration allocation failed"));
        }
        install_automation_user_scripts(config)?;

        let container: id = unsafe { msg_send![class!(NSView), alloc] };
        if container == nil {
            return Err(anyhow!("NSView allocation failed"));
        }
        let host = unsafe {
            let superview: id = msg_send![parent, superview];
            if superview == nil { parent } else { superview }
        };
        let frame = appkit_frame(parent, host, bounds);
        let container: id = unsafe { msg_send![container, initWithFrame: frame] };
        if container == nil {
            return Err(anyhow!("NSView init failed"));
        }

        let webview: id = unsafe { msg_send![class!(WKWebView), alloc] };
        if webview == nil {
            return Err(anyhow!("WKWebView allocation failed"));
        }
        let webview_frame = appkit_container_bounds(bounds);
        let webview: id =
            unsafe { msg_send![webview, initWithFrame: webview_frame configuration: config] };
        if webview == nil {
            return Err(anyhow!("WKWebView init failed"));
        }

        unsafe {
            let autoresizing = NSViewWidthSizable | NSViewHeightSizable;
            let _: () = msg_send![container, setAutoresizingMask: autoresizing];
            let _: () = msg_send![container, setWantsLayer: YES];
            let layer: id = msg_send![container, layer];
            if layer != nil {
                let _: () = msg_send![layer, setMasksToBounds: YES];
            }
            let _: () = msg_send![webview, setAutoresizingMask: autoresizing];
            let _: () = msg_send![container, addSubview: webview];
            if host == parent {
                let _: () = msg_send![parent, addSubview: container];
            } else {
                let _: () = msg_send![
                    host,
                    addSubview: container
                    positioned: NSWindowOrderingMode::NSWindowBelow
                    relativeTo: parent
                ];
            }
        }

        let session = Self {
            parent,
            host,
            container,
            webview,
        };
        session.navigate(url)?;
        Ok(session)
    }

    pub fn set_rect(&self, bounds: Bounds<Pixels>) -> Result<()> {
        let frame = appkit_frame(self.parent, self.host, bounds);
        let webview_frame = appkit_container_bounds(bounds);
        unsafe {
            let _: () = msg_send![self.container, setFrame: frame];
            let _: () = msg_send![self.webview, setFrame: webview_frame];
        }
        Ok(())
    }

    pub fn set_visible(&self, visible: bool) -> Result<()> {
        unsafe {
            let hidden = if visible { NO } else { YES };
            let _: () = msg_send![self.container, setHidden: hidden];
            let _: () = msg_send![self.webview, setHidden: hidden];
        }
        Ok(())
    }

    pub fn release_keyboard_focus(&self) {
        unsafe {
            let blur_script = NSString::alloc(nil).init_str(
                "document.activeElement && document.activeElement.blur && document.activeElement.blur();",
            );
            let _: () =
                msg_send![self.webview, evaluateJavaScript: blur_script completionHandler: nil];

            let window: id = msg_send![self.webview, window];
            if window == nil {
                return;
            }

            let first_responder: id = msg_send![window, firstResponder];
            let browser_has_focus = first_responder == nil
                || first_responder == self.webview
                || first_responder == self.container
                || (ns_is_kind_of(first_responder, "NSView")
                    && (msg_send![first_responder, isDescendantOf: self.webview]
                        || msg_send![first_responder, isDescendantOf: self.container]));

            if browser_has_focus {
                let _: () = msg_send![window, makeFirstResponder: self.parent];
            }
        }
    }

    pub fn navigate(&self, url: &str) -> Result<()> {
        let ns_url_string = unsafe { NSString::alloc(nil).init_str(url) };
        let ns_url = unsafe { NSURL::URLWithString_(nil, ns_url_string) };
        if ns_url == nil {
            return Err(anyhow!("invalid WKWebView URL: {url}"));
        }
        let request: id = unsafe { msg_send![class!(NSURLRequest), requestWithURL: ns_url] };
        if request == nil {
            return Err(anyhow!("NSURLRequest allocation failed for {url}"));
        }
        unsafe {
            let _: id = msg_send![self.webview, loadRequest: request];
        }
        Ok(())
    }

    pub fn page_state(&self) -> WKPageState {
        unsafe {
            let url: id = msg_send![self.webview, URL];
            let absolute_url: id = if url == nil {
                nil
            } else {
                msg_send![url, absoluteString]
            };
            let title: id = msg_send![self.webview, title];
            let is_loading: bool = msg_send![self.webview, isLoading];

            WKPageState {
                url: ns_string_to_string(absolute_url),
                title: ns_string_to_string(title),
                is_loading,
            }
        }
    }

    pub fn can_go_back(&self) -> bool {
        unsafe { msg_send![self.webview, canGoBack] }
    }

    pub fn go_back(&self) -> Result<()> {
        unsafe {
            let _: id = msg_send![self.webview, goBack];
        }
        Ok(())
    }

    pub fn capture_screenshot(
        &self,
        format: &str,
        _quality: Option<i64>,
        on_done: JsonCallback,
    ) -> Result<()> {
        let format = screenshot_format(format)?;
        let on_done = Cell::new(Some(on_done));
        let block = ConcreteBlock::new(move |image: id, error: id| {
            let result = unsafe {
                if error != nil {
                    let message = ns_error_description(error)
                        .unwrap_or_else(|| "WKWebView snapshot failed".to_string());
                    Err(anyhow!(message))
                } else if image == nil {
                    Err(anyhow!("WKWebView snapshot returned no image"))
                } else {
                    ns_image_to_screenshot_json(image, format)
                }
            };

            if let Some(on_done) = on_done.take() {
                on_done(result);
            }
        });
        let block = block.copy();
        unsafe {
            let _: () = msg_send![self.webview, takeSnapshotWithConfiguration: nil completionHandler: block];
        }
        Ok(())
    }

    pub fn create_pdf(
        &self,
        _landscape: bool,
        _print_background: bool,
        on_done: JsonCallback,
    ) -> Result<()> {
        let config: id = unsafe { msg_send![class!(WKPDFConfiguration), new] };
        if config == nil {
            return Err(anyhow!("WKPDFConfiguration allocation failed"));
        }

        let on_done = Cell::new(Some(on_done));
        let block = ConcreteBlock::new(move |data: id, error: id| {
            let result = unsafe {
                if error != nil {
                    let message = ns_error_description(error)
                        .unwrap_or_else(|| "WKWebView PDF generation failed".to_string());
                    Err(anyhow!(message))
                } else if data == nil {
                    Err(anyhow!("WKWebView PDF generation returned no data"))
                } else {
                    ns_data_to_pdf_json(data)
                }
            };

            if let Some(on_done) = on_done.take() {
                on_done(result);
            }
        });
        let block = block.copy();
        unsafe {
            let _: () = msg_send![self.webview, createPDFWithConfiguration: config completionHandler: block];
        }
        Ok(())
    }

    pub fn get_cookies(&self, on_done: JsonCallback) -> Result<()> {
        let store = self.http_cookie_store()?;
        let on_done = Cell::new(Some(on_done));
        let block = ConcreteBlock::new(move |cookies: id| {
            let result = unsafe { ns_cookies_to_json(cookies) };
            if let Some(on_done) = on_done.take() {
                on_done(result);
            }
        });
        let block = block.copy();
        unsafe {
            let _: () = msg_send![store, getAllCookies: block];
        }
        Ok(())
    }

    pub fn set_cookie(&self, cookie: Value, on_done: JsonCallback) -> Result<()> {
        let store = self.http_cookie_store()?;
        let cookie = unsafe { ns_cookie_from_json(&cookie) }?;
        let on_done = Cell::new(Some(on_done));
        let block = ConcreteBlock::new(move || {
            if let Some(on_done) = on_done.take() {
                on_done(Ok(serde_json::json!({ "success": true })));
            }
        });
        let block = block.copy();
        unsafe {
            let _: () = msg_send![store, setCookie: cookie completionHandler: block];
        }
        Ok(())
    }

    pub fn delete_cookies_named(&self, name: String, on_done: JsonCallback) -> Result<()> {
        let store = self.http_cookie_store()?;
        let on_done = Rc::new(Cell::new(Some(on_done)));
        let block = {
            let on_done = Rc::clone(&on_done);
            ConcreteBlock::new(move |cookies: id| {
                let deleted_name = name.clone();
                let cookies = unsafe { ns_cookies_named(cookies, &name) };
                delete_cookie_batch(
                    store,
                    cookies,
                    Rc::clone(&on_done),
                    move |count| serde_json::json!({ "deleted": deleted_name, "count": count }),
                );
            })
        };
        let block = block.copy();
        unsafe {
            let _: () = msg_send![store, getAllCookies: block];
        }
        Ok(())
    }

    pub fn clear_cookies(&self, on_done: JsonCallback) -> Result<()> {
        let store = self.http_cookie_store()?;
        let on_done = Rc::new(Cell::new(Some(on_done)));
        let block = {
            let on_done = Rc::clone(&on_done);
            ConcreteBlock::new(move |cookies: id| {
                let cookies = unsafe { ns_cookie_array(cookies) };
                delete_cookie_batch(
                    store,
                    cookies,
                    Rc::clone(&on_done),
                    |count| serde_json::json!({ "cleared": true, "count": count }),
                );
            })
        };
        let block = block.copy();
        unsafe {
            let _: () = msg_send![store, getAllCookies: block];
        }
        Ok(())
    }

    fn http_cookie_store(&self) -> Result<id> {
        let configuration: id = unsafe { msg_send![self.webview, configuration] };
        if configuration == nil {
            return Err(anyhow!("WKWebView configuration unavailable"));
        }
        let data_store: id = unsafe { msg_send![configuration, websiteDataStore] };
        if data_store == nil {
            return Err(anyhow!("WKWebsiteDataStore unavailable"));
        }
        let cookie_store: id = unsafe { msg_send![data_store, httpCookieStore] };
        if cookie_store == nil {
            return Err(anyhow!("WKHTTPCookieStore unavailable"));
        }
        Ok(cookie_store)
    }
}

impl Drop for WKWebViewSession {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![self.webview, stopLoading];
            let _: () = msg_send![self.webview, removeFromSuperview];
            let _: () = msg_send![self.container, removeFromSuperview];
            let _: () = msg_send![self.webview, release];
            let _: () = msg_send![self.container, release];
            let _: () = msg_send![self.parent, setNeedsDisplay: YES];
        }
    }
}

fn install_automation_user_scripts(config: id) -> Result<()> {
    let user_content_controller: id = unsafe { msg_send![class!(WKUserContentController), new] };
    if user_content_controller == nil {
        return Err(anyhow!("WKUserContentController allocation failed"));
    }

    let source =
        unsafe { NSString::alloc(nil).init_str(crate::automation::instrumentation::SCRIPT) };
    let injection_time: usize = 0; // WKUserScriptInjectionTimeAtDocumentStart
    let user_script: id = unsafe { msg_send![class!(WKUserScript), alloc] };
    if user_script == nil {
        return Err(anyhow!("WKUserScript allocation failed"));
    }
    let user_script: id = unsafe {
        msg_send![
            user_script,
            initWithSource: source
            injectionTime: injection_time
            forMainFrameOnly: NO
        ]
    };
    if user_script == nil {
        return Err(anyhow!("WKUserScript init failed"));
    }

    unsafe {
        let _: () = msg_send![user_content_controller, addUserScript: user_script];
        let _: () = msg_send![config, setUserContentController: user_content_controller];
    }
    Ok(())
}

fn appkit_frame(parent: NativeView, host: NativeView, bounds: Bounds<Pixels>) -> NSRect {
    let parent_bounds = unsafe { NSView::bounds(parent) };
    let x = f64::from(bounds.origin.x);
    let width = f64::from(bounds.size.width).max(0.0);
    let height = f64::from(bounds.size.height).max(0.0);
    let top = f64::from(bounds.origin.y);
    let y = (parent_bounds.size.height - top - height).max(0.0);
    let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
    if host == parent {
        frame
    } else {
        unsafe { msg_send![parent, convertRect: frame toView: host] }
    }
}

fn appkit_container_bounds(bounds: Bounds<Pixels>) -> NSRect {
    let width = f64::from(bounds.size.width).max(0.0);
    let height = f64::from(bounds.size.height).max(0.0);
    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height))
}

impl AutomationSession for WKWebViewSession {
    fn platform(&self) -> BrowserPlatform {
        BrowserPlatform::WkWebView
    }

    fn call_cdp(&self, method: &str, params_json: &str, on_done: JsonCallback) -> Result<()> {
        let _ = (method, params_json, on_done);
        Err(unsupported_capability(self.platform(), "call_cdp"))
    }

    fn evaluate_expression(
        &self,
        expression: &str,
        await_promise: bool,
        on_done: JsonCallback,
    ) -> Result<()> {
        if await_promise {
            return evaluate_async_expression(self.webview, expression, on_done);
        }

        let script = wrap_wk_evaluate_script(expression)?;
        let script = unsafe { NSString::alloc(nil).init_str(&script) };
        let on_done = Cell::new(Some(on_done));
        let block = ConcreteBlock::new(move |value: id, error: id| {
            let result = unsafe {
                if error != nil {
                    let message = ns_error_description(error)
                        .unwrap_or_else(|| "WKWebView JavaScript evaluation failed".to_string());
                    Err(anyhow!(message))
                } else {
                    wk_evaluate_result_to_cdp(value)
                }
            };

            if let Some(on_done) = on_done.take() {
                on_done(result);
            }
        });
        let block = block.copy();
        unsafe {
            let _: () =
                msg_send![self.webview, evaluateJavaScript: script completionHandler: block];
        }
        Ok(())
    }

    fn invoke_on_element(
        &self,
        handle: ElementHandle,
        function_declaration: &str,
        arguments: Option<&[Value]>,
        on_done: JsonCallback,
    ) -> Result<()> {
        let _ = (handle, function_declaration, arguments, on_done);
        Err(unsupported_capability(self.platform(), "invoke_on_element"))
    }

    fn dispatch_key_event(&self, event: KeyDispatch, on_done: UnitCallback) -> Result<()> {
        let _ = (event, on_done);
        Err(unsupported_capability(
            self.platform(),
            "dispatch_key_event",
        ))
    }

    fn dispatch_mouse_event(&self, event: MouseDispatch, on_done: JsonCallback) -> Result<()> {
        let _ = (event, on_done);
        Err(unsupported_capability(
            self.platform(),
            "dispatch_mouse_event",
        ))
    }
}

fn evaluate_async_expression(webview: id, expression: &str, on_done: JsonCallback) -> Result<()> {
    let script = wrap_wk_async_script(expression)?;
    let script = unsafe { NSString::alloc(nil).init_str(&script) };
    let arguments: id = unsafe { msg_send![class!(NSDictionary), new] };
    let content_world: id = unsafe { msg_send![class!(WKContentWorld), pageWorld] };
    let on_done = Cell::new(Some(on_done));
    let block = ConcreteBlock::new(move |value: id, error: id| {
        let result = unsafe {
            if error != nil {
                let message = ns_error_description(error)
                    .unwrap_or_else(|| "WKWebView async JavaScript evaluation failed".to_string());
                Err(anyhow!(message))
            } else {
                wk_evaluate_result_to_cdp(value)
            }
        };

        if let Some(on_done) = on_done.take() {
            on_done(result);
        }
    });
    let block = block.copy();
    unsafe {
        let _: () = msg_send![
            webview,
            callAsyncJavaScript: script
            arguments: arguments
            inFrame: nil
            inContentWorld: content_world
            completionHandler: block
        ];
    }
    Ok(())
}

fn wrap_wk_evaluate_script(expression: &str) -> Result<String> {
    let expression = serde_json::to_string(expression)?;
    let value_expr = format!("(0, eval)({expression})");
    let success = r#"(value) => JSON.stringify({ ok: true, value })"#;
    let failure = r#"(error) => JSON.stringify({
        ok: false,
        error: String(error && (error.stack || error.message) || error)
    })"#;

    let body = format!(
        "try {{ return String(({success})({value_expr})); }} catch (error) {{ return String(({failure})(error)); }}"
    );
    Ok(format!("(() => {{ {body} }})()"))
}

fn wrap_wk_async_script(expression: &str) -> Result<String> {
    let expression = serde_json::to_string(expression)?;
    let value_expr = format!("(0, eval)({expression})");
    let success = r#"(value) => JSON.stringify({ ok: true, value })"#;
    let failure = r#"(error) => JSON.stringify({
        ok: false,
        error: String(error && (error.stack || error.message) || error)
    })"#;
    Ok(format!(
        "return String(await Promise.resolve({value_expr}).then({success}, {failure}));"
    ))
}

unsafe fn wk_evaluate_result_to_cdp(value: id) -> Result<Value> {
    if value == nil {
        return Ok(cdp_value_from_json(Value::Null));
    }
    if let Some(json) = unsafe { ns_string_to_string(value) } {
        let envelope = wk_evaluate_json_envelope(&json)?;
        return wk_evaluate_envelope_to_cdp(envelope);
    }
    let native_value = unsafe { ns_json_object_to_value(value) }?;
    if native_value.get("ok").is_some() {
        return wk_evaluate_envelope_to_cdp(native_value);
    }
    Ok(cdp_value_from_json(native_value))
}

fn wk_evaluate_json_to_cdp(json: &str) -> Result<Value> {
    wk_evaluate_envelope_to_cdp(wk_evaluate_json_envelope(json)?)
}

fn wk_evaluate_json_envelope(json: &str) -> Result<Value> {
    serde_json::from_str(json).map_err(|err| anyhow!("WKWebView result parse failed: {err}"))
}

fn wk_evaluate_envelope_to_cdp(envelope: Value) -> Result<Value> {
    if envelope.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = envelope
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("WKWebView JavaScript evaluation failed");
        return Err(anyhow!(message.to_string()));
    }
    Ok(cdp_value_from_json(
        envelope.get("value").cloned().unwrap_or(Value::Null),
    ))
}

unsafe fn ns_json_object_to_value(value: id) -> Result<Value> {
    if value == nil {
        return Ok(Value::Null);
    }
    if unsafe { ns_is_kind_of(value, "NSNull") } {
        return Ok(Value::Null);
    }
    if let Some(string) = unsafe { ns_string_to_string(value) } {
        return Ok(Value::String(string));
    }
    if unsafe { ns_is_kind_of(value, "NSNumber") } {
        return Ok(unsafe { ns_number_to_json(value) });
    }
    if unsafe { ns_is_kind_of(value, "NSArray") } {
        return unsafe { ns_array_to_json(value) };
    }
    if unsafe { ns_is_kind_of(value, "NSDictionary") } {
        return unsafe { ns_dictionary_to_json(value) };
    }

    let valid: bool = unsafe { msg_send![class!(NSJSONSerialization), isValidJSONObject: value] };
    if !valid {
        return Err(anyhow!(
            "WKWebView JavaScript result was not JSON-serializable ({})",
            unsafe { ns_object_debug_summary(value) }
        ));
    }

    let mut error: id = nil;
    let data: id = unsafe {
        msg_send![
            class!(NSJSONSerialization),
            dataWithJSONObject: value
            options: 0usize
            error: &mut error
        ]
    };
    if data == nil {
        let message = unsafe { ns_error_description(error) }
            .unwrap_or_else(|| "WKWebView JavaScript result serialization failed".to_string());
        return Err(anyhow!(message));
    }

    let len: usize = unsafe { msg_send![data, length] };
    let bytes: *const u8 = unsafe { msg_send![data, bytes] };
    if bytes.is_null() {
        return Err(anyhow!(
            "WKWebView JavaScript result serialization returned null bytes"
        ));
    }
    let json = std::str::from_utf8(unsafe { slice::from_raw_parts(bytes, len) })
        .map_err(|err| anyhow!("WKWebView result was not UTF-8 JSON: {err}"))?;
    serde_json::from_str(json).map_err(|err| anyhow!("WKWebView result parse failed: {err}"))
}

unsafe fn ns_object_debug_summary(value: id) -> String {
    if value == nil {
        return "nil".to_string();
    }

    let class_name = unsafe { ns_string_to_string(msg_send![value, className]) }
        .unwrap_or_else(|| "unknown class".to_string());
    let description = unsafe { ns_string_to_string(msg_send![value, description]) }
        .unwrap_or_else(|| "no description".to_string());
    format!("{class_name}: {description}")
}

unsafe fn ns_is_kind_of(value: id, class_name: &str) -> bool {
    let target_class = match class_name {
        "NSNull" => class!(NSNull),
        "NSNumber" => class!(NSNumber),
        "NSArray" => class!(NSArray),
        "NSDictionary" => class!(NSDictionary),
        "NSView" => class!(NSView),
        _ => return false,
    };
    unsafe { msg_send![value, isKindOfClass: target_class] }
}

unsafe fn ns_number_to_json(value: id) -> Value {
    let obj_c_type: *const c_char = unsafe { msg_send![value, objCType] };
    if !obj_c_type.is_null() {
        let ty = unsafe { CStr::from_ptr(obj_c_type) }.to_bytes();
        if ty == b"c" || ty == b"B" {
            let boolean: bool = unsafe { msg_send![value, boolValue] };
            return Value::Bool(boolean);
        }
    }
    let number: f64 = unsafe { msg_send![value, doubleValue] };
    serde_json::Number::from_f64(number)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

unsafe fn ns_array_to_json(value: id) -> Result<Value> {
    let count = unsafe { NSArray::count(value) } as usize;
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let item = unsafe { value.objectAtIndex(index as u64) };
        values.push(unsafe { ns_json_object_to_value(item) }?);
    }
    Ok(Value::Array(values))
}

unsafe fn ns_dictionary_to_json(value: id) -> Result<Value> {
    let keys: id = unsafe { msg_send![value, allKeys] };
    let count = unsafe { NSArray::count(keys) } as usize;
    let mut object = serde_json::Map::with_capacity(count);
    for index in 0..count {
        let key = unsafe { keys.objectAtIndex(index as u64) };
        let key = unsafe { ns_string_to_string(key) }.unwrap_or_else(|| format!("key-{index}"));
        let item: id = unsafe { msg_send![value, objectForKey: keys.objectAtIndex(index as u64)] };
        object.insert(key, unsafe { ns_json_object_to_value(item) }?);
    }
    Ok(Value::Object(object))
}

fn cdp_value_from_json(value: Value) -> Value {
    let type_name = match &value {
        Value::Null => "object",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) | Value::Object(_) => "object",
    };
    serde_json::json!({
        "result": {
            "type": type_name,
            "value": value,
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScreenshotFormat {
    Png,
    Jpeg,
}

fn screenshot_format(format: &str) -> Result<ScreenshotFormat> {
    match format.to_ascii_lowercase().as_str() {
        "png" => Ok(ScreenshotFormat::Png),
        "jpeg" | "jpg" => Ok(ScreenshotFormat::Jpeg),
        other => Err(anyhow!(
            "unsupported screenshot format {other:?}; use png or jpeg"
        )),
    }
}

unsafe fn ns_image_to_screenshot_json(image: id, format: ScreenshotFormat) -> Result<Value> {
    let tiff_data: id = unsafe { msg_send![image, TIFFRepresentation] };
    if tiff_data == nil {
        return Err(anyhow!("NSImage TIFFRepresentation failed"));
    }

    let rep: id = unsafe { msg_send![class!(NSBitmapImageRep), imageRepWithData: tiff_data] };
    if rep == nil {
        return Err(anyhow!("NSBitmapImageRep imageRepWithData failed"));
    }

    let properties: id = unsafe { msg_send![class!(NSDictionary), dictionary] };
    let storage_type: usize = match format {
        ScreenshotFormat::Png => 4,  // NSPNGFileType
        ScreenshotFormat::Jpeg => 3, // NSJPEGFileType
    };
    let data: id =
        unsafe { msg_send![rep, representationUsingType: storage_type properties: properties] };
    if data == nil {
        return Err(anyhow!("NSBitmapImageRep representationUsingType failed"));
    }

    let len: usize = unsafe { msg_send![data, length] };
    let bytes: *const u8 = unsafe { msg_send![data, bytes] };
    if bytes.is_null() {
        return Err(anyhow!("screenshot data had null bytes"));
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    let encoded = base64::engine::general_purpose::STANDARD.encode(slice);
    let mime = match format {
        ScreenshotFormat::Png => "image/png",
        ScreenshotFormat::Jpeg => "image/jpeg",
    };
    Ok(serde_json::json!({
        "data": encoded,
        "mimeType": mime,
        "bytes": len,
    }))
}

unsafe fn ns_data_to_pdf_json(data: id) -> Result<Value> {
    let len: usize = unsafe { msg_send![data, length] };
    let bytes: *const u8 = unsafe { msg_send![data, bytes] };
    if bytes.is_null() {
        return Err(anyhow!("PDF data had null bytes"));
    }
    let slice = unsafe { slice::from_raw_parts(bytes, len) };
    let encoded = base64::engine::general_purpose::STANDARD.encode(slice);
    Ok(serde_json::json!({
        "data": encoded,
        "bytes": len,
    }))
}

unsafe fn ns_cookies_to_json(cookies: id) -> Result<Value> {
    if cookies == nil {
        return Ok(serde_json::json!({ "cookies": [], "count": 0 }));
    }

    let count: usize = unsafe { msg_send![cookies, count] };
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let cookie: id = unsafe { msg_send![cookies, objectAtIndex: index] };
        if cookie != nil {
            rows.push(unsafe { ns_cookie_to_json(cookie) });
        }
    }
    Ok(serde_json::json!({
        "count": rows.len(),
        "cookies": rows,
    }))
}

unsafe fn ns_cookie_to_json(cookie: id) -> Value {
    let name: id = unsafe { msg_send![cookie, name] };
    let value: id = unsafe { msg_send![cookie, value] };
    let domain: id = unsafe { msg_send![cookie, domain] };
    let path: id = unsafe { msg_send![cookie, path] };
    let expires_date: id = unsafe { msg_send![cookie, expiresDate] };
    let same_site: id = unsafe { msg_send![cookie, sameSitePolicy] };
    let secure: bool = unsafe { msg_send![cookie, isSecure] };
    let http_only: bool = unsafe { msg_send![cookie, isHTTPOnly] };

    let expires = if expires_date == nil {
        Value::from(-1)
    } else {
        let seconds: f64 = unsafe { msg_send![expires_date, timeIntervalSince1970] };
        Value::from(seconds)
    };
    let same_site = unsafe { ns_string_to_string(same_site) };

    cookie_json_from_parts(
        unsafe { ns_string_to_string(name) },
        unsafe { ns_string_to_string(value) },
        unsafe { ns_string_to_string(domain) },
        unsafe { ns_string_to_string(path) },
        expires,
        secure,
        http_only,
        same_site,
    )
}

fn cookie_json_from_parts(
    name: Option<String>,
    value: Option<String>,
    domain: Option<String>,
    path: Option<String>,
    expires: Value,
    secure: bool,
    http_only: bool,
    same_site: Option<String>,
) -> Value {
    serde_json::json!({
        "name": name.unwrap_or_default(),
        "value": value.unwrap_or_default(),
        "domain": domain.unwrap_or_default(),
        "path": path.unwrap_or_else(|| "/".to_string()),
        "expires": expires,
        "secure": secure,
        "httpOnly": http_only,
        "sameSite": same_site,
    })
}

unsafe fn ns_cookie_from_json(cookie: &Value) -> Result<id> {
    let name = cookie
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("cookie_set requires cookie.name"))?;
    let value = cookie
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("cookie_set requires cookie.value"))?;
    let path = cookie.get("path").and_then(Value::as_str).unwrap_or("/");
    let mut properties = vec![
        cookie_property("Name", ns_string(name)),
        cookie_property("Value", ns_string(value)),
        cookie_property("Path", ns_string(path)),
    ];

    if let Some(domain) = cookie.get("domain").and_then(Value::as_str) {
        properties.push(cookie_property("Domain", ns_string(domain)));
    } else if let Some(url) = cookie.get("url").and_then(Value::as_str) {
        properties.push(cookie_property("OriginURL", ns_string(url)));
    } else {
        return Err(anyhow!("cookie_set requires cookie.domain or cookie.url"));
    }

    if cookie.get("secure").and_then(Value::as_bool) == Some(true) {
        properties.push(cookie_property("Secure", ns_bool(true)));
    }
    if cookie.get("httpOnly").and_then(Value::as_bool) == Some(true)
        || cookie.get("http_only").and_then(Value::as_bool) == Some(true)
    {
        properties.push(cookie_property("HttpOnly", ns_bool(true)));
    }
    if let Some(same_site) = cookie.get("sameSite").and_then(Value::as_str) {
        properties.push(cookie_property("SameSitePolicy", ns_string(same_site)));
    }
    if let Some(expires) = cookie.get("expires").and_then(Value::as_f64) {
        if expires >= 0.0 {
            let date: id =
                unsafe { msg_send![class!(NSDate), dateWithTimeIntervalSince1970: expires] };
            properties.push(cookie_property("Expires", date));
        }
    }

    let keys: Vec<id> = properties.iter().map(|(key, _)| *key).collect();
    let values: Vec<id> = properties.iter().map(|(_, value)| *value).collect();
    let dictionary: id = unsafe {
        msg_send![
            class!(NSDictionary),
            dictionaryWithObjects: values.as_ptr()
            forKeys: keys.as_ptr()
            count: properties.len()
        ]
    };
    let cookie: id = unsafe { msg_send![class!(NSHTTPCookie), cookieWithProperties: dictionary] };
    if cookie == nil {
        return Err(anyhow!("NSHTTPCookie creation failed"));
    }
    Ok(cookie)
}

fn cookie_property(key: &str, value: id) -> (id, id) {
    (ns_string(key), value)
}

fn ns_string(value: &str) -> id {
    unsafe { NSString::alloc(nil).init_str(value) }
}

fn ns_bool(value: bool) -> id {
    unsafe { msg_send![class!(NSNumber), numberWithBool: if value { YES } else { NO }] }
}

unsafe fn ns_cookie_array(cookies: id) -> Vec<id> {
    if cookies == nil {
        return Vec::new();
    }
    let count: usize = unsafe { msg_send![cookies, count] };
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let cookie: id = unsafe { msg_send![cookies, objectAtIndex: index] };
        if cookie != nil {
            unsafe {
                let _: () = msg_send![cookie, retain];
            }
            rows.push(cookie);
        }
    }
    rows
}

unsafe fn ns_cookies_named(cookies: id, expected_name: &str) -> Vec<id> {
    unsafe { ns_cookie_array(cookies) }
        .into_iter()
        .filter_map(|cookie| {
            let name: id = unsafe { msg_send![cookie, name] };
            if unsafe { ns_string_to_string(name) }.as_deref() == Some(expected_name) {
                Some(cookie)
            } else {
                unsafe {
                    let _: () = msg_send![cookie, release];
                }
                None
            }
        })
        .collect()
}

fn delete_cookie_batch<F>(
    store: id,
    cookies: Vec<id>,
    on_done: Rc<Cell<Option<JsonCallback>>>,
    result: F,
) where
    F: FnOnce(usize) -> Value + 'static,
{
    let count = cookies.len();
    if count == 0 {
        if let Some(on_done) = on_done.take() {
            on_done(Ok(result(0)));
        }
        return;
    }

    let remaining = Rc::new(Cell::new(count));
    let result = Rc::new(Cell::new(Some(result)));
    for cookie in cookies {
        let remaining = Rc::clone(&remaining);
        let on_done = Rc::clone(&on_done);
        let result = Rc::clone(&result);
        let block = ConcreteBlock::new(move || {
            unsafe {
                let _: () = msg_send![cookie, release];
            }
            let next = remaining.get().saturating_sub(1);
            remaining.set(next);
            if next == 0 {
                if let (Some(on_done), Some(result)) = (on_done.take(), result.take()) {
                    on_done(Ok(result(count)));
                }
            }
        });
        let block = block.copy();
        unsafe {
            let _: () = msg_send![store, deleteCookie: cookie completionHandler: block];
        }
    }
}

unsafe fn ns_error_description(error: id) -> Option<String> {
    let description: id = unsafe { msg_send![error, localizedDescription] };
    unsafe { ns_string_to_string(description) }
}

unsafe fn ns_string_to_string(value: id) -> Option<String> {
    if value == nil {
        return None;
    }
    let bytes: *const c_char = unsafe { msg_send![value, UTF8String] };
    if bytes.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(bytes) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp_value_wraps_json_values_with_runtime_type() {
        assert_eq!(
            cdp_value_from_json(Value::Bool(true)),
            serde_json::json!({ "result": { "type": "boolean", "value": true } })
        );
        assert_eq!(
            cdp_value_from_json(serde_json::json!({ "a": 1 })),
            serde_json::json!({ "result": { "type": "object", "value": { "a": 1 } } })
        );
    }

    #[test]
    fn wk_evaluate_json_to_cdp_surfaces_success_and_error() {
        assert_eq!(
            wk_evaluate_json_to_cdp(r#"{ "ok": true, "value": "ready" }"#).unwrap(),
            serde_json::json!({ "result": { "type": "string", "value": "ready" } })
        );

        let err = wk_evaluate_json_to_cdp(r#"{ "ok": false, "error": "boom" }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"));
    }

    #[test]
    fn wrap_wk_evaluate_script_embeds_expression_as_json_string() {
        let script = wrap_wk_evaluate_script("document.title").unwrap();
        assert!(script.contains("(0, eval)(\"document.title\")"));
        assert!(script.contains("JSON.stringify"));

        let script = wrap_wk_async_script("fetch('/').then(r => r.status)").unwrap();
        assert!(script.starts_with("return String(await "));
        assert!(script.contains("Promise.resolve"));
        assert!(script.contains("fetch('/').then(r => r.status)"));
    }

    #[test]
    fn screenshot_format_accepts_png_and_jpeg_aliases() {
        assert_eq!(screenshot_format("png").unwrap(), ScreenshotFormat::Png);
        assert_eq!(screenshot_format("jpeg").unwrap(), ScreenshotFormat::Jpeg);
        assert_eq!(screenshot_format("jpg").unwrap(), ScreenshotFormat::Jpeg);
        assert!(screenshot_format("webp").is_err());
    }

    #[test]
    fn cookie_json_from_parts_preserves_storage_state_shape() {
        assert_eq!(
            cookie_json_from_parts(
                Some("sid".to_string()),
                Some("abc".to_string()),
                Some("example.com".to_string()),
                Some("/app".to_string()),
                Value::from(1_780_000_000.0),
                true,
                true,
                Some("Lax".to_string()),
            ),
            serde_json::json!({
                "name": "sid",
                "value": "abc",
                "domain": "example.com",
                "path": "/app",
                "expires": 1_780_000_000.0,
                "secure": true,
                "httpOnly": true,
                "sameSite": "Lax",
            })
        );
    }

    #[test]
    fn cookie_json_from_parts_defaults_session_cookie_fields() {
        let cookie = cookie_json_from_parts(
            Some("session".to_string()),
            Some("value".to_string()),
            None,
            None,
            Value::from(-1),
            false,
            false,
            None,
        );
        assert_eq!(cookie["domain"], "");
        assert_eq!(cookie["path"], "/");
        assert_eq!(cookie["expires"], -1);
        assert_eq!(cookie["sameSite"], Value::Null);
    }
}
