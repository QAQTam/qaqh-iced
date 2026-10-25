//! `qaqh-client` ↔ iced 桥。
//!
//! 所有客户端任务都 spawn 在 `qaqh_client::runtime_handle()` 指向的全局 tokio
//! runtime 上(传输层自己的续租/SSE 任务也在其上);事件与回执统一经单个
//! mpsc 通道送回 iced(由 Subscription 转成 Message)。iced 侧永远不直接
//! await 客户端 future——它只消费通道,executor 归属因此完全无关紧要。
//!
//! 会话打开时序对齐 TUI:`attach(SessionResume)` → `activate_timeline`
//! (拉首页 + 起 timeline 流 + 起 v2 单流)→ 流 `Open` 后 bootstrap;
//! 同时 best-effort 认领 driver 席位(未认领会话本就放行,认领只为与
//! 其它壳共存时不被抢写)。

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use qaqh_client::{
    ActionRequest, Client, ClientHandlers, ClientOptions, ClientV2ControlDelta, ClientV2Payload,
    CommandOptions, ConversationCommand, ConversationInputPurpose, ControlCommand, QueryRequest,
    RingingCommand, RingingCommandAckStatus, TimelineStatus, ToolCommand, V2StreamStatus,
    runtime_handle,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::message::{BridgeEvent, SessionCard};

pub struct Bridge {
    tx: UnboundedSender<BridgeEvent>,
    client: RwLock<Option<Arc<Client>>>,
    handle: tokio::runtime::Handle,
    /// 待回执的 SessionCreate 命令 id:只有 causation_id 匹配的
    /// `SessionCreated` 事件才会打开新标签(别的客户端建的会话不打扰)。
    /// `Arc` 是因为 on_v2_event 回调闭包也要读它。
    pending_creates: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl Bridge {
    pub fn new(tx: UnboundedSender<BridgeEvent>) -> Arc<Self> {
        let bridge = Arc::new(Self {
            tx,
            client: RwLock::new(None),
            handle: runtime_handle(),
            pending_creates: Arc::new(std::sync::Mutex::new(HashSet::new())),
        });
        bridge.spawn_connect();
        bridge
    }

    fn emit(&self, event: BridgeEvent) {
        let _ = self.tx.send(event);
    }

    /// 当前客户端;未连接时 `None`。
    pub fn client(&self) -> Option<Arc<Client>> {
        self.client.read().ok().and_then(|guard| guard.clone())
    }

    /// 连接 daemon(允许自动拉起)并监听连接相位。
    fn spawn_connect(self: &Arc<Self>) {
        let tx = self.tx.clone();
        let handlers = ClientHandlers {
            on_v2_event: {
                let tx = tx.clone();
                let pending_creates = self.pending_creates.clone();
                Arc::new(move |session_id: String, event| {
                    // 我们创建的会话在这里就地识别:daemon 对 SessionCreate
                    // 命令回 causation_id == command_id;不匹配的(别的客户端
                    // 建的会话)不打扰。
                    if let ClientV2Payload::ControlDelta(ClientV2ControlDelta::SessionCreated {
                        ..
                    }) = &event.payload
                        && let Some(command_id) = event.causation_id.as_deref()
                        && let Ok(mut guard) = pending_creates.lock()
                        && guard.remove(command_id)
                    {
                        let _ = tx.send(BridgeEvent::SessionCreated {
                            session_id: session_id.clone(),
                        });
                    }
                    let _ = tx.send(BridgeEvent::V2Event {
                        session_id,
                        event: Arc::new(event),
                    });
                })
            },
            on_v2_status: {
                let tx = tx.clone();
                // 只关心 Open:重连/关闭由传输层自愈,失败语义用失联相位兜底。
                Arc::new(move |session_id: String, status: V2StreamStatus| {
                    if matches!(status, V2StreamStatus::Open { .. }) {
                        let _ = tx.send(BridgeEvent::V2StreamOpen { session_id });
                    }
                })
            },
            on_liveness: Arc::new(|| {}),
            on_v2_reset: {
                let tx = tx.clone();
                Arc::new(move |session_id: String, reset| {
                    let _ = tx.send(BridgeEvent::V2Reset {
                        session_id,
                        reset: Arc::new(reset),
                    });
                })
            },
            on_timeline_entry: {
                let tx = tx.clone();
                Arc::new(move |session_id: String, entry| {
                    let _ = tx.send(BridgeEvent::TimelineEntry {
                        session_id,
                        entry: Arc::new(entry),
                    });
                })
            },
            on_timeline_status: {
                let tx = tx.clone();
                Arc::new(move |status: TimelineStatus| {
                    // 只有 Closed 值得上报;重连是传输层自愈路径,不上屏。
                    if let TimelineStatus::Closed { session_id, reason } = status {
                        let _ = tx.send(BridgeEvent::TimelineLost {
                            session_id,
                            reason,
                            missing: false,
                        });
                    }
                })
            },
            on_timeline_snapshot: {
                let tx = tx.clone();
                Arc::new(move |page| {
                    let session_id = page.session_id.clone();
                    let _ = tx.send(BridgeEvent::TimelineSnapshot {
                        session_id,
                        page: Arc::new(page),
                    });
                })
            },
        };

        let this = Arc::downgrade(self);
        self.handle.spawn(async move {
            // daemon 首次冷启动(生成默认配置 + 初始化)可能超过单次等待窗口:
            // 整个 connect 失败后按固定间隔再试两次,失败才把相位交给 UI。
            let mut last_error = None;
            for attempt in 0..3u32 {
                if attempt > 0 {
                    let _ = tx.send(BridgeEvent::Opening);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                match connect_once(handlers.clone()).await {
                    Ok(client) => {
                        let client = Arc::new(client);
                        let epoch = client
                            .session_state()
                            .await
                            .map(|state| state.server_epoch)
                            .unwrap_or_default();
                        if let Some(this) = this.upgrade()
                            && let Ok(mut guard) = this.client.write()
                        {
                            *guard = Some(client.clone());
                        }
                        let _ = tx.send(BridgeEvent::Connected { epoch });
                        // 连接相位监听:续租失败重新协商时 watch 会送出新值,
                        // daemon 重启(epoch 变化)因此对 UI 可见。
                        watch_session_phase(client, tx).await;
                        return;
                    }
                    Err(error) => {
                        log::warn!("[qaqh-iced] connect attempt {} failed: {error}", attempt + 1);
                        last_error = Some(error);
                    }
                }
            }
            if let Some(error) = last_error {
                let _ = tx.send(BridgeEvent::ConnectFailed(error.to_string()));
            }
        });
    }

    /// 手动重连:关旧客户端并重建(重新读 discovery,daemon 重启/搬家均可恢复)。
    pub fn reconnect(self: &Arc<Self>) {
        if let Some(old) = self.client() {
            old.close();
        }
        if let Ok(mut guard) = self.client.write() {
            *guard = None;
        }
        self.emit(BridgeEvent::Opening);
        self.spawn_connect();
    }
}

/// 单次连接尝试(允许自动拉起 daemon,等待窗口覆盖首次冷启动)。
async fn connect_once(handlers: ClientHandlers) -> Result<Client, qaqh_client::ClientError> {
    Client::connect_async(ClientOptions {
        handlers,
        launch_daemon_if_missing: true,
        daemon_path: None,
        start_timeout: Duration::from_secs(20),
        remote: None,
    })
    .await
}

/// 监听 (server_epoch, client_session_id) watch:重新协商/daemon 重启对 UI 可见。
async fn watch_session_phase(client: Arc<Client>, tx: UnboundedSender<BridgeEvent>) {
    let mut rx = client.session_ctx_rx();
    loop {
        match rx.borrow_and_update().clone() {
            Some((epoch, _)) => {
                let _ = tx.send(BridgeEvent::Connected { epoch });
            }
            None => {
                let _ = tx.send(BridgeEvent::Opening);
            }
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
}

impl Bridge {
    /// 拉取权威配置(`config.load` → typed `ConfigDto`)。
    pub fn fetch_config(&self) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::ConfigLoaded(Err("未连接 daemon".into())));
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let result = match client.query(QueryRequest::ConfigLoad).await {
                Ok(value) => serde_json::from_value::<qaqh_config_api::ConfigDto>(value)
                    .map_err(|error| format!("config.load 形状不匹配: {error}")),
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(BridgeEvent::ConfigLoaded(result.map(Arc::new)));
        });
    }

    /// 保存设置(Merge Patch,只含脏字段;成功后自动回读权威快照)。
    ///
    /// `patch` 已在调用方过 `ConfigPatch::validate`;这里透传 daemon 复验。
    pub fn save_config(&self, patch: qaqh_config_api::ConfigPatch) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::ConfigSaved(Err("未连接 daemon".into())));
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let fields = match serde_json::to_value(&patch) {
                Ok(value) => value,
                Err(error) => {
                    let _ = tx.send(BridgeEvent::ConfigSaved(Err(error.to_string())));
                    return;
                }
            };
            let result = client
                .action(ActionRequest::ConfigSave { fields })
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            if result.is_ok() {
                // TUI 同款:写入成功后回读,权威快照取代本地缓冲。
                let reload = client.query(QueryRequest::ConfigLoad).await;
                let loaded = match reload {
                    Ok(value) => {
                        serde_json::from_value::<qaqh_config_api::ConfigDto>(value)
                            .map_err(|error| format!("已写入,但配置回读失败: {error}"))
                    }
                    Err(error) => Err(format!("已写入,但配置回读失败: {error}")),
                };
                let _ = tx.send(BridgeEvent::ConfigSaved(Ok(())));
                let _ = tx.send(BridgeEvent::ConfigLoaded(loaded.map(Arc::new)));
            } else {
                let _ = tx.send(BridgeEvent::ConfigSaved(result));
            }
        });
    }

    /// 切换活跃 profile(服务端单写口,写后广播 reload;成功后回读)。
    pub fn apply_profile(&self, name: String) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::ProfileApplied {
                name: name.clone(),
                result: Err("未连接 daemon".into()),
            });
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let result = client
                .action(ActionRequest::ProfileApply { name: name.clone() })
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            if result.is_ok()
                && let Ok(value) = client.query(QueryRequest::ConfigLoad).await
                    && let Ok(config) =
                        serde_json::from_value::<qaqh_config_api::ConfigDto>(value)
                {
                    let _ = tx.send(BridgeEvent::ConfigLoaded(Ok(Arc::new(config))));
                }
            let _ = tx.send(BridgeEvent::ProfileApplied { name, result });
        });
    }

    /// 拉取会话列表(侧栏)。
    pub fn fetch_sessions(&self) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::QueryError("未连接 daemon".into()));
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            match client.query(QueryRequest::SessionList).await {
                Ok(value) => {
                    let _ = tx.send(BridgeEvent::SessionList(parse_session_list(value)));
                }
                Err(error) => {
                    let _ = tx.send(BridgeEvent::QueryError(error.to_string()));
                }
            }
        });
    }

    /// 打开一个会话:attach → 激活 timeline → 认领 driver 席位。
    pub fn open_session(&self, session_id: String) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::QueryError("未连接 daemon".into()));
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            // attach 未落地时 timeline 读路径会 401,按 TUI 同款小步重试。
            for attempt in 0..10u32 {
                match client.attach(&session_id).await {
                    Ok(_) => break,
                    Err(error) if attempt < 9 => {
                        log::warn!("attach {session_id} 重试({attempt}): {error}");
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    Err(error) => {
                        let _ = tx.send(BridgeEvent::QueryError(format!(
                            "attach {session_id} 失败: {error}"
                        )));
                        return;
                    }
                }
            }
            if let Err(error) = client.activate_timeline_with(&session_id, Some(60)).await {
                let _ = tx.send(BridgeEvent::QueryError(format!(
                    "打开会话失败: {error}"
                )));
                return;
            }
            // 未决会话保持放行;席位被别人持有时忽略即可。
            if let Err(error) = client.claim_driver(&session_id).await {
                log::info!("claim_driver {session_id}: {error}");
            }
        });
    }

    /// 拉取权威快照(v2 单流建立后调用)。
    pub fn bootstrap(&self, session_id: String) {
        let Some(client) = self.client() else {
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            // 新会话首个 canonical 事实落盘前快照是 404/409,少量重试即可。
            for attempt in 0..5u32 {
                match client.bootstrap(&session_id).await {
                    Ok(bootstrap) => {
                        let _ = tx.send(BridgeEvent::Bootstrap {
                            session_id,
                            bootstrap: Arc::new(bootstrap),
                        });
                        return;
                    }
                    Err(qaqh_client::ClientError::Http { status: 404 | 409, .. })
                        if attempt < 4 =>
                    {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    Err(error) => {
                        log::warn!("bootstrap {session_id} 失败: {error}");
                        return;
                    }
                }
            }
        });
    }

    /// 向上翻一页历史。`before_index` 是排他游标(最旧已知回合的全局序号)。
    pub fn load_older(&self, session_id: String, before_index: u64) {
        let Some(client) = self.client() else {
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            match client
                .fetch_timeline_page(&session_id, Some(before_index), Some(30))
                .await
            {
                Ok(page) => {
                    let _ = tx.send(BridgeEvent::OlderPage {
                        session_id,
                        page: Arc::new(page),
                    });
                }
                Err(error) => {
                    let _ = tx.send(BridgeEvent::Ack {
                        label: "加载历史".into(),
                        ok: false,
                        error: Some(error.to_string()),
                    });
                }
            }
        });
    }

    /// 创建新会话;新会话经 `SessionCreated`(causation 匹配)回开。
    pub fn create_session(&self) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::Ack {
                label: "新会话".into(),
                ok: false,
                error: Some("未连接 daemon".into()),
            });
            return;
        };
        let command_id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut guard) = self.pending_creates.lock() {
            guard.insert(command_id.clone());
        }
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let command = RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: vec![],
            });
            let options = CommandOptions {
                command_id: Some(command_id.clone()),
                expected_revision: None,
                driver_epoch: None,
            };
            let rejected = matches!(
                client.send_command(None, command, options).await,
                Ok(ack) if ack.status == RingingCommandAckStatus::Rejected
            );
            if rejected {
                let _ = tx.send(BridgeEvent::Ack {
                    label: "新会话".into(),
                    ok: false,
                    error: Some("daemon 拒绝了创建请求".into()),
                });
            }
        });
    }

    /// 发送用户消息(触发一个新回合)。
    pub fn send_message(&self, session_id: String, text: String) {
        self.command(
            "发送",
            Some(session_id),
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                text,
                images: vec![],
                attachments: None,
                message_id: None,
                input_purpose: ConversationInputPurpose::TriggerTurn,
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
            }),
        );
    }

    /// 中止当前回合。
    pub fn cancel_turn(&self, session_id: String) {
        self.command(
            "中止",
            Some(session_id),
            RingingCommand::Conversation(ConversationCommand::ConversationCancel {
                turn_id: None,
            }),
        );
    }

    /// 权限应答(不经 driver 门控)。
    pub fn respond_permission(
        &self,
        session_id: String,
        tool_call_id: String,
        approved: bool,
        trust_folder: bool,
    ) {
        self.command(
            "权限应答",
            Some(session_id),
            RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                tool_call_id,
                approved,
                trust_folder,
            }),
        );
    }

    /// 提交 ask 答案。
    pub fn ask_respond(
        &self,
        session_id: String,
        interaction_id: String,
        answers: Vec<qaqh_client::AskAnswer>,
    ) {
        self.command(
            "提交回答",
            Some(session_id),
            RingingCommand::Control(ControlCommand::InteractionAskRespond {
                interaction_id,
                answers,
            }),
        );
    }

    /// 关闭 ask 交互不作答(中止被挂起的回合)。
    pub fn ask_dismiss(&self, session_id: String, interaction_id: String) {
        self.command(
            "跳过提问",
            Some(session_id),
            RingingCommand::Control(ControlCommand::InteractionAskDismiss { interaction_id }),
        );
    }

    /// 提交 plan 评审结论。
    pub fn plan_respond(
        &self,
        session_id: String,
        interaction_id: String,
        approved: bool,
        message: Option<String>,
    ) {
        self.command(
            "计划评审",
            Some(session_id),
            RingingCommand::Control(ControlCommand::PlanReviewRespond {
                interaction_id,
                approved,
                message,
                autonomous: false,
            }),
        );
    }

    /// 下载外置的交互正文(content ref)。
    pub fn fetch_interaction_body(
        &self,
        interaction_id: String,
        call_id: Option<String>,
        content_id: String,
    ) {
        let Some(client) = self.client() else {
            return;
        };
        let tx = self.tx.clone();
        self.handle.spawn(async move {
            let result = client
                .download_content_by_id(&content_id)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(BridgeEvent::InteractionBody {
                interaction_id,
                call_id,
                result,
            });
        });
    }

    /// 通用命令发送:ack(受理/拒绝)回 `Ack`。
    fn command(&self, label: &str, session_id: Option<String>, command: RingingCommand) {
        let Some(client) = self.client() else {
            self.emit(BridgeEvent::Ack {
                label: label.to_string(),
                ok: false,
                error: Some("未连接 daemon".into()),
            });
            return;
        };
        let tx = self.tx.clone();
        let label = label.to_string();
        self.handle.spawn(async move {
            let event = match client
                .send_command(session_id.as_deref(), command, CommandOptions::default())
                .await
            {
                Ok(ack) => match ack.status {
                    RingingCommandAckStatus::Accepted => BridgeEvent::Ack {
                        label,
                        ok: true,
                        error: None,
                    },
                    RingingCommandAckStatus::Rejected => BridgeEvent::Ack {
                        label,
                        ok: false,
                        error: Some(
                            ack.message
                                .or(ack.code)
                                .unwrap_or_else(|| "被拒绝".into()),
                        ),
                    },
                },
                Err(error) => BridgeEvent::Ack {
                    label,
                    ok: false,
                    error: Some(error.to_string()),
                },
            };
            let _ = tx.send(event);
        });
    }
}

/// `session.list` → 侧栏卡片(经共享的 `sanitize_session_list` 收敛)。
fn parse_session_list(value: serde_json::Value) -> Vec<SessionCard> {
    qaqh_client::sanitize_session_list(value)
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let session_id = entry.get("session_id")?.as_str()?.to_string();
                    Some(SessionCard {
                        session_id,
                        title: string_field(entry, "title"),
                        status: string_field(entry, "status"),
                        updated_at: entry
                            .get("updated_at")
                            .and_then(serde_json::Value::as_u64),
                        turn_count: entry
                            .get("turn_count")
                            .and_then(serde_json::Value::as_u64),
                        message_count: entry
                            .get("message_count")
                            .and_then(serde_json::Value::as_u64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn string_field(entry: &serde_json::Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}
