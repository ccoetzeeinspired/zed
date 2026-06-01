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
    let agentPageRevision = 1;
    let agentSnapshotCounter = 1;
    let agentRefCounter = 1;

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

    // The DOM node carries a `__reactFiber$…` key pointing at its fiber.
    function reactFiber(el) {
        let node = el;
        for (let depth = 0; depth < 12 && node; depth++, node = node.parentElement) {
            const key = Object.keys(node).find(k => k.startsWith('__reactFiber$'));
            if (key) return node[key];
        }
        return null;
    }

    // Resolve a fiber `type` to a component display name, unwrapping
    // forwardRef / memo. Returns null for host components (div, span…).
    function componentName(type, depth) {
        depth = depth || 0;
        if (depth > 5 || !type || typeof type === 'string') return null;
        if (typeof type === 'function') return type.displayName || type.name || null;
        if (typeof type === 'object') {
            if (type.displayName) return type.displayName;
            if (type.render) return type.render.displayName || type.render.name || null; // forwardRef
            if (type.type) return componentName(type.type, depth + 1); // memo
        }
        return null;
    }

    // Generic wrapper names that aren't useful to surface.
    const SKIP_NAMES = /^(ForwardRef|Memo|Fragment|Suspense|StrictMode|Provider|Consumer|Profiler|Anonymous)$/;

    // Best-effort source identity for the clicked element:
    //  - `_debugSource` file:line on React < 19 (React 19 removed it);
    //  - the nearest React component name (survives in dev on all
    //    versions — the React-19 path);
    //  - `data-source-*` / `data-component` / `data-testid` attributes.
    function detectReactSource(el) {
      try {
        let component = null;
        for (let fiber = reactFiber(el), hops = 0; fiber && hops < 30; hops++, fiber = fiber.return) {
            if (fiber._debugSource) {
                const s = fiber._debugSource;
                return {
                    fileName: s.fileName,
                    lineNumber: s.lineNumber,
                    columnNumber: s.columnNumber,
                    component: component || componentName(fiber.type),
                };
            }
            if (!component) {
                const name = componentName(fiber.type);
                if (name && !SKIP_NAMES.test(name)) component = name;
            }
        }
        const file = el.closest('[data-source-file]')?.getAttribute('data-source-file');
        const line = el.closest('[data-source-line]')?.getAttribute('data-source-line');
        if (file) {
            return { fileName: file, lineNumber: line ? parseInt(line) : null, columnNumber: null, component };
        }
        const dataComp = el.closest('[data-component]')?.getAttribute('data-component');
        const testid = el.closest('[data-testid]')?.getAttribute('data-testid');
        if (component || dataComp || testid) {
            return { component: component || dataComp || null, testid: testid || null };
        }
        return null;
      } catch (err) {
        // Never let source detection break element selection / picking.
        return null;
      }
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

    function visible(el) {
        if (!(el instanceof Element)) return false;
        const rect = el.getBoundingClientRect();
        const style = getComputedStyle(el);
        return rect.width > 0 && rect.height > 0 &&
            style.visibility !== 'hidden' && style.display !== 'none';
    }

    function norm(s) {
        return (s || '').replace(/\s+/g, ' ').trim();
    }

    function elementText(el) {
        return norm(el.innerText || el.textContent || el.getAttribute('aria-label') || '');
    }

    function roleOf(el) {
        const explicit = el.getAttribute('role');
        if (explicit) return explicit;
        const tag = el.tagName.toLowerCase();
        if (tag === 'button') return 'button';
        if (tag === 'a' && el.hasAttribute('href')) return 'link';
        if (tag === 'input') {
            const type = (el.getAttribute('type') || 'text').toLowerCase();
            if (type === 'submit' || type === 'button') return 'button';
            return 'textbox';
        }
        return null;
    }

    function accessibleName(el) {
        return norm(el.getAttribute('aria-label') || el.value || elementText(el));
    }

    function serializeAgentTarget(el, confidence) {
        const rect = el.getBoundingClientRect();
        return {
            selector: cssPath(el),
            tag: el.tagName.toLowerCase(),
            text: elementText(el).slice(0, 500),
            role: roleOf(el),
            accessibleName: accessibleName(el).slice(0, 500),
            rect: { x: rect.left, y: rect.top, w: rect.width, h: rect.height },
            source: detectReactSource(el),
            confidence,
        };
    }

    function allVisibleElements() {
        return Array.from(document.querySelectorAll('body *')).filter(visible);
    }

    function bumpAgentPageRevision() {
        agentPageRevision += 1;
    }

    function installAgentPageRevisionObserver() {
        try {
            const root = document.documentElement;
            if (!root) {
                document.addEventListener('DOMContentLoaded', installAgentPageRevisionObserver, { once: true });
                return;
            }
            new MutationObserver(bumpAgentPageRevision).observe(root, {
                childList: true,
                subtree: true,
                attributes: true,
                characterData: true,
            });
        } catch (err) {
            console.warn(TAG, 'page revision observer unavailable', err);
        }
    }

    function roleOfForSnapshot(el) {
        const explicit = roleOf(el);
        if (explicit) return explicit;
        const tag = el.tagName.toUpperCase();
        const mapped = {
            H1: 'heading',
            H2: 'heading',
            H3: 'heading',
            H4: 'heading',
            H5: 'heading',
            H6: 'heading',
            UL: 'list',
            OL: 'list',
            LI: 'listitem',
            MAIN: 'main',
            NAV: 'navigation',
            FORM: 'form',
            SECTION: el.getAttribute('aria-label') ? 'region' : null,
            ARTICLE: 'article',
        }[tag];
        return mapped || null;
    }

    function isSnapshotVisible(el) {
        if (!visible(el)) return false;
        return getComputedStyle(el).opacity !== '0';
    }

    function isInteractiveSnapshotElement(el) {
        return el.matches('button, a[href], input, textarea, select, [role], [tabindex], summary, label');
    }

    function snapshotNodeForElement(el, depth) {
        if (!isSnapshotVisible(el) || depth > 6) return null;
        const role = roleOfForSnapshot(el);
        const text = elementText(el).slice(0, 220);
        const interactive = isInteractiveSnapshotElement(el);
        const children = [];
        for (const child of Array.from(el.children || [])) {
            const childNode = snapshotNodeForElement(child, depth + 1);
            if (childNode) children.push(childNode);
        }
        if (!role && !interactive && !text && children.length === 0) return null;
        if (!role && !interactive && children.length === 1 && !text) return children[0];
        const rect = el.getBoundingClientRect();
        const node = {
            role: role || (text ? 'text' : null),
            name: interactive ? accessibleName(el).slice(0, 220) : null,
            text: interactive ? null : text,
            selector: interactive ? cssPath(el) : null,
            rect: interactive ? { x: rect.left, y: rect.top, w: rect.width, h: rect.height } : null,
            children,
            disabled: Boolean(el.disabled || el.getAttribute('aria-disabled') === 'true'),
            editable: el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el.isContentEditable,
        };
        if (interactive) node.ref = `e${agentRefCounter++}`;
        return node;
    }

    function collectAgentSnapshot(requestId) {
        try {
            agentRefCounter = 1;
            const rootElement = document.querySelector('main') || document.body;
            const snapshotId = `snap-${agentSnapshotCounter++}`;
            const rootNode = rootElement ? snapshotNodeForElement(rootElement, 0) : null;
            post({
                kind: 'agent_snapshot_result',
                requestId,
                snapshot: {
                    snapshotId,
                    pageRevision: agentPageRevision,
                    url: location.href,
                    title: document.title || '',
                    root: rootNode ? [rootNode] : [],
                },
            });
        } catch (err) {
            post({
                kind: 'agent_snapshot_result',
                requestId,
                snapshot: {
                    snapshotId: `snap-${agentSnapshotCounter++}`,
                    pageRevision: agentPageRevision,
                    url: location.href,
                    title: document.title || '',
                    root: [],
                    reason: String(err && err.message || err),
                },
            });
        }
    }

    function isDirectlyInteractive(el) {
        return el.matches('button, a[href], input, textarea, select, [role], [tabindex], summary, label');
    }

    function isUsefulSnapshotElement(el) {
        if (!isDirectlyInteractive(el)) return false;
        const rect = el.getBoundingClientRect();
        if (rect.width < 4 || rect.height < 4) return false;
        const tag = el.tagName.toLowerCase();
        if (tag === 'input') {
            const type = (el.getAttribute('type') || 'text').toLowerCase();
            if (type === 'hidden') return false;
        }
        if (Array.from(el.children || []).some(child => visible(child) && isDirectlyInteractive(child))) {
            const role = roleOf(el);
            return role === 'button' || role === 'link' || role === 'textbox';
        }
        return Boolean(roleOf(el) || accessibleName(el) || elementText(el));
    }

    function collectVisibleElements(requestId) {
        try {
            const seen = new Set();
            const elements = allVisibleElements()
                .filter(isUsefulSnapshotElement)
                .sort((a, b) => {
                    const ar = a.getBoundingClientRect();
                    const br = b.getBoundingClientRect();
                    return (ar.top - br.top) || (ar.left - br.left);
                })
                .map(el => serializeAgentTarget(el, 'strong'))
                .filter(target => {
                    if (seen.has(target.selector)) return false;
                    seen.add(target.selector);
                    target.text = String(target.text || '').slice(0, 180);
                    target.accessibleName = String(target.accessibleName || '').slice(0, 180);
                    return true;
                })
                .slice(0, 80);
            post({
                kind: 'agent_visible_elements_result',
                requestId,
                snapshot: {
                    url: location.href,
                    title: document.title || '',
                    elements,
                },
            });
        } catch (err) {
            post({
                kind: 'agent_visible_elements_result',
                requestId,
                snapshot: {
                    url: location.href,
                    title: document.title || '',
                    elements: [],
                },
            });
        }
    }

    function agentCandidateScore(el, query, needle) {
        const rect = el.getBoundingClientRect();
        const text = elementText(el).toLowerCase();
        const name = accessibleName(el).toLowerCase();
        const role = roleOf(el);
        let score = 0;
        if (query.type === 'text_exact' && text === needle) score -= 10000;
        if (query.type === 'text_contains' && (text === needle || name === needle)) score -= 8000;
        if (role === 'button' || role === 'link' || role === 'textbox') score -= 3000;
        if (el.matches('button, a[href], input, textarea, select, [role], [tabindex]')) score -= 2000;
        if (Array.from(el.children || []).some(child => elementText(child).toLowerCase().includes(needle))) {
            score += 4000;
        }
        score += Math.min(text.length, 2000);
        score += Math.min(rect.width * rect.height, 100000) / 100;
        return score;
    }

    function sortAgentCandidates(elements, query, needle) {
        return elements.sort((a, b) => agentCandidateScore(a, query, needle) - agentCandidateScore(b, query, needle));
    }

    function resolveAgentTarget(requestId, query) {
        try {
            let candidates = [];
            if (!query || !query.type) {
                post({ kind: 'agent_target_not_found', requestId, reason: 'Missing query type' });
                return;
            }
            if (query.type === 'selected') {
                if (!selected || !visible(selected)) {
                    post({ kind: 'agent_target_not_found', requestId, reason: 'No selected element' });
                    return;
                }
                candidates = [serializeAgentTarget(selected, 'exact')];
            } else if (query.type === 'selector') {
                candidates = Array.from(document.querySelectorAll(query.selector || ''))
                    .filter(visible)
                    .map(el => serializeAgentTarget(el, 'exact'));
            } else if (query.type === 'text_exact' || query.type === 'text_contains') {
                const needle = norm(query.text).toLowerCase();
                const elements = allVisibleElements().filter(el => {
                    const hay = elementText(el).toLowerCase();
                    return query.type === 'text_exact' ? hay === needle : hay.includes(needle);
                });
                candidates = sortAgentCandidates(elements, query, needle)
                    .map(el => serializeAgentTarget(el, query.type === 'text_exact' ? 'exact' : 'strong'));
            } else if (query.type === 'role_and_name') {
                const role = norm(query.role).toLowerCase();
                const name = norm(query.name).toLowerCase();
                const elements = allVisibleElements().filter(el => {
                    return (roleOf(el) || '').toLowerCase() === role &&
                        accessibleName(el).toLowerCase().includes(name);
                });
                candidates = sortAgentCandidates(elements, query, name)
                    .map(el => serializeAgentTarget(el, 'strong'));
            } else if (query.type === 'point') {
                const el = document.elementFromPoint(query.x, query.y);
                candidates = el && visible(el) ? [serializeAgentTarget(el, 'exact')] : [];
            }

            if (candidates.length === 0) {
                post({ kind: 'agent_target_not_found', requestId, reason: 'No visible element matched' });
            } else if (candidates.length === 1) {
                post({ kind: 'agent_target_resolved', requestId, target: candidates[0] });
            } else {
                post({ kind: 'agent_target_ambiguous', requestId, candidates: candidates.slice(0, 8) });
            }
        } catch (err) {
            post({ kind: 'agent_target_not_found', requestId, reason: String(err && err.message || err) });
        }
    }

    function setNativeValue(el, value) {
        const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype :
            el instanceof HTMLInputElement ? HTMLInputElement.prototype : null;
        const setter = proto && Object.getOwnPropertyDescriptor(proto, 'value') &&
            Object.getOwnPropertyDescriptor(proto, 'value').set;
        if (setter) setter.call(el, value);
        else el.value = value;
    }

    function typeIntoAgentTarget(requestId, selector, text) {
        try {
            const el = document.querySelector(selector || '');
            if (!el || !visible(el)) {
                post({ kind: 'agent_type_text_result', requestId, ok: false, reason: 'Target is not visible' });
                return;
            }

            el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
            if (typeof el.focus === 'function') el.focus({ preventScroll: true });

            if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {
                setNativeValue(el, text);
                try {
                    el.setSelectionRange(String(text).length, String(text).length);
                } catch (_) {}
                el.dispatchEvent(new InputEvent('input', {
                    bubbles: true,
                    composed: true,
                    inputType: 'insertText',
                    data: text,
                }));
                el.dispatchEvent(new Event('change', { bubbles: true, composed: true }));
                const valueMatches = el.value === text;
                const valid = typeof el.checkValidity === 'function' ? el.checkValidity() : true;
                const reason = !valueMatches ? `Observed value did not match requested text: ${el.value}` :
                    !valid ? (el.validationMessage || 'Field validation failed') : null;
                post({ kind: 'agent_type_text_result', requestId, ok: valueMatches && valid, value: el.value, reason });
                return;
            }

            if (el.isContentEditable) {
                el.textContent = text;
                el.dispatchEvent(new InputEvent('input', {
                    bubbles: true,
                    composed: true,
                    inputType: 'insertText',
                    data: text,
                }));
                post({ kind: 'agent_type_text_result', requestId, ok: el.textContent === text, value: el.textContent || '' });
                return;
            }

            post({ kind: 'agent_type_text_result', requestId, ok: false, reason: `Target <${el.tagName.toLowerCase()}> cannot receive text` });
        } catch (err) {
            post({ kind: 'agent_type_text_result', requestId, ok: false, reason: String(err && err.message || err) });
        }
    }

    function disabledForAction(el) {
        return Boolean(el.disabled || el.getAttribute('aria-disabled') === 'true');
    }

    function editableForAction(el) {
        return el instanceof HTMLInputElement ||
            el instanceof HTMLTextAreaElement ||
            el.isContentEditable;
    }

    function receivesEvents(el) {
        const rect = el.getBoundingClientRect();
        const x = rect.left + rect.width / 2;
        const y = rect.top + rect.height / 2;
        const hit = document.elementFromPoint(x, y);
        return Boolean(hit && (hit === el || el.contains(hit) || hit.contains(el)));
    }

    function actionabilityForSelector(requestId, selector, requiresEditable) {
        try {
            const el = document.querySelector(selector || '');
            const attached = Boolean(el);
            const isVisible = attached && visible(el);
            const stable = true;
            const enabled = attached && !disabledForAction(el);
            const editable = attached && editableForAction(el);
            const events = attached && isVisible && receivesEvents(el);
            let reason = null;
            if (!attached) reason = 'Element is detached';
            else if (!isVisible) reason = 'Element is not visible';
            else if (!stable) reason = 'Element is not stable';
            else if (!enabled) reason = 'Element is disabled';
            else if (requiresEditable && !editable) reason = 'Element is not editable';
            else if (!events) reason = 'Element does not receive events';
            post({
                kind: 'agent_actionability_result',
                requestId,
                ok: !reason,
                selector,
                checks: {
                    attached,
                    visible: Boolean(isVisible),
                    stable,
                    enabled: Boolean(enabled),
                    editable: Boolean(editable),
                    receivesEvents: Boolean(events),
                },
                reason,
            });
        } catch (err) {
            post({
                kind: 'agent_actionability_result',
                requestId,
                ok: false,
                selector,
                checks: {
                    attached: false,
                    visible: false,
                    stable: false,
                    enabled: false,
                    editable: false,
                    receivesEvents: false,
                },
                reason: String(err && err.message || err),
            });
        }
    }

    function collectPageState(requestId) {
        try {
            const active = document.activeElement instanceof Element ? document.activeElement : null;
            const messageElements = Array.from(document.querySelectorAll(
                '[role="alert"], [aria-live], .toast, .error, .invalid, [data-error], input, textarea, select'
            ));
            const messages = [];
            for (const el of messageElements) {
                if (!visible(el)) continue;
                if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) {
                    if (el.validationMessage) messages.push(el.validationMessage);
                    continue;
                }
                const text = norm(elementText(el));
                if (text) messages.push(text);
            }
            post({
                kind: 'agent_page_state_result',
                requestId,
                state: {
                    url: location.href,
                    title: document.title || '',
                    activeSelector: active ? cssPath(active) : null,
                    activeValue: active && 'value' in active ? String(active.value || '') : null,
                    messages: Array.from(new Set(messages)).slice(0, 8),
                }
            });
        } catch (err) {
            post({
                kind: 'agent_page_state_result',
                requestId,
                state: {
                    url: location.href,
                    title: document.title || '',
                    messages: [String(err && err.message || err)],
                }
            });
        }
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

    window.addEventListener('popstate', bumpAgentPageRevision, true);
    window.addEventListener('hashchange', bumpAgentPageRevision, true);

    window.chrome.webview.addEventListener('message', evt => {
        const msg = evt.data;
        console.log(TAG, 'host message:', msg);
        let parsed = null;
        if (typeof msg === 'string' && msg[0] === '{') {
            try { parsed = JSON.parse(msg); } catch (_) {}
        }
        if (parsed && parsed.kind === 'find_element') {
            resolveAgentTarget(parsed.requestId, parsed.query);
        } else if (parsed && parsed.kind === 'snapshot') {
            collectAgentSnapshot(parsed.requestId);
        } else if (parsed && parsed.kind === 'visible_elements') {
            collectVisibleElements(parsed.requestId);
        } else if (parsed && parsed.kind === 'actionability') {
            actionabilityForSelector(parsed.requestId, parsed.selector, Boolean(parsed.requiresEditable));
        } else if (parsed && parsed.kind === 'type_text') {
            typeIntoAgentTarget(parsed.requestId, parsed.selector, parsed.text || '');
        } else if (parsed && parsed.kind === 'page_state') {
            collectPageState(parsed.requestId);
        } else if (parsed && parsed.kind === 'clear_agent_cursor') {
            post({ kind: 'agent_target_not_found', requestId: parsed.requestId, reason: 'cleared' });
        } else if (msg === 'activate') {
            armed = true;
            ensureOverlay();
        } else if (msg === 'deactivate') {
            armed = false; hovered = null; clearSelection();
        } else if (msg === 'clear_selection') {
            clearSelection();
        }
    });

    installAgentPageRevisionObserver();
    window.__zedDesignMode = true;
    console.log(TAG, 'installed at', location.href);
    post({ kind: 'ready' });
})();
"#;

#[cfg(test)]
mod tests {
    use super::SCRIPT;

    #[test]
    fn agent_listener_is_registered_before_installed_sentinel() {
        let listener = SCRIPT
            .find("window.chrome.webview.addEventListener('message'")
            .expect("script should install the host-message listener");
        let sentinel = SCRIPT
            .find("window.__zedDesignMode = true")
            .expect("script should mark itself installed");

        assert!(
            listener < sentinel,
            "script must not mark itself installed before the host-message listener exists"
        );
    }

    #[test]
    fn fallible_dom_revision_observer_is_not_installed_before_agent_listener() {
        let observer_install = SCRIPT
            .find("installAgentPageRevisionObserver();")
            .expect("script should install page revision observer");
        let listener = SCRIPT
            .find("window.chrome.webview.addEventListener('message'")
            .expect("script should install the host-message listener");

        assert!(
            listener < observer_install,
            "page revision observer must not be able to abort before host-message listener install"
        );
    }

    #[test]
    fn design_mode_activation_and_picking_are_installed_before_ready() {
        let pick_events = SCRIPT
            .find("const PICK_EVENTS = [")
            .expect("script should define picking events");
        let hover = SCRIPT
            .find("document.addEventListener('mousemove'")
            .expect("script should install hover preview listener");
        let activate = SCRIPT
            .find("msg === 'activate'")
            .expect("script should handle design-mode activation");
        let ready = SCRIPT
            .find("post({ kind: 'ready' });")
            .expect("script should signal readiness");

        assert!(pick_events < ready, "element picking must be installed before ready");
        assert!(hover < ready, "hover preview must be installed before ready");
        assert!(activate < ready, "activation handler must be installed before ready");
    }

    #[test]
    fn page_revision_observer_handles_early_document_without_aborting_install() {
        assert!(
            SCRIPT.contains("const root = document.documentElement;"),
            "observer should read documentElement into a guarded root"
        );
        assert!(
            SCRIPT.contains("if (!root)"),
            "observer should handle documentElement being unavailable at document-created time"
        );
        assert!(
            SCRIPT.contains("console.warn(TAG, 'page revision observer unavailable', err);"),
            "observer failures should be contained instead of aborting script install"
        );
    }
}
