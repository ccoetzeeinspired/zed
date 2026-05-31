//! CP6: `browser_tabs` — list / select / open / close embedded browser tabs.
//!
//! Unlike the other automation tools (which drive a single tab's page over
//! CDP), this one manipulates the Zed **workspace**: it enumerates `BrowserView`
//! items across the workspace's panes, and activates / opens / closes them.
//!
//! Tab order is the enumeration order of `Workspace::items_of_type`
//! (pane-by-pane, item order within a pane) — for the common single-pane case
//! that's left-to-right visual tab order. `active` marks the focused browser
//! tab in the active pane.

use anyhow::{Result, anyhow};
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, SharedString};
use serde_json::{Value, json};
use workspace::{SaveIntent, Workspace};

use crate::browser_view::{BrowserView, open_new_tab};

/// One row of the tab list (sans the live entity handle).
#[derive(Debug, Clone, PartialEq)]
struct TabRow {
    title: String,
    url: String,
    active: bool,
}

/// Snapshot every browser tab in the workspace, paired with its entity handle
/// (handle needed for select/close; the row for the JSON response).
fn collect_tabs(workspace: &Entity<Workspace>, cx: &App) -> Vec<(Entity<BrowserView>, TabRow)> {
    workspace.read_with(cx, |ws, cx| {
        let active_id = ws.active_item_as::<BrowserView>(cx).map(|v| v.entity_id());
        ws.items_of_type::<BrowserView>(cx)
            .map(|view| {
                let id = view.entity_id();
                let bview = view.read(cx);
                let item = bview.item().read(cx);
                let row = TabRow {
                    title: item.title().to_string(),
                    url: item.url().to_string(),
                    active: Some(id) == active_id,
                };
                (view.clone(), row)
            })
            .collect()
    })
}

/// Build the `{ tabs: [{index,title,url,active}], count }` response value.
fn build_json(rows: &[TabRow]) -> Value {
    let tabs: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            json!({
                "index": index,
                "title": row.title,
                "url": row.url,
                "active": row.active,
            })
        })
        .collect();
    json!({ "tabs": tabs, "count": tabs.len() })
}

fn snapshot_json(workspace: &Entity<Workspace>, cx: &mut AsyncApp) -> Result<Value> {
    let rows: Vec<TabRow> = cx
        .update(|app| collect_tabs(workspace, app))
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    Ok(build_json(&rows))
}

/// `action: "list"` — enumerate browser tabs (read-only).
pub fn list(workspace: &Entity<Workspace>, cx: &mut AsyncApp) -> Result<Value> {
    snapshot_json(workspace, cx)
}

/// `action: "select"` — activate (focus) the browser tab at `index`. The
/// active tab's WebView2 underlay is reordered to the front by the normal
/// render path, so only the selected page shows through its cutout.
pub async fn select(
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    index: usize,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let tabs = cx.update(|app| collect_tabs(&workspace, app));
    let (view, _) = tabs
        .get(index)
        .ok_or_else(|| anyhow!("tab index {index} out of range ({} tab(s) open)", tabs.len()))?;
    let view = view.clone();

    let activated = cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            // activate_pane = true (also focus the pane), focus_item = true.
            ws.activate_item(&view, true, true, window, cx)
        })
    })?;
    if !activated {
        return Err(anyhow!("failed to activate tab {index} (not found in any pane)"));
    }

    snapshot_json(&workspace, cx)
}

/// `action: "new"` — open a new browser tab at `url` (defaults to the
/// configured homepage) and activate it.
pub async fn new_tab(
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    url: SharedString,
    cx: &mut AsyncApp,
) -> Result<Value> {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            open_new_tab(ws, url.clone(), window, cx);
        })
    })?;

    snapshot_json(&workspace, cx)
}

/// `action: "close"` — close the browser tab at `index`.
pub async fn close(
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    index: usize,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let tabs = cx.update(|app| collect_tabs(&workspace, app));
    let (view, _) = tabs
        .get(index)
        .ok_or_else(|| anyhow!("tab index {index} out of range ({} tab(s) open)", tabs.len()))?;
    let view = view.clone();
    let item_id = view.entity_id();

    let task = cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            let pane = ws
                .pane_for(&view)
                .ok_or_else(|| anyhow!("tab {index} is not in any pane (already closed?)"))?;
            // Browser tabs are never dirty; skip the save prompt.
            Ok::<_, anyhow::Error>(pane.update(cx, |pane, cx| {
                pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
            }))
        })
    })??;
    task.await?;

    snapshot_json(&workspace, cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, url: &str, active: bool) -> TabRow {
        TabRow {
            title: title.into(),
            url: url.into(),
            active,
        }
    }

    #[test]
    fn build_json_indexes_counts_and_marks_active() {
        let rows = vec![
            row("Google", "https://google.com/", false),
            row("Takealot", "https://takealot.com/", true),
        ];
        let v = build_json(&rows);
        assert_eq!(v["count"], 2);
        assert_eq!(v["tabs"][0]["index"], 0);
        assert_eq!(v["tabs"][0]["title"], "Google");
        assert_eq!(v["tabs"][0]["active"], false);
        assert_eq!(v["tabs"][1]["index"], 1);
        assert_eq!(v["tabs"][1]["url"], "https://takealot.com/");
        assert_eq!(v["tabs"][1]["active"], true);
    }

    #[test]
    fn build_json_empty_is_zero_count() {
        let v = build_json(&[]);
        assert_eq!(v["count"], 0);
        assert_eq!(v["tabs"].as_array().unwrap().len(), 0);
    }
}
