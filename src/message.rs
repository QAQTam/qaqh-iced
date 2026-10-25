//! 应用消息与桥接事件。
//!
//! `Message` 是 iced 的 Elm 消息;`BridgeEvent` 是 `qaqh-client` 回调/回执经
//! 单一 mpsc 通道送达的运行时报文。两层绝不混用:wire 类型只出现在
//! `BridgeEvent` 内部,view 层只消费投影后的状态。

use std::sync::Arc;

use qaqh_client::{
    ClientV2Bootstrap, ClientV2Event, ClientV2Reset, TimelineEntry, TimelinePage,
};

#[derive(Debug, Clone)]
pub enum Message {
    Bridge(BridgeEvent),
    /// 输入框编辑(含按键)。
    Composer(iced::widget::text_editor::Action),
    Send,
    Cancel,
    SelectSession(String),
    NewSession,
    /// 连接失败后的手动重连。
    Retry,
    LoadOlder,
    /// 转录滚动位置变化(y 相对偏移:0 顶 / 1 底)。
    TranscriptScrolled(f32),
    PermissionDecide {
        approved: bool,
        trust_folder: bool,
    },
    PermissionToggleTrust,
    AskSelect {
        question: usize,
        option: usize,
    },
    AskCustom {
        question: usize,
        value: String,
    },
    AskSubmit,
    AskDismiss,
    PlanDecide {
        approved: bool,
    },
    PlanDraft(String),
    LinkClicked(iced::widget::markdown::Uri),
    /// 清掉自动消失的提示条。
    ClearNotice,
    // ── 设置页 ──
    /// 打开设置页(侧栏入口);进入时刷新权威配置。
    OpenSettings,
    /// 返回会话视图。
    CloseSettings,
    /// 文本/数字缓冲编辑。
    SettingsText(crate::settings::SettingField, String),
    /// 下拉选择(wire / credential / reasoningEffort)。
    SettingsPickWire(String),
    SettingsPickCredential(String),
    SettingsPickEffort(String),
    /// 权限档位(1-4)。
    SettingsPickPermission(u64),
    /// 复选框。
    SettingsToggle(crate::settings::SettingField, bool),
    /// 保存(脏字段 Merge Patch)。
    SettingsSave,
    /// 重新加载权威配置(放弃本地草稿)。
    SettingsReload,
    /// 切换活跃 profile。
    ProfileApply(String),
}

/// 侧栏会话卡片(`session.list` 投影)。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCard {
    pub session_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: Option<u64>,
    pub turn_count: Option<u64>,
    pub message_count: Option<u64>,
}

/// 传输层 → 界面层的全部报文。
///
/// 大载荷用 `Arc` 包裹:同一报文只 clone 指针,时间线快照可能上万行。
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// 已协商出 lease。epoch 变化意味着 daemon 重启过。
    Connected { epoch: String },
    /// 正在协商(冷启动/重连中)。
    Opening,
    ConnectFailed(String),
    SessionList(Vec<SessionCard>),
    QueryError(String),
    /// timeline 快照(激活/重定基)。**权威替换**整个转录。
    TimelineSnapshot {
        session_id: String,
        page: Arc<TimelinePage>,
    },
    TimelineEntry {
        session_id: String,
        entry: Arc<TimelineEntry>,
    },
    /// timeline 流结束。`missing` = daemon 明确回答会话不存在(404)。
    TimelineLost {
        session_id: String,
        reason: String,
        missing: bool,
    },
    /// 更早的历史页(向上翻页)。
    OlderPage {
        session_id: String,
        page: Arc<TimelinePage>,
    },
    V2Event {
        session_id: String,
        event: Arc<ClientV2Event>,
    },
    /// v2 单流建立(或重连成功):必须重新 bootstrap 该会话。
    V2StreamOpen { session_id: String },
    V2Reset {
        session_id: String,
        reset: Arc<ClientV2Reset>,
    },
    Bootstrap {
        session_id: String,
        bootstrap: Arc<ClientV2Bootstrap>,
    },
    /// 我们发起的 SessionCreate 已在 daemon 侧落地。
    SessionCreated {
        session_id: String,
    },
    /// 命令/查询回执(label 是动作名,用于 toast)。
    Ack {
        label: String,
        ok: bool,
        error: Option<String>,
    },
    /// 交互正文(content ref 外置时)取回。
    InteractionBody {
        interaction_id: String,
        call_id: Option<String>,
        result: Result<Vec<u8>, String>,
    },
    /// 权威配置快照(`config.load` 回读)。
    ConfigLoaded(Result<Arc<qaqh_config_api::ConfigDto>, String>),
    /// 设置保存结果;成功后桥内已自动回读(会再送 `ConfigLoaded`)。
    ConfigSaved(Result<(), String>),
    /// profile 切换结果(成功后桥内已自动回读)。
    ProfileApplied {
        name: String,
        result: Result<(), String>,
    },
}
