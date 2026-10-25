//! 单会话的界面状态:timeline reducer 与挂起交互面板。
//!
//! 协议类型一律取自 `qaqh-client` 的权威出口(与 TUI 同一条纪律:壳层不手抄
//! wire 类型)。本文件只维护两类跨事件不变量:
//!
//! 1. **timeline 聚合** —— `TimelineEntry` 事件流在界面上重放出「回合 → 轮次 →
//!    块」树;快照页(TimelineSnapshot)是权威替换,事件只对快照之上的增量生效。
//! 2. **交互生命周期** —— 未决交互按身份记账(first-answer-wins / 不得重开),
//!    面板正文(permission/ask/plan)直接从 canonical body JSON 构造。

use std::collections::BTreeSet;

use qaqh_client::{
    ClientV2Bootstrap, ClientV2Delivery, DomainAskQuestion, TimelineBlock, TimelineBlockKind,
    TimelineBlockState, TimelineEntry, TimelineEvent, TimelineFailure, TimelinePage,
    TimelineTool, TimelineTurn, TimelineTurnState, UsageInfo,
};

// ───────────────────────── 时间线聚合 ─────────────────────────

/// 一个模型轮次(一次 API 调用)的块序列。
#[derive(Debug)]
pub struct Round {
    pub round_num: u32,
    pub sealed: bool,
    pub blocks: Vec<Block>,
}

/// 转录块。文本块持有增量解析的 markdown 视图(渲染层转 owned 元素)。
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Text 变体大是本质:markdown 视图就地缓存
pub enum Block {
    Text {
        id: String,
        state: TimelineBlockState,
        md: iced::widget::markdown::Content,
    },
    Reasoning {
        id: String,
        state: TimelineBlockState,
        raw: String,
    },
    Tool {
        id: String,
        tool: TimelineTool,
        progress: String,
    },
    Notice {
        id: String,
        text: String,
    },
}

impl Block {
    fn id(&self) -> &str {
        match self {
            Block::Text { id, .. }
            | Block::Reasoning { id, .. }
            | Block::Tool { id, .. }
            | Block::Notice { id, .. } => id,
        }
    }

    fn from_timeline(block: &TimelineBlock) -> Self {
        let state = block.state;
        match block.kind {
            TimelineBlockKind::Text => Block::Text {
                id: block.block_id.clone(),
                state,
                md: iced::widget::markdown::Content::parse(&block.text),
            },
            TimelineBlockKind::Reasoning => Block::Reasoning {
                id: block.block_id.clone(),
                state,
                raw: block.text.clone(),
            },
            TimelineBlockKind::Tool => {
                let tool = block.tool.clone().unwrap_or_else(|| TimelineTool {
                    tool_call_id: block.block_id.clone(),
                    name: String::new(),
                    state: qaqh_client::TimelineToolState::Prepared,
                    summary: None,
                    args_json: None,
                    output: None,
                    diff: None,
                    progress: String::new(),
                    progress_truncated: false,
                    progress_stream: None,
                    progress_bytes_total: 0,
                    display: None,
                    failure: None,
                    permission: None,
                    exit_code: None,
                    completed_at_ms: None,
                });
                Block::Tool {
                    id: block.block_id.clone(),
                    // TimelineBlock 本身没有 progress 槽,从 tool 里取。
                    progress: block
                        .tool
                        .as_ref()
                        .map(|tool| tool.progress.clone())
                        .unwrap_or_default(),
                    tool: bound_tool(tool),
                }
            }
            TimelineBlockKind::Notice => Block::Notice {
                id: block.block_id.clone(),
                text: block.text.clone(),
            },
        }
    }

    /// 追加流式文本增量。
    fn append_text(&mut self, delta: &str) {
        match self {
            Block::Text { md, .. } => md.push_str(delta),
            Block::Reasoning { raw, .. } => raw.push_str(delta),
            _ => {}
        }
    }

    /// 用整段文本覆盖(checkpoint 全量形态)。
    fn replace_text(&mut self, text: &str) {
        match self {
            Block::Text { md, .. } => *md = iced::widget::markdown::Content::parse(text),
            Block::Reasoning { raw, .. } => *raw = text.to_string(),
            _ => {}
        }
    }

    fn seal(&mut self) {
        match self {
            Block::Text { state, .. } | Block::Reasoning { state, .. } => {
                *state = TimelineBlockState::Sealed;
            }
            _ => {}
        }
    }
}

/// 一个回合(用户输入 + 全部轮次)。
#[derive(Debug)]
pub struct Turn {
    pub turn_id: String,
    /// 分页游标用;实时追加的回合没有。
    pub turn_index: Option<u64>,
    pub user_text: String,
    pub state: TimelineTurnState,
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<Round>,
    /// 渲染缓存代数:本回合内容每被改动一次就 +1。lazy 视图以
    /// `(turn_id, revision)` 为依赖 —— 封口回合停止增长,永不重建;
    /// 活跃回合随流式增量逐帧重建(这是唯一该重建的)。
    pub revision: u64,
}

impl Turn {
    fn from_timeline(turn: &TimelineTurn) -> Self {
        Self {
            turn_id: turn.turn_id.clone(),
            turn_index: turn.turn_index,
            user_text: turn.user_text.clone(),
            state: turn.state,
            failure: turn.failure.clone(),
            revision: 0,
            rounds: turn
                .rounds
                .iter()
                .map(|round| Round {
                    round_num: round.round_num,
                    sealed: round.sealed,
                    blocks: round.blocks.iter().map(Block::from_timeline).collect(),
                })
                .collect(),
        }
    }

    fn ensure_round(&mut self, round_num: u32) -> &mut Round {
        if let Some(position) = self.rounds.iter().position(|round| round.round_num == round_num)
        {
            return &mut self.rounds[position];
        }
        self.rounds.push(Round {
            round_num,
            sealed: false,
            blocks: Vec::new(),
        });
        self.rounds.last_mut().expect("just pushed")
    }
}

/// 前端投影里工具输出的保留上限(UI 只展示尾部窗口;磁盘归档是事实源)。
const TOOL_OUTPUT_WINDOW: usize = 16 * 1024;

/// 把工具输出族字段裁到尾部窗口,UTF-8 边界安全。语义无损:
/// 渲染层本来就只取尾部 ~1.6k 字符,这里拦的是内存而不是像素。
fn bound_tool(mut tool: TimelineTool) -> TimelineTool {
    if let Some(output) = &mut tool.output {
        *output = tail_window(output, TOOL_OUTPUT_WINDOW);
    }
    if let Some(display) = &mut tool.display {
        match &mut display.body {
            Some(qaqh_client::TimelineToolBody::Text { text, .. }) => {
                *text = tail_window(text, TOOL_OUTPUT_WINDOW);
            }
            Some(qaqh_client::TimelineToolBody::Shell { output, .. }) => {
                *output = tail_window(output, TOOL_OUTPUT_WINDOW);
            }
            Some(qaqh_client::TimelineToolBody::Streams { stdout, stderr, .. }) => {
                *stdout = tail_window(stdout, TOOL_OUTPUT_WINDOW);
                *stderr = tail_window(stderr, TOOL_OUTPUT_WINDOW);
            }
            _ => {}
        }
    }
    tool
}

/// 取尾部至多 `max_chars` 个字符(超长时以 … 起头)。
fn tail_window(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().skip(count - max_chars).collect();
    format!("…{truncated}")
}

// ───────────────────────── 挂起交互面板 ─────────────────────────

/// 权限确认面板(canonical permission body 投影)。
#[derive(Debug, Clone)]
pub struct PermissionPanel {
    pub interaction_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action_summary: Option<String>,
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u8,
    pub risk: String,
    pub consequence: String,
    pub trust_folder: bool,
}

/// ask 面板:问题清单 + 逐题选择/自定义输入。
#[derive(Debug)]
pub struct AskPanel {
    pub interaction_id: String,
    pub mode: qaqh_client::AskMode,
    pub questions: Vec<DomainAskQuestion>,
    pub selections: Vec<Option<usize>>,
    pub customs: Vec<String>,
}

impl AskPanel {
    /// 逐题收集答案;有未回答的题时返回 `None`。
    pub fn collect_answers(&self) -> Option<Vec<qaqh_client::AskAnswer>> {
        let mut answers = Vec::new();
        for (index, question) in self.questions.iter().enumerate() {
            let answer = self.selections
                .get(index)
                .and_then(|selected| selected.map(|i| question.options[i].clone()))
                .or_else(|| {
                    let custom = self.customs.get(index)?;
                    (!custom.trim().is_empty()).then(|| custom.trim().to_string())
                });
            answers.push(qaqh_client::AskAnswer {
                question_id: question.id.clone(),
                answer: answer?,
            });
        }
        Some(answers)
    }
}

/// plan 评审面板。
#[derive(Debug)]
pub struct PlanPanel {
    pub interaction_id: String,
    /// plan_content 的 markdown 解析缓存(渲染层借 items 用)。
    pub md: iced::widget::markdown::Content,
    pub review_type: String,
    pub message: String,
}

/// 当前挂起的交互(composer 上方的模态卡)。
#[derive(Debug)]
pub enum Pending {
    Permission(PermissionPanel),
    Ask(AskPanel),
    Plan(PlanPanel),
}

impl Pending {
    pub fn interaction_id(&self) -> &str {
        match self {
            Pending::Permission(panel) => &panel.interaction_id,
            Pending::Ask(panel) => &panel.interaction_id,
            Pending::Plan(panel) => &panel.interaction_id,
        }
    }
}

/// 从 canonical 交互正文 JSON 构造面板。
///
/// 正文由 `qaqh-domain` 的 `interaction_body` 单点序列化;逐字段 serde default,
/// 畸形字段归默认值而不是整块丢弃(对齐 TUI 的类型化地板)。
pub fn parse_interaction_body(
    bytes: &[u8],
    interaction_id: &str,
    call_id: Option<&str>,
) -> Option<Pending> {
    #[derive(serde::Deserialize)]
    struct AskBody {
        #[serde(default = "default_ask_mode")]
        mode: qaqh_client::AskMode,
        #[serde(default)]
        questions: Vec<DomainAskQuestion>,
    }
    fn default_ask_mode() -> qaqh_client::AskMode {
        qaqh_client::AskMode::Single
    }
    #[derive(serde::Deserialize)]
    struct PermissionBody {
        #[serde(default)]
        tool_name: String,
        #[serde(default)]
        action_summary: Option<String>,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default)]
        category: String,
        #[serde(default)]
        level: u8,
        #[serde(default)]
        risk: String,
        #[serde(default)]
        consequence: String,
    }

    let body: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match body.get("kind").and_then(serde_json::Value::as_str) {
        Some("ask") => {
            let parsed: AskBody = serde_json::from_value(body).ok()?;
            let mut questions = parsed.questions;
            // 零问题的 ask 不可交互(空面板吞输入):合成兜底问题,允许自定义。
            if questions.is_empty() {
                questions.push(DomainAskQuestion {
                    id: "fallback".into(),
                    question: "模型请求确认(正文未携带问题清单)".into(),
                    options: Vec::new(),
                    allow_custom: true,
                });
            }
            let count = questions.len();
            Some(Pending::Ask(AskPanel {
                interaction_id: interaction_id.to_string(),
                mode: parsed.mode,
                questions,
                selections: vec![None; count],
                customs: vec![String::new(); count],
            }))
        }
        Some("permission") => {
            let call_id = call_id?;
            let mut parsed: PermissionBody = serde_json::from_value(body).ok()?;
            let tool_name = if parsed.tool_name.is_empty() {
                "工具".to_string()
            } else {
                std::mem::take(&mut parsed.tool_name)
            };
            Some(Pending::Permission(PermissionPanel {
                interaction_id: interaction_id.to_string(),
                tool_call_id: call_id.to_string(),
                tool_name,
                action_summary: parsed.action_summary,
                reason: parsed.reason,
                paths: parsed.paths,
                category: parsed.category,
                level: parsed.level,
                risk: parsed.risk,
                consequence: parsed.consequence,
                trust_folder: false,
            }))
        }
        Some("plan") => {
            let plan_content = body
                .get("plan_content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let md = iced::widget::markdown::Content::parse(&plan_content);
            Some(Pending::Plan(PlanPanel {
                interaction_id: interaction_id.to_string(),
                md,
                review_type: body
                    .get("review_type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("plan")
                    .to_string(),
                message: String::new(),
            }))
        }
        _ => None,
    }
}

// ───────────────────────── 会话状态 ─────────────────────────

/// 单会话聚合状态:timeline + v2 时序闸 + 挂起交互。
pub struct SessionState {
    pub session_id: String,
    pub title: String,
    /// turn_id → turns 下标(事件定位 O(1);prepend/整页替换时重建)。
    turn_positions: std::collections::HashMap<String, usize>,
    /// daemon 物化窗口未覆盖历史开头(更早回合存在但翻页取不到)。
    pub truncated_before: bool,
    // v2 时序闸(对齐 TUI ringing_v2.rs 的不变量)
    pub server_epoch: Option<String>,
    pub log_id: Option<String>,
    pub cursor: Option<String>,
    pub last_fact_seq: u64,
    pub last_projection_index: u16,
    pub state_revision: u64,
    /// 有待处理 reset:保留旧状态,新 bootstrap 校验通过后原子替换。
    pub reset_epoch: Option<String>,
    /// reset 宣告只读(SnapshotMissing / UpgradeRequired):不给写入口。
    pub read_only: bool,
    // 交互身份账本(first-answer-wins)
    pub pending_interactions: BTreeSet<String>,
    pub terminal_interactions: BTreeSet<String>,
    /// 本壳已经答过的交互:补投的同 id 请求不再弹(防幽灵面板)。
    pub suppressed_interactions: BTreeSet<String>,
    // 界面投影
    pub turns: Vec<Turn>,
    pub has_more: bool,
    pub total_turns: usize,
    pub loading_older: bool,
    pub activity: Option<qaqh_client::ClientV2ActivityState>,
    pub usage: Option<UsageInfo>,
    pub pending: Option<Pending>,
}

impl SessionState {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            title: String::new(),
            turn_positions: std::collections::HashMap::new(),
            truncated_before: false,
            server_epoch: None,
            log_id: None,
            cursor: None,
            last_fact_seq: 0,
            last_projection_index: 0,
            state_revision: 0,
            reset_epoch: None,
            read_only: false,
            pending_interactions: BTreeSet::new(),
            terminal_interactions: BTreeSet::new(),
            suppressed_interactions: BTreeSet::new(),
            turns: Vec::new(),
            has_more: false,
            total_turns: 0,
            loading_older: false,
            activity: None,
            usage: None,
            pending: None,
        }
    }

    /// 权威 bootstrap 原子替换时序基线与未决交互。
    ///
    /// reset 后的 bootstrap 必须指向 reset 宣告的 epoch(旧响应不能滚回)。
    pub fn apply_bootstrap(&mut self, bootstrap: &ClientV2Bootstrap) {
        if bootstrap.server_epoch.trim().is_empty() {
            return;
        }
        if let Some(reset_epoch) = &self.reset_epoch
            && &bootstrap.server_epoch != reset_epoch
        {
            return;
        }
        let revision = bootstrap
            .control
            .state_revision
            .max(bootstrap.conversation.state_revision)
            .max(bootstrap.tool.state_revision);
        if self.reset_epoch.is_none()
            && self.server_epoch.as_deref() == Some(bootstrap.server_epoch.as_str())
            && revision < self.state_revision
        {
            return; // 同 epoch 内的过期快照
        }
        self.server_epoch = Some(bootstrap.server_epoch.clone());
        self.cursor = Some(bootstrap.snapshot_cursor.as_str().to_string());
        self.state_revision = revision;
        self.reset_epoch = None;
        self.read_only = false;
        self.activity = Some(bootstrap.control.state.activity);
        // 权威 bootstrap 的 interactions 已是 daemon 过滤后的未决集合。
        self.pending_interactions = bootstrap
            .control
            .state
            .interactions
            .iter()
            .map(|interaction| interaction.interaction_id.clone())
            .collect();
        for interaction in &bootstrap.control.state.interactions {
            self.restore_interaction(interaction);
        }
    }

    /// v2 事件准入闸:epoch/序号单调 + 去重。返回 false = 丢弃。
    pub fn admit_v2_event(&mut self, event: &qaqh_client::ClientV2Event) -> bool {
        if self.reset_epoch.is_some() {
            return false; // 等新 bootstrap,事件一概不收
        }
        let Some(current_epoch) = self.server_epoch.as_deref() else {
            return false; // bootstrap 前到达的事件无从校验
        };
        if event.server_epoch != current_epoch {
            return false;
        }
        match event.delivery {
            ClientV2Delivery::Reliable => {
                let (Some(fact_seq), Some(projection_index), Some(_), Some(revision)) = (
                    event.fact_seq,
                    event.projection_index,
                    event.cursor.as_ref(),
                    event.revision,
                ) else {
                    return false;
                };
                let Some(log_id) = event.log_id.as_deref() else {
                    return false;
                };
                match &self.log_id {
                    Some(current) if current != log_id => return false,
                    None => self.log_id = Some(log_id.to_string()),
                    _ => {}
                }
                let incoming = (fact_seq, projection_index);
                let current = (self.last_fact_seq, self.last_projection_index);
                if incoming <= current {
                    return false;
                }
                self.cursor = event.cursor.as_ref().map(|token| token.as_str().to_string());
                self.last_fact_seq = fact_seq;
                self.last_projection_index = projection_index;
                self.state_revision = self.state_revision.max(revision);
                true
            }
            ClientV2Delivery::Replaceable => {
                let Some(revision) = event.revision else {
                    return false;
                };
                if revision <= self.state_revision {
                    return false;
                }
                self.state_revision = revision;
                true
            }
            ClientV2Delivery::Ephemeral => true,
        }
    }

    /// 标记 reset;保留旧 UI 状态直到新 bootstrap 通过校验。
    pub fn begin_reset(&mut self, reason: qaqh_client::ClientV2ResetReason, epoch: &str) {
        self.reset_epoch = Some(epoch.to_string());
        self.read_only = matches!(
            reason,
            qaqh_client::ClientV2ResetReason::SnapshotMissing
                | qaqh_client::ClientV2ResetReason::UpgradeRequired
        );
    }

    // ── 时间线 ──

    /// 权威快照整页替换(激活/重定基)。
    pub fn apply_page(&mut self, page: &TimelinePage) {
        self.turns = page.snapshot.turns.iter().map(Turn::from_timeline).collect();
        self.rebuild_turn_positions();
        self.has_more = page.has_more;
        self.total_turns = page.total_turns;
        self.truncated_before = page.truncated_before;
        self.loading_older = false;
    }

    fn rebuild_turn_positions(&mut self) {
        self.turn_positions.clear();
        for (index, turn) in self.turns.iter().enumerate() {
            self.turn_positions.insert(turn.turn_id.clone(), index);
        }
    }

    /// 更早的历史页前插(去重)。
    pub fn prepend_page(&mut self, page: &TimelinePage) {
        let known: BTreeSet<&str> = self.turns.iter().map(|t| t.turn_id.as_str()).collect();
        let older: Vec<Turn> = page
            .snapshot
            .turns
            .iter()
            .filter(|turn| !known.contains(turn.turn_id.as_str()))
            .map(Turn::from_timeline)
            .collect();
        let count = older.len();
        self.turns.splice(0..0, older);
        self.rebuild_turn_positions();
        self.has_more = page.has_more;
        // 新 fetched 的更旧页自己的标志描述「合并后窗口」的开头状态。
        self.truncated_before = page.truncated_before;
        self.loading_older = false;
        if count == 0 {
            self.has_more = false;
        }
    }

    /// 消费一条 timeline 事件(流式增量)。
    pub fn apply_entry(&mut self, entry: &TimelineEntry) {
        match &entry.event {
            TimelineEvent::TurnOpened { user_text } => {
                if !self.turn_positions.contains_key(&entry.turn_id) {
                    self.turns.push(Turn {
                        turn_id: entry.turn_id.clone(),
                        turn_index: None,
                        user_text: user_text.clone(),
                        state: TimelineTurnState::Running,
                        failure: None,
                        rounds: Vec::new(),
                        revision: 0,
                    });
                    self.turn_positions
                        .insert(entry.turn_id.clone(), self.turns.len() - 1);
                } else if let Some(turn) = self.turn_mut(&entry.turn_id)
                    && turn.user_text.is_empty()
                {
                    turn.user_text = user_text.clone();
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::BlockOpened { block } => {
                let round_num = entry.round_num.unwrap_or(0);
                if let Some(turn) = self.turn_mut(&entry.turn_id) {
                    let round = turn.ensure_round(round_num);
                    if !round.blocks.iter().any(|existing| existing.id() == block.block_id)
                    {
                        round.blocks.push(Block::from_timeline(block));
                        self.touch(&entry.turn_id);
                    }
                }
            }
            TimelineEvent::TextDelta {
                block_id, delta, ..
            } => {
                if let Some(block) = self.block_mut(&entry.turn_id, block_id) {
                    block.append_text(delta);
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::BlockCheckpoint {
                block_id, arg, text,
            } => {
                if let Some(block) = self.block_mut(&entry.turn_id, block_id) {
                    if let Some(arg) = arg.as_deref().filter(|arg| !arg.is_empty()) {
                        block.append_text(arg);
                    } else if !text.is_empty() {
                        block.replace_text(text);
                    }
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::ToolUpdated { block_id, tool } => {
                if let Some(Block::Tool { tool: slot, .. }) =
                    self.block_mut(&entry.turn_id, block_id)
                {
                    *slot = bound_tool(tool.clone());
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::ToolProgress { block_id, chunk, .. } => {
                if let Some(Block::Tool { progress, .. }) = self.block_mut(&entry.turn_id, block_id)
                {
                    progress.push_str(chunk);
                    // 界面只保留尾部窗口,避免长输出把状态撑爆。
                    const PROGRESS_WINDOW: usize = 16 * 1024;
                    if progress.len() > PROGRESS_WINDOW {
                        let cut = progress.len() - PROGRESS_WINDOW;
                        let boundary = (cut..progress.len())
                            .find(|index| progress.is_char_boundary(*index))
                            .unwrap_or(progress.len());
                        progress.drain(..boundary);
                    }
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::BlockSealed { block_id } => {
                if let Some(block) = self.block_mut(&entry.turn_id, block_id) {
                    block.seal();
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::RoundSealed { is_final } => {
                let round_num = entry.round_num.unwrap_or(0);
                if let Some(turn) = self.turn_mut(&entry.turn_id)
                    && let Some(round) = turn
                        .rounds
                        .iter_mut()
                        .find(|round| round.round_num == round_num)
                {
                    round.sealed = true;
                    if *is_final {
                        for block in &mut round.blocks {
                            block.seal();
                        }
                    }
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::TurnSealed { state, failure } => {
                if let Some(turn) = self.turn_mut(&entry.turn_id) {
                    turn.state = *state;
                    turn.failure = failure.clone();
                    self.touch(&entry.turn_id);
                }
            }
            TimelineEvent::UsageSampled { usage } => {
                self.usage = Some(usage.clone());
            }
            TimelineEvent::ToolEstimated { .. } => {}
        }
    }

    fn turn_mut(&mut self, turn_id: &str) -> Option<&mut Turn> {
        let index = *self.turn_positions.get(turn_id)?;
        self.turns.get_mut(index)
    }

    /// 记一次回合内容变更(渲染缓存代数 +1)。
    fn touch(&mut self, turn_id: &str) {
        if let Some(turn) = self.turn_mut(turn_id) {
            turn.revision = turn.revision.wrapping_add(1);
        }
    }

    fn block_mut(&mut self, turn_id: &str, block_id: &str) -> Option<&mut Block> {
        let turn = self.turn_mut(turn_id)?;
        turn.rounds
            .iter_mut()
            .rev()
            .find_map(|round| {
                round
                    .blocks
                    .iter_mut()
                    .find(|block| block.id() == block_id)
            })
    }

    /// 向上翻页游标:最旧回合的全局序号(排他)。缺序号(纯实时流)时不可翻。
    pub fn oldest_turn_index(&self) -> Option<u64> {
        self.turns.first().and_then(|turn| turn.turn_index)
    }

    // ── 交互 ──

    /// bootstrap 里的未决交互 → 面板(正文可能内联或外置)。
    fn restore_interaction(
        &mut self,
        interaction: &qaqh_client::ClientV2PendingInteraction,
    ) {
        if self.suppressed_interactions.contains(&interaction.interaction_id)
            || self.terminal_interactions.contains(&interaction.interaction_id)
        {
            return;
        }
        let call_id = (!interaction.call_id.is_empty()).then(|| interaction.call_id.clone());
        if let Some(qaqh_client::ClientV2PendingContentValue::Inline { text }) = &interaction.request
        {
            self.mount_interaction(&interaction.interaction_id, call_id.as_deref(), text.as_bytes());
        }
        // 外置正文由 App 层走 content 端点取回(App 持有 Bridge)。
    }

    /// delta 请求的未决交互 → 面板。返回正文是否需要外置取回。
    pub fn request_interaction(
        &mut self,
        interaction_id: &str,
        call_id: Option<&str>,
        request: &qaqh_client::ClientV2ContentValue,
    ) -> InteractionRequest {
        let suppressed = self.suppressed_interactions.contains(interaction_id)
            || call_id.is_some_and(|call_id| self.suppressed_interactions.contains(call_id));
        let terminal = self.terminal_interactions.contains(interaction_id);
        if suppressed || terminal {
            return InteractionRequest::Ignored;
        }
        if !self.pending_interactions.insert(interaction_id.to_string()) {
            return InteractionRequest::Ignored; // 重复投递
        }
        match request {
            qaqh_client::ClientV2ContentValue::Inline { text } => {
                self.mount_interaction(interaction_id, call_id, text.as_bytes());
                InteractionRequest::Mounted
            }
            qaqh_client::ClientV2ContentValue::Ref { content_ref } => {
                InteractionRequest::FetchRef(content_ref.hash().as_str().to_string())
            }
            qaqh_client::ClientV2ContentValue::Unavailable(_) => InteractionRequest::Ignored,
        }
    }

    /// 挂载交互面板(内联正文与外置正文取回后共用)。
    pub fn mount_interaction(
        &mut self,
        interaction_id: &str,
        call_id: Option<&str>,
        bytes: &[u8],
    ) {
        if let Some(pending) = parse_interaction_body(bytes, interaction_id, call_id) {
            self.pending = Some(pending);
        }
    }

    /// 交互落定(resolved / expired):收账 + 摘面板。
    pub fn close_interaction(&mut self, interaction_id: &str) {
        self.pending_interactions.remove(interaction_id);
        self.terminal_interactions.insert(interaction_id.to_string());
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.interaction_id() == interaction_id)
        {
            self.pending = None;
        }
    }

    /// 工具完成:对应权限面板下架。
    pub fn tool_finished(&mut self, call_id: &str) {
        if let Some(Pending::Permission(panel)) = self.pending.as_ref()
            && panel.tool_call_id == call_id
        {
            self.pending = None;
        }
    }

    /// 本壳应答后:记账 + 乐观下架(后续 resolved/expired 事件走 close_interaction)。
    ///
    /// 权限面板同时压制 tool_call_id:补投的同 id 权限请求不再弹(防幽灵面板)。
    pub fn acknowledge_answered(&mut self, interaction_id: &str) {
        let call_id = match self.pending.as_ref() {
            Some(Pending::Permission(panel)) => Some(panel.tool_call_id.clone()),
            _ => None,
        };
        self.suppressed_interactions
            .insert(interaction_id.to_string());
        if let Some(call_id) = call_id {
            self.suppressed_interactions.insert(call_id);
        }
        self.pending_interactions.remove(interaction_id);
        self.pending = None;
    }

    pub fn is_streaming(&self) -> bool {
        self.activity
            .as_ref()
            .is_some_and(|state| matches!(state, qaqh_client::ClientV2ActivityState::Running))
    }
}

/// `request_interaction` 的结果。
pub enum InteractionRequest {
    /// 已挂面板。
    Mounted,
    /// 正文外置,需按 content id 取回。
    FetchRef(String),
    /// 重复/已终结/被压制,忽略。
    Ignored,
}

// ───────────────────────── 会话标题取用 ─────────────────────────

/// bootstrap 里没有标题(标题走 meta/list),保持为空由侧栏填充。
impl SessionState {
    pub fn display_title(&self) -> String {
        if self.title.is_empty() {
            "未命名会话".to_string()
        } else {
            self.title.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_client::TimelineEvent;

    fn entry(turn_id: &str, event: TimelineEvent) -> TimelineEntry {
        TimelineEntry {
            timeline_seq: 1,
            turn_id: turn_id.to_string(),
            round_num: Some(0),
            event,
        }
    }

    fn tool_output(output: &str) -> TimelineEvent {
        TimelineEvent::ToolUpdated {
            block_id: "b1".into(),
            tool: TimelineTool {
                tool_call_id: "c1".into(),
                name: "exec".into(),
                state: qaqh_client::TimelineToolState::Succeeded,
                summary: None,
                args_json: None,
                output: Some(output.to_string()),
                diff: None,
                progress: String::new(),
                progress_truncated: false,
                progress_stream: None,
                progress_bytes_total: 0,
                display: None,
                failure: None,
                permission: None,
                exit_code: None,
                completed_at_ms: None,
            },
        }
    }

    /// ② 工具输出在落账时就被裁到尾窗(UI 只展示尾部;磁盘是事实源)。
    #[test]
    fn tool_output_is_bounded() {
        let mut state = SessionState::new("s");
        state.apply_entry(&entry("t1", TimelineEvent::TurnOpened { user_text: "hi".into() }));
        state.apply_entry(&entry(
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                    reasoning_kind: None,
                },
            },
        ));
        let huge = "x".repeat(200_000);
        state.apply_entry(&entry("t1", tool_output(&huge)));
        let Some(Turn { rounds, .. }) = state.turns.last() else {
            panic!("turn");
        };
        let Block::Tool { tool, .. } = &rounds[0].blocks[0] else {
            panic!("tool block");
        };
        let output = tool.output.as_deref().expect("output");
        assert!(output.chars().count() <= TOOL_OUTPUT_WINDOW + 1, "len={}", output.chars().count());
        assert!(output.starts_with('…'));
    }

    /// ④ TurnOpened 去重走索引;后续事件经索引定位。
    #[test]
    fn turn_opened_is_indexed() {
        let mut state = SessionState::new("s");
        state.apply_entry(&entry("t1", TimelineEvent::TurnOpened { user_text: "a".into() }));
        state.apply_entry(&entry("t1", TimelineEvent::TurnOpened { user_text: "a".into() }));
        assert_eq!(state.turns.len(), 1);
        // 索引可定位:TextDelta 正常生效
        state.apply_entry(&entry(
            "t1",
            TimelineEvent::BlockOpened {
                block: TimelineBlock {
                    block_id: "b1".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Text,
                    state: TimelineBlockState::Open,
                    text: String::new(),
                    tool: None,
                    reasoning_kind: None,
                },
            },
        ));
        state.apply_entry(&entry(
            "t1",
            TimelineEvent::TextDelta { block_id: "b1".into(), fragment_seq: 1, delta: "hello".into() },
        ));
        let Some(Turn { rounds, .. }) = state.turns.last() else { panic!("turn") };
        let Block::Text { md, .. } = &rounds[0].blocks[0] else { panic!("text block") };
        assert_eq!(md.items().len(), 1);
    }

    /// ① revision 随内容变更单调;无内容变更不动。
    #[test]
    fn revision_tracks_mutations() {
        let mut state = SessionState::new("s");
        state.apply_entry(&entry("t1", TimelineEvent::TurnOpened { user_text: "a".into() }));
        let revision_after_open = state.turns[0].revision;
        state.apply_entry(&entry("t1", TimelineEvent::UsageSampled {
            usage: qaqh_client::UsageInfo::default(),
        }));
        assert_eq!(state.turns[0].revision, revision_after_open, "usage 不改回合内容");
        state.apply_entry(&entry(
            "t1",
            TimelineEvent::TextDelta { block_id: "b1".into(), fragment_seq: 1, delta: "x".into() },
        ));
        // 未知块的 delta 不生效也不 bump
        assert_eq!(state.turns[0].revision, revision_after_open);
        state.apply_entry(&entry("t1", TimelineEvent::TurnSealed {
            state: TimelineTurnState::Completed,
            failure: None,
        }));
        assert!(state.turns[0].revision > revision_after_open);
    }

    /// ③ truncated_before 从权威页传播。
    #[test]
    fn truncated_before_propagates() {
        let mut state = SessionState::new("s");
        let page = qaqh_client::TimelinePage {
            schema: qaqh_client::RINGING_SCHEMA.into(),
            version: qaqh_client::RINGING_V2_VERSION,
            server_epoch: "e".into(),
            session_id: "s".into(),
            snapshot: qaqh_client::TimelineSnapshot { watermark: 0, turns: vec![] },
            has_more: false,
            total_turns: 0,
            truncated_before: true,
        };
        state.apply_page(&page);
        assert!(state.truncated_before);
    }
}
