# 证据 2026-11-13：写约束令牌下 node 管道 stdio 的失败对象

本目录回答 `docs/16` §10.7 的两个问题：**node 的 `EPERM: spawnSync` 到底是哪一个 Win32 调用被拒**，
以及**往令牌 default DACL 里补写 SID 能不能修**。它是 `docs/16` §10.7「已定位结案」一节的依据。

## 方法

没有装 Process Monitor，改用等价手段：在**与 `src-tauri/src/sandbox/windows_impl.rs:205-245`
同形状**的受限令牌下起一个探针进程，由它逐条调用 libuv `1.52.1` 的 stdio 管道路径
（`src/win/pipe.c:209-345`：`CreateNamedPipeA(NULL SD)` → `CreateFileA` 客户端 → `ConnectNamedPipe`），
每一步打印 `GetLastError`，并在同一令牌下跑 node 自己的 `execSync`。

libuv 把这段路径的 Win32 错误直接返回，`uv_translate_sys_error(ERROR_ACCESS_DENIED)` = `UV_EPERM`
（libuv `src/win/error.c:158`），所以 node 侧看到的是 `EPERM` 而不是 `EACCES`——这也是不能凭
错误码反推被拒对象的原因。node 打印的 `errno: -4048` 正是 `UV_EPERM`，与上面的映射一致。

令牌形状（`probe.py` 复刻）：

- `flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`
- restricting SID 列表 = `[logon, Everyone, 写 SID]`，顺序与 `windows_impl.rs:210-225` 一致
- default DACL 由 `--mode` 切换（见下表）；「写 SID」按 `write_sid.rs` 的 `S-1-4-x-y` 公式派生

## 结果矩阵（`run-env.log`：Windows 10.0.26300.9818 / node 26.13.1 / libuv 1.52.1 / pnpm 12.16.1，Medium IL 普通用户）

| default DACL 形状 | node `execSync`（管道） | node `execSync(stdio:'inherit')` | 命名管道 `CreateNamedPipeA(NULL SD)` | 命名管道客户端 `CreateFileA`（RW+ATTRS+WRITE_DAC） | 匿名管道 `CreatePipe(NULL SD)` |
|---|---|---|---|---|---|
| `none`（继承用户默认 DACL，即 dsh 注释描述的起点） | **EPERM** | 成功 | 成功 | **拒绝(5)** | **拒绝(5)** |
| `current`（本仓现状：logon 全权 + OWNER RIGHTS 读控制，替换式） | **EPERM** | 成功 | 成功 | **拒绝(5)** | 成功 |
| `cur+write`（本仓现状再并入写 SID `FILE_ALL_ACCESS`） | **EPERM** | 成功 | 成功 | **拒绝(5)** | 成功 |
| `dsh`（继承的现有 DACL 上合并写 SID `FILE_ALL_ACCESS`） | **EPERM** | 成功 | 成功 | **拒绝(5)** | 成功 |
| `cur+owner`（诊断：把令牌 owner 改成写 SID） | **EPERM** | 成功 | 成功 | **拒绝(5)** | 成功 |

`cur+owner` 的 `SetTokenInformation(TokenOwner)` 直接失败：Win32 **1307 (ERROR_PRIVILEGE_NOT_HELD)**，
即需要 `SeRestorePrivilege`，本档不额外加特权，此路不通。

分权利粒度（`current` 模式，同一个命名管道，逐项重开）：

| 客户端请求的权限 | 结果 |
|---|---|
| `GENERIC_READ` | 成功(0) |
| `FILE_READ_ATTRIBUTES` | 成功(0) |
| `GENERIC_WRITE` | **拒绝(5)** |
| `WRITE_DAC` | **拒绝(5)** |
| `GENERIC_READ|GENERIC_WRITE|FILE_READ_ATTRIBUTES|FILE_WRITE_ATTRIBUTES|WRITE_DAC` | **拒绝(5)** |

## 关键原始数据（各 `run-*.log` 第 2-4 行）

四种 default DACL 形状下，命名管道的 DACL **逐字节相同**，不随令牌 default DACL 变化：

```
D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-3226398052-62638092-3539548929-1001)(A;;FR;;;WD)(A;;FR;;;AN)
```

即 SYSTEM/Administrators/创建者本人全权，Everyone/Anonymous 只读。受限令牌的写检查只看 restricting
SID，而列表里的 Everyone 在该 DACL 中只有 `FR`，因此**写**打开被拒、**读**打开放行。

匿名管道则相反，DACL 确实来自令牌 default DACL：

```
current : D:(A;;FA;;;S-1-5-5-0-675595)(A;;RC;;;OW)
cur+write: D:(A;;FA;;;S-1-5-5-0-675595)(A;;RC;;;OW)(A;;FA;;;S-1-4-711010719-218466395)
dsh     : D:(A;;FA;;;S-1-4-711010719-218466395)(A;;FA;;;S-1-5-21-...-1001)(A;;FA;;;SY)(A;;0x1200a9;;;S-1-5-5-0-675595)
```

（`S-1-5-5-0-675595` 是本次登录会话 SID，即 logon SID。）这解释了为什么 dsh 的 default DACL 合并
对**匿名管道**有效，却对 libuv 的**命名管道**无效。

`none` 模式另有一条副证：受限进程连 `OpenProcessToken(TOKEN_QUERY|...)` 都被拒（错误 5），
因为继承来的 default DACL 里没有任何 restricting SID。本仓的替换式 default DACL 已经避免了这类退化。

## 旁证：pnpm

| 命令（同一受限令牌） | 结果 |
|---|---|
| `pnpm.exe --version`（`C:\Users\jun\AppData\Roaming\npm\node_modules\pnpm\pnpm.exe`） | 成功，输出 `12.16.1`（`run-pnpm-exe.log`） |
| `node -e "execSync('pnpm --version')"` | 不适用：execSync 第一步就在命名管道客户端被拒（同 `run-current.log` 的 EPERM） |

即：工具链本体能在写约束档跑，**它用 libuv 管道再派生孙进程的那一步**才失败。

## 复跑

```powershell
python "$env:TEMP\shaniu-pipe-probe\probe.py" current
# 或直接跑本目录的副本：
python .\probe.py dsh
python .\probe.py current "`"C:\Users\jun\AppData\Roaming\npm\node_modules\pnpm\pnpm.exe`" --version"
```

输出每行以 `PROBE ` 开头，JSON 里 `win32_error` 就是该步的 `GetLastError`。
注意 `step=token restricted_sid_count` 那一行把计数写在了 `win32_error` 字段（历史命名，不是错误码）。

## 边界

- 只覆盖本机（10.0.26300.9818 / 26H2）与本令牌形状；没有在其它 Windows 版本上交叉验证。
  dsh 的 README 把它登记为平台限制（见 `docs/16` §10.7 的引用），与版本无关的说法目前只有本机证据。
- 探针进程是 `python.exe`，不是 node.exe；调用序列按 libuv 源码复刻。归属依据是
  `src/win/pipe.c:209-345` + `src/win/error.c:158`，不是运行时 hook。
- `run-none.log` 里匿名管道建不起来（错误 5），是「没有 restricting SID 的 default DACL」的退化表现，
  不代表本仓现状。
