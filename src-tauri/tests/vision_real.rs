//! 真机验证：`view_image` → 实际模型请求（截图 → 模型描述）。
//!
//! 运行前先把一张桌面截图放进工作区，并注入密钥：
//! ```powershell
//! $env:VISION_REAL_WS = "C:\...\shaniu-vision-ws"; $env:VISION_REAL_PNG = "screen.png"
//! $env:SHANIU_TEST_API_KEY=...; $env:SHANIU_TEST_BASE_URL=...; $env:SHANIU_TEST_MODEL=...
//! cargo test --test vision_real -- --ignored --nocapture
//! ```
//!
//! 判据：模型要说出截图里的**具体内容**（窗口、文字、图标），而不是「这是一张图片」。

use std::sync::Arc;

use futures::StreamExt;
use shaniu_lib::config::SandboxMode;
use shaniu_lib::model_client::{stream_model, HistoryItem, ModelStreamEvent, ProviderConfig, Role};
use shaniu_lib::tool_router::{attachment, ToolContext, ToolExecutor};
use shaniu_lib::tools::view_image::ViewImage;
use tokio_util::sync::CancellationToken;

struct NoopSink;

impl shaniu_lib::events::EventSink for NoopSink {
    fn emit(&self, _msg: &shaniu_lib::events::EventMsg) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
#[ignore = "需要桌面截图、外网与测试密钥；手动运行"]
async fn view_image_reaches_the_model() {
    let workspace = std::env::var("VISION_REAL_WS").expect("缺少 VISION_REAL_WS");
    let file = std::env::var("VISION_REAL_PNG").unwrap_or_else(|_| "screen.png".into());
    let base_url = std::env::var("SHANIU_TEST_BASE_URL").expect("缺少 SHANIU_TEST_BASE_URL");
    let api_key = std::env::var("SHANIU_TEST_API_KEY").expect("缺少 SHANIU_TEST_API_KEY");
    let model = std::env::var("SHANIU_TEST_MODEL").expect("缺少 SHANIU_TEST_MODEL");

    let tool = ViewImage::new();
    let context = ToolContext::new(
        &workspace,
        CancellationToken::new(),
        "call-view-image-real",
        Arc::new(NoopSink),
        SandboxMode::ReadOnly,
    );
    let started = std::time::Instant::now();
    let output = tool
        .execute(serde_json::json!({ "path": file }), &context)
        .await
        .expect("view_image 执行失败");
    println!("== 工具输出 ==\n{}\n", output.output);
    let image = attachment(&output.output).expect("工具输出里必须带图片引用");
    println!(
        "== 引用 == path={} {}×{} tokens={} mime={} 耗时={}ms",
        image.path,
        image.width,
        image.height,
        image.tokens,
        image.mime,
        started.elapsed().as_millis()
    );

    let provider = ProviderConfig {
        provider: "openai-chat".into(),
        base_url,
        model,
        reasoning_effort: None,
        headers: Default::default(),
    };
    let history = vec![
        HistoryItem::Message {
            role: Role::User,
            text: "这张截图里有什么？请说出具体的窗口标题、可见文字或图标，不要只说“这是一张图片”。"
                .into(),
            reasoning: None,
        },
        HistoryItem::ImageMessage {
            role: Role::User,
            text: format!("[view_image] {}", image.path),
            images: vec![image],
        },
    ];
    let request_started = std::time::Instant::now();
    let mut stream = stream_model(&provider, &api_key, &history, &[], "你是图像描述助手。")
        .await
        .expect("请求未建立");
    let mut answer = String::new();
    let mut reasoning = String::new();
    let mut usage = String::new();
    while let Some(event) = stream.next().await {
        match event {
            ModelStreamEvent::OutputTextDelta { delta } => answer.push_str(&delta),
            ModelStreamEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            ModelStreamEvent::Completed { usage: used } => {
                usage = format!(
                    "input={} cached={} output={}",
                    used.input_tokens,
                    used.cached_input_tokens.unwrap_or(0),
                    used.output_tokens
                );
            }
            ModelStreamEvent::Failed { message, .. } => panic!("端点报错: {message}"),
            _ => {}
        }
    }
    println!("== usage == {usage}（请求耗时 {}ms）", request_started.elapsed().as_millis());
    println!("== 推理片段 ==\n{}\n", reasoning.chars().take(600).collect::<String>());
    println!("== 回答 ==\n{answer}\n");

    let trimmed = answer.trim();
    assert!(!trimmed.is_empty(), "模型没有给出任何正文");
    assert!(
        !trimmed.contains("我看不到") && !trimmed.contains("无法查看图片"),
        "模型声称看不到图片，说明图片没有真正送达: {trimmed}"
    );
}
