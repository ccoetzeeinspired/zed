#!/usr/bin/env node
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

import { callZedAutomation, requireZedOk } from "./ipc.js";

const server = new McpServer({
  name: "zed-browser",
  version: "0.1.0",
});

function textContent(text: string) {
  return { content: [{ type: "text" as const, text }] };
}

server.tool(
  "browser_navigate",
  "Navigate the embedded Zed browser tab to a URL",
  { url: z.string().describe("The URL to navigate to") },
  async ({ url }) => {
    const result = await requireZedOk(
      await callZedAutomation("navigate", { url }),
    );
    return textContent(`Navigated to ${(result as { url?: string }).url ?? url}`);
  },
);

server.tool(
  "browser_snapshot",
  "Capture accessibility snapshot of the current page in the embedded Zed browser tab",
  {},
  async () => {
    const result = (await requireZedOk(
      await callZedAutomation("snapshot"),
    )) as { yaml?: string; ref_count?: number };
    const yaml = result.yaml ?? "";
    const header =
      result.ref_count != null
        ? `### Page snapshot (${result.ref_count} refs)\n\n`
        : "### Page snapshot\n\n";
    return textContent(`${header}${yaml}`);
  },
);

server.tool(
  "browser_click",
  "Perform click on a web page element in the embedded Zed browser tab",
  {
    element: z
      .string()
      .optional()
      .describe("Human-readable element description"),
    target: z
      .string()
      .optional()
      .describe("Exact target element reference from the page snapshot (e.g. e14)"),
    ref: z
      .string()
      .optional()
      .describe("Alias for target — snapshot ref (e.g. e14)"),
    doubleClick: z.boolean().optional().describe("Perform a double-click"),
    button: z
      .enum(["left", "right", "middle"])
      .optional()
      .describe("Mouse button (default left)"),
    modifiers: z
      .array(z.enum(["Alt", "Control", "Meta", "Shift"]))
      .optional()
      .describe("Modifier keys to hold during the click"),
  },
  async ({ target, ref, doubleClick, button, modifiers }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_click requires target or ref from browser_snapshot");
    }
    await requireZedOk(
      await callZedAutomation("click", {
        ref: elementRef,
        target: elementRef,
        doubleClick: doubleClick ?? false,
        button: button ?? "left",
        modifiers: modifiers ?? [],
      }),
    );
    return textContent(`Clicked ${elementRef}`);
  },
);

server.tool(
  "browser_type",
  "Type text into an editable element in the embedded Zed browser tab",
  {
    element: z
      .string()
      .optional()
      .describe("Human-readable element description"),
    target: z.string().optional().describe("Exact target element reference from snapshot"),
    ref: z.string().optional().describe("Alias for target"),
    text: z.string().describe("Text to type into the element"),
    submit: z
      .boolean()
      .optional()
      .describe("Whether to press Enter after typing"),
    slowly: z
      .boolean()
      .optional()
      .describe("Type one character at a time with real key events (for keystroke-driven fields)"),
    slowlyDelayMs: z
      .number()
      .int()
      .min(0)
      .optional()
      .describe("Per-character delay in ms when slowly=true (0 = as fast as possible; use to pace human-like input)"),
  },
  async ({ target, ref, text, submit, slowly, slowlyDelayMs }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_type requires target or ref from browser_snapshot");
    }
    await requireZedOk(
      await callZedAutomation("type", {
        ref: elementRef,
        target: elementRef,
        text,
        submit: submit ?? false,
        slowly: slowly ?? false,
        slowlyDelayMs: slowlyDelayMs ?? 0,
      }),
    );
    return textContent(`Typed into ${elementRef}`);
  },
);

server.tool(
  "browser_press_key",
  "Press a key on the keyboard in the embedded Zed browser tab",
  {
    key: z
      .string()
      .describe(
        "Key name or character to press, e.g. Enter, ArrowLeft, Escape, a, " +
          "or a modifier chord like Control+a",
      ),
  },
  async ({ key }) => {
    await requireZedOk(await callZedAutomation("press_key", { key }));
    return textContent(`Pressed ${key}`);
  },
);

server.tool(
  "browser_evaluate",
  "Evaluate JavaScript in the embedded Zed browser tab (optionally against an element)",
  {
    function: z
      .string()
      .describe("A JS function expression, e.g. () => document.title or el => el.textContent"),
    ref: z
      .string()
      .optional()
      .describe("Snapshot ref — the function is called with that element as `this` and arg 0"),
    target: z.string().optional().describe("Alias for ref"),
  },
  async ({ function: fn, ref, target }) => {
    const params: Record<string, unknown> = { function: fn };
    const elementRef = ref ?? target;
    if (elementRef) params.ref = elementRef;
    const result = (await requireZedOk(
      await callZedAutomation("evaluate", params),
    )) as { result?: unknown };
    return textContent(JSON.stringify(result.result ?? null, null, 2));
  },
);

server.tool(
  "browser_select_option",
  "Select option(s) in a <select> dropdown in the embedded Zed browser tab",
  {
    element: z.string().optional().describe("Human-readable element description"),
    ref: z.string().optional().describe("Snapshot ref of the <select> element"),
    target: z.string().optional().describe("Alias for ref"),
    values: z
      .union([z.string(), z.array(z.string())])
      .describe("Option value(s), label(s), or visible text to select"),
  },
  async ({ ref, target, values }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_select_option requires target or ref from browser_snapshot");
    }
    const result = (await requireZedOk(
      await callZedAutomation("select_option", { ref: elementRef, values }),
    )) as { matched?: number; value?: string };
    return textContent(`Selected ${result.matched ?? 0} option(s); value=${result.value ?? ""}`);
  },
);

server.tool(
  "browser_hover",
  "Hover the mouse over an element in the embedded Zed browser tab",
  {
    element: z.string().optional().describe("Human-readable element description"),
    ref: z.string().optional().describe("Snapshot ref of the element to hover"),
    target: z.string().optional().describe("Alias for ref"),
  },
  async ({ ref, target }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_hover requires target or ref from browser_snapshot");
    }
    await requireZedOk(await callZedAutomation("hover", { ref: elementRef }));
    return textContent(`Hovered ${elementRef}`);
  },
);

server.tool(
  "browser_file_upload",
  "Upload file(s) to a <input type=file> in the embedded Zed browser tab",
  {
    ref: z.string().optional().describe("Snapshot ref of the file input"),
    target: z.string().optional().describe("Alias for ref"),
    paths: z
      .union([z.string(), z.array(z.string())])
      .describe("Absolute path(s) of the file(s) to upload (must exist on disk)"),
  },
  async ({ ref, target, paths }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_file_upload requires target or ref from browser_snapshot");
    }
    const result = (await requireZedOk(
      await callZedAutomation("file_upload", { ref: elementRef, paths }),
    )) as { uploaded?: number };
    return textContent(`Uploaded ${result.uploaded ?? 0} file(s) to ${elementRef}`);
  },
);

server.tool(
  "browser_drag",
  "Drag from one element to another in the embedded Zed browser tab (mouse-based)",
  {
    startRef: z.string().describe("Snapshot ref of the element to drag from"),
    endRef: z.string().describe("Snapshot ref of the element to drop onto"),
  },
  async ({ startRef, endRef }) => {
    await requireZedOk(
      await callZedAutomation("drag", { startRef, endRef }),
    );
    return textContent(`Dragged ${startRef} → ${endRef}`);
  },
);

server.tool(
  "browser_drop",
  "Drop data onto an element in the embedded Zed browser tab (synthetic HTML5 drop; data/MIME only, not files — use browser_file_upload for files)",
  {
    ref: z.string().optional().describe("Snapshot ref of the drop target"),
    target: z.string().optional().describe("Alias for ref"),
    data: z.string().optional().describe("Data payload to drop"),
    mime: z.string().optional().describe("MIME type for the data (default text/plain)"),
  },
  async ({ ref, target, data, mime }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_drop requires target or ref from browser_snapshot");
    }
    await requireZedOk(
      await callZedAutomation("drop", { ref: elementRef, data, mime }),
    );
    return textContent(`Dropped onto ${elementRef}`);
  },
);

server.tool(
  "browser_handle_dialog",
  "Arm handling of JS dialogs (alert/confirm/prompt) in the embedded Zed browser tab. Call BEFORE the action that triggers the dialog; re-arm after navigation.",
  {
    accept: z.boolean().optional().describe("Accept (true, default) or dismiss (false) the dialog"),
    promptText: z.string().optional().describe("Text to enter for a prompt() dialog when accepting"),
  },
  async ({ accept, promptText }) => {
    const result = (await requireZedOk(
      await callZedAutomation("handle_dialog", { accept: accept ?? true, promptText }),
    )) as { accept?: boolean; last?: { type: string; message: string } | null };
    const last = result.last ? ` (last seen: ${result.last.type} "${result.last.message}")` : "";
    return textContent(`Dialog handling armed: accept=${result.accept}${last}`);
  },
);

// --- Coordinate ("vision") mouse tools: raw viewport CSS-pixel coordinates ---

server.tool(
  "browser_mouse_move_xy",
  "Move the mouse to (x, y) pixel coordinates in the embedded Zed browser tab",
  { x: z.number().describe("X in CSS px"), y: z.number().describe("Y in CSS px") },
  async ({ x, y }) => {
    await requireZedOk(await callZedAutomation("mouse_move_xy", { x, y }));
    return textContent(`Moved to (${x}, ${y})`);
  },
);

server.tool(
  "browser_mouse_click_xy",
  "Click at (x, y) pixel coordinates in the embedded Zed browser tab",
  {
    x: z.number().describe("X in CSS px"),
    y: z.number().describe("Y in CSS px"),
    button: z.enum(["left", "right", "middle"]).optional().describe("Mouse button (default left)"),
    doubleClick: z.boolean().optional().describe("Double-click"),
  },
  async ({ x, y, button, doubleClick }) => {
    await requireZedOk(
      await callZedAutomation("mouse_click_xy", { x, y, button: button ?? "left", doubleClick: doubleClick ?? false }),
    );
    return textContent(`Clicked at (${x}, ${y})`);
  },
);

server.tool(
  "browser_mouse_down",
  "Press a mouse button at (x, y) in the embedded Zed browser tab (pair with browser_mouse_up)",
  {
    x: z.number().describe("X in CSS px"),
    y: z.number().describe("Y in CSS px"),
    button: z.enum(["left", "right", "middle"]).optional().describe("Mouse button (default left)"),
  },
  async ({ x, y, button }) => {
    await requireZedOk(await callZedAutomation("mouse_down", { x, y, button: button ?? "left" }));
    return textContent(`Mouse down at (${x}, ${y})`);
  },
);

server.tool(
  "browser_mouse_up",
  "Release a mouse button at (x, y) in the embedded Zed browser tab",
  {
    x: z.number().describe("X in CSS px"),
    y: z.number().describe("Y in CSS px"),
    button: z.enum(["left", "right", "middle"]).optional().describe("Mouse button (default left)"),
  },
  async ({ x, y, button }) => {
    await requireZedOk(await callZedAutomation("mouse_up", { x, y, button: button ?? "left" }));
    return textContent(`Mouse up at (${x}, ${y})`);
  },
);

server.tool(
  "browser_mouse_drag_xy",
  "Drag the mouse from (startX, startY) to (endX, endY) in the embedded Zed browser tab",
  {
    startX: z.number().describe("Start X in CSS px"),
    startY: z.number().describe("Start Y in CSS px"),
    endX: z.number().describe("End X in CSS px"),
    endY: z.number().describe("End Y in CSS px"),
    button: z.enum(["left", "right", "middle"]).optional().describe("Mouse button (default left)"),
  },
  async ({ startX, startY, endX, endY, button }) => {
    await requireZedOk(
      await callZedAutomation("mouse_drag_xy", { startX, startY, endX, endY, button: button ?? "left" }),
    );
    return textContent(`Dragged (${startX}, ${startY}) → (${endX}, ${endY})`);
  },
);

server.tool(
  "browser_mouse_wheel",
  "Scroll the mouse wheel by (deltaX, deltaY) at (x, y) in the embedded Zed browser tab",
  {
    deltaX: z.number().optional().describe("Horizontal scroll delta (px)"),
    deltaY: z.number().optional().describe("Vertical scroll delta (px, positive = down)"),
    x: z.number().optional().describe("X in CSS px (default 0)"),
    y: z.number().optional().describe("Y in CSS px (default 0)"),
  },
  async ({ deltaX, deltaY, x, y }) => {
    await requireZedOk(
      await callZedAutomation("mouse_wheel", { x: x ?? 0, y: y ?? 0, deltaX: deltaX ?? 0, deltaY: deltaY ?? 0 }),
    );
    return textContent(`Wheel (${deltaX ?? 0}, ${deltaY ?? 0}) at (${x ?? 0}, ${y ?? 0})`);
  },
);

server.tool(
  "browser_take_screenshot",
  "Take a screenshot (PNG/JPEG) of the embedded Zed browser tab",
  {
    fullPage: z
      .boolean()
      .optional()
      .describe("Capture the full scrollable page instead of just the viewport"),
    type: z.enum(["png", "jpeg"]).optional().describe("Image format (default png)"),
    quality: z
      .number()
      .int()
      .min(0)
      .max(100)
      .optional()
      .describe("JPEG quality 0-100 (jpeg only)"),
    ref: z
      .string()
      .optional()
      .describe("Snapshot ref (e.g. e14) to screenshot just that element"),
    target: z.string().optional().describe("Alias for ref"),
  },
  async ({ fullPage, type, quality, ref, target }) => {
    const params: Record<string, unknown> = {};
    if (fullPage != null) params.full_page = fullPage;
    if (type) params.type = type;
    if (quality != null) params.quality = quality;
    const elementRef = ref ?? target;
    if (elementRef) params.ref = elementRef;
    const result = (await requireZedOk(
      await callZedAutomation("screenshot", params),
    )) as { data: string; mimeType: string };
    return {
      content: [
        { type: "image" as const, data: result.data, mimeType: result.mimeType },
      ],
    };
  },
);

server.tool(
  "browser_scroll",
  "Scroll the embedded Zed browser tab — by a pixel delta, or to bring a snapshot element into view",
  {
    ref: z
      .string()
      .optional()
      .describe("Snapshot ref (e.g. e14) to scroll into view; handles inner scroll containers"),
    target: z.string().optional().describe("Alias for ref"),
    dx: z
      .number()
      .optional()
      .describe("Horizontal pixels to scroll, positive = right (ignored if ref given)"),
    dy: z
      .number()
      .optional()
      .describe("Vertical pixels to scroll, positive = down (ignored if ref given)"),
  },
  async ({ ref, target, dx, dy }) => {
    const elementRef = ref ?? target;
    const params: Record<string, unknown> = {};
    if (elementRef) params.ref = elementRef;
    if (dx != null) params.dx = dx;
    if (dy != null) params.dy = dy;
    const result = (await requireZedOk(
      await callZedAutomation("scroll", params),
    )) as { x?: number; y?: number; maxY?: number };
    return textContent(
      `Scrolled to x=${result.x ?? "?"}, y=${result.y ?? "?"} (maxY=${result.maxY ?? "?"})`,
    );
  },
);

server.tool(
  "browser_tabs",
  "List, select, open, or close browser tabs in the embedded Zed browser",
  {
    action: z
      .enum(["list", "select", "new", "close"])
      .optional()
      .describe("Tab operation (defaults to list)"),
    index: z
      .number()
      .int()
      .optional()
      .describe("0-based tab index for select/close (from a browser_tabs list)"),
    url: z
      .string()
      .optional()
      .describe("URL to open for action=new (defaults to the configured homepage)"),
  },
  async ({ action, index, url }) => {
    const params: Record<string, unknown> = {};
    if (action) params.action = action;
    if (index != null) params.index = index;
    if (url) params.url = url;
    const result = (await requireZedOk(
      await callZedAutomation("tabs", params),
    )) as { tabs?: Array<{ index: number; title: string; url: string; active: boolean }>; count?: number };
    const tabs = result.tabs ?? [];
    const lines = tabs.map(
      (t) => `${t.active ? "*" : " "} [${t.index}] ${t.title || "(untitled)"} — ${t.url}`,
    );
    const body = lines.length ? lines.join("\n") : "(no browser tabs open)";
    return textContent(`### Browser tabs (${result.count ?? tabs.length})\n${body}`);
  },
);

server.tool(
  "browser_wait_for",
  "Wait for text to appear or a specified time to pass in the embedded Zed browser tab",
  {
    time: z.number().optional().describe("Time to wait in seconds"),
    text: z.string().optional().describe("Text to wait for on the page"),
    textGone: z
      .string()
      .optional()
      .describe("Text to wait to disappear from the page"),
  },
  async ({ time, text, textGone }) => {
    if (textGone) {
      await requireZedOk(await callZedAutomation("wait_for", { textGone }));
      return textContent(`Text gone ${JSON.stringify(textGone)}`);
    }
    if (text) {
      await requireZedOk(await callZedAutomation("wait_for", { text }));
      return textContent(`Found text ${JSON.stringify(text)}`);
    }
    if (time != null && time > 0) {
      await requireZedOk(await callZedAutomation("wait_for", { time }));
      return textContent(`Waited ${time}s`);
    }
    await requireZedOk(await callZedAutomation("wait_for", { wait_load: true }));
    return textContent("Page load complete");
  },
);

server.tool(
  "browser_navigate_back",
  "Go back to the previous page in the embedded Zed browser tab",
  {},
  async () => {
    const result = (await requireZedOk(
      await callZedAutomation("navigate_back"),
    )) as { url?: string };
    return textContent(`Navigated back to ${result.url ?? ""}`);
  },
);

server.tool(
  "browser_fill_form",
  "Fill multiple form fields in one call in the embedded Zed browser tab",
  {
    fields: z
      .array(
        z.object({
          ref: z.string().describe("Snapshot ref of the field"),
          value: z
            .union([z.string(), z.boolean()])
            .describe("Text for inputs, option for selects, or boolean for checkboxes/radios"),
          type: z
            .enum(["textbox", "checkbox", "radio", "combobox", "select"])
            .optional()
            .describe("Field kind (default textbox)"),
        }),
      )
      .describe("The fields to fill"),
  },
  async ({ fields }) => {
    const result = (await requireZedOk(
      await callZedAutomation("fill_form", { fields }),
    )) as { filled?: number };
    return textContent(`Filled ${result.filled ?? 0} field(s)`);
  },
);

server.tool(
  "browser_close",
  "Close the active browser tab in the embedded Zed browser",
  {},
  async () => {
    await requireZedOk(await callZedAutomation("close"));
    return textContent("Closed the active browser tab");
  },
);

async function main() {
  // Verify Zed is reachable before accepting MCP sessions.
  try {
    await requireZedOk(await callZedAutomation("ping"));
  } catch (err) {
    console.error(
      `[zed-browser-mcp] Warning: Zed automation IPC not reachable (${err}). ` +
        "Start Zed, open a browser tab (browser: new tab), then retry.",
    );
  }

  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("[zed-browser-mcp] ready (stdio MCP → Zed WebView2 tab)");
}

main().catch((err) => {
  console.error("[zed-browser-mcp] fatal:", err);
  process.exit(1);
});
