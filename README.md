<!-- Relative path so GitHub does not rewrite via camo (often broken in CN). -->
<p align="center">
  <img src="docs/logo.png" alt="PinkCode" width="128" />
</p>

<h1 align="center">PinkCode - Grok Desktop GUI</h1>

<p align="center">
  <strong>Effortless Parallel Tasks & Crystal-Clear Usage Visuals.</strong>
</p>

<p align="center">
  <a href="#screenshot">Screenshot</a>
  ·
  <a href="#features">Features</a>
  ·
  <a href="#installation">Installation</a>
  ·
  <a href="#development">Development</a>
  ·
  <a href="#architecture">Architecture</a>
</p>

Run multiple [Grok Build](https://x.ai/cli) tasks side by side, follow every task in a readable live timeline, and see where your credits and tokens go. PinkCode turns Grok Build's CLI workflow into a visual desktop workspace while keeping `grok` itself in charge: it connects over [ACP](https://spec.acp.dev) (Agent Client Protocol) via stdio and does not run a separate agent loop.

**Tauri 2 · React 19 · TypeScript · Rust**

## Screenshot

<p align="center">
  <img src="docs/product.jpg" alt="PinkCode — parallel tasks, live Timeline, usage, workspace" width="100%" />
</p>

## Features

PinkCode is centered on two things that are difficult to manage from a terminal alone: keeping several agent tasks moving at once and understanding their activity and cost at a glance.

| Focus | What PinkCode makes easier |
|------|----------|
| **Parallel tasks** | Work across multiple Grok Build sessions from one task board. Create, switch, prompt, and stop tasks independently; each task connects to `grok` over ACP when you first send a message. Existing sessions are loaded from `~/.grok` (`%USERPROFILE%\.grok` on Windows). |
| **Clear task activity** | Read user messages, agent responses, thoughts, tool calls, shell output, plans, and events in one live timeline. Subagent and background-task cards update live; attach/reconnect refills running work via ACP list APIs. Reconnect restores history from `updates.jsonl`. |
| **Usage visuals** | See weekly Grok credit usage with a per-product breakdown, a 7-day token-usage series from session logs, and live turn tokens/cost while a turn is running. |

The rest of the interface keeps those parallel workflows practical:

| Area | Behavior |
|------|----------|
| **File changes** | Review agent file hunks from `hunk_records.jsonl`. |
| **Workspace & Git** | Browse the project tree, preview text and images, and manage Git: branch status (ahead/behind), staged/unstaged lists, inline file diffs, **per-hunk stage/unstage**, and commit. |
| **Modes & plans** | Shift+Tab-style cycle aligned with Grok Build: **Normal → Plan → Auto → Always-approve**. Plan is orthogonal to permission mode; free-text send becomes `/plan …`. When the agent exits plan mode, review and choose Approve, Request changes, or Quit. |
| **Model** | Switch the session model mid-task over ACP `session/set_model`. |
| **Permissions** | Default (ask), Accept edits, Auto (classified by Grok), Bypass permissions, Don't ask. Per-task prefs in `~/.pinkcode/task_prefs.json`. Handles tool permission, file writes, plan approval, and ask-user questions; the task list surfaces **Needs input** when a reverse-request is open. |
| **Updates** | Check GitHub Releases automatically on startup and optionally install an update in one click. |

## Installation

### 1. Install Grok Build

PinkCode requires the [Grok Build CLI](https://x.ai/cli).

**Windows (PowerShell):**

```powershell
irm https://x.ai/cli/install.ps1 | iex
```

**macOS / Linux / WSL:**

```bash
curl -fsSL https://x.ai/cli/install.sh | bash
```

By default, Grok stores its data under `~/.grok` on macOS/Linux and
`%USERPROFILE%\.grok` on Windows. Set `GROK_HOME` to use another location.

### 2. Install PinkCode

Download a prebuilt installer from **[GitHub Releases](https://github.com/3xian/PinkCode/releases)**:

- Windows x64: NSIS installer
- macOS: Apple Silicon and Intel builds
- Linux: build from source; CI installers are not available yet

On Wayland (niri, Sway, Hyprland, …) WebKitGTK cannot composite a transparent
window, and GTK client-side decorations render as a black bar. Launch with:

```bash
PinkCode --disable-csd --disable-transparency
```

## Development

### Prerequisites

| | macOS | Windows 11 | Linux |
|---|---|---|---|
| Node | 24+ | 24+ | 24+ |
| Rust | stable | stable (`x86_64-pc-windows-msvc`) | stable |
| Platform | Xcode CLT | MSVC Build Tools + [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) | webkit2gtk ([Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)) |
| Grok Build | installed | installed | installed |

Windows toolchain (once):

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

```powershell
powershell -ExecutionPolicy Bypass -File scripts/windows-setup.ps1
```

### Build and run

```bash
npm ci
npm run tauri:dev          # development
npm run tauri:build        # local installer under src-tauri/target/release/bundle/
npm run check              # frontend + Rust (fmt/clippy/test) — same as CI
```

**Env (optional)**

| Variable | Meaning |
|----------|---------|
| `GROK_BIN` | Path to `grok` / `grok.exe` |
| `GROK_HOME` | Grok data root (default `~/.grok`) |

**CLI (optional)**

| Flag | Meaning |
|------|---------|
| `--disable-csd` | Hide client-side decorations (black title bar on Wayland) |
| `--disable-transparency` | Opaque window (WebKitGTK has no Wayland alpha protocol) |
| `-h`, `--help` | Show flags |

## Architecture

```
UI (React 19 + TypeScript)
  |-- invoke() --> Tauri commands
  |                 sessions, agent lifecycle, permissions,
  |                 billing, workspace FS, git status / hunk apply
  |-- listen() <-- Tauri events
                    agent-* | sessions-changed
        |
        |-- AgentManager — N x `grok agent stdio` (ACP) + host permission gate
        |-- Session index — FS watcher on ~/.grok/sessions (cards, hunks, stats)
        |-- Billing — HTTP calls to Grok billing API (OIDC auth via ~/.grok/auth.json)
```

PinkCode communicates with Grok Build over ACP (JSON-RPC over stdio): prompts, `session/set_mode`, `session/set_model`, usage/recap extensions, and lifecycle notifications. The host-side permission gate intercepts reverse RPCs (`session/request_permission`, `fs/write_text_file`, `x.ai/exit_plan_mode`, `x.ai/ask_user_question`) and applies the configured risk policy before allowing or denying agent actions.

## License

Copyright (c) 2026 3xian.

Licensed under the [Apache License 2.0](LICENSE).
