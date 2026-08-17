//! `trestle` 命令行。**瘦客户端**：和 MCP 前端共用同一个 daemon。
//!
//! 它存在的理由之一：Claude Code 的 Monitor 只接受本地 shell 命令或 ws URL，
//! 够不到 MCP。所以 daemon 挂了的时候，CLI 是那条兜底路径。

use std::io::Read as _;

use clap::{Parser, Subcommand};
use serde_json::json;

use trestle_core::config::ConfigStore;
use trestle_daemon::ipc::{IpcClient, RequestBody};

#[derive(Parser)]
#[command(
    name = "trestle",
    about = "给 coding agent 用的远程基础设施运行时",
    version
)]
struct Cli {
    /// 配置与状态所在目录。默认是程序所在目录（可用 TRESTLE_HOME 覆盖）。
    #[arg(long, global = true)]
    home: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 有哪些机器、归哪个 connector。不连接任何机器，秒回。
    Targets,

    /// 在目标上跑一条短命令。
    Exec {
        target: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        timeout: Option<u64>,
    },

    /// 读远端文件。
    Read {
        target: String,
        path: String,
        #[arg(long)]
        start_line: Option<u32>,
        #[arg(long)]
        max_lines: Option<u32>,
    },

    /// 写远端文件（内容从 stdin 读）。
    Write {
        target: String,
        path: String,
        #[arg(long)]
        append: bool,
        #[arg(long)]
        make_dirs: bool,
    },

    /// 本地 → 远端。文件与目录自动识别。
    Upload {
        target: String,
        local: String,
        remote: String,
        #[arg(long)]
        sync: bool,
        #[arg(long)]
        dry_run: bool,
    },

    /// 远端 → 本地。
    Download {
        target: String,
        remote: String,
        local: String,
        #[arg(long)]
        sync: bool,
        #[arg(long)]
        dry_run: bool,
    },

    /// 把远端一个端口映射到本地。**本地端口由 host 分配**，你不能指定。
    Forward { target: String, remote_port: u16 },

    /// 调一个工具（插件贡献的那些）。参数是一段 JSON。
    Call {
        tool: String,
        #[arg(default_value = "{}")]
        args: String,
    },

    /// 有哪些工具。
    Tools,

    /// 谁在线、在干什么、开着哪些转发。
    Agents,

    /// 留一句话给别的 agent。TTL 必填——没有过期时间的留言板会变成垃圾堆。
    Note {
        scope: String,
        text: String,
        #[arg(long, default_value_t = 3600)]
        ttl: u64,
    },

    /// 看留言板。
    Notes {
        #[arg(default_value = "")]
        scope: String,
    },

    /// 插件：看、新建、热加载。
    #[command(subcommand)]
    Plugin(PluginCommand),

    /// 建链、量延迟、检查 connector 前置条件。
    Doctor { targets: Vec<String> },

    /// 让 daemon 退出。
    Stop,
}

#[derive(Subcommand)]
enum PluginCommand {
    /// 装了哪些插件、各自贡献了哪些工具、有什么权限。
    List,

    /// 生成一个**一次编译通过**的插件脚手架。
    ///
    /// 这是「摩擦 → capability」闭环的入口：遇到一个没有工具的操作，
    /// 生成脚手架、填十几行、reload，它就变成常驻工具了。
    New {
        name: String,
        #[arg(long, default_value = "")]
        description: String,
    },

    /// 重新扫描插件目录并热加载。之后 Claude Code **不用重连**就能看到新工具。
    Reload,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TRESTLE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            // 错误消息本身就是产物，原样打出来，不加装饰。
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<std::process::ExitCode> {
    let root = cli
        .home
        .map(std::path::PathBuf::from)
        .unwrap_or_else(ConfigStore::default_root);

    let client = connect_or_spawn(&root).await?;
    let hello = client
        .call(RequestBody::Hello {
            label: "cli".into(),
        })
        .await?;
    let agent = hello["agent"].as_str().unwrap_or("cli").to_string();

    let code = match cli.command {
        Command::Targets => {
            let grouped = client.call(RequestBody::Targets).await?;
            print_targets(&grouped);
            std::process::ExitCode::SUCCESS
        }

        Command::Exec {
            target,
            command,
            cwd,
            timeout,
        } => {
            let mut payload = json!({"command": command.join(" ")});
            if let Some(c) = cwd {
                payload["cwd"] = json!(c);
            }
            if let Some(t) = timeout {
                payload["timeout_secs"] = json!(t);
            }
            let out = op(&client, &agent, &target, "shell", &payload).await?;
            print!("{}", out["stdout"].as_str().unwrap_or(""));
            eprint!("{}", out["stderr"].as_str().unwrap_or(""));
            if out["timed_out"] == true {
                eprintln!(
                    "\nshell on {target} timed out; process group killed.\n\
                     For long-running work use `trestle call job_start` instead."
                );
            }
            match out["exit_code"].as_i64().unwrap_or(0) {
                0 => std::process::ExitCode::SUCCESS,
                n => std::process::ExitCode::from(n.clamp(1, 255) as u8),
            }
        }

        Command::Read {
            target,
            path,
            start_line,
            max_lines,
        } => {
            let payload = json!({"path": path, "start_line": start_line, "max_lines": max_lines});
            let out = op(&client, &agent, &target, "read", &payload).await?;
            print!("{}", out["content"].as_str().unwrap_or(""));
            if out["truncated"] == true {
                eprintln!("\n[truncated; {} lines total]", out["total_lines"]);
            }
            std::process::ExitCode::SUCCESS
        }

        Command::Write {
            target,
            path,
            append,
            make_dirs,
        } => {
            let mut content = String::new();
            std::io::stdin().read_to_string(&mut content).ok();
            let payload =
                json!({"path": path, "content": content, "append": append, "make_dirs": make_dirs});
            let out = op(&client, &agent, &target, "write", &payload).await?;
            println!(
                "{} bytes -> {}",
                out["bytes"],
                out["path"].as_str().unwrap_or("")
            );
            std::process::ExitCode::SUCCESS
        }

        Command::Upload {
            target,
            local,
            remote,
            sync,
            dry_run,
        } => {
            let payload = json!({
                "local_path": local, "remote_path": remote,
                "options": {"sync": sync, "dry_run": dry_run}
            });
            let out = op(&client, &agent, &target, "upload", &payload).await?;
            print_transfer(&out, dry_run);
            std::process::ExitCode::SUCCESS
        }

        Command::Download {
            target,
            remote,
            local,
            sync,
            dry_run,
        } => {
            let payload = json!({
                "remote_path": remote, "local_path": local,
                "options": {"sync": sync, "dry_run": dry_run}
            });
            let out = op(&client, &agent, &target, "download", &payload).await?;
            print_transfer(&out, dry_run);
            std::process::ExitCode::SUCCESS
        }

        Command::Forward {
            target,
            remote_port,
        } => {
            let out = op(
                &client,
                &agent,
                &target,
                "forward",
                &json!({"remote_port": remote_port}),
            )
            .await?;
            println!(
                "{}  ->  {target}:{remote_port}",
                out["url"].as_str().unwrap_or("")
            );
            println!("这条通道属于本次会话；Ctrl-C 退出就会被回收。");
            tokio::signal::ctrl_c().await.ok();
            std::process::ExitCode::SUCCESS
        }

        Command::Call { tool, args } => {
            let out = client
                .call(RequestBody::CallTool {
                    agent: agent.clone(),
                    tool,
                    args,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&out)?);
            std::process::ExitCode::SUCCESS
        }

        Command::Tools => {
            let tools = client.call(RequestBody::ListTools).await?;
            if let Some(list) = tools.as_array() {
                for t in list {
                    println!(
                        "\x1b[1m{}\x1b[0m  {}",
                        t["name"].as_str().unwrap_or(""),
                        t["description"].as_str().unwrap_or("")
                    );
                }
                println!("\n{} 个工具", list.len());
            }
            std::process::ExitCode::SUCCESS
        }

        Command::Agents => {
            let out = client.call(RequestBody::Agents).await?;
            let sessions = out["sessions"].as_array().cloned().unwrap_or_default();
            if sessions.is_empty() {
                println!("当前没有 agent 连着");
            }
            for s in &sessions {
                println!(
                    "\x1b[1m{}\x1b[0m  {}  最近：{} {}",
                    s["id"].as_str().unwrap_or(""),
                    s["label"].as_str().unwrap_or(""),
                    s["last_action"].as_str().unwrap_or("(还没做什么)"),
                    s["last_target"].as_str().unwrap_or("")
                );
            }
            let forwards = out["forwards"].as_array().cloned().unwrap_or_default();
            if !forwards.is_empty() {
                println!("\n端口转发：");
                for f in &forwards {
                    println!(
                        "  127.0.0.1:{} -> {}:{}  （属于 {}）",
                        f["local_port"],
                        f["target"].as_str().unwrap_or(""),
                        f["remote_port"],
                        f["owner"].as_str().unwrap_or("")
                    );
                }
            }
            std::process::ExitCode::SUCCESS
        }

        Command::Note { scope, text, ttl } => {
            let out = client
                .call(RequestBody::PutNote {
                    agent: agent.clone(),
                    scope,
                    text,
                    ttl_secs: ttl,
                })
                .await?;
            println!(
                "留言已记下，{} 秒后过期",
                (out["expires_ms"].as_u64().unwrap_or(0) as i64
                    - out["at_ms"].as_u64().unwrap_or(0) as i64)
                    / 1000
            );
            std::process::ExitCode::SUCCESS
        }

        Command::Notes { scope } => {
            let out = client
                .call(RequestBody::Notes {
                    scope: if scope.is_empty() { None } else { Some(scope) },
                })
                .await?;
            let notes = out.as_array().cloned().unwrap_or_default();
            if notes.is_empty() {
                println!("留言板是空的");
            }
            for n in &notes {
                println!(
                    "\x1b[1m{}\x1b[0m  {}  —— {}",
                    n["scope"].as_str().unwrap_or(""),
                    n["text"].as_str().unwrap_or(""),
                    n["author"].as_str().unwrap_or("")
                );
            }
            std::process::ExitCode::SUCCESS
        }

        Command::Plugin(PluginCommand::List) => {
            let plugins = client.call(RequestBody::Plugins).await?;
            for p in plugins.as_array().cloned().unwrap_or_default() {
                let tools = p["tools"].as_array().cloned().unwrap_or_default();
                // 装不上的插件也在清单里，而且要显眼。它「不见了」才是最难查的。
                if p["ok"] == false {
                    println!(
                        "\x1b[31m✗ {}\x1b[0m  装不上",
                        p["name"].as_str().unwrap_or("")
                    );
                    println!("  原因  {}", p["detail"].as_str().unwrap_or(""));
                    println!("  怎么办 {}", p["remedy"].as_str().unwrap_or(""));
                    continue;
                }
                println!(
                    "\x1b[1m{}\x1b[0m {}  {}",
                    p["name"].as_str().unwrap_or(""),
                    p["version"].as_str().unwrap_or(""),
                    p["description"].as_str().unwrap_or("")
                );
                println!(
                    "  工具  {}",
                    tools
                        .iter()
                        .filter_map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let caps = &p["capabilities"];
                let mut granted = Vec::new();
                for kind in caps["arbitrate"].as_array().cloned().unwrap_or_default() {
                    granted.push(format!("arbitrate:{}", kind.as_str().unwrap_or("")));
                }
                if caps["tasks"] == true {
                    granted.push("tasks".to_string());
                }
                if caps["ws"] == true {
                    granted.push("ws".to_string());
                }
                if caps["forward"] == true {
                    granted.push("forward".to_string());
                }
                for prog in caps["local_exec"].as_array().cloned().unwrap_or_default() {
                    granted.push(format!("local-exec:{}", prog.as_str().unwrap_or("")));
                }
                println!(
                    "  权限  {}",
                    if granted.is_empty() {
                        "（只有七个基本操作）".to_string()
                    } else {
                        granted.join(", ")
                    }
                );
            }
            std::process::ExitCode::SUCCESS
        }

        Command::Plugin(PluginCommand::New { name, description }) => {
            let dir = scaffold(&root, &name, &description)?;
            println!("生成了 {}", dir.display());
            println!();
            println!("下一步：");
            println!("  1. 改 src/lib.rs 的 list_tools 与 call");
            println!("  2. .\\scripts\\build-plugins.ps1");
            println!("  3. trestle plugin reload");
            println!();
            println!("第 3 步之后 Claude Code 不用重连就能看到新工具。");
            std::process::ExitCode::SUCCESS
        }

        Command::Plugin(PluginCommand::Reload) => {
            let out = client.call(RequestBody::PluginReload).await?;
            println!("重新加载了 {} 个插件", out["plugins"]);
            println!("已通知所有连着的 agent 刷新工具列表。");
            std::process::ExitCode::SUCCESS
        }

        Command::Doctor { targets } => {
            let out = client.call(RequestBody::Doctor { targets }).await?;
            let mut failures = 0;

            for c in out["connectors"].as_array().cloned().unwrap_or_default() {
                let ok = c["ok"] == true;
                if !ok {
                    failures += 1;
                }
                println!(
                    "{:<18} {}",
                    c["connector"].as_str().unwrap_or(""),
                    if ok {
                        "\x1b[32mready\x1b[0m".to_string()
                    } else {
                        format!(
                            "\x1b[31mnot ready\x1b[0m\n{}",
                            c["error"].as_str().unwrap_or("")
                        )
                    }
                );
            }
            println!();
            for t in out["targets"].as_array().cloned().unwrap_or_default() {
                let ok = t["ok"] == true;
                if !ok {
                    failures += 1;
                }
                println!(
                    "{:<10} {}",
                    t["target"].as_str().unwrap_or(""),
                    if ok {
                        "\x1b[32mok\x1b[0m".to_string()
                    } else {
                        format!(
                            "\x1b[31mFAIL\x1b[0m\n           {}",
                            t["error"].as_str().unwrap_or("")
                        )
                    }
                );
            }
            // Web UI 的端口是随机分配的，而 daemon 是被懒启动的——它的 stderr
            // 进了 null，所以启动日志里那行地址没人看得见。这里是唯一说得出它的地方。
            if let Some(info) = trestle_daemon::ipc::DaemonInfo::read(&root)
                && info.http_port != 0
            {
                println!("\nWeb UI    http://127.0.0.1:{}/", info.http_port);
            }

            if failures > 0 {
                std::process::ExitCode::FAILURE
            } else {
                std::process::ExitCode::SUCCESS
            }
        }

        Command::Stop => {
            client.call(RequestBody::Shutdown).await?;
            println!("daemon 正在退出");
            std::process::ExitCode::SUCCESS
        }
    };

    // 走之前打个招呼：这次会话开的转发会被回收。
    let _ = client.call(RequestBody::Bye { agent }).await;
    Ok(code)
}

async fn op(
    client: &IpcClient,
    agent: &str,
    target: &str,
    op: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    Ok(client
        .call(RequestBody::Op {
            agent: agent.to_string(),
            target: target.to_string(),
            op: op.to_string(),
            payload: payload.to_string(),
        })
        .await?)
}

fn print_targets(grouped: &serde_json::Value) {
    let Some(map) = grouped.as_object() else {
        println!("no targets configured");
        return;
    };
    for (connector, machines) in map {
        println!("\x1b[1m{connector}\x1b[0m");
        for m in machines.as_array().cloned().unwrap_or_default() {
            println!(
                "  {:<8} {}@{}:{}  {}",
                m["name"].as_str().unwrap_or(""),
                m["user"].as_str().unwrap_or(""),
                m["host"].as_str().unwrap_or(""),
                m["port"],
                m["workdir"].as_str().unwrap_or("")
            );
            for line in m["note"].as_str().unwrap_or("").lines() {
                println!("           {line}");
            }
        }
    }
}

fn print_transfer(out: &serde_json::Value, dry_run: bool) {
    if dry_run {
        println!(
            "would transfer {} file(s), {} bytes",
            out["files"], out["bytes"]
        );
        for p in out["planned"].as_array().cloned().unwrap_or_default() {
            println!("  {}", p.as_str().unwrap_or(""));
        }
        return;
    }
    println!(
        "{} file(s), {} bytes -> {}",
        out["files"],
        out["bytes"],
        out["path"].as_str().unwrap_or("")
    );
    if let Some(sha) = out["sha256"].as_str() {
        println!("sha256 {sha}");
    }
}

/// 从模板生成一个插件骨架。
///
/// 「一次编译通过」是硬要求：如果生成出来的东西还要人先修一遍才能编，
/// 那这条闭环就断在第一步了。
fn scaffold(
    root: &std::path::Path,
    name: &str,
    description: &str,
) -> anyhow::Result<std::path::PathBuf> {
    // 名字要能直接当 crate 名和工具名前缀用。
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
    {
        anyhow::bail!(
            "plugin name '{name}' must be lowercase letters, digits and dashes \
             (it becomes a crate name and a tool-name prefix)"
        );
    }

    let repo = root.parent().unwrap_or(root);
    let templates = repo.join("plugins").join("templates").join("rust");
    if !templates.exists() {
        anyhow::bail!("cannot find the plugin template at {}", templates.display());
    }

    let dir = repo.join("plugins").join("tools").join(name);
    if dir.exists() {
        anyhow::bail!("{} already exists", dir.display());
    }
    std::fs::create_dir_all(dir.join("src"))?;

    let description = if description.is_empty() {
        format!("{name} 插件")
    } else {
        description.to_string()
    };
    // 模板里的 wit 路径是相对插件目录的：plugins/tools/<name>/ → 仓库根的 wit/
    let render = |raw: &str| {
        raw.replace("{{NAME}}", name)
            .replace("{{NAME_SNAKE}}", &name.replace('-', "_"))
            .replace("{{DESCRIPTION}}", &description)
            .replace("{{WIT_PATH}}", "../../../wit")
    };

    for (from, to) in [
        ("Cargo.toml.tmpl", "Cargo.toml"),
        ("manifest.toml.tmpl", "manifest.toml"),
        ("src/lib.rs.tmpl", "src/lib.rs"),
    ] {
        let raw = std::fs::read_to_string(templates.join(from))?;
        std::fs::write(dir.join(to), render(&raw))?;
    }
    Ok(dir)
}

/// lazy 启动：连不上就把 daemon 拉起来。用户永远不需要手动 `trestled start`。
async fn connect_or_spawn(root: &std::path::Path) -> anyhow::Result<IpcClient> {
    if let Ok(client) = IpcClient::connect(root).await {
        return Ok(client);
    }

    let exe = std::env::current_exe()?
        .parent()
        .map(|d| {
            d.join(if cfg!(windows) {
                "trestled.exe"
            } else {
                "trestled"
            })
        })
        .ok_or_else(|| anyhow::anyhow!("cannot locate trestled next to this binary"))?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--home").arg(root);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // daemon 要活得比这次 CLI 调用长，所以脱离进程组。
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("cannot start {}: {e}", exe.display()))?;

    // 等它就绪。**第一次**要把全部插件从头编一遍——componentize-py 产出的那个
    // 组件是 18 MB，冷缓存下这一步就要一分多钟，而它恰好发生在「刚装完、第一次用」
    // 的时刻。给 30 秒等于保证第一次必然失败。之后有编译缓存，都是一两秒。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let mut wait = std::time::Duration::from_millis(120);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(wait).await;
        if let Ok(client) = IpcClient::connect(root).await {
            return Ok(client);
        }
        wait = (wait * 2).min(std::time::Duration::from_millis(700));
    }
    Err(anyhow::anyhow!(
        "started {} but it never became reachable; run `trestled --home {} --foreground` to see why",
        exe.display(),
        root.display()
    ))
}
