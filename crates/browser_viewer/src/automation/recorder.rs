//! CP15: record an automation run and codegen a runnable Playwright spec.
//!
//! The recorder hooks the single dispatch chokepoint
//! (`ipc.rs::dispatch_request`). On each *successful*, ref-bearing action it
//! captures the resolved **role + accessible name** (NOT the ephemeral `eN`
//! ref, which is valid for only one page generation) plus the post-action URL.
//! [`codegen`] turns that action log into a Playwright `.spec.ts`.
//!
//! Why this is deterministic (not LLM reconstruction): our snapshot refs are
//! derived from role + accessible name, so `e14` *is*
//! `{role:"textbox", name:"Email"}`, which maps ~1:1 to
//! `page.getByRole('textbox', { name: 'Email' })`. We own the chokepoint, so we
//! record the durable-locator info at the moment of every action.
//!
//! See `plans/browser-codegen.md` (CP15).

use std::sync::Mutex;

use serde_json::{Value, json};

/// A durable, *unique* structural locator captured for an element at record
/// time (verified `querySelectorAll().length === 1` in the page). More stable
/// than an accessible name (which can carry counts/dates/localization).
#[derive(Debug, Clone, PartialEq)]
pub enum DurableLoc {
    /// A `data-testid` value → `page.getByTestId(v)` (Playwright's most durable).
    TestId(String),
    /// A unique CSS selector (id / other test-id attr / link href / name attr).
    Css(String),
}

/// A resolved element target → a Playwright locator.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub role: String,
    pub name: String,
    /// `Some(i)` when this `(role, name)` matched >1 element on the page at
    /// record time — codegen uses it (via `.nth(i)`) only as a last-resort
    /// disambiguator when no `durable` locator is available.
    pub index: Option<usize>,
    /// The most durable *unique* locator for this element, if one was found.
    /// Preferred over a positional `.nth(i)`, and (for a `data-testid`) over the
    /// accessible name too.
    pub durable: Option<DurableLoc>,
}

impl Target {
    pub fn new(role: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            name: name.into(),
            index: None,
            durable: None,
        }
    }

    /// Set the disambiguation index (`Some` only when there were duplicates).
    pub fn with_index(mut self, index: Option<usize>) -> Self {
        self.index = index;
        self
    }

    /// Attach the durable unique locator (if one was resolved at record time).
    pub fn with_durable(mut self, durable: Option<DurableLoc>) -> Self {
        self.durable = durable;
        self
    }
}

/// One field of a recorded `fill_form` batch.
#[derive(Debug, Clone, PartialEq)]
pub struct FormFieldRec {
    pub target: Target,
    pub value: String,
    /// `"checkbox"` | `"select"` | text (default).
    pub kind: Option<String>,
}

/// A single recorded action, in a form that maps mechanically to Playwright.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordedAction {
    Navigate {
        url: String,
    },
    NavigateBack,
    Click {
        target: Target,
        button: String,
        double: bool,
        modifiers: Vec<String>,
    },
    Type {
        target: Target,
        text: String,
        submit: bool,
        slowly: bool,
        delay_ms: u64,
    },
    FillForm {
        fields: Vec<FormFieldRec>,
    },
    SelectOption {
        target: Target,
        values: Vec<String>,
    },
    PressKey {
        key: String,
    },
    Hover {
        target: Target,
    },
    ScrollTo {
        target: Target,
    },
    ScrollBy {
        dx: f64,
        dy: f64,
    },
    FileUpload {
        target: Target,
        paths: Vec<String>,
    },
    Drag {
        from: Target,
        to: Target,
    },
    MouseMoveXy {
        x: f64,
        y: f64,
    },
    MouseClickXy {
        x: f64,
        y: f64,
        button: String,
        double: bool,
    },
    MouseDownXy {
        x: f64,
        y: f64,
        button: String,
    },
    MouseUpXy {
        x: f64,
        y: f64,
        button: String,
    },
    MouseDragXy {
        sx: f64,
        sy: f64,
        ex: f64,
        ey: f64,
    },
    MouseWheel {
        dx: f64,
        dy: f64,
    },
    WaitForText {
        text: String,
    },
    WaitForTextGone {
        text: String,
    },
    HandleDialog {
        accept: bool,
        prompt_text: Option<String>,
    },
    VerifyElementVisible {
        target: Target,
    },
    VerifyListVisible {
        target: Target,
    },
    VerifyTextVisible {
        text: String,
    },
    VerifyValue {
        target: Target,
        value: String,
    },
}

#[derive(Debug, Clone)]
struct Entry {
    action: RecordedAction,
    /// The page URL observed *after* the action completed. A change vs. the
    /// previous entry's URL ⇒ codegen inserts a `waitForLoadState()`.
    url: String,
}

struct Recording {
    active: bool,
    start_url: String,
    storage_state: Option<Value>,
    entries: Vec<Entry>,
}

static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);

/// Begin a fresh recording, discarding any previous buffer. `storage_state` (if
/// captured at `start`) seeds `test.use({ storageState: 'storage.json' })`.
pub fn start(start_url: String, storage_state: Option<Value>) {
    let mut guard = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Recording {
        active: true,
        start_url,
        storage_state,
        entries: Vec::new(),
    });
}

/// True while a recording is active and accepting actions.
pub fn is_recording() -> bool {
    RECORDING
        .lock()
        .map(|g| g.as_ref().map(|r| r.active).unwrap_or(false))
        .unwrap_or(false)
}

/// Append a successful action with its post-action URL (no-op if inactive).
pub fn push(action: RecordedAction, url: String) {
    let mut guard = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rec) = guard.as_mut() {
        if rec.active {
            rec.entries.push(Entry { action, url });
        }
    }
}

/// Stop accepting actions; the buffer is kept so `codegen` can still run.
/// Returns the number of recorded actions.
pub fn stop() -> usize {
    let mut guard = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(rec) => {
            rec.active = false;
            rec.entries.len()
        }
        None => 0,
    }
}

/// Status as a JSON object for the `browser_record` tool.
pub fn status_json() -> Value {
    let guard = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(rec) => json!({
            "recording": rec.active,
            "actions": rec.entries.len(),
            "storageStateCaptured": rec.storage_state.is_some(),
            "startUrl": rec.start_url,
        }),
        None => json!({ "recording": false, "actions": 0, "storageStateCaptured": false }),
    }
}

/// Generate the Playwright spec from the current buffer.
///
/// Returns `(script, storage_state)` — the spec text and, when storage state
/// was captured at `start`, the JSON object the Node side should write to
/// `storage.json` next to the spec (the script references it relatively).
pub fn codegen() -> (String, Option<Value>) {
    let guard = RECORDING.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(rec) => (render(rec), rec.storage_state.clone()),
        None => (render_empty(), None),
    }
}

fn render_empty() -> String {
    "import { test, expect } from '@playwright/test';\n\n\
     test('recorded flow', async ({ page }) => {\n  \
     // No actions were recorded.\n});\n"
        .to_string()
}

fn render(rec: &Recording) -> String {
    let mut out = String::new();
    out.push_str("import { test, expect } from '@playwright/test';\n\n");

    if rec.storage_state.is_some() {
        out.push_str("test.use({ storageState: 'storage.json' });\n\n");
    }

    out.push_str("test('recorded flow', async ({ page }) => {\n");

    // Dialog handler must be registered before the action that triggers it.
    if let Some((accept, prompt_text)) = rec.entries.iter().find_map(|e| match &e.action {
        RecordedAction::HandleDialog {
            accept,
            prompt_text,
        } => Some((*accept, prompt_text.clone())),
        _ => None,
    }) {
        let handler = if accept {
            match prompt_text {
                Some(t) => format!("dialog.accept({})", js_str(&t)),
                None => "dialog.accept()".to_string(),
            }
        } else {
            "dialog.dismiss()".to_string()
        };
        out.push_str(&format!("  page.on('dialog', dialog => {handler});\n"));
    }

    // Ensure the script lands on the right page even if the recording started
    // after navigation (e.g. storageState-seeded auth, no explicit navigate).
    let starts_with_nav = matches!(
        rec.entries.first().map(|e| &e.action),
        Some(RecordedAction::Navigate { .. })
    );
    let seedable = !rec.start_url.is_empty() && rec.start_url != "about:blank";
    let mut last_url = rec.start_url.clone();
    if !starts_with_nav && seedable {
        push_line(&mut out, &format!("await page.goto({});", js_str(&rec.start_url)));
        push_line(&mut out, "await page.waitForLoadState();");
    }

    for entry in &rec.entries {
        for line in render_action(&entry.action) {
            push_line(&mut out, &line);
        }
        // A URL change after a non-navigation action ⇒ a navigation happened
        // (e.g. a click that submitted a form); wait for it to settle.
        let is_nav = matches!(
            entry.action,
            RecordedAction::Navigate { .. } | RecordedAction::NavigateBack
        );
        if !is_nav && entry.url != last_url && !entry.url.is_empty() {
            push_line(&mut out, "await page.waitForLoadState();");
        }
        if !entry.url.is_empty() {
            last_url = entry.url.clone();
        }
    }

    out.push_str("});\n");
    out
}

/// Append a body line at 2-space indent.
fn push_line(out: &mut String, line: &str) {
    out.push_str("  ");
    out.push_str(line);
    out.push('\n');
}

fn render_action(action: &RecordedAction) -> Vec<String> {
    match action {
        RecordedAction::Navigate { url } => vec![
            format!("await page.goto({});", js_str(url)),
            "await page.waitForLoadState();".to_string(),
        ],
        RecordedAction::NavigateBack => vec![
            "await page.goBack();".to_string(),
            "await page.waitForLoadState();".to_string(),
        ],
        RecordedAction::Click {
            target,
            button,
            double,
            modifiers,
        } => {
            let opts = click_options(button, modifiers);
            let method = if *double { "dblclick" } else { "click" };
            vec![format!("await {}.{method}({opts});", locator(target))]
        }
        RecordedAction::Type {
            target,
            text,
            submit,
            slowly,
            delay_ms,
        } => {
            let mut lines: Vec<String> = Vec::new();
            if *slowly {
                let opts = if *delay_ms > 0 {
                    format!("{}, {{ delay: {delay_ms} }}", js_str(text))
                } else {
                    js_str(text)
                };
                lines.push(format!("await {}.pressSequentially({opts});", locator(target)));
            } else {
                lines.push(format!("await {}.fill({});", locator(target), js_str(text)));
            }
            if *submit {
                lines.push(format!("await {}.press('Enter');", locator(target)));
            }
            lines
        }
        RecordedAction::FillForm { fields } => fields
            .iter()
            .flat_map(|f| {
                let loc = locator(&f.target);
                match f.kind.as_deref() {
                    Some("checkbox") => {
                        let checked = matches!(f.value.as_str(), "true" | "1" | "checked" | "on");
                        vec![format!("await {loc}.setChecked({checked});")]
                    }
                    Some("select") => {
                        vec![format!("await {loc}.selectOption({});", js_str(&f.value))]
                    }
                    _ => vec![format!("await {loc}.fill({});", js_str(&f.value))],
                }
            })
            .collect(),
        RecordedAction::SelectOption { target, values } => {
            vec![format!(
                "await {}.selectOption({});",
                locator(target),
                js_str_array(values)
            )]
        }
        RecordedAction::PressKey { key } => {
            vec![format!("await page.keyboard.press({});", js_str(key))]
        }
        RecordedAction::Hover { target } => {
            vec![format!("await {}.hover();", locator(target))]
        }
        RecordedAction::ScrollTo { target } => {
            vec![format!("await {}.scrollIntoViewIfNeeded();", locator(target))]
        }
        RecordedAction::ScrollBy { dx, dy } => {
            vec![format!("await page.mouse.wheel({}, {});", num(*dx), num(*dy))]
        }
        RecordedAction::FileUpload { target, paths } => {
            vec![format!(
                "await {}.setInputFiles({});",
                locator(target),
                js_str_array(paths)
            )]
        }
        RecordedAction::Drag { from, to } => {
            vec![format!("await {}.dragTo({});", locator(from), locator(to))]
        }
        RecordedAction::MouseMoveXy { x, y } => {
            vec![format!("await page.mouse.move({}, {});", num(*x), num(*y))]
        }
        RecordedAction::MouseClickXy {
            x,
            y,
            button,
            double,
        } => {
            let mut opts: Vec<String> = Vec::new();
            if button != "left" {
                opts.push(format!("button: {}", js_str(button)));
            }
            if *double {
                opts.push("clickCount: 2".to_string());
            }
            let opts = if opts.is_empty() {
                String::new()
            } else {
                format!(", {{ {} }}", opts.join(", "))
            };
            vec![format!(
                "await page.mouse.click({}, {}{opts});",
                num(*x),
                num(*y)
            )]
        }
        RecordedAction::MouseDownXy { x, y, button } => vec![
            format!("await page.mouse.move({}, {});", num(*x), num(*y)),
            format!("await page.mouse.down({});", mouse_button_opts(button)),
        ],
        RecordedAction::MouseUpXy { x, y, button } => vec![
            format!("await page.mouse.move({}, {});", num(*x), num(*y)),
            format!("await page.mouse.up({});", mouse_button_opts(button)),
        ],
        RecordedAction::MouseDragXy { sx, sy, ex, ey } => vec![
            format!("await page.mouse.move({}, {});", num(*sx), num(*sy)),
            "await page.mouse.down();".to_string(),
            format!("await page.mouse.move({}, {});", num(*ex), num(*ey)),
            "await page.mouse.up();".to_string(),
        ],
        RecordedAction::MouseWheel { dx, dy } => {
            vec![format!("await page.mouse.wheel({}, {});", num(*dx), num(*dy))]
        }
        RecordedAction::WaitForText { text } => {
            vec![format!(
                "await expect(page.getByText({})).toBeVisible();",
                js_str(text)
            )]
        }
        RecordedAction::WaitForTextGone { text } => {
            vec![format!(
                "await expect(page.getByText({})).toHaveCount(0);",
                js_str(text)
            )]
        }
        // Registered once at the top of the test body; nothing inline.
        RecordedAction::HandleDialog { .. } => Vec::new(),
        RecordedAction::VerifyElementVisible { target } => {
            vec![format!("await expect({}).toBeVisible();", locator(target))]
        }
        RecordedAction::VerifyListVisible { target } => {
            vec![format!("await expect({}).toBeVisible();", locator(target))]
        }
        RecordedAction::VerifyTextVisible { text } => {
            vec![format!(
                "await expect(page.getByText({})).toBeVisible();",
                js_str(text)
            )]
        }
        RecordedAction::VerifyValue { target, value } => {
            vec![format!(
                "await expect({}).toHaveValue({});",
                locator(target),
                js_str(value)
            )]
        }
    }
}

/// Primary locator: `page.getByRole('role', { name: 'name', exact: true })`.
///
/// `exact: true` matters: the recorded `name` is the element's *full* accessible
/// name, but Playwright's default `getByRole` name match is a case-insensitive
/// **substring**, which over-matches (e.g. `'Secure Area'` also hits
/// `'Welcome to the Secure Area…'`). Exact matching is both more faithful to what
/// we recorded and kills that whole class of strict-mode collisions. (It does NOT
/// fix genuine multiplicity — N truly identical elements — which still needs
/// `.nth(i)`/a fallback selector; see the codegen plan §4.4.)
fn locator(t: &Target) -> String {
    // 1. data-testid → Playwright's most durable locator. Test ids are an
    //    intentional, stable contract, so prefer one even over a unique name.
    if let Some(DurableLoc::TestId(v)) = &t.durable {
        return format!("page.getByTestId({})", js_str(v));
    }
    let base = if t.name.is_empty() {
        format!("page.getByRole({})", js_str(&t.role))
    } else {
        format!(
            "page.getByRole({}, {{ name: {}, exact: true }})",
            js_str(&t.role),
            js_str(&t.name)
        )
    };
    // 2. Unambiguous role+name → accessible-first. NOTE: we deliberately do NOT
    //    override a unique accessible name with an id/href selector — ids and
    //    hrefs can be framework-generated/volatile (e.g. React `:r1:`, session
    //    params), so an accessible name is the safer default here.
    if t.index.is_none() {
        return base;
    }
    // 3. Ambiguous (role+name matched >1): a *verified-unique* structural
    //    selector is a semantic disambiguator — strictly better than guessing by
    //    DOM position.
    if let Some(DurableLoc::Css(sel)) = &t.durable {
        return format!("page.locator({})", js_str(sel));
    }
    // 4. Last resort: positional index.
    format!("{base}.nth({})", t.index.unwrap_or(0))
}

fn click_options(button: &str, modifiers: &[String]) -> String {
    let mut opts: Vec<String> = Vec::new();
    if button != "left" {
        opts.push(format!("button: {}", js_str(button)));
    }
    if !modifiers.is_empty() {
        opts.push(format!("modifiers: {}", js_str_array(modifiers)));
    }
    if opts.is_empty() {
        String::new()
    } else {
        format!("{{ {} }}", opts.join(", "))
    }
}

fn mouse_button_opts(button: &str) -> String {
    if button == "left" {
        String::new()
    } else {
        format!("{{ button: {} }}", js_str(button))
    }
}

/// Format an f64 without a trailing `.0` for whole numbers (cleaner output).
fn num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// A single-quoted JS string literal with the necessary escapes.
fn js_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// `['a', 'b']` from a slice of strings.
fn js_str_array(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| js_str(s)).collect();
    format!("[{}]", inner.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(start_url: &str, entries: Vec<RecordedAction>) -> Recording {
        Recording {
            active: false,
            start_url: start_url.to_string(),
            storage_state: None,
            entries: entries
                .into_iter()
                .map(|action| Entry {
                    action,
                    url: start_url.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn click_maps_to_get_by_role() {
        let r = rec(
            "https://example.com",
            vec![RecordedAction::Click {
                target: Target::new("button", "Sign In"),
                button: "left".into(),
                double: false,
                modifiers: vec![],
            }],
        );
        let script = render(&r);
        assert!(script.contains(
            "await page.getByRole('button', { name: 'Sign In', exact: true }).click();"
        ));
        assert!(script.contains("import { test, expect } from '@playwright/test';"));
    }

    #[test]
    fn type_with_submit_fills_then_presses_enter() {
        let r = rec(
            "https://example.com",
            vec![RecordedAction::Type {
                target: Target::new("textbox", "Email"),
                text: "a@b.com".into(),
                submit: true,
                slowly: false,
                delay_ms: 0,
            }],
        );
        let script = render(&r);
        assert!(script.contains(".fill('a@b.com');"));
        assert!(script.contains(".press('Enter');"));
    }

    #[test]
    fn navigate_emits_goto_and_wait() {
        let r = rec(
            "about:blank",
            vec![RecordedAction::Navigate {
                url: "https://example.com/login".into(),
            }],
        );
        let script = render(&r);
        assert!(script.contains("await page.goto('https://example.com/login');"));
        assert!(script.contains("await page.waitForLoadState();"));
        // No seed goto when the first action is already a navigate.
        assert_eq!(script.matches("page.goto").count(), 1);
    }

    #[test]
    fn seeds_initial_goto_when_no_leading_navigate() {
        let r = rec(
            "https://app.example.com/dashboard",
            vec![RecordedAction::VerifyTextVisible {
                text: "Welcome".into(),
            }],
        );
        let script = render(&r);
        assert!(script.contains("await page.goto('https://app.example.com/dashboard');"));
    }

    #[test]
    fn storage_state_seeds_test_use() {
        let mut r = rec("https://app.example.com", vec![]);
        r.storage_state = Some(json!({ "cookies": [] }));
        let script = render(&r);
        assert!(script.contains("test.use({ storageState: 'storage.json' });"));
    }

    #[test]
    fn dialog_registers_handler_at_top() {
        let r = rec(
            "https://example.com",
            vec![RecordedAction::HandleDialog {
                accept: true,
                prompt_text: None,
            }],
        );
        let script = render(&r);
        assert!(script.contains("page.on('dialog', dialog => dialog.accept());"));
    }

    #[test]
    fn click_with_button_and_modifiers() {
        let r = rec(
            "https://example.com",
            vec![RecordedAction::Click {
                target: Target::new("link", "Open"),
                button: "right".into(),
                double: true,
                modifiers: vec!["Shift".into()],
            }],
        );
        let script = render(&r);
        assert!(script.contains(".dblclick({ button: 'right', modifiers: ['Shift'] });"));
    }

    #[test]
    fn ambiguous_target_emits_nth() {
        let r = rec(
            "https://shop.example.com",
            vec![RecordedAction::Click {
                target: Target::new("button", "Add to cart").with_index(Some(2)),
                button: "left".into(),
                double: false,
                modifiers: vec![],
            }],
        );
        let script = render(&r);
        assert!(script.contains(
            "await page.getByRole('button', { name: 'Add to cart', exact: true }).nth(2).click();"
        ));
    }

    #[test]
    fn testid_preferred_over_role_name() {
        let r = rec(
            "https://x.com",
            vec![RecordedAction::Click {
                target: Target::new("button", "Submit")
                    .with_durable(Some(DurableLoc::TestId("submit-btn".into()))),
                button: "left".into(),
                double: false,
                modifiers: vec![],
            }],
        );
        let s = render(&r);
        assert!(s.contains("await page.getByTestId('submit-btn').click();"));
        assert!(!s.contains("getByRole"));
    }

    #[test]
    fn ambiguous_uses_css_durable_not_nth() {
        let r = rec(
            "https://x.com",
            vec![RecordedAction::Click {
                target: Target::new("link", "World")
                    .with_index(Some(3))
                    .with_durable(Some(DurableLoc::Css("a[href=\"/world\"]".into()))),
                button: "left".into(),
                double: false,
                modifiers: vec![],
            }],
        );
        let s = render(&r);
        assert!(s.contains("await page.locator('a[href=\"/world\"]').click();"));
        assert!(!s.contains(".nth("));
    }

    #[test]
    fn unambiguous_target_omits_nth() {
        let r = rec(
            "https://example.com",
            vec![RecordedAction::Click {
                target: Target::new("button", "Sign In").with_index(None),
                button: "left".into(),
                double: false,
                modifiers: vec![],
            }],
        );
        let script = render(&r);
        assert!(!script.contains(".nth("));
    }

    #[test]
    fn js_str_escapes_quotes_and_newlines() {
        assert_eq!(js_str("it's\n"), "'it\\'s\\n'");
    }

    #[test]
    fn url_change_inserts_wait_for_load() {
        let mut r = rec("https://example.com", vec![]);
        r.entries.push(Entry {
            action: RecordedAction::Click {
                target: Target::new("button", "Go"),
                button: "left".into(),
                double: false,
                modifiers: vec![],
            },
            url: "https://example.com/next".into(),
        });
        let script = render(&r);
        // Click that changed the URL is followed by a load wait.
        let after_click = &script[script.find(".click();").unwrap()..];
        assert!(after_click.contains("await page.waitForLoadState();"));
    }
}
