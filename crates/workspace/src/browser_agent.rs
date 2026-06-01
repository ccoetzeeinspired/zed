use anyhow::{Result, anyhow};
use gpui::{App, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum BrowserAgentRequest {
    CurrentPage,
    Open {
        url: String,
    },
    Navigate {
        url: String,
    },
    Snapshot {
        request_id: String,
    },
    Screenshot {
        request_id: String,
    },
    ClickRef {
        snapshot_id: String,
        element_ref: String,
    },
    FillRef {
        snapshot_id: String,
        element_ref: String,
        text: String,
        submit: bool,
    },
    ScrollToRef {
        request_id: String,
        snapshot_id: String,
        element_ref: String,
        #[serde(default = "default_scroll_align")]
        align: String,
    },
    Scroll {
        request_id: String,
        delta_x: f64,
        delta_y: f64,
        #[serde(default = "default_scroll_steps")]
        steps: u32,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
    },
    FindElement {
        request_id: String,
        query_kind: String,
        query: String,
    },
    ClickElement {
        request_id: String,
    },
    TypeText {
        request_id: String,
        text: String,
    },
    Console {
        #[serde(default)]
        level: Option<String>,
        #[serde(default = "default_diagnostic_limit")]
        limit: usize,
    },
    Network {
        #[serde(default)]
        failures_only: bool,
        #[serde(default = "default_diagnostic_limit")]
        limit: usize,
    },
    Expect {
        kind: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        selector: Option<String>,
        #[serde(default = "default_expect_timeout_ms")]
        timeout_ms: u64,
    },
    Trace,
    ClearCursor,
}

fn default_scroll_steps() -> u32 {
    8
}

fn default_scroll_align() -> String {
    "center".to_string()
}

fn default_diagnostic_limit() -> usize {
    20
}

fn default_expect_timeout_ms() -> u64 {
    1_000
}

pub type BrowserAgentHandler = Rc<dyn Fn(BrowserAgentRequest, &mut App) -> Task<Result<Value>>>;
pub type BrowserAgentOpenHandler = Rc<dyn Fn(String, &mut App) -> Task<Result<Value>>>;

thread_local! {
    static ACTIVE_BROWSER_HANDLER: RefCell<Option<BrowserAgentHandler>> = const { RefCell::new(None) };
    static BROWSER_OPEN_HANDLER: RefCell<Option<BrowserAgentOpenHandler>> = const { RefCell::new(None) };
}

pub fn register_active_browser(handler: BrowserAgentHandler) {
    ACTIVE_BROWSER_HANDLER.with(|active| {
        *active.borrow_mut() = Some(handler);
    });
}

pub fn register_browser_opener(handler: BrowserAgentOpenHandler) {
    BROWSER_OPEN_HANDLER.with(|opener| {
        *opener.borrow_mut() = Some(handler);
    });
}

pub fn call_active_browser(request: BrowserAgentRequest, cx: &mut App) -> Task<Result<Value>> {
    ACTIVE_BROWSER_HANDLER.with(|active| {
        if let BrowserAgentRequest::Open { url } = request {
            return BROWSER_OPEN_HANDLER.with(|opener| {
                let Some(opener) = opener.borrow().as_ref().cloned() else {
                    return Task::ready(Err(anyhow!("No Zed browser workspace is available")));
                };
                opener(url, cx)
            });
        }

        if let Some(handler) = active.borrow().as_ref().cloned() {
            return handler(request, cx);
        }

        if let BrowserAgentRequest::Navigate { url } = request {
            return BROWSER_OPEN_HANDLER.with(|opener| {
                let Some(opener) = opener.borrow().as_ref().cloned() else {
                    return Task::ready(Err(anyhow!("No Zed browser workspace is available")));
                };
                opener(url, cx)
            });
        }

        Task::ready(Err(anyhow!("No active Zed browser tab is registered")))
    })
}

#[cfg(test)]
mod tests {
    use super::BrowserAgentRequest;

    #[test]
    fn browser_agent_scroll_request_round_trips() {
        let request = serde_json::from_value::<BrowserAgentRequest>(serde_json::json!({
            "name": "scroll",
            "request_id": "slow-scroll",
            "delta_x": 0.0,
            "delta_y": 720.0,
            "steps": 8,
            "x": 575.0,
            "y": 420.0
        }));

        assert!(
            request.is_ok(),
            "scroll should be a first-class browser agent request: {request:?}"
        );

        assert_eq!(
            serde_json::to_value(request.unwrap()).unwrap(),
            serde_json::json!({
                "name": "scroll",
                "request_id": "slow-scroll",
                "delta_x": 0.0,
                "delta_y": 720.0,
                "steps": 8,
                "x": 575.0,
                "y": 420.0
            })
        );
    }

    #[test]
    fn browser_agent_screenshot_request_round_trips() {
        let request = serde_json::from_value::<BrowserAgentRequest>(serde_json::json!({
            "name": "screenshot",
            "request_id": "visual-recovery"
        }));

        assert!(
            request.is_ok(),
            "screenshot should be a first-class browser agent request: {request:?}"
        );

        assert_eq!(
            serde_json::to_value(request.unwrap()).unwrap(),
            serde_json::json!({
                "name": "screenshot",
                "request_id": "visual-recovery"
            })
        );
    }

    #[test]
    fn browser_agent_scroll_to_ref_request_round_trips() {
        let request = serde_json::from_value::<BrowserAgentRequest>(serde_json::json!({
            "name": "scroll_to_ref",
            "request_id": "reviews-scroll",
            "snapshot_id": "snapshot-product",
            "element_ref": "e42",
            "align": "center"
        }));

        assert!(
            request.is_ok(),
            "scroll_to_ref should be a first-class browser agent request: {request:?}"
        );

        assert_eq!(
            serde_json::to_value(request.unwrap()).unwrap(),
            serde_json::json!({
                "name": "scroll_to_ref",
                "request_id": "reviews-scroll",
                "snapshot_id": "snapshot-product",
                "element_ref": "e42",
                "align": "center"
            })
        );
    }

    #[test]
    fn browser_agent_console_request_round_trips() {
        let request = serde_json::from_value::<BrowserAgentRequest>(serde_json::json!({
            "name": "console",
            "level": "error",
            "limit": 10
        }));

        assert!(
            request.is_ok(),
            "console should be a browser request: {request:?}"
        );
        assert_eq!(
            serde_json::to_value(request.unwrap()).unwrap(),
            serde_json::json!({
                "name": "console",
                "level": "error",
                "limit": 10
            })
        );
    }

    #[test]
    fn browser_agent_network_request_round_trips() {
        let request = serde_json::from_value::<BrowserAgentRequest>(serde_json::json!({
            "name": "network",
            "failures_only": true,
            "limit": 20
        }));

        assert!(
            request.is_ok(),
            "network should be a browser request: {request:?}"
        );
        assert_eq!(
            serde_json::to_value(request.unwrap()).unwrap(),
            serde_json::json!({
                "name": "network",
                "failures_only": true,
                "limit": 20
            })
        );
    }

    #[test]
    fn browser_agent_expect_request_round_trips() {
        let request = serde_json::from_value::<BrowserAgentRequest>(serde_json::json!({
            "name": "expect",
            "kind": "text_contains",
            "value": "Reviews",
            "selector": "main",
            "timeout_ms": 1500
        }));

        assert!(
            request.is_ok(),
            "expect should be a browser request: {request:?}"
        );
        assert_eq!(
            serde_json::to_value(request.unwrap()).unwrap(),
            serde_json::json!({
                "name": "expect",
                "kind": "text_contains",
                "value": "Reviews",
                "selector": "main",
                "timeout_ms": 1500
            })
        );
    }
}
