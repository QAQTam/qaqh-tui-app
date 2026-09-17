#!/bin/bash
# Linux CI：准备兄弟仓库依赖，然后跑 test + clippy + fmt。
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
# ── 版本策略（待定，见 docs/todo U-30）────────────────────────────────
# 两个 rev **钉死**在下面，保证 CI 可复现。代价是 CI 与本地可能跑在不同版本上
# （本地跟随各自仓的 main）。改动这里必须同步 `Cargo.toml` 里 [patch.crates-io]
# 的注释。ratatui 用的是**上游未发版的 main**（ratatui#2743 修复尚未发版）。
#
# ── 严格模式 ─────────────────────────────────────────────────────────
# `STRICT_DEPS=1`（CI 里由 .cnb.yml 设置）：兄弟仓库目录已存在时**也要校验 rev**，
# 不符就地修正、修不了则失败。这挡住「半成品残留目录让 CI 用错版本」。
# 未设置时（本机开发）：目录已存在就只告警、**不动开发者的工作树**。
#
# ── 可测性 ───────────────────────────────────────────────────────────
# 解析与 prepare() 都是纯函数，可被 `scripts/tests/ci-linux-parse-test.sh` source
# 后直接断言（不联网）。主流程只在脚本被直接执行时运行。

set -euo pipefail

# 与 Cargo.toml 的 path 依赖对应。改动请同步 Cargo.toml 的注释。
QAQH_BACKEND_REPO="${QAQH_BACKEND_REPO:-https://cnb.cool/QAQ-Harness/qaqh-backend.git}"
QAQH_BACKEND_REV="${QAQH_BACKEND_REV:-8c1c1544fd9f9754275970d8aa338287ef20a440}"
RATATUI_REPO="${RATATUI_REPO:-https://github.com/ratatui/ratatui.git}"
RATATUI_REV="${RATATUI_REV:-e02e2a622eda6e4cae105df48a48f641cdba0303}"

# 从 rust-toolchain.toml 解析 `channel`。
#
# 成功 → stdout 打印 channel，返回 0；失败（文件缺失 / 没有 channel 行 / 值为空）
# → 返回非零，**不打印任何东西**。调用方必须处理失败，不要假装有 fallback：
# 上一版用 `grep | head | sed` 直接赋值，`grep` 无匹配时在 `set -e` 下会**静默
# 中止整个脚本**，fallback 分支成了死代码（PR #19 复审指出）。
#
# 解析规则：只认行首（允许前导空白）的 `channel =`；引号可有可无；**忽略行尾注释**。
# 不能用贪婪的 `s/.*"([^"]+)".*/\1/` —— 它会取到**最后一个**引号里的内容，
# 于是 `channel = "1.98.0" # 备用 "v1.98.1"` 会解析成 `v1.98.1`，
# 组件就会装到另一套 toolchain 上，把「误诊」重新引入。
resolve_toolchain() {
    local file="$1" out
    [ -f "$file" ] || return 1
    out=$(sed -nE \
        's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"?([^"#[:space:]]+)"?.*$/\1/p' \
        "$file" | head -1) || true
    [ -n "$out" ] || return 1
    printf '%s\n' "$out"
}

# 确保 $dir 处有 $repo 的 $rev。
# 目录已存在时**始终校验 rev**（这一点与上一版不同：上一版对任何已有目录直接
# return 0，于是 clone 成功但 checkout 失败留下的半成品目录会让 CI 用错版本）。
prepare() {
    local dir="$1" repo="$2" rev="$3" name="$4" got
    if [ -e "$dir" ]; then
        if ! got=$(git -C "$dir" rev-parse HEAD 2>/dev/null); then
            echo "  [$name] 目录已存在但**不是 git 仓库**: $dir" >&2
            [ "${STRICT_DEPS:-0}" = "1" ] || { echo "  [$name] 非严格模式，跳过" >&2; return 0; }
            return 1
        fi
        if [ "$got" = "$rev" ]; then
            echo "  [$name] 已存在且 rev 相符: $got"
            return 0
        fi
        echo "  [$name] ⚠ 已存在但 rev 不符：期望 $rev，实得 $got" >&2
        if [ "${STRICT_DEPS:-0}" != "1" ]; then
            echo "  [$name] 非严格模式（本机开发）：不动你的工作树，继续" >&2
            return 0
        fi
        echo "  [$name] 严格模式：就地 fetch 并 checkout $rev"
        git -C "$dir" fetch --quiet origin || true
        git -C "$dir" checkout --quiet "$rev"
        got=$(git -C "$dir" rev-parse HEAD)
        if [ "$got" != "$rev" ]; then
            echo "  [$name] ✗ 就地修正失败：实得 $got" >&2
            return 1
        fi
        echo "  [$name] ✓ 已修正为 $got"
        return 0
    fi

    echo "  [$name] 拉取 $repo @ $rev"
    git clone --filter=blob:none --no-checkout "$repo" "$dir"
    git -C "$dir" checkout --quiet "$rev"
    got=$(git -C "$dir" rev-parse HEAD)
    if [ "$got" != "$rev" ]; then
        echo "  [$name] ✗ rev 不符：期望 $rev，实得 $got" >&2
        return 1
    fi
    echo "  [$name] ✓ $got"
}

main() {
    local root parent tc_file tc
    root=$(git rev-parse --show-toplevel)
    parent=$(dirname "$root")

    echo "== 依赖准备 =="
    echo "仓库根: $root"
    echo "兄弟仓库父目录: $parent"
    echo "严格模式(STRICT_DEPS): ${STRICT_DEPS:-0}"

    prepare "$parent/qaqh-backend" "$QAQH_BACKEND_REPO" "$QAQH_BACKEND_REV" "qaqh-backend"
    prepare "$parent/ratatui" "$RATATUI_REPO" "$RATATUI_REV" "ratatui"

    # libssl-dev / pkg-config：为可能仍需要系统 TLS 头文件的构建准备（幂等）。
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update -qq
        apt-get install -y --no-install-recommends pkg-config libssl-dev >/dev/null
    fi

    echo
    echo "== 工具链与组件 =="
    # ⚠ 容易误诊的坑：CI 镜像是 `rust:1.98.1`，而本仓 `rust-toolchain.toml` 钉的是
    # 1.98.0。两套 toolchain 在 rustup home 里并存、组件各自独立。cargo 在仓库目录
    # 里运行时会按 rust-toolchain.toml 切到 1.98.0（此时才下载那套），而
    # `rustup component add` 不带 `--toolchain` 时装的是**当前活动**那套。
    # 所以「镜像里缺 fmt」是错的归因——真正会缺组件的是**仓库钉的那套**。
    if command -v rustup >/dev/null 2>&1; then
        # 用绝对路径：`[ -f rust-toolchain.toml ]` 依赖 cwd，而 $root 已在手。
        tc_file="$root/rust-toolchain.toml"
        if tc=$(resolve_toolchain "$tc_file"); then
            echo "  按 rust-toolchain.toml 使用 toolchain: $tc"
            rustup component add --toolchain "$tc" clippy rustfmt
        else
            # 显式失败，不静默中止、也不假装 fallback：解析不出 channel 时
            # 「猜一个」正是上一版出问题的地方。
            echo "✗ 无法从 $tc_file 解析出 channel（文件缺失 / 无 channel 行 / 值为空）" >&2
            echo "  请在 rust-toolchain.toml 写明 channel，或修正本脚本的解析。" >&2
            exit 1
        fi
        echo "  cargo 实际使用: $(rustup show active-toolchain 2>/dev/null || echo '(未知)')"
    fi

    echo
    echo "== cargo test =="
    cargo test --all-targets

    echo
    echo "== cargo clippy =="
    cargo clippy --all-targets -- -D warnings

    echo
    echo "== fmt（仅本仓）=="
    # 注意：**不能**裸跑 `cargo fmt --all` —— 它会连带格式化兄弟仓库里别人的工作树
    # （`../qaqh-backend` 在 HEAD 上本就不是 fmt-clean）。不带 `--all` 只检查本包。
    if ! cargo fmt --version >/dev/null 2>&1; then
        echo "✗ cargo fmt 不可用。当前活动 toolchain：$(rustup show active-toolchain 2>/dev/null || echo '(rustup 不可用)')" >&2
        echo "  若上面那行显示的不是 rust-toolchain.toml 里的版本，说明组件装错了 toolchain。" >&2
        exit 1
    fi
    cargo fmt --check

    echo
    echo "CI OK"
}

# 被 source 时（测试）只暴露函数，不跑主流程。
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
