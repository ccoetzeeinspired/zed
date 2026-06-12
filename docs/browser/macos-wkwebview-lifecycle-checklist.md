# macOS WKWebView Lifecycle Verification

Use this checklist when validating native browser tab embedding on macOS.

1. Open an embedded browser tab and navigate to `https://example.com`.
   - Expected: the page renders inside the tab bounds.
   - Expected: the address bar and tab chrome remain visible above the page.

2. Open an editor tab in the same pane, then switch between the editor tab and browser tab.
   - Expected: the browser surface is hidden while the editor tab is active.
   - Expected: no black rectangle or stale webpage remains over the editor.

3. Open two embedded browser tabs in the same pane with different pages, then switch between them.
   - Expected: only the selected browser tab's page is visible.
   - Expected: the inactive browser tab does not bleed through the active tab.

4. Split the pane, keep a browser tab visible in one pane, and focus an editor in the other pane.
   - Expected: the browser remains visible in its own pane.
   - Expected: changing the active pane does not move or expose stale browser surfaces.

5. Open the command palette, file finder, or another GPUI overlay over a browser tab.
   - Expected: the overlay paints above the page and accepts keyboard input.
   - Expected: pressing Space in the agent chat does not scroll the browser.

6. Close an active browser tab, then type in the agent chat or an editor.
   - Expected: the native WKWebView disappears immediately.
   - Expected: keyboard focus is released back to Zed.

## Regression Gates

Run this section before closing macOS browser automation work or after changing
`browser_view`, `wkwebview_host`, `gpui_macos` native cutouts, or macOS
automation scripts.

1. Embedded surface and overlay behavior
   - Open a browser tab, then open an editor tab in the same pane.
   - Open the command palette over the browser tab.
   - Expected: the page never floats above the editor or command palette.
   - Expected: no black rectangle remains after switching tabs or apps.

2. Manual browser interaction
   - Click the page, click a page input, type text, and submit with Enter.
   - Expected: page clicks, inputs, and buttons respond like a normal browser.
   - Expected: after typing a URL in the address bar and pressing Enter, the
     page remains manually clickable.

3. Focus handoff back to Zed
   - Click or scroll inside the page.
   - Click the ACP chat or an editor.
   - Press Space and type a sentence containing spaces.
   - Expected: Space inserts text in ACP/editor and does not scroll the page.

4. URL and title synchronization
   - Navigate to `https://youtube.com`, then open a watch/search result page.
   - Expected: the browser address bar tracks the actual page URL, not the
     original typed host.
   - Expected: tab title updates from native WebKit page state.

5. Snapshot-first agent behavior
   - Ask the agent to open Takealot and scroll to a heading containing
     `wearable`.
   - Expected: the agent calls `browser_snapshot` before screenshots.
   - Expected: the snapshot exposes the visible commerce heading as a distinct
     heading node, not only inside one huge container text node.
   - Expected: nearby product links/buttons/cards appear as separate refs.
   - Expected: screenshots are used only for visual confirmation or debugging.

6. Scroll and snapshot consistency
   - Scroll with `browser_scroll`.
   - Take another snapshot.
   - Expected: the page visibly moves.
   - Expected: the snapshot reflects content near the new viewport position
     closely enough for the next action to be chosen from refs.

## Automated Coverage Map

- Snapshot shape and ref registry: `cargo test -p browser_viewer snapshot --lib`
- macOS snapshot script contracts: `cargo test -p browser_viewer snapshot_macos --lib`
- macOS focus/tool policy: `cargo test -p browser_viewer macos_ --lib`
- Package check before manual testing:
  `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=target-macos-run cargo check -p browser_viewer`
