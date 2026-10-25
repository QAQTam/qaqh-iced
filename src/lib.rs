//! QAQ-Harness iced 桌面前端(库形态)。
//!
//! 拆出 lib 是为了集成测试(`tests/`)能直接构造 `SessionState` 做长上下文
//! 压力验证 —— 二进制 crate 的内部模块对外部测试不可见。`main.rs` 只保留
//! iced 装配与 logger,全部逻辑都在这里。

pub mod backend;
pub mod message;
pub mod session;
pub mod settings;
pub mod state;
pub mod view;
pub mod view_settings;
