//! CP4: navigate + wait-for helpers.

use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{App, AsyncApp, Entity};
use serde_json::Value;

use crate::automation::cdp::CdpSession;
use crate::browser_view::BrowserView;

/// Default timeout for navigation / wait-for.
pub const DEFAULT_NAV_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Options for [`run_wait_for`].
#[derive(Debug, Clone)]
pub struct WaitForOptions {
    /// Wait until the tab is not loading (`NavigationCompleted`).
    pub wait_load: bool,
    /// Wait until this substring appears in the document title or body text.
    pub text: Option<String>,
    /// Wait until this substring is *absent* from the document title and body.
    pub text_gone: Option<String>,
    pub timeout: Duration,
}

impl Default for WaitForOptions {
    fn default() -> Self {
        Self {
            wait_load: true,
            text: None,
            text_gone: None,
            timeout: DEFAULT_NAV_TIMEOUT,
        }
    }
}

/// Normalize user/MCP URL input (no search fallback — bare words are errors).
pub fn normalize_navigate_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("navigate URL is empty"));
    }
    let lower = trimmed.to_ascii_lowercase();
    const KNOWN_SCHEMES: &[&str] = &[
        "http://",
        "https://",
        "file://",
        "about:",
        "data:",
        "javascript:",
    ];
    if KNOWN_SCHEMES.iter().any(|p| lower.starts_with(p)) {
        return Ok(trimmed.to_string());
    }
    if trimmed.starts_with("localhost") || trimmed.starts_with("127.0.0.1") {
        return Ok(format!("http://{trimmed}"));
    }
    let looks_like_host = trimmed.contains('.')
        && !trimmed.contains(' ')
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if looks_like_host {
        return Ok(format!("https://{trimmed}"));
    }
    Err(anyhow!(
        "navigate URL {input:?} is not a URL — include a scheme (https://…) or a host with a dot"
    ))
}

struct TabState {
    is_loading: bool,
    title: String,
    url: String,
}

fn read_tab_state(view: &BrowserView, cx: &App) -> TabState {
    let item = view.item().read(cx);
    TabState {
        is_loading: item.is_loading(),
        title: item.title().to_string(),
        url: item.url().to_string(),
    }
}

fn page_contains_text_expression(text: &str) -> String {
    let needle = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(() => {{ const hay = (document.title || '') + '\\n' + (document.body?.innerText || ''); return hay.includes({needle}); }})()"
    )
}

fn evaluate_bool(session: &crate::webview2_host::WebView2Session, expression: &str) -> oneshot::Receiver<Result<bool>> {
    let (tx, rx) = oneshot::channel();
    let mut tx_slot = Some(tx);
    let cdp = CdpSession::new(session);
    let expression = expression.to_string();
    let _ = cdp.evaluate_expression(
        &expression,
        Box::new(move |result| {
            if let Some(tx) = tx_slot.take() {
                let parsed = result.and_then(value_to_bool);
                let _ = tx.send(parsed);
            }
        }),
    );
    rx
}

fn value_to_bool(value: Value) -> Result<bool> {
    let result = value.get("result").unwrap_or(&value);
    if let Some(inner) = result.get("value") {
        return inner
            .as_bool()
            .ok_or_else(|| anyhow!("Runtime.evaluate did not return a boolean"));
    }
    if let Some(details) = value.get("exceptionDetails") {
        return Err(anyhow!("Runtime.evaluate failed: {details}"));
    }
    Err(anyhow!("Runtime.evaluate returned unexpected shape"))
}

pub async fn wait_for(browser: Entity<BrowserView>, options: WaitForOptions, cx: &mut AsyncApp) {
    if wait_for_result(browser, options, cx).await.is_err() {
        // Errors already logged inside wait_for_result.
    }
}

pub async fn wait_for_result(
    browser: Entity<BrowserView>,
    options: WaitForOptions,
    cx: &mut AsyncApp,
) -> Result<()> {
    let deadline = std::time::Instant::now() + options.timeout;
    let label = options
        .text
        .as_deref()
        .map(|t| format!("text {t:?}"))
        .or_else(|| options.text_gone.as_deref().map(|t| format!("text-gone {t:?}")))
        .unwrap_or_else(|| "load".to_string());

    loop {
        let state = browser.update(cx, |view, cx| read_tab_state(view, cx));

        let load_ready = !options.wait_load || !state.is_loading;
        let mut text_ready = options.text.is_none();
        if let Some(ref needle) = options.text {
            if state.title.contains(needle.as_str()) {
                text_ready = true;
            } else if load_ready {
                let rx = browser.update(cx, |view, cx| {
                    view.with_webview_session(cx, |session| {
                        evaluate_bool(session, &page_contains_text_expression(needle))
                    })
                });
                if let Some(rx) = rx {
                    if let Ok(Ok(true)) = rx.await {
                        text_ready = true;
                    }
                }
            }
        }

        // text_gone: ready when the needle is in neither the title nor the body.
        let mut gone_ready = options.text_gone.is_none();
        if let Some(ref needle) = options.text_gone {
            if !state.title.contains(needle.as_str()) && load_ready {
                let rx = browser.update(cx, |view, cx| {
                    view.with_webview_session(cx, |session| {
                        evaluate_bool(session, &page_contains_text_expression(needle))
                    })
                });
                if let Some(rx) = rx {
                    if let Ok(Ok(false)) = rx.await {
                        gone_ready = true;
                    }
                }
            }
        }

        if load_ready && text_ready && gone_ready {
            log::info!(
                "browser automation wait for {label} OK (title={:?}, url={})",
                state.title,
                state.url
            );
            return Ok(());
        }

        if std::time::Instant::now() >= deadline {
            let message = format!(
                "browser automation wait for {label} timed out after {:?} (loading={}, title={:?}, url={})",
                options.timeout,
                state.is_loading,
                state.title,
                state.url
            );
            log::error!("{message}");
            return Err(anyhow!(message));
        }

        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_https_scheme() {
        assert_eq!(
            normalize_navigate_url("example.com").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn normalize_localhost_uses_http() {
        assert_eq!(
            normalize_navigate_url("localhost:3000").unwrap(),
            "http://localhost:3000"
        );
    }

    #[test]
    fn normalize_rejects_bare_words() {
        assert!(normalize_navigate_url("TrueLens").is_err());
    }
}
