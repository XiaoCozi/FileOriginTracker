//! 「关于」对话框。
//!
//! 在设置窗口底部点「关于」按钮会弹出本窗口，集中展示项目的基本信息：
//! 项目名称、版本、用途、GitHub 项目链接、作者和开源协议。
//!
//! 其中 GitHub 链接做成了可点击的文字（蓝色 + `SS_NOTIFY` 样式），
//! 点击后会调用系统默认浏览器打开项目主页。
//!
//! 本窗口是**模态**的：弹出后会在本线程单独跑一个消息循环，
//! 直到用户关闭窗口才返回，和 `note.rs` 的备注编辑窗口用法一致。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetDlgItem, IsWindow, LoadCursorW, RegisterClassExW, SendMessageW, ShowWindow,
    TranslateMessage, CW_USEDEFAULT, HMENU, IDC_ARROW, ICON_BIG, ICON_SMALL, MSG, SW_SHOWNORMAL,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC, WM_SETFONT,
    WM_SETICON, WNDCLASSEXW, BS_PUSHBUTTON, WS_CAPTION, WS_CHILD, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE,
};

use crate::util::wide;

/// 窗口标题。
const WINDOW_TITLE: &str = "关于 FileOriginTracker";
/// 窗口类名，注册到系统时用，必须全局唯一。
const WINDOW_CLASS: &str = "FileOriginTrackerAboutWindow";

/// 控件 ID。
///
/// - `ID_GITHUB_LINK`：GitHub 链接静态控件，点击时通过 `WM_COMMAND` 通知窗口过程。
/// - `ID_OK_BUTTON`：确定按钮，关闭对话框。
const ID_GITHUB_LINK: usize = 4001;
const ID_OK_BUTTON: usize = 4002;

/// 项目信息常量，集中放在一起方便以后修改。
const PROJECT_NAME: &str = "FileOriginTracker（文件来源追踪）";
const PROJECT_URL: &str = "https://github.com/XiaoCozi/FileOriginTracker";
const PROJECT_AUTHOR: &str = "葱籽";
const PROJECT_LICENSE: &str = "MIT 协议";
/// 用途说明：简短描述项目是干什么的。`\n` 让文字分成两行显示。
const PROJECT_PURPOSE: &str =
    "记录每个新建 / 修改 / 删除的文件或文件夹是由哪个程序操作的，\n并可通过资源管理器右键菜单查询来源。";

/// 静态控件样式常量。
///
/// windows crate 0.58 没有直接导出 `SS_*` 常量名，
/// 这里用 Win32 官方数值：`SS_LEFT=0x0`（左对齐）、`SS_NOTIFY=0x100`（可发送点击通知）。
const SS_LEFT: u32 = 0x0000_0000;
const SS_NOTIFY: u32 = 0x0000_0100;

/// 窗口类是否已经注册过，避免重复注册报错。
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 弹出「关于」对话框。
///
/// 这是一个模态调用：函数会一直阻塞到用户关闭对话框才返回，
/// 调用方无需处理返回值。
pub fn show(owner: HWND) {
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let instance = HINSTANCE(module.0);
    if !register_class(instance) {
        return;
    }

    let class_name = wide(WINDOW_CLASS);
    let title = wide(WINDOW_TITLE);

    let hwnd = unsafe {
        CreateWindowExW(
            // `WS_EX_TOPMOST`：让关于窗口浮在设置窗口上方，不会被挡住。
            WINDOW_EX_STYLE(0x0000_0008),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            340,
            owner,
            HMENU::default(),
            instance,
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        return;
    };

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }

    // 模态：在本线程单独跑一个消息循环，直到窗口被销毁。
    // 这样设置窗口会暂时失去响应，必须先关掉关于窗口才能继续操作。
    let mut message = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        if !ret.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if unsafe { IsWindow(hwnd) }.0 == 0 {
            break;
        }
    }
}

/// 注册窗口类。只在第一次调用时真正注册，后续直接返回 true。
fn register_class(instance: HINSTANCE) -> bool {
    if CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
        return true;
    }

    let class_name = wide(WINDOW_CLASS);
    let mut window_class = WNDCLASSEXW::default();
    window_class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    window_class.lpfnWndProc = Some(window_procedure);
    window_class.hInstance = instance;
    // 白色背景 + 统一字体，与其他窗口观感一致。
    window_class.hbrBackground = crate::util::ui_brush();
    window_class.hCursor = unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default() };
    window_class.lpszClassName = PCWSTR(class_name.as_ptr());

    if unsafe { RegisterClassExW(&window_class) } == 0 {
        CLASS_REGISTERED.store(false, Ordering::SeqCst);
        return false;
    }
    true
}

/// 窗口过程：处理窗口创建、按钮点击、控件颜色等消息。
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
            let id = (wparam.0 & 0xFFFF) as usize;
            match id {
                // 点击了 GitHub 链接 → 用默认浏览器打开项目主页。
                ID_GITHUB_LINK => open_url(PROJECT_URL),
                // 点击了确定按钮 → 销毁窗口（模态循环随之退出）。
                ID_OK_BUTTON => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // 统一背景为白色。同时把 GitHub 链接控件的文字设成蓝色，
            // 让它看起来像一个超链接；其他文字保持深灰。
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut std::ffi::c_void);
            let control = HWND(lparam.0 as *mut std::ffi::c_void);
            let link = GetDlgItem(hwnd, ID_GITHUB_LINK as i32).unwrap_or_default();

            if control == link {
                // 蓝色（COLORREF 是 BGR 顺序，所以 0x00RRGGBB 里蓝色在最后）。
                SetTextColor(device, COLORREF(0x0000_00FF));
            } else {
                SetTextColor(device, COLORREF(0x0020_2020));
            }
            SetBkColor(device, COLORREF(0x00FF_FFFF));
            LRESULT(crate::util::ui_brush().0 as isize)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 创建窗口里的所有文字和按钮。
unsafe fn create_controls(window: HWND) {
    let font = crate::util::ui_font();

    // 设置窗口图标（任务栏和标题栏左上角），与主程序保持一致。
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

    // 普通文字样式。
    let label_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);
    // 链接样式：`SS_NOTIFY` 让静态控件可以发送 `WM_COMMAND` 通知，
    // 这样点击链接时窗口过程能收到消息并打开浏览器。
    let link_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | SS_NOTIFY | SS_LEFT);
    // 按钮样式。
    let button_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32);

    // —— 项目名称（标题）——
    create_static(window, PROJECT_NAME, 20, 16, 440, 24, font, label_style, 0);

    // —— 版本号 ——
    // `env!("CARGO_PKG_VERSION")` 在编译期把 Cargo.toml 里的版本号嵌进来，
    // 所以改版本号后重新编译，关于窗口里的版本会自动同步。
    let version = format!("版本：v{}", env!("CARGO_PKG_VERSION"));
    create_static(window, &version, 20, 44, 440, 20, font, label_style, 0);

    // —— 用途 ——
    create_static(window, "用途：", 20, 76, 60, 20, font, label_style, 0);
    create_static(window, PROJECT_PURPOSE, 80, 76, 380, 60, font, label_style, 0);

    // —— GitHub 项目链接（可点击）——
    create_static(window, "GitHub 项目：", 20, 146, 100, 20, font, label_style, 0);
    create_static(
        window,
        PROJECT_URL,
        120,
        146,
        340,
        20,
        font,
        link_style,
        ID_GITHUB_LINK,
    );

    // —— 作者 ——
    create_static(window, "作者：", 20, 176, 60, 20, font, label_style, 0);
    create_static(window, PROJECT_AUTHOR, 80, 176, 380, 20, font, label_style, 0);

    // —— 开源协议 ——
    create_static(window, "开源协议：", 20, 206, 80, 20, font, label_style, 0);
    create_static(window, PROJECT_LICENSE, 100, 206, 360, 20, font, label_style, 0);

    // —— 确定按钮 ——
    let button_class = wide("BUTTON");
    let ok_label = wide("确定");
    if let Ok(button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(ok_label.as_ptr()),
        button_style,
        190, // 水平居中：(480 - 100) / 2 = 190
        250, // 客户区高度约 301px，按钮底部 282px，下方留约 19px 边距
        100,
        32,
        window,
        HMENU(ID_OK_BUTTON as *mut std::ffi::c_void),
        HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0),
        None,
    ) {
        SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

/// 创建一个静态文字控件。
///
/// `id` 传 0 表示普通标签；传 `ID_GITHUB_LINK` 表示可点击链接。
unsafe fn create_static(
    window: HWND,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    font: windows::Win32::Graphics::Gdi::HFONT,
    style: WINDOW_STYLE,
    id: usize,
) {
    let class = wide("STATIC");
    let text = wide(text);
    let instance = GetModuleHandleW(None).unwrap_or_default();

    if let Ok(control) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class.as_ptr()),
        PCWSTR(text.as_ptr()),
        style,
        x,
        y,
        width,
        height,
        window,
        HMENU(id as *mut std::ffi::c_void),
        HINSTANCE(instance.0),
        None,
    ) {
        SendMessageW(control, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

/// 用系统默认浏览器打开一个 URL。
///
/// `ShellExecuteW` 的 `lpOperation` 传 "open"，系统会自动找到默认浏览器打开链接。
/// 返回值小于 32 表示失败，但这里不弹窗报错——点链接失败最多就是没反应，
/// 用户可以自己复制地址栏里的链接访问。
fn open_url(url: &str) {
    let operation = wide("open");
    let url_wide = wide(url);
    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            PCWSTR(operation.as_ptr()),
            PCWSTR(url_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
