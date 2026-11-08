//! `session_export` 的行为测试：路径安全、ZIP 结构、压缩生效、失败不留残骸。
//!
//! 断言的都是**可观察行为**（能否解压、压缩率、目标路径有没有文件），不是常量快照。

use super::*;
use std::io::Read;

/// 每个用例独立目录，避免并行测试互踩。
fn temp_dir(tag: &str) -> crate::test_support::TestTempDir {
    crate::test_support::TestTempDir::new(&format!("sx-{tag}"))
}

fn text_entry(path: &str, content: &str) -> ExportEntry {
    ExportEntry {
        path: path.to_string(),
        content: Some(content.to_string()),
        source_path: None,
    }
}

#[test]
fn rejects_path_traversal_and_absolute_paths() {
    // 每条都是真实会被解压器利用或产生歧义的形状。
    for bad in [
        "../evil.jsonl",
        "a/../../evil",
        "/abs/session.jsonl",
        "C:/windows/system32/x",
        "sub\\agent\\session.jsonl",
        "a//b",
        "",
        "a/./b",
        "a/\u{0007}b",
    ] {
        assert!(
            validate_entry_path(bad).is_err(),
            "应当拒绝非法条目路径: {bad:?}"
        );
    }
}

#[test]
fn accepts_canonical_layout_paths() {
    for good in [
        "session.v4.jsonl",
        "subagents/01H8XYZ/session.v4.jsonl",
        "media/abc123.png",
        "files/ab/cdef0123/report.pdf",
    ] {
        assert!(
            validate_entry_path(good).is_ok(),
            "应当接受合法条目路径: {good:?}"
        );
    }
}

#[test]
fn writes_readable_zip_with_deflate() {
    let dir = temp_dir("zip");
    let dest = dir.join("session.v4.zip");
    // 高重复文本：DEFLATE 后必然远小于原文，能区分 Store 与 Deflated。
    let body = "hello world\n".repeat(4000);
    let raw_len = body.len() as u64;

    let report = write_zip(&dest, &[text_entry("session.v4.jsonl", &body)]).expect("写 ZIP");
    assert_eq!(report.entries, 1);
    assert_eq!(report.path, dest.to_string_lossy());
    assert_eq!(report.bytes, fs::metadata(&dest).unwrap().len());
    assert!(
        !dest.with_file_name("session.v4.zip.part").exists(),
        "成功后不应残留 .part"
    );

    let mut archive = zip::ZipArchive::new(File::open(&dest).unwrap()).expect("解压");
    assert_eq!(archive.len(), 1);
    {
        let mut entry = archive.by_index(0).expect("第一条");
        assert_eq!(entry.name().expect("条目名").as_ref(), "session.v4.jsonl");
        assert_eq!(entry.compression(), CompressionMethod::Deflated, "必须走 DEFLATE");
        let mut text = String::new();
        entry.read_to_string(&mut text).expect("读回");
        assert_eq!(text, body, "内容必须逐字往返");
    }

    // 压缩真的生效：归档体积明显小于原文（含 ZIP 头开销仍应远低于原文）。
    assert!(
        report.bytes < raw_len / 2,
        "压缩未生效: zip={} raw={raw_len}",
        report.bytes
    );

}

#[test]
fn streams_byte_entries_from_source_file() {
    let dir = temp_dir("bytes");
    let source = dir.join("attachment.bin");
    let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
    fs::write(&source, &payload).unwrap();
    let dest = dir.join("out.zip");

    write_zip(
        &dest,
        &[
            text_entry("session.v4.jsonl", "header\n"),
            ExportEntry {
                path: "media/abc.bin".to_string(),
                content: None,
                source_path: Some(source.to_string_lossy().into_owned()),
            },
        ],
    )
    .expect("写 ZIP");

    let mut archive = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
    let mut entry = archive.by_name("media/abc.bin").expect("附件条目");
    let mut round = Vec::new();
    entry.read_to_end(&mut round).unwrap();
    assert_eq!(round, payload, "字节条目必须逐字节往返");
    drop(entry);

}

#[test]
fn refuses_bad_entries_before_touching_disk() {
    let dir = temp_dir("bad");
    let dest = dir.join("out.zip");

    // 空清单
    assert!(write_zip(&dest, &[]).is_err());

    // 同时给两种内容
    let both = ExportEntry {
        path: "session.v4.jsonl".into(),
        content: Some("x".into()),
        source_path: Some("y".into()),
    };
    assert!(write_zip(&dest, &[both]).is_err());

    // 路径穿越
    assert!(write_zip(&dest, &[text_entry("../x.jsonl", "x")]).is_err());

    // 源文件不存在
    let missing = ExportEntry {
        path: "media/x.bin".into(),
        content: None,
        source_path: Some(dir.join("nope.bin").to_string_lossy().into_owned()),
    };
    assert!(write_zip(&dest, &[missing]).is_err());

    assert!(!dest.exists(), "任何校验/写入失败都不得留下目标文件");
    assert!(
        !dir.join("out.zip.part").exists(),
        "失败必须清理临时文件"
    );

}

#[test]
fn destination_failure_does_not_leave_partial_file() {
    let dir = temp_dir("nodest");
    // 目标目录不存在：创建临时文件就会失败，不应在任何地方留下残骸。
    let dest = dir.join("missing-subdir").join("out.zip");
    let err = write_zip(&dest, &[text_entry("session.v4.jsonl", "x")]).unwrap_err();
    assert!(err.contains("临时文件"), "错误应指出来源: {err}");
    assert!(!dir.join("missing-subdir").exists());
}
