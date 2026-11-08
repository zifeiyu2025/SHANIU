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

use shaniu_lib::tools::web::{Bing, FallbackProvider, SearchProvider, WebClient};
use tokio_util::sync::CancellationToken;

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
