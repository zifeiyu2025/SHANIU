"""步骤 1 实测：在同一形状的「写约束」受限令牌下，逐条调用 libuv 的 stdio 管道路径。

libuv 1.52.1 `src/win/pipe.c:209-345` 的调用顺序：
  1) CreateNamedPipeA(name, access|FILE_FLAG_FIRST_PIPE_INSTANCE, ..., 0, 65536, 65536, 0, NULL)
  2) CreateFileA(name, client_access, 0, &sa, OPEN_EXISTING, FILE_FLAG_OVERLAPPED, NULL)
  3) ConnectNamedPipe(...)
libuv 把这里的 Win32 错误直接返回；`uv_translate_sys_error(ERROR_ACCESS_DENIED)` = UV_EPERM
（src/win/error.c:158），所以 node 侧看到的就是 `EPERM: spawnSync ...`。

令牌形状复刻本仓 windows_impl.rs：
  flags = DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED
  restricting = [logon, Everyone, write SID]
default DACL 由 --mode 决定：
  none      不改（受限令牌继承用户 default DACL）
  current   本仓现状：替换成 logon 全权 + OWNER RIGHTS 读控制
  cur+write 本仓现状 + 合并 write SID 的 FILE_ALL_ACCESS（候选修法）
  dsh       继承的现有 default DACL + 合并 write SID 的 FILE_ALL_ACCESS（dsh 形状）
"""

import ctypes
import ctypes.wintypes as wt
import hashlib
import json
import os
import shutil
import sys

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
advapi32 = ctypes.WinDLL("advapi32", use_last_error=True)

TOKEN_ASSIGN_PRIMARY = 0x0001
TOKEN_DUPLICATE = 0x0002
TOKEN_QUERY = 0x0008
TOKEN_ADJUST_DEFAULT = 0x0080
TOKEN_ADJUST_SESSIONID = 0x0100

TokenGroups = 2
TokenDefaultDacl = 6
TokenRestrictedSids = 11
SE_GROUP_LOGON_ID = 0xC0000000

WinWorldSid = 1
SECURITY_MAX_SID_SIZE = 68

DISABLE_MAX_PRIVILEGE = 0x1
LUA_TOKEN = 0x4
WRITE_RESTRICTED = 0x8

GENERIC_ALL = 0x10000000
GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
FILE_READ_ATTRIBUTES = 0x80
FILE_WRITE_ATTRIBUTES = 0x100
READ_CONTROL = 0x00020000
WRITE_DAC = 0x00040000
FILE_ALL_ACCESS = 0x001F01FF

PIPE_ACCESS_DUPLEX = 0x3
FILE_FLAG_OVERLAPPED = 0x40000000
FILE_FLAG_FIRST_PIPE_INSTANCE = 0x00080000
OPEN_EXISTING = 3
ERROR_PIPE_CONNECTED = 535

GRANT_ACCESS = 1
NO_MULTIPLE_TRUSTEE = 0
TRUSTEE_IS_SID = 0
TRUSTEE_IS_UNKNOWN = 0
STARTF_USESTDHANDLES = 0x100

TRUSTEE_W = type("TRUSTEE_W", (ctypes.Structure,), {})
TRUSTEE_W._fields_ = [
    ("pMultipleTrustee", ctypes.c_void_p),
    ("MultipleTrusteeOperation", ctypes.c_int),
    ("TrusteeForm", ctypes.c_int),
    ("TrusteeType", ctypes.c_int),
    ("ptstrName", ctypes.c_void_p),
]

EXPLICIT_ACCESS_W = type("EXPLICIT_ACCESS_W", (ctypes.Structure,), {})
EXPLICIT_ACCESS_W._fields_ = [
    ("grfAccessPermissions", wt.DWORD),
    ("grfAccessMode", ctypes.c_int),
    ("grfInheritance", wt.DWORD),
    ("Trustee", TRUSTEE_W),
]

SID_AND_ATTRIBUTES = type("SID_AND_ATTRIBUTES", (ctypes.Structure,), {})
SID_AND_ATTRIBUTES._fields_ = [("Sid", ctypes.c_void_p), ("Attributes", wt.DWORD)]

SECURITY_ATTRIBUTES = type("SECURITY_ATTRIBUTES", (ctypes.Structure,), {})
SECURITY_ATTRIBUTES._fields_ = [
    ("nLength", wt.DWORD),
    ("lpSecurityDescriptor", ctypes.c_void_p),
    ("bInheritHandle", wt.BOOL),
]

STARTUPINFOW = type("STARTUPINFOW", (ctypes.Structure,), {})
STARTUPINFOW._fields_ = [
    ("cb", wt.DWORD), ("lpReserved", wt.LPWSTR), ("lpDesktop", wt.LPWSTR), ("lpTitle", wt.LPWSTR),
    ("dwX", wt.DWORD), ("dwY", wt.DWORD), ("dwXSize", wt.DWORD), ("dwYSize", wt.DWORD),
    ("dwXCountChars", wt.DWORD), ("dwYCountChars", wt.DWORD), ("dwFillAttribute", wt.DWORD),
    ("dwFlags", wt.DWORD), ("wShowWindow", wt.WORD), ("cbReserved2", wt.WORD),
    ("lpReserved2", ctypes.c_void_p), ("hStdInput", wt.HANDLE), ("hStdOutput", wt.HANDLE),
    ("hStdError", wt.HANDLE),
]

PROCESS_INFORMATION = type("PROCESS_INFORMATION", (ctypes.Structure,), {})
PROCESS_INFORMATION._fields_ = [
    ("hProcess", wt.HANDLE), ("hThread", wt.HANDLE),
    ("dwProcessId", wt.DWORD), ("dwThreadId", wt.DWORD),
]

# ---------------- 原型 ----------------
kernel32.GetCurrentProcess.restype = wt.HANDLE
kernel32.GetStdHandle.argtypes = [wt.DWORD]
kernel32.GetStdHandle.restype = wt.HANDLE
kernel32.CloseHandle.argtypes = [wt.HANDLE]
kernel32.LocalFree.argtypes = [ctypes.c_void_p]
kernel32.CreateNamedPipeA.argtypes = [
    wt.LPCSTR, wt.DWORD, wt.DWORD, wt.DWORD, wt.DWORD, wt.DWORD, wt.DWORD, ctypes.c_void_p]
kernel32.CreateNamedPipeA.restype = wt.HANDLE
kernel32.CreateFileA.argtypes = [
    wt.LPCSTR, wt.DWORD, wt.DWORD, ctypes.POINTER(SECURITY_ATTRIBUTES), wt.DWORD, wt.DWORD, wt.HANDLE]
kernel32.CreateFileA.restype = wt.HANDLE
kernel32.ConnectNamedPipe.argtypes = [wt.HANDLE, ctypes.c_void_p]
kernel32.WaitForSingleObject.argtypes = [wt.HANDLE, wt.DWORD]
kernel32.GetExitCodeProcess.argtypes = [wt.HANDLE, ctypes.POINTER(wt.DWORD)]
kernel32.CreateProcessW.argtypes = [
    wt.LPCWSTR, wt.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, wt.BOOL, wt.DWORD,
    ctypes.c_void_p, wt.LPCWSTR, ctypes.POINTER(STARTUPINFOW), ctypes.POINTER(PROCESS_INFORMATION)]
kernel32.CreatePipe.argtypes = [
    ctypes.POINTER(wt.HANDLE), ctypes.POINTER(wt.HANDLE),
    ctypes.POINTER(SECURITY_ATTRIBUTES), wt.DWORD]

advapi32.OpenProcessToken.argtypes = [wt.HANDLE, wt.DWORD, ctypes.POINTER(wt.HANDLE)]
advapi32.GetTokenInformation.argtypes = [
    wt.HANDLE, ctypes.c_int, ctypes.c_void_p, wt.DWORD, ctypes.POINTER(wt.DWORD)]
advapi32.SetTokenInformation.argtypes = [wt.HANDLE, ctypes.c_int, ctypes.c_void_p, wt.DWORD]
advapi32.CreateRestrictedToken.argtypes = [
    wt.HANDLE, wt.DWORD, wt.DWORD, ctypes.c_void_p, wt.DWORD, ctypes.c_void_p,
    wt.DWORD, ctypes.c_void_p, ctypes.POINTER(wt.HANDLE)]
advapi32.CreateWellKnownSid.argtypes = [
    ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, ctypes.POINTER(wt.DWORD)]
advapi32.ConvertStringSidToSidW.argtypes = [wt.LPCWSTR, ctypes.POINTER(ctypes.c_void_p)]
advapi32.GetLengthSid.argtypes = [ctypes.c_void_p]
advapi32.GetLengthSid.restype = wt.DWORD
advapi32.CopySid.argtypes = [wt.DWORD, ctypes.c_void_p, ctypes.c_void_p]
advapi32.SetEntriesInAclW.argtypes = [
    wt.ULONG, ctypes.POINTER(EXPLICIT_ACCESS_W), ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p)]
advapi32.SetEntriesInAclW.restype = wt.DWORD
advapi32.CreateProcessAsUserW.argtypes = [
    wt.HANDLE, wt.LPCWSTR, wt.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, wt.BOOL, wt.DWORD,
    ctypes.c_void_p, wt.LPCWSTR, ctypes.POINTER(STARTUPINFOW), ctypes.POINTER(PROCESS_INFORMATION)]

ERROR_INVALID_HANDLE_VALUE = 2 ** 64 - 1


def check(ok, name):
    if not ok:
        raise OSError(ctypes.get_last_error(), f"{name} 失败")
    return ok


def report(step, ok, err=None):
    print("PROBE " + json.dumps({"step": step, "ok": bool(ok), "win32_error": err},
                                ensure_ascii=False), flush=True)


def open_current_token():
    token = wt.HANDLE()
    check(
        advapi32.OpenProcessToken(
            kernel32.GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY
            | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID,
            ctypes.byref(token),
        ),
        "OpenProcessToken",
    )
    return token


def token_groups(token_handle):
    needed = wt.DWORD()
    advapi32.GetTokenInformation(token_handle, TokenGroups, None, 0, ctypes.byref(needed))
    buf = ctypes.create_string_buffer(needed.value)
    check(
        advapi32.GetTokenInformation(token_handle, TokenGroups, buf, needed.value, ctypes.byref(needed)),
        "GetTokenInformation(TokenGroups)",
    )
    count = int.from_bytes(buf.raw[:4], "little")
    offset = 8  # TOKEN_GROUPS::Groups 在 x64 上偏移 8
    stride = ctypes.sizeof(SID_AND_ATTRIBUTES)
    return [SID_AND_ATTRIBUTES.from_buffer_copy(
        buf.raw[offset + i * stride: offset + (i + 1) * stride]) for i in range(count)]


def world_sid():
    buf = ctypes.create_string_buffer(SECURITY_MAX_SID_SIZE)
    size = wt.DWORD(SECURITY_MAX_SID_SIZE)
    check(advapi32.CreateWellKnownSid(WinWorldSid, None, buf, ctypes.byref(size)),
          "CreateWellKnownSid(WinWorldSid)")
    return buf


def sid_from_text(text):
    sid = ctypes.c_void_p()
    check(advapi32.ConvertStringSidToSidW(text, ctypes.byref(sid)), f"ConvertStringSidToSidW({text})")
    return sid


def derive_write_sid(path):
    digest = hashlib.sha256(path.lower().encode()).digest()
    first = int.from_bytes(digest[0:4], "little") % (2**30 - 1) + 1
    second = int.from_bytes(digest[4:8], "little") % (2**30 - 1) + 1
    return f"S-1-4-{first}-{second}"


def get_default_dacl(token_handle):
    needed = wt.DWORD()
    advapi32.GetTokenInformation(token_handle, TokenDefaultDacl, None, 0, ctypes.byref(needed))
    if needed.value == 0:
        raise OSError(ctypes.get_last_error(), "GetTokenInformation(TokenDefaultDacl) 查询长度失败")
    buf = ctypes.create_string_buffer(needed.value)
    check(advapi32.GetTokenInformation(token_handle, TokenDefaultDacl, buf, needed.value,
                                       ctypes.byref(needed)),
          "GetTokenInformation(TokenDefaultDacl)")
    return int.from_bytes(buf.raw[:8], "little")


def set_default_dacl(token_handle, entries, old_acl):
    arr = (EXPLICIT_ACCESS_W * len(entries))(*entries)
    new_acl = ctypes.c_void_p()
    rc = advapi32.SetEntriesInAclW(len(entries), arr, old_acl, ctypes.byref(new_acl))
    if rc != 0:
        raise OSError(rc, "SetEntriesInAclW 失败")
    info = ctypes.c_void_p(new_acl.value)
    check(advapi32.SetTokenInformation(token_handle, TokenDefaultDacl, ctypes.byref(info),
                                       ctypes.sizeof(info)),
          "SetTokenInformation(TokenDefaultDacl)")
    kernel32.LocalFree(new_acl)
    return new_acl.value


def explicit(sid, mask):
    addr = sid if isinstance(sid, int) else ctypes.cast(sid, ctypes.c_void_p).value
    return EXPLICIT_ACCESS_W(
        grfAccessPermissions=mask,
        grfAccessMode=GRANT_ACCESS,
        grfInheritance=0,
        Trustee=TRUSTEE_W(0, NO_MULTIPLE_TRUSTEE, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, addr),
    )


def build_restricted_token(mode, write_sid_text):
    base = open_current_token()
    logon = None
    for item in token_groups(base):
        if item.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID:
            length = advapi32.GetLengthSid(item.Sid)
            copy = ctypes.create_string_buffer(length)
            check(advapi32.CopySid(length, copy, item.Sid), "CopySid(logon)")
            logon = copy
            break
    if logon is None:
        raise RuntimeError("令牌里没有 logon SID")

    world = world_sid()
    write_sid = sid_from_text(write_sid_text)

    entries = (SID_AND_ATTRIBUTES * 3)(
        SID_AND_ATTRIBUTES(ctypes.cast(logon, ctypes.c_void_p), 0),
        SID_AND_ATTRIBUTES(ctypes.cast(world, ctypes.c_void_p), 0),
        SID_AND_ATTRIBUTES(write_sid, 0),
    )
    token = wt.HANDLE()
    check(
        advapi32.CreateRestrictedToken(
            base, DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
            0, None, 0, None, 3, entries, ctypes.byref(token)),
        "CreateRestrictedToken",
    )
    logon_ptr = ctypes.cast(logon, ctypes.c_void_p)

    if mode == "none":
        pass
    elif mode == "current":
        owner_rights = sid_from_text("S-1-3-4")
        set_default_dacl(token, [explicit(logon_ptr, GENERIC_ALL),
                                 explicit(owner_rights, READ_CONTROL)], None)
    elif mode == "cur+write":
        owner_rights = sid_from_text("S-1-3-4")
        set_default_dacl(token, [explicit(logon_ptr, GENERIC_ALL),
                                 explicit(owner_rights, READ_CONTROL),
                                 explicit(write_sid, FILE_ALL_ACCESS)], None)
    elif mode == "dsh":
        current = get_default_dacl(token)
        if not current:
            raise RuntimeError("受限令牌没有 default DACL 可合并")
        set_default_dacl(token, [explicit(write_sid, FILE_ALL_ACCESS)], current)
    elif mode == "cur+owner":
        owner_rights = sid_from_text("S-1-3-4")
        set_default_dacl(token, [explicit(logon_ptr, GENERIC_ALL),
                                 explicit(owner_rights, READ_CONTROL)], None)
        owner_info = ctypes.c_void_p(write_sid.value)
        ok = advapi32.SetTokenInformation(token, 4, ctypes.byref(owner_info),
                                          ctypes.sizeof(owner_info))
        print(json.dumps({"set_token_owner": bool(ok),
                          "win32_error": ctypes.get_last_error()}), flush=True)
    else:
        raise SystemExit(f"未知 mode: {mode}")

    kernel32.CloseHandle(base)
    return token


# ---------------- 子进程探针 ----------------

SE_KERNEL_OBJECT = 6
DACL_SECURITY_INFORMATION = 0x4

advapi32.GetSecurityInfo.argtypes = [
    wt.HANDLE, ctypes.c_int, wt.DWORD,
    ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p),
    ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p),
    ctypes.POINTER(ctypes.c_void_p)]
advapi32.GetSecurityInfo.restype = wt.DWORD
advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW.argtypes = [
    ctypes.c_void_p, wt.DWORD, wt.DWORD,
    ctypes.POINTER(wt.LPWSTR), ctypes.POINTER(wt.DWORD)]
advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW.restype = wt.BOOL


def pipe_sddl(handle):
    sd = ctypes.c_void_p()
    rc = advapi32.GetSecurityInfo(handle, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
                                  None, None, None, None, ctypes.byref(sd))
    if rc != 0:
        return f"<GetSecurityInfo 失败 {rc}>"
    text = wt.LPWSTR()
    size = wt.DWORD()
    ok = advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW(
        sd, 1, DACL_SECURITY_INFORMATION, ctypes.byref(text), ctypes.byref(size))
    out = text.value if ok else f"<SDDL 转换失败 {ctypes.get_last_error()}>"
    if ok:
        kernel32.LocalFree(text)
    kernel32.LocalFree(sd)
    return out


def try_pipe_pair(tag, client_access):
    """建一对管道并尝试客户端连接；返回 (server_ok, client_err, sddl)。"""
    name = f"\\\\.\\pipe\\shaniu-probe-{os.getpid()}-{tag}".encode()
    server = kernel32.CreateNamedPipeA(
        name, PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE, 0, 1, 65536, 65536, 0, None)
    server_err = ctypes.get_last_error()
    if server == ERROR_INVALID_HANDLE_VALUE:
        return False, server_err, None, None
    sddl = pipe_sddl(server)
    sa = SECURITY_ATTRIBUTES(ctypes.sizeof(SECURITY_ATTRIBUTES), None, True)
    client = kernel32.CreateFileA(name, client_access, 0, ctypes.byref(sa), OPEN_EXISTING,
                                  FILE_FLAG_OVERLAPPED, None)
    client_err = ctypes.get_last_error()
    if client != ERROR_INVALID_HANDLE_VALUE:
        kernel32.ConnectNamedPipe(server, None)
    return (client != ERROR_INVALID_HANDLE_VALUE), client_err, sddl, (server, client)


def child_probe(write_sid_text):
    try:
        token = open_current_token()
        needed = wt.DWORD()
        advapi32.GetTokenInformation(token, TokenRestrictedSids, None, 0, ctypes.byref(needed))
        buf = ctypes.create_string_buffer(needed.value)
        advapi32.GetTokenInformation(token, TokenRestrictedSids, buf, needed.value,
                                     ctypes.byref(needed))
        report("token restricted_sid_count", True, int.from_bytes(buf.raw[:4], "little"))
    except OSError as error:
        report("token(OpenProcessToken)", False, error.errno)

    full_access = (GENERIC_READ | GENERIC_WRITE | FILE_READ_ATTRIBUTES
                   | FILE_WRITE_ATTRIBUTES | WRITE_DAC)
    full_ok, err, sddl, handles = try_pipe_pair("full", full_access)
    report("pipe DACL (server SDDL)", True, sddl)
    report("CreateFileA(pipe client, GENERIC_RW|ATTRS|WRITE_DAC)", full_ok, err)

    read_end = wt.HANDLE()
    write_end = wt.HANDLE()
    if kernel32.CreatePipe(ctypes.byref(read_end), ctypes.byref(write_end), None, 0):
        report("anonymous pipe (CreatePipe, NULL SA) DACL", True, pipe_sddl(read_end))
        kernel32.CloseHandle(read_end)
        kernel32.CloseHandle(write_end)
    else:
        report("anonymous pipe (CreatePipe, NULL SA) DACL", False, ctypes.get_last_error())

    for tag, mask in (("read", GENERIC_READ), ("write", GENERIC_WRITE),
                      ("writedac", WRITE_DAC), ("readattrs", FILE_READ_ATTRIBUTES)):
        ok, err, _sddl, pair = try_pipe_pair(tag, mask)
        report(f"CreateFileA(pipe client, {tag})", ok, err)
        if pair:
            for handle in pair:
                if handle != ERROR_INVALID_HANDLE_VALUE:
                    kernel32.CloseHandle(handle)

    if not full_ok or handles is None:
        return
    server, client = handles
    if client == ERROR_INVALID_HANDLE_VALUE:
        kernel32.CloseHandle(server)
        return

    # 用同一对管道当 stdio 起 cmd，模拟 libuv 的 CreateProcessW 段
    si = STARTUPINFOW()
    si.cb = ctypes.sizeof(STARTUPINFOW)
    si.dwFlags = STARTF_USESTDHANDLES
    si.hStdInput = kernel32.GetStdHandle(0xFFFFFFF6)
    si.hStdOutput = client
    si.hStdError = client
    pi = PROCESS_INFORMATION()
    cmdline = ctypes.create_unicode_buffer("cmd.exe /c echo hi")
    ok = kernel32.CreateProcessW(None, cmdline, None, None, True, 0, None, None,
                                 ctypes.byref(si), ctypes.byref(pi))
    err = ctypes.get_last_error()
    report("CreateProcessW(cmd.exe, stdio=pipe)", ok, err)
    if ok:
        kernel32.WaitForSingleObject(pi.hProcess, 30000)
        code = wt.DWORD()
        kernel32.GetExitCodeProcess(pi.hProcess, ctypes.byref(code))
        report("cmd.exe exit_code", True, code.value)
    kernel32.CloseHandle(server)
    kernel32.CloseHandle(client)


def parent(mode, extra_cmd=None):
    workspace = os.path.join(os.environ.get("TEMP", "."), "shaniu-pipe-probe", "ws")
    os.makedirs(workspace, exist_ok=True)
    write_sid_text = derive_write_sid(workspace)
    token = build_restricted_token(mode, write_sid_text)
    print(json.dumps({"mode": mode, "write_sid": write_sid_text, "workspace": workspace}), flush=True)

    def spawn_as_restricted(cmdline_text):
        cmdline = ctypes.create_unicode_buffer(cmdline_text)
        si = STARTUPINFOW()
        si.cb = ctypes.sizeof(STARTUPINFOW)
        pi = PROCESS_INFORMATION()
        ok = advapi32.CreateProcessAsUserW(
            token, None, cmdline, None, None, False, 0, None, workspace,
            ctypes.byref(si), ctypes.byref(pi))
        err = ctypes.get_last_error()
        if not ok:
            report(f"CreateProcessAsUserW({cmdline_text})", False, err)
            return None
        kernel32.WaitForSingleObject(pi.hProcess, 120000)
        code = wt.DWORD()
        kernel32.GetExitCodeProcess(pi.hProcess, ctypes.byref(code))
        return code.value

    code = spawn_as_restricted(
        f'"{sys.executable}" "{os.path.abspath(__file__)}" --child {write_sid_text}')
    report("child probe exit_code", code == 0, code)

    node = shutil.which("node")
    if node:
        code = spawn_as_restricted(
            f'"{node}" -e "require(\'child_process\').execSync(\'cmd /c echo hi\')"')
        report("node execSync(pipe stdio) exit_code", code == 0, code)
        code = spawn_as_restricted(
            f'"{node}" -e "require(\'child_process\').execSync(\'cmd /c echo hi\', {{stdio: \'inherit\'}})"')
        report("node execSync(stdio=inherit) exit_code", code == 0, code)

    if extra_cmd:
        code = spawn_as_restricted(extra_cmd)
        report(f"exec {extra_cmd} exit_code", code == 0, code)


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "--child":
        child_probe(sys.argv[2])
    else:
        parent(sys.argv[1] if len(sys.argv) > 1 else "current",
               sys.argv[2] if len(sys.argv) > 2 else None)
