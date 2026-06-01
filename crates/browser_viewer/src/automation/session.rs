//! Per-tab automation session state and ref registry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Handle assigned in snapshots (`e1`, `e2`, …) for Playwright-shaped targeting.
#[derive(Debug, Clone)]
pub struct ElementRef {
    pub ref_id: String,
    pub ax_node_id: String,
    pub backend_dom_node_id: Option<i32>,
    pub role: String,
    pub name: String,
    /// 0-based index of this element among snapshot elements that share the same
    /// `(role, name)` *within the same frame*, in DOM/AX order. With `dup_count`,
    /// lets codegen emit `.nth(i)` to disambiguate an otherwise-ambiguous locator.
    pub dup_index: usize,
    /// Number of snapshot elements sharing this `(role, name)` within the same
    /// frame. `> 1` means the locator is ambiguous and needs `.nth(dup_index)`.
    pub dup_count: usize,
    /// A CSS selector for the owning `<iframe>` when this element lives in a child
    /// frame (`None` = main frame). Codegen scopes the locator through
    /// `page.frameLocator(<frame_selector>)`. Actions don't need it — WebView2
    /// flattens frames, so `backendDOMNodeId` resolves cross-frame directly.
    pub frame_selector: Option<String>,
}

/// Ref map for the current page generation. Invalidated when navigation bumps
/// `AutomationSessionState::page_generation`.
#[derive(Debug, Clone, Default)]
pub struct RefRegistry {
    page_generation: u64,
    refs: HashMap<String, ElementRef>,
    next_index: u32,
}

impl RefRegistry {
    pub fn new(page_generation: u64) -> Self {
        Self {
            page_generation,
            refs: HashMap::new(),
            next_index: 1,
        }
    }

    pub fn page_generation(&self) -> u64 {
        self.page_generation
    }

    pub fn ref_count(&self) -> usize {
        self.refs.len()
    }

    pub fn allocate_ref(&mut self) -> String {
        let id = format!("e{}", self.next_index);
        self.next_index += 1;
        id
    }

    pub fn insert(&mut self, ref_id: String, element: ElementRef) {
        self.refs.insert(ref_id, element);
    }

    pub fn get(&self, ref_id: &str) -> Option<&ElementRef> {
        self.refs.get(ref_id)
    }
}

/// Monotonic generation counter bumped on navigation; holds the latest ref map.
#[derive(Debug, Default)]
pub struct AutomationSessionState {
    page_generation: AtomicU64,
    registry: Mutex<RefRegistry>,
}

impl AutomationSessionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn page_generation(&self) -> u64 {
        self.page_generation.load(Ordering::SeqCst)
    }

    pub fn bump_page_generation(&self) -> u64 {
        let generation = self.page_generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut registry) = self.registry.lock() {
            *registry = RefRegistry::new(generation);
        }
        generation
    }

    pub fn replace_registry(&self, registry: RefRegistry) {
        if let Ok(mut guard) = self.registry.lock() {
            *guard = registry;
        }
    }

    pub fn registry(&self) -> RefRegistry {
        self.registry
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| RefRegistry::new(self.page_generation()))
    }

    pub fn resolve_ref(&self, ref_id: &str) -> Option<ElementRef> {
        let registry = self.registry.lock().ok()?;
        if registry.page_generation() != self.page_generation() {
            return None;
        }
        registry.get(ref_id).cloned()
    }
}
