//! 应用层状态与消息路由(iced Model)。
//!
//! 职责边界:`backend::Bridge` 拥有传输;`session::SessionState` 拥有单会话
//! 投影;本文件只做三件事 —— 消息分发、会话生命周期(active/opened 账本)、
//! 与 iced `Task` 的组装(滚动/提示条)。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iced::widget::text_editor;
use iced::{Subscription, Task, stream};
use qaqh_client::{
    ClientV2ControlDelta, ClientV2Event, ClientV2MetaDelta, ClientV2Payload, TimelineEntry,
};

use crate::backend::Bridge;
use crate::message::{BridgeEvent, Message, SessionCard};
use crate::session::{InteractionRequest, Pending, SessionState};
use crate::settings::{SettingField, SettingsPage};

/// 桥接通道的接收端槽:`Subscription::run` 的 worker 只接受 `fn` 指针,
/// 所以接收端放在进程级槽里,worker 启动时恰好取走一次。
static EVENT_RX: std::sync::OnceLock<
    Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<BridgeEvent>>>,
> = std::sync::OnceLock::new();

/// 启动前安装事件接收端(main 在建桥后调用一次)。
pub fn install_event_rx(rx: tokio::sync::mpsc::UnboundedReceiver<BridgeEvent>) {
    let _ = EVENT_RX.set(Mutex::new(Some(rx)));
}

/// 连接相位。
#[derive(Debug, Clone)]
pub enum Phase {
    Connecting,
    Ready { epoch: String },
    Failed(String),
}

/// 应用根状态。
pub struct App {
    pub bridge: Arc<Bridge>,
    pub phase: Phase,
    /// 最近一次值得上屏的错误(自动消失)。
    pub notice: Option<String>,
    pub sessions: Vec<SessionCard>,
    pub sessions_fetched: bool,
    pub active: Option<String>,
    /// 已 attach + 起流的会话。
    pub opened: HashSet<String>,
    pub states: HashMap<String, SessionState>,
    pub composer: text_editor::Content,
    /// 转录自动跟随(用户向上翻阅时解除)。
    pub follow: bool,
    /// 当前视图(会话 / 设置)。
    pub view_mode: ViewMode,
    /// 设置页状态(权威快照 + 编辑缓冲)。
    pub settings: SettingsPage,
}

/// 主区视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Chat,
    Settings,
}

impl App {
    pub fn new(bridge: Arc<Bridge>) -> Self {
        Self {
            bridge,
            phase: Phase::Connecting,
            notice: None,
            sessions: Vec::new(),
            sessions_fetched: false,
            active: None,
            opened: HashSet::new(),
            states: HashMap::new(),
            composer: text_editor::Content::new(),
            follow: true,
            view_mode: ViewMode::Chat,
            settings: SettingsPage::default(),
        }
    }

    // ───────────────────── 消息入口 ─────────────────────

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Bridge(event) => self.bridge_event(event),
            Message::Composer(action) => {
                self.composer.perform(action);
                Task::none()
            }
            Message::Send => self.send(),
            Message::Cancel => {
                if let Some(session_id) = self.active.clone() {
                    self.bridge.cancel_turn(session_id);
                }
                Task::none()
            }
            Message::SelectSession(session_id) => self.select_session(session_id),
            Message::NewSession => {
                self.bridge.create_session();
                Task::none()
            }
            Message::Retry => {
                if matches!(self.phase, Phase::Failed(_)) {
                    self.phase = Phase::Connecting;
                    self.sessions_fetched = false;
                    self.bridge.reconnect();
                }
                Task::none()
            }
            Message::LoadOlder => self.load_older(),
            Message::TranscriptScrolled(offset) => {
                // 0.0 同时是「到顶」与「内容不足一屏」;两者都保持跟随。
                self.follow = offset >= 0.995 || offset <= 0.0;
                Task::none()
            }
            Message::PermissionDecide {
                approved,
                trust_folder,
            } => self.respond_permission(approved, trust_folder),
            Message::PermissionToggleTrust => {
                if let Some(session) = self.active_state_mut()
                    && let Some(Pending::Permission(panel)) = session.pending.as_mut()
                {
                    panel.trust_folder = !panel.trust_folder;
                }
                Task::none()
            }
            Message::AskSelect { question, option } => {
                if let Some(panel) = self.pending_ask_mut()
                    && let Some(selected) = panel.selections.get_mut(question)
                {
                    *selected = Some(option);
                }
                Task::none()
            }
            Message::AskCustom { question, value } => {
                if let Some(panel) = self.pending_ask_mut()
                    && let Some(custom) = panel.customs.get_mut(question)
                {
                    *custom = value;
                }
                Task::none()
            }
            Message::AskSubmit => self.submit_ask(),
            Message::AskDismiss => self.dismiss_ask(),
            Message::PlanDecide { approved } => self.respond_plan(approved),
            Message::PlanDraft(value) => {
                if let Some(session) = self.active_state_mut()
                    && let Some(Pending::Plan(panel)) = session.pending.as_mut()
                {
                    panel.message = value;
                }
                Task::none()
            }
            Message::LinkClicked(uri) => {
                open_uri(&uri);
                Task::none()
            }
            Message::ClearNotice => {
                self.notice = None;
                Task::none()
            }
            // ── 设置页 ──
            Message::OpenSettings => {
                self.view_mode = ViewMode::Settings;
                if self.settings.config.is_none() {
                    self.settings.loading = true;
                    self.bridge.fetch_config();
                }
                Task::none()
            }
            Message::CloseSettings => {
                self.view_mode = ViewMode::Chat;
                Task::none()
            }
            Message::SettingsText(field, value) => {
                self.set_buffer(field, value);
                Task::none()
            }
            Message::SettingsPickWire(value) => {
                self.settings.buffers.wire = value;
                self.refresh_parse_error();
                Task::none()
            }
            Message::SettingsPickCredential(value) => {
                self.settings.buffers.credential = value;
                self.refresh_parse_error();
                Task::none()
            }
            Message::SettingsPickEffort(value) => {
                self.settings.buffers.reasoning_effort = value;
                self.refresh_parse_error();
                Task::none()
            }
            Message::SettingsPickPermission(value) => {
                self.settings.buffers.permission_level = value;
                Task::none()
            }
            Message::SettingsToggle(field, value) => {
                match field {
                    SettingField::ComplianceEnabled => {
                        self.settings.buffers.compliance_enabled = value;
                    }
                    SettingField::SandboxEnabled => {
                        self.settings.buffers.sandbox_enabled = value;
                    }
                    _ => {}
                }
                Task::none()
            }
            Message::SettingsSave => self.save_settings(),
            Message::SettingsReload => {
                self.settings.loading = true;
                self.settings.parse_error = None;
                self.bridge.fetch_config();
                Task::none()
            }
            Message::ProfileApply(name) => {
                self.bridge.apply_profile(name);
                Task::none()
            }
        }
    }

    /// 事件流订阅:单个 mpsc 通道 → BridgeEvent。
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::run(events_worker).map(Message::Bridge)
    }

    // ───────────────────── 用户动作 ─────────────────────

    fn send(&mut self) -> Task<Message> {
        let Some(session_id) = self.active.clone() else {
            return Task::none();
        };
        if self
            .states
            .get(&session_id)
            .is_some_and(|state| state.read_only)
        {
            return self.set_notice("会话处于只读状态(等待重新定基),暂不能发送");
        }
        let text = self.composer.text().trim().to_string();
        if text.is_empty() {
            return Task::none();
        }
        self.composer = text_editor::Content::new();
        self.bridge.send_message(session_id, text);
        self.follow = true;
        scroll_to_end()
    }

    fn select_session(&mut self, session_id: String) -> Task<Message> {
        self.active = Some(session_id.clone());
        if !self.opened.contains(&session_id) {
            self.open_session(&session_id);
        }
        if self.follow {
            scroll_to_end()
        } else {
            Task::none()
        }
    }

    /// attach + 激活流 + 建(或复用)本地状态。
    fn open_session(&mut self, session_id: &str) {
        self.opened.insert(session_id.to_string());
        self.states
            .entry(session_id.to_string())
            .or_insert_with(|| SessionState::new(session_id));
        self.bridge.open_session(session_id.to_string());
    }

    fn load_older(&mut self) -> Task<Message> {
        let Some(session_id) = self.active.clone() else {
            return Task::none();
        };
        let Some(state) = self.states.get(&session_id) else {
            return Task::none();
        };
        if state.loading_older {
            return Task::none();
        }
        let Some(before_index) = state.oldest_turn_index() else {
            return Task::none();
        };
        if let Some(state) = self.states.get_mut(&session_id) {
            state.loading_older = true;
        }
        self.bridge.load_older(session_id, before_index);
        Task::none()
    }

    fn respond_permission(&mut self, approved: bool, trust_folder: bool) -> Task<Message> {
        let Some(session_id) = self.active.clone() else {
            return Task::none();
        };
        let Some(state) = self.states.get_mut(&session_id) else {
            return Task::none();
        };
        let Some(Pending::Permission(panel)) = state.pending.as_ref() else {
            return Task::none();
        };
        let tool_call_id = panel.tool_call_id.clone();
        let interaction_id = panel.interaction_id.clone();
        state.acknowledge_answered(&interaction_id);
        self.bridge
            .respond_permission(session_id, tool_call_id, approved, trust_folder);
        Task::none()
    }

    fn submit_ask(&mut self) -> Task<Message> {
        let Some(session_id) = self.active.clone() else {
            return Task::none();
        };
        let Some(state) = self.states.get_mut(&session_id) else {
            return Task::none();
        };
        let Some(Pending::Ask(panel)) = state.pending.as_ref() else {
            return Task::none();
        };
        let Some(answers) = panel.collect_answers() else {
            return self.set_notice("还有问题未回答");
        };
        let interaction_id = panel.interaction_id.clone();
        state.acknowledge_answered(&interaction_id);
        self.bridge.ask_respond(session_id, interaction_id, answers);
        Task::none()
    }

    fn dismiss_ask(&mut self) -> Task<Message> {
        let Some(session_id) = self.active.clone() else {
            return Task::none();
        };
        let Some(state) = self.states.get_mut(&session_id) else {
            return Task::none();
        };
        let Some(Pending::Ask(panel)) = state.pending.as_ref() else {
            return Task::none();
        };
        let interaction_id = panel.interaction_id.clone();
        state.acknowledge_answered(&interaction_id);
        self.bridge.ask_dismiss(session_id, interaction_id);
        Task::none()
    }

    fn respond_plan(&mut self, approved: bool) -> Task<Message> {
        let Some(session_id) = self.active.clone() else {
            return Task::none();
        };
        let Some(state) = self.states.get_mut(&session_id) else {
            return Task::none();
        };
        let Some(Pending::Plan(panel)) = state.pending.as_ref() else {
            return Task::none();
        };
        let interaction_id = panel.interaction_id.clone();
        let message = (!approved).then(|| {
            let trimmed = panel.message.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        });
        state.acknowledge_answered(&interaction_id);
        self.bridge
            .plan_respond(session_id, interaction_id, approved, message.flatten());
        Task::none()
    }

    // ───────────────────── 桥接事件 ─────────────────────

    fn bridge_event(&mut self, event: BridgeEvent) -> Task<Message> {
        match event {
            BridgeEvent::Connected { epoch } => {
                self.phase = Phase::Ready { epoch };
                if !self.sessions_fetched {
                    self.bridge.fetch_sessions();
                }
                Task::none()
            }
            BridgeEvent::Opening => {
                self.phase = Phase::Connecting;
                Task::none()
            }
            BridgeEvent::ConnectFailed(error) => {
                self.phase = Phase::Failed(error);
                Task::none()
            }
            BridgeEvent::SessionList(cards) => {
                self.sessions = cards;
                self.sessions_fetched = true;
                self.reconcile_titles();
                if self.active.is_none() {
                    match self.sessions.first() {
                        Some(card) => self.select_session(card.session_id.clone()),
                        None => {
                            // 空列表:自动建一个会话,给用户一个可输入的起点。
                            self.bridge.create_session();
                            Task::none()
                        }
                    }
                } else {
                    Task::none()
                }
            }
            BridgeEvent::QueryError(error) => self.set_notice(error),
            BridgeEvent::SessionCreated { session_id } => {
                self.active = Some(session_id.clone());
                self.open_session(&session_id);
                self.bridge.fetch_sessions();
                Task::none()
            }
            BridgeEvent::TimelineSnapshot {
                session_id,
                page,
            } => {
                if let Some(state) = self.states.get_mut(&session_id) {
                    state.apply_page(&page);
                }
                self.scroll_if_following(&session_id)
            }
            BridgeEvent::TimelineEntry { session_id, entry } => {
                self.apply_timeline_entry(&session_id, &entry)
            }
            BridgeEvent::TimelineLost {
                session_id,
                reason,
                missing,
            } => {
                if missing {
                    // daemon 明确回答会话不存在:下线本地状态。
                    self.states.remove(&session_id);
                    self.opened.remove(&session_id);
                    if self.active.as_deref() == Some(session_id.as_str()) {
                        self.active = None;
                    }
                    self.bridge.fetch_sessions();
                    Task::none()
                } else {
                    self.set_notice(format!("timeline 流结束:{reason}"))
                }
            }
            BridgeEvent::OlderPage { session_id, page } => {
                if let Some(state) = self.states.get_mut(&session_id) {
                    state.prepend_page(&page);
                }
                Task::none()
            }
            BridgeEvent::V2StreamOpen { session_id } => {
                if !self.opened.contains(&session_id) {
                    self.open_session(&session_id);
                }
                if self.active.is_none() {
                    self.active = Some(session_id.clone());
                }
                self.bridge.bootstrap(session_id);
                Task::none()
            }
            BridgeEvent::V2Event { session_id, event } => {
                self.apply_v2_event(&session_id, &event)
            }
            BridgeEvent::V2Reset {
                session_id,
                reset,
            } => {
                if let Some(state) = self.states.get_mut(&session_id) {
                    state.begin_reset(reset.reason, &reset.server_epoch);
                }
                // reset → 重新 bootstrap(客户端会按 reset 基线重建)。
                self.bridge.bootstrap(session_id);
                Task::none()
            }
            BridgeEvent::Bootstrap {
                session_id,
                bootstrap,
            } => {
                let mut task = Task::none();
                if let Some(state) = self.states.get_mut(&session_id) {
                    state.apply_bootstrap(&bootstrap);
                    // 外置正文走 content 端点取回(内联正文已在 apply_bootstrap 挂上)。
                    for interaction in &bootstrap.control.state.interactions {
                        if let Some(qaqh_client::ClientV2PendingContentValue::Ref {
                            content_ref,
                        }) = &interaction.request
                        {
                            let call_id = (!interaction.call_id.is_empty())
                                .then(|| interaction.call_id.clone());
                            self.bridge.fetch_interaction_body(
                                interaction.interaction_id.clone(),
                                call_id,
                                content_ref.clone(),
                            );
                        }
                    }
                    if state.pending.is_some() {
                        task = scroll_to_end();
                    }
                }
                task
            }
            BridgeEvent::Ack { label, ok, error } => {
                if !ok {
                    let detail =
                        error.unwrap_or_else(|| "未知错误".into());
                    self.set_notice(format!("{label}失败:{detail}"))
                } else if label == "发送" {
                    let session_id = self.active.clone().unwrap_or_default();
                    self.scroll_if_following(&session_id)
                } else {
                    Task::none()
                }
            }
            BridgeEvent::InteractionBody {
                interaction_id,
                call_id,
                result,
            } => match result {
                Ok(bytes) => {
                    if let Some(state) = self
                        .states
                        .values_mut()
                        .find(|state| state.pending_interactions.contains(&interaction_id))
                        && state.pending.is_none()
                    {
                        state.mount_interaction(&interaction_id, call_id.as_deref(), &bytes);
                        if state.pending.is_some() {
                            return scroll_to_end();
                        }
                    }
                    Task::none()
                }
                Err(error) => self.set_notice(format!("交互正文取回失败:{error}")),
            },
            BridgeEvent::ConfigLoaded(result) => match result {
                Ok(config) => {
                    self.settings.apply_config((*config).clone());
                    Task::none()
                }
                Err(error) => {
                    self.settings.loading = false;
                    self.set_notice(format!("配置加载失败:{error}"))
                }
            },
            BridgeEvent::ConfigSaved(result) => {
                self.settings.saving = false;
                match result {
                    Ok(()) => self.set_notice("设置已保存"),
                    Err(error) => self.set_notice(format!("保存失败:{error}")),
                }
            }
            BridgeEvent::ProfileApplied { name, result } => match result {
                Ok(()) => self.set_notice(format!("已切换到 profile「{name}」")),
                Err(error) => self.set_notice(format!("切换 profile 失败:{error}")),
            },
        }
    }

    fn apply_timeline_entry(
        &mut self,
        session_id: &str,
        entry: &TimelineEntry,
    ) -> Task<Message> {
        let Some(state) = self.states.get_mut(session_id) else {
            return Task::none();
        };
        state.apply_entry(entry);
        self.scroll_if_following(session_id)
    }

    fn apply_v2_event(&mut self, session_id: &str, event: &ClientV2Event) -> Task<Message> {
        let Some(state) = self.states.get_mut(session_id) else {
            return Task::none();
        };
        if !state.admit_v2_event(event) {
            return Task::none();
        }
        match &event.payload {
            ClientV2Payload::ControlDelta(delta) => match delta {
                ClientV2ControlDelta::Activity { state: activity, .. } => {
                    state.activity = Some(*activity);
                    Task::none()
                }
                ClientV2ControlDelta::InteractionRequested {
                    interaction_id,
                    call_id,
                    request,
                    ..
                } => {
                    match state.request_interaction(
                        interaction_id.as_str(),
                        call_id.as_ref().map(|id| id.as_str()),
                        request,
                    ) {
                        InteractionRequest::FetchRef(content_id) => {
                            self.bridge.fetch_interaction_body(
                                interaction_id.as_str().to_string(),
                                call_id.as_ref().map(|id| id.as_str().to_string()),
                                content_id,
                            );
                            Task::none()
                        }
                        InteractionRequest::Mounted => scroll_to_end(),
                        InteractionRequest::Ignored => Task::none(),
                    }
                }
                ClientV2ControlDelta::InteractionResolved { interaction_id, .. } => {
                    state.close_interaction(interaction_id.as_str());
                    Task::none()
                }
                ClientV2ControlDelta::InteractionExpired { interaction_id, .. } => {
                    state.close_interaction(interaction_id.as_str());
                    Task::none()
                }
                ClientV2ControlDelta::ToolFinished { call_id, .. } => {
                    state.tool_finished(call_id.as_str());
                    Task::none()
                }
                _ => Task::none(),
            },
            ClientV2Payload::MetaDelta(delta) => match delta {
                ClientV2MetaDelta::TitleChanged { title, .. } => {
                    let title = title.clone();
                    if let Some(session) = self.states.get_mut(session_id) {
                        session.title = title.clone();
                    }
                    if let Some(card) = self
                        .sessions
                        .iter_mut()
                        .find(|card| card.session_id == session_id)
                    {
                        card.title = title;
                    }
                    Task::none()
                }
                ClientV2MetaDelta::Deleted { .. } => {
                    self.states.remove(session_id);
                    self.opened.remove(session_id);
                    if self.active.as_deref() == Some(session_id) {
                        self.active = None;
                    }
                    self.bridge.fetch_sessions();
                    Task::none()
                }
                _ => Task::none(),
            },
            _ => Task::none(),
        }
    }

    // ───────────────────── 小工具 ─────────────────────

    /// 就绪状态下才提供「新会话」入口。
    pub fn has_session_support(&self) -> bool {
        matches!(self.phase, Phase::Ready { .. })
    }

    // ── 设置页辅助 ──

    fn set_buffer(&mut self, field: SettingField, value: String) {
        let buffers = &mut self.settings.buffers;
        match field {
            SettingField::ApiKey => buffers.api_key = value,
            SettingField::Model => buffers.model = value,
            SettingField::BaseUrl => buffers.base_url = value,
            SettingField::MaxTokens => buffers.max_tokens = value,
            SettingField::ContextLength => buffers.context_length = value,
            SettingField::AutoCompactThreshold => buffers.auto_compact_threshold = value,
            SettingField::TokenizerPath => buffers.tokenizer_path = value,
            SettingField::SessionIdleUnloadSecs => buffers.session_idle_unload_secs = value,
            SettingField::SubModel => buffers.sub_model = value,
            SettingField::SubBaseUrl => buffers.sub_base_url = value,
            SettingField::SubApiKey => buffers.sub_api_key = value,
            SettingField::SubMaxTokens => buffers.sub_max_tokens = value,
            SettingField::SubTimeoutSecs => buffers.sub_timeout_secs = value,
            SettingField::SubDefaultTools => buffers.sub_default_tools = value,
            SettingField::ExecDefaultShell => buffers.exec_default_shell = value,
            SettingField::ComplianceEnabled | SettingField::SandboxEnabled => {}
        }
        self.refresh_parse_error();
    }

    /// 每次编辑后重算解析错误(数字字段非法等);合法时清掉旧错误。
    fn refresh_parse_error(&mut self) {
        self.settings.parse_error = self.settings.build_patch().err();
    }

    fn save_settings(&mut self) -> Task<Message> {
        if self.settings.saving {
            return Task::none();
        }
        let Some(_) = &self.settings.config else {
            return self.set_notice("配置尚未加载");
        };
        let patch = match self.settings.build_patch() {
            Ok(patch) => patch,
            Err(error) => {
                self.settings.parse_error = Some(error.clone());
                return self.set_notice(error);
            }
        };
        if patch.is_empty() {
            return self.set_notice("设置无改动");
        }
        if let Err(error) = patch.validate() {
            return self.set_notice(format!("校验失败:{error}"));
        }
        self.settings.saving = true;
        self.bridge.save_config(patch);
        Task::none()
    }

    fn active_state_mut(&mut self) -> Option<&mut SessionState> {
        let session_id = self.active.as_ref()?;
        self.states.get_mut(session_id)
    }

    fn pending_ask_mut(&mut self) -> Option<&mut crate::session::AskPanel> {
        match self.active_state_mut()?.pending.as_mut() {
            Some(Pending::Ask(panel)) => Some(panel),
            _ => None,
        }
    }

    /// 同步侧栏标题(列表刷新不改写已知标题,只补缺)。
    fn reconcile_titles(&mut self) {
        for card in &mut self.sessions {
            if card.title.is_empty()
                && let Some(state) = self.states.get(&card.session_id)
                && !state.title.is_empty()
            {
                card.title = state.title.clone();
            }
        }
    }

    fn scroll_if_following(&mut self, session_id: &str) -> Task<Message> {
        if self.follow && self.active.as_deref() == Some(session_id) {
            scroll_to_end()
        } else {
            Task::none()
        }
    }

    fn set_notice(&mut self, text: impl Into<String>) -> Task<Message> {
        self.notice = Some(text.into());
        Task::perform(tokio::time::sleep(Duration::from_secs(6)), |_| {
            Message::ClearNotice
        })
    }
}

/// 转录滚动容器置底。
pub fn scroll_to_end() -> Task<Message> {
    iced::widget::operation::snap_to_end(transcript_scroll_id())
}

pub fn transcript_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("qaqh-transcript")
}

/// 用系统默认方式打开链接(仅 xdg-open 系平台;失败静默)。
fn open_uri(uri: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(uri)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// 事件通道 worker:槽里恰好有一个接收端,worker 启动时取走并循环转发。
fn events_worker() -> impl iced::futures::Stream<Item = BridgeEvent> {
    use iced::futures::SinkExt;

    stream::channel(100, async move |mut sender| {
        let Some(mut rx) = EVENT_RX
            .get()
            .and_then(|slot| slot.lock().ok())
            .and_then(|mut guard| guard.take())
        else {
            return;
        };
        while let Some(event) = rx.recv().await {
            if sender.send(event).await.is_err() {
                break;
            }
        }
    })
}
