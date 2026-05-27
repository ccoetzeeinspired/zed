//! In-editor PDF viewer.
//!
//! Registers a [`PdfView`] project item that claims `*.pdf` paths. When a PDF is
//! opened, its pages are rasterized to PNGs (via poppler's `pdftoppm`) into a
//! per-file temp cache, and rendered as a vertically scrolling column of images.
//!
//! This is the v1 ("preview now, then fork") native viewer: it reuses the
//! proven poppler rasterizer and gpui's path-based `img()` so there is no heavy
//! native PDF dependency to link. A later pass can swap the rasterizer for an
//! in-process `pdfium` renderer with lazy per-page rendering.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, SharedString, Styled, Task, Window, actions, div, img,
};
use project::{Project, ProjectEntryId, ProjectPath};
use ui::prelude::*;
use workspace::{
    Pane,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem},
};

const RENDER_DPI: &str = "200";
const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 6.0;
const ZOOM_STEP: f32 = 1.1;

actions!(
    pdf_viewer,
    [
        /// Zoom in.
        ZoomIn,
        /// Zoom out.
        ZoomOut,
        /// Reset zoom to fit width (100%).
        ResetZoom,
    ]
);

/// Project-level handle for an opened PDF: where it lives and its rendered pages.
pub struct PdfItem {
    abs_path: PathBuf,
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
    pages: Vec<PathBuf>,
}

impl project::ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        // Check the extension on the absolute path: when a PDF is opened
        // standalone it becomes its own worktree root, so the worktree-relative
        // path is empty and has no extension.
        let abs_path = project.read(cx).absolute_path(path, cx)?;
        let is_pdf = abs_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"));
        if !is_pdf {
            return None;
        }
        log::info!("pdf_viewer: opening {abs_path:?}");

        let entry_id = project.read(cx).entry_for_path(path, cx).map(|entry| entry.id);
        let project_path = path.clone();

        Some(cx.spawn(async move |cx| {
            let render_path = abs_path.clone();
            let pages = cx
                .background_executor()
                .spawn(async move { render_pdf_to_pngs(&render_path) })
                .await?;
            let item = cx.update(|cx| {
                cx.new(|_| PdfItem {
                    abs_path,
                    project_path,
                    entry_id,
                    pages,
                })
            });
            Ok(item)
        }))
    }

    fn entry_id(&self, _cx: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _cx: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

/// The editor pane that displays a [`PdfItem`]'s rendered pages.
pub struct PdfView {
    item: Entity<PdfItem>,
    focus_handle: FocusHandle,
    /// Page width as a fraction of the viewport: 1.0 == fit-to-width.
    zoom: f32,
}

impl PdfView {
    pub fn new(item: Entity<PdfItem>, cx: &mut Context<Self>) -> Self {
        Self {
            item,
            focus_handle: cx.focus_handle(),
            zoom: 1.0,
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = (self.zoom * ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM);
        cx.notify();
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = (self.zoom / ZOOM_STEP).clamp(MIN_ZOOM, MAX_ZOOM);
        cx.notify();
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom = 1.0;
        cx.notify();
    }
}

impl EventEmitter<()> for PdfView {}

impl Focusable for PdfView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = self.item.read(cx).pages.clone();
        let zoom = self.zoom;

        div()
            .track_focus(&self.focus_handle)
            .key_context("PdfView")
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                div()
                    .id("pdf-scroll")
                    .size_full()
                    .overflow_scroll()
                    .child(
                        v_flex()
                            .items_center()
                            .gap_4()
                            .p_4()
                            .children(pages.into_iter().enumerate().map(|(ix, page)| {
                                // max_w(relative(zoom)): 1.0 fits width; >1.0 overflows
                                // (horizontal scroll); aspect ratio is preserved.
                                img(page)
                                    .id(("pdf-page", ix))
                                    .max_w(relative(zoom))
                                    .shadow_md()
                            })),
                    ),
            )
    }
}

impl Item for PdfView {
    type Event = ();

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.item.entity_id(), self.item.read(cx))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.item
            .read(cx)
            .abs_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF".to_owned())
            .into()
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        let path = self.item.read(cx).abs_path.clone();
        file_icons::FileIcons::get_icon(&path, cx).map(Icon::from_path)
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }
}

impl ProjectItem for PdfView {
    type Item = PdfItem;

    fn for_project_item(
        _project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, cx)
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
}

// --- rasterization -------------------------------------------------------

/// Rasterize every page of `pdf` to a PNG, returning the page image paths in
/// order. Results are cached per (path, size, mtime) so re-opening is instant.
fn render_pdf_to_pngs(pdf: &Path) -> Result<Vec<PathBuf>> {
    let out_dir = cache_dir_for(pdf)?;
    std::fs::create_dir_all(&out_dir)?;

    let cached = collect_pages(&out_dir);
    if !cached.is_empty() {
        return Ok(cached);
    }

    let pdftoppm = find_pdftoppm().context(
        "couldn't find poppler's `pdftoppm`; install poppler or add its bin dir to PATH",
    )?;
    let status = std::process::Command::new(&pdftoppm)
        .args(["-png", "-r", RENDER_DPI])
        .arg(pdf)
        .arg(out_dir.join("page"))
        .status()
        .context("failed to run pdftoppm")?;
    anyhow::ensure!(status.success(), "pdftoppm exited with {status}");

    let pages = collect_pages(&out_dir);
    anyhow::ensure!(!pages.is_empty(), "pdftoppm produced no pages");
    Ok(pages)
}

/// Locate `pdftoppm.exe` — the WinGet poppler install first, then PATH.
fn find_pdftoppm() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "pdftoppm.exe" } else { "pdftoppm" };

    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let packages = Path::new(&local).join("Microsoft\\WinGet\\Packages");
        if let Ok(entries) = std::fs::read_dir(&packages) {
            for entry in entries.flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("oschwartz10612.Poppler")
                {
                    continue;
                }
                if let Ok(versions) = std::fs::read_dir(entry.path()) {
                    for version in versions.flatten() {
                        let candidate = version.path().join("Library\\bin").join(exe);
                        if candidate.is_file() {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
    }

    // Fall back to PATH resolution.
    Some(PathBuf::from(exe))
}

fn cache_dir_for(pdf: &Path) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};

    let meta = std::fs::metadata(pdf).context("reading PDF metadata")?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pdf.hash(&mut hasher);
    meta.len().hash(&mut hasher);
    mtime.hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());

    Ok(std::env::temp_dir().join("zed-pdf-viewer").join(key))
}

fn collect_pages(dir: &Path) -> Vec<PathBuf> {
    let mut pages: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("png")))
            .collect(),
        Err(_) => Vec::new(),
    };
    pages.sort_by_key(|p| page_number(p));
    pages
}

/// `pdftoppm` names pages like `page-1.png`, `page-12.png`; sort by that number.
fn page_number(path: &Path) -> u32 {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit('-').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}
