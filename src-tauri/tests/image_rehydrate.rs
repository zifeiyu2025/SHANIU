//! 重开会话时历史图片的字节回灌（spec §5 / §5.1 / §5.2）。
//!
//! 档案只存引用，字节留在 `attachments/<owner_session>/`。两条硬要求：
//! 1. 重开档案能按引用把字节找回来——否则模型看到的是「图片已不可用」占位；
//! 2. 定位必须按 `ImageRef.path`：`fork_session` 复制出的引用仍指向父会话目录，
//!    按当前会话 id 拼路径在「分支 + 带图」时必然落空。

use std::path::Path;

use base64::Engine as _;
use shaniu_lib::attachments;
use shaniu_lib::images::{self, ImageRef};
use shaniu_lib::model_client::{HistoryItem, Role};
use shaniu_lib::rollout::{self, ConfigSnapshot, RolloutItem, RolloutWriter};
use shaniu_lib::test_support::TestTempDir;

fn meta(agent: &str, ws: &Path) -> RolloutItem {
    RolloutItem::SessionMeta {
        session_id: agent.into(),
        agent_id: agent.into(),
        parent_id: None,
        created_at: 1_700_000_000_000,
        config_snapshot: ConfigSnapshot {
            model: "m".into(),
            sandbox: "WorkspaceWrite".into(),
            approval: "AutoReadOnly".into(),
            workspace_root: ws.display().to_string(),
            ..Default::default()
        },
    }
}

fn image_ref(owner: &str, receipt: &str, bytes: &[u8]) -> ImageRef {
    ImageRef {
        path: format!("attachments/{owner}/{receipt}.bin"),
        sha256: images::sha256_hex(bytes),
        width: 2,
        height: 3,
        tokens: 1,
        mime: "image/png".into(),
        attachment_id: Some(receipt.to_string()),
        bytes: Some(bytes.len() as u64),
    }
}

/// 落一份只含一条带图用户消息的档案，返回档案路径。
fn write_archive(dir: &Path, agent: &str, image: ImageRef) -> std::path::PathBuf {
    let rollout_dir = dir.join("rollouts");
    std::fs::create_dir_all(&rollout_dir).unwrap();
    let writer = RolloutWriter::create(&rollout_dir, &meta(agent, dir)).unwrap();
    writer
        .append(&RolloutItem::ResponseItem {
            item: HistoryItem::ImageMessage {
                role: Role::User,
                text: "看图".into(),
                images: vec![image],
            },
            at_ms: 1,
        })
        .unwrap();
    rollout_dir.join(format!("{agent}.jsonl"))
}

fn store_image(dir: &Path, owner: &str, bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    attachments::store(dir, owner, "shot.png", "image/png", &encoded, Some(2), Some(3))
        .unwrap()
        .receipt_id
}

fn archived_refs(history: &[HistoryItem]) -> Vec<ImageRef> {
    history
        .iter()
        .filter_map(|item| match item {
            HistoryItem::ImageMessage { images, .. } => Some(images.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn reopening_restores_bytes_from_attachment_dir() {
    let dir = TestTempDir::new("rehydrate-resume");
    // 每条用例用独有内容：进程级 `image_store` 可能被同二进制里的其他用例预热。
    let bytes = b"\x89PNG-reopen-case-unique".to_vec();
    let receipt = store_image(dir.path(), "a1", &bytes);
    let image = image_ref("a1", &receipt, &bytes);
    let path = write_archive(dir.path(), "a1", image.clone());

    let resumed = rollout::resume::resume(&path).unwrap();
    let refs = archived_refs(&resumed.history);
    assert_eq!(refs.len(), 1, "档案里应能读回带图消息");
    // 前置断言：缓存里还没有这张图，否则下面的「还原」可能是别处放进去的。
    assert!(images::image_store::data_url(&image.sha256).is_none());

    let (restored, failed) = shaniu_lib::rehydrate_attachments(dir.path(), &refs);
    assert_eq!((restored, failed), (1, 0));
    assert!(
        images::image_store::data_url(&image.sha256).is_some(),
        "重开档案后必须能按引用取回字节"
    );
}

#[test]
fn fork_restores_bytes_via_parent_path_not_current_session() {
    let dir = TestTempDir::new("rehydrate-fork");
    let bytes = b"\x89PNG-fork-case-unique".to_vec();
    let receipt = store_image(dir.path(), "parent", &bytes);
    let image = image_ref("parent", &receipt, &bytes);
    let parent_path = write_archive(dir.path(), "parent", image.clone());

    let forked = rollout::resume::fork(
        &parent_path,
        &dir.path().join("rollouts"),
        "child",
        2,
    )
    .unwrap();
    let resumed = rollout::resume::resume(&forked.path).unwrap();
    let refs = archived_refs(&resumed.history);
    assert_eq!(refs.len(), 1, "分叉档案应保留父会话的带图消息");

    // 反向对照一：按「当前会话 id」直接读，必须失败（分支 id ≠ 父会话 id）。
    assert!(
        attachments::read_full(dir.path(), "child", &receipt).is_err(),
        "按当前会话拼路径不该读到父会话的附件；这里能读到说明用例已失去分辨力"
    );
    // 反向对照二：把「按当前会话拼路径」交给回灌，必须是 0 还原 1 失败。
    let wrong = images::rehydrate(&refs, |image| {
        attachments::read_full(
            dir.path(),
            "child",
            image.attachment_id.as_deref().unwrap(),
        )
        .ok()
        .map(|(_media_type, bytes)| bytes)
    });
    assert_eq!(wrong, (0, 1), "按当前会话 id 拼路径的写法必须红");

    // 生产路径：按 `ImageRef.path` 定位 → 父会话目录，必须成功。
    let (restored, failed) = shaniu_lib::rehydrate_attachments(dir.path(), &refs);
    assert_eq!((restored, failed), (1, 0));
    assert!(images::image_store::data_url(&image.sha256).is_some());
}
