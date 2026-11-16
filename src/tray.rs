//! 系统托盘模块。
//!
//! 托盘使用一个不可见窗口接收 Windows 消息。没有操作时线程处于阻塞等待状态，
//! 不占用 CPU。托盘菜单提供：查看最近记录、打开数据目录、设置、
//! 以管理员身份重启、退出。
//!
//! 注册 / 注销右键菜单已移到“设置”窗口，避免托盘菜单过长。

use anyhow::Result;
use log::info;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, InsertMenuW, PostQuitMessage, RegisterClassExW, SetForegroundWindow,
    TrackPopupMenu, TranslateMessage, HMENU, MF_BYPOSITION, MF_STRING, MSG, SW_SHOWNORMAL,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, WINDOW_EX_STYLE, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK,
    WM_RBUTTONUP, WM_USER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

use crate::util::wide;

/// 自定义托盘消息及菜单命令 ID。
const WM_TRAY_ICON: u32 = WM_USER + 100;
const MENU_SHOW_HISTORY: u32 = 1001;
const MENU_OPEN_DATA_DIR: u32 = 1002;
const MENU_LIST_MANAGER: u32 = 1003;
const MENU_RESTART_AS_ADMIN: u32 = 1005;
const MENU_SETTINGS: u32 = 1006;
const MENU_QUIT: u32 = 9999;

pub struct TrayIcon {
    hwnd: HWND,
}

impl TrayIcon {
    pub fn new() -> Self {
        Self {
            hwnd: HWND::default(),
        }
    }

    /// 创建托盘图标并进入消息循环；用户选择“退出”后返回。
    pub fn run(&mut self) -> Result<()> {
        let class_name = wide("FileOriginTrackerTrayWndClass");
        let window_name = wide("FileOriginTracker");
        let module = unsafe { GetModuleHandleW(None)? };
        let instance = HINSTANCE(module.0);

        let mut window_class = WNDCLASSEXW::default();
        window_class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        window_class.lpfnWndProc = Some(window_procedure);
        window_class.hInstance = instance;
        window_class.lpszClassName = PCWSTR(class_name.as_ptr());

        if unsafe { RegisterClassExW(&window_class) } == 0 {
            return Err(anyhow::anyhow!("注册托盘窗口类失败：{:?}", unsafe {
                GetLastError()
            }));
        }

        self.hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(window_name.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                1,
                1,
                HWND::default(),
                HMENU::default(),
                instance,
                None,
            )?
        };

        self.add_notify_icon()?;
        info!("系统托盘图标已创建");

        info!("进入消息循环...");
        let mut message = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
            if !result.as_bool() {
                info!("GetMessageW 返回 false，退出消息循环");
                break;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }
        }

        self.remove_notify_icon();
        info!("消息循环已退出");
        Ok(())
    }

    fn add_notify_icon(&self) -> Result<()> {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY_ICON;
        // 使用程序自己设计的图标；加载失败时退回系统默认图标。
        data.hIcon = crate::util::app_icon();

        copy_to_fixed_buffer(
            &wide("文件来源追踪 - 后台记录中（双击查看最近记录）"),
            &mut data.szTip,
        );

        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            return Err(anyhow::anyhow!("添加系统托盘图标失败"));
        }
        Ok(())
    }

    fn remove_notify_icon(&self) {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = 1;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

/// 隐藏窗口的消息处理函数。
unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_TRAY_ICON => {
            match lparam.0 as u32 & 0xFFFF {
                WM_RBUTTONUP => {
                    let _ = show_tray_menu(hwnd);
                }
                // 左键双击托盘图标：打开最近记录窗口
                WM_LBUTTONDBLCLK => {
                    if let Err(error) = crate::history::show() {
                        log::error!("打开最近记录窗口失败：{:#}", error);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 as u32 & 0xFFFF {
                MENU_SHOW_HISTORY => {
                    if let Err(error) = crate::history::show() {
                        log::error!("打开最近记录窗口失败：{:#}", error);
                    }
                }
                MENU_OPEN_DATA_DIR => open_data_dir(),
                MENU_LIST_MANAGER => {
                    if let Err(error) = crate::list_window::show() {
                        log::error!("打开名单管理窗口失败：{:#}", error);
                    }
                }
                MENU_RESTART_AS_ADMIN => {
                    restart_as_admin();
                    let _ = DestroyWindow(hwnd);
                }
                MENU_SETTINGS => {
                    if let Err(error) = crate::settings::show() {
                        log::error!("打开设置窗口失败：{:#}", error);
                    }
                }
                MENU_QUIT => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn show_tray_menu(hwnd: HWND) -> Result<()> {
    let menu = CreatePopupMenu()?;
    let items = [
        (MENU_SHOW_HISTORY, "查看最近记录"),
        (MENU_LIST_MANAGER, "名单管理（黑/白名单）"),
        (MENU_OPEN_DATA_DIR, "打开数据目录"),
        (MENU_SETTINGS, "设置"),
        (MENU_RESTART_AS_ADMIN, "以管理员身份重新启动"),
        (MENU_QUIT, "退出"),
    ];

    for (index, (id, text)) in items.iter().enumerate() {
        // InsertMenuW 会复制字符串，所以临时缓冲区在这里可以安全释放。
        let text_wide = wide(text);
        InsertMenuW(
            menu,
            index as u32,
            MF_BYPOSITION | MF_STRING,
            *id as usize,
            PCWSTR(text_wide.as_ptr()),
        )?;
    }

    let mut point = POINT::default();
    GetCursorPos(&mut point)?;
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        point.x,
        point.y,
        0,
        hwnd,
        None,
    );
    DestroyMenu(menu)?;
    Ok(())
}

/// 打开数据库所在目录。
fn open_data_dir() {
    if let Ok(path) = crate::config::Config::default_data_dir() {
        let _ = std::fs::create_dir_all(&path);
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
}

/// 以管理员身份重新启动自己（ETW 内核事件跟踪需要管理员权限）。
fn restart_as_admin() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let operation = wide("runas");
    let file = wide(&exe.to_string_lossy());
    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn copy_to_fixed_buffer(source: &[u16], destination: &mut [u16]) {
    let length = source.len().min(destination.len().saturating_sub(1));
    destination[..length].copy_from_slice(&source[..length]);
    destination[length] = 0;
}
