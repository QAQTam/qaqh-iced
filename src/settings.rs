//! 设置页状态:权威快照(ConfigDto)+ 编辑缓冲 + 脏字段 → ConfigPatch。
//!
//! 读写闭环对齐 TUI(`settings_ops.rs`):
//! `ConfigLoad` → 缓冲初始化 → 只把**改动过的**字段填进 [`ConfigPatch`] →
//! `validate()` → `ConfigSave` → 成功后回读权威快照。
//!
//! dirty 判定 = 缓冲值 ≠ 权威值,而不是「用户碰过没」:回读后即使缓冲被
//! 重置,判定也天然一致;Merge Patch 只发脏字段,多端并发编辑互不覆盖。

use qaqh_config_api::{ConfigDto, ConfigPatch, ExecPatch, SandboxPatch, SubagentPatch};

/// 设置页可编辑字段(消息路由与渲染共用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingField {
    ApiKey,
    Model,
    BaseUrl,
    MaxTokens,
    ContextLength,
    AutoCompactThreshold,
    ComplianceEnabled,
    SandboxEnabled,
    TokenizerPath,
    SessionIdleUnloadSecs,
    SubModel,
    SubBaseUrl,
    SubApiKey,
    SubMaxTokens,
    SubTimeoutSecs,
    SubDefaultTools,
    ExecDefaultShell,
}

/// 文本/数字缓冲。字符串字段直接存原文;数字字段存原文、保存时统一解析。
#[derive(Debug, Clone, Default)]
pub struct Buffers {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub max_tokens: String,
    pub context_length: String,
    pub auto_compact_threshold: String,
    pub compliance_enabled: bool,
    pub tokenizer_path: String,
    pub session_idle_unload_secs: String,
    pub sub_model: String,
    pub sub_base_url: String,
    pub sub_api_key: String,
    pub sub_max_tokens: String,
    pub sub_timeout_secs: String,
    pub sub_default_tools: String,
    pub exec_default_shell: String,
    // 下拉选择(持久值,不是缓冲)
    pub wire: String,
    pub credential: String,
    pub reasoning_effort: String,
    pub permission_level: u64,
    pub sandbox_enabled: bool,
}

/// 设置页状态。
#[derive(Debug, Default)]
pub struct SettingsPage {
    /// 权威快照(`config.load` 的最近一次回读);`None` = 未加载。
    pub config: Option<ConfigDto>,
    pub buffers: Buffers,
    pub loading: bool,
    pub saving: bool,
    /// 缓冲解析错误(数字字段非法等);保存前拦截。
    pub parse_error: Option<String>,
}

impl SettingsPage {
    /// 权威快照落地 → 重置全部缓冲(打开页面首次加载 / 保存后回读共用)。
    pub fn apply_config(&mut self, config: ConfigDto) {
        let sub = &config.subagent;
        self.buffers = Buffers {
            api_key: String::new(), // 密钥不回显:占位符显示「已配置/未配置」
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            max_tokens: config.max_tokens.to_string(),
            context_length: config.context_length.to_string(),
            auto_compact_threshold: format!("{}", config.auto_compact_threshold),
            compliance_enabled: config.compliance_enabled,
            tokenizer_path: config.tokenizer_path.clone().unwrap_or_default(),
            session_idle_unload_secs: config.session_idle_unload_secs.to_string(),
            sub_model: sub.model.clone(),
            sub_base_url: sub.base_url.clone(),
            sub_api_key: String::new(),
            sub_max_tokens: sub.max_tokens.to_string(),
            sub_timeout_secs: sub.timeout_secs.to_string(),
            sub_default_tools: sub.default_tools.join(", "),
            exec_default_shell: config.exec.default_shell.clone().unwrap_or_default(),
            wire: config.wire.clone(),
            credential: config.credential.clone(),
            reasoning_effort: config.reasoning_effort.clone(),
            permission_level: config.permission_level as u64,
            sandbox_enabled: config.sandbox.enabled,
        };
        self.config = Some(config);
        self.parse_error = None;
        self.saving = false;
        self.loading = false;
    }

    /// 缓冲 → 只含脏字段的 [`ConfigPatch`]。数字解析失败返回人类可读错误。
    pub fn build_patch(&self) -> Result<ConfigPatch, String> {
        let Some(config) = &self.config else {
            return Ok(ConfigPatch::default());
        };
        let b = &self.buffers;

        let parse_u64 = |raw: &str, label: &str| -> Result<Option<u64>, String> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("{label} 不是有效非负整数:{raw:?}"))
        };
        let changed_str = |raw: &str, current: &str| -> Option<String> {
            (raw != current).then(|| raw.to_string())
        };
        let changed_u64 = |raw: &str, current: u64, label: &str| -> Result<Option<u64>, String> {
            match parse_u64(raw, label)? {
                Some(value) if value != current => Ok(Some(value)),
                _ => Ok(None),
            }
        };
        let changed_f64 = |raw: &str, current: f64, label: &str| -> Result<Option<f64>, String> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let value: f64 = trimmed
                .parse()
                .map_err(|_| format!("{label} 不是有效数字:{raw:?}"))?;
            Ok((value != current).then_some(value))
        };
        let changed_bool = |raw: bool, current: bool| -> Option<bool> {
            (raw != current).then_some(raw)
        };

        let mut patch = ConfigPatch::default();

        // 密钥守卫:空缓冲 = 保持现值;只有显式输入才进 patch。
        if !b.api_key.trim().is_empty() {
            patch.api_key = Some(b.api_key.trim().to_string());
        }
        patch.model = changed_str(&b.model, &config.model);
        patch.base_url = changed_str(&b.base_url, &config.base_url);
        patch.wire = changed_str(&b.wire, &config.wire);
        patch.credential = changed_str(&b.credential, &config.credential);
        patch.max_tokens = changed_u64(&b.max_tokens, config.max_tokens, "maxTokens")?;
        patch.context_length =
            changed_u64(&b.context_length, config.context_length, "contextLength")?;
        patch.reasoning_effort = changed_str(&b.reasoning_effort, &config.reasoning_effort);
        patch.auto_compact_threshold = changed_f64(
            &b.auto_compact_threshold,
            config.auto_compact_threshold,
            "autoCompactThreshold",
        )?;
        patch.compliance_enabled = changed_bool(b.compliance_enabled, config.compliance_enabled);
        // tokenizer_path 是 Option<String>:空缓冲与 None 等价(启发式兜底)。
        {
            let current = config.tokenizer_path.as_deref().unwrap_or_default();
            if b.tokenizer_path != current {
                let trimmed = b.tokenizer_path.trim().to_string();
                patch.tokenizer_path = Some(if trimmed.is_empty() { String::new() } else { trimmed });
            }
        }
        patch.session_idle_unload_secs = changed_u64(
            &b.session_idle_unload_secs,
            config.session_idle_unload_secs,
            "sessionIdleUnloadSecs",
        )?;
        patch.permission_level = (b.permission_level != config.permission_level as u64)
            .then_some(b.permission_level);

        if b.exec_default_shell != config.exec.default_shell.as_deref().unwrap_or_default() {
            patch.exec = Some(ExecPatch {
                default_shell: Some(b.exec_default_shell.trim().to_string()),
            });
        }
        if b.sandbox_enabled != config.sandbox.enabled {
            patch.sandbox = Some(SandboxPatch {
                enabled: Some(b.sandbox_enabled),
            });
        }

        let sub_current = &config.subagent;
        let sub_api_key_dirty = !b.sub_api_key.trim().is_empty();
        let sub_default_tools: Option<Vec<String>> = {
            // 去重保序:重复项对 daemon 是无害噪音,不值得发一趟 patch。
            let mut parsed: Vec<String> = Vec::new();
            for item in b.sub_default_tools.split(',') {
                let item = item.trim();
                if !item.is_empty() && !parsed.iter().any(|known| known == item) {
                    parsed.push(item.to_string());
                }
            }
            (parsed != sub_current.default_tools).then_some(parsed)
        };
        let sub = SubagentPatch {
            model: changed_str(&b.sub_model, &sub_current.model),
            base_url: changed_str(&b.sub_base_url, &sub_current.base_url),
            api_key: sub_api_key_dirty.then(|| b.sub_api_key.trim().to_string()),
            max_tokens: changed_u64(&b.sub_max_tokens, sub_current.max_tokens, "subagent.maxTokens")?,
            timeout_secs: changed_u64(
                &b.sub_timeout_secs,
                sub_current.timeout_secs,
                "subagent.timeoutSecs",
            )?,
            default_tools: sub_default_tools,
            max_depth: None, // 只在 daemon 启动时读取,UI 不提供编辑
            message_in_flight_per_pair: None,
            message_outbound_per_sender: None,
        };
        if sub.model.is_some()
            || sub.base_url.is_some()
            || sub.api_key.is_some()
            || sub.max_tokens.is_some()
            || sub.timeout_secs.is_some()
            || sub.default_tools.is_some()
        {
            patch.subagent = Some(sub);
        }

        Ok(patch)
    }

    /// 是否有可保存的改动(解析合法的前提下)。
    pub fn has_changes(&self) -> bool {
        self.build_patch().is_ok_and(|patch| !patch.is_empty())
    }

    /// 凭据来源三档(pick_list 数据源;顺序即展示顺序)。
    pub fn credential_options() -> &'static [&'static str] {
        CREDENTIAL_VALUES
    }

    /// wire 四个规范拼写(读侧输出与写侧接受的同一套)。
    pub fn wire_options() -> &'static [&'static str] {
        WIRE_VALUES
    }

    pub fn effort_options() -> &'static [&'static str] {
        EFFORT_VALUES
    }

    pub fn permission_options() -> &'static [u64] {
        &[1, 2, 3, 4]
    }

    /// 密钥输入框的占位符(权威快照给出「已配置/未配置」)。
    pub fn api_key_placeholder(&self) -> &'static str {
        match &self.config {
            Some(config) if config.api_key == "****" => "已配置(留空保持;输入以更换)",
            _ => "未配置",
        }
    }

    pub fn sub_api_key_placeholder(&self) -> &'static str {
        match &self.config {
            Some(config) if config.subagent.api_key_set => {
                "已配置(留空保持;输入以更换)"
            }
            _ => "未配置",
        }
    }
}

/// 与 `ConfigPatch::validate` 的值域一致;`&str` 直接可作 pick_list 选项。
pub const CREDENTIAL_VALUES: &[&str] = &["api_key", "chatgpt_plan", "deepseek_account"];
pub const WIRE_VALUES: &[&str] = &["chat", "responses", "messages", "gemini"];
pub const EFFORT_VALUES: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 config-api 契约测试同源的完整 wire 形状(当前 camelCase 契约)。
    fn fixture() -> ConfigDto {
        serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "baseUrl": "https://x/v1",
            "wire": "chat",
            "credential": "api_key",
            "maxTokens": 96000,
            "contextLength": 1000000,
            "reasoningEffort": "max",
            "autoCompactThreshold": 0.95,
            "permissionLevel": 2,
            "apiKey": "****",
            "lang": null,
            "fontFamily": "",
            "theme": null,
            "notificationsEnabled": true,
            "activeProfile": "default",
            "profiles": ["default"],
            "complianceEnabled": false,
            "subagent": {
                "model": "",
                "baseUrl": "",
                "apiKey": "",
                "apiKeySet": false,
                "maxTokens": 4096,
                "timeoutSecs": 120,
                "defaultTools": ["read"]
            },
            "mcp": { "enabled": false, "idleShutdownSecs": 300, "servers": [] },
            "lsp": { "enabled": false, "idleShutdownSecs": 600, "servers": [] },
            "tokenizerPath": null,
            "exec": { "defaultShell": "bash" },
            "sandbox": { "enabled": true },
            "sessionIdleUnloadSecs": 900
        }))
        .expect("完整 wire 形状必须可解析")
    }

    fn loaded() -> SettingsPage {
        let mut page = SettingsPage::default();
        page.apply_config(fixture());
        page
    }

    /// 权威快照落地后,未编辑的缓冲必须产出空 patch(整包写回毒化的反闸)。
    #[test]
    fn clean_buffers_produce_empty_patch() {
        let page = loaded();
        let patch = page.build_patch().expect("clean build");
        assert!(patch.is_empty(), "{patch:?}");
    }

    /// 只发脏字段:改动的进 patch,未改动的不出现。
    #[test]
    fn dirty_fields_land_in_patch() {
        let mut page = loaded();
        page.buffers.model = "new-model".into();
        page.buffers.max_tokens = "128000".into();
        page.buffers.sub_timeout_secs = "240".into();
        let patch = page.build_patch().expect("dirty build");
        assert_eq!(patch.model.as_deref(), Some("new-model"));
        assert_eq!(patch.max_tokens, Some(128_000));
        assert_eq!(patch.subagent.expect("sub").timeout_secs, Some(240));
        // 未改字段不出现
        assert!(patch.base_url.is_none());
        assert!(patch.wire.is_none());
        assert!(patch.credential.is_none());
        assert!(patch.context_length.is_none());
    }

    /// 密钥守卫:掩码永不回传;空缓冲 = 保持现值;显式输入才进 patch。
    #[test]
    fn api_key_guard() {
        let mut page = loaded();
        // 掩码只出现在权威快照;缓冲始终为空 → 不进 patch。
        assert_eq!(page.config.as_ref().expect("cfg").api_key, "****");
        assert!(page.build_patch().expect("clean").api_key.is_none());
        page.buffers.api_key = "  sk-new  ".into();
        let patch = page.build_patch().expect("dirty");
        assert_eq!(patch.api_key.as_deref(), Some("sk-new"), "输入会被 trim");
    }

    /// 非法数字给出可读错误,而不是静默变成 0。
    #[test]
    fn invalid_number_is_reported() {
        let mut page = loaded();
        page.buffers.max_tokens = "abc".into();
        let error = page.build_patch().expect_err("必须报错");
        assert!(error.contains("maxTokens"), "{error}");
    }

    /// 布尔/枚举/档位的脏判定走同一套比较。
    #[test]
    fn picks_and_toggles_are_patched() {
        let mut page = loaded();
        page.buffers.sandbox_enabled = false;
        page.buffers.compliance_enabled = true;
        page.buffers.permission_level = 4;
        page.buffers.wire = "responses".into();
        page.buffers.credential = "chatgpt_plan".into();
        page.buffers.reasoning_effort = "high".into();
        page.buffers.auto_compact_threshold = "0.8".into();
        let patch = page.build_patch().expect("dirty");
        assert_eq!(
            patch.sandbox.as_ref().expect("sandbox").enabled,
            Some(false)
        );
        assert_eq!(patch.compliance_enabled, Some(true));
        assert_eq!(patch.permission_level, Some(4));
        assert_eq!(patch.wire.as_deref(), Some("responses"));
        assert_eq!(patch.credential.as_deref(), Some("chatgpt_plan"));
        assert_eq!(patch.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(patch.auto_compact_threshold, Some(0.8));
        patch.validate().expect("值域必须合法");
    }

    /// 工具白名单:逗号分隔解析 + 与权威值相同则不进 patch。
    #[test]
    fn subagent_tools_roundtrip() {
        let mut page = loaded();
        // 相同集合(顺序/空白无关)→ 不算改动
        page.buffers.sub_default_tools = " read ,  read ".into();
        let patch = page.build_patch().expect("build");
        assert!(patch.subagent.is_none(), "重复集合不算脏:{patch:?}");
        // 新集合 → 进 patch 且为解析后的列表
        page.buffers.sub_default_tools = "read, exec".into();
        let patch = page.build_patch().expect("build");
        assert_eq!(
            patch.subagent.expect("sub").default_tools,
            Some(vec!["read".to_string(), "exec".to_string()])
        );
    }
}
