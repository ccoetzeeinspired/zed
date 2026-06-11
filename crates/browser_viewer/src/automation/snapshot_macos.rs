//! DOM-backed snapshot producer for macOS WKWebView.
//!
//! WKWebView does not expose Chromium's `Accessibility.getFullAXTree`, so the
//! macOS path emits the same parser shape from page JavaScript instead.

/// JavaScript expression that returns `{ nodes: [...] }` with Playwright-shaped
/// roles/names plus a durable `wkDomToken` for later element actions.
pub fn macos_snapshot_script() -> &'static str {
    r#"
(() => {
  const tokenAttr = "data-zed-wk-dom-token";
  const nodes = [];
  let nodeId = 1;
  let nextToken = Number(window.__zedBrowserAutomationNextToken || 1);

  const rootId = String(nodeId++);
  const root = {
    nodeId: rootId,
    role: { value: "RootWebArea" },
    name: { value: document.title || location.href || "" },
    childIds: []
  };
  nodes.push(root);

  const trim = (value) => String(value || "").replace(/\s+/g, " ").trim();
  const visible = (element) => {
    const style = window.getComputedStyle(element);
    if (style.display === "none" || style.visibility === "hidden" || Number(style.opacity) === 0) {
      return false;
    }
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };
  const tokenFor = (element) => {
    let token = element.getAttribute(tokenAttr);
    if (!token) {
      token = `wk-${Date.now()}-${nextToken++}`;
      element.setAttribute(tokenAttr, token);
      window.__zedBrowserAutomationNextToken = nextToken;
    }
    return token;
  };
  const cssEscape = (value) => window.CSS && CSS.escape ? CSS.escape(value) : String(value).replace(/["\\]/g, "\\$&");
  const attrSelector = (name, value) => `[${name}="${cssEscape(value)}"]`;
  const unique = (selector, element) => {
    try {
      const matches = Array.from(document.querySelectorAll(selector));
      return matches.length === 1 && matches[0] === element;
    } catch (_) {
      return false;
    }
  };
  const durableSelectorFor = (element) => {
    for (const attr of ["data-testid", "data-test-id", "data-test", "data-qa", "data-cy"]) {
      const value = trim(element.getAttribute(attr));
      if (value && unique(attrSelector(attr, value), element)) {
        return attr === "data-testid"
          ? { kind: "testid", selector: value }
          : { kind: "css", selector: attrSelector(attr, value) };
      }
    }
    const id = trim(element.id);
    if (id && unique(`#${cssEscape(id)}`, element)) return { kind: "css", selector: `#${cssEscape(id)}` };
    for (const attr of ["name", "href", "src", "title", "alt"]) {
      const value = trim(element.getAttribute(attr));
      if (value && unique(attrSelector(attr, value), element)) {
        return { kind: "css", selector: attrSelector(attr, value) };
      }
    }
    return null;
  };
  const explicitRole = (element) => {
    const role = trim(element.getAttribute("role"));
    return role && role !== "presentation" && role !== "none" ? role : "";
  };
  const implicitRole = (element) => {
    const tag = element.tagName.toLowerCase();
    if (/^h[1-6]$/.test(tag)) return "heading";
    if (tag === "a" && element.href) return "link";
    if (tag === "button") return "button";
    if (tag === "select") return "combobox";
    if (tag === "textarea") return "textbox";
    if (tag === "img") return "img";
    if (tag === "summary") return "button";
    if (tag === "input") {
      const type = (element.getAttribute("type") || "text").toLowerCase();
      if (["button", "submit", "reset"].includes(type)) return "button";
      if (["checkbox", "radio", "slider"].includes(type)) return type;
      return "textbox";
    }
    if (tag === "li") return "listitem";
    return "";
  };
  const accessibleName = (element, role) => {
    const labelledBy = trim(element.getAttribute("aria-labelledby"));
    if (labelledBy) {
      const labelledName = trim(labelledBy.split(/\s+/).map((id) => document.getElementById(id)?.innerText || "").join(" "));
      if (labelledName) return labelledName;
    }
    const aria = trim(element.getAttribute("aria-label"));
    if (aria) return aria;
    const tag = element.tagName.toLowerCase();
    if (tag === "img") return trim(element.getAttribute("alt") || element.getAttribute("title"));
    if (tag === "input" || tag === "textarea") {
      const label = element.labels && element.labels.length ? trim(Array.from(element.labels).map((label) => label.innerText).join(" ")) : "";
      return label || trim(element.getAttribute("placeholder") || element.value || element.getAttribute("title"));
    }
    if (tag === "select") {
      const label = element.labels && element.labels.length ? trim(Array.from(element.labels).map((label) => label.innerText).join(" ")) : "";
      return label || trim(element.selectedOptions && element.selectedOptions[0] ? element.selectedOptions[0].innerText : "");
    }
    if (role === "heading" || role === "button" || role === "link" || role === "listitem") {
      return trim(element.innerText || element.textContent);
    }
    return trim(element.getAttribute("title"));
  };

  for (const element of document.body ? document.body.querySelectorAll("*") : []) {
    if (!visible(element)) continue;
    const role = explicitRole(element) || implicitRole(element);
    if (!role) continue;
    const name = accessibleName(element, role);
    if (!name) continue;

    const id = String(nodeId++);
    root.childIds.push(id);
    const node = {
      nodeId: id,
      role: { value: role },
      name: { value: name },
      childIds: [],
      wkDomToken: tokenFor(element)
    };
    const durable = durableSelectorFor(element);
    if (durable) {
      node.durableSelector = durable.selector;
      node.durableSelectorKind = durable.kind;
    }
    const headingLevel = role === "heading" ? Number((element.tagName || "").slice(1)) : 0;
    if (headingLevel >= 1 && headingLevel <= 6) {
      node.properties = [{ name: "level", value: { value: String(headingLevel) } }];
    }
    nodes.push(node);
  }

  return { nodes };
})()
"#
}

/// JavaScript expression that resolves a macOS snapshot token and invokes the
/// user function with the element as both `this` and first argument.
pub fn macos_element_evaluate_script(function: &str, token: &str) -> String {
    let token_json = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"
(() => {{
  const token = {token_json};
  const fn = ({function});
  const element = Array.from(document.querySelectorAll("[data-zed-wk-dom-token]"))
    .find((candidate) => candidate.getAttribute("data-zed-wk-dom-token") === token);
  if (!element) {{
    throw new Error(`snapshot token ${{token}} was not found; run browser_snapshot again`);
  }}
  return fn.call(element, element);
}})()
"#
    )
}

pub fn macos_page_contains_text_script(text: &str) -> String {
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"
(() => {{
  const needle = {text_json};
  const hay = `${{document.title || ""}}\n${{document.body?.innerText || ""}}`;
  return hay.includes(needle);
}})()
"#
    )
}

pub fn macos_page_state_script() -> &'static str {
    r#"
(() => ({
  readyState: document.readyState,
  url: location.href,
  title: document.title || ""
}))()
"#
}

pub fn macos_element_visible_script(token: &str) -> String {
    macos_element_evaluate_script(
        r#"el => {
  const r = el.getBoundingClientRect();
  const s = window.getComputedStyle(el);
  return r.width > 0 && r.height > 0 && s.visibility !== "hidden" && s.display !== "none" && s.opacity !== "0";
}"#,
        token,
    )
}

pub fn macos_element_value_script(token: &str) -> String {
    macos_element_evaluate_script(
        r#"el => ("value" in el) ? String(el.value) : String(el.textContent || "")"#,
        token,
    )
}

pub fn macos_list_visible_script(token: &str) -> String {
    macos_element_evaluate_script(
        r#"el => {
  const r = el.getBoundingClientRect();
  const s = window.getComputedStyle(el);
  const visible = r.width > 0 && r.height > 0 && s.visibility !== "hidden" && s.display !== "none";
  const items = el.querySelectorAll("li, [role=listitem], [role=option]").length || el.children.length;
  return { visible, items };
}"#,
        token,
    )
}

pub fn macos_click_script(token: &str, button: &str, double: bool, modifiers: &[String]) -> String {
    let button_json = serde_json::to_string(button).unwrap_or_else(|_| "\"left\"".to_string());
    let double_json = if double { "true" } else { "false" };
    let alt = modifiers
        .iter()
        .any(|m| modifier_eq(m, "alt") || modifier_eq(m, "option"));
    let ctrl = modifiers
        .iter()
        .any(|m| modifier_eq(m, "control") || modifier_eq(m, "ctrl"));
    let meta = modifiers
        .iter()
        .any(|m| modifier_eq(m, "meta") || modifier_eq(m, "cmd") || modifier_eq(m, "command"));
    let shift = modifiers.iter().any(|m| modifier_eq(m, "shift"));
    macos_element_evaluate_script(
        &format!(
            r#"el => {{
  const buttonName = {button_json};
  const doubleClick = {double_json};
  const buttonCode = buttonName === "middle" ? 1 : buttonName === "right" ? 2 : 0;
  const eventInit = {{
    bubbles: true,
    cancelable: true,
    view: window,
    button: buttonCode,
    buttons: buttonCode === 2 ? 2 : buttonCode === 1 ? 4 : 1,
    altKey: {alt},
    ctrlKey: {ctrl},
    metaKey: {meta},
    shiftKey: {shift}
  }};
  el.scrollIntoView({{ block: "center", inline: "center" }});
  el.focus?.({{ preventScroll: true }});
  if (buttonName === "left" && !doubleClick && !eventInit.altKey && !eventInit.ctrlKey && !eventInit.metaKey && !eventInit.shiftKey) {{
    el.click();
    return true;
  }}
  for (const type of ["pointerover", "mouseover", "pointermove", "mousemove", "pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
    el.dispatchEvent(new MouseEvent(type, eventInit));
  }}
  if (doubleClick) {{
    el.dispatchEvent(new MouseEvent("dblclick", {{ ...eventInit, detail: 2 }}));
  }}
  return true;
}}"#
        ),
        token,
    )
}

pub fn macos_hover_script(token: &str, modifiers: &[String]) -> String {
    let alt = modifiers
        .iter()
        .any(|m| modifier_eq(m, "alt") || modifier_eq(m, "option"));
    let ctrl = modifiers
        .iter()
        .any(|m| modifier_eq(m, "control") || modifier_eq(m, "ctrl"));
    let meta = modifiers
        .iter()
        .any(|m| modifier_eq(m, "meta") || modifier_eq(m, "cmd") || modifier_eq(m, "command"));
    let shift = modifiers.iter().any(|m| modifier_eq(m, "shift"));
    macos_element_evaluate_script(
        &format!(
            r#"el => {{
  el.scrollIntoView({{ block: "center", inline: "center" }});
  const rect = el.getBoundingClientRect();
  const x = rect.left + rect.width / 2;
  const y = rect.top + rect.height / 2;
  const init = {{
    bubbles: true,
    cancelable: true,
    view: window,
    clientX: x,
    clientY: y,
    altKey: {alt},
    ctrlKey: {ctrl},
    metaKey: {meta},
    shiftKey: {shift}
  }};
  const pointer = window.PointerEvent || MouseEvent;
  for (const type of ["pointerover", "pointerenter", "pointermove"]) {{
    el.dispatchEvent(new pointer(type, init));
  }}
  for (const type of ["mouseover", "mouseenter", "mousemove"]) {{
    el.dispatchEvent(new MouseEvent(type, init));
  }}
  return {{ x, y }};
}}"#
        ),
        token,
    )
}

pub fn macos_type_script(token: &str, text: &str, submit: bool) -> String {
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    let submit_json = if submit { "true" } else { "false" };
    macos_element_evaluate_script(
        &format!(
            r#"el => {{
  const text = {text_json};
  const submit = {submit_json};
  el.scrollIntoView({{ block: "center", inline: "center" }});
  if ("value" in el) {{
    const prototype = Object.getPrototypeOf(el);
    const descriptor = prototype && Object.getOwnPropertyDescriptor(prototype, "value");
    if (descriptor && descriptor.set) descriptor.set.call(el, text);
    else el.value = text;
    el.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: text }}));
    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  }} else if (el.isContentEditable) {{
    el.textContent = text;
    el.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: text }}));
  }} else {{
    el.textContent = text;
    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
  }}
  if (submit) {{
    el.dispatchEvent(new KeyboardEvent("keydown", {{ bubbles: true, cancelable: true, key: "Enter", code: "Enter" }}));
    el.dispatchEvent(new KeyboardEvent("keyup", {{ bubbles: true, cancelable: true, key: "Enter", code: "Enter" }}));
    el.form?.requestSubmit?.();
  }}
  return true;
}}"#
        ),
        token,
    )
}

pub fn macos_select_option_script(token: &str, values: &[String]) -> String {
    let values_json = serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string());
    macos_element_evaluate_script(
        &format!(
            r#"el => {{
  if (!(el instanceof HTMLSelectElement)) {{
    throw new Error("target is not a <select> element");
  }}
  const wanted = new Set({values_json}.map(String));
  let matched = 0;
  for (const option of Array.from(el.options)) {{
    const hit = wanted.has(String(option.value)) || wanted.has(String(option.label)) || wanted.has(String(option.textContent || "").trim());
    option.selected = hit;
    if (hit) matched += 1;
    if (hit && !el.multiple) break;
  }}
  el.dispatchEvent(new Event("input", {{ bubbles: true }}));
  el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  return {{ matched, value: el.value }};
}}"#
        ),
        token,
    )
}

pub fn macos_set_checked_script(token: &str, checked: bool) -> String {
    let checked_json = if checked { "true" } else { "false" };
    macos_element_evaluate_script(
        &format!(
            r#"el => {{
  if (!(el instanceof HTMLInputElement) || !["checkbox", "radio"].includes(el.type)) {{
    throw new Error("target is not a checkbox or radio input");
  }}
  el.scrollIntoView({{ block: "center", inline: "center" }});
  el.checked = {checked_json};
  el.dispatchEvent(new Event("input", {{ bubbles: true }}));
  el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  return {{ checked: el.checked }};
}}"#
        ),
        token,
    )
}

fn web_storage_expr(store: &str) -> &'static str {
    if store == "session" || store == "sessionStorage" {
        "window.sessionStorage"
    } else {
        "window.localStorage"
    }
}

pub fn macos_storage_list_script(store: &str) -> String {
    let storage = web_storage_expr(store);
    format!(
        r#"
(() => {{
  const storage = {storage};
  const items = {{}};
  for (let i = 0; i < storage.length; i++) {{
    const key = storage.key(i);
    items[key] = storage.getItem(key);
  }}
  return items;
}})()
"#
    )
}

pub fn macos_storage_get_script(store: &str, key: &str) -> String {
    let storage = web_storage_expr(store);
    let key_json = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
    format!("(() => {storage}.getItem({key_json}))()")
}

pub fn macos_storage_set_script(store: &str, key: &str, value: &str) -> String {
    let storage = web_storage_expr(store);
    let key_json = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
    let value_json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    format!("(() => {{ {storage}.setItem({key_json}, {value_json}); return true; }})()")
}

pub fn macos_storage_delete_script(store: &str, key: &str) -> String {
    let storage = web_storage_expr(store);
    let key_json = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
    format!("(() => {{ {storage}.removeItem({key_json}); return true; }})()")
}

pub fn macos_storage_clear_script(store: &str) -> String {
    let storage = web_storage_expr(store);
    format!("(() => {{ {storage}.clear(); return true; }})()")
}

pub fn macos_storage_state_script() -> String {
    r#"
(() => {
  const copy = (storage) => {
    const items = {};
    for (let i = 0; i < storage.length; i++) {
      const key = storage.key(i);
      items[key] = storage.getItem(key);
    }
    return items;
  };
  return {
    url: location.href,
    cookies: [],
    localStorage: copy(window.localStorage),
    sessionStorage: copy(window.sessionStorage)
  };
})()
"#
    .to_string()
}

pub fn macos_set_storage_state_script(state: &serde_json::Value) -> String {
    let local = state
        .get("localStorage")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let session = state
        .get("sessionStorage")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let local_json = serde_json::to_string(&local).unwrap_or_else(|_| "{}".to_string());
    let session_json = serde_json::to_string(&session).unwrap_or_else(|_| "{}".to_string());
    format!(
        r#"
(() => {{
  const apply = (storage, items) => {{
    let count = 0;
    for (const [key, value] of Object.entries(items || {{}})) {{
      storage.setItem(key, typeof value === "string" ? value : JSON.stringify(value));
      count++;
    }}
    return count;
  }};
  return {{
    cookies: 0,
    items: apply(window.localStorage, {local_json}) + apply(window.sessionStorage, {session_json})
  }};
}})()
"#
    )
}

pub fn macos_handle_dialog_script(accept: bool, prompt_text: Option<&str>) -> String {
    let accept_json = if accept { "true" } else { "false" };
    let prompt_json = prompt_text
        .map(|text| serde_json::to_string(text).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    format!(
        r#"
(() => {{
  const accept = {accept_json};
  const promptText = {prompt_json};
  if (!window.__zedDialogInstalled) {{
    window.__zedDialogInstalled = true;
    window.__zedLastDialog = null;
    window.__zedDialogPolicy = {{ accept: true, promptText: null }};
    const record = (type, message, defaultValue) => {{
      window.__zedLastDialog = {{
        type,
        message: String(message == null ? "" : message),
        defaultValue: String(defaultValue == null ? "" : defaultValue)
      }};
    }};
    window.alert = (message) => {{
      record("alert", message, "");
    }};
    window.confirm = (message) => {{
      record("confirm", message, "");
      return !!window.__zedDialogPolicy.accept;
    }};
    window.prompt = (message, defaultValue) => {{
      record("prompt", message, defaultValue);
      if (!window.__zedDialogPolicy.accept) return null;
      return window.__zedDialogPolicy.promptText != null
        ? window.__zedDialogPolicy.promptText
        : String(defaultValue == null ? "" : defaultValue);
    }};
  }}
  window.__zedDialogPolicy = {{ accept, promptText }};
  return window.__zedLastDialog;
}})()
"#
    )
}

pub fn macos_console_messages_script(level: Option<&str>, clear: bool) -> String {
    let level_json = level
        .map(|level| serde_json::to_string(level).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let clear_js = if clear {
        "window.__zedConsole = [];"
    } else {
        ""
    };
    format!(
        r#"
(() => {{
  {instrumentation}
  const messages = window.__zedConsole || [];
  const level = {level_json};
  const result = (level ? messages.filter((message) => message.level === level) : messages).slice();
  {clear_js}
  return result;
}})()
"#,
        instrumentation = crate::automation::instrumentation::SCRIPT,
    )
}

pub fn macos_network_requests_script(clear: bool) -> String {
    let clear_js = if clear {
        "window.__zedNetwork = [];"
    } else {
        ""
    };
    format!(
        r#"
(() => {{
  {instrumentation}
  const result = (window.__zedNetwork || []).slice();
  {clear_js}
  return result;
}})()
"#,
        instrumentation = crate::automation::instrumentation::SCRIPT,
    )
}

pub fn macos_network_request_script(id: i64) -> String {
    format!(
        r#"
(() => {{
  {instrumentation}
  return (window.__zedNetwork || []).find((request) => request.id === {id}) || null;
}})()
"#,
        instrumentation = crate::automation::instrumentation::SCRIPT,
    )
}

pub fn macos_scroll_into_view_script(token: &str) -> String {
    macos_element_evaluate_script(
        r#"el => {
  el.scrollIntoView({ block: "center", inline: "nearest" });
  const root = document.scrollingElement || document.documentElement;
  return { x: window.scrollX, y: window.scrollY, maxY: Math.max(0, root.scrollHeight - root.clientHeight) };
}"#,
        token,
    )
}

pub fn macos_scroll_by_script(dx: f64, dy: f64) -> String {
    format!(
        r#"
(() => {{
  window.scrollBy({{ left: {dx}, top: {dy}, behavior: "instant" }});
  const root = document.scrollingElement || document.documentElement;
  return {{ x: window.scrollX, y: window.scrollY, maxY: Math.max(0, root.scrollHeight - root.clientHeight) }};
}})()
"#
    )
}

pub fn macos_element_center_script(token: &str) -> String {
    macos_element_evaluate_script(
        r#"el => {
  el.scrollIntoView({ block: "center", inline: "center" });
  const rect = el.getBoundingClientRect();
  return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
}"#,
        token,
    )
}

pub fn macos_drop_script(token: &str, data: Option<&str>, mime: Option<&str>) -> String {
    let data_json = data
        .map(|data| serde_json::to_string(data).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let mime_json = mime
        .map(|mime| serde_json::to_string(mime).unwrap_or_else(|_| "null".to_string()))
        .unwrap_or_else(|| "null".to_string());
    macos_element_evaluate_script(
        &format!(
            r#"el => {{
  const data = {data_json};
  const mime = {mime_json};
  el.scrollIntoView({{ block: "center", inline: "center" }});
  const dataTransfer = new DataTransfer();
  if (data != null) dataTransfer.setData(mime || "text/plain", String(data));
  const options = {{ bubbles: true, cancelable: true, dataTransfer }};
  for (const type of ["dragenter", "dragover", "drop"]) {{
    el.dispatchEvent(new DragEvent(type, options));
  }}
  return true;
}}"#
        ),
        token,
    )
}

fn mouse_button_code(button: &str) -> i32 {
    match button {
        "middle" => 1,
        "right" => 2,
        _ => 0,
    }
}

pub fn macos_mouse_event_script(
    event_type: &str,
    x: f64,
    y: f64,
    button: &str,
    buttons: i32,
    click_count: i32,
) -> String {
    let event_type_json =
        serde_json::to_string(event_type).unwrap_or_else(|_| "\"mousemove\"".to_string());
    let button_code = mouse_button_code(button);
    format!(
        r#"
(() => {{
  const type = {event_type_json};
  const x = {x};
  const y = {y};
  const target = document.elementFromPoint(x, y) || document.body || document.documentElement;
  if (!target) return {{ x, y, hit: false }};
  const init = {{
    bubbles: true,
    cancelable: true,
    view: window,
    clientX: x,
    clientY: y,
    button: {button_code},
    buttons: {buttons},
    detail: {click_count}
  }};
  const pointer = window.PointerEvent || MouseEvent;
  if (type.startsWith("pointer")) target.dispatchEvent(new pointer(type, init));
  else target.dispatchEvent(new MouseEvent(type, init));
  return {{ x, y, hit: true, tag: target.tagName || "" }};
}})()
"#
    )
}

pub fn macos_mouse_click_script(x: f64, y: f64, button: &str, double: bool) -> String {
    let button_code = mouse_button_code(button);
    let buttons = match button {
        "middle" => 4,
        "right" => 2,
        _ => 1,
    };
    let click_count = if double { 2 } else { 1 };
    let dblclick = if double {
        r#"target.dispatchEvent(new MouseEvent("dblclick", { ...init, detail: 2 }));"#
    } else {
        ""
    };
    format!(
        r#"
(() => {{
  const x = {x};
  const y = {y};
  const target = document.elementFromPoint(x, y) || document.body || document.documentElement;
  if (!target) return {{ x, y, hit: false }};
  const init = {{
    bubbles: true,
    cancelable: true,
    view: window,
    clientX: x,
    clientY: y,
    button: {button_code},
    buttons: {buttons},
    detail: {click_count}
  }};
  target.dispatchEvent(new MouseEvent("mousemove", {{ ...init, buttons: 0, detail: 0 }}));
  target.dispatchEvent(new MouseEvent("mousedown", init));
  target.focus?.({{ preventScroll: true }});
  target.dispatchEvent(new MouseEvent("mouseup", {{ ...init, buttons: 0 }}));
  target.dispatchEvent(new MouseEvent("click", init));
  {dblclick}
  return {{ x, y, button: "{button}", double: {double}, hit: true, tag: target.tagName || "" }};
}})()
"#
    )
}

pub fn macos_mouse_drag_script(sx: f64, sy: f64, ex: f64, ey: f64, button: &str) -> String {
    let button_code = mouse_button_code(button);
    let buttons = match button {
        "middle" => 4,
        "right" => 2,
        _ => 1,
    };
    format!(
        r#"
(() => {{
  const sx = {sx}, sy = {sy}, ex = {ex}, ey = {ey};
  const target = document.elementFromPoint(sx, sy) || document.body || document.documentElement;
  if (!target) return {{ from: [sx, sy], to: [ex, ey], hit: false }};
  const init = (x, y, type, held) => ({{
    bubbles: true,
    cancelable: true,
    view: window,
    clientX: x,
    clientY: y,
    button: {button_code},
    buttons: held ? {buttons} : 0
  }});
  target.dispatchEvent(new MouseEvent("mousemove", init(sx, sy, "mousemove", false)));
  target.dispatchEvent(new MouseEvent("mousedown", init(sx, sy, "mousedown", true)));
  target.dispatchEvent(new MouseEvent("mousemove", init((sx + ex) / 2, (sy + ey) / 2, "mousemove", true)));
  const endTarget = document.elementFromPoint(ex, ey) || target;
  endTarget.dispatchEvent(new MouseEvent("mousemove", init(ex, ey, "mousemove", true)));
  endTarget.dispatchEvent(new MouseEvent("mouseup", init(ex, ey, "mouseup", false)));
  return {{ from: [sx, sy], to: [ex, ey], button: "{button}", hit: true }};
}})()
"#
    )
}

pub fn macos_mouse_wheel_script(x: f64, y: f64, delta_x: f64, delta_y: f64) -> String {
    format!(
        r#"
(() => {{
  const x = {x};
  const y = {y};
  const target = document.elementFromPoint(x, y) || document.scrollingElement || document.documentElement;
  target.dispatchEvent(new WheelEvent("wheel", {{
    bubbles: true,
    cancelable: true,
    view: window,
    clientX: x,
    clientY: y,
    deltaX: {delta_x},
    deltaY: {delta_y},
    deltaMode: 0
  }}));
  window.scrollBy({{ left: {delta_x}, top: {delta_y}, behavior: "instant" }});
  return {{ x, y, deltaX: {delta_x}, deltaY: {delta_y}, scrollX: window.scrollX, scrollY: window.scrollY }};
}})()
"#
    )
}

pub fn macos_press_key_script(key: &str, code: &str, text: Option<&str>, modifiers: i32) -> String {
    let key_json = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
    let code_json = serde_json::to_string(code).unwrap_or_else(|_| "\"\"".to_string());
    let text_json = text
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string()))
        .unwrap_or_else(|| "null".to_string());
    let alt = modifiers & 1 != 0;
    let ctrl = modifiers & 2 != 0;
    let meta = modifiers & 4 != 0;
    let shift = modifiers & 8 != 0;
    format!(
        r#"
(() => {{
  const target = document.activeElement || document.body;
  const key = {key_json};
  const code = {code_json};
  const text = {text_json};
  const init = {{
    bubbles: true,
    cancelable: true,
    key,
    code,
    altKey: {alt},
    ctrlKey: {ctrl},
    metaKey: {meta},
    shiftKey: {shift}
  }};
  const down = new KeyboardEvent("keydown", init);
  const accepted = target.dispatchEvent(down);
  if (accepted && text && !init.ctrlKey && !init.metaKey && !init.altKey && key !== "Enter") {{
    if ("value" in target) {{
      const start = Number.isFinite(target.selectionStart) ? target.selectionStart : String(target.value || "").length;
      const end = Number.isFinite(target.selectionEnd) ? target.selectionEnd : start;
      const before = String(target.value || "").slice(0, start);
      const after = String(target.value || "").slice(end);
      target.value = `${{before}}${{text}}${{after}}`;
      const cursor = start + text.length;
      target.setSelectionRange?.(cursor, cursor);
    }} else if (target.isContentEditable) {{
      document.execCommand?.("insertText", false, text);
    }}
    target.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: text }}));
  }}
  if (accepted && key === "Enter") {{
    target.form?.requestSubmit?.();
  }}
  target.dispatchEvent(new KeyboardEvent("keyup", init));
  return true;
}})()
"#
    )
}

fn modifier_eq(value: &str, expected: &str) -> bool {
    value.eq_ignore_ascii_case(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_snapshot_script_emits_parser_compatible_shape() {
        let script = macos_snapshot_script();
        assert!(script.contains("nodes"));
        assert!(script.contains("RootWebArea"));
        assert!(script.contains("wkDomToken"));
        assert!(script.contains("data-zed-wk-dom-token"));
        assert!(script.contains("durableSelector"));
        assert!(script.contains("data-testid"));
        assert!(script.contains("heading"));
    }

    #[test]
    fn macos_snapshot_regression_keeps_playwright_like_locator_sources() {
        let script = macos_snapshot_script();
        for expected in [
            "data-testid",
            "aria-label",
            "aria-labelledby",
            "placeholder",
            "button",
            "textbox",
            "combobox",
            "checkbox",
            "radio",
            "heading",
            "wkDomToken",
            "durableSelector",
        ] {
            assert!(script.contains(expected), "{expected}");
        }
    }

    #[test]
    fn macos_element_evaluate_script_calls_function_with_token_element() {
        let script = macos_element_evaluate_script("el => el.textContent", "wk-\"quoted\"");
        assert!(script.contains("const token = "));
        assert!(script.contains("wk-"));
        assert!(script.contains("\\\"quoted\\\""));
        assert!(script.contains("document.querySelectorAll"));
        assert!(script.contains("fn.call(element, element)"));
        assert!(script.contains("run browser_snapshot again"));
    }

    #[test]
    fn macos_page_contains_text_script_escapes_text() {
        let script = macos_page_contains_text_script("Thanks \"lot\"");
        assert!(script.contains(r#"const needle = "Thanks \"lot\"";"#));
        assert!(script.contains("document.body?.innerText"));
        assert!(script.contains("hay.includes(needle)"));
    }

    #[test]
    fn macos_page_state_script_reports_navigation_state() {
        let script = macos_page_state_script();
        assert!(script.contains("document.readyState"));
        assert!(script.contains("location.href"));
        assert!(script.contains("document.title"));
    }

    #[test]
    fn macos_assertion_scripts_reuse_snapshot_tokens() {
        assert!(macos_element_visible_script("wk-1").contains("getBoundingClientRect"));
        assert!(macos_element_value_script("wk-1").contains("String(el.value)"));
        assert!(macos_list_visible_script("wk-1").contains("querySelectorAll"));
    }

    #[test]
    fn macos_action_scripts_preserve_token_and_payloads() {
        assert!(macos_click_script("wk-1", "left", false, &[]).contains("el.click()"));
        assert!(macos_hover_script("wk-1", &[]).contains("pointerover"));
        assert!(macos_hover_script("wk-1", &[]).contains("mousemove"));
        let type_script = macos_type_script("wk-1", "a \"b\"", true);
        assert!(type_script.contains("const text ="));
        assert!(type_script.contains(r#"a \"b\""#));
        assert!(!type_script.contains(".focus"));
        assert!(
            macos_select_option_script("wk-1", &["A".to_string()]).contains("HTMLSelectElement")
        );
        let checked_script = macos_set_checked_script("wk-1", true);
        assert!(checked_script.contains("el.checked = true"));
        assert!(!checked_script.contains(".focus"));
        assert!(macos_storage_list_script("local").contains("window.localStorage"));
        assert!(macos_storage_get_script("session", "key").contains("window.sessionStorage"));
        assert!(macos_storage_set_script("local", "k", "v").contains("setItem"));
        assert!(macos_storage_delete_script("local", "k").contains("removeItem"));
        assert!(macos_storage_clear_script("session").contains("clear()"));
        assert!(macos_storage_state_script().contains("localStorage"));
        assert!(
            macos_set_storage_state_script(&serde_json::json!({"localStorage":{"k":"v"}}))
                .contains("cookies: 0")
        );
        assert!(macos_handle_dialog_script(true, Some("ok")).contains("window.alert"));
        assert!(macos_handle_dialog_script(false, None).contains("promptText = null"));
        assert!(macos_console_messages_script(Some("error"), true).contains("__zedConsole = []"));
        assert!(macos_console_messages_script(None, false).contains("__zedInstrumented"));
        assert!(macos_network_requests_script(true).contains("__zedNetwork = []"));
        assert!(macos_network_request_script(12).contains("request.id === 12"));
        assert!(macos_scroll_into_view_script("wk-1").contains("scrollIntoView"));
        assert!(macos_scroll_by_script(10.0, 20.0).contains("window.scrollBy"));
        assert!(macos_element_center_script("wk-1").contains("getBoundingClientRect"));
        assert!(
            macos_drop_script("wk-1", Some("payload"), Some("text/plain")).contains("DataTransfer")
        );
        assert!(
            macos_mouse_event_script("mousemove", 1.0, 2.0, "left", 0, 0)
                .contains("elementFromPoint")
        );
        assert!(macos_mouse_click_script(1.0, 2.0, "left", true).contains("dblclick"));
        assert!(macos_mouse_drag_script(1.0, 2.0, 3.0, 4.0, "left").contains("mousedown"));
        assert!(macos_mouse_wheel_script(1.0, 2.0, 3.0, 4.0).contains("WheelEvent"));
    }

    #[test]
    fn macos_press_key_script_dispatches_keyboard_events() {
        let script = macos_press_key_script("Enter", "Enter", Some("\r"), 0);
        assert!(script.contains(r#"const key = "Enter";"#));
        assert!(script.contains("KeyboardEvent(\"keydown\""));
        assert!(script.contains("requestSubmit"));
        assert!(script.contains("KeyboardEvent(\"keyup\""));
    }
}
