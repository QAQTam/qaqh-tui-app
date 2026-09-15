//! 会话列表条目的 **UI 视图**（宽松解析）。
//!
//! 本模块原为「服务面方法表」——38 个方法名常量 + `MethodKind`/`MethodInfo`
//! 表，镜像上游 `qaqh-runtime/src/ringing/service_methods.rs`。
//!
//! T-01 阶段 1.5 把服务面切到 `Client::query`/`action` 后，方法名归权威枚举
//! 持有（`QueryRequest::into_parts` / `ActionRequest::into_parts`），那 38 个
//! 常量**全部零引用**、`lookup()` 无调用点。整张表连 `#![allow(dead_code)]`
//! 一起删除——与 T-13 同款：`allow(dead_code)` 加自带测试足以让死镜像活下去。
//!
//! 只留下真正被 UI 消费的 [`SessionMetaView`]。

use qaqh_client::ConversationMode;

/// 会话列表条目（daemon 返回 SessionMeta + 运行时字段；宽松解析）。
#[derive(Debug, Clone, Default)]
pub struct SessionMetaView {
    pub seed: String,
    pub updated_at: Option<u64>,
    pub model: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// 0=Code, 1=Plan（meta 编码）。
    pub mode: Option<u8>,
    pub archived: bool,
    pub ephemeral: bool,
    pub running: bool,
}

impl SessionMetaView {
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let obj = value.as_object()?;
        Some(Self {
            seed: obj.get("seed")?.as_str()?.to_owned(),
            updated_at: obj.get("updated_at").and_then(serde_json::Value::as_u64),
            model: obj
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            title: obj
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            cwd: obj
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            mode: obj
                .get("mode")
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as u8),
            archived: obj
                .get("archived")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            ephemeral: obj
                .get("ephemeral")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            running: obj
                .get("running")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// 展示标题：title → cwd 尾段 → seed。
    ///
    /// last_summary 不再参与标题（2026-09）：它是「最后一条 assistant 回复
    /// 首行」的预览（每轮 save_append 覆盖），当标题用会导致列表标题随
    /// 对话漂移成“模型最近说了什么的开头”。标题职责完全交给后端 title
    /// 字段（入站即 LLM 总结用户需求生成；旧会话无 title 回退 cwd/seed）。
    /// 该字段因无任何读取点，已连同视图里的其余死字段一并删除。
    pub fn display_title(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return t.to_owned();
        }
        if let Some(c) = self.cwd.as_deref().filter(|s| !s.is_empty()) {
            let trimmed = c.trim_end_matches(['/', '\\']);
            if let Some(idx) = trimmed.rfind(['/', '\\']) {
                return trimmed[idx + 1..].to_owned();
            }
            return trimmed.to_owned();
        }
        self.seed.clone()
    }

    pub fn conversation_mode(&self) -> ConversationMode {
        match self.mode {
            Some(1) => ConversationMode::Plan,
            _ => ConversationMode::Code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_meta_display_title_fallbacks() {
        // last_summary 不得再当标题（2026-09）：它是最后回复预览，不是标题。
        let v = serde_json::json!({
            "seed": "0123abcd",
            "cwd": "F:\\code\\qaqh",
            "last_summary": "修复 SSE 解码",
            "archived": true,
            "running": true,
            "mode": 1
        });
        let meta = SessionMetaView::parse(&v).unwrap();
        assert_eq!(meta.display_title(), "qaqh");
        assert!(meta.archived && meta.running);
        assert_eq!(meta.conversation_mode(), ConversationMode::Plan);
    }

    #[test]
    fn session_meta_title_beats_last_summary() {
        // title 一旦存在（入站 LLM 总结已推送），永远优先于 last_summary。
        let v = serde_json::json!({
            "seed": "0123abcd",
            "title": "Bun 引导 daemon",
            "last_summary": "完成：Bun 引导真实 daemon 的 web 形态已验证"
        });
        let meta = SessionMetaView::parse(&v).unwrap();
        assert_eq!(meta.display_title(), "Bun 引导 daemon");
    }
}
