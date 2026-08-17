#!/usr/bin/env bash
# 编译 Trestle、装成一个自包含的目录、放进 PATH，并注册给 Claude Code 与 Codex。
#
# 装出来的目录是自包含的：三个二进制、配置、插件、状态全在一起。
# trestle-mcp 要能在**自己旁边**找到 trestled（它负责把 daemon 拉起来），
# 配置与插件默认也在同一个目录，所以这三个文件不能分开放。
#
#   ./scripts/install.sh                  只装
#   ./scripts/install.sh --register       装完注册给 agent
#   ./scripts/install.sh --dest ~/.trestle --register
#   ./scripts/install.sh --skip-build     只重新装配目录
#   ./scripts/install.sh --uninstall
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

dest="$repo/dist"
skip_build=0
register=0
uninstall=0
only=both

while [ $# -gt 0 ]; do
    case "$1" in
        --dest) dest="$2"; shift 2 ;;
        --skip-build) skip_build=1; shift ;;
        --register) register=1; shift ;;
        --uninstall) uninstall=1; shift ;;
        --only) only="$2"; shift 2 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
mkdir -p "$dest"; dest="$(cd "$dest" && pwd)"

step() { printf '\033[36m==> %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
warn() { printf '\033[33m    ! %s\033[0m\n' "$*"; }

bin_link="${XDG_BIN_HOME:-$HOME/.local/bin}"

# ── 卸 ────────────────────────────────────────────────────────────────
if [ "$uninstall" -eq 1 ]; then
    step '注销'
    command -v claude >/dev/null 2>&1 && { claude mcp remove trestle -s user >/dev/null 2>&1 || true; note 'Claude Code'; }
    command -v codex  >/dev/null 2>&1 && { codex  mcp remove trestle        >/dev/null 2>&1 || true; note 'Codex'; }
    [ -e "$bin_link/trestle" ] && { rm -f "$bin_link/trestle"; note "去掉 $bin_link/trestle"; }
    rm -rf "$HOME/.claude/skills/trestle" 2>/dev/null || true
    [ -x "$dest/trestle" ] && "$dest/trestle" stop >/dev/null 2>&1 || true
    # 配置与凭据留着：卸载不该顺手删掉你的机器清单。
    step "$dest 里的 trestle.toml / secrets.toml 留着了"
    exit 0
fi

# ── 编 ────────────────────────────────────────────────────────────────
if [ "$skip_build" -eq 0 ]; then
    step 'cargo build --release'
    cargo build --release --bin trestled --bin trestle --bin trestle-mcp

    step '编插件到 wasm'
    rustup target add wasm32-wasip2 >/dev/null 2>&1 || true
    while read -r manifest; do
        dir="$(dirname "$manifest")"
        name="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$manifest" | head -1)"
        if [ -f "$dir/app.py" ]; then
            if command -v componentize-py >/dev/null 2>&1; then
                (cd "$dir" && componentize-py -d ../../../wit -w tool-plugin componentize app -o "$name.wasm" >/dev/null)
                note "$name (python)"
            else
                warn "$name 跳过：没有 componentize-py"
            fi
            continue
        fi
        (cd "$dir" && cargo build --release --target wasm32-wasip2 >/dev/null)
        cp "$dir/target/wasm32-wasip2/release/${name//-/_}.wasm" "$dir/$name.wasm"
        note "$name"
    done < <(find plugins -name manifest.toml -not -path '*/templates/*')
fi

# ── 装 ────────────────────────────────────────────────────────────────
step "装到 $dest"
# daemon 可能正跑着，占着自己的文件。
[ -x "$dest/trestle" ] && "$dest/trestle" stop >/dev/null 2>&1 || true
sleep 1
for b in trestled trestle trestle-mcp; do
    install -m 0755 "target/release/$b" "$dest/$b"
done
note 'trestled, trestle, trestle-mcp'

step '插件'
count=0
while read -r manifest; do
    dir="$(dirname "$manifest")"
    name="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$manifest" | head -1)"
    [ -f "$dir/$name.wasm" ] || { warn "$name 没有 .wasm，跳过"; continue; }
    kind="$(basename "$(dirname "$dir")")"
    mkdir -p "$dest/plugins/$kind/$name"
    cp "$manifest" "$dest/plugins/$kind/$name/manifest.toml"
    cp "$dir/$name.wasm" "$dest/plugins/$kind/$name/$name.wasm"
    count=$((count + 1))
done < <(find plugins -name manifest.toml -not -path '*/templates/*')
note "$count 个插件"

rm -rf "$dest/plugins/templates"; cp -r plugins/templates "$dest/plugins/templates"
# 脚手架里写的是 ../../../wit，装出来的目录里没有它，`trestle plugin new`
# 生成的插件就编不了。
rm -rf "$dest/wit"; cp -r wit "$dest/wit"
note '脚手架模板与 WIT'

# ── 配置：已存在的绝不覆盖 ─────────────────────────────────────────────
step '配置'
cp config/trestle.example.toml config/secrets.example.toml "$dest/"
if [ -f "$dest/trestle.toml" ]; then
    note 'trestle.toml 已存在，没有动它'
elif [ -f config/trestle.toml ]; then
    cp config/trestle.toml "$dest/trestle.toml"; note '从 config/trestle.toml 复制了一份'
else
    cp config/trestle.example.toml "$dest/trestle.toml"
    warn 'trestle.toml 来自样例：里面是 RFC 5737 文档地址，连不上任何机器。改它。'
fi
if [ -f "$dest/secrets.toml" ]; then
    note 'secrets.toml 已存在，没有动它'
elif [ -f config/secrets.toml ]; then
    cp config/secrets.toml "$dest/secrets.toml"; note '从 config/secrets.toml 复制了一份'
else
    warn '没有 secrets.toml：照着 secrets.example.toml 写一份'
fi
# 这个目录里有凭据。
chmod 700 "$dest" 2>/dev/null || true
chmod 600 "$dest/secrets.toml" "$dest/trestle.toml" 2>/dev/null || true
mkdir -p "$dest/state"

# ── PATH ──────────────────────────────────────────────────────────────
# 只把 CLI 链出去。trestled 与 trestle-mcp 是靠「和调用者同目录」被找到的，
# 不需要也不应该进 PATH。软链解析之后 current_exe() 仍然指向安装目录，
# 所以 CLI 照样找得到 daemon。
step 'PATH'
mkdir -p "$bin_link"
ln -sf "$dest/trestle" "$bin_link/trestle"
note "$bin_link/trestle -> $dest/trestle"
case ":$PATH:" in
    *":$bin_link:"*) note "$bin_link 已经在 PATH 里" ;;
    *)
        line="export PATH=\"$bin_link:\$PATH\""
        added=0
        for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
            [ -f "$rc" ] || continue
            grep -qF "$bin_link" "$rc" && { note "$rc 里已经有了"; added=1; continue; }
            printf '\n# Trestle\n%s\n' "$line" >> "$rc"
            note "写进了 $rc"
            added=1
        done
        [ "$added" -eq 0 ] && warn "把这行加进你的 shell 配置：$line"
        note '当前这个 shell 还是旧的 PATH，新开一个或者 source 一下才生效'
        ;;
esac

# ── 自检 ──────────────────────────────────────────────────────────────
step '自检'
[ -x "$dest/trestled" ] && [ -x "$dest/trestle-mcp" ] || { echo "装完之后二进制还是不在" >&2; exit 1; }
note 'trestled 与 trestle-mcp 同目录'

rm -f "$dest/daemon.json"
note '起 daemon（第一次要把插件全编一遍，可能要一两分钟）'
"$dest/trestled" --home "$dest" >/dev/null 2>&1 &
ready=0
for _ in $(seq 1 240); do
    sleep 1
    if [ -f "$dest/daemon.json" ] && "$dest/trestle" tools >/dev/null 2>&1; then ready=1; break; fi
    printf '.'
done
printf '\n'
[ "$ready" -eq 1 ] || { echo "daemon 起不来。看看它自己怎么说：$dest/trestled --home $dest --foreground" >&2; exit 1; }
note "工具面回答正常：$("$dest/trestle" tools | tail -1)"

# ── 注册 ──────────────────────────────────────────────────────────────
if [ "$register" -eq 1 ]; then
    if [ "$only" = both ] || [ "$only" = claude ]; then
        step '注册给 Claude Code'
        if command -v claude >/dev/null 2>&1; then
            claude mcp remove trestle -s user >/dev/null 2>&1 || true
            claude mcp add trestle -s user -e TRESTLE_AGENT=claude-code -- "$dest/trestle-mcp" && note 'ok'
            mkdir -p "$HOME/.claude/skills"
            rm -rf "$HOME/.claude/skills/trestle"
            cp -r .claude/skills/trestle "$HOME/.claude/skills/trestle"
            note "skill 装到 $HOME/.claude/skills/trestle"
        else
            warn '找不到 claude 命令，跳过'
        fi
    fi
    if [ "$only" = both ] || [ "$only" = codex ]; then
        step '注册给 Codex'
        if command -v codex >/dev/null 2>&1; then
            codex mcp remove trestle >/dev/null 2>&1 || true
            codex mcp add trestle --env TRESTLE_AGENT=codex -- "$dest/trestle-mcp" && note 'ok'
        else
            warn '找不到 codex 命令，跳过'
        fi
    fi
fi

echo
step '装好了'
echo "  目录   $dest"
echo "  CLI    trestle（新开一个 shell 之后可以直接用）"
[ "$register" -eq 1 ] && echo "  下一步 重开一个 Claude Code / Codex 会话" || echo "  注册   再跑一次，带上 --register"
