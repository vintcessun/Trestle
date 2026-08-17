# 08 · 怎么跑起来

## 一条命令装完

```powershell
.\scripts\install.ps1 -Register     # Windows
```

```bash
./scripts/install.sh --register     # Linux / macOS
```

它做五件事：编（含插件编到 wasm）、装成一个自包含的目录（默认 `dist/`）、
把配置与凭据搬过去（已存在就不覆盖）、把 CLI 放进 PATH、注册给 Claude Code 与 Codex。

| 参数 | 作用 |
|---|---|
| `-Register` / `--register` | 注册给两个客户端 |
| `-Only codex` / `--only codex` | 只注册一个 |
| `-SkipBuild` / `--skip-build` | 不重编，只重新装配 |
| `-Dest` / `--dest` | 装到别处 |
| `-Uninstall` / `--uninstall` | 注销并停掉 daemon（配置与凭据保留） |

为什么必须"装"而不是直接跑 `target/release/trestle-mcp`：`trestle-mcp` 要在
**自己旁边**找到 `trestled`（它负责把 daemon 拉起来），配置、插件与状态也默认
在同一个目录。散着放这条链就断了。

PATH 里只放 CLI。`trestled` 与 `trestle-mcp` 是靠"和调用者同目录"被找到的，
不需要进 PATH；软链解析之后 `current_exe()` 仍然指向安装目录，所以照样找得到。

## 手动构建

```powershell
cargo build --release              # 三个二进制：trestled / trestle / trestle-mcp
.\scripts\build-plugins.ps1        # 全部插件编到 wasm
```

首次需要：

```powershell
rustup target add wasm32-wasip2
uv tool install componentize-py    # 只有要写 Python 插件才需要
```

## 在 WSL 里跑 Linux 那一份

```bash
cd /mnt/d/.../Trestle
export CARGO_TARGET_DIR=$HOME/trestle-target   # ← 这一行不能省
cargo test --workspace
```

`CARGO_TARGET_DIR` 必须换掉。仓库挂在 `/mnt/` 下时，WSL 与 Windows 看到的是
同一个 `target/`，而两边的产物路径完全重叠（都是 `target/debug/`）。共用的结果是
每次切换平台都全量重编，还会把对方刚编好的东西覆盖掉。

## Portable：所有运行期文件都在程序目录

```
<程序目录>/
  trestled.exe  trestle.exe  trestle-mcp.exe
  trestle.toml            统一配置
  secrets.toml            凭据（gitignore）
  daemon.json             IPC 端口 + token（权限收到仅当前用户）
  state/                  job 表 / 留言板 / forward 声明 / 插件 KV
  plugins/                *.wasm
```

wasmtime 的编译缓存也开着。没有它的话 daemon 每次启动都要把全部插件从头编一遍——
Rust 插件几百 KB 无所谓，但一个 componentize-py 产出的插件是 18 MB。
实测启动 2.1s（冷缓存）→ 1.0s（热缓存）；而 daemon 是 lazy 启动的，
那个差值直接加在「agent 第一次调用」的延迟上。

开发期用 `TRESTLE_HOME` 指到 `config/`，免得配置跟着 `target/debug/` 跑。

程序目录不可写时（比如装在 `Program Files`）会明确报错并建议装到用户目录——
**不会静默回退到 AppData**，那正是要避免的混乱。

## 配置

一个文件、一个入口。分节：`[daemon]` `[defaults]` `[connectors.<name>]` `[targets.<name>]`。

```toml
[connectors.gpu-cluster]     # 配置节的名字 = 这一组机器的称呼
plugin = "ssh-socks5"            # 驱动：通用的「经 SOCKS5 连过去的 SSH」
socks = "127.0.0.1:11080"
allow_exec = ["docker"]          # 准它在本机跑什么

[connectors.gpu-cluster.ready]   # 前置条件：探不通就把代理拉起来
start = ["docker", "start", "vpn-proxy"]

[targets.gpu-4]
connector = "gpu-cluster"
host = "203.0.113.31"
port = 2204
user = "alice"
workdir = "/home/alice/data"
aliases = ["node-16"]
note = "..."
```

**机器叫什么名字由这里决定，不由 connector 硬编码。** 想把 `198.51.100.10` 叫 `web-1`，
改一行就行。

凭据在 `secrets.toml`，支持三种写法：

```toml
[targets.gpu-4]
password = "明文"
# password = "env:TRESTLE_X63_PW"      从环境变量读，值不落盘
# password = "file:C:/path/to/secret"  从文件读（读完会去掉尾部换行）

[targets.web-1]
key_path = "~/.ssh/id_ed25519"
```

connector 与插件通过 `config-schema()` 声明自己需要哪些字段，Web UI 据此渲染表单。

## 接进 Claude Code 与 Codex

安装脚本的 `-Register` / `--register` 已经做了，等价于：

```powershell
claude mcp add trestle -s user -e TRESTLE_AGENT=claude-code -- <程序目录>/trestle-mcp
codex  mcp add trestle    --env TRESTLE_AGENT=codex        -- <程序目录>/trestle-mcp
```

不需要先起 daemon——前端连不上会自己把它拉起来。

`TRESTLE_AGENT` 决定这个会话在 `agents_list` 里叫什么。写错的话 Codex 的会话
会在留言板上冒充 Claude Code，而多 agent 协同的全部意义就是知道对面是谁。

**怎么教 agent 用好它**，两条路：

* **Claude Code**：`.claude/skills/trestle/SKILL.md`，`-Register` 会装到
  `~\.claude\skills\`。按需加载，不占常驻上下文。
* **所有客户端（含 Codex）**：MCP 的 `instructions` 字段。Codex 没有 skill，
  所以那段是它唯一的机会——里面只放"不知道就会做错"的事
  （没有默认机、长任务用 job、要卡用 gpu_acquire、unknown state 不要重试）。

查一眼：

```powershell
claude mcp list
codex mcp list
```

## CLI

```
trestle targets                     有哪些机器，按 connector 分组，秒回
trestle exec gpu-4 "nvidia-smi"       跑一条短命令
trestle read gpu-4 /path/to/file
trestle upload gpu-4 ./local /remote --sync
trestle forward gpu-4 8080            映射端口，本地口由 host 分配
trestle call job_start '{"target":"gpu-4","command":"python train.py"}'
trestle tools                       有哪些工具
trestle agents                      谁在线、在干什么、开着哪些转发
trestle note "gpu-4:/data/exp1" "在跑实验" --ttl 3600
trestle notes
trestle plugin list / new <name> / reload
trestle doctor                      建链、检查 connector 前置条件
trestle stop                        让 daemon 退出
```

## 测试

```powershell
cargo test --workspace                                          # 不需要真机
wsl python3 agent-py/test_agent.py                              # 远端 agent 协议
$env:TRESTLE_HOME = "<repo>\config"
cargo test --workspace -- --ignored --test-threads=1             # 真机验收
```

真调测试**默认 `#[ignore]`**，因为它们真的会连服务器、真的起进程、真的传文件。
但它们才是有价值的那部分——「看起来对」和「真的对」之间的差距只有真调能量出来。

## 验收标准

改完之后这六条都要绿，缺一条都不算完成：

```powershell
cargo test --workspace                                # 117 项，约 5 秒
cargo test --workspace -- --ignored --test-threads=1  # 14 项真机
wsl python3 agent-py/test_agent.py                    # 61 项
cargo clippy --workspace --all-targets                # 必须 0 warnings
# 插件是独立 workspace，要分别过一遍：
#   cd plugins/<kind>/<name> && cargo clippy --target wasm32-wasip2 --all-targets
cargo fmt --all --check                               # 必须干净
cargo build --workspace                               # 必须无 warning
```

插件改了之后先 `.\scripts\build-plugins.ps1`，否则测试跑的是旧的 `.wasm`。

## 排查

**`trestle doctor`** 先跑这个。它会检查每个 connector 的前置条件并对每台机器发一次探测。

**日志**：`$env:TRESTLE_LOG = "info"`（或 `trestle_transport=debug` 看每次 SSH exec 的耗时）。
daemon 前台跑：`trestled --home <目录> --foreground`。

**gpu-1 连不上**：多半是 VPN 容器没起来。connector 会自己 `docker start`，但容器**必须
先存在**——它不会替你 `docker run`。错误消息里带着创建命令。

**远端 agent 有问题**：`~/.trestle/agent.log` 在服务器上。
`pkill -f 'trestle_agent.py --serve'` 杀掉它，下次调用会自动重装。

**Windows 控制台中文乱码**：CLI 显式设了 UTF-8 输出，但 `chcp 65001` 有时仍然必要。
