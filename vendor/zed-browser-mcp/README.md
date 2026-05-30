# zed-browser-mcp

MCP server for the **embedded Zed browser tab** (WebView2). Tools drive the
in-editor browser via loopback IPC to `browser_viewer::automation` — not a
detached browser process.

## Setup (one-time per clone)

```powershell
cd D:\src\zed\vendor\zed-browser-mcp
npm install
npm run build   # produces dist/index.js (gitignored)
```

Zed must be running with at least one browser tab open (`browser: new tab`).

## Zed settings

Add to `%APPDATA%\Zed\settings.json`:

```jsonc
"context_servers": {
  "zed-browser": {
    "command": "node",
    "args": ["D:/src/zed/vendor/zed-browser-mcp/dist/index.js"]
  }
}
```

Enable the server in your agent profile (Agent Panel → Settings → MCP servers), or set `"enable_all_context_servers": true` on the profile.

For **claude-acp**, configured `context_servers` are forwarded automatically as `mcpServers` when a session starts.

## Tier 1 tools

| Tool | Description |
|------|-------------|
| `browser_navigate` | Navigate the embedded tab |
| `browser_snapshot` | Accessibility YAML + `eN` refs |
| `browser_click` | Click by ref (`target` or `ref`) |
| `browser_type` | Type into input by ref |
| `browser_wait_for` | Wait for load, text, or time |

## IPC

Default loopback: `127.0.0.1:19382` (override with `ZED_BROWSER_AUTOMATION_PORT` in Zed's environment).

Protocol: one JSON line per request/response:

```json
{"id":"…","method":"snapshot","params":{}}
{"id":"…","ok":true,"result":{"yaml":"…","ref_count":42}}
```

## Agent loop example

```text
browser_navigate → browser_wait_for → browser_snapshot → browser_type / browser_click → …
```

Always re-snapshot after navigation — refs are invalidated on page change.
