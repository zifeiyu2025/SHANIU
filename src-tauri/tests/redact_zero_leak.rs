//! SH-042 密钥零泄漏（docs/01 §风险登记册 R2 的健身函数）：
//! 密钥一旦被登记（DPAPI 读取/保存时自动登记），任何落盘路径——日志、rollout——
//! 都不得出现原文。这条测试是"为什么脱敏要挂在收口点而不是逐处调用"的直接证据。

use std::io::Write as _;
use std::path::Path;

use shaniu_lib::config;
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::rollout::{ConfigSnapshot, RolloutItem};

/// 临时目录：按测试名隔离，测完自己清（panic 也清）。
fn temp_dir(name: &str) -> shaniu_lib::test_support::TestTempDir {
    shaniu_lib::test_support::TestTempDir::new(&format!("redact-{name}"))
}

fn read_dir_text(dir: &Path) -> String {
    let mut out = String::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with(config::LOG_FILE_PREFIX) {
            out.push_str(&std::fs::read_to_string(entry.path()).unwrap());
        }
    }
    out
}

#[test]
fn registered_key_never_survives_redact_in_any_context() {
    let key = "sk-registered-0123456789abcdefXYZ";
    config::register_secret(key);

    // 各种可能把它带出去的形态：JSON 字符串、HTTP 头、裸文本、提示词拼接
    let contexts = [
        format!(r#"{{"role":"user","text":"我的密钥是 {key} 帮我检查"}}"#),
        format!("Authorization: Bearer {key}"),
        format!("任务：用 {key} 调一下接口"),
        key.to_string(),
    ];
    for ctx in contexts {
        let redacted = config::redact(&ctx);
        assert!(!redacted.contains(key), "泄漏形态：{redacted}");
        assert!(redacted.contains(config::REDACTED), "应有脱敏标记：{redacted}");
    }
}

#[test]
fn unfamiliar_key_shapes_are_redacted_without_registration() {
    // 没登记过的陌生 key：模式层兜底
    let cases = [
        "sk-unknown-key-1234567890",
        "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.e30.abc",
        r#"{"api_key": "some-secret-value-123"}"#,
        "export API_KEY=another-secret-9876",
    ];
    for case in cases {
        let redacted = config::redact(case);
        assert_ne!(redacted, case, "形似密钥未被擦除：{case}");
        assert!(!redacted.contains("some-secret-value-123"));
        assert!(!redacted.contains("another-secret-9876"));
        assert!(!redacted.contains("eyJhbGciOiJIUzI1NiJ9.e30.abc"));
    }
}

#[test]
fn non_secret_text_passes_through_untouched() {
    config::register_secret("sk-registered-0123456789abcdefXYZ");
    let plain = "git status --short\n M src/main.rs\nwarning: LF will be replaced by CRLF";
    assert_eq!(config::redact(plain), plain);
}

#[test]
fn redacting_writer_survives_key_split_across_writes() {
    let key = "sk-split-across-chunks-0123456789";
    config::register_secret(key);
    let mut out: Vec<u8> = Vec::new();
    {
        let mut w = config::RedactingWriter::new(&mut out);
        // tracing 的真实行为：一条事件分多次 write（时间戳/级别/字段各一段），
        // 密钥可能恰好被劈开——这就是为什么必须按行攒缓冲，不能按 write 调用逐段过虑
        w.write_all(b"INFO shaniu: task=\"call with ").unwrap();
        w.write_all(key.as_bytes()).unwrap();
        w.write_all(b" please\"\nsecond line clean\n").unwrap();
        w.flush().unwrap();
    }
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains(key), "劈开的密钥被拼回去泄漏了：{text}");
    assert!(text.contains(config::REDACTED));
    assert!(text.contains("second line clean"));
}

#[test]
fn rollout_lines_never_contain_registered_key() {
    let key = "sk-rollout-leak-probe-0123456789";
    config::register_secret(key);
    let dir = temp_dir("rollout");

    let meta = RolloutItem::SessionMeta {
        session_id: "sess-redact".into(),
        agent_id: "agent-redact".into(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot {
            model: "m".into(),
            base_url: "https://x/v1".into(),
            ..Default::default()
        },
    };
    let writer = shaniu_lib::rollout::RolloutWriter::create(&dir, &meta).unwrap();
    writer
        .append(&RolloutItem::ResponseItem {
            item: HistoryItem::Message {
                role: Role::User,
                text: format!("帮我用 {key} 跑一下"),
                reasoning: None,
            },
            at_ms: 0,
        })
        .unwrap();
    writer
        .append(&RolloutItem::ResponseItem {
            item: HistoryItem::Message {
                role: Role::Assistant,
                text: format!("好的，我会用 {key}（不会写进任何日志）"),
                reasoning: None,
            },
            at_ms: 0,
        })
        .unwrap();
    drop(writer);

    let content = std::fs::read_to_string(dir.join("agent-redact.jsonl")).unwrap();
    assert!(!content.contains(key), "rollout 泄漏密钥：{content}");
    assert!(content.contains(config::REDACTED), "应有脱敏标记：{content}");
}

#[test]
fn logging_pipeline_never_writes_key_to_disk() {
    let key = "sk-log-leak-probe-0123456789abcdef";
    config::register_secret(key);
    let data_root = temp_dir("logging");

    let (subscriber, guard) = config::build_logging(&data_root).unwrap();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!("任务里粘了密钥 {}", key);
        tracing::warn!("Authorization: Bearer some-fresh-token-123456");
    });
    drop(guard); // non_blocking 后台线程随 guard 冲刷并关闭

    let content = read_dir_text(&data_root.join("logs"));
    assert!(!content.is_empty(), "日志应已落盘");
    assert!(!content.contains(key), "日志泄漏密钥：{content}");
    assert!(!content.contains("some-fresh-token-123456"), "未登记令牌泄漏：{content}");
    assert!(content.contains(config::REDACTED), "应有脱敏标记：{content}");
    // docs §9.3：数据目录解析结果写日志第一行这类启动诊断能正常产出
    assert!(content.contains("INFO") || content.contains("WARN"));
}

#[test]
fn expired_log_files_are_pruned_by_mtime() {
    let dir = temp_dir("prune");
    let old = dir.join("shaniu.log.2026-10-01");
    let fresh = dir.join("shaniu.log.2026-11-05");
    std::fs::write(&old, "old").unwrap();
    std::fs::write(&fresh, "fresh").unwrap();

    // 把旧文件 mtime 拨到 20 天前（FileTimes 稳定 API，无需日期库）
    use std::fs::FileTimes;
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(20 * 24 * 3600);
    let f = std::fs::File::options().write(true).open(&old).unwrap();
    f.set_times(FileTimes::new().set_modified(stale)).unwrap();
    drop(f);

    config::prune_expired_logs(&dir, config::LOG_RETENTION_DAYS);
    assert!(!old.exists(), "超期日志应被清理");
    assert!(fresh.exists(), "保留期内日志不能误删");
}
