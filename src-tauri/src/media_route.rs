//! `/api/file` 媒体路由（SH-135）：工作区里的图片 / PDF 预览的取字节通道。
//!
//! **为什么挂在 `http://tauri.localhost/api/file` 而不是自定义 scheme**：前端
//! `fileMediaUrl(document.baseURI, abs)` 已经产出这条同源 URL（`workspace-path/src/index.ts`），
//! 同源 = CSP `default-src 'self'` 已覆盖，因此**不需要**新增 `img-src` 或任何指令
//! （CSP 已冻结）。宿主侧用 `WebviewWindowBuilder::on_web_resource_request` 在资产服务
//! 之后接管这一条路径，其余请求一个字节都不改。
//!
//! **边界**：读类只读、不越出宿主登记的根（默认工作区 + `config.workspaces`），判据与
//! `tool_router::resolve_in_workspace` 完全一致（canonicalize + starts_with）；末段是符号
//! 链接按 `workspace_files::require_regular` 同口径拒绝。扩展名走白名单，白名单之外连
//! 文件都不打开——这条路由不是"通用文件服务"。
//!
//! **请求线程的硬约束**（2026-11-10 用户要求）：不 panic、不等配置写锁、超长 path 直接拒。
//! panic 的表现是白屏，而白屏没人能归因；等锁会把图片请求和设置页保存互锁死。

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use tauri::Manager as _;

use crate::app_state::AppState;
use crate::config::AppConfig;

/// 路由路径：只接管这一条，其余交给资产服务。
pub const ROUTE: &str = "/api/file";

/// `path` 查询参数的长度上限：超长串不进入任何路径解析（canonicalize 之前就拒）。
pub const MAX_PATH_BYTES: usize = 8_192;

/// 单次响应的字节上限，与 `workspace_files::MAX_FILE_BYTES` 同值同口径。
pub const MAX_MEDIA_BYTES: u64 = crate::workspace_files::MAX_FILE_BYTES;

/// 媒体响应：核心层产物，不依赖 Tauri 类型，测试可直接断言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReply {
    pub status: u16,
    /// `Some` 只出现在 200：错误响应不带内容类型，避免浏览器把它当可渲染内容。
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

fn fail(status: u16, code: &str) -> MediaReply {
    MediaReply { status, content_type: None, body: code.as_bytes().to_vec() }
}

/// 扩展名白名单（小写）。不在表里 = 415，**不碰盘**。
///
/// 用显式表而不是 `mime_guess`：需求是"只放媒体"，全量 MIME 表会把 `.html` / `.js`
/// 一并放进来，那正是这条路由最不该能读的东西。
fn media_type(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        // SVG 是活动内容，但只经 `<img>` 渲染、且响应带 `sandbox; default-src 'none'`，
        // 直接打开也不会执行脚本（与上游 media-references 同口径）。
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

/// 媒体路由可读的根集合（默认工作区 + 额外工作区），顺序稳定、去重、跳过空项。
///
/// 每个根都过工作区唯一判据：额外工作区若指到 `data\secrets` 这类位置，媒体路由
/// 不能成为它的读取通道（即使配置里已经存在这一项）。
pub fn media_roots_of(config: &AppConfig, data_root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![crate::app_state::workspace_root_of(config, data_root)];
    for profile in &config.workspaces {
        let path = profile.path.trim();
        if path.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(path);
        if !crate::config::workspace_gate::check_against(&candidate, data_root).is_allow() {
            continue;
        }
        if !roots.contains(&candidate) {
            roots.push(candidate);
        }
    }
    roots
}

/// 请求路径的静态校验顺序：长度 → 控制字符 → UNC → 绝对性 → `..`。
/// 全部在解析之前完成，失败一律 400（不区分原因细节之外的语义）。
fn validate_shape(path: Option<&str>) -> Result<&str, MediaReply> {
    let Some(path) = path else { return Err(fail(400, "missing path")) };
    if path.is_empty() {
        return Err(fail(400, "missing path"));
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(fail(400, "path too long"));
    }
    if path.chars().any(|c| c.is_control()) {
        return Err(fail(400, "absolute path required"));
    }
    if path.starts_with("\\\\") || path.starts_with("//") {
        return Err(fail(400, "absolute path required"));
    }
    if !Path::new(path).is_absolute() {
        return Err(fail(400, "absolute path required"));
    }
    if Path::new(path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(fail(400, "absolute path required"));
    }
    Ok(path)
}

/// 逐根解析：命中即返回；全部未命中时按"是否存在"区分 404 / 403。
fn resolve(roots: &[PathBuf], path: &str) -> Result<PathBuf, MediaReply> {
    for root in roots {
        if let Ok(found) = crate::tool_router::resolve_in_workspace(root, path) {
            return Ok(found);
        }
    }
    match Path::new(path).canonicalize() {
        // 不存在（或不可解析）：404 不泄露根结构。
        Err(_) => Err(fail(404, "not found")),
        // 存在但不在任何登记根下：越界。
        Ok(_) => Err(fail(403, "outside workspace")),
    }
}

/// 读一个媒体文件：校验 → 解析 → 普通文件检查 → 大小上限 → 读字节。
pub fn serve(roots: &[PathBuf], path: Option<&str>) -> MediaReply {
    let path = match validate_shape(path) {
        Ok(path) => path,
        Err(reply) => return reply,
    };
    let Some(mime) = media_type(path) else {
        return fail(415, "unsupported media type");
    };
    let target = match resolve(roots, path) {
        Ok(target) => target,
        Err(reply) => return reply,
    };
    // `resolve_in_workspace` 已经 canonicalize，末段链接通常已解析成目标；这里仍按
    // `workspace_files::require_regular` 同口径再查一次（不跟随链接、必须是普通文件），
    // 用于兜住"链接在解析后仍然存在"的边缘情形，代价只是一次 lstat。
    let link = match std::fs::symlink_metadata(&target) {
        Ok(link) => link,
        Err(_) => return fail(404, "not found"),
    };
    if link.file_type().is_symlink() || !link.is_file() {
        return fail(403, "not a regular file");
    }
    if link.len() > MAX_MEDIA_BYTES {
        return fail(413, "file exceeds byte limit");
    }
    let body = match std::fs::read(&target) {
        Ok(body) => body,
        Err(_) => return fail(500, "read failed"),
    };
    // 元数据与读之间文件被换掉时，实际字节仍受同一上限约束。
    if body.len() as u64 > MAX_MEDIA_BYTES {
        return fail(413, "file exceeds byte limit");
    }
    MediaReply { status: 200, content_type: Some(mime), body }
}

/// 把查询串里的 `path` 解出来（只解码一次：前端已 `encodeURIComponent`）。
/// 返回 `Err(())` = 百分号编码非法，调用方转 400。
fn path_param(query: Option<&str>) -> Result<Option<String>, ()> {
    let Some(query) = query else { return Ok(None) };
    for pair in query.split('&') {
        let Some(raw) = pair.strip_prefix("path=") else { continue };
        return percent_encoding::percent_decode_str(raw)
            .decode_utf8()
            .map(|decoded| Some(decoded.into_owned()))
            .map_err(|_| ());
    }
    Ok(None)
}

/// 把结果写成 HTTP 响应。**不新增 CSP 指令**：这里只给自己的响应加头，
/// 资产响应的 CSP 原样不动（本函数只在 `/api/file` 上被调用）。
fn write(response: &mut tauri::http::Response<Cow<'static, [u8]>>, reply: MediaReply, head: bool) {
    use tauri::http::header::HeaderValue;
    let length = reply.body.len();
    let mut builder = tauri::http::Response::builder()
        .status(reply.status)
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", "sandbox; default-src 'none'")
        // 没有 `?v=` 版本参数，URL 稳定而内容会变：`immutable` 会让改写后的图片
        // 一直显示旧字节（上游用 immutable 是因为它带版本号）。这里如实禁止缓存。
        .header("Cache-Control", "private, no-store");
    if let Some(mime) = reply.content_type {
        builder = builder.header("Content-Type", mime);
    }
    if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
        builder = builder.header("Content-Length", value);
    }
    let body: Cow<'static, [u8]> = if head { Cow::Owned(Vec::new()) } else { Cow::Owned(reply.body) };
    if let Ok(next) = builder.body(body) {
        *response = next;
    }
    // 构造失败保持原响应：宁可这张图不显示，也不把请求线程打崩。
}

/// `on_web_resource_request` 的入口：只接管 `/api/file`，其余请求原样放行。
///
/// 全程不 `unwrap`：`try_state` 取不到、编码非法、内部 panic 一律收敛成 500 短码。
pub fn handle_request(
    app: &tauri::AppHandle,
    request: &tauri::http::Request<Vec<u8>>,
    response: &mut tauri::http::Response<Cow<'static, [u8]>>,
) {
    if request.uri().path() != ROUTE {
        return;
    }
    let head = request.method() == tauri::http::Method::HEAD;
    if !head && request.method() != tauri::http::Method::GET {
        write(response, fail(405, "method not allowed"), false);
        return;
    }
    // 钩子跑在 WebView2 的请求线程上：这里的 panic 表现是白屏，谁也无法归因。
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(state) = app.try_state::<AppState>() else {
            return fail(500, "state unavailable");
        };
        match path_param(request.uri().query()) {
            Ok(decoded) => serve(&state.media_roots(), decoded.as_deref()),
            Err(()) => fail(400, "invalid path encoding"),
        }
    }))
    .unwrap_or_else(|_| fail(500, "internal error"));
    write(response, reply, head);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTempDir;

    /// 1×1 PNG（最小合法字节，足够证明"读的是原字节"）。
    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00,
    ];

    fn fixture() -> (TestTempDir, PathBuf) {
        let dir = TestTempDir::new("media");
        std::fs::write(dir.join("a.png"), PNG).expect("写 png");
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn serves_bytes_from_a_registered_root() {
        let (dir, root) = fixture();
        let reply = serve(&[root], Some(&dir.join("a.png").display().to_string()));
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, Some("image/png"));
        assert_eq!(reply.body.as_slice(), PNG);
    }

    #[test]
    fn rejects_parent_components_and_relative_paths() {
        let (_dir, root) = fixture();
        assert_eq!(serve(std::slice::from_ref(&root), Some("../a.png")).status, 400);
        assert_eq!(serve(&[root], Some("a.png")).status, 400, "相对路径必须拒，不替调用方补根");
        // 绝对但不存在、又不属于任何登记根：404（不是 400——形状是合法的）。
        assert_eq!(serve(&[], Some("C:\\definitely-missing\\a.png")).status, 404);
    }

    #[test]
    fn rejects_unc_control_characters_and_overlong_paths() {
        assert_eq!(serve(&[], Some("\\\\server\\share\\a.png")).status, 400);
        assert_eq!(serve(&[], Some("C:\\a\u{0}b.png")).status, 400);
        let long = format!("C:\\{}.png", "a".repeat(MAX_PATH_BYTES));
        assert_eq!(serve(&[], Some(&long)).status, 400);
        assert_eq!(serve(&[], Some(&long)).body.as_slice(), b"path too long");
    }

    #[test]
    fn existing_path_outside_roots_is_forbidden_and_missing_is_not_found() {
        let (dir, root) = fixture();
        let outside = dir.join("outside.png");
        std::fs::write(&outside, PNG).expect("写根外文件");
        let reply = serve(&[root.join("nested")], Some(&outside.display().to_string()));
        assert_eq!(reply.status, 403);
        assert_eq!(reply.body.as_slice(), b"outside workspace");

        let missing = dir.join("ghost.png");
        assert_eq!(serve(&[root], Some(&missing.display().to_string())).status, 404);
    }

    #[test]
    fn directories_and_symlinks_are_refused_with_a_reverse_control() {
        let (dir, root) = fixture();
        // 反向对照：真文件必须 200，否则"目录/链接被拒"没有意义。
        assert_eq!(serve(std::slice::from_ref(&root), Some(&dir.join("a.png").display().to_string())).status, 200);
        // 目录即使带媒体扩展名也必须 403：白名单先过，文件类型由 lstat 兜底。
        let nested = dir.join("nested.png");
        std::fs::create_dir_all(&nested).expect("建目录");
        assert_eq!(serve(std::slice::from_ref(&root), Some(&nested.display().to_string())).status, 403);

        let link = dir.join("link.png");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(dir.join("a.png"), &link).is_ok();
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(dir.join("a.png"), &link).is_ok();
        if made {
            // 与文件工具同口径：`resolve_in_workspace` 先 canonicalize 再判边界，
            // 所以"链到根内"照常读到目标字节（`read_file` 也是这样），
            // "链到根外"才会被边界判据拦下。下面两条构成本项的反向对照。
            assert_eq!(serve(std::slice::from_ref(&root), Some(&link.display().to_string())).status, 200);

            let outside = TestTempDir::new("media-link-outside");
            std::fs::write(outside.join("secret.png"), PNG).expect("写根外目标");
            let escaping = dir.join("escaping.png");
            #[cfg(windows)]
            let made_escaping =
                std::os::windows::fs::symlink_file(outside.join("secret.png"), &escaping).is_ok();
            #[cfg(not(windows))]
            let made_escaping =
                std::os::unix::fs::symlink(outside.join("secret.png"), &escaping).is_ok();
            if made_escaping {
                let reply = serve(&[root], Some(&escaping.display().to_string()));
                assert_eq!(reply.status, 403);
                assert_eq!(reply.body.as_slice(), b"outside workspace");
            }
        }
    }

    #[test]
    fn non_media_extensions_are_refused_before_touching_disk() {
        let (dir, root) = fixture();
        // 超限的 txt 仍应回 415 而不是 413：证明没有走到大小/读取分支。
        let big = dir.join("big.txt");
        let file = std::fs::File::create(&big).expect("建大文件");
        file.set_len(MAX_MEDIA_BYTES + 1).expect("设长度");
        assert_eq!(serve(std::slice::from_ref(&root), Some(&big.display().to_string())).status, 415);
        assert_eq!(serve(&[root], Some(&dir.join("x.exe").display().to_string())).status, 415);
    }

    #[test]
    fn oversized_media_is_refused_by_metadata() {
        let (dir, root) = fixture();
        let big = dir.join("big.png");
        let file = std::fs::File::create(&big).expect("建大图");
        file.set_len(MAX_MEDIA_BYTES + 1).expect("设长度");
        let reply = serve(&[root], Some(&big.display().to_string()));
        assert_eq!(reply.status, 413);
        assert_eq!(reply.body.as_slice(), b"file exceeds byte limit");
    }

    #[test]
    fn second_root_and_uppercase_extension_are_supported() {
        let first = TestTempDir::new("media-r1");
        let second = TestTempDir::new("media-r2");
        let file = second.join("b.JPG");
        std::fs::write(&file, PNG).expect("写 jpg");
        let reply = serve(
            &[first.path().to_path_buf(), second.path().to_path_buf()],
            Some(&file.display().to_string()),
        );
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type, Some("image/jpeg"));
    }

    #[test]
    fn svg_and_pdf_are_served_with_sandbox_friendly_types() {
        let dir = TestTempDir::new("media-svg");
        let svg = dir.join("icon.svg");
        std::fs::write(&svg, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").expect("写 svg");
        let pdf = dir.join("doc.pdf");
        std::fs::write(&pdf, b"%PDF-1.7\n").expect("写 pdf");
        let root = dir.path().to_path_buf();
        assert_eq!(serve(std::slice::from_ref(&root), Some(&svg.display().to_string())).content_type, Some("image/svg+xml"));
        assert_eq!(serve(&[root], Some(&pdf.display().to_string())).content_type, Some("application/pdf"));
    }

    #[test]
    fn missing_or_empty_path_is_a_client_error() {
        assert_eq!(serve(&[], None).status, 400);
        assert_eq!(serve(&[], Some("")).body.as_slice(), b"missing path");
    }

    #[test]
    fn query_decoding_happens_exactly_once() {
        let decoded = path_param(Some("load=7&path=D%3A%5Cws%5Ca%2520b.png")).expect("可解码");
        assert_eq!(decoded.as_deref(), Some("D:\\ws\\a%20b.png"), "二次解码会把 %20 变成空格");
        assert_eq!(path_param(Some("load=1")).expect("无 path"), None);
        // `%` 后跟非十六进制时 percent-decoding 原样保留（不是错误）；真正会失败的是
        // 解出来不是合法 UTF-8 的字节序列——那必须转成 400，不能当成路径去解析。
        assert!(path_param(Some("path=%FF%FE")).is_err());
    }

    #[test]
    fn roots_include_the_default_and_every_named_workspace() {
        let mut config = AppConfig::default();
        config.workspace.root = String::from("D:\\ws\\default");
        config.workspaces = vec![
            crate::config::WorkspaceProfile { id: String::from("w1"), path: String::from("D:\\ws\\one"), title: String::new() },
            crate::config::WorkspaceProfile { id: String::from("w2"), path: String::from("  "), title: String::new() },
            crate::config::WorkspaceProfile { id: String::from("w3"), path: String::from("D:\\ws\\default"), title: String::new() },
        ];
        let roots = media_roots_of(&config, Path::new("C:\\data"));
        assert_eq!(roots, vec![PathBuf::from("D:\\ws\\default"), PathBuf::from("D:\\ws\\one")]);
    }

    /// 额外工作区指到数据根/`data\secrets` 时不能成为媒体读取通道。
    /// 反向对照：去掉判据后 `data\secrets` 会出现在根集合里（该断言必须红）。
    #[test]
    fn roots_skip_workspaces_inside_the_data_root() {
        let base = crate::test_support::TestTempDir::new("media-roots-gate");
        let data_root = base.join("data");
        let secrets = data_root.join("secrets");
        let outside = base.join("outside");
        let profile = |path: &std::path::Path| crate::config::WorkspaceProfile {
            id: String::from("w"),
            path: path.display().to_string(),
            title: String::new(),
        };
        let mut config = AppConfig::default();
        config.workspace.root = secrets.display().to_string();
        config.workspaces = vec![profile(&secrets), profile(&data_root), profile(&outside)];

        let roots = media_roots_of(&config, &data_root);
        assert_eq!(
            roots,
            vec![data_root.join(crate::config::WORKSPACE_DIR), outside],
            "被拒的根既不能当默认，也不能作为额外工作区进入媒体面"
        );
    }
}
