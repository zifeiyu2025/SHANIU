//! ToolSpec：工具的静态契约（名称/描述/JSON Schema 参数）。
//!
//! `ToolSpec` 是共享契约（docs §2.2 shared kernel）：tool_router 与 model_client
//! 都消费它，方向永远是 tool_router → model_client（单向）。

use serde::Serialize;

use crate::model_client::ToolDeclaration;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema 对象（参数校验由各 executor 自行完成）。
    pub parameters: serde_json::Value,
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
        }
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
}
