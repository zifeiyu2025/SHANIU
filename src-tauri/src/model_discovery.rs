//! 端点模型发现（SH-098，docs/04 §1.3 `discover_models`）。
//!
//! 宿主带着**用户自己填的端点**与密钥访问 `GET {base_url}/models`，供设置页
//! "拉取模型"。三条安全边界：
//!
//! 1. **只连用户填的端点**：URL 完全来自表单/文档，不接受任何间接来源；协议限定
//!    http/https，不解析相对路径。
//! 2. **密钥不落日志、不进错误**：任何错误消息只含 URL 与状态码；探针密钥
//!    （表单里刚输入还没存的）用完即弃，不落任何存储。
//! 3. **响应封顶**：4 MiB 上限，超大响应按失败处理——模型列表没有正当理由这么大。
//!
//! 解析兼容 OpenAI 系（`data` 数组）与 `models` 数组两种形状；容量字段按上游
//! discovery.ts 的多拼写口径取（`context_window`/`context_length`、
//! `max_output_tokens`/`max_completion_tokens`）。

use std::collections::BTreeMap;

use serde::Serialize;
use tauri::Manager;

const DISCOVERY_TIMEOUT_SECS: u64 = 15;
const MAX_DISCOVERY_BYTES: usize = 4 * 1024 * 1024;

/// 一行发现结果（camelCase 出到前端，与 `get_settings` 同风格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub input_modalities: Vec<String>,
    /// 端点声明的工具调用能力（SH-132）。`None` = 端点没给，按未知处理，
    /// 交给实测探针——不要拿模型名去猜。
    pub tools: Option<bool>,
    /// 端点是否原生支持托管搜索（SH-132）。同样是 `None` = 不知道。
    pub builtin_web_search: Option<bool>,
}

/// 拉取模型列表的核心实现。`api_key` 调用方已保证非空；错误消息永不携带密钥。
pub async fn discover_endpoint_models(
    base_url: &str,
    headers: &BTreeMap<String, String>,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, String> {
    let base = base_url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(format!("端点地址必须以 http(s):// 开头：{base}"));
    }
    let (url, request) = crate::model_client::models_probe_request(
        base,
        headers,
        api_key,
        std::time::Duration::from_secs(DISCOVERY_TIMEOUT_SECS),
    )
    .map_err(|e| format!("HTTP 客户端不可用: {e}"))?;
    let response = request
        .send()
        .await
        .map_err(|e| format!("无法访问 {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let mut message = format!("{url} 返回 {status}");
        if status.as_u16() == 401 || status.as_u16() == 403 {
            message.push_str("（请检查 API Key 是否正确）");
        }
        return Err(message);
    }
    let body = response.bytes().await.map_err(|e| format!("读取 {url} 响应失败: {e}"))?;
    if body.len() > MAX_DISCOVERY_BYTES {
        return Err(format!("{url} 的响应超过 4 MiB 上限，不像是模型列表"));
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| format!("{url} 的响应不是 JSON"))?;
    let items = parsed
        .get("data")
        .and_then(serde_json::Value::as_array)
        .or_else(|| parsed.get("models").and_then(serde_json::Value::as_array))
        .ok_or_else(|| format!("{url} 的响应里没有模型清单（data/models 字段缺失），请手工填写模型行"))?;

    let mut out: Vec<DiscoveredModel> = Vec::with_capacity(items.len());
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        let Some(id) = item.get("id").and_then(serde_json::Value::as_str).map(str::to_string) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let name = item
            .get("name")
            .or_else(|| item.get("display_name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|n| !n.is_empty());
        let context_window = first_number(item, &["context_window", "context_length"]);
        let max_tokens = first_number(item, &["max_output_tokens", "max_completion_tokens"]);
        // 能力线索与能力模块共用同一套防御性解析（两种字段形状、类型不符按缺失）。
        let meta = crate::capability::parse_metadata(item);
        let input_modalities = meta
            .input_modalities
            .unwrap_or_else(|| vec!["text".to_string()])
            // 只保留我们真支持的模态：端点声明的 `video` 不能变成能力。
            .into_iter()
            .filter(|m| matches!(m.as_str(), "text" | "image"))
            .collect::<Vec<_>>();
        let input_modalities = if input_modalities.is_empty() { vec!["text".to_string()] } else { input_modalities };
        out.push(DiscoveredModel {
            id,
            name,
            context_window,
            max_tokens,
            input_modalities,
            tools: meta.supports_tools,
            builtin_web_search: meta.builtin_web_search,
        });
    }
    if out.is_empty() {
        return Err(format!("{url} 的响应里没有可用模型行（缺 id 字段），请手工填写"));
    }
    tracing::info!(count = out.len(), "端点模型发现成功");
    Ok(out)
}

/// 按拼写顺序取第一个非负整数字段。
fn first_number(item: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(n) = item.get(*key).and_then(serde_json::Value::as_u64) {
            return Some(n);
        }
    }
    None
}

/// Tauri 命令壳：密钥优先级 = 表单探针（未存的）> DPAPI 存储的该服务商密钥。
#[tauri::command]
pub async fn discover_models(
    app: tauri::AppHandle,
    provider: String,
    base_url: String,
    api_key: Option<String>,
) -> Result<Vec<DiscoveredModel>, String> {
    let state = app.state::<crate::app_state::AppState>();
    let cfg = state.config();
    let mut headers = BTreeMap::new();
    let mut stored_key: Option<String> = None;
    let provider_id = provider.trim();
    if !provider_id.is_empty() {
        if let Some(row) = cfg.providers.iter().find(|p| p.id == provider_id) {
            headers = row.headers.clone();
            stored_key = crate::config::SecretStore::new(&state.config_root())
                .load_key(&row.secret_name())
                .map_err(|e| format!("读取密钥失败: {e}"))?;
        }
    }
    // 探针密钥（表单里刚输入还没存的）优先于存储密钥：用户改了 Key 还没点保存
    // 也能拉取——上游 discovery 的同一口径。
    let key = match api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(probe) => probe.to_string(),
        None => stored_key
            .ok_or_else(|| "拉取模型需要密钥：该服务商没有已存的 API Key，表单里也没有输入".to_string())?,
    };
    if base_url.trim().is_empty() {
        return Err("端点地址为空：拉取模型需要 baseURL".into());
    }
    discover_endpoint_models(&base_url, &headers, &key).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 单请求桩：捕获请求头，按给定状态与正文应答。
    struct Stub {
        url: String,
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Stub {
        fn start(status_line: &str, body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let status = status_line.to_string();
            let body = body.to_string();
            let captured: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
            let sink = captured.clone();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // 先查再读：请求头齐了就应答，绝不在客户端等响应时再阻塞读
                // （第二轮 read 会永远阻塞——客户端在等我们回话）
                loop {
                    if String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                        break;
                    }
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&buf).to_string();
                // 先登记再应答：客户端一读到响应就可能立刻断言请求头，此时桩线程
                // 若还没写入 captured，`stub.header(..)` 就是 None。CI 上核少、桩线程
                // 排不上，这条竞态稳定复现（本地多核则常赢）——真因在写入顺序，不是环境。
                *sink.lock().unwrap() = request
                    .lines()
                    .filter_map(|line| line.split_once(": ").map(|(n, v)| format!("{n}: {v}")))
                    .collect();
                let response = format!("{status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes());
            });
            Self { url: format!("http://{addr}/v1"), captured, handle: Some(handle) }
        }

        fn header(&self, name: &str) -> Option<String> {
            let _ = self.handle; // 请求应答完成由 use-case 的 await 顺序保证
            self.captured
                .lock()
                .unwrap()
                .iter()
                .find(|h| h.to_lowercase().starts_with(&format!("{}:", name.to_lowercase())))
                .cloned()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[tokio::test]
    async fn parses_openai_data_array_and_carries_auth_and_custom_headers() {
        let stub = Stub::start(
            "HTTP/1.1 200 OK",
            r#"{"data":[{"id":"m2","context_length":32000},{"id":"m1","context_window":128000,"max_output_tokens":4096,"input_modalities":["text","image"]},{"id":"m1"}]}"#,
        );
        let headers = [("X-Gateway-Sign".to_string(), "sig".to_string())].into_iter().collect();
        let models = discover_endpoint_models(&stub.url, &headers, "stub-key-123").await.unwrap();
        // 按 id 去重保序；容量字段多拼写兼容
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "m2");
        assert_eq!(models[0].context_window, Some(32000));
        assert_eq!(models[1].id, "m1");
        assert_eq!(models[1].context_window, Some(128000));
        assert_eq!(models[1].max_tokens, Some(4096));
        assert_eq!(models[1].input_modalities, vec!["text".to_string(), "image".to_string()]);
        // 认证头与部署头都在请求上
        let auth = stub.header("authorization").unwrap();
        assert_eq!(auth, "authorization: Bearer stub-key-123");
        assert_eq!(stub.header("x-gateway-sign").unwrap(), "x-gateway-sign: sig");
    }

    #[tokio::test]
    async fn auth_failure_reports_status_without_leaking_key() {
        let stub = Stub::start("HTTP/1.1 401 Unauthorized", r#"{"error":"bad key ak_SUPERSECRET"}"#);
        let err = discover_endpoint_models(&stub.url, &Default::default(), "ak_SUPERSECRET").await.unwrap_err();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("API Key"), "{err}");
        assert!(!err.contains("ak_SUPERSECRET"), "错误消息绝不能携带密钥：{err}");
    }

    #[tokio::test]
    async fn non_json_and_missing_listing_are_errors_with_hand_fill_hint() {
        let bad = Stub::start("HTTP/1.1 200 OK", "<html>not json</html>");
        let err = discover_endpoint_models(&bad.url, &Default::default(), "k").await.unwrap_err();
        assert!(err.contains("不是 JSON"), "{err}");

        let empty = Stub::start("HTTP/1.1 200 OK", r#"{"object":"list","data":[]}"#);
        let err = discover_endpoint_models(&empty.url, &Default::default(), "k").await.unwrap_err();
        assert!(err.contains("手工填写"), "{err}");
    }

    #[test]
    fn rejects_non_http_and_blank_base_urls() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let err = discover_endpoint_models("ftp://x/v1", &Default::default(), "k").await.unwrap_err();
            assert!(err.contains("http(s)://"), "{err}");
            let err = discover_endpoint_models("  ", &Default::default(), "k").await.unwrap_err();
            assert!(err.contains("http(s)://"), "{err}");
        });
    }
}
