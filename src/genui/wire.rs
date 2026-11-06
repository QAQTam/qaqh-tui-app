//! A2UI v1.0 wire 类型（agent → renderer 消息信封）。
//!
//! 手写 TS 镜像的对位移植：权威参照是桌面端 `genui/v2/types.ts`（其注释声明
//! vendored 自 `specification/v1_0/json/agent_to_renderer.json`，锁 commit 不裸追
//! 上游）。字段名与 wire 完全一致（surfaceId/@path/@call/...），便于直接序列化
//! 与调试。消息流：`createSurface → updateComponents* → updateDataModel* →
//! deleteSurface`，另有 `callRendererFunction`/`agentFunctionResponse`（P4 前只
//! 识别不执行）。
//!
//! DynamicValue 不单独建模 serde：wire 上是「字面量 | `{"@path":..}` |
//! `{"@call":..}`」三态，属性表保持原样 JSON、读取时经 [`to_dyn`] 惰性判别，
//! 避免 flatten+untagged 的脆弱组合（与桌面端 types.ts 的手写窄化同款取舍）。

use serde::Deserialize;
use serde_json::{Map, Value};

pub const A2UI_VERSION: &str = "v1.0";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Message {
    pub version: String,
    #[serde(rename = "createSurface")]
    pub create_surface: Option<CreateSurface>,
    #[serde(rename = "updateComponents")]
    pub update_components: Option<UpdateComponents>,
    #[serde(rename = "updateDataModel")]
    pub update_data_model: Option<UpdateDataModel>,
    #[serde(rename = "deleteSurface")]
    pub delete_surface: Option<DeleteSurface>,
    #[serde(rename = "callRendererFunction")]
    pub call_renderer_function: Option<CallRendererFunction>,
    #[serde(rename = "agentFunctionResponse")]
    pub agent_function_response: Option<AgentFunctionResponse>,
}

impl Message {
    /// 解析一行 wire JSON；除信封字段外必须恰好携带一种操作。
    pub fn parse(line: &str) -> Result<Self, String> {
        let msg: Self = serde_json::from_str(line).map_err(|e| format!("JSON 解析失败: {e}"))?;
        if msg.version != A2UI_VERSION {
            return Err(format!("未知协议版本 {}（本 renderer 支持 {A2UI_VERSION}）", msg.version));
        }
        let ops = [
            msg.create_surface.is_some(),
            msg.update_components.is_some(),
            msg.update_data_model.is_some(),
            msg.delete_surface.is_some(),
            msg.call_renderer_function.is_some(),
            msg.agent_function_response.is_some(),
        ];
        match ops.iter().filter(|op| **op).count() {
            1 => Ok(msg),
            _ => Err("消息必须恰好携带一种操作键".to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CreateSurface {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(rename = "catalogId")]
    pub catalog_id: Option<String>,
    #[serde(rename = "sendDataModel", default)]
    pub send_data_model: bool,
    #[serde(default)]
    pub components: Vec<WireComponent>,
    #[serde(rename = "dataModel", default)]
    pub data_model: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UpdateComponents {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(default)]
    pub components: Vec<WireComponent>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UpdateDataModel {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    /// JSON Pointer；缺省或 "/" 指整个 dataModel。
    #[serde(default)]
    pub path: Option<String>,
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DeleteSurface {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CallRendererFunction {
    #[serde(rename = "surfaceId")]
    pub surface_id: Option<String>,
    #[serde(rename = "functionCallId")]
    pub function_call_id: String,
    #[serde(rename = "callFunction")]
    pub call_function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentFunctionResponse {
    #[serde(rename = "functionCallId")]
    pub function_call_id: String,
}

/// catalog 组件实例：信封字段固定，其余属性原样保留、按需转 [`Dyn`]。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WireComponent {
    pub id: String,
    pub component: String,
    #[serde(default)]
    pub child: Option<String>,
    #[serde(default)]
    pub children: Option<ChildList>,
    #[serde(rename = "onPress")]
    pub on_press: Option<ActionEvent>,
    #[serde(default)]
    pub show: Option<Value>,
    pub metadata: Option<Metadata>,
    #[serde(flatten)]
    pub props: Map<String, Value>,
}

impl WireComponent {
    pub fn prop_dyn(&self, name: &str) -> Option<Dyn> {
        self.props.get(name).map(to_dyn)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ChildList {
    Static(Vec<String>),
    /// spec 的「模板 + dataModel 路径」动态子列表（闸门只查存在性，展开在渲染期）。
    Template(TemplateChildList),
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TemplateChildList {
    #[serde(rename = "componentId")]
    pub component_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Metadata {
    #[serde(default)]
    pub extensions: Option<Map<String, Value>>,
}

/// 用户动作意图（组件的 `onPress.event`）：发给 agent 的命名事件 + 可解析上下文。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ActionEvent {
    pub event: EventSpec,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EventSpec {
    pub name: String,
    #[serde(rename = "userMessage", default)]
    pub user_message: Option<Value>,
    #[serde(default)]
    pub context: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FunctionCall {
    #[serde(rename = "@call")]
    pub name: String,
    #[serde(default)]
    pub args: Option<Map<String, Value>>,
}

/// 三态值：字面量 | `{"@path": ...}` 绑定 | `{"@call": ...}` 函数。
#[derive(Debug, Clone, PartialEq)]
pub enum Dyn {
    Lit(Value),
    Path(String),
    Call { name: String, args: Map<String, Value> },
}

pub fn to_dyn(v: &Value) -> Dyn {
    match v {
        Value::Object(m) if m.contains_key("@path") => match m.get("@path") {
            Some(Value::String(p)) => Dyn::Path(p.clone()),
            _ => Dyn::Lit(v.clone()),
        },
        Value::Object(m) if m.contains_key("@call") => Dyn::Call {
            name: m.get("@call").and_then(Value::as_str).unwrap_or("").to_string(),
            args: m.get("args").and_then(Value::as_object).cloned().unwrap_or_default(),
        },
        other => Dyn::Lit(other.clone()),
    }
}

/// 子引用：静态 id 或模板子列表（对齐桌面端渲染层 renderChildren 的取引用方式）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChildRef {
    Id(String),
    Template(String),
}

/// 收集一个 wire 组件的下一层引用（child + children 信封；模板不在此展开）。
pub fn child_refs(comp: &WireComponent) -> Vec<ChildRef> {
    let mut refs = Vec::new();
    if let Some(c) = &comp.child {
        refs.push(ChildRef::Id(c.clone()));
    }
    match &comp.children {
        Some(ChildList::Static(ids)) => {
            refs.extend(ids.iter().map(|id| ChildRef::Id(id.clone())));
        }
        Some(ChildList::Template(t)) => {
            refs.push(ChildRef::Template(t.component_id.clone()));
        }
        None => {}
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 wire 形状 fixture（按桌面端 translate.ts 约定手写；与 dsl::tests 的
    /// 编译链产物交叉验证）。root 包装组件在最后——translate.ts 的
    /// `components.push(root)` 顺序。
    pub(super) const PLAN_CREATE: &str = r#"{"version":"v1.0","createSurface":{"surfaceId":"plan","catalogId":"qaqh.dev:standard-v2","sendDataModel":false,"components":[
      {"id":"card","component":"TodoList","title":"执行计划","children":["t1","t2","summary","ask","send"]},
      {"id":"t1","component":"Todo","text":"分帧器","done":{"@path":"/s/t_frame"},"meta":"阶段 1"},
      {"id":"t2","component":"Todo","text":"生成式界面","done":{"@path":"/s/t_genui"}},
      {"id":"summary","component":"Text","text":{"@call":"qaqh:arith.add","args":{"left":"进度：","right":{"@path":"/s/t_frame"}}}},
      {"id":"ask","component":"TextInput","label":"补充说明","value":{"@path":"/s/confirm_note"},"placeholder":"（点此输入）"},
      {"id":"send","component":"Button","label":"提交确认",
       "disabled":{"@call":"qaqh:logic.eq","args":{"left":{"@path":"/s/confirm_note"},"right":""}},
       "onPress":{"event":{"name":"qaqh.action.followUp","context":{"prompt":"请按计划继续","fields":[{"label":"说明","key":"confirm_note"}]}}}},
      {"id":"root","component":"Stack","children":["card"]}
    ],"dataModel":{"s":{"t_frame":false,"t_genui":false,"confirm_note":""}}}}"#;

    #[test]
    fn wire_parse_accepts_real_shapes_and_rejects_bad() {
        assert!(Message::parse(PLAN_CREATE).is_ok());
        assert!(Message::parse(
            r#"{"version":"v1.0","updateComponents":{"surfaceId":"t","components":[{"id":"root","component":"Stack"}]}}"#
        )
        .is_ok());
        assert!(Message::parse(r#"{"version":"v0.9","createSurface":{"surfaceId":"x"}}"#).is_err());
        assert!(Message::parse(r#"{"version":"v1.0"}"#).is_err());
        assert!(Message::parse("not json").is_err());
    }

    #[test]
    fn to_dyn_discriminates_three_value_kinds() {
        assert_eq!(to_dyn(&Value::String("x".into())), Dyn::Lit(Value::String("x".into())));
        assert_eq!(
            to_dyn(&serde_json::json!({"@path": "/s/a"})),
            Dyn::Path("/s/a".into())
        );
        match to_dyn(&serde_json::json!({"@call": "qaqh:logic.not", "args": {"value": true}})) {
            Dyn::Call { name, args } => {
                assert_eq!(name, "qaqh:logic.not");
                assert!(args.contains_key("value"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn child_refs_covers_child_children_and_template() {
        let comp: WireComponent = serde_json::from_str(
            r#"{"id":"n","component":"Stack","child":"a","children":["b","c"]}"#,
        )
        .unwrap();
        assert_eq!(
            child_refs(&comp),
            vec![ChildRef::Id("a".into()), ChildRef::Id("b".into()), ChildRef::Id("c".into())]
        );
        let tpl: WireComponent = serde_json::from_str(
            r#"{"id":"n","component":"Stack","children":{"componentId":"row","path":"/s/list"}}"#,
        )
        .unwrap();
        assert_eq!(child_refs(&tpl), vec![ChildRef::Template("row".into())]);
    }
}
