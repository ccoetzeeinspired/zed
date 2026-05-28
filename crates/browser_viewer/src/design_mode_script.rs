//! Design-mode JS, injected via `AddScriptToExecuteOnDocumentCreated`
//! so it runs on every navigation in the WebView.
//!
//! Protocol with the Rust host:
//!
//! - Host → page (via `PostWebMessageAsString`):
//!     - `"activate"` — turn on hover highlights + selection capture
//!     - `"deactivate"` — turn off; clear highlight; remove overlay
//!     - `"clear_selection"` — drop the current selection but stay armed
//!
//! - Page → host (via `chrome.webview.postMessage`, JSON):
//!     - `{ kind: "element_selected", selector, outerHTML, rect, source }`
//!         on click while armed
//!     - `{ kind: "page_scrolled", scrollX, scrollY }`
//!         on scroll while a selection is active, so the host can
//!         re-anchor the "Describe the change" floating input.
//!     - `{ kind: "ready" }` after install (idempotent — fires once
//!         even if the script re-evaluates)
//!
//! Source-hint detection cascade is per plans/browser-viewer.md §6.4:
//! React `_debugSource`, `data-source-{file,line}`, then
//! `data-component` / `data-testid`. Production React strips
//! `_debugSource`, so dev builds will show file paths, production
//! sites won't.

pub const SCRIPT: &str = r#"
(() => {
    const TAG = '[zed-design-mode]';
    if (window.__zedDesignMode) {
        try { window.chrome.webview.postMessage(JSON.stringify({ kind: 'ready' })); } catch (_) {}
        return;
    }
    if (!window.chrome || !window.chrome.webview) {
        console.warn(TAG, 'chrome.webview not available — design mode disabled');
        return;
    }
    window.__zedDesignMode = true;
    console.log(TAG, 'installed at', location.href);

    const OVERLAY_BORDER = '2px solid rgb(0, 255, 136)';
    let overlay = null;
    function ensureOverlay() {
        if (overlay && overlay.isConnected) return overlay;
        overlay = document.createElement('div');
        Object.assign(overlay.style, {
            position: 'fixed', pointerEvents: 'none', zIndex: '2147483647',
            border: OVERLAY_BORDER, boxSizing: 'border-box',
            transition: 'all 60ms ease-out', display: 'none',
            background: 'rgba(0, 255, 136, 0.08)',
        });
        (document.body || document.documentElement).appendChild(overlay);
        return overlay;
    }

    let armed = false;
    let hovered = null;
    let selected = null;

    function post(msg) {
        try { window.chrome.webview.postMessage(JSON.stringify(msg)); }
        catch (err) { console.warn(TAG, 'postMessage failed', err); }
    }

    function cssPath(el) {
        if (!(el instanceof Element)) return '';
        const path = [];
        while (el && el.nodeType === Node.ELEMENT_NODE && path.length < 8) {
            let sel = el.nodeName.toLowerCase();
            if (el.id) { sel += '#' + el.id; path.unshift(sel); break; }
            let sib = el, nth = 1;
            while ((sib = sib.previousElementSibling)) {
                if (sib.nodeName.toLowerCase() === sel) nth++;
            }
            if (nth !== 1) sel += `:nth-of-type(${nth})`;
            path.unshift(sel);
            el = el.parentElement;
        }
        return path.join(' > ');
    }

    function detectReactSource(el) {
        let node = el;
        for (let depth = 0; depth < 12 && node; depth++, node = node.parentElement) {
            const fiberKey = Object.keys(node).find(k => k.startsWith('__reactFiber$'));
            if (!fiberKey) continue;
            let fiber = node[fiberKey];
            for (let hops = 0; hops < 24 && fiber; hops++, fiber = fiber.return) {
                if (fiber._debugSource) {
                    const s = fiber._debugSource;
                    return { fileName: s.fileName, lineNumber: s.lineNumber, columnNumber: s.columnNumber };
                }
            }
        }
        const file = el.closest('[data-source-file]')?.getAttribute('data-source-file');
        const line = el.closest('[data-source-line]')?.getAttribute('data-source-line');
        if (file) return { fileName: file, lineNumber: line ? parseInt(line) : null, columnNumber: null };
        const comp = el.closest('[data-component]')?.getAttribute('data-component');
        const testid = el.closest('[data-testid]')?.getAttribute('data-testid');
        if (comp || testid) return { component: comp || null, testid: testid || null };
        return null;
    }

    function paintOverlay(rect) {
        const ov = ensureOverlay();
        Object.assign(ov.style, {
            display: 'block',
            left: rect.left + 'px', top: rect.top + 'px',
            width: rect.width + 'px', height: rect.height + 'px',
        });
    }

    function clearSelection() {
        selected = null;
        if (overlay) overlay.style.display = 'none';
    }

    // Capture-phase swallow. Every event the page could navigate on,
    // we intercept first. preventDefault stops the default action
    // (form submit, anchor navigation); stopImmediatePropagation stops
    // any other listener on the same target — including React's
    // synthetic event delegation root.
    function swallow(e) {
        e.preventDefault();
        e.stopPropagation();
        e.stopImmediatePropagation();
    }

    // Picking layer: while armed, intercept every "you clicked something"
    // event the page could react to. We can't just listen for `click`
    // because many sites navigate on `pointerdown` / `mousedown` /
    // `touchstart` before click ever fires.
    const PICK_EVENTS = [
        'pointerdown', 'pointerup',
        'mousedown', 'mouseup',
        'click', 'auxclick', 'dblclick',
        'touchstart', 'touchend',
        'contextmenu',
    ];

    for (const evt of PICK_EVENTS) {
        document.addEventListener(evt, e => {
            if (!armed) return;
            swallow(e);
            // Run the "pick this element" logic on the down phase only,
            // since some browsers don't deliver later events after we
            // suppress the down.
            if (evt === 'pointerdown' || evt === 'mousedown') {
                const el = e.target;
                if (!el || el === overlay) return;
                selected = el;
                const rect = el.getBoundingClientRect();
                paintOverlay(rect);
                post({
                    kind: 'element_selected',
                    selector: cssPath(el),
                    outerHTML: (el.outerHTML || '').slice(0, 8000),
                    rect: { x: rect.left, y: rect.top, w: rect.width, h: rect.height },
                    source: detectReactSource(el),
                    tag: el.tagName.toLowerCase(),
                    id: el.id || null,
                    classes: el.className && typeof el.className === 'string' ? el.className : null,
                });
            }
        }, true);
    }

    document.addEventListener('mousemove', e => {
        if (!armed || selected) return;
        const el = e.target;
        if (!el || el === overlay || el === hovered) return;
        hovered = el;
        paintOverlay(el.getBoundingClientRect());
    }, true);

    window.addEventListener('scroll', () => {
        if (!armed || !selected) return;
        const rect = selected.getBoundingClientRect();
        paintOverlay(rect);
        post({ kind: 'page_scrolled', scrollX: window.scrollX, scrollY: window.scrollY,
               rect: { x: rect.left, y: rect.top, w: rect.width, h: rect.height } });
    }, true);

    window.chrome.webview.addEventListener('message', evt => {
        const msg = evt.data;
        console.log(TAG, 'host message:', msg);
        if (msg === 'activate') {
            armed = true;
            ensureOverlay();
        } else if (msg === 'deactivate') {
            armed = false; hovered = null; clearSelection();
        } else if (msg === 'clear_selection') {
            clearSelection();
        }
    });

    post({ kind: 'ready' });
})();
"#;
