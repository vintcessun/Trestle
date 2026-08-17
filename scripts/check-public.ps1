<#
.SYNOPSIS
    检查跟踪中的文件里有没有混进个人基础设施信息。

.DESCRIPTION
    这个仓库将来要公开。真实的 IP、端口、用户名、机器清单、硬件规模、机构标识
    一旦进了 git，就等于长期公开——**历史也是公开的**。

    这个检查存在的理由很具体：这类东西已经漏进来过两次。第一次是把整份
    `trestle.toml` 提交了；第二次是脱敏之后，又在 README 里手写了一遍机队清单。
    人会忘，检查不会。

    只看 **git 跟踪中的文件**。你自己的 `config/trestle.toml` 与 `secrets.toml`
    已经 gitignore，本来就不该被扫。

.EXAMPLE
    .\scripts\check-public.ps1
#>
[CmdletBinding()]
param([switch]$Quiet)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$repo = Split-Path $PSScriptRoot -Parent
Set-Location $repo

# 每一条都配一句「为什么」。没有理由的规则会被下一个人当噪音关掉。
$rules = @(
    @{ Name = '公网 IP（非文档保留段）'
       # 文档保留段：192.0.2.x / 198.51.100.x / 203.0.113.x（RFC 5737）
       Pattern = '(?<![\d.])(?!127\.|0\.|10\.|192\.168\.|192\.0\.2\.|198\.51\.100\.|203\.0\.113\.|172\.(1[6-9]|2\d|3[01])\.|169\.254\.|22[4-9]\.|23\d\.)\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}(?![\d.])'
       Why = '真实主机地址。示例请用 RFC 5737 的 203.0.113.x / 198.51.100.x' }

    @{ Name = '真实用户名/家目录'
       Pattern = '/home/(?!alice\b|user\b|youruser\b)[a-z][a-z0-9_-]{2,}|/Users/(?!alice\b)[a-z]'
       Why = '示例统一用 alice' }

    @{ Name = '机构可识别串'
       Pattern = 'xmu|XMU|厦[大门]|securelink|SecureLink|edu\.cn'
       Why = '会把这套基础设施指认到具体单位' }

    # ⚠️ 这份文件里**必须**出现这些模式，否则它就什么都查不出来。
    #    所以任何脱敏脚本都要把它排除掉——它已经被自己的脱敏脚本改坏过一次。
    @{ Name = '硬件清单'
       Pattern = 'A100|A800|H100|H800|SXM4|V100|RTX\s*\d'
       Why = '具体型号与规模是资产披露。示例写「8 x GPU」就够了' }

    @{ Name = '个人本机路径'
       Pattern = '[A-Z]:\\(Scripts|Users)\\[A-Za-z0-9_]+\\'
       Why = '示例请用 <repo> 或相对路径' }

    @{ Name = '真实机队规模'
       Pattern = '[六七八九]台真机'
       Why = '机队规模也是资产信息' }
)

# 这些路径本来就是外部原文或生成物，不扫。
$skip = @('docs/reference/', 'scripts/check-public.ps1', 'Cargo.lock')

$files = git ls-files | Where-Object {
    $f = $_
    -not ($skip | Where-Object { $f -like "$_*" -or $f -eq $_ })
}

$hits = @()
foreach ($f in $files) {
    if (-not (Test-Path $f)) { continue }
    # 二进制与产物跳过
    if ($f -match '\.(wasm|png|jpg|ico|pyc|exe|pdb)$') { continue }
    $text = Get-Content $f -Raw -ErrorAction SilentlyContinue
    if (-not $text) { continue }
    foreach ($rule in $rules) {
        foreach ($m in [regex]::Matches($text, $rule.Pattern)) {
            # 行号，方便直接跳过去
            $line = ($text.Substring(0, $m.Index) -split "`n").Count
            $hits += [pscustomobject]@{
                File = $f; Line = $line; Rule = $rule.Name
                Match = $m.Value.Trim(); Why = $rule.Why
            }
        }
    }
}

if ($hits.Count -eq 0) {
    if (-not $Quiet) {
        Write-Host "✓ 跟踪中的文件里没有发现个人基础设施信息" -ForegroundColor Green
    }
    exit 0
}

Write-Host "✗ 发现 $($hits.Count) 处可能的个人信息：" -ForegroundColor Red
$hits | Group-Object Rule | ForEach-Object {
    Write-Host ""
    Write-Host "  $($_.Name)" -ForegroundColor Yellow
    Write-Host "  → $($_.Group[0].Why)" -ForegroundColor DarkGray
    $_.Group | Select-Object -First 12 | ForEach-Object {
        Write-Host ("    {0}:{1}  {2}" -f $_.File, $_.Line, $_.Match)
    }
    if ($_.Group.Count -gt 12) { Write-Host "    … 还有 $($_.Group.Count - 12) 处" }
}
Write-Host ""
exit 1
