//! QAQH UI IR（对位移植桌面端 `compiler/ir.ts`）：类型区分联合、封闭 props
//! （目录外属性一律拒绝）、纯表达式与受控动作。
//!
//! `show`（条件渲染）与 `children` 一样是所有节点可用的通用属性。
//! `string`/`number`/`boolean` 是**标量槽位**：字面量与纯表达式都收（协议对模型
//! 的描述是「属性值是字符串字面量或 {表达式}」）；`expr` 槽位反过来同样收字面量。
//! 两向放宽只影响取值形状，不放松目录封闭性——表达式仍走 check_expr，只读
//! Surface local 状态域。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

// 预算常量单一来源在 store（渲染闸门与编译闸门同值同源）。
use crate::genui::store::{MAX_NODES, MAX_TREE_DEPTH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    Str,
    Num,
    Bool,
    Expr,
    Action,
    List,
    Binding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateType {
    Str,
    Num,
    Integer,
    Bool,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Gt,
    Lt,
    And,
    Or,
}

/// 纯表达式：字面量、状态读取、循环变量、二元算术/拼接/比较、逻辑运算。
/// 不做 JS 隐式转换（spec §8.3）；add 在字符串参与时降级为拼接。
#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    State(String),
    Item(String),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

/// local 动作：只在用户事件事务中求值。followUp 把选定字段拼成聊天输入，
/// 按现有会话输入语义排队（不上传完整私有状态）；copy/openLink/append/remove
/// 是低风险外围动作，只触碰 Surface local 状态域。
#[derive(Debug, Clone)]
pub enum Action {
    Set { key: String, value: Expr },
    Reset,
    FollowUp { prompt: String, fields: Vec<(String, String)> },
    Copy { value: Expr },
    OpenLink(String),
    Append { key: String, value: Expr },
    Remove { key: String, value: Expr },
}

#[derive(Debug, Clone)]
pub enum PropValue {
    Lit(Value),
    Expr(Expr),
    Action(Action),
    List(Vec<String>),
    Binding(String),
    /// for/repeat：source 是 list 态 key，as_ 是循环变量名。
    Repeat(String, String),
}

#[derive(Debug, Clone)]
pub struct IrNode {
    pub id: String,
    pub type_: String,
    pub props: BTreeMap<String, PropValue>,
    /// 解析时的源码行号，只用于错误定位，不进 wire IR。
    pub line: usize,
    /// for/repeat：本节点（含子树）按 list 态逐项渲染模板。
    pub repeat: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct StateDecl {
    pub type_: StateType,
    pub initial: Value,
}

#[derive(Debug, Clone, Default)]
pub struct SurfaceIr {
    pub surface_id: String,
    pub title: Option<String>,
    pub fallback: Option<String>,
    pub root_id: Option<String>,
    pub states: BTreeMap<String, StateDecl>,
    /// 插入顺序；nodes 是 id → 节点映射。
    pub order: Vec<String>,
    pub nodes: BTreeMap<String, IrNode>,
    /// 编译期软告警（不阻断渲染）：目前只有「孤儿节点」。
    pub warnings: Vec<String>,
}

/// 带行号的具名错误：渲染层直接展示，不静默吞。
#[derive(Debug, Clone)]
pub struct GenUiError {
    pub message: String,
    pub line: Option<usize>,
}

impl std::fmt::Display for GenUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}（第 {line} 行）", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

pub type GenUiResult<T> = Result<T, GenUiError>;

pub(crate) fn ui_err(line: usize, message: impl Into<String>) -> GenUiError {
    GenUiError { message: message.into(), line: Some(line) }
}

/// 受信组件目录（spec §5.1/§8.1）：封闭，未知组件/属性一律拒绝。
pub const CATALOG: &[&str] = &[
    "Card", "Stack", "Row", "Title", "Text", "Metric", "Badge", "Button", "Divider", "TextInput",
    "Checkbox", "TodoList", "Todo", "TagList",
];

pub fn prop_kind(component: &str, name: &str) -> Option<PropKind> {
    use PropKind::*;
    if name == "show" {
        return Some(Expr);
    }
    Some(match (component, name) {
        ("Card", "title") | ("TodoList", "title") => Str,
        ("Card", "children")
        | ("Stack", "children")
        | ("Row", "children")
        | ("TodoList", "children") => List,
        ("Title", "text") | ("Text", "text") | ("Badge", "text") | ("Todo", "text") => Expr,
        ("Metric", "label") | ("TextInput", "label") | ("Checkbox", "label") => Str,
        ("Metric", "value") | ("Todo", "meta") => Expr,
        ("Button", "label") => Str,
        ("Button", "onPress") => Action,
        ("Button", "disabled") => Expr,
        ("TextInput", "value")
        | ("Checkbox", "checked")
        | ("Todo", "done")
        | ("TagList", "items") => Binding,
        ("TextInput", "placeholder") | ("TextInput", "pattern") => Str,
        ("TextInput", "required") => Expr,
        ("TextInput", "maxLength") => Num,
        _ => return None,
    })
}

/// binding 属性要求的状态类型。
fn binding_type(component: &str, name: &str) -> Option<StateType> {
    match (component, name) {
        ("TextInput", "value") => Some(StateType::Str),
        ("Checkbox", "checked") | ("Todo", "done") => Some(StateType::Bool),
        ("TagList", "items") => Some(StateType::List),
        _ => None,
    }
}

/// 无 root 时的平铺集合：只返回「森林顶层」节点——没有被任何 children 引用的
/// 节点。直接按 order 平铺会把已随父节点渲染过的子树再画一遍。
pub fn forest_roots(ir: &SurfaceIr) -> Vec<String> {
    let mut nested = BTreeSet::new();
    for id in &ir.order {
        if let Some(PropValue::List(items)) = ir.nodes[id].props.get("children") {
            nested.extend(items.iter().cloned());
        }
    }
    ir.order.iter().filter(|id| !nested.contains(*id)).cloned().collect()
}

/// 裸 `expr{state}` 简写 → binding（TextInput.value={name} 等价 ui.bind）。
pub fn normalize_bindings(ir: &mut SurfaceIr) {
    for id in ir.order.clone() {
        let Some(node) = ir.nodes.get_mut(&id) else { continue };
        let is_binding_slot = |name: &str| prop_kind(&node.type_, name) == Some(PropKind::Binding);
        for (name, value) in node.props.iter_mut() {
            if !is_binding_slot(name) {
                continue;
            }
            if let PropValue::Expr(Expr::State(key)) = value {
                *value = PropValue::Binding(key.clone());
            }
        }
    }
}

/// 编译期软告警（不阻断渲染）：**孤儿节点**——有 root 时渲染器只从根往下走，
/// 「既不是 root、也没有被任何 children 引用」的节点永远不会出现在屏幕上。
/// 只报警不报错：漏挂的节点可能是流式中间态，阻断渲染的代价更大。
pub fn surface_warnings(ir: &SurfaceIr) -> Vec<String> {
    // 无 root（或 root 未到齐）时渲染器平铺森林顶层，不存在孤儿。
    let Some(root) = &ir.root_id else { return vec![] };
    if !ir.nodes.contains_key(root) {
        return vec![];
    }
    let mut referenced = BTreeSet::new();
    for id in &ir.order {
        if let Some(PropValue::List(items)) = ir.nodes[id].props.get("children") {
            referenced.extend(items.iter().cloned());
        }
    }
    let orphans: Vec<&String> =
        ir.order.iter().filter(|id| *id != root && !referenced.contains(*id)).collect();
    if orphans.is_empty() {
        return vec![];
    }
    let head: Vec<&str> = orphans.iter().take(8).map(|s| s.as_str()).collect();
    let tail = if orphans.len() > 8 { format!(" 等 {} 个", orphans.len()) } else { String::new() };
    vec![format!(
        "节点 {}{tail} 未被任何 children 引用，不会渲染；补进容器的 children 或删掉声明",
        head.join("、")
    )]
}

fn check_expr(expr: &Expr, ir: &SurfaceIr, line: usize) -> GenUiResult<()> {
    match expr {
        Expr::Lit(_) => Ok(()),
        Expr::State(key) => {
            if ir.states.contains_key(key) {
                Ok(())
            } else {
                Err(ui_err(line, format!("表达式引用未知状态 {key}")))
            }
        }
        Expr::Item(_) => Ok(()), // 循环变量：作用域在渲染期检查
        Expr::Not(v) => check_expr(v, ir, line),
        Expr::Bin(_, l, r) => {
            check_expr(l, ir, line)?;
            check_expr(r, ir, line)
        }
    }
}

/// 编译期校验：目录封闭性、引用完整性、拓扑（环/深度）。
/// `partial`（流式部分编译）：children 前向引用先剪枝、root 未到齐先平铺。
pub fn validate_surface(ir: &mut SurfaceIr, partial: bool) -> GenUiResult<()> {
    if ir.surface_id.is_empty() {
        return Err(GenUiError { message: "surface 缺少 id".into(), line: None });
    }
    if ir.order.len() > MAX_NODES {
        return Err(GenUiError { message: format!("节点数超过预算（{MAX_NODES}）"), line: None });
    }
    let node_ids: BTreeSet<&String> = ir.order.iter().collect();
    for id in ir.order.clone() {
        let node = ir.nodes[&id].clone();
        if !CATALOG.contains(&node.type_.as_str()) {
            return Err(ui_err(node.line, format!("未知组件类型:{}", node.type_)));
        }
        for (name, value) in &node.props {
            let Some(kind) = prop_kind(&node.type_, name) else {
                return Err(ui_err(node.line, format!("{} 不支持属性 {}", node.type_, name)));
            };
            let fail = || ui_err(node.line, format!("属性 {name} 需要 {kind:?}"));
            match (kind, value) {
                (PropKind::Expr, PropValue::Expr(e)) => check_expr(e, ir, node.line)?,
                // expr 槽位同样收字面量（TS：value.kind !== "lit" 才 fail）。
                (PropKind::Expr, PropValue::Lit(_)) => {}
                (PropKind::Expr, _) => return Err(fail()),
                (PropKind::Str | PropKind::Num | PropKind::Bool, PropValue::Lit(v)) => {
                    // 对齐 TS typeof 语义：整数与浮点都是 number。
                    let ok = matches!(
                        (kind, v),
                        (PropKind::Str, Value::String(_))
                            | (PropKind::Num, Value::Number(_))
                            | (PropKind::Bool, Value::Bool(_))
                    );
                    if !ok {
                        return Err(fail());
                    }
                }
                (PropKind::Str | PropKind::Num | PropKind::Bool, PropValue::Expr(e)) => {
                    check_expr(e, ir, node.line)?;
                    if let Expr::Lit(v) = e {
                        let ok = matches!(
                            (kind, v),
                            (PropKind::Str, Value::String(_))
                                | (PropKind::Num, Value::Number(_))
                                | (PropKind::Bool, Value::Bool(_))
                        );
                        if !ok {
                            return Err(fail());
                        }
                    }
                }
                (PropKind::Str | PropKind::Num | PropKind::Bool, _) => return Err(fail()),
                (PropKind::Action, PropValue::Action(action)) => {
                    let has = |key: &str| ir.states.contains_key(key);
                    match action {
                        Action::Set { key, value } => {
                            if !has(key) {
                                return Err(ui_err(node.line, format!("动作引用未知状态 {key}")));
                            }
                            check_expr(value, ir, node.line)?;
                        }
                        Action::FollowUp { fields, .. } => {
                            for (label, key) in fields {
                                if !has(key) {
                                    return Err(ui_err(
                                        node.line,
                                        format!("followUp 字段 {label} 引用未知状态 {key}"),
                                    ));
                                }
                            }
                        }
                        Action::Append { key, value } | Action::Remove { key, value } => {
                            let Some(state) = ir.states.get(key) else {
                                return Err(ui_err(node.line, format!("动作引用未知状态 {key}")));
                            };
                            if state.type_ != StateType::List {
                                return Err(ui_err(
                                    node.line,
                                    format!("append/remove 需要 list 型状态 {key}"),
                                ));
                            }
                            check_expr(value, ir, node.line)?;
                        }
                        Action::Copy { value } => check_expr(value, ir, node.line)?,
                        Action::Reset | Action::OpenLink(_) => {}
                    }
                }
                (PropKind::Action, _) => return Err(fail()),
                (PropKind::List, PropValue::List(items)) => {
                    if partial {
                        // 流式：children 前向引用未到齐先剪掉，等下一帧补齐。
                        let node = ir.nodes.get_mut(&id).expect("order 与 nodes 同步");
                        if let Some(PropValue::List(items)) = node.props.get_mut("children") {
                            items.retain(|child| node_ids.contains(child));
                        }
                    } else {
                        for child in items {
                            if !node_ids.contains(child) {
                                return Err(ui_err(
                                    node.line,
                                    format!("children 引用未知节点 {child}"),
                                ));
                            }
                        }
                    }
                }
                (PropKind::List, _) => return Err(fail()),
                (PropKind::Binding, PropValue::Binding(key)) => {
                    let Some(state) = ir.states.get(key) else {
                        return Err(ui_err(node.line, format!("绑定引用未知状态 {key}")));
                    };
                    if let Some(expected) = binding_type(&node.type_, name)
                        && state.type_ != expected
                    {
                        return Err(ui_err(
                            node.line,
                            format!("绑定 {key} 需要 {expected:?} 型状态"),
                        ));
                    }
                }
                (PropKind::Binding, _) => {
                    return Err(ui_err(
                        node.line,
                        format!("属性 {name} 需要 binding（用 ui.bind(\"key\") 或直接写 {{状态名}}）"),
                    ));
                }
            }
        }
    }

    if let Some(root) = ir.root_id.clone() {
        if node_ids.contains(&root) {
            // ok
        } else if partial {
            ir.root_id = None; // 流式：root 指向的节点未到齐先平铺
        } else {
            return Err(GenUiError { message: format!("root 引用未知节点 {root}"), line: None });
        }
    }

    // for/repeat：source 必须是 list 态；循环变量不得与状态同名。
    for id in ir.order.clone() {
        let Some((source, as_)) = ir.nodes[&id].repeat.clone() else { continue };
        let line = ir.nodes[&id].line;
        match ir.states.get(&source) {
            Some(state) if state.type_ == StateType::List => {}
            Some(_) if partial => continue, // 流式：数据源未到齐先跳过
            Some(_) => return Err(ui_err(line, format!("repeat 需要 list 型状态 {source}"))),
            None if partial => continue,
            None => return Err(ui_err(line, format!("repeat 引用未知状态 {source}"))),
        }
        if ir.states.contains_key(&as_) {
            return Err(ui_err(line, format!("循环变量 {as_} 与状态同名")));
        }
    }

    // 环与深度：对全部节点做 DFS（纯环节点没有外部引用者，不能只从根出发）。
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    for id in ir.order.clone() {
        visit_children(ir, &id, 1, &mut visiting, &mut done)?;
    }
    Ok(())
}

fn visit_children(
    ir: &SurfaceIr,
    id: &str,
    depth: usize,
    visiting: &mut BTreeSet<String>,
    done: &mut BTreeSet<String>,
) -> GenUiResult<()> {
    if done.contains(id) {
        return Ok(());
    }
    if visiting.contains(id) {
        return Err(GenUiError {
            message: format!("children 形成环:{id}"),
            line: ir.nodes.get(id).map(|n| n.line),
        });
    }
    if depth > MAX_TREE_DEPTH {
        return Err(GenUiError {
            message: format!("节点树深度超过 {MAX_TREE_DEPTH}"),
            line: ir.nodes.get(id).map(|n| n.line),
        });
    }
    visiting.insert(id.to_string());
    if let Some(PropValue::List(items)) = ir.nodes[id].props.get("children") {
        for child in items.clone() {
            visit_children(ir, &child, depth + 1, visiting, done)?;
        }
    }
    visiting.remove(id);
    done.insert(id.to_string());
    Ok(())
}
