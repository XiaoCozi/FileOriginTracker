//! “最近记录”窗口。
//!
//! 用原生 Win32 ListView 展示数据库中最新的记录
//! （时间 / 程序 / 路径 / 类型）。
//! 使用原生控件而不是网页或 GPU 界面，空闲时几乎不占 CPU：
//! 只有窗口处于激活状态时，才每 10 秒自动刷新一次列表。

use anyhow::Result;
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ScreenToClient, UpdateWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCF_TEXT, LVCF_WIDTH,
    LVCOLUMNW, LVHITTESTINFO, LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVM_SETITEMTEXTW, LVM_SUBITEMHITTEST, LVN_ITEMACTIVATE, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW,
    KillTimer, LoadCursorW, MoveWindow, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetTimer, ShowWindow, TranslateMessage, CW_USEDEFAULT, HMENU, ICON_BIG, ICON_SMALL, IDC_ARROW,
    MSG, SW_HIDE, SW_SHOWNORMAL, WA_INACTIVE, WINDOW_EX_STYLE, WM_ACTIVATE, WM_CLOSE, WM_CREATE,
    WM_NOTIFY, WM_SETICON, WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_BORDER, WS_CHILD,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::db::{Database, FileOrigin};
use crate::util::wide;

const WINDOW_TITLE: &str = "文件来源追踪 - 最近记录";
const WINDOW_CLASS: &str = "FileOriginTrackerHistoryWindow";

/// 自动刷新间隔（毫秒）。只在窗口激活时启用。
const REFRESH_TIMER_ID: usize = 1;
const REFRESH_INTERVAL_MS: u32 = 10_000;

/// 最多展示的记录条数，避免列表过大影响打开速度。
const MAX_ROWS: u32 = 500;

/// 列表控件样式：子窗口 + 可见 + 边框 + 报表模式 + 单选 + 始终显示选中项。
///
/// 注意：这里不能加 `LVS_SORTDESCENDING` 等自动排序样式。一旦启用自动排序，
/// 每次插入行都会被重新排序，之后再用插入时使用的下标去填“程序/路径”列，
/// 就会写到别的行上，表现为一堆只有时间、没有内容的空白行。
/// 记录本身已经按时间倒序从数据库取出，不需要控件再排序。
const LIST_STYLE: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE =
    windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_BORDER.0 | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
    );

static HISTORY_WINDOW: AtomicIsize = AtomicIsize::new(0);
static LIST_VIEW: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 列表的列定义：（标题，宽度）。顺序必须和填充数据时使用的列下标一致。
///
/// 双击时也用它按光标 x 坐标反推用户点的是哪一列，比依赖
/// `LVM_SUBITEMHITTEST` 返回的 iSubItem 更稳妥。
const COLUMNS: [(&str, i32); 4] = [
    ("创建时间", 170),
    ("创建程序", 200),
    ("文件 / 文件夹路径", 520),
    ("类型", 100),
];

/// 列表控件的子窗口 ID。处理 WM_NOTIFY 时要靠它确认通知来自列表。
const LIST_CONTROL_ID: usize = 1;

/// 当前列表里显示的行数据。
///
/// ListView 只保存文本，双击时需要按行号找回完整记录（程序路径、文件路径），
/// 所以刷新列表时同步缓存一份。
static ROW_CACHE: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 记录窗口使用的只读数据库连接（惰性打开，打开失败时列表为空）。
static HISTORY_DB: Lazy<Mutex<Option<Database>>> = Lazy::new(|| Mutex::new(None));

/// 显示（或重新激活）最近记录窗口。
pub fn show() -> Result<()> {
    let existing = HISTORY_WINDOW.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
        }
        refresh_list();
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
            1000,
            560,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )?
    };

    HISTORY_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = UpdateWindow(hwnd);
    }
    refresh_list();
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
    window_class.hCursor = unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW)? };
    window_class.lpszClassName = PCWSTR(class_name.as_ptr());

    if unsafe { RegisterClassExW(&window_class) } == 0 {
        CLASS_REGISTERED.store(false, Ordering::SeqCst);
        return Err(anyhow::anyhow!("注册记录窗口类失败"));
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
            let module = GetModuleHandleW(None).unwrap_or_default();
            create_list_view(hwnd, HINSTANCE(module.0));
            // 窗口左上角与任务栏显示本程序自己的图标。
            let icon = crate::util::app_icon();
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(icon.0 as isize),
            );
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(icon.0 as isize),
            );
            LRESULT(0)
        }
        WM_NOTIFY => {
            let header = &*(lparam.0 as *const NMHDR);
            if header.idFrom == LIST_CONTROL_ID && header.code == LVN_ITEMACTIVATE {
                // 双击时用鼠标位置做命中测试拿到“行 + 列”。
                // 这比依赖 NMLISTVIEW 里的 iSubItem 更可靠：
                // 某些情况下该字段是 -1，会导致双击完全没反应。
                let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
                if let Some((index, sub_item)) = hit_test_item(list) {
                    // 记录一行日志，方便排查“双击没反应”这类问题：
                    // 能看到用户实际点中的是第几行、第几列，以及那一行的目标。
                    let cached = ROW_CACHE
                        .lock()
                        .ok()
                        .and_then(|cache| cache.get(index).cloned());
                    match cached {
                        Some(row) => log::info!(
                            "[记录] 双击第 {} 行第 {} 列：程序={} 路径={}",
                            index + 1,
                            sub_item,
                            row.process_path,
                            row.file_path
                        ),
                        None => log::warn!("[记录] 双击第 {} 行，但缓存里没有对应记录", index + 1),
                    }
                    open_row_target(index, sub_item);
                }
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
            if !list.0.is_null() {
                resize_list(hwnd, list);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                // 窗口失去焦点后停止刷新，避免后台无意义的数据库查询。
                let _ = KillTimer(hwnd, REFRESH_TIMER_ID);
            } else {
                refresh_list();
                let _ = SetTimer(hwnd, REFRESH_TIMER_ID, REFRESH_INTERVAL_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == REFRESH_TIMER_ID {
                refresh_list();
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 只隐藏窗口，保留句柄，下次打开更快。
            let _ = KillTimer(hwnd, REFRESH_TIMER_ID);
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 创建列表控件，并设置三列。
unsafe fn create_list_view(window: HWND, instance: HINSTANCE) {
    // 列表控件属于通用控件，必须先初始化。
    let controls = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES,
    };
    let _ = InitCommonControlsEx(&controls);

    let list_class = wide("SysListView32");
    let list = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(list_class.as_ptr()),
        PCWSTR::null(),
        LIST_STYLE,
        0,
        0,
        100,
        100,
        window,
        HMENU(LIST_CONTROL_ID as *mut std::ffi::c_void),
        instance,
        None,
    ) {
        Ok(list) => list,
        Err(_) => return,
    };

    LIST_VIEW.store(list.0 as isize, Ordering::SeqCst);

    let columns = COLUMNS;

    for (index, (title, width)) in columns.iter().enumerate() {
        let title = wide(title);
        let mut column = LVCOLUMNW::default();
        column.mask = LVCF_TEXT | LVCF_WIDTH;
        column.cx = *width;
        column.pszText = PWSTR(title.as_ptr() as *mut u16);
        SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            WPARAM(index),
            LPARAM(&column as *const _ as isize),
        );
    }

    resize_list(window, list);
}

/// 让列表跟随窗口大小变化。
unsafe fn resize_list(window: HWND, list: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(window, &mut rect).is_err() {
        return;
    }
    let width = (rect.right - rect.left - 16).max(100);
    let height = (rect.bottom - rect.top - 16).max(100);
    let _ = MoveWindow(list, 8, 8, width, height, true);
}

/// 从数据库读取最近记录（失败时返回空列表）。
fn load_rows() -> Vec<FileOrigin> {
    let mut guard = match HISTORY_DB.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if guard.is_none() {
        if let Ok(data_dir) = crate::config::Config::default_data_dir() {
            if let Ok(db) = Database::open_readonly(&data_dir) {
                *guard = Some(db);
            }
        }
    }

    match guard.as_ref() {
        Some(db) => {
            let rows = db.recent(MAX_ROWS).unwrap_or_default();
            // 同步缓存，供双击时按行号取回完整记录。
            if let Ok(mut cache) = ROW_CACHE.lock() {
                *cache = rows.clone();
            }
            rows
        }
        None => Vec::new(),
    }
}

/// 用鼠标位置在列表上做命中测试，返回（行下标，列下标）。
///
/// 行下标由 `LVM_SUBITEMHITTEST` 给出；列下标则直接用光标在列表内的 x 坐标
/// 对照 `COLUMNS` 的宽度累加计算。这样即使某些系统上 iSubItem 返回 0 或 -1，
/// 双击“创建程序 / 路径”列也不会被误判成时间列。
unsafe fn hit_test_item(list: HWND) -> Option<(usize, i32)> {
    if list.0.is_null() {
        return None;
    }

    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_err() {
        return None;
    }
    // ScreenToClient 返回 BOOL，失败时为 0。
    if !ScreenToClient(list, &mut point).as_bool() {
        return None;
    }

    let mut info = LVHITTESTINFO::default();
    info.pt = point;
    let index = SendMessageW(
        list,
        LVM_SUBITEMHITTEST,
        WPARAM(0),
        LPARAM(&mut info as *mut _ as isize),
    )
    .0;

    if index < 0 {
        None
    } else {
        Some((index as usize, column_at_x(point.x)))
    }
}

/// 按列表内的 x 坐标计算列下标（0 起）。
fn column_at_x(x: i32) -> i32 {
    let mut right = 0;
    for (index, (_, width)) in COLUMNS.iter().enumerate() {
        right += *width;
        if x < right {
            return index as i32;
        }
    }
    // 超出最后一列时按最后一列处理。
    COLUMNS.len() as i32 - 1
}

/// 双击列表行时打开对应目录。
///
/// - 双击「创建程序」列：打开该程序所在目录并选中程序文件
/// - 双击「文件 / 文件夹路径」列：打开文件所在目录并选中它；
///   如果记录本身就是文件夹，则直接打开这个文件夹
fn open_row_target(index: usize, sub_item: i32) {
    let row = match ROW_CACHE.lock() {
        Ok(cache) => cache.get(index).cloned(),
        Err(poisoned) => poisoned.into_inner().get(index).cloned(),
    };
    let Some(row) = row else {
        return;
    };

    match sub_item {
        1 => crate::util::reveal_in_explorer(&row.process_path),
        2 => {
            if std::path::Path::new(&row.file_path).is_dir() {
                crate::util::open_directory(&row.file_path);
            } else {
                crate::util::reveal_in_explorer(&row.file_path);
            }
        }
        // 时间列不做操作。
        _ => {}
    }
}

/// 清空并重新填充列表。
fn refresh_list() {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    let rows = load_rows();

    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));

        for (index, row) in rows.iter().enumerate() {
            let time_text = wide(&crate::util::timestamp_to_local_text(row.created_at));
            let mut item = LVITEMW::default();
            item.mask = LVIF_TEXT;
            item.iItem = index as i32;
            item.iSubItem = 0;
            item.pszText = PWSTR(time_text.as_ptr() as *mut u16);

            // LVM_INSERTITEMW 返回新行真正所在的下标，必须用它来填后续列，
            // 不能假设行一定位于 index，否则列内容会错位或丢失。
            let inserted = SendMessageW(
                list,
                LVM_INSERTITEMW,
                WPARAM(0),
                LPARAM(&item as *const _ as isize),
            );
            if inserted.0 < 0 {
                continue;
            }
            let row_index = inserted.0 as usize;

            let program_text = wide(&row.process_name);
            insert_sub_item(list, row_index, 1, &program_text);

            let path_text = wide(&row.file_path);
            insert_sub_item(list, row_index, 2, &path_text);

            // 类型列：让用户直观看到这条记录属于哪类监控。
            let action_text = wide(&action_label(&row.action));
            insert_sub_item(list, row_index, 3, &action_text);
        }
    }
}

/// 把数据库里的动作标识转换成界面上显示的中文类型。
///
/// - `create_dir`  ：新建文件夹
/// - `modify`      ：修改文件
/// - 其它（`create_file`）：新建文件
fn action_label(action: &str) -> String {
    match action {
        "create_dir" => "新建文件夹".to_string(),
        "modify" => "修改文件".to_string(),
        _ => "新建文件".to_string(),
    }
}

/// 给某一行填充子列文本。
unsafe fn insert_sub_item(list: HWND, row: usize, column: i32, text: &[u16]) {
    let mut item = LVITEMW::default();
    item.iSubItem = column;
    item.pszText = PWSTR(text.as_ptr() as *mut u16);
    SendMessageW(
        list,
        LVM_SETITEMTEXTW,
        WPARAM(row),
        LPARAM(&item as *const _ as isize),
    );
}

/// 进入窗口消息循环。
///
/// 主程序由托盘循环负责消息分发；当以 `history` 子命令单独打开记录窗口时，
/// 由这里的循环维持窗口运行，直到用户关闭窗口。
pub fn run_message_loop() {
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::column_at_x;

    /// 验证“光标 x 坐标 -> 列下标”的换算与列宽严格一致。
    /// 这直接决定双击“创建程序 / 路径”列时能否打开对应目录。
    #[test]
    fn column_at_x_matches_column_widths() {
        assert_eq!(column_at_x(0), 0);
        assert_eq!(column_at_x(169), 0);
        assert_eq!(column_at_x(170), 1);
        assert_eq!(column_at_x(369), 1);
        assert_eq!(column_at_x(370), 2);
        assert_eq!(column_at_x(889), 2);
        assert_eq!(column_at_x(890), 3);
        assert_eq!(column_at_x(5000), 3);
    }
}
