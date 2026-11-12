//! 「只约束写」模型的写身份派生（docs/16 §1.2）。
//!
//! 受限令牌的 restricting 列表需要两个写 SID：工作区写 SID 与 temp 写 SID。
//! 二者都由**规范化后的绝对路径**经 SHA-256 确定性派生（对齐 dsh
//! `sandbox-windows-acl/src/workspace-sid.ts:35-53`）：
//!
//! - 工作区：`S-1-4-x-y`
//! - temp  ：`S-1-4-x-y-1`（第三位做域分隔，避免与两位的工作区 SID 撞形状）
//! - `x`/`y` 均为 30 位正整数（`digest % (2^30 - 1) + 1`）
//!
//! 为什么确定性而不是落盘账本：路径 → SID 是纯函数，跨进程/跨重启/重装都一致，
//! 不会出现「账本与真实 DACL 漂移」；ACE 只在首次物化，之后靠精确命中跳过。
//!
//! **输入必须规范化**：`acl::normalize` 收敛大小写别名、`\\?\` 前缀与 junction，
//! 否则同一目录的两种拼写会派生出两个 SID，等于开两个口子。

use std::path::Path;

use sha2::{Digest, Sha256};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows::Win32::Security::PSID;

use super::acl;
use super::windows_impl::wide;

/// 由 `ConvertStringSidToSidW` 分配、`LocalFree` 释放的 SID（含文本形式，便于审计）。
pub struct OwnedSid {
    raw: PSID,
    text: String,
}

// SID 缓冲区在生命周期内不可变；Win32 的 SID 操作线程安全。
unsafe impl Send for OwnedSid {}
unsafe impl Sync for OwnedSid {}

impl OwnedSid {
    pub fn as_psid(&self) -> PSID {
        self.raw
    }

    /// `S-1-4-x-y[-1]` 文本。审计与日志用它，不要另做格式化。
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Drop for OwnedSid {
    fn drop(&mut self) {
        if !self.raw.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.raw.0)));
            }
        }
    }
}

/// 工作区写 SID：`sha256(规范化工作区路径)` → `S-1-4-x-y`。
pub fn workspace_write_sid(workspace_root: &Path) -> Result<OwnedSid, String> {
    let canonical = acl::normalize(workspace_root);
    derive(canonical.to_string_lossy().as_bytes(), None).map_err(|error| {
        format!(
            "派生工作区写 SID 失败（{}）：{error}",
            canonical.display()
        )
    })
}

/// temp 写 SID：`sha256("temp\0" + 规范化 temp 路径)` → `S-1-4-x-y-1`。
pub fn temp_write_sid(temp_dir: &Path) -> Result<OwnedSid, String> {
    let canonical = acl::normalize(temp_dir);
    let mut input = b"temp\0".to_vec();
    input.extend_from_slice(canonical.to_string_lossy().as_bytes());
    derive(&input, Some(1))
        .map_err(|error| format!("派生 temp 写 SID 失败（{}）：{error}", canonical.display()))
}

fn derive(input: &[u8], third: Option<u32>) -> Result<OwnedSid, String> {
    let digest = Sha256::digest(input);
    let first = u32::from_le_bytes(digest[0..4].try_into().expect("SHA-256 摘要至少 8 字节"))
        % (2u32.pow(30) - 1)
        + 1;
    let second = u32::from_le_bytes(digest[4..8].try_into().expect("SHA-256 摘要至少 8 字节"))
        % (2u32.pow(30) - 1)
        + 1;
    let text = match third {
        Some(domain) => format!("S-1-4-{first}-{second}-{domain}"),
        None => format!("S-1-4-{first}-{second}"),
    };
    let text_wide = wide(&text);
    let mut raw = PSID(std::ptr::null_mut());
    unsafe { ConvertStringSidToSidW(PCWSTR(text_wide.as_ptr()), &mut raw) }
        .map_err(|error| format!("SID 文本「{text}」转二进制失败: {error}"))?;
    Ok(OwnedSid { raw, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_sid_is_deterministic_for_the_same_path() {
        let root = std::env::temp_dir();
        let first = workspace_write_sid(&root).expect("派生应成功");
        let second = workspace_write_sid(&root).expect("派生应成功");
        assert_eq!(first.text(), second.text(), "同一路径必须派生出同一个写 SID");
        assert!(
            first.text().starts_with("S-1-4-"),
            "工作区写 SID 必须是 S-1-4-x-y：{}",
            first.text()
        );
        assert_eq!(
            first.text().split('-').count(),
            5,
            "工作区 SID 应有两位子权威：{}",
            first.text()
        );
    }

    #[test]
    fn temp_sid_is_domain_separated_and_distinct_from_workspace_sid() {
        let root = std::env::temp_dir();
        let workspace = workspace_write_sid(&root).expect("派生应成功");
        let temp = temp_write_sid(&root).expect("派生应成功");
        assert_ne!(workspace.text(), temp.text(), "同一目录的工作区 SID 与 temp SID 不得相同");
        assert!(
            temp.text().ends_with("-1") && temp.text().split('-').count() == 6,
            "temp SID 应有第三位域分隔：{}",
            temp.text()
        );
    }

    #[test]
    fn different_paths_derive_different_sids() {
        let first = workspace_write_sid(Path::new("C:\\shaniu-sid-a")).expect("派生应成功");
        let second = workspace_write_sid(Path::new("C:\\shaniu-sid-b")).expect("派生应成功");
        assert_ne!(first.text(), second.text(), "不同路径必须派生出不同的写 SID");
    }
}
