//! Resolve automation targets across all open workspaces.

use gpui::{AnyWindowHandle, App, Entity};
use workspace::{AppState, Workspace};

use crate::browser_view::BrowserView;

/// Locate a browser tab to drive from any open workspace.
///
/// Prefers the active item when it is a [`BrowserView`], otherwise the most
/// recently activated browser tab (same rules as per-workspace resolution).
pub fn resolve_automation_target(workspace: &Workspace, cx: &App) -> Option<Entity<BrowserView>> {
    if let Some(item) = workspace.active_item(cx)
        && let Some(browser) = item.act_as::<BrowserView>(cx)
    {
        return Some(browser);
    }
    workspace.recent_active_item_by_type::<BrowserView>(cx)
}

/// Find a browser tab across all open workspaces (front-most window first).
pub fn resolve_automation_target_global(cx: &App) -> Option<Entity<BrowserView>> {
    let app_state = AppState::global(cx);
    let store = app_state.workspace_store.read(cx);
    let window_handles = cx.window_stack().unwrap_or_else(|| cx.windows());

    for window in window_handles {
        for (ws_window, weak) in store.workspaces_with_windows() {
            if ws_window != window {
                continue;
            }
            let Some(workspace) = weak.upgrade() else {
                continue;
            };
            if let Some(browser) =
                workspace.read_with(cx, |ws, cx| resolve_automation_target(ws, cx))
            {
                return Some(browser);
            }
        }
    }

    for weak in store.workspaces() {
        let Some(workspace) = weak.upgrade() else {
            continue;
        };
        if let Some(browser) = workspace.read_with(cx, |ws, cx| resolve_automation_target(ws, cx)) {
            return Some(browser);
        }
    }

    None
}

/// Resolve the workspace (and its window) for `browser_tabs` operations.
///
/// Prefers the front-most workspace that already holds a browser tab (so tab
/// operations act where the other tools act); falls back to the front-most
/// workspace window otherwise (so `new` works even with zero browser tabs open).
pub fn resolve_automation_workspace_global(
    cx: &App,
) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    let app_state = AppState::global(cx);
    let store = app_state.workspace_store.read(cx);
    let window_handles = cx.window_stack().unwrap_or_else(|| cx.windows());

    // Pass 1: front-most window whose workspace already has a browser tab.
    for window in &window_handles {
        for (ws_window, weak) in store.workspaces_with_windows() {
            if ws_window != *window {
                continue;
            }
            let Some(workspace) = weak.upgrade() else {
                continue;
            };
            let has_browser =
                workspace.read_with(cx, |ws, cx| resolve_automation_target(ws, cx).is_some());
            if has_browser {
                return Some((*window, workspace));
            }
        }
    }

    // Pass 2: front-most window that has any workspace.
    for window in &window_handles {
        for (ws_window, weak) in store.workspaces_with_windows() {
            if ws_window != *window {
                continue;
            }
            if let Some(workspace) = weak.upgrade() {
                return Some((*window, workspace));
            }
        }
    }

    // Pass 3: any workspace, even without window-stack ordering.
    for (ws_window, weak) in store.workspaces_with_windows() {
        if let Some(workspace) = weak.upgrade() {
            return Some((ws_window, workspace));
        }
    }

    None
}
