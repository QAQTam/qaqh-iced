//! QAQ-Harness 的 iced 桌面前端。
//!
//! 传输层全部复用 `qaqh-client`(与 TUI / 桌面壳同源):发现/lease、per-seed
//! 单流与 timeline 流、命令面与服务面都不在本仓实现。本仓只做 iced 的
//! Model/View/Update,外加一层「客户端回调 → 单通道 → Subscription」桥接
//! (见 `backend.rs`)。
//!
//! 运行前提:后端 daemon 可用。没有 discovery 记录时会尝试按 qaqh-client 的
//! 候选序自动拉起 `qaqh-daemon`(dev 布局指 `../qaqh-backend/target/debug`)。

mod backend;
mod message;
mod session;
mod settings;
mod state;
mod view;
mod view_settings;

use message::BridgeEvent;

fn main() -> iced::Result {
    // GUI 程序双击启动时没有控制台;`QAQH_ICED_LOG=<path>` 时安装极简文件
    // logger(与 TUI 仓的 QAQH_TUI_LOG 同款取舍),默认不写盘、不改变行为。
    if let Ok(path) = std::env::var("QAQH_ICED_LOG")
        && let Ok(file) = std::fs::File::create(&path)
    {
        let _ = simple_file_logger(file);
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<BridgeEvent>();
    let bridge = backend::Bridge::new(tx);
    state::install_event_rx(rx);

    iced::application(
        {
            let bridge = bridge.clone();
            move || state::App::new(bridge.clone())
        },
        state::App::update,
        state::App::view,
    )
    .title("QAQ-Harness")
    .subscription(state::App::subscription)
    .theme(|_: &state::App| iced::Theme::Dark)
    .window_size((1240.0, 820.0))
    .run()
}

/// 极简文件 logger:时间戳 + 级别 + 消息,单行写入,不带任何依赖。
fn simple_file_logger(file: std::fs::File) -> Result<(), log::SetLoggerError> {
    struct FileLogger(std::sync::Mutex<std::fs::File>);

    impl log::Log for FileLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            use std::io::Write;
            let Ok(mut file) = self.0.lock() else {
                return;
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            let _ = writeln!(file, "{now} {:5} {}", record.level(), record.args());
            let _ = file.flush();
        }

        fn flush(&self) {}
    }

    log::set_boxed_logger(Box::new(FileLogger(std::sync::Mutex::new(file))))?;
    log::set_max_level(log::LevelFilter::Info);
    Ok(())
}
