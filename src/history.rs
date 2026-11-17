//! “最近记录”窗口。
//!
//! 用原生 Win32 ListView 展示数据库中最新的记录
//! （监控时间 / 程序 / 路径 / 类型）。
//! 使用原生控件而不是网页或 GPU 界面，空闲时几乎不占 CPU：
//! 只有窗口处于激活状态时，才每 10 秒自动刷新一次列表。
//!
//! 交互：
//! - 顶部同一行工具栏：搜索框（按程序名 / 路径 / 类型实时过滤）+「起 / 止」两个
//!   复选框与时间框（勾上才按该端时间过滤，精确到秒）
//! - 点击列头按该列排序，再点一次切换升序 / 降序
//! - 「类型」列按记录种类显示彩色底：新建文件=天蓝、新建文件夹=青绿、
//!   修改文件=浅紫、删除文件=红、删除文件夹=橙
//! - 双击「创建程序」列打开程序所在目录，双击「路径」列打开文件所在目录
//!
//! 注意：记录操作（打开路径 / 复制词条 / 打开程序目录 / 设置备注 / 加入名单）
//! 已统一放到「程序统计」窗口的右键菜单，这里不再提供右键菜单。

use anyhow::Result;
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM,
};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::Graphics::Gdi::{ScreenToClient, SetBkColor, SetTextColor, UpdateWindow};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, CDDS_ITEMPREPAINT, CDDS_PREPAINT,
    CDDS_SUBITEM, CDRF_DODEFAULT, CDRF_NEWFONT, CDRF_NOTIFYITEMDRAW, CDRF_NOTIFYSUBITEMDRAW,
    DTM_GETSYSTEMTIME, DTM_SETFORMATW, ICC_DATE_CLASSES, ICC_LISTVIEW_CLASSES,
    INITCOMMONCONTROLSEX, LIST_VIEW_ITEM_STATE_FLAGS, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW,
    LVHITTESTINFO, LVIF_STATE, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS,
    LVM_GETNEXTITEM, LVM_GETTOPINDEX, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETCOLUMNW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVM_SUBITEMHITTEST,
    LVNI_SELECTED, LVN_COLUMNCLICK, LVN_ITEMACTIVATE, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT,
    LVS_EX_GRIDLINES, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMLISTVIEW,
    NMLVCUSTOMDRAW, NM_CUSTOMDRAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW,
    GetWindowTextW, KillTimer, LoadCursorW, MessageBoxW, MoveWindow, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetTimer, ShowWindow, TranslateMessage, BM_GETCHECK, BM_SETCHECK,
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CW_USEDEFAULT, ES_AUTOHSCROLL, HMENU, ICON_BIG, ICON_SMALL,
    IDC_ARROW, MB_ICONINFORMATION, MB_OK, MSG, SW_HIDE, SW_SHOWNORMAL, WA_INACTIVE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_NOTIFY, WM_SETFONT, WM_SETICON, WM_SETREDRAW, WM_SIZE, WM_TIMER,
    WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use crate::db::{Database, FileOrigin};
use crate::util::wide;

const WINDOW_TITLE: &str = "文件来源追踪 - 最近记录";
const WINDOW_CLASS: &str = "FileOriginTrackerHistoryWindow";

/// 自动刷新间隔（毫秒）。只在窗口激活时启用。
const REFRESH_TIMER_ID: usize = 1;
const REFRESH_INTERVAL_MS: u32 = 10_000;

/// WM_VSCROLL：标准垂直滚动消息。
const WM_VSCROLL: u32 = 0x0115;

/// SB_LINEDOWN：WM_VSCROLL 的 wParam 取值，表示向下滚动一行。
const SB_LINEDOWN: usize = 1;

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
/// 时间筛选的两个日期时间框（DateTimePicker）。
static FILTER_START_PICKER: AtomicIsize = AtomicIsize::new(0);
static FILTER_END_PICKER: AtomicIsize = AtomicIsize::new(0);
/// 起、止两端各自是否启用（对应时间框左边的复选框）。
static FILTER_START_ENABLE: AtomicIsize = AtomicIsize::new(0);
static FILTER_END_ENABLE: AtomicIsize = AtomicIsize::new(0);
/// 当前排序列：0=监控时间 1=程序 2=路径 3=类型。
static SORT_COLUMN: AtomicIsize = AtomicIsize::new(0);
/// 是否升序。默认 false：时间倒序，最新的记录在最上面。
static SORT_ASCENDING: AtomicBool = AtomicBool::new(false);
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
/// 时间筛选控件 ID：起、止两个日期时间框和一个「清除筛选」按钮。
const FILTER_START_PICKER_ID: usize = 3;
const FILTER_END_PICKER_ID: usize = 4;
const FILTER_CLEAR_BUTTON_ID: usize = 5;
/// 起、止两端的启用复选框 ID。
///
/// 时间框自带的勾选框（DTS_SHOWNONE）又小又挤，看起来很不舒服，
/// 所以改用独立的复选框：勾上才按该端时间筛选，不勾表示这一端不限。
const FILTER_START_ENABLE_ID: usize = 6;
const FILTER_END_ENABLE_ID: usize = 7;
/// 顶部工具栏右侧的「程序统计」按钮。
const STATS_BUTTON_ID: usize = 8;

/// 日期时间框的值变化通知码（winuser.h 里的 DTN_DATETIMECHANGE = 0U-759U）。
const DTN_DATETIMECHANGE_CODE: u32 = 0xFFFF_FD09;
/// DTM_GETSYSTEMTIME 的返回码：0 表示拿到了有效时间。
const GDT_VALID: isize = 0;

/// 「类型」列的列下标（0 起）。
const TYPE_COLUMN: i32 = 3;

/// 「类型」列每种记录的单元格背景色。
///
/// COLORREF 的字节顺序是 0x00BBGGRR（蓝、绿、红），不是常见的 RGB，
/// 所以这里都按 BGR 写，注释里给出对应的 RGB 方便对照。
///
/// - 新建文件   ：天蓝   RGB(135, 206, 235)
/// - 新建文件夹 ：青绿   RGB(32, 178, 170)
/// - 修改文件   ：浅紫   RGB(224, 200, 240)
/// - 删除文件   ：红     RGB(219, 60, 71)
/// - 删除文件夹 ：橙     RGB(255, 165, 0)
fn type_cell_color(action: &str) -> COLORREF {
    match action {
        "create_dir" => COLORREF(0x00AA_B220),  // 青绿
        "modify" => COLORREF(0x00F0_C8E0),      // 浅紫
        "delete_file" => COLORREF(0x0047_3CDB), // 红
        "delete_dir" => COLORREF(0x0000_A5FF),  // 橙
        _ => COLORREF(0x00EB_CE87),             // 天蓝（新建文件）
    }
}

/// 单元格文字颜色。
///
/// 浅色底用深灰字；「删除文件」是深红底，必须换成白字才看得清。
fn type_cell_text_color(action: &str) -> COLORREF {
    match action {
        "delete_file" => COLORREF(0x00FF_FFFF), // 白
        _ => COLORREF(0x0020_2020),             // 深灰
    }
}

/// 编辑框内容变化时向父窗口发送的通知码（commctrl 之外的 Windows 标准通知）。
const EN_CHANGE_CODE: u32 = 0x0300;

/// 剪贴板文本格式（CF_UNICODETEXT）。
const CF_UNICODETEXT: u32 = 13;

/// 顶部工具栏高度（像素）：搜索框和时间筛选在同一行。
const TOOLBAR_HEIGHT: i32 = 36;

/// 当前列表里显示的行数据。
///
/// ListView 只保存文本，双击时需要按行号找回完整记录（程序路径、文件路径），
/// 所以刷新列表时同步缓存一份。
static ROW_CACHE: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 从数据库读到的全部记录（未过滤）。切换搜索关键字时从这份数据里筛，不用重新查库。
static ALL_ROWS: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

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
                // 「程序统计」按钮：打开按程序汇总的统计窗口。
                _ if id == STATS_BUTTON_ID as u32 => {
                    if let Err(error) = crate::stats::show() {
                        log::error!("打开程序统计窗口失败：{:#}", error);
                    }
                }
                // 搜索框内容变化：立刻按关键字重新过滤列表。
                _ if id == SEARCH_CONTROL_ID as u32 && code == EN_CHANGE_CODE => {
                    apply_filter(false);
                }
                // 清除时间筛选：两个复选框取消勾选，重新读取全部记录。
                _ if id == FILTER_CLEAR_BUTTON_ID as u32 => {
                    clear_time_filter();
                }
                // 「起 / 止」复选框被点击：按新的勾选状态立即刷新列表。
                _ if id == FILTER_START_ENABLE_ID as u32 || id == FILTER_END_ENABLE_ID as u32 => {
                    reload_list();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            let header = &*(lparam.0 as *const NMHDR);

            // 时间筛选：任一日期时间框被勾选 / 修改时间，立即重新查询并刷新列表。
            if (header.idFrom == FILTER_START_PICKER_ID || header.idFrom == FILTER_END_PICKER_ID)
                && header.code == DTN_DATETIMECHANGE_CODE
            {
                refresh_list();
                return LRESULT(0);
            }

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
                    LVN_COLUMNCLICK => {
                        // 点击列头：按该列排序；再点同一列则切换升序 / 降序。
                        let info = &*(lparam.0 as *const NMLISTVIEW);
                        toggle_sort(info.iSubItem);
                        apply_filter(false);
                    }
                    NM_CUSTOMDRAW => {
                        // 自定义绘制：只为「类型」列刷彩色背景，其它列保持系统默认绘制。
                        return handle_custom_draw(lparam);
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
        // 列表控件 + 日期时间选择框都属于通用控件，必须先初始化。
        dwICC: ICC_LISTVIEW_CLASSES | ICC_DATE_CLASSES,
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
    // 宽度 160：比原来缩短，给右边的「程序统计」按钮腾出位置。
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
        160,
        24,
        window,
        HMENU(SEARCH_CONTROL_ID as *mut std::ffi::c_void),
        instance,
        None,
    ) {
        SEARCH_EDIT.store(edit.0 as isize, Ordering::SeqCst);
        SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    // 搜索框右侧的「程序统计」入口：打开按程序汇总的统计窗口。
    // 和托盘菜单里的「程序统计」是同一个窗口，这里只是多一个更顺手的入口。
    let stats_class = wide("BUTTON");
    let stats_text = wide("程序统计");
    if let Ok(stats_button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(stats_class.as_ptr()),
        PCWSTR(stats_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        222,
        8,
        84,
        24,
        window,
        HMENU(STATS_BUTTON_ID as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(stats_button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    // ===== 同一行的右侧：时间筛选 =====
    // 每个时间框左边有一个独立复选框：
    // 勾上 = 按这一端的时间筛选，不勾 = 这一端不限时间，改动立即生效。
    let filter_label_text = wide("筛选时间");
    if let Ok(filter_label) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(label_class.as_ptr()),
        PCWSTR(filter_label_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        312,
        13,
        60,
        20,
        window,
        HMENU::default(),
        instance,
        None,
    ) {
        SendMessageW(filter_label, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    // 两个「起 / 止」复选框：普通的方形复选框，比日期框自带的勾选框清爽得多。
    let checkbox_class = wide("BUTTON");
    let start_enable_text = wide("起");
    let end_enable_text = wide("止");
    for (id, x, text) in [
        (FILTER_START_ENABLE_ID, 376, &start_enable_text),
        (FILTER_END_ENABLE_ID, 638, &end_enable_text),
    ] {
        if let Ok(checkbox) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(checkbox_class.as_ptr()),
            PCWSTR(text.as_ptr()),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
            x,
            11,
            38,
            20,
            window,
            HMENU(id as *mut c_void),
            instance,
            None,
        ) {
            if id == FILTER_START_ENABLE_ID {
                FILTER_START_ENABLE.store(checkbox.0 as isize, Ordering::SeqCst);
            } else {
                FILTER_END_ENABLE.store(checkbox.0 as isize, Ordering::SeqCst);
            }
            SendMessageW(checkbox, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        }
    }

    // 日期时间框统一格式：年月日时分秒，方便精确到某一秒。
    let picker_class = wide("SysDateTimePick32");
    let picker_format = wide("yyyy-MM-dd HH:mm:ss");
    // 不使用 DTS_SHOWNONE：时间框里不再内嵌小勾选框，是否启用由左边的复选框决定。
    let picker_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0);

    // 宽度 190：刚好容纳「2026-11-16 22:08:31」19 个字符 + 右侧下拉箭头，
    // 小于 190 时最后的秒数会被裁掉。
    for (id, x) in [(FILTER_START_PICKER_ID, 418), (FILTER_END_PICKER_ID, 680)] {
        if let Ok(picker) = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(picker_class.as_ptr()),
            PCWSTR::null(),
            picker_style,
            x,
            8,
            190,
            24,
            window,
            HMENU(id as *mut c_void),
            instance,
            None,
        ) {
            // 记住句柄，刷新 / 清除筛选时按句柄读写时间。
            if id == FILTER_START_PICKER_ID {
                FILTER_START_PICKER.store(picker.0 as isize, Ordering::SeqCst);
            } else {
                FILTER_END_PICKER.store(picker.0 as isize, Ordering::SeqCst);
            }
            SendMessageW(picker, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            SendMessageW(
                picker,
                DTM_SETFORMATW,
                WPARAM(0),
                LPARAM(picker_format.as_ptr() as isize),
            );
        }
    }

    // 「至」：连接起止两个时间框。
    let to_text = wide("至");
    if let Ok(to_label) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(label_class.as_ptr()),
        PCWSTR(to_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        614,
        13,
        20,
        20,
        window,
        HMENU::default(),
        instance,
        None,
    ) {
        SendMessageW(to_label, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    }

    // 「清除筛选」：去掉两个复选框的勾选，恢复显示全部记录。
    let clear_text = wide("清除筛选");
    let button_class = wide("BUTTON");
    if let Ok(clear_button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(clear_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        878,
        8,
        84,
        24,
        window,
        HMENU(FILTER_CLEAR_BUTTON_ID as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(clear_button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
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

    // 给列头加上当前排序方向的箭头（默认时间倒序：↓）。
    update_sort_headers();

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

    // 搜索框固定 160 宽，右侧依次是「程序统计」按钮和时间筛选控件；
    // 窗口变窄时只有列表宽度会被压缩，顶部控件位置保持不动。

    // 列表放在工具栏下方。
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

    let range = time_filter_range();
    match guard.as_ref() {
        // 有时间筛选时只查这个时间段内的记录，没有则查全部最近记录。
        Some(db) => db.recent_filtered(MAX_ROWS, range).unwrap_or_default(),
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
///
/// 该函数对「程序统计」窗口同样可用，所以是 crate 内公开的。
pub(crate) fn open_path_target(row: &FileOrigin) {
    if std::path::Path::new(&row.file_path).is_dir() {
        crate::util::open_directory(&row.file_path);
    } else {
        crate::util::reveal_in_explorer(&row.file_path);
    }
}

/// 后台定时刷新（每 10 秒一次）。
///
/// 关键点：只有数据真的变化时才重建列表。
/// 早先的做法是每个刷新周期都把列表删空再重新插入，用户往下翻历史记录时
/// 就会看到列表“闪一下”并被弹回顶部，完全没法浏览。
/// 绝大多数刷新周期里并没有新记录，直接返回即可。
fn refresh_list() {
    let rows = load_rows();

    // 和上次读到的数据完全相同就什么都不做，连重绘都不触发。
    let unchanged = ALL_ROWS.lock().map(|cache| *cache == rows).unwrap_or(false);
    if unchanged {
        return;
    }

    if let Ok(mut cache) = ALL_ROWS.lock() {
        *cache = rows;
    }
    // 有新记录：重建列表，但保持用户当前的滚动位置。
    apply_filter(true);
}

/// 用户主动改变筛选条件（搜索 / 排序 / 时间范围）后刷新列表。
///
/// 这时列表内容本来就会整体变化，恢复旧滚动位置没有意义，
/// 回到顶部更符合预期，所以不保持滚动位置。
fn reload_list() {
    let rows = load_rows();
    if let Ok(mut cache) = ALL_ROWS.lock() {
        *cache = rows;
    }
    apply_filter(false);
}

/// 按搜索框里的关键字过滤记录，并重新填充列表。
///
/// `keep_scroll` 为 true 时尽量保持用户当前的滚动位置和选中行（定时刷新用）；
/// 用户主动改了筛选条件时传 false，列表直接回到顶部。
fn apply_filter(keep_scroll: bool) {
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

    let mut filtered = filtered;
    sort_rows(&mut filtered);
    fill_list(&filtered, keep_scroll);
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

/// 读取一个复选框是否被勾选。
fn checkbox_checked(checkbox: &AtomicIsize) -> bool {
    let hwnd = HWND(checkbox.load(Ordering::SeqCst) as *mut c_void);
    if hwnd.0.is_null() {
        return false;
    }
    let state = unsafe { SendMessageW(hwnd, BM_GETCHECK, WPARAM(0), LPARAM(0)) }.0 as u32;
    state == BST_CHECKED.0
}

/// 读取两个时间框，得到「起 / 止」的 Unix 秒范围。
///
/// - 两端复选框都没勾选：返回 `None`（不筛选）
/// - 只勾选了一个：另一端用极小 / 极大值表示“不限”
/// - 起止顺序反了：自动交换，避免用户手滑后列表空白
fn time_filter_range() -> Option<(i64, i64)> {
    let start = if checkbox_checked(&FILTER_START_ENABLE) {
        picker_time(HWND(
            FILTER_START_PICKER.load(Ordering::SeqCst) as *mut c_void
        ))
    } else {
        None
    };
    let end = if checkbox_checked(&FILTER_END_ENABLE) {
        picker_time(HWND(FILTER_END_PICKER.load(Ordering::SeqCst) as *mut c_void))
    } else {
        None
    };

    match (start, end) {
        (None, None) => None,
        (Some(start), None) => Some((start, i64::MAX)),
        (None, Some(end)) => Some((i64::MIN, end)),
        (Some(start), Some(end)) => Some(if start <= end {
            (start, end)
        } else {
            (end, start)
        }),
    }
}

/// 读取一个日期时间框里的时间，转成 Unix 秒。
///
/// 返回 `None` 表示这个框没被勾选（DTS_SHOWNONE 的复选框是空的），也就是“不限时间”。
fn picker_time(picker: HWND) -> Option<i64> {
    if picker.0.is_null() {
        return None;
    }

    let mut system_time = SYSTEMTIME::default();
    let result = unsafe {
        SendMessageW(
            picker,
            DTM_GETSYSTEMTIME,
            WPARAM(0),
            LPARAM(&mut system_time as *mut _ as isize),
        )
    }
    .0 as isize;
    if result != GDT_VALID {
        return None;
    }

    // 日期时间框给的是本地时间，交给 util 换算成和数据库一致的 Unix 时间戳。
    crate::util::local_system_time_to_unix(&system_time)
}

/// 清除时间筛选：取消两端的勾选，再重新读取全部记录。
fn clear_time_filter() {
    for checkbox in [&FILTER_START_ENABLE, &FILTER_END_ENABLE] {
        let hwnd = HWND(checkbox.load(Ordering::SeqCst) as *mut c_void);
        if !hwnd.0.is_null() {
            unsafe {
                SendMessageW(
                    hwnd,
                    BM_SETCHECK,
                    WPARAM(BST_UNCHECKED.0 as usize),
                    LPARAM(0),
                );
            }
        }
    }
    reload_list();
}

/// 切换排序列 / 升降序。
///
/// 点一列：按该列排序（时间列默认最新在上，其它列默认升序）；
/// 再点同一列：在当前方向上反转。
fn toggle_sort(column: i32) {
    if column < 0 {
        return;
    }
    let column = column as isize;
    if SORT_COLUMN.load(Ordering::SeqCst) == column {
        let next = !SORT_ASCENDING.load(Ordering::SeqCst);
        SORT_ASCENDING.store(next, Ordering::SeqCst);
    } else {
        SORT_COLUMN.store(column, Ordering::SeqCst);
        // 时间列默认“最新在最上面”，文字列默认从 A 到 Z。
        SORT_ASCENDING.store(column != 0, Ordering::SeqCst);
    }
    update_sort_headers();
}

/// 按当前排序状态给记录排序（时间 / 程序 / 路径 / 类型）。
fn sort_rows(rows: &mut [FileOrigin]) {
    let column = SORT_COLUMN.load(Ordering::SeqCst);
    let ascending = SORT_ASCENDING.load(Ordering::SeqCst);

    rows.sort_by(|a, b| {
        let ordering = match column {
            1 => program_column_text(a)
                .to_lowercase()
                .cmp(&program_column_text(b).to_lowercase()),
            2 => a.file_path.to_lowercase().cmp(&b.file_path.to_lowercase()),
            3 => action_label(&a.action).cmp(&action_label(&b.action)),
            // 0 或其它：按监控时间。
            _ => a.created_at.cmp(&b.created_at),
        };
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
}

/// 在列头文字后面加上 ↑ / ↓，让用户一眼看出当前按哪列、哪个方向排序。
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

        // 只改文字，不动宽度（原有列宽保持不变）。
        let mut column_info = LVCOLUMNW::default();
        column_info.mask = LVCF_TEXT;
        column_info.pszText = PWSTR(text.as_ptr() as *mut u16);
        unsafe {
            SendMessageW(
                list,
                LVM_SETCOLUMNW,
                WPARAM(index),
                LPARAM(&column_info as *const _ as isize),
            );
        }
    }
}

/// 处理列表的 NM_CUSTOMDRAW 通知：给「类型」列的单元格刷彩色背景。
///
/// Windows 的 ListView 不支持直接设置某个单元格的背景色，只能通过
/// 自定义绘制（custom draw）通知插手绘制过程，分三步：
///
/// 1. `CDDS_PREPAINT`（整张列表开始绘制）：返回 `CDRF_NOTIFYITEMDRAW`，
///    告诉系统“每画一行都要再通知我一次”；
/// 2. `CDDS_ITEMPREPAINT`（开始画某一行）：返回 `CDRF_NOTIFYSUBITEMDRAW`，
///    要求“每个单元格也通知我”；
/// 3. `CDDS_ITEMPREPAINT | CDDS_SUBITEM`（开始画某个单元格）：这里才能拿到
///    具体的列号 `iSubItem`，把背景色和文字色改掉后返回 `CDRF_NEWFONT`，
///    系统就会用我们给的颜色继续绘制这一格。
///
/// 其它阶段一律返回 `CDRF_DODEFAULT`，交还给系统按默认方式绘制。
unsafe fn handle_custom_draw(lparam: LPARAM) -> LRESULT {
    let custom = &mut *(lparam.0 as *mut NMLVCUSTOMDRAW);
    let stage = custom.nmcd.dwDrawStage.0;

    // 第一步：整表开始绘制，要求逐行通知。
    if stage == CDDS_PREPAINT.0 {
        return LRESULT(CDRF_NOTIFYITEMDRAW as isize);
    }

    // 第二步：某一行开始绘制，要求逐单元格通知。
    if stage == CDDS_ITEMPREPAINT.0 {
        return LRESULT(CDRF_NOTIFYSUBITEMDRAW as isize);
    }

    // 第三步：某个单元格开始绘制（CDDS_ITEMPREPAINT 已经包含 CDDS_ITEM 位，
    // 与 CDDS_SUBITEM 相或后才是“单元格”阶段）。
    if stage == (CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0) && custom.iSubItem == TYPE_COLUMN {
        // dwItemSpec 就是行号；行号与 ROW_CACHE 一一对应（fill_list 按同样顺序缓存）。
        let action = ROW_CACHE
            .lock()
            .ok()
            .and_then(|cache| {
                cache
                    .get(custom.nmcd.dwItemSpec)
                    .map(|row| row.action.clone())
            })
            .unwrap_or_default();

        custom.clrTextBk = type_cell_color(&action);
        custom.clrText = type_cell_text_color(&action);
        return LRESULT(CDRF_NEWFONT as isize);
    }

    LRESULT(CDRF_DODEFAULT as isize)
}

/// 把给定记录填入列表，并同步行缓存（双击 / 右键要靠它按行号找回记录）。
///
/// `keep_scroll` 为 true 时，重建前记住第一条可见的行和选中的行，
/// 重建后原样恢复，避免后台刷新把用户从正在看的位置“拽”回顶部。
fn fill_list(rows: &[FileOrigin], keep_scroll: bool) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    if let Ok(mut cache) = ROW_CACHE.lock() {
        *cache = rows.to_vec();
    }

    unsafe {
        // 保存第一条可见行的下标：列表重建后会回到顶部，后面用它滚回去。
        let top_index = if keep_scroll {
            SendMessageW(list, LVM_GETTOPINDEX, WPARAM(0), LPARAM(0)).0 as i32
        } else {
            0
        };

        // 保存选中行；不需要保持时用 -1 作为哨兵，后面会跳过恢复。
        let selected = if keep_scroll {
            SendMessageW(
                list,
                LVM_GETNEXTITEM,
                // wParam = -1 表示从第一行开始往后找
                WPARAM(usize::MAX),
                LPARAM(LVNI_SELECTED as isize),
            )
            .0 as i32
        } else {
            -1
        };

        // 暂停重绘：删除 → 逐行插入的过程中会产生大量中间状态，
        // 不关掉重绘就会看到列表整片闪白。
        SendMessageW(list, WM_SETREDRAW, WPARAM(0), LPARAM(0));

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

        // 恢复重绘，并主动请求重画一次，避免列表停留在空白状态。
        SendMessageW(list, WM_SETREDRAW, WPARAM(1), LPARAM(0));
        let _ = InvalidateRect(list, None, true);

        // 恢复滚动位置：ListView 在“删除全部 + 重新插入”后会回到顶部，
        // 这里用 WM_VSCROLL 逐行滚回去。
        // 不用 LVM_SETTOPINDEX 是因为实测它对当前列表控件不生效（设置后读回仍是 0）。
        if keep_scroll && top_index > 0 && !rows.is_empty() {
            let target = top_index.min(rows.len() as i32 - 1);
            // 滚动过程临时关闭重绘，避免几百次重绘造成闪烁。
            SendMessageW(list, WM_SETREDRAW, WPARAM(0), LPARAM(0));
            for _ in 0..target {
                SendMessageW(list, WM_VSCROLL, WPARAM(SB_LINEDOWN), LPARAM(0));
            }
            SendMessageW(list, WM_SETREDRAW, WPARAM(1), LPARAM(0));
            let _ = InvalidateRect(list, None, true);
        }

        // 恢复选中行：索引必须仍然有效，否则会错误地选中另一条记录。
        if keep_scroll && selected >= 0 && (selected as usize) < rows.len() {
            select_row(list, selected as usize);
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

/// 右键菜单里「加入名单」这一项的显示文字。
///
/// 名称随设置里的模式变化，用户不用打开设置也能知道会加进哪个名单。
pub(crate) fn menu_add_to_list_text() -> &'static str {
    match crate::config::Config::load_or_default() {
        Ok(config) if config.is_white_list() => "加入白名单（此程序）",
        _ => "加入黑名单（此程序）",
    }
}

/// 把当前记录的「创建程序」加入黑/白名单。
///
/// 加入黑名单后，这个程序以后再创建/修改文件都不会被记录；
/// 加入白名单后，则只有这个程序的操作会被记录（白名单模式下）。
///
/// `owner` 是弹出提示框时使用的父窗口：从「最近记录」窗口调用时是记录窗口，
/// 从「程序统计」窗口调用时是统计窗口。
pub(crate) fn add_program_to_list(owner: HWND, process_path: &str) {
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
        .and_then(|db| db.add_list_entry(&config.list_mode, process_path, false));

    match result {
        Ok(()) => {
            log::info!(
                "[名单] 已把 {} 加入{}",
                process_path,
                config.list_mode_name()
            );
            let text = wide(&format!(
                "已把下面这个程序加入{}：\n\n{}\n\n名单可以在托盘菜单的「名单管理」里查看和移除。",
                config.list_mode_name(),
                process_path
            ));
            let title = wide("文件来源追踪");
            unsafe {
                let _ = MessageBoxW(
                    owner,
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
///
/// `owner` 是备注窗口的父窗口；`process_name / process_path / note` 来自
/// 当前右键选中的记录或程序汇总行，因此「最近记录」和「程序统计」两个窗口
/// 都能复用这套逻辑。
///
/// 只有真正保存成功时才返回 `true`（用户点取消、写库失败都返回 `false`），
/// 调用方可以据此决定是否需要刷新自己的列表。
pub(crate) fn edit_program_note(
    owner: HWND,
    process_name: &str,
    process_path: &str,
    note: &str,
) -> bool {
    // 弹出模态窗口；用户点“取消”时返回 None，直接结束。
    let Some(note) = crate::note::show(owner, process_name, process_path, note) else {
        return false;
    };

    // 记录窗口用的是只读连接，写备注必须另开一个可写连接。
    let result = crate::config::Config::default_data_dir()
        .map_err(|error| anyhow::anyhow!("{:#}", error))
        .and_then(|data_dir| Database::init(&data_dir))
        .and_then(|db| db.set_program_note(process_path, &note));

    match result {
        Ok(()) => {
            log::info!(
                "[记录] 程序备注已保存：{} -> {}",
                process_path,
                if note.is_empty() {
                    "（已清除）"
                } else {
                    note.as_str()
                }
            );
            // 立刻刷新记录列表：同一个程序的其它记录也要马上显示新备注。
            refresh_list();
            true
        }
        Err(error) => {
            log::error!("[记录] 保存程序备注失败：{:#}", error);
            false
        }
    }
}

/// 把文本复制到剪贴板（Unicode 文本）。
///
/// 「程序统计」窗口的右键菜单也会用到，所以是 crate 内公开的。
pub(crate) fn copy_to_clipboard(text: &str) {
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
        "delete_file" => "删除文件".to_string(),
        "delete_dir" => "删除文件夹".to_string(),
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
    use super::{program_column_text, sort_rows, FileOrigin, SORT_ASCENDING, SORT_COLUMN};
    use std::sync::atomic::Ordering;

    fn test_row(path: &str, program: &str, created_at: i64) -> FileOrigin {
        FileOrigin {
            file_path: path.to_string(),
            process_name: program.to_string(),
            process_path: program.to_string(),
            created_at,
            action: "create_file".to_string(),
            note: String::new(),
        }
    }

    /// 验证排序：按路径升序 / 降序，以及按时间升降序。
    ///
    /// 排序状态存在全局静态变量里，测试里显式设置并在结束时还原，
    /// 避免影响其它依赖默认排序的测试。
    #[test]
    fn sort_rows_orders_by_selected_column() {
        let mut rows = vec![
            test_row(r"C:\b\b.txt", "m.exe", 2),
            test_row(r"C:\a\a.txt", "Zeta.exe", 3),
            test_row(r"C:\c\c.txt", "alpha.exe", 1),
        ];

        // 路径升序：a < b < c
        SORT_COLUMN.store(2, Ordering::SeqCst);
        SORT_ASCENDING.store(true, Ordering::SeqCst);
        sort_rows(&mut rows);
        assert_eq!(rows[0].file_path, r"C:\a\a.txt");
        assert_eq!(rows[2].file_path, r"C:\c\c.txt");

        // 路径降序
        SORT_ASCENDING.store(false, Ordering::SeqCst);
        sort_rows(&mut rows);
        assert_eq!(rows[0].file_path, r"C:\c\c.txt");

        // 程序名升序（忽略大小写：alpha < m < Zeta）
        SORT_COLUMN.store(1, Ordering::SeqCst);
        SORT_ASCENDING.store(true, Ordering::SeqCst);
        sort_rows(&mut rows);
        assert_eq!(program_column_text(&rows[0]), "alpha.exe");
        assert_eq!(program_column_text(&rows[2]), "Zeta.exe");

        // 时间升序
        SORT_COLUMN.store(0, Ordering::SeqCst);
        SORT_ASCENDING.store(true, Ordering::SeqCst);
        sort_rows(&mut rows);
        assert_eq!(rows[0].created_at, 1);

        // 还原默认：时间倒序
        SORT_ASCENDING.store(false, Ordering::SeqCst);
        sort_rows(&mut rows);
        assert_eq!(rows[0].created_at, 3);
    }

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
