# zed-browser-mcp

MCP server for the **embedded Zed browser tab**. Tools drive the
in-editor browser via loopback IPC to `browser_viewer::automation` — not a
detached browser process.

## Setup (one-time per clone)

```powershell
cd D:\src\zed\vendor\zed-browser-mcp
npm install
npm run build   # refreshes tracked dist/index.js when TypeScript changes
```

`dist/index.js` is checked in so Zed can expose this server to ACP sessions
without requiring an install/build step at agent launch time.

Zed must be running with at least one embedded browser tab open.

Local ACP sessions in Zed receive this server automatically from the built-in
`zed-browser` MCP descriptor. Manual `context_servers` configuration is only
needed when running the MCP server outside that built-in ACP path.

## Zed settings

For manual testing, add the server to Zed's `settings.json`.

macOS example:

```jsonc
"context_servers": {
  "zed-browser": {
    "command": "node",
    "args": ["/Users/you/src/zed/vendor/zed-browser-mcp/dist/index.js"]
  }
}
```

Windows example:

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
| `browser_wait_for` | Wait for load, URL, visible text, text-gone, element ref, or time |

## IPC

Default loopback: `127.0.0.1:19382` (override with `ZED_BROWSER_AUTOMATION_PORT` in Zed's environment).

Protocol: one JSON line per request/response:

```json
{"id":"…","method":"snapshot","params":{}}
{"id":"…","ok":true,"result":{"yaml":"…","ref_count":42}}
```

## Agent loop example

```text
browser_navigate → browser_wait_for(url/text/load) → browser_snapshot → browser_wait_for(ref/text) → browser_type / browser_click → …
```

Use `browser_wait_for` and `browser_verify_*` for structured assertions before
falling back to screenshots. Screenshots are for visual confirmation, not the
primary way to search for text or prove navigation.

Always re-snapshot after navigation — refs are invalidated on page change.
