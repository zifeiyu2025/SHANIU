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
    let stamp = shaniu_lib::clock::now_ms();
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
    // 多采样：每档必须留下 3 个原始指标值，翻案时才能分清是数据抖了还是判据错了。
    // 档位名以声明为准（不写死 "high"：`deepseek-flash` 声明的是 low / high / max）。
    let levels = row.declared_reasoning_levels(&target.base_url);
    let samples = &outcomes["reasoning"].samples;
    println!("[reasoning] samples={samples:?}");
    for level in [levels.first(), levels.last()].into_iter().flatten() {
        let values = samples.get(level).unwrap_or_else(|| panic!("缺少 {level} 档样本"));
        assert_eq!(values.len(), capability::REASONING_SAMPLES, "{level} 档样本数不对: {values:?}");
    }

    // 三连都必须有结论或明确的"无法判定"；`None` 不能再被当成失败——第四态是合法结论。
    for item in ["vision", "tools", "reasoning"] {
        let outcome = &outcomes[item];
        assert!(
            outcome.verified.is_some() || outcome.evidence.contains("数据不足") || outcome.evidence.contains("未验证"),
            "{item} 既没有判定也没说明原因：{}",
            outcome.evidence
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
    // 有确定结论时，`verified` 与 `verifiedLevels` 必须一致；第四态（None）不给任何档位。
    if let Some(verified) = reasoning.verified {
        assert_eq!(verified, !reasoning.verified_levels.is_empty(), "verified 与 verifiedLevels 必须一致");
    } else {
        assert!(reasoning.verified_levels.is_empty(), "第四态不该给出任何已验证档位");
    }

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

/// 第四态对照组：本地靶子让 `reasoning_tokens` 剧烈波动。
///
/// 这是本卡判据的**反例锁**：如果判据退回"只有生效/不生效"，这组数据一定被强行
/// 归到某一边并给出确定结论。正确结论只有一个——「数据不足，无法判定」。
/// 不联网，跑在常规 `cargo test` 里。
#[tokio::test]
async fn wildly_fluctuating_reasoning_tokens_lands_on_insufficient() {
    // 推理请求按交错顺序（低,高,低,高,低,高）返回。低档 [10,100,400]：中位 100、
    // MAD 90（相对 90%）；高档 [350,400,10]：中位 350、MAD 50。任何一档的抖动都
    // 大于两档中位数差的可解释范围，只能判「不知道」。
    // 数值都小于 512，不触发触顶守卫——这里要单独锁"离散度"这条分支。
    let (target, reasoning_requests) = spawn_reasoning_target(vec![10, 350, 100, 400, 400, 10]);
    let row = local_row("flaky-reasoning");
    let outcomes = capability::probe_all(&target, &row).await;
    let reasoning = &outcomes["reasoning"];
    println!("\n== 第四态对照 ==");
    println!("verified={:?} samples={:?}", reasoning.verified, reasoning.samples);
    println!("evidence={}", reasoning.evidence);

    assert_eq!(
        reasoning.verified, None,
        "档内剧烈波动时必须落到第四态，不许硬判成生效/不生效: {}",
        reasoning.evidence
    );
    assert!(
        reasoning.evidence.contains("数据不足"),
        "证据必须写明『数据不足，无法判定』: {}",
        reasoning.evidence
    );
    assert!(
        reasoning.evidence.contains("离散度"),
        "这组数据应该由档内离散度拦下: {}",
        reasoning.evidence
    );
    assert!(reasoning.verified_levels.is_empty(), "第四态不该给出任何已验证档位");
    assert!(reasoning.unjudgeable, "数据采完但判不出来 = 「不可判」终态");
    assert_eq!(reasoning.samples["low"], vec![10, 100, 400], "低档原始样本没留档");
    assert_eq!(reasoning.samples["high"], vec![350, 400, 10], "高档原始样本没留档");
    assert_eq!(reasoning.censored["low"], 0, "这些样本都没触顶，触顶计数应为 0");
    assert_eq!(
        reasoning_requests.load(std::sync::atomic::Ordering::SeqCst),
        REASONING_REQUESTS,
        "推理探针应恰好发 {REASONING_REQUESTS} 次请求（两档 × 每档 3 次）"
    );
}

/// 高比值必须落成**确定结论并写进缓存**——否则"门槛 3"只是嘴上的规则。
///
/// 10.28 量级正是 2026-11-09 概率题那轮的效应/噪声比；这里用稳定的 40 vs 411 复现，
/// 断言它确实被判「生效」、写进缓存文件、且下次直接命中不再打请求。
#[tokio::test]
async fn high_effect_noise_ratio_is_cached_as_effective() {
    let (target, reasoning_requests) = spawn_reasoning_target(vec![40, 411, 40, 411, 40, 411]);
    let row = local_row("high-ratio");
    let root = temp_root("high-ratio");

    let first = capability::probe_and_cache(&root, &target, &row, &capability::Inflight::default()).await;
    let reasoning = &first["reasoning"];
    println!("\n== 高比值 ==");
    println!("verified={:?} evidence={}", reasoning.verified, reasoning.evidence);
    assert_eq!(reasoning.verified, Some(true), "40 → 411 的效应/噪声约 18，必须判生效");
    assert!(!reasoning.unjudgeable, "有确定结论就不是「不可判」");
    assert!(
        reasoning.evidence.contains("效应/噪声"),
        "证据必须给出比值本身，不能只写『生效』: {}",
        reasoning.evidence
    );
    assert_eq!(reasoning_requests.load(std::sync::atomic::Ordering::SeqCst), REASONING_REQUESTS);

    // 写缓存：第二次直接命中，不再发推理请求。
    let cached = ProbeCache::load(&root);
    let stored = cached.get(&target.cache_key()).expect("高比值结论必须落盘");
    assert_eq!(stored["reasoning"].verified, Some(true), "缓存里的结论丢了");
    assert_eq!(stored["reasoning"].samples["high"], vec![411, 411, 411], "原始样本没落盘");
    let second = capability::probe_and_cache(&root, &target, &row, &capability::Inflight::default()).await;
    assert_eq!(second["reasoning"].verified, Some(true));
    assert_eq!(
        reasoning_requests.load(std::sync::atomic::Ordering::SeqCst),
        REASONING_REQUESTS,
        "缓存命中后不应再打推理请求"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 触顶守卫：所有样本都贴到 512 上限时，**不许用截断数据算出生效**。
///
/// 这组数据的比值（600/512 ≈ 1.17 中位差 88）本来不大，但即便把高档拉到明显更高，
/// 只要有一个样本触顶也必须落「数据不足」。这里刻意让两档都触顶，验证：
/// 结论为 None、censored 计数进缓存与证据、终态标记为 true。
#[tokio::test]
async fn censored_samples_are_never_reported_as_effective() {
    let (target, _requests) = spawn_reasoning_target(vec![512, 600, 512, 600, 512, 600]);
    let row = local_row("censored");
    let outcomes = capability::probe_all(&target, &row).await;
    let reasoning = &outcomes["reasoning"];
    println!("\n== 触顶守卫 ==");
    println!("verified={:?} censored={:?} evidence={}", reasoning.verified, reasoning.censored, reasoning.evidence);

    assert_eq!(reasoning.verified, None, "触顶样本不可信，不许下结论: {}", reasoning.evidence);
    assert!(reasoning.verified_levels.is_empty(), "触顶时不该给出任何已验证档位");
    assert!(reasoning.unjudgeable, "触顶是终态，要永久缓存，不再自动重探");
    assert!(reasoning.evidence.contains("触顶"), "证据要点名触顶: {}", reasoning.evidence);
    assert_eq!(reasoning.censored["low"], 3, "低档 3 个样本全触顶");
    assert_eq!(reasoning.censored["high"], 3, "高档 3 个样本全触顶");
}

/// 本地推理靶子：视觉/工具请求回普通回答，推理请求按 `tokens` 顺序回
/// `completion_tokens` 与 `reasoning_tokens`（数值相同，便于断言指标口径）。
fn spawn_reasoning_target(tokens: Vec<u64>) -> (ProbeTarget, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("本地靶子起不来");
    let address = listener.local_addr().unwrap();
    let reasoning_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = reasoning_requests.clone();
    // 靶子线程在 12 次请求后继续等连接（客户端可能重试），不 join——断言靠计数器。
    std::thread::spawn(move || {
        let mut reasoning_seen = 0usize;
        for stream in listener.incoming().take(12) {
            let Ok(mut stream) = stream else { continue };
            let request = read_http_request(&mut stream);
            let body = String::from_utf8_lossy(&request);
            let completion = if body.contains("reasoning_effort") {
                let value = tokens.get(reasoning_seen).copied().unwrap_or(0);
                reasoning_seen += 1;
                counter.store(reasoning_seen, std::sync::atomic::Ordering::SeqCst);
                format!(
                    r#"{{"id":"local","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"ok"}},"finish_reason":"stop"}}],"usage":{{"completion_tokens":{value},"completion_tokens_details":{{"reasoning_tokens":{value}}}}}}}"#
                )
            } else {
                // 视觉/工具请求回一个普通回答即可，本用例不关心它们。
                r#"{"id":"local","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"completion_tokens":1}}"#.to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{completion}",
                completion.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    let target = ProbeTarget {
        base_url: format!("http://{address}/v1"),
        protocol: "openai-chat".into(),
        model: "local-reasoning".into(),
        headers: BTreeMap::new(),
        api_key: "local-test-key".into(),
    };
    (target, reasoning_requests)
}

fn local_row(model: &str) -> ModelProfile {
    ModelProfile {
        id: model.into(),
        reasoning_efforts: BTreeMap::from([
            ("low".to_string(), "low".to_string()),
            ("high".to_string(), "high".to_string()),
        ]),
        ..Default::default()
    }
}

/// 每档样本数 × 2 档。写死在这里是为了让"成本翻倍"这件事在测试里也可断言。
const REASONING_REQUESTS: usize = 6;

/// 读完一个 HTTP/1.1 请求（含按 `content-length` 补齐的请求体）。
/// 视觉探针的请求体带 base64 图片，不能只读第一段。
fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..read]);
        let Some(head_end) = data.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
        let head = String::from_utf8_lossy(&data[..head_end]).to_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if data.len() >= head_end + 4 + length {
            break;
        }
    }
    data
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
