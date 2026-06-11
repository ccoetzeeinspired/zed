//! Platform-neutral automation session capability boundary.

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::automation::session::ElementHandle;

pub type JsonCallback = Box<dyn FnOnce(Result<Value>) + 'static>;
pub type UnitCallback = Box<dyn FnOnce(Result<()>) + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserPlatform {
    WebView2,
    WkWebView,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyDispatch {
    pub event_type: String,
    pub key: String,
    pub code: String,
    pub modifiers: i64,
    pub windows_virtual_key_code: i64,
    pub text: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MouseDispatch {
    pub event_type: String,
    pub x: f64,
    pub y: f64,
    pub button: String,
    pub buttons: i64,
    pub click_count: i64,
    pub modifiers: i64,
}

pub trait AutomationSession {
    fn platform(&self) -> BrowserPlatform;

    fn call_cdp(&self, method: &str, params_json: &str, on_done: JsonCallback) -> Result<()>;

    fn evaluate_expression(
        &self,
        expression: &str,
        await_promise: bool,
        on_done: JsonCallback,
    ) -> Result<()>;

    fn invoke_on_element(
        &self,
        handle: ElementHandle,
        function_declaration: &str,
        arguments: Option<&[Value]>,
        on_done: JsonCallback,
    ) -> Result<()>;

    fn dispatch_key_event(&self, event: KeyDispatch, on_done: UnitCallback) -> Result<()>;

    fn dispatch_mouse_event(&self, event: MouseDispatch, on_done: JsonCallback) -> Result<()>;
}

pub fn unsupported_capability(platform: BrowserPlatform, capability: &str) -> anyhow::Error {
    anyhow!("browser automation capability {capability} is not supported by {platform:?}")
}
