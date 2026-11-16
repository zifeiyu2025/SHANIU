# D9 CI 修复记录（2026-11-16）

## 现象

`315b643` 的 CI run `43840673027`：`product-smoke` 通过，`gate` 的「后端测试」失败。

本地复跑 `cargo test`（改前）失败于：

```
tests/permission_min.rs:189: child_cannot_reach_tools_outside_its_allowlist
拒绝必须回填为策略拒绝，而不是用户拒绝
```

## 原因

D9 把策略拒绝的文案改成 `拒绝说明：type=policy_forbidden；...` + 人话，但人话里
丢了既有判据词「策略拒绝」，触发了 `permission_min` 的严格断言。该断言本身是
对的（策略拒绝不能伪装成用户拒绝），因此**不改用例**，只在两类策略拒绝的人话
开头补回「策略拒绝：」。

## 修复与验证

- 修改：`src-tauri/src/permission/mod.rs` 的 `DenialKind::PolicyForbidden` /
  `DenialKind::ToolNotInToolset` 人话前缀。
- `cargo test --test permission_min`：5 passed。
- `cargo test`：全绿（输出见同目录 `08-full-cargo-test.txt`）。
- 后续 CI 结论以修复提交的 run 为准。
