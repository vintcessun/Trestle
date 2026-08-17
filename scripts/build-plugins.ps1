# 编译所有插件到 wasm 并放到 plugins/<kind>/<name>/<name>.wasm。
#
# cargo 产出的文件名是下划线（lab_gpu_servers.wasm），而 manifest 里的名字是
# 连字符（gpu-cluster）——host 按 manifest 名字找文件，所以这里要改名。

Set-Location (Split-Path $PSScriptRoot -Parent)

$plugins = Get-ChildItem -Path plugins -Recurse -Filter manifest.toml |
    Where-Object { $_.DirectoryName -notmatch 'templates' }

if (-not $plugins) { Write-Host "no plugins found"; exit 0 }

$failed = 0
foreach ($m in $plugins) {
    $dir = $m.DirectoryName
    $name = (Select-String -Path $m.FullName -Pattern '^name\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    Write-Host "building $name ..." -NoNewline

    # Python 插件走 componentize-py，产出直接就是最终文件名。
    if (Test-Path (Join-Path $dir "app.py")) {
        Push-Location $dir
        try {
            $out = & componentize-py -d ..\..\..\wit -w tool-plugin componentize app -o "$name.wasm" 2>&1 | Out-String
            $code = $LASTEXITCODE
        } finally { Pop-Location }
        if ($code -ne 0) {
            Write-Host " FAILED"
            Write-Host $out
            $failed++
            continue
        }
        Write-Host (" ok  {0:N0} bytes (python)" -f (Get-Item (Join-Path $dir "$name.wasm")).Length)
        continue
    }

    Push-Location $dir
    try {
        # cargo 的进度写在 stderr 上，所以这里不能让 stderr 被当成失败信号——
        # 只有退出码说了算。
        $out = & cargo build --release --target wasm32-wasip2 2>&1 | Out-String
        $code = $LASTEXITCODE
    } finally { Pop-Location }

    if ($code -ne 0) {
        Write-Host " FAILED"
        Write-Host $out
        $failed++
        continue
    }

    $built = Join-Path $dir "target\wasm32-wasip2\release\$($name -replace '-','_').wasm"
    if (-not (Test-Path $built)) {
        Write-Host " FAILED (no artifact at $built)"
        $failed++
        continue
    }
    $dest = Join-Path $dir "$name.wasm"
    Copy-Item $built $dest -Force
    Write-Host (" ok  {0:N0} bytes" -f (Get-Item $dest).Length)
}

if ($failed -gt 0) { exit 1 }
