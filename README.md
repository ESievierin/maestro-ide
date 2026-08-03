# MaestroIDE

<!-- random comment -->

A Tauri desktop app for orchestrating parallel agentic development on top of git worktrees and
Claude Code (via the Claude Agent SDK). The central entity is a task/worktree, not a file.

## Architecture

- **`src-tauri/`** — Rust core. All business logic lives here: event bus, worktree manager,
  session lifecycle, agent bridge, diff engine, gate registry, prompt templates, SQLite store.
- **`src/`** — React + TypeScript frontend. Renders state and sends commands; no business logic.
- **`sidecar/`** — Node.js sidecar embedding the Claude Agent SDK. Executes agent sessions;
  the Rust core owns all state. NDJSON protocol over stdio.
- **`prompts-defaults/`** — default prompt templates copied to `~/.maestro/prompts/` on first run.

## Development

```sh
npm install            # frontend deps
cd sidecar && npm install && cd ..
npm run tauri dev      # launch the app (starts vite + cargo)
```

Rust checks (from `src-tauri/`):

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Frontend checks (from repo root):

```sh
npm run lint
npm run typecheck
npm run format:check
```
