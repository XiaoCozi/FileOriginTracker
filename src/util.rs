//! 通用小工具函数。

use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{FILETIME, HINSTANCE, SYSTEMTIME};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, HBRUSH, HFONT, OUT_DEFAULT_PRECIS,
};
use windows::Win32::Storage::FileSystem::{FileTimeToLocalFileTime, LocalFileTimeToFileTime};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime};
use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, HICON, IDI_APPLICATION};

/// 界面统一字体缓存（微软雅黑 UI 9pt）。
static UI_FONT: AtomicIsize = AtomicIsize::new(0);
/// 界面统一背景色画刷缓存（白色）。
static UI_BRUSH: AtomicIsize = AtomicIsize::new(0);

/// 取得界面统一字体。
///
/// 系统默认控件字体是「宋体 / 9pt」，在高分屏上又小又发虚；
/// 换成「微软雅黑 UI」后中文显示更清晰，两个窗口的观感也统一。
pub fn ui_font() -> HFONT {
    let cached = UI_FONT.load(Ordering::SeqCst);
    if cached != 0 {
        return HFONT(cached as *mut core::ffi::c_void);
    }

    let face = wide("Microsoft YaHei UI");
    let font = unsafe {
        CreateFontW(
            -13,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    };
    UI_FONT.store(font.0 as isize, Ordering::SeqCst);
    font
}

/// 取得界面统一背景画刷（白色）。
///
/// 处理 WM_CTLCOLORSTATIC / WM_CTLCOLOREDIT 时返回它，
/// 让标签和输入框的背景与窗口保持一致，不会出现灰色块。
pub fn ui_brush() -> HBRUSH {
    let cached = UI_BRUSH.load(Ordering::SeqCst);
    if cached != 0 {
        return HBRUSH(cached as *mut core::ffi::c_void);
    }
    let brush = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x00FF_FFFF)) };
    UI_BRUSH.store(brush.0 as isize, Ordering::SeqCst);
    brush
}

/// 释放字体与画刷（程序退出前调用，避免资源泄漏告警）。
pub fn release_ui_resources() {
    let font = UI_FONT.swap(0, Ordering::SeqCst);
    if font != 0 {
        unsafe {
            let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(font as *mut _));
        }
    }
    let brush = UI_BRUSH.swap(0, Ordering::SeqCst);
    if brush != 0 {
        unsafe {
            let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(brush as *mut _));
        }
    }
}

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

/// 把「本地时间的 SYSTEMTIME」换算成 Unix 时间戳（秒）。
///
/// 用于「最近记录」窗口的时间筛选：日期时间框给的 SYSTEMTIME 是本地时间，
/// 数据库里存的是 UTC 时间戳，需要先转成 FILETIME 再转 UTC 后相减。
/// 任何一步失败都返回 `None`，调用方按“没有筛选”处理。
pub fn local_system_time_to_unix(system_time: &SYSTEMTIME) -> Option<i64> {
    // 1601-01-01 到 1970-01-01 之间的秒数差
    const SECONDS_BETWEEN_1601_AND_1970: i64 = 11_644_473_600;

    let mut local_file_time = FILETIME::default();
    let mut utc_file_time = FILETIME::default();
    unsafe {
        SystemTimeToFileTime(system_time, &mut local_file_time).ok()?;
        LocalFileTimeToFileTime(&local_file_time, &mut utc_file_time).ok()?;
    }

    // FILETIME 的高 / 低 32 位拼成 100 纳秒单位的整数，再换算成秒。
    let ticks = ((utc_file_time.dwHighDateTime as i64) << 32) | utc_file_time.dwLowDateTime as i64;
    Some(ticks / 10_000_000 - SECONDS_BETWEEN_1601_AND_1970)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unix 时间戳 → 本地时间文本 → 再换回 Unix 秒，应该基本不变（容差 2 秒）。
    #[test]
    fn local_system_time_conversion_roundtrip() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // timestamp_to_local_text 固定输出 "YYYY-MM-DD HH:MM:SS"。
        let text = timestamp_to_local_text(now);
        let mut halves = text.split(' ');
        let date = halves.next().expect("缺少日期部分");
        let clock = halves.next().expect("缺少时间部分");
        let date_parts: Vec<u16> = date
            .split('-')
            .map(|part| part.parse().expect("日期格式错误"))
            .collect();
        let clock_parts: Vec<u16> = clock
            .split(':')
            .map(|part| part.parse().expect("时间格式错误"))
            .collect();

        let system_time = SYSTEMTIME {
            wYear: date_parts[0],
            wMonth: date_parts[1],
            wDay: date_parts[2],
            wHour: clock_parts[0],
            wMinute: clock_parts[1],
            wSecond: clock_parts[2],
            ..Default::default()
        };

        let converted = local_system_time_to_unix(&system_time).expect("本地时间换算失败");
        assert!(
            (converted - now).abs() <= 2,
            "换算结果是 {converted}，原时间戳是 {now}"
        );
    }
}
