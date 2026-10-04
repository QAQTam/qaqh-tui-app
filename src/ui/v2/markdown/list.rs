//! 列表与列表项状态。

use ratatui::style::Style;

use super::text::StyledSpan;

/// 一层列表的状态：有序列表要记住下一个序号。
pub(super) struct ListState {
    pub(super) ordered: bool,
    pub(super) next: u64,
}

/// 当前正在累积的列表项。
pub(super) struct ItemState {
    pub(super) prefix: String,
    pub(super) continuation: String,
    pub(super) prefix_style: Style,
    pub(super) spans: Vec<StyledSpan>,
}
