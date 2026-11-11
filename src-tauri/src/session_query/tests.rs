//! SH-113 行为与契约测试：测命中正确性、去重、hasMore 阈值、snippet、只读边界、
//! schema 归属，不写字面量快照。命令面按 vendored wire 收敛为 `query-only`，
//! 故这里不写任何"游标失效"这类无人调用的契约测试。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

use crate::config;
use crate::model_client::{HistoryItem, Role};
use crate::rollout::{self, ConfigSnapshot, RolloutItem};
use crate::session_query::documents;
use crate::session_query::query;
use crate::session_query::schema;
use crate::session_query::{
    open_attempts, retry_index_now, search, SearchError, INDEX_FILE, RESULT_LIMIT, SNIPPET_CHARS,
};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "shaniu-sq-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(rollout::ROLLOUT_DIR)).unwrap();
        Self { root }
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(config::CACHE_DIR).join(INDEX_FILE)
    }

    fn archive_path(&self, id: &str) -> PathBuf {
        self.root.join(rollout::ROLLOUT_DIR).join(format!("{id}.jsonl"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn meta(id: &str) -> RolloutItem {
    RolloutItem::SessionMeta {
        session_id: id.to_string(),
        agent_id: id.to_string(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot::default(),
    }
}

fn message(role: Role, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem {
        item: HistoryItem::Message {
            role,
            text: text.to_string(),
            reasoning: None,
        },
        at_ms: 0,
    }
}

fn call(name: &str, arguments: &str, call_id: &str) -> RolloutItem {
    RolloutItem::ResponseItem {
        item: HistoryItem::FunctionCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
            call_id: call_id.to_string(),
        },
        at_ms: 0,
    }
}

fn output(call_id: &str, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem {
        item: HistoryItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: text.to_string(),
        },
        at_ms: 0,
    }
}

/// 直接按 04 §3.1 的 JSONL 形状写一份档案（末尾带换行，`rollout::read` 严格可回读）。
fn write_archive(fixture: &Fixture, id: &str, items: &[RolloutItem]) -> PathBuf {
    let lines: Vec<String> = std::iter::once(meta(id))
        .chain(items.iter().cloned())
        .map(|item| serde_json::to_string(&item).unwrap())
        .collect();
    let path = fixture.archive_path(id);
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    path
}

fn append_to_archive(fixture: &Fixture, id: &str, item: &RolloutItem) {
    use std::io::Write as _;
    let path = fixture.archive_path(id);
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, "{}", serde_json::to_string(item).unwrap()).unwrap();
    file.flush().unwrap();
}

fn ids(page: &crate::session_query::SearchPage) -> Vec<String> {
    page.items.iter().map(|hit| hit.session_id.clone()).collect()
}

/// 中文单字与连续短语都能命中；工具输出片段同样可搜（SH-113 卡验收）。
#[test]
fn cjk_single_character_and_phrase_and_tool_output_are_searchable() {
    let fx = Fixture::new("cjk");
    write_archive(
        &fx,
        "s1",
        &[
            message(Role::User, "把端口改成9090"),
            call("unified_exec", "{\"cmd\":\"echo\"}", "c1"),
            output("c1", "工具输出：中文短语命中测试"),
        ],
    );
    write_archive(
        &fx,
        "s2",
        &[message(Role::Assistant, "另一段中文短语记录")],
    );

    let phrase = search(&fx.root, "中文短语").unwrap();
    assert_eq!(ids(&phrase).len(), 2, "连续中文短语必须在两份会话里命中");
    assert!(phrase.items.iter().all(|hit| hit.snippet.contains("中文短语")));

    let single = search(&fx.root, "中").unwrap();
    assert_eq!(single.items.len(), 2, "中文单字必须成词，单字查询要命中");

    let tool = search(&fx.root, "工具输出").unwrap();
    assert_eq!(ids(&tool), vec!["s1".to_string()], "工具输出片段必须可搜");

    let port = search(&fx.root, "端口").unwrap();
    assert_eq!(ids(&port), vec!["s1".to_string()]);
    assert_eq!(search(&fx.root, "9090").unwrap().items.len(), 1);
}

/// FTS5 语法字符一律当数据：不注入、不报语法错误、不静默吞掉请求。
#[test]
fn fts_syntax_characters_are_data_not_executable_match_syntax() {
    let fx = Fixture::new("syntax");
    write_archive(
        &fx,
        "s1",
        &[message(Role::User, "OR NEAR AND 星号* 引号\" 括号( ) 冒号: 减号-")],
    );

    // 每一个都是合法请求（Ok），不会因为 MATCH 语法失败而 IndexFailed。
    for probe in [
        "OR",
        "NEAR",
        "*",
        "\"",
        "NEAR(中 文)",
        "OR NEAR",
        "星号*",
        "括号( )",
        "a:b",
        "-",
    ] {
        let result = search(&fx.root, probe);
        assert!(
            result.is_ok(),
            "语法字符 {probe:?} 必须当数据，不能拼成可执行 MATCH：{:?}",
            result.err()
        );
    }
    // 数据本身仍能被原样搜到（引号翻倍转义后是 phrase 内的字面引号）。
    assert_eq!(search(&fx.root, "OR").unwrap().items.len(), 1);
    assert_eq!(search(&fx.root, "NEAR").unwrap().items.len(), 1);
    assert_eq!(search(&fx.root, "*").unwrap().items.len(), 0,
        "星号是数据，文档里没有独立星号 token，0 命中是诚实结果而不是语法错误");
}

/// `hasMore` 是「请收窄查询」提示，不是分页：超过固定上限才为真；
/// 每个 sessionId 只出一条最强命中的 snippet（前端 contentBySession 会去重）。
#[test]
fn has_more_is_a_narrowing_hint_and_each_session_yields_one_best_snippet() {
    let fx = Fixture::new("hasmore");
    // 每个会话故意放两条命中文档（正文 + 工具输出），验证按会话去重。
    for index in 0..(RESULT_LIMIT + 5) {
        write_archive(
            &fx,
            &format!("s{index:02}"),
            &[
                message(Role::User, "收窄查询正文片段"),
                call("unified_exec", "{\"cmd\":\"echo\"}", "c1"),
                output("c1", "收窄查询工具输出片段"),
            ],
        );
    }
    let page = search(&fx.root, "收窄查询").unwrap();
    assert_eq!(page.items.len(), RESULT_LIMIT as usize, "宿主只交固定上限内的命中");
    assert!(page.has_more, "命中会话数超过上限 → hasMore=true（提示收窄，不是续页）");
    let unique: HashSet<&str> = page.items.iter().map(|hit| hit.session_id.as_str()).collect();
    assert_eq!(unique.len(), page.items.len(), "每个 sessionId 只给一条最强命中");
    assert!(page.items.iter().all(|hit| hit.snippet.contains("收窄查询")));

    // 少于上限：hasMore=false，全部命中都交出。
    let small = Fixture::new("hasmore-few");
    for index in 0..3 {
        write_archive(&small, &format!("q{index}"), &[message(Role::User, "少量命中正文")]);
    }
    let few = search(&small.root, "少量命中").unwrap();
    assert_eq!(few.items.len(), 3);
    assert!(!few.has_more, "没有超出上限就不该提示收窄");
}

/// snippet 是纯文本：去掉 highlight 标记、剥掉零宽分隔符、按 Unicode 码点限长；
/// wire 形状只有 sessionId + snippet 两键，不夹带分数/标题/seq。
#[test]
fn snippets_are_plain_text_and_wire_has_only_two_keys() {
    let fx = Fixture::new("snippet");
    let long = format!("命中{}", "很长的正文内容".repeat(60));
    write_archive(&fx, "s", &[message(Role::User, &long)]);

    let page = search(&fx.root, "命中").unwrap();
    let hit = &page.items[0];
    assert!(hit.snippet.chars().count() <= SNIPPET_CHARS, "按码点而不是字节截断");
    assert!(hit.snippet.contains("命中"), "片段要以命中位置为中心");
    assert!(!hit.snippet.contains('\u{200B}'), "零宽分隔符不该出现在给用户的片段里");
    assert!(!hit.snippet.contains(query::FTS_HIGHLIGHT_START));
    assert!(!hit.snippet.contains(query::FTS_HIGHLIGHT_END));
    let value = serde_json::to_value(hit).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), 2, "wire 只有 sessionId + snippet");
    assert!(object.contains_key("sessionId"));
    assert!(object.contains_key("snippet"));
    assert!(!object.contains_key("score"));
}

/// 删掉派生索引后重启（等价于新建 connection）自动重建，结果集逐项不变。
#[test]
fn deleting_index_rebuilds_from_archives_with_identical_results() {
    let fx = Fixture::new("rebuild");
    write_archive(&fx, "a", &[message(Role::User, "可重建的中文短语")]);
    write_archive(&fx, "b", &[message(Role::Assistant, "另一份中文短语")]);

    let before = search(&fx.root, "中文短语").unwrap();
    assert_eq!(before.items.len(), 2);
    assert!(fx.index_path().is_file());

    fs::remove_file(fx.index_path()).unwrap();
    assert!(!fx.index_path().exists());

    let after = search(&fx.root, "中文短语").unwrap();
    assert_eq!(before.items, after.items, "自动重建后命中集合与 snippet 必须逐项不变");
    assert_eq!(before.has_more, after.has_more);
    assert!(fx.index_path().is_file(), "重建必须重新落盘");
}

/// 索引文件真损坏：改名保留旧件后自动重建，搜索仍可用，且旧件没有被删除。
#[test]
fn corrupt_index_is_quarantined_and_rebuilt_without_deleting_it() {
    let fx = Fixture::new("corrupt");
    write_archive(&fx, "a", &[message(Role::User, "损坏后重建的正文")]);
    fs::create_dir_all(fx.index_path().parent().unwrap()).unwrap();
    fs::write(fx.index_path(), b"this is definitely not a sqlite database").unwrap();

    let page = search(&fx.root, "损坏后重建").unwrap();
    assert_eq!(page.items.len(), 1, "真损坏必须安全重建而不是整体失败");
    assert!(fx.index_path().is_file(), "重建后新库必须落盘");
    let preserved = fs::read_dir(fx.root.join(config::CACHE_DIR))
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().contains("corrupt-"));
    assert!(preserved, "损坏索引必须改名保留，不能误删");
}

/// 单个档案读不回来只跳过它自己：其余会话照常出结果，且必须报告「这次不完整」。
///
/// 反向对照：把 `observe_archives` 里的跳过改回 `?` 直接返回，本用例在 `search` 处
/// 直接拿到 `Err`，两条断言都到不了——必须红。
#[test]
fn one_unreadable_archive_is_skipped_and_reported_not_fatal() {
    let fx = Fixture::new("partial");
    write_archive(&fx, "good", &[message(Role::User, "这次搜索必须仍然命中")]);
    fs::write(fx.archive_path("broken"), b"{ this is not a rollout\n").unwrap();

    let page = search(&fx.root, "这次搜索").unwrap();
    assert_eq!(ids(&page), vec!["good".to_string()], "坏档案不能拖垮整次搜索");
    assert_eq!(page.skipped_sessions, 1, "少给了一条就必须说出来，不能悄悄少给");
}

/// 已索引的会话后来读不回来：保留旧命中（读失败 ≠ 删除），并计入 skipped。
///
/// 反向对照：把 `reconcile` 里 `deleted` 的 `!unreadable.contains` 去掉，这份档案会
/// 被当成已删除、旧索引行被清掉，第一条断言立刻红。
#[test]
fn previously_indexed_session_survives_a_transient_unreadable_archive() {
    let fx = Fixture::new("retain");
    write_archive(&fx, "keep", &[message(Role::User, "保留旧命中的正文")]);
    assert_eq!(search(&fx.root, "保留旧命中").unwrap().items.len(), 1);

    fs::write(fx.archive_path("keep"), b"{ truncated").unwrap();
    let page = search(&fx.root, "保留旧命中").unwrap();
    assert_eq!(
        ids(&page),
        vec!["keep".to_string()],
        "一次读失败不等于删除，旧命中必须保留（宁可稍旧，不能凭空消失）"
    );
    assert_eq!(page.skipped_sessions, 1);
}


/// 运行中的会话新写入正文/工具输出（rollout 逐行 flush）后，下一次搜索立刻能搜到。
#[test]
fn live_session_new_content_is_searchable_immediately() {
    let fx = Fixture::new("live");
    let path = write_archive(&fx, "running", &[message(Role::User, "初始正文")]);
    let before = search(&fx.root, "运行中新增的独特标记").unwrap();
    assert!(before.items.is_empty(), "写入前不该命中");

    // 档案里输出总是伴随调用，两条一起追加才符合「先全部 call、再全部 output」的形状。
    append_to_archive(&fx, "running", &call("unified_exec", "{\"cmd\":\"echo\"}", "c9"));
    append_to_archive(&fx, "running", &output("c9", "工具结果包含运行中新增的独特标记"));
    assert!(fs::metadata(&path).unwrap().len() > 0);

    let after = search(&fx.root, "运行中新增的独特标记").unwrap();
    assert_eq!(ids(&after), vec!["running".to_string()],
        "live-preferred observation：本次观察到的增量必须并入 TEMP 表即时可搜");
}

/// 派生索引对 rollout 档案只读：多次搜索（含重建）前后字节与 mtime 完全一致。
#[test]
fn archives_are_never_written_by_the_search_index() {
    let fx = Fixture::new("readonly");
    let a = write_archive(
        &fx,
        "a",
        &[
            message(Role::User, "档案内容永远不变"),
            call("read_file", "{\"path\":\"a.txt\"}", "c1"),
            output("c1", "工具结果保持原样"),
        ],
    );
    let b = write_archive(&fx, "b", &[message(Role::Assistant, "另一份档案")]);

    let snapshot = |paths: &[&Path]| -> Vec<(Vec<u8>, std::time::SystemTime)> {
        paths
            .iter()
            .map(|path| {
                let meta = fs::metadata(path).unwrap();
                (fs::read(path).unwrap(), meta.modified().unwrap())
            })
            .collect()
    };
    let before = snapshot(&[&a, &b]);

    let _ = search(&fx.root, "档案内容").unwrap();
    let _ = search(&fx.root, "工具结果").unwrap();
    fs::remove_file(fx.index_path()).unwrap();
    let _ = search(&fx.root, "档案内容").unwrap();
    // 缓存目录里只允许出现派生索引，不能出现任何档案副本/改写痕迹。
    let cache_entries: Vec<String> = fs::read_dir(fx.root.join(config::CACHE_DIR))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(cache_entries.iter().any(|name| name == INDEX_FILE));

    let after = snapshot(&[&a, &b]);
    assert_eq!(before, after, "rollout 档案的字节与 mtime 必须零写入（AGENTS §3 / SH-113 红线）");
}

/// application-id 不符：拒绝打开，既有文件一个字节都不动（上游保守口径）。
#[test]
fn foreign_application_id_is_refused_without_touching_the_file() {
    let fx = Fixture::new("foreign");
    write_archive(&fx, "a", &[message(Role::User, "正文")]);
    fs::create_dir_all(fx.index_path().parent().unwrap()).unwrap();
    {
        let conn = Connection::open(fx.index_path()).unwrap();
        conn.execute_batch(
            "PRAGMA application_id = 424242;
             CREATE TABLE somebody_elses_data (payload TEXT);
             INSERT INTO somebody_elses_data VALUES ('do not touch');",
        )
        .unwrap();
    }
    let bytes_before = fs::read(fx.index_path()).unwrap();
    let mtime_before = fs::metadata(fx.index_path()).unwrap().modified().unwrap();

    let err = search(&fx.root, "正文").unwrap_err();
    assert!(
        matches!(err, SearchError::IndexFailed(ref message) if message.contains("属于其它应用")),
        "别人的库必须拒绝而不是清空：{err:?}"
    );
    assert_eq!(fs::read(fx.index_path()).unwrap(), bytes_before, "文件字节不得变化");
    assert_eq!(
        fs::metadata(fx.index_path()).unwrap().modified().unwrap(),
        mtime_before,
        "文件 mtime 不得变化"
    );
    // 确认真的是"没被当成我们的索引"：那张表仍在。
    let conn = Connection::open(fx.index_path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM somebody_elses_data", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

/// 空/纯空白查询按上游回空表且连索引都不打开；NUL 与超长给类型化错误。
#[test]
fn empty_whitespace_nul_and_overlong_queries_follow_wire() {
    let fx = Fixture::new("boundary");
    write_archive(&fx, "a", &[message(Role::User, "边界测试")]);

    for raw in ["", "   \t\n "] {
        let page = search(&fx.root, raw).unwrap();
        assert!(page.items.is_empty(), "空查询按上游语义回空表");
        assert!(!page.has_more);
    }
    assert!(!fx.index_path().exists(), "空查询不该付 SQLite 打开成本（首搜惰性）");

    assert!(matches!(search(&fx.root, "中\0文").unwrap_err(), SearchError::InvalidQuery(_)));
    assert!(matches!(
        search(&fx.root, &"a".repeat(query::MAX_QUERY_UTF16 + 1)).unwrap_err(),
        SearchError::QueryTooLong(_)
    ));
    let at_limit = search(&fx.root, &"a".repeat(query::MAX_QUERY_UTF16)).unwrap();
    assert!(at_limit.items.is_empty(), "恰好到上限是合法请求，0 命中是诚实结果");
    assert_eq!(RESULT_LIMIT, 20, "宿主固定结果上限取 wire 的 20");
}

/// 排名确定性：命中 span 多者优先、文档短者优先；同一请求两次结果顺序逐项一致。
#[test]
fn ranking_is_deterministic_and_prefers_more_spans_then_shorter_documents() {
    let fx = Fixture::new("ranking");
    write_archive(&fx, "many-spans", &[message(Role::User, "中文短语 中文短语 中文短语")]);
    write_archive(&fx, "one-span-long", &[message(Role::Assistant, "中文短语 后面跟着很长很长的一段无关说明文字")]);
    write_archive(&fx, "one-span-short", &[message(Role::Assistant, "中文短语")]);

    let first = search(&fx.root, "中文短语").unwrap();
    assert_eq!(first.items.len(), 3);
    assert_eq!(first.items[0].session_id, "many-spans", "命中 span 多者排最前");
    assert_eq!(first.items[2].session_id, "one-span-long", "同为 1 span 时短文档优先");
    let second = search(&fx.root, "中文短语").unwrap();
    assert_eq!(first.items, second.items, "排名必须确定性，不依赖行号/插入顺序抖动");
    assert!(!first.has_more);
    let value = serde_json::to_value(&first.items[0]).unwrap();
    assert!(value.get("sessionId").is_some());
    assert!(value.get("snippet").is_some());
    assert!(value.get("score").is_none(), "分数只用于排序，不出现在 wire 里");
}

/// 同一份历史：直接抽取与"写进档案再回放读回"抽出的文档序列逐项一致（SH-108 范式）。
#[test]
fn document_extraction_is_stable_across_live_and_replayed_history() {
    let items = vec![
        message(Role::User, "把端口改成9090"),
        call("read_file", "{\"path\":\"a.txt\"}", "c1"),
        call("unified_exec", "{\"cmd\":\"ls\"}", "c2"),
        output("c2", "a.txt\nb.txt"),
        output("c1", "文件内容"),
        RolloutItem::Compacted { range_start: 0, range_end: 2, summary: "前两轮摘要".into() },
        RolloutItem::SubagentRef { child_id: "child".into(), rollout_path: "p".into() },
    ];
    let direct = documents::build_documents(&items, 1_700_000_000_000);
    let direct_again = documents::build_documents(&items, 1_700_000_000_000);
    assert_eq!(direct, direct_again, "纯函数抽取必须确定性");

    let fx = Fixture::new("documents");
    write_archive(&fx, "a", &items);
    let archive = rollout::read(&fx.archive_path("a")).unwrap();
    let replayed = documents::build_documents(&archive.items, 1_700_000_000_000);
    assert_eq!(direct, replayed, "实时序列与档案回放重建序列必须逐项一致");

    let all_text = direct.iter().map(|doc| doc.text.as_str()).collect::<Vec<_>>().join("\n");
    assert!(all_text.contains("unified_exec"));
    assert!(all_text.contains("b.txt"), "工具输出片段必须被抽取");
    assert!(all_text.contains(&query::fts_text("前两轮摘要")), "压缩锚点摘要必须可索引");
    assert!(!all_text.contains("child"), "子代理引用不是可搜索内容");
    assert!(direct.iter().any(|doc| doc.doc_type == "tool/result"));
    assert!(direct.iter().all(|doc| !doc.text.contains('\0')));
}

/// query 纯函数：literal phrase 转义、CJK 对称、snippet 边界与可读性。
#[test]
fn query_helpers_are_pure_and_contract_safe() {
    assert_eq!(query::quote_fts_data("中文 OR \"NEAR\""), "\"中文 OR \"\"NEAR\"\"\"");
    assert_eq!(query::normalize_query("  中文   短语  ").unwrap().data, query::fts_text("中文 短语"));
    // 文档与查询两侧对称：连续短语仍是相邻 token 的 phrase。
    assert!(query::fts_text("中文短语").contains('\u{200B}'));
    assert_eq!(query::fts_text("中文短语").chars().filter(|c| *c == '中').count(), 1);

    let marked = format!("前{}中文短语{}后{}", query::FTS_HIGHLIGHT_START, query::FTS_HIGHLIGHT_END, "缀".repeat(400));
    let snippet = query::make_snippet(&marked, SNIPPET_CHARS);
    assert!(snippet.chars().count() <= SNIPPET_CHARS);
    assert!(snippet.contains("中文短语"), "snippet 必须以命中位置为中心");
    assert!(!snippet.contains('\u{200B}'), "零宽分隔符不该出现在给用户的片段里");
    assert!(!snippet.contains(query::FTS_HIGHLIGHT_START));
}

/// schema：空库可认领、版本不匹配的原地重建仍可用、未知用户表拒绝。
#[test]
fn schema_ownership_is_conservative_and_supports_in_place_version_reset() {
    let fx = Fixture::new("schema");
    write_archive(&fx, "a", &[message(Role::User, "schema 归属测试")]);
    fs::create_dir_all(fx.index_path().parent().unwrap()).unwrap();

    // application_id=0 且有未知用户表 → 拒绝，不认领。
    {
        let conn = Connection::open(fx.index_path()).unwrap();
        conn.execute_batch("CREATE TABLE unexpected (x);").unwrap();
    }
    let err = search(&fx.root, "schema").unwrap_err();
    assert!(matches!(err, SearchError::IndexFailed(ref m) if m.contains("拒绝改写")), "{err:?}");

    // 换成"我们的 application id + 过期版本" → 原地重置，搜索恢复正常。
    fs::remove_file(fx.index_path()).unwrap();
    {
        let conn = Connection::open(fx.index_path()).unwrap();
        conn.execute_batch(&format!(
            "PRAGMA application_id = {}; PRAGMA user_version = 0;
             CREATE TABLE persisted_sessions (id TEXT PRIMARY KEY, revision TEXT, generation INTEGER);",
            schema::APPLICATION_ID
        ))
        .unwrap();
    }
    let page = search(&fx.root, "schema").unwrap();
    assert_eq!(page.items.len(), 1, "认识的不兼容派生 schema 必须能原地重置并重建索引");
    // 重建后落在**当前**版本上（断言版本落盘，而不是常量自证）
    let conn = Connection::open(fx.index_path()).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, schema::SCHEMA_VERSION);
}

/// 连接参数（2026-11-11）：WAL + 5 秒忙等待。旧代码只有 application_id，
/// 这两条断言都会红。
#[test]
fn index_connection_uses_wal_and_five_second_busy_timeout() {
    let fx = Fixture::new("pragmas");
    let conn = schema::open_index(&fx.index_path()).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal", "派生索引必须跑在 WAL 下");
    let timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(timeout, 5_000, "忙等待必须是 5 秒量级，不能是默认 0");
}

/// 双开搜索：另一个连接持写锁时，第二次搜索应**等待后成功**，而不是立刻
/// `IndexFailed`（旧代码 `busy_timeout=0` 会立即失败 → 本用例红）。
#[test]
fn search_waits_for_a_concurrent_writer_instead_of_failing() {
    use std::time::{Duration, Instant};

    let fx = Fixture::new("busy");
    write_archive(&fx, "a", &[message(Role::User, "并发写等待")]);
    // 先建好索引（这张连接即"另一个实例"），再用 BEGIN IMMEDIATE 占住写锁。
    let holder = schema::open_index(&fx.index_path()).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    let root = fx.root.clone();
    let searcher = std::thread::spawn(move || {
        let start = Instant::now();
        let result = search(&root, "并发写等待");
        (result, start.elapsed())
    });
    // 让搜索线程真的撞上写锁再放行。
    std::thread::sleep(Duration::from_millis(300));
    holder.execute_batch("COMMIT").unwrap();

    let (result, elapsed) = searcher.join().unwrap();
    assert!(
        result.is_ok(),
        "争用应等待后成功，而不是立刻 IndexFailed：{:?}",
        result.err()
    );
    assert!(
        elapsed >= Duration::from_millis(250),
        "第二个连接必须真的等待过锁，实际 {elapsed:?}"
    );
}

/// 索引建不起来时记住失败：冷却期内不再打开索引文件，错误码与原因不变。
///
/// 反向对照：去掉 `search()` 里的 `cooldown_message` 前置检查，第二次搜索会再打开
/// 一次索引，计数变成 +2，本用例红。
#[test]
fn index_failure_enters_cooldown_without_touching_the_file_again() {
    let fx = Fixture::new("cooldown");
    retry_index_now();
    // 把 cache 目录做成普通文件：索引路径不可创建，open_index 必然 IndexFailed。
    fs::write(fx.root.join(config::CACHE_DIR), b"not a directory").unwrap();
    write_archive(&fx, "a", &[message(Role::User, "冷却期内的正文")]);

    let before = open_attempts();
    let first = search(&fx.root, "冷却期内");
    let second = search(&fx.root, "冷却期内");
    let after = open_attempts();

    assert!(
        matches!(first, Err(SearchError::IndexFailed(_))),
        "索引打不开必须报错，不能拿空结果冒充「没搜到」"
    );
    assert_eq!(after - before, 1, "冷却期内不该再打开索引文件：撞一次墙就够");

    let (Err(SearchError::IndexFailed(first)), Err(SearchError::IndexFailed(second))) =
        (&first, &second)
    else {
        panic!("两次都应是 IndexFailed");
    };
    assert!(
        second.contains(first.as_str()),
        "冷却期返回的必须还是同一条原因：{first} vs {second}"
    );
    assert!(second.contains("不再重复尝试"));

    // 重试入口：清掉记忆后必须真的重新尝试（这面墙还在，所以计数 +1）。
    retry_index_now();
    let _ = search(&fx.root, "冷却期内");
    assert_eq!(
        open_attempts() - after,
        1,
        "手动重试必须能绕过冷却、真的再试一次"
    );
}

/// 隔离件只留最近几份，`cache/` 不被历史垃圾撑起来。
#[test]
fn quarantine_keeps_only_the_most_recent_copies() {
    let fx = Fixture::new("quarantine-cap");
    let cache = fx.root.join(config::CACHE_DIR);
    fs::create_dir_all(&cache).unwrap();
    for stamp in 1..=5 {
        fs::write(
            cache.join(format!("{INDEX_FILE}.corrupt-0-{stamp:013}")),
            b"older quarantine",
        )
        .unwrap();
    }
    write_archive(&fx, "a", &[message(Role::User, "上限清理后的正文")]);
    fs::write(fx.index_path(), b"this is definitely not a sqlite database").unwrap();

    assert_eq!(search(&fx.root, "上限清理后的正文").unwrap().items.len(), 1);
    let mut kept: Vec<String> = fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".corrupt-"))
        .collect();
    kept.sort();
    assert_eq!(
        kept.len(),
        schema::QUARANTINE_KEEP,
        "只留最近 {} 份隔离件，实际：{kept:?}",
        schema::QUARANTINE_KEEP
    );
    assert!(
        kept.iter().any(|name| name.contains("-0000000000005")),
        "被保留的应是较新的几份：{kept:?}"
    );
}


/// 索引层：被压缩锚点取代的原文标 `shadowed`，摘要与区间外内容仍是 `current`。
///
/// 序号口径与 `rollout::resume` 一致——工具输出**占**历史序号。锚点 0..2 吃掉
/// 「消息 + 调用」，输出自己那一项仍在历史里，所以它的文档仍是 `current`。
#[test]
fn documents_mark_compacted_originals_as_shadowed() {
    let items = vec![
        message(Role::User, "旧目标：端口 3306"),
        call("unified_exec", "{\"cmd\":\"ls\"}", "c1"),
        output("c1", "旧工具输出"),
        RolloutItem::Compacted { range_start: 0, range_end: 2, summary: "摘要：数据库配置已确定".into() },
        message(Role::Assistant, "压缩后的新回复"),
    ];
    let docs = documents::build_documents(&items, 1_700_000_000_000);

    // 索引文本按 query::fts_text 归一化过（CJK 字间插零宽空格），比对前先还原
    let plain = |text: &str| text.replace('\u{200b}', "");
    let surface_of = |needle: &str| {
        docs.iter()
            .find(|doc| plain(&doc.text).contains(needle))
            .unwrap_or_else(|| panic!("缺少包含 {needle} 的文档：{docs:?}"))
            .surface
    };
    assert_eq!(surface_of("3306"), "shadowed", "被摘要取代的原文字档仍在，但模型当前不再看到");
    assert_eq!(surface_of("unified_exec"), "shadowed", "调用与它占的区间一起被取代");
    assert_eq!(surface_of("旧工具输出"), "current", "输出是独立历史项，锚点没覆盖它");
    assert_eq!(surface_of("已确定"), "current", "摘要本身就是当前内容");
    assert_eq!(surface_of("压缩后的新回复"), "current");
}

/// 端到端：搜到被压缩的原文时，snippet 必须自带「模型当前不再看到」的标注；
/// 搜到仍在上下文里的内容时不加。
///
/// 反向对照：把 `build_documents` 的 surface 判定去掉（恒为 `current`），
/// 第一条断言即红——那正是"能查到"被当成"还在生效"的形态。
#[test]
fn searching_a_compacted_original_says_the_model_no_longer_sees_it() {
    let fx = Fixture::new("shadowed");
    write_archive(
        &fx,
        "s1",
        &[
            message(Role::User, "确定端口为 3306"),
            RolloutItem::Compacted {
                range_start: 0,
                range_end: 1,
                summary: "摘要：已确定数据库配置".into(),
            },
            message(Role::Assistant, "收到"),
        ],
    );

    let shadowed = search(&fx.root, "3306").unwrap();
    assert_eq!(ids(&shadowed), vec!["s1".to_string()], "原文仍必须能搜到");
    let snippet = &shadowed.items[0].snippet;
    assert!(
        snippet.starts_with(crate::session_query::SHADOWED_SNIPPET_PREFIX),
        "搜到已压缩的原文必须标注模型当前不再看到：{snippet}"
    );
    assert!(snippet.contains("3306"), "标注不能吞掉命中内容：{snippet}");

    let current = search(&fx.root, "数据库配置").unwrap();
    assert_eq!(ids(&current), vec!["s1".to_string()]);
    assert!(
        !current.items[0].snippet.contains(crate::session_query::SHADOWED_SNIPPET_PREFIX),
        "摘要仍在上下文里，不该被标注成已压缩：{}",
        current.items[0].snippet
    );
}
