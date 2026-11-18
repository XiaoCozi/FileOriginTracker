//! 「程序统计」窗口。
//!
//! 按「创建程序」聚合统计它一共创建了多少文件 / 文件夹、修改、删除多少，
//! 并支持按类型展开查看明细。
//!
//! 与「最近记录」窗口的区别：
//! - 那边是「一条一条记录」的时间流；
//! - 这边是「一个程序一行」的汇总表，用来快速看出哪个程序在大量写盘。
//!
//! 交互设计：
//! - 每个程序占一行，后面五列是各类型的计数，最后一列是合计
//! - 双击某个计数单元格（例如「创建文件夹」列）→ 在该程序行下方展开
//!   这一类型的明细（只包含该类型，和这一列的数字严格对应），再双击收起
//! - 双击「合计」列 → 展开该程序所有类型的明细（混排、按时间倒序）
//! - 同一个程序可以同时展开多个类型，明细按列顺序排列
//! - 双击「程序」列 → 收起这个程序的所有明细
//! - 双击明细行 → 打开该文件所在目录
//! - 在程序行 / 明细行上右键 → 与「最近记录」窗口一致的记录操作：
//!   打开文件路径、复制词条、打开创建程序所在目录、设置程序备注、加入名单
//!   （「打开文件路径」「复制词条」只在明细行上提供，程序行没有具体文件路径）
//! - 右键菜单底部保留删除功能：
//!   - 在程序行上右键 → 删除该程序的全部记录
//!   - 在程序行的某个计数单元格上右键 → 删除该程序这一类型的全部记录
//!   - 在明细行上右键 → 删除这一条记录（都会先弹确认框）
//! - 点击列头排序，再点一次切换升序 / 降序
//!
//! 性能方面的考虑与「最近记录」窗口一致：
//! - 只有窗口处于激活状态才每 10 秒刷新一次
//! - 数据没有变化时直接返回，不重建列表（不会闪、不会跳回顶部）
//! - 单个程序每种类型最多展示 500 条明细，避免某个程序几万条记录把界面拖慢

use anyhow::Result;
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    InvalidateRect, ScreenToClient, SetBkColor, SetTextColor, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCFMT_LEFT, LVCFMT_RIGHT,
    LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVHITTESTINFO, LVIF_STATE, LVIF_TEXT, LVITEMW,
    LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_GETTOPINDEX, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW,
    LVM_SUBITEMHITTEST, LVNI_SELECTED, LVN_COLUMNCLICK, LVN_ITEMACTIVATE, LVS_EX_DOUBLEBUFFER,
    LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_EX_INFOTIP, LVS_EX_TWOCLICKACTIVATE, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMLISTVIEW, NM_RCLICK,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetClientRect, GetCursorPos,
    InsertMenuW, KillTimer, LoadCursorW, MessageBoxW, MoveWindow, PostMessageW, RegisterClassExW,
    SendMessageW, SetForegroundWindow, SetTimer, ShowWindow, TrackPopupMenu, HMENU, ICON_BIG,
    ICON_SMALL, IDC_ARROW, IDYES, MB_ICONWARNING, MB_YESNO, MF_BYPOSITION, MF_SEPARATOR,
    MF_STRING, SW_HIDE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN, WA_INACTIVE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORSTATIC,
    WM_NOTIFY, WM_NULL, WM_SETFONT, WM_SETICON, WM_SETREDRAW, WM_SIZE, WM_TIMER, WNDCLASSEXW,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::db::{Database, FileOrigin, ProgramStat};
use crate::util::wide;

const WINDOW_TITLE: &str = "文件来源追踪 - 程序统计";
const WINDOW_CLASS: &str = "FileOriginTrackerStatsWindow";

/// 自动刷新间隔（毫秒）。只在窗口激活时启用。
const REFRESH_TIMER_ID: usize = 1;
const REFRESH_INTERVAL_MS: u32 = 10_000;

/// 单个程序、单个类型最多展开多少条明细。
const MAX_DETAIL_ROWS: u32 = 500;

/// 顶部提示文字区域的高度。
const TIP_HEIGHT: i32 = 32;

/// 列表控件 ID，用于在 WM_NOTIFY 中确认通知来源。
const LIST_CONTROL_ID: usize = 1;

/// 列定义：（标题，宽度）。
///
/// 第 0 列放程序名，展开时这一列也用来显示明细文本，所以给得比较宽；
/// 第 1~5 列是五类计数，第 6 列是合计。
const COLUMNS: [(&str, i32); 7] = [
    ("程序", 560),
    ("创建文件", 80),
    ("创建文件夹", 90),
    ("修改", 70),
    ("删除文件", 80),
    ("删除文件夹", 90),
    ("合计", 80),
];

/// 第 1~5 列依次对应的动作名，顺序必须与 [`COLUMNS`] 一致。
const ACTION_COLUMNS: [&str; 5] = [
    "create_file",
    "create_dir",
    "modify",
    "delete_file",
    "delete_dir",
];

/// 「程序」列的下标。
const PROGRAM_COLUMN: i32 = 0;
/// 「合计」列的下标。
const TOTAL_COLUMN: i32 = 6;

/// 展开状态里用来表示「所有类型」的伪动作名。
///
/// 双击「合计」列时把 `(程序路径, "all")` 记进展开集合，
/// 表示这个程序要展示全部类型的明细（混排、按时间倒序）。
const ALL_ACTIONS: &str = "all";

/// 右键菜单命令 ID：删除整个程序的所有记录。
const MENU_DELETE_PROGRAM: u32 = 5001;
/// 右键菜单命令 ID：删除一条明细记录。
const MENU_DELETE_RECORD: u32 = 5002;
/// 右键菜单命令 ID：删除某个程序的某一类动作的全部记录。
const MENU_DELETE_ACTION: u32 = 5003;

/// 以下命令从「最近记录」窗口的右键菜单移植过来。
/// 命令 ID 与那边的 4001~4005 错开，避免两个窗口互相干扰。
const MENU_OPEN_PATH: u32 = 5011;
const MENU_COPY_ENTRY: u32 = 5012;
const MENU_OPEN_PROGRAM_DIR: u32 = 5013;
const MENU_SET_NOTE: u32 = 5014;
const MENU_ADD_TO_LIST: u32 = 5015;

/// WM_VSCROLL：标准垂直滚动消息（重建列表后用来滚回原来的位置）。
const WM_VSCROLL: u32 = 0x0115;
/// SB_LINEDOWN：WM_VSCROLL 的 wParam 取值，表示向下滚动一行。
const SB_LINEDOWN: usize = 1;

static STATS_WINDOW: AtomicIsize = AtomicIsize::new(0);
static LIST_VIEW: AtomicIsize = AtomicIsize::new(0);

/// 当前排序列。默认 6（合计），降序：写得最多的程序排最前面。
static SORT_COLUMN: AtomicIsize = AtomicIsize::new(TOTAL_COLUMN as isize);
/// 是否升序。默认 false（数值列从大到小更直观）。
static SORT_ASCENDING: AtomicBool = AtomicBool::new(false);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 从数据库读到的程序汇总（未排序）。
static ALL_STATS: Lazy<Mutex<Vec<ProgramStat>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 当前展开的「程序 + 类型」组合，元素是 (程序完整路径, 动作名)。
///
/// 用 Vec 而不是 HashSet：组合数量很少（一般只有几个），
/// 线性查找完全够用，还能保持展开顺序方便调试。
static EXPANDED: Lazy<Mutex<Vec<(String, String)>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 当前列表里显示的行。ListView 本身只存文本，
/// 双击时要靠行号回到这里取出完整数据。
static ROW_CACHE: Lazy<Mutex<Vec<StatsRow>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 统计窗口使用的只读数据库连接（惰性打开）。
static STATS_DB: Lazy<Mutex<Option<Database>>> = Lazy::new(|| Mutex::new(None));

/// 右键菜单当前针对的行号（-1 表示没有）。
static CONTEXT_ROW: AtomicIsize = AtomicIsize::new(-1);

/// 右键菜单当前针对的列号（用于判断点的是哪个计数单元格）。
static CONTEXT_SUBITEM: AtomicIsize = AtomicIsize::new(-1);

/// 列表中的一行：要么是程序汇总行，要么是展开出来的明细行。
#[derive(Debug, Clone, PartialEq)]
enum StatsRow {
    /// 程序汇总行。
    Program(ProgramStat),
    /// 明细行：属于某个程序的某一种类型。
    Detail { action: String, record: FileOrigin },
}

/// 显示（或重新激活）程序统计窗口。
pub fn show() -> Result<()> {
    let existing = STATS_WINDOW.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
        }
        refresh_stats(true);
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
            120,
            100,
            1140,
            660,
            HWND::default(),
            windows::Win32::UI::WindowsAndMessaging::HMENU::default(),
            instance,
            None,
        )?
    };

    STATS_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = UpdateWindow(hwnd);
    }
    refresh_stats(false);
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
    // 与其它窗口一致的白色背景。
    window_class.hbrBackground = crate::util::ui_brush();
    window_class.hCursor = unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW)? };
    window_class.lpszClassName = PCWSTR(class_name.as_ptr());

    if unsafe { RegisterClassExW(&window_class) } == 0 {
        CLASS_REGISTERED.store(false, Ordering::SeqCst);
        return Err(anyhow::anyhow!("注册程序统计窗口类失败"));
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
            // 右键菜单选择的命令会以 WM_COMMAND 回到这里。
            match (wparam.0 & 0xFFFF) as u32 {
                MENU_DELETE_PROGRAM => delete_program_command(hwnd),
                MENU_DELETE_RECORD => delete_record_command(hwnd),
                MENU_DELETE_ACTION => delete_action_command(hwnd),
                // ---- 以下是从「最近记录」窗口移植过来的记录操作 ----
                // 「打开文件路径」「复制词条」需要具体文件路径，只对明细行有效。
                MENU_OPEN_PATH => {
                    if let Some(StatsRow::Detail { record, .. }) = context_row() {
                        crate::history::open_path_target(&record);
                    }
                }
                MENU_COPY_ENTRY => {
                    if let Some(StatsRow::Detail { record, .. }) = context_row() {
                        crate::history::copy_to_clipboard(&record.file_path);
                    }
                }
                // 程序级操作：程序汇总行和明细行都带着创建程序信息，两种行都可用。
                MENU_OPEN_PROGRAM_DIR => {
                    if let Some((_, process_path, _)) = context_program() {
                        crate::util::reveal_in_explorer(&process_path);
                    }
                }
                MENU_SET_NOTE => {
                    if let Some((process_name, process_path, note)) = context_program() {
                        if crate::history::edit_program_note(
                            hwnd,
                            &process_name,
                            &process_path,
                            &note,
                        ) {
                            // 备注直接显示在「程序」列，保存成功后重读统计。
                            refresh_stats(true);
                        }
                    }
                }
                MENU_ADD_TO_LIST => {
                    if let Some((_, process_path, _)) = context_program() {
                        crate::history::add_program_to_list(hwnd, &process_path);
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
                        // 双击：根据点是「程序行」还是「明细行」执行不同操作。
                        handle_double_click(hwnd, list);
                    }
                    NM_RCLICK => {
                        // 右键：选中该行并弹出删除菜单（菜单内容取决于行和列）。
                        if let Some((index, sub_item)) = hit_test_item(list) {
                            select_row(list, index);
                            show_row_menu(hwnd, index, sub_item);
                        }
                    }
                    LVN_COLUMNCLICK => {
                        let info = &*(lparam.0 as *const NMLISTVIEW);
                        toggle_sort(info.iSubItem);
                        rebuild(true);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // 顶部提示文字的背景与窗口一致（白色），避免出现灰色块。
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
                refresh_stats(true);
                let _ = SetTimer(hwnd, REFRESH_TIMER_ID, REFRESH_INTERVAL_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == REFRESH_TIMER_ID {
                refresh_stats(true);
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

/// 创建窗口里的控件：一行提示文字 + 统计列表。
unsafe fn create_controls(window: HWND, instance: HINSTANCE) {
    // ListView 属于通用控件，必须先初始化。
    let controls = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES,
    };
    let _ = InitCommonControlsEx(&controls);

    let font = crate::util::ui_font();

    // 顶部操作提示：告诉用户「展开」是怎么操作的，否则很容易不知道该点哪里。
    let tip_class = wide("STATIC");
    let tip_text = wide("双击计数单元格展开该类型的明细，双击「合计」展开全部类型的明细，再双击收起；双击明细行打开文件所在目录；点击列头排序。");
    if let Ok(tip) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(tip_class.as_ptr()),
        PCWSTR(tip_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
        12,
        10,
        1100,
        20,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU::default(),
        instance,
        None,
    ) {
        SendMessageW(tip, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    let list_class = wide("SysListView32");
    let list_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS);
    let list = match CreateWindowExW(
        WINDOW_EX_STYLE(0x0000_0200), // WS_EX_CLIENTEDGE：立体边框
        PCWSTR(list_class.as_ptr()),
        PCWSTR::null(),
        list_style,
        0,
        TIP_HEIGHT,
        100,
        100,
        window,
        windows::Win32::UI::WindowsAndMessaging::HMENU(LIST_CONTROL_ID as *mut std::ffi::c_void),
        instance,
        None,
    ) {
        Ok(list) => list,
        Err(_) => return,
    };

    LIST_VIEW.store(list.0 as isize, Ordering::SeqCst);
    SendMessageW(list, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));

    // 整行选中 + 网格线 + 双缓冲 + 长文本悬停提示（路径太长时用得上）。
    // LVS_EX_TWOCLICKACTIVATE：双击时才会发送 LVN_ITEMACTIVATE，
    // 没有这个扩展样式，双击展开明细完全不会触发。
    let extended = LVS_EX_FULLROWSELECT
        | LVS_EX_GRIDLINES
        | LVS_EX_DOUBLEBUFFER
        | LVS_EX_INFOTIP
        | LVS_EX_TWOCLICKACTIVATE;
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        WPARAM(extended as usize),
        LPARAM(extended as isize),
    );

    for (index, (title, width)) in COLUMNS.iter().enumerate() {
        let title = wide(title);
        let mut column = LVCOLUMNW::default();
        column.mask = LVCF_FMT | LVCF_TEXT | LVCF_WIDTH;
        column.cx = *width;
        // 程序 / 明细列左对齐，五类计数和合计右对齐，方便一眼比较大小。
        column.fmt = if index == 0 {
            LVCFMT_LEFT
        } else {
            LVCFMT_RIGHT
        };
        column.pszText = PWSTR(title.as_ptr() as *mut u16);
        SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            WPARAM(index),
            LPARAM(&column as *const _ as isize),
        );
    }

    update_sort_headers();
    resize_controls(window, list);
}

/// 让提示文字和列表跟随窗口大小变化。
unsafe fn resize_controls(window: HWND, list: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(window, &mut rect).is_err() {
        return;
    }
    let client_width = rect.right - rect.left;
    let client_height = rect.bottom - rect.top;

    let list_width = (client_width - 16).max(100);
    let list_height = (client_height - TIP_HEIGHT - 8).max(100);
    let _ = MoveWindow(list, 8, TIP_HEIGHT, list_width, list_height, true);
}

/// 从数据库读取程序统计（失败时返回空列表）。
fn load_stats() -> Vec<ProgramStat> {
    let mut guard = match STATS_DB.lock() {
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
        Some(db) => db.program_stats().unwrap_or_default(),
        None => Vec::new(),
    }
}

/// 取某个程序某一类动作的明细。
fn load_details(process_path: &str, action: &str) -> Vec<FileOrigin> {
    let mut guard = match STATS_DB.lock() {
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
        Some(db) => db
            .records_by_process_action(process_path, action, MAX_DETAIL_ROWS)
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// 取某个程序全部类型的明细（按时间倒序）。
fn load_all_details(process_path: &str) -> Vec<FileOrigin> {
    let mut guard = match STATS_DB.lock() {
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
        Some(db) => db
            .records_by_process(process_path, MAX_DETAIL_ROWS)
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// 后台定时刷新。
///
/// 供其它窗口调用：如果统计窗口已经打开，立即按数据库里的最新数据刷新一次。
///
/// 典型场景是「最近记录」窗口改了程序备注——备注存在数据库里，
/// 统计窗口只要重读数据就能看到，不需要两个窗口互相传值。
///
/// 窗口还没创建过（从未打开过）时什么也不做，避免在空句柄上操作控件。
pub(crate) fn refresh_external() {
    if STATS_WINDOW.load(Ordering::SeqCst) == 0 || LIST_VIEW.load(Ordering::SeqCst) == 0 {
        return;
    }
    // 备注变化会体现在 ProgramStat 的 note 字段里，`refresh_stats` 比对到
    // 数据不同就会自动重建列表，因此这里直接调用即可。
    refresh_stats(true);
}

/// 汇总数据没有变化就直接返回：这样用户正在看某一屏幕的统计时，
/// 列表不会每 10 秒闪一下、也不会被弹回顶部。
fn refresh_stats(keep_scroll: bool) {
    let stats = load_stats();

    let unchanged = match ALL_STATS.lock() {
        Ok(cache) => *cache == stats,
        Err(poisoned) => *poisoned.into_inner() == stats,
    };
    if unchanged {
        return;
    }

    match ALL_STATS.lock() {
        Ok(mut cache) => *cache = stats,
        Err(poisoned) => *poisoned.into_inner() = stats,
    }
    rebuild(keep_scroll);
}

/// 按当前排序和展开状态重新生成整个列表。
fn rebuild(keep_scroll: bool) {
    let mut stats = match ALL_STATS.lock() {
        Ok(cache) => cache.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    sort_stats(&mut stats);

    let rows = build_rows(&stats);
    fill_list(&rows, keep_scroll);
}

/// 把「程序行 + 已展开的明细行」拼成最终要显示的行序列。
///
/// 明细按固定类型顺序插入，保证同一个程序同时展开多个类型时排列稳定：
/// 创建文件 → 创建文件夹 → 修改 → 删除文件 → 删除文件夹。
fn build_rows(stats: &[ProgramStat]) -> Vec<StatsRow> {
    let expanded = match EXPANDED.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };

    let mut rows = Vec::new();
    for stat in stats {
        rows.push(StatsRow::Program(stat.clone()));

        // 「合计」展开：该程序所有类型的明细混在一起，按时间倒序。
        let all_expanded = expanded
            .iter()
            .any(|(path, name)| path == &stat.process_path && name == ALL_ACTIONS);
        if all_expanded {
            for record in load_all_details(&stat.process_path) {
                let action = record.action.clone();
                rows.push(StatsRow::Detail { action, record });
            }
            continue;
        }

        for action in ACTION_COLUMNS {
            // 计数为 0 的类型没有必要展开（展开也只会是空列表）。
            if stat.count_of(action) <= 0 {
                continue;
            }
            let is_expanded = expanded
                .iter()
                .any(|(path, name)| path == &stat.process_path && name == action);
            if !is_expanded {
                continue;
            }

            for record in load_details(&stat.process_path, action) {
                rows.push(StatsRow::Detail {
                    action: action.to_string(),
                    record,
                });
            }
        }
    }
    rows
}

/// 双击处理。
///
/// - 明细行：打开该文件 / 文件夹所在位置
/// - 程序行的「程序」列：收起该程序的全部明细
/// - 程序行的计数列：展开 / 收起对应类型的明细
/// - 程序行的「合计」列：不做操作
unsafe fn handle_double_click(window: HWND, list: HWND) {
    let Some((index, sub_item)) = hit_test_item(list) else {
        return;
    };
    let row = cached_row(index);
    let Some(row) = row else {
        return;
    };

    match row {
        StatsRow::Detail { record, .. } => open_detail_target(&record),
        StatsRow::Program(stat) => {
            if sub_item == PROGRAM_COLUMN {
                // 双击程序名：有展开就全部收起，没有展开则不动。
                collapse_all(&stat.process_path);
                rebuild(true);
            } else if sub_item == TOTAL_COLUMN {
                // 双击「合计」：展开 / 收起该程序所有类型的明细。
                toggle_all(&stat.process_path);
                rebuild(true);
            } else if (1..=5).contains(&sub_item) {
                let action = ACTION_COLUMNS[(sub_item - 1) as usize];
                // 该类型没有记录时不做任何事，避免出现「点了没反应还以为是卡住」。
                if stat.count_of(action) <= 0 {
                    return;
                }
                toggle_expand(&stat.process_path, action);
                rebuild(true);
            }
        }
    }

    let _ = window;
}

/// 切换某个「程序 + 类型」的展开状态。
///
/// 如果这个程序当前是「全部类型」展开状态，会先收起它，
/// 避免同一批记录出现两次。
fn toggle_expand(process_path: &str, action: &str) {
    let mut guard = match EXPANDED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    // 单类型展开与「全部类型」展开互斥。
    guard.retain(|(path, name)| !(path == process_path && name == ALL_ACTIONS));

    if let Some(position) = guard
        .iter()
        .position(|(path, name)| path == process_path && name == action)
    {
        guard.remove(position);
    } else {
        guard.push((process_path.to_string(), action.to_string()));
    }
}

/// 切换某个程序「全部类型」的展开状态（双击「合计」列）。
///
/// 展开时顺带清掉该程序已展开的单类型，收起时也一并清掉，
/// 保证明细不会重复出现。
fn toggle_all(process_path: &str) {
    let mut guard = match EXPANDED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    let was_open = guard
        .iter()
        .any(|(path, name)| path == process_path && name == ALL_ACTIONS);
    guard.retain(|(path, _)| path != process_path);

    if !was_open {
        guard.push((process_path.to_string(), ALL_ACTIONS.to_string()));
    }
}

/// 收起某个程序的所有明细。
fn collapse_all(process_path: &str) {
    let mut guard = match EXPANDED.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.retain(|(path, _)| path != process_path);
}

/// 按行号从缓存里取出整行数据。
fn cached_row(index: usize) -> Option<StatsRow> {
    match ROW_CACHE.lock() {
        Ok(cache) => cache.get(index).cloned(),
        Err(poisoned) => poisoned.into_inner().get(index).cloned(),
    }
}

/// 在右键位置弹出操作菜单。
///
/// 菜单分两部分：
/// 1. 「最近记录」窗口里那组记录操作（打开路径 / 复制词条 / 打开程序目录 /
///    设置备注 / 加入名单）；
/// 2. 删除操作，内容随「行 + 列」变化：
///    - 程序行的某个计数单元格：删除该程序这一类型的全部记录
///    - 程序行的程序名 / 合计列：删除该程序的全部记录
///    - 明细行：删除这一条记录
///
/// 「打开文件路径」「复制词条」只对明细行提供，因为程序汇总行没有具体文件路径。
unsafe fn show_row_menu(window: HWND, index: usize, sub_item: i32) {
    let Some(row) = cached_row(index) else {
        return;
    };
    CONTEXT_ROW.store(index as isize, Ordering::SeqCst);
    CONTEXT_SUBITEM.store(sub_item as isize, Ordering::SeqCst);

    let Ok(menu) = CreatePopupMenu() else {
        return;
    };

    // ---- 第一部分：移植过来的记录操作 ----
    let mut position: u32 = 0;
    if matches!(row, StatsRow::Detail { .. }) {
        append_menu_item(menu, position, MENU_OPEN_PATH, "打开文件路径");
        position += 1;
        append_menu_item(menu, position, MENU_COPY_ENTRY, "复制词条");
        position += 1;
    }
    append_menu_item(menu, position, MENU_OPEN_PROGRAM_DIR, "打开创建程序所在目录");
    position += 1;
    append_menu_item(menu, position, MENU_SET_NOTE, "设置程序备注…");
    position += 1;
    append_menu_item(
        menu,
        position,
        MENU_ADD_TO_LIST,
        crate::history::menu_add_to_list_text(),
    );
    position += 1;

    // ---- 第二部分：原有的删除操作，用分隔线和上面隔开 ----
    let delete_item = match &row {
        StatsRow::Detail { .. } => {
            Some((MENU_DELETE_RECORD, "删除这条记录".to_string()))
        }
        StatsRow::Program(stat) => {
            if (1..=5).contains(&sub_item) {
                let action = ACTION_COLUMNS[(sub_item - 1) as usize];
                // 该类型没有任何记录时不必显示删除项（删了也是 0 条）。
                if stat.count_of(action) <= 0 {
                    None
                } else {
                    Some((
                        MENU_DELETE_ACTION,
                        format!(
                            "删除「{}」的全部{}记录",
                            stat.process_name,
                            action_label(action)
                        ),
                    ))
                }
            } else {
                Some((
                    MENU_DELETE_PROGRAM,
                    format!("删除「{}」的全部记录", stat.process_name),
                ))
            }
        }
    };

    if let Some((command, text)) = delete_item {
        let _ = InsertMenuW(menu, position, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
        let text = wide(&text);
        let _ = InsertMenuW(
            menu,
            position + 1,
            MF_BYPOSITION | MF_STRING,
            command as usize,
            PCWSTR(text.as_ptr()),
        );
    }

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

/// 在菜单末尾追加一个普通字符串菜单项。
///
/// `position` 用「按位置插入」的方式指定顺序，调用方按 0、1、2… 递增即可。
unsafe fn append_menu_item(menu: HMENU, position: u32, command: u32, text: &str) {
    let text = wide(text);
    let _ = InsertMenuW(
        menu,
        position,
        MF_BYPOSITION | MF_STRING,
        command as usize,
        PCWSTR(text.as_ptr()),
    );
}

/// 右键菜单当前针对的行。
fn context_row() -> Option<StatsRow> {
    let index = CONTEXT_ROW.load(Ordering::SeqCst);
    if index < 0 {
        return None;
    }
    cached_row(index as usize)
}

/// 右键菜单当前针对的「创建程序」信息：（程序名、程序路径、备注）。
///
/// 程序汇总行和明细行都带着这三项，所以「打开创建程序所在目录」
/// 「设置程序备注」「加入名单」对两种行都可用。
fn context_program() -> Option<(String, String, String)> {
    match context_row()? {
        StatsRow::Program(stat) => Some((stat.process_name, stat.process_path, stat.note)),
        StatsRow::Detail { record, .. } => {
            Some((record.process_name, record.process_path, record.note))
        }
    }
}

/// 弹出「是 / 否」确认框，返回用户是否点了「是」。
///
/// 删除记录不可恢复，必须先确认，避免手滑。
unsafe fn confirm(window: HWND, text: &str) -> bool {
    let text = wide(text);
    let title = wide("确认删除");
    MessageBoxW(
        window,
        PCWSTR(text.as_ptr()),
        PCWSTR(title.as_ptr()),
        MB_YESNO | MB_ICONWARNING,
    ) == IDYES
}

/// 右键「删除该程序的全部记录」。
unsafe fn delete_program_command(window: HWND) {
    let Some(StatsRow::Program(stat)) = context_row() else {
        return;
    };

    let message = format!(
        "确定要删除「{}」的全部记录吗？\n\n程序路径：{}\n\n它的创建、修改、删除记录都会一并删除，且无法恢复。",
        stat.process_name, stat.process_path
    );
    if !confirm(window, &message) {
        return;
    }

    let Some(db) = writable_database() else {
        log::warn!("无法打开数据库，删除程序记录失败");
        return;
    };

    match db.delete_program_records(&stat.process_path) {
        Ok(count) => {
            log::info!("已删除程序 {} 的 {} 条记录", stat.process_path, count);
            // 该程序可能已经从统计里消失，顺手清掉它的展开状态。
            collapse_all(&stat.process_path);
            reload_after_delete();
        }
        Err(error) => log::error!("删除程序记录失败：{:#}", error),
    }
}

/// 右键某个计数单元格：删除该程序这一类型的全部记录。
unsafe fn delete_action_command(window: HWND) {
    let Some(StatsRow::Program(stat)) = context_row() else {
        return;
    };

    let sub_item = CONTEXT_SUBITEM.load(Ordering::SeqCst);
    if !(1..=5).contains(&sub_item) {
        return;
    }
    let action = ACTION_COLUMNS[(sub_item - 1) as usize];
    let count = stat.count_of(action);

    let message = format!(
        "确定要删除「{}」的全部{}记录吗？（共 {} 条）\n\n程序路径：{}\n\n只删除这一类型，其它类型不受影响，删除后无法恢复。",
        stat.process_name,
        action_label(action),
        count,
        stat.process_path
    );
    if !confirm(window, &message) {
        return;
    }

    let Some(db) = writable_database() else {
        log::warn!("无法打开数据库，删除类型记录失败");
        return;
    };

    match db.delete_program_action_records(&stat.process_path, action) {
        Ok(deleted) => {
            log::info!(
                "已删除 {} 的 {} 记录 {} 条",
                stat.process_path,
                action,
                deleted
            );
            reload_after_delete();
        }
        Err(error) => log::error!("删除类型记录失败：{:#}", error),
    }
}

/// 右键「删除这条记录」。
unsafe fn delete_record_command(window: HWND) {
    let Some(StatsRow::Detail { action, record }) = context_row() else {
        return;
    };

    let message = format!(
        "确定要删除这条记录吗？\n\n类型：{}\n路径：{}\n\n删除后无法恢复。",
        action_label(&action),
        record.file_path
    );
    if !confirm(window, &message) {
        return;
    }

    let Some(db) = writable_database() else {
        log::warn!("无法打开数据库，删除记录失败");
        return;
    };

    match db.delete_record(&record.file_path, &action) {
        Ok(count) => {
            log::info!("已删除 {} 条记录：{} {}", count, record.file_path, action);
            reload_after_delete();
        }
        Err(error) => log::error!("删除记录失败：{:#}", error),
    }
}

/// 打开可写数据库连接（删除记录用）。
///
/// 统计窗口平时用只读连接查询；删除属于低频操作，
/// 这里临时打开一个可写连接，用完即释放，不长期占用写锁。
fn writable_database() -> Option<Database> {
    let data_dir = crate::config::Config::default_data_dir().ok()?;
    Database::init(&data_dir).ok()
}

/// 删除后强制重新查库并重建列表。
///
/// 先把缓存清空，`refresh_stats` 里「数据没变就不重绘」的判断就必然失败，
/// 从而一定会重新读取统计。
fn reload_after_delete() {
    match ALL_STATS.lock() {
        Ok(mut cache) => cache.clear(),
        Err(poisoned) => poisoned.into_inner().clear(),
    }
    refresh_stats(true);
}

/// 双击明细行：打开文件所在目录（文件夹则直接打开）。
fn open_detail_target(record: &FileOrigin) {
    if std::path::Path::new(&record.file_path).is_dir() {
        crate::util::open_directory(&record.file_path);
    } else {
        crate::util::reveal_in_explorer(&record.file_path);
    }
}

/// 点击列头：切换排序列 / 方向。
fn toggle_sort(column: i32) {
    if column < 0 || column > TOTAL_COLUMN {
        return;
    }
    let column = column as isize;
    if SORT_COLUMN.load(Ordering::SeqCst) == column {
        let next = !SORT_ASCENDING.load(Ordering::SeqCst);
        SORT_ASCENDING.store(next, Ordering::SeqCst);
    } else {
        SORT_COLUMN.store(column, Ordering::SeqCst);
        // 程序名列默认 A→Z；计数列默认从多到少，更符合看统计的习惯。
        SORT_ASCENDING.store(column == PROGRAM_COLUMN as isize, Ordering::SeqCst);
    }
    update_sort_headers();
}

/// 按当前排序状态给程序汇总排序（明细行会跟随所属程序一起移动）。
fn sort_stats(stats: &mut [ProgramStat]) {
    let column = SORT_COLUMN.load(Ordering::SeqCst);
    let ascending = SORT_ASCENDING.load(Ordering::SeqCst);

    stats.sort_by(|a, b| {
        let ordering = match column {
            0 => program_text(a)
                .to_lowercase()
                .cmp(&program_text(b).to_lowercase()),
            1 => a.create_file.cmp(&b.create_file),
            2 => a.create_dir.cmp(&b.create_dir),
            3 => a.modify.cmp(&b.modify),
            4 => a.delete_file.cmp(&b.delete_file),
            5 => a.delete_dir.cmp(&b.delete_dir),
            _ => a.total().cmp(&b.total()),
        };
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
}

/// 在列头文字后面加上 ↑ / ↓，标出当前排序列和方向。
fn update_sort_headers() {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    let column = SORT_COLUMN.load(Ordering::SeqCst);
    let ascending = SORT_ASCENDING.load(Ordering::SeqCst);

    for (index, (title, _)) in COLUMNS.iter().enumerate() {
        let text = if index as isize == column {
            format!("{} {}", title, if ascending { "↑" } else { "↓" })
        } else {
            (*title).to_string()
        };
        let text = wide(&text);

        let mut info = LVCOLUMNW::default();
        info.mask = LVCF_TEXT;
        info.pszText = PWSTR(text.as_ptr() as *mut u16);
        unsafe {
            SendMessageW(
                list,
                LVM_SETCOLUMNW,
                WPARAM(index),
                LPARAM(&info as *const _ as isize),
            );
        }
    }
}

/// 用鼠标位置做命中测试，返回（行下标，列下标）。
unsafe fn hit_test_item(list: HWND) -> Option<(usize, i32)> {
    if list.0.is_null() {
        return None;
    }

    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_err() {
        return None;
    }
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
        return None;
    }

    // 优先使用控件返回的列号；个别系统上它是 -1，这时按光标 x 坐标自己算。
    let sub_item = if info.iSubItem >= 0 {
        info.iSubItem
    } else {
        column_at_x(point.x)
    };
    Some((index as usize, sub_item))
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
    COLUMNS.len() as i32 - 1
}

/// 把行数据填入列表，并同步行缓存。
///
/// `keep_scroll` 为 true 时记住当前可见的首行和选中行，重建后再恢复，
/// 避免展开 / 收起或后台刷新把用户正在看的位置拽回顶部。
fn fill_list(rows: &[StatsRow], keep_scroll: bool) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    match ROW_CACHE.lock() {
        Ok(mut cache) => *cache = rows.to_vec(),
        Err(poisoned) => *poisoned.into_inner() = rows.to_vec(),
    }

    unsafe {
        let top_index = if keep_scroll {
            SendMessageW(list, LVM_GETTOPINDEX, WPARAM(0), LPARAM(0)).0 as i32
        } else {
            0
        };

        let selected = if keep_scroll {
            SendMessageW(
                list,
                LVM_GETNEXTITEM,
                WPARAM(usize::MAX),
                LPARAM(LVNI_SELECTED as isize),
            )
            .0 as i32
        } else {
            -1
        };

        // 暂停重绘：一次性删光再插入，能避免整片闪白。
        SendMessageW(list, WM_SETREDRAW, WPARAM(0), LPARAM(0));
        SendMessageW(list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));

        for (index, row) in rows.iter().enumerate() {
            let first_column = match row {
                StatsRow::Program(stat) => program_text(stat),
                StatsRow::Detail { action, record } => detail_text(action, record),
            };

            let text = wide(&first_column);
            let mut item = LVITEMW::default();
            item.mask = LVIF_TEXT;
            item.iItem = index as i32;
            item.iSubItem = 0;
            item.pszText = PWSTR(text.as_ptr() as *mut u16);

            // LVM_INSERTITEMW 返回实际行号，后续列必须按它来填。
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

            // 程序行才有计数；明细行的计数列留空，避免和汇总数字混淆。
            if let StatsRow::Program(stat) = row {
                for (offset, action) in ACTION_COLUMNS.iter().enumerate() {
                    let text = wide(&stat.count_of(action).to_string());
                    insert_sub_item(list, row_index, offset as i32 + 1, &text);
                }
                let text = wide(&stat.total().to_string());
                insert_sub_item(list, row_index, TOTAL_COLUMN, &text);
            }
        }

        SendMessageW(list, WM_SETREDRAW, WPARAM(1), LPARAM(0));
        let _ = InvalidateRect(list, None, true);

        // 恢复滚动位置：ListView 重新插入后会回到顶部，
        // 这里用 WM_VSCROLL 逐行滚回去（LVM_SETTOPINDEX 实测不生效）。
        if keep_scroll && top_index > 0 && !rows.is_empty() {
            let target = top_index.min(rows.len() as i32 - 1);
            SendMessageW(list, WM_SETREDRAW, WPARAM(0), LPARAM(0));
            for _ in 0..target {
                SendMessageW(list, WM_VSCROLL, WPARAM(SB_LINEDOWN), LPARAM(0));
            }
            SendMessageW(list, WM_SETREDRAW, WPARAM(1), LPARAM(0));
            let _ = InvalidateRect(list, None, true);
        }

        if keep_scroll && selected >= 0 && (selected as usize) < rows.len() {
            select_row(list, selected as usize);
        }
    }
}

/// 选中某一行。
unsafe fn select_row(list: HWND, index: usize) {
    let mut item = LVITEMW::default();
    item.mask = LVIF_STATE;
    item.stateMask = windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(
        windows::Win32::UI::Controls::LVIS_SELECTED.0
            | windows::Win32::UI::Controls::LVIS_FOCUSED.0,
    );
    item.state = windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(
        windows::Win32::UI::Controls::LVIS_SELECTED.0
            | windows::Win32::UI::Controls::LVIS_FOCUSED.0,
    );
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM(index),
        LPARAM(&item as *const _ as isize),
    );
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

/// 程序行的第一列文字：程序名，有备注时接上「（备注）」。
fn program_text(stat: &ProgramStat) -> String {
    /// 列表里最多显示多少个备注字符。
    const MAX_NOTE_CHARS: usize = 24;

    let note = stat.note.trim();
    if note.is_empty() {
        return stat.process_name.clone();
    }

    if note.chars().count() > MAX_NOTE_CHARS {
        let short: String = note.chars().take(MAX_NOTE_CHARS).collect();
        format!("{}（{}…）", stat.process_name, short)
    } else {
        format!("{}（{}）", stat.process_name, note)
    }
}

/// 明细行的第一列文字，形如：
/// `    └ 创建文件夹  2026-11-17 22:31:05  D:\下载\资料`
///
/// 缩进和「└」让用户一眼看出这是某程序展开出来的子项。
fn detail_text(action: &str, record: &FileOrigin) -> String {
    format!(
        "    └ {}  {}  {}",
        action_label(action),
        crate::util::timestamp_to_local_text(record.created_at),
        record.file_path
    )
}

/// 动作名转成界面上的中文。
fn action_label(action: &str) -> &'static str {
    match action {
        "create_dir" => "创建文件夹",
        "modify" => "修改文件",
        "delete_file" => "删除文件",
        "delete_dir" => "删除文件夹",
        _ => "创建文件",
    }
}
