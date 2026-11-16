//! “最近记录”窗口。
//!
//! 用原生 Win32 ListView 展示数据库中最新的记录
//! （监控时间 / 程序 / 路径 / 类型）。
//! 使用原生控件而不是网页或 GPU 界面，空闲时几乎不占 CPU：
//! 只有窗口处于激活状态时，才每 10 秒自动刷新一次列表。
//!
//! 交互：
//! - 顶部搜索框按程序名 / 路径 / 类型实时过滤
//! - 双击「创建程序」列打开程序所在目录，双击「路径」列打开文件所在目录
//! - 在记录上点右键弹出菜单：打开文件路径 / 复制词条 / 打开创建程序所在目录

use anyhow::Result;
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{ScreenToClient, SetBkColor, SetTextColor, UpdateWindow};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LIST_VIEW_ITEM_STATE_FLAGS,
    LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVHITTESTINFO, LVIF_STATE, LVIF_TEXT, LVIS_FOCUSED,
    LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVM_SUBITEMHITTEST,
    LVN_ITEMACTIVATE, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NM_RCLICK,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetClientRect,
    GetCursorPos, GetMessageW, GetWindowTextW, InsertMenuW, KillTimer, LoadCursorW, MessageBoxW,
    MoveWindow, PostMessageW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetTimer,
    ShowWindow, TrackPopupMenu, TranslateMessage, CW_USEDEFAULT, ES_AUTOHSCROLL, HMENU, ICON_BIG,
    ICON_SMALL, IDC_ARROW, MB_ICONINFORMATION, MB_OK, MF_BYPOSITION, MF_SEPARATOR, MF_STRING, MSG,
    SW_HIDE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN, WA_INACTIVE,
    WINDOW_EX_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_NOTIFY, WM_NULL, WM_SETFONT, WM_SETICON, WM_SIZE, WM_TIMER, WNDCLASSEXW,
    WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
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
static SEARCH_EDIT: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 列表的列定义：（标题，宽度）。顺序必须和填充数据时使用的列下标一致。
///
/// 双击 / 右键时也用它按光标 x 坐标反推用户点的是哪一列，比依赖
/// `LVM_SUBITEMHITTEST` 返回的 iSubItem 更稳妥。
const COLUMNS: [(&str, i32); 4] = [
    ("监控时间", 170),
    ("创建程序", 280),
    ("文件 / 文件夹路径", 440),
    ("类型", 100),
];

/// 列表控件的子窗口 ID。处理 WM_NOTIFY 时要靠它确认通知来自列表。
const LIST_CONTROL_ID: usize = 1;
/// 搜索框的子窗口 ID。WM_COMMAND 里靠它区分搜索框的输入事件。
const SEARCH_CONTROL_ID: usize = 2;

/// 编辑框内容变化时向父窗口发送的通知码（commctrl 之外的 Windows 标准通知）。
const EN_CHANGE_CODE: u32 = 0x0300;

/// 右键菜单命令 ID。
const MENU_OPEN_PATH: u32 = 4001;
const MENU_COPY_ENTRY: u32 = 4002;
const MENU_OPEN_PROGRAM_DIR: u32 = 4003;
const MENU_SET_NOTE: u32 = 4004;
/// 把这条记录里的「创建程序」加入当前黑/白名单。
const MENU_ADD_TO_LIST: u32 = 4005;

/// 剪贴板文本格式（CF_UNICODETEXT）。
const CF_UNICODETEXT: u32 = 13;

/// 在搜索框下方给列表留出的高度（像素）。
const TOOLBAR_HEIGHT: i32 = 36;

/// 当前列表里显示的行数据。
///
/// ListView 只保存文本，双击 / 右键时需要按行号找回完整记录（程序路径、文件路径），
/// 所以刷新列表时同步缓存一份。
static ROW_CACHE: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 从数据库读到的全部记录（未过滤）。切换搜索关键字时从这份数据里筛，不用重新查库。
static ALL_ROWS: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 右键菜单当前针对的行号（-1 表示没有）。
static CONTEXT_ROW: AtomicIsize = AtomicIsize::new(-1);

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
    // 窗口背景统一用白色画刷，配合控件字体让界面更干净。
    window_class.hbrBackground = crate::util::ui_brush();
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
            create_controls(hwnd, HINSTANCE(module.0));
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
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            match id {
                // 搜索框内容变化：立刻按关键字重新过滤列表。
                _ if id == SEARCH_CONTROL_ID as u32 && code == EN_CHANGE_CODE => {
                    apply_filter();
                }
                MENU_OPEN_PATH => {
                    if let Some(row) = context_row() {
                        open_path_target(&row);
                    }
                }
                MENU_COPY_ENTRY => {
                    if let Some(row) = context_row() {
                        copy_to_clipboard(&row.file_path);
                    }
                }
                MENU_OPEN_PROGRAM_DIR => {
                    if let Some(row) = context_row() {
                        crate::util::reveal_in_explorer(&row.process_path);
                    }
                }
                MENU_SET_NOTE => {
                    if let Some(row) = context_row() {
                        edit_program_note(&row);
                    }
                }
                MENU_ADD_TO_LIST => {
                    if let Some(row) = context_row() {
                        add_program_to_list(&row);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            let header = &*(lparam.0 as *const NMHDR);
            if header.idFrom == LIST_CONTROL_ID {
                let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
                match header.code {
                    LVN_ITEMACTIVATE => {
                        // 双击时用鼠标位置做命中测试拿到“行 + 列”。
                        // 这比依赖 NMLISTVIEW 里的 iSubItem 更可靠：
                        // 某些情况下该字段是 -1，会导致双击完全没反应。
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
                                None => {
                                    log::warn!(
                                        "[记录] 双击第 {} 行，但缓存里没有对应记录",
                                        index + 1
                                    )
                                }
                            }
                            open_row_target(index, sub_item);
                        }
                    }
                    NM_RCLICK => {
                        // 右键点在记录上：选中该行并弹出操作菜单。
                        if let Some((index, _)) = hit_test_item(list) {
                            select_row(list, index);
                            show_row_menu(hwnd, index);
                        }
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            // 让「搜索」标签和搜索框背景与窗口一致（白色），
            // 否则系统会画出默认的灰色背景，和白色窗口拼在一起很突兀。
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            SetBkColor(device, COLORREF(0x00FF_FFFF));
            SetTextColor(device, COLORREF(0x0020_2020));
            LRESULT(crate::util::ui_brush().0 as isize)
        }
        WM_SIZE => {
            let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
            if !list.0.is_null() {
                resize_controls(hwnd, list);
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

/// 创建窗口里的所有控件：搜索框 + 记录列表。
unsafe fn create_controls(window: HWND, instance: HINSTANCE) {
    // 列表控件属于通用控件，必须先初始化。
    let controls = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES,
    };
    let _ = InitCommonControlsEx(&controls);

    // 搜索框左边的说明文字。
    let font = crate::util::ui_font();
    let label_class = wide("STATIC");
    let label_text = wide("搜索");
    let label = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(label_class.as_ptr()),
        PCWSTR(label_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        12,
        13,
        40,
        20,
        window,
        HMENU::default(),
        instance,
        None,
    );
    if let Ok(label) = label {
        SendMessageW(label, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    // 搜索框：用户输入时通过 WM_COMMAND(EN_CHANGE) 通知窗口，实时过滤列表。
    let edit_class = wide("EDIT");
    if let Ok(edit) = CreateWindowExW(
        WINDOW_EX_STYLE(0x0000_0200), // WS_EX_CLIENTEDGE：输入框加立体边框
        PCWSTR(edit_class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD
            | WS_VISIBLE
            | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32),
        56,
        8,
        300,
        24,
        window,
        HMENU(SEARCH_CONTROL_ID as *mut std::ffi::c_void),
        instance,
        None,
    ) {
        SEARCH_EDIT.store(edit.0 as isize, Ordering::SeqCst);
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    let list_class = wide("SysListView32");
    let list = match CreateWindowExW(
        WINDOW_EX_STYLE(0x0000_0200), // WS_EX_CLIENTEDGE：列表加立体边框
        PCWSTR(list_class.as_ptr()),
        PCWSTR::null(),
        LIST_STYLE,
        0,
        TOOLBAR_HEIGHT,
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
    SendMessageW(list, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));

    // 扩展样式：整行选中、网格线、双缓冲（滚动时不闪）。
    let extended = LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER;
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        WPARAM(extended as usize),
        LPARAM(extended as isize),
    );

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

    resize_controls(window, list);
}

/// 让搜索框和列表跟随窗口大小变化。
unsafe fn resize_controls(window: HWND, list: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(window, &mut rect).is_err() {
        return;
    }
    let client_width = rect.right - rect.left;
    let client_height = rect.bottom - rect.top;

    // 搜索框宽度跟着窗口走，右边留 8 像素边距。
    let edit = HWND(SEARCH_EDIT.load(Ordering::SeqCst) as *mut c_void);
    if !edit.0.is_null() {
        let edit_width = (client_width - 56 - 8).max(120);
        let _ = MoveWindow(edit, 56, 8, edit_width, 24, true);
    }

    // 列表放在搜索栏下方。
    let list_width = (client_width - 16).max(100);
    let list_height = (client_height - TOOLBAR_HEIGHT - 8).max(100);
    let _ = MoveWindow(list, 8, TOOLBAR_HEIGHT, list_width, list_height, true);
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
        Some(db) => db.recent(MAX_ROWS).unwrap_or_default(),
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
    let row = cached_row(index);
    let Some(row) = row else {
        return;
    };

    match sub_item {
        1 => crate::util::reveal_in_explorer(&row.process_path),
        2 => open_path_target(&row),
        // 时间列不做操作。
        _ => {}
    }
}

/// 按行号从缓存里取回完整记录（双击 / 右键共用）。
fn cached_row(index: usize) -> Option<FileOrigin> {
    match ROW_CACHE.lock() {
        Ok(cache) => cache.get(index).cloned(),
        Err(poisoned) => poisoned.into_inner().get(index).cloned(),
    }
}

/// 打开记录里的文件 / 文件夹所在位置。
///
/// - 普通文件：打开所在目录并选中它
/// - 文件夹：直接打开这个文件夹
fn open_path_target(row: &FileOrigin) {
    if std::path::Path::new(&row.file_path).is_dir() {
        crate::util::open_directory(&row.file_path);
    } else {
        crate::util::reveal_in_explorer(&row.file_path);
    }
}

/// 从数据库重新读取记录，再按当前搜索关键字刷新列表。
fn refresh_list() {
    let rows = load_rows();
    if let Ok(mut cache) = ALL_ROWS.lock() {
        *cache = rows;
    }
    apply_filter();
}

/// 按搜索框里的关键字过滤记录，并重新填充列表。
fn apply_filter() {
    let keyword = search_text().to_lowercase();
    let rows = ALL_ROWS
        .lock()
        .map(|cache| cache.clone())
        .unwrap_or_default();

    let filtered: Vec<FileOrigin> = if keyword.is_empty() {
        rows
    } else {
        rows.into_iter()
            .filter(|row| matches_keyword(row, &keyword))
            .collect()
    };

    fill_list(&filtered);
}

/// 判断一条记录是否匹配搜索关键字（程序名 / 路径 / 类型 / 备注，忽略大小写）。
fn matches_keyword(row: &FileOrigin, keyword: &str) -> bool {
    row.process_name.to_lowercase().contains(keyword)
        || row.file_path.to_lowercase().contains(keyword)
        || row.process_path.to_lowercase().contains(keyword)
        || row.note.to_lowercase().contains(keyword)
        || action_label(&row.action).to_lowercase().contains(keyword)
}

/// 读取搜索框里当前的文字。
fn search_text() -> String {
    let edit = HWND(SEARCH_EDIT.load(Ordering::SeqCst) as *mut c_void);
    if edit.0.is_null() {
        return String::new();
    }
    let mut buffer = vec![0u16; 512];
    let length = unsafe { GetWindowTextW(edit, &mut buffer) }.max(0) as usize;
    let length = length.min(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

/// 把给定记录填入列表，并同步行缓存（双击 / 右键要靠它按行号找回记录）。
fn fill_list(rows: &[FileOrigin]) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    if let Ok(mut cache) = ROW_CACHE.lock() {
        *cache = rows.to_vec();
    }

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

            let program_text = wide(&program_column_text(row));
            insert_sub_item(list, row_index, 1, &program_text);

            let path_text = wide(&row.file_path);
            insert_sub_item(list, row_index, 2, &path_text);

            // 类型列：让用户直观看到这条记录属于哪类监控。
            let action_text = wide(&action_label(&row.action));
            insert_sub_item(list, row_index, 3, &action_text);
        }
    }
}

/// 选中某一行，让用户看清右键菜单针对的是哪条记录。
unsafe fn select_row(list: HWND, index: usize) {
    let mut item = LVITEMW::default();
    item.mask = LVIF_STATE;
    item.stateMask = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
    item.state = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM(index),
        LPARAM(&item as *const _ as isize),
    );
}

/// 在右键位置弹出记录操作菜单。
unsafe fn show_row_menu(window: HWND, index: usize) {
    CONTEXT_ROW.store(index as isize, Ordering::SeqCst);

    let Ok(menu) = CreatePopupMenu() else {
        return;
    };

    let items: [(u32, &str); 5] = [
        (MENU_OPEN_PATH, "打开文件路径"),
        (MENU_COPY_ENTRY, "复制词条"),
        (MENU_OPEN_PROGRAM_DIR, "打开创建程序所在目录"),
        (MENU_SET_NOTE, "设置程序备注…"),
        (MENU_ADD_TO_LIST, menu_add_to_list_text()),
    ];

    for (position, (command, text)) in items.iter().enumerate() {
        let text = wide(text);
        let _ = InsertMenuW(
            menu,
            position as u32,
            MF_BYPOSITION | MF_STRING,
            *command as usize,
            PCWSTR(text.as_ptr()),
        );
    }
    // 前两项是“对这条记录本身的操作”，后面几项针对创建者程序，用分隔线隔开。
    let _ = InsertMenuW(menu, 2, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());

    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_err() {
        let _ = DestroyMenu(menu);
        return;
    }

    // TrackPopupMenu 要求窗口在前台，否则点菜单外面菜单不会消失。
    let _ = SetForegroundWindow(window);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        window,
        None,
    );
    let _ = DestroyMenu(menu);
    // 让菜单正常关闭（Windows 官方示例里的做法）。
    let _ = PostMessageW(window, WM_NULL, WPARAM(0), LPARAM(0));
}

/// 右键菜单当前针对的记录。
fn context_row() -> Option<FileOrigin> {
    let index = CONTEXT_ROW.load(Ordering::SeqCst);
    if index < 0 {
        return None;
    }
    cached_row(index as usize)
}

/// 右键菜单里「加入名单」这一项的显示文字。
///
/// 名称随设置里的模式变化，用户不用打开设置也能知道会加进哪个名单。
fn menu_add_to_list_text() -> &'static str {
    match crate::config::Config::load_or_default() {
        Ok(config) if config.is_white_list() => "加入白名单（此程序）",
        _ => "加入黑名单（此程序）",
    }
}

/// 把当前记录的「创建程序」加入黑/白名单。
///
/// 加入黑名单后，这个程序以后再创建/修改文件都不会被记录；
/// 加入白名单后，则只有这个程序的操作会被记录（白名单模式下）。
fn add_program_to_list(row: &FileOrigin) {
    let config = match crate::config::Config::load_or_default() {
        Ok(config) => config,
        Err(error) => {
            log::error!("[名单] 读取配置失败：{:#}", error);
            return;
        }
    };

    let result = crate::config::Config::default_data_dir()
        .map_err(|error| anyhow::anyhow!("{:#}", error))
        .and_then(|data_dir| Database::init(&data_dir))
        .and_then(|db| db.add_list_entry(&config.list_mode, &row.process_path, false));

    let window = HWND(HISTORY_WINDOW.load(Ordering::SeqCst) as *mut c_void);
    match result {
        Ok(()) => {
            log::info!(
                "[名单] 已把 {} 加入{}",
                row.process_path,
                config.list_mode_name()
            );
            let text = wide(&format!(
                "已把下面这个程序加入{}：\n\n{}\n\n名单可以在托盘菜单的「名单管理」里查看和移除。",
                config.list_mode_name(),
                row.process_path
            ));
            let title = wide("文件来源追踪");
            unsafe {
                let _ = MessageBoxW(
                    window,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
        }
        Err(error) => log::error!("[名单] 加入名单失败：{:#}", error),
    }
}

/// 打开备注编辑窗口，并把用户输入的备注保存到数据库。
///
/// 备注按「程序完整路径」保存：同一个程序以后产生的每条记录都会带上它；
/// 输入空内容表示删除这条备注。
fn edit_program_note(row: &FileOrigin) {
    let owner = HWND(HISTORY_WINDOW.load(Ordering::SeqCst) as *mut c_void);

    // 弹出模态窗口；用户点“取消”时返回 None，直接结束。
    let Some(note) = crate::note::show(owner, &row.process_name, &row.process_path, &row.note)
    else {
        return;
    };

    // 记录窗口用的是只读连接，写备注必须另开一个可写连接。
    let result = crate::config::Config::default_data_dir()
        .map_err(|error| anyhow::anyhow!("{:#}", error))
        .and_then(|data_dir| Database::init(&data_dir))
        .and_then(|db| db.set_program_note(&row.process_path, &note));

    match result {
        Ok(()) => {
            log::info!(
                "[记录] 程序备注已保存：{} -> {}",
                row.process_path,
                if note.is_empty() {
                    "（已清除）"
                } else {
                    note.as_str()
                }
            );
            // 立刻刷新列表：同一个程序的其它记录也要马上显示新备注。
            refresh_list();
        }
        Err(error) => log::error!("[记录] 保存程序备注失败：{:#}", error),
    }
}

/// 把文本复制到剪贴板（Unicode 文本）。
fn copy_to_clipboard(text: &str) {
    let utf16 = wide(text);
    let bytes = utf16.len() * 2;

    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return;
        }
        let _ = EmptyClipboard();

        // 剪贴板要求用“可移动的全局内存”存放数据，系统随后接管这块内存的所有权。
        if let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
            let pointer = GlobalLock(handle);
            if !pointer.is_null() {
                std::ptr::copy_nonoverlapping(
                    utf16.as_ptr() as *const u8,
                    pointer as *mut u8,
                    bytes,
                );
                let _ = GlobalUnlock(handle);
            }
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(handle.0));
        }

        let _ = CloseClipboard();
    }
}

/// 「创建程序」列显示的文字。
///
/// 有备注时把备注接在程序名后面，例如 `notepad.exe（我自己启动的记事本）`，
/// 这样用户扫一眼就知道这个程序是干什么的。备注太长时截断，
/// 避免一条记录把整列撑开（完整内容可以通过搜索框查）。
fn program_column_text(row: &FileOrigin) -> String {
    /// 列表里最多显示多少个备注字符。
    const MAX_NOTE_CHARS: usize = 24;

    let note = row.note.trim();
    if note.is_empty() {
        return row.process_name.clone();
    }

    if note.chars().count() > MAX_NOTE_CHARS {
        let short: String = note.chars().take(MAX_NOTE_CHARS).collect();
        format!("{}（{}…）", row.process_name, short)
    } else {
        format!("{}（{}）", row.process_name, note)
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
        assert_eq!(column_at_x(449), 1);
        assert_eq!(column_at_x(450), 2);
        assert_eq!(column_at_x(889), 2);
        assert_eq!(column_at_x(890), 3);
        assert_eq!(column_at_x(5000), 3);
    }
}
