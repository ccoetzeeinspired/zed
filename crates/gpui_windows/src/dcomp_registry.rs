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
    /// Parent visual this child was inserted under. Stored so `Drop`
    /// can call `RemoveVisual` on the same parent — overlays attach
    /// to `comp_visual`, underlays attach to `comp_container`.
    parent: IDCompositionVisual,
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
        // Remove the visual from its parent's child list and commit.
        // Failures are logged but not propagated — drop must not panic.
        unsafe {
            if let Err(err) = self.parent.RemoveVisual(&self.visual) {
                log::warn!("HostedVisual.drop: RemoveVisual failed: {err}");
            }
            if let Err(err) = self.dcomp.comp_device.Commit() {
                log::warn!("HostedVisual.drop: Commit after RemoveVisual failed: {err}");
            }
        }
    }
}

/// Create a new child visual attached *above* GPUI's swap chain — the
/// "overlay" position. Anything painted here covers GPUI's UI. Use the
/// underlay variant ([`create_underlay_visual_for_hwnd`]) for hosted
/// content like WebView2 that should sit *under* GPUI overlays.
///
/// Fails if the window's DComp tree is not registered (DComp disabled, or
/// the renderer was dropped).
pub fn create_child_visual_for_hwnd(hwnd: HWND) -> Result<HostedVisual> {
    let dcomp = lookup(hwnd)
        .with_context(|| format!("no DirectComposition registered for HWND {:?}", hwnd.0))?;
    let parent = dcomp.comp_visual.clone();
    let visual = unsafe {
        dcomp
            .comp_device
            .CreateVisual()
            .context("IDCompositionDevice::CreateVisual")?
    };
    unsafe {
        parent
            .AddVisual(&visual, true, None::<&IDCompositionVisual>)
            .context("AddVisual")?;
    }
    Ok(HostedVisual {
        dcomp,
        visual,
        parent,
    })
}

/// Create a new child visual attached as an *underlay* — sits below
/// GPUI's swap chain in the composition tree. GPUI's UI paints on top;
/// pixels GPUI leaves transparent (alpha = 0) let this visual show
/// through. Used by `browser_viewer` so the WebView2 page renders
/// under the address bar / floating panels / drawing strokes.
///
/// Requires the host window's swap chain to be alpha-premultiplied —
/// which GPUI's composition swap chain already is (see
/// `directx_renderer.rs` — `DXGI_ALPHA_MODE_PREMULTIPLIED`).
pub fn create_underlay_visual_for_hwnd(hwnd: HWND) -> Result<HostedVisual> {
    let dcomp = lookup(hwnd)
        .with_context(|| format!("no DirectComposition registered for HWND {:?}", hwnd.0))?;
    let parent = dcomp.comp_container.clone();
    let visual = unsafe {
        dcomp
            .comp_device
            .CreateVisual()
            .context("IDCompositionDevice::CreateVisual")?
    };
    unsafe {
        // `insertAbove = TRUE` with referenceVisual = NULL → BEGINNING
        // of the child list, which renders FIRST (= back of z-order).
        // `comp_visual` (the GPUI swap-chain holder) is added with
        // `insertAbove = FALSE` in `set_swap_chain` → END of list → front.
        // So the underlay ends up beneath GPUI's UI in z-order: page
        // shows where GPUI is transparent, GPUI overlays show on top.
        parent
            .AddVisual(&visual, true, None::<&IDCompositionVisual>)
            .context("AddVisual (underlay)")?;
    }
    Ok(HostedVisual {
        dcomp,
        visual,
        parent,
    })
}
