.rules

# Fork context (read this first)

This is a personal fork of `zed-industries/zed`, hosted at
`https://github.com/ccoetzeeinspired/zed`. The notes below describe how this
fork differs from upstream, why those differences exist, and the workflow for
keeping it in sync. These notes live only on the `pdf-viewer` branch so that
`main` stays a byte-for-byte mirror of upstream and never produces sync
conflicts on this file.

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

### `stubs/msvc_spectre_libs/` — build workaround

A local no-op crate that replaces the crates.io `msvc_spectre_libs` via
`[patch.crates-io]` in the workspace `Cargo.toml`. The upstream crate's
`build.rs` panics (when its `error` feature is enabled, as `microsoft/pet`
does) unless the VS "Spectre-mitigated libs" component is installed. This
stub does nothing and lets the linker use the normal CRT — fine for a
personal build, not appropriate to upstream.

## Build notes

- Target directory is the out-of-tree `D:\zt\` (configured via
  `CARGO_TARGET_DIR` or a `.cargo/config.toml` somewhere in the environment).
- Runtime dependency: poppler's `pdftoppm.exe`. Install via
  `winget install oschwartz10612.Poppler`, or ensure it's on PATH.

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
cargo build                           # verify it still compiles
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
