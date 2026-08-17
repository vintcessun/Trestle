# Trestle

[![CI](https://github.com/vintcessun/trestle/actions/workflows/ci.yml/badge.svg)](https://github.com/vintcessun/trestle/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English] [[简体中文](README_zh.md)]

Trestle is an infrastructure runtime for coding agents. It runs a local daemon and
exposes your servers to Claude Code, Codex and other MCP clients as a set of tools:
run commands, read and edit files, move data between machines, manage long-running
jobs, allocate GPUs and forward ports.

It has seven primitive operations — read, write, edit, shell, upload, download and
forward. Everything else, including job control, cross-machine transfers, GPU
arbitration and the web UI, is a WebAssembly plugin built on top of them.

```console
$ trestle targets
gpu-cluster
  gpu-1    alice@203.0.113.10:2201  /mnt/data/alice/work
           8 x GPU. Only reachable through the proxy.
  gpu-4    alice@203.0.113.31:2204  /home/alice/data
           8 x GPU. Most free disk; prefer this one for new work.
cloud
  web-1    root@198.51.100.10:22  /root

$ trestle exec gpu-4 "nvidia-smi --query-gpu=name --format=csv,noheader"
$ trestle call job_start '{"target":"gpu-4","command":"python train.py","gpus":"auto:2"}'
```

## Installation

Requires Rust 1.90 or later and the `wasm32-wasip2` target. Python 3.9+ must be
available on the machines you connect to.

```console
$ rustup target add wasm32-wasip2

# Windows
$ .\scripts\install.ps1 -Register

# Linux, macOS
$ ./scripts/install.sh --register
```

This builds the binaries and the plugins, installs them into `dist/`, puts `trestle`
on your `PATH`, and registers the MCP server with Claude Code and Codex. Open a new
shell and a new agent session afterwards.

| Flag | Effect |
|---|---|
| `-Register` / `--register` | register with Claude Code and Codex |
| `-Only codex` / `--only codex` | register with one of them |
| `-SkipBuild` / `--skip-build` | reassemble without rebuilding |
| `-Dest` / `--dest` | install somewhere other than `dist/` |
| `-Uninstall` / `--uninstall` | unregister and stop the daemon |

The three binaries have to stay in one directory: `trestle-mcp` looks for `trestled`
next to itself, and configuration, plugins and state default to the same place.
An existing `trestle.toml` or `secrets.toml` is never overwritten.

Prebuilt archives for Windows, Linux and macOS are attached to each
[release](https://github.com/vintcessun/trestle/releases).

## Configuration

`trestle.toml` holds machines and connectors; `secrets.toml`, in the same directory,
holds credentials and is gitignored. Copy the examples and edit them.

```toml
[connectors.gpu-cluster]
plugin = "ssh-socks5"                # driver: SSH through a SOCKS5 proxy
socks = "127.0.0.1:11080"
allow_exec = ["docker"]              # local commands this connector may run

[connectors.gpu-cluster.ready]       # optional: bring the proxy up if it is down
check = ["docker", "ps", "-a", "--filter", "name=^vpn-proxy$", "--format", "{{.Names}}"]
check_expect = "vpn-proxy"
start = ["docker", "start", "vpn-proxy"]

[targets.gpu-4]
connector = "gpu-cluster"
host = "203.0.113.31"
port = 22
user = "alice"
workdir = "/home/alice/data"
aliases = ["node-16"]
note = "8 x GPU. Most free disk; prefer this one for new work."
```

`note` is passed through to the agent, so it is a good place for things like which
disk is full and where files belong. See
[`config/trestle.example.toml`](config/trestle.example.toml) for every field.

## Usage

Agents call the tools directly. The same operations are available from the CLI:

```console
$ trestle targets                     # machines, grouped by connector
$ trestle exec gpu-4 "nvidia-smi"
$ trestle read gpu-4 /path/to/file
$ trestle upload gpu-4 ./local /remote --sync
$ trestle forward gpu-4 8080          # local port is assigned by the host
$ trestle agents                      # who is connected and what they are doing
$ trestle doctor                      # connect, measure latency, print the web UI URL
```

The daemon starts on demand; there is no `trestled start`. The web UI, served on the
daemon's HTTP port, shows machine status, the job table, a live event stream and a
configuration editor.

## Tools

| Tools | Purpose |
|---|---|
| `base_read` `base_write` `base_edit` `base_shell` | files and commands |
| `base_upload` `base_download` `base_forward` | transfers and port forwarding |
| `job_start` `job_list` `job_logs` `job_wait` `job_stop` | long-running work |
| `fs_list` `fs_find` `fs_stat` `fs_tree` `fs_disk` | remote filesystem |
| `gpu_status` `gpu_find` `gpu_acquire` `gpu_release` | GPU arbitration |
| `fleet_status` `fleet_run` `targets_list` | fleet-wide |
| `xfer_between` `xfer_distribute` | machine to machine |
| `monitor_open` | a WebSocket endpoint for live output |
| `agents_list` `notes_list` `note_put` | coordination between agents |

Every tool that acts on one machine requires an explicit `target`; there is no
default machine.

## How it works

```
Claude Code / Codex / CLI / browser
            |  MCP stdio, IPC, HTTP
        trestled
            |  seven operations, routed by target
     connector plugin (wasm)  ->  SSH, proxy, long-lived connection
     remote agent (python, long-lived)
```

A connector owns one way in. It exposes a name and the seven operations upward, and
handles which machines it manages, how to reach them, reconnection and remote agent
deployment downward. Two drivers ship with Trestle: `ssh-socks5` and `ssh-direct`.
One driver can back any number of connectors.

Plugins have no I/O of their own. A WebAssembly component has no syscalls, so host
imports are the only way out, and each import checks a capability declared in the
plugin's manifest.

Connections live in the daemon rather than in each MCP session, because a client
starts one MCP process per session and reconnecting costs seconds per machine.

The reasoning behind these choices is in [docs/01-architecture.md](docs/01-architecture.md).

## Writing a plugin

```console
$ trestle plugin new mytool --description "what it does"
$ cd plugins/tools/mytool && cargo build --release --target wasm32-wasip2
$ trestle plugin reload
```

The scaffold compiles unmodified. After `plugin reload` the daemon pushes
`tools/list_changed`, so clients see the new tool without reconnecting. Plugins can
also be written in Python through `componentize-py`, at the cost of a much larger
component.

## Documentation

Design docs are written in Chinese.

| Doc | Contents |
|---|---|
| [01-architecture.md](docs/01-architecture.md) | the whole picture and one call end to end |
| [02-seven-operations.md](docs/02-seven-operations.md) | the primitives and their semantics |
| [03-connectors.md](docs/03-connectors.md) | connectors, the transport toolbox, the remote agent |
| [04-plugins.md](docs/04-plugins.md) | capabilities, instance pools, interface compatibility |
| [05-monitor-and-ui.md](docs/05-monitor-and-ui.md) | the Monitor ws contract, events, the web UI |
| [06-multi-agent.md](docs/06-multi-agent.md) | presence, session-scoped resources, arbitration |
| [07-fleet-lessons.md](docs/07-fleet-lessons.md) | measurements and traps from the previous generation |
| [08-operating.md](docs/08-operating.md) | installing, configuring, wiring into agents, debugging |
| [09-source-map.md](docs/09-source-map.md) | what each file does |

## Development

```console
$ cargo test --workspace                 # no machines required
$ python3 agent-py/test_agent.py         # remote agent protocol
$ ./scripts/check-public.ps1             # no personal infrastructure in tracked files

$ TRESTLE_HOME=$PWD/config cargo test --workspace -- --ignored --test-threads=1
```

Tests that connect to real servers are `#[ignore]` by default.

Layout:

```
crates/         core, transport, host, daemon, mcp, cli
agent-py/       remote agent (long-lived, stdlib only)
plugins/        connectors/, lib/, tools/, templates/
wit/            plugin interface (connector and tool-plugin worlds)
```

## License

MIT.
