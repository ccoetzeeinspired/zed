//! Accessibility snapshot → Playwright-shaped YAML + ref registry.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::Value;

use crate::automation::session::{DurableSelector, ElementHandle, ElementRef, RefRegistry};

/// Default maximum refs emitted per snapshot (token / perf guard). Raised from
/// the original 500 after large pages (e.g. naledi.co.za) exceeded it and left
/// late elements unaddressable. Override with `ZED_BROWSER_AUTOMATION_MAX_REFS`.
const DEFAULT_MAX_REFS: usize = 2000;

fn max_refs() -> usize {
    std::env::var("ZED_BROWSER_AUTOMATION_MAX_REFS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_REFS)
}

/// Parsed page snapshot for agent consumption.
#[derive(Debug, Clone)]
pub struct PageSnapshot {
    pub yaml: String,
    pub ref_count: usize,
    pub page_generation: u64,
    pub registry: RefRegistry,
}

#[derive(Debug, Clone, Deserialize)]
struct AxTreeResponse {
    #[serde(default)]
    nodes: Vec<AxNode>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxNode {
    #[serde(deserialize_with = "deserialize_node_id")]
    node_id: String,
    #[serde(default)]
    ignored: bool,
    #[serde(default)]
    role: Option<AxValue>,
    #[serde(default)]
    name: Option<AxValue>,
    #[serde(default)]
    properties: Vec<AxProperty>,
    #[serde(default, deserialize_with = "deserialize_node_id_list")]
    child_ids: Vec<String>,
    #[serde(default, rename = "backendDOMNodeId")]
    backend_dom_node_id: Option<Value>,
    #[serde(default, rename = "wkDomToken")]
    wk_dom_token: Option<String>,
    #[serde(default, rename = "durableSelector")]
    durable_selector: Option<String>,
    #[serde(default, rename = "durableSelectorKind")]
    durable_selector_kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AxValue {
    #[serde(default)]
    value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct AxProperty {
    name: String,
    value: AxValue,
}

impl AxValue {
    fn as_string(&self) -> Option<String> {
        match &self.value {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::Bool(b)) => Some(b.to_string()),
            None | Some(Value::Null) => None,
            _ => None,
        }
    }
}

fn deserialize_node_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value_to_node_id(value).map_err(serde::de::Error::custom)
}

fn deserialize_node_id_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(value_to_node_id)
        .collect::<Result<Vec<_>, _>>()
        .map_err(serde::de::Error::custom)
}

fn value_to_node_id(value: Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(format!("expected node id string or number, got {other}")),
    }
}

impl AxNode {
    fn role_str(&self) -> String {
        self.role
            .as_ref()
            .and_then(|r| r.as_string())
            .unwrap_or_else(|| "generic".to_string())
    }

    fn name_str(&self) -> String {
        self.name
            .as_ref()
            .and_then(|n| n.as_string())
            .unwrap_or_default()
    }

    fn property_value(&self, key: &str) -> Option<String> {
        self.properties
            .iter()
            .find(|p| p.name == key)
            .and_then(|p| p.value.as_string())
    }

    fn backend_dom_node_id(&self) -> Option<i32> {
        match &self.backend_dom_node_id {
            Some(Value::Number(n)) => n.as_i64().and_then(|v| i32::try_from(v).ok()),
            _ => None,
        }
    }

    fn durable_selector(&self) -> Option<DurableSelector> {
        let selector = self.durable_selector.as_ref()?.clone();
        match self.durable_selector_kind.as_deref() {
            Some("testid") => Some(DurableSelector::TestId(selector)),
            Some("css") | None => Some(DurableSelector::Css(selector)),
            Some(_) => None,
        }
    }
}

/// One frame's AX tree plus how to reach it from Playwright.
pub struct FrameAx {
    /// `Accessibility.getFullAXTree` result for this frame (`{ nodes: [...] }`).
    pub ax: Value,
    /// URL of the frame (shown in the snapshot label for child frames).
    pub url: String,
    /// CSS selector for the owning `<iframe>` element; `None` for the main frame.
    pub owner_selector: Option<String>,
}

/// Build a snapshot from a single AX tree (main frame only). Thin wrapper over
/// [`snapshot_from_frames`] — used by the dev snapshot action and unit tests.
pub fn snapshot_from_ax_tree(cdp_result: Value, page_generation: u64) -> Result<PageSnapshot> {
    snapshot_from_frames(
        vec![FrameAx {
            ax: cdp_result,
            url: String::new(),
            owner_selector: None,
        }],
        page_generation,
    )
}

/// Build a snapshot spanning the main frame plus any child frames. Refs are
/// numbered continuously across frames; each ref carries the `frame_selector`
/// of its owning iframe (so codegen can scope it via `page.frameLocator(...)`),
/// and duplicate `(role, name)` indices are computed **per frame** (the scope a
/// `getByRole` runs in). The first frame is treated as the main frame.
pub fn snapshot_from_frames(frames: Vec<FrameAx>, page_generation: u64) -> Result<PageSnapshot> {
    let max_refs = max_refs();
    let mut registry = RefRegistry::new(page_generation);
    let mut lines: Vec<String> = Vec::new();
    let mut ref_count = 0usize;

    for (frame_index, frame) in frames.into_iter().enumerate() {
        let is_main = frame_index == 0;
        // Child frames get a labeled header; their content is indented under it.
        let depth_base = if is_main {
            0
        } else {
            let label = if frame.url.is_empty() {
                "- iframe".to_string()
            } else {
                format!("- iframe \"{}\"", escape_yaml_string(&frame.url))
            };
            lines.push(label);
            1
        };

        if ref_count >= max_refs {
            break;
        }
        append_frame(
            frame.ax,
            frame.owner_selector.as_deref(),
            depth_base,
            max_refs,
            &mut registry,
            &mut lines,
            &mut ref_count,
        );
    }

    let yaml = if lines.is_empty() {
        "- (empty accessibility tree)".to_string()
    } else {
        lines.join("\n")
    };

    Ok(PageSnapshot {
        yaml,
        ref_count,
        page_generation,
        registry,
    })
}

/// Walk one frame's AX tree, appending its rows to `lines` (indented by
/// `depth_base`) and its refs to `registry` (tagged with `frame_selector`).
/// Duplicate `(role, name)` indices are computed within this frame only.
fn append_frame(
    cdp_result: Value,
    frame_selector: Option<&str>,
    depth_base: usize,
    max_refs: usize,
    registry: &mut RefRegistry,
    lines: &mut Vec<String>,
    ref_count: &mut usize,
) {
    let tree: AxTreeResponse = if cdp_result.get("nodes").is_some() {
        serde_json::from_value(cdp_result).unwrap_or(AxTreeResponse { nodes: Vec::new() })
    } else {
        serde_json::from_value(Value::Object(
            [("nodes".into(), cdp_result)].into_iter().collect(),
        ))
        .unwrap_or(AxTreeResponse { nodes: Vec::new() })
    };
    if tree.nodes.is_empty() {
        return;
    }

    let mut by_id: HashMap<String, AxNode> = HashMap::new();
    for node in tree.nodes {
        by_id.insert(node.node_id.clone(), node);
    }
    let Ok(root_id) = find_root_id(&by_id) else {
        return;
    };

    struct PendingRef {
        ref_id: String,
        ax_node_id: String,
        backend_dom_node_id: Option<i32>,
        wk_dom_token: Option<String>,
        durable_selector: Option<DurableSelector>,
        role: String,
        name: String,
    }
    let mut pending: Vec<PendingRef> = Vec::new();

    let mut stack = vec![(root_id, depth_base)];
    while let Some((node_id, depth)) = stack.pop() {
        let Some(node) = by_id.get(&node_id) else {
            continue;
        };
        if node.ignored {
            for child in node.child_ids.iter().rev() {
                stack.push((child.clone(), depth));
            }
            continue;
        }

        let role = node.role_str();
        let name = node.name_str();
        let include = should_include_in_snapshot(&role, &name);

        if include && *ref_count < max_refs {
            let ref_id = registry.allocate_ref();
            pending.push(PendingRef {
                ref_id: ref_id.clone(),
                ax_node_id: node.node_id.clone(),
                backend_dom_node_id: node.backend_dom_node_id(),
                wk_dom_token: node.wk_dom_token.clone(),
                durable_selector: node.durable_selector(),
                role: role.clone(),
                name: name.clone(),
            });
            *ref_count += 1;

            let indent = "  ".repeat(depth);
            let mut line = format!("{indent}- {role}");
            if !name.is_empty() {
                line.push_str(&format!(" \"{}\"", escape_yaml_string(&name)));
            }
            if let Some(level) = node.property_value("level") {
                line.push_str(&format!(" [level={level}]"));
            }
            line.push_str(&format!(" [ref={ref_id}]"));
            lines.push(line);
        }

        let child_depth = if include { depth + 1 } else { depth };
        for child in node.child_ids.iter().rev() {
            stack.push((child.clone(), child_depth));
        }
    }

    // Per-(role,name) totals within this frame → which locators are ambiguous.
    let mut totals: HashMap<(String, String), usize> = HashMap::new();
    for p in &pending {
        *totals.entry((p.role.clone(), p.name.clone())).or_default() += 1;
    }
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for p in pending {
        let key = (p.role.clone(), p.name.clone());
        let dup_index = *seen.get(&key).unwrap_or(&0);
        *seen.entry(key.clone()).or_default() += 1;
        let dup_count = *totals.get(&key).unwrap_or(&1);
        let element_handle = p
            .backend_dom_node_id
            .map(ElementHandle::CdpBackendNodeId)
            .or_else(|| p.wk_dom_token.map(ElementHandle::WkDomToken));
        registry.insert(
            p.ref_id.clone(),
            ElementRef {
                ref_id: p.ref_id,
                ax_node_id: p.ax_node_id,
                backend_dom_node_id: p.backend_dom_node_id,
                element_handle,
                role: p.role,
                name: p.name,
                durable_selector: p.durable_selector,
                dup_index,
                dup_count,
                frame_selector: frame_selector.map(str::to_string),
            },
        );
    }
}

fn find_root_id(by_id: &HashMap<String, AxNode>) -> Result<String> {
    if let Some(root) = by_id.values().find(|n| n.role_str() == "RootWebArea") {
        return Ok(root.node_id.clone());
    }

    let mut referenced = HashSet::new();
    for node in by_id.values() {
        for child in &node.child_ids {
            referenced.insert(child.as_str());
        }
    }
    for id in by_id.keys() {
        if !referenced.contains(id.as_str()) {
            return Ok(id.clone());
        }
    }
    by_id
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| anyhow!("no root node in AX tree"))
}

fn should_include_in_snapshot(role: &str, name: &str) -> bool {
    if role == "RootWebArea" && name.is_empty() {
        return false;
    }
    if role == "generic" && name.is_empty() {
        return false;
    }
    if role == "none" || role == "InlineTextBox" {
        return false;
    }
    true
}

fn escape_yaml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree_json() -> Value {
        serde_json::json!({
            "nodes": [
                {
                    "nodeId": "1",
                    "role": { "value": "RootWebArea" },
                    "name": { "value": "Example Domain" },
                    "childIds": ["2", "3"]
                },
                {
                    "nodeId": "2",
                    "role": { "value": "heading" },
                    "name": { "value": "Example Domain" },
                    "properties": [{ "name": "level", "value": { "value": "1" } }],
                    "childIds": []
                },
                {
                    "nodeId": "3",
                    "role": { "value": "link" },
                    "name": { "value": "Learn more" },
                    "backendDOMNodeId": 42,
                    "childIds": []
                },
                {
                    "nodeId": "4",
                    "ignored": true,
                    "role": { "value": "StaticText" },
                    "name": { "value": "hidden" },
                    "childIds": []
                }
            ]
        })
    }

    #[test]
    fn snapshot_lists_roles_with_refs() {
        let snap = snapshot_from_ax_tree(sample_tree_json(), 1).unwrap();
        assert!(
            snap.yaml
                .contains("heading \"Example Domain\" [level=1] [ref=e")
        );
        assert!(snap.yaml.contains("link \"Learn more\" [ref=e"));
        assert!(!snap.yaml.contains("hidden"));
        assert_eq!(snap.ref_count, 3); // root + heading + link
    }

    #[test]
    fn registry_stores_backend_node_id() {
        let snap = snapshot_from_ax_tree(sample_tree_json(), 7).unwrap();
        let link = snap.registry.get("e3").expect("link ref");
        assert_eq!(link.role, "link");
        assert_eq!(link.backend_dom_node_id, Some(42));
        assert_eq!(snap.registry.page_generation(), 7);
    }

    #[test]
    fn snapshot_from_frames_assigns_frame_selector_to_child_refs() {
        let main = serde_json::json!({
            "nodes": [
                { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "" }, "childIds": [] }
            ]
        });
        let child = serde_json::json!({
            "nodes": [
                { "nodeId": "10", "role": { "value": "RootWebArea" }, "name": { "value": "" }, "childIds": ["11"] },
                { "nodeId": "11", "role": { "value": "button" }, "name": { "value": "Pay" }, "backendDOMNodeId": 42, "childIds": [] }
            ]
        });

        let snapshot = snapshot_from_frames(
            vec![
                FrameAx {
                    ax: main,
                    url: String::new(),
                    owner_selector: None,
                },
                FrameAx {
                    ax: child,
                    url: "https://pay.example/frame".into(),
                    owner_selector: Some("iframe#pay".into()),
                },
            ],
            7,
        )
        .expect("snapshot");

        assert!(
            snapshot
                .yaml
                .contains("- iframe \"https://pay.example/frame\"")
        );
        let pay = snapshot.registry.get("e1").expect("pay ref");
        assert_eq!(pay.role, "button");
        assert_eq!(pay.name, "Pay");
        assert_eq!(pay.frame_selector.as_deref(), Some("iframe#pay"));
    }

    #[test]
    fn snapshot_from_frames_computes_duplicate_indices_per_frame() {
        let frame = |root: &str, child: &str| {
            serde_json::json!({
                "nodes": [
                    { "nodeId": root, "role": { "value": "RootWebArea" }, "name": { "value": "" }, "childIds": [child] },
                    { "nodeId": child, "role": { "value": "button" }, "name": { "value": "Submit" }, "backendDOMNodeId": 11, "childIds": [] }
                ]
            })
        };

        let snapshot = snapshot_from_frames(
            vec![
                FrameAx {
                    ax: frame("1", "2"),
                    url: String::new(),
                    owner_selector: None,
                },
                FrameAx {
                    ax: frame("10", "11"),
                    url: "https://child.example".into(),
                    owner_selector: Some("iframe#child".into()),
                },
            ],
            8,
        )
        .expect("snapshot");

        let main = snapshot.registry.get("e1").expect("main submit");
        let child = snapshot.registry.get("e2").expect("child submit");
        assert_eq!((main.dup_index, main.dup_count), (0, 1));
        assert_eq!((child.dup_index, child.dup_count), (0, 1));
    }

    #[test]
    fn cdp_snapshot_populates_webview2_element_handle() {
        let tree = serde_json::json!({
            "nodes": [
                { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "" }, "childIds": ["2"] },
                { "nodeId": "2", "role": { "value": "button" }, "name": { "value": "Save" }, "backendDOMNodeId": 99, "childIds": [] }
            ]
        });

        let snapshot = snapshot_from_ax_tree(tree, 9).expect("snapshot");
        let save = snapshot.registry.get("e1").expect("save ref");
        assert_eq!(save.backend_dom_node_id, Some(99));
        assert_eq!(
            save.element_handle,
            Some(crate::automation::session::ElementHandle::CdpBackendNodeId(
                99
            ))
        );
    }

    #[test]
    fn wk_snapshot_populates_wk_dom_token_element_handle() {
        let tree = serde_json::json!({
            "nodes": [
                { "nodeId": "1", "role": { "value": "RootWebArea" }, "name": { "value": "" }, "childIds": ["2"] },
                {
                    "nodeId": "2",
                    "role": { "value": "button" },
                    "name": { "value": "Save" },
                    "wkDomToken": "zed-42",
                    "durableSelector": "save-button",
                    "durableSelectorKind": "testid",
                    "childIds": []
                }
            ]
        });

        let snapshot = snapshot_from_ax_tree(tree, 9).expect("snapshot");
        let save = snapshot.registry.get("e1").expect("save ref");
        assert_eq!(save.backend_dom_node_id, None);
        assert_eq!(
            save.element_handle,
            Some(crate::automation::session::ElementHandle::WkDomToken(
                "zed-42".into()
            ))
        );
        assert_eq!(
            save.durable_selector,
            Some(crate::automation::session::DurableSelector::TestId(
                "save-button".into()
            ))
        );
    }

    #[test]
    fn duplicate_role_name_gets_indices_and_count() {
        let tree = serde_json::json!({
            "nodes": [
                {
                    "nodeId": "1",
                    "role": { "value": "RootWebArea" },
                    "name": { "value": "Shop" },
                    "childIds": ["2", "3", "4"]
                },
                { "nodeId": "2", "role": { "value": "button" }, "name": { "value": "Add to cart" }, "childIds": [] },
                { "nodeId": "3", "role": { "value": "button" }, "name": { "value": "Add to cart" }, "childIds": [] },
                { "nodeId": "4", "role": { "value": "button" }, "name": { "value": "Checkout" }, "childIds": [] }
            ]
        });
        let snap = snapshot_from_ax_tree(tree, 1).unwrap();
        let first = snap.registry.get("e2").unwrap();
        let second = snap.registry.get("e3").unwrap();
        let unique = snap.registry.get("e4").unwrap();
        assert_eq!((first.dup_index, first.dup_count), (0, 2));
        assert_eq!((second.dup_index, second.dup_count), (1, 2));
        assert_eq!((unique.dup_index, unique.dup_count), (0, 1));
    }

    #[test]
    fn empty_nodes_yields_empty_snapshot() {
        // A frame with no nodes is skipped (not a hard error) so multi-frame
        // snapshots don't fail when one frame is empty.
        let snap = snapshot_from_ax_tree(serde_json::json!({ "nodes": [] }), 0).unwrap();
        assert_eq!(snap.ref_count, 0);
        assert!(snap.yaml.contains("empty accessibility tree"));
    }

    #[test]
    fn snapshot_accepts_boolean_property_values_and_numeric_node_ids() {
        let tree = serde_json::json!({
            "nodes": [
                {
                    "nodeId": 1,
                    "role": { "value": "RootWebArea" },
                    "name": { "value": "Example" },
                    "properties": [
                        { "name": "focusable", "value": { "value": true } },
                        { "name": "level", "value": { "value": 1 } }
                    ],
                    "childIds": [2]
                },
                {
                    "nodeId": "2",
                    "role": { "value": "link" },
                    "name": { "value": "More information" },
                    "childIds": []
                }
            ]
        });
        let snap = snapshot_from_ax_tree(tree, 1).unwrap();
        assert!(snap.yaml.contains("link \"More information\""));
        assert_eq!(snap.ref_count, 2);
    }
}
