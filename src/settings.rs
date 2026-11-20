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
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
    BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowTextLengthW, GetWindowTextW, LoadCursorW,
    MessageBoxW, RegisterClassExW, SendMessageW, SetWindowTextW, ShowWindow, BS_AUTOCHECKBOX,
    BS_AUTORADIOBUTTON, BS_OWNERDRAW, CW_USEDEFAULT, ES_AUTOHSCROLL, ES_MULTILINE, ES_NUMBER,
    ES_READONLY, HMENU, ICON_BIG, ICON_SMALL, IDC_ARROW, MB_ICONWARNING, MB_OK, SW_HIDE,
    SW_SHOWNORMAL, WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_CTLCOLORBTN, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DRAWITEM, WM_PAINT,
    WM_SETICON, WNDCLASSEXW, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};

use crate::config::Config;
use crate::db::Database;
use crate::storage::{self, StorageLocation, SwitchMode};
use crate::util::wide;

const WINDOW_TITLE: &str = "文件来源追踪 - 设置";
const WINDOW_CLASS: &str = "FileOriginTrackerSettingsWindow";

/// 控件 ID。窗口过程通过它们区分是哪一个控件发来的通知。
///
/// 注意：这里的每个 ID 都必须全局唯一。如果两个控件用了同一个 ID，
/// `WM_COMMAND` 无法区分是谁发的通知，会出现「点 A 控件却触发 B 功能」的怪现象。
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
/// 监控开关：删除文件 / 删除文件夹。
///
/// 这里使用 2013 / 2014，是因为 2011、2012 已经被下面的
/// 「白名单单选」和「名单管理按钮」占用。早期版本误用了重复 ID，
/// 导致点击这两个复选框会触发别的按钮功能。
const ID_MONITOR_DELETE_FILE_CHECK: usize = 2013;
const ID_MONITOR_DELETE_DIR_CHECK: usize = 2014;
/// 注册 / 注销资源管理器右键菜单（从托盘菜单移到这里）。
const ID_REGISTER_SHELL_BUTTON: usize = 2008;
const ID_UNREGISTER_SHELL_BUTTON: usize = 2009;
/// 黑白名单模式：黑名单 / 白名单单选框。
const ID_BLACKLIST_RADIO: usize = 2010;
const ID_WHITELIST_RADIO: usize = 2011;
/// 打开名单管理窗口。
const ID_MANAGE_LIST_BUTTON: usize = 2012;
/// 打开「关于」对话框。
const ID_ABOUT_BUTTON: usize = 2015;
/// 存储位置：共用 / 独立目录。
const ID_SHARED_PATH_RADIO: usize = 2016;
const ID_SEPARATE_PATH_RADIO: usize = 2017;
const ID_SHARED_PATH_EDIT: usize = 2018;
const ID_CONFIG_PATH_EDIT: usize = 2019;
const ID_DATA_PATH_EDIT: usize = 2020;
const ID_BROWSE_SHARED_BUTTON: usize = 2021;
const ID_BROWSE_CONFIG_BUTTON: usize = 2022;
const ID_BROWSE_DATA_BUTTON: usize = 2023;
const ID_PROGRAM_PATH_BUTTON: usize = 2024;
const ID_APPDATA_PATH_BUTTON: usize = 2025;
const ID_MIGRATE_RADIO: usize = 2026;
const ID_FRESH_RADIO: usize = 2027;
const ID_CANCEL_SWITCH_BUTTON: usize = 2028;
const ID_SHARED_PATH_LABEL: usize = 2029;
const ID_CONFIG_PATH_LABEL: usize = 2030;
const ID_DATA_PATH_LABEL: usize = 2031;
const ID_SWITCH_STATUS_LABEL: usize = 2032;
const ID_PREVIOUS_PATH_EDIT: usize = 2033;

/// 开机自启使用的注册表位置：当前用户登录时自动运行。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE_NAME: &str = "FileOriginTracker";

static SETTINGS_WINDOW: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 各分组框的位置与标题。`WM_PAINT` 里按这份表统一绘制，
/// 和创建控件的坐标保持一致（改坐标时两处要一起改）。
const SECTIONS: [(&str, i32, i32, i32, i32); 6] = [
    ("记录保存", 16, 12, 512, 84),
    ("启动", 16, 104, 512, 62),
    ("文件监控（可分别开关，全部关闭时停止监控）", 16, 174, 512, 206),
    ("黑白名单模式", 16, 388, 512, 80),
    ("资源管理器右键菜单", 16, 476, 512, 88),
    ("存储位置（路径切换在重启后生效）", 544, 12, 512, 552),
];

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
            1096,
            700,
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
        WM_PAINT => {
            // 自己画分组框：系统的 BS_GROUPBOX 带 3D 凹凸边，
            // 和几何风格不搭，这里用细线 + 左上角标题代替。
            let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
            let device = windows::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut paint);
            let mut client = windows::Win32::Foundation::RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
            crate::theme::fill(device, &client, crate::theme::BG);
            for (title, x, y, width, height) in SECTIONS {
                let rect = windows::Win32::Foundation::RECT {
                    left: x,
                    top: y,
                    right: x + width,
                    bottom: y + height,
                };
                crate::theme::draw_section(device, &rect, title);
            }
            let _ = windows::Win32::Graphics::Gdi::EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_DRAWITEM => {
            // 所有普通按钮统一画成平面几何风格；「保存」是主按钮。
            let info = &*(lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT);
            // 自绘复选框/单选框不归这里画（它们由 theme 的子类负责）。
            if crate::theme::is_themed_control(info.hwndItem) {
                return LRESULT(1);
            }
            let id = (info.CtlID & 0xFFFF) as usize;
            crate::theme::draw_button_auto(info, id == ID_SAVE_BUTTON);
            LRESULT(1)
        }
        WM_COMMAND => {
            // 先补上自绘复选框/单选框的点击切换（owner-draw 不会自动切换）。
            crate::theme::apply_click_state(wparam, lparam);
            match wparam.0 & 0xFFFF {
                ID_SAVE_BUTTON => {
                    save_and_apply(hwnd);
                    LRESULT(0)
                }
                ID_SHARED_PATH_RADIO | ID_SEPARATE_PATH_RADIO => {
                    change_path_mode(hwnd);
                    LRESULT(0)
                }
                ID_BROWSE_SHARED_BUTTON => {
                    browse_into(hwnd, ID_SHARED_PATH_EDIT, "选择配置和数据库共用目录");
                    LRESULT(0)
                }
                ID_BROWSE_CONFIG_BUTTON => {
                    browse_into(hwnd, ID_CONFIG_PATH_EDIT, "选择配置目录");
                    LRESULT(0)
                }
                ID_BROWSE_DATA_BUTTON => {
                    browse_into(hwnd, ID_DATA_PATH_EDIT, "选择数据库目录");
                    LRESULT(0)
                }
                ID_PROGRAM_PATH_BUTTON => {
                    match storage::program_data_dir() {
                        Ok(path) => fill_shortcut_path(hwnd, path),
                        Err(error) => show_warning(&format!("无法确定程序目录：{:#}", error)),
                    }
                    LRESULT(0)
                }
                ID_APPDATA_PATH_BUTTON => {
                    match storage::default_location() {
                        Ok(location) => fill_shortcut_path(hwnd, location.config_dir),
                        Err(error) => show_warning(&format!("无法确定默认目录：{:#}", error)),
                    }
                    LRESULT(0)
                },
                ID_CANCEL_SWITCH_BUTTON => {
                    match storage::cancel_pending_switch() {
                        Ok(()) => {
                            load_values(hwnd);
                            show_warning("已取消待重启执行的路径切换，当前位置没有改变。");
                        }
                        Err(error) => show_warning(&format!("取消路径切换失败：{:#}", error)),
                    }
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
                ID_MANAGE_LIST_BUTTON => {
                    if let Err(error) = crate::list_window::show() {
                        log::error!("打开名单管理窗口失败：{:#}", error);
                    }
                    LRESULT(0)
                }
                ID_ABOUT_BUTTON => {
                    // 弹出「关于」对话框（模态，直到用户关闭才返回）。
                    crate::about::show(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORBTN => {
            // 白色背景 + 深灰文字：标签、输入框和窗口融为一体，
            // 不会出现系统默认的灰色块。
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut std::ffi::c_void);
            SetBkColor(device, crate::theme::BG);
            SetTextColor(device, crate::theme::TEXT);
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
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_OWNERDRAW as u32);
    let edit_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_NUMBER as u32 | ES_AUTOHSCROLL as u32,
    );

    // ---- 分组 1：记录保存 ----
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
            "BUTTON",
            "删除文件监控（记录被删除的文件）",
            check_style,
            34,
            274,
            460,
            24,
            window,
            ID_MONITOR_DELETE_FILE_CHECK,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "删除文件夹监控（记录被删除的文件夹）",
            check_style,
            34,
            300,
            460,
            24,
            window,
            ID_MONITOR_DELETE_DIR_CHECK,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "五个开关都关闭后立即停止监控，不再记录任何文件事件，也不占用 CPU；\n修改监控写入较频繁，默认不开启。",
            label_style,
            34,
            328,
            470,
            40,
            window,
            0,
        ),
        font,
    );

    // ---- 分组 4：名单模式 ----
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "黑名单",
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32 | 0x8, // WS_GROUP
            ),
            34,
            404,
            80,
            24,
            window,
            ID_BLACKLIST_RADIO,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "白名单",
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32),
            120,
            404,
            80,
            24,
            window,
            ID_WHITELIST_RADIO,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "黑名单：不记录名单里的程序 / 文件夹\n白名单：只记录名单里的程序 / 文件夹",
            label_style,
            34,
            428,
            350,
            36,
            window,
            0,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "管理名单…",
            button_style,
            400,
            428,
            110,
            30,
            window,
            ID_MANAGE_LIST_BUTTON,
        ),
        font,
    );

    // ---- 分组 5：资源管理器右键菜单 ----
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "STATIC",
            "注册后可以在文件 / 文件夹上右键，查看它的创建来源。",
            label_style,
            34,
            496,
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
            522,
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
            522,
            120,
            30,
            window,
            ID_UNREGISTER_SHELL_BUTTON,
        ),
        font,
    );

    // ---- 右栏：配置与数据库存储位置 ----
    create_storage_controls(window, font);

    // ---- 底部按钮：从左到右依次是「关于」「保存」「关闭」，符合 Windows 习惯 ----
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "关于",
            button_style,
            204,
            600,
            100,
            32,
            window,
            ID_ABOUT_BUTTON,
        ),
        font,
    );
    set_font(
        create_control(
            WINDOW_EX_STYLE::default(),
            "BUTTON",
            "保存",
            button_style,
            316,
            600,
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
            600,
            100,
            32,
            window,
            ID_CLOSE_BUTTON,
        ),
        font,
    );

    // 复选框 / 单选框统一换成几何风格的自绘外观（点击与取值逻辑不变）。
    crate::theme::style_children(window);
}

/// 创建右栏的存储位置控件。路径编辑框不支持数字限制，可输入完整 Windows 路径。
unsafe fn create_storage_controls(
    window: HWND,
    font: windows::Win32::Graphics::Gdi::HFONT,
) {
    let label_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);
    let button_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_OWNERDRAW as u32);
    let radio_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32,
    );
    let grouped_radio_style = WINDOW_STYLE(radio_style.0 | 0x8); // WS_GROUP：开始新单选组
    let path_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL as u32);

    // 这个小闭包只负责“创建后设置字体”，让下面的坐标和文字更容易核对。
    let add = |ex_style, class, text, style, x, y, width, height, id| unsafe {
        let control = create_control(
            ex_style, class, text, style, x, y, width, height, window, id,
        );
        set_font(control, font);
        control
    };

    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "配置和数据库共用同一目录",
        grouped_radio_style,
        562,
        34,
        250,
        24,
        ID_SHARED_PATH_RADIO,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "使用两个独立路径",
        radio_style,
        820,
        34,
        210,
        24,
        ID_SEPARATE_PATH_RADIO,
    );

    add(
        WINDOW_EX_STYLE::default(),
        "STATIC",
        "共用目录：",
        label_style,
        562,
        72,
        90,
        22,
        ID_SHARED_PATH_LABEL,
    );
    add(
        WS_EX_CLIENTEDGE,
        "EDIT",
        "",
        path_style,
        654,
        66,
        300,
        26,
        ID_SHARED_PATH_EDIT,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "浏览…",
        button_style,
        962,
        64,
        76,
        30,
        ID_BROWSE_SHARED_BUTTON,
    );

    add(
        WINDOW_EX_STYLE::default(),
        "STATIC",
        "配置目录：",
        label_style,
        562,
        72,
        90,
        22,
        ID_CONFIG_PATH_LABEL,
    );
    add(
        WS_EX_CLIENTEDGE,
        "EDIT",
        "",
        path_style,
        654,
        66,
        300,
        26,
        ID_CONFIG_PATH_EDIT,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "浏览…",
        button_style,
        962,
        64,
        76,
        30,
        ID_BROWSE_CONFIG_BUTTON,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "STATIC",
        "数据库目录：",
        label_style,
        562,
        112,
        90,
        22,
        ID_DATA_PATH_LABEL,
    );
    add(
        WS_EX_CLIENTEDGE,
        "EDIT",
        "",
        path_style,
        654,
        106,
        300,
        26,
        ID_DATA_PATH_EDIT,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "浏览…",
        button_style,
        962,
        104,
        76,
        30,
        ID_BROWSE_DATA_BUTTON,
    );

    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "程序目录下的数据文件夹",
        button_style,
        562,
        150,
        230,
        30,
        ID_PROGRAM_PATH_BUTTON,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "默认 AppData\\Local 目录",
        button_style,
        804,
        150,
        234,
        30,
        ID_APPDATA_PATH_BUTTON,
    );

    add(
        WINDOW_EX_STYLE::default(),
        "STATIC",
        "路径切换方式：",
        label_style,
        562,
        196,
        200,
        22,
        0,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "迁移现有配置和记录，成功后删除原文件",
        grouped_radio_style,
        562,
        220,
        470,
        24,
        ID_MIGRATE_RADIO,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "保留设置并创建空数据库，不删除原位置数据",
        radio_style,
        562,
        248,
        470,
        24,
        ID_FRESH_RADIO,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "STATIC",
        "",
        label_style,
        562,
        282,
        470,
        44,
        ID_SWITCH_STATUS_LABEL,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "BUTTON",
        "取消待生效切换",
        button_style,
        562,
        334,
        180,
        30,
        ID_CANCEL_SWITCH_BUTTON,
    );
    add(
        WINDOW_EX_STYLE::default(),
        "STATIC",
        "最近一次重新记录前的位置（可复制）：",
        label_style,
        562,
        378,
        400,
        22,
        0,
    );
    add(
        WS_EX_CLIENTEDGE,
        "EDIT",
        "",
        WINDOW_STYLE(
            WS_CHILD.0
                | WS_VISIBLE.0
                | WS_TABSTOP.0
                | WS_VSCROLL.0
                | ES_MULTILINE as u32
                | ES_READONLY as u32,
        ),
        562,
        404,
        476,
        122,
        ID_PREVIOUS_PATH_EDIT,
    );
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
        "[设置] 载入：保留 {} 天，监控（创建文件={} 创建文件夹={} 修改文件={} 删除文件={} 删除文件夹={}）",
        config.retention_days,
        config.monitor_create_file,
        config.monitor_create_dir,
        config.monitor_modify_file,
        config.monitor_delete_file,
        config.monitor_delete_dir
    );

    unsafe {
        let edit = child(window, ID_RETENTION_EDIT);
        let text = wide(&config.retention_days.to_string());
        let _ = SetWindowTextW(edit, PCWSTR(text.as_ptr()));

        let autostart = child(window, ID_AUTOSTART_CHECK);
        // 勾选状态统一交给主题保管（控件被改成了 owner-draw）。
        crate::theme::set_checked(autostart, autostart_enabled());

        // 三个监控开关分别回填。
        // 以配置里的用户意愿为准：即使因为权限问题没启动成功，
        // 也不应该悄悄把用户的“开启”选择改成关闭。
        crate::theme::set_checked(child(window, ID_MONITOR_FILE_CHECK), config.monitor_create_file);
        crate::theme::set_checked(child(window, ID_MONITOR_DIR_CHECK), config.monitor_create_dir);
        crate::theme::set_checked(
            child(window, ID_MONITOR_MODIFY_CHECK),
            config.monitor_modify_file,
        );
        crate::theme::set_checked(
            child(window, ID_MONITOR_DELETE_FILE_CHECK),
            config.monitor_delete_file,
        );
        crate::theme::set_checked(
            child(window, ID_MONITOR_DELETE_DIR_CHECK),
            config.monitor_delete_dir,
        );

        // 黑白名单模式：两个单选框只能有一个选中。
        let white = config.is_white_list();
        crate::theme::set_checked(child(window, ID_BLACKLIST_RADIO), !white);
        crate::theme::set_checked(child(window, ID_WHITELIST_RADIO), white);
    }

    if let Err(error) = load_storage_values(window) {
        log::warn!("[设置] 读取存储位置失败：{:#}", error);
        show_warning(&format!("读取存储位置失败：{:#}", error));
    }
}

/// 回填存储路径。有待切换任务时显示目标路径，否则显示当前生效路径。
fn load_storage_values(window: HWND) -> Result<()> {
    let state = storage::load_state()?;
    let (location, mode, pending) = match state.pending.as_ref() {
        Some(pending) => (pending.target.clone(), pending.mode, true),
        None => (state.active.clone(), SwitchMode::Migrate, false),
    };
    let shared = storage::paths_equal(&location.config_dir, &location.data_dir);

    unsafe {
        crate::theme::set_checked(child(window, ID_SHARED_PATH_RADIO), shared);
        crate::theme::set_checked(child(window, ID_SEPARATE_PATH_RADIO), !shared);
        crate::theme::set_checked(
            child(window, ID_MIGRATE_RADIO),
            mode == SwitchMode::Migrate,
        );
        crate::theme::set_checked(child(window, ID_FRESH_RADIO), mode == SwitchMode::Fresh);
    }

    set_text(
        window,
        ID_SHARED_PATH_EDIT,
        &location.config_dir.to_string_lossy(),
    );
    set_text(
        window,
        ID_CONFIG_PATH_EDIT,
        &location.config_dir.to_string_lossy(),
    );
    set_text(
        window,
        ID_DATA_PATH_EDIT,
        &location.data_dir.to_string_lossy(),
    );

    let status = if pending {
        format!(
            "已保存待切换任务（{}）。\n完全退出并重启主程序后生效，当前仍使用原位置。",
            if mode == SwitchMode::Migrate {
                "迁移数据"
            } else {
                "重新记录"
            }
        )
    } else {
        "当前显示的是正在使用的存储位置。".to_string()
    };
    set_text(window, ID_SWITCH_STATUS_LABEL, &status);

    let previous = state.previous.map(|location| {
        if storage::paths_equal(&location.config_dir, &location.data_dir) {
            format!("原共用目录：{}", location.config_dir.display())
        } else {
            format!(
                "原配置目录：{}\r\n原数据库目录：{}",
                location.config_dir.display(),
                location.data_dir.display()
            )
        }
    });
    set_text(
        window,
        ID_PREVIOUS_PATH_EDIT,
        previous.as_deref().unwrap_or("暂无记录。"),
    );

    unsafe {
        let _ = ShowWindow(
            child(window, ID_CANCEL_SWITCH_BUTTON),
            if pending { SW_SHOWNORMAL } else { SW_HIDE },
        );
    }
    update_path_mode(window);
    Ok(())
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
    let monitor_delete_file = read_check(window, ID_MONITOR_DELETE_FILE_CHECK);
    let monitor_delete_dir = read_check(window, ID_MONITOR_DELETE_DIR_CHECK);
    // 白名单单选框被选中就是白名单模式，否则黑名单。
    let list_mode = if read_check(window, ID_WHITELIST_RADIO) {
        crate::list_rules::MODE_WHITE
    } else {
        crate::list_rules::MODE_BLACK
    };
    log::info!(
        "[设置] 保存：保留天数文本={:?} 天数={} 开机自启={} 监控（创建文件={} 创建文件夹={} 修改文件={} 删除文件={} 删除文件夹={}）名单模式={}",
        days_text,
        days,
        autostart,
        monitor_file,
        monitor_dir,
        monitor_modify,
        monitor_delete_file,
        monitor_delete_dir,
        list_mode
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
    config.monitor_delete_file = monitor_delete_file;
    config.monitor_delete_dir = monitor_delete_dir;
    config.list_mode = list_mode.to_string();

    let storage_state = match storage::load_state() {
        Ok(state) => state,
        Err(error) => {
            show_warning(&format!("读取存储位置失败：{:#}", error));
            return;
        }
    };
    let target = selected_storage_location(window);
    let path_changed = !storage::paths_equal(&target.config_dir, &storage_state.active.config_dir)
        || !storage::paths_equal(&target.data_dir, &storage_state.active.data_dir);
    let switch_mode = if read_check(window, ID_FRESH_RADIO) {
        SwitchMode::Fresh
    } else {
        SwitchMode::Migrate
    };
    let had_pending = storage_state.pending.is_some();
    let mut path_error = None;

    // 路径恢复成当前值时，同时撤销尚未执行的任务。
    if !path_changed && had_pending {
        if let Err(error) = storage::cancel_pending_switch() {
            path_error = Some(format!("取消待切换任务失败：{error:#}"));
        }
    }

    if let Err(error) = config.save() {
        show_warning(&format!("保存配置失败：{:#}", error));
        return;
    }

    // 修改已有任务时先撤销旧任务；新目标校验失败后就不会意外执行旧任务。
    if path_changed && had_pending && path_error.is_none() {
        if let Err(error) = storage::cancel_pending_switch() {
            path_error = Some(format!("更新待切换任务失败：{error:#}"));
        }
    }

    // 只登记任务，绝不在监控和数据库仍运行时搬动文件。
    if path_changed && path_error.is_none() {
        if let Err(error) = storage::schedule_switch(target, switch_mode, &config) {
            path_error = Some(format!("保存路径切换任务失败：{error:#}"));
        }
    }
    let restart_required = path_changed && path_error.is_none();
    let switch_cancelled = !path_changed && had_pending && path_error.is_none();

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

    // 白名单为空时不会记录任何文件，这里额外提醒一次，避免用户以为程序坏了。
    let white_list_empty = config.is_white_list()
        && Database::init(&config.data_dir)
            .and_then(|db| db.list_entries(crate::list_rules::MODE_WHITE))
            .map(|entries| entries.is_empty())
            .unwrap_or(false);

    if let Some(error) = path_error {
        show_warning(&format!(
            "普通设置已经保存并应用，但存储路径没有改变。\n\n{error}"
        ));
        return;
    }

    let mut message = if restart_required {
        "普通设置已保存并立即生效。\n\n路径切换将在完全退出并重新启动主程序后执行。\n重启前仍使用原配置和数据库。".to_string()
    } else if switch_cancelled {
        "设置已保存，待重启执行的路径切换已取消。".to_string()
    } else {
        "设置已保存并立即生效。".to_string()
    };
    if white_list_empty {
        message.push_str(
            "\n\n注意：当前是白名单模式，但白名单还是空的，\n所以现在不会记录任何文件。\n请点「管理名单…」添加程序或文件夹。",
        );
    }

    unsafe {
        // 以设置窗口为所有者，消息框不会跑到窗口后面。
        let _ = MessageBoxW(
            window,
            PCWSTR(wide(&message).as_ptr()),
            PCWSTR(wide("文件来源追踪").as_ptr()),
            MB_OK,
        );
        let _ = ShowWindow(window, SW_HIDE);
    }
}

/// 按监控开关的当前状态启动或停止文件监控。
///
/// - 全部关闭：停止 ETW 会话，彻底不再接收事件。
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
    if config.monitor_delete_file {
        parts.push("删除文件");
    }
    if config.monitor_delete_dir {
        parts.push("删除文件夹");
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

/// 按共用 / 独立模式读取用户当前填写的目录。
fn selected_storage_location(window: HWND) -> StorageLocation {
    if read_check(window, ID_SHARED_PATH_RADIO) {
        let directory = PathBuf::from(read_text(window, ID_SHARED_PATH_EDIT).trim());
        StorageLocation {
            config_dir: directory.clone(),
            data_dir: directory,
        }
    } else {
        StorageLocation {
            config_dir: PathBuf::from(read_text(window, ID_CONFIG_PATH_EDIT).trim()),
            data_dir: PathBuf::from(read_text(window, ID_DATA_PATH_EDIT).trim()),
        }
    }
}

/// 用户主动切换模式时携带当前输入，避免隐藏编辑框里的旧值覆盖新路径。
fn change_path_mode(window: HWND) {
    if read_check(window, ID_SHARED_PATH_RADIO) {
        let config_path = read_text(window, ID_CONFIG_PATH_EDIT);
        if !config_path.trim().is_empty() {
            set_text(window, ID_SHARED_PATH_EDIT, &config_path);
        }
    } else {
        let shared_path = read_text(window, ID_SHARED_PATH_EDIT);
        if !shared_path.trim().is_empty() {
            set_text(window, ID_CONFIG_PATH_EDIT, &shared_path);
            set_text(window, ID_DATA_PATH_EDIT, &shared_path);
        }
    }
    update_path_mode(window);
}

/// 切换共用 / 独立模式时，只显示对应的路径行，避免两套输入框重叠。
fn update_path_mode(window: HWND) {
    let shared = read_check(window, ID_SHARED_PATH_RADIO);
    for id in [
        ID_SHARED_PATH_LABEL,
        ID_SHARED_PATH_EDIT,
        ID_BROWSE_SHARED_BUTTON,
    ] {
        unsafe {
            let _ = ShowWindow(child(window, id), if shared { SW_SHOWNORMAL } else { SW_HIDE });
        }
    }
    for id in [
        ID_CONFIG_PATH_LABEL,
        ID_CONFIG_PATH_EDIT,
        ID_BROWSE_CONFIG_BUTTON,
        ID_DATA_PATH_LABEL,
        ID_DATA_PATH_EDIT,
        ID_BROWSE_DATA_BUTTON,
    ] {
        unsafe {
            let _ = ShowWindow(child(window, id), if shared { SW_HIDE } else { SW_SHOWNORMAL });
        }
    }
}

/// 快捷目录只负责填值，不会立即创建目录或移动数据。
fn fill_shortcut_path(window: HWND, path: PathBuf) {
    let text = path.to_string_lossy();
    if read_check(window, ID_SHARED_PATH_RADIO) {
        set_text(window, ID_SHARED_PATH_EDIT, &text);
    } else {
        set_text(window, ID_CONFIG_PATH_EDIT, &text);
        set_text(window, ID_DATA_PATH_EDIT, &text);
    }
}

/// 使用系统文件夹选择器选择目录，并填入指定编辑框。
fn browse_into(window: HWND, edit_id: usize, title: &str) {
    let mut display_name = [0u16; 260];
    let title = wide(title);
    let mut browse = BROWSEINFOW::default();
    browse.hwndOwner = window;
    browse.pszDisplayName = PWSTR(display_name.as_mut_ptr());
    browse.lpszTitle = PCWSTR(title.as_ptr());
    browse.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;

    let pidl = unsafe { SHBrowseForFolderW(&browse) };
    if pidl.is_null() {
        return;
    }
    let mut buffer = [0u16; 260];
    let ok = unsafe { SHGetPathFromIDListW(pidl, &mut buffer) };
    unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
    if !ok.as_bool() {
        return;
    }

    let length = buffer
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..length]);
    if !path.is_empty() {
        set_text(window, edit_id, &path);
    }
}

fn set_text(window: HWND, id: usize, text: &str) {
    let control = child(window, id);
    if control.0.is_null() {
        return;
    }
    let text = wide(text);
    unsafe {
        let _ = SetWindowTextW(control, PCWSTR(text.as_ptr()));
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
    // 路径可能长于普通设置文本，按控件实际长度分配缓冲区。
    let capacity = unsafe { GetWindowTextLengthW(control) }.max(0) as usize + 1;
    let mut buffer = vec![0u16; capacity];
    let length = unsafe { GetWindowTextW(control, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn read_check(window: HWND, id: usize) -> bool {
    let control = child(window, id);
    if control.0.is_null() {
        return false;
    }
    // 勾选状态由主题保管（owner-draw 控件不能用 BM_GETCHECK）。
    unsafe { crate::theme::is_checked(control) }
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

