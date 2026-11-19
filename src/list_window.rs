//! 黑白名单管理窗口。
//!
//! 黑名单和白名单放在同一个列表里展示（「名单」列标明属于哪一个），
//! 不需要切换模式就能看到全部条目：
//! - 顶部说明文字显示当前生效的模式（在「设置」窗口里切换）
//! - 列表三列：名单 / 类型 / 路径
//! - 底部：先选「添加到：黑名单 / 白名单」，再点「添加程序…」或「添加文件夹…」
//! - 「移除选中」按该行自己所属的名单删除，与当前模式无关
//! - 选中一行后点「切换名单」（或直接双击该行）可以在黑名单 / 白名单之间互换
//!
//! 条目保存在数据库的 `list_entries` 表里，
//! ETW 写库线程每 2 秒重新加载一次规则，所以增删后很快生效。

use anyhow::Result;
use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, SetBkColor, SetTextColor};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCF_TEXT, LVCF_WIDTH,
    LVCOLUMNW, LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMTEXTW, LVNI_SELECTED,
    LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES, LVS_REPORT, LVS_SHOWSELALWAYS,
    LVS_SINGLESEL, NMHDR, NM_DBLCLK,
};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, LoadCursorW, MessageBoxW, MoveWindow,
    RegisterClassExW, SendMessageW, SetWindowTextW, ShowWindow, BM_GETCHECK, BM_SETCHECK,
    BS_AUTORADIOBUTTON, BS_OWNERDRAW, BS_PUSHLIKE, HMENU, ICON_BIG, ICON_SMALL, IDC_ARROW,
    MB_ICONWARNING, MB_OK, SW_HIDE, SW_SHOWNORMAL, WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DRAWITEM,
    WM_NOTIFY, WM_SETICON, WM_SIZE, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use crate::config::Config;
use crate::db::{Database, ListEntry};
use crate::list_rules::{MODE_BLACK, MODE_WHITE};
use crate::util::wide;

const WINDOW_CLASS: &str = "FileOriginTrackerListWindow";
const WINDOW_TITLE: &str = "文件来源追踪 - 黑白名单";

/// 控件 ID。
const HINT_STATIC_ID: usize = 0;
const LIST_CONTROL_ID: usize = 1;
const ADD_PROGRAM_BUTTON: usize = 3101;
const ADD_FOLDER_BUTTON: usize = 3102;
const REMOVE_BUTTON: usize = 3103;
const CLOSE_BUTTON: usize = 3104;
/// 「添加到」目标名单单选框。
const TARGET_BLACK_RADIO: usize = 3105;
const TARGET_WHITE_RADIO: usize = 3106;
/// 「添加到：」这个标签的控件 ID（缩放窗口时也要跟着移动）。
const TARGET_LABEL_ID: usize = 3107;
/// 「切换名单」按钮：把选中条目在黑名单 / 白名单之间切换。
const TOGGLE_BUTTON: usize = 3108;

/// 底部一排控件统一使用的尺寸，保证文字、按钮完全对齐。
const BOTTOM_BUTTON_HEIGHT: i32 = 32;
/// 底部控件距离窗口底边的留白。
const BOTTOM_MARGIN: i32 = 12;

static LIST_WINDOW: AtomicIsize = AtomicIsize::new(0);
static LIST_VIEW: AtomicIsize = AtomicIsize::new(0);
static HINT_STATIC: AtomicIsize = AtomicIsize::new(0);
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 列表当前展示的条目：(所属名单模式, 条目)。
/// 移除时按选中行号从这里取所属名单，与当前模式无关。
static CACHE: Lazy<Mutex<Vec<(String, ListEntry)>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// 列表列定义：（标题，宽度）。
const COLUMNS: [(&str, i32); 3] = [("名单", 100), ("类型", 80), ("程序 / 文件夹路径", 700)];

/// 打开（或重新激活）名单管理窗口。
pub fn show() -> Result<()> {
    let existing = LIST_WINDOW.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        }
        refresh(hwnd);
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
            140,
            120,
            960,
            600,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )?
    };

    LIST_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    }
    refresh(hwnd);
    Ok(())
}

/// 当前生效的模式（只在提示文字里显示，不再决定列表内容）。
fn current_mode() -> String {
    Config::load_or_default()
        .map(|config| config.list_mode)
        .unwrap_or_else(|_| MODE_BLACK.to_string())
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
    window_class.hbrBackground = crate::util::ui_brush();
    window_class.hCursor = unsafe { LoadCursorW(HINSTANCE::default(), IDC_ARROW)? };
    window_class.lpszClassName = PCWSTR(class_name.as_ptr());

    if unsafe { RegisterClassExW(&window_class) } == 0 {
        CLASS_REGISTERED.store(false, Ordering::SeqCst);
        return Err(anyhow::anyhow!("注册名单窗口类失败"));
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
        WM_SIZE => {
            resize_list(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => match wparam.0 & 0xFFFF {
            ADD_PROGRAM_BUTTON => {
                add_program(hwnd);
                LRESULT(0)
            }
            ADD_FOLDER_BUTTON => {
                add_folder(hwnd);
                LRESULT(0)
            }
            REMOVE_BUTTON => {
                remove_selected(hwnd);
                LRESULT(0)
            }
            TOGGLE_BUTTON => {
                toggle_selected(hwnd, true);
                LRESULT(0)
            }
            CLOSE_BUTTON => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            // 切换「添加到」的目标名单后，立刻更新两个按钮上的勾选标记。
            TARGET_BLACK_RADIO | TARGET_WHITE_RADIO => {
                update_target_text(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        },
        WM_NOTIFY => {
            let header = &*(lparam.0 as *const NMHDR);
            // 双击列表某一行 = 直接把它切换到另一个名单（快速操作，不弹提示）。
            if header.idFrom == LIST_CONTROL_ID && header.code == NM_DBLCLK {
                toggle_selected(hwnd, false);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            let device = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            SetBkColor(device, crate::theme::BG);
            SetTextColor(device, crate::theme::TEXT);
            LRESULT(crate::util::ui_brush().0 as isize)
        }
        WM_DRAWITEM => {
            // 底部操作按钮统一为平面几何风格。
            let info = &*(lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT);
            crate::theme::draw_button_auto(info, false);
            LRESULT(1)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE {
                refresh(hwnd);
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

/// 创建说明文字、列表、底部按钮。
unsafe fn create_controls(window: HWND, instance: HINSTANCE) {
    let font = crate::util::ui_font();

    let mut controls = INITCOMMONCONTROLSEX::default();
    controls.dwSize = std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32;
    controls.dwICC = ICC_LISTVIEW_CLASSES;
    let _ = InitCommonControlsEx(&controls);

    // 顶部说明文字：内容随当前模式变化，在 refresh() 里更新。
    let hint = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        PCWSTR(wide("STATIC").as_ptr()),
        PCWSTR(wide("").as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
        12,
        14,
        900,
        20,
        window,
        HMENU(HINT_STATIC_ID as *mut c_void),
        instance,
        None,
    )
    .unwrap_or_default();
    HINT_STATIC.store(hint.0 as isize, Ordering::SeqCst);

    // 列表：报表模式 + 整行选中 + 网格线 + 双缓冲。
    let list_class = wide("SysListView32");
    let list = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        PCWSTR(list_class.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE(
            WS_CHILD.0
                | WS_VISIBLE.0
                | WS_BORDER.0
                | LVS_REPORT
                | LVS_SINGLESEL
                | LVS_SHOWSELALWAYS,
        ),
        12,
        44,
        920,
        440,
        window,
        HMENU(LIST_CONTROL_ID as *mut c_void),
        instance,
        None,
    )
    .unwrap_or_default();
    LIST_VIEW.store(list.0 as isize, Ordering::SeqCst);

    let extended = LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES | LVS_EX_DOUBLEBUFFER;
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        WPARAM(extended as usize),
        LPARAM(extended as isize),
    );

    for (index, (title, width)) in COLUMNS.iter().enumerate() {
        let mut column = LVCOLUMNW::default();
        column.mask = LVCF_TEXT | LVCF_WIDTH;
        column.cx = *width;
        let text = wide(title);
        column.pszText = PWSTR(text.as_ptr() as *mut u16);
        SendMessageW(
            list,
            LVM_INSERTCOLUMNW,
            WPARAM(index),
            LPARAM(&column as *const _ as isize),
        );
    }

    // 列头统一改成几何风格（浅灰面板 + 细线 + 强调色排序三角）。
    crate::theme::style_list_header(list);

    // 底部一行：添加到哪个名单 + 三个操作按钮，最后是「关闭」。
    //
    // 对齐要点（之前看起来“乱”的原因）：
    // - 单选按钮原来只有 24px 高、比按钮低 4px，看起来高低不齐；
    // - 现在所有控件统一 32px 高、同一个 y 坐标，一眼看过去是一条直线。
    // 点击要点：
    // - 单选框改成「按钮样式」（BS_PUSHLIKE），整个按钮区域都能点，
    //   不用再精确点到那个小圆点上。
    // 标签用 SS_CENTERIMAGE 让文字在这 32px 里垂直居中，与按钮文字对齐。
    const SS_CENTERIMAGE: u32 = 0x0200;
    let label_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | SS_CENTERIMAGE);
    let radio_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32 | BS_PUSHLIKE as u32,
    );
    let button_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_OWNERDRAW as u32);

    let mut controls_to_style: Vec<HWND> = Vec::new();

    // 底部控件的初始 y 坐标（和 resize_list 里的算法保持一致）。
    let bottom_y = 600 - BOTTOM_MARGIN - BOTTOM_BUTTON_HEIGHT;

    // 「添加到：」标签
    controls_to_style.push(
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(wide("STATIC").as_ptr()),
            PCWSTR(wide("添加到：").as_ptr()),
            label_style,
            12,
            bottom_y,
            86,
            BOTTOM_BUTTON_HEIGHT,
            window,
            HMENU(TARGET_LABEL_ID as *mut c_void),
            instance,
            None,
        )
        .unwrap_or_default(),
    );

    // 目标名单：黑名单（WS_GROUP 开始一组单选）/ 白名单。
    // 用「按钮样式」的单选框，点击区域就是整个 104×32 的按钮，方便点。
    controls_to_style.push(
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(wide("BUTTON").as_ptr()),
            PCWSTR(wide("黑名单").as_ptr()),
            WINDOW_STYLE(radio_style.0 | 0x8), // 0x8 = WS_GROUP
            104,
            bottom_y,
            104,
            BOTTOM_BUTTON_HEIGHT,
            window,
            HMENU(TARGET_BLACK_RADIO as *mut c_void),
            instance,
            None,
        )
        .unwrap_or_default(),
    );
    controls_to_style.push(
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(wide("BUTTON").as_ptr()),
            PCWSTR(wide("白名单").as_ptr()),
            radio_style,
            214,
            bottom_y,
            104,
            BOTTOM_BUTTON_HEIGHT,
            window,
            HMENU(TARGET_WHITE_RADIO as *mut c_void),
            instance,
            None,
        )
        .unwrap_or_default(),
    );

    let buttons: [(usize, &str, i32); 4] = [
        (ADD_PROGRAM_BUTTON, "添加程序…", 330),
        (ADD_FOLDER_BUTTON, "添加文件夹…", 446),
        (REMOVE_BUTTON, "移除选中", 562),
        (TOGGLE_BUTTON, "切换名单", 678),
    ];
    for (id, text, x) in buttons {
        controls_to_style.push(
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(wide("BUTTON").as_ptr()),
                PCWSTR(wide(text).as_ptr()),
                button_style,
                x,
                bottom_y,
                110,
                BOTTOM_BUTTON_HEIGHT,
                window,
                HMENU(id as *mut c_void),
                instance,
                None,
            )
            .unwrap_or_default(),
        );
    }

    // 「关闭」按钮位置由 resize_list 计算（贴右下角）。
    controls_to_style.push(
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(wide("BUTTON").as_ptr()),
            PCWSTR(wide("关闭").as_ptr()),
            button_style,
            828,
            bottom_y,
            110,
            BOTTOM_BUTTON_HEIGHT,
            window,
            HMENU(CLOSE_BUTTON as *mut c_void),
            instance,
            None,
        )
        .unwrap_or_default(),
    );

    // 统一字体。
    for control in controls_to_style {
        if !control.0.is_null() {
            SendMessageW(
                control,
                windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
                WPARAM(font.0 as usize),
                LPARAM(1),
            );
        }
    }
    let hint_control = HWND(HINT_STATIC.load(Ordering::SeqCst) as *mut c_void);
    SendMessageW(
        hint_control,
        windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
        WPARAM(font.0 as usize),
        LPARAM(1),
    );
    SendMessageW(
        list,
        windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
        WPARAM(font.0 as usize),
        LPARAM(1),
    );

    // 「添加到」默认选中当前生效的模式，避免加错名单。
    let white = current_mode() == MODE_WHITE;
    let black_radio =
        windows::Win32::UI::WindowsAndMessaging::GetDlgItem(window, TARGET_BLACK_RADIO as i32)
            .unwrap_or_default();
    let white_radio =
        windows::Win32::UI::WindowsAndMessaging::GetDlgItem(window, TARGET_WHITE_RADIO as i32)
            .unwrap_or_default();
    SendMessageW(
        black_radio,
        BM_SETCHECK,
        WPARAM(if white { 0 } else { 1 }),
        LPARAM(0),
    );
    SendMessageW(
        white_radio,
        BM_SETCHECK,
        WPARAM(if white { 1 } else { 0 }),
        LPARAM(0),
    );
    update_target_text(window);

    // 单选按钮（BS_PUSHLIKE）也交给全局主题画：平面按钮 + 选中态强调色边框，
    // 不再使用系统默认的灰色凸起外观。
    crate::theme::style_children(window);
}

/// 刷新「添加到」两个按钮的显示状态。
///
/// 这两个单选框用的是按钮外观（BS_PUSHLIKE），选中状态现在由全局主题
/// 直接画出来（强调色边框 + 左侧小色块），所以文字保持干净的名字即可；
/// 这里负责在切换后让两个按钮都重画一次。
unsafe fn update_target_text(window: HWND) {
    for (id, name) in [
        (TARGET_BLACK_RADIO, "黑名单"),
        (TARGET_WHITE_RADIO, "白名单"),
    ] {
        let control = windows::Win32::UI::WindowsAndMessaging::GetDlgItem(window, id as i32)
            .unwrap_or_default();
        if control.0.is_null() {
            continue;
        }
        let text = wide(name);
        let _ = SetWindowTextW(control, PCWSTR(text.as_ptr()));
        let _ = InvalidateRect(control, None, true);
    }
}

/// 窗口大小变化时让列表跟随拉伸，底部按钮跟随下移。
unsafe fn resize_list(window: HWND) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }
    let mut client = windows::Win32::Foundation::RECT::default();
    if GetClientRect(window, &mut client).is_err() {
        return;
    }
    let width = (client.right - 24).max(300);
    // 底部留出「一排 32px 按钮 + 12px 下边距 + 16px 间距」的空间给列表。
    let reserved = BOTTOM_BUTTON_HEIGHT + BOTTOM_MARGIN + 16;
    let height = (client.bottom - 44 - reserved).max(120);
    let _ = MoveWindow(list, 12, 44, width, height, true);

    // 底部一排控件整体跟着窗口底部走，y 坐标和高度完全一致，保证对齐。
    let bottom = client.bottom - BOTTOM_MARGIN - BOTTOM_BUTTON_HEIGHT;
    let controls: [(usize, i32, i32); 8] = [
        (TARGET_LABEL_ID, 12, 86),
        (TARGET_BLACK_RADIO, 104, 104),
        (TARGET_WHITE_RADIO, 214, 104),
        (ADD_PROGRAM_BUTTON, 330, 110),
        (ADD_FOLDER_BUTTON, 446, 110),
        (REMOVE_BUTTON, 562, 110),
        (TOGGLE_BUTTON, 678, 110),
        (CLOSE_BUTTON, client.right - 122, 110),
    ];
    for (id, x, width) in controls {
        let control = windows::Win32::UI::WindowsAndMessaging::GetDlgItem(window, id as i32)
            .unwrap_or_default();
        if !control.0.is_null() {
            let _ = MoveWindow(control, x, bottom, width, BOTTOM_BUTTON_HEIGHT, true);
        }
    }
}

/// 刷新说明文字和列表内容。
fn refresh(_window: HWND) {
    // 说明文字里显示当前生效的模式，提醒用户规则以哪个名单为准。
    let mode = current_mode();
    let mode_name = if mode == MODE_WHITE {
        "白名单"
    } else {
        "黑名单"
    };
    let hint = format!(
        "当前生效：{}（在设置里切换）｜黑名单中的程序/文件夹不记录，白名单中的才记录｜双击条目可切换名单。",
        mode_name
    );
    let hint = wide(&hint);

    unsafe {
        let hint_control = HWND(HINT_STATIC.load(Ordering::SeqCst) as *mut c_void);
        if !hint_control.0.is_null() {
            let _ = SetWindowTextW(hint_control, PCWSTR(hint.as_ptr()));
        }
    }

    let Ok(data_dir) = Config::default_data_dir() else {
        return;
    };
    let Ok(db) = Database::init(&data_dir) else {
        log::warn!("[名单] 打开数据库失败，无法读取名单");
        return;
    };
    let entries = db.all_list_entries().unwrap_or_default();
    fill_list(&entries);
    if let Ok(mut cache) = CACHE.lock() {
        *cache = entries;
    }
}

/// 把条目填进列表（黑名单和白名单按「名单」列区分，按模式与路径排序）。
fn fill_list(entries: &[(String, ListEntry)]) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));

        for (index, (mode, entry)) in entries.iter().enumerate() {
            let mode_text = wide(if mode == MODE_WHITE {
                "白名单"
            } else {
                "黑名单"
            });
            let mut item = LVITEMW::default();
            item.mask = LVIF_TEXT;
            item.iItem = index as i32;
            item.pszText = PWSTR(mode_text.as_ptr() as *mut u16);
            let inserted = SendMessageW(
                list,
                LVM_INSERTITEMW,
                WPARAM(0),
                LPARAM(&item as *const _ as isize),
            );
            if inserted.0 < 0 {
                continue;
            }

            let kind = wide(if entry.is_folder {
                "文件夹"
            } else {
                "程序"
            });
            let mut kind_item = LVITEMW::default();
            kind_item.iSubItem = 1;
            kind_item.pszText = PWSTR(kind.as_ptr() as *mut u16);
            SendMessageW(
                list,
                LVM_SETITEMTEXTW,
                WPARAM(index),
                LPARAM(&kind_item as *const _ as isize),
            );

            let path = wide(&entry.path);
            let mut path_item = LVITEMW::default();
            path_item.iSubItem = 2;
            path_item.pszText = PWSTR(path.as_ptr() as *mut u16);
            SendMessageW(
                list,
                LVM_SETITEMTEXTW,
                WPARAM(index),
                LPARAM(&path_item as *const _ as isize),
            );
        }
    }
}

/// 读取「添加到」选中的目标名单。
fn target_mode(window: HWND) -> &'static str {
    let white = unsafe {
        let control =
            windows::Win32::UI::WindowsAndMessaging::GetDlgItem(window, TARGET_WHITE_RADIO as i32)
                .unwrap_or_default();
        if control.0.is_null() {
            false
        } else {
            SendMessageW(control, BM_GETCHECK, WPARAM(0), LPARAM(0)).0 == 1
        }
    };
    if white {
        MODE_WHITE
    } else {
        MODE_BLACK
    }
}

/// 弹出文件选择框，把选中的程序加入「添加到」指定的名单。
fn add_program(window: HWND) {
    let mut buffer = vec![0u16; 2048];
    // 双 null 结尾：第一段是显示名，第二段是通配符；整个过滤器以两个 0 结束。
    let filter: Vec<u16> = "程序 (*.exe)\0*.exe\0所有文件 (*.*)\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide("选择要加入名单的程序");

    let mut dialog = OPENFILENAMEW::default();
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = window;
    dialog.lpstrFilter = PCWSTR(filter.as_ptr());
    dialog.lpstrFile = PWSTR(buffer.as_mut_ptr());
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrTitle = PCWSTR(title.as_ptr());
    dialog.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;

    if !unsafe { GetOpenFileNameW(&mut dialog) }.as_bool() {
        return;
    }

    let path = read_wide(&buffer);
    if !path.is_empty() {
        save_entry(window, &path, false);
    }
}

/// 弹出文件夹选择框，把选中的文件夹加入「添加到」指定的名单。
fn add_folder(window: HWND) {
    let mut display_name = [0u16; 260];
    let title = wide("选择要加入名单的文件夹");

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

    let path = read_wide(&buffer);
    if !path.is_empty() {
        save_entry(window, &path, true);
    }
}

/// 把一条新条目写入「添加到」指定的名单，然后刷新列表。
fn save_entry(window: HWND, path: &str, is_folder: bool) {
    let Ok(data_dir) = Config::default_data_dir() else {
        return;
    };
    let mode = target_mode(window);
    match Database::init(&data_dir).and_then(|db| db.add_list_entry(mode, path, is_folder)) {
        Ok(()) => log::info!(
            "[名单] 已添加到{}：{} {}",
            if mode == MODE_WHITE {
                "白名单"
            } else {
                "黑名单"
            },
            if is_folder { "文件夹" } else { "程序" },
            path
        ),
        Err(error) => log::error!("[名单] 添加失败：{:#}", error),
    }
    refresh(window);
}

/// 移除列表里当前选中的那一条（按该行自己所属的名单删除）。
fn remove_selected(window: HWND) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    // LVM_GETNEXTITEM：wParam 传起始下标 -1（即 usize::MAX），lParam 为 LVNI_SELECTED。
    let index = unsafe {
        SendMessageW(
            list,
            LVM_GETNEXTITEM,
            WPARAM(usize::MAX),
            LPARAM(LVNI_SELECTED as isize),
        )
    }
    .0;

    let entry = if index >= 0 {
        CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.get(index as usize).cloned())
    } else {
        None
    };

    let Some((mode, entry)) = entry else {
        let text = wide("请先在列表里选中一条要移除的条目。");
        let title = wide("文件来源追踪");
        unsafe {
            let _ = MessageBoxW(
                window,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        }
        return;
    };

    let Ok(data_dir) = Config::default_data_dir() else {
        return;
    };
    match Database::init(&data_dir).and_then(|db| db.remove_list_entry(&mode, &entry.path)) {
        Ok(()) => log::info!(
            "[名单] 已从{}移除：{}",
            if mode == MODE_WHITE {
                "白名单"
            } else {
                "黑名单"
            },
            entry.path
        ),
        Err(error) => log::error!("[名单] 移除失败：{:#}", error),
    }
    refresh(window);
}

/// 把当前选中的条目切换到另一个名单（黑名单 ↔ 白名单）。
///
/// `warn_when_empty`：没有选中任何行时要不要弹提示。
/// - 点「切换名单」按钮时传 true，提示用户先选中一行；
/// - 双击列表空白处时传 false，静默忽略，避免误弹窗口。
fn toggle_selected(window: HWND, warn_when_empty: bool) {
    let list = HWND(LIST_VIEW.load(Ordering::SeqCst) as *mut c_void);
    if list.0.is_null() {
        return;
    }

    // 和「移除选中」一样，先取当前选中的行号（没有选中时是 -1）。
    let index = unsafe {
        SendMessageW(
            list,
            LVM_GETNEXTITEM,
            WPARAM(usize::MAX),
            LPARAM(LVNI_SELECTED as isize),
        )
    }
    .0;

    let entry = if index >= 0 {
        CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.get(index as usize).cloned())
    } else {
        None
    };

    let Some((mode, entry)) = entry else {
        if warn_when_empty {
            let text = wide("请先在列表里选中一条要切换的条目。");
            let title = wide("文件来源追踪");
            unsafe {
                let _ = MessageBoxW(
                    window,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONWARNING,
                );
            }
        }
        return;
    };

    // 黑名单 -> 白名单；白名单 -> 黑名单。
    let target = if mode == MODE_WHITE {
        MODE_BLACK
    } else {
        MODE_WHITE
    };

    let Ok(data_dir) = Config::default_data_dir() else {
        return;
    };
    match Database::init(&data_dir).and_then(|db| db.move_list_entry(&mode, &entry.path, target)) {
        Ok(()) => log::info!(
            "[名单] 已把 {} 从{}切换到{}",
            entry.path,
            if mode == MODE_WHITE {
                "白名单"
            } else {
                "黑名单"
            },
            if target == MODE_WHITE {
                "白名单"
            } else {
                "黑名单"
            }
        ),
        Err(error) => log::error!("[名单] 切换名单失败：{:#}", error),
    }
    refresh(window);
}

/// 从以 0 结尾的宽字符缓冲区里取出字符串。
fn read_wide(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}
