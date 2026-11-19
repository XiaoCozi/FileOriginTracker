//! 全局几何风格主题。
//!
//! 所有窗口（最近记录 / 设置 / 程序统计 / 黑白名单 / 关于 / 备注 / 导出）
//! 共用这里的一份调色板、字体和绘制函数，保证观感一致：
//!
//! - 纯白背景 + 1 像素细描边，没有圆角、渐变和阴影
//! - 强调色统一为蓝色（`ACCENT`），只用在主按钮和选中态上
//! - 字体统一「微软雅黑 UI」：正文 9pt、标题 11pt 加粗
//!
//! **性能说明**：这些函数只在窗口重绘时被调用（`WM_PAINT` / `WM_DRAWITEM`），
//! 没有定时器、动画或后台绘制，空闲时 CPU 占用与原生控件完全一样。

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DRAW_TEXT_FORMAT, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::Controls::{
    HDF_SORTDOWN, HDF_SORTUP, HDITEMW, HDI_FORMAT, HDI_TEXT, HDM_GETITEMCOUNT, HDM_GETITEMRECT,
    HDM_GETITEMW, LVM_GETHEADER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, SendMessageW, WM_ERASEBKGND, WM_PAINT, WM_PRINTCLIENT,
};

use crate::util::wide;

/// 把习惯的 RGB 顺序转成 Windows 的 COLORREF（0x00BBGGRR）。
const fn rgb(red: u32, green: u32, blue: u32) -> COLORREF {
    COLORREF(red | (green << 8) | (blue << 16))
}

/// 窗口 / 控件背景（纯白）。
pub const BG: COLORREF = rgb(255, 255, 255);
/// 浅灰面板，用于普通按钮、数值框等次要元素。
pub const PANEL: COLORREF = rgb(247, 248, 250);
/// 常规描边。
pub const BORDER: COLORREF = rgb(216, 220, 229);
/// 更淡的描边，用于内部细框、分组线。
pub const BORDER_SOFT: COLORREF = rgb(235, 237, 241);
/// 主要文字。
pub const TEXT: COLORREF = rgb(31, 35, 41);
/// 次要文字（说明、单位、表头）。
pub const MUTED: COLORREF = rgb(140, 146, 156);
/// 最淡的文字（相邻月份的日期等）。
pub const FAINT: COLORREF = rgb(190, 195, 204);
/// 强调色：主按钮、选中格子、今天描边。
pub const ACCENT: COLORREF = rgb(47, 107, 255);
/// 强调色的浅填充（悬停、按下）。
pub const ACCENT_SOFT: COLORREF = rgb(234, 241, 255);
/// 强调色更深的填充（按下状态）。
pub const ACCENT_HOVER: COLORREF = rgb(214, 226, 255);

/// 四套字体句柄（标题 / 粗体 / 正文 / 小字），进程内只创建一次。
static FONT_TITLE: AtomicIsize = AtomicIsize::new(0);
static FONT_BOLD: AtomicIsize = AtomicIsize::new(0);
static FONT_BODY: AtomicIsize = AtomicIsize::new(0);
static FONT_SMALL: AtomicIsize = AtomicIsize::new(0);

/// 标题字体：11pt 加粗，用于年月标题、分组标题。
pub fn title_font() -> HFONT {
    cached_font(&FONT_TITLE, -15, 600)
}

/// 加粗正文：9pt 加粗，用于选中的日期、数值。
pub fn bold_font() -> HFONT {
    cached_font(&FONT_BOLD, -13, 600)
}

/// 正文字体：9pt，和列表、标签一致。
pub fn body_font() -> HFONT {
    cached_font(&FONT_BODY, -13, 400)
}

/// 小字：8pt，用于星期、单位等辅助信息。
pub fn small_font() -> HFONT {
    cached_font(&FONT_SMALL, -11, 400)
}

/// 惰性创建并缓存字体。
fn cached_font(slot: &AtomicIsize, height: i32, weight: i32) -> HFONT {
    let existing = slot.load(Ordering::SeqCst);
    if existing != 0 {
        return HFONT(existing as *mut c_void);
    }
    let face = wide("Microsoft YaHei UI");
    let font = unsafe {
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
    };
    slot.store(font.0 as isize, Ordering::SeqCst);
    font
}

/// 纯色填充一个矩形。
pub unsafe fn fill(hdc: HDC, rect: &RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rect, brush);
    // 画刷用完立即释放，避免每帧累积 GDI 对象。
    let _ = DeleteObject(HGDIOBJ(brush.0));
}

/// 1 像素细线描边（四个长条拼成）。
///
/// 不用 `Rectangle()` 是因为它会带上当前画笔和画刷，容易画出多余的填充；
/// 直接铺色块效果稳定，也符合“几何线条”的观感。
pub unsafe fn frame(hdc: HDC, rect: &RECT, color: COLORREF) {
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

/// 画一行文字。`flags` 用 `DT_*` 常量组合。
pub unsafe fn draw_text(
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
    let length = buffer.len().saturating_sub(1);
    DrawTextW(hdc, &mut buffer[..length], rect, DRAW_TEXT_FORMAT(flags));

    SelectObject(hdc, previous);
}

/// 绘制一个平面按钮（由 `WM_DRAWITEM` 调用）。
///
/// - 普通按钮：浅灰底 + 细描边
/// - 主按钮（`primary = true`）：强调色实心底 + 白字
/// - 按下：边框变成强调色；禁用：灰字
pub unsafe fn draw_button(info: &DRAWITEMSTRUCT, label: &str, primary: bool) {
    let rect = info.rcItem;
    let pressed = info.itemState.0 & 0x0001 != 0; // ODS_SELECTED
    let disabled = info.itemState.0 & 0x0004 != 0; // ODS_DISABLED

    if primary {
        let background = if disabled {
            BORDER
        } else if pressed {
            ACCENT_HOVER
        } else {
            ACCENT
        };
        fill(info.hDC, &rect, background);
        frame(info.hDC, &rect, background);
        let mut text_rect = rect;
        draw_text(
            info.hDC,
            label,
            &mut text_rect,
            DT_CENTER_FLAGS,
            BG,
            body_font(),
        );
    } else {
        let background = if disabled {
            BG
        } else if pressed {
            ACCENT_SOFT
        } else {
            PANEL
        };
        let border = if disabled {
            BORDER_SOFT
        } else if pressed {
            ACCENT
        } else {
            BORDER
        };
        fill(info.hDC, &rect, background);
        frame(info.hDC, &rect, border);
        let mut text_rect = rect;
        draw_text(
            info.hDC,
            label,
            &mut text_rect,
            DT_CENTER_FLAGS,
            if disabled { FAINT } else { TEXT },
            body_font(),
        );
    }
}

/// 从按钮自身读取文字后绘制（`BS_OWNERDRAW` 按钮仍保留窗口文字）。
///
/// 这样各个窗口处理 `WM_DRAWITEM` 时只需要判断“哪些控件 ID 归我画”，
/// 不用再维护一份「ID → 文案」的映射表。
pub unsafe fn draw_button_auto(info: &DRAWITEMSTRUCT, primary: bool) {
    let mut buffer = [0u16; 128];
    let length =
        windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(info.hwndItem, &mut buffer);
    let label = String::from_utf16_lossy(&buffer[..length as usize]);
    draw_button(info, &label, primary);
}

/// 居中 + 垂直居中 + 单行，按钮文字统一用它。
const DT_CENTER_FLAGS: u32 = windows::Win32::Graphics::Gdi::DT_CENTER.0
    | windows::Win32::Graphics::Gdi::DT_VCENTER.0
    | windows::Win32::Graphics::Gdi::DT_SINGLELINE.0;

/// 在窗口的 `WM_PAINT` 里画一个“分组”：细线边框 + 左上角标题。///
/// 用来替代系统的 `BS_GROUPBOX`（那个带 3D 凹凸边，和几何风格不搭）。
pub unsafe fn draw_section(hdc: HDC, rect: &RECT, title: &str) {
    frame(hdc, rect, BORDER);

    // 按实际文字宽度铺白底，标题有多长就盖多长，
    // 既不会裁掉长标题，也不会在边框上留下过长的缺口。
    let font = bold_font();
    let previous = SelectObject(hdc, HGDIOBJ(font.0));
    let buffer = wide(title);
    let length = buffer.len().saturating_sub(1);
    let mut size = windows::Win32::Foundation::SIZE::default();
    let _ = windows::Win32::Graphics::Gdi::GetTextExtentPoint32W(hdc, &buffer[..length], &mut size);
    SelectObject(hdc, previous);

    let mut title_rect = RECT {
        left: rect.left + 12,
        top: rect.top - 8,
        right: rect.left + 22 + size.cx,
        bottom: rect.top + 10,
    };
    fill(
        hdc,
        &RECT {
            left: title_rect.left,
            top: title_rect.top,
            right: title_rect.right,
            bottom: title_rect.bottom,
        },
        BG,
    );
    draw_text(
        hdc,
        title,
        &mut title_rect,
        DT_LEFT.0 | DT_SINGLELINE.0 | DT_VCENTER.0,
        TEXT,
        bold_font(),
    );
}

// ===== 自绘复选框 / 单选框 =====
//
// 原生的勾选框和单选框由系统画，圆角 + 灰色，和几何风格不一致。
// 这里不改变控件类型（仍是 BS_AUTOCHECKBOX / BS_AUTORADIOBUTTON），
// 只通过子类化接管它的 WM_PAINT：
// - 点击切换、BM_GETCHECK 读取状态等行为仍由系统负责，逻辑代码不用改
// - 外观改成直角方块：选中时填充强调色，未选中只有 1 像素描边
//
// 子类化只在控件重绘时执行，开销可以忽略。

/// `BS_TYPEMASK`：取出按钮类型位，用来判断控件是复选框还是单选框。
const BS_TYPEMASK: u32 = 0x000F;
const BS_AUTOCHECKBOX_STYLE: u32 = 3;
const BS_AUTORADIOBUTTON_STYLE: u32 = 9;
const BS_CHECKBOX_STYLE: u32 = 2;
const BS_RADIOBUTTON_STYLE: u32 = 4;
const BS_AUTO3STATE_STYLE: u32 = 6;
/// `BS_OWNERDRAW`：改成自绘按钮后，系统不再画任何原生外观。
const BS_OWNERDRAW_STYLE: u32 = 0x000B;

/// 给窗口里的所有复选框 / 单选框装上几何风格外观。
///
/// 在 `create_controls` 末尾调用一次即可。
///
/// 做法分两步：
/// 1. 把控件类型改成 `BS_OWNERDRAW`。这样即使系统的原生绘制在某条
///    路径上绕过了我们，原生勾选框也不会被画出来（owner-draw 的原生
///    绘制是“通知父窗口去画”，自己不画），从根上避免新旧图案重叠。
/// 2. 记住原始类型（复选框 / 单选框）并安装子类，由我们统一绘制。
///
/// 代价是 owner-draw 按钮不会自动切换勾选状态，所以点击后需要在父窗口的
/// `WM_COMMAND` 里调用 [`apply_click_state`] 手动完成切换。
pub unsafe fn style_children(parent: HWND) {
    let _ = windows::Win32::UI::WindowsAndMessaging::EnumChildWindows(
        parent,
        Some(enum_child),
        LPARAM(0),
    );
}

unsafe extern "system" fn enum_child(
    child: HWND,
    _data: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    let mut class_name = [0u16; 32];
    let length = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(child, &mut class_name);
    let class = String::from_utf16_lossy(&class_name[..length as usize]);
    if !class.eq_ignore_ascii_case("button") {
        return true.into();
    }

    let style = windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
        child,
        windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
    ) as u32;

    let kind = style & BS_TYPEMASK;
    if matches!(
        kind,
        BS_AUTOCHECKBOX_STYLE
            | BS_CHECKBOX_STYLE
            | BS_AUTORADIOBUTTON_STYLE
            | BS_RADIOBUTTON_STYLE
            | BS_AUTO3STATE_STYLE
    ) {
        // 原始类型存进控件的 USERDATA，绘制和点击切换时都要用。
        // 同时把系统当前的勾选状态一起打包进去：有些窗口在套用主题之前
        // 就用 BM_SETCHECK 设置过默认选中项（例如黑白名单的单选框），
        // 这里要把它带过来，否则转换后会显示成未选中。
        let native_state = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            child,
            windows::Win32::UI::WindowsAndMessaging::BM_GETCHECK,
            WPARAM(0),
            LPARAM(0),
        )
        .0 as u32
            & 0x3;
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            child,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            (kind | (native_state << STATE_SHIFT)) as isize,
        );
        // 把类型位换成 BS_OWNERDRAW，保留 WS_* 等其余样式位。
        let new_style = (style & !BS_TYPEMASK) | BS_OWNERDRAW_STYLE;
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongW(
            child,
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
            new_style as i32,
        );
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(child, Some(flat_checkbox), 1, 0);
        // 子类化之后立刻要求整块重画一次，把控件创建时系统画的旧外观覆盖掉。
        let _ = InvalidateRect(child, None, true);
    }
    true.into()
}

/// 在父窗口的 `WM_COMMAND` 开头调用，补上 owner-draw 控件缺失的自动切换。
///
/// - 复选框：在「未选 / 选中」之间切换
/// - 三态复选框：0 → 1 → 2 循环
/// - 单选框：选中自己，并取消同一组内其它单选框
///
/// 只处理由 [`style_children`] 改造过的控件，其它按钮（普通按钮、菜单命令）
/// 会原样放行，因此可以放心地在所有窗口里统一调用。
pub unsafe fn apply_click_state(wparam: WPARAM, lparam: LPARAM) {
    // 只处理 BN_CLICKED。
    let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;
    if notification != windows::Win32::UI::WindowsAndMessaging::BN_CLICKED as u32 {
        return;
    }

    let control = HWND(lparam.0 as *mut c_void);
    if control.0.is_null() {
        return;
    }
    let parent =
        windows::Win32::UI::WindowsAndMessaging::GetParent(control).unwrap_or_default();
    if parent.0.is_null() {
        return;
    }
    let kind = stored_kind(control);
    if kind == 0 {
        // 不是我们改造过的控件。
        return;
    }

    match kind {
        BS_AUTORADIOBUTTON_STYLE | BS_RADIOBUTTON_STYLE => {
            store_state(control, 1);
            uncheck_other_radios(parent, control);
        }
        BS_AUTO3STATE_STYLE => {
            let state = stored_state(control);
            store_state(control, (state + 1) % 3);
        }
        _ => {
            let state = stored_state(control);
            store_state(control, if state == 1 { 0 } else { 1 });
        }
    }

    let _ = InvalidateRect(control, None, true);
}

/// 读取一个复选框 / 单选框当前是否被选中。
///
/// `BS_OWNERDRAW` 按钮不支持 `BM_GETCHECK`，所以状态由主题自己保管在
/// 控件的 USERDATA 里；业务代码统一用这个函数读取。
pub unsafe fn is_checked(control: HWND) -> bool {
    stored_state(control) != 0
}

/// 设置复选框 / 单选框的选中状态，并触发重画。
pub unsafe fn set_checked(control: HWND, checked: bool) {
    if stored_kind(control) == 0 {
        // 没被主题改造过的普通按钮，保持系统原有行为。
        let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            control,
            windows::Win32::UI::WindowsAndMessaging::BM_SETCHECK,
            WPARAM(usize::from(checked)),
            LPARAM(0),
        );
        return;
    }
    store_state(control, u32::from(checked));
    let _ = InvalidateRect(control, None, true);
}

/// 这个控件是否是主题接管的自绘复选框 / 单选框。
pub unsafe fn is_themed_control(control: HWND) -> bool {
    stored_kind(control) != 0
}

// USERDATA 打包格式：低 4 位是原始类型，第 8~9 位是勾选状态（0/1/2）。
const STATE_SHIFT: u32 = 8;

unsafe fn stored_kind(control: HWND) -> u32 {
    let packed = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
        control,
        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
    ) as u32;
    packed & BS_TYPEMASK
}

unsafe fn stored_state(control: HWND) -> u32 {
    let packed = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
        control,
        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
    ) as u32;
    (packed >> STATE_SHIFT) & 0x3
}

unsafe fn store_state(control: HWND, state: u32) {
    let kind = stored_kind(control);
    let packed = kind | ((state & 0x3) << STATE_SHIFT);
    windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
        control,
        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        packed as isize,
    );
}

/// 取消同一组里其它单选框的勾选。
///
/// 分组规则和系统一致：从当前控件往前找到最近的 `WS_GROUP` 作为组起点，
/// 往后遇到下一个 `WS_GROUP` 就结束。
unsafe fn uncheck_other_radios(parent: HWND, control: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, GWL_STYLE, GW_CHILD, GW_HWNDNEXT, WS_GROUP,
    };

    // 按 Z 序（也就是创建顺序）收集所有兄弟控件。
    let mut siblings: Vec<HWND> = Vec::new();
    let mut child =
        windows::Win32::UI::WindowsAndMessaging::GetWindow(parent, GW_CHILD).unwrap_or_default();
    while !child.0.is_null() {
        siblings.push(child);
        child = windows::Win32::UI::WindowsAndMessaging::GetWindow(child, GW_HWNDNEXT)
            .unwrap_or_default();
    }
    let Some(index) = siblings.iter().position(|handle| *handle == control) else {
        return;
    };

    // 组起点：往前找最近的 WS_GROUP 控件。
    // WS_GROUP 标记的是“组从我开始”，所以这个带 WS_GROUP 的控件本身
    // 也属于本组，找到后要把 start 指到它，而不是停在它的下一个。
    let mut start = index;
    while start > 0 {
        start -= 1;
        let previous_style = GetWindowLongW(siblings[start], GWL_STYLE) as u32;
        if previous_style & WS_GROUP.0 != 0 {
            break;
        }
    }

    // 组终点：往后直到遇到带 WS_GROUP 的控件。
    let mut end = index + 1;
    while end < siblings.len() {
        let style = GetWindowLongW(siblings[end], GWL_STYLE) as u32;
        if style & WS_GROUP.0 != 0 {
            break;
        }
        end += 1;
    }

    for sibling in &siblings[start..end] {
        if *sibling == control {
            continue;
        }
        let kind = stored_kind(*sibling);
        if matches!(kind, BS_AUTORADIOBUTTON_STYLE | BS_RADIOBUTTON_STYLE) {
            store_state(*sibling, 0);
            let _ = InvalidateRect(*sibling, None, true);
        }
    }
}

/// 控件子类过程：只接管绘制，其余消息全部交回系统处理。
unsafe extern "system" fn flat_checkbox(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            // 关键修复：系统有时只让控件重画一小块（例如只重画文字焦点框），
            // BeginPaint 返回的 DC 会被“更新区域”裁剪，没覆盖到的地方就会
            // 残留系统之前画的原生勾选框，看起来像新旧图案叠在一起。
            // 这里改用 GetDC 拿到完整客户区 DC，每次整块重画，彻底盖掉旧像素。
            let full = GetDC(hwnd);
            if !full.is_invalid() {
                draw_checkbox(hwnd, full);
                let _ = ReleaseDC(hwnd, full);
            } else {
                draw_checkbox(hwnd, paint.hdc);
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_PRINTCLIENT => {
            // 打印、截图等场景也会走这条消息，同样按新样式绘制。
            draw_checkbox(hwnd, HDC(wparam.0 as *mut c_void));
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => windows::Win32::UI::Shell::DefSubclassProc(hwnd, message, wparam, lparam),
    }
}

/// 画一个勾选框 / 单选框。
unsafe fn draw_checkbox(hwnd: HWND, device: HDC) {
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    fill(device, &client, BG);

    // 读取控件当前状态（owner-draw 按钮不能用 BM_GETCHECK，从 USERDATA 读）。
    let checked = is_checked(hwnd);
    let enabled = windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(hwnd).as_bool();

    // 控件已经被改成 BS_OWNERDRAW，类型位不再是复选框/单选框，
    // 所以从 USERDATA 里取回改造前保存的原始类型。
    let stored = stored_kind(hwnd);
    let kind = if stored != 0 {
        stored
    } else {
        windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
        ) as u32
            & BS_TYPEMASK
    };
    let is_radio = matches!(
        kind,
        BS_AUTORADIOBUTTON_STYLE | BS_RADIOBUTTON_STYLE
    );

    // 方块尺寸固定 15×15，垂直居中。
    let box_size = 15;
    let top = client.top + (client.bottom - client.top - box_size) / 2;
    let box_rect = RECT {
        left: client.left + 2,
        top,
        right: client.left + 2 + box_size,
        bottom: top + box_size,
    };

    if is_radio {
        fill(device, &box_rect, if checked { ACCENT } else { BG });
        frame(device, &box_rect, if checked { ACCENT } else { BORDER });
        if checked {
            // 单选框用“实心内块”表示选中，比小圆点更符合几何风格。
            fill(
                device,
                &RECT {
                    left: box_rect.left + 4,
                    top: box_rect.top + 4,
                    right: box_rect.right - 4,
                    bottom: box_rect.bottom - 4,
                },
                BG,
            );
        }
    } else {
        fill(device, &box_rect, if checked { ACCENT } else { BG });
        frame(device, &box_rect, if checked { ACCENT } else { BORDER });
        if checked {
            // 用两条 2 像素线段画对勾，比拼接色块更清晰、更接近几何图标。
            let pen = windows::Win32::Graphics::Gdi::CreatePen(
                windows::Win32::Graphics::Gdi::PS_SOLID,
                2,
                BG,
            );
            let previous_pen = SelectObject(device, HGDIOBJ(pen.0));
            let _ = windows::Win32::Graphics::Gdi::MoveToEx(
                device,
                box_rect.left + 3,
                box_rect.top + 8,
                None,
            );
            let _ =
                windows::Win32::Graphics::Gdi::LineTo(device, box_rect.left + 6, box_rect.top + 11);
            let _ =
                windows::Win32::Graphics::Gdi::LineTo(device, box_rect.left + 12, box_rect.top + 4);
            SelectObject(device, previous_pen);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
    }

    // 文字。
    let mut text_rect = RECT {
        left: box_rect.right + 7,
        top: client.top,
        right: client.right,
        bottom: client.bottom,
    };
    draw_text(
        device,
        &control_text(hwnd),
        &mut text_rect,
        DT_LEFT.0 | DT_SINGLELINE.0 | DT_VCENTER.0,
        if enabled { TEXT } else { FAINT },
        body_font(),
    );
}

/// 读取控件自身的窗口文字。
unsafe fn control_text(hwnd: HWND) -> String {
    let mut buffer = [0u16; 160];
    let length = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buffer);
    String::from_utf16_lossy(&buffer[..length as usize])
}

// ===== 列表表头 =====
//
// 系统表头是灰色 3D 凸起，和几何风格不搭。这里在控件创建完成后
// 给表头装上子类，接管它的 WM_PAINT：
// - 整条表头铺浅灰面板色，底部一条细线
// - 每一列用深色加粗文字，列之间画淡色分隔线
// - 排序方向改用强调色小三角，和整体配色一致
//
// 点击列头排序、拖动列宽等行为仍由系统处理，我们只改外观。

/// 给 ListView 的表头换上几何风格外观。
///
/// 在列插入完成之后调用一次即可。
pub unsafe fn style_list_header(list: HWND) {
    let header = SendMessageW(list, LVM_GETHEADER, WPARAM(0), LPARAM(0));
    let header = HWND(header.0 as *mut c_void);
    if header.0.is_null() {
        return;
    }
    let _ = windows::Win32::UI::Shell::SetWindowSubclass(header, Some(flat_header), 2, 0);
    let _ = InvalidateRect(header, None, true);
}

/// 表头子类过程：接管绘制，其余消息交回系统。
unsafe extern "system" fn flat_header(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match message {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let full = GetDC(hwnd);
            if !full.is_invalid() {
                draw_header(hwnd, full);
                let _ = ReleaseDC(hwnd, full);
            } else {
                draw_header(hwnd, paint.hdc);
            }
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_PRINTCLIENT => {
            draw_header(hwnd, HDC(wparam.0 as *mut c_void));
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => windows::Win32::UI::Shell::DefSubclassProc(hwnd, message, wparam, lparam),
    }
}

/// 绘制整条表头。
unsafe fn draw_header(hwnd: HWND, device: HDC) {
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    // 先铺满面板色：最后一列右侧的空白区域也保持同一种颜色。
    fill(device, &client, PANEL);

    let count = SendMessageW(hwnd, HDM_GETITEMCOUNT, WPARAM(0), LPARAM(0)).0;
    for index in 0..count {
        // 查询每一列的矩形（表头客户区坐标）。
        let mut rect = RECT::default();
        let ok = SendMessageW(
            hwnd,
            HDM_GETITEMRECT,
            WPARAM(index as usize),
            LPARAM(&mut rect as *mut _ as isize),
        );
        if ok.0 == 0 {
            continue;
        }

        // 列之间的淡色分隔线，两端留一点空隙，避免看起来像表格线。
        fill(
            device,
            &RECT {
                left: rect.right - 1,
                top: rect.top + 5,
                right: rect.right,
                bottom: rect.bottom - 5,
            },
            BORDER_SOFT,
        );

        // 读取列标题文字。
        let mut buffer = [0u16; 128];
        let mut item = HDITEMW::default();
        item.mask = HDI_TEXT;
        item.pszText = PWSTR(buffer.as_mut_ptr());
        item.cchTextMax = buffer.len() as i32;
        let has_text = SendMessageW(
            hwnd,
            HDM_GETITEMW,
            WPARAM(index as usize),
            LPARAM(&mut item as *mut _ as isize),
        )
        .0 != 0;
        let length = if has_text {
            buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len())
        } else {
            0
        };
        let label = String::from_utf16_lossy(&buffer[..length]);

        // 读取排序方向（如果有）。
        let mut format_item = HDITEMW::default();
        format_item.mask = HDI_FORMAT;
        let has_format = SendMessageW(
            hwnd,
            HDM_GETITEMW,
            WPARAM(index as usize),
            LPARAM(&mut format_item as *mut _ as isize),
        )
        .0 != 0;
        let sort_up = has_format && format_item.fmt.0 & HDF_SORTUP.0 != 0;
        let sort_down = has_format && format_item.fmt.0 & HDF_SORTDOWN.0 != 0;

        // 右侧留 18 像素给排序三角。
        let mut text_rect = RECT {
            left: rect.left + 8,
            top: rect.top,
            right: rect.right - 18,
            bottom: rect.bottom,
        };
        draw_text(
            device,
            &label,
            &mut text_rect,
            DT_LEFT.0 | DT_SINGLELINE.0 | DT_VCENTER.0,
            TEXT,
            bold_font(),
        );

        if sort_up || sort_down {
            draw_sort_arrow(device, &rect, sort_up);
        }
    }

    // 底部一条细线，把表头和列表内容分开。
    fill(
        device,
        &RECT {
            left: client.left,
            top: client.bottom - 1,
            right: client.right,
            bottom: client.bottom,
        },
        BORDER,
    );
}

/// 在表头列右侧画一个 8×5 的排序三角（向上 = 升序）。
unsafe fn draw_sort_arrow(device: HDC, column: &RECT, up: bool) {
    let center_x = column.right - 11;
    let center_y = (column.top + column.bottom) / 2;
    // 逐行画越来越宽（或越来越窄）的色条，拼成三角形。
    for row in 0..5 {
        // 升序：上窄下宽（▲）；降序：上宽下窄（▼）。
        let half = if up { row + 1 } else { 5 - row };
        let top = center_y - 2 + row;
        fill(
            device,
            &RECT {
                left: center_x - half,
                top,
                right: center_x + half,
                bottom: top + 1,
            },
            ACCENT,
        );
    }
}
