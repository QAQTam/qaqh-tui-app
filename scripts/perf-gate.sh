#!/bin/bash
# M6.4 性能门禁（plan §5.2 V2-M6.4「首帧/流式/长会话/commit 基准」）。
#
# 两层：
#   1. **硬门禁**（默认）：跑 `perf_gate_v2_runtime`，钉的是**结构性性质**
#      （数量级），不是绝对耗时——见 `src/app/render/bench.rs` 的判据表。
#      已接进 `scripts/ci-linux.sh`，退化会直接把 CI 打红。
#   2. **基线记录**（`--full`）：额外跑两个信息性基准（commit runtime /
#      规模曲线），把原始输出存成产物，供人看趋势。**不做硬阈值**——
#      绝对耗时会随 CPU powersave/boost 与机器差异波动（M6.4 报告 §0）。
#
# 用法：
#   scripts/perf-gate.sh            # 硬门禁（CI 用）
#   scripts/perf-gate.sh --full     # 硬门禁 + 完整基准记录
#   OUT=/tmp/x.txt scripts/perf-gate.sh --full
#
# 为什么必须 `--test-threads=1`：内存判据走全局分配器计数，`cargo test` 默认
# 并行会让其它测试的分配污染 `live()`。这个参数不是可选的。

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT" || exit 1

OUT=${OUT:-/tmp/qaqh-perf-baseline.txt}
full=0
for arg in "$@"; do
    case "$arg" in
        --full) full=1 ;;
        -h | --help)
            sed -n '2,20p' "$0"
            exit 0
            ;;
        *)
            echo "未知参数：$arg（可用：--full）" >&2
            exit 2
            ;;
    esac
done

echo "== M6.4 性能门禁（单线程）=="
if ! cargo test --bin qaqh-tui perf_gate_v2_runtime -- \
    --ignored --nocapture --test-threads=1; then
    echo "✗ 性能门禁未通过——上面那条断言就是退化点。" >&2
    exit 1
fi
echo "✓ 性能门禁通过"

if [ "$full" -eq 1 ]; then
    echo
    echo "== 基线记录（信息性，不做阈值）→ $OUT =="
    : >"$OUT"
    for bench in \
        bench_v2_commit_runtime \
        bench_v2_runtime_scale_curve \
        bench_long_context_memory_curve \
        bench_streaming_and_multitool; do
        echo "--- $bench" | tee -a "$OUT"
        cargo test --bin qaqh-tui "render::bench::$bench" -- \
            --ignored --nocapture --test-threads=1 2>&1 | tee -a "$OUT" |
            grep -vE "^(   Compiling|    Finished|     Running|running [0-9]+ test|test result:|test app::|$)" || true
    done
    echo "已写入 $OUT"
fi
