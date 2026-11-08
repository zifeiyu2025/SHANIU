//! 真机测试：真实访问公开搜索页。
//!
//! 默认 `#[ignore]`，因为依赖外网与搜索引擎的当前行为（可能被限流）。
//! 运行方式：
//! `cargo test --test web_real -- --ignored --nocapture`
//!
//! 目前唯一内置 provider 是 Bing。这里验证真实网络下的条数与耗时，并打印来源。
//! 解析细节由 `tools::web::search` 的单元测试用固定夹具覆盖。

use std::sync::Arc;
use std::time::Instant;

use shaniu_lib::tools::web::{Bing, FallbackProvider, SearchProvider, WebClient, WebFetch};
use tokio_util::sync::CancellationToken;

/// 集成测试在 crate 外部，拿不到 `#[cfg(test)]` 的 `NullEventSink`，这里补一个丢弃型出口。
struct NoopSink;

impl shaniu_lib::events::EventSink for NoopSink {
    fn emit(&self, _msg: &shaniu_lib::events::EventMsg) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "需要外网；手动运行"]
async fn bing_real_queries() {
    let client = Arc::new(WebClient::new());
    // 走 FallbackProvider 保持与生产一致的调用形态（目前链上只有一家）。
    let provider = FallbackProvider::new(vec![Arc::new(Bing::new(client))]);
    let queries = [
        ("通用", "rust programming language"),
        ("技术文档", "tokio select! macro documentation"),
        ("中文", "杭州 西湖 旅游 攻略"),
    ];

    let mut succeeded = 0;
    for (kind, query) in queries {
        let started = Instant::now();
        match provider.search(query, 5, &CancellationToken::new()).await {
            Ok(hits) => {
                succeeded += 1;
                println!(
                    "[{kind}] query={query:?} 条数={} 耗时={}ms 来源={:?}",
                    hits.len(),
                    started.elapsed().as_millis(),
                    hits.first().map(|hit| hit.source.as_str()).unwrap_or("-")
                );
                for (index, hit) in hits.iter().enumerate().take(3) {
                    println!("  {}. {} — {}", index + 1, hit.title, hit.url);
                }
            }
            Err(error) => {
                println!(
                    "[{kind}] query={query:?} 失败 kind={:?} 耗时={}ms 详情={}",
                    error.kind,
                    started.elapsed().as_millis(),
                    error.detail
                );
            }
        }
    }

    assert!(
        succeeded > 0,
        "三个查询全部失败：说明当前网络、搜索页结构或限流状态有问题，请按上面的 kind 判断"
    );
}

/// 正常公网抓取：会真的出网，因此同样 `#[ignore]`。
/// 私网/重定向/上限等安全路径由 `tools::web::fetch` 的本地靶子测试覆盖。
#[tokio::test]
#[ignore = "需要外网；手动运行"]
async fn web_fetch_real_page() {
    use shaniu_lib::config::SandboxMode;
    use shaniu_lib::tool_router::{ToolContext, ToolExecutor};

    let tool = WebFetch::new(Arc::new(WebClient::new()));
    let sink: Arc<dyn shaniu_lib::events::EventSink> = Arc::new(NoopSink);
    let context = ToolContext::new(
        std::env::temp_dir(),
        CancellationToken::new(),
        "call-web-fetch-real",
        sink,
        SandboxMode::ReadOnly,
    );
    let started = Instant::now();
    let output = tool
        .execute(
            serde_json::json!({ "url": "https://example.com/", "max_chars": 2000 }),
            &context,
        )
        .await
        .expect("公网抓取失败");
    println!(
        "web_fetch https://example.com/ 耗时={}ms success={} 输出前 200 字：\n{}",
        started.elapsed().as_millis(),
        output.success,
        output.output.chars().take(200).collect::<String>()
    );
    assert!(output.success);
    assert!(output.output.contains("<<<WEB_FETCH:"));
    assert!(output.output.contains("status=200"));
    assert!(output.output.contains("Example Domain"));
}
