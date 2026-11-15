//! “设置”窗口。
//!
//! 提供以下可调整项：
//! 1. 记录保留天数：超过天数的旧记录会在启动时和每天自动清理，并压缩数据库
//! 2. 开机自动启动：写入当前用户的注册表 Run 项，登录后自动运行本程序
//! 3. 文件监控（三个独立开关）：创建文件、创建文件夹、修改文件。
//!    全部关闭后完全停止 ETW 会话。
//! 4. 资源管理器右键菜单的注册 / 注销（原先放在托盘菜单里）
//!
//! 界面使用原生 Win32 控件（静态文本、编辑框、复选框、按钮），
//! 不使用对话框资源文件，避免额外引入 .rc 编译步骤。

use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowTextW, LoadCursorW, MessageBoxW, RegisterClassExW,
    SendMessageW, SetWindowTextW, ShowWindow, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX,
    BS_GROUPBOX, BS_PUSHBUTTON, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_NUMBER, HMENU, ICON_BIG,
    ICON_SMALL, IDC_ARROW, MB_ICONWARNING, MB_OK, SW_HIDE, SW_SHOWNORMAL, WA_INACTIVE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORBTN,
    WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_SETICON, WNDCLASSEXW, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use crate::config::Config;
use crate::db::Database;
use crate::util::wide;

const WINDOW_TITLE: &str = "文件来源追踪 - 设置";
const WINDOW_CLASS: &str = "FileOriginTrackerSettingsWindow";

/// 控件 ID。窗口过程通过它们区分是哪一个控件发来的通知。
const ID_RETENTION_EDIT: usize = 2001;
const ID_AUTOSTART_CHECK: usize = 2002;
/// 监控开关：新建文件。
const ID_MONITOR_FILE_CHECK: usize = 2003;
const ID_SAVE_BUTTON: usize = 2004;
const ID_CLOSE_BUTTON: usize = 2005;
/// 监控开关：新建文件夹。
const ID_MONITOR_DIR_CHECK: usize = 2006;
/// 监控开关：修改文件。
const ID_MONITOR_MODIFY_CHECK: usize = 2007;
/// 注册 / 注销资源管理器右键菜单（从托盘菜单移到这里）。
const ID_REGISTER_SHELL_BUTTON: usize = 2008;
const ID_UNREGISTER_SHELL_BUTTON: usize = 2009;

/// 开机自启使用的注册表位置：当前用户登录时自动运行。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE_NAME: &str = "FileOriginTracker";

static SETTINGS_WINDOW: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 打开（或重新激活）设置窗口。
pub fn show() -> Result<()> {
    let existing = SETTINGS_WINDOW.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut std::ffi::c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        }
        load_values(HWND(existing as *mut std::ffi::c_void));
        return Ok(());
    }

    let module = unsafe { GetModuleHandleW(None)? };
    let instance = HINSTANCE(module.0);
    register_class(instance)?;

    let class_name = wide(WINDOW_CLASS);
    let title = wide(WINDOW_TITLE);

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            560,
            528,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )?
    };

    SETTINGS_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }
    load_values(hwnd);
    Ok(())
}

fn register_class(instance: HINSTANCE) -> Result<()> {
    if CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let class_name = wide(WINDOW_CLASS);
    let mut window_class = WNDCLASSEXW::default();
    window_class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    window_class.lpfnWndProc = Some(window_procedure);
    window_class.hInstance = instance;
    // 白色背景 + 统一字体，与「最近记录」窗口保持一致的观感。
    window_class.hbrBackground = crate::util::ui_brush();
    window_class.hCursor = unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW)? };
    window_class.lpszClassName = PCWSTR(class_name.as_ptr());

    if unsafe { RegisterClassExW(&window_class) } == 0 {
        CLASS_REGISTERED.store(false, Ordering::SeqCst);
        return Err(anyhow::anyhow!("注册设置窗口类失败：{:?}", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            create_controls(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xFFFF {
                ID_SAVE_BUTTON => {
                    save_and_apply(hwnd);
                    LRESULT(0)
                }
                ID_CLOSE_BUTTON => {
                    // 只隐藏，窗口句柄保留，下次打开更快。
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    LRESULT(0)
                }
                ID_REGISTER_SHELL_BUTTON => {
                    register_shell_menu(hwnd);
                    LRESULT(0)
                }
                ID_UNREGISTER_SHELL_BUTTON => {
                    unregister_shell_menu(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN => {
            // 白色背景 + 深灰文字：分组框、标签、输入框和窗口融为一体，
            // 不会出现系统默认的灰色块。
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut std::ffi::c_void);
            SetBkColor(device, COLORREF(0x00FF_FFFF));
            SetTextColor(device, COLORREF(0x0020_2020));
            LRESULT(crate::util::ui_brush().0 as isize)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE {
                load_values(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 创建窗口里的所有控件。
///
/// 布局思路：用四个分组框把设置项按用途分开
/// （记录保存 / 启动 / 文件监控 / 资源管理器右键菜单），
/// 底部右侧放「保存」「关闭」。用户从上往下扫一遍就知道每项是干什么的，
/// 不会像原来那样一堆文字挤在一起分不清重点。
unsafe fn create_controls(window: HWND) {
    let font = crate::util::ui_font();
    let icon = crate::util::app_icon();
    SendMessageW(
        window,
        WM_SETICON,
        WPARAM(ICON_BIG as usize),
        LPARAM(icon.0 as isize),
    );
    SendMessageW(
        window,
        WM_SETICON,
        WPARAM(ICON_SMALL as usize),
        LPARAM(icon.0 as isize),
    );

    // 三类控件共用的样式，避免每处都重复写一长串。
    let label_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);
    let check_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32);
    let button_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32);
    let edit_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_NUMBER as u32 | ES_AUTOHSCROLL as u32,
    );

    // ---- 分组 1：记录保存 ----
    create_group(window, "记录保存", 16, 12, 512, 84, font);
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "记录保留天数（1 ~ 3650）：",
            label_style,
            34,
            38,
            190,
            22,
            window,
            0,
        ),
        font,
    );
    set_font(
        create_control(
            WS_EX_CLIENTEDGE, // 输入框加立体边框，比 WS_BORDER 更清晰
            "EDIT",
            "",
            edit_style,
            232,
            34,
            90,
            26,
            window,
            ID_RETENTION_EDIT,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "天",
            label_style,
            332,
            38,
            30,
            22,
            window,
            0,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "超过天数的旧记录会被自动删除，并压缩数据库释放空间。",
            label_style,
            34,
            64,
            470,
            20,
            window,
            0,
        ),
        font,
    );

    // ---- 分组 2：启动 ----
    create_group(window, "启动", 16, 104, 512, 62, font);
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "开机自动启动（登录后自动在后台记录）",
            check_style,
            34,
            128,
            460,
            24,
            window,
            ID_AUTOSTART_CHECK,
        ),
        font,
    );

    // ---- 分组 3：文件监控 ----
    create_group(
        window,
        "文件监控（可分别开关，全部关闭时停止监控）",
        16,
        174,
        512,
        154,
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "创建文件监控（记录新建的文件）",
            check_style,
            34,
            196,
            460,
            24,
            window,
            ID_MONITOR_FILE_CHECK,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "创建文件夹监控（记录新建的文件夹）",
            check_style,
            34,
            222,
            460,
            24,
            window,
            ID_MONITOR_DIR_CHECK,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "修改文件监控（记录文件内容被修改，默认关闭）",
            check_style,
            34,
            248,
            460,
            24,
            window,
            ID_MONITOR_MODIFY_CHECK,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "三个开关都关闭后立即停止监控，不再记录任何文件事件，也不占用 CPU；\n修改监控写入较频繁，默认不开启。",
            label_style,
            34,
            276,
            470,
            40,
            window,
            0,
        ),
        font,
    );

    // ---- 分组 4：资源管理器右键菜单 ----
    create_group(window, "资源管理器右键菜单", 16, 336, 512, 88, font);
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "注册后可以在文件 / 文件夹上右键，查看它的创建来源。",
            label_style,
            34,
            356,
            470,
            20,
            window,
            0,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "注册右键菜单",
            button_style,
            34,
            382,
            120,
            30,
            window,
            ID_REGISTER_SHELL_BUTTON,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "注销右键菜单",
            button_style,
            170,
            382,
            120,
            30,
            window,
            ID_UNREGISTER_SHELL_BUTTON,
        ),
        font,
    );

    // ---- 底部按钮：靠右排列，符合 Windows 习惯 ----
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "保存",
            button_style,
            316,
            440,
            100,
            32,
            window,
            ID_SAVE_BUTTON,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "关闭",
            button_style,
            428,
            440,
            100,
            32,
            window,
            ID_CLOSE_BUTTON,
        ),
        font,
    );
}

/// 创建一个分组框（带标题的方框），用来把相关设置归到一组。
unsafe fn create_group(
    window: HWND,
    title: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    font: windows::Win32::Graphics::Gdi::HFONT,
) {
    let control = create_control(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        title,
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_GROUPBOX as u32),
        x,
        y,
        width,
        height,
        window,
        0,
    );
    set_font(control, font);
}

/// 创建一个子控件并返回句柄。
#[allow(clippy::too_many_arguments)]
unsafe fn create_control(
    ex_style: WINDOW_EX_STYLE,
    class: &str,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    id: usize,
) -> HWND {
    let class_name = wide(class);
    let text = wide(text);
    let instance = GetModuleHandleW(None).unwrap_or_default();

    CreateWindowExW(
        ex_style,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(text.as_ptr()),
        style,
        x,
        y,
        width,
        height,
        parent,
        HMENU(id as *mut std::ffi::c_void),
        HINSTANCE(instance.0),
        None,
    )
    .unwrap_or_default()
}

/// 给控件设置统一界面字体（微软雅黑 UI），避免出现很小的旧式点阵字体。
unsafe fn set_font(control: HWND, font: windows::Win32::Graphics::Gdi::HFONT) {
    if !control.0.is_null() {
        SendMessageW(
            control,
            windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
            WPARAM(font.0 as usize),
            LPARAM(1),
        );
    }
}
/// 从配置和注册表读取当前值并填入控件。
fn load_values(window: HWND) {
    let Ok(config) = Config::load_or_default() else {
        log::warn!("[设置] 读取配置失败，跳过界面刷新");
        return;
    };
    log::info!(
        "[设置] 载入：保留 {} 天，监控（创建文件={} 创建文件夹={} 修改文件={}）",
        config.retention_days,
        config.monitor_create_file,
        config.monitor_create_dir,
        config.monitor_modify_file
    );

    unsafe {
        let edit = child(window, ID_RETENTION_EDIT);
        let text = wide(&config.retention_days.to_string());
        let _ = SetWindowTextW(edit, PCWSTR(text.as_ptr()));

        let autostart = child(window, ID_AUTOSTART_CHECK);
        let checked = if autostart_enabled() { 1 } else { 0 };
        SendMessageW(autostart, BM_SETCHECK, WPARAM(checked), LPARAM(0));

        // 三个监控开关分别回填。
        // 以配置里的用户意愿为准：即使因为权限问题没启动成功，
        // 也不应该悄悄把用户的“开启”选择改成关闭。
        let monitor_file = child(window, ID_MONITOR_FILE_CHECK);
        SendMessageW(
            monitor_file,
            BM_SETCHECK,
            WPARAM(if config.monitor_create_file { 1 } else { 0 }),
            LPARAM(0),
        );

        let monitor_dir = child(window, ID_MONITOR_DIR_CHECK);
        SendMessageW(
            monitor_dir,
            BM_SETCHECK,
            WPARAM(if config.monitor_create_dir { 1 } else { 0 }),
            LPARAM(0),
        );

        let monitor_modify = child(window, ID_MONITOR_MODIFY_CHECK);
        SendMessageW(
            monitor_modify,
            BM_SETCHECK,
            WPARAM(if config.monitor_modify_file { 1 } else { 0 }),
            LPARAM(0),
        );
    }
}

/// 读取控件值、保存到配置文件，并立即生效。
fn save_and_apply(window: HWND) {
    let days_text = read_text(window, ID_RETENTION_EDIT);
    let days: i64 = match days_text.trim().parse() {
        Ok(value) if (1..=3650).contains(&value) => value,
        _ => {
            show_warning("保留天数必须是 1 到 3650 之间的整数。");
            return;
        }
    };

    let autostart = read_check(window, ID_AUTOSTART_CHECK);
    let monitor_file = read_check(window, ID_MONITOR_FILE_CHECK);
    let monitor_dir = read_check(window, ID_MONITOR_DIR_CHECK);
    let monitor_modify = read_check(window, ID_MONITOR_MODIFY_CHECK);
    log::info!(
        "[设置] 保存：保留天数文本={:?} 天数={} 开机自启={} 监控（创建文件={} 创建文件夹={} 修改文件={}）",
        days_text,
        days,
        autostart,
        monitor_file,
        monitor_dir,
        monitor_modify
    );

    let mut config = match Config::load_or_default() {
        Ok(config) => config,
        Err(error) => {
            show_warning(&format!("读取配置失败：{:#}", error));
            return;
        }
    };
    config.retention_days = days;
    config.monitor_create_file = monitor_file;
    config.monitor_create_dir = monitor_dir;
    config.monitor_modify_file = monitor_modify;

    if let Err(error) = config.save() {
        show_warning(&format!("保存配置失败：{:#}", error));
        return;
    }

    // 1. 开机自启：写入/删除注册表 Run 项。
    if let Err(error) = set_autostart(autostart) {
        show_warning(&format!("设置开机自启失败：{:#}", error));
    }

    // 2. 监控开关：立即启动或停止 ETW 追踪，三个开关任一开启就继续监控。
    let monitor_any = config.any_monitor_enabled();
    match apply_monitor(&config) {
        Ok(()) => log::info!(
            "[设置] 文件监控已应用（{}），当前状态：{}",
            monitor_summary(&config),
            if monitor_any {
                "监控中"
            } else {
                "已停止"
            }
        ),
        Err(error) => show_warning(&format!("应用监控开关失败：{:#}", error)),
    }

    // 3. 保留天数：立即按新设置清理并压缩一次数据库。
    match Database::init(&config.data_dir) {
        Ok(db) => match db.maintain(days) {
            Ok(deleted) => log::info!("按保留 {} 天清理完成，删除 {} 条旧记录", days, deleted),
            Err(error) => log::warn!("清理旧记录失败：{:#}", error),
        },
        Err(error) => log::warn!("打开数据库失败：{:#}", error),
    }

    unsafe {
        // 以设置窗口为所有者，消息框不会跑到窗口后面。
        let _ = MessageBoxW(
            window,
            PCWSTR(wide("设置已保存并立即生效。").as_ptr()),
            PCWSTR(wide("文件来源追踪").as_ptr()),
            MB_OK,
        );
        let _ = ShowWindow(window, SW_HIDE);
    }
}

/// 按三个开关的当前状态启动或停止文件监控。
///
/// - 三个都关闭：停止 ETW 会话，彻底不再接收事件。
/// - 至少一个开启：重新启动 ETW 会话，让新的过滤条件立即生效
///   （`start_monitoring` 内部会先停止旧会话）。
fn apply_monitor(config: &Config) -> Result<()> {
    if !config.any_monitor_enabled() {
        crate::etw::stop_monitoring();
        return Ok(());
    }

    let db = Database::init(&config.data_dir)?;
    crate::etw::start_monitoring(db, config.clone())?;
    Ok(())
}

/// 生成监控开关的中文描述，用于日志。
fn monitor_summary(config: &Config) -> String {
    let mut parts = Vec::new();
    if config.monitor_create_file {
        parts.push("创建文件");
    }
    if config.monitor_create_dir {
        parts.push("创建文件夹");
    }
    if config.monitor_modify_file {
        parts.push("修改文件");
    }
    if parts.is_empty() {
        "全部关闭".to_string()
    } else {
        parts.join(" / ")
    }
}

/// 注册资源管理器右键菜单，并提示用户是否成功。
fn register_shell_menu(window: HWND) {
    match crate::shell_menu::register_context_menu() {
        Ok(()) => {
            log::info!("[设置] 右键菜单已注册");
            unsafe {
                let text = wide(
                    "右键菜单已注册。\n\n如果资源管理器里暂时看不到，请重启资源管理器或重新登录。",
                );
                let title = wide("文件来源追踪");
                let _ = MessageBoxW(window, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_OK);
            }
        }
        Err(error) => {
            log::error!("[设置] 注册右键菜单失败：{:#}", error);
            show_warning(&format!("注册右键菜单失败：{:#}", error));
        }
    }
}

/// 注销资源管理器右键菜单，并提示用户是否成功。
fn unregister_shell_menu(window: HWND) {
    match crate::shell_menu::unregister_context_menu() {
        Ok(()) => {
            log::info!("[设置] 右键菜单已注销");
            unsafe {
                let text = wide("右键菜单已注销。");
                let title = wide("文件来源追踪");
                let _ = MessageBoxW(window, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_OK);
            }
        }
        Err(error) => {
            log::error!("[设置] 注销右键菜单失败：{:#}", error);
            show_warning(&format!("注销右键菜单失败：{:#}", error));
        }
    }
}

fn child(window: HWND, id: usize) -> HWND {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetDlgItem(window, id as i32).unwrap_or_default()
    }
}

fn read_text(window: HWND, id: usize) -> String {
    let control = child(window, id);
    if control.0.is_null() {
        return String::new();
    }
    let mut buffer = vec![0u16; 64];
    let length = unsafe { GetWindowTextW(control, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn read_check(window: HWND, id: usize) -> bool {
    let control = child(window, id);
    if control.0.is_null() {
        return false;
    }
    unsafe { SendMessageW(control, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1 }
}

fn show_warning(text: &str) {
    let text = wide(text);
    let title = wide("文件来源追踪");
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONWARNING,
        );
    }
}

/// 查询当前用户是否已经设置了开机自启。
fn autostart_enabled() -> bool {
    read_registry_string(RUN_KEY, RUN_VALUE_NAME).is_some()
}

/// 写入或删除开机自启项。
fn set_autostart(enabled: bool) -> Result<()> {
    if !enabled {
        delete_registry_value(RUN_KEY, RUN_VALUE_NAME)?;
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    // 带引号，避免路径中有空格时被系统拆开。
    let command = format!("\"{}\"", exe.to_string_lossy());
    write_registry_string(RUN_KEY, RUN_VALUE_NAME, &command)
}

fn read_registry_string(key_path: &str, value_name: &str) -> Option<String> {
    let key_path_wide = wide(key_path);
    let value_name_wide = wide(value_name);

    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path_wide.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) != windows::Win32::Foundation::ERROR_SUCCESS
        {
            return None;
        }

        let mut buffer = vec![0u8; 4096];
        let mut length = buffer.len() as u32;
        let mut value_type = REG_SZ;
        let status = RegQueryValueExW(
            key,
            PCWSTR(value_name_wide.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buffer.as_mut_ptr()),
            Some(&mut length),
        );
        let _ = RegCloseKey(key);

        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return None;
        }

        let wide_data =
            std::slice::from_raw_parts(buffer.as_ptr().cast::<u16>(), (length as usize) / 2);
        let text = String::from_utf16_lossy(wide_data);
        Some(text.trim_end_matches('\0').to_string())
    }
}

fn write_registry_string(key_path: &str, value_name: &str, value: &str) -> Result<()> {
    let key_path_wide = wide(key_path);
    let value_name_wide = wide(value_name);
    let value_wide = wide(value);

    unsafe {
        let mut key = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path_wide.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(anyhow::anyhow!("打开注册表项失败：0x{:08X}", status.0));
        }

        let bytes =
            std::slice::from_raw_parts(value_wide.as_ptr().cast::<u8>(), value_wide.len() * 2);
        let status = RegSetValueExW(
            key,
            PCWSTR(value_name_wide.as_ptr()),
            0,
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(key);

        if status != windows::Win32::Foundation::ERROR_SUCCESS {
            return Err(anyhow::anyhow!("写入注册表值失败：0x{:08X}", status.0));
        }
    }
    Ok(())
}

fn delete_registry_value(key_path: &str, value_name: &str) -> Result<()> {
    let key_path_wide = wide(key_path);
    let value_name_wide = wide(value_name);

    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path_wide.as_ptr()),
            0,
            KEY_WRITE,
            &mut key,
        ) != windows::Win32::Foundation::ERROR_SUCCESS
        {
            // 项不存在时视为已经关闭，直接返回成功。
            return Ok(());
        }

        let status = RegDeleteValueW(key, PCWSTR(value_name_wide.as_ptr()));
        let _ = RegCloseKey(key);

        if status != windows::Win32::Foundation::ERROR_SUCCESS
            && status != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND
        {
            return Err(anyhow::anyhow!("删除注册表值失败：0x{:08X}", status.0));
        }
    }
    Ok(())
}
