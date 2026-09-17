#!/bin/bash
# Linux CI：准备兄弟仓库依赖，然后跑 test + clippy。
#
# ── 为什么需要这个脚本 ────────────────────────────────────────────────
# 本仓的 Cargo.toml 有两组**指向兄弟仓库的相对 path 依赖**：
#
#   qaqh-client / qaqh-config-api  →  ../qaqh-backend/crates/...
#   ratatui 一族（[patch.crates-io]） →  ../ratatui/...
#
# 本机布局是 `~/Projects/{qaqh-tui-app,qaqh-backend,ratatui}` 同级，所以能解析。
# 但 CNB 的仓库根是 `/workspace`，`../ratatui` 解析成 `/ratatui` —— **不存在**。
# 于是 cargo 在依赖解析阶段就死：
#
#   error: failed to load source for dependency `ratatui`
#     unable to update /ratatui/ratatui
#     failed to read `/ratatui/ratatui/Cargo.toml`
#     No such file or directory (os error 2)
#
# 这自 2026-09-15（T-01 迁移引入 path 依赖）起就让 `main` 的 CI 一直是红的。
#
# ── 版本策略 ─────────────────────────────────────────────────────────
# 两个 rev **钉死**在下面，保证 CI 可复现。改动这里必须同步：
#   - Cargo.toml 里 [patch.crates-io] 的注释（写了 ratatui 的 rev）；
#   - 本文件的 rev 常量。
#
# ratatui 用的是**上游未发版的 main**（ratatui#2743 宽字形修复尚未发版），
# 该 commit 就在 github 上游，无需私有 fork。
#
# ── 本地行为 ─────────────────────────────────────────────────────────
# 若兄弟仓库目录已存在，**跳过**（不动开发者正在用的工作树）。所以本脚本在
# 本机跑是安全的 no-op，可以放心用来验证。

set -euo pipefail

# 与 Cargo.toml 的 path 依赖对应。改动请同步 Cargo.toml 的注释。
QAQH_BACKEND_REPO="${QAQH_BACKEND_REPO:-https://cnb.cool/QAQ-Harness/qaqh-backend.git}"
QAQH_BACKEND_REV="${QAQH_BACKEND_REV:-8c1c1544fd9f9754275970d8aa338287ef20a440}"
RATATUI_REPO="${RATATUI_REPO:-https://github.com/ratatui/ratatui.git}"
RATATUI_REV="${RATATUI_REV:-e02e2a622eda6e4cae105df48a48f641cdba0303}"

ROOT=$(git rev-parse --show-toplevel)
PARENT=$(dirname "$ROOT")

echo "== 依赖准备 =="
echo "仓库根: $ROOT"
echo "兄弟仓库父目录: $PARENT"

# $1=目标目录  $2=git 仓库  $3=rev  $4=显示名
prepare() {
    local dir="$1" repo="$2" rev="$3" name="$4"
    if [ -e "$dir" ]; then
        echo "  [$name] 已存在，跳过（本地开发不受影响）: $dir"
        return 0
    fi
    echo "  [$name] 拉取 $repo @ $rev"
    git clone --filter=blob:none --no-checkout "$repo" "$dir"
    git -C "$dir" checkout --quiet "$rev"
    local got
    got=$(git -C "$dir" rev-parse HEAD)
    if [ "$got" != "$rev" ]; then
        echo "  [$name] ✗ rev 不符：期望 $rev，实得 $got" >&2
        return 1
    fi
    echo "  [$name] ✓ $got"
}

prepare "$PARENT/qaqh-backend" "$QAQH_BACKEND_REPO" "$QAQH_BACKEND_REV" "qaqh-backend"
prepare "$PARENT/ratatui" "$RATATUI_REPO" "$RATATUI_REV" "ratatui"

# libssl-dev / pkg-config：为可能仍需要系统 TLS 头文件的构建准备（幂等）。
if command -v apt-get >/dev/null 2>&1; then
    apt-get update -qq
    apt-get install -y --no-install-recommends pkg-config libssl-dev >/dev/null
fi

echo
echo "== cargo test =="
cargo test --all-targets

echo
echo "== cargo clippy =="
if command -v rustup >/dev/null 2>&1; then
    rustup component add clippy >/dev/null 2>&1 || true
fi
cargo clippy --all-targets -- -D warnings

echo
echo "== fmt（仅本仓）=="
# 注意：不能裸跑 `cargo fmt --all` —— 它会连带格式化兄弟仓库里别人的工作树。
cargo fmt --check

echo
echo "CI OK"
