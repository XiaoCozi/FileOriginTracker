# 生成两个发行包：
#   1. 双文件版（推荐）：FileOriginTracker.exe + file_origin_tracker_shell.dll
#   2. 单文件版：把 DLL 作为 RCDATA 资源写进 exe，启动时自动释放
#
# 单文件版虽然方便携带，但“程序运行时把 PE 文件写到磁盘”是木马加载器的
# 典型行为，可能被 360 的 QVM 等机器学习引擎误报，所以默认推荐双文件版。
#
# 用法（在项目根目录或任意位置执行都可以）：
#   powershell -ExecutionPolicy Bypass -File tools\pack.ps1
# 产物：
#   dist\FileOriginTracker.exe                        双文件版主程序
#   dist\file_origin_tracker_shell.dll                双文件版扩展 DLL
#   dist\FileOriginTracker-v<版本号>.zip              双文件版发行包
#   dist\FileOriginTracker-single.exe                 单文件版（直接分发，不打压缩包）

param(
    # 跳过编译，直接使用 target\release 里已有的产物。
    # GitHub Actions 会先单独执行 cargo build，再带这个开关调用本脚本。
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$project = Split-Path -Parent $PSScriptRoot
$target = Join-Path $project 'target\release'
$exe = Join-Path $target 'file-origin-tracker.exe'
$dll = Join-Path $target 'file_origin_tracker_shell.dll'

# 1. 编译（优先用 PATH 里的 cargo，其次用默认安装位置）
if ($SkipBuild) {
    Write-Host '已指定 -SkipBuild，跳过编译，直接使用现有产物。'
} else {
    $cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
    if (-not $cargo) {
        $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    }
    Write-Host "正在编译：$cargo"
    & $cargo build --release --offline --manifest-path (Join-Path $project 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw '编译失败，未生成单文件 exe' }
}

foreach ($file in @($exe, $dll)) {
    if (-not (Test-Path $file)) { throw "缺少产物：$file" }
}

# 2. 复制主程序和 DLL 到 dist（双文件版）
$dist = Join-Path $project 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$single = Join-Path $dist 'FileOriginTracker.exe'
$dllCopy = Join-Path $dist 'file_origin_tracker_shell.dll'
Copy-Item -Path $exe -Destination $single -Force
Copy-Item -Path $dll -Destination $dllCopy -Force

# 从 Cargo.toml 读取版本号，用于 zip 文件名
$version = '0.0.0'
$cargoToml = Join-Path $project 'Cargo.toml'
if (Test-Path $cargoToml) {
    $match = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if ($match) { $version = $match.Matches[0].Groups[1].Value }
}

# 3. 双文件版发行包
$zip = Join-Path $dist ("FileOriginTracker-v{0}.zip" -f $version)
Compress-Archive -Path $single, $dllCopy -DestinationPath $zip -Force

# 4. 单文件版：把 DLL 作为 RCDATA 资源写进 exe 副本
$singleExe = Join-Path $dist 'FileOriginTracker-single.exe'
Copy-Item -Path $exe -Destination $singleExe -Force

$dllBytes = [System.IO.File]::ReadAllBytes($dll)

# 重复执行本脚本时 PowerShell 会复用已加载的类型，不会重复定义。
if (-not ('FotResourcePatcher' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class FotResourcePatcher
{
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr BeginUpdateResource(string fileName, bool deleteExistingResources);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool UpdateResource(IntPtr update, IntPtr type, IntPtr name, ushort language, byte[] data, uint size);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool EndUpdateResource(IntPtr update, bool discard);
}
'@
}

$handle = [FotResourcePatcher]::BeginUpdateResource($singleExe, $false)
if ($handle -eq [IntPtr]::Zero) {
    throw "打开单文件版 exe 失败：$singleExe"
}

$written = $false
try {
    # 参数依次是：资源类型 RT_RCDATA(10)、资源 ID(101)、语言 0（中性）。
    $written = [FotResourcePatcher]::UpdateResource(
        $handle, [IntPtr]10, [IntPtr]101, 0, $dllBytes, [uint32]$dllBytes.Length)
    if (-not $written) {
        $code = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()
        throw "写入 DLL 资源失败（错误码：$code）"
    }
}
finally {
    # 写入失败时丢弃改动，避免在 dist 留下一个不完整的 exe。
    if (-not [FotResourcePatcher]::EndUpdateResource($handle, (-not $written))) {
        throw '保存单文件版 exe 失败'
    }
}

# 单文件版不打包 zip：它本身就是一个 exe，直接提供下载即可。

# 5. 输出结果
Write-Host ''
Write-Host '已生成发行文件：'
Write-Host ("  {0}（{1:N2} MB）" -f $single, ((Get-Item $single).Length / 1MB))
Write-Host ("  {0}（{1:N2} MB）" -f $dllCopy, ((Get-Item $dllCopy).Length / 1MB))
Write-Host ("  {0}（{1:N2} MB）" -f $zip, ((Get-Item $zip).Length / 1MB))
Write-Host ("  {0}（{1:N2} MB）" -f $singleExe, ((Get-Item $singleExe).Length / 1MB))
Write-Host ''
Write-Host '注意：单文件版会在运行时把 DLL 释放到固定的数据基准目录，可能被 360 等杀毒软件误报。'
Write-Host '      如果被拦截，请改用双文件版（exe 与 dll 放在同一目录，推荐）。'
