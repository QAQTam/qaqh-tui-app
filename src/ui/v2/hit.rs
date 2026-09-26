//! 每帧命中图与权威几何。
//!
//! P0-A 只提供不依赖生产事件循环的基础设施：绘制层在真实 render 位置登记
//! `HitRegion`，终端层在整帧 flush 成功后发布 `FrameHitMap`，后续事件只通过
//! `FrameHitMap::resolve` 解释坐标。
//!
//! 这里刻意不保存任何 App 状态或屏幕坐标持久状态；`PointerTarget` 只表达语义
//! 目标，坐标只在当前帧的 HitMap 中短暂存在。

use std::fmt;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::MouseButton;
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use crate::app::{ModalHit, WorkspaceHit};
use crate::ui::v2::fullscreen::{MessageAction, MessageRole};
use crate::ui::v2::route::ScreenRoute;

/// 已真正提交到终端的绘制序号。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(u64);

impl FrameId {
    #[cfg(test)]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// 帧内可命中的语义目标。
///
/// 目标不携带屏幕坐标；需要坐标的动作从命中的 `HitRegion` 读取当前帧几何。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointerTarget {
    /// 整屏 Modal 阻断层；命中它表示“点在弹窗外”，不得穿透到底层。
    ModalRoot,
    Modal(ModalHit),
    Workspace(WorkspaceHit),
    Agent(AgentTarget),
    Scrollbar(ScrollbarPart),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTarget {
    BackToLatest,
    Message {
        turn_id: String,
        block_id: String,
        role: MessageRole,
    },
    /// 历史 thinking 正文展开/收起。
    Thinking {
        turn_id: String,
        block_id: String,
    },
    /// 工具卡正文展开/收起。
    Tool {
        turn_id: String,
        block_id: String,
    },
    MenuAction(MessageAction),
    /// 菜单外框（阻断层）：命中它表示"点在菜单里但不在可执行行上"，
    /// 不得穿透到底下的消息行。
    MenuRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPart {
    Track,
    Thumb,
}

/// 视觉锚点的期望；P0-C 的 strict probe 会按它检查实际 buffer。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorExpectation {
    /// 锚点必须在可见帧内，且该 cell 非空。
    NonEmptyCell,
    /// 锚点必须显示指定 glyph。
    Glyph(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualAnchor {
    pub position: Position,
    pub expectation: AnchorExpectation,
}

impl VisualAnchor {
    pub const fn non_empty(position: Position) -> Self {
        Self {
            position,
            expectation: AnchorExpectation::NonEmptyCell,
        }
    }

    pub const fn glyph(position: Position, glyph: char) -> Self {
        Self {
            position,
            expectation: AnchorExpectation::Glyph(glyph),
        }
    }
}

/// 当前帧里的一个可见命中区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitRegion {
    /// 屏幕绝对坐标。
    pub rect: Rect,
    /// 自身裁剪区，通常来自父容器 viewport。
    pub clip: Rect,
    /// 诊断用局部坐标。
    pub local_rect: Rect,
    pub target: PointerTarget,
    pub button: MouseButton,
    pub enabled: bool,
    pub z: u16,
    pub anchor: VisualAnchor,
}

impl HitRegion {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        rect: Rect,
        clip: Rect,
        local_rect: Rect,
        target: PointerTarget,
        button: MouseButton,
        enabled: bool,
        z: u16,
        anchor: VisualAnchor,
    ) -> Self {
        Self {
            rect,
            clip,
            local_rect,
            target,
            button,
            enabled,
            z,
            anchor,
        }
    }

    /// 自身 rect 与 clip 的可见交集；仍需再与 frame area 相交。
    pub fn visible_rect(&self) -> Rect {
        intersect(self.rect, self.clip)
    }
}

/// 一层级命中的诊断错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitMapError {
    EmptyRect {
        target: PointerTarget,
    },
    NoVisibleIntersection {
        target: PointerTarget,
        rect: Rect,
        clip: Rect,
    },
    OutsideFrame {
        target: PointerTarget,
        rect: Rect,
        frame: Rect,
    },
    AnchorOutside {
        target: PointerTarget,
        anchor: Position,
        rect: Rect,
    },
    SameZOverlap {
        z: u16,
        targets: Vec<PointerTarget>,
    },
}

/// 某一帧、某一路由下的全部命中区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHitMap {
    pub frame_id: FrameId,
    pub route: ScreenRoute,
    pub terminal_size: Size,
    pub scroll_offset: usize,
    pub regions: Vec<HitRegion>,
}

impl FrameHitMap {
    pub fn frame_area(&self) -> Rect {
        Rect::new(0, 0, self.terminal_size.width, self.terminal_size.height)
    }

    fn visible_rect(&self, region: &HitRegion) -> Rect {
        intersect(region.visible_rect(), self.frame_area())
    }

    /// 按半开区间、clip、enabled、button 和最高 z 解析目标。
    ///
    /// 同一点命中多个同 z 区域是规范错误；strict probe 会在发布帧前拒绝它。
    pub fn resolve(
        &self,
        column: u16,
        row: u16,
        button: MouseButton,
    ) -> Result<Option<&HitRegion>, HitMapError> {
        let mut top_z = None;
        let mut top: Vec<&HitRegion> = Vec::new();

        for region in &self.regions {
            if !region.enabled
                || region.button != button
                || !contains(self.visible_rect(region), column, row)
            {
                continue;
            }
            match top_z {
                None => {
                    top_z = Some(region.z);
                    top.push(region);
                }
                Some(z) if region.z > z => {
                    top_z = Some(region.z);
                    top.clear();
                    top.push(region);
                }
                Some(z) if region.z == z => top.push(region),
                Some(_) => {}
            }
        }

        match top.as_slice() {
            [] => Ok(None),
            [region] => Ok(Some(*region)),
            _ => Err(HitMapError::SameZOverlap {
                z: top_z.unwrap_or_default(),
                targets: top
                    .into_iter()
                    .map(|region| region.target.clone())
                    .collect(),
            }),
        }
    }

    /// P0-A 的帧级几何校验；P0-C 会在此基础上增加实际 buffer/anchor 探针。
    pub fn validate(&self) -> Result<(), Vec<HitMapError>> {
        let frame = self.frame_area();
        let mut errors = Vec::new();

        for region in &self.regions {
            if region.rect.is_empty() {
                errors.push(HitMapError::EmptyRect {
                    target: region.target.clone(),
                });
            }
            if intersect(region.rect, region.clip).is_empty() {
                errors.push(HitMapError::NoVisibleIntersection {
                    target: region.target.clone(),
                    rect: region.rect,
                    clip: region.clip,
                });
            }
            if !contains_rect(frame, region.rect) {
                errors.push(HitMapError::OutsideFrame {
                    target: region.target.clone(),
                    rect: region.rect,
                    frame,
                });
            }
            if !contains(
                region.rect,
                region.anchor.position.x,
                region.anchor.position.y,
            ) {
                errors.push(HitMapError::AnchorOutside {
                    target: region.target.clone(),
                    anchor: region.anchor.position,
                    rect: region.rect,
                });
            }
        }

        for (index, first) in self.regions.iter().enumerate() {
            for second in self.regions.iter().skip(index.saturating_add(1)) {
                if first.z == second.z
                    && !intersect(self.visible_rect(first), self.visible_rect(second)).is_empty()
                {
                    errors.push(HitMapError::SameZOverlap {
                        z: first.z,
                        targets: vec![first.target.clone(), second.target.clone()],
                    });
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// 绘制阶段收集 HitRegion 的 builder。
#[derive(Debug)]
pub struct HitMapBuilder {
    frame_id: FrameId,
    route: ScreenRoute,
    terminal_size: Size,
    scroll_offset: usize,
    regions: Vec<HitRegion>,
}

impl HitMapBuilder {
    pub fn new(
        frame_id: FrameId,
        route: ScreenRoute,
        terminal_size: Size,
        scroll_offset: usize,
    ) -> Self {
        Self {
            frame_id,
            route,
            terminal_size,
            scroll_offset,
            regions: Vec::new(),
        }
    }

    pub fn push(&mut self, region: HitRegion) {
        self.regions.push(region);
    }

    pub fn finish(self) -> FrameHitMap {
        FrameHitMap {
            frame_id: self.frame_id,
            route: self.route,
            terminal_size: self.terminal_size,
            scroll_offset: self.scroll_offset,
            regions: self.regions,
        }
    }
}

/// z 层级基线；局部子层只能在对应区间内细分。
pub mod z {
    pub const AGENT_MESSAGE: u16 = 10;
    pub const AGENT_SCROLLBAR: u16 = 30;
    /// Thumb is a higher-z sub-target inside the scrollbar track.
    pub const AGENT_SCROLLBAR_THUMB: u16 = 31;
    /// 浮层按钮（回到最新）。
    pub const AGENT_OVERLAY: u16 = 50;
    /// 消息菜单外框；菜单画在浮层按钮之上。
    pub const AGENT_MENU: u16 = 60;
    /// 消息菜单的可执行行；必须高于外框阻断层。
    pub const AGENT_MENU_ROW: u16 = 70;
    pub const WORKSPACE_ROW: u16 = 100;
    pub const WORKSPACE_FOOTER: u16 = 200;
    pub const MODAL_ROOT: u16 = 300;
    pub const MODAL_CONTENT: u16 = 310;
    pub const MODAL_BUTTON: u16 = 320;
}

/// ratatui `Rect` 的半开区间包含判定。
pub fn contains(rect: Rect, column: u16, row: u16) -> bool {
    !rect.is_empty()
        && column >= rect.x
        && column < rect.right()
        && row >= rect.y
        && row < rect.bottom()
}

pub fn contains_rect(outer: Rect, inner: Rect) -> bool {
    if inner.is_empty() {
        return false;
    }
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right() <= outer.right()
        && inner.bottom() <= outer.bottom()
}

pub fn intersect(a: Rect, b: Rect) -> Rect {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    if right <= left || bottom <= top {
        Rect::ZERO
    } else {
        Rect::new(left, top, right - left, bottom - top)
    }
}

/// 一行内容里第一个可见字形的屏幕位置。
///
/// 绘制层每行通常带内边距或 `▶`/空格前缀，锚点必须落在**字形**上：strict probe
/// 会拿它去实际 buffer 里查非空 cell。整行都是空白时退化成行首，交给探针报错。
pub fn line_anchor(line: &Line<'_>, rect: Rect) -> VisualAnchor {
    let mut offset = 0u16;
    for span in &line.spans {
        for ch in span.content.chars() {
            if !ch.is_whitespace() {
                let x = rect
                    .x
                    .saturating_add(offset)
                    .min(rect.right().saturating_sub(1));
                return VisualAnchor::non_empty(Position::new(x, rect.y));
            }
            offset = offset.saturating_add(
                u16::try_from(UnicodeWidthChar::width(ch).unwrap_or(0)).unwrap_or(u16::MAX),
            );
        }
    }
    VisualAnchor::non_empty(Position::new(rect.x, rect.y))
}

/// 用显式锚点构造命中区：`local_rect` 由 `rect - clip` 推出。空 rect 返回 `None`。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn anchor_region(
    rect: Rect,
    clip: Rect,
    target: PointerTarget,
    button: MouseButton,
    enabled: bool,
    z: u16,
    anchor: VisualAnchor,
) -> Option<HitRegion> {
    if rect.is_empty() {
        return None;
    }
    Some(HitRegion::new(
        rect,
        clip,
        Rect::new(
            rect.x.saturating_sub(clip.x),
            rect.y.saturating_sub(clip.y),
            rect.width,
            rect.height,
        ),
        target,
        button,
        enabled,
        z,
        anchor,
    ))
}

/// 用真实渲染的 `Line` 构造命中区：锚点取该行的第一个字形。
///
/// 这是 [`anchor_region`] 的常见特例；空 rect 同样返回 `None`。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn line_region(
    rect: Rect,
    clip: Rect,
    target: PointerTarget,
    button: MouseButton,
    enabled: bool,
    z: u16,
    line: &Line<'_>,
) -> Option<HitRegion> {
    anchor_region(
        rect,
        clip,
        target,
        button,
        enabled,
        z,
        line_anchor(line, rect),
    )
}

/// 命中探针开关（spec §8.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HitProbe {
    /// 默认关闭：只跑几何自检。
    #[default]
    Off,
    /// `QAQH_HIT_PROBE=1`：每帧自检，失败写结构化诊断，且**不发布**该帧。
    Warn,
    /// `QAQH_HIT_PROBE=strict`：失败直接返回错误。
    Strict,
}

impl HitProbe {
    /// 读取 `QAQH_HIT_PROBE`；未设置或其它值一律关闭。
    pub fn from_env() -> Self {
        match std::env::var("QAQH_HIT_PROBE").ok().as_deref() {
            Some("1") => Self::Warn,
            Some("strict") => Self::Strict,
            _ => Self::Off,
        }
    }

    pub const fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// 一条探针失败诊断；字段覆盖 spec §8.3 要求的全部定位信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    pub check: &'static str,
    pub frame_id: FrameId,
    pub route: ScreenRoute,
    pub target: Option<PointerTarget>,
    pub screen_rect: Rect,
    pub local_rect: Rect,
    pub z: u16,
    pub button: MouseButton,
    pub probe_point: Position,
    pub expected: Option<PointerTarget>,
    pub actual: Option<PointerTarget>,
    pub terminal_size: Size,
    pub scroll_offset: usize,
}

impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hit-probe check={} frame_id={} route={:?} target={:?} rect={:?} local_rect={:?} \
             z={} button={:?} point=({},{}) expected={:?} actual={:?} terminal={}x{} scroll={}",
            self.check,
            self.frame_id.get(),
            self.route,
            self.target,
            self.screen_rect,
            self.local_rect,
            self.z,
            self.button,
            self.probe_point.x,
            self.probe_point.y,
            self.expected,
            self.actual,
            self.terminal_size.width,
            self.terminal_size.height,
            self.scroll_offset,
        )
    }
}

/// 一次探针查询的结果；同 z 重叠单独成一类，避免被当成普通 miss。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeHit {
    Region(PointerTarget, u16),
    Miss,
    Overlap,
}

/// disabled 目标不允许被任何点解析到。
///
/// 单独抽成纯函数是为了让探针本身可证伪：`resolve` 结构上已经过滤 disabled，
/// 所以只有拿一个"伪造的命中"才能验证这条检查真的会红。
fn disabled_dispatchable(region: &HitRegion, hit: &ProbeHit) -> bool {
    !region.enabled && matches!(hit, ProbeHit::Region(target, _) if *target == region.target)
}

/// 高 z 区域在重叠点必须由 >= 它自己的 z 解析；否则就是 z 优先级没生效。
fn z_order_violated(high: &HitRegion, hit: &ProbeHit) -> bool {
    match hit {
        ProbeHit::Region(_, z) => *z < high.z,
        ProbeHit::Miss | ProbeHit::Overlap => true,
    }
}

impl FrameHitMap {
    fn failure(
        &self,
        region: Option<&HitRegion>,
        target: Option<PointerTarget>,
        check: &'static str,
        point: Position,
        expected: Option<PointerTarget>,
        actual: Option<PointerTarget>,
    ) -> ProbeFailure {
        ProbeFailure {
            check,
            frame_id: self.frame_id,
            route: self.route.clone(),
            target,
            screen_rect: region.map_or(Rect::ZERO, |region| region.rect),
            local_rect: region.map_or(Rect::ZERO, |region| region.local_rect),
            z: region.map_or(0, |region| region.z),
            button: region.map_or(MouseButton::Left, |region| region.button),
            probe_point: point,
            expected,
            actual,
            terminal_size: self.terminal_size,
            scroll_offset: self.scroll_offset,
        }
    }

    fn probe_hit(&self, column: u16, row: u16, button: MouseButton) -> ProbeHit {
        match self.resolve(column, row, button) {
            Ok(Some(region)) => ProbeHit::Region(region.target.clone(), region.z),
            Ok(None) => ProbeHit::Miss,
            Err(_) => ProbeHit::Overlap,
        }
    }

    /// 把 `validate` 的几何错误统一成探针诊断。
    fn geometry_failures(&self) -> Vec<ProbeFailure> {
        self.validate()
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|error| {
                let (check, target, point) = match &error {
                    HitMapError::EmptyRect { target } => ("empty_rect", target.clone(), None),
                    HitMapError::NoVisibleIntersection { target, .. } => {
                        ("invisible_rect", target.clone(), None)
                    }
                    HitMapError::OutsideFrame { target, .. } => {
                        ("outside_frame", target.clone(), None)
                    }
                    HitMapError::AnchorOutside { target, anchor, .. } => {
                        ("anchor_outside", target.clone(), Some(*anchor))
                    }
                    HitMapError::SameZOverlap { targets, .. } => (
                        "same_z_overlap",
                        targets.first().cloned().unwrap_or(PointerTarget::ModalRoot),
                        None,
                    ),
                };
                let region = self.regions.iter().find(|region| region.target == target);
                let point = point.unwrap_or_else(|| {
                    region.map_or(Position::new(0, 0), |region| {
                        Position::new(region.rect.x, region.rect.y)
                    })
                });
                self.failure(region, Some(target), check, point, None, None)
            })
            .collect()
    }

    /// 对**真实渲染 buffer** 做帧级自检（spec §8.2）。
    ///
    /// 语义上比 spec 字面稍宽一格：一个区域被**更高 z** 的目标覆盖时，允许该点
    /// 解析成覆盖层（`ModalRoot` 这种整屏阻断层天然被内容盖住），但**不允许**
    /// 被更低 z 或同 z 抢走。这样既锁住"看到的就能点"，又不会误报阻断层。
    pub fn probe(
        &self,
        buffer: &Buffer,
        expected_frame_id: FrameId,
        expected_route: &ScreenRoute,
    ) -> Result<(), Vec<ProbeFailure>> {
        let mut failures = Vec::new();

        if self.frame_id != expected_frame_id {
            failures.push(self.failure(
                None,
                None,
                "frame_id_mismatch",
                Position::new(0, 0),
                Some(PointerTarget::ModalRoot),
                None,
            ));
        }
        if &self.route != expected_route {
            failures.push(self.failure(
                None,
                None,
                "route_mismatch",
                Position::new(0, 0),
                None,
                None,
            ));
        }
        failures.extend(self.geometry_failures());

        for region in &self.regions {
            if region.rect.is_empty() {
                continue;
            }
            let expected = Some(region.target.clone());
            let mut points = vec![
                Position::new(region.rect.x, region.rect.y),
                Position::new(region.rect.right().saturating_sub(1), region.rect.y),
                Position::new(region.rect.x, region.rect.bottom().saturating_sub(1)),
                Position::new(
                    region.rect.right().saturating_sub(1),
                    region.rect.bottom().saturating_sub(1),
                ),
                Position::new(
                    region.rect.x.saturating_add(region.rect.width / 2),
                    region.rect.y.saturating_add(region.rect.height / 2),
                ),
            ];
            points.sort_by_key(|point| (point.y, point.x));
            points.dedup();

            for point in points {
                let hit = self.probe_hit(point.x, point.y, region.button);
                if disabled_dispatchable(region, &hit) {
                    failures.push(self.failure(
                        Some(region),
                        expected.clone(),
                        "disabled_dispatchable",
                        point,
                        None,
                        Some(region.target.clone()),
                    ));
                }
                if !region.enabled {
                    continue;
                }
                match hit {
                    ProbeHit::Region(target, _) if target == region.target => {}
                    // 被更高 z 覆盖：这是合法遮挡。
                    ProbeHit::Region(_, z) if z > region.z => {}
                    ProbeHit::Region(target, _) => failures.push(self.failure(
                        Some(region),
                        expected.clone(),
                        "point_lost_to_lower_z",
                        point,
                        expected.clone(),
                        Some(target),
                    )),
                    ProbeHit::Miss => failures.push(self.failure(
                        Some(region),
                        expected.clone(),
                        "point_missed",
                        point,
                        expected.clone(),
                        None,
                    )),
                    ProbeHit::Overlap => failures.push(self.failure(
                        Some(region),
                        expected.clone(),
                        "same_z_overlap",
                        point,
                        expected.clone(),
                        None,
                    )),
                }
            }

            // 外扩一格不得命中自己。
            for point in outside_points(region.rect, self.terminal_size) {
                if contains(region.rect, point.x, point.y) {
                    continue;
                }
                if matches!(self.probe_hit(point.x, point.y, region.button), ProbeHit::Region(target, _) if target == region.target)
                {
                    failures.push(self.failure(
                        Some(region),
                        expected.clone(),
                        "outside_reachable",
                        point,
                        None,
                        Some(region.target.clone()),
                    ));
                }
            }

            // 视觉锚点必须在真实 buffer 上成立。
            if !anchor_matches(buffer, region) {
                failures.push(self.failure(
                    Some(region),
                    expected.clone(),
                    "anchor_missing",
                    region.anchor.position,
                    expected.clone(),
                    None,
                ));
            }
        }

        // 高 z 覆盖低 z：重叠处必须由 >= 高 z 的目标解析。
        for (index, first) in self.regions.iter().enumerate() {
            for second in self.regions.iter().skip(index.saturating_add(1)) {
                let (low, high) = if first.z <= second.z {
                    (first, second)
                } else {
                    (second, first)
                };
                if low.z == high.z || !high.enabled || low.button != high.button {
                    continue;
                }
                let overlap = intersect(low.visible_rect(), high.visible_rect());
                if overlap.is_empty() {
                    continue;
                }
                let point = Position::new(
                    overlap.x.saturating_add(overlap.width / 2),
                    overlap.y.saturating_add(overlap.height / 2),
                );
                let hit = self.probe_hit(point.x, point.y, high.button);
                if z_order_violated(high, &hit) {
                    let actual = match hit {
                        ProbeHit::Region(target, _) => Some(target),
                        ProbeHit::Miss | ProbeHit::Overlap => None,
                    };
                    failures.push(self.failure(
                        Some(low),
                        Some(low.target.clone()),
                        "z_order_not_respected",
                        point,
                        Some(high.target.clone()),
                        actual,
                    ));
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }
}

/// 矩形的四个外扩邻居（越界/落在矩形内的点会被调用方过滤）。
fn outside_points(rect: Rect, size: Size) -> Vec<Position> {
    let candidates = [
        (rect.x.saturating_sub(1), rect.y),
        (rect.x, rect.y.saturating_sub(1)),
        (rect.right(), rect.y),
        (rect.x, rect.bottom()),
    ];
    candidates
        .into_iter()
        .filter(|(x, y)| *x < size.width && *y < size.height)
        .map(|(x, y)| Position::new(x, y))
        .collect()
}

/// 锚点在真实 buffer 上是否成立（存在 + 非空 / 指定 glyph）。
fn anchor_matches(buffer: &Buffer, region: &HitRegion) -> bool {
    let position = region.anchor.position;
    if !contains(buffer.area, position.x, position.y) {
        return false;
    }
    let symbol = buffer[(position.x, position.y)].symbol();
    match region.anchor.expectation {
        AnchorExpectation::NonEmptyCell => !symbol.trim().is_empty(),
        AnchorExpectation::Glyph(glyph) => symbol == glyph.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ModalHit, WorkspaceHit};
    use crate::ui::v2::route::WorkspaceRoute;

    fn target(name: &str) -> PointerTarget {
        match name {
            "permission" => PointerTarget::Modal(ModalHit::PermissionApprove),
            "workspace" => PointerTarget::Workspace(WorkspaceHit::Back),
            _ => PointerTarget::Agent(AgentTarget::BackToLatest),
        }
    }

    fn region(name: &str, rect: Rect, z: u16) -> HitRegion {
        HitRegion::new(
            rect,
            Rect::new(0, 0, 100, 50),
            Rect::new(0, 0, rect.width, rect.height),
            target(name),
            MouseButton::Left,
            true,
            z,
            VisualAnchor::glyph(Position::new(rect.x, rect.y), 'x'),
        )
    }

    fn map(regions: Vec<HitRegion>) -> FrameHitMap {
        FrameHitMap {
            frame_id: FrameId::new(7),
            route: ScreenRoute::Agent,
            terminal_size: Size::new(100, 50),
            scroll_offset: 0,
            regions,
        }
    }

    #[test]
    fn contains_uses_half_open_bounds_and_rejects_empty_rects() {
        let rect = Rect::new(2, 3, 4, 5);
        assert!(contains(rect, 2, 3), "top-left corner is inside");
        assert!(contains(rect, 5, 7), "bottom-right cell is inside");
        assert!(!contains(rect, 6, 7), "right edge is exclusive");
        assert!(!contains(rect, 5, 8), "bottom edge is exclusive");
        assert!(!contains(Rect::ZERO, 0, 0), "empty rect has no hit");
        assert!(!contains(Rect::new(2, 3, 0, 5), 2, 3));
        assert!(!contains(Rect::new(2, 3, 4, 0), 2, 3));
    }

    #[test]
    fn intersect_and_contains_rect_handle_clip_and_empty_geometry() {
        let a = Rect::new(0, 2, 10, 8);
        let b = Rect::new(8, 0, 10, 10);
        assert_eq!(intersect(a, b), Rect::new(8, 2, 2, 8));
        assert!(intersect(a, Rect::new(20, 20, 2, 2)).is_empty());

        assert!(contains_rect(
            Rect::new(0, 0, 10, 10),
            Rect::new(1, 1, 8, 8)
        ));
        assert!(!contains_rect(
            Rect::new(0, 0, 10, 10),
            Rect::new(1, 1, 10, 8)
        ));
        assert!(!contains_rect(Rect::new(0, 0, 10, 10), Rect::ZERO));
    }

    #[test]
    fn line_region_anchors_on_first_glyph_and_rejects_empty_rects() {
        let line = Line::from(vec![
            ratatui::text::Span::raw("   "),
            ratatui::text::Span::raw("1 ◉ 方案乙"),
        ]);
        let rect = Rect::new(10, 4, 20, 1);
        let region = line_region(
            rect,
            Rect::new(0, 0, 40, 12),
            target("workspace"),
            MouseButton::Left,
            true,
            z::WORKSPACE_ROW,
            &line,
        )
        .expect("non-empty rect yields a region");
        assert_eq!(
            region.anchor.position,
            Position::new(13, 4),
            "锚点必须跳过行首空格，落在第一个字形上"
        );
        assert_eq!(region.local_rect, Rect::new(10, 4, 20, 1));

        assert!(
            line_region(
                Rect::ZERO,
                Rect::new(0, 0, 40, 12),
                target("workspace"),
                MouseButton::Left,
                true,
                z::WORKSPACE_ROW,
                &line,
            )
            .is_none(),
            "空 rect 不登记"
        );
    }

    #[test]
    fn anchor_region_offsets_local_rect_by_clip_and_rejects_empty() {
        let clip = Rect::new(3, 2, 20, 10);
        let rect = Rect::new(5, 4, 6, 2);
        let region = anchor_region(
            rect,
            clip,
            target("agent"),
            MouseButton::Left,
            false,
            z::AGENT_OVERLAY,
            VisualAnchor::non_empty(Position::new(5, 4)),
        )
        .expect("non-empty rect yields a region");
        assert_eq!(region.local_rect, Rect::new(2, 2, 6, 2));
        assert!(!region.enabled);
        assert!(
            anchor_region(
                Rect::ZERO,
                clip,
                target("agent"),
                MouseButton::Left,
                true,
                z::AGENT_OVERLAY,
                VisualAnchor::non_empty(Position::new(0, 0)),
            )
            .is_none(),
            "空 rect 不登记"
        );
    }

    #[test]
    fn frame_id_is_monotonic() {
        let id = FrameId::new(3);
        assert_eq!(id.next(), FrameId::new(4));
        assert_eq!(FrameId::new(u64::MAX).next(), FrameId::new(u64::MAX));
    }

    #[test]
    fn resolve_prefers_higher_z_and_filters_disabled() {
        let mut low = region("agent", Rect::new(0, 0, 10, 2), 0);
        low.enabled = false;
        let middle = region("workspace", Rect::new(0, 0, 10, 2), z::WORKSPACE_ROW);
        let high = region("permission", Rect::new(2, 0, 2, 1), z::MODAL_BUTTON);
        let map = map(vec![low, middle, high]);

        let hit = map
            .resolve(2, 0, MouseButton::Left)
            .expect("no overlap")
            .expect("hit");
        assert_eq!(hit.target, target("permission"));

        let hit = map
            .resolve(8, 0, MouseButton::Left)
            .expect("no overlap")
            .expect("hit");
        assert_eq!(hit.target, target("workspace"));
    }

    #[test]
    fn resolve_reports_same_z_overlap() {
        let map = map(vec![
            region("agent", Rect::new(0, 0, 5, 1), z::AGENT_MESSAGE),
            region("workspace", Rect::new(4, 0, 5, 1), z::AGENT_MESSAGE),
        ]);
        let error = map
            .resolve(4, 0, MouseButton::Left)
            .expect_err("same-z overlap must be rejected");
        assert!(matches!(
            error,
            HitMapError::SameZOverlap {
                z: z::AGENT_MESSAGE,
                ..
            }
        ));
    }

    #[test]
    fn validate_rejects_invisible_region_and_bad_anchor() {
        let mut clipped = region("agent", Rect::new(0, 0, 10, 1), 0);
        clipped.clip = Rect::new(20, 20, 2, 2);
        let mut outside = region("workspace", Rect::new(99, 49, 4, 4), z::WORKSPACE_ROW);
        outside.anchor.position = Position::new(0, 0);
        let errors = map(vec![clipped, outside])
            .validate()
            .expect_err("invalid frame must fail");
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, HitMapError::NoVisibleIntersection { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, HitMapError::OutsideFrame { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, HitMapError::AnchorOutside { .. }))
        );
    }

    #[test]
    fn builder_preserves_frame_context() {
        let mut builder = HitMapBuilder::new(
            FrameId::new(11),
            ScreenRoute::Workspace(crate::ui::v2::route::WorkspaceRoute::Help),
            Size::new(80, 24),
            9,
        );
        builder.push(region(
            "workspace",
            Rect::new(0, 23, 8, 1),
            z::WORKSPACE_FOOTER,
        ));
        let map = builder.finish();
        assert_eq!(map.frame_id, FrameId::new(11));
        assert_eq!(map.terminal_size, Size::new(80, 24));
        assert_eq!(map.scroll_offset, 9);
        assert_eq!(map.regions.len(), 1);
    }
    // ───────────────────── 探针（spec §8）反例测试 ─────────────────────

    /// 只有一个字形 `x` 的 buffer：任何错位的锚点都会落在空白 cell 上。
    fn probe_buffer() -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 6));
        buffer[(2, 1)].set_symbol("x");
        buffer
    }

    fn probe_region(target: PointerTarget, rect: Rect, anchor: Position) -> HitRegion {
        HitRegion::new(
            rect,
            Rect::new(0, 0, 20, 6),
            rect,
            target,
            MouseButton::Left,
            true,
            z::WORKSPACE_FOOTER,
            VisualAnchor::non_empty(anchor),
        )
    }

    fn probe_region_back(rect: Rect, anchor: Position) -> HitRegion {
        probe_region(PointerTarget::Workspace(WorkspaceHit::Back), rect, anchor)
    }

    fn probe_map(regions: Vec<HitRegion>) -> FrameHitMap {
        FrameHitMap {
            frame_id: FrameId::new(5),
            route: ScreenRoute::Agent,
            terminal_size: Size::new(20, 6),
            scroll_offset: 0,
            regions,
        }
    }

    fn probe_ok(map: &FrameHitMap, buffer: &Buffer) -> bool {
        map.probe(buffer, map.frame_id, &map.route).is_ok()
    }

    /// 正向对照：合法帧必须通过；否则下面所有"必须失败"都没有意义。
    #[test]
    fn probe_accepts_a_well_formed_frame() {
        let buffer = probe_buffer();
        let map = probe_map(vec![probe_region_back(
            Rect::new(2, 1, 1, 1),
            Position::new(2, 1),
        )]);
        assert!(probe_ok(&map, &buffer));
    }

    /// 反例：rect 右移一列 / 下移一行后，锚点落到空白 → 必须报 `anchor_missing`。
    #[test]
    fn probe_detects_shifted_rect() {
        let buffer = probe_buffer();
        for rect in [Rect::new(3, 1, 1, 1), Rect::new(2, 2, 1, 1)] {
            let map = probe_map(vec![probe_region_back(rect, Position::new(rect.x, rect.y))]);
            let failures = map
                .probe(&buffer, map.frame_id, &map.route)
                .expect_err("错位 rect 必须被探针抓到");
            assert!(
                failures
                    .iter()
                    .any(|failure| failure.check == "anchor_missing"),
                "{rect:?} → {failures:?}"
            );
        }
    }

    /// 反例：rect 正确但锚点落在空白 cell → 必须报 `anchor_missing`。
    #[test]
    fn probe_detects_blank_anchor() {
        let buffer = probe_buffer();
        let map = probe_map(vec![probe_region_back(
            Rect::new(2, 1, 3, 1),
            Position::new(4, 1),
        )]);
        let failures = map
            .probe(&buffer, map.frame_id, &map.route)
            .expect_err("空白锚点必须被探针抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "anchor_missing"),
            "{failures:?}"
        );
    }

    /// 反例：两个同 z 区域重叠 → 必须报 `same_z_overlap`。
    #[test]
    fn probe_detects_same_z_overlap() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 6));
        for x in 2..6 {
            buffer[(x, 1)].set_symbol("x");
        }
        let map = probe_map(vec![
            probe_region_back(Rect::new(2, 1, 3, 1), Position::new(2, 1)),
            probe_region(
                PointerTarget::Workspace(WorkspaceHit::SessionRow(0)),
                Rect::new(3, 1, 3, 1),
                Position::new(3, 1),
            ),
        ]);
        let failures = map
            .probe(&buffer, map.frame_id, &map.route)
            .expect_err("同 z 重叠必须被探针抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "same_z_overlap"),
            "{failures:?}"
        );
    }

    /// 反例：route / frame_id 与"当前这一帧"不一致 → 必须报错。
    #[test]
    fn probe_detects_frame_identity_mismatch() {
        let buffer = probe_buffer();
        let map = probe_map(vec![probe_region_back(
            Rect::new(2, 1, 1, 1),
            Position::new(2, 1),
        )]);

        let failures = map
            .probe(&buffer, FrameId::new(6), &ScreenRoute::Agent)
            .expect_err("frame_id 不一致必须被抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "frame_id_mismatch"),
            "{failures:?}"
        );

        let failures = map
            .probe(
                &buffer,
                FrameId::new(5),
                &ScreenRoute::Workspace(WorkspaceRoute::Help),
            )
            .expect_err("route 不一致必须被抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "route_mismatch"),
            "{failures:?}"
        );
    }

    /// 反例：不可见 rect / 越出可见帧 → 必须报几何错误。
    #[test]
    fn probe_detects_invisible_and_out_of_frame_rects() {
        let buffer = probe_buffer();

        let mut invisible = probe_region_back(Rect::new(2, 1, 1, 1), Position::new(2, 1));
        invisible.clip = Rect::new(10, 4, 5, 2);
        let map = probe_map(vec![invisible]);
        let failures = map
            .probe(&buffer, map.frame_id, &map.route)
            .expect_err("不可见 rect 必须被探针抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "invisible_rect"),
            "{failures:?}"
        );

        let map = probe_map(vec![probe_region_back(
            Rect::new(18, 1, 5, 1),
            Position::new(18, 1),
        )]);
        let failures = map
            .probe(&buffer, map.frame_id, &map.route)
            .expect_err("越帧 rect 必须被探针抓到");
        assert!(
            failures
                .iter()
                .any(|failure| failure.check == "outside_frame"),
            "{failures:?}"
        );
    }

    /// 探针自身必须可证伪：`resolve` 结构上已经保证的两条不变量
    /// （disabled 不可 dispatch、高 z 覆盖低 z）用"伪造命中"直接验证检查会红。
    #[test]
    fn probe_check_predicates_are_falsifiable() {
        let mut disabled = probe_region_back(Rect::new(2, 1, 1, 1), Position::new(2, 1));
        disabled.enabled = false;
        let hit_self = ProbeHit::Region(disabled.target.clone(), disabled.z);
        assert!(
            disabled_dispatchable(&disabled, &hit_self),
            "disabled 命中自己必须报错"
        );
        assert!(!disabled_dispatchable(&disabled, &ProbeHit::Miss));
        let mut enabled = disabled.clone();
        enabled.enabled = true;
        assert!(!disabled_dispatchable(&enabled, &hit_self));

        let high = probe_region_back(Rect::new(2, 1, 4, 1), Position::new(2, 1));
        let lower = ProbeHit::Region(
            PointerTarget::Workspace(WorkspaceHit::SessionRow(0)),
            high.z.saturating_sub(1),
        );
        assert!(z_order_violated(&high, &lower), "高 z 被低 z 抢走必须报错");
        assert!(z_order_violated(&high, &ProbeHit::Miss));
        assert!(!z_order_violated(
            &high,
            &ProbeHit::Region(high.target.clone(), high.z)
        ));
        assert!(!z_order_violated(
            &high,
            &ProbeHit::Region(high.target.clone(), high.z.saturating_add(1))
        ));
    }

    /// 诊断必须带齐 spec §8.3 的全部字段。
    #[test]
    fn probe_diagnostic_carries_every_required_field() {
        let buffer = probe_buffer();
        let map = probe_map(vec![probe_region_back(
            Rect::new(3, 1, 1, 1),
            Position::new(3, 1),
        )]);
        let failures = map
            .probe(&buffer, map.frame_id, &map.route)
            .expect_err("错位必须被抓到");
        let failure = failures.first().expect("至少一条诊断");
        let rendered = failure.to_string();
        for field in [
            "frame_id=",
            "route=",
            "target=",
            "rect=",
            "local_rect=",
            "z=",
            "button=",
            "point=",
            "expected=",
            "actual=",
            "terminal=",
            "scroll=",
        ] {
            assert!(rendered.contains(field), "诊断缺字段 {field}：{rendered}");
        }
    }

    #[test]
    fn hit_probe_env_switch_maps_values() {
        // 只在环境里确实没设时断言默认值，避免被 `QAQH_HIT_PROBE=strict` 的
        // 外层命令带崩。
        if std::env::var("QAQH_HIT_PROBE").is_err() {
            assert_eq!(HitProbe::from_env(), HitProbe::Off);
        }
        assert!(!HitProbe::Off.enabled());
        assert!(HitProbe::Warn.enabled());
        assert!(HitProbe::Strict.enabled());
    }
}
