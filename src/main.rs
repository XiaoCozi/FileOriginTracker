//! File Origin Tracker - 后台文件创建来源追踪器（主程序）。
//!
//! 功能：
//! 1. 通过 ETW 捕获系统中新建的文件/文件夹，并记录创建它的程序
//! 2. 记录保存到本地 SQLite 数据库（默认在用户 AppData 目录）
//! 3. 系统托盘常驻，双击图标查看最近记录（时间 / 程序 / 路径）
//! 4. 右键菜单由 `file_origin_tracker.dll` 提供，标题动态显示
//!    “该文件/文件夹来自 XXX 程序创建”，点击打开程序所在目录
//!
//! 代码结构见 `src/lib.rs`。

// 关键：告诉编译器这是一个 Windows GUI 程序，启动时不显示控制台/CMD 窗口。
// 必须放在 crate 根（main.rs 顶部）才会生效。
#![windows_subsystem = "windows"]

use anyhow::Result;
use log::{error, info, warn, Level, LevelFilter, Log, Metadata, Record};
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use file_origin_tracker_shell::{config, db, etw, shell_menu, tray};

/// 简单的文件日志器：把日志写入数据目录下的 `file_origin_tracker.log`。
///
/// 因为主程序使用了 `windows_subsystem = "windows"`，没有控制台窗口，
/// `env_logger` 输出到 stdout/stderr 的内容用户看不到，所以这里改成写文件。
///
/// 日志按体积滚动：超过 `MAX_LOG_BYTES` 就把旧文件改名为 `.log.old` 并重新开始，
/// 只保留一份备份。这样无论运行多久，日志占用都不会超过约 2MB。
struct FileLogger {
    /// 日志文件路径，滚动时需要用它重命名。
    path: std::path::PathBuf,
    /// 当前日志文件句柄与已写入字节数，用 Mutex 保护以支持多线程写入。
    state: Mutex<LogState>,
}

struct LogState {
    file: Option<File>,
    bytes: u64,
}

/// 单个日志文件的最大体积：2MB。
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

impl FileLogger {
    /// 创建日志器并打开（或创建）日志文件。
    fn new(data_dir: &std::path::Path) -> Self {
        let _ = std::fs::create_dir_all(data_dir);
        let path = data_dir.join("file_origin_tracker.log");
        let state = open_log(&path);
        Self {
            path,
            state: Mutex::new(state),
        }
    }
}

/// 以追加模式打开日志文件，并记录它当前的大小（用于判断何时滚动）。
///
/// 如果发现旧版本留下的超大日志（例如 200MB 以上），会直接清空，
/// 避免它继续占用磁盘；清空失败也不影响程序运行。
fn open_log(path: &std::path::Path) -> LogState {
    let mut bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if bytes > MAX_LOG_BYTES {
        if let Ok(file) = File::options().write(true).open(path) {
            let _ = file.set_len(0);
            bytes = 0;
        }
    }

    let file = File::options().create(true).append(true).open(path).ok();
    LogState { file, bytes }
}

impl Log for FileLogger {
    /// 只记录 Info 及以上级别的日志，减少磁盘写入。
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    /// 写入一条日志，格式：`Unix时间戳 [级别] 内容`。
    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let Ok(mut guard) = self.state.lock() else {
            return;
        };

        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!("{} [{}] {}\n", secs, record.level(), record.args());

        // 超过上限就滚动：旧的改名为 .old（覆盖上一次的 .old），再重新打开。
        if guard.bytes + line.len() as u64 > MAX_LOG_BYTES {
            guard.file = None;
            let backup = self.path.with_extension("log.old");
            let _ = std::fs::remove_file(&backup);
            let rotated = std::fs::rename(&self.path, &backup).is_ok();
            if rotated {
                *guard = open_log(&self.path);
            } else {
                // 改名失败（文件被占用或无权限）时退化为原地清空，保证不无限增长。
                if let Ok(file) = File::options().write(true).open(&self.path) {
                    let _ = file.set_len(0);
                }
                *guard = open_log(&self.path);
            }
        }

        if let Some(file) = guard.file.as_mut() {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
            guard.bytes += line.len() as u64;
        }
    }

    fn flush(&self) {}
}

/// 检查当前进程是否以管理员权限（提升的令牌）运行。
///
/// ETW 内核文件事件追踪（Microsoft-Windows-Kernel-File）需要管理员权限才能启动会话。
/// 普通权限运行时，StartTraceW 会返回 ERROR_ACCESS_DENIED，导致无法追踪文件创建。
fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        // 打开当前进程的访问令牌，只请求查询权限。
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut return_length = 0u32;
        // 查询令牌是否被提升（即是否以管理员身份运行）。
        let result = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut return_length,
        );
        let _ = CloseHandle(token);
        // TokenIsElevated != 0 表示当前进程拥有管理员权限。
        result.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// 以管理员身份重新启动当前程序，然后退出当前（非管理员）进程。
///
/// 使用 `ShellExecuteExW` 的 `runas` 动词弹出 UAC 提权对话框。
/// 加上 `SEE_MASK_NO_CONSOLE` 标志，避免提权后的新进程继承/弹出控制台窗口。
/// 主程序本身已经用 `#![windows_subsystem = "windows"]` 编译为 GUI 子系统，
/// 不会自带控制台；这里的标志是防止 ShellExecute 在某些情况下额外开一个 cmd 窗口。
/// 单实例互斥体的名称。
///
/// `Local\` 前缀表示“只在当前登录会话内唯一”：
/// 同一用户重复启动会被拦住；不同用户各自登录后可以各跑一份，互不干扰。
const SINGLE_INSTANCE_MUTEX: &str = "Local\\FileOriginTracker.SingleInstance";

/// 尝试成为唯一实例。
///
/// 返回值说明：
/// - `Some(句柄)`：抢到了互斥体，是唯一实例。句柄必须一直持有到进程结束，
///   否则互斥体被释放，后面再启动的进程就检测不到“已有实例”了。
/// - `None`：已经有实例在运行。本函数会顺手把已有窗口唤到前台，调用方直接退出即可。
fn acquire_single_instance() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        let name = HSTRING::from(SINGLE_INSTANCE_MUTEX);
        let handle = CreateMutexW(None, false, &name).ok()?;

        // CreateMutexW 即使互斥体已存在也会返回句柄，只是错误码会变成
        // ERROR_ALREADY_EXISTS。所以不能只看返回值，必须再查一次错误码。
        if GetLastError() == ERROR_ALREADY_EXISTS {
            focus_existing_instance();
            let _ = CloseHandle(handle);
            return None;
        }

        Some(handle)
    }
}

/// 把已经在运行的实例的「最近记录」窗口还原并置前，让用户知道程序就在托盘里。
fn focus_existing_instance() {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

    unsafe {
        let title = HSTRING::from("文件来源追踪 - 最近记录");
        if let Ok(window) = FindWindowW(PCWSTR::null(), &title) {
            if !window.is_invalid() {
                let _ = ShowWindow(window, SW_RESTORE);
                let _ = SetForegroundWindow(window);
            }
        }
    }
}

/// 监控启动失败时弹窗提示。
///
/// 如果不提示，用户只会看到托盘图标，误以为程序正在正常记录，
/// 实际上一条记录都不会产生，所以这里必须主动告知原因与日志位置。
fn show_monitor_start_failure(detail: &str) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};

    let text = HSTRING::from(format!(
        "文件监控启动失败，程序不会记录任何文件操作。\n\n\
         常见原因：\n\
         1. 未以管理员身份运行；\n\
         2. 被安全软件拦截。\n\n\
         详细信息：{detail}\n\n\
         日志位置：数据目录下的 file_origin_tracker.log"
    ));
    let title = HSTRING::from("文件来源追踪 - 监控启动失败");

    unsafe {
        let _ = MessageBoxW(HWND::default(), &text, &title, MB_OK | MB_ICONWARNING);
    }
}

fn restart_as_admin() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NO_CONSOLE, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let operation = file_origin_tracker_shell::util::wide("runas");
    let file = file_origin_tracker_shell::util::wide(&exe.to_string_lossy());

    let mut sei = SHELLEXECUTEINFOW::default();
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    // SEE_MASK_NO_CONSOLE: 不为新进程创建控制台窗口
    sei.fMask = SEE_MASK_NO_CONSOLE;
    sei.hwnd = HWND::default();
    sei.lpVerb = PCWSTR(operation.as_ptr());
    sei.lpFile = PCWSTR(file.as_ptr());
    sei.nShow = SW_SHOWNORMAL.0;

    unsafe {
        let _ = ShellExecuteExW(&mut sei);
    }
}

fn main() -> Result<()> {
    // 0. 注册 panic hook，把崩溃信息写入日志（panic=abort 时也能在终止前记录）。
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<no message>".to_string());
        log::error!("[PANIC] {} - {}", location, msg);
    }));

    // 1. 初始化文件日志（必须在所有日志调用之前）。
    let data_dir = config::Config::default_data_dir()?;
    let logger = FileLogger::new(&data_dir);
    // 设置全局日志器，忽略"已经设置过"的错误（多次启动时可能发生）。
    log::set_boxed_logger(Box::new(logger))
        .map(|()| log::set_max_level(LevelFilter::Info))
        .ok();

    info!("=== File Origin Tracker 启动 ===");

    // 2. 兼容旧版静态菜单的命令：`程序.exe shell-menu "文件路径"`
    // 该模式只查询并弹窗，不启动托盘和 ETW，也不需要管理员权限。
    let arguments: Vec<String> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("shell-menu") {
        if let Some(target_path) = arguments.get(2) {
            shell_menu::handle_shell_menu(target_path)?;
        }
        return Ok(());
    }

    // `程序.exe history`：只打开"最近记录"窗口，适合创建桌面快捷方式。
    // 查询记录只读数据库，同样不需要管理员权限，因此放在提权检查之前，
    // 避免每次打开记录窗口都弹出 UAC 提示。
    if arguments.get(1).map(String::as_str) == Some("history") {
        file_origin_tracker_shell::history::show()?;
        file_origin_tracker_shell::history::run_message_loop();
        return Ok(());
    }

    // 3. 管理员权限检测与自动提权。
    // ETW 内核追踪必须要管理员权限，否则根本收不到文件创建事件。
    if !is_elevated() {
        info!("当前非管理员权限，正在请求 UAC 提权重启...");
        restart_as_admin();
        // 提权后的新进程会继续运行；当前非管理员进程直接退出，避免出现两个实例。
        return Ok(());
    }
    info!("已获得管理员权限，ETW 追踪可用");

    // 3.5 单实例保护：同一登录会话只允许运行一个主程序。
    //     必须放在提权检查之后：非管理员进程会先请求提权然后退出，
    //     如果把互斥体放在提权之前创建，提权后的新进程会被自己拦住。
    //     也不能放在前面 `history` / `shell-menu` 分支之前，
    //     否则双击快捷方式打开记录窗口时会被正在运行的实例挡掉。
    let Some(_single_instance) = acquire_single_instance() else {
        info!("检测到已有实例在运行，本次启动退出");
        return Ok(());
    };

    // 4. 加载配置
    let cfg = config::Config::load_or_default()?;
    info!("数据存储目录: {}", cfg.data_dir.display());

    // 5. 初始化数据库
    let db = db::Database::init(&cfg.data_dir)?;
    info!("数据库初始化完成");

    // 6. 单文件版：先把 exe 内打包的右键菜单扩展 DLL 释放到数据目录。
    //    开发版（exe 里没有打包数据）这里什么都不做，仍然使用同目录的 DLL。
    // 右键菜单扩展 DLL 与主程序放在同一目录（双文件分发）。
    // 程序不再从自身资源里释放 DLL，避免“把可执行文件写到磁盘”这种
    // 容易被杀软判定为恶意加载器的行为。
    match shell_menu::dll_path() {
        Ok(dll) if dll.exists() => info!("右键菜单扩展已就绪：{}", dll.display()),
        Ok(dll) => warn!("未找到右键菜单扩展 DLL：{}", dll.display()),
        Err(error) => warn!("无法确定右键菜单扩展路径：{:#}", error),
    }

    // 7. 注册右键菜单扩展（写入当前用户注册表，不需要管理员权限）
    if cfg.auto_register_context_menu {
        match shell_menu::register_context_menu() {
            Ok(()) => info!("右键菜单已注册（请重启资源管理器或重新登录后生效）"),
            Err(error) => error!("右键菜单注册失败: {:#}", error),
        }
    }

    // 8. 按设置清理过期记录，并压缩数据库文件（只在确实删除过数据时执行）。
    match db.maintain(cfg.retention_days) {
        Ok(deleted) if deleted > 0 => info!(
            "已按保留 {} 天清理 {} 条旧记录，并压缩数据库",
            cfg.retention_days, deleted
        ),
        Ok(_) => info!("没有需要清理的旧记录（保留 {} 天）", cfg.retention_days),
        Err(error) => warn!("清理旧记录失败：{:#}", error),
    }

    // 每天再自动清理一次，避免程序长期运行后数据库持续变大。
    let maintenance_config_dir = cfg.data_dir.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(24 * 3600));
        if let Ok(config) = config::Config::load_or_default() {
            if let Ok(db) = db::Database::init(&maintenance_config_dir) {
                match db.maintain(config.retention_days) {
                    Ok(deleted) if deleted > 0 => {
                        info!("每日清理：删除 {} 条旧记录", deleted)
                    }
                    Ok(_) => {}
                    Err(error) => warn!("每日清理失败：{:#}", error),
                }
            }
        }
    });

    // 9. 按设置启动 ETW 监听（后台线程）。
    // 三个监控开关（新建文件 / 新建文件夹 / 修改文件）只要有一个开启就启动会话；
    // 全部关闭时完全跳过，不产生任何记录，也不占用 CPU。
    if cfg.any_monitor_enabled() {
        match etw::start_monitoring(db, cfg.clone()) {
            Ok(()) => info!("ETW 文件追踪已启动"),
            Err(error) => {
                error!("ETW 文件追踪启动失败: {:#}", error);
                error!("提示：请右键以管理员身份运行本程序，才能记录文件创建来源。");
                // 仅仅写日志不够：GUI 程序没有控制台，用户看不到这行日志，
                // 会误以为程序在正常记录，所以这里弹窗明确告知。
                show_monitor_start_failure(&error.to_string());
            }
        }
    } else {
        info!("三个监控开关均已关闭，本次启动不记录任何文件事件");
    }

    // 10. 启动系统托盘（阻塞主线程，处理 Windows 消息循环）
    info!("启动系统托盘...");
    let mut tray = tray::TrayIcon::new();
    tray.run()?;

    // 11. 清理
    info!("正在退出...");
    etw::stop_monitoring();
    // 释放界面统一字体与画刷（此时窗口已经全部销毁）。
    file_origin_tracker_shell::util::release_ui_resources();

    Ok(())
}
