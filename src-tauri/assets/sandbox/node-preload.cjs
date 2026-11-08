// 仅在 AppContainer 里作为 `--require` 预加载：兼容 node 的 JS realpath。
//
// `fs.realpathSync` 的 JS 实现会逐级 lstat 到**盘符根**；AppContainer 令牌读不到
// 数据盘根（如 `D:\`，归 SYSTEM，非提权改不了其 DACL），于是对合法文件的 realpath
// 也抛 EPERM。`realpathSync.native` 直接按目标句柄解析，不需要祖先目录属性。
// 这里只在 EPERM/EACCES 时回退到 native：返回值仍是同一个规范化路径，文件系统
// 权限边界不变（真正的读写在目标路径上照常按 ACL 判定）。
'use strict';

const fs = require('node:fs');
const original = fs.realpathSync;

if (!original.__shaniuSandboxPatched) {
  const patched = function realpathSync(target, options) {
    try {
      return original.call(this, target, options);
    } catch (error) {
      if (error && (error.code === 'EPERM' || error.code === 'EACCES')) {
        try {
          // 注意用 original.native：patched 上没有 native 属性。
          return original.native(target, options);
        } catch {
          // 落回原始错误：native 也失败时保留用户代码原本会看到的报错。
        }
      }
      throw error;
    }
  };
  patched.native = original.native;
  patched.__shaniuSandboxPatched = true;
  fs.realpathSync = patched;
  // ESM 的 `import { realpathSync } from 'fs'` 是 CJS 导出快照，必须显式同步。
  require('node:module').syncBuiltinESMExports();
}
