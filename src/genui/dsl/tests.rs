//! DSL 编译链的契约测试。
//!
//! 两类锚：
//! 1. **交叉验证**——真实编译链（tsx → translate）的产物与按桌面端
//!    translate.ts 约定手写的 wire fixture 逐字段相等：translate.ts 改语义时
//!    这里先红，防止 TUI 静默漂移；
//! 2. **真实配方全链**——genui-check.tsx 的 fixture（即 backend_prompt.md 教学
//!    后模型输出的形状）经 decoder → parser → validate → translate → registry
//!    全链可达，流式/容尾/show 剪枝/目录拒绝各有一闸。
//!
//! 渲染级断言（布局/命中/交互）在 demo 沙盒（qaqh-genui-demo）里，本模块只测
//! 纯逻辑——渲染接线是后续切片，届时把渲染断言随切片移入。

use std::collections::BTreeMap;

use serde_json::Value;

use super::decoder::{ContentPart, UiFormat, split_content_parts};
use super::ingest_ui_part;
use super::ir::GenUiResult;
use super::{compile_source, translate};
use crate::genui::store::Registry;

// ── 真实配方 fixture（逐字取自桌面端 genui-check.tsx，围栏版本 v=0.2）──────

/// ASK_TODO：TodoList + TextInput + followUp 提交（disabled 随输入联动）。
const ASK_TODO_FENCE: &str = concat!(
    "说明文字\n",
    "```qaqh-ui v=0.2 format=tsx\n",
    "ui.surface(\"plan\", { root: \"card\", title: \"计划与确认\" });\n",
    "const t_frame = ui.state(\"t_frame\", \"boolean\", false);\n",
    "const t_genui = ui.state(\"t_genui\", \"boolean\", false);\n",
    "const confirm_note = ui.state(\"confirm_note\", \"string\", \"\");\n",
    "ui.node(\"card\", <TodoList title=\"执行计划\" children={[\"t1\", \"t2\", \"summary\", \"ask\", \"send\"]} />);\n",
    "ui.node(\"t1\", <Todo text=\"分帧器\" done={ui.bind(\"t_frame\")} meta=\"阶段 1\" />);\n",
    "ui.node(\"t2\", <Todo text=\"生成式界面\" done={ui.bind(\"t_genui\")} />);\n",
    "ui.node(\"summary\", <Text text={\"进度：\" + t_frame} />);\n",
    "ui.node(\"ask\", <TextInput label=\"补充说明\" value={confirm_note} required={true} />);\n",
    "ui.node(\"send\", <Button label=\"提交确认\" disabled={confirm_note == \"\"} onPress={ui.followUp(\"请按计划继续\", { \"说明\": confirm_note })} />);\n",
    "ui.end();\n",
    "```\n",
);

/// V2_COUNTER：Metric + set/reset + TagList 增删 + repeat 模板行。
/// 两张顶层卡必须显式嵌一层 Stack（包装根只包一个组件，见 translate.rs）。
const V2_COUNTER_FENCE: &str = concat!(
    "```qaqh-ui v=0.2 format=tsx\n",
    "ui.surface(\"counter\", { root: \"wrap\", fallback: \"v2 计数器，可增加或重置。\" });\n",
    "const count = ui.state(\"count\", \"integer\", 0);\n",
    "const tags = ui.state(\"tags\", \"list\", [\"alpha\"]);\n",
    "ui.node(\"wrap\", <Stack children={[\"card\", \"tagcard\"]} />);\n",
    "ui.node(\"card\", <Card title=\"v2 计数器\" children={[\"value\", \"increment\", \"reset\"]} />);\n",
    "ui.node(\"value\", <Metric label=\"当前计数\" value={count} />);\n",
    "ui.node(\"increment\", <Button label=\"增加\" onPress={ui.set(\"count\", count + 1)} />);\n",
    "ui.node(\"reset\", <Button label=\"重置\" onPress={ui.reset()} />);\n",
    "ui.node(\"tagcard\", <Card title=\"标签\" children={[\"tags\", \"add\", \"del\", \"rows\"]} />);\n",
    "ui.node(\"tags\", <TagList items={tags} />);\n",
    "ui.node(\"add\", <Button label=\"加 beta\" onPress={ui.append(\"tags\", \"beta\")} />);\n",
    "ui.node(\"del\", <Button label=\"删 beta\" onPress={ui.remove(\"tags\", \"beta\")} />);\n",
    "ui.node(\"rows\", <Text text={\"· \" + item} repeat={ui.for(\"tags\", \"item\")} />);\n",
    "ui.end();\n",
    "```\n",
);

/// 与桌面端 translate.ts 产物一致的手写 wire（交叉验证锚）。root 包装组件在
/// 最后——translate.ts 的 `components.push(root)` 顺序。
const COUNTER_CREATE: &str = r#"{"version":"v1.0","createSurface":{"surfaceId":"counter","catalogId":"qaqh.dev:standard-v2","sendDataModel":false,"components":[
  {"id":"wrap","component":"Stack","children":["card","tagcard"]},
  {"id":"card","component":"Card","title":"v2 计数器","children":["value","increment","reset"]},
  {"id":"value","component":"Metric","label":"当前计数","value":{"@path":"/s/count"}},
  {"id":"increment","component":"Button","label":"增加","onPress":{"event":{"name":"qaqh.action.set","context":{"key":"count","value":{"@call":"qaqh:arith.add","args":{"left":{"@path":"/s/count"},"right":1}}}}}},
  {"id":"reset","component":"Button","label":"重置","onPress":{"event":{"name":"qaqh.action.reset","context":{}}}},
  {"id":"tagcard","component":"Card","title":"标签","children":["tags","add","del","rows"]},
  {"id":"tags","component":"TagList","items":{"@path":"/s/tags"}},
  {"id":"add","component":"Button","label":"加 beta","onPress":{"event":{"name":"qaqh.action.append","context":{"key":"tags","value":"beta"}}}},
  {"id":"del","component":"Button","label":"删 beta","onPress":{"event":{"name":"qaqh.action.remove","context":{"key":"tags","value":"beta"}}}},
  {"id":"rows","component":"Text","text":{"@call":"qaqh:arith.add","args":{"left":"· ","right":{"@path":""}}},
   "metadata":{"extensions":{"qaqh.dev/repeat":{"path":"/s/tags","as":"item"}}}},
  {"id":"root","component":"Stack","children":["wrap"]}
],"dataModel":{"s":{"count":0,"tags":["alpha"]}}}}"#;

/// FORM_JSONL：jsonl 载体 + 表单组件 + show 条件渲染。
const FORM_JSONL_FENCE: &str = concat!(
    "```qaqh-ui v=0.2 format=jsonl\n",
    "{\"op\":\"surface.open\",\"surface\":\"form\",\"root\":\"card\",\"fallback\":\"表单演示\"}\n",
    "{\"op\":\"state\",\"key\":\"name\",\"type\":\"string\",\"initial\":\"\"}\n",
    "{\"op\":\"state\",\"key\":\"dark\",\"type\":\"boolean\",\"initial\":false}\n",
    "{\"op\":\"state\",\"key\":\"tags\",\"type\":\"list\",\"initial\":[\"alpha\"]}\n",
    "{\"op\":\"node\",\"id\":\"card\",\"component\":\"Card\",\"props\":{\"title\":\"表单\",\"children\":[\"name\",\"on\",\"off\",\"badge\",\"tags\",\"add\",\"del\",\"rows\"]}}\n",
    "{\"op\":\"node\",\"id\":\"name\",\"component\":\"TextInput\",\"props\":{\"label\":\"名称\",\"value\":{\"bind\":\"name\"},\"placeholder\":\"输入名称\",\"required\":true,\"maxLength\":10}}\n",
    "{\"op\":\"node\",\"id\":\"on\",\"component\":\"Button\",\"props\":{\"label\":\"开启\",\"onPress\":{\"set\":\"dark\",\"value\":{\"lit\":true}}}}\n",
    "{\"op\":\"node\",\"id\":\"off\",\"component\":\"Button\",\"props\":{\"label\":\"关闭\",\"onPress\":{\"set\":\"dark\",\"value\":{\"lit\":false}}}}\n",
    "{\"op\":\"node\",\"id\":\"badge\",\"component\":\"Badge\",\"props\":{\"text\":\"深色已开启\",\"show\":{\"state\":\"dark\"}}}\n",
    "{\"op\":\"node\",\"id\":\"tags\",\"component\":\"TagList\",\"props\":{\"items\":{\"bind\":\"tags\"}}}\n",
    "{\"op\":\"node\",\"id\":\"add\",\"component\":\"Button\",\"props\":{\"label\":\"加标签\",\"onPress\":{\"append\":\"tags\",\"value\":{\"lit\":\"beta\"}}}}\n",
    "{\"op\":\"node\",\"id\":\"del\",\"component\":\"Button\",\"props\":{\"label\":\"移除标签\",\"onPress\":{\"remove\":\"tags\",\"value\":{\"lit\":\"beta\"}}}}\n",
    "{\"op\":\"node\",\"id\":\"rows\",\"component\":\"Text\",\"props\":{\"text\":{\"op\":\"add\",\"left\":{\"lit\":\"· \"},\"right\":{\"item\":\"item\"}},\"repeat\":{\"source\":\"tags\",\"as\":\"item\"}}}\n",
    "{\"op\":\"surface.close\"}\n",
    "```\n",
);

/// 流式表格（genui-check 的 stages）：md → holdback → 开门 → 部分编译 → seal。
const TABLE_RECORDS: &str = concat!(
    "{\"op\":\"surface.open\",\"surface\":\"table\",\"root\":\"card\",\"title\":\"会议名单\"}\n",
    "{\"op\":\"state\",\"key\":\"rows\",\"type\":\"list\",\"initial\":[\"张三\",\"李四\"]}\n",
    "{\"op\":\"node\",\"id\":\"card\",\"component\":\"Card\",\"props\":{\"title\":\"名单\",\"children\":[\"r1\",\"r2\",\"r3\"]}}\n",
    "{\"op\":\"node\",\"id\":\"r1\",\"component\":\"Text\",\"props\":{\"text\":\"张三\"}}\n",
);
const TABLE_DONE_TAIL: &str = concat!(
    "{\"op\":\"node\",\"id\":\"r2\",\"component\":\"Text\",\"props\":{\"text\":\"李四\"}}\n",
    "{\"op\":\"node\",\"id\":\"r3\",\"component\":\"Text\",\"props\":{\"text\":\"王五\"}}\n",
    "{\"op\":\"surface.close\"}\n",
);

fn table_stages() -> Vec<String> {
    let fence_head = "```qaqh-ui v=0.2 format=jsonl\n";
    vec![
        "说明文字".to_string(),
        "说明文字\n```qaqh-u".to_string(),
        format!("说明文字\n{fence_head}"),
        format!("说明文字\n{fence_head}{TABLE_RECORDS}"),
        format!("说明文字\n{fence_head}{TABLE_RECORDS}{TABLE_DONE_TAIL}```\n结尾"),
    ]
}

/// 从围栏文本取出唯一的 UI 分帧（seal 状态）。
fn ui_part_of(fence: &str) -> ContentPart {
    split_content_parts(fence, true)
        .into_iter()
        .find(|p| matches!(p, ContentPart::Ui { .. }))
        .expect("fixture 必须含 UI 围栏")
}

fn compile_ok(part: &ContentPart) -> GenUiResult<super::ir::SurfaceIr> {
    let ContentPart::Ui { source, format, sealed, .. } = part else {
        panic!("不是 UI 分帧");
    };
    compile_source(source, *format, *sealed)
}

#[test]
fn dsl_translate_counter_matches_handwritten_wire_exactly() {
    let ir = compile_ok(&ui_part_of(V2_COUNTER_FENCE)).unwrap();
    let wire = translate::translate_create(&ir);
    let expected = crate::genui::wire::Message::parse(COUNTER_CREATE).unwrap();
    assert_eq!(wire.create_surface, expected.create_surface, "translate 产物与手写 wire 不一致");
}

#[test]
fn dsl_ask_todo_chain_reaches_registry() {
    let part = ui_part_of(ASK_TODO_FENCE);
    let mut registry = Registry::default();
    let mut created = BTreeMap::new();
    let ir = ingest_ui_part(&mut registry, &part, &mut created).unwrap();
    assert!(ir.warnings.is_empty(), "标准配方不应有孤儿告警: {:?}", ir.warnings);
    let state = &registry.surfaces["plan"];
    // 绑定归一：value={confirm_note} 简写落成 /s/ 路径。
    assert_eq!(
        state.components["ask"].prop_dyn("value"),
        Some(crate::genui::wire::Dyn::Path("/s/confirm_note".into()))
    );
    // followUp 动作形状。
    let on_press = state.components["send"].on_press.as_ref().unwrap();
    assert_eq!(on_press.event.name, "qaqh.action.followUp");
    let ctx = on_press.event.context.as_ref().unwrap();
    assert_eq!(ctx.get("prompt"), Some(&Value::String("请按计划继续".into())));
    // 表达式降级：「进度：」+ t_frame → arith.add。
    assert_eq!(
        state.components["summary"].prop_dyn("text"),
        Some(crate::genui::wire::Dyn::Call {
            name: "qaqh:arith.add".into(),
            args: serde_json::from_value(serde_json::json!({
                "left": "进度：",
                "right": {"@path": "/s/t_frame"}
            }))
            .unwrap(),
        })
    );
    assert_eq!(state.data_model.pointer("/s/confirm_note"), Some(&Value::String(String::new())));
}

#[test]
fn dsl_decoder_streams_table_through_stages() {
    // genui-check 的分阶段全文：holdback → 部分编译（前向引用剪枝）→ seal 转正。
    let mut registry = Registry::default();
    let mut created = BTreeMap::new();
    for (i, stage) in table_stages().iter().enumerate() {
        let parts = split_content_parts(stage, false);
        match i {
            0 | 1 => assert!(
                parts.iter().all(|p| matches!(p, ContentPart::Markdown(_))),
                "阶段 {i} 应只有 markdown（holdback 吞掉围栏头）: {parts:?}"
            ),
            2 => assert!(
                parts.iter().any(|p| matches!(p, ContentPart::Ui { sealed: false, .. })),
                "阶段 2 应产出未 seal 的 UI 分帧"
            ),
            3 | 4 => {
                for part in &parts {
                    if matches!(part, ContentPart::Ui { .. }) {
                        let ir = ingest_ui_part(&mut registry, part, &mut created).unwrap();
                        if i == 3 {
                            assert!(!ir.nodes.contains_key("r2"), "部分编译不应有 r2");
                            // 前向引用剪枝：card 的 children 已剪到只剩 r1。
                            assert_eq!(
                                registry.surfaces["table"].components["card"].children,
                                Some(crate::genui::wire::ChildList::Static(vec!["r1".into()]))
                            );
                        } else {
                            assert!(ir.nodes.contains_key("r3"), "seal 后应有 r3");
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    let dm = &registry.surfaces["table"].data_model;
    assert_eq!(dm.pointer("/s/rows"), Some(&serde_json::json!(["张三", "李四"])));
}

#[test]
fn dsl_jsonl_form_compiles_with_show_and_bindings() {
    let part = ui_part_of(FORM_JSONL_FENCE);
    let mut registry = Registry::default();
    let mut created = BTreeMap::new();
    let ir = ingest_ui_part(&mut registry, &part, &mut created).unwrap();
    assert!(ir.warnings.is_empty(), "{:?}", ir.warnings);
    let state = &registry.surfaces["form"];
    // show 条件渲染进 wire（渲染层求值，dark=false 时不画）。
    assert_eq!(
        state.components["badge"].show,
        Some(serde_json::json!({"@path": "/s/dark"}))
    );
    // jsonl 的 {bind:} 与受控校验属性（maxLength 整数是合法 number）。
    assert_eq!(
        state.components["name"].prop_dyn("value"),
        Some(crate::genui::wire::Dyn::Path("/s/name".into()))
    );
    assert_eq!(
        state.components["name"].prop_dyn("maxLength"),
        Some(crate::genui::wire::Dyn::Lit(serde_json::json!(10)))
    );
    // repeat 展开扩展 + item 空路径。
    let rows = &state.components["rows"];
    assert_eq!(
        rows.metadata.as_ref().unwrap().extensions.as_ref().unwrap()["qaqh.dev/repeat"],
        serde_json::json!({"path": "/s/tags", "as": "item"})
    );
}

#[test]
fn dsl_tolerant_tail_compiles_completed_prefix_only() {
    // 流式 tsx：截断在 summary 之前（缺 ask/send/ui.end）——已到齐的先编译。
    let cut = ASK_TODO_FENCE.split("ui.node(\"summary\"").next().unwrap().to_string();
    let parts = split_content_parts(&cut, false);
    let ir = parts
        .iter()
        .find_map(|p| match p {
            ContentPart::Ui { .. } => compile_ok(p).ok(),
            _ => None,
        })
        .expect("截断源码应部分编译成功");
    assert!(ir.nodes.contains_key("t2"));
    assert!(!ir.nodes.contains_key("send"), "截断处之后的节点不应出现");
    // card 已到齐：root 引用保留（partial 只在 root 指向的节点未到齐时才平铺）。
    assert_eq!(ir.root_id.as_deref(), Some("card"));
}

#[test]
fn dsl_rejects_unknown_component_and_out_of_catalog_props() {
    let bad = "```qaqh-ui v=0.2 format=tsx\nui.surface(\"x\");\nui.node(\"a\", <Widget text=\"hi\" />);\nui.end();\n```\n";
    let err = compile_ok(&ui_part_of(bad)).unwrap_err();
    assert!(err.message.contains("未知组件类型"), "{err}");
    let bad_prop =
        "```qaqh-ui v=0.2 format=tsx\nui.surface(\"x\");\nui.node(\"a\", <Text text=\"hi\" color=\"red\" />);\nui.end();\n```\n";
    let err = compile_ok(&ui_part_of(bad_prop)).unwrap_err();
    assert!(err.message.contains("不支持属性"), "{err}");
    let bad_action =
        "```qaqh-ui v=0.2 format=tsx\nui.surface(\"x\");\nconst n = ui.state(\"n\", \"integer\", 0);\nui.node(\"a\", <Text text={ui.set(\"n\", 1)} />);\nui.end();\n```\n";
    let err = compile_ok(&ui_part_of(bad_action)).unwrap_err();
    // 动作只能落在 onPress 事件槽：text 是 expr 槽，目录校验在槽位处拒绝。
    assert!(err.message.contains("属性 text 需要"), "{err}");
}

#[test]
fn dsl_orphan_warning_for_unreferenced_nodes() {
    // 软告警（不阻断）：声明了 repeat 行却忘了挂进根容器 → 编译通过、具名提示。
    let orphan = concat!(
        "```qaqh-ui v=0.2 format=tsx\n",
        "ui.surface(\"orphan\", { root: \"card\" });\n",
        "const tags = ui.state(\"tags\", \"list\", [\"alpha\"]);\n",
        "ui.node(\"card\", <Card title=\"标签\" children={[\"tlist\"]} />);\n",
        "ui.node(\"tlist\", <TagList items={tags} />);\n",
        "ui.node(\"trow\", <Text text={\"· \" + item} repeat={ui.for(\"tags\", \"item\")} />);\n",
        "ui.end();\n",
        "```\n",
    );
    let ir = compile_ok(&ui_part_of(orphan)).unwrap();
    assert_eq!(ir.warnings.len(), 1, "{:?}", ir.warnings);
    assert!(ir.warnings[0].contains("trow"), "{:?}", ir.warnings);
}
