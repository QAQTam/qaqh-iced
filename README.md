# QAQ-Harness iced 前端

QAQ-Harness 的 **iced 桌面前端**(Ringing v2 协议)。传输层零自建 —— 全部复用
后端仓的 [`qaqh-client`](../qaqh-backend/crates/qaqh-client)(与 TUI / 桌面壳
同一条权威实现):发现/lease、per-seed 单流与 timeline 流、命令面与服务面。

## 功能

- **连接生命周期**:自动发现本地 daemon(读 `daemon.json`,pid 判活),没有
  活 daemon 时按候选序自动拉起 `qaqh-daemon`;连接失败自动重试,epoch 变化
  (daemon 重启)对 UI 可见,失败后可手动重连。
- **会话列表 / 新建 / 切换**:侧栏卡片(标题 · 状态 · 回合数);首次连接时
  自动打开最近会话,空列表自动创建新会话;打开会话 = attach → 激活 timeline
  流 → 认领 driver 席位。
- **转录渲染**:timeline 快照(权威替换)+ 流式事件(增量重放),支持
  markdown 文本块(iced 0.14 markdown 组件,流式增量解析)、思考块、工具卡
  (状态图标 / 标题 / 行数摘要 / 输出尾部窗口)、回合终态;向上翻页加载更早
  历史(排他游标),滚动跟随可被用户上翻打断。
- **输入**:多行编辑器,Enter 发送 / Shift+Enter 换行;运行中可中止回合。
- **挂起交互**(审批):permission(允许/拒绝/信任此目录)、ask(逐题单选 +
  自定义输入)、plan(markdown 预览 + 批准/拒绝/理由)。支持 bootstrap 恢复、
  content-ref 外置正文取回、first-answer-wins 与防幽灵面板记账。
- **状态栏**:连接相位 / epoch、会话活动状态、usage(输入/输出/合计)、
  只读提示、自动消失的错误通知。
- **设置页**(sidecar 入口:侧栏「⚙ 设置」):读写走后端的
  [`qaqh-config-api`](../qaqh-backend/crates/qaqh-config-api) 唯一真相 ——
  `ConfigDto` 读模型 + `ConfigPatch` 写模型(Merge Patch,只发脏字段)。
  覆盖连接(wire/credential/baseUrl/model/apiKey)、生成(maxTokens/
  contextLength/reasoningEffort/autoCompactThreshold)、行为(权限档位 1-4/
  沙箱/合规/空闲卸载/默认 shell/tokenizer)、子代理六字段与 profile 切换。
  保存即热更新(daemon 广播 AgentReloadConfig);tokenizerPath 需重启并在
  UI 标注;密钥永不回显(掩码),留空 = 保持现值;MCP/LSP 段后端暂为只读。

## 构建 / 运行

```bash
cargo run          # debug
cargo build --release
```

前置:兄弟目录存在 `../qaqh-backend`(path 依赖 `qaqh-client`)。仓库内
没有 daemon 二进制时,应用会按 qaqh-client 的候选序寻找
`../qaqh-backend/target/debug/qaqh-daemon` 并自动拉起(也可先手动
`qaqh-daemon run`)。

数据根:`$XDG_CONFIG_HOME/qaqh`(或 `~/.config/qaqh`),与 daemon 共用,
可被 `QAQH_DATA_DIR` 覆盖。

日志:`QAQH_ICED_LOG=<path>` 时安装极简文件 logger(GUI 启动无控制台;
默认不装、不写盘)。

## 架构

```
src/
├── main.rs      入口:iced::application 装配 + env 门控文件 logger
├── message.rs   Message(Elm 消息)+ BridgeEvent(传输层报文)
├── backend.rs   Bridge:qaqh-client ↔ iced 桥(单 mpsc 通道)
├── state.rs     App:update 路由 / 会话账本 / 订阅接线
├── session.rs   单会话投影:timeline reducer + v2 时序闸 + 交互面板
├── settings.rs  设置页状态:权威 ConfigDto + 编辑缓冲 + 脏字段 → ConfigPatch
├── view.rs      主视图渲染(侧栏 / 转录 / 审批卡 / 输入区 / 状态栏)
└── view_settings.rs  设置页渲染(分组卡片 + 字段行)
```

关键取舍(与 TUI 仓同源):

- **协议类型只从 `qaqh-client` 出口取用**,本仓不手抄任何 wire 类型;
  形状漂移会是编译错误而不是静默丢帧。
- **客户端任务全部 spawn 在 qaqh-client 自己的全局 tokio runtime 上**
  (`qaqh_client::runtime_handle()`);iced 侧只经单个 mpsc 通道消费事件
  (`Subscription::run` worker 转发),executor 归属与 tokio 完全解耦。
- **打开会话时序对齐 TUI**:`attach(SessionResume)` →
  `activate_timeline`(拉首页 + timeline 流 + v2 单流)→ 流 `Open` 后
  bootstrap;bootstrap 失败(新会话首个 fact 未落盘的 404/409)小步重试。
- **v2 事件准入闸**沿用 TUI 不变量:epoch 校验、`(fact_seq,
  projection_index)` 字典序单调、replaceable 按 revision、重复/倒退丢弃;
  未决交互按身份记账(终态不得重开,应答过的同 id 请求不再弹面板)。
- timeline 流断开重连由传输层自愈;只有 daemon 明确 404(会话不存在)才
  下线本地状态。
- 设置读写闭环对齐 TUI:`config.load` → 缓冲 → 只发脏字段的
  `ConfigPatch`(`validate()` 客户端预校验)→ `config.save` → 回读权威
  快照;dirty 判定 = 缓冲 ≠ 权威值(而非「碰过没」),`config.save` 与
  `profile.apply` 写后 daemon 广播重载,无需重启。

## 与后端契约

改 wire 类型后,由后端唯一入口导出 SDK 契约(见后端 README),本仓经
path 依赖直接拿到权威类型,无镜像需要同步。
