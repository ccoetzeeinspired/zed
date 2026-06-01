//! Agent cursor state for the embedded browser.

use crate::browser_protocol::BrowserResolvedElement;

#[derive(Debug, Clone)]
pub enum AgentCursorStatus {
    Preview,
    Clicking,
    Clicked,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct AgentCursorState {
    pub request_id: String,
    pub target: BrowserResolvedElement,
    pub status: AgentCursorStatus,
    pub label: String,
    pub ambiguity: Vec<BrowserResolvedElement>,
    pub pointer_position: Option<(f32, f32)>,
}

impl AgentCursorState {
    pub fn preview(request_id: String, target: BrowserResolvedElement) -> Self {
        let label = target
            .accessible_name
            .as_ref()
            .or(target.text.as_ref())
            .map(|text| {
                let tag = target.tag.as_deref().unwrap_or("element");
                let excerpt = text.chars().take(48).collect::<String>();
                format!("{tag} \"{excerpt}\"")
            })
            .unwrap_or_else(|| target.tag.clone().unwrap_or_else(|| "element".to_string()));

        Self {
            request_id,
            target,
            status: AgentCursorStatus::Preview,
            label,
            ambiguity: Vec::new(),
            pointer_position: Some((24., 24.)),
        }
    }
}
