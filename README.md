**English** · [简体中文](README_zh.md)

# Trestle

**An extensible infrastructure runtime for coding agents.**

> Trestle lets agents turn infrastructure friction into reusable capabilities.

A remote-infrastructure runtime for coding agents (Claude Code, Codex, and friends).
It is not another SSH-over-MCP wrapper — that lane is crowded. Trestle bets on
something else:

```
Agent hits friction  →  solves it once  →  freezes it into a capability  →  reuses it forever
        ↑                                                                        │
        └──────────────────────── next friction ←────────────────────────────────┘
```

The whole system rests on two sentences:

> **1. There are exactly seven primitive operations**: read / write / edit / shell /
> upload / download / forward.
>
> **2. A connector is one self-contained way in.** Upward it exposes nothing but a
> name and those seven operations. Downward it owns which machines it manages, how
> to reach them, how to keep the connection alive, what to do when it dies, and how
> the remote agent gets deployed. **Callers never learn whether SSH is involved.**

Everything else — job control, file browsing, cross-machine transfers, fleet
overviews, GPUs, monitoring, the web UI — is a **WASM plugin** built on those seven
operations. Plugins have no I/O of their own: a wasm component has no syscalls, so
host imports are the only place it can touch the outside world, and every one of
those imports checks a capability on the way in.

## Install

```powershell
.\scripts\install.ps1 -Register
```

Builds, assembles a self-contained directory (`dist\`), and registers it with both
Claude Code and Codex. Open a new session and the tools are there.

The installed directory is self-contained on purpose: three binaries, config,
credentials, plugins and state all together. `trestle-mcp` has to find `trestled`
*next to itself*, and scattering those across three places breaks that chain. An
existing `trestle.toml` or `secrets.toml` is **never overwritten**.

Teaching an agent to use it well takes two different routes. Claude Code reads
`.claude/skills/trestle/SKILL.md` (`-Register` installs it to `~\.claude\skills\`).
Codex has no skills, so the same must-know rules live in the MCP `instructions`
field — which every client receives.

## What it does today

```
$ trestle targets                    # six machines, grouped by connector, instant
$ trestle exec gpu-4 "nvidia-smi"      # 36ms warm
$ trestle call job_start '{"target":"gpu-4","command":"python train.py","gpus":"auto:2"}'
$ trestle call monitor_open '{"timeout_secs":3600,"only_job":"train-..."}'
$ trestle agents                     # who is online, doing what, holding which forwards
$ trestle plugin new mytool          # scaffold → build → reload → a permanent tool
```

**The friction → capability loop is real**: the scaffold `plugin new` emits compiles
without editing a single character, and after `plugin reload` Claude Code sees the
new tool **without reconnecting**.

## Measured (six real machines, 2026-08-17)

| | steady-state cold start | warm call | self-heal |
|---|---|---|---|
| gpu-4 | 566ms | 36ms | 508ms |
| gpu-1 (via VPN) | 2.4s | 55ms | 2.5s |
| web-1 / web-2 | 1.0–1.2s | 26 / 116ms | ~1s |

Warm is 36× faster than cold. That is the entire reason state lives in a daemon
rather than in each MCP session.

## Documentation

The design docs are written in Chinese.

| Doc | Contents |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | The whole picture, four load-bearing decisions, one call end to end |
| [02-seven-operations.md](docs/02-seven-operations.md) | The seven primitives, and why each one is shaped the way it is |
| [03-connectors.md](docs/03-connectors.md) | What a connector contains, the transport toolbox, the remote agent |
| [04-plugins.md](docs/04-plugins.md) | What a plugin can see, capabilities, instance pools, writing one |
| [05-monitor-and-ui.md](docs/05-monitor-and-ui.md) | The Monitor ws contract, the event model, the web UI |
| [06-multi-agent.md](docs/06-multi-agent.md) | Presence, session-scoped resources, the noticeboard, single-point arbitration |
| [07-fleet-lessons.md](docs/07-fleet-lessons.md) | **Measurements and six traps** (the one doc inherited from the previous generation) |
| [08-operating.md](docs/08-operating.md) | Installing, configuring, wiring into Claude Code and Codex, the CLI, debugging |
| [09-source-map.md](docs/09-source-map.md) | What every file does, its skeleton, and what to look at when reviewing |

## Layout

```
crates/     core · transport · host · daemon · mcp · cli
agent-py/   the standard remote agent (uv, long-lived, stdlib only)
plugins/    connectors/{ssh-socks5,ssh-direct} · lib/connector-ready
            tools/{job,fs,gpu,xfer,fleet,monitor,hello-py}
            templates/rust/   ← what `trestle plugin new` copies
wit/        the plugin interface (two worlds: connector and tool-plugin)
```

## Stack (versions verified)

| Layer | Choice | Version |
|---|---|---|
| Plugin runtime | [`wasmtime`](https://crates.io/crates/wasmtime) + Component Model | 47.0.3 |
| MCP frontend | [`rmcp`](https://crates.io/crates/rmcp) (the official Rust SDK) | 3.1.2 |
| SSH | [`russh`](https://crates.io/crates/russh) (pure Rust, async) | 0.62.6 |
| Plugin bindings | `wit-bindgen` / `componentize-py` | 0.60 / 0.25 |
| Async / HTTP / WS | `tokio` 1.53 / `axum` 0.8 / `tokio-tungstenite` 0.30 | — |
| Remote side | python 3.9+ (uv pins the version; stdlib only) | — |

Plugins compile to `wasm32-wasip2` — about 150 KB for a Rust plugin, about 18 MB for
a Python one.

## Tests

```powershell
cargo test --workspace                                # no real machines needed
wsl python3 agent-py/test_agent.py                    # remote agent protocol, 61 cases
$env:TRESTLE_HOME = "<repo>\config"
cargo test --workspace -- --ignored --test-threads=1  # acceptance against real machines
```

The real-machine tests are `#[ignore]` by default because they genuinely connect to
servers, start processes and move files. They are also the valuable half: calling
every tool for real is how the previous generation found a bug in 1 of 53 tools that
no mock test would ever have caught.
