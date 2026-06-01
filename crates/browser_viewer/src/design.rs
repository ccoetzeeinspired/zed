//! Design-mode state + message shapes shared between the injected JS
//! and the Rust BrowserItem.
//!
//! The JS sends one of three message kinds (see
//! `design_mode_script.rs`); we parse them into [`DesignInbound`]
//! variants and apply state changes in `BrowserItem`.

use serde::{Deserialize, Serialize};

/// Where in the document an element sits, in CSS pixels relative to
/// the viewport. Matches `Element.getBoundingClientRect()` semantics:
/// `(x, y)` is the top-left, `(w, h)` is size — both float, both can
/// be negative when the element is scrolled off-screen.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ElementRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Best-effort identifier of where the element lives in source. Filled
/// by the script's `detectReactSource` cascade — `_debugSource` is the
/// gold path, `data-source-*` attributes come next, then coarse
/// component / testid hints.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementSource {
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub line_number: Option<u32>,
    #[serde(default)]
    pub column_number: Option<u32>,
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub testid: Option<String>,
}

/// A clicked element, ready for the host to anchor the "Describe the
/// change" input next to and bundle into the submission.
#[derive(Debug, Clone, Deserialize)]
pub struct ElementSelection {
    pub selector: String,
    #[serde(rename = "outerHTML")]
    pub outer_html: String,
    pub rect: ElementRect,
    #[serde(default)]
    pub source: Option<ElementSource>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub classes: Option<String>,
}

/// Page-scroll notification — fires only while a selection is active,
/// so the host can re-anchor the floating input without burning IPC
/// on every idle scroll.
#[derive(Debug, Clone, Deserialize)]
pub struct ScrollUpdate {
    #[serde(rename = "scrollX")]
    pub scroll_x: f32,
    #[serde(rename = "scrollY")]
    pub scroll_y: f32,
    pub rect: ElementRect,
}

/// Tagged union of everything the injected JS posts back. Anything
/// not matching one of the known kinds is dropped silently — keeps
/// the protocol forward-compatible with future script revisions
/// without breaking old hosts.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DesignInbound {
    /// The script finished installing.
    Ready,
    /// User clicked an element while design mode was armed.
    ElementSelected(ElementSelection),
    /// User scrolled while a selection was active.
    PageScrolled(ScrollUpdate),
}

impl DesignInbound {
    /// Parse a raw message string from the page. Returns `None` for
    /// malformed JSON or unknown `kind` values — callers should
    /// silently ignore those.
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}
