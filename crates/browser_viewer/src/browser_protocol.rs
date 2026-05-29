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
}

impl BrowserAutomationInbound {
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}
