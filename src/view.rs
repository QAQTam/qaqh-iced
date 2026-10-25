//! 界面渲染:侧栏 / 转录 / 审批卡 / 输入区 / 状态栏。
//!
//! 只消费投影后的状态(不碰协议类型);样式函数集中在 `style` 段,
//! 颜色一律 `color!`(0.14 的宏按 u8 取值)。

use iced::widget::markdown::{Bullet, Item};
use iced::widget::{button, checkbox, column, container, lazy, markdown, radio, rich_text, row,
                   rule, scrollable, text, text_editor, text_input, Space};
use iced::{color, keyboard, Alignment, Element, Font, Length, Padding, Theme};

use crate::message::Message;
use crate::session::{AskPanel, Block, Pending, PermissionPanel, PlanPanel, Turn};
use crate::state::{transcript_scroll_id, App, Phase};

// ───────────────────────── 样式 ─────────────────────────

pub(crate) mod style {
    use iced::widget::{button, container};
    use iced::Theme;

    /// 用户消息气泡(右侧)。
    pub fn user_bubble(theme: &Theme) -> container::Style {
        let palette = theme.extended_palette();
        container::Style {
            background: Some(palette.primary.weak.color.into()),
            text_color: Some(palette.primary.strong.text),
            border: iced::border::rounded(10),
            ..Default::default()
        }
    }

    /// 工具卡片 / 思考块。
    pub fn card(theme: &Theme) -> container::Style {
        let palette = theme.extended_palette();
        container::Style {
            background: Some(palette.background.weak.color.into()),
            border: iced::border::rounded(8),
            ..Default::default()
        }
    }

    /// 审批模态卡(强调边框)。
    pub fn approval(theme: &Theme) -> container::Style {
        let palette = theme.extended_palette();
        container::Style {
            background: Some(palette.secondary.weak.color.into()),
            border: iced::border::rounded(10),
            ..Default::default()
        }
    }

    /// 侧栏会话条目(选中态)。0.14 的按钮样式回调带 Status。
    pub fn session_selected(theme: &Theme, _status: button::Status) -> button::Style {
        let palette = theme.extended_palette();
        button::Style {
            background: Some(palette.primary.weak.color.into()),
            text_color: palette.primary.strong.text,
            border: iced::border::rounded(6),
            ..Default::default()
        }
    }

    pub fn session_idle(theme: &Theme, _status: button::Status) -> button::Style {
        let palette = theme.extended_palette();
        button::Style {
            background: Some(palette.background.weak.color.into()),
            text_color: palette.background.base.text,
            border: iced::border::rounded(6),
            ..Default::default()
        }
    }

    /// 主按钮(发送/允许)。
    pub fn primary(theme: &Theme, _status: button::Status) -> button::Style {
        let palette = theme.extended_palette();
        button::Style {
            background: Some(palette.primary.base.color.into()),
            text_color: palette.primary.base.text,
            border: iced::border::rounded(6),
            ..Default::default()
        }
    }

    /// 危险按钮(拒绝/中止)。
    pub fn danger(theme: &Theme, _status: button::Status) -> button::Style {
        let palette = theme.extended_palette();
        button::Style {
            background: Some(palette.danger.base.color.into()),
            text_color: palette.danger.base.text,
            border: iced::border::rounded(6),
            ..Default::default()
        }
    }

    pub fn ghost(theme: &Theme, _status: button::Status) -> button::Style {
        let palette = theme.extended_palette();
        button::Style {
            background: Some(palette.background.weak.color.into()),
            text_color: palette.background.base.text,
            border: iced::border::rounded(6),
            ..Default::default()
        }
    }
}

// ───────────────────────── 根布局 ─────────────────────────

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        row![sidebar(self), main_area(self)]
            .spacing(8)
            .padding(8)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// 侧栏:标题 + 连接状态 + 会话列表。
fn sidebar(app: &App) -> Element<'_, Message> {
    let header = column![
        text("QAQ-Harness").size(17),
        text(phase_label(app)).size(12).color(phase_color(app)),
    ]
    .spacing(2);

    let new_button = button(text("＋ 新会话").size(13))
        .on_press_maybe(app.has_session_support().then_some(Message::NewSession))
        .style(style::ghost)
        .width(Length::Fill);

    let settings_button = button(text("⚙ 设置").size(13))
        .on_press(Message::OpenSettings)
        .style(style::ghost)
        .width(Length::Fill);

    let selected = app.active.as_deref();
    let items = scrollable(
        column(
            app.sessions
                .iter()
                .map(|card| session_row(card, selected))
                .collect::<Vec<_>>(),
        )
        .spacing(2),
    )
    .height(Length::Fill)
    .width(Length::Fill);

    column![header, new_button, settings_button, items]
        .spacing(8)
        .width(260)
        .height(Length::Fill)
        .into()
}

fn session_row<'a>(
    card: &'a crate::message::SessionCard,
    selected: Option<&'a str>,
) -> Element<'a, Message> {
    let is_active = selected == Some(card.session_id.as_str());
    let status_icon = session_status_icon(&card.status);
    let label = column![
        text(session_display_title(card)).size(13),
        text(format!(
            "{status_icon} {} · {} 轮",
            card.status,
            card.turn_count.unwrap_or(0)
        ))
        .size(11)
        .color(color!(158, 166, 178)),
    ]
    .spacing(1);
    button(container(label).width(Length::Fill).padding(Padding::new(6.0)))
        .on_press(Message::SelectSession(card.session_id.clone()))
        .style(if is_active {
            style::session_selected
        } else {
            style::session_idle
        })
        .width(Length::Fill)
        .into()
}

/// 主区:连接中/失败/就绪三态。
fn main_area(app: &App) -> Element<'_, Message> {
    match &app.phase {
        Phase::Connecting => placeholder_view("正在连接 daemon…"),
        Phase::Failed(error) => column![
            text("连接失败").size(18),
            text(error.as_str()).size(13).color(color!(217, 115, 115)),
            button(text("重试").size(13))
                .on_press(Message::Retry)
                .style(style::primary)
                .padding(Padding::new(6.0).top(4).bottom(4)),
        ]
        .spacing(10)
        .align_x(Alignment::Center)
        .into(),
        Phase::Ready { .. } => match app.view_mode {
            crate::state::ViewMode::Settings => super::view_settings::settings_page(app),
            crate::state::ViewMode::Chat => ready_area(app),
        },
    }
}

fn ready_area(app: &App) -> Element<'_, Message> {
    let Some(active) = app.active.as_deref() else {
        return placeholder_view("没有会话 —— 点「新会话」开始");
    };
    let Some(state) = app.states.get(active) else {
        return placeholder_view("正在打开会话…");
    };

    let mut main = column![transcript(state)].spacing(8);

    if state.pending.is_some() {
        main = main.push(approval_card(state));
    }

    main = main.push(composer(app, state));
    main = main.push(rule::horizontal(1.0));
    main = main.push(status_bar(app, state));
    main.width(Length::Fill)
        .height(Length::Fill)
        .spacing(6)
        .into()
}

fn placeholder_view(label: &str) -> Element<'_, Message> {
    container(text(label).color(color!(140, 148, 161)))
        .width(Length::Fill)
        .height(Length::Fill)
        .center(Length::Fill)
        .into()
}

/// 填满剩余宽度的空隙(替代旧 helper `horizontal_space`)。
fn filler() -> Space {
    Space::new().width(Length::Fill)
}

// ───────────────────────── 转录 ─────────────────────────

fn transcript<'a>(state: &'a crate::session::SessionState) -> Element<'a, Message> {
    let mut list = column![].spacing(14);

    if state.truncated_before {
        list = list.push(
            text("⏳ 更早的历史已归档 —— 超出当前会话窗口,翻页取不到(数据仍在 daemon 归档里)")
                .size(12)
                .color(color!(191, 166, 77)),
        );
    }

    if state.has_more {
        let label = if state.loading_older {
            "加载中…"
        } else {
            "加载更早的历史"
        };
        list = list.push(
            row![
                filler(),
                button(text(label).size(12))
                    .on_press_maybe((!state.loading_older).then_some(Message::LoadOlder))
                    .style(style::ghost),
                filler(),
            ]
            .width(Length::Fill),
        );
    }

    for turn in &state.turns {
        // O(n) → O(活跃回合):依赖 (turn_id, revision) —— 回合内容每变一次
        // revision +1,封口回合依赖冻结,lazy 直接复用上次产出的元素树。
        list = list.push(lazy(
            (turn.turn_id.as_str(), turn.revision),
            move |_| turn_view(turn),
        ));
    }

    if state.turns.is_empty() {
        list = list.push(
            text("还没有消息 — 在下方输入第一条消息开始对话")
                .size(13)
                .color(color!(140, 148, 161)),
        );
    }

    scrollable(list.padding(Padding::new(4.0).right(8)))
        .id(transcript_scroll_id())
        .on_scroll(|viewport| Message::TranscriptScrolled(viewport.relative_offset().y))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// 单回合的 **owned** 渲染(lazy 闭包产出必须是 'static 元素树)。
///
/// 依赖数据在构建时克隆成 owned(String / spans Arc / 工具卡克隆)——
/// 只有该回合 revision 变化时才重建一次,所以克隆成本摊在变更上,
/// 而不是每帧全量克隆(那是另一种 O(n)/帧)。
fn turn_view(turn: &Turn) -> Element<'static, Message> {
    let mut items = column![].spacing(8);

    if !turn.user_text.is_empty() {
        let bubble = container(text(turn.user_text.clone()).size(14))
            .padding(Padding::new(10.0))
            .max_width(560)
            .style(style::user_bubble);
        items = items.push(row![filler(), bubble].width(Length::Fill));
    }

    for round in &turn.rounds {
        for block in &round.blocks {
            items = items.push(block_view(block));
        }
    }

    if let Some(failure) = &turn.failure {
        let label = if failure.message.is_empty() {
            failure.code.clone()
        } else {
            format!("{} — {}", failure.code, failure.message)
        };
        items = items.push(
            text(format!("⚠ 回合未完成:{label}"))
                .size(12)
                .color(color!(230, 140, 89)),
        );
    } else if turn.state == qaqh_client::TimelineTurnState::Failed {
        items = items.push(text("⚠ 回合失败").size(12).color(color!(230, 115, 115)));
    } else if turn.state == qaqh_client::TimelineTurnState::Cancelled {
        items = items.push(
            text("回合已取消")
                .size(12)
                .color(color!(166, 173, 184)),
        );
    }

    items.into()
}

/// 设置页 plan 预览仍走标准 markdown 视图(借用渲染,不进 lazy)。
fn markdown_element(md: &iced::widget::markdown::Content) -> Element<'_, Message> {
    markdown::view(
        md.items(),
        markdown::Settings::with_text_size(13, markdown::Style::from(Theme::Dark)),
    )
    .map(Message::LinkClicked)
}

fn block_view(block: &Block) -> Element<'static, Message> {
    match block {
        Block::Text { md, .. } => owned_markdown(md.items()),
        Block::Reasoning { raw, state, .. } => {
            if raw.trim().is_empty() {
                return Space::new().height(0).into();
            }
            let marker = if *state == qaqh_client::TimelineBlockState::Open {
                "思考中…"
            } else {
                "思考"
            };
            container(
                column![
                    text(marker.to_string()).size(11).color(color!(128, 135, 148)),
                    text(raw.clone()).size(12).color(color!(158, 166, 178)),
                ]
                .spacing(2),
            )
            .padding(Padding::new(10.0).left(12.0))
            .style(style::card)
            .width(Length::Fill)
            .into()
        }
        Block::Tool { tool, progress, .. } => tool_card(tool, progress),
        Block::Notice { text: body, .. } => text(body.clone())
            .size(12)
            .color(color!(153, 161, 173))
            .into(),
    }
}

// ── owned markdown:lazy 内的 'static markdown 渲染 ──
//
// `markdown::view` 借用 items,进不了 lazy 的 Element<'static> 约束。
// 这里的关键观察:`markdown::Text::spans(style)` 返回 **owned 的**
// `Arc<[Span<'static, Uri>]>`(行内样式/链接全保真),所以块级外观
// 自己搭、行内内容走 spans,即可全 owned。块级外观对聊天场景做了
// 合理简化(标题字号、代码块容器、列表缩进;表格降级为行)。

const MD_BASE: f32 = 14.0;

fn md_style() -> markdown::Style {
    markdown::Style::from(Theme::Dark)
}

fn md_rich(text: &markdown::Text, size: f32) -> Element<'static, Message> {
    rich_text(text.spans(md_style()))
        .size(size)
        .on_link_click(Message::LinkClicked)
        .into()
}

fn owned_markdown(items: &[Item]) -> Element<'static, Message> {
    owned_items(items, 0, MD_BASE)
}

fn owned_items(items: &[Item], depth: usize, base: f32) -> Element<'static, Message> {
    let mut column_ = column![].spacing(base * 0.55);
    for item in items {
        column_ = column_.push(owned_item(item, depth, base));
    }
    column_.into()
}

fn owned_item(item: &Item, depth: usize, base: f32) -> Element<'static, Message> {
    match item {
        Item::Heading(level, text) => {
            let size = match level {
                markdown::HeadingLevel::H1 => base + 6.0,
                markdown::HeadingLevel::H2 => base + 4.0,
                markdown::HeadingLevel::H3 => base + 2.0,
                _ => base,
            };
            md_rich(text, size)
        }
        Item::Paragraph(text) => md_rich(text, base),
        Item::CodeBlock { lines, .. } => {
            let mut block = column![].spacing(1);
            for line in lines {
                let code_line: Element<'static, Message> = rich_text(line.spans(md_style()))
                    .font(Font::MONOSPACE)
                    .size(base - 2.0)
                    .on_link_click(Message::LinkClicked)
                    .into();
                block = block.push(code_line);
            }
            container(block.padding(8.0))
                .width(Length::Fill)
                .style(style::card)
                .into()
        }
        Item::List { bullets, .. } => {
            let mut list = column![].spacing(3);
            for bullet in bullets {
                let marker = match bullet {
                    Bullet::Task { done: true, .. } => "☑",
                    Bullet::Task { .. } => "☐",
                    Bullet::Point { .. } => "•",
                };
                let content = match bullet {
                    Bullet::Point { items } | Bullet::Task { items, .. } => {
                        owned_items(items, depth + 1, base)
                    }
                };
                list = list.push(
                    row![
                        text(marker.to_string())
                            .size(base - 1.0)
                            .color(color!(140, 148, 161)),
                        content,
                    ]
                    .spacing(6)
                    .align_y(Alignment::Start),
                );
            }
            container(list)
            .padding(Padding::new(0.0).left(14.0 * depth.min(1) as f32))
            .into()
        }
        Item::Quote(items) => container(owned_items(items, depth, base - 0.5))
            .padding(Padding::new(8.0).left(12.0))
            .style(style::card)
            .width(Length::Fill)
            .into(),
        Item::Image { url, alt, .. } => column![
            md_rich(alt, base),
            text(url.clone())
                .size(base - 3.0)
                .color(color!(120, 127, 138)),
        ]
        .spacing(2)
        .into(),
        Item::Rule => rule::horizontal(1.0).into(),
        // 表格:Row.cells 是私有的,公共 API 渲染不了 —— 聊天输出里罕见,
        // 降级为占位说明(设置页的 plan 预览走标准 markdown::view,不受此限)。
        Item::Table { .. } => text("[表格内容请参考原文]").size(base - 2.0).into(),
    }
}

/// 工具调用卡:状态图标 + 标题行 + (可选)输出窗口。owned 版本。
fn tool_card(tool: &qaqh_client::TimelineTool, progress: &str) -> Element<'static, Message> {
    let (icon, icon_color) = match tool.state {
        qaqh_client::TimelineToolState::Prepared
        | qaqh_client::TimelineToolState::Running => ("◐", color!(140, 166, 242)),
        qaqh_client::TimelineToolState::Succeeded => ("✓", color!(115, 191, 128)),
        qaqh_client::TimelineToolState::Backgrounded => ("⏸", color!(191, 166, 77)),
        qaqh_client::TimelineToolState::Failed | qaqh_client::TimelineToolState::Cancelled => {
            ("✗", color!(217, 115, 115))
        }
    };

    let name = if tool.name.is_empty() {
        "工具".to_string()
    } else {
        tool.name.clone()
    };
    let detail = tool_header_detail(tool)
        .or_else(|| {
            tool.display
                .as_ref()
                .and_then(|display| display.summary.clone())
        })
        .or_else(|| tool.summary.clone())
        .unwrap_or_default();

    let mut card = column![
        row![
            text(icon).color(icon_color).size(13),
            text(name).size(13),
            text(detail).size(12).color(color!(158, 166, 178)),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    ]
    .spacing(4);

    // 行数摘要(diff 类工具)
    if let Some(display) = &tool.display
        && (display.lines_added > 0 || display.lines_removed > 0)
    {
        card = card.push(
            text(format!(
                "+{} / -{} 行",
                display.lines_added, display.lines_removed
            ))
            .size(11)
            .color(color!(115, 191, 128)),
        );
    }

    // 输出:运行中看 progress,终态看 output(尾部窗口展示)
    let output = if matches!(
        tool.state,
        qaqh_client::TimelineToolState::Running | qaqh_client::TimelineToolState::Prepared
    ) {
        (!progress.is_empty()).then(|| tail_chars(progress, 1200))
    } else {
        tool.output
            .as_deref()
            .filter(|output| !output.trim().is_empty())
            .map(|output| tail_chars(output, 1600))
    };
    if let Some(output) = output {
        card = card.push(
            container(
                text(output)
                    .font(Font::MONOSPACE)
                    .size(11)
                    .color(color!(179, 186, 199)),
            )
            .width(Length::Fill),
        );
    }

    if let Some(failure) = &tool.failure
        && !failure.message.is_empty()
    {
        card = card.push(
            text(format!("{} — {}", failure.code, failure.message))
                .size(11)
                .color(color!(217, 128, 115)),
        );
    }

    container(card.padding(8.0))
        .width(Length::Fill)
        .max_width(760)
        .style(style::card)
        .into()
}

fn tool_header_detail(tool: &qaqh_client::TimelineTool) -> Option<String> {
    let header = tool.display.as_ref()?.header.as_ref()?;
    Some(match header {
        qaqh_client::TimelineToolHeader::Path { path, op } => {
            format!("{op:?} {path}").to_lowercase()
        }
        qaqh_client::TimelineToolHeader::Shell { command } => command.clone(),
        qaqh_client::TimelineToolHeader::Query { query, .. } => query.clone(),
        qaqh_client::TimelineToolHeader::Other { label } => label.clone(),
        qaqh_client::TimelineToolHeader::Unknown => return None,
    })
}

/// 取尾部 N 个字符(输出窗口),保留 UTF-8 边界。
fn tail_chars(source: &str, max_chars: usize) -> String {
    let count = source.chars().count();
    if count <= max_chars {
        return source.to_string();
    }
    let skip = count - max_chars;
    let truncated: String = source.chars().skip(skip).collect();
    format!("…{truncated}")
}

// ───────────────────────── 审批卡 ─────────────────────────

fn approval_card<'a>(state: &'a crate::session::SessionState) -> Element<'a, Message> {
    match state.pending.as_ref() {
        Some(Pending::Permission(panel)) => permission_card(panel),
        Some(Pending::Ask(panel)) => ask_card(panel),
        Some(Pending::Plan(panel)) => plan_card(panel),
        None => Space::new().height(0).into(),
    }
}

fn permission_card(panel: &PermissionPanel) -> Element<'_, Message> {
    let risk = if panel.risk.is_empty() {
        String::new()
    } else {
        format!(" · 风险:{}", panel.risk)
    };
    let mut body = column![text(format!("🔐 权限请求 — {}", panel.tool_name)).size(15)].spacing(6);

    if let Some(summary) = &panel.action_summary
        && !summary.trim().is_empty()
    {
        body = body.push(text(summary.as_str()).size(13));
    }
    if !panel.reason.trim().is_empty() {
        body = body.push(text(panel.reason.as_str()).size(13));
    }
    for path in &panel.paths {
        body = body.push(
            text(path.as_str())
                .font(Font::MONOSPACE)
                .size(11)
                .color(color!(179, 186, 199)),
        );
    }
    if !panel.consequence.trim().is_empty() {
        body = body.push(
            text(panel.consequence.as_str())
                .size(12)
                .color(color!(158, 166, 178)),
        );
    }
    body = body.push(
        text(format!(
            "类别 {} · 级别 {}{risk} — 允许后本轮继续执行",
            if panel.category.is_empty() { "未标注" } else { &panel.category },
            panel.level
        ))
        .size(11)
        .color(color!(140, 148, 161)),
    );
    body = body.push(
        checkbox(panel.trust_folder)
            .label("信任此目录(后续同类操作不再询问)")
            .on_toggle(|_| Message::PermissionToggleTrust)
            .size(14)
            .text_size(12),
    );
    body = body.push(
        row![
            button(text("允许").size(13))
                .on_press(Message::PermissionDecide {
                    approved: true,
                    trust_folder: panel.trust_folder,
                })
                .style(style::primary)
                .padding(Padding::new(6.0).top(4).bottom(4)),
            button(text("拒绝").size(13))
                .on_press(Message::PermissionDecide {
                    approved: false,
                    trust_folder: false,
                })
                .style(style::danger)
                .padding(Padding::new(6.0).top(4).bottom(4)),
        ]
        .spacing(8),
    );

    container(body.padding(12))
        .width(Length::Fill)
        .style(style::approval)
        .into()
}

fn ask_card(panel: &AskPanel) -> Element<'_, Message> {
    let title = if panel.mode == qaqh_client::AskMode::Batch {
        "❓ 模型提问(多题)"
    } else {
        "❓ 模型提问"
    };
    let mut body = column![text(title).size(15)].spacing(10);

    for (index, question) in panel.questions.iter().enumerate() {
        let mut question_column = column![text(question.question.as_str()).size(13)].spacing(4);

        for (option_index, option) in question.options.iter().enumerate() {
            question_column = question_column.push(
                radio(
                    option.as_str(),
                    option_index,
                    panel.selections.get(index).copied().flatten(),
                    move |_| Message::AskSelect {
                        question: index,
                        option: option_index,
                    },
                )
                .size(14)
                .text_size(12)
                .spacing(6),
            );
        }

        if question.allow_custom {
            let empty = String::new();
            let value = panel.customs.get(index).unwrap_or(&empty);
            question_column = question_column.push(
                text_input("自定义回答(可选)", value)
                    .on_input(move |value| Message::AskCustom {
                        question: index,
                        value,
                    })
                    .size(12)
                    .padding(4),
            );
        }

        body = body.push(question_column);
    }

    body = body.push(
        row![
            button(text("提交回答").size(13))
                .on_press(Message::AskSubmit)
                .style(style::primary)
                .padding(Padding::new(6.0).top(4).bottom(4)),
            button(text("跳过(中止回合)").size(13))
                .on_press(Message::AskDismiss)
                .style(style::danger)
                .padding(Padding::new(6.0).top(4).bottom(4)),
        ]
        .spacing(8),
    );

    container(scrollable(body.padding(12)))
        .width(Length::Fill)
        .max_height(320)
        .style(style::approval)
        .into()
}

fn plan_card(panel: &PlanPanel) -> Element<'_, Message> {
    let mut body = column![text(format!("📋 计划评审({})", panel.review_type)).size(15)].spacing(8);

    body = body.push(
        container(markdown_element(&panel.md))
            .max_height(280)
            .width(Length::Fill),
    );
    body = body.push(
        text_input("拒绝理由(可选,拒绝时生效)", &panel.message)
            .on_input(Message::PlanDraft)
            .size(12)
            .padding(4),
    );
    body = body.push(
        row![
            button(text("批准执行").size(13))
                .on_press(Message::PlanDecide { approved: true })
                .style(style::primary)
                .padding(Padding::new(6.0).top(4).bottom(4)),
            button(text("拒绝").size(13))
                .on_press(Message::PlanDecide { approved: false })
                .style(style::danger)
                .padding(Padding::new(6.0).top(4).bottom(4)),
        ]
        .spacing(8),
    );

    container(body.padding(12))
        .width(Length::Fill)
        .style(style::approval)
        .into()
}

// ───────────────────────── 输入区 ─────────────────────────

fn composer<'a>(app: &'a App, state: &'a crate::session::SessionState) -> Element<'a, Message> {
    let streaming = state.is_streaming();
    let disabled = state.read_only;

    let editor = text_editor(&app.composer)
        .placeholder("输入消息…(Enter 发送,Shift+Enter 换行)")
        .on_action(Message::Composer)
        .key_binding(composer_key_binding)
        .height(Length::Fixed(92.0))
        .padding(6);

    let send_button = button(text("发送").size(13))
        .on_press_maybe((!streaming && !disabled).then_some(Message::Send))
        .style(style::primary)
        .padding(Padding::new(10.0).top(6).bottom(6));

    let cancel_button = button(text("中止").size(13))
        .on_press_maybe(streaming.then_some(Message::Cancel))
        .style(style::danger)
        .padding(Padding::new(10.0).top(6).bottom(6));

    let mut right = column![send_button].spacing(4);
    if streaming {
        right = right.push(cancel_button);
    }

    row![editor, right]
        .align_y(Alignment::End)
        .spacing(8)
        .width(Length::Fill)
        .into()
}

/// Enter 发送、Shift+Enter 换行;其余按键走默认绑定。
fn composer_key_binding(keypress: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    if matches!(keypress.key.as_ref(), keyboard::Key::Named(keyboard::key::Named::Enter))
        && !keypress.modifiers.shift()
    {
        return Some(text_editor::Binding::Custom(Message::Send));
    }
    text_editor::Binding::from_key_press(keypress)
}

// ───────────────────────── 状态栏 ─────────────────────────

fn status_bar<'a>(app: &'a App, state: &'a crate::session::SessionState) -> Element<'a, Message> {
    let mut items = column![].spacing(2);

    let mut top = row![text(state.display_title()).size(12)].spacing(10);
    if let Some(activity) = &state.activity {
        let (label, activity_color) = match activity {
            qaqh_client::ClientV2ActivityState::Running => ("● 运行中", color!(115, 191, 128)),
            qaqh_client::ClientV2ActivityState::Interrupted => {
                ("● 已暂停(等待处理)", color!(191, 166, 77))
            }
            qaqh_client::ClientV2ActivityState::Idle => ("● 空闲", color!(140, 148, 161)),
        };
        top = top.push(text(label).size(12).color(activity_color));
    }
    if state.read_only {
        top = top.push(
            text("只读(等待重新定基)")
                .size(12)
                .color(color!(191, 166, 77)),
        );
    }
    items = items.push(top);

    let mut bottom = row![
        text(phase_label(app)).size(11).color(phase_color(app)),
        text(short_id(&state.session_id))
            .size(11)
            .color(color!(120, 127, 138)),
    ]
    .spacing(12);
    if let Some(usage) = &state.usage {
        bottom = bottom.push(
            text(format!(
                "输入 {} · 输出 {} · 合计 {}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            ))
            .size(11)
            .color(color!(140, 148, 161)),
        );
    }
    items = items.push(bottom);

    if let Some(notice) = &app.notice {
        items = items.push(
            text(notice.as_str())
                .size(11)
                .color(color!(230, 153, 102)),
        );
    }

    items.into()
}

// ───────────────────────── 小工具 ─────────────────────────

fn phase_label(app: &App) -> String {
    match &app.phase {
        Phase::Connecting => "连接中…".into(),
        Phase::Ready { epoch } => format!("已连接 · epoch {}", short_id(epoch)),
        Phase::Failed(error) => format!("连接失败:{error}"),
    }
}

fn phase_color(app: &App) -> iced::Color {
    match &app.phase {
        Phase::Connecting => color!(191, 166, 77),
        Phase::Ready { .. } => color!(115, 191, 128),
        Phase::Failed(_) => color!(217, 115, 115),
    }
}

fn session_display_title(card: &crate::message::SessionCard) -> String {
    if card.title.trim().is_empty() {
        "未命名会话".to_string()
    } else {
        card.title.clone()
    }
}

fn session_status_icon(status: &str) -> &'static str {
    match status {
        "working" | "running" => "▶",
        "waiting" | "waiting_user" => "⏸",
        "completed" => "✓",
        "failed" | "error" => "✗",
        _ => "·",
    }
}

/// epoch/id 的前 8 位展示。
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
