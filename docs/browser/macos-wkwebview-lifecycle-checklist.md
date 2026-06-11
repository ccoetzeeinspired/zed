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
