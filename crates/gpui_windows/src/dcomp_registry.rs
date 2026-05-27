//! FORK: HWND → DirectComposition registry, lets external code (the
//! `browser_viewer` crate) attach child visuals to a GPUI window's
//! composition tree.
//!
//! Each `DirectXRenderer` that successfully constructs a `DirectComposition`
//! wraps it in an `Arc` and calls [`register`] to publish a `Weak<>` reference
//! keyed by HWND. External code looks up by HWND, upgrades to a strong
//! reference, attaches a visual to the root, and commits.
//!
//! The registry never holds a strong reference. When the renderer's `Arc`
//! drops (window close, device-lost recovery), the `Weak` lookup just fails
//! cleanly; any stale entries are reused on the next `register` for the
//! same HWND.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Weak},
};

use anyhow::{Context as _, Result};
use windows::Win32::{Foundation::HWND, Graphics::DirectComposition::IDCompositionVisual};

use crate::directx_renderer::DirectComposition;

thread_local! {
    // COM objects in `DirectComposition` are STA-bound to the GPUI UI thread.
    // All registrations and lookups happen on that thread, so a thread-local
    // is both correct and avoids needing `unsafe impl Send/Sync`.
    static REGISTRY: RefCell<HashMap<isize, Weak<DirectComposition>>> =
        RefCell::new(HashMap::new());
}

pub(crate) fn register(hwnd: HWND, dcomp: &Arc<DirectComposition>) {
    REGISTRY.with(|r| {
        r.borrow_mut()
            .insert(hwnd.0 as isize, Arc::downgrade(dcomp));
    });
}

fn lookup(hwnd: HWND) -> Option<Arc<DirectComposition>> {
    REGISTRY.with(|r| r.borrow().get(&(hwnd.0 as isize))?.upgrade())
}

/// Handle returned by [`create_child_visual_for_hwnd`]. Holds a strong
/// reference to the window's composition so the visual stays attached for
/// as long as the handle is alive. Drop the handle to remove the visual.
pub struct HostedVisual {
    dcomp: Arc<DirectComposition>,
    visual: IDCompositionVisual,
}

impl HostedVisual {
    /// The underlying IDCompositionVisual. WebView2's
    /// `ICoreWebView2CompositionController::SetRootVisualTarget` accepts this
    /// directly (it QueryInterfaces for IDCompositionVisual2/3 internally).
    pub fn visual(&self) -> &IDCompositionVisual {
        &self.visual
    }

    /// Commit pending changes to the visual's position/transform. Call after
    /// any `SetOffsetX/Y` or `SetTransform` operations on the visual.
    pub fn commit(&self) -> Result<()> {
        unsafe {
            self.dcomp
                .comp_device
                .Commit()
                .context("IDCompositionDevice::Commit")?;
        }
        Ok(())
    }
}

impl Drop for HostedVisual {
    fn drop(&mut self) {
        // Remove the visual from the root visual's child list and commit.
        // Failures are logged but not propagated — drop must not panic.
        unsafe {
            if let Err(err) = self.dcomp.comp_visual.RemoveVisual(&self.visual) {
                log::warn!("HostedVisual.drop: RemoveVisual failed: {err}");
            }
            if let Err(err) = self.dcomp.comp_device.Commit() {
                log::warn!("HostedVisual.drop: Commit after RemoveVisual failed: {err}");
            }
        }
    }
}

/// Create a new child visual attached to the root visual of the GPUI window
/// identified by `hwnd`. The visual starts at offset (0, 0) and renders no
/// content until the caller sets content (e.g., via WebView2's
/// `SetRootVisualTarget`) and commits.
///
/// Fails if the window's DComp tree is not registered (DComp disabled, or
/// the renderer was dropped).
pub fn create_child_visual_for_hwnd(hwnd: HWND) -> Result<HostedVisual> {
    let dcomp = lookup(hwnd)
        .with_context(|| format!("no DirectComposition registered for HWND {:?}", hwnd.0))?;
    let visual = unsafe {
        dcomp
            .comp_device
            .CreateVisual()
            .context("IDCompositionDevice::CreateVisual")?
    };
    unsafe {
        dcomp
            .comp_visual
            .AddVisual(&visual, true, None::<&IDCompositionVisual>)
            .context("AddVisual")?;
        // Do NOT commit here; the caller will configure transform and content
        // first, then call `HostedVisual::commit()` for an atomic appearance.
    }
    Ok(HostedVisual { dcomp, visual })
}
