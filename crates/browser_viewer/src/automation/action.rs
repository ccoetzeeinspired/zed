//! CP2–3: ref-targeted click and type via CDP DOM (no coordinate injection).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::automation::cdp::{Completion, call_devtools_on_webview, finish, parse_cdp_response};
use crate::automation::session::ElementRef;
use crate::webview2_host::WebView2Session;

/// Default actionability wait for click/type.
pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const RETRY_INTERVAL: Duration = Duration::from_millis(200);

const CLICK_SCRIPT: &str = r#"function() {
  if (this.disabled) {
    throw new Error('element is disabled');
  }
  const style = window.getComputedStyle(this);
  if (style.visibility === 'hidden' || style.display === 'none') {
    throw new Error('element is not visible');
  }
  const rect = this.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) {
    throw new Error('element has no layout box');
  }
  this.scrollIntoView({ block: 'center', inline: 'center' });
  if (document.activeElement && document.activeElement !== this) {
    document.activeElement.blur();
  }
  if (this.form && typeof this.form.requestSubmit === 'function') {
    this.form.requestSubmit(this);
  } else if (typeof this.click === 'function') {
    this.click();
  } else {
    this.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
  }
  return true;
}"#;

// React/Vue controlled inputs ignore plain `.value = …` — use the native setter +
// InputEvent so framework state matches what the user sees.
const TYPE_SCRIPT: &str = r#"function(text, submit) {
  function setNativeInputValue(el, value) {
    const proto = el instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
    const descriptor = Object.getOwnPropertyDescriptor(proto, 'value');
    if (el._valueTracker) {
      el._valueTracker.setValue('');
    }
    if (descriptor && descriptor.set) {
      descriptor.set.call(el, value);
    } else {
      el.value = value;
    }
    el.dispatchEvent(new InputEvent('input', {
      bubbles: true,
      cancelable: true,
      inputType: 'insertText',
      data: value,
    }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }
  if (this.disabled) {
    throw new Error('element is disabled');
  }
  const style = window.getComputedStyle(this);
  if (style.visibility === 'hidden' || style.display === 'none') {
    throw new Error('element is not visible');
  }
  this.scrollIntoView({ block: 'center', inline: 'center' });
  this.focus();
  const tag = this.tagName ? this.tagName.toUpperCase() : '';
  // When a submit (Enter) will follow, keep focus so the key event lands on
  // this element and triggers implicit form submission; otherwise blur so the
  // page sees the field as committed.
  if (tag === 'INPUT' || tag === 'TEXTAREA') {
    setNativeInputValue(this, text);
    if (!submit) { this.blur(); }
    return true;
  }
  if (this.isContentEditable) {
    this.textContent = text;
    this.dispatchEvent(new InputEvent('input', {
      bubbles: true,
      cancelable: true,
      inputType: 'insertText',
      data: text,
    }));
    if (!submit) { this.blur(); }
    return true;
  }
  throw new Error('element is not a text input');
}"#;

/// Validate a snapshot ref and ensure it carries a DOM backend node id.
pub fn element_for_action(element: ElementRef) -> Result<ElementRef> {
    if element.backend_dom_node_id.is_none() {
        return Err(anyhow!(
            "ref {} ({}/{}) has no backendDOMNodeId — take a fresh snapshot",
            element.ref_id,
            element.role,
            element.name
        ));
    }
    Ok(element)
}

/// One-shot click attempt on a backend DOM node (no retry).
pub fn try_click_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    on_done: Box<dyn FnOnce(Result<()>) + 'static>,
) -> Result<()> {
    invoke_on_backend_node(
        session,
        backend_node_id,
        CLICK_SCRIPT,
        None,
        Box::new(move |result| on_done(result.map(|_| ()))),
    )
}

/// One-shot type attempt on a backend DOM node (no retry).
///
/// When `keep_focus_for_submit` is true the script skips its trailing
/// `blur()`, so a following Enter key press lands on the still-focused element
/// and triggers implicit form submission.
pub fn try_type_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    text: &str,
    keep_focus_for_submit: bool,
    on_done: Box<dyn FnOnce(Result<()>) + 'static>,
) -> Result<()> {
    let args = vec![json!({ "value": text }), json!({ "value": keep_focus_for_submit })];
    invoke_on_backend_node(
        session,
        backend_node_id,
        TYPE_SCRIPT,
        Some(&args),
        Box::new(move |result| on_done(result.map(|_| ()))),
    )
}

// Scroll the element to the centre of the viewport. `scrollIntoView` walks up
// the scroll-parent chain, so this also scrolls inner (non-window) containers.
// Returns the resulting window scroll position for caller feedback.
const SCROLL_INTO_VIEW_SCRIPT: &str = r#"function() {
  this.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const el = document.scrollingElement || document.documentElement;
  return { x: window.scrollX, y: window.scrollY, maxY: Math.max(0, el.scrollHeight - el.clientHeight) };
}"#;

// Page-coordinate bounding box of the element (CSS px, document-relative), for
// use as a `Page.captureScreenshot` clip with `captureBeyondViewport: true`.
const BOUNDING_RECT_SCRIPT: &str = r#"function() {
  const r = this.getBoundingClientRect();
  return { x: r.left + window.scrollX, y: r.top + window.scrollY, width: r.width, height: r.height };
}"#;

/// One-shot "measure this element's page-coordinate bounding box".
pub fn try_bounding_rect_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
) -> Result<()> {
    invoke_on_backend_node(session, backend_node_id, BOUNDING_RECT_SCRIPT, None, on_done)
}

/// One-shot "scroll this element into view" on a backend DOM node.
pub fn try_scroll_into_view_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
) -> Result<()> {
    invoke_on_backend_node(
        session,
        backend_node_id,
        SCROLL_INTO_VIEW_SCRIPT,
        None,
        on_done,
    )
}

// Select `<option>`s in a `<select>` by value, label, or visible text; fire
// input + change so frameworks observe the change. Returns how many matched and
// the select's resulting value.
const SELECT_OPTION_SCRIPT: &str = r#"function(values) {
  const tag = this.tagName ? this.tagName.toUpperCase() : '';
  if (tag !== 'SELECT') {
    throw new Error('element is not a <select>');
  }
  const wanted = Array.isArray(values) ? values : [values];
  let matched = 0;
  for (const opt of Array.from(this.options)) {
    const hit = wanted.includes(opt.value) || wanted.includes(opt.label) || wanted.includes(opt.text);
    opt.selected = hit;
    if (hit) { matched++; }
  }
  this.dispatchEvent(new Event('input', { bubbles: true }));
  this.dispatchEvent(new Event('change', { bubbles: true }));
  return { matched: matched, value: this.value };
}"#;

/// One-shot "select option(s)" on a `<select>` backend node. `values` is a JSON
/// array of strings (matched against option value/label/text).
pub fn try_select_option_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    values: &[Value],
    on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
) -> Result<()> {
    let args = vec![json!({ "value": values })];
    invoke_on_backend_node(session, backend_node_id, SELECT_OPTION_SCRIPT, Some(&args), on_done)
}

// Scroll the element to viewport centre and return that centre point in
// *viewport* CSS px — the coordinate space `Input.dispatchMouseEvent` expects.
const HOVER_POINT_SCRIPT: &str = r#"function() {
  this.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
  const r = this.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
}"#;

/// One-shot "scroll into view + report viewport-centre point" for hover.
pub fn try_hover_point_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
) -> Result<()> {
    invoke_on_backend_node(session, backend_node_id, HOVER_POINT_SCRIPT, None, on_done)
}

/// Run `functionDeclaration` on the node identified by `backend_node_id`.
pub fn invoke_on_backend_node(
    session: &WebView2Session,
    backend_node_id: i32,
    function_declaration: &str,
    arguments: Option<&[Value]>,
    on_done: Box<dyn FnOnce(Result<Value>) + 'static>,
) -> Result<()> {
    let webview = session.webview.clone();
    let function_declaration = function_declaration.to_string();
    let arguments = arguments.map(|args| args.to_vec());
    let completion: Completion<Value> = Rc::new(RefCell::new(Some(on_done)));
    let enable_completion = completion.clone();
    let webview_for_enable = webview.clone();

    call_devtools_on_webview(
        &webview_for_enable,
        "DOM.enable",
        "{}",
        Box::new(move |enable_raw| match enable_raw.and_then(|raw| parse_cdp_response(raw)) {
            Err(err) => finish(&enable_completion, Err(err)),
            Ok(_) => {
                let resolve_params = json!({ "backendNodeId": backend_node_id }).to_string();
                let webview_for_resolve = webview.clone();
                let function_declaration = function_declaration.clone();
                let arguments = arguments.clone();
                let completion = completion.clone();
                let _ = call_devtools_on_webview(
                    &webview_for_resolve,
                    "DOM.resolveNode",
                    &resolve_params,
                    Box::new(move |resolve_raw| {
                        let resolve_completion = completion.clone();
                        match resolve_raw
                            .and_then(|raw| parse_cdp_response(raw))
                            .and_then(|value| object_id_from_resolve(&value))
                        {
                            Err(err) => finish(&resolve_completion, Err(err)),
                            Ok(object_id) => {
                                let call_params = call_function_on_params(
                                    &object_id,
                                    &function_declaration,
                                    arguments.as_deref(),
                                );
                                let call_completion = completion.clone();
                                let webview_for_call = webview.clone();
                                let _ = call_devtools_on_webview(
                                    &webview_for_call,
                                    "Runtime.callFunctionOn",
                                    &call_params,
                                    Box::new(move |call_raw| {
                                        finish(&call_completion, parse_call_function_result(call_raw));
                                    }),
                                );
                            }
                        }
                    }),
                );
            }
        }),
    )
}

fn call_function_on_params(
    object_id: &str,
    function_declaration: &str,
    arguments: Option<&[Value]>,
) -> String {
    let mut params = json!({
        "objectId": object_id,
        "functionDeclaration": function_declaration,
        "returnByValue": true,
        "awaitPromise": false,
    });
    if let Some(args) = arguments {
        params["arguments"] = json!(args);
    }
    params.to_string()
}

fn object_id_from_resolve(value: &Value) -> Result<String> {
    value
        .get("object")
        .and_then(|object| object.get("objectId"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("DOM.resolveNode response missing object.objectId"))
}

fn parse_call_function_result(raw: Result<String>) -> Result<Value> {
    let value = raw.and_then(parse_cdp_response)?;
    if let Some(details) = value.get("exceptionDetails") {
        let message = details
            .get("text")
            .or_else(|| details.get("exception").and_then(|e| e.get("description")))
            .and_then(|v| v.as_str())
            .unwrap_or("Runtime.callFunctionOn failed");
        return Err(anyhow!("{message}"));
    }
    Ok(value.get("result").cloned().unwrap_or(value))
}

pub(crate) type SessionAttempt =
    Arc<dyn Fn(&WebView2Session, Box<dyn FnOnce(Result<()>) + 'static>) -> Result<()> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_from_resolve_parses_cdp_shape() {
        let value = json!({
            "object": {
                "objectId": "ABC123",
                "backendNodeId": 42
            }
        });
        assert_eq!(object_id_from_resolve(&value).unwrap(), "ABC123");
    }

    #[test]
    fn parse_call_function_result_surfaces_exception() {
        let raw = Ok(r#"{"exceptionDetails":{"text":"element is disabled"}}"#.to_string());
        assert!(parse_call_function_result(raw).is_err());
    }

    #[test]
    fn call_function_on_params_includes_arguments() {
        let params: Value = serde_json::from_str(&call_function_on_params(
            "OBJ",
            "function() {}",
            Some(&[json!({"value": "hi"})]),
        ))
        .unwrap();
        assert_eq!(params["objectId"], "OBJ");
        assert_eq!(params["arguments"][0]["value"], "hi");
    }
}
