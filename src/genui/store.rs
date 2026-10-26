//! SurfaceRegistry：按序消费 A2UI 消息，维护每个 surface 的组件表与 dataModel。
//!
//! 对位移植桌面端 `genui/v2/store.ts`。v0.1 的「全文重解析 + 整树重挂」在这里
//! 是「按消息 patch」——流式卡顿的根治点。校验分两层：
//!
//! - 即时（apply 时）：信封形状、surface 生命周期（create→update→delete）、预算；
//! - 惰性（[`Registry::render_blocker`]）：root 存在且从 root 可达的引用全部解析、
//!   无环、不超深——renderer 只在可渲染时挂载。updateComponents 允许前向引用
//!   （spec 不禁止跨消息定义），坏引用在闸门处暴露，而不是阻断流式。
//!
//! 预算（节点数/深度/dataModel 体积）是 DoS 防线，A2UI spec 留给 renderer。

use std::collections::BTreeMap;

use serde_json::Value;

use super::wire::{self, Message};

pub const MAX_NODES: usize = 512;
pub const MAX_TREE_DEPTH: usize = 16;
/// dataModel 单值序列化体积预算：防模型灌巨包。
pub const MAX_DATA_MODEL_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct SurfaceState {
    pub surface_id: String,
    pub catalog_id: Option<String>,
    pub send_data_model: bool,
    pub components: BTreeMap<String, wire::WireComponent>,
    pub data_model: Value,
    /// reset 动作的基线快照。
    pub initial_model: Value,
}

#[derive(Debug, Default)]
pub struct Registry {
    pub surfaces: BTreeMap<String, SurfaceState>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyError {
    Version(String),
    NoOp,
    UnknownSurface(String),
    Duplicate(String),
    Budget(String),
    Malformed(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(f, "未知协议版本 {v}"),
            Self::NoOp => write!(f, "消息不含任何已知操作键"),
            Self::UnknownSurface(id) => write!(f, "surface {id} 不存在"),
            Self::Duplicate(id) => write!(f, "surface {id} 已存在"),
            Self::Budget(m) => write!(f, "超预算: {m}"),
            Self::Malformed(m) => write!(f, "协议违规: {m}"),
        }
    }
}

/// apply 的结果附注：本仓 renderer 函数白名单为空（P4 再接 copy/openLink 等），
/// `callRendererFunction` 只记录不执行——降级可见，不静默。
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Applied(&'static str),
    Unsupported(&'static str),
}

impl Registry {
    pub fn apply(&mut self, msg: Message) -> Result<Note, ApplyError> {
        if msg.version != super::wire::A2UI_VERSION {
            return Err(ApplyError::Version(msg.version));
        }
        if let Some(c) = msg.create_surface {
            if self.surfaces.contains_key(&c.surface_id) {
                return Err(ApplyError::Duplicate(c.surface_id));
            }
            if c.components.len() > MAX_NODES {
                return Err(ApplyError::Budget(format!("组件数超过 {MAX_NODES}")));
            }
            let mut comps = BTreeMap::new();
            for comp in c.components {
                if comps.insert(comp.id.clone(), comp).is_some() {
                    return Err(ApplyError::Malformed("组件 id 重复".into()));
                }
            }
            let dm = Value::Object(c.data_model);
            if serde_json::to_string(&dm).unwrap_or_default().len() > MAX_DATA_MODEL_BYTES {
                return Err(ApplyError::Budget("dataModel 体积超限".into()));
            }
            self.surfaces.insert(
                c.surface_id.clone(),
                SurfaceState {
                    surface_id: c.surface_id,
                    catalog_id: c.catalog_id,
                    send_data_model: c.send_data_model,
                    components: comps,
                    initial_model: dm.clone(),
                    data_model: dm,
                },
            );
            return Ok(Note::Applied("create"));
        }
        if let Some(u) = msg.update_components {
            let Some(state) = self.surfaces.get_mut(&u.surface_id) else {
                return Err(ApplyError::UnknownSurface(u.surface_id));
            };
            for comp in u.components {
                if state.components.len() >= MAX_NODES && !state.components.contains_key(&comp.id) {
                    return Err(ApplyError::Budget(format!("组件数超过 {MAX_NODES}")));
                }
                state.components.insert(comp.id.clone(), comp);
            }
            return Ok(Note::Applied("updateComponents"));
        }
        if let Some(u) = msg.update_data_model {
            let Some(state) = self.surfaces.get_mut(&u.surface_id) else {
                return Err(ApplyError::UnknownSurface(u.surface_id));
            };
            let ptr = normalize_ptr(u.path.as_deref());
            pointer_set(&mut state.data_model, &ptr, u.value);
            if serde_json::to_string(&state.data_model).unwrap_or_default().len()
                > MAX_DATA_MODEL_BYTES
            {
                return Err(ApplyError::Budget("dataModel 体积超限".into()));
            }
            return Ok(Note::Applied("updateDataModel"));
        }
        if let Some(d) = msg.delete_surface {
            if self.surfaces.remove(&d.surface_id).is_none() {
                return Err(ApplyError::UnknownSurface(d.surface_id));
            }
            return Ok(Note::Applied("delete"));
        }
        if let Some(c) = msg.call_renderer_function {
            let _ = c;
            return Ok(Note::Unsupported("callRendererFunction（白名单为空）"));
        }
        if let Some(r) = msg.agent_function_response {
            let _ = r;
            return Ok(Note::Unsupported("agentFunctionResponse（无未决调用）"));
        }
        Err(ApplyError::NoOp)
    }

    /// 渲染闸门：root 存在、从 root 出发引用全部可解析、无环、不超深/超预算。
    /// 返回 Some(原因) = 还不能渲染（流式中或违规，具名暴露）。
    pub fn render_blocker(&self, surface_id: &str) -> Option<String> {
        let state = self.surfaces.get(surface_id)?;
        let root = state.components.get("root")?;
        let mut visiting = Vec::new();
        let mut done = Vec::new();
        let mut count = 0usize;
        let blocker = visit(state, root, 0, &mut visiting, &mut done, &mut count);
        if blocker.is_none() && count > MAX_NODES {
            return Some(format!("组件数超过 {MAX_NODES}"));
        }
        blocker
    }
}

fn visit(
    state: &SurfaceState,
    comp: &wire::WireComponent,
    depth: usize,
    visiting: &mut Vec<String>,
    done: &mut Vec<String>,
    count: &mut usize,
) -> Option<String> {
    if done.contains(&comp.id) {
        return None;
    }
    if visiting.contains(&comp.id) {
        return Some(format!("children 形成环: {}", comp.id));
    }
    if depth > MAX_TREE_DEPTH {
        return Some(format!("组件树深度超过 {MAX_TREE_DEPTH}"));
    }
    *count += 1;
    visiting.push(comp.id.clone());
    for r in wire::child_refs(comp) {
        match r {
            wire::ChildRef::Id(id) => {
                let Some(child) = state.components.get(&id) else {
                    return Some(format!("引用未解析: {id}"));
                };
                if let Some(b) = visit(state, child, depth + 1, visiting, done, count) {
                    return Some(b);
                }
            }
            // 模板子列表：闸门只要求被引用的模板组件存在（展开在渲染期）。
            wire::ChildRef::Template(id) => {
                if !state.components.contains_key(&id) {
                    return Some(format!("引用未解析: {id}"));
                }
            }
        }
    }
    visiting.pop();
    done.push(comp.id.clone());
    None
}

// ── JSON Pointer 增删改查（serde_json 自带查，写删手补）────────────────────

/// 桌面端约定：缺省或 "/" 指整个 dataModel。
fn normalize_ptr(path: Option<&str>) -> String {
    match path {
        None | Some("/") => String::new(),
        Some(p) => p.to_string(),
    }
}

fn unescape_seg(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

pub fn pointer_set(root: &mut Value, ptr: &str, value: Value) {
    if ptr.is_empty() {
        *root = value;
        return;
    }
    let segs: Vec<String> = ptr.trim_start_matches('/').split('/').map(unescape_seg).collect();
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        let last = i == segs.len() - 1;
        if last {
            if let Some(obj) = cur.as_object_mut() {
                obj.insert(seg.clone(), value);
            } else if let Some(arr) = cur.as_array_mut()
                && let Ok(idx) = seg.parse::<usize>()
            {
                if idx < arr.len() {
                    arr[idx] = value;
                } else {
                    arr.push(value);
                }
            }
            return;
        }
        if cur.as_object_mut().is_none_or(|o| o.get(seg).is_none_or(Value::is_null))
            && let Some(obj) = cur.as_object_mut()
        {
            obj.insert(seg.clone(), Value::Object(serde_json::Map::new()));
        }
        let Some(next) = cur.pointer_mut(&format!("/{seg}")) else { return };
        cur = next;
    }
}

pub fn pointer_remove(root: &mut Value, ptr: &str) -> Option<Value> {
    let segs: Vec<String> = ptr.trim_start_matches('/').split('/').map(unescape_seg).collect();
    let (last, parents) = segs.split_last()?;
    let mut cur = root;
    for seg in parents {
        cur = cur.pointer_mut(&format!("/{seg}"))?;
    }
    match cur {
        Value::Object(obj) => obj.remove(last),
        Value::Array(arr) => last
            .parse::<usize>()
            .ok()
            .filter(|i| *i < arr.len())
            .map(|i| arr.remove(i)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 流式两阶段 fixture：create 只带 r1，card 已引用 r2/r3（前向引用）。
    const TABLE_STAGE1: &str = r#"{"version":"v1.0","createSurface":{"surfaceId":"table","catalogId":"qaqh.dev:standard-v2","sendDataModel":false,"components":[
      {"id":"root","component":"Stack","children":["card"]},
      {"id":"card","component":"Card","title":"会议名单","children":["r1","r2","r3"]},
      {"id":"r1","component":"Text","text":"张三"}
    ],"dataModel":{}}}"#;
    const TABLE_STAGE2: &str = r#"{"version":"v1.0","updateComponents":{"surfaceId":"table","components":[
      {"id":"r2","component":"Text","text":"李四"},
      {"id":"r3","component":"Text","text":"王五"}
    ]}}"#;

    #[test]
    fn render_gate_blocks_until_forward_refs_resolve() {
        let mut reg = Registry::default();
        reg.apply(Message::parse(TABLE_STAGE1).unwrap()).unwrap();
        assert_eq!(reg.render_blocker("table"), Some("引用未解析: r2".into()));
        reg.apply(Message::parse(TABLE_STAGE2).unwrap()).unwrap();
        assert_eq!(reg.render_blocker("table"), None);
        assert_eq!(reg.render_blocker("missing"), None, "未知 surface 不设闸（上层按不存在处理）");
    }

    #[test]
    fn budget_rejects_oversized_surface() {
        let comps: Vec<String> = (0..600)
            .map(|i| format!(r#"{{"id":"c{i}","component":"Text","text":"x"}}"#))
            .collect();
        let msg = format!(
            r#"{{"version":"v1.0","createSurface":{{"surfaceId":"big","components":[{}]}}}}"#,
            comps.join(",")
        );
        let mut reg = Registry::default();
        assert!(matches!(reg.apply(Message::parse(&msg).unwrap()), Err(ApplyError::Budget(_))));
    }

    #[test]
    fn gate_detects_cycles() {
        let mut reg = Registry::default();
        reg.apply(
            Message::parse(
                r#"{"version":"v1.0","createSurface":{"surfaceId":"cyc","components":[
                  {"id":"root","component":"Stack","children":["a"]},
                  {"id":"a","component":"Stack","children":["b"]},
                  {"id":"b","component":"Stack","children":["a"]}
                ],"dataModel":{}}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(reg.render_blocker("cyc").unwrap().contains("形成环"));
    }

    #[test]
    fn call_renderer_function_is_recorded_not_executed() {
        let mut reg = Registry::default();
        reg.apply(
            Message::parse(
                r#"{"version":"v1.0","createSurface":{"surfaceId":"s","components":[],"dataModel":{}}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let note = reg
            .apply(
                Message::parse(
                    r#"{"version":"v1.0","callRendererFunction":{"functionCallId":"f1","callFunction":{"@call":"qaqh:copy"}}}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(note, Note::Unsupported("callRendererFunction（白名单为空）"));
    }

    #[test]
    fn pointer_set_remove_roundtrip() {
        let mut v: Value = json!({"s": {"a": 1, "list": ["x"]}});
        pointer_set(&mut v, "/s/a", json!(9));
        assert_eq!(v.pointer("/s/a"), Some(&json!(9)));
        pointer_set(&mut v, "/s/list/0", json!("y"));
        assert_eq!(v.pointer("/s/list/0"), Some(&json!("y")));
        pointer_set(&mut v, "/s/deep/nest", json!(true));
        assert_eq!(v.pointer("/s/deep/nest"), Some(&json!(true)));
        assert_eq!(pointer_remove(&mut v, "/s/list/0"), Some(json!("y")));
        assert_eq!(pointer_remove(&mut v, "/s/a"), Some(json!(9)));
        let mut root = Value::Object(serde_json::Map::new());
        pointer_set(&mut root, "", json!(7));
        assert_eq!(root, json!(7));
    }
}
