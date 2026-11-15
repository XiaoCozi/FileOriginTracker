//! 「程序备注」输入窗口。
//!
//! 在「最近记录」里右键某条记录 → 设置备注，就会弹出这个小窗口。
//! 备注按程序完整路径保存，之后该程序的所有记录都会显示这条备注，
//! 例如：`notepad.exe（我自己启动的记事本）`。

use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetWindowTextW,
    IsWindow, LoadCursorW, RegisterClassExW, SendMessageW, SetWindowTextW, ShowWindow,
    TranslateMessage, CW_USEDEFAULT, HMENU, IDC_ARROW, MSG, SW_SHOWNORMAL, WINDOW_EX_STYLE,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_SETFONT, WNDCLASSEXW,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_SYSMENU, WS_VISIBLE,
};

use crate::util::wide;

const WINDOW_TITLE: &str = "设置程序备注";
const WINDOW_CLASS: &str = "FileOriginTrackerNoteWindow";

const ID_NOTE_EDIT: usize = 3001;
const ID_SAVE_BUTTON: usize = 3002;
const ID_CANCEL_BUTTON: usize = 3003;

/// 当前对话框窗口句柄（0 表示没有）。
static DIALOG_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// 用户是否点了“保存”。
static SAVED: AtomicBool = AtomicBool::new(false);
/// 保存下来的备注文字。
static RESULT: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));
/// 正在编辑的程序名与路径，用于绘制提示文字。
static PROGRAM_NAME: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));
static PROGRAM_PATH: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(String::new()));
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 弹出备注编辑窗口。
///
/// 返回 `Some(文字)` 表示用户保存了（文字可能为空，代表删除备注），
/// 返回 `None` 表示用户取消。
pub fn show(
    owner: HWND,
    program_name: &str,
    program_path: &str,
    current_note: &str,
) -> Option<String> {
    *PROGRAM_NAME.lock().unwrap() = program_name.to_string();
    *PROGRAM_PATH.lock().unwrap() = program_path.to_string();
    *RESULT.lock().unwrap() = current_note.to_string();
    SAVED.store(false, Ordering::SeqCst);

    let module = unsafe { GetModuleHandleW(None).ok()? };
    let instance = HINSTANCE(module.0);
    if !register_class(instance) {
        return None;
    }

    let class_name = wide(WINDOW_CLASS);
    let title = wide(WINDOW_TITLE);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0x0000_0008), // WS_EX_TOPMOST：让备注窗口浮在记录窗口上方
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            460,
            210,
            owner,
            HMENU::default(),
            instance,
            None,
        )
        .ok()?
    };

    DIALOG_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);

    // 把当前备注填进输入框。
    let edit =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, ID_NOTE_EDIT as i32) };
    if let Ok(edit) = edit {
        let text = wide(current_note);
        unsafe {
            let _ = SetWindowTextW(edit, PCWSTR(text.as_ptr()));
        }
    }

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }

    // 模态：在本线程里单独跑一个消息循环，直到窗口被销毁。
    let mut message = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        if !ret.as_bool() {
            break; // 收到退出消息
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if unsafe { IsWindow(hwnd) }.0 == 0 {
            break;
        }
    }

    if SAVED.load(Ordering::SeqCst) {
        Some(RESULT.lock().unwrap().clone())
    } else {
        None
    }
}

fn register_class(instance: HINSTANCE) -> bool {
    if CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
        return true;
    }

    let class_name = wide(WINDOW_CLASS);
    let mut window_class = WNDCLASSEXW::default();
    window_class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    window_class.lpfnWndProc = Some(window_procedure);
    window_class.hInstance = instance;
    window_class.hbrBackground = crate::util::ui_brush();
    window_class.hCursor =
        unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default() };
    window_class.lpszClassName = PCWSTR(class_name.as_ptr());

    if unsafe { RegisterClassExW(&window_class) } == 0 {
        CLASS_REGISTERED.store(false, Ordering::SeqCst);
        return false;
    }
    true
}

unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0);
            create_controls(hwnd, instance);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as usize;
            match id {
                ID_SAVE_BUTTON => {
                    let edit = windows::Win32::UI::WindowsAndMessaging::GetDlgItem(
                        hwnd,
                        ID_NOTE_EDIT as i32,
                    )
                    .unwrap_or_default();
                    let mut buffer = vec![0u16; 512];
                    let length = GetWindowTextW(edit, &mut buffer).max(0) as usize;
                    let text = String::from_utf16_lossy(&buffer[..length.min(buffer.len())]);
                    *RESULT.lock().unwrap() = text;
                    SAVED.store(true, Ordering::SeqCst);
                    let _ = DestroyWindow(hwnd);
                }
                ID_CANCEL_BUTTON => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            // 让标签和输入框背景与窗口一致（白色），避免出现灰色块。
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            SetBkColor(device, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
            SetTextColor(device, windows::Win32::Foundation::COLORREF(0x0020_2020));
            LRESULT(crate::util::ui_brush().0 as isize)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 创建窗口里的标签、输入框和两个按钮。
unsafe fn create_controls(window: HWND, instance: HINSTANCE) {
    let font = crate::util::ui_font();
    let name = PROGRAM_NAME.lock().map(|v| v.clone()).unwrap_or_default();
    let path = PROGRAM_PATH.lock().map(|v| v.clone()).unwrap_or_default();

    // 程序名
    create_static(
        window,
        instance,
        &format!("程序：{}", name),
        16,
        14,
        420,
        20,
        font,
    );
    // 程序路径（可能很长，直接显示便于确认备注对象）
    create_static(window, instance, &path, 16, 38, 420, 20, font);
    // 备注标题
    create_static(window, instance, "备注：", 16, 74, 48, 20, font);

    let edit_class = wide("EDIT");
    if let Ok(edit) = CreateWindowExW(
        WINDOW_EX_STYLE(0x0000_0200), // WS_EX_CLIENTEDGE：输入框加立体边框
        PCWSTR(edit_class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD | WS_VISIBLE | WS_BORDER,
        68,
        70,
        368,
        26,
        window,
        HMENU(ID_NOTE_EDIT as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    let button_class = wide("BUTTON");
    for (id, text, x) in [
        (ID_SAVE_BUTTON, "保存", 250),
        (ID_CANCEL_BUTTON, "取消", 346),
    ] {
        let label = wide(text);
        if let Ok(button) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(button_class.as_ptr()),
            PCWSTR(label.as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                    windows::Win32::UI::WindowsAndMessaging::BS_PUSHBUTTON as u32,
                ),
            x,
            120,
            88,
            30,
            window,
            HMENU(id as *mut c_void),
            instance,
            None,
        ) {
            SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        }
    }
}

/// 创建一个静态文字控件。
unsafe fn create_static(
    window: HWND,
    instance: HINSTANCE,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    font: windows::Win32::Graphics::Gdi::HFONT,
) {
    let class = wide("STATIC");
    let text = wide(text);
    if let Ok(control) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class.as_ptr()),
        PCWSTR(text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        window,
        HMENU::default(),
        instance,
        None,
    ) {
        SendMessageW(control, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}
