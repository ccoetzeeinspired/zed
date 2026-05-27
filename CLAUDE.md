.rules

# Fork context (read this first)

This is a personal fork of `zed-industries/zed`, hosted at
`https://github.com/ccoetzeeinspired/zed`. The notes below describe how this
fork differs from upstream, why those differences exist, and the workflow for
keeping it in sync. These notes live only on the `pdf-viewer` and
`claude-only` branches so that `main` stays a byte-for-byte mirror of
upstream and never produces sync conflicts on this file.

## Specs and plans (under `plans/`)

Design documents for in-flight or upcoming features live under `plans/`.
Read the relevant plan before starting implementation work on the
corresponding feature.

| Plan                                            | Status        | Branch (future)  |
|-------------------------------------------------|---------------|------------------|
| [`browser-viewer.md`](plans/browser-viewer.md)  | Draft, ready  | `browser-viewer` |

## Remotes

| Remote     | URL                                                   | Role                                    |
|------------|-------------------------------------------------------|-----------------------------------------|
| `origin`   | `https://github.com/ccoetzeeinspired/zed.git`         | Personal fork — push here.              |
| `upstream` | `https://github.com/zed-industries/zed.git`           | Source repo — read-only, never push.    |

## Branches

- **`main`** — tracks `origin/main`, which mirrors `upstream/main`. No local
  modifications. Only used as a rebase base when syncing.
- **`pdf-viewer`** — the working branch. Carries the PDF viewer crate, the
  msvc_spectre_libs build stub, and this CLAUDE.md addition.

## What this fork adds, and why

### `crates/pdf_viewer/` — in-editor PDF viewer

A v1 "preview now, then fork" native PDF viewer. Upstream Zed has no PDF
support; this fills the gap without dragging in a heavy native PDF dependency.

Approach: pages are rasterized to PNGs via poppler's `pdftoppm` into a
per-file temp cache (`%TEMP%\zed-pdf-viewer\<hash>\`), and rendered as a
vertically scrolling column of `img()` elements. Cache key is
`(path, size, mtime)`, so re-opening is instant.

- `PdfItem` claims `*.pdf` paths via `project::ProjectItem::try_open`. It
  extension-checks the absolute path (not the worktree-relative path) because
  a standalone-opened PDF becomes its own worktree root with an empty
  relative path.
- `PdfView` zoom uses `max_w(relative(zoom))`: 1.0 = fit-to-width, >1.0
  overflows horizontally. Clamped 0.2–6.0, ×1.1 step.
- `pdftoppm` is located by walking `%LOCALAPPDATA%\Microsoft\WinGet\Packages\oschwartz10612.Poppler*\<ver>\Library\bin\`
  first, then falling back to PATH.
- Keybindings (`assets/keymaps/default-windows.json`): `ctrl-=` / `ctrl-+`
  zoom in, `ctrl--` zoom out, `ctrl-0` reset.
- Wired in via `pdf_viewer::init(cx)` in `crates/zed/src/main.rs` and the
  test harness in `crates/zed/src/zed.rs`.

Future direction noted in the module docs: swap the rasterizer for an
in-process `pdfium` renderer with lazy per-page rendering.

### Agent panel — gated to Claude Code only

This fork's `claude-only` branch restricts the in-Zed agent sidebar so it
can only ever route to the Claude Agent (`claude-acp`) external agent,
which in turn uses the user's Claude Code subscription auth. The native
"Zed Agent" path and any other LLM-provider routing are hidden from the
user-facing flow.

The deliberate choice was to **gate, not delete**, the native agent. The
`Agent::NativeAgent` enum variant and its ~50 references across
`agent_panel.rs`, `agent_configuration.rs`, `agent_connection_store.rs`,
and `manage_profiles_modal.rs` remain in the tree. Reasons:

- **Why:** those files are heavily churned upstream (the 103-commit
  sync we did during fork setup touched several of them). Literally
  ripping out `NativeAgent` would turn every sync into a multi-hour
  merge job, forever, with zero user-visible benefit over gating.
- **How to apply:** when you see `Agent::NativeAgent` referenced in a
  diff or upstream change, leave it alone — it's unreachable from the
  panel UI but still load-bearing for collab workspaces and other code
  paths. Only the *user-facing surfaces* (picker entries, default
  selection) are touched.

The actual gating diff is small and lives in `agent_panel.rs`. Search for
`FORK: claude-acp only` to find it. Two changes:

- Constructor defaults `selected_agent` to `Custom { id: "claude-acp" }`
  instead of `Agent::default()` (which is `NativeAgent`).
- The new-thread picker menu drops the "Zed Agent" and
  "New From Summary" entries; both led to native-agent threads.

Side effects deliberately not gated:

- Inline assist (Cmd-K) and terminal-assist still use
  `crates/language_models/`. They're not part of the agent panel, so
  they bill via whatever provider the user has configured for those
  features (Anthropic API key, Zed Pro, etc.). If you want to gate
  those too, that's a future task — much bigger blast radius.

### `vendor/claude-agent-acp/` — patched ACP bridge

To make Claude Code slash commands work in the panel, this fork vendors
the ACP bridge (`@agentclientprotocol/claude-agent-acp`) under
`vendor/claude-agent-acp/` so we can iterate on it locally instead of
relying on the npx-fetched npm package.

Setup procedure (one-time per fresh clone):

```powershell
cd D:\src\zed\vendor\claude-agent-acp
npm install
npm run build              # produces dist/index.js (gitignored)
```

The user's global Zed settings (`%APPDATA%\Zed\settings.json`) must
point `claude-acp` at the local build:

```jsonc
"agent_servers": {
  "claude-acp": {
    "type": "custom",
    "command": "node",
    "args": ["D:/src/zed/vendor/claude-agent-acp/dist/index.js"],
    "env": { "ANTHROPIC_API_KEY": "" }
  }
}
```

- **Why `ANTHROPIC_API_KEY: ""`:** signals the bridge/SDK to use OAuth
  (Claude Code login) instead of API-key auth — matches what the
  upstream registry path does.
- **How to refresh after editing `src/`:** re-run `npm run build`, then
  restart Zed (the bridge subprocess is spawned per session).

Files under `vendor/claude-agent-acp/`:

- `src/` — TypeScript source (modify here).
- `dist/` — Build output (gitignored, locally generated by `npm run build`).
- `node_modules/` — Dependencies (gitignored, locally installed by `npm install`).
- `package.json`, `tsconfig.json`, etc. — Build config (committed).

The vendored source is currently a snapshot of upstream
`agentclientprotocol/claude-agent-acp` at v0.37.0 with no local patches
beyond the ACP-bridge surface itself. Any future fork-specific patches
go in `src/acp-agent.ts` and similar; use a comment marker like
`// FORK:` so they're easy to find on rebases.

### `stubs/msvc_spectre_libs/` — build workaround

A local no-op crate that replaces the crates.io `msvc_spectre_libs` via
`[patch.crates-io]` in the workspace `Cargo.toml`. The upstream crate's
`build.rs` panics (when its `error` feature is enabled, as `microsoft/pet`
does) unless the VS "Spectre-mitigated libs" component is installed. This
stub does nothing and lets the linker use the normal CRT — fine for a
personal build, not appropriate to upstream.

## Build notes

- Target directory is the default in-tree `D:\src\zed\target\`.
  Built binary lands at `D:\src\zed\target\debug\zed.exe` (debug) or
  `D:\src\zed\target\release\zed.exe` (release).
- An older out-of-tree target dir at `D:\zt\` exists from a previous
  shell session that had `CARGO_TARGET_DIR=D:\zt` set. **Don't run
  binaries from `D:\zt\` — they're stale.** Safe to delete the whole
  `D:\zt\` tree to reclaim ~16 GB. The current shell has no
  `CARGO_TARGET_DIR` set and no project `.cargo/config.toml`
  override, so cargo uses the in-tree default.
- Runtime dependency: poppler's `pdftoppm.exe`. Install via
  `winget install oschwartz10612.Poppler`, or ensure it's on PATH.

### Build with `-j 4` on this machine

Always build with `cargo build -j 4` (not bare `cargo build`).

- **Why:** the machine has 8C/16T and 32 GB RAM. Cargo defaults to one
  `rustc` per logical core (16), and several Zed crates
  (`language_model`, `editor`, `theme`, `wasmtime-wasi`) peak at 4–8 GB
  per `rustc` instance. 16 parallel rustcs at that footprint blow past
  available RAM and trigger `rustc-LLVM ERROR: out of memory`, which
  manifests as cascading "invalid metadata" / "only metadata stub
  found" errors in unrelated crates. Closing memory hogs (Chrome,
  Slack) helps, but `-j 4` is the reliable fix: ~24 GB peak, fits in
  available RAM with headroom.
- **How to apply:** every cargo invocation in the sync workflow and
  during day-to-day development. From-scratch debug builds at `-j 4`
  finish in ~5 minutes on this hardware.

### After a toolchain bump, `cargo clean` first

If `rust-toolchain.toml` changes between syncs (upstream bumps the
pinned Rust version), the next build will fail with errors like:

```
error[E0786]: found invalid metadata files for crate `gpui`
error: only metadata stub found for `dylib` dependency `std` ...
```

- **Why:** stale `.rmeta` files in `D:\zt\` were written by the old
  compiler and can't be read by the new one. These same errors can
  *also* be caused by mid-build OOM (see `-j 4` note above); the
  differentiator is whether the build output earlier shows `rustup`
  installing components.
- **How to apply:** run `cargo clean` once after a toolchain bump,
  then build normally. Costs the ~5 min from-scratch build time.

## Sync workflow — pulling upstream changes into this fork

Run this whenever you want to incorporate new upstream Zed commits:

```powershell
# 1. Update local main from upstream
git fetch upstream
git checkout main
git merge --ff-only upstream/main
git push origin main                  # keep the fork's main current too

# 2. Rebase pdf-viewer onto the new main
git checkout pdf-viewer
git rebase main
#    ...resolve conflicts if any (likely Cargo.toml / Cargo.lock /
#    crates/zed/src/main.rs / crates/zed/src/zed.rs — see below)...
cargo build -j 4                      # verify it still compiles (see Build notes for why -j 4)
git push --force-with-lease origin pdf-viewer
```

### Conflict hot spots

Three files in the `pdf-viewer` diff sit in code paths upstream churns
frequently:

- **`Cargo.toml`** — the `members = [...]` insertion and the
  `[workspace.dependencies]` entry sit in alphabetically-sorted lists.
  Re-insert in the right alphabetical slot if upstream reorders.
- **`Cargo.lock`** — usually easiest to take upstream's version
  (`git checkout --theirs Cargo.lock`) then re-run `cargo build` to
  regenerate with our deps included.
- **`crates/zed/src/main.rs`** and **`crates/zed/src/zed.rs`** — our
  `pdf_viewer::init(cx)` call sits in the init list that gets reordered.
  Just re-add the line after upstream's version of the list.

`assets/keymaps/default-windows.json` and everything under
`crates/pdf_viewer/` and `stubs/` won't conflict — upstream doesn't touch
them.

### Rebase vs. merge

We rebase, not merge. Reasons:

- **Why:** keeps `pdf-viewer` as a clean, linear set of "PDF viewer"
  commits on top of current upstream. Easier to inspect, easier to
  eventually open as an upstream PR if desired.
- **How to apply:** always rebase `pdf-viewer` onto `main`; never merge
  `main` into `pdf-viewer`. Push with `--force-with-lease`, never plain
  `--force`.

## What NOT to do

- **Never push to `upstream`** — you don't have write access, but the
  attempt will still surprise you. Push only to `origin`.
- **Never commit fork-specific changes to `main`.** `main` exists solely to
  mirror upstream. All fork changes go on `pdf-viewer` (or other feature
  branches off `main`).
- **Don't try to upstream the `msvc_spectre_libs` stub.** It's a personal
  build workaround, not a fix appropriate for the source repo.
