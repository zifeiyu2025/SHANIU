//! ToolSpec：工具的静态契约（名称/描述/JSON Schema 参数 + 可选风险档位）。
//!
//! `ToolSpec` 是共享契约（docs §2.2 shared kernel）：tool_router 与 model_client
//! 都消费它，方向永远是 tool_router → model_client（单向）。
//!
//! `risk` 是 SH-063 的 additive 扩展（docs/02 ADR-107）：只有**用户批准过的 MCP 工具**
//! 会带上它，取值来自信任记录，绝不来自 server 自报的 `readOnlyHint`/`destructiveHint`。
//! 内置工具保持 `None`，风险仍由 `permission::risk_of` 按名称推导。

use serde::Serialize;

use crate::events::RiskLevel;
use crate::model_client::ToolDeclaration;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 对象（参数校验由各 executor 自行完成）。
    pub parameters: serde_json::Value,
    /// 用户为该工具指定的风险档位；`None` = 走 `permission::risk_of`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
}

impl ToolSpec {
    pub fn new(
        name: &str,
        description: &str,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
            risk: None,
        }
    }

    /// 附加风险档位（MCP 注册路径使用；additive，不影响既有构造方）。
    pub fn with_risk(mut self, risk: Option<RiskLevel>) -> Self {
        self.risk = risk;
        self
    }

    /// 桥接为 model_client 的工具声明（下发进模型请求 tools 数组）。
    pub fn to_declaration(&self) -> ToolDeclaration {
        ToolDeclaration {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_declaration_copies_contract() {
        let spec = ToolSpec::new(
            "read_file",
            "读文件",
            serde_json::json!({"type":"object","required":["path"]}),
        );
        let decl = spec.to_declaration();
        assert_eq!(decl.name, "read_file");
        assert_eq!(decl.description, "读文件");
        assert_eq!(decl.parameters["required"][0], "path");
    }

    #[test]
    fn risk_is_additive_and_absent_from_model_declaration() {
        let spec = ToolSpec::new("mcp__srv__x", "x", serde_json::json!({"type":"object"}))
            .with_risk(Some(RiskLevel::Read));
        assert_eq!(spec.risk, Some(RiskLevel::Read));
        // 风险档位不能进入模型可见的声明（只影响审批门禁）。
        let json = serde_json::to_value(spec.to_declaration()).unwrap();
        assert!(json.get("risk").is_none());
        // 内置工具不带 risk 时序列化不出现该字段（additive 兼容）。
        let builtin = serde_json::to_value(ToolSpec::new("read_file", "x", serde_json::json!({})))
            .unwrap();
        assert!(builtin.get("risk").is_none());
    }
}
