//! jsonl 载体解析（对位移植桌面端 `compiler/jsonl.ts`，spec §7 的 jsonl 形态）。
//!
//! 每行一个 JSON record，降低到与 tsx DSL 相同的 SurfaceIR，校验共用
//! validate_surface。record 集合：surface.open / state / derive / node /
//! surface.close；表达式与属性值用显式 kinded wire 对象，不接受任何 JS 语法
//! ——不可信文本只走 JSON.parse。派生态（derive）按声明顺序内联（与 tsx
//! 解析器一致，天然无环）。

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::ir::{
    Action, Expr, GenUiError, GenUiResult, IrNode, PropValue, StateDecl, StateType, SurfaceIr,
    ui_err,
};
use super::tsx::{MAX_NODES, op_kind};

const BINARY_OPS: [&str; 10] = ["add", "sub", "mul", "div", "eq", "neq", "gt", "lt", "and", "or"];
const ACTION_KINDS: [&str; 7] = ["set", "reset", "followUp", "copy", "openLink", "append", "remove"];

#[derive(Default)]
struct WireCtx {
    derives: BTreeMap<String, Expr>,
}

fn wire_expr(value: &Value, line: usize, ctx: &WireCtx) -> GenUiResult<Expr> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Ok(Expr::Lit(value.clone())),
        Value::Object(record) => {
            if let Some(lit) = record.get("lit") {
                if lit.is_string() || lit.is_number() || lit.is_boolean() {
                    return Ok(Expr::Lit(lit.clone()));
                }
                return Err(ui_err(line, format!("无法解析表达式:{lit}")));
            }
            if let Some(Value::String(key)) = record.get("state") {
                // 派生态按声明顺序内联；wire 上不出现 derive 引用。
                if let Some(expr) = ctx.derives.get(key) {
                    return Ok(expr.clone());
                }
                return Ok(Expr::State(key.clone()));
            }
            if let Some(Value::String(item)) = record.get("item") {
                return Ok(Expr::Item(item.clone()));
            }
            if let Some(inner) = record.get("not") {
                return Ok(Expr::Not(Box::new(wire_expr(inner, line, ctx)?)));
            }
            if let Some(Value::String(op)) = record.get("op")
                && BINARY_OPS.contains(&op.as_str())
            {
                let left = wire_expr(record.get("left").unwrap_or(&Value::Null), line, ctx)?;
                let right = wire_expr(record.get("right").unwrap_or(&Value::Null), line, ctx)?;
                return Ok(Expr::Bin(op_kind(op), Box::new(left), Box::new(right)));
            }
            Err(ui_err(line, format!("无法解析表达式:{value}")))
        }
        _ => Err(ui_err(line, format!("无法解析表达式:{value}"))),
    }
}

fn wire_action(value: &Value, line: usize, ctx: &WireCtx) -> GenUiResult<Action> {
    let Value::Object(record) = value else {
        return Err(ui_err(line, format!("无法解析动作:{value}")));
    };
    if record.get("reset") == Some(&Value::Bool(true)) {
        return Ok(Action::Reset);
    }
    if let Some(Value::String(key)) = record.get("set") {
        return Ok(Action::Set {
            key: key.clone(),
            value: wire_expr(record.get("value").unwrap_or(&Value::Null), line, ctx)?,
        });
    }
    if record.contains_key("copy") {
        return Ok(Action::Copy {
            value: wire_expr(record.get("copy").unwrap_or(&Value::Null), line, ctx)?,
        });
    }
    if let Some(Value::String(url)) = record.get("openLink") {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(ui_err(line, format!("openLink 只允许 http(s) 链接:{url}")));
        }
        return Ok(Action::OpenLink(url.clone()));
    }
    if let Some(key) = record.get("append").or_else(|| record.get("remove")).and_then(Value::as_str)
    {
        if key.is_empty() {
            return Err(ui_err(line, "append/remove 需要状态 key"));
        }
        let value = wire_expr(record.get("value").unwrap_or(&Value::Null), line, ctx)?;
        return Ok(if record.contains_key("append") {
            Action::Append { key: key.to_string(), value }
        } else {
            Action::Remove { key: key.to_string(), value }
        });
    }
    if let Some(Value::String(prompt)) = record.get("followUp") {
        let fields_raw = record.get("fields");
        if fields_raw.is_some_and(|f| !f.is_array()) {
            return Err(ui_err(line, "followUp fields 必须是数组"));
        }
        let mut fields = vec![];
        for item in fields_raw.and_then(Value::as_array).unwrap_or(&vec![]) {
            let Some(Value::String(label)) = item.get("label") else {
                return Err(ui_err(line, "followUp 字段需要 label/key"));
            };
            let Some(Value::String(key)) = item.get("key") else {
                return Err(ui_err(line, "followUp 字段需要 label/key"));
            };
            if key.is_empty() {
                return Err(ui_err(line, "followUp 字段需要 label/key"));
            }
            fields.push((label.clone(), key.clone()));
        }
        return Ok(Action::FollowUp { prompt: prompt.clone(), fields });
    }
    Err(ui_err(line, format!("无法解析动作:{value}")))
}

fn wire_prop(value: &Value, line: usize, ctx: &WireCtx) -> GenUiResult<PropValue> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Ok(PropValue::Lit(value.clone())),
        Value::Array(items) => {
            if !items.iter().all(Value::is_string) {
                return Err(ui_err(line, "children 数组必须是字符串 id"));
            }
            Ok(PropValue::List(
                items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            ))
        }
        Value::Object(record) => {
            if let Some(expr) = record.get("expr") {
                return Ok(PropValue::Expr(wire_expr(expr, line, ctx)?));
            }
            if let Some(Value::String(key)) = record.get("bind") {
                if key.is_empty() {
                    return Err(ui_err(line, "绑定需要状态 key"));
                }
                return Ok(PropValue::Binding(key.clone()));
            }
            if let (Some(Value::String(source)), Some(Value::String(as_))) =
                (record.get("source"), record.get("as"))
            {
                if source.is_empty() || as_.is_empty() {
                    return Err(ui_err(line, "repeat 需要 source/as"));
                }
                return Ok(PropValue::Repeat(source.clone(), as_.clone()));
            }
            if ACTION_KINDS.iter().any(|kind| record.contains_key(*kind)) {
                return Ok(PropValue::Action(wire_action(value, line, ctx)?));
            }
            // 裸表达式 wire（{state}/{lit}/{item}/{not}/{op:二元}）直接作为 prop 值。
            if record.contains_key("state")
                || record.contains_key("lit")
                || record.contains_key("item")
                || record.contains_key("not")
                || record.get("op").and_then(Value::as_str).is_some_and(|op| BINARY_OPS.contains(&op))
            {
                return Ok(PropValue::Expr(wire_expr(value, line, ctx)?));
            }
            Err(ui_err(line, format!("无法解析属性值:{value}")))
        }
        _ => Err(ui_err(line, format!("无法解析属性值:{value}"))),
    }
}

pub fn parse_surface_jsonl(source: &str, final_: bool) -> GenUiResult<SurfaceIr> {
    let mut states: BTreeMap<String, StateDecl> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut nodes: BTreeMap<String, IrNode> = BTreeMap::new();
    let mut ctx = WireCtx::default();
    let mut surface_id: Option<String> = None;
    let mut title = None;
    let mut fallback = None;
    let mut root_id: Option<String> = None;
    let mut closed = false;

    let lines: Vec<&str> = source.split('\n').collect();
    for (index, raw) in lines.iter().enumerate() {
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        let line_no = index + 1;
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            // 流式容尾：最后一行半截 JSON 等下一帧补齐，不拖累已到齐的 record。
            if !final_ && index == lines.len() - 1 {
                break;
            }
            return Err(ui_err(
                line_no,
                format!("jsonl 行不是合法 JSON:{}", &text[..text.len().min(40)]),
            ));
        };
        let Some(op) = record.get("op").and_then(Value::as_str) else {
            return Err(ui_err(line_no, "jsonl record 缺少 op"));
        };
        if closed {
            return Err(ui_err(line_no, "surface.close 之后不允许再有 record"));
        }
        match op {
            "surface.open" => {
                if surface_id.is_some() {
                    return Err(ui_err(line_no, "重复的 surface.open"));
                }
                let Some(id) =
                    record.get("surface").and_then(Value::as_str).filter(|s| !s.is_empty())
                else {
                    return Err(ui_err(line_no, "surface.open 缺少 surface id"));
                };
                surface_id = Some(id.to_string());
                if let Some(Value::String(root)) = record.get("root") {
                    root_id = Some(root.clone());
                }
                if let Some(Value::String(t)) = record.get("title") {
                    title = Some(t.clone());
                }
                if let Some(Value::String(f)) = record.get("fallback") {
                    fallback = Some(f.clone());
                }
            }
            "state" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "state 必须在 surface.open 之后"));
                }
                let Some(Value::String(key)) = record.get("key") else {
                    return Err(ui_err(line_no, "state 缺少 key"));
                };
                let type_text = record.get("type").and_then(Value::as_str).unwrap_or("");
                let type_ = match type_text {
                    "string" => StateType::Str,
                    "number" => StateType::Num,
                    "integer" => StateType::Integer,
                    "boolean" => StateType::Bool,
                    "list" => StateType::List,
                    _ => return Err(ui_err(line_no, format!("不支持的状态类型 {type_text}"))),
                };
                if states.contains_key(key) {
                    return Err(ui_err(line_no, format!("重复的状态声明 {key}")));
                }
                let initial = record.get("initial").cloned().unwrap_or(Value::Null);
                let ok = match type_ {
                    StateType::List => {
                        initial.as_array().is_some_and(|a| a.iter().all(Value::is_string))
                    }
                    StateType::Str => initial.is_string(),
                    StateType::Bool => initial.is_boolean(),
                    StateType::Num | StateType::Integer => {
                        initial.as_f64().is_some_and(|n| n.is_finite())
                    }
                };
                if !ok {
                    return Err(ui_err(line_no, format!("状态 {key} 初始值与类型不符")));
                }
                if type_ == StateType::Integer
                    && !initial.as_f64().is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= 0.0)
                {
                    return Err(ui_err(line_no, "integer 初始值必须是安全整数"));
                }
                states.insert(key.clone(), StateDecl { type_, initial });
            }
            "derive" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "derive 必须在 surface.open 之后"));
                }
                let Some(Value::String(key)) = record.get("key") else {
                    return Err(ui_err(line_no, "derive 缺少 key"));
                };
                if ctx.derives.contains_key(key) || states.contains_key(key) {
                    return Err(ui_err(line_no, format!("重复的声明 {key}")));
                }
                let expr = wire_expr(record.get("expr").unwrap_or(&Value::Null), line_no, &ctx)?;
                ctx.derives.insert(key.clone(), expr);
            }
            "node" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "node 必须在 surface.open 之后"));
                }
                let Some(Value::String(id)) = record.get("id") else {
                    return Err(ui_err(line_no, "node 缺少 id"));
                };
                let Some(Value::String(component)) = record.get("component") else {
                    return Err(ui_err(line_no, "node 缺少 component"));
                };
                if let Some(props) = record.get("props")
                    && !props.is_object()
                {
                    return Err(ui_err(line_no, "node props 必须是对象"));
                }
                if nodes.contains_key(id) || order.contains(id) {
                    return Err(ui_err(line_no, format!("重复的节点 id {id}")));
                }
                let mut decoded: BTreeMap<String, PropValue> = BTreeMap::new();
                let mut repeat = None;
                for (name, value) in
                    record.get("props").and_then(Value::as_object).unwrap_or(&Map::new())
                {
                    let decoded_value = wire_prop(value, line_no, &ctx)?;
                    if name == "repeat"
                        && let PropValue::Repeat(source, as_) = decoded_value
                    {
                        repeat = Some((source, as_));
                        continue;
                    }
                    decoded.insert(name.clone(), decoded_value);
                }
                nodes.insert(
                    id.clone(),
                    IrNode { id: id.clone(), type_: component.clone(), props: decoded, line: line_no, repeat },
                );
                order.push(id.clone());
            }
            "surface.close" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "surface.close 必须在 surface.open 之后"));
                }
                closed = true;
            }
            other => return Err(ui_err(line_no, format!("未知的 jsonl op:{other}"))),
        }
    }
    let Some(surface_id) = surface_id else {
        return Err(GenUiError { message: "jsonl 缺少 surface.open".into(), line: None });
    };
    // final=false（流式部分编译）：允许缺 surface.close，先渲染已到齐的 record。
    if !closed && final_ {
        return Err(GenUiError { message: "jsonl 缺少 surface.close".into(), line: None });
    }
    if order.len() > MAX_NODES {
        return Err(GenUiError { message: format!("节点数超过预算（{MAX_NODES}）"), line: None });
    }
    Ok(SurfaceIr { surface_id, title, fallback, root_id, states, order, nodes, warnings: vec![] })
}
