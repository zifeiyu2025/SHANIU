//! 健身函数 FF-3（docs/01 §5）：沙箱隔离探针。
//!
//! 用真实子进程验证 workspace-write 的**行为边界**，而不是断言配置值：
//! 1) 工作区内写入应成功——证明沙箱本身可用（否则"越界被拒"可能是沙箱根本没跑起来）；
//! 2) 工作区外写入应失败——FF-3 的核心不变量；
//! 3) 工作区外读取应失败——AppContainer 的读隔离（SH-021）；
//! 4) loopback 隔离——容器连不回宿主 127.0.0.1 端口（与网络能力无关的另一条边界）；
//!    出网本身是默认能力，正向证据见 `container_reaches_internet_without_any_grant`；
//! 5) 受限令牌层独立生效——逻辑沙箱路径同样剥离特权（纵深两层各自可验证）。
//!
//! 允许位置按用户 2026-11-08 的决定：工作区根 / temp / 工具链目录。
//! 因此"越界"探针的目标**不能放在 temp**——temp 是明确授权的位置。

#![cfg(windows)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use shaniu_lib::config::SandboxMode;
use shaniu_lib::sandbox::{prepare_spawn, PreparedSpawn};
use tokio::sync::mpsc;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("shaniu-probe-{tag}-{}-{n}", id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 工作区外、且不在 temp / 工具链目录里的位置（用户主目录下）。
fn outside_dir(tag: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let home = std::env::var_os("USERPROFILE").expect("探针需要 USERPROFILE 定位工作区外目录");
    let dir = PathBuf::from(home).join(format!("shaniu-probe-{tag}-{}-{n}", id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 在 workspace-write 沙箱内执行 `cmd /C <line>`，返回退出码与合并输出。
async fn run_sandboxed_capture(cwd: &Path, line: &str) -> (i32, String) {
    let (tx, mut rx) = mpsc::channel::<shaniu_lib::sandbox::ExecLine>(256);
    let collector = tokio::spawn(async move {
        let mut output = String::new();
        while let Some((_, text)) = rx.recv().await {
            output.push_str(&text);
            output.push('\n');
        }
        output
    });
    let spawn = prepare_spawn(SandboxMode::WorkspaceWrite, "cmd", &["/C", line], cwd, tx)
        .expect("workspace-write 沙箱应可创建（fail-closed 之外的正常路径）");
    let code = match spawn {
        PreparedSpawn::Restricted(mut child) => child.wait().await.expect("等待子进程失败"),
        PreparedSpawn::Plain(_) => panic!("workspace-write 不得退化为无沙箱执行（FF-4）"),
    };
    let output = collector.await.expect("读线程不应 panic");
    (code, output)
}

async fn run_sandboxed(cwd: &Path, line: &str) -> i32 {
    run_sandboxed_capture(cwd, line).await.0
}

#[tokio::test]
async fn sandbox_can_write_inside_workspace() {
    let ws = unique_dir("in");
    let target = ws.join("probe.txt");
    let code = run_sandboxed(&ws, &format!("echo inside> \"{}\"", target.display())).await;

    assert_eq!(code, 0, "工作区内写入的退出码应为 0");
    assert!(target.is_file(), "工作区内写入应真实落盘（证明沙箱可运行）");
    std::fs::remove_dir_all(&ws).ok();
}

#[tokio::test]
async fn readonly_mode_refuses_to_spawn() {
    let ws = unique_dir("ro");
    let (tx, _rx) = mpsc::channel(4);
    let err = prepare_spawn(SandboxMode::ReadOnly, "cmd", &["/C", "echo x"], &ws, tx)
        .err()
        .expect("read-only 必须拒绝命令执行");
    assert!(
        matches!(err, shaniu_lib::sandbox::SandboxError::PolicyDenied(_)),
        "read-only 拒绝应归为策略拒绝，而非创建失败：{err}"
    );
    std::fs::remove_dir_all(&ws).ok();
}

/// FF-3 核心不变量：工作区外写入必须失败。
#[tokio::test]
async fn sandbox_denies_write_outside_workspace() {
    let ws = unique_dir("ws");
    let outside = outside_dir("outside");
    let target = outside.join("escaped.txt");

    let code = run_sandboxed(&ws, &format!("echo escaped> \"{}\"", target.display())).await;

    // 无论 cmd 返回码如何，关键断言是：越界文件不得出现
    assert!(
        !target.exists(),
        "工作区外写入成功（cmd 退出码 {code}）：AppContainer 未提供路径级隔离"
    );
    std::fs::remove_dir_all(&ws).ok();
    std::fs::remove_dir_all(&outside).ok();
}

/// AppContainer 读隔离：工作区外的文件内容不得进入沙箱进程。
#[tokio::test]
async fn sandbox_denies_read_outside_workspace() {
    let ws = unique_dir("read-ws");
    let outside = outside_dir("read-outside");
    let secret = outside.join("secret.txt");
    let marker = "SHANIU-READ-ISOLATION-MARKER";
    std::fs::write(&secret, marker).unwrap();

    let (_code, output) =
        run_sandboxed_capture(&ws, &format!("type \"{}\"", secret.display())).await;

    assert!(
        !output.contains(marker),
        "工作区外文件内容被读进沙箱（输出：{output}）"
    );
    std::fs::remove_dir_all(&ws).ok();
    std::fs::remove_dir_all(&outside).ok();
}

/// 一个人工的 HTTP 服务：用于验证「容器连不回宿主 loopback」不是靠猜。
///
/// 必须**先读掉请求再回写**：如果 accept 后不回读就 `write_all` 并立刻 close，
/// curl 会拿到 RST（exit 56），对照组就变成假红——而对照组一红，隔离用例的结论
/// 就不可信了（分不清"被沙箱拦"还是"服务本身没通"）。
/// 同理，accept 出来的 socket 必须显式切回阻塞：Windows 会让它继承监听端的非阻塞标志，
/// 「看起来读了」其实立刻 WouldBlock，等价于没读（CI `42600499992` 的假红来源）。
struct LocalServer {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl LocalServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        use std::io::Read as _;
                        // Windows 上 accept 出来的 socket 会继承监听端的非阻塞标志：
                        // 不显式切回阻塞，`read` 会立刻 WouldBlock，请求还留在内核缓冲里就去写响应并关闭。
                        // 带未读数据 close 时 Windows 发 RST，curl 报 56——对照组变成假红
                        // （CI `42600499992` 就是这么红的：`status 56, stdout "000"`）。
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut request = [0u8; 2048];
                        let mut filled = 0usize;
                        // 读到请求头结束再回写：只读一次会在慢一点的环境里读到半个请求。
                        while filled < request.len() {
                            match stream.read(&mut request[filled..]) {
                                Ok(0) => break,
                                Ok(read) => {
                                    filled += read;
                                    if request[..filled].windows(4).any(|part| part == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        let _ = stream.flush();
                        // 半关闭写方向：让 curl 先读到完整响应与 EOF，再随 drop 收尾。
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        // 关之前把客户端余下数据读干净：还有未读数据就 close，同样会发 RST。
                        let mut sink = [0u8; 512];
                        while matches!(stream.read(&mut sink), Ok(read) if read > 0) {}
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { port, stop }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// AppContainer 的 loopback 隔离：容器内进程连不到宿主的 127.0.0.1 服务。
///
/// 注意这条**不是**「默认断网」的证据——网络已不是策略维度，容器一律带
/// `internetClient`（出网见 `container_reaches_internet_without_any_grant`）。
/// 这里证明的是另一条独立边界：容器不能回连宿主本机端口，宿主服务不被容器探测。
/// 对照组（非沙箱进程）必须能连通，否则这条用例就成了"服务没起来"的假绿。
#[tokio::test]
async fn appcontainer_isolates_host_loopback() {
    let server = LocalServer::start();
    let url = format!("http://127.0.0.1:{}/", server.port);

    // 对照组：普通进程能连上 → 服务本身可用
    let control = std::process::Command::new("curl")
        .args(["-s", "-o", "NUL", "-w", "%{http_code}", "--max-time", "3", &url])
        .output()
        .expect("需要系统 curl 做对照");
    assert!(
        control.status.success() && String::from_utf8_lossy(&control.stdout).trim() == "200",
        "对照组 curl 未连通本地服务，loopback 隔离用例结论不可信：{control:?}"
    );

    let ws = unique_dir("net");
    let (code, output) = run_sandboxed_capture(
        &ws,
        &format!(
            "curl -s -o NUL --max-time 3 \"{}\"",
            url
        ),
    )
    .await;
    assert_ne!(code, 0, "容器内进程不应能回连宿主 loopback（输出：{output}）");
    std::fs::remove_dir_all(&ws).ok();
}

/// 容器出网的**正向**证据：没有任何授权动作，workspace-write 容器直接能打外网。
///
/// 这是删掉网络闸之后的反向对照——旧用例证明「不授权就连不上」，本用例证明
/// 「不授权也能连上」。两条不能同时存在，所以旧用例被这条替换，而不是被删掉后
/// 什么都不留。
///
/// 为什么打外网而不是 loopback：AppContainer 的 loopback 隔离是另一条限制
/// （带不带 `internetClient` 都连不上 127.0.0.1），拿 loopback 当靶子会得到
/// 与网络能力无关的结论。对照组用普通进程 curl——对照组不通说明是网络本身的问题。
///
/// 标 `#[ignore]`：需要外网，不进 CI；跑法是
/// `cargo test --offline --test sandbox_probe -- --ignored container_reaches_internet --nocapture`。
#[tokio::test]
#[ignore = "需要外网：容器默认出网的真机验证"]
async fn container_reaches_internet_without_any_grant() {
    let url = std::env::var("SHANIU_NETWORK_PROBE_URL")
        .unwrap_or_else(|_| "https://www.baidu.com/".to_string());

    // 对照组：普通进程能连通，否则下面的"容器应成功"是假红。
    let control = std::process::Command::new("curl")
        .args(["-s", "-o", "NUL", "-w", "%{http_code}", "--max-time", "10", &url])
        .output()
        .expect("需要系统 curl 做对照");
    assert_eq!(
        String::from_utf8_lossy(&control.stdout).trim(),
        "200",
        "对照组 curl 未连通 {url}，出网用例结论不可信：{control:?}"
    );

    let ws = unique_dir("netgrant");
    let (code, output) = run_sandboxed_https(&ws, &url).await;
    assert_eq!(
        code, 0,
        "网络不再受策略控制，容器应能直接出网（输出：{output}）"
    );
    assert!(
        output.contains("200"),
        "curl 应返回 200，实际输出：{output}"
    );

    std::fs::remove_dir_all(&ws).ok();
}

/// 在 workspace-write 沙箱里跑一次 HTTPS 请求。
async fn run_sandboxed_https(cwd: &Path, url: &str) -> (i32, String) {
    let (tx, mut rx) = mpsc::channel::<shaniu_lib::sandbox::ExecLine>(256);
    let collector = tokio::spawn(async move {
        let mut output = String::new();
        while let Some((_, text)) = rx.recv().await {
            output.push_str(&text);
            output.push('\n');
        }
        output
    });
    let args = ["-s", "-o", "NUL", "-w", "%{http_code}", "--max-time", "10", url];
    let spawn = shaniu_lib::sandbox::prepare_spawn_with(
        shaniu_lib::sandbox::SpawnRequest {
            policy: SandboxMode::WorkspaceWrite,
            workspace_root: cwd,
            program: "curl",
            args: &args,
            cwd,
            os: shaniu_lib::sandbox::current_os_mode(),
        },
        tx,
    )
    .expect("workspace-write 沙箱应可创建");
    let code = match spawn {
        PreparedSpawn::Restricted(mut child) => child.wait().await.expect("等待子进程失败"),
        PreparedSpawn::Plain(_) => panic!("workspace-write 不得退化为无沙箱执行（FF-4）"),
    };
    let output = collector.await.expect("读线程不应 panic");
    (code, output)
}

/// 纵深第 1 层（受限令牌）独立验证：不走 AppContainer 时，特权也必须已被剥离。
#[tokio::test]
async fn restricted_token_layer_strips_privileges_independently() {
    let ws = unique_dir("token");
    let (tx, mut rx) = mpsc::channel::<shaniu_lib::sandbox::ExecLine>(256);
    let collector = tokio::spawn(async move {
        let mut output = String::new();
        while let Some((_, text)) = rx.recv().await {
            output.push_str(&text);
            output.push('\n');
        }
        output
    });
    let mut child = shaniu_lib::sandbox::windows_impl::spawn_restricted_command(
        "whoami",
        &["/priv"],
        &ws,
        tx,
    )
    .expect("受限令牌路径应可启动");
    let code = child.wait().await.expect("等待子进程失败");
    let output = collector.await.expect("读线程不应 panic");
    assert_eq!(code, 0, "whoami 应成功：{output}");
    // 2026-11-09 加固：原断言只查 3 个高危特权，在**非提权**环境下等于空转
    // （普通用户令牌本来就没有 SeDebug/SeBackup/SeImpersonate），删除逻辑失效也照样绿。
    // 现在要求「只剩 SeChangeNotifyPrivilege」：它是绕过目录遍历检查的能力，
    // 进程加载/开路径依赖它，必须保留；其余特权一条都不许留。
    let leftover: Vec<&str> = output
        .lines()
        .filter(|line| line.trim_start().starts_with("Se") && line.contains("Privilege"))
        .collect();
    let unexpected: Vec<&str> = leftover
        .iter()
        .copied()
        .filter(|line| !line.trim_start().starts_with("SeChangeNotifyPrivilege"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "受限令牌仍有应被删除的特权：{}",
        unexpected.join(" | ")
    );
    assert!(
        leftover.len() <= 1,
        "受限令牌特权数应至多 1（仅 SeChangeNotifyPrivilege），实际 {} 条：{}",
        leftover.len(),
        leftover.join(" | ")
    );
    for privileged in ["SeDebugPrivilege", "SeBackupPrivilege", "SeImpersonatePrivilege"] {
        assert!(
            !output.contains(privileged),
            "受限令牌未剥离 {privileged}（输出：{output}）"
        );
    }
    std::fs::remove_dir_all(&ws).ok();
}

/// 输出**逐行完整性**探针（FF-3 沙箱路径的数据保真一侧）。
///
/// 回归锁：受限沙箱的管道读线程曾把 `BufReader` 建在读循环内部——每轮新建会读走一整块
/// （默认 8KB）却只交出一行，缓冲区里的剩余行随 drop 一起丢弃。症状是"命令输出莫名缺行"：
/// 列举 5 个文件，模型只收到第一行，于是反复重跑同一条命令也看不到完整清单。
/// 这类丢失不会让任何一退出码变红，只有把"每一行都必须到齐"写成断言才拦得住。
///
/// 为什么用 `for %f in (*)` 而不是 `dir /b`：AppContainer 下的 `cmd /C dir` **实测静默无输出**
/// （退出码 1、stdout 零字节；`vol` 也返回空卷标），而同一容器里 `for %f in (*)`、`type`、
/// `echo` 都正常——这是 cmd 依赖卷信息导致的行为差异，不是路径隔离失效。回归锁要锁的是
/// 管道逐行保真，不该被这个 quirk 绑架，所以用同样会「一次吐多行」的 `for` 列举。
#[tokio::test]
async fn sandbox_streams_every_output_line() {
    let names = ["alpha.txt", "bravo.txt", "charlie.txt", "delta.txt", "echofiv.txt"];
    let ws = unique_dir("lines");
    for name in names {
        std::fs::write(ws.join(name), "x").unwrap();
    }

    let (tx, mut rx) = mpsc::channel(256);
    let collector = tokio::spawn(async move {
        let mut lines = Vec::new();
        while let Some((_, line)) = rx.recv().await {
            lines.push(line);
        }
        lines
    });
    let spawn = prepare_spawn(
        SandboxMode::WorkspaceWrite,
        "cmd",
        &["/C", "for %f in (*) do @echo %f"],
        &ws,
        tx,
    )
    .expect("workspace-write 沙箱应可创建");
    let code = match spawn {
        PreparedSpawn::Restricted(mut child) => child.wait().await.expect("等待子进程失败"),
        PreparedSpawn::Plain(_) => panic!("workspace-write 不得退化为无沙箱执行"),
    };
    let lines = collector.await.expect("读线程不应 panic");
    assert_eq!(code, 0, "沙箱内 for 列举应成功（实际输出：{lines:?}）");

    for name in names {
        assert!(
            lines.iter().any(|l| l == name),
            "输出行 {name} 丢失（沙箱管道只回传了部分行）。实际收到：{lines:?}"
        );
    }
    std::fs::remove_dir_all(&ws).ok();
}
