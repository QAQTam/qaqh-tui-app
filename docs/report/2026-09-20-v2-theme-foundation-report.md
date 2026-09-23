# QAQH TUI v2 主题底座报告（V2-M2）

> 状态：**已完成**
> 日期：2026-09-20
> 上游计划：[`2026-09-20-v2视觉与交互重构-plan.md`](../plan/2026-09-20-v2视觉与交互重构-plan.md)
> 关联规范：[`2026-09-20-v2视觉token-spec.md`](../spec/2026-09-20-v2视觉token-spec.md)

---

## 0. 结论

V2-M2 主题底座完成：

- 新增 `src/theme/`，冻结 surface / text / accent / semantic / chrome / diff /
  markdown / glyph / spacing / border / modifier token；
- 实现 QAQH Night、QAQH Day、Terminal、Auto 四套入口；
- 实现 truecolor → 256 → 16 → NO_COLOR 确定性降级；
- `Theme::current()` 启动期解析并缓存，绘制路径不读环境变量、不重复量化；
- inline 原型已改为只消费语义 token，`src/terminal/` 内不再出现 `Color::*`；
- 补齐安全、可访问性、量化边界、缓存和大输入边界测试。

---

## 1. 模块与职责

```text
src/theme/
  mod.rs            token 结构、ThemeKind、Theme::current、快照/对比度/缓存锁
  color_support.rs  环境探测、truecolor/256/16/NO_COLOR 量化
  night.rs          QAQH Night 显式 fallback
  day.rs            QAQH Day 派生 fallback
  terminal.rs       Terminal 原生主题（Reset surface + named ANSI）
  markdown.rs       markdown token 的三套主题映射
```

主题选择：

```text
QAQH_THEME=night|day|terminal|auto
```

默认 `night`。`auto` 读取 `COLORFGBG` 的背景分量，无法判断时回退深色；
`NO_COLOR` 存在或 `TERM=dumb` 时进入 `NoColor` 档。

---

## 2. 颜色降级与终端行为

| 档位 | 输出约束 |
|---|---|
| TrueColor | RGB token |
| Ansi256 | `Indexed` fallback；Night 使用规范表，Day 使用确定性最近色 |
| Ansi16 | named ANSI fallback；Night 使用规范表，Day 使用确定性最近色 |
| NO_COLOR | 所有前景/背景 token 为 `Reset`，保留 glyph 与修饰符 |

Terminal 主题不走品牌 surface：

- `surface.* = Reset`；
- `text.primary = Reset`；
- accent 使用 ANSI 16 命名色；
- selection 由 `Modifier::REVERSED` 表达；
- 不修改终端光标颜色。

---

## 3. 安全、性能与可访问性补强

### 安全边界

- `NO_COLOR` 下遍历全部 token，禁止残留任何 RGB / Indexed / named 颜色；
- Ansi16 量化遍历 0..=255 全部 indexed 色，禁止 RGB / Indexed 泄漏；
- inline live buffer 增加 64 KiB 上限，异常输入不会无限增长；
- `ThemeKind` 对未知值显式回退 Night，不 panic。

### 性能边界

- `Theme::current()` 返回 `&'static Theme`，用 `OnceLock` 缓存；
- `Theme` 为 `Copy` 且尺寸上限测试为 512 字节，防止引入堆状态；
- `quantize` 为无分配纯函数，并验证各档幂等；
- inline resize 覆盖连续多尺寸重绘，避免每次 resize 重新建立终端状态。

### 可访问性

- Night / Day 正文与 base surface 对比度测试要求 ≥ 7:1；
- 16 色仍保留语义 fallback；
- 状态表达继续要求 glyph / 文案与颜色并存。

---

## 4. 测试

新增测试覆盖：

```text
theme::color_support::tests::*                 7
theme::markdown::tests::*                      1
theme::tests::*                               11
terminal::inline::tests::*                     5（M1 3 + M2 2）
```

重点用例：

- `quantize_truecolor_to_ansi256_never_keeps_rgb`
- `quantize_truecolor_to_ansi16_never_keeps_rgb_or_indexed`
- `every_indexed_color_quantizes_to_ansi16_without_rgb_leak`
- `no_color_never_leaks_palette_colors`
- `quantization_is_idempotent_for_every_supported_palette`
- `night_theme_has_semantic_contrast`
- `day_theme_has_semantic_contrast`
- `terminal_theme_uses_reset_surfaces_and_reversed_selection`
- `theme_snapshots_are_stable`
- `theme_is_copy_and_bounded`
- `current_theme_is_cached`
- `live_input_is_bounded`
- `inline_viewport_survives_repeated_resize`

---

## 5. 验证结果

```text
cargo fmt --check                            # 通过
cargo clippy --all-targets -- -D warnings    # 通过
cargo test --all-targets                     # 256 passed / 0 failed / 6 ignored
scripts/tests/ci-linux-parse-test.sh         # 15 passed / 0 failed
```

组件颜色纪律检查：

```bash
rg 'Color::' src/terminal
# 无输出
```

`Color::*` 仅允许存在于 `src/theme/`。

---

## 6. 后端边界

M2 不修改后端契约，也不新增 wire 字段。视觉切片继续使用冻结锚点：

```text
qaqh-backend @ ffa6d84e838ced425d1b12b2ae896c824d512c56
```

本地后端 HEAD 已前进到 `da848ee`，但本切片不追新字段；后续仍按批次统一适配，
并同步更新 CI pin 与锚点文档。

---

## 7. 下一步

V2-M3：Transcript v2。

- User / Assistant / Thinking / Tool 四类块；
- 消费 `Theme::current()`，禁止组件硬编码颜色；
- 覆盖 CJK、宽字符、折叠、diff、工具状态；
- 复用 commit ledger，保证重放 / resize 不重复 emit。
