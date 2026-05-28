//! Design-mode submission bundle: everything captured when the user
//! clicks "Submit" in the floating panel. As of Phase 4.F submit
//! dispatches an ACP prompt into the claude-acp agent panel (see
//! `BrowserView::on_design_submit`). `write_bundle` is retained as a
//! debug-only fallback that persists the bundle to disk under
//! `%TEMP%/zed-browser-design/<timestamp>/` when the
//! `ZED_BROWSER_DESIGN_DEBUG_BUNDLE` environment variable is set.

use anyhow::{Context as _, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::design::ElementSource;

#[derive(Debug, Clone)]
pub struct DesignBundle {
    pub prompt: String,
    pub selector: String,
    pub outer_html: String,
    pub source: Option<ElementSource>,
    pub drawing_svg: String,
    pub page_url: String,
    pub screenshot_png: Vec<u8>,
}

/// JSON metadata that lands next to the screenshot + drawing files.
/// Stable shape so future ACP wiring can read it without re-parsing
/// raw bundle internals.
#[derive(Debug, Clone, Serialize)]
struct BundleMetadata<'a> {
    schema_version: u32,
    prompt: &'a str,
    selector: &'a str,
    page_url: &'a str,
    outer_html: &'a str,
    source: Option<MetadataSource<'a>>,
    files: BundleFiles<'a>,
}

#[derive(Debug, Clone, Serialize)]
struct MetadataSource<'a> {
    file_name: Option<&'a str>,
    line_number: Option<u32>,
    column_number: Option<u32>,
    component: Option<&'a str>,
    testid: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
struct BundleFiles<'a> {
    screenshot: &'a str,
    drawing: &'a str,
}

/// Persist `bundle` to a fresh per-submission directory under
/// `%TEMP%/zed-browser-design/<unix-ms>/` and return the directory.
pub fn write_bundle(bundle: &DesignBundle) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir()
        .join("zed-browser-design")
        .join(format!("{ts}"));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create bundle dir {}", dir.display()))?;
    let screenshot_path = dir.join("screenshot.png");
    let drawing_path = dir.join("drawing.svg");
    let metadata_path = dir.join("bundle.json");

    std::fs::write(&screenshot_path, &bundle.screenshot_png)
        .with_context(|| format!("write {}", screenshot_path.display()))?;
    std::fs::write(&drawing_path, bundle.drawing_svg.as_bytes())
        .with_context(|| format!("write {}", drawing_path.display()))?;

    let metadata_src = bundle.source.as_ref().map(|s| MetadataSource {
        file_name: s.file_name.as_deref(),
        line_number: s.line_number,
        column_number: s.column_number,
        component: s.component.as_deref(),
        testid: s.testid.as_deref(),
    });
    let metadata = BundleMetadata {
        schema_version: 1,
        prompt: &bundle.prompt,
        selector: &bundle.selector,
        page_url: &bundle.page_url,
        outer_html: &bundle.outer_html,
        source: metadata_src,
        files: BundleFiles {
            screenshot: "screenshot.png",
            drawing: "drawing.svg",
        },
    };
    let metadata_json = serde_json::to_vec_pretty(&metadata)?;
    std::fs::write(&metadata_path, metadata_json)
        .with_context(|| format!("write {}", metadata_path.display()))?;

    Ok(dir)
}
