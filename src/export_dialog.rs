//! 「导出 Excel」的条件选择窗口。
//!
//! 点「最近记录」工具栏的「导出 Excel」会先弹出这个窗口，
//! 让用户选择导出范围：
//! - **导出全部记录**：忽略下面所有条件，把数据库里的记录整个导出；
//! - **按条件筛选导出**：可以组合使用
//!   创建程序、时间范围、监视类型、关键字、备注 这几个条件。
//!
//! 窗口是模态的：弹出后会阻塞记录窗口，直到用户点「导出」或「取消」。

use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, SYSTEMTIME, WPARAM};
use windows::Win32::Graphics::Gdi::{HFONT, SetBkColor, SetTextColor};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    DTM_GETSYSTEMTIME, DTM_SETFORMATW, ICC_DATE_CLASSES, ICC_LISTVIEW_CLASSES,
    INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL,
    CBS_AUTOHSCROLL, CBS_DROPDOWN, CreateWindowExW, CW_USEDEFAULT, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetDlgItem, GetMessageW, GetWindowTextW, IsWindow, LoadCursorW,
    MSG, MessageBoxW, RegisterClassExW, SW_SHOWNORMAL, SendMessageW, ShowWindow, TranslateMessage,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_SETFONT, WNDCLASSEXW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_GROUP, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL, BM_GETCHECK,
};

use crate::db::{Database, RecordFilter};
use crate::util::wide;

const WINDOW_TITLE: &str = "导出 Excel";
const WINDOW_CLASS: &str = "FileOriginTrackerExportDialog";

/// 控件 ID。
const ID_SCOPE_ALL: usize = 6101;
const ID_SCOPE_FILTER: usize = 6102;
const ID_PROGRAM_COMBO: usize = 6103;
const ID_TIME_START_ENABLE: usize = 6104;
const ID_TIME_START: usize = 6105;
const ID_TIME_END_ENABLE: usize = 6106;
const ID_TIME_END: usize = 6107;
/// 五个监视类型复选框，ID 从 6110 起连续编号。
const ID_ACTION_BASE: usize = 6110;
const ID_KEYWORD: usize = 6115;
const ID_NOTE: usize = 6116;
const ID_EXPORT_BUTTON: usize = 6120;
const ID_CANCEL_BUTTON: usize = 6121;

/// 复选框顺序必须与 `ACTIONS` 一致。
const ACTION_LABELS: [&str; 5] = ["新建文件", "新建文件夹", "修改文件", "删除文件", "删除文件夹"];
/// 与复选框一一对应的数据库 action 值。
const ACTIONS: [&str; 5] = [
    "create_file",
    "create_dir",
    "modify",
    "delete_file",
    "delete_dir",
];

/// 用户选择的导出范围。
pub enum ExportScope {
    /// 导出全部记录（不受任何条件限制）。
    All,
    /// 按条件筛选导出。
    Filter(RecordFilter),
}

/// 用户确认后的结果。`None` 表示点了取消。
static RESULT: Lazy<Mutex<Option<ExportScope>>> = Lazy::new(|| Mutex::new(None));
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 属于「筛选」范围、需要随单选按钮一起启用 / 禁用的控件句柄。
static FILTER_CONTROLS: Lazy<Mutex<Vec<isize>>> = Lazy::new(|| Mutex::new(Vec::new()));
/// 下拉框每一项对应的程序完整路径（下标与组合框条目一一对应）。
static COMBO_PATHS: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 弹出导出条件窗口。
///
/// 返回 `Some(范围)` 表示用户点了「导出」，`None` 表示取消。
pub fn show(owner: HWND) -> Option<ExportScope> {
    *RESULT.lock().unwrap() = None;
    FILTER_CONTROLS.lock().unwrap().clear();
    COMBO_PATHS.lock().unwrap().clear();

    let module = unsafe { GetModuleHandleW(None).ok()? };
    let instance = HINSTANCE(module.0);
    if !register_class(instance) {
        return None;
    }

    let class_name = wide(WINDOW_CLASS);
    let title = wide(WINDOW_TITLE);
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0x0000_0008), // WS_EX_TOPMOST：浮在记录窗口上方
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            400,
            owner,
            windows::Win32::UI::WindowsAndMessaging::HMENU::default(),
            instance,
            None,
        )
        .ok()?
    };

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }

    // 模态：在本线程里单独跑一个消息循环，直到窗口被销毁。
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

    RESULT.lock().unwrap().take()
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
        unsafe { LoadCursorW(HINSTANCE::default(), windows::Win32::UI::WindowsAndMessaging::IDC_ARROW).unwrap_or_default() };
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
            // 默认选中「导出全部」，此时下方筛选控件全部禁用。
            apply_scope_state(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as usize;
            match id {
                // 两个单选按钮切换：决定下方筛选控件是否可用。
                ID_SCOPE_ALL | ID_SCOPE_FILTER => apply_scope_state(hwnd),
                ID_EXPORT_BUTTON => try_confirm(hwnd),
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
            // 标签与输入框背景保持白色，和窗口一致。
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            SetBkColor(device, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
            SetTextColor(device, windows::Win32::Foundation::COLORREF(0x0020_2020));
            LRESULT(crate::util::ui_brush().0 as isize)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 根据当前选中的单选按钮，启用或禁用下方所有筛选控件。
unsafe fn apply_scope_state(window: HWND) {
    let filter_mode = control(GetDlgItem(window, ID_SCOPE_FILTER as i32))
        .map(|handle| is_checked(handle))
        .unwrap_or(false);

    for handle in FILTER_CONTROLS.lock().unwrap().iter() {
        let _ = EnableWindow(HWND(*handle as *mut c_void), filter_mode);
    }
}

/// 用户点「导出」：读取所有控件，组装筛选条件并关闭窗口。
unsafe fn try_confirm(window: HWND) {
    let all = control(GetDlgItem(window, ID_SCOPE_ALL as i32))
        .map(|handle| is_checked(handle))
        .unwrap_or(true);

    let scope = if all {
        ExportScope::All
    } else {
        // 监视类型：勾了哪几个就查询哪几个；一个都不勾表示不限。
        let mut actions = Vec::new();
        for (index, action) in ACTIONS.iter().enumerate() {
            let checked = control(GetDlgItem(window, (ID_ACTION_BASE + index) as i32))
                .map(|handle| is_checked(handle))
                .unwrap_or(false);
            if checked {
                actions.push((*action).to_string());
            }
        }

        let range = read_range(window);
        // 起止时间填反了先提示，不关闭窗口，方便用户改。
        if let Some((start, end)) = range {
            if start > end {
                let text = wide("起始时间晚于结束时间，请重新选择。");
                let title = wide("时间范围有误");
                let _ = MessageBoxW(
                    window,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    windows::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
                );
                return;
            }
        }

        ExportScope::Filter(RecordFilter {
            program: program_filter_value(window),
            keyword: control_text(window, ID_KEYWORD),
            note: control_text(window, ID_NOTE),
            actions,
            range,
        })
    };

    *RESULT.lock().unwrap() = Some(scope);
    let _ = DestroyWindow(window);
}

/// 读取「创建程序」输入框的值。
///
/// 如果用户是从下拉列表里选的，就用该条目对应的**完整路径**做精确匹配；
/// 如果是自己手打的文字，就当作模糊关键字。
unsafe fn program_filter_value(window: HWND) -> String {
    let Ok(combo) = GetDlgItem(window, ID_PROGRAM_COMBO as i32) else {
        return String::new();
    };
    let index = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    if index >= 0 {
        if let Some(path) = COMBO_PATHS.lock().unwrap().get(index as usize) {
            return path.clone();
        }
    }
    control_text(window, ID_PROGRAM_COMBO)
}

/// 读取时间范围。
///
/// 两端各自有复选框：勾上才按该端过滤。
/// 只勾起点 → 终点用最大值；只勾终点 → 起点用 0。
unsafe fn read_range(window: HWND) -> Option<(i64, i64)> {
    let start_enabled = control(GetDlgItem(window, ID_TIME_START_ENABLE as i32))
        .map(|handle| is_checked(handle))
        .unwrap_or(false);
    let end_enabled = control(GetDlgItem(window, ID_TIME_END_ENABLE as i32))
        .map(|handle| is_checked(handle))
        .unwrap_or(false);

    if !start_enabled && !end_enabled {
        return None;
    }

    let start = if start_enabled {
        picker_unix(window, ID_TIME_START).unwrap_or(0)
    } else {
        0
    };
    let end = if end_enabled {
        picker_unix(window, ID_TIME_END).unwrap_or(i64::MAX)
    } else {
        i64::MAX
    };
    Some((start, end))
}

/// 读取一个日期时间框并转成 Unix 秒。
unsafe fn picker_unix(window: HWND, id: usize) -> Option<i64> {
    let control = GetDlgItem(window, id as i32).ok()?;
    let mut system_time = SYSTEMTIME::default();
    let result = SendMessageW(
        control,
        DTM_GETSYSTEMTIME,
        WPARAM(0),
        LPARAM(&mut system_time as *mut _ as isize),
    )
    .0;
    // GDT_VALID = 0 表示取到了有效时间。
    if result != 0 {
        return None;
    }
    crate::util::local_system_time_to_unix(&system_time)
}

/// 读取某个控件的文字（最多 1024 个宽字符）。
fn control_text(window: HWND, id: usize) -> String {
    let Ok(handle) = (unsafe { GetDlgItem(window, id as i32) }) else {
        return String::new();
    };
    let mut buffer = vec![0u16; 1024];
    let length = unsafe { GetWindowTextW(handle, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length.min(buffer.len())])
}

unsafe fn is_checked(handle: HWND) -> bool {
    SendMessageW(handle, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1
}

/// 把 `Result<HWND>` 简化成 `Option<HWND>`。
fn control(result: windows::core::Result<HWND>) -> Option<HWND> {
    result.ok()
}

/// 创建窗口里的全部控件。
unsafe fn create_controls(window: HWND, instance: HINSTANCE) {
    // 日期时间框属于通用控件，必须先初始化（重复初始化是安全的）。
    let controls = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES | ICC_DATE_CLASSES,
    };
    let _ = InitCommonControlsEx(&controls);

    let font = crate::util::ui_font();
    let button_class = wide("BUTTON");
    let edit_class = wide("EDIT");

    // ---- 第一行：导出范围二选一 ----
    for (id, text, x, group) in [
        (ID_SCOPE_ALL, "导出全部记录", 16, true),
        (ID_SCOPE_FILTER, "按条件筛选导出", 190, false),
    ] {
        let label = wide(text);
        let style = WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | if group { WS_GROUP } else { WINDOW_STYLE(0) }
            | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32);
        if let Ok(button) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(button_class.as_ptr()),
            PCWSTR(label.as_ptr()),
            style,
            x,
            10,
            150,
            22,
            window,
            windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut c_void),
            instance,
            None,
        ) {
            SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            // 默认选中「导出全部」。
            if id == ID_SCOPE_ALL {
                SendMessageW(
                    button,
                    windows::Win32::UI::WindowsAndMessaging::BM_SETCHECK,
                    WPARAM(1),
                    LPARAM(0),
                );
            }
        }
    }

    // ---- 创建程序：可编辑下拉框 ----
    create_static(window, instance, "创建程序：", 16, 42, 80, 20, font);
    let combo_class = wide("COMBOBOX");
    if let Ok(combo) = CreateWindowExW(
        WINDOW_EX_STYLE(0x0000_0200), // WS_EX_CLIENTEDGE
        PCWSTR(combo_class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WS_GROUP
            | WS_VSCROLL
            | WINDOW_STYLE((CBS_DROPDOWN | CBS_AUTOHSCROLL) as u32),
        100,
        38,
        510,
        220, // 下拉列表展开后的高度
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU(ID_PROGRAM_COMBO as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(combo, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        remember_filter_control(combo);
        fill_program_choices(combo);
    }

    // ---- 时间范围 ----
    create_static(window, instance, "时间范围：", 16, 76, 80, 20, font);
    create_checkbox(window, instance, ID_TIME_START_ENABLE, "起", 100, 74, 36, font);
    create_checkbox(window, instance, ID_TIME_END_ENABLE, "止", 370, 74, 36, font);
    create_static(window, instance, "至", 348, 76, 18, 20, font);

    let picker_class = wide("SysDateTimePick32");
    let picker_format = wide("yyyy-MM-dd HH:mm:ss");
    for (id, x) in [(ID_TIME_START, 140), (ID_TIME_END, 410)] {
        if let Ok(picker) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(picker_class.as_ptr()),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE,
            x,
            70,
            200,
            24,
            window,
            windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut c_void),
            instance,
            None,
        ) {
            SendMessageW(picker, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            SendMessageW(
                picker,
                DTM_SETFORMATW,
                WPARAM(0),
                LPARAM(picker_format.as_ptr() as isize),
            );
            remember_filter_control(picker);
        }
    }

    // ---- 监视类型：五个复选框 ----
    create_static(window, instance, "监视类型：", 16, 108, 80, 20, font);
    let boxes = [100, 186, 282, 368, 454];
    for (index, label) in ACTION_LABELS.iter().enumerate() {
        create_checkbox(
            window,
            instance,
            ID_ACTION_BASE + index,
            label,
            boxes[index],
            106,
            if index == 1 || index == 4 { 90 } else { 80 },
            font,
        );
    }

    // ---- 关键字与备注 ----
    create_static(window, instance, "关键字（路径/程序）：", 16, 140, 152, 20, font);
    create_edit(window, instance, ID_KEYWORD, 172, 134, 440, &edit_class, font);

    create_static(window, instance, "备注包含：", 16, 172, 80, 20, font);
    create_edit(window, instance, ID_NOTE, 100, 166, 512, &edit_class, font);

    // ---- 提示文字 ----
    create_static(
        window,
        instance,
        "提示：留空的项表示不限；选择「导出全部记录」时会忽略以上所有条件。",
        16,
        204,
        600,
        20,
        font,
    );

    // ---- 底部按钮 ----
    let export_text = wide("导出");
    if let Ok(button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(export_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        420,
        300,
        90,
        30,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU(ID_EXPORT_BUTTON as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    let cancel_text = wide("取消");
    if let Ok(button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(cancel_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        520,
        300,
        90,
        30,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU(ID_CANCEL_BUTTON as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}

/// 记住一个需要随「筛选」开关一起启停的控件。
fn remember_filter_control(handle: HWND) {
    FILTER_CONTROLS.lock().unwrap().push(handle.0 as isize);
}

/// 把数据库里出现过的程序填进下拉框。
///
/// 显示成 `名称   (完整路径)`，但真正用于筛选的是路径，存在 `COMBO_PATHS` 里。
/// 最多填 200 条，避免程序数量过多时下拉列表极长。
unsafe fn fill_program_choices(combo: HWND) {
    let Ok(data_dir) = crate::config::Config::default_data_dir() else {
        return;
    };
    let Ok(db) = Database::open_readonly(&data_dir) else {
        return;
    };
    let Ok(choices) = db.program_choices() else {
        return;
    };

    let mut paths = COMBO_PATHS.lock().unwrap();
    for (name, path) in choices.into_iter().take(200) {
        let display = wide(&format!("{}   ({})", name, path));
        SendMessageW(
            combo,
            CB_ADDSTRING,
            WPARAM(0),
            LPARAM(display.as_ptr() as isize),
        );
        paths.push(path);
    }
}

/// 创建一个复选框，并登记为「筛选用控件」。
unsafe fn create_checkbox(
    window: HWND,
    instance: HINSTANCE,
    id: usize,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    font: HFONT,
) {
    let class = wide("BUTTON");
    let label = wide(text);
    if let Ok(handle) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class.as_ptr()),
        PCWSTR(label.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        x,
        y,
        width,
        20,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(handle, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        remember_filter_control(handle);
    }
}

/// 创建一个单行输入框，并登记为「筛选用控件」。
unsafe fn create_edit(
    window: HWND,
    instance: HINSTANCE,
    id: usize,
    x: i32,
    y: i32,
    width: i32,
    class: &[u16],
    font: HFONT,
) {
    if let Ok(handle) = CreateWindowExW(
        WINDOW_EX_STYLE(0x0000_0200), // WS_EX_CLIENTEDGE
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD
            | WS_VISIBLE
            | WS_TABSTOP
            | WINDOW_STYLE(windows::Win32::UI::WindowsAndMessaging::ES_AUTOHSCROLL as u32),
        x,
        y,
        width,
        24,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU(id as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(handle, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        remember_filter_control(handle);
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
    font: HFONT,
) {
    let class = wide("STATIC");
    let text = wide(text);
    if let Ok(handle) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(class.as_ptr()),
        PCWSTR(text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU::default(),
        instance,
        None,
    ) {
        SendMessageW(handle, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }
}
