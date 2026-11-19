//! “最近记录”窗口。
//!
//! 用原生 Win32 ListView 展示数据库中最新的记录
//! （日期 / 时间 / 程序 / 路径 / 类型）。
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
//! - 右键点击任意一行：打开文件路径 / 复制词条 / 打开创建程序所在目录 /
//!   设置程序备注 / 加入名单（与「程序统计」窗口的右键菜单保持一致）

use anyhow::Result;
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
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
    LVS_EX_GRIDLINES, LVS_EX_TWOCLICKACTIVATE, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL,
    NMHDR, NMLISTVIEW, NMLVCUSTOMDRAW, NM_CUSTOMDRAW, NM_RCLICK,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetClientRect,
    GetCursorPos, GetMessageW, GetWindowTextW, InsertMenuW, KillTimer, LoadCursorW, MessageBoxW,
    MoveWindow, PostMessageW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetTimer,
    SetWindowTextW, ShowWindow, TrackPopupMenu, TranslateMessage, BM_GETCHECK, BM_SETCHECK,
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CW_USEDEFAULT, ES_AUTOHSCROLL, HMENU, ICON_BIG, ICON_SMALL,
    IDC_ARROW, MB_ICONINFORMATION, MB_OK, MF_BYPOSITION, MF_STRING, MSG, SW_HIDE, SW_SHOWNORMAL,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON, TPM_TOPALIGN, WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_NOTIFY,
    WM_NULL, WM_SETFONT, WM_SETICON, WM_SETREDRAW, WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_BORDER,
    WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
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
/// 当前排序列：0=日期 1=时间 2=程序 3=路径 4=类型。
static SORT_COLUMN: AtomicIsize = AtomicIsize::new(0);
/// 是否升序。默认 false：时间倒序，最新的记录在最上面。
static SORT_ASCENDING: AtomicBool = AtomicBool::new(false);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 右键菜单当前针对的行号（-1 表示没有）。
///
/// 菜单弹出后窗口过程会收到 WM_COMMAND，此时鼠标早已离开，
/// 只能靠这个变量记住「用户是右键点了哪一行」。
static CONTEXT_ROW: AtomicIsize = AtomicIsize::new(-1);

/// 列表的列定义：（标题，宽度）。顺序必须和填充数据时使用的列下标一致。
///
/// 双击 / 右键时也用它按光标 x 坐标反推用户点的是哪一列，比依赖
/// `LVM_SUBITEMHITTEST` 返回的 iSubItem 更稳妥。
const COLUMNS: [(&str, i32); 5] = [
    ("日期", 100),
    ("时间", 75),
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
/// 顶部工具栏最右侧的「导出 Excel」按钮。
const EXPORT_BUTTON_ID: usize = 9;
/// 底部翻页栏：「上一页 / 下一页」按钮。
const PAGE_PREV_BUTTON_ID: usize = 10;
const PAGE_NEXT_BUTTON_ID: usize = 11;

/// 右键菜单命令 ID。
///
/// 这些 ID 只在本窗口的消息循环里使用，和「程序统计」窗口的 5001~5015、
/// 「关于」窗口的 4001~4002 各自独立，不会互相干扰
/// （它们分属不同的顶层窗口，消息不会串）。
const MENU_OPEN_PATH: u32 = 4001;
const MENU_COPY_ENTRY: u32 = 4002;
const MENU_OPEN_PROGRAM_DIR: u32 = 4003;
const MENU_SET_NOTE: u32 = 4004;
const MENU_ADD_TO_LIST: u32 = 4005;

/// 日期时间框的值变化通知码（winuser.h 里的 DTN_DATETIMECHANGE = 0U-759U）。
const DTN_DATETIMECHANGE_CODE: u32 = 0xFFFF_FD09;
/// DTM_GETSYSTEMTIME 的返回码：0 表示拿到了有效时间。
const GDT_VALID: isize = 0;

/// 各列的列下标（0 起），与 `COLUMNS` 的顺序一一对应。
const DATE_COLUMN: i32 = 0;
const TIME_COLUMN: i32 = 1;
const PROGRAM_COLUMN: i32 = 2;
const PATH_COLUMN: i32 = 3;
/// 「类型」列的列下标。
const TYPE_COLUMN: i32 = 4;

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
/// 底部翻页栏占用的高度（列表要给这里留出空间）。
const PAGE_BAR_HEIGHT: i32 = 34;
/// 翻页栏里按钮的控件高度。
const PAGE_BAR_CONTROL_HEIGHT: i32 = 24;

/// 当前列表里显示的行数据。
///
/// ListView 只保存文本，双击时需要按行号找回完整记录（程序路径、文件路径），
/// 所以刷新列表时同步缓存一份。
static ROW_CACHE: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 从数据库读到的**当前页**记录（已在 SQL 层按关键字 / 时间筛选并排序）。
static ALL_ROWS: Lazy<Mutex<Vec<FileOrigin>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 记录窗口使用的只读数据库连接（惰性打开，打开失败时列表为空）。
static HISTORY_DB: Lazy<Mutex<Option<Database>>> = Lazy::new(|| Mutex::new(None));

/// 每页显示多少条记录。
///
/// 翻页在数据库里用 `LIMIT / OFFSET` 完成，内存中始终只有一页数据，
/// 所以再多历史记录也不会拖慢窗口。
const PAGE_SIZE: usize = 200;

/// 当前页码（0 起）。
static PAGE: AtomicUsize = AtomicUsize::new(0);

/// 当前筛选条件下的总条数（用于显示「共 N 条」和计算总页数）。
static TOTAL_ROWS: AtomicIsize = AtomicIsize::new(0);

/// 底部翻页栏的控件句柄。
static PAGE_LABEL: AtomicIsize = AtomicIsize::new(0);
static PAGE_PREV_BUTTON: AtomicIsize = AtomicIsize::new(0);
static PAGE_NEXT_BUTTON: AtomicIsize = AtomicIsize::new(0);

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
            1100,
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
                // 「导出 Excel」按钮：把当前筛选后的列表导出成 .xlsx。
                _ if id == EXPORT_BUTTON_ID as u32 => {
                    export_to_excel(hwnd);
                }
                // 搜索框内容变化：立刻按关键字重新过滤列表。
                _ if id == SEARCH_CONTROL_ID as u32 && code == EN_CHANGE_CODE => {
                    reload_list();
                }
                // 翻页：上一页 / 下一页。
                _ if id == PAGE_PREV_BUTTON_ID as u32 => {
                    let page = PAGE.load(Ordering::SeqCst);
                    if page > 0 {
                        PAGE.store(page - 1, Ordering::SeqCst);
                        reload_current_page();
                    }
                }
                _ if id == PAGE_NEXT_BUTTON_ID as u32 => {
                    PAGE.store(PAGE.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                    reload_current_page();
                }
                // 清除时间筛选：两个复选框取消勾选，重新读取全部记录。
                _ if id == FILTER_CLEAR_BUTTON_ID as u32 => {
                    clear_time_filter();
                }
                // 「起 / 止」复选框被点击：按新的勾选状态立即刷新列表。
                _ if id == FILTER_START_ENABLE_ID as u32 || id == FILTER_END_ENABLE_ID as u32 => {
                    reload_list();
                }
                // ---- 右键菜单命令 ----
                // 打开当前记录指向的文件 / 文件夹所在位置。
                _ if id == MENU_OPEN_PATH => {
                    if let Some(row) = context_row() {
                        open_path_target(&row);
                    }
                }
                // 把当前记录的文件路径复制到剪贴板。
                _ if id == MENU_COPY_ENTRY => {
                    if let Some(row) = context_row() {
                        copy_to_clipboard(&row.file_path);
                    }
                }
                // 打开「创建程序」所在目录并选中程序文件。
                _ if id == MENU_OPEN_PROGRAM_DIR => {
                    if let Some(row) = context_row() {
                        crate::util::reveal_in_explorer(&row.process_path);
                    }
                }
                // 编辑该「创建程序」的备注。备注按程序路径保存到数据库，
                // 因此「程序统计」窗口看到的也是同一个备注（两边天然同步）。
                _ if id == MENU_SET_NOTE => {
                    if let Some(row) = context_row() {
                        // 保存成功时 `edit_program_note` 内部已经刷新本窗口，
                        // 并会通知统计窗口一起刷新，这里不用再做额外处理。
                        crate::history::edit_program_note(
                            hwnd,
                            &row.process_name,
                            &row.process_path,
                            &row.note,
                        );
                    }
                }
                // 把该「创建程序」加入黑 / 白名单。
                _ if id == MENU_ADD_TO_LIST => {
                    if let Some(row) = context_row() {
                        add_program_to_list(hwnd, &row.process_path);
                    }
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
                        // toggle_sort 内部已经会重新查询列表，这里不用再刷新。
                        let info = &*(lparam.0 as *const NMLISTVIEW);
                        toggle_sort(info.iSubItem);
                    }
                    NM_RCLICK => {
                        // 右键：先选中该行（让用户看清菜单针对哪条记录），再弹出菜单。
                        if let Some((index, _)) = hit_test_item(list) {
                            select_row(list, index);
                            show_row_menu(hwnd, index);
                        }
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

    // 「导出 Excel」：把当前列表里的记录（受搜索框和时间筛选影响）导出成 .xlsx。
    // 位置放在工具栏最右侧，需要窗口宽度足够，所以默认窗口宽度设为 1100。
    let export_text = wide("导出 Excel");
    if let Ok(export_button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(export_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        970,
        8,
        90,
        24,
        window,
        HMENU(EXPORT_BUTTON_ID as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(export_button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
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
    // LVS_EX_TWOCLICKACTIVATE 必须加上：只有带这个样式时，双击列表项
    // 才会发出 LVN_ITEMACTIVATE 通知（网关里双击打开目录就是靠它）。
    // 之前漏了这一项，导致「双击创建程序列打开目录」这一功能实际不生效。
    let extended =
        LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER | LVS_EX_TWOCLICKACTIVATE;
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

    // 底部翻页栏：「上一页」「下一页」+ 页码说明文字。
    // 位置在 resize_controls 里按窗口高度重新计算。
    let prev_text = wide("上一页");
    if let Ok(button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(prev_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        12,
        500,
        80,
        24,
        window,
        HMENU(PAGE_PREV_BUTTON_ID as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        PAGE_PREV_BUTTON.store(button.0 as isize, Ordering::SeqCst);
    }

    let next_text = wide("下一页");
    if let Ok(button) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(button_class.as_ptr()),
        PCWSTR(next_text.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
        100,
        500,
        80,
        24,
        window,
        HMENU(PAGE_NEXT_BUTTON_ID as *mut c_void),
        instance,
        None,
    ) {
        SendMessageW(button, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        PAGE_NEXT_BUTTON.store(button.0 as isize, Ordering::SeqCst);
    }

    if let Ok(label) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(label_class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD | WS_VISIBLE,
        190,
        504,
        400,
        20,
        window,
        HMENU::default(),
        instance,
        None,
    ) {
        SendMessageW(label, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        PAGE_LABEL.store(label.0 as isize, Ordering::SeqCst);
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

    // 搜索框固定 160 宽，右侧依次是「程序统计」按钮和时间筛选控件；
    // 窗口变窄时只有列表宽度会被压缩，顶部控件位置保持不动。

    // 列表放在工具栏下方，底部再留出一条翻页栏。
    let list_width = (client_width - 16).max(100);
    let list_height = (client_height - TOOLBAR_HEIGHT - PAGE_BAR_HEIGHT).max(100);
    let _ = MoveWindow(list, 8, TOOLBAR_HEIGHT, list_width, list_height, true);

    // 翻页栏贴着窗口底部。
    let bar_top = client_height - PAGE_BAR_CONTROL_HEIGHT - 4;
    let prev = HWND(PAGE_PREV_BUTTON.load(Ordering::SeqCst) as *mut c_void);
    if !prev.0.is_null() {
        let _ = MoveWindow(prev, 12, bar_top, 80, PAGE_BAR_CONTROL_HEIGHT, true);
    }
    let next = HWND(PAGE_NEXT_BUTTON.load(Ordering::SeqCst) as *mut c_void);
    if !next.0.is_null() {
        let _ = MoveWindow(next, 100, bar_top, 80, PAGE_BAR_CONTROL_HEIGHT, true);
    }
    let label = HWND(PAGE_LABEL.load(Ordering::SeqCst) as *mut c_void);
    if !label.0.is_null() {
        let _ = MoveWindow(label, 192, bar_top + 3, 400, 20, true);
    }
}

/// 从数据库读取当前页的记录，同时返回符合条件的总条数。
///
/// 筛选（关键字 / 时间范围）、排序、分页全部在 SQL 里完成，
/// 所以无论历史数据有多少，内存里始终只有一页记录。
fn load_page() -> (Vec<FileOrigin>, i64) {
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

    // 当前页条件：搜索框文字 + 时间筛选 + 当前排序 + 页码偏移。
    let request = crate::db::PageRequest {
        keyword: search_text(),
        range: time_filter_range(),
        sort_column: SORT_COLUMN.load(Ordering::SeqCst) as i32,
        ascending: SORT_ASCENDING.load(Ordering::SeqCst),
        limit: PAGE_SIZE,
        offset: PAGE.load(Ordering::SeqCst) * PAGE_SIZE,
    };

    match guard.as_ref() {
        Some(db) => match db.query_page(&request) {
            Ok(result) => result,
            Err(error) => {
                log::error!("读取记录分页失败：{:#}", error);
                (Vec::new(), 0)
            }
        },
        None => (Vec::new(), 0),
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
        PROGRAM_COLUMN => crate::util::reveal_in_explorer(&row.process_path),
        PATH_COLUMN => open_path_target(&row),
        // 日期 / 时间 / 类型列不做操作。
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

/// 右键菜单当前针对的记录（右键点中的那一行）。
fn context_row() -> Option<FileOrigin> {
    let index = CONTEXT_ROW.load(Ordering::SeqCst);
    if index < 0 {
        return None;
    }
    cached_row(index as usize)
}

/// 弹出记录行的右键菜单。
///
/// 菜单共五项，与「程序统计」窗口保持一致：
/// 打开文件路径 / 复制词条 / 打开创建程序所在目录 / 设置程序备注 / 加入名单。
///
/// 之所以用 `InsertMenuW` 按位置逐个插入，而不是一次性构造菜单，
/// 是因为「加入名单」的文字要按当前名单模式动态生成（黑名单 / 白名单）。
unsafe fn show_row_menu(window: HWND, index: usize) {
    // 行号可能因为定时刷新而失效，先确认缓存里确实有这条记录。
    if cached_row(index).is_none() {
        return;
    }
    CONTEXT_ROW.store(index as isize, Ordering::SeqCst);

    let Ok(menu) = CreatePopupMenu() else {
        return;
    };

    // 前四项是固定文字。
    insert_menu_text(menu, 0, MENU_OPEN_PATH, "打开文件路径");
    insert_menu_text(menu, 1, MENU_COPY_ENTRY, "复制词条");
    insert_menu_text(menu, 2, MENU_OPEN_PROGRAM_DIR, "打开创建程序所在目录");
    insert_menu_text(menu, 3, MENU_SET_NOTE, "设置程序备注…");
    // 最后一项随名单模式变化。
    insert_menu_text(menu, 4, MENU_ADD_TO_LIST, menu_add_to_list_text());

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

/// 在菜单的指定位置插入一个普通字符串菜单项。
unsafe fn insert_menu_text(menu: HMENU, position: u32, command: u32, text: &str) {
    let text = wide(text);
    let _ = InsertMenuW(
        menu,
        position,
        MF_BYPOSITION | MF_STRING,
        command as usize,
        PCWSTR(text.as_ptr()),
    );
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
    let (rows, total) = load_page();

    // 总条数可能因为新记录而变化，翻页栏要跟着更新。
    TOTAL_ROWS.store(total as isize, Ordering::SeqCst);
    update_page_bar();

    // 和上次读到的数据完全相同就什么都不做，连重绘都不触发。
    let unchanged = ALL_ROWS.lock().map(|cache| *cache == rows).unwrap_or(false);
    if unchanged {
        return;
    }

    if let Ok(mut cache) = ALL_ROWS.lock() {
        *cache = rows.clone();
    }
    // 有新记录：重建列表，但保持用户当前的滚动位置。
    fill_list(&rows, true);
}

/// 用户主动改变筛选条件（搜索 / 排序 / 时间范围）后刷新列表。
///
/// 条件变了，页码必须回到第 1 页，否则可能停在一个空页上。
fn reload_list() {
    PAGE.store(0, Ordering::SeqCst);
    reload_current_page();
}

/// 按当前页码重新查询并填充列表（翻页按钮、筛选条件变化都走这里）。
fn reload_current_page() {
    let (mut rows, total) = load_page();

    // 数据被删到不足当前页时，自动退到最后一页，避免停在空白页。
    if rows.is_empty() && total > 0 && PAGE.load(Ordering::SeqCst) > 0 {
        let last_page = (total as usize - 1) / PAGE_SIZE;
        PAGE.store(last_page, Ordering::SeqCst);
        let (last_rows, last_total) = load_page();
        rows = last_rows;
        TOTAL_ROWS.store(last_total as isize, Ordering::SeqCst);
    } else {
        TOTAL_ROWS.store(total as isize, Ordering::SeqCst);
    }

    if let Ok(mut cache) = ALL_ROWS.lock() {
        *cache = rows.clone();
    }
    fill_list(&rows, false);
    update_page_bar();
}

/// 刷新底部翻页栏的文字和按钮状态。
fn update_page_bar() {
    let total = TOTAL_ROWS.load(Ordering::SeqCst);
    let page = PAGE.load(Ordering::SeqCst);
    let page_size = PAGE_SIZE as isize;
    // 没有数据时也显示「1 / 1」，避免出现「1 / 0」这种怪数字。
    let total_pages = if total <= 0 {
        1
    } else {
        ((total + page_size - 1) / page_size) as usize
    };

    let label = HWND(PAGE_LABEL.load(Ordering::SeqCst) as *mut c_void);
    if !label.0.is_null() {
        let text = if total <= 0 {
            "共 0 条".to_string()
        } else {
            format!("第 {} / {} 页 · 共 {} 条", page + 1, total_pages, total)
        };
        let text = wide(&text);
        unsafe {
            let _ = SetWindowTextW(label, PCWSTR(text.as_ptr()));
        }
    }

    let prev = HWND(PAGE_PREV_BUTTON.load(Ordering::SeqCst) as *mut c_void);
    if !prev.0.is_null() {
        unsafe {
            let _ = EnableWindow(prev, page > 0);
        }
    }

    let next = HWND(PAGE_NEXT_BUTTON.load(Ordering::SeqCst) as *mut c_void);
    if !next.0.is_null() {
        unsafe {
            let _ = EnableWindow(next, page + 1 < total_pages);
        }
    }
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
    // 排序在数据库查询里完成，改排序后要回到第 1 页重新查。
    reload_list();
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

/// 把 `"YYYY-MM-DD HH:MM:SS"` 拆成（日期, 时间）两部分。
///
/// 列表和导出都按这个格式显示时间。万一将来格式变了、字符串里没有空格，
/// 就把整串放进日期列、时间列留空，避免直接崩溃或丢数据。
fn split_date_time(text: &str) -> (&str, &str) {
    match text.split_once(' ') {
        Some((date, time)) => (date, time),
        None => (text, ""),
    }
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
            // 时间戳先格式化成 "YYYY-MM-DD HH:MM:SS"，再拆成日期和时间两列。
            let full_time = crate::util::timestamp_to_local_text(row.created_at);
            let (date_part, time_part) = split_date_time(&full_time);
            let date_text = wide(date_part);
            let mut item = LVITEMW::default();
            item.mask = LVIF_TEXT;
            item.iItem = index as i32;
            item.iSubItem = DATE_COLUMN;
            item.pszText = PWSTR(date_text.as_ptr() as *mut u16);

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

            // 时间列（第 1 列）。
            let time_text = wide(time_part);
            insert_sub_item(list, row_index, TIME_COLUMN, &time_text);

            let program_text = wide(&program_column_text(row));
            insert_sub_item(list, row_index, PROGRAM_COLUMN, &program_text);

            let path_text = wide(&row.file_path);
            insert_sub_item(list, row_index, PATH_COLUMN, &path_text);

            // 类型列：让用户直观看到这条记录属于哪类监控。
            let action_text = wide(&action_label(&row.action));
            insert_sub_item(list, row_index, TYPE_COLUMN, &action_text);
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
            // 备注是存在数据库里、按程序共享的，顺手通知「程序统计」窗口刷新，
            // 这样两个页面始终显示同一份备注（统计窗口没打开时该调用无副作用）。
            crate::stats::refresh_external();
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
/// 把数据库里的动作代码翻译成中文类型名。
///
/// `pub(crate)` 是为了让数据库层在按关键字搜索时也能复用同一套中文名。
pub(crate) fn action_label(action: &str) -> String {
    match action {
        "create_dir" => "新建文件夹".to_string(),
        "modify" => "修改文件".to_string(),
        "delete_file" => "删除文件".to_string(),
        "delete_dir" => "删除文件夹".to_string(),
        _ => "新建文件".to_string(),
    }
}

/// 导出 Excel 的完整流程。
///
/// 一共三步：
/// 1. 弹出条件窗口，让用户选择「导出全部」或「按条件筛选导出」；
/// 2. 按所选条件从数据库查询记录（不受列表 500 条上限限制）；
/// 3. 让用户选择保存位置并写出 `.xlsx`。
unsafe fn export_to_excel(window: HWND) {
    // 第一步：条件窗口。用户点「取消」时返回 None，直接结束。
    let Some(scope) = crate::export_dialog::show(window) else {
        return;
    };

    // 第二步：查库。「导出全部」等价于所有条件都留空。
    let rows: Vec<FileOrigin> = match scope {
        crate::export_dialog::ExportScope::All => {
            query_for_export(&crate::db::RecordFilter::default())
        }
        crate::export_dialog::ExportScope::Filter(filter) => query_for_export(&filter),
    };

    if rows.is_empty() {
        let text = wide("当前没有可导出的记录。");
        let title = wide("导出 Excel");
        let _ = MessageBoxW(
            window,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
        return;
    }

    // 让用户选择保存位置；用户点「取消」时返回 None，直接结束。
    let Some(path) = ask_save_path(window) else {
        return;
    };

    match write_xlsx(&path, &rows) {
        Ok(()) => {
            let text = wide(&format!(
                "已导出 {} 条记录到：\n{}",
                rows.len(),
                path.display()
            ));
            let title = wide("导出成功");
            let _ = MessageBoxW(
                window,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
            log::info!("已导出 {} 条记录到 {}", rows.len(), path.display());
        }
        Err(error) => {
            let text = wide(&format!("导出失败：\n{}", error));
            let title = wide("导出失败");
            let _ = MessageBoxW(
                window,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
            );
            log::error!("导出 Excel 失败：{}", error);
        }
    }
}

/// 按给定条件从数据库读取记录（不限条数），供导出使用。
///
/// 和 `load_rows` 的区别：这里不套用列表的 500 条上限，
/// 也不受列表当前排序影响，导出的就是数据库里符合条件的全部数据。
fn query_for_export(filter: &crate::db::RecordFilter) -> Vec<FileOrigin> {
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

    guard
        .as_ref()
        .and_then(|db| db.query_records(filter).ok())
        .unwrap_or_default()
}

/// 弹出系统的「另存为」对话框，返回用户选择的保存路径。
///
/// 默认文件名为 `file_origin_export_<Unix 时间戳>.xlsx`，
/// 这样连续导出多次也不会互相覆盖。
fn ask_save_path(window: HWND) -> Option<std::path::PathBuf> {
    use windows::Win32::UI::Controls::Dialogs::{
        GetSaveFileNameW, OFN_HIDEREADONLY, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let default_name = format!("file_origin_export_{stamp}.xlsx");

    // lpstrFile 是输入输出缓冲区：进入时是默认文件名，返回时是完整路径。
    let mut buffer = vec![0u16; 1024];
    for (index, unit) in default_name.encode_utf16().enumerate() {
        if index + 1 < buffer.len() {
            buffer[index] = unit;
        }
    }

    // 过滤器格式：显示名\0通配符\0显示名\0通配符\0\0（整体以两个 0 结束）。
    let filter: Vec<u16> = "Excel 工作簿 (*.xlsx)\0*.xlsx\0所有文件 (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide("导出为 Excel 表格");
    // 用户不写扩展名时，系统自动补 .xlsx。
    let default_ext: Vec<u16> = "xlsx\0".encode_utf16().collect();

    let mut dialog = OPENFILENAMEW::default();
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = window;
    dialog.lpstrFilter = PCWSTR(filter.as_ptr());
    dialog.lpstrFile = PWSTR(buffer.as_mut_ptr());
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrTitle = PCWSTR(title.as_ptr());
    dialog.lpstrDefExt = PCWSTR(default_ext.as_ptr());
    // 覆盖已有文件时提示确认；目录必须存在。
    dialog.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;

    if !unsafe { GetSaveFileNameW(&mut dialog) }.as_bool() {
        return None;
    }

    let end = buffer.iter().position(|&unit| unit == 0).unwrap_or(0);
    let path = String::from_utf16_lossy(&buffer[..end]);
    if path.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(path))
    }
}

/// 把记录写进一个真正的 `.xlsx` 工作簿。
///
/// 使用 `rust_xlsxwriter`（纯 Rust，无外部运行库依赖），
/// 生成的是标准 Excel 文件，可以直接用 Excel / WPS / LibreOffice 打开。
fn write_xlsx(path: &std::path::Path, rows: &[FileOrigin]) -> Result<(), String> {
    use rust_xlsxwriter::{Color, Format, FormatAlign, FormatBorder, Workbook};

    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet
        .set_name("文件来源记录")
        .map_err(|error| error.to_string())?;

    // 表头样式：加粗、浅蓝底色、细边框、居中。
    let header = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xE8_EE_F7))
        .set_border(FormatBorder::Thin)
        .set_align(FormatAlign::Center);

    // 列定义：（表头，列宽）。日期和时间拆成两列，方便在 Excel 里按日汇总。
    const HEADERS: [(&str, f64); 7] = [
        ("日期", 13.0),
        ("时间", 11.0),
        ("类型", 12.0),
        ("创建程序", 26.0),
        ("备注", 24.0),
        ("文件 / 文件夹路径", 70.0),
        ("创建程序路径", 55.0),
    ];

    for (column, (title, width)) in HEADERS.iter().enumerate() {
        let column = column as u16;
        sheet
            .set_column_width(column, *width)
            .map_err(|error| error.to_string())?;
        sheet
            .write_string_with_format(0, column, *title, &header)
            .map_err(|error| error.to_string())?;
    }

    // 数据行：从第 1 行（0 起）开始写，和列表的顺序完全一致。
    for (index, row) in rows.iter().enumerate() {
        let line = index as u32 + 1;
        // 与列表一致：先把时间戳拆成日期和时间两列。
        let full_time = crate::util::timestamp_to_local_text(row.created_at);
        let (date_part, time_part) = split_date_time(&full_time);

        sheet
            .write_string(line, 0, date_part)
            .map_err(|error| error.to_string())?;
        sheet
            .write_string(line, 1, time_part)
            .map_err(|error| error.to_string())?;
        sheet
            .write_string(line, 2, action_label(&row.action))
            .map_err(|error| error.to_string())?;
        sheet
            .write_string(line, 3, &row.process_name)
            .map_err(|error| error.to_string())?;
        sheet
            .write_string(line, 4, &row.note)
            .map_err(|error| error.to_string())?;
        sheet
            .write_string(line, 5, &row.file_path)
            .map_err(|error| error.to_string())?;
        sheet
            .write_string(line, 6, &row.process_path)
            .map_err(|error| error.to_string())?;
    }

    // 冻结首行，滚动时表头始终可见。
    sheet
        .set_freeze_panes(1, 0)
        .map_err(|error| error.to_string())?;
    // 给表头加上自动筛选下拉箭头，方便在 Excel 里二次筛选。
    sheet
        .autofilter(0, 0, rows.len() as u32, HEADERS.len() as u16 - 1)
        .map_err(|error| error.to_string())?;

    workbook.save(path).map_err(|error| error.to_string())
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
    use super::{program_column_text, FileOrigin};

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

    /// 验证“光标 x 坐标 -> 列下标”的换算与列宽严格一致。
    /// 这直接决定双击“创建程序 / 路径”列时能否打开对应目录。
    #[test]
    fn column_at_x_matches_column_widths() {
        // 列宽依次为：日期 100、时间 75、程序 280、路径 440、类型 100。
        assert_eq!(column_at_x(0), 0);
        assert_eq!(column_at_x(99), 0);
        assert_eq!(column_at_x(100), 1);
        assert_eq!(column_at_x(174), 1);
        assert_eq!(column_at_x(175), 2);
        assert_eq!(column_at_x(454), 2);
        assert_eq!(column_at_x(455), 3);
        assert_eq!(column_at_x(894), 3);
        assert_eq!(column_at_x(895), 4);
        assert_eq!(column_at_x(5000), 4);
    }

    /// 验证时间字符串能正确拆成日期和时间。
    #[test]
    fn split_date_time_splits_on_space() {
        assert_eq!(
            super::split_date_time("2026-11-19 08:30:05"),
            ("2026-11-19", "08:30:05")
        );
        // 没有空格时整串进日期列，时间列留空，不丢内容。
        assert_eq!(super::split_date_time("2026-11-19"), ("2026-11-19", ""));
    }

    /// 验证导出的确生成了一个合法的 .xlsx 文件。
    ///
    /// .xlsx 本质是 zip 压缩包，所以只要文件头是 `PK`、
    /// 并且压缩包里出现了标准文件名 `xl/workbook.xml`，就说明结构正确。
    #[test]
    fn export_writes_valid_xlsx() {
        let dir = std::env::temp_dir().join("file_origin_tracker_xlsx_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.xlsx");

        let mut row = test_row(r"C:\probe\a.txt", "notepad.exe", 1_700_000_000);
        row.note = "测试备注".to_string();
        row.action = "create_dir".to_string();

        super::write_xlsx(&path, &[row]).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], b"PK", "xlsx 应以 zip 头 PK 开头");
        assert!(
            bytes.windows(15).any(|window| window == b"xl/workbook.xml"),
            "压缩包里应包含 xl/workbook.xml"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
