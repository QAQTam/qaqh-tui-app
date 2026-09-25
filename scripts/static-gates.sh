#!/bin/bash
# TUI 静态门禁：把「TUI 不碰后端内部」这条跨仓契约变成可执行检查。
#
# 出处：后端冻结语义 `docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md`
#       §0 裁决 8/9、§11、§12；TUI issue #46 task 6。
#
# 设计原则：**白名单必须显式且带理由**。白名单外的任何新命中都会让门禁红——
# 这样「豁免」是被记录的决定，而不是被遗忘的例外。
#
# 用法：scripts/static-gates.sh        （CI 里由 scripts/ci-linux.sh 调用）
#
# 判据全部是「结构可 grep」的，不做语义推断；不可 grep 的条款（如「旧 epoch
# 不得回滚新状态」）属于 v2 SessionModel 的实现门禁，不在本脚本。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT" || exit 1

fail=0
note() { printf '  [%s] %s\n' "$1" "$2"; }

echo "== 静态门禁（跨仓契约）=="

# ── G1：TUI 不直接依赖后端内部 crate ────────────────────────────────
# spec §0 裁决 8：TUI 只经 `qaqh-client` / `qaqh-config-api` 消费契约。
G1_BANNED='qaqh-(ringing|domain|session)'
g1_hits=$(sed -n '/^\[dependencies\]/,/^\[/p' Cargo.toml \
    | grep -nE "^[a-z0-9_-]+ *=" | grep -E "$G1_BANNED" || true)
if [ -n "$g1_hits" ]; then
    note ✗ "G1 依赖面：直接依赖了后端内部 crate（只允许 qaqh-client / qaqh-config-api）"
    printf '%s\n' "$g1_hits" | sed 's/^/      /'
    fail=1
else
    note ✓ "G1 依赖面：未直接依赖 qaqh-ringing / qaqh-domain / qaqh-session"
fi

# ── G2：展示路径不手解 serde_json::Value ───────────────────────────
# spec §11 / issue #46 task 6：展示层消费 typed 投影，不自己解 JSON。
#
# **白名单（带理由）**：
#   src/app/mod.rs — API **直通**（`query` / `action` / `ConfigLoaded` 的
#     `Value`），不做展示解析。
#   src/app/settings_ops.rs — 把 typed draft **序列化**成 wire 值，不解析。
G2_ALLOWED_FILES='src/app/mod\.rs|src/app/settings_ops\.rs'
g2_files=$(grep -rl "serde_json::Value" --include=*.rs src/ | sort)
g2_bad=$(printf '%s\n' "$g2_files" | grep -vE "^($G2_ALLOWED_FILES)$" || true)
# 解析（from_str::<…Value…>）比类型出现更严格：展示层不允许手解 JSON。
g2_parse=$(grep -rn "serde_json::from_str::<[^>]*Value" --include=*.rs src/ || true)
if [ -n "$g2_bad" ] || [ -n "$g2_parse" ]; then
    [ -n "$g2_bad" ] && {
        note ✗ "G2 展示层：白名单外的 serde_json::Value 出现"
        printf '%s\n' "$g2_bad" | sed 's/^/      /'
    }
    [ -n "$g2_parse" ] && {
        note ✗ "G2 展示层：白名单外手解 JSON（from_str::<Value>）"
        printf '%s\n' "$g2_parse" | sed 's/^/      /'
    }
    fail=1
else
    note ✓ "G2 展示层：serde_json::Value 仅存在于 API 直通/序列化豁免"
fi

# ── G3：不读取服务端存储布局 ───────────────────────────────────────
# spec §0 裁决 8：TUI 不读 journal / checkpoint / offload / messages 布局。
#
# 只匹配**字符串字面量里的路径**（`"…/journal/…"` / `"…/offload/…"`）以及
# `messages.jsonl` / `checkpoint.json` 这两个具体文件名。
# 不匹配散文：`offloaded` 是合法 wire 字段名，文档注释里的「回合头/offload/用户输入」
# 与工具名 `journal` 都与存储布局无关（首版正则在三处误报，已收紧）。
g3_hits=$(grep -rnE '"[^"]*(journal|offload)/[^"]*"|messages\.jsonl|checkpoint\.json' \
    --include=*.rs src/ || true)
if [ -n "$g3_hits" ]; then
    note ✗ "G3 存储面：出现服务端存储布局的路径式引用"
    printf '%s\n' "$g3_hits" | sed 's/^/      /' | head -10
    fail=1
else
    note ✓ "G3 存储面：未引用服务端存储布局（journal / checkpoint / offload / messages）"
fi

# ── G4：renderer 状态不进 wire / reducer ───────────────────────────
# spec §11：SessionModel / reducer 不得携带渲染层状态。
g4_hits=$(grep -nE '^\s*use .*(Theme|text::Line|style::Style)|ratatui::' \
    src/app/ringing_v2.rs src/app/timeline_model.rs 2>/dev/null || true)
if [ -n "$g4_hits" ]; then
    note ✗ "G4 分层：reducer / wire model 里出现渲染层类型"
    printf '%s\n' "$g4_hits" | sed 's/^/      /'
    fail=1
else
    note ✓ "G4 分层：reducer / wire model 未引入渲染层类型"
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "静态门禁 FAILED"
    echo "（白名单与理由见本脚本注释；新增豁免请连同理由一起改这里）"
    exit 1
fi
echo "静态门禁 OK"
