//! 通用小工具函数。

use std::os::windows::process::CommandExt;

use windows::Win32::Foundation::{FILETIME, HINSTANCE, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, HICON, IDI_APPLICATION};

/// 把 Rust 字符串转换为 Windows API 需要的、以 0 结尾的 UTF-16 缓冲区。
pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 在资源管理器中打开某个文件/文件夹所在的目录，并选中它。
///
/// 关键点：必须用 `raw_arg` 原样传递 `/select,"完整路径"`。
/// 如果改用普通 `.arg()`，Rust 会把含空格的整个参数再包一层引号，
/// 变成 `"/select,C:\Program Files\..."`，explorer 解析失败后会退回
/// 打开“文档”文件夹——这正是之前右键菜单跳错目录的原因。
pub fn reveal_in_explorer(path: &str) {
    let mut command = std::process::Command::new("explorer.exe");
    command.raw_arg(format!("/select,\"{}\"", path));
    let _ = command.spawn();
}

/// 用资源管理器打开一个目录。
pub fn open_directory(path: &str) {
    let mut command = std::process::Command::new("explorer.exe");
    command.raw_arg(format!("\"{}\"", path));
    let _ = command.spawn();
}

/// 取得“包含指定函数地址的模块”的文件路径。
///
/// 右键菜单扩展运行在资源管理器进程里，这时 `current_exe()` 得到的是
/// explorer.exe，而不是我们自己的 DLL。用模块地址查询才能拿到真实路径。
pub fn module_path_from_address(address: *const ()) -> Option<std::path::PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    unsafe {
        let mut module = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address as *const u16),
            &mut module,
        )
        .ok()?;

        let mut buffer = vec![0u16; 32_768];
        let length = GetModuleFileNameW(module, &mut buffer) as usize;
        if length == 0 {
            return None;
        }
        Some(std::path::PathBuf::from(String::from_utf16_lossy(
            &buffer[..length],
        )))
    }
}

/// 加载本程序自己嵌入的图标（资源 ID 为 1）。
///
/// 如果资源不存在（例如旧版本 exe），退回到系统默认应用图标，
/// 保证托盘、窗口不会因为缺图标而显示异常。
pub fn app_icon() -> HICON {
    unsafe {
        let module = GetModuleHandleW(None).unwrap_or_default();
        // MAKEINTRESOURCE(1)：资源 ID 1 的图标，由 build.rs 写入 exe/dll。
        if let Ok(icon) = LoadIconW(HINSTANCE(module.0), windows::core::PCWSTR(1 as *const u16)) {
            return icon;
        }
        LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default()
    }
}

/// 把 Unix 时间戳（秒）转换为本地时间文本，例如 `2026-11-15 16:20:31`。
///
/// Windows 的 FILETIME 以 1601-01-01 为起点、单位是 100 纳秒，
/// 所以先换算成 FILETIME，再交给系统 API 转成本地时间。
pub fn timestamp_to_local_text(unix_seconds: i64) -> String {
    // 1601-01-01 到 1970-01-01 之间的秒数差
    const SECONDS_BETWEEN_1601_AND_1970: i64 = 11_644_473_600;

    let Some(ticks) = (unix_seconds + SECONDS_BETWEEN_1601_AND_1970).checked_mul(10_000_000) else {
        return String::from("未知时间");
    };

    let file_time = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };

    let mut local_time = FILETIME::default();
    let mut system_time = SYSTEMTIME::default();

    unsafe {
        if FileTimeToLocalFileTime(&file_time, &mut local_time).is_ok()
            && FileTimeToSystemTime(&local_time, &mut system_time).is_ok()
        {
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                system_time.wYear,
                system_time.wMonth,
                system_time.wDay,
                system_time.wHour,
                system_time.wMinute,
                system_time.wSecond
            );
        }
    }

    String::from("未知时间")
}
