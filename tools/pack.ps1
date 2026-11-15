# 把主程序 exe 和右键菜单扩展 DLL 合并成一个单文件 exe。
#
# 合并方式：在 exe 末尾追加 [DLL 内容][8 字节长度][16 字节标记]。
# Windows 加载 exe 时会忽略末尾的附加数据，程序启动后会自己把 DLL
# 释放到 %LOCALAPPDATA%\FileOriginTracker\FileOriginTracker\data\shell\。
#
# 用法（在项目根目录或任意位置执行都可以）：
#   powershell -ExecutionPolicy Bypass -File tools\pack.ps1
# 产物：
#   dist\FileOriginTracker.exe

$ErrorActionPreference = 'Stop'

$project = Split-Path -Parent $PSScriptRoot
$target = Join-Path $project 'target\release'
$exe = Join-Path $target 'file-origin-tracker.exe'
$dll = Join-Path $target 'file_origin_tracker_shell.dll'

# 1. 编译（优先用 PATH 里的 cargo，其次用默认安装位置）
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) {
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}
Write-Host "正在编译：$cargo"
& $cargo build --release --offline --manifest-path (Join-Path $project 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw '编译失败，未生成单文件 exe' }

foreach ($file in @($exe, $dll)) {
    if (-not (Test-Path $file)) { throw "缺少产物：$file" }
}

# 2. 读取两个文件的内容并拼接
$exeBytes = [System.IO.File]::ReadAllBytes($exe)
$dllBytes = [System.IO.File]::ReadAllBytes($dll)
$magic = [System.Text.Encoding]::ASCII.GetBytes('FOT-SHELL-DLL-01')
$lengthBytes = [System.BitConverter]::GetBytes([int64]$dllBytes.Length)

$output = New-Object byte[] ($exeBytes.Length + $dllBytes.Length + $lengthBytes.Length + $magic.Length)
[System.Array]::Copy($exeBytes, 0, $output, 0, $exeBytes.Length)
[System.Array]::Copy($dllBytes, 0, $output, $exeBytes.Length, $dllBytes.Length)
[System.Array]::Copy($lengthBytes, 0, $output, $exeBytes.Length + $dllBytes.Length, $lengthBytes.Length)
[System.Array]::Copy($magic, 0, $output, $exeBytes.Length + $dllBytes.Length + $lengthBytes.Length, $magic.Length)

# 3. 写到 dist 目录
$dist = Join-Path $project 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$single = Join-Path $dist 'FileOriginTracker.exe'
[System.IO.File]::WriteAllBytes($single, $output)

Write-Host ''
Write-Host "已生成单文件程序：$single"
Write-Host ("大小：{0:N2} MB（exe {1:N2} MB + dll {2:N2} MB）" -f `
    ($output.Length / 1MB), ($exeBytes.Length / 1MB), ($dllBytes.Length / 1MB))
