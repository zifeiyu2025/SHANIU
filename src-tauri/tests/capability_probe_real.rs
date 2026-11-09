//! SH-132 真机收尾（A / C）：能力探针在真实端点上的行为。
//!
//! 为什么必须有"故意让它失败"的一例：只验"能通过"的探针，等于没验探针——
//! 声明写错、端点静默忽略参数，都会以"没报错"的方式混过去。所以本文件既跑
//! 正常三连，也用**纯文本模型声明视觉**造一次不符。
//!
//! 运行（密钥只在启动测试进程的那条命令里注入，AGENTS §4.1）：
//! ```powershell
//! $env:SHANIU_TEST_API_KEY = $env:DeepSeekAPI
//! $env:SHANIU_TEST_BASE_URL = "https://api.deepseek.com/v1"
//! $env:SHANIU_TEST_MODEL = "deepseek-flash"
//! cargo test --test capability_probe_real -- --ignored --nocapture
//! ```
//!
//! 429 用例不联网（本地靶子），随常规 `cargo test` 一起跑。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use shaniu_lib::capability::{self, ProbeCache, ProbeTarget};
use shaniu_lib::config::ModelProfile;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("缺少 {name}"))
}

/// 隔离数据根：只落 `%TEMP%`（AGENTS §3），每个用例一份，避免互相读到缓存。
fn temp_root(tag: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let root = std::env::temp_dir().join(format!("shaniu-cap-{tag}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn target_of(model: &str) -> ProbeTarget {
    ProbeTarget {
        base_url: required("SHANIU_TEST_BASE_URL"),
        protocol: "openai-chat".into(),
        model: model.to_string(),
        headers: BTreeMap::new(),
        api_key: required("SHANIU_TEST_API_KEY"),
        token_budget_hint: 8_192,
    }
}

fn cache_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(shaniu_lib::config::CACHE_DIR).join(capability::CACHE_FILE)
}

/// A：真实端点三连探针。贴每项 `verified` / 耗时 / 证据原文，并验证缓存命中不重探。
#[tokio::test]
#[ignore = "需要外网与测试密钥；手动运行"]
async fn deepseek_triple_probe_with_cache() {
    let model = required("SHANIU_TEST_MODEL");
    // 行内不写任何能力：全部走内置表，顺带验证"内置表 → 探针"这条链路。
    let row = ModelProfile { id: model.clone(), ..Default::default() };
    let target = target_of(&model);
    let root = temp_root("triple");

    let started = std::time::Instant::now();
    let inflight = capability::Inflight::default();
    capability::ensure_probed(root.clone(), target.clone(), row.clone(), &inflight).await;
    assert!(
        !inflight.contains(&target.cache_key()),
        "探针跑完必须清掉「检测中」登记，否则设置页会永远停在检测中"
    );
    println!("\n== 首次实测（含落盘）总耗时 {}ms ==", started.elapsed().as_millis());

    let cache = ProbeCache::load(&root);
    let outcomes = cache
        .get(&target.cache_key())
        .cloned()
        .expect("缓存里必须有本次实测结果");
    for item in ["vision", "tools", "reasoning"] {
        let outcome = outcomes.get(item).unwrap_or_else(|| panic!("缺少 {item} 探针结果"));
        println!(
            "[{item}] verified={:?} 耗时={:?}ms evidence={}",
            outcome.verified.unwrap_or(false),
            outcome.elapsed_ms,
            outcome.evidence
        );
    }
    println!(
        "[reasoning] verifiedLevels={:?}",
        outcomes.get("reasoning").map(|o| o.verified_levels.clone()).unwrap_or_default()
    );

    // 三连都必须给出判定（`None` 只允许出现在网络/限流失败），否则"实测"名不副实。
    for item in ["vision", "tools", "reasoning"] {
        assert!(
            outcomes[item].verified.is_some(),
            "{item} 没有给出判定：{}",
            outcomes[item].evidence
        );
        assert!(outcomes[item].elapsed_ms.is_some(), "{item} 没有记录耗时");
    }
    // 推理证据里必须是两个真实数值，不是"通过"两个字。
    let reasoning = &outcomes["reasoning"];
    assert!(
        reasoning.evidence.chars().any(|c| c.is_ascii_digit()),
        "推理证据里没有实际 reasoning_tokens 数值: {}",
        reasoning.evidence
    );
    assert_eq!(
        reasoning.verified,
        Some(!reasoning.verified_levels.is_empty()),
        "verified 与 verifiedLevels 必须一致"
    );

    // 缓存命中：第二次 ensure_probed 不得重写缓存（checked_at_ms 会变），也不得再打请求。
    let before = std::fs::read(cache_path(&root)).expect("首次实测必须落盘缓存");
    let second = std::time::Instant::now();
    capability::ensure_probed(
        root.clone(),
        target.clone(),
        row.clone(),
        &capability::Inflight::default(),
    )
    .await;
    println!(
        "== 第二次 ensure_probed 耗时 {}ms（缓存命中，未重探）==",
        second.elapsed().as_millis()
    );
    let after = std::fs::read(cache_path(&root)).expect("缓存文件必须仍在");
    assert_eq!(before, after, "第二次调用重写了缓存：缓存没有生效");
    let checked: Vec<u64> = outcomes.values().map(|o| o.checked_at_ms).collect();
    println!("== 缓存 checked_at_ms = {checked:?} ==");
}

/// C：故意造"不符"——`deepseek-v4-pro` 端点实测只接受 text，这里声明它有视觉。
///
/// 判据不是"探针能跑通"，而是：`verified=false` 被写进缓存、视图 `state=mismatch`、
/// 日志出现「能力实测与声明不符」，**且声明值不被静默改写**。
#[tokio::test]
#[ignore = "需要外网与测试密钥；手动运行"]
async fn declared_vision_on_text_model_reports_mismatch() {
    let model = std::env::var("SHANIU_TEST_MISMATCH_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-pro".into());
    let base_url = required("SHANIU_TEST_BASE_URL");
    let row = ModelProfile {
        id: model.clone(),
        input: vec!["text".into(), "image".into()],
        ..Default::default()
    };
    let target = target_of(&model);
    let root = temp_root("mismatch");

    let log = Capture::default();
    let _ = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .try_init();

    capability::ensure_probed(
        root.clone(),
        target.clone(),
        row.clone(),
        &capability::Inflight::default(),
    )
    .await;

    let cache = ProbeCache::load(&root);
    let outcomes = cache.get(&target.cache_key()).expect("缓存里必须有本次实测结果");
    let vision = outcomes.get("vision").expect("视觉探针必须跑过");
    println!("\n== C 不符证据 ==");
    println!("vision.verified={:?} elapsed={:?}ms", vision.verified, vision.elapsed_ms);
    println!("vision.evidence={}", vision.evidence);
    assert_eq!(
        vision.verified,
        Some(false),
        "纯文本模型声明视觉，必须判『与声明不符』: {}",
        vision.evidence
    );
    // 两次问答都要有原文，否则"不符"没有可复核的根据。
    assert!(vision.evidence.contains('红') && vision.evidence.contains('绿'), "证据里缺少两问结果");

    // 设置页拿到的是 model_view，这里直接断言同一份视图。
    let view = capability::model_view(&row, &base_url, Some(outcomes), false);
    println!("设置页视图: state={} source={} value={}", view["vision"]["state"], view["vision"]["source"], view["vision"]["value"]);
    assert_eq!(view["vision"]["state"], "mismatch", "设置页必须显示『与声明不符』");
    assert_eq!(view["vision"]["source"], "manual", "来源仍应是声明它的那一层");
    assert_eq!(view["vision"]["value"], true, "探针不得改写声明值");

    // 声明层不被探针覆盖：清掉本地视图引用后，行里的 input 仍是 text+image。
    assert_eq!(row.input, vec!["text".to_string(), "image".to_string()]);

    let logs = log.text();
    println!("捕获日志:\n{logs}");
    assert!(logs.contains("能力实测与声明不符"), "日志没有告警，属于静默失败");
    assert!(logs.contains("capability=vision"), "日志没有点名是哪一项");

    // 声明文件里也应能读到实测记录（不是只在内存里）。
    let raw = std::fs::read_to_string(cache_path(&root)).expect("缓存必须落盘");
    assert!(raw.contains("\"verified\": false"), "缓存里没有『不符』的实测记录");
}

/// 429 只能记「未验证」，绝不能记「不生效」——两者结论的后果完全不同。
///
/// 不联网：本地靶子固定回 429，跑在常规 `cargo test` 里。
#[tokio::test]
async fn rate_limit_is_unverified_not_unsupported() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("本地靶子起不来");
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        }
    });
    let target = ProbeTarget {
        base_url: format!("http://{address}/v1"),
        protocol: "openai-chat".into(),
        model: "rate-limited".into(),
        headers: BTreeMap::new(),
        api_key: "local-test-key".into(),
        token_budget_hint: 0,
    };
    let row = ModelProfile {
        id: "rate-limited".into(),
        input: vec!["text".into(), "image".into()],
        ..Default::default()
    };
    let outcomes = capability::probe_all(&target, &row).await;
    for item in ["vision", "tools"] {
        let outcome = &outcomes[item];
        assert_eq!(outcome.verified, None, "{item}: 429 必须记未验证");
        assert!(outcome.evidence.contains("429"), "{item}: 证据要点明限流: {}", outcome.evidence);
        assert!(outcome.evidence.contains("未验证"), "{item}: 文案必须写明未验证: {}", outcome.evidence);
    }
}

/// 写日志到内存，供 C 用例断言"日志也报了"。
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
