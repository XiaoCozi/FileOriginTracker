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

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    HGDIOBJ, HDC, HFONT, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DRAW_TEXT_FORMAT, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, WM_ERASEBKGND, WM_PAINT,
};
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;

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
pub unsafe fn draw_button(info: &DRAWITEMSTRUCT, label: &str, primary: bool) {    let rect = info.rcItem;
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
    let length = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(info.hwndItem, &mut buffer);
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
const BS_AUTORADIOBUTTON_STYLE: u32 = 5;
const BS_CHECKBOX_STYLE: u32 = 2;
const BS_RADIOBUTTON_STYLE: u32 = 4;
const BS_AUTO3STATE_STYLE: u32 = 7;
/// `BS_PUSHLIKE`：名单窗口里被画成按钮样子的单选框，保持系统外观即可。
const BS_PUSHLIKE_STYLE: u32 = 0x1000;

/// 给窗口里的所有复选框 / 单选框装上几何风格外观。
///
/// 在 `create_controls` 末尾调用一次即可；带有 `BS_PUSHLIKE` 的
/// 按钮式单选框会被跳过（那类控件本身就应该是按钮形状）。
pub unsafe fn style_children(parent: HWND) {
    let _ = windows::Win32::UI::WindowsAndMessaging::EnumChildWindows(
        parent,
        Some(enum_child),
        LPARAM(0),
    );
}

unsafe extern "system" fn enum_child(child: HWND, _data: LPARAM) -> windows::Win32::Foundation::BOOL {
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
    if style & BS_PUSHLIKE_STYLE != 0 {
        return true.into();
    }

    let kind = style & BS_TYPEMASK;
    if matches!(
        kind,
        BS_AUTOCHECKBOX_STYLE
            | BS_CHECKBOX_STYLE
            | BS_AUTORADIOBUTTON_STYLE
            | BS_RADIOBUTTON_STYLE
            | BS_AUTO3STATE_STYLE
    ) {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(child, Some(flat_checkbox), 1, 0);
    }
    true.into()
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
            let device = BeginPaint(hwnd, &mut paint);
            draw_checkbox(hwnd, device);
            let _ = EndPaint(hwnd, &paint);
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

    // 读取控件当前状态。
    let style = windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
    ) as u32;
    let checked = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::BM_GETCHECK,
        WPARAM(0),
        LPARAM(0),
    )
    .0
        == 1;
    let enabled = windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(hwnd).as_bool();
    let is_radio = matches!(style & BS_TYPEMASK, BS_AUTORADIOBUTTON_STYLE | BS_RADIOBUTTON_STYLE);

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
            let _ = windows::Win32::Graphics::Gdi::LineTo(
                device,
                box_rect.left + 6,
                box_rect.top + 11,
            );
            let _ = windows::Win32::Graphics::Gdi::LineTo(
                device,
                box_rect.left + 12,
                box_rect.top + 4,
            );
            SelectObject(device, previous_pen);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
    }

    // 文字。
    let mut buffer = [0u16; 160];
    let length = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buffer);
    let label = String::from_utf16_lossy(&buffer[..length as usize]);
    let mut text_rect = RECT {
        left: box_rect.right + 7,
        top: client.top,
        right: client.right,
        bottom: client.bottom,
    };
    draw_text(
        device,
        &label,
        &mut text_rect,
        DT_LEFT.0 | DT_SINGLELINE.0 | DT_VCENTER.0,
        if enabled { TEXT } else { FAINT },
        body_font(),
    );
}
