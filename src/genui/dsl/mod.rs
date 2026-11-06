//! qaqh-ui 围栏 → SurfaceIR → A2UI v1.0 wire 的编译链。
//!
//! 对位移植桌面端 `source/decoder.ts` + `compiler/{ir,parse,jsonl}.ts` +
//! `v2/translate.ts`。子模块划分：
//!
//! - [`decoder`]：assistant 正文分帧——只有顶层 ` ```qaqh-ui v=0.1|0.2
//!   format=tsx|jsonl ` 围栏有 UI 语义，普通代码围栏/引用块不触发；流式 holdback。
//! - [`ir`]：封闭组件目录 + 编译期校验（目录外属性拒绝、绑定类型、环/深度/预算）。
//! - [`tsx`]：受限 DSL 解析器——「解析而非执行」，`ui.*` 是编译 intrinsics，
//!   不调用任何 JavaScript；未知语句/组件/状态具名报错。
//! - [`jsonl`]：程序化载体（每行一个 record；「工具直接吐 UI 优先走 jsonl」）。
//! - [`translate`]：IR → A2UI v1.0 消息（`/s/` 状态域、`qaqh:arith|logic.*`
//!   表达式降级、`qaqh.action.*` 事件、`qaqh.dev/repeat` 扩展、root 包装根）。
//!
//! 设计要点（对齐桌面端 docs/genui-v02-a2ui-migration 的语义）：
//! - 状态域扁平 key → dataModel 路径 `/s/<key>`（命名空间保留给层级模型）；
//! - 本地动作全部降级为 `qaqh.action.*` 命名事件，renderer 内拦截，不进 agent 通道；
//! - seal（整帧）走 createSurface 一次性带全组件；流式（每帧）走
//!   updateComponents 全量 + updateDataModel 整体替换 `/s/`。

pub mod decoder;
pub mod ir;
pub mod jsonl;
pub mod translate;
pub mod tsx;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use crate::genui::store::Registry;
use decoder::{ContentPart, UiFormat};
use ir::SurfaceIr;

/// `\\n`/`\\t`/`\\x` 反转义（字符串字面量 → 实际值）。
pub(super) fn unescape(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 源码 → 校验过的 SurfaceIR（parse → normalize → validate → 软告警）。
/// `final_ = false` 为流式部分编译：允许缺 ui.end/surface.close、children
/// 前向引用剪枝——已到齐的先渲染，seal 后整帧转正。
pub fn compile_source(source: &str, format: UiFormat, final_: bool) -> ir::GenUiResult<SurfaceIr> {
    let mut ir = match format {
        UiFormat::Jsonl => jsonl::parse_surface_jsonl(source, final_)?,
        UiFormat::Tsx => tsx::parse_surface(source, final_)?,
    };
    ir::normalize_bindings(&mut ir);
    ir::validate_surface(&mut ir, !final_)?;
    // 软告警只在 seal（整帧转正）后算：流式途中节点还没挂上容器属于正常中间态。
    if final_ {
        ir.warnings = ir::surface_warnings(&ir);
    }
    Ok(ir)
}

/// 把一条 [`ContentPart::Ui`] 喂进 registry：首帧 create（对齐桌面端
/// SurfaceView2 的 `!created → translateCreate`），后续帧 update。
/// 解析失败返回具名错误（调用方降级占位卡），不静默吞。
pub fn ingest_ui_part(
    registry: &mut Registry,
    part: &ContentPart,
    created: &mut BTreeMap<String, ()>,
) -> ir::GenUiResult<SurfaceIr> {
    let ContentPart::Ui { format, source, sealed, .. } = part else {
        return Err(ir::GenUiError { message: "不是 UI 分帧".into(), line: None });
    };
    let ir = compile_source(source, *format, *sealed)?;
    if !created.contains_key(&ir.surface_id) {
        registry
            .apply(translate::translate_create(&ir))
            .map_err(|e| ir::GenUiError { message: e.to_string(), line: None })?;
    } else {
        for msg in translate::translate_update(&ir) {
            registry
                .apply(msg)
                .map_err(|e| ir::GenUiError { message: e.to_string(), line: None })?;
        }
    }
    created.insert(ir.surface_id.clone(), ());
    Ok(ir)
}
