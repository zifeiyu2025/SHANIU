<#
.SYNOPSIS
    沙箱令牌与路径访问的只读探针。

.DESCRIPTION
    把 docs/13、docs/14 里「容器令牌特权 / AccessCheck / GetFileAttributes·FindFirstFile·CreateFile」
    这三类观测固化成可复跑脚本。**只读**：不写 DACL、不改任何文件，只把观测写成 JSONL。

    默认观测当前进程令牌；给 -TokenPid 时改为观测该进程令牌：AccessCheck 用该令牌句柄，
    文件访问探测在该令牌的模拟身份下执行（模拟失败会如实记一行并把结果留空）。

.PARAMETER Paths
    要探测的对象（目录或文件）。默认覆盖盘根、C:\Users、用户目录、TEMP 与仓库 docs。

.PARAMETER TokenPid
    可选。取其进程令牌做观测与模拟。不给则用当前进程令牌（自校验基线）。

.PARAMETER OutDir
    JSONL 落盘目录，默认 <repo>/scratch/evidence-<今天>。

.EXAMPLE
    pwsh -File scripts/dev/sandbox-token-probe.ps1
    pwsh -File scripts/dev/sandbox-token-probe.ps1 -TokenPid 12345
#>
[CmdletBinding()]
param(
    [string[]] $Paths,
    [int] $TokenPid = 0,
    [string] $OutDir
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $OutDir) { $OutDir = Join-Path $repo ("scratch/evidence-" + (Get-Date -Format 'yyyy-MM-dd')) }
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$outFile = Join-Path $OutDir "token-access-probe-$stamp.jsonl"

if (-not $Paths -or $Paths.Count -eq 0) {
    $Paths = @('C:\', 'C:\Users', $env:USERPROFILE, $env:TEMP, (Join-Path $repo 'docs'))
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class ShaniuProbe
{
    [StructLayout(LayoutKind.Sequential)]
    public struct GENERIC_MAPPING { public uint GenericRead, GenericWrite, GenericExecute, GenericAll; }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct WIN32_FIND_DATAW
    {
        public uint dwFileAttributes;
        public long ftCreationTime, ftLastAccessTime, ftLastWriteTime;
        public uint dwVolumeSerialNumber, nFileSizeHigh, nFileSizeLow, nNumberOfLinks, nFileIndexHigh, nFileIndexLow;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string cFileName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 14)] public string cAlternateFileName;
    }

    const uint READ_CONTROL = 0x20000;
    const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    const uint OPEN_EXISTING = 3;
    const uint TOKEN_QUERY = 0x8;
    const uint TOKEN_DUPLICATE = 0x2;
    const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    const int OWNER_SECURITY_INFORMATION = 0x1;
    const int DACL_SECURITY_INFORMATION = 0x4;
    const int TokenIsAppContainer = 29;
    const int TokenIntegrityLevel = 25;

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string n, uint a, uint s, IntPtr sa, uint cd, uint fa, IntPtr t);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint GetFileAttributesW(string n);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr FindFirstFileW(string n, out WIN32_FIND_DATAW d);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool FindClose(IntPtr h);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint GetNamedSecurityInfoW(string name, int objectType, int si,
        out IntPtr owner, out IntPtr group, out IntPtr dacl, out IntPtr sacl, out IntPtr sd);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AccessCheck(IntPtr sd, IntPtr token, uint da, ref GENERIC_MAPPING m,
        IntPtr priv, ref uint plen, out uint granted, [MarshalAs(UnmanagedType.Bool)] out bool status);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool IsValidSecurityDescriptor(IntPtr sd);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr p, uint a, out IntPtr t);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool DuplicateTokenEx(IntPtr e, uint a, IntPtr sa, int il, int tt, out IntPtr nt);
    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool GetTokenInformation(IntPtr t, int c, ref int info, int len, out int ret);
    [DllImport("advapi32.dll", SetLastError = true, EntryPoint = "GetTokenInformation")]
    static extern bool GetTokenInformationPtr(IntPtr t, int c, IntPtr info, int len, out int ret);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertSidToStringSidW(IntPtr sid, out IntPtr str);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr LocalFree(IntPtr h);

    public static void Close(IntPtr h) { if (h != IntPtr.Zero) CloseHandle(h); }

    // "ok:0x<attrs>" 或 "err:<win32>"
    public static string Attrs(string path)
    {
        uint a = GetFileAttributesW(path);
        int e = Marshal.GetLastWin32Error();
        if (a == 0xFFFFFFFF) return "err:" + e;
        return "ok:0x" + a.ToString("X");
    }

    public static string Find(string pattern)
    {
        WIN32_FIND_DATAW d;
        IntPtr h = FindFirstFileW(pattern, out d);
        int e = Marshal.GetLastWin32Error();
        if (h == new IntPtr(-1)) return "err:" + e;
        FindClose(h);
        return "ok";
    }

    public static string Open(string path, uint access)
    {
        IntPtr h = CreateFileW(path, access, 7, IntPtr.Zero, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, IntPtr.Zero);
        int e = Marshal.GetLastWin32Error();
        if (h == new IntPtr(-1)) return "err:" + e;
        CloseHandle(h);
        return "ok";
    }

    // pid<=0 时用当前进程
    public static IntPtr Token(int pid)
    {
        IntPtr proc = pid > 0 ? OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) : GetCurrentProcess();
        if (proc == IntPtr.Zero) throw new Exception("OpenProcess failed: " + Marshal.GetLastWin32Error());
        IntPtr tok;
        if (!OpenProcessToken(proc, TOKEN_QUERY | TOKEN_DUPLICATE, out tok))
            throw new Exception("OpenProcessToken failed: " + Marshal.GetLastWin32Error());
        return tok;
    }

    // 复制成模拟令牌：AccessCheck 的 ClientToken 必须是 impersonation token，
    // WindowsIdentity.Impersonate 也用它。
    public static IntPtr DuplicateForImpersonation(IntPtr tok)
    {
        IntPtr dup;
        if (!DuplicateTokenEx(tok, 0x000F01FF, IntPtr.Zero, 2, 2, out dup))
            throw new Exception("DuplicateTokenEx failed: " + Marshal.GetLastWin32Error());
        return dup;
    }

    // "granted:0x.." / "denied:0x.." / "err:<code>@sd|check"
    // 用**当前进程令牌**读 DACL（不受目标令牌限制），再用 clientToken 做判定。
    public static string AccessCheckPath(string path, IntPtr clientToken, uint desired)
    {
        IntPtr owner, group, dacl, sacl, sd;
        uint e = GetNamedSecurityInfoW(path, 1, 0x5, out owner, out group, out dacl, out sacl, out sd);
        if (e != 0) return "err:" + e + "@sd";
        try
        {
            var map = new GENERIC_MAPPING();
            uint granted; bool ok;
            uint privLen = 1024;
            IntPtr priv = Marshal.AllocHGlobal((int)privLen);
            try
            {
                // PRIVILEGE_SET { DWORD PrivilegeCount; DWORD Control; }，Control=1(ALL_NECESSARY)
                Marshal.WriteInt32(priv, 0);
                Marshal.WriteInt32(priv, 4, 1);
                if (!AccessCheck(sd, clientToken, desired, ref map, priv, ref privLen, out granted, out ok))
                    return "err:" + Marshal.GetLastWin32Error() + "@check(sdValid=" + IsValidSecurityDescriptor(sd) + ")";
                return (ok ? "granted:0x" : "denied:0x") + granted.ToString("X");
            }
            finally { Marshal.FreeHGlobal(priv); }
        }
        finally { LocalFree(sd); }
    }

    // "isAppContainer=0|1;integritySid=S-1-16-..."
    public static string TokenSummary(IntPtr tok)
    {
        int appc = 0, ret = 0;
        GetTokenInformation(tok, TokenIsAppContainer, ref appc, 4, out ret);

        string integrity = "?";
        int len = 0;
        GetTokenInformationPtr(tok, TokenIntegrityLevel, IntPtr.Zero, 0, out len);
        if (len > 0)
        {
            IntPtr buf = Marshal.AllocHGlobal(len);
            try
            {
                int got;
                if (GetTokenInformationPtr(tok, TokenIntegrityLevel, buf, len, out got))
                {
                    IntPtr sid = Marshal.ReadIntPtr(buf);
                    integrity = SidString(sid);
                }
            }
            finally { Marshal.FreeHGlobal(buf); }
        }
        return "isAppContainer=" + appc + ";integritySid=" + integrity;
    }

    static string SidString(IntPtr sid)
    {
        if (sid == IntPtr.Zero) return "?";
        IntPtr s;
        if (!ConvertSidToStringSidW(sid, out s)) return "?";
        try { return Marshal.PtrToStringUni(s); }
        finally { LocalFree(s); }
    }
}
'@

function Write-Row {
    param([hashtable]$Row)
    $Row['ts'] = (Get-Date).ToString('o')
    ($Row | ConvertTo-Json -Compress -Depth 5) | Add-Content -Path $outFile -Encoding utf8
}

$token = [ShaniuProbe]::Token($TokenPid)
$clientToken = [ShaniuProbe]::DuplicateForImpersonation($token)
$impersonation = $null
try {
    $summary = [ShaniuProbe]::TokenSummary($token)

    # -TokenPid 时先模拟目标身份，让 whoami /priv 与文件探测都反映目标令牌。
    if ($TokenPid -gt 0) {
        try {
            $impersonation = [System.Security.Principal.WindowsIdentity]::new([IntPtr]$clientToken).Impersonate()
            Write-Row @{ section = 'impersonation'; pid = $TokenPid; result = 'ok' }
        }
        catch {
            Write-Row @{ section = 'impersonation'; pid = $TokenPid; result = 'failed'; error = $_.Exception.Message }
            $impersonation = $null
        }
    }

    $privs = @()
    $whoamiPriv = (& whoami /priv) 2>&1 | Out-String
    foreach ($line in ($whoamiPriv -split "`r?`n")) {
        if ($line -match '^(Se\w+Privilege)\s+(\S+)') { $privs += [pscustomobject]@{ name = $Matches[1]; state = $Matches[2] } }
    }
    Write-Row @{ section = 'token'; pid = $TokenPid; summary = $summary; privileges = $privs }

    $masks = [ordered]@{
        'FILE_TRAVERSE' = 0x20
        'FILE_READ_ATTRIBUTES' = 0x80
        'FILE_LIST_DIRECTORY' = 0x1
        'READ_CONTROL' = 0x20000
        'RA|S' = 0x100080
        'FILE_GENERIC_EXECUTE' = 0x1200A0
    }

    foreach ($p in $Paths) {
        foreach ($name in $masks.Keys) {
            $r = [ShaniuProbe]::AccessCheckPath($p, $clientToken, [uint32]$masks[$name])
            $row = @{ section = 'accesscheck'; path = $p; mask = $name; desired = ('0x{0:X}' -f $masks[$name]); result = $r }
            if ($r -like 'err:*') {
                # AccessCheck 不可用时（本机报 1338，SD 校验为 True），退回到最硬的判定：
                # 用同一掩码直接 CreateFile，看内核是否放行。
                $row['fallback_createfile'] = [ShaniuProbe]::Open($p, [uint32]$masks[$name])
            }
            Write-Row $row
        }
        Write-Row @{ section = 'probe'; path = $p; api = 'GetFileAttributes'; result = [ShaniuProbe]::Attrs($p) }
        Write-Row @{ section = 'probe'; path = $p; api = 'FindFirstFile'; result = [ShaniuProbe]::Find((Join-Path $p '*')) }
        Write-Row @{ section = 'probe'; path = $p; api = 'CreateFile(READ_ATTRIBUTES)'; result = [ShaniuProbe]::Open($p, 0x80) }
        Write-Row @{ section = 'probe'; path = $p; api = 'CreateFile(GENERIC_READ)'; result = [ShaniuProbe]::Open($p, [uint32]2147483648) }
    }
}
finally {
    if ($impersonation) { $impersonation.Undo() }
    [ShaniuProbe]::Close($clientToken)
    [ShaniuProbe]::Close($token)
}

Write-Host "wrote $outFile"
