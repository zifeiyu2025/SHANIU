//! MCP stdio 客户端（docs/02 §3 / §3.1，ADR-105/107/108）。
//!
//! 交付顺序（每步独立可提交，SH-060 一张卡内推进）：
//!  1. 协议编解码（[`rpc`]）：JSON-RPC 2.0 over NDJSON，server→client 请求默认拒绝；
//!  2. 进程监管：经 `sandbox::prepare_stdio` 启动（受限令牌 + Job + 句柄白名单）、
//!     env 白名单、超时、崩溃重启一次（重启前重验可执行文件哈希）；
//!  3. `initialize` / `tools/list` / `tools/call` 的请求关联、取消与 stderr 审计事件；
//!  4. 清单转换与事务化注册（`ToolRouter::try_register` + 双端短哈希消歧）。
//!
//! 边界（§3.1.6）：
//! - 本模块**不**调用 `ApprovalBroker::gate`——裁决只在 `agent_loop`；
//! - 本模块**不**直接向模型注入声明——MCP 工具一律转成 `ToolSpec` 注册；
//! - 依赖边只允许 `config` / `events` / `tool_router` / `sandbox`（docs/01 E47）。

pub mod command;
pub mod fingerprint;
pub mod guard;
pub mod manifest;
pub mod module_tree;
pub mod output;
pub mod process;
pub mod registry;
pub mod rpc;
pub mod runtime;
pub mod session;
pub mod trust;
