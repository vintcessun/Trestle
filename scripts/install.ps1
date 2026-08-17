<#
.SYNOPSIS
    编译 Trestle、装成一个自包含的目录，并把它注册给 Claude Code 与 Codex。

.DESCRIPTION
    装出来的目录是**自包含**的：三个可执行文件、配置、插件、状态全在一起。
    这是刻意的（见 docs/08）——所有运行期文件跟程序在同一个目录，不会出现
    「配置在这、状态在那、插件在第三个地方」。

    trestle-mcp 靠两件事找到自己的东西：
      * `trestled.exe` 必须**和它在同一个目录**（它要能把 daemon 拉起来）；
      * 配置与状态默认就在那个目录（`TRESTLE_HOME` 可以覆盖）。
    所以这不是「把 exe 复制过去」那么简单，插件和配置得一起搬。

.PARAMETER Dest
    装到哪。默认是仓库下的 dist\。

.PARAMETER SkipBuild
    不重新编，只重新装配目录（改完插件想快速刷新时用）。

.PARAMETER Register
    装完之后注册给 Claude Code 与 Codex。

.PARAMETER Only
    只注册给其中一个：claude 或 codex。

.EXAMPLE
    .\scripts\install.ps1 -Register
#>
[CmdletBinding()]
param(
    [string]$Dest,
    [switch]$SkipBuild,
    [switch]$Register,
    [ValidateSet('claude', 'codex', 'both')]
    [string]$Only = 'both',
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
# 外部命令的非零退出码**不当成终止性错误**。`claude mcp remove` 在服务器不存在时
# 就退 1，而"先删再加"正是幂等注册的做法——让它抛异常会把整个安装打断。
$PSNativeCommandUseErrorActionPreference = $false

$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

if (-not $Dest) { $Dest = Join-Path $repo 'dist' }
$Dest = [System.IO.Path]::GetFullPath($Dest)

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Note($msg) { Write-Host "    $msg" -ForegroundColor DarkGray }
function Warn($msg) { Write-Host "    ! $msg" -ForegroundColor Yellow }

# ── 0. 卸 ────────────────────────────────────────────────────────────
if ($Uninstall) {
    Step '注销'
    if (Get-Command claude -ErrorAction SilentlyContinue) {
        claude mcp remove trestle -s user 2>&1 | Out-Null
        Note 'Claude Code ✓'
    }
    if (Get-Command codex -ErrorAction SilentlyContinue) {
        codex mcp remove trestle 2>&1 | Out-Null
        Note 'Codex ✓'
    }
    $skillDst = Join-Path $env:USERPROFILE '.claude\skills\trestle'
    if (Test-Path $skillDst) { Remove-Item $skillDst -Recurse -Force; Note 'skill ✓' }

    if (Test-Path (Join-Path $Dest 'trestle.exe')) {
        & (Join-Path $Dest 'trestle.exe') stop 2>&1 | Out-Null
        Start-Sleep -Milliseconds 800
    }
    Get-Process -Name trestled -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue

    # **配置与凭据留着**。卸载不该顺手删掉你的机器清单——重装一次就想让它回来。
    Step "$Dest 里的 trestle.toml / secrets.toml 留着了，其余可以自己删"
    return
}

# ── 1. 编 ────────────────────────────────────────────────────────────
if (-not $SkipBuild) {
    Step 'cargo build --release'
    cargo build --release --bin trestled --bin trestle --bin trestle-mcp
    if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }

    Step '编插件到 wasm'
    & (Join-Path $PSScriptRoot 'build-plugins.ps1')
    if ($LASTEXITCODE -ne 0) { throw '插件构建失败' }
}

# ── 2. 装 ────────────────────────────────────────────────────────────
Step "装到 $Dest"
New-Item -ItemType Directory -Force -Path $Dest | Out-Null

$exes = @('trestled.exe', 'trestle.exe', 'trestle-mcp.exe')

# 先请 daemon 自己退。
#
# ⚠️ 只能用 CLI 去说这句话。`trestle-mcp.exe stop` 不是「停止」——它会起一个
# MCP server 然后**等 stdin**，于是脚本永远等下去。这个坑踩过一次。
$cliPath = Join-Path $Dest 'trestle.exe'
if (Test-Path $cliPath) {
    & $cliPath stop 2>&1 | Out-Null
    Start-Sleep -Milliseconds 800
}

foreach ($e in $exes) {
    $src = Join-Path $repo "target\release\$e"
    if (-not (Test-Path $src)) { throw "没找到 $src（先跑一次不带 -SkipBuild）" }
    $dst = Join-Path $Dest $e
    try {
        Copy-Item $src $dst -Force
    } catch {
        # 还占着 = 有进程在跑。trestle-mcp 由 Claude Code / Codex 的会话拉起，
        # 它们不会自己退——只能杀。
        $procName = $e -replace '\.exe$', ''
        Warn "$e 被占用，结束 $procName 进程"
        Get-Process -Name $procName -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 600
        Copy-Item $src $dst -Force
    }
}
Note ($exes -join ', ')

# 插件：只搬 host 真正要的东西（manifest + .wasm），源码和 target\ 不进 dist。
Step '插件'
$pluginCount = 0
Get-ChildItem -Path (Join-Path $repo 'plugins') -Recurse -Filter manifest.toml |
    Where-Object { $_.DirectoryName -notmatch '\\templates\\' } |
    ForEach-Object {
        $srcDir = $_.DirectoryName
        $name = (Select-String -Path $_.FullName -Pattern '^name\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
        $wasm = Join-Path $srcDir "$name.wasm"
        if (-not (Test-Path $wasm)) {
            Warn "$name 没有 .wasm，跳过（跑一次 scripts\build-plugins.ps1）"
            return
        }
        # plugins\<kind>\<name>\ —— kind 从源目录里原样继承
        $kind = Split-Path (Split-Path $srcDir -Parent) -Leaf
        $outDir = Join-Path $Dest "plugins\$kind\$name"
        New-Item -ItemType Directory -Force -Path $outDir | Out-Null
        Copy-Item $_.FullName (Join-Path $outDir 'manifest.toml') -Force
        Copy-Item $wasm (Join-Path $outDir "$name.wasm") -Force
        $script:pluginCount++
    }
Note "$pluginCount 个插件"

# 脚手架模板：`trestle plugin new` 要用。
$tpl = Join-Path $repo 'plugins\templates'
if (Test-Path $tpl) {
    Copy-Item $tpl (Join-Path $Dest 'plugins\templates') -Recurse -Force
    Note '插件脚手架模板'
}

# WIT 也要跟过来。
#
# 脚手架里写的是 `path: "../../../wit"`——相对 `plugins/tools/<name>/` 的位置。
# 装出来的目录里没有 wit/ 的话，`trestle plugin new` 生成的插件根本编不了，
# 而「遇到摩擦就现场长一个工具」正是这套东西的卖点。
$wit = Join-Path $repo 'wit'
if (Test-Path $wit) {
    Copy-Item $wit (Join-Path $Dest 'wit') -Recurse -Force
    Note 'WIT 接口定义（plugin new 生成的插件靠它编译）'
}

# ── 3. 配置 ──────────────────────────────────────────────────────────
# **绝不覆盖已有的配置**：装一次和装十次，你的机器清单都不该变。
Step '配置'
$destConfig = Join-Path $Dest 'trestle.toml'
Copy-Item (Join-Path $repo 'config\trestle.example.toml') (Join-Path $Dest 'trestle.example.toml') -Force
Copy-Item (Join-Path $repo 'config\secrets.example.toml') (Join-Path $Dest 'secrets.example.toml') -Force

if (Test-Path $destConfig) {
    Note 'trestle.toml 已存在，没有动它'
} elseif (Test-Path (Join-Path $repo 'config\trestle.toml')) {
    Copy-Item (Join-Path $repo 'config\trestle.toml') $destConfig
    Note '从 config\trestle.toml 复制了一份机器清单'
} else {
    Copy-Item (Join-Path $repo 'config\trestle.example.toml') $destConfig
    Warn 'trestle.toml 是从样例来的：里面是 RFC 5737 文档地址，连不上任何机器。改它。'
}

$destSecrets = Join-Path $Dest 'secrets.toml'
if (Test-Path $destSecrets) {
    Note 'secrets.toml 已存在，没有动它'
} elseif (Test-Path (Join-Path $repo 'config\secrets.toml')) {
    Copy-Item (Join-Path $repo 'config\secrets.toml') $destSecrets
    Note '从 config\secrets.toml 复制了一份凭据'
} else {
    Warn '没有 secrets.toml：照着 secrets.example.toml 写一份，否则连不上机器'
}

# 装出来的目录里有凭据，权限收到当前用户。
foreach ($f in @($destSecrets, $destConfig)) {
    if (Test-Path $f) {
        icacls $f /inheritance:r /grant:r "$($env:USERNAME):(R,W)" 2>&1 | Out-Null
    }
}
Note '配置与凭据的权限已收到当前用户'

New-Item -ItemType Directory -Force -Path (Join-Path $Dest 'state') | Out-Null

# ── 4. 自检 ──────────────────────────────────────────────────────────
Step '自检'
$mcp = Join-Path $Dest 'trestle-mcp.exe'
$cli = Join-Path $Dest 'trestle.exe'
foreach ($need in @($mcp, $cli, (Join-Path $Dest 'trestled.exe'))) {
    if (-not (Test-Path $need)) { throw "装完之后 $need 还是不在" }
}
# trestle-mcp 要能在同目录找到 trestled，否则它拉不起 daemon。
Note 'trestled.exe 与 trestle-mcp.exe 同目录 ✓'

# **显式**把 daemon 拉起来再自检，不靠 CLI 的懒启动。
#
# 两个理由。一是这一步会把全部插件编一遍——componentize-py 那个组件 18 MB，
# 冷缓存下要一分多钟，而这恰好发生在"刚装完"的时刻；显式起来才能给你进度，
# 而不是让一条命令看起来卡死。二是懒启动会在一个被捕获的管道里 fork 出一个
# 长命进程，脚本这边就再也等不到管道关闭。
$daemonExe = Join-Path $Dest 'trestled.exe'
$daemonJson = Join-Path $Dest 'daemon.json'
if (Test-Path $daemonJson) {
    Note '已经有一个 daemon 在跑，让它退出以便用新的二进制'
    & $cli stop 2>&1 | Out-Null
    Start-Sleep -Milliseconds 800
    Remove-Item $daemonJson -ErrorAction SilentlyContinue
}

Note '起 daemon（第一次要把插件全编一遍，可能要一两分钟）'
Start-Process -FilePath $daemonExe -ArgumentList '--home', $Dest -WindowStyle Hidden | Out-Null

$deadline = (Get-Date).AddSeconds(240)
$ready = $false
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 700
    if (Test-Path $daemonJson) {
        & $cli tools 2>&1 | Out-Null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
    }
    Write-Host '.' -NoNewline
}
Write-Host ''
if (-not $ready) {
    throw "daemon 起不来。看看它自己怎么说：`n  $daemonExe --home `"$Dest`" --foreground"
}

$tools = & $cli tools 2>&1 | Out-String
$toolCount = [regex]::Match($tools, '(\d+)\s*个工具').Groups[1].Value
Note "工具面回答正常：$toolCount 个工具"

# ── 5. 注册 ──────────────────────────────────────────────────────────
if ($Register) {
    if ($Only -in @('claude', 'both')) {
        Step '注册给 Claude Code'
        if (Get-Command claude -ErrorAction SilentlyContinue) {
            # 幂等：先删再加，否则第二次装会报 already exists。
            claude mcp remove trestle -s user 2>&1 | Out-Null
            claude mcp add trestle -s user -e TRESTLE_AGENT=claude-code -- $mcp
            if ($LASTEXITCODE -eq 0) { Note 'claude mcp add trestle ✓' }
            else { Warn 'claude mcp add 失败' }
        } else {
            Warn '找不到 claude 命令，跳过'
        }
    }
    if ($Only -in @('codex', 'both')) {
        Step '注册给 Codex'
        if (Get-Command codex -ErrorAction SilentlyContinue) {
            codex mcp remove trestle 2>&1 | Out-Null
            codex mcp add trestle --env TRESTLE_AGENT=codex -- $mcp
            if ($LASTEXITCODE -eq 0) { Note 'codex mcp add trestle ✓' }
            else { Warn 'codex mcp add 失败' }
        } else {
            Warn '找不到 codex 命令，跳过'
        }
    }

    # Skill 只有 Claude Code 认；Codex 读的是仓库里的 AGENTS.md。
    if ($Only -in @('claude', 'both')) {
        $skillSrc = Join-Path $repo '.claude\skills\trestle'
        $skillDst = Join-Path $env:USERPROFILE '.claude\skills\trestle'
        if (Test-Path $skillSrc) {
            New-Item -ItemType Directory -Force -Path $skillDst | Out-Null
            Copy-Item (Join-Path $skillSrc '*') $skillDst -Recurse -Force
            Note "skill 装到 $skillDst"
        }
    }
}

# ── 6. PATH ──────────────────────────────────────────────────────────
# 不加进 PATH 的话，`trestle` 只能靠全路径调用，CLI 基本等于不能用。
#
# 只改用户级 PATH，不碰系统级。写回去之前先看它在不在里面——重复装十次不该
# 让 PATH 里多出十份。
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';' | Where-Object { $_ })
if ($entries -notcontains $Dest) {
    [Environment]::SetEnvironmentVariable('Path', (($entries + $Dest) -join ';'), 'User')
    Step 'PATH'
    Note "已把 $Dest 加进用户 PATH"
    Note '当前这个终端还是旧的 PATH，新开一个才生效'
} else {
    Step 'PATH'
    Note '已经在 PATH 里了'
}
# 当前进程也加上，后面的自检和提示能直接用 trestle
if (($env:Path -split ';') -notcontains $Dest) { $env:Path = "$env:Path;$Dest" }

Write-Host ''
Step '装好了'
Write-Host "  目录     $Dest"
Write-Host "  CLI      $cli"
if ($Register) {
    Write-Host '  下一步   重开一个 Claude Code / Codex 会话，工具就在了'
    Write-Host '           查一眼：claude mcp list   /   codex mcp list'
} else {
    Write-Host '  注册     再跑一次，带上 -Register'
}
