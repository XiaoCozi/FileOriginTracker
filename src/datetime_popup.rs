//! 几何风格的「日期 + 时间」选择浮窗。
//!
//! 系统自带的日期时间框（`SysDateTimePick32`）弹出的日历又小又挤，
//! 而且只能选日期，选时间还得回去用键盘改，非常别扭。
//! 这里用一个完全自绘的弹出窗口替换它：
//!
//! - **几何风格**：直角矩形、无渐变、无圆角，只用平涂色块和 1 像素细线
//! - **空间宽松**：格子 46×36，四周留 16 像素内边距，不再挤成一团
//! - **同时选时间**：日历下方有 时 / 分 / 秒 三组「− 数值 +」调节器
//! - **快捷入口**：「今天」一键回到当前时间，「确定 / 取消」决定是否采纳
//!
//! 交互方式：
//! - 鼠标点格子选日期，点 `−` / `+` 调整时分秒
//! - `←` `→` 前后一天，`↑` `↓` 前后一周，`PageUp` `PageDown` 换月
//! - `Enter` 确定，`Esc` 取消
//!
//! 实现上是一个 `WS_POPUP` 窗口 + 本线程内的模态消息循环，
//! 和 `note.rs` 的做法一致：打开时禁用父窗口，关闭后恢复，
//! 因此浮窗打开期间不会误操作后面的记录列表。

use once_cell::sync::Lazy;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SYSTEMTIME, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, HGDIOBJ, HDC, HFONT, PAINTSTRUCT,
    SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    OUT_DEFAULT_PRECIS, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetSystemMetrics, GetWindowRect, IsWindow, LoadCursorW, RegisterClassExW, SetForegroundWindow,
    ShowWindow, TranslateMessage, HMENU, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL,
    WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW, WM_ACTIVATE, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::util::wide;

/// 窗口类名。注册一次后重复使用。
const WINDOW_CLASS: &str = "FileOriginTrackerDateTimePopup";

// ===== 尺寸常量（全部为像素）=====
//
// 窗口总体比系统日历大一圈，配合 16 像素内边距，视觉上更舒展。
const WIDTH: i32 = 368;
const HEIGHT: i32 = 460;
/// 四周统一内边距。
const PAD: i32 = 16;

/// 月份标题那一行。
const HEADER_Y: i32 = 16;
const HEADER_H: i32 = 34;

/// 星期几标题行。
const WEEKDAY_Y: i32 = 60;
const WEEKDAY_H: i32 = 22;

/// 日期格子：46 宽、36 高，格子之间留 2 像素缝隙，
/// 这样即使不做圆角、不用阴影，也不会糊成一片。
const GRID_X: i32 = PAD + 1;
const GRID_Y: i32 = 88;
const CELL_W: i32 = 46;
const CELL_H: i32 = 36;
const CELL_GAP: i32 = 2;

/// 日历和时间区域之间的分隔线。
const DIVIDER_Y: i32 = 324;

/// 时间区域：「时 / 分 / 秒」标题 + 调节器。
const TIME_LABEL_Y: i32 = 334;
const TIME_ROW_Y: i32 = 356;
const TIME_ROW_H: i32 = 34;
/// 每组「− 数值 +」的宽度，以及组与组之间的间隔。
const TIME_GROUP_W: i32 = 104;
const TIME_GROUP_GAP: i32 = 12;

/// 底部按钮行。
const FOOTER_Y: i32 = 408;
const FOOTER_H: i32 = 36;

// ===== 调色板 =====
//
// COLORREF 的字节顺序是 0x00BBGGRR（蓝、绿、红），不是常见的 RGB，
// 所以用这个 const 函数把习惯的 RGB 顺序转换成 Windows 需要的格式。
const fn rgb(red: u32, green: u32, blue: u32) -> COLORREF {
    COLORREF(red | (green << 8) | (blue << 16))
}

/// 浮窗背景（纯白）。
const BG: COLORREF = rgb(255, 255, 255);
/// 浅灰面板色，用于未选中的按钮和数值框。
const PANEL: COLORREF = rgb(247, 248, 250);
/// 常规描边色。
const BORDER: COLORREF = rgb(216, 220, 229);
/// 更淡的描边，用于内部数值框。
const BORDER_SOFT: COLORREF = rgb(235, 237, 241);
/// 主要文字色（近黑）。
const TEXT: COLORREF = rgb(31, 35, 41);
/// 次要文字色（灰）。
const MUTED: COLORREF = rgb(140, 146, 156);
/// 相邻月份的日期文字色（更淡）。
const FAINT: COLORREF = rgb(190, 195, 204);
/// 强调色：选中日期、确定按钮、焦点描边都用它。
const ACCENT: COLORREF = rgb(47, 107, 255);
/// 强调色的浅色填充（悬停）。
const ACCENT_SOFT: COLORREF = rgb(234, 241, 255);
/// 强调色的深一点的填充（按钮按下感）。
const ACCENT_HOVER: COLORREF = rgb(214, 226, 255);

/// 虚函数键码（winuser.h），只用到这几个。
const VK_ESCAPE: u32 = 0x1B;
const VK_RETURN: u32 = 0x0D;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;

/// 一个可点击区域。
///
/// 绘制和命中测试共用同一个枚举，避免“画在这里、点在那里”的错位问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    None,
    /// 上月标题旁的四个翻页按钮：上一年 / 上一月 / 下一月 / 下一年。
    PrevYear,
    PrevMonth,
    NextMonth,
    NextYear,
    /// 日期格子（行、列，均从 0 开始）。
    Day(usize, usize),
    /// 时分秒的加减按钮。
    HourMinus,
    HourPlus,
    MinuteMinus,
    MinutePlus,
    SecondMinus,
    SecondPlus,
    /// 底部三个按钮。
    Today,
    Cancel,
    Ok,
}

/// 浮窗的全部状态。
///
/// 本程序同一时刻只会打开一个日期浮窗，所以用全局静态变量保存即可，
/// 不需要把状态挂在窗口的 user data 上。
struct State {
    /// 日历当前显示的年份 / 月份（可能和选中日期不同月份，用于翻页浏览）。
    view_year: i32,
    view_month: u32,
    /// 当前选中的日期。
    sel_year: i32,
    sel_month: u32,
    sel_day: u32,
    /// 当前选中的时间。
    hour: u32,
    minute: u32,
    second: u32,
    /// 鼠标悬停的区域，用于绘制高亮。
    hot: Hit,
    /// 点击“确定”后的结果（Unix 秒）。`None` 表示用户取消。
    result: Option<i64>,
    /// 浮窗是否已经获得过焦点。
    ///
    /// 用于避免刚创建时误收到一条失去焦点的消息就把窗口关掉。
    activated: bool,
}

impl State {
    fn empty() -> Self {
        Self {
            view_year: 2026,
            view_month: 1,
            sel_year: 2026,
            sel_month: 1,
            sel_day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            hot: Hit::None,
            result: None,
            activated: false,
        }
    }
}

static STATE: Lazy<Mutex<State>> = Lazy::new(|| Mutex::new(State::empty()));
static CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// 四套字体句柄（标题 / 粗体 / 正文 / 小字），首次打开时创建、进程结束前复用。
static FONT_TITLE: AtomicIsize = AtomicIsize::new(0);
static FONT_BOLD: AtomicIsize = AtomicIsize::new(0);
static FONT_BODY: AtomicIsize = AtomicIsize::new(0);
static FONT_SMALL: AtomicIsize = AtomicIsize::new(0);

/// 打开日期时间浮窗，返回用户选中的 Unix 秒数。
///
/// - `owner`：父窗口，浮窗打开期间会被禁用（模态效果）
/// - `anchor`：定位基准，通常是「起」或「止」那个时间框
/// - `initial_unix`：初始时间，决定日历先显示到哪个月
///
/// 返回 `None` 表示用户取消（点了「取消」、按了 `Esc` 或点到别处）。
pub fn show(owner: HWND, anchor: HWND, initial_unix: i64) -> Option<i64> {
    // 把初始时间拆成本地时间，填进状态。
    let local = crate::util::unix_to_local_system_time(initial_unix);
    {
        let mut state = lock_state();
        state.view_year = local.wYear as i32;
        state.view_month = local.wMonth.max(1) as u32;
        state.sel_year = local.wYear as i32;
        state.sel_month = local.wMonth.max(1) as u32;
        state.sel_day = local.wDay.max(1) as u32;
        state.hour = local.wHour as u32;
        state.minute = local.wMinute as u32;
        state.second = local.wSecond as u32;
        state.hot = Hit::None;
        state.result = None;
        state.activated = false;
    }

    let module = unsafe { GetModuleHandleW(None).ok()? };
    let instance = HINSTANCE(module.0);
    if !register_class(instance) {
        return None;
    }
    ensure_fonts();

    // 默认显示在时间框正下方；屏幕下方放不下时翻到上方，并保证不超出屏幕。
    let (x, y) = popup_position(anchor)?;

    let class_name = wide(WINDOW_CLASS);
    let title = wide("选择时间");
    let hwnd = unsafe {
        CreateWindowExW(
            // TOOLWINDOW：不在任务栏单独占一项；TOPMOST：保证盖在记录窗口上面。
            WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE(WS_POPUP.0),
            x,
            y,
            WIDTH,
            HEIGHT,
            owner,
            HMENU::default(),
            instance,
            None,
        )
        .ok()?
    };

    unsafe {
        // 模态：先禁用父窗口，避免用户点到后面的列表。
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }

    // 本线程内单独跑一个消息循环，直到浮窗被销毁。
    // 和 note.rs 一样：每处理完一条消息检查一次窗口是否还在，
    // 销毁窗口的那条消息处理完就会立刻跳出循环。
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

    unsafe {
        let _ = EnableWindow(owner, true);
        let _ = SetForegroundWindow(owner);
    }

    lock_state().result
}

/// 给「最近记录」窗口里的时间框画外观（自绘按钮）。
///
/// 时间框本身是一个 `BS_OWNERDRAW` 按钮，系统只负责发 `WM_DRAWITEM`，
/// 长什么样完全由这里决定：
/// - 启用时：白底、浅灰描边、深色文字，右侧一个几何线条日历图标
/// - 未启用（左边复选框没勾）：灰底、灰字，提示“这一端暂不筛选”
pub fn draw_field(info: &DRAWITEMSTRUCT, text: &str, enabled: bool) {
    unsafe {
        let hdc = info.hDC;
        let rect = info.rcItem;
        // ODS_SELECTED = 0x0001，表示按钮正被按下。
        let pressed = info.itemState.0 & 0x0001 != 0;

        let background = if !enabled {
            PANEL
        } else if pressed {
            ACCENT_SOFT
        } else {
            BG
        };
        let border = if !enabled {
            BORDER_SOFT
        } else if pressed {
            ACCENT
        } else {
            BORDER
        };
        fill(hdc, &rect, background);
        frame(hdc, &rect, border);

        // 文字从左边留 10 像素开始，右侧 26 像素留给日历图标。
        let mut text_rect = RECT {
            left: rect.left + 10,
            top: rect.top,
            right: rect.right - 26,
            bottom: rect.bottom,
        };
        draw_text(
            hdc,
            text,
            &mut text_rect,
            DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
            if enabled { TEXT } else { MUTED },
            body_font(),
        );

        // 右侧日历图标（14×14）。
        let glyph_x = rect.right - 22;
        let glyph_y = rect.top + (rect.bottom - rect.top - 14) / 2;
        draw_calendar_glyph(hdc, glyph_x, glyph_y, if enabled { MUTED } else { FAINT });
    }
}

// ===== 窗口过程 =====

fn register_class(instance: HINSTANCE) -> bool {
    if CLASS_REGISTERED.swap(true, Ordering::SeqCst) {
        return true;
    }

    let class_name = wide(WINDOW_CLASS);
    let mut window_class = WNDCLASSEXW::default();
    window_class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    window_class.lpfnWndProc = Some(window_procedure);
    window_class.hInstance = instance;
    // 背景在 WM_PAINT 里整块绘制，这里不需要系统画刷。
    window_class.hbrBackground = windows::Win32::Graphics::Gdi::HBRUSH::default();
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
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        // 背景由 WM_PAINT 一次性画完，告诉系统不用再擦一遍，避免闪烁。
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let hit = hit_test(mouse_x(lparam), mouse_y(lparam));
            let changed = {
                let mut state = lock_state();
                if state.hot == hit {
                    false
                } else {
                    state.hot = hit;
                    true
                }
            };
            if changed {
                // 只重绘不擦背景，鼠标移动时不会闪。
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let _ = SetFocus(hwnd);
            let hit = hit_test(mouse_x(lparam), mouse_y(lparam));
            apply_hit(hwnd, hit);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            handle_key(hwnd, wparam.0 as u32);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let action = (wparam.0 & 0xFFFF) as u32;
            if action == WA_INACTIVE {
                // 点到别的窗口（例如别的程序）时直接关掉，避免浮窗孤零零留在屏幕上。
                let should_close = {
                    let state = lock_state();
                    state.activated
                };
                if should_close {
                    let _ = DestroyWindow(hwnd);
                }
            } else {
                lock_state().activated = true;
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

// ===== 交互逻辑 =====

fn handle_key(hwnd: HWND, key: u32) {
    match key {
        VK_ESCAPE => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        VK_RETURN => confirm(hwnd),
        VK_LEFT => shift_days(-1),
        VK_RIGHT => shift_days(1),
        VK_UP => shift_days(-7),
        VK_DOWN => shift_days(7),
        VK_PRIOR => adjust_view(-1),
        VK_NEXT => adjust_view(1),
        _ => return,
    }
    if unsafe { IsWindow(hwnd) }.0 != 0 {
        let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, false) };
    }
}

/// 执行一次点击。`hit` 为 `Hit::None` 时什么也不做。
fn apply_hit(hwnd: HWND, hit: Hit) {
    match hit {
        Hit::None => return,
        Hit::PrevYear => adjust_view(-12),
        Hit::PrevMonth => adjust_view(-1),
        Hit::NextMonth => adjust_view(1),
        Hit::NextYear => adjust_view(12),
        Hit::Day(row, column) => select_cell(row, column),
        Hit::HourMinus => step_hour(-1),
        Hit::HourPlus => step_hour(1),
        Hit::MinuteMinus => step_minute(-1),
        Hit::MinutePlus => step_minute(1),
        Hit::SecondMinus => step_second(-1),
        Hit::SecondPlus => step_second(1),
        Hit::Today => set_to_now(),
        Hit::Cancel => unsafe {
            let _ = DestroyWindow(hwnd);
            return;
        },
        Hit::Ok => {
            // 先算出时间再销毁窗口：销毁后状态虽然还在，但这样逻辑更清晰。
            confirm(hwnd);
            return;
        }
    }

    if unsafe { IsWindow(hwnd) }.0 != 0 {
        let _ = unsafe { windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, false) };
    }
}

/// 当前选中的日期时间换算成 Unix 秒，写进结果并关闭浮窗。
fn confirm(hwnd: HWND) {
    let value = {
        let state = lock_state();
        let system_time = SYSTEMTIME {
            wYear: state.sel_year.clamp(1601, 30827) as u16,
            wMonth: state.sel_month.clamp(1, 12) as u16,
            wDay: state.sel_day.clamp(1, 31) as u16,
            wHour: state.hour.min(23) as u16,
            wMinute: state.minute.min(59) as u16,
            wSecond: state.second.min(59) as u16,
            wMilliseconds: 0,
            ..Default::default()
        };
        crate::util::local_system_time_to_unix(&system_time)
    };

    if let Some(value) = value {
        lock_state().result = Some(value);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// 翻月 / 翻年。`months` 可以是负数（往前翻）。
fn adjust_view(months: i32) {
    let mut state = lock_state();
    let mut month = state.view_month as i32 + months;
    let mut year = state.view_year;
    while month < 1 {
        month += 12;
        year -= 1;
    }
    while month > 12 {
        month -= 12;
        year += 1;
    }
    state.view_month = month as u32;
    state.view_year = year;
}

/// 选中某个格子对应的日期；点相邻月份的格子时视图跟着跳过去。
fn select_cell(row: usize, column: usize) {
    let mut state = lock_state();
    let view_year = state.view_year;
    let view_month = state.view_month;
    let index = cell_index(view_year, view_month, row, column);
    let (year, month, day) = resolve_cell(view_year, view_month, index);

    state.sel_year = year;
    state.sel_month = month;
    state.sel_day = day;
    state.view_year = year;
    state.view_month = month;
}

/// 按 `delta` 天前后移动选中日期（跨月、跨年自动处理）。
fn shift_days(delta: i32) {
    let mut state = lock_state();
    let mut year = state.sel_year;
    let mut month = state.sel_month as i32;
    let mut day = state.sel_day as i32 + delta;

    // 往前越过 1 号就退到上个月的最后一天。
    while day < 1 {
        month -= 1;
        if month < 1 {
            month = 12;
            year -= 1;
        }
        day += days_in_month(year, month as u32) as i32;
    }
    // 往后越过月末就进到下个月。
    while day > days_in_month(year, month as u32) as i32 {
        day -= days_in_month(year, month as u32) as i32;
        month += 1;
        if month > 12 {
            month = 1;
            year += 1;
        }
    }

    state.sel_year = year;
    state.sel_month = month as u32;
    state.sel_day = day as u32;
    state.view_year = year;
    state.view_month = month as u32;
}

/// 小时加减，0~23 循环。
fn step_hour(delta: i32) {
    let mut state = lock_state();
    state.hour = (state.hour as i32 + delta).rem_euclid(24) as u32;
}

/// 分钟加减，0~59 循环。
fn step_minute(delta: i32) {
    let mut state = lock_state();
    state.minute = (state.minute as i32 + delta).rem_euclid(60) as u32;
}

/// 秒加减，0~59 循环。
fn step_second(delta: i32) {
    let mut state = lock_state();
    state.second = (state.second as i32 + delta).rem_euclid(60) as u32;
}

/// 「今天」：跳到系统当前日期时间，并把视图挪到当月。
fn set_to_now() {
    let now = no_std_now_unix();
    let local = crate::util::unix_to_local_system_time(now);
    let mut state = lock_state();
    state.view_year = local.wYear as i32;
    state.view_month = local.wMonth.max(1) as u32;
    state.sel_year = local.wYear as i32;
    state.sel_month = local.wMonth.max(1) as u32;
    state.sel_day = local.wDay.max(1) as u32;
    state.hour = local.wHour as u32;
    state.minute = local.wMinute as u32;
    state.second = local.wSecond as u32;
}

/// 当前 Unix 时间戳（秒）。取不到时返回 0，调用方会退回系统本地时间兜底。
fn no_std_now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

// ===== 布局计算 =====

/// 四个翻页按钮的位置，顺序与 [`Hit`] 对应。
fn nav_hits() -> [(Hit, RECT); 4] {
    [
        (
            Hit::PrevYear,
            RECT {
                left: PAD,
                top: HEADER_Y,
                right: PAD + 34,
                bottom: HEADER_Y + HEADER_H,
            },
        ),
        (
            Hit::PrevMonth,
            RECT {
                left: PAD + 38,
                top: HEADER_Y,
                right: PAD + 72,
                bottom: HEADER_Y + HEADER_H,
            },
        ),
        (
            Hit::NextMonth,
            RECT {
                left: WIDTH - PAD - 72,
                top: HEADER_Y,
                right: WIDTH - PAD - 38,
                bottom: HEADER_Y + HEADER_H,
            },
        ),
        (
            Hit::NextYear,
            RECT {
                left: WIDTH - PAD - 34,
                top: HEADER_Y,
                right: WIDTH - PAD,
                bottom: HEADER_Y + HEADER_H,
            },
        ),
    ]
}

/// 时分秒的六个加减按钮。
fn time_hits() -> [(Hit, RECT); 6] {
    let mut result = [(Hit::None, RECT::default()); 6];
    let units = [
        (Hit::HourMinus, Hit::HourPlus),
        (Hit::MinuteMinus, Hit::MinutePlus),
        (Hit::SecondMinus, Hit::SecondPlus),
    ];
    for (index, (minus, plus)) in units.iter().enumerate() {
        let x = PAD + index as i32 * (TIME_GROUP_W + TIME_GROUP_GAP);
        result[index * 2] = (
            *minus,
            RECT {
                left: x,
                top: TIME_ROW_Y,
                right: x + 30,
                bottom: TIME_ROW_Y + TIME_ROW_H,
            },
        );
        result[index * 2 + 1] = (
            *plus,
            RECT {
                left: x + TIME_GROUP_W - 30,
                top: TIME_ROW_Y,
                right: x + TIME_GROUP_W,
                bottom: TIME_ROW_Y + TIME_ROW_H,
            },
        );
    }
    result
}

/// 底部三个按钮的位置。
fn footer_hits() -> [(Hit, RECT); 3] {
    [
        (
            Hit::Today,
            RECT {
                left: PAD,
                top: FOOTER_Y,
                right: PAD + 80,
                bottom: FOOTER_Y + FOOTER_H,
            },
        ),
        (
            Hit::Cancel,
            RECT {
                left: WIDTH - PAD - 112 - 8 - 92,
                top: FOOTER_Y,
                right: WIDTH - PAD - 112 - 8,
                bottom: FOOTER_Y + FOOTER_H,
            },
        ),
        (
            Hit::Ok,
            RECT {
                left: WIDTH - PAD - 112,
                top: FOOTER_Y,
                right: WIDTH - PAD,
                bottom: FOOTER_Y + FOOTER_H,
            },
        ),
    ]
}

/// 某个日期格子（行、列从 0 开始）的矩形。
fn cell_rect(row: usize, column: usize) -> RECT {
    let left = GRID_X + column as i32 * (CELL_W + CELL_GAP);
    let top = GRID_Y + row as i32 * (CELL_H + CELL_GAP);
    RECT {
        left,
        top,
        right: left + CELL_W,
        bottom: top + CELL_H,
    }
}

/// 判断坐标是否落在矩形内。
fn contains(rect: &RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

/// 把鼠标坐标换算成可点击区域。
///
/// 顺序很重要：先判断上层的小按钮，最后才是大片的日期格子。
fn hit_test(x: i32, y: i32) -> Hit {
    for (hit, rect) in nav_hits() {
        if contains(&rect, x, y) {
            return hit;
        }
    }
    for (hit, rect) in time_hits() {
        if contains(&rect, x, y) {
            return hit;
        }
    }
    for (hit, rect) in footer_hits() {
        if contains(&rect, x, y) {
            return hit;
        }
    }
    for row in 0..6 {
        for column in 0..7 {
            if contains(&cell_rect(row, column), x, y) {
                return Hit::Day(row, column);
            }
        }
    }
    Hit::None
}

/// 日期格子的序号：第 0 格对应日历第一行第一列，
/// 1 表示当月 1 号，0 或负数表示上个月的末尾几天。
fn cell_index(view_year: i32, view_month: u32, row: usize, column: usize) -> i32 {
    (row * 7 + column) as i32 - weekday_of(view_year, view_month, 1) as i32 + 1
}

/// 把格子序号换算成具体年月日（会自动跨到上 / 下个月）。
fn resolve_cell(view_year: i32, view_month: u32, index: i32) -> (i32, u32, u32) {
    let days = days_in_month(view_year, view_month) as i32;
    if index < 1 {
        let (year, month) = previous_month(view_year, view_month);
        let day = days_in_month(year, month) as i32 + index;
        (year, month, day.max(1) as u32)
    } else if index > days {
        let (year, month) = next_month(view_year, view_month);
        (year, month, (index - days) as u32)
    } else {
        (view_year, view_month, index as u32)
    }
}

// ===== 日期计算 =====

/// 某年某月有几天（含闰年判断）。
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// 闰年判断：能被 4 整除但不能被 100 整除，或者能被 400 整除。
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// 某年某月 1 号是星期几（0 = 周日，6 = 周六）。
///
/// 使用 Sakamoto 算法，纯整数运算，不需要任何系统调用，
/// 因此可以安全地在绘制和命中测试里频繁调用。
fn weekday_of(year: i32, month: u32, day: u32) -> u32 {
    const OFFSETS: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut adjusted_year = year;
    if month < 3 {
        adjusted_year -= 1;
    }
    let index = (month as usize).clamp(1, 12) - 1;
    let value = adjusted_year
        + adjusted_year / 4
        - adjusted_year / 100
        + adjusted_year / 400
        + OFFSETS[index]
        + day as i32;
    value.rem_euclid(7) as u32
}

/// 上一个月（跨年自动处理）。
fn previous_month(year: i32, month: u32) -> (i32, u32) {
    if month <= 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

/// 下一个月（跨年自动处理）。
fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month >= 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

// ===== 绘制 =====

/// 双缓冲绘制整块窗口：先画到内存位图，再一次性贴到屏幕，
/// 鼠标悬停、翻月时都不会闪。
unsafe fn paint(hwnd: HWND) {
    let mut paint_struct = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut paint_struct);
    if hdc.0.is_null() {
        return;
    }

    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let width = (client.right - client.left).max(1);
    let height = (client.bottom - client.top).max(1);

    let memory = CreateCompatibleDC(hdc);
    let bitmap = CreateCompatibleBitmap(hdc, width, height);
    let previous = SelectObject(memory, HGDIOBJ(bitmap.0));

    draw_all(memory);

    let _ = BitBlt(hdc, 0, 0, width, height, memory, 0, 0, SRCCOPY);

    SelectObject(memory, previous);
    let _ = DeleteObject(HGDIOBJ(bitmap.0));
    let _ = DeleteDC(memory);
    let _ = EndPaint(hwnd, &paint_struct);
}

/// 绘制整个浮窗。
unsafe fn draw_all(hdc: HDC) {
    // 先把状态拷一份，避免绘制过程中一直占着锁。
    let snapshot = {
        let state = lock_state();
        Snapshot {
            view_year: state.view_year,
            view_month: state.view_month,
            sel_year: state.sel_year,
            sel_month: state.sel_month,
            sel_day: state.sel_day,
            hour: state.hour,
            minute: state.minute,
            second: state.second,
            hot: state.hot,
        }
    };
    let today = {
        let local = crate::util::unix_to_local_system_time(no_std_now_unix());
        (local.wYear as i32, local.wMonth as u32, local.wDay as u32)
    };

    // 整体底色 + 外框
    fill(
        hdc,
        &RECT {
            left: 0,
            top: 0,
            right: WIDTH,
            bottom: HEIGHT,
        },
        BG,
    );
    frame(
        hdc,
        &RECT {
            left: 0,
            top: 0,
            right: WIDTH,
            bottom: HEIGHT,
        },
        BORDER,
    );

    // ---- 顶部：翻页按钮 + 年月标题 ----
    let labels = [("«", Hit::PrevYear), ("‹", Hit::PrevMonth), ("›", Hit::NextMonth), ("»", Hit::NextYear)];
    for (label, hit) in labels {
        let rect = nav_hits()
            .iter()
            .find(|(candidate, _)| *candidate == hit)
            .map(|(_, rect)| *rect)
            .unwrap_or_default();
        let hovered = snapshot.hot == hit;
        fill(hdc, &rect, if hovered { ACCENT_SOFT } else { PANEL });
        frame(hdc, &rect, if hovered { ACCENT } else { BORDER_SOFT });
        let mut text_rect = rect;
        draw_text(
            hdc,
            label,
            &mut text_rect,
            DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
            if hovered { ACCENT } else { MUTED },
            title_font(),
        );
    }

    let mut title_rect = RECT {
        left: PAD + 76,
        top: HEADER_Y,
        right: WIDTH - PAD - 76,
        bottom: HEADER_Y + HEADER_H,
    };
    let title = format!("{} 年 {} 月", snapshot.view_year, snapshot.view_month);
    draw_text(
        hdc,
        &title,
        &mut title_rect,
        DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
        TEXT,
        title_font(),
    );

    // ---- 星期标题 ----
    const WEEKDAYS: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
    for (column, label) in WEEKDAYS.iter().enumerate() {
        let cell = cell_rect(0, column);
        let mut rect = RECT {
            left: cell.left,
            top: WEEKDAY_Y,
            right: cell.right,
            bottom: WEEKDAY_Y + WEEKDAY_H,
        };
        draw_text(
            hdc,
            label,
            &mut rect,
            DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
            MUTED,
            small_font(),
        );
    }

    // ---- 日期格子 ----
    let offset = weekday_of(snapshot.view_year, snapshot.view_month, 1) as i32;
    for row in 0..6 {
        for column in 0..7 {
            let index = (row * 7 + column) as i32 - offset + 1;
            let (year, month, day) =
                resolve_cell(snapshot.view_year, snapshot.view_month, index);
            let rect = cell_rect(row, column);

            let selected = year == snapshot.sel_year
                && month == snapshot.sel_month
                && day == snapshot.sel_day;
            let in_view_month = year == snapshot.view_year && month == snapshot.view_month;
            let hovered = snapshot.hot == Hit::Day(row, column);

            if selected {
                // 选中：整格铺强调色 + 白字，几何风格的“实心方块”。
                fill(hdc, &rect, ACCENT);
            } else if hovered {
                fill(hdc, &rect, ACCENT_SOFT);
            }

            // 今天：即使没被选中，也加一圈强调色描边，方便定位。
            if !selected && (year, month, day) == today {
                frame(hdc, &rect, ACCENT);
            }

            let mut text_rect = rect;
            draw_text(
                hdc,
                &day.to_string(),
                &mut text_rect,
                DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
                if selected {
                    BG
                } else if in_view_month {
                    TEXT
                } else {
                    FAINT
                },
                if selected { bold_font() } else { body_font() },
            );
        }
    }

    // ---- 分隔线 ----
    fill(
        hdc,
        &RECT {
            left: PAD,
            top: DIVIDER_Y,
            right: WIDTH - PAD,
            bottom: DIVIDER_Y + 1,
        },
        BORDER_SOFT,
    );

    // ---- 时间区域 ----
    let units = [
        ("时", snapshot.hour, Hit::HourMinus, Hit::HourPlus),
        ("分", snapshot.minute, Hit::MinuteMinus, Hit::MinutePlus),
        ("秒", snapshot.second, Hit::SecondMinus, Hit::SecondPlus),
    ];
    let hit_rects = time_hits();
    for (index, (label, value, minus, plus)) in units.iter().enumerate() {
        let group_x = PAD + index as i32 * (TIME_GROUP_W + TIME_GROUP_GAP);

        // 单位标题
        let mut label_rect = RECT {
            left: group_x,
            top: TIME_LABEL_Y,
            right: group_x + TIME_GROUP_W,
            bottom: TIME_LABEL_Y + 18,
        };
        draw_text(
            hdc,
            label,
            &mut label_rect,
            DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
            MUTED,
            small_font(),
        );

        // 「−」「+」按钮
        for (hit, text) in [(*minus, "-"), (*plus, "+")] {
            let rect = hit_rects
                .iter()
                .find(|(candidate, _)| candidate == &hit)
                .map(|(_, rect)| *rect)
                .unwrap_or_default();
            let hovered = snapshot.hot == hit;
            fill(hdc, &rect, if hovered { ACCENT_SOFT } else { PANEL });
            frame(hdc, &rect, if hovered { ACCENT } else { BORDER_SOFT });
            let mut text_rect = rect;
            draw_text(
                hdc,
                text,
                &mut text_rect,
                DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
                if hovered { ACCENT } else { TEXT },
                bold_font(),
            );
        }

        // 中间数值框
        let value_rect = RECT {
            left: group_x + 31,
            top: TIME_ROW_Y,
            right: group_x + 73,
            bottom: TIME_ROW_Y + TIME_ROW_H,
        };
        fill(hdc, &value_rect, PANEL);
        frame(hdc, &value_rect, BORDER_SOFT);
        let mut text_rect = value_rect;
        draw_text(
            hdc,
            &format!("{:02}", value),
            &mut text_rect,
            DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
            TEXT,
            bold_font(),
        );
    }

    // ---- 底部按钮 ----
    let footer = footer_hits();
    for (hit, label, filled) in [
        (Hit::Today, "今天", false),
        (Hit::Cancel, "取消", false),
        (Hit::Ok, "确定", true),
    ] {
        let rect = footer
            .iter()
            .find(|(candidate, _)| *candidate == hit)
            .map(|(_, rect)| *rect)
            .unwrap_or_default();
        let hovered = snapshot.hot == hit;

        if filled {
            fill(hdc, &rect, if hovered { ACCENT_HOVER } else { ACCENT });
            frame(hdc, &rect, ACCENT);
            let mut text_rect = rect;
            draw_text(
                hdc,
                label,
                &mut text_rect,
                DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
                BG,
                body_font(),
            );
        } else {
            fill(hdc, &rect, if hovered { ACCENT_SOFT } else { PANEL });
            frame(hdc, &rect, if hovered { ACCENT } else { BORDER });
            let mut text_rect = rect;
            draw_text(
                hdc,
                label,
                &mut text_rect,
                DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
                if hovered { ACCENT } else { TEXT },
                body_font(),
            );
        }
    }
}

/// 绘制时用到的状态副本。
struct Snapshot {
    view_year: i32,
    view_month: u32,
    sel_year: i32,
    sel_month: u32,
    sel_day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    hot: Hit,
}

/// 纯色填充一个矩形。
unsafe fn fill(hdc: HDC, rect: &RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rect, brush);
    // 画刷用完立即释放，避免每帧累积 GDI 对象。
    let _ = DeleteObject(HGDIOBJ(brush.0));
}

/// 只用 1 像素细线描边（四个长条拼成）。
///
/// 不用 `Rectangle()` 是因为它会带上当前画笔和画刷，容易画出多余的填充；
/// 这里直接铺色块，效果稳定，也符合“几何线条”的观感。
unsafe fn frame(hdc: HDC, rect: &RECT, color: COLORREF) {
    fill(
        hdc,
        &RECT {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.top + 1,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: rect.left,
            top: rect.bottom - 1,
            right: rect.right,
            bottom: rect.bottom,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: rect.left,
            top: rect.top + 1,
            right: rect.left + 1,
            bottom: rect.bottom - 1,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: rect.right - 1,
            top: rect.top + 1,
            right: rect.right,
            bottom: rect.bottom - 1,
        },
        color,
    );
}

/// 画一行文字。
///
/// `flags` 用 `DT_*` 常量组合（居中对齐、垂直居中、单行）。
unsafe fn draw_text(
    hdc: HDC,
    value: &str,
    rect: &mut RECT,
    flags: u32,
    color: COLORREF,
    font: HFONT,
) {
    let previous = SelectObject(hdc, HGDIOBJ(font.0));
    SetTextColor(hdc, color);
    // 透明背景，否则文字周围会出现一块底色。
    SetBkMode(hdc, TRANSPARENT);

    let mut buffer = wide(value);
    // 去掉结尾的 0，DrawTextW 自己按长度处理。
    let length = buffer.len().saturating_sub(1);
    DrawTextW(
        hdc,
        &mut buffer[..length],
        rect,
        DRAW_TEXT_FORMAT(flags),
    );

    SelectObject(hdc, previous);
}

/// 时间框右侧的小日历图标：一个方框 + 顶部条 + 三个小方块。
unsafe fn draw_calendar_glyph(hdc: HDC, x: i32, y: i32, color: COLORREF) {
    frame(
        hdc,
        &RECT {
            left: x,
            top: y,
            right: x + 14,
            bottom: y + 14,
        },
        color,
    );
    fill(
        hdc,
        &RECT {
            left: x + 1,
            top: y + 1,
            right: x + 13,
            bottom: y + 4,
        },
        color,
    );
    for (dx, dy) in [(3, 7), (9, 7), (3, 11)] {
        fill(
            hdc,
            &RECT {
                left: x + dx,
                top: y + dy,
                right: x + dx + 2,
                bottom: y + dy + 2,
            },
            color,
        );
    }
}

// ===== 字体 =====

/// 首次打开时创建四套字体；之后直接复用。
fn ensure_fonts() {
    if FONT_BODY.load(Ordering::SeqCst) != 0 {
        return;
    }
    unsafe {
        FONT_TITLE.store(create_font(-20, 600).0 as isize, Ordering::SeqCst);
        FONT_BOLD.store(create_font(-18, 600).0 as isize, Ordering::SeqCst);
        FONT_BODY.store(create_font(-18, 400).0 as isize, Ordering::SeqCst);
        FONT_SMALL.store(create_font(-14, 400).0 as isize, Ordering::SeqCst);
    }
}

/// 创建「微软雅黑 UI」字体。`height` 为负表示字符高度（像素）。
unsafe fn create_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        DEFAULT_PITCH.0 as u32,
        PCWSTR(face.as_ptr()),
    )
}

fn title_font() -> HFONT {
    HFONT(FONT_TITLE.load(Ordering::SeqCst) as *mut c_void)
}

fn bold_font() -> HFONT {
    HFONT(FONT_BOLD.load(Ordering::SeqCst) as *mut c_void)
}

fn body_font() -> HFONT {
    HFONT(FONT_BODY.load(Ordering::SeqCst) as *mut c_void)
}

fn small_font() -> HFONT {
    HFONT(FONT_SMALL.load(Ordering::SeqCst) as *mut c_void)
}

// ===== 小工具 =====

fn lock_state() -> std::sync::MutexGuard<'static, State> {
    match STATE.lock() {
        Ok(guard) => guard,
        // 之前的绘制如果 panic 过，锁会被标记为中毒；这里直接取出继续用，
        // 总比让整个界面崩掉好。
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 鼠标消息 lparam 的低 16 位是 x 坐标（有符号）。
fn mouse_x(lparam: LPARAM) -> i32 {
    (lparam.0 & 0xFFFF) as u16 as i16 as i32
}

/// 高 16 位是 y 坐标（有符号）。
fn mouse_y(lparam: LPARAM) -> i32 {
    ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// 计算浮窗左上角坐标：优先贴在时间框下方，空间不够就翻到上方。
fn popup_position(anchor: HWND) -> Option<(i32, i32)> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(anchor, &mut rect).ok()?;
    }

    let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };

    let mut x = rect.left;
    let mut y = rect.bottom + 4;

    if y + HEIGHT > screen_height - 8 {
        y = rect.top - HEIGHT - 4;
    }
    if y < 8 {
        y = 8;
    }
    if x + WIDTH > screen_width - 8 {
        x = screen_width - WIDTH - 8;
    }
    if x < 8 {
        x = 8;
    }

    Some((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 多个测试共用同一个全局 STATE，加一把锁让它们串行执行，
    /// 避免并行跑测试时互相覆盖状态。
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    /// 闰年 2 月 29 天、平年 2 月 28 天。
    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 12), 31);
    }

    /// 用几个已知日期校验星期计算（0 = 周日）。
    #[test]
    fn weekday_of_matches_known_dates() {
        assert_eq!(weekday_of(2026, 11, 19), 4); // 周四
        assert_eq!(weekday_of(2024, 2, 29), 4); // 周四
        assert_eq!(weekday_of(2000, 1, 1), 6); // 周六
        assert_eq!(weekday_of(2026, 1, 1), 4); // 周四
    }

    /// 跨月、跨年移动日期。
    #[test]
    fn shift_days_crosses_month_and_year() {
        let _guard = TEST_GUARD.lock().unwrap();
        {
            let mut state = lock_state();
            state.sel_year = 2026;
            state.sel_month = 10;
            state.sel_day = 31;
            drop(state);
        }
        shift_days(1);
        {
            let state = lock_state();
            assert_eq!(
                (state.sel_year, state.sel_month, state.sel_day),
                (2026, 11, 1)
            );
        }
        shift_days(-2);
        {
            let state = lock_state();
            assert_eq!(
                (state.sel_year, state.sel_month, state.sel_day),
                (2026, 10, 30)
            );
        }

        {
            let mut state = lock_state();
            state.sel_year = 2026;
            state.sel_month = 1;
            state.sel_day = 1;
        }
        shift_days(-1);
        {
            let state = lock_state();
            assert_eq!(
                (state.sel_year, state.sel_month, state.sel_day),
                (2025, 12, 31)
            );
        }
    }

    /// 相邻月份的格子序号应能正确解析成上一月 / 下一月的日期。
    #[test]
    fn resolve_cell_handles_adjacent_months() {
        // 2026-11-01 是周日，所以 11 月 1 日正好落在第 1 格。
        assert_eq!(weekday_of(2026, 11, 1), 0);
        assert_eq!(resolve_cell(2026, 11, 1), (2026, 11, 1));
        assert_eq!(resolve_cell(2026, 11, 0), (2026, 10, 31));
        assert_eq!(resolve_cell(2026, 11, 31), (2026, 12, 1));
    }

    /// 时分秒加减应正确循环。
    #[test]
    fn time_steps_wrap_around() {
        let _guard = TEST_GUARD.lock().unwrap();
        {
            let mut state = lock_state();
            state.hour = 0;
            state.minute = 59;
            state.second = 59;
        }
        step_hour(-1);
        step_minute(1);
        step_second(1);
        let state = lock_state();
        assert_eq!(state.hour, 23);
        assert_eq!(state.minute, 0);
        assert_eq!(state.second, 0);
    }
}
