//! v0.1 SurfaceIR → A2UI v1.0 消息序列的编译后端（对位移植桌面端
//! `v2/translate.ts`）。TSX/JSONL 围栏保留为 authoring 语法糖：parser 产出
//! SurfaceIR，本模块把它翻译成 agent_to_renderer 消息，renderer 只认消息。
//!
//! 设计要点：
//! - 状态域扁平 key → dataModel 路径 `/s/<key>`（命名空间保留给层级模型）；
//! - v0.1 表达式 → `qaqh:arith/logic.*` 函数调用（无表达式语言进 wire）；
//! - 本地动作 → `qaqh.action.*` 命名事件，renderer 内拦截，不进 agent 通道；
//! - repeat → 组件 metadata.extensions["qaqh.dev/repeat"]（A2UI 扩展槽是合法
//!   wire），渲染层按 dataModel 列表展开模板；
//! - root 保障：A2UI 规范根 id 必须是 "root"——v0.1 的 rootId（或森林）包一层
//!   Stack 包装根。⚠ 与桌面端一致：包装根只包 `rootId ?? forestRoots[0]` 一个
//!   组件，多顶层 surface 的其余节点成为孤儿（软告警）——模型必须显式嵌 Stack。
//!   这是刻意 bug-compatible（见 genui/mod.rs），修复时两边同步。

use serde_json::{json, Map, Value};

use crate::genui::wire::{
    ActionEvent, ChildList, EventSpec, Message, Metadata, UpdateComponents, UpdateDataModel,
    WireComponent, A2UI_VERSION,
};

use super::ir::{forest_roots, Action, BinOp, Expr, IrNode, PropValue, SurfaceIr};

pub const V2_CATALOG_ID: &str = "qaqh.dev:standard-v2";
/// metadata.extensions 里携带 repeat 模板信息的扩展键（vendor 命名空间）。
pub const REPEAT_EXTENSION: &str = "qaqh.dev/repeat";

/// v0.1 状态引用 → dataModel 绝对路径。
fn state_path(key: &str) -> String {
    format!("/s/{key}")
}

pub(super) fn lower_expr(expr: &Expr) -> Value {
    match expr {
        Expr::Lit(v) => v.clone(),
        Expr::State(key) => json!({ "@path": state_path(key) }),
        // 模板项本身：list 态是 string[]，项即字符串 → 相对空路径。
        Expr::Item(_) => json!({ "@path": "" }),
        Expr::Not(value) => {
            json!({ "@call": "qaqh:logic.not", "args": { "value": lower_expr(value) } })
        }
        Expr::Bin(op, left, right) => {
            let name = match op {
                BinOp::Add => "qaqh:arith.add",
                BinOp::Sub => "qaqh:arith.sub",
                BinOp::Mul => "qaqh:arith.mul",
                BinOp::Div => "qaqh:arith.div",
                BinOp::Eq => "qaqh:logic.eq",
                BinOp::Neq => "qaqh:logic.neq",
                // ⚠ bug-compatible：桌面端 translate.ts 的 kind 映射把 >=/<=
                // 压成严格 gt/lt（等号丢失）。修复时两边同步 + 契约测试更新。
                BinOp::Gt => "qaqh:logic.gt",
                BinOp::Lt => "qaqh:logic.lt",
                BinOp::And => "qaqh:logic.and",
                BinOp::Or => "qaqh:logic.or",
            };
            json!({ "@call": name, "args": { "left": lower_expr(left), "right": lower_expr(right) } })
        }
    }
}

/// 动作降级：全部变成命名事件，renderer 本地拦截（`qaqh.action.` 前缀），
/// 语义与 v0.1 Action 一致——followUp 仍走会话输入队列，不产生 toolcall。
pub(super) fn lower_action(action: &Action) -> Value {
    match action {
        Action::Set { key, value } => {
            json!({ "name": "qaqh.action.set", "context": { "key": key, "value": lower_expr(value) } })
        }
        Action::Reset => json!({ "name": "qaqh.action.reset", "context": {} }),
        Action::Append { key, value } => {
            json!({ "name": "qaqh.action.append", "context": { "key": key, "value": lower_expr(value) } })
        }
        Action::Remove { key, value } => {
            json!({ "name": "qaqh.action.remove", "context": { "key": key, "value": lower_expr(value) } })
        }
        Action::Copy { value } => {
            json!({ "name": "qaqh.action.copy", "context": { "value": lower_expr(value) } })
        }
        Action::OpenLink(url) => json!({ "name": "qaqh.action.openLink", "context": { "url": url } }),
        Action::FollowUp { prompt, fields } => json!({
            "name": "qaqh.action.followUp",
            "context": {
                "prompt": prompt,
                "fields": fields
                    .iter()
                    .map(|(label, key)| json!({ "label": label, "key": key }))
                    .collect::<Vec<_>>(),
            }
        }),
    }
}

fn lower_prop(prop: &PropValue) -> Value {
    match prop {
        PropValue::Lit(v) => v.clone(),
        PropValue::Expr(e) => lower_expr(e),
        PropValue::Action(a) => lower_action(a),
        // 非 children 的 list 属性（目录里不存在）按字面量数组降级。
        PropValue::List(items) => {
            Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
        }
        PropValue::Binding(key) => json!({ "@path": state_path(key) }),
        PropValue::Repeat(..) => unreachable!("repeat 在 parse 阶段已抽离"),
    }
}

fn to_wire_component(node: &IrNode) -> WireComponent {
    let mut props: Map<String, Value> = Map::new();
    let mut children: Option<Vec<String>> = None;
    let mut on_press = None;
    let mut show = None;
    for (name, prop) in &node.props {
        if name == "children" {
            if let PropValue::List(items) = prop {
                children = Some(items.clone());
            }
            continue; // 容器关系走 child/children 信封，不走属性表
        }
        if name == "onPress" {
            if let PropValue::Action(a) = prop {
                let event = lower_action(a);
                on_press = Some(ActionEvent {
                    event: EventSpec {
                        name: event["name"].as_str().unwrap_or("").to_string(),
                        user_message: None,
                        context: event.get("context").and_then(Value::as_object).cloned(),
                    },
                });
            }
            continue;
        }
        let lowered = lower_prop(prop);
        if name == "show" {
            show = Some(lowered);
        } else {
            props.insert(name.clone(), lowered);
        }
    }
    let metadata = node.repeat.as_ref().map(|(source, as_)| {
        let mut extensions = Map::new();
        extensions
            .insert(REPEAT_EXTENSION.to_string(), json!({ "path": state_path(source), "as": as_ }));
        Metadata { extensions: Some(extensions) }
    });
    WireComponent {
        id: node.id.clone(),
        component: node.type_.clone(),
        child: None,
        children: children.map(ChildList::Static),
        on_press,
        show,
        metadata,
        props,
    }
}

/// 组装规范根：A2UI 要求 id="root" 的组件存在（包装根，见模块注释的孤儿语义）。
pub fn translate_components(ir: &SurfaceIr) -> Vec<WireComponent> {
    let mut components: Vec<WireComponent> =
        ir.order.iter().map(|id| to_wire_component(&ir.nodes[id])).collect();
    let inner_root = ir
        .root_id
        .clone()
        .or_else(|| forest_roots(ir).first().cloned())
        .unwrap_or_default();
    components.push(WireComponent {
        id: "root".to_string(),
        component: "Stack".to_string(),
        child: None,
        children: if inner_root.is_empty() {
            None
        } else {
            Some(ChildList::Static(vec![inner_root]))
        },
        on_press: None,
        show: None,
        metadata: None,
        props: Map::new(),
    });
    components
}

fn initial_states(ir: &SurfaceIr) -> Map<String, Value> {
    ir.states.iter().map(|(k, v)| (k.clone(), v.initial.clone())).collect()
}

/// seal（整帧）：createSurface 一次性带全组件（含包装根）与初始 dataModel。
pub fn translate_create(ir: &SurfaceIr) -> Message {
    Message {
        version: A2UI_VERSION.to_string(),
        create_surface: Some(crate::genui::wire::CreateSurface {
            surface_id: ir.surface_id.clone(),
            catalog_id: Some(V2_CATALOG_ID.to_string()),
            send_data_model: false,
            components: translate_components(ir),
            data_model: Map::from_iter([("s".to_string(), Value::Object(initial_states(ir)))]),
        }),
        update_components: None,
        update_data_model: None,
        delete_surface: None,
        call_renderer_function: None,
        agent_function_response: None,
    }
}

/// 流式（每帧）：updateComponents 全量 + updateDataModel 整体替换 /s/。
pub fn translate_update(ir: &SurfaceIr) -> Vec<Message> {
    vec![
        Message {
            version: A2UI_VERSION.to_string(),
            create_surface: None,
            update_components: Some(UpdateComponents {
                surface_id: ir.surface_id.clone(),
                components: translate_components(ir),
            }),
            update_data_model: None,
            delete_surface: None,
            call_renderer_function: None,
            agent_function_response: None,
        },
        Message {
            version: A2UI_VERSION.to_string(),
            create_surface: None,
            update_components: None,
            update_data_model: Some(UpdateDataModel {
                surface_id: ir.surface_id.clone(),
                path: Some("/s".to_string()),
                value: Value::Object(initial_states(ir)),
            }),
            delete_surface: None,
            call_renderer_function: None,
            agent_function_response: None,
        },
    ]
}
