//! Typed message shapes for browser automation commands.
//!
//! The injected browser automation script performs DOM-side target
//! resolution and reports results through WebView2 `postMessage`. Rust owns
//! the durable target state, overlay rendering, and native WebView2 input.

use serde::{Deserialize, Serialize};

use crate::design::{ElementRect, ElementSource};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserElementQuery {
    Selected,
    Selector { selector: String },
    TextExact { text: String },
    TextContains { text: String },
    RoleAndName { role: String, name: String },
    Point { x: f32, y: f32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserTargetConfidence {
    Exact,
    Strong,
    Ambiguous,
    Weak,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserResolvedElement {
    pub selector: String,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub accessible_name: Option<String>,
    pub rect: ElementRect,
    #[serde(default)]
    pub source: Option<ElementSource>,
    pub confidence: BrowserTargetConfidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTypeTextOutcome {
    pub ok: bool,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPageState {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub active_selector: Option<String>,
    #[serde(default)]
    pub active_value: Option<String>,
    #[serde(default)]
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentVisibleElementsSnapshot {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub elements: Vec<BrowserResolvedElement>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSnapshotNode {
    #[serde(default, rename = "ref")]
    pub node_ref: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub rect: Option<ElementRect>,
    #[serde(default)]
    pub children: Vec<BrowserSnapshotNode>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub editable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAgentSnapshot {
    pub snapshot_id: String,
    pub page_revision: u64,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub root: Vec<BrowserSnapshotNode>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserActionabilityChecks {
    pub attached: bool,
    pub visible: bool,
    pub stable: bool,
    pub enabled: bool,
    pub editable: bool,
    pub receives_events: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserActionabilityOutcome {
    pub ok: bool,
    pub selector: String,
    pub checks: BrowserActionabilityChecks,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAgentTraceEntry {
    pub sequence: u64,
    pub tool: String,
    #[serde(default, rename = "requestId")]
    pub request_id: Option<String>,
    #[serde(default, rename = "snapshotId")]
    pub snapshot_id: Option<String>,
    #[serde(default, rename = "ref")]
    pub element_ref: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    pub ok: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConsoleEventSummary {
    pub sequence: u64,
    pub level: String,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub column: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserNetworkEventSummary {
    pub sequence: u64,
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub error_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserExpectResult {
    pub ok: bool,
    pub kind: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub observed: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowserAutomationInbound {
    AgentTargetResolved {
        #[serde(rename = "requestId")]
        request_id: String,
        target: BrowserResolvedElement,
    },
    AgentTargetNotFound {
        #[serde(rename = "requestId")]
        request_id: String,
        reason: String,
    },
    AgentTargetAmbiguous {
        #[serde(rename = "requestId")]
        request_id: String,
        candidates: Vec<BrowserResolvedElement>,
    },
    AgentTypeTextResult {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(flatten)]
        outcome: AgentTypeTextOutcome,
    },
    AgentPageStateResult {
        #[serde(rename = "requestId")]
        request_id: String,
        state: AgentPageState,
    },
    AgentVisibleElementsResult {
        #[serde(rename = "requestId")]
        request_id: String,
        snapshot: AgentVisibleElementsSnapshot,
    },
    AgentSnapshotResult {
        #[serde(rename = "requestId")]
        request_id: String,
        snapshot: BrowserAgentSnapshot,
    },
    AgentActionabilityResult {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(flatten)]
        outcome: BrowserActionabilityOutcome,
    },
}

impl BrowserAutomationInbound {
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::ElementRect;

    #[test]
    fn browser_snapshot_round_trips_refs_and_text_nodes() {
        let snapshot = BrowserAgentSnapshot {
            snapshot_id: "snap-1".to_string(),
            page_revision: 7,
            url: Some("https://example.test/products".to_string()),
            title: Some("Products".to_string()),
            root: vec![
                BrowserSnapshotNode {
                    node_ref: Some("e1".to_string()),
                    role: Some("link".to_string()),
                    name: Some("Standing Desk".to_string()),
                    text: None,
                    selector: Some("main > a".to_string()),
                    rect: Some(ElementRect {
                        x: 10.,
                        y: 20.,
                        w: 120.,
                        h: 30.,
                    }),
                    children: Vec::new(),
                    disabled: false,
                    editable: false,
                },
                BrowserSnapshotNode {
                    node_ref: None,
                    role: Some("text".to_string()),
                    name: None,
                    text: Some("From R 1,599".to_string()),
                    selector: None,
                    rect: None,
                    children: Vec::new(),
                    disabled: false,
                    editable: false,
                },
            ],
            reason: None,
        };

        let encoded = serde_json::to_string(&snapshot).expect("snapshot should encode");
        let decoded: BrowserAgentSnapshot =
            serde_json::from_str(&encoded).expect("snapshot should decode");

        assert_eq!(decoded.snapshot_id, "snap-1");
        assert_eq!(decoded.page_revision, 7);
        assert_eq!(decoded.root[0].node_ref.as_deref(), Some("e1"));
        assert_eq!(decoded.root[1].text.as_deref(), Some("From R 1,599"));
    }

    #[test]
    fn browser_automation_inbound_parses_agent_snapshot_result() {
        let raw = r#"{
            "kind": "agent_snapshot_result",
            "requestId": "snapshot-1",
            "snapshot": {
                "snapshotId": "snap-1",
                "pageRevision": 3,
                "url": "https://example.test",
                "title": "Example",
                "root": [
                    {
                        "ref": "e1",
                        "role": "button",
                        "name": "Submit",
                        "selector": "button",
                        "rect": { "x": 1, "y": 2, "w": 3, "h": 4 },
                        "children": [],
                        "disabled": false,
                        "editable": false
                    }
                ]
            }
        }"#;

        let parsed = BrowserAutomationInbound::parse(raw).expect("message should parse");
        match parsed {
            BrowserAutomationInbound::AgentSnapshotResult {
                request_id,
                snapshot,
            } => {
                assert_eq!(request_id, "snapshot-1");
                assert_eq!(snapshot.snapshot_id, "snap-1");
                assert_eq!(snapshot.root[0].node_ref.as_deref(), Some("e1"));
            }
            other => panic!("expected AgentSnapshotResult, got {other:?}"),
        }
    }

    #[test]
    fn browser_automation_inbound_parses_actionability_result() {
        let raw = r#"{
            "kind": "agent_actionability_result",
            "requestId": "act-1",
            "ok": false,
            "selector": "button",
            "checks": {
                "attached": true,
                "visible": true,
                "stable": true,
                "enabled": false,
                "editable": false,
                "receivesEvents": true
            },
            "reason": "Element is disabled"
        }"#;

        let parsed = BrowserAutomationInbound::parse(raw).expect("message should parse");
        match parsed {
            BrowserAutomationInbound::AgentActionabilityResult {
                request_id,
                outcome,
            } => {
                assert_eq!(request_id, "act-1");
                assert!(!outcome.ok);
                assert_eq!(outcome.reason.as_deref(), Some("Element is disabled"));
                assert!(!outcome.checks.enabled);
            }
            other => panic!("expected AgentActionabilityResult, got {other:?}"),
        }
    }

    #[test]
    fn browser_agent_trace_entry_uses_mcp_field_names() {
        let entry = BrowserAgentTraceEntry {
            sequence: 3,
            tool: "browser.click".to_string(),
            request_id: Some("snap-1:e2".to_string()),
            snapshot_id: Some("snap-1".to_string()),
            element_ref: Some("e2".to_string()),
            selector: Some("button#login".to_string()),
            ok: false,
            reason: Some("Element is disabled".to_string()),
        };

        let value = serde_json::to_value(entry).expect("trace entry should serialize");

        assert_eq!(value["requestId"], "snap-1:e2");
        assert_eq!(value["snapshotId"], "snap-1");
        assert_eq!(value["ref"], "e2");
        assert_eq!(value["selector"], "button#login");
    }

    #[test]
    fn browser_diagnostic_events_use_mcp_field_names() {
        let console = BrowserConsoleEventSummary {
            sequence: 7,
            level: "error".to_string(),
            text: "Uncaught Error: boom".to_string(),
            url: Some("https://example.test/app.js".to_string()),
            line: Some(42),
            column: Some(5),
        };
        let network = BrowserNetworkEventSummary {
            sequence: 8,
            url: "https://example.test/api".to_string(),
            method: Some("GET".to_string()),
            status: Some(500),
            error_text: None,
        };

        let console_value = serde_json::to_value(console).expect("console event serializes");
        let network_value = serde_json::to_value(network).expect("network event serializes");

        assert_eq!(console_value["sequence"], 7);
        assert_eq!(console_value["level"], "error");
        assert_eq!(console_value["text"], "Uncaught Error: boom");
        assert_eq!(console_value["line"], 42);
        assert_eq!(network_value["url"], "https://example.test/api");
        assert_eq!(network_value["status"], 500);
    }

    #[test]
    fn browser_expect_result_serializes_assertion_context() {
        let result = BrowserExpectResult {
            ok: false,
            kind: "text_contains".to_string(),
            value: Some("Reviews".to_string()),
            selector: Some("main".to_string()),
            observed: Some("Product details".to_string()),
            reason: Some("Timed out waiting for text".to_string()),
        };

        let value = serde_json::to_value(result).expect("expect result serializes");

        assert_eq!(value["ok"], false);
        assert_eq!(value["kind"], "text_contains");
        assert_eq!(value["value"], "Reviews");
        assert_eq!(value["selector"], "main");
        assert_eq!(value["observed"], "Product details");
        assert_eq!(value["reason"], "Timed out waiting for text");
    }
}
