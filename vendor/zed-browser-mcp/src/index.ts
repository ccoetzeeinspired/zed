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
  },
  async ({ target, ref }) => {
    const elementRef = ref ?? target;
    if (!elementRef) {
      throw new Error("browser_click requires target or ref from browser_snapshot");
    }
    await requireZedOk(
      await callZedAutomation("click", { ref: elementRef, target: elementRef }),
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
  },
  async ({ target, ref, text, submit }) => {
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
      .describe("Text to wait to disappear (not implemented — use snapshot)"),
  },
  async ({ time, text, textGone }) => {
    if (textGone) {
      throw new Error(
        "textGone is not implemented yet — take a fresh browser_snapshot instead",
      );
    }
    if (time != null && time > 0) {
      await requireZedOk(await callZedAutomation("wait_for", { time }));
      return textContent(`Waited ${time}s`);
    }
    if (text) {
      await requireZedOk(await callZedAutomation("wait_for", { text }));
      return textContent(`Found text ${JSON.stringify(text)}`);
    }
    await requireZedOk(await callZedAutomation("wait_for", { wait_load: true }));
    return textContent("Page load complete");
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
