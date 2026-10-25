//! 设置页渲染:分组卡片 + 字段行。
//!
//! 数据全部来自 `settings::SettingsPage`(权威 ConfigDto + 编辑缓冲);
//! 保存走 `ConfigPatch::validate` → `config.save`(Merge Patch),本文件不做
//! 任何值域判断,只负责把缓冲与控件连起来。

use iced::widget::{button, checkbox, column, container, pick_list, row, rule, scrollable, text,
                   text_input, Space};
use iced::{color, Alignment, Element, Length, Padding};

use crate::message::Message;
use crate::settings::{SettingField, SettingsPage};
use crate::state::App;

use super::view::style;

const LABEL_WIDTH: f32 = 170.0;

pub fn settings_page(app: &App) -> Element<'_, Message> {
    let settings = &app.settings;

    // 头部:标题 + 返回 / 刷新 / 保存
    let can_save = settings.config.is_some()
        && !settings.saving
        && settings.parse_error.is_none()
        && settings.has_changes();
    let header = row![
        text("设置").size(18),
        button(text("返回").size(12))
            .on_press(Message::CloseSettings)
            .style(style::ghost)
            .padding(Padding::new(8.0).top(3.0).bottom(3.0)),
        Space::new().width(Length::Fill),
        button(text(if settings.loading { "加载中…" } else { "刷新" }).size(12))
            .on_press_maybe((!settings.loading).then_some(Message::SettingsReload))
            .style(style::ghost)
            .padding(Padding::new(8.0).top(3.0).bottom(3.0)),
        button(text(if settings.saving { "保存中…" } else { "保存" }).size(13))
            .on_press_maybe(can_save.then_some(Message::SettingsSave))
            .style(style::primary)
            .padding(Padding::new(12.0).top(4.0).bottom(4.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let mut page = column![header].spacing(10);

    // 状态行:解析错误 / 保存中 / 无权威快照
    if let Some(error) = &settings.parse_error {
        page = page.push(
            text(format!("⚠ {error}"))
                .size(12)
                .color(color!(230, 140, 89)),
        );
    }
    if settings.config.is_none() {
        page = page.push(
            text(if settings.loading {
                "正在加载配置…"
            } else {
                "配置尚未加载 —— 点「刷新」重试"
            })
            .size(13)
            .color(color!(140, 148, 161)),
        );
        return scrollable(page.padding(8).width(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    let Some(config) = settings.config.as_ref() else {
        return Space::new().into();
    };

    page = page.push(connection_section(settings));
    page = page.push(rule::horizontal(1.0));
    page = page.push(generation_section(settings));
    page = page.push(rule::horizontal(1.0));
    page = page.push(behavior_section(settings));
    page = page.push(rule::horizontal(1.0));
    page = page.push(subagent_section(settings));
    page = page.push(rule::horizontal(1.0));
    page = page.push(profile_section(config));
    page = page.push(
        text("MCP / LSP 配置当前为只读(写模型未开放),请直接编辑 daemon 的 config.toml。")
            .size(11)
            .color(color!(120, 127, 138)),
    );

    scrollable(page.padding(8).width(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

// ───────────────────────── 分区 ─────────────────────────

fn connection_section(settings: &SettingsPage) -> Element<'_, Message> {
    let b = &settings.buffers;
    let mut section = section_header("连接");

    section = section.push(field_row(
        "凭据来源",
        pick_list(
            SettingsPage::credential_options(),
            Some(b.credential.as_str()),
            |value| Message::SettingsPickCredential(value.to_string()),
        )
        .width(Length::Fixed(220.0)),
        Some(match b.credential.as_str() {
            "chatgpt_plan" | "deepseek_account" => "额度档:登录管理请使用 TUI 的账号页",
            _ => "BYOK:下方填 API Key",
        }),
    ));
    section = section.push(field_row(
        "API Key",
        text_input(settings.api_key_placeholder(), &b.api_key)
            .secure(true)
            .on_input(|value| Message::SettingsText(SettingField::ApiKey, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    section = section.push(field_row(
        "Base URL",
        text_input("", &b.base_url)
            .on_input(|value| Message::SettingsText(SettingField::BaseUrl, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    section = section.push(field_row(
        "协议 (wire)",
        pick_list(
            SettingsPage::wire_options(),
            Some(b.wire.as_str()),
            |value| Message::SettingsPickWire(value.to_string()),
        )
        .width(Length::Fixed(220.0)),
        None,
    ));
    section = section.push(field_row(
        "模型",
        text_input("", &b.model)
            .on_input(|value| Message::SettingsText(SettingField::Model, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    card(section).into()
}

fn generation_section(settings: &SettingsPage) -> Element<'_, Message> {
    let b = &settings.buffers;
    let mut section = section_header("生成");

    section = section.push(field_row(
        "最大输出 tokens",
        number_input(&b.max_tokens, SettingField::MaxTokens),
        None,
    ));
    section = section.push(field_row(
        "上下文窗口",
        number_input(&b.context_length, SettingField::ContextLength),
        Some("本地压缩的唯一分母"),
    ));
    section = section.push(field_row(
        "推理力度",
        pick_list(
            SettingsPage::effort_options(),
            Some(b.reasoning_effort.as_str()),
            |value| Message::SettingsPickEffort(value.to_string()),
        )
        .width(Length::Fixed(220.0)),
        None,
    ));
    section = section.push(field_row(
        "自动压缩阈值",
        text_input("0 = 关闭", &b.auto_compact_threshold)
            .on_input(|value| Message::SettingsText(SettingField::AutoCompactThreshold, value))
            .width(Length::Fixed(220.0)),
        Some("[0, 1];0 = 关闭自动压缩"),
    ));
    card(section).into()
}

fn behavior_section(settings: &SettingsPage) -> Element<'_, Message> {
    let b = &settings.buffers;
    let mut section = section_header("行为");

    section = section.push(field_row(
        "权限档位",
        pick_list(
            SettingsPage::permission_options(),
            Some(b.permission_level),
            Message::SettingsPickPermission,
        )
        .width(Length::Fixed(220.0)),
        Some("1 只读 · 2 工作区写 · 3 跳过权限 · 4 沙箱运行"),
    ));
    section = section.push(field_row(
        "沙箱",
        checkbox(b.sandbox_enabled)
            .label("启用命令沙箱隔离")
            .on_toggle(|value| Message::SettingsToggle(SettingField::SandboxEnabled, value))
            .size(14)
            .text_size(12),
        None,
    ));
    section = section.push(field_row(
        "合规守卫",
        checkbox(b.compliance_enabled)
            .label("启用内容合规检查")
            .on_toggle(|value| Message::SettingsToggle(SettingField::ComplianceEnabled, value))
            .size(14)
            .text_size(12),
        None,
    ));
    section = section.push(field_row(
        "空闲卸载(秒)",
        number_input(&b.session_idle_unload_secs, SettingField::SessionIdleUnloadSecs),
        Some("0 = 禁用自动卸载"),
    ));
    section = section.push(field_row(
        "默认 shell",
        text_input("auto = 自动探测", &b.exec_default_shell)
            .on_input(|value| Message::SettingsText(SettingField::ExecDefaultShell, value))
            .width(Length::Fixed(220.0)),
        None,
    ));
    section = section.push(field_row(
        "tokenizer 路径",
        text_input("空 = 启发式估算", &b.tokenizer_path)
            .on_input(|value| Message::SettingsText(SettingField::TokenizerPath, value))
            .width(Length::Fixed(360.0)),
        Some("变更需重启 daemon"),
    ));
    card(section).into()
}

fn subagent_section(settings: &SettingsPage) -> Element<'_, Message> {
    let b = &settings.buffers;
    let mut section = section_header("子代理");

    section = section.push(field_row(
        "模型",
        text_input("空 = 继承主配置", &b.sub_model)
            .on_input(|value| Message::SettingsText(SettingField::SubModel, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    section = section.push(field_row(
        "Base URL",
        text_input("空 = 继承主配置", &b.sub_base_url)
            .on_input(|value| Message::SettingsText(SettingField::SubBaseUrl, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    section = section.push(field_row(
        "API Key",
        text_input(settings.sub_api_key_placeholder(), &b.sub_api_key)
            .secure(true)
            .on_input(|value| Message::SettingsText(SettingField::SubApiKey, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    section = section.push(field_row(
        "最大输出 tokens",
        number_input(&b.sub_max_tokens, SettingField::SubMaxTokens),
        None,
    ));
    section = section.push(field_row(
        "超时(秒)",
        number_input(&b.sub_timeout_secs, SettingField::SubTimeoutSecs),
        None,
    ));
    section = section.push(field_row(
        "工具白名单",
        text_input("逗号分隔;空 = 全部可用", &b.sub_default_tools)
            .on_input(|value| Message::SettingsText(SettingField::SubDefaultTools, value))
            .width(Length::Fixed(360.0)),
        None,
    ));
    card(section).into()
}

fn profile_section(config: &qaqh_config_api::ConfigDto) -> Element<'_, Message> {
    let mut section = section_header("Profile");

    section = section.push(field_row(
        "活跃 Profile",
        text(config.active_profile.as_str()).size(13),
        None,
    ));
    if config.profiles.is_empty() {
        section = section.push(field_row("可切换", text("无").size(13), None));
    } else {
        section = section.push(field_row(
            "切换到",
            pick_list(
                config.profiles.as_slice(),
                config.profiles.first().cloned(),
                Message::ProfileApply,
            )
            .width(Length::Fixed(220.0)),
            Some("选择即应用(服务端单写口,写后广播重载)"),
        ));
    }
    card(section).into()
}

// ───────────────────────── 小构件 ─────────────────────────

fn section_header(title: &str) -> iced::widget::Column<'_, Message> {
    column![text(title.to_string()).size(15)].spacing(8)
}

fn card(content: iced::widget::Column<'_, Message>) -> iced::widget::Container<'_, Message> {
    container(content.padding(12))
        .width(Length::Fill)
        .max_width(860)
        .style(style::card)
}

fn field_row<'a>(
    label: &str,
    control: impl Into<Element<'a, Message>>,
    hint: Option<&str>,
) -> Element<'a, Message> {
    let control: Element<'a, Message> = control.into();
    let mut line = row![
        text(label.to_string())
            .size(13)
            .width(Length::Fixed(LABEL_WIDTH)),
        control,
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    if let Some(hint) = hint {
        line = line.push(text(hint.to_string()).size(11).color(color!(140, 148, 161)));
    }
    line.into()
}

fn number_input(value: &str, field: SettingField) -> iced::widget::TextInput<'_, Message> {
    text_input("", value)
        .on_input(move |input| Message::SettingsText(field, input))
        .width(Length::Fixed(220.0))
}
