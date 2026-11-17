//! 任务栏托盘右键/左键弹出面板（Fluent 11 原生风格 · Direct2D + DirectWrite 分层浮窗）。
//!
//! 取代原本的 Win95 `CreatePopupMenu`：
//!   · 主屏只放高频项：状态头 / 说话热键 / 5 档视觉反馈+情境色板 / 常用开关 / 底部撤回与退出；
//!   · 低频项（排版细节、静音断句、维护排障）收进面板内折叠抽屉，零两级级联悬停菜单；
//!   · 所有操作直接转发给 `tray::trigger_action(id)`，隐藏命令宿主 `OuuutputTrayWnd` 的
//!     `WM_COMMAND`（1..384）继续 100% 兼容既有回归脚本与单实例退出逻辑。

use std::cell::RefCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use crate::tray::{
    self, EDGE_FPS_CHOICES, HALO_OPACITY_NAMES,
    HALO_SPEED_NAMES, HALO_SPREAD_SHORT, HOTKEY_CANDIDATES, ID_AUTOSTART, ID_CTX_BIAS,
    ID_CTX_VERBOSE, ID_EDGE_FPS_BASE, ID_EDGE_PEAK_BASE, ID_EDGE_SPEED_BASE, ID_EDGE_SPREAD_BASE,
    ID_EDGE_TINT_BASE, ID_EXIT, ID_FIX_CASE, ID_HALO_OPACITY_BASE, ID_HALO_SPEED_BASE,
    ID_HALO_SPREAD_BASE, ID_HALO_TINT_BASE, ID_HOTKEY_BASE, ID_LEADING_SPACE, ID_MERGE_TERMS,
    ID_ORB_RESET, ID_ORB_SIZE_BASE, ID_OVERLAY_BASE, ID_PUNCT_SPACE, ID_RELEASE_KEYS,
    ID_RESTART, ID_RING_SENS_BASE, ID_RING_SIZE_BASE, ID_SHOW_CONSOLE, ID_SILENCE_BASE,
    ID_TRAILING_SPACE, ID_UNDO_LAST, INDICATOR_SIZE_NAMES, RING_SENS_NAMES, SILENCE_CHOICES,
    TrayFlags,
};

pub const MENU_CLASS: &str = "OuuutputTrayMenuWnd";

const WM_DESTROY: u32 = 0x0002;
const WM_ACTIVATE: u32 = 0x0006;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_KEYDOWN: u32 = 0x0100;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_MENU_REFRESH: u32 = 0x0400 + 120;

const WA_INACTIVE: usize = 0;
const VK_ESCAPE: usize = 0x1B;
const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const HWND_TOPMOST: isize = -1;
const SWP_NOACTIVATE: u32 = 0x0010;
const SPI_GETWORKAREA: u32 = 0x0030;
const TME_LEAVE: u32 = 0x0000_0002;
const ULW_ALPHA: u32 = 2;

const PANEL_W_DP: f32 = 344.0;
const SHADOW_PAD_DP: f32 = 16.0;
const PANEL_PAD_DP: f32 = 12.0;

static MENU_HWND: AtomicIsize = AtomicIsize::new(0);
static MENU_VISIBLE: AtomicBool = AtomicBool::new(false);
static LAST_CLOSE_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ------------------------------------------------------------------ Win32 FFI

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Size {
    cx: i32,
    cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RectI {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BlendFunction {
    blend_op: u8,
    blend_flags: u8,
    source_constant_alpha: u8,
    alpha_format: u8,
}

#[repr(C)]
struct PaintStruct {
    hdc: isize,
    f_erase: i32,
    rc_paint: RectI,
    f_restore: i32,
    f_inc_update: i32,
    rgb_reserved: [u8; 32],
}

#[repr(C)]
struct TrackMouseEventBuf {
    cb_size: u32,
    dw_flags: u32,
    hwnd_track: isize,
    dw_hover_time: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels: i32,
    bi_y_pels: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 3],
}

type WndProc = unsafe extern "system" fn(isize, u32, usize, isize) -> isize;

#[repr(C)]
struct WndClassW {
    style: u32,
    lpfn_wnd_proc: Option<WndProc>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: isize,
    h_icon: isize,
    h_cursor: isize,
    hbr_background: isize,
    menu_name: *const u16,
    class_name: *const u16,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(lpwc: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: isize,
        menu: isize,
        inst: isize,
        param: isize,
    ) -> isize;
    fn DestroyWindow(hwnd: isize) -> i32;
    fn DefWindowProcW(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn SetWindowPos(
        hwnd: isize,
        after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn SetFocus(hwnd: isize) -> isize;
    fn GetCursorPos(pt: *mut Point) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wp: usize, lp: isize) -> i32;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn BeginPaint(hwnd: isize, ps: *mut PaintStruct) -> isize;
    fn EndPaint(hwnd: isize, ps: *const PaintStruct) -> i32;
    fn SystemParametersInfoW(ui_action: u32, ui_param: u32, pv_param: *mut core::ffi::c_void, f_win_ini: u32) -> i32;
    fn GetDpiForWindow(hwnd: isize) -> u32;
    fn TrackMouseEvent(lp_event_track: *mut TrackMouseEventBuf) -> i32;
    fn UpdateLayeredWindow(
        hwnd: isize,
        hdc_dst: isize,
        ppt_dst: *mut Point,
        psize: *mut Size,
        hdc_src: isize,
        ppt_src: *mut Point,
        cr_key: u32,
        pblend: *const BlendFunction,
        dw_flags: u32,
    ) -> i32;
    fn LoadCursorW(inst: isize, name: usize) -> isize;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn DeleteDC(hdc: isize) -> i32;
    fn SelectObject(hdc: isize, obj: isize) -> isize;
    fn DeleteObject(obj: isize) -> i32;
    fn CreateDIBSection(
        hdc: isize,
        bmi: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut u8,
        section: isize,
        offset: u32,
    ) -> isize;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(key: isize, sub: *const u16, opt: u32, sam: u32, out: *mut isize) -> i32;
    fn RegQueryValueExW(
        key: isize,
        name: *const u16,
        reserved: *mut u32,
        ty: *mut u32,
        data: *mut u8,
        bytes: *mut u32,
    ) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 读取 Windows 个性化「应用浅/深色」开关（0 = 深色，其余 = 浅色）。
pub fn system_is_dark() -> bool {
    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const KEY_QUERY_VALUE: u32 = 0x0001;
    unsafe {
        let sub = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        let mut h: isize = 0;
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_QUERY_VALUE, &mut h) != 0 {
            return false;
        }
        let name = wide("AppsUseLightTheme");
        let mut ty: u32 = 0;
        let mut val: u32 = 1;
        let mut bytes: u32 = 4;
        let rc = RegQueryValueExW(
            h,
            name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            (&mut val as *mut u32).cast(),
            &mut bytes,
        );
        RegCloseKey(h);
        rc == 0 && val == 0
    }
}

fn now_ms() -> u64 {
    use std::sync::LazyLock;
    use std::time::Instant;
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    START.elapsed().as_millis() as u64 + 10_000
}

// ------------------------------------------------------------------ 交互数据模型与纯布局

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drawer {
    Hotkey,
    Typography,
    Tools,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    ToggleDrawer(Drawer),
    CommandKeepOpen(usize),
    CommandClose(usize),
    CycleEdgeSpread,
    CycleEdgePeak,
    CycleEdgeSpeed,
    CycleEdgeFps,
    CycleHaloSpread,
    CycleHaloSpeed,
    CycleHaloOpacity,
}
#[derive(Clone, Copy, Debug)]
struct RectF {
    l: f32,
    t: f32,
    r: f32,
    b: f32,
}

impl RectF {
    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.l && x <= self.r && y >= self.t && y <= self.b
    }
}

#[derive(Clone, Debug)]
enum WidgetKind {
    CardBg,
    Divider,
    BrandBadge,
    StatusDot,
    Text {
        text: String,
        size: f32,
        bold: bool,
        tone: Tone,
        align: Align,
    },
    VersionPill(String),
    PillButton {
        text: String,
        active: bool,
    },
    Chip {
        text: String,
        on: bool,
    },
    Segment {
        text: String,
        on: bool,
        group_bg: bool,
    },
    Swatch {
        grad: SwatchStyle,
        on: bool,
    },
    MiniCycleBtn(String),
    ToggleRow {
        title: String,
        subtitle: Option<String>,
        on: bool,
    },
    AccordionHeader {
        title: String,
        right: String,
    },
    ToolBtn {
        text: String,
    },
    FooterBtn {
        text: String,
        danger: bool,
    },
}

#[derive(Clone, Copy, Debug)]
enum Tone {
    Ink,
    Muted,
}

#[derive(Clone, Copy, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug)]
enum SwatchStyle {
    Halo(usize),
    #[allow(dead_code)]
    Edge(usize),
}

#[derive(Clone, Debug)]
struct Widget {
    rect: RectF,
    kind: WidgetKind,
    action: Option<Action>,
}

#[derive(Clone, Debug)]
struct Layout {
    panel_w: f32,
    panel_h: f32,
    widgets: Vec<Widget>,
}

/// 纯函数布局引擎（单位：dp，原点 = 面板左上角；渲染时统一平移 `SHADOW_PAD_DP` 并乘 `scale`）。
fn build_layout(f: &TrayFlags, open: Option<Drawer>, title_ver: &str) -> Layout {
    let mut w: Vec<Widget> = Vec::with_capacity(64);
    let inner_l = PANEL_PAD_DP;
    let inner_r = PANEL_W_DP - PANEL_PAD_DP;
    let inner_w = inner_r - inner_l;
    let mut y = PANEL_PAD_DP;

    // 1) 状态头（Header）
    w.push(Widget {
        rect: RectF { l: inner_l + 2.0, t: y + 4.0, r: inner_l + 30.0, b: y + 32.0 },
        kind: WidgetKind::BrandBadge,
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 40.0, t: y + 2.0, r: inner_r - 58.0, b: y + 19.0 },
        kind: WidgetKind::Text {
            text: "ouuutput 语音输入".into(),
            size: 13.5,
            bold: true,
            tone: Tone::Ink,
            align: Align::Left,
        },
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 40.0, t: y + 22.0, r: inner_l + 46.0, b: y + 28.0 },
        kind: WidgetKind::StatusDot,
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 51.0, t: y + 18.0, r: inner_r - 56.0, b: y + 33.0 },
        kind: WidgetKind::Text {
            text: format!("已就绪 · 按住 {} 说话", tray::key_label_of(f.hotkey)),
            size: 11.2,
            bold: false,
            tone: Tone::Muted,
            align: Align::Left,
        },
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_r - 52.0, t: y + 7.0, r: inner_r - 2.0, b: y + 27.0 },
        kind: WidgetKind::VersionPill(title_ver.to_string()),
        action: None,
    });
    y += 40.0;
    w.push(Widget {
        rect: RectF { l: inner_l, t: y, r: inner_r, b: y + 1.0 },
        kind: WidgetKind::Divider,
        action: None,
    });
    y += 10.0;

    // 2) 说话快捷键卡片
    let hk_open = open == Some(Drawer::Hotkey);
    let hk_card_top = y;
    let hk_rows = if hk_open { 2.0 } else { 0.0 };
    let hk_card_h = if hk_open { 50.0 + hk_rows * 30.0 + 6.0 } else { 48.0 };
    w.push(Widget {
        rect: RectF { l: inner_l, t: hk_card_top, r: inner_r, b: hk_card_top + hk_card_h },
        kind: WidgetKind::CardBg,
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 10.0, t: y + 7.0, r: inner_r - 122.0, b: y + 24.0 },
        kind: WidgetKind::Text {
            text: "说话快捷键".into(),
            size: 12.5,
            bold: true,
            tone: Tone::Ink,
            align: Align::Left,
        },
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 10.0, t: y + 24.0, r: inner_r - 122.0, b: y + 40.0 },
        kind: WidgetKind::Text {
            text: "点击切换按住触发键".into(),
            size: 11.0,
            bold: false,
            tone: Tone::Muted,
            align: Align::Left,
        },
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_r - 116.0, t: y + 10.0, r: inner_r - 10.0, b: y + 38.0 },
        kind: WidgetKind::PillButton {
            text: format!("{} {}", tray::key_label_of(f.hotkey), if hk_open { "▴" } else { "▾" }),
            active: hk_open,
        },
        action: Some(Action::ToggleDrawer(Drawer::Hotkey)),
    });
    if hk_open {
        let dy = y + 48.0;
        w.push(Widget {
            rect: RectF { l: inner_l + 10.0, t: dy, r: inner_r - 10.0, b: dy + 1.0 },
            kind: WidgetKind::Divider,
            action: None,
        });
        let cols_row1 = 3usize;
        let gap = 6.0;
        let cw1 = (inner_w - 20.0 - gap * 2.0) / 3.0;
        for (i, (conf, label)) in HOTKEY_CANDIDATES.iter().enumerate() {
            let short = match *conf {
                "mousex1" => "鼠标侧键 X1",
                "mousex2" => "鼠标侧键 X2",
                _ => *label,
            };
            let (row, col, cw) = if i < cols_row1 {
                (0, i, cw1)
            } else {
                let cw2 = (inner_w - 20.0 - gap * 3.0) / 4.0;
                (1, i - cols_row1, cw2)
            };
            let cx = inner_l + 10.0 + col as f32 * (cw + gap);
            let cy = dy + 8.0 + row as f32 * 28.0;
            w.push(Widget {
                rect: RectF { l: cx, t: cy, r: cx + cw, b: cy + 23.0 },
                kind: WidgetKind::Chip {
                    text: short.into(),
                    on: *conf == f.hotkey,
                },
                action: Some(Action::CommandKeepOpen(ID_HOTKEY_BASE + i)),
            });
        }
    }
    y += hk_card_h + 8.0;

    // 3) 录音视觉反馈卡片（5 档分段 + 情境色板与实时调节胶囊）
    let halo_names = [
        "电光蓝",
        "天青水色",
        "蓝紫极光",
        "Siri 绚烂",
        "流光彩虹",
        "赛博霓虹",
        "翡翠幽光",
        "落日熔金",
    ];
    // 细条(1) 与 边缘光(3) 已按需求注释停用，只保留：关闭(0) / 声波(2) / 白环(5) / 流光(4)
    let has_ctx = matches!(f.overlay_mode, 2 | 4 | 5);
    let mode_card_top = y;
    let mode_card_h = match f.overlay_mode {
        4 => 150.0,
        2 => 144.0,
        5 => 144.0,
        _ => 68.0,
    };
    w.push(Widget {
        rect: RectF { l: inner_l, t: mode_card_top, r: inner_r, b: mode_card_top + mode_card_h },
        kind: WidgetKind::CardBg,
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 10.0, t: y + 8.0, r: inner_r - 116.0, b: y + 25.0 },
        kind: WidgetKind::Text {
            text: "录音视觉反馈".into(),
            size: 12.5,
            bold: true,
            tone: Tone::Ink,
            align: Align::Left,
        },
        action: None,
    });
    let orb_sz_name = INDICATOR_SIZE_NAMES.get(f.orb_size_idx).copied().unwrap_or("中");
    let ring_sz_name = INDICATOR_SIZE_NAMES.get(f.ring_size_idx).copied().unwrap_or("中");
    let ring_sens_name = RING_SENS_NAMES.get(f.ring_sens_idx).copied().unwrap_or("中");
    let right_hint = match f.overlay_mode {
        4 => halo_names.get(f.halo_tint).copied().unwrap_or("电光蓝").to_string(),
        2 => format!("{} · {orb_sz_name}", halo_names.get(f.halo_tint).copied().unwrap_or("电光蓝")),
        5 => format!("{ring_sz_name} · 灵敏{ring_sens_name}"),
        // 1 => "跟随字幕".to_string(),
        _ => "静默".to_string(),
    };
    w.push(Widget {
        rect: RectF { l: inner_r - 116.0, t: y + 8.0, r: inner_r - 10.0, b: y + 25.0 },
        kind: WidgetKind::Text {
            text: right_hint,
            size: 11.0,
            bold: false,
            tone: Tone::Muted,
            align: Align::Right,
        },
        action: None,
    });
    // Segmented strip container（4 档：关闭 / 声波 / 白环 / 流光）
    let seg_l = inner_l + 10.0;
    let seg_t = y + 30.0;
    let seg_w = inner_w - 20.0;
    let seg_h = 28.0;
    w.push(Widget {
        rect: RectF { l: seg_l, t: seg_t, r: seg_l + seg_w, b: seg_t + seg_h },
        kind: WidgetKind::Segment { text: String::new(), on: false, group_bg: true },
        action: None,
    });
    // let legacy_labels = ["关闭", "细条", "声波", "边缘光", "流光"]; // 已注释停用细条/边缘光
    let mode_items: [(usize, &str); 4] = [(0, "关闭"), (2, "声波"), (5, "白环"), (4, "流光")];
    let cell_w = (seg_w - 6.0) / mode_items.len() as f32;
    for (idx, (mode_id, lbl)) in mode_items.iter().enumerate() {
        let sl = seg_l + 3.0 + idx as f32 * cell_w;
        w.push(Widget {
            rect: RectF { l: sl, t: seg_t + 3.0, r: sl + cell_w, b: seg_t + seg_h - 3.0 },
            kind: WidgetKind::Segment {
                text: (*lbl).into(),
                on: f.overlay_mode == *mode_id,
                group_bg: false,
            },
            action: Some(Action::CommandKeepOpen(ID_OVERLAY_BASE + *mode_id)),
        });
    }
    if has_ctx {
        let cy = y + 66.0;
        w.push(Widget {
            rect: RectF { l: inner_l + 10.0, t: cy, r: inner_r - 10.0, b: cy + 1.0 },
            kind: WidgetKind::Divider,
            action: None,
        });
        if f.overlay_mode == 4 {
            let cur_name = halo_names.get(f.halo_tint).copied().unwrap_or("电光蓝");
            w.push(Widget {
                rect: RectF { l: inner_l + 10.0, t: cy + 5.0, r: inner_r - 10.0, b: cy + 21.0 },
                kind: WidgetKind::Text {
                    text: format!("流光配色 · {cur_name}"),
                    size: 11.2,
                    bold: false,
                    tone: Tone::Muted,
                    align: Align::Left,
                },
                action: None,
            });
            // 8 套流光色点（均匀铺满 300dp 宽度：step=39dp, 直径 22dp）
            for i in 0..8 {
                let sx = inner_l + 10.0 + i as f32 * 39.0;
                w.push(Widget {
                    rect: RectF { l: sx, t: cy + 24.0, r: sx + 22.0, b: cy + 46.0 },
                    kind: WidgetKind::Swatch {
                        grad: SwatchStyle::Halo(i),
                        on: f.halo_tint == i,
                    },
                    action: Some(Action::CommandKeepOpen(ID_HALO_TINT_BASE + i)),
                });
            }
            // 4 枚流光实时调参胶囊：范围 / 速度 / 浓淡 / 帧率
            let spread_s = HALO_SPREAD_SHORT.get(f.halo_spread_idx).copied().unwrap_or("标准");
            let speed_s = HALO_SPEED_NAMES.get(f.halo_speed_idx).copied().unwrap_or("中");
            let opac_s = HALO_OPACITY_NAMES.get(f.halo_opacity_idx).copied().unwrap_or("中");
            let fps_v = EDGE_FPS_CHOICES.get(f.edge_fps_idx).copied().unwrap_or(60);
            let halo_chips = [
                (format!("范围:{spread_s}"), Action::CycleHaloSpread, 80.0),
                (format!("流速:{speed_s}"), Action::CycleHaloSpeed, 68.0),
                (format!("浓淡:{opac_s}"), Action::CycleHaloOpacity, 68.0),
                (format!("{fps_v}Hz"), Action::CycleEdgeFps, 60.0),
            ];
            let mut lx = inner_l + 10.0;
            for (txt, act, cw) in halo_chips {
                w.push(Widget {
                    rect: RectF { l: lx, t: cy + 54.0, r: lx + cw, b: cy + 76.0 },
                    kind: WidgetKind::MiniCycleBtn(txt),
                    action: Some(act),
                });
                lx += cw + 8.0;
            }
        } else if f.overlay_mode == 2 {
            let cur_name = halo_names.get(f.halo_tint).copied().unwrap_or("电光蓝");
            w.push(Widget {
                rect: RectF { l: inner_l + 10.0, t: cy + 5.0, r: inner_r - 170.0, b: cy + 23.0 },
                kind: WidgetKind::Text {
                    text: format!("声波 · {cur_name}"),
                    size: 11.2,
                    bold: false,
                    tone: Tone::Muted,
                    align: Align::Left,
                },
                action: None,
            });
            for (si, slbl) in INDICATOR_SIZE_NAMES.iter().enumerate() {
                let pl = inner_r - 166.0 + si as f32 * 34.0;
                w.push(Widget {
                    rect: RectF { l: pl, t: cy + 4.0, r: pl + 30.0, b: cy + 24.0 },
                    kind: WidgetKind::PillButton {
                        text: (*slbl).into(),
                        active: f.orb_size_idx == si,
                    },
                    action: Some(Action::CommandKeepOpen(ID_ORB_SIZE_BASE + si)),
                });
            }
            w.push(Widget {
                rect: RectF { l: inner_r - 60.0, t: cy + 4.0, r: inner_r - 10.0, b: cy + 24.0 },
                kind: WidgetKind::MiniCycleBtn("归位".into()),
                action: Some(Action::CommandKeepOpen(ID_ORB_RESET)),
            });
            for i in 0..8 {
                let sx = inner_l + 10.0 + i as f32 * 39.0;
                w.push(Widget {
                    rect: RectF { l: sx, t: cy + 30.0, r: sx + 22.0, b: cy + 52.0 },
                    kind: WidgetKind::Swatch {
                        grad: SwatchStyle::Halo(i),
                        on: f.halo_tint == i,
                    },
                    action: Some(Action::CommandKeepOpen(ID_HALO_TINT_BASE + i)),
                });
            }
            w.push(Widget {
                rect: RectF { l: inner_l + 10.0, t: cy + 57.0, r: inner_r - 10.0, b: cy + 72.0 },
                kind: WidgetKind::Text {
                    text: "按住热键淡入 · 松开淡出 · 左键拖动可改位置".into(),
                    size: 10.4,
                    bold: false,
                    tone: Tone::Muted,
                    align: Align::Left,
                },
                action: None,
            });
        } else if f.overlay_mode == 5 {
            w.push(Widget {
                rect: RectF { l: inner_l + 10.0, t: cy + 5.0, r: inner_l + 56.0, b: cy + 23.0 },
                kind: WidgetKind::Text {
                    text: "大小".into(),
                    size: 11.2,
                    bold: false,
                    tone: Tone::Muted,
                    align: Align::Left,
                },
                action: None,
            });
            for (si, slbl) in INDICATOR_SIZE_NAMES.iter().enumerate() {
                let pl = inner_l + 60.0 + si as f32 * 48.0;
                w.push(Widget {
                    rect: RectF { l: pl, t: cy + 4.0, r: pl + 42.0, b: cy + 24.0 },
                    kind: WidgetKind::PillButton {
                        text: (*slbl).into(),
                        active: f.ring_size_idx == si,
                    },
                    action: Some(Action::CommandKeepOpen(ID_RING_SIZE_BASE + si)),
                });
            }
            w.push(Widget {
                rect: RectF { l: inner_r - 60.0, t: cy + 4.0, r: inner_r - 10.0, b: cy + 24.0 },
                kind: WidgetKind::MiniCycleBtn("归位".into()),
                action: Some(Action::CommandKeepOpen(ID_ORB_RESET)),
            });
            w.push(Widget {
                rect: RectF { l: inner_l + 10.0, t: cy + 31.0, r: inner_l + 56.0, b: cy + 49.0 },
                kind: WidgetKind::Text {
                    text: "灵敏度".into(),
                    size: 11.2,
                    bold: false,
                    tone: Tone::Muted,
                    align: Align::Left,
                },
                action: None,
            });
            for (si, slbl) in RING_SENS_NAMES.iter().enumerate() {
                let pl = inner_l + 60.0 + si as f32 * 48.0;
                w.push(Widget {
                    rect: RectF { l: pl, t: cy + 30.0, r: pl + 42.0, b: cy + 50.0 },
                    kind: WidgetKind::PillButton {
                        text: (*slbl).into(),
                        active: f.ring_sens_idx == si,
                    },
                    action: Some(Action::CommandKeepOpen(ID_RING_SENS_BASE + si)),
                });
            }
            w.push(Widget {
                rect: RectF { l: inner_l + 10.0, t: cy + 56.0, r: inner_r - 10.0, b: cy + 70.0 },
                kind: WidgetKind::Text {
                    text: "按住热键淡入 · 中心全透明 · 左键拖动可改位置".into(),
                    size: 10.4,
                    bold: false,
                    tone: Tone::Muted,
                    align: Align::Left,
                },
                action: None,
            });
        }
        // 边缘光模式（3）与细条模式（1）已按需求注释停用
    }
    y += mode_card_h + 8.0;

    // 4) 常用开关卡片（保留最日常的 2 项）
    let sw_card_h = 84.0;
    w.push(Widget {
        rect: RectF { l: inner_l, t: y, r: inner_r, b: y + sw_card_h },
        kind: WidgetKind::CardBg,
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 6.0, t: y + 4.0, r: inner_r - 6.0, b: y + 42.0 },
        kind: WidgetKind::ToggleRow {
            title: "上下文智能记忆".into(),
            subtitle: Some("自动识别刚刚输入过的专有名词".into()),
            on: f.ctx_bias,
        },
        action: Some(Action::CommandKeepOpen(ID_CTX_BIAS)),
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 6.0, t: y + 42.0, r: inner_r - 6.0, b: y + 80.0 },
        kind: WidgetKind::ToggleRow {
            title: "开机自动启动".into(),
            subtitle: Some("登录 Windows 后静默驻留托盘".into()),
            on: f.autostart,
        },
        action: Some(Action::CommandKeepOpen(ID_AUTOSTART)),
    });
    y += sw_card_h + 8.0;

    // 5) 排版与断句细节（折叠抽屉）
    let typo_open = open == Some(Drawer::Typography);
    let typo_h = if typo_open { 218.0 } else { 38.0 };
    w.push(Widget {
        rect: RectF { l: inner_l, t: y, r: inner_r, b: y + typo_h },
        kind: WidgetKind::CardBg,
        action: None,
    });
    let sil_str = if f.silence_sec == 0 {
        "不断句".to_string()
    } else {
        format!("{} 秒", f.silence_sec)
    };
    w.push(Widget {
        rect: RectF { l: inner_l + 6.0, t: y + 4.0, r: inner_r - 6.0, b: y + 34.0 },
        kind: WidgetKind::AccordionHeader {
            title: "排版与断句细节".into(),
            right: format!("{sil_str} · {}", if typo_open { "收起 ▴" } else { "展开 ▾" }),
        },
        action: Some(Action::ToggleDrawer(Drawer::Typography)),
    });
    if typo_open {
        let mut ty = y + 38.0;
        w.push(Widget {
            rect: RectF { l: inner_l + 10.0, t: ty, r: inner_r - 10.0, b: ty + 1.0 },
            kind: WidgetKind::Divider,
            action: None,
        });
        ty += 6.0;
        let items = [
            ("标点转空格（保留问号）", f.punct_to_space, ID_PUNCT_SPACE),
            ("英文拼写连写（R E A → REA）", f.merge_terms, ID_MERGE_TERMS),
            ("英文默认小写修正", f.fix_case, ID_FIX_CASE),
            ("前面自动加空格", f.leading_space, ID_LEADING_SPACE),
            ("后面自动加空格", f.trailing_space, ID_TRAILING_SPACE),
        ];
        for (lbl, on, cmd) in items {
            w.push(Widget {
                rect: RectF { l: inner_l + 6.0, t: ty, r: inner_r - 6.0, b: ty + 27.0 },
                kind: WidgetKind::ToggleRow {
                    title: lbl.into(),
                    subtitle: None,
                    on,
                },
                action: Some(Action::CommandKeepOpen(cmd)),
            });
            ty += 27.0;
        }
        ty += 4.0;
        w.push(Widget {
            rect: RectF { l: inner_l + 10.0, t: ty, r: inner_r - 10.0, b: ty + 1.0 },
            kind: WidgetKind::Divider,
            action: None,
        });
        ty += 7.0;
        w.push(Widget {
            rect: RectF { l: inner_l + 10.0, t: ty + 2.0, r: inner_l + 64.0, b: ty + 22.0 },
            kind: WidgetKind::Text {
                text: "静音断句".into(),
                size: 11.0,
                bold: false,
                tone: Tone::Muted,
                align: Align::Left,
            },
            action: None,
        });
        let cw = (inner_w - 76.0 - 16.0) / 5.0;
        for (i, sec) in SILENCE_CHOICES.iter().enumerate() {
            let cx = inner_l + 66.0 + i as f32 * (cw + 4.0);
            let lbl = if *sec == 0 { "不断句".to_string() } else { format!("{sec} 秒") };
            w.push(Widget {
                rect: RectF { l: cx, t: ty, r: cx + cw, b: ty + 23.0 },
                kind: WidgetKind::Chip {
                    text: lbl,
                    on: *sec == f.silence_sec,
                },
                action: Some(Action::CommandKeepOpen(ID_SILENCE_BASE + i)),
            });
        }
    }
    y += typo_h + 8.0;

    // 6) 维护与排障工具（折叠抽屉）
    let tools_open = open == Some(Drawer::Tools);
    let tools_h = if tools_open { 108.0 } else { 38.0 };
    w.push(Widget {
        rect: RectF { l: inner_l, t: y, r: inner_r, b: y + tools_h },
        kind: WidgetKind::CardBg,
        action: None,
    });
    w.push(Widget {
        rect: RectF { l: inner_l + 6.0, t: y + 4.0, r: inner_r - 6.0, b: y + 34.0 },
        kind: WidgetKind::AccordionHeader {
            title: "维护与排障工具".into(),
            right: if tools_open { "收起 ▴" } else { "展开 ▾" }.into(),
        },
        action: Some(Action::ToggleDrawer(Drawer::Tools)),
    });
    if tools_open {
        let ty = y + 38.0;
        w.push(Widget {
            rect: RectF { l: inner_l + 10.0, t: ty, r: inner_r - 10.0, b: ty + 1.0 },
            kind: WidgetKind::Divider,
            action: None,
        });
        let bw = (inner_w - 20.0 - 6.0) / 2.0;
        let btns = [
            ("打开运行日志", Action::CommandClose(ID_SHOW_CONSOLE)),
            ("释放卡住的键", Action::CommandClose(ID_RELEASE_KEYS)),
            (
                if f.ctx_verbose { "偏置提示：开" } else { "偏置提示：关" },
                Action::CommandKeepOpen(ID_CTX_VERBOSE),
            ),
            ("重启语音服务", Action::CommandClose(ID_RESTART)),
        ];
        for (i, (txt, act)) in btns.into_iter().enumerate() {
            let row = i / 2;
            let col = i % 2;
            let bx = inner_l + 10.0 + col as f32 * (bw + 6.0);
            let by = ty + 7.0 + row as f32 * 30.0;
            w.push(Widget {
                rect: RectF { l: bx, t: by, r: bx + bw, b: by + 25.0 },
                kind: WidgetKind::ToolBtn { text: txt.into() },
                action: Some(act),
            });
        }
    }
    y += tools_h + 10.0;

    // 7) 底部操作行：撤回上一句 + 退出
    let exit_w = 72.0;
    let undo_w = inner_w - exit_w - 8.0;
    w.push(Widget {
        rect: RectF { l: inner_l, t: y, r: inner_l + undo_w, b: y + 32.0 },
        kind: WidgetKind::FooterBtn {
            text: "撤回上一句".into(),
            danger: false,
        },
        action: Some(Action::CommandClose(ID_UNDO_LAST)),
    });
    w.push(Widget {
        rect: RectF { l: inner_r - exit_w, t: y, r: inner_r, b: y + 32.0 },
        kind: WidgetKind::FooterBtn {
            text: "退出".into(),
            danger: true,
        },
        action: Some(Action::CommandClose(ID_EXIT)),
    });
    y += 32.0 + PANEL_PAD_DP;

    Layout { panel_w: PANEL_W_DP, panel_h: y, widgets: w }
}

/// 将浮窗精确锚定在点击坐标附近（通常在右下角任务栏上方），并严格夹在工作区内。
pub fn anchor_in_work_area(
    cursor_x: i32,
    cursor_y: i32,
    win_w: i32,
    win_h: i32,
    work_l: i32,
    work_t: i32,
    work_r: i32,
    work_b: i32,
    pad: i32,
) -> (i32, i32) {
    let min_x = work_l + pad;
    let max_x = (work_r - win_w - pad).max(min_x);
    let min_y = work_t + pad;
    let max_y = (work_b - win_h - pad).max(min_y);

    // 默认在光标左上方一点展开（最贴合右下角任务栏通知区点击习惯）
    let mut x = cursor_x - win_w / 2;
    let near_bottom = cursor_y > (work_t + work_b) / 2;
    let mut y = if near_bottom {
        cursor_y - win_h + pad
    } else {
        cursor_y - pad
    };

    x = x.clamp(min_x, max_x);
    y = y.clamp(min_y, max_y);
    (x, y)
}

// ------------------------------------------------------------------ D2D / DIB 画布缓存（只属托盘线程）

struct Palette {
    panel: u32,
    panel_border: u32,
    card: u32,
    card_border: u32,
    hover: u32,
    seg_bg: u32,
    seg_active: u32,
    ink: u32,
    muted: u32,
    accent: u32,
    accent_ink: u32,
    accent_soft: u32,
    ok: u32,
    danger: u32,
    danger_soft: u32,
    switch_off: u32,
    shadow: u32,
    dark: bool,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            panel: 0xF722_2327,
            panel_border: 0x1EFFFFFF,
            card: 0xAE30_3238,
            card_border: 0x16FFFFFF,
            hover: 0x14FFFFFF,
            seg_bg: 0x2600_0000,
            seg_active: 0xFF39_3B42,
            ink: 0xFFF3_F4F6,
            muted: 0xFFA1_A8B5,
            accent: 0xFF60_CDFF,
            accent_ink: 0xFF07_1923,
            accent_soft: 0x2660_CDFF,
            ok: 0xFF6C_CB5F,
            danger: 0xFFFF_7B72,
            danger_soft: 0x24FF_7B72,
            switch_off: 0x59FF_FFFF,
            shadow: 0x6000_0000,
            dark: true,
        }
    } else {
        Palette {
            panel: 0xF5FC_FCFD,
            panel_border: 0x180F_172A,
            card: 0xD8FF_FFFF,
            card_border: 0x140F_172A,
            hover: 0x0B00_0000,
            seg_bg: 0x1C78_8296,
            seg_active: 0xFFFF_FFFF,
            ink: 0xFF11_1827,
            muted: 0xFF52_5866,
            accent: 0xFF00_67C0,
            accent_ink: 0xFFFF_FFFF,
            accent_soft: 0x1C00_67C0,
            ok: 0xFF0F_7B0F,
            danger: 0xFFC4_2B1C,
            danger_soft: 0x18C4_2B1C,
            switch_off: 0x5700_0000,
            shadow: 0x220F_172A,
            dark: false,
        }
    }
}

struct MenuCanvas {
    w: i32,
    h: i32,
    mem_dc: isize,
    dib: isize,
    old_bmp: isize,
}

impl MenuCanvas {
    unsafe fn create(w: i32, h: i32) -> Option<Self> {
        let bmi = BitmapInfo {
            header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: w,
                bi_height: -h,
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: 0,
                bi_size_image: (w * h * 4) as u32,
                ..Default::default()
            },
            colors: [0; 3],
        };
        let mem_dc = CreateCompatibleDC(0);
        if mem_dc == 0 {
            eprintln!("[ouuutput][tray_menu] CreateCompatibleDC failed: {}", std::io::Error::last_os_error());
            return None;
        }
        let mut bits: *mut u8 = ptr::null_mut();
        let dib = CreateDIBSection(mem_dc, &bmi, 0, &mut bits, 0, 0);
        if dib == 0 || bits.is_null() {
            eprintln!("[ouuutput][tray_menu] CreateDIBSection failed: {}", std::io::Error::last_os_error());
            DeleteDC(mem_dc);
            return None;
        }
        let old_bmp = SelectObject(mem_dc, dib);
        Some(Self { w, h, mem_dc, dib, old_bmp })
    }
}

impl Drop for MenuCanvas {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.old_bmp);
            DeleteObject(self.dib);
            DeleteDC(self.mem_dc);
        }
    }
}

struct MenuRt {
    _d2d_factory: windows::Win32::Graphics::Direct2D::ID2D1Factory,
    rt: windows::Win32::Graphics::Direct2D::ID2D1DCRenderTarget,
    target: windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    dw_factory: windows::Win32::Graphics::DirectWrite::IDWriteFactory,
    bound: Option<(isize, i32, i32)>,
}

struct MenuState {
    open: Option<Drawer>,
    hover_idx: Option<usize>,
    scale: f32,
    dark: bool,
    flags: TrayFlags,
    layout: Layout,
    canvas: Option<MenuCanvas>,
    rt: Option<MenuRt>,
    tracking_leave: bool,
    /// 记录打开抽屉前或点选时的底部基准，避免每次展开让底边飞出屏幕。
    anchor_pt: Point,
}

impl MenuState {
    fn new() -> Self {
        let flags = tray::snapshot_flags();
        let ver = concat!("v", env!("CARGO_PKG_VERSION"));
        let layout = build_layout(&flags, None, ver);
        Self {
            open: None,
            hover_idx: None,
            scale: 1.0,
            dark: system_is_dark(),
            flags,
            layout,
            canvas: None,
            rt: None,
            tracking_leave: false,
            anchor_pt: Point::default(),
        }
    }
}

thread_local! {
    static STATE: RefCell<MenuState> = RefCell::new(MenuState::new());
}

fn ensure_rt(st: &mut MenuState) -> bool {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
    };

    if st.rt.is_some() {
        return true;
    }
    unsafe {
        let d2d_factory = match d2d::D2D1CreateFactory::<d2d::ID2D1Factory>(
            d2d::D2D1_FACTORY_TYPE_SINGLE_THREADED,
            None,
        ) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[ouuutput][tray_menu] D2D1CreateFactory failed: {e}");
                return false;
            }
        };
        let props = d2d::D2D1_RENDER_TARGET_PROPERTIES {
            r#type: d2d::D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: d2dc::D2D1_PIXEL_FORMAT {
                format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: d2dc::D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            usage: d2d::D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: d2d::D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = match d2d_factory.CreateDCRenderTarget(&props) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[ouuutput][tray_menu] CreateDCRenderTarget failed: {e}");
                return false;
            }
        };
        let target: d2d::ID2D1RenderTarget = match rt.cast() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[ouuutput][tray_menu] cast ID2D1RenderTarget failed: {e}");
                return false;
            }
        };
        let dw_factory = match DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[ouuutput][tray_menu] DWriteCreateFactory failed: {e}");
                return false;
            }
        };
        st.rt = Some(MenuRt { _d2d_factory: d2d_factory, rt, target, dw_factory, bound: None });
        true
    }
}

fn argb(c: u32) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
    windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        r: ((c >> 16) & 0xFF) as f32 / 255.0,
        g: ((c >> 8) & 0xFF) as f32 / 255.0,
        b: (c & 0xFF) as f32 / 255.0,
        a: ((c >> 24) & 0xFF) as f32 / 255.0,
    }
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
    windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

unsafe fn solid_brush(
    target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    col: &windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
    ctx: &str,
) -> Option<windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush> {
    match target.CreateSolidColorBrush(col, None) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("[ouuutput][tray_menu] CreateSolidColorBrush({ctx}) failed: {e}");
            None
        }
    }
}

unsafe fn draw_text_line(
    target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    dw: &windows::Win32::Graphics::DirectWrite::IDWriteFactory,
    rect: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
    text: &str,
    px_size: f32,
    bold: bool,
    color_f: windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F,
    align: Align,
) {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_BOLD,
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
    };

    let face = wide("Microsoft YaHei UI");
    let locale = wide("zh-CN");
    let weight = if bold { DWRITE_FONT_WEIGHT_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
    let fmt = match dw.CreateTextFormat(
        PCWSTR(face.as_ptr()),
        None,
        weight,
        DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_STRETCH_NORMAL,
        px_size,
        PCWSTR(locale.as_ptr()),
    ) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[ouuutput][tray_menu] CreateTextFormat failed: {e}");
            return;
        }
    };
    if let Err(e) = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER) {
        eprintln!("[ouuutput][tray_menu] SetParagraphAlignment failed: {e}");
        return;
    }
    if let Err(e) = fmt.SetTextAlignment(match align {
        Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
        Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
        Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
    }) {
        eprintln!("[ouuutput][tray_menu] SetTextAlignment failed: {e}");
        return;
    }
    let brush = match target.CreateSolidColorBrush(&color_f, None) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[ouuutput][tray_menu] CreateSolidColorBrush failed: {e}");
            return;
        }
    };
    let wtxt = wide(text);
    target.DrawText(
        &wtxt[..wtxt.len().saturating_sub(1)],
        &fmt,
        &rect,
        &brush,
        d2d::D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );
}

unsafe fn draw_swatch(
    target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    cx: f32,
    cy: f32,
    r: f32,
    style: SwatchStyle,
    on: bool,
    pal: &Palette,
    s: f32,
) {
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;

    let (c0, c1, c2) = match style {
        SwatchStyle::Halo(0) => (0xFF38_BDF8, 0xFF00_7AFF, 0xFF1E_40AF),
        SwatchStyle::Halo(1) => (0xFF22_D3EE, 0xFF02_84C7, 0xFF03_69A1),
        SwatchStyle::Halo(2) => (0xFFC0_84FC, 0xFF3B_82F6, 0xFF1D_4ED8),
        SwatchStyle::Halo(3) => (0xFF38_BDF8, 0xFFF4_72B6, 0xFF63_66F1),
        SwatchStyle::Halo(4) => (0xFFF4_3F5E, 0xFF10_B981, 0xFF8B_5CF6),
        SwatchStyle::Halo(5) => (0xFF00_F0FF, 0xFFFF_1E8A, 0xFF4C_1D95),
        SwatchStyle::Halo(6) => (0xFF6E_E7B7, 0xFF10_B981, 0xFF06_5F46),
        SwatchStyle::Halo(_) => (0xFFFB_7185, 0xFFF9_7316, 0xFF9A_3412),
        SwatchStyle::Edge(0) => (0xFFFF_FFFF, 0xFFE2_E8F0, 0xFFCB_D5E1),
        SwatchStyle::Edge(1) => (0xFFF4_3F5E, 0xFF22_C55E, 0xFF3B_82F6),
        SwatchStyle::Edge(2) => (0xFFFB_923C, 0xFFF9_7316, 0xFFEA_580C),
        SwatchStyle::Edge(3) => (0xFF67_E8F9, 0xFF22_D3EE, 0xFF08_91B2),
        SwatchStyle::Edge(_) => (0xFF2D_D4BF, 0xFF81_8CF8, 0xFFC0_84FC),
    };
    let stops = [
        d2dc::D2D1_GRADIENT_STOP { position: 0.0, color: argb(c0) },
        d2dc::D2D1_GRADIENT_STOP { position: 0.5, color: argb(c1) },
        d2dc::D2D1_GRADIENT_STOP { position: 1.0, color: argb(c2) },
    ];
    let ell = d2d::D2D1_ELLIPSE {
        point: d2dc::D2D_POINT_2F { x: cx, y: cy },
        radiusX: r,
        radiusY: r,
    };
    match target.CreateGradientStopCollection(&stops, d2d::D2D1_GAMMA_2_2, d2d::D2D1_EXTEND_MODE_CLAMP) {
        Ok(coll) => match target.CreateLinearGradientBrush(
            &d2d::D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: d2dc::D2D_POINT_2F { x: cx - r, y: cy - r },
                endPoint: d2dc::D2D_POINT_2F { x: cx + r, y: cy + r },
            },
            None,
            &coll,
        ) {
            Ok(br) => target.FillEllipse(&ell, &br),
            Err(e) => eprintln!("[ouuutput][tray_menu] swatch CreateLinearGradientBrush failed: {e}"),
        },
        Err(e) => eprintln!("[ouuutput][tray_menu] swatch CreateGradientStopCollection failed: {e}"),
    }
    if let Some(border_b) = solid_brush(target, &rgba(128, 128, 128, 0.45), "swatch_border") {
        target.DrawEllipse(&ell, &border_b, 1.0 * s, None);
    }
    if on {
        let ring = d2d::D2D1_ELLIPSE {
            point: d2dc::D2D_POINT_2F { x: cx, y: cy },
            radiusX: r + 2.5 * s,
            radiusY: r + 2.5 * s,
        };
        if let Some(acc) = solid_brush(target, &argb(pal.accent), "swatch_ring") {
            target.DrawEllipse(&ring, &acc, 1.8 * s, None);
        }
    }
}

unsafe fn draw_switch(
    target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    r: RectF,
    on: bool,
    pal: &Palette,
    s: f32,
) {
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;

    let sw_w = 34.0 * s;
    let sw_h = 18.0 * s;
    let sl = (r.r - 4.0) * s - sw_w;
    let st = (r.t + r.b) * 0.5 * s - sw_h * 0.5;
    let track = d2d::D2D1_ROUNDED_RECT {
        rect: d2dc::D2D_RECT_F { left: sl, top: st, right: sl + sw_w, bottom: st + sw_h },
        radiusX: sw_h * 0.5,
        radiusY: sw_h * 0.5,
    };
    let track_col = if on { argb(pal.accent) } else { argb(pal.switch_off) };
    if let Some(tb) = solid_brush(target, &track_col, "switch_track") {
        target.FillRoundedRectangle(&track, &tb);
    }
    let knob_r = 6.0 * s;
    let kx = if on { sl + sw_w - 3.0 * s - knob_r } else { sl + 3.0 * s + knob_r };
    let ky = st + sw_h * 0.5;
    let knob_col = if on && pal.dark { argb(pal.accent_ink) } else { argb(0xFFFF_FFFF) };
    if let Some(kb) = solid_brush(target, &knob_col, "switch_knob") {
        target.FillEllipse(
            &d2d::D2D1_ELLIPSE {
                point: d2dc::D2D_POINT_2F { x: kx, y: ky },
                radiusX: knob_r,
                radiusY: knob_r,
            },
            &kb,
        );
    }
}

fn render_and_present(hwnd: isize) {
    use windows::Foundation::Numerics::Matrix3x2;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;
    use windows::Win32::Graphics::Gdi::HDC;

    STATE.with(|slot| unsafe {
        let mut st = slot.borrow_mut();
        let s = st.scale.max(1.0);
        let win_w = ((st.layout.panel_w + SHADOW_PAD_DP * 2.0) * s).ceil() as i32;
        let win_h = ((st.layout.panel_h + SHADOW_PAD_DP * 2.0) * s).ceil() as i32;

        // 重摆位置（严格夹紧工作区）
        let mut wa = RectI::default();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut wa as *mut RectI).cast(), 0);
        let (wx, wy) = anchor_in_work_area(
            st.anchor_pt.x,
            st.anchor_pt.y,
            win_w,
            win_h,
            wa.left,
            wa.top,
            wa.right,
            wa.bottom,
            (8.0 * s) as i32,
        );

        let resize = st.canvas.as_ref().map(|c| (c.w, c.h)) != Some((win_w, win_h));
        if resize {
            if let Some(rt) = st.rt.as_mut() {
                rt.bound = None;
            }
            st.canvas = MenuCanvas::create(win_w, win_h);
        }
        let Some(mem_dc) = st.canvas.as_ref().map(|c| c.mem_dc) else {
            return;
        };
        if !ensure_rt(&mut st) {
            return;
        }
        let Some(rt_ref) = st.rt.as_ref() else {
            return;
        };
        let need_bind = rt_ref.bound != Some((mem_dc, win_w, win_h));
        if need_bind {
            let sub = RECT { left: 0, top: 0, right: win_w, bottom: win_h };
            if let Err(e) = rt_ref.rt.BindDC(HDC(mem_dc), &sub) {
                eprintln!("[ouuutput][tray_menu] BindDC failed: {e}");
                st.rt = None;
                return;
            }
        }
        let Some(rt_ref) = st.rt.as_ref() else {
            return;
        };
        let target = &rt_ref.target;
        let dw = &rt_ref.dw_factory;
        let pal = palette(st.dark);
        let ox = SHADOW_PAD_DP * s;
        let oy = SHADOW_PAD_DP * s;

        target.BeginDraw();
        target.SetTransform(&Matrix3x2::identity());
        target.Clear(Some(&rgba(0, 0, 0, 0.0)));
        target.SetTransform(&Matrix3x2::translation(ox, oy));

        let pw = st.layout.panel_w * s;
        let ph = st.layout.panel_h * s;
        let rad = 12.0 * s;

        // 多层柔和投影
        for (expand, dy, alpha) in [(9.0, 5.0, 0.22), (5.0, 3.0, 0.36), (2.0, 1.0, 0.50)] {
            let mut sh_col = argb(pal.shadow);
            sh_col.a *= alpha;
            if let Some(sb) = solid_brush(target, &sh_col, "shadow") {
                let rr = d2d::D2D1_ROUNDED_RECT {
                    rect: d2dc::D2D_RECT_F {
                        left: -expand * s,
                        top: (-expand + dy) * s,
                        right: pw + expand * s,
                        bottom: ph + (expand + dy) * s,
                    },
                    radiusX: rad + expand * 0.4 * s,
                    radiusY: rad + expand * 0.4 * s,
                };
                target.FillRoundedRectangle(&rr, &sb);
            }
        }

        // 主面板底 + 边框
        let panel_rr = d2d::D2D1_ROUNDED_RECT {
            rect: d2dc::D2D_RECT_F { left: 0.0, top: 0.0, right: pw, bottom: ph },
            radiusX: rad,
            radiusY: rad,
        };
        if let Some(pb) = solid_brush(target, &argb(pal.panel), "panel") {
            target.FillRoundedRectangle(&panel_rr, &pb);
        }
        if let Some(pbd) = solid_brush(target, &argb(pal.panel_border), "panel_border") {
            target.DrawRoundedRectangle(&panel_rr, &pbd, 1.0 * s, None);
        }

        let hover_idx = st.hover_idx;
        let tone_col = |t: Tone| match t {
            Tone::Ink => argb(pal.ink),
            Tone::Muted => argb(pal.muted),
        };

        for (idx, w) in st.layout.widgets.iter().enumerate() {
            let hovered = hover_idx == Some(idx) && w.action.is_some();
            let rect_s = d2dc::D2D_RECT_F {
                left: w.rect.l * s,
                top: w.rect.t * s,
                right: w.rect.r * s,
                bottom: w.rect.b * s,
            };
            match &w.kind {
                WidgetKind::CardBg => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 9.0 * s, radiusY: 9.0 * s };
                    if let Some(b) = solid_brush(target, &argb(pal.card), "card") {
                        target.FillRoundedRectangle(&rr, &b);
                    }
                    if let Some(bd) = solid_brush(target, &argb(pal.card_border), "card_border") {
                        target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                    }
                }
                WidgetKind::Divider => {
                    if let Some(b) = solid_brush(target, &argb(pal.card_border), "divider") {
                        target.FillRectangle(&rect_s, &b);
                    }
                }
                WidgetKind::BrandBadge => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 8.0 * s, radiusY: 8.0 * s };
                    let stops = [
                        d2dc::D2D1_GRADIENT_STOP { position: 0.0, color: argb(0xFF25_63EB) },
                        d2dc::D2D1_GRADIENT_STOP { position: 1.0, color: argb(0xFF06_B6D4) },
                    ];
                    match target.CreateGradientStopCollection(&stops, d2d::D2D1_GAMMA_2_2, d2d::D2D1_EXTEND_MODE_CLAMP) {
                        Ok(coll) => match target.CreateLinearGradientBrush(
                            &d2d::D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                                startPoint: d2dc::D2D_POINT_2F { x: rect_s.left, y: rect_s.top },
                                endPoint: d2dc::D2D_POINT_2F { x: rect_s.right, y: rect_s.bottom },
                            },
                            None,
                            &coll,
                        ) {
                            Ok(gb) => target.FillRoundedRectangle(&rr, &gb),
                            Err(e) => eprintln!("[ouuutput][tray_menu] badge CreateLinearGradientBrush failed: {e}"),
                        },
                        Err(e) => eprintln!("[ouuutput][tray_menu] badge CreateGradientStopCollection failed: {e}"),
                    }
                    // 矢量麦克风图标
                    if let Some(wb) = solid_brush(target, &argb(0xFFFF_FFFF), "badge_mic") {
                        let cx = (rect_s.left + rect_s.right) * 0.5;
                        let cy = (rect_s.top + rect_s.bottom) * 0.5;
                        let cap = d2d::D2D1_ROUNDED_RECT {
                            rect: d2dc::D2D_RECT_F {
                                left: cx - 3.2 * s,
                                top: cy - 7.5 * s,
                                right: cx + 3.2 * s,
                                bottom: cy + 1.5 * s,
                            },
                            radiusX: 3.2 * s,
                            radiusY: 3.2 * s,
                        };
                        target.FillRoundedRectangle(&cap, &wb);
                        let arc_r = d2dc::D2D_RECT_F {
                            left: cx - 5.6 * s,
                            top: cy - 2.0 * s,
                            right: cx + 5.6 * s,
                            bottom: cy + 4.5 * s,
                        };
                        target.DrawRoundedRectangle(
                            &d2d::D2D1_ROUNDED_RECT { rect: arc_r, radiusX: 5.5 * s, radiusY: 5.0 * s },
                            &wb,
                            1.5 * s,
                            None,
                        );
                        target.DrawLine(
                            d2dc::D2D_POINT_2F { x: cx, y: cy + 4.5 * s },
                            d2dc::D2D_POINT_2F { x: cx, y: cy + 7.6 * s },
                            &wb,
                            1.6 * s,
                            None,
                        );
                        target.DrawLine(
                            d2dc::D2D_POINT_2F { x: cx - 3.5 * s, y: cy + 7.6 * s },
                            d2dc::D2D_POINT_2F { x: cx + 3.5 * s, y: cy + 7.6 * s },
                            &wb,
                            1.6 * s,
                            None,
                        );
                    }
                }
                WidgetKind::StatusDot => {
                    if let Some(b) = solid_brush(target, &argb(pal.ok), "status_dot") {
                        target.FillEllipse(
                            &d2d::D2D1_ELLIPSE {
                                point: d2dc::D2D_POINT_2F {
                                    x: (rect_s.left + rect_s.right) * 0.5,
                                    y: (rect_s.top + rect_s.bottom) * 0.5,
                                },
                                radiusX: 3.0 * s,
                                radiusY: 3.0 * s,
                            },
                            &b,
                        );
                    }
                }
                WidgetKind::Text { text, size, bold, tone, align } => {
                    draw_text_line(target, dw, rect_s, text, *size * s, *bold, tone_col(*tone), *align);
                }
                WidgetKind::VersionPill(ver) => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 10.0 * s, radiusY: 10.0 * s };
                    if let Some(b) = solid_brush(target, &argb(pal.card), "ver_pill") {
                        target.FillRoundedRectangle(&rr, &b);
                    }
                    if let Some(bd) = solid_brush(target, &argb(pal.card_border), "ver_pill_bd") {
                        target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                    }
                    draw_text_line(target, dw, rect_s, ver, 10.8 * s, false, argb(pal.muted), Align::Center);
                }
                WidgetKind::PillButton { text, active } => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 6.0 * s, radiusY: 6.0 * s };
                    let bg = if *active { pal.accent_soft } else if hovered { pal.hover } else { 0 };
                    if bg != 0 {
                        if let Some(b) = solid_brush(target, &argb(bg), "pill_bg") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                    }
                    let bd_col = if *active { pal.accent } else { pal.card_border };
                    if let Some(bd) = solid_brush(target, &argb(bd_col), "pill_bd") {
                        target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                    }
                    let fg = if *active { pal.accent } else { pal.ink };
                    draw_text_line(target, dw, rect_s, text, 11.8 * s, true, argb(fg), Align::Center);
                }
                WidgetKind::Chip { text, on } => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 6.0 * s, radiusY: 6.0 * s };
                    if *on {
                        if let Some(b) = solid_brush(target, &argb(pal.accent), "chip_on") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                        draw_text_line(target, dw, rect_s, text, 11.2 * s, true, argb(pal.accent_ink), Align::Center);
                    } else {
                        if hovered {
                            if let Some(b) = solid_brush(target, &argb(pal.hover), "chip_hover") {
                                target.FillRoundedRectangle(&rr, &b);
                            }
                        }
                        if let Some(bd) = solid_brush(target, &argb(pal.card_border), "chip_bd") {
                            target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                        }
                        draw_text_line(target, dw, rect_s, text, 11.2 * s, false, argb(pal.ink), Align::Center);
                    }
                }
                WidgetKind::Segment { text, on, group_bg } => {
                    if *group_bg {
                        let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 7.0 * s, radiusY: 7.0 * s };
                        if let Some(b) = solid_brush(target, &argb(pal.seg_bg), "seg_bg") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                    } else {
                        let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 5.0 * s, radiusY: 5.0 * s };
                        if *on {
                            if let Some(b) = solid_brush(target, &argb(pal.seg_active), "seg_on") {
                                target.FillRoundedRectangle(&rr, &b);
                            }
                        } else if hovered {
                            if let Some(b) = solid_brush(target, &argb(pal.hover), "seg_hover") {
                                target.FillRoundedRectangle(&rr, &b);
                            }
                        }
                        let fg = if *on { pal.ink } else { pal.muted };
                        draw_text_line(target, dw, rect_s, text, 11.5 * s, *on, argb(fg), Align::Center);
                    }
                }
                WidgetKind::Swatch { grad, on } => {
                    let cx = (rect_s.left + rect_s.right) * 0.5;
                    let cy = (rect_s.top + rect_s.bottom) * 0.5;
                    let r = (rect_s.right - rect_s.left) * 0.5 - 1.0 * s;
                    draw_swatch(target, cx, cy, r, *grad, *on, &pal, s);
                }
                WidgetKind::MiniCycleBtn(txt) => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 5.0 * s, radiusY: 5.0 * s };
                    if hovered {
                        if let Some(b) = solid_brush(target, &argb(pal.hover), "mini_hover") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                    }
                    if let Some(bd) = solid_brush(target, &argb(pal.card_border), "mini_bd") {
                        target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                    }
                    draw_text_line(target, dw, rect_s, txt, 10.6 * s, false, argb(pal.ink), Align::Center);
                }
                WidgetKind::ToggleRow { title, subtitle, on } => {
                    if hovered {
                        let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 6.0 * s, radiusY: 6.0 * s };
                        if let Some(b) = solid_brush(target, &argb(pal.hover), "row_hover") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                    }
                    let tx_l = (w.rect.l + 4.0) * s;
                    let tx_r = (w.rect.r - 44.0) * s;
                    if let Some(sub) = subtitle {
                        let r1 = d2dc::D2D_RECT_F { left: tx_l, top: (w.rect.t + 3.0) * s, right: tx_r, bottom: (w.rect.t + 20.0) * s };
                        let r2 = d2dc::D2D_RECT_F { left: tx_l, top: (w.rect.t + 20.0) * s, right: tx_r, bottom: (w.rect.t + 35.0) * s };
                        draw_text_line(target, dw, r1, title, 12.2 * s, true, argb(pal.ink), Align::Left);
                        draw_text_line(target, dw, r2, sub, 10.8 * s, false, argb(pal.muted), Align::Left);
                    } else {
                        let r1 = d2dc::D2D_RECT_F { left: tx_l, top: rect_s.top, right: tx_r, bottom: rect_s.bottom };
                        draw_text_line(target, dw, r1, title, 12.0 * s, false, argb(pal.ink), Align::Left);
                    }
                    draw_switch(target, w.rect, *on, &pal, s);
                }
                WidgetKind::AccordionHeader { title, right } => {
                    if hovered {
                        let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 6.0 * s, radiusY: 6.0 * s };
                        if let Some(b) = solid_brush(target, &argb(pal.hover), "acc_hover") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                    }
                    let r1 = d2dc::D2D_RECT_F { left: (w.rect.l + 4.0) * s, top: rect_s.top, right: (w.rect.r - 120.0) * s, bottom: rect_s.bottom };
                    let r2 = d2dc::D2D_RECT_F { left: (w.rect.r - 120.0) * s, top: rect_s.top, right: (w.rect.r - 4.0) * s, bottom: rect_s.bottom };
                    draw_text_line(target, dw, r1, title, 12.2 * s, true, argb(pal.ink), Align::Left);
                    draw_text_line(target, dw, r2, right, 11.0 * s, false, argb(pal.muted), Align::Right);
                }
                WidgetKind::ToolBtn { text } => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 6.0 * s, radiusY: 6.0 * s };
                    if hovered {
                        if let Some(b) = solid_brush(target, &argb(pal.hover), "tool_hover") {
                            target.FillRoundedRectangle(&rr, &b);
                        }
                    }
                    if let Some(bd) = solid_brush(target, &argb(pal.card_border), "tool_bd") {
                        target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                    }
                    let pad = d2dc::D2D_RECT_F { left: rect_s.left + 8.0 * s, top: rect_s.top, right: rect_s.right - 6.0 * s, bottom: rect_s.bottom };
                    draw_text_line(target, dw, pad, text, 11.3 * s, false, argb(pal.ink), Align::Left);
                }
                WidgetKind::FooterBtn { text, danger } => {
                    let rr = d2d::D2D1_ROUNDED_RECT { rect: rect_s, radiusX: 7.0 * s, radiusY: 7.0 * s };
                    let bg = if *danger { pal.danger_soft } else if hovered { pal.hover } else { pal.card };
                    if let Some(b) = solid_brush(target, &argb(bg), "footer_bg") {
                        target.FillRoundedRectangle(&rr, &b);
                    }
                    if !*danger {
                        if let Some(bd) = solid_brush(target, &argb(pal.card_border), "footer_bd") {
                            target.DrawRoundedRectangle(&rr, &bd, 1.0 * s, None);
                        }
                    }
                    let fg = if *danger { pal.danger } else { pal.ink };
                    draw_text_line(target, dw, rect_s, text, 12.0 * s, *danger, argb(fg), Align::Center);
                }
            }
        }

        if let Err(e) = target.EndDraw(None, None) {
            eprintln!("[ouuutput][tray_menu] EndDraw failed: {e}");
            st.rt = None;
            return;
        }
        if let Some(rt_mut) = st.rt.as_mut() {
            rt_mut.bound = Some((mem_dc, win_w, win_h));
        }

        let mut pt_dst = Point { x: wx, y: wy };
        let mut sz = Size { cx: win_w, cy: win_h };
        let mut pt_src = Point { x: 0, y: 0 };
        let blend = BlendFunction {
            blend_op: 0,
            blend_flags: 0,
            source_constant_alpha: 255,
            alpha_format: 1,
        };
        SetWindowPos(hwnd, HWND_TOPMOST, wx, wy, win_w, win_h, SWP_NOACTIVATE);
        if UpdateLayeredWindow(hwnd, 0, &mut pt_dst, &mut sz, mem_dc, &mut pt_src, 0, &blend, ULW_ALPHA) == 0 {
            eprintln!("[ouuutput] tray_menu UpdateLayeredWindow 失败: {}", std::io::Error::last_os_error());
        }
    });
}

fn rebuild_layout(st: &mut MenuState) {
    let ver = concat!("v", env!("CARGO_PKG_VERSION"));
    st.layout = build_layout(&st.flags, st.open, ver);
}

fn apply_local_optimistic(st: &mut MenuState, id: usize) {
    match id {
        ID_CTX_BIAS => st.flags.ctx_bias = !st.flags.ctx_bias,
        ID_AUTOSTART => st.flags.autostart = !st.flags.autostart,
        ID_PUNCT_SPACE => st.flags.punct_to_space = !st.flags.punct_to_space,
        ID_MERGE_TERMS => st.flags.merge_terms = !st.flags.merge_terms,
        ID_FIX_CASE => st.flags.fix_case = !st.flags.fix_case,
        ID_LEADING_SPACE => st.flags.leading_space = !st.flags.leading_space,
        ID_TRAILING_SPACE => st.flags.trailing_space = !st.flags.trailing_space,
        ID_CTX_VERBOSE => st.flags.ctx_verbose = !st.flags.ctx_verbose,
        _ if id >= ID_OVERLAY_BASE && id <= ID_OVERLAY_BASE + 5 => {
            st.flags.overlay_mode = id - ID_OVERLAY_BASE;
        }
        _ if id >= ID_ORB_SIZE_BASE && id < ID_ORB_SIZE_BASE + 3 => {
            st.flags.orb_size_idx = id - ID_ORB_SIZE_BASE;
        }
        _ if id >= ID_RING_SIZE_BASE && id < ID_RING_SIZE_BASE + 3 => {
            st.flags.ring_size_idx = id - ID_RING_SIZE_BASE;
        }
        _ if id >= ID_RING_SENS_BASE && id < ID_RING_SENS_BASE + 3 => {
            st.flags.ring_sens_idx = id - ID_RING_SENS_BASE;
        }
        _ if id >= ID_HALO_TINT_BASE && id < ID_HALO_TINT_BASE + 8 => {
            st.flags.halo_tint = id - ID_HALO_TINT_BASE;
        }
        _ if id >= ID_HALO_SPREAD_BASE && id < ID_HALO_SPREAD_BASE + 3 => {
            st.flags.halo_spread_idx = id - ID_HALO_SPREAD_BASE;
        }
        _ if id >= ID_HALO_SPEED_BASE && id < ID_HALO_SPEED_BASE + 3 => {
            st.flags.halo_speed_idx = id - ID_HALO_SPEED_BASE;
        }
        _ if id >= ID_HALO_OPACITY_BASE && id < ID_HALO_OPACITY_BASE + 3 => {
            st.flags.halo_opacity_idx = id - ID_HALO_OPACITY_BASE;
        }
        _ if id >= ID_EDGE_TINT_BASE && id < ID_EDGE_TINT_BASE + 5 => {
            st.flags.edge_tint = id - ID_EDGE_TINT_BASE;
        }
        _ if id >= ID_HOTKEY_BASE && id < ID_HOTKEY_BASE + HOTKEY_CANDIDATES.len() => {
            st.flags.hotkey = HOTKEY_CANDIDATES[id - ID_HOTKEY_BASE].0;
        }
        _ if id >= ID_SILENCE_BASE && id < ID_SILENCE_BASE + SILENCE_CHOICES.len() => {
            st.flags.silence_sec = SILENCE_CHOICES[id - ID_SILENCE_BASE];
        }
        _ if id >= ID_EDGE_SPREAD_BASE && id < ID_EDGE_SPREAD_BASE + 3 => {
            st.flags.edge_spread_idx = id - ID_EDGE_SPREAD_BASE;
        }
        _ if id >= ID_EDGE_PEAK_BASE && id < ID_EDGE_PEAK_BASE + 3 => {
            st.flags.edge_peak_idx = id - ID_EDGE_PEAK_BASE;
        }
        _ if id >= ID_EDGE_SPEED_BASE && id < ID_EDGE_SPEED_BASE + 3 => {
            st.flags.edge_speed_idx = id - ID_EDGE_SPEED_BASE;
        }
        _ if id >= ID_EDGE_FPS_BASE && id < ID_EDGE_FPS_BASE + 2 => {
            st.flags.edge_fps_idx = id - ID_EDGE_FPS_BASE;
        }
        _ => {}
    }
}

fn hide_menu(hwnd: isize) {
    if MENU_VISIBLE.swap(false, Ordering::Relaxed) {
        LAST_CLOSE_TICK.store(now_ms(), Ordering::Relaxed);
    }
    unsafe { ShowWindow(hwnd, SW_HIDE); }
}

unsafe extern "system" fn menu_wnd_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PaintStruct = std::mem::zeroed();
            if BeginPaint(hwnd, &mut ps) != 0 {
                EndPaint(hwnd, &ps);
            }
            0
        }
        WM_ACTIVATE => {
            if (wp & 0xFFFF) == WA_INACTIVE && MENU_VISIBLE.load(Ordering::Relaxed) {
                hide_menu(hwnd);
            }
            0
        }
        WM_KEYDOWN => {
            if wp == VK_ESCAPE {
                hide_menu(hwnd);
            }
            0
        }
        WM_MENU_REFRESH => {
            if MENU_VISIBLE.load(Ordering::Relaxed) {
                STATE.with(|slot| {
                    let mut st = slot.borrow_mut();
                    st.flags = tray::snapshot_flags();
                    rebuild_layout(&mut st);
                });
                render_and_present(hwnd);
            }
            0
        }
        WM_MOUSEMOVE => {
            let px = (lp as i32 & 0xFFFF) as i16 as f32;
            let py = ((lp as i32 >> 16) & 0xFFFF) as i16 as f32;
            let changed = STATE.with(|slot| {
                let mut st = slot.borrow_mut();
                if !st.tracking_leave {
                    let mut tme = TrackMouseEventBuf {
                        cb_size: std::mem::size_of::<TrackMouseEventBuf>() as u32,
                        dw_flags: TME_LEAVE,
                        hwnd_track: hwnd,
                        dw_hover_time: 0,
                    };
                    TrackMouseEvent(&mut tme);
                    st.tracking_leave = true;
                }
                let dp_x = px / st.scale - SHADOW_PAD_DP;
                let dp_y = py / st.scale - SHADOW_PAD_DP;
                let hit = st.layout.widgets.iter().rposition(|w| w.action.is_some() && w.rect.contains(dp_x, dp_y));
                let diff = st.hover_idx != hit;
                st.hover_idx = hit;
                diff
            });
            if changed {
                render_and_present(hwnd);
            }
            0
        }
        WM_MOUSELEAVE => {
            let changed = STATE.with(|slot| {
                let mut st = slot.borrow_mut();
                st.tracking_leave = false;
                let diff = st.hover_idx.is_some();
                st.hover_idx = None;
                diff
            });
            if changed {
                render_and_present(hwnd);
            }
            0
        }
        WM_LBUTTONDOWN => 0,
        WM_LBUTTONUP => {
            let px = (lp as i32 & 0xFFFF) as i16 as f32;
            let py = ((lp as i32 >> 16) & 0xFFFF) as i16 as f32;
            let act = STATE.with(|slot| {
                let st = slot.borrow();
                let dp_x = px / st.scale - SHADOW_PAD_DP;
                let dp_y = py / st.scale - SHADOW_PAD_DP;
                st.layout
                    .widgets
                    .iter()
                    .rev()
                    .find(|w| w.action.is_some() && w.rect.contains(dp_x, dp_y))
                    .and_then(|w| w.action)
            });
            if let Some(a) = act {
                match a {
                    Action::ToggleDrawer(d) => {
                        STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            st.open = if st.open == Some(d) { None } else { Some(d) };
                            rebuild_layout(&mut st);
                        });
                        render_and_present(hwnd);
                    }
                    Action::CommandKeepOpen(id) => {
                        STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            apply_local_optimistic(&mut st, id);
                            if id >= ID_HOTKEY_BASE && id < ID_HOTKEY_BASE + HOTKEY_CANDIDATES.len() {
                                st.open = None;
                            }
                            rebuild_layout(&mut st);
                        });
                        tray::trigger_action(id);
                        render_and_present(hwnd);
                    }
                    Action::CommandClose(id) => {
                        hide_menu(hwnd);
                        tray::trigger_action(id);
                    }
                    Action::CycleEdgeSpread => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.edge_spread_idx + 1) % 3;
                            let id = ID_EDGE_SPREAD_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                    Action::CycleEdgePeak => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.edge_peak_idx + 1) % 3;
                            let id = ID_EDGE_PEAK_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                    Action::CycleEdgeSpeed => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.edge_speed_idx + 1) % 3;
                            let id = ID_EDGE_SPEED_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                    Action::CycleEdgeFps => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.edge_fps_idx + 1) % 2;
                            let id = ID_EDGE_FPS_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                    Action::CycleHaloSpread => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.halo_spread_idx + 1) % 3;
                            let id = ID_HALO_SPREAD_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                    Action::CycleHaloSpeed => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.halo_speed_idx + 1) % 3;
                            let id = ID_HALO_SPEED_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                    Action::CycleHaloOpacity => {
                        let next_id = STATE.with(|slot| {
                            let mut st = slot.borrow_mut();
                            let idx = (st.flags.halo_opacity_idx + 1) % 3;
                            let id = ID_HALO_OPACITY_BASE + idx;
                            apply_local_optimistic(&mut st, id);
                            rebuild_layout(&mut st);
                            id
                        });
                        tray::trigger_action(next_id);
                        render_and_present(hwnd);
                    }
                }
            }
            0
        }
        WM_DESTROY => {
            MENU_VISIBLE.store(false, Ordering::Relaxed);
            MENU_HWND.store(0, Ordering::Relaxed);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn ensure_menu_window() -> isize {
    let existing = MENU_HWND.load(Ordering::Relaxed);
    if existing != 0 {
        return existing;
    }
    let inst = GetModuleHandleW(ptr::null());
    let cls = wide(MENU_CLASS);
    let wc = WndClassW {
        style: 0,
        lpfn_wnd_proc: Some(menu_wnd_proc),
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        h_instance: inst,
        h_icon: 0,
        h_cursor: LoadCursorW(0, 32512), // IDC_ARROW
        hbr_background: 0,
        menu_name: ptr::null(),
        class_name: cls.as_ptr(),
    };
    if RegisterClassW(&wc) == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(1410) {
            eprintln!("[ouuutput] tray_menu RegisterClassW 失败: {e}");
            return 0;
        }
    }
    let title = wide("ouuutput 托盘菜单");
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
        cls.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        0,
        0,
        10,
        10,
        0,
        0,
        inst,
        0,
    );
    if hwnd != 0 {
        MENU_HWND.store(hwnd, Ordering::Relaxed);
    } else {
        eprintln!("[ouuutput] tray_menu CreateWindowExW 失败: {}", std::io::Error::last_os_error());
    }
    hwnd
}

/// 在托盘线程内由 `WM_TRAY` 调用：弹出或收起 Fluent 托盘面板。
pub(crate) unsafe fn toggle_or_show() {
    let hwnd = ensure_menu_window();
    if hwnd == 0 {
        return;
    }
    // 若当前已可见，或者刚因点击托盘图标触发失焦关闭（<180ms），则收起不立即重开
    if MENU_VISIBLE.load(Ordering::Relaxed)
        || now_ms().saturating_sub(LAST_CLOSE_TICK.load(Ordering::Relaxed)) < 180
    {
        hide_menu(hwnd);
        return;
    }

    let mut pt = Point::default();
    GetCursorPos(&mut pt);
    let dpi = GetDpiForWindow(hwnd);
    let scale = if dpi >= 96 { dpi as f32 / 96.0 } else { 1.0 };

    STATE.with(|slot| {
        let mut st = slot.borrow_mut();
        st.flags = tray::snapshot_flags();
        st.open = None;
        st.hover_idx = None;
        st.dark = system_is_dark();
        st.scale = scale;
        st.anchor_pt = pt;
        rebuild_layout(&mut st);
    });

    MENU_VISIBLE.store(true, Ordering::Relaxed);
    render_and_present(hwnd);
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
}

/// 主线程写回 `TrayFlags` 后通知已打开的面板重画最新状态。
pub(crate) fn notify_refresh() {
    if !MENU_VISIBLE.load(Ordering::Relaxed) {
        return;
    }
    let hwnd = MENU_HWND.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd, WM_MENU_REFRESH, 0, 0); }
    }
}

pub(crate) unsafe fn destroy() {
    let hwnd = MENU_HWND.swap(0, Ordering::Relaxed);
    if hwnd != 0 {
        DestroyWindow(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_flags() -> TrayFlags {
        TrayFlags {
            punct_to_space: false,
            merge_terms: true,
            ctx_bias: true,
            ctx_verbose: false,
            leading_space: true,
            trailing_space: true,
            fix_case: true,
            silence_sec: 15,
            overlay_mode: 4,
            halo_tint: 0,
            orb_size_idx: 1,
            ring_size_idx: 1,
            ring_sens_idx: 1,
            halo_spread_idx: 1,
            halo_speed_idx: 1,
            halo_opacity_idx: 1,
            edge_tint: 0,
            edge_spread_idx: 1,
            edge_peak_idx: 1,
            edge_speed_idx: 1,
            edge_fps_idx: 0,
            hotkey: "mousex1",
            autostart: false,
        }
    }

    #[test]
    fn layout_contextual_mode_height_and_widgets() {
        let mut f = sample_flags();
        let l_halo = build_layout(&f, None, "v0.3.2");
        assert!(l_halo.widgets.iter().any(|w| matches!(w.kind, WidgetKind::Swatch { grad: SwatchStyle::Halo(0), on: true })));
        assert!(l_halo.widgets.iter().any(|w| matches!(w.kind, WidgetKind::Swatch { grad: SwatchStyle::Halo(5), .. })));
        assert_eq!(l_halo.widgets.iter().filter(|w| matches!(w.kind, WidgetKind::Swatch { .. })).count(), 8);
        assert_eq!(l_halo.widgets.iter().filter(|w| matches!(w.kind, WidgetKind::MiniCycleBtn(_))).count(), 4);

        f.overlay_mode = 2;
        let l_orb = build_layout(&f, None, "v0.3.2");
        assert_eq!(l_orb.widgets.iter().filter(|w| matches!(w.kind, WidgetKind::Swatch { .. })).count(), 8);
        assert_eq!(l_orb.widgets.iter().filter(|w| matches!(w.action, Some(Action::CommandKeepOpen(id)) if (ID_ORB_SIZE_BASE..ID_ORB_SIZE_BASE + 3).contains(&id))).count(), 3);
        assert_eq!(l_orb.widgets.iter().filter(|w| matches!(w.kind, WidgetKind::Segment { group_bg: false, .. })).count(), 4);

        f.overlay_mode = 5;
        let l_ring = build_layout(&f, None, "v0.3.2");
        assert_eq!(l_ring.widgets.iter().filter(|w| matches!(w.kind, WidgetKind::Swatch { .. })).count(), 0);
        assert_eq!(l_ring.widgets.iter().filter(|w| matches!(w.action, Some(Action::CommandKeepOpen(id)) if (ID_RING_SIZE_BASE..ID_RING_SIZE_BASE + 3).contains(&id))).count(), 3);
        assert_eq!(l_ring.widgets.iter().filter(|w| matches!(w.action, Some(Action::CommandKeepOpen(id)) if (ID_RING_SENS_BASE..ID_RING_SENS_BASE + 3).contains(&id))).count(), 3);
        f.overlay_mode = 0;
        let l_off = build_layout(&f, None, "v0.3.2");
        assert!(!l_off.widgets.iter().any(|w| matches!(w.kind, WidgetKind::Swatch { .. })));
        assert!(l_off.panel_h < l_halo.panel_h);
    }

    #[test]
    fn layout_drawers_expand_cleanly_within_width() {
        let f = sample_flags();
        for d in [Some(Drawer::Hotkey), Some(Drawer::Typography), Some(Drawer::Tools)] {
            let l = build_layout(&f, d, "v0.3.2");
            for w in &l.widgets {
                assert!(w.rect.l >= 0.0 && w.rect.r <= l.panel_w + 0.5, "oob x: {:?}", w.rect);
                assert!(w.rect.t >= 0.0 && w.rect.b <= l.panel_h + 0.5, "oob y: {:?}", w.rect);
            }
        }
    }

    #[test]
    fn anchor_clamps_to_work_area() {
        // 右下角任务栏图标附近：面板应自动往左上方收进工作区
        let (x, y) = anchor_in_work_area(3820, 2140, 750, 900, 0, 0, 3840, 2100, 16);
        assert!(x >= 16 && x + 750 <= 3840 - 16, "x={x}");
        assert!(y >= 16 && y + 900 <= 2100 - 16, "y={y}");
    }
}
