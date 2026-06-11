#!/usr/bin/env node
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";

import { callZedAutomation, requireZedOk } from "./ipc.js";

const server = new McpServer({
  name: "zed-browser",
  version: "0.1.0",
  description:
    "Controls the embedded browser tab inside Zed/ACP, not an external or generic Codex browser.",
}, {
  instructions:
    "Use zed-browser first for any request about the browser visible inside Zed, ACP chat, @Browser/current browser context, Takealot pages opened in Zed, or end-to-end testing in the embedded Zed browser. Prefer browser_snapshot before screenshots or external web search so actions are grounded in the current Zed tab.",
});

function textContent(text: string) {
  return { content: [{ type: "text" as const, text }] };
}

server.tool(
  "browser_navigate",
  "Navigate the embedded browser tab inside Zed/ACP to a URL. Use this instead of generic in-app browser navigation when the user asks for the Zed browser.",
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
  "Capture an accessibility snapshot of the current page in the embedded Zed browser tab. Use this first for Zed browser tasks before screenshots, coordinate clicks, or external search.",
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
  "Click a snapshot element in the embedded Zed browser tab using a ref from browser_snapshot.",
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
  "Type text into an editable element in the embedded Zed browser tab using a ref from browser_snapshot.",
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
  "browser_console_messages",
  "Read buffered console messages (console.* + uncaught errors) from the embedded Zed browser tab",
  {
    level: z
      .enum(["log", "info", "warn", "error", "debug"])
      .optional()
      .describe("Only messages of this level"),
    clear: z.boolean().optional().describe("Clear the console buffer after reading"),
  },
  async ({ level, clear }) => {
    const result = (await requireZedOk(
      await callZedAutomation("console_messages", { level, clear: clear ?? false }),
    )) as { messages?: Array<{ level: string; text: string }>; count?: number };
    const msgs = result.messages ?? [];
    const body = msgs.length
      ? msgs.map((m) => `[${m.level}] ${m.text}`).join("\n")
      : "(no console messages)";
    return textContent(`### Console (${result.count ?? msgs.length})\n${body}`);
  },
);

server.tool(
  "browser_network_requests",
  "List network requests (fetch/XHR) captured in the embedded Zed browser tab",
  {
    clear: z.boolean().optional().describe("Clear the network buffer after reading"),
  },
  async ({ clear }) => {
    const result = (await requireZedOk(
      await callZedAutomation("network_requests", { clear: clear ?? false }),
    )) as {
      requests?: Array<{ id: number; method: string; url: string; status: number | null; ms: number | null }>;
      count?: number;
    };
    const reqs = result.requests ?? [];
    const body = reqs.length
      ? reqs
          .map((r) => `#${r.id} ${r.method} ${r.status ?? "…"} ${r.url}${r.ms != null ? ` (${r.ms}ms)` : ""}`)
          .join("\n")
      : "(no network requests)";
    return textContent(`### Network (${result.count ?? reqs.length})\n${body}`);
  },
);

server.tool(
  "browser_network_request",
  "Get one captured network request by id from the embedded Zed browser tab",
  {
    id: z.number().int().describe("Request id (from browser_network_requests)"),
  },
  async ({ id }) => {
    const result = (await requireZedOk(
      await callZedAutomation("network_request", { id }),
    )) as { request?: unknown };
    return textContent(JSON.stringify(result.request ?? null, null, 2));
  },
);

server.tool(
  "browser_resize",
  "Resize the embedded Zed browser tab's viewport (for responsive testing)",
  {
    width: z.number().int().describe("Viewport width in CSS px"),
    height: z.number().int().describe("Viewport height in CSS px"),
  },
  async ({ width, height }) => {
    await requireZedOk(await callZedAutomation("resize", { width, height }));
    return textContent(`Resized viewport to ${width}×${height}`);
  },
);

server.tool(
  "browser_pdf_save",
  "Save the current page as a PDF file from the embedded Zed browser tab",
  {
    filename: z.string().optional().describe("Output path; defaults to a temp file"),
    landscape: z.boolean().optional().describe("Landscape orientation"),
    printBackground: z.boolean().optional().describe("Print background graphics (default true)"),
  },
  async ({ filename, landscape, printBackground }) => {
    const result = (await requireZedOk(
      await callZedAutomation("pdf_save", {
        landscape: landscape ?? false,
        printBackground: printBackground ?? true,
      }),
    )) as { data: string; bytes: number };
    const out = filename ?? join(tmpdir(), `zed-browser-${Date.now()}.pdf`);
    const buf = Buffer.from(result.data, "base64");
    writeFileSync(out, buf);
    return textContent(`Saved PDF (${buf.length} bytes) to ${out}`);
  },
);

// ---- CP12: storage — cookies, local/session storage, storage_state ----

server.tool(
  "browser_cookie_list",
  "List cookies for the current page in the embedded Zed browser tab",
  {},
  async () => {
    const r = (await requireZedOk(await callZedAutomation("cookie_list"))) as {
      cookies?: Array<{ name: string; value: string; domain?: string }>;
      count?: number;
    };
    const cs = r.cookies ?? [];
    const body = cs.length ? cs.map((c) => `${c.name}=${c.value}${c.domain ? ` (${c.domain})` : ""}`).join("\n") : "(no cookies)";
    return textContent(`### Cookies (${r.count ?? cs.length})\n${body}`);
  },
);

server.tool(
  "browser_cookie_get",
  "Get a cookie by name from the embedded Zed browser tab",
  { name: z.string().describe("Cookie name") },
  async ({ name }) => {
    const r = (await requireZedOk(await callZedAutomation("cookie_get", { name }))) as { cookie?: unknown };
    return textContent(JSON.stringify(r.cookie ?? null, null, 2));
  },
);

server.tool(
  "browser_cookie_set",
  "Set a cookie in the embedded Zed browser tab (defaults to the current page's URL)",
  {
    name: z.string(),
    value: z.string(),
    url: z.string().optional(),
    domain: z.string().optional(),
    path: z.string().optional(),
    secure: z.boolean().optional(),
    httpOnly: z.boolean().optional(),
    sameSite: z.enum(["Strict", "Lax", "None"]).optional(),
    expires: z.number().optional().describe("Unix epoch seconds"),
  },
  async (cookie) => {
    await requireZedOk(await callZedAutomation("cookie_set", { cookie }));
    return textContent(`Set cookie ${cookie.name}`);
  },
);

server.tool(
  "browser_cookie_delete",
  "Delete a cookie by name from the embedded Zed browser tab",
  { name: z.string() },
  async ({ name }) => {
    await requireZedOk(await callZedAutomation("cookie_delete", { name }));
    return textContent(`Deleted cookie ${name}`);
  },
);

server.tool(
  "browser_cookie_clear",
  "Clear all cookies in the embedded Zed browser",
  {},
  async () => {
    await requireZedOk(await callZedAutomation("cookie_clear"));
    return textContent("Cleared all cookies");
  },
);

function registerWebStorage(kind: "local" | "session") {
  const store = kind;
  const pfx = kind === "local" ? "localstorage" : "sessionstorage";
  const label = kind === "local" ? "localStorage" : "sessionStorage";
  server.tool(
    `browser_${pfx}_get`,
    `Get a ${label} value by key in the embedded Zed browser tab`,
    { key: z.string() },
    async ({ key }) => {
      const r = (await requireZedOk(await callZedAutomation("storage_get", { store, key }))) as { value?: unknown };
      return textContent(JSON.stringify(r.value ?? null));
    },
  );
  server.tool(
    `browser_${pfx}_set`,
    `Set a ${label} key/value in the embedded Zed browser tab`,
    { key: z.string(), value: z.string() },
    async ({ key, value }) => {
      await requireZedOk(await callZedAutomation("storage_set", { store, key, value }));
      return textContent(`Set ${label}[${key}]`);
    },
  );
  server.tool(
    `browser_${pfx}_list`,
    `List all ${label} entries in the embedded Zed browser tab`,
    {},
    async () => {
      const r = (await requireZedOk(await callZedAutomation("storage_list", { store }))) as { items?: Record<string, string> };
      const items = r.items ?? {};
      const keys = Object.keys(items);
      const body = keys.length ? keys.map((k) => `${k} = ${items[k]}`).join("\n") : `(${label} empty)`;
      return textContent(`### ${label} (${keys.length})\n${body}`);
    },
  );
  server.tool(
    `browser_${pfx}_delete`,
    `Remove a ${label} key in the embedded Zed browser tab`,
    { key: z.string() },
    async ({ key }) => {
      await requireZedOk(await callZedAutomation("storage_delete", { store, key }));
      return textContent(`Deleted ${label}[${key}]`);
    },
  );
  server.tool(
    `browser_${pfx}_clear`,
    `Clear all ${label} in the embedded Zed browser tab`,
    {},
    async () => {
      await requireZedOk(await callZedAutomation("storage_clear", { store }));
      return textContent(`Cleared ${label}`);
    },
  );
}
registerWebStorage("local");
registerWebStorage("session");

server.tool(
  "browser_storage_state",
  "Capture the current page's cookies + local/session storage to a JSON file (for auth/session reuse)",
  { filename: z.string().optional().describe("Output path; defaults to a temp file") },
  async ({ filename }) => {
    const state = await requireZedOk(await callZedAutomation("storage_state"));
    const out = filename ?? join(tmpdir(), `zed-storage-${Date.now()}.json`);
    writeFileSync(out, JSON.stringify(state, null, 2));
    return textContent(`Saved storage state to ${out}`);
  },
);

server.tool(
  "browser_set_storage_state",
  "Restore cookies + storage into the current page from a saved storage-state file (or inline state)",
  {
    filename: z.string().optional().describe("Path to a storage-state JSON file"),
    state: z.record(z.unknown()).optional().describe("Inline storage-state object (instead of filename)"),
  },
  async ({ filename, state }) => {
    let payload = state;
    if (!payload && filename) {
      payload = JSON.parse(readFileSync(filename, "utf8"));
    }
    if (!payload) {
      throw new Error("browser_set_storage_state requires filename or state");
    }
    const r = (await requireZedOk(await callZedAutomation("set_storage_state", { state: payload }))) as {
      cookies?: number;
      items?: number;
    };
    return textContent(`Restored ${r.cookies ?? 0} cookie(s) + ${r.items ?? 0} storage item(s)`);
  },
);

// ---- CP13: assertions (verify_*) — pass silently, throw on failure ----

server.tool(
  "browser_verify_element_visible",
  "Assert that a snapshot-ref element is visible in the embedded Zed browser tab (errors if not)",
  {
    ref: z.string().optional().describe("Snapshot ref to assert visible"),
    target: z.string().optional().describe("Alias for ref"),
  },
  async ({ ref, target }) => {
    const elementRef = ref ?? target;
    if (!elementRef) throw new Error("browser_verify_element_visible requires ref");
    await requireZedOk(await callZedAutomation("verify_element_visible", { ref: elementRef }));
    return textContent(`✓ ${elementRef} is visible`);
  },
);

server.tool(
  "browser_verify_list_visible",
  "Assert a snapshot-ref list is visible and has at least one item (errors if not)",
  {
    ref: z.string().optional().describe("Snapshot ref of the list"),
    target: z.string().optional().describe("Alias for ref"),
  },
  async ({ ref, target }) => {
    const elementRef = ref ?? target;
    if (!elementRef) throw new Error("browser_verify_list_visible requires ref");
    const r = (await requireZedOk(
      await callZedAutomation("verify_list_visible", { ref: elementRef }),
    )) as { items?: number };
    return textContent(`✓ ${elementRef} is a visible list (${r.items ?? "?"} items)`);
  },
);

server.tool(
  "browser_verify_text_visible",
  "Assert text is visible on the page in the embedded Zed browser tab (errors if not)",
  { text: z.string().describe("Text expected to be visible") },
  async ({ text }) => {
    await requireZedOk(await callZedAutomation("verify_text_visible", { text }));
    return textContent(`✓ text ${JSON.stringify(text)} is visible`);
  },
);

server.tool(
  "browser_verify_value",
  "Assert a snapshot-ref element's value equals the expected value (errors if not)",
  {
    ref: z.string().optional().describe("Snapshot ref of the input/element"),
    target: z.string().optional().describe("Alias for ref"),
    value: z.string().describe("Expected value"),
  },
  async ({ ref, target, value }) => {
    const elementRef = ref ?? target;
    if (!elementRef) throw new Error("browser_verify_value requires ref");
    await requireZedOk(await callZedAutomation("verify_value", { ref: elementRef, value }));
    return textContent(`✓ ${elementRef} value equals ${JSON.stringify(value)}`);
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

// ---- CP15: record + codegen (agent run → runnable Playwright spec) ----

server.tool(
  "browser_record",
  "Record the automation run for codegen. action=start clears+enables the " +
    "recorder (optionally captures storage_state to seed auth); stop disables " +
    "it and returns the action count; status reports current state. After stop, " +
    "use browser_codegen to emit the Playwright spec.",
  {
    action: z
      .enum(["start", "stop", "status"])
      .describe("start | stop | status"),
    captureStorageState: z
      .boolean()
      .optional()
      .describe(
        "On start: also capture cookies+storage so the generated spec can seed " +
          "auth via test.use({ storageState }) and skip the login flow",
      ),
  },
  async ({ action, captureStorageState }) => {
    const result = (await requireZedOk(
      await callZedAutomation("record", { action, captureStorageState }),
    )) as {
      recording?: boolean;
      actions?: number;
      storageStateCaptured?: boolean;
    };
    if (action === "start") {
      return textContent(
        `Recording started${result.storageStateCaptured ? " (storage state captured)" : ""}.`,
      );
    }
    if (action === "stop") {
      return textContent(
        `Recording stopped — ${result.actions ?? 0} action(s) captured. Run browser_codegen to emit the spec.`,
      );
    }
    return textContent(
      `Recording: ${result.recording ? "active" : "inactive"}, ` +
        `${result.actions ?? 0} action(s)` +
        `${result.storageStateCaptured ? ", storage state captured" : ""}.`,
    );
  },
);

server.tool(
  "browser_codegen",
  "Generate a runnable Playwright .spec.ts from the current recording buffer " +
    "and write it to disk. If storage state was captured at record start, also " +
    "writes storage.json next to the spec (referenced by test.use).",
  {
    filename: z
      .string()
      .optional()
      .describe("Output path for the .spec.ts; defaults to a temp file"),
  },
  async ({ filename }) => {
    const result = (await requireZedOk(
      await callZedAutomation("codegen"),
    )) as { script?: string; storageState?: unknown };
    const script = result.script ?? "";
    const out = filename ?? join(tmpdir(), `recorded-flow-${Date.now()}.spec.ts`);
    writeFileSync(out, script);
    let note = "";
    if (result.storageState != null) {
      const stateOut = join(dirname(out), "storage.json");
      writeFileSync(stateOut, JSON.stringify(result.storageState, null, 2));
      note = ` (+ storage state at ${stateOut})`;
    }
    return textContent(
      `Wrote Playwright spec to ${out}${note}\n\n` +
        "Run it under real Playwright:\n" +
        `  npx playwright test ${out}\n\n` +
        "----- spec -----\n" +
        script,
    );
  },
);

async function main() {
  // Verify Zed is reachable before accepting MCP sessions.
  try {
    await requireZedOk(await callZedAutomation("ping"));
  } catch (err) {
    console.error(
      `[zed-browser-mcp] Warning: Zed automation IPC not reachable (${err}). ` +
        "Start Zed, open an embedded browser tab, then retry.",
    );
  }

  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("[zed-browser-mcp] ready (stdio MCP → embedded Zed browser tab)");
}

main().catch((err) => {
  console.error("[zed-browser-mcp] fatal:", err);
  process.exit(1);
});
