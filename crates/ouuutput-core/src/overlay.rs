//! 光标旁的悬浮状态条 —— Direct2D 版（v2，六号方案）。
//!
//! ## 为什么重写
//!
//! 旧版走 GDI 的 Window DC。实测（`examples/gdiprobe.rs`）那条路上
//! `FillRect` / `RoundRect` / `Rectangle` / `TextOutW` **全都不落笔**，
//! 只有 `ExtTextOutW` 能画 —— 所以只能拿 `█` 字符铺底 + 1px 直角边框，
//! 做不出圆角、真阴影、半透明背景。用户评价「太丑」。
//!
//! Direct2D 走**独立的渲染目标**（DCRenderTarget / HWND RenderTarget），
//! 不受那条路径的限制：圆角、渐变、真阴影、抗锯齿文字全都能画。
//!
//! ## 设计规格（参考本地语音输入法的常见样式，配色自定）
//!
//! 常见悬浮条几何与质感参数实测：
//!   · 圆角 = 高度/2（胶囊），或小圆角矩形
//!   · 阴影 dy=2 / blur=3 / 黑 20%
//!   · 描边 0.5–1px，白 8%
//!   · 字体 Microsoft YaHei UI 13px，文字 85% 白
//!
//! 我们沿用几何与质感参数，配色用「暖墨」（暖棕黑 + 琥珀），见下方常量。
//!
//! ## 关键约束（少任何一条都会出事故）
//!   WS_EX_NOACTIVATE   不抢焦点。抢了焦点 => 注入打到别处、用户输入被打断。
//!   WS_EX_TOOLWINDOW   不进 alt-tab、不进任务栏。
//!   WS_EX_TOPMOST      永远浮在最上层。
//!   WS_EX_LAYERED      半透明（这里用 per-pixel alpha 走 UpdateLayeredWindow）
//!   WM_MOUSEACTIVATE   返回 MA_NOACTIVATE，用户点它也抢不走焦点。
//!
//! ## 线程模型
//! 独立线程持窗 + 40ms 定时器轮询共享状态（沿用旧版，已验证可靠）。

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_LAYERED: u32 = 0x0008_0000;
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
/// 鼠标穿透:窗口铺满整屏后**绝不能挡住点击**(配合 per-pixel alpha 全透明,用户看不到它)。
const WS_EX_TRANSPARENT: u32 = 0x0000_0020;

const SW_SHOWNOACTIVATE: i32 = 4;
const HWND_TOPMOST: isize = -1;
const SWP_NOACTIVATE: u32 = 0x0010;

const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_TIMER: u32 = 0x0113;
const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_NCHITTEST: u32 = 0x0084;
const HTTRANSPARENT: isize = -1;
const HTCLIENT: isize = 1;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const MA_NOACTIVATE: isize = 3;
const WM_SETCURSOR: u32 = 0x0020;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_CAPTURECHANGED: u32 = 0x0215;
const CS_DBLCLKS: u32 = 0x0008;
const GWL_EXSTYLE: i32 = -20;
const IDC_SIZEALL: usize = 32646;

const OVERLAY_CLASS: &str = "OuuutputOverlayWnd";


/// 当前屏幕的 DPI 缩放系数（1.0 = 96 DPI / 100%）。
///
/// 为什么必须要有它：进程是 **per-monitor DPI aware v2** 的，
/// `GetSystemMetrics` 返回**物理像素**；而用户说的"太小"是**视觉大小**。
/// 本机实测屏幕 3320x3846 物理像素 / 225% 缩放 = 1476x1709 逻辑像素。
///
/// 早先所有尺寸都写成裸像素（MIN_W=520 等），在 225% 缩放下
/// 实际只占 231 逻辑像素宽 —— 视觉上当然小得看不清。
/// 现在统一：**设计稿按 100% 写，运行时乘 `scale()`**。
fn scale() -> f32 {
    unsafe {
        // GetDpiForWindow 需要 hwnd；这里用屏幕 DC 的 LOGPIXELSX 更简单，
        // 而且和 GetSystemMetrics 的坐标系一致（都是物理像素）。
        let dc = GetDC(0);
        if dc == 0 {
            return 1.0;
        }
        let dpi = GetDeviceCaps(dc, 88); // LOGPIXELSX
        ReleaseDC(0, dc);
        if dpi <= 0 {
            1.0
        } else {
            dpi as f32 / 96.0
        }
    }
}

/// 设计稿像素 -> 物理像素。
fn px(v: i32) -> i32 {
    (v as f32 * scale()).round() as i32
}

/// 边缘光的动画时钟：返回 (呼吸相位, 流动相位)，都在 0..1。
///
/// ⚠️ 必须用**相对时间**，不能用 `SystemTime::now()` 的绝对毫秒。
/// 绝对时间戳约 1.7e12，f32 在那个量级的量化步长约 25 ——
/// 把 0..1 的周长参数 `t` 加上去会被**完全吞掉**，
/// 结果是颜色沿边缘不变化（踩过这个坑）。
///
/// 相对时间最多几小时（~1e7 ms），f32 精度足够。
fn edge_clock() -> (f32, f32) {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let ms = start.elapsed().as_millis() as f64;

    // 速度倍率直接作用在周期上(周期越短越快),下限留 200ms —— 再快就不是"呼吸"是闪了。
    let speed = edge_speed() as f64;
    let breath_ms = (EDGE_BREATH_MS as f64 / speed).max(200.0);
    let flow_ms = (EDGE_FLOW_MS as f64 / speed).max(600.0);
    let breath = ((ms % breath_ms) / breath_ms) as f32;
    let flow = ((ms % flow_ms) / flow_ms) as f32;
    (breath, flow)
}

/// 边缘光**缓动**的上一拍时刻(用来把"每帧 6%"折算成与帧率无关的等效系数)。
static EDGE_LAST_TICK: std::sync::OnceLock<Mutex<std::time::Instant>> = std::sync::OnceLock::new();
/// 边缘光**上一次真正重画**的时刻(30fps 时靠它隔拍跳过整屏绘制)。
static EDGE_LAST_PAINT: std::sync::OnceLock<Mutex<std::time::Instant>> = std::sync::OnceLock::new();

// ---- 设计规格 ----
// (早先那块"圆角面板"图纸的内边距/圆角/阴影/最小宽度常量已随该方案废弃删除:
//  现在的形态是细条 / 悬浮球 / 边缘光,尺寸一律按屏幕比例算,见 `anchor_pos`。)
/// 距屏幕底边（任务栏上方）留多少像素。
const BOTTOM_GAP: i32 = 56;

// ---- 细条（Bar）专用：只有一行正文 ----
const BAR_PAD_X: i32 = 18;
const BAR_PAD_Y: i32 = 10;
const BAR_LINE_H: i32 = 28;
const BAR_MIN_W: i32 = 320;
const BAR_MAX_W: i32 = 860;
const BAR_MIN_H: i32 = 44;
/// 估宽用：ASCII 字符宽 / 中日韩字符宽（设计像素）。
const BAR_CHAR_ASCII: i32 = 10;
const BAR_CHAR_CJK: i32 = 18;

// ---- 悬浮声波（Orb：无框无外晕的裸流体弦波）与 纯白流体圆环（Ring） ----
/// 声波画布宽高（设计像素）：小 / 中(默认) / 大。
const ORB_DIMS: [(i32, i32); 3] = [(112, 30), (146, 38), (190, 48)];
/// 纯白流体圆环画布宽高（设计像素）：小 / 中(默认) / 大。
const RING_DIMS: [(i32, i32); 3] = [(42, 42), (58, 58), (80, 80)];
/// 默认锚点：任务栏顶边上方留出的设计像素间距。
const ORB_BOTTOM_GAP: i32 = 10;
/// 自适应底噪归一化下限（仅用于柔和调制流体振幅；显隐完全由是否按住快捷键决定）。
const ORB_VOICE_THRESH_MIN: f32 = 0.0085;
const ORB_SILENCE_HOLD_MS: u64 = 380;
const BAR_RADIUS: i32 = 10;
const BAR_FONT: f32 = 17.0;
const BAR_INK: u32 = 0xF2_F5F0E8;

// ---- 屏幕边缘光（Edge）----
/// 光带从屏幕边缘往内延伸多少设计像素。
/// 太大就挡内容，太小看不出来 —— 110 在 225% 缩放下约 248 物理像素，够"晕开"又不挡字。
const EDGE_SPREAD_DEFAULT: i32 = 56;
pub const EDGE_SPREAD_MIN: i32 = 24;
pub const EDGE_SPREAD_MAX: i32 = 140;

// 带宽/亮度/速度做成**运行时**可调(HANDOFF 里那条已知限制:以前写死,托盘只能换色)。
//
// 为什么不是常量:这三个是纯**观感**参数,用户看一眼就知道要往哪边调;写死等于每次微调
// 都要改代码重编译。颜色早就做成了运行时的(`set_edge_tint`),这三个跟它同一套:
// 配置项给初值、托盘现场改、改完写回配置。
//
// f32 不放进原子量,存成千分位整数(量级足够、读回稳定)。
static EDGE_SPREAD_PX: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(EDGE_SPREAD_DEFAULT);
static EDGE_PEAK_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((EDGE_PEAK_DEFAULT * 1000.0) as u32);
static EDGE_SPEED_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((EDGE_SPEED_DEFAULT * 1000.0) as u32);

/// 边缘光带宽(px)。
pub fn edge_spread() -> i32 {
    EDGE_SPREAD_PX
        .load(Ordering::Relaxed)
        .clamp(EDGE_SPREAD_MIN, EDGE_SPREAD_MAX)
}

/// 设带宽。越界自动夹到合法区间 —— 配置写错不该让 HUD 变成一堵墙、也不该让它消失。
pub fn set_edge_spread(px: i32) {
    EDGE_SPREAD_PX.store(px.clamp(EDGE_SPREAD_MIN, EDGE_SPREAD_MAX), Ordering::Relaxed);
}

/// 边缘光亮度(最亮处的 alpha,0..1)。
pub fn edge_peak() -> f32 {
    (EDGE_PEAK_MILLI.load(Ordering::Relaxed) as f32 / 1000.0).clamp(EDGE_PEAK_MIN, EDGE_PEAK_MAX)
}

pub fn set_edge_peak(v: f32) {
    let v = v.clamp(EDGE_PEAK_MIN, EDGE_PEAK_MAX);
    EDGE_PEAK_MILLI.store((v * 1000.0) as u32, Ordering::Relaxed);
}

/// 边缘光动画速度倍率(1.0 = 默认节奏)。
pub fn edge_speed() -> f32 {
    (EDGE_SPEED_MILLI.load(Ordering::Relaxed) as f32 / 1000.0)
        .clamp(EDGE_SPEED_MIN, EDGE_SPEED_MAX)
}

pub fn set_edge_speed(v: f32) {
    let v = v.clamp(EDGE_SPEED_MIN, EDGE_SPEED_MAX);
    EDGE_SPEED_MILLI.store((v * 1000.0) as u32, Ordering::Relaxed);
}
/// 最亮处的白色不透明度（0..1）。
/// 边缘光重绘帧率(**只有 60 和 30 两档**)。
///
/// 为什么要它:边缘光是**整屏**逐帧重绘 + 整屏 `UpdateLayeredWindow`,60fps 下这一项
/// 就能吃掉一个核的可观份额(T6.6)。30fps 把它砍一半,代价是动画少一半帧 ——
/// 呼吸(2200ms)和流动(8000ms)都是慢动画,少一半帧看不太出来。
///
/// ⚠️ 降帧率**不能改动画时长**:缓动原来是"每帧走 6%",帧率一砍半淡入淡出就慢一倍
/// (用户看到的是"变迟钝"而不是"省电")。所以缓动改成了按实际间隔折算等效系数
/// (见 `EDGE_LAST_TICK` 那段),这样两档的淡入时长一致,只有平滑度不同。
static EDGE_FPS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(60);

/// 边缘光重绘帧率(60 或 30)。
pub fn edge_fps() -> u32 {
    EDGE_FPS.load(Ordering::Relaxed)
}

/// 设边缘光重绘帧率:只认 30 和 60,其它值就近夹到这两档
/// (写 45 只会让人以为"生效了",其实两档之外没有第三条路)。
pub fn set_edge_fps(v: u32) {
    EDGE_FPS.store(if v <= 45 { 30 } else { 60 }, Ordering::Relaxed);
}

const EDGE_PEAK_DEFAULT: f32 = 0.42;
pub const EDGE_PEAK_MIN: f32 = 0.10;
pub const EDGE_PEAK_MAX: f32 = 0.90;
const EDGE_SPEED_DEFAULT: f32 = 1.0;
pub const EDGE_SPEED_MIN: f32 = 0.4;
pub const EDGE_SPEED_MAX: f32 = 3.0;
/// 呼吸周期（毫秒）。录音时亮度在这个周期里来回起伏。
const EDGE_BREATH_MS: u64 = 2200;
/// 彩色沿周长流动一圈的时间（毫秒）。8 秒是"看得出在动但不晃眼"的节奏。
const EDGE_FLOW_MS: u64 = 8000;

// 暖墨配色
//
// ⚠️ 格式是 **0xAARRGGBB**（和 `argb()` 一致）。
// 早先抄常量时漏了 alpha 通道（写成 0x00RRGGBB），结果整个 HUD 全透明、
// 屏幕上只剩一层淡淡的阴影 —— 排查了半天才发现是 alpha=0。

// 细条要**半透明** —— 用户明确要求「做成半透明的」，而且要能透出下面的字。
// 88/255 ≈ 35% 不透明：足够压住背景保证可读，又能看见底下的内容。
const BAR_BG: u32 = 0x88_1E1A16;
const BAR_BORDER: u32 = 0x22_FFE0C0;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[repr(C)]
struct PaintStruct {
    hdc: isize,
    erase: i32,
    rc: Rect,
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}
#[repr(C)]
struct Msg {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
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
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Size {
    cx: i32,
    cy: i32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BlendFunction {
    blend_op: u8,
    blend_flags: u8,
    source_constant_alpha: u8,
    alpha_format: u8,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
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
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct GuiThreadInfo {
    cb_size: u32,
    flags: u32,
    hwnd_active: isize,
    hwnd_focus: isize,
    hwnd_capture: isize,
    hwnd_menu_owner: isize,
    hwnd_move_size: isize,
    hwnd_caret: isize,
    rc_caret: Rect,
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn CreateDIBSection(
        hdc: isize,
        bmi: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut u8,
        section: isize,
        offset: u32,
    ) -> isize;
    fn DeleteDC(hdc: isize) -> i32;
    fn SelectObject(hdc: isize, obj: isize) -> isize;
    fn DeleteObject(obj: isize) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    /// 伪句柄(`(HANDLE)-2`),指的是"当前线程",**不需要也不能** CloseHandle。
    fn GetCurrentThread() -> isize;
    fn SetThreadPriority(h: isize, p: i32) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(c: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32,
        cls: *const u16,
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
    fn DestroyWindow(h: isize) -> i32;
    fn DefWindowProcW(h: isize, m: u32, w: usize, l: isize) -> isize;
    fn ShowWindow(h: isize, cmd: i32) -> i32;
    fn SetWindowPos(h: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn SetTimer(h: isize, id: usize, ms: u32, proc: isize) -> usize;
    fn PostQuitMessage(code: i32);
    fn GetForegroundWindow() -> isize;
    fn GetWindowThreadProcessId(h: isize, pid: *mut u32) -> u32;
    fn GetGUIThreadInfo(tid: u32, info: *mut GuiThreadInfo) -> i32;
    fn ClientToScreen(h: isize, pt: *mut Point) -> i32;
    fn GetCursorPos(pt: *mut Point) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn SystemParametersInfoW(action: u32, param: u32, pv: *mut core::ffi::c_void, winini: u32) -> i32;
    fn SetProcessDPIAware() -> i32;
    fn SetProcessDpiAwarenessContext(ctx: isize) -> i32;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn GetMessageW(m: *mut Msg, h: isize, a: u32, b: u32) -> i32;
    fn TranslateMessage(m: *const Msg) -> i32;
    fn DispatchMessageW(m: *const Msg) -> isize;
    fn GetDC(h: isize) -> isize;
    fn GetDeviceCaps(hdc: isize, index: i32) -> i32;
    fn ReleaseDC(h: isize, hdc: isize) -> i32;
    fn BeginPaint(h: isize, ps: *mut PaintStruct) -> isize;
    fn EndPaint(h: isize, ps: *const PaintStruct) -> i32;
    fn UpdateLayeredWindow(
        hwnd: isize,
        hdc_dst: isize,
        pt_dst: *mut Point,
        size: *mut Size,
        hdc_src: isize,
        pt_src: *mut Point,
        color_key: u32,
        blend: *const BlendFunction,
        flags: u32,
    ) -> i32;
    fn GetWindowLongW(h: isize, idx: i32) -> i32;
    fn SetWindowLongW(h: isize, idx: i32, val: i32) -> i32;
    fn SetCapture(h: isize) -> isize;
    fn ReleaseCapture() -> i32;
    fn LoadCursorW(inst: isize, name: usize) -> isize;
    fn SetCursor(hc: isize) -> isize;
}

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

static HIGH_RES_TIMER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn set_high_res_timer(enable: bool) {
    let prev = HIGH_RES_TIMER.swap(enable, Ordering::Relaxed);
    if prev != enable {
        unsafe {
            if enable {
                timeBeginPeriod(1);
            } else {
                timeEndPeriod(1);
            }
        }
    }
}

// ------------------------------------------------------------------ 共享状态
/// 边缘光的颜色方案。
///
/// 为什么需要它：白色在深色背景上叠 42% 只是把背景提亮成**灰色雾** ——
/// 用户反馈"看不太清楚"，但再加亮就变成一片白，更难看。
/// 饱和色在同样的 alpha 下辨识度高得多（色彩对比 > 明度对比）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum EdgeTint {
/// 纯白（原来的方案）
#[default]
White,
/// 彩虹：四条边各一个色相，绕屏幕转一圈
Rainbow,
/// 暖橙：和 HUD 的琥珀色一致
Amber,
/// 青蓝：冷色调，夜里看不刺眼
Cyan,
/// 极光：青紫渐变，最"科技感"
Aurora,
}

impl EdgeTint {
pub fn from_conf(s: &str) -> EdgeTint {
    match s {
        "rainbow" => EdgeTint::Rainbow,
        "amber" => EdgeTint::Amber,
        "cyan" => EdgeTint::Cyan,
        "aurora" => EdgeTint::Aurora,
        _ => EdgeTint::White,
    }
}
pub fn as_conf(self) -> &'static str {
    match self {
        EdgeTint::White => "white",
        EdgeTint::Rainbow => "rainbow",
        EdgeTint::Amber => "amber",
        EdgeTint::Cyan => "cyan",
        EdgeTint::Aurora => "aurora",
    }
}
pub fn label(self) -> &'static str {
    match self {
        EdgeTint::White => "白光",
        EdgeTint::Rainbow => "彩虹光",
        EdgeTint::Amber => "暖橙光",
        EdgeTint::Cyan => "青蓝光",
        EdgeTint::Aurora => "极光（青紫）",
    }
}
}
/// 流光光晕（Ego 同款柔和漫射光晕）的配色方案。
///
/// 默认「电光蓝」对齐 Ego Lite 经典蓝色流光；另提供七套氛围配色（含赛博霓虹等）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum HaloTint {
    /// 电光蓝（默认）：#007aff / #38bdf8 / #1e40af
    #[default]
    Blue,
    /// 天青水色：#0284c7 / #22d3ee / #0369a1
    Cyan,
    /// 蓝紫极光：#3b82f6 / #c084fc / #1d4ed8
    Violet,
    /// Siri 绚烂：#38bdf8 / #f472b6 / #6366f1
    Siri,
    /// 流光彩虹：全色相缓慢流转
    Rainbow,
    /// 赛博霓虹：#ff1e8a / #00f0ff / #4c1d95（双霓虹巡游）
    Cyberpunk,
    /// 翡翠幽光：#10b981 / #6ee7b7 / #065f46
    Emerald,
    /// 落日熔金：#f97316 / #fb7185 / #9a3412
    Sunset,
}

impl HaloTint {
    pub fn from_conf(s: &str) -> HaloTint {
        match s.trim().to_ascii_lowercase().as_str() {
            "cyan" => HaloTint::Cyan,
            "violet" | "aurora" => HaloTint::Violet,
            "siri" => HaloTint::Siri,
            "rainbow" => HaloTint::Rainbow,
            "cyberpunk" | "cyber" | "neon" => HaloTint::Cyberpunk,
            "emerald" | "green" | "mint" => HaloTint::Emerald,
            "sunset" | "gold" | "amber" => HaloTint::Sunset,
            _ => HaloTint::Blue,
        }
    }
    pub fn as_conf(self) -> &'static str {
        match self {
            HaloTint::Blue => "blue",
            HaloTint::Cyan => "cyan",
            HaloTint::Violet => "violet",
            HaloTint::Siri => "siri",
            HaloTint::Rainbow => "rainbow",
            HaloTint::Cyberpunk => "cyberpunk",
            HaloTint::Emerald => "emerald",
            HaloTint::Sunset => "sunset",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            HaloTint::Blue => "电光蓝（默认）",
            HaloTint::Cyan => "天青水色",
            HaloTint::Violet => "蓝紫极光",
            HaloTint::Siri => "Siri 绚烂",
            HaloTint::Rainbow => "流光彩虹",
            HaloTint::Cyberpunk => "赛博霓虹",
            HaloTint::Emerald => "翡翠幽光",
            HaloTint::Sunset => "落日熔金",
        }
    }
    pub fn palette(self) -> ((f32, f32, f32), (f32, f32, f32), (f32, f32, f32)) {
        // (primary, highlight, deep)
        match self {
            HaloTint::Blue => (
                (0.0 / 255.0, 122.0 / 255.0, 255.0 / 255.0),
                (56.0 / 255.0, 189.0 / 255.0, 248.0 / 255.0),
                (30.0 / 255.0, 64.0 / 255.0, 175.0 / 255.0),
            ),
            HaloTint::Cyan => (
                (2.0 / 255.0, 132.0 / 255.0, 199.0 / 255.0),
                (34.0 / 255.0, 211.0 / 255.0, 238.0 / 255.0),
                (3.0 / 255.0, 105.0 / 255.0, 161.0 / 255.0),
            ),
            HaloTint::Violet => (
                (59.0 / 255.0, 130.0 / 255.0, 246.0 / 255.0),
                (192.0 / 255.0, 132.0 / 255.0, 252.0 / 255.0),
                (29.0 / 255.0, 78.0 / 255.0, 216.0 / 255.0),
            ),
            HaloTint::Siri => (
                (56.0 / 255.0, 189.0 / 255.0, 248.0 / 255.0),
                (244.0 / 255.0, 114.0 / 255.0, 182.0 / 255.0),
                (99.0 / 255.0, 102.0 / 255.0, 241.0 / 255.0),
            ),
            HaloTint::Rainbow => (
                (0.0 / 255.0, 168.0 / 255.0, 255.0 / 255.0),
                (120.0 / 255.0, 224.0 / 255.0, 255.0 / 255.0),
                (168.0 / 255.0, 85.0 / 255.0, 247.0 / 255.0),
            ),
            HaloTint::Cyberpunk => (
                (255.0 / 255.0, 30.0 / 255.0, 138.0 / 255.0),
                (0.0 / 255.0, 240.0 / 255.0, 255.0 / 255.0),
                (76.0 / 255.0, 29.0 / 255.0, 149.0 / 255.0),
            ),
            HaloTint::Emerald => (
                (16.0 / 255.0, 185.0 / 255.0, 129.0 / 255.0),
                (110.0 / 255.0, 231.0 / 255.0, 183.0 / 255.0),
                (6.0 / 255.0, 95.0 / 255.0, 70.0 / 255.0),
            ),
            HaloTint::Sunset => (
                (249.0 / 255.0, 115.0 / 255.0, 22.0 / 255.0),
                (251.0 / 255.0, 113.0 / 255.0, 133.0 / 255.0),
                (154.0 / 255.0, 52.0 / 255.0, 18.0 / 255.0),
            ),
        }
    }
}

/// 流光光晕形态参数（用户实调定稿的默认值）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlowConfig {
    pub blur_radius: f32,
    pub flow_speed: f32,
    pub peak_thickness: f32,
    pub valley_thickness: f32,
    pub dynamics: f32,
    pub opacity: f32,
    pub rim_opacity: f32,
}

pub const GLOW_CONFIG: GlowConfig = GlowConfig {
    blur_radius: 52.0,
    flow_speed: 0.5,
    peak_thickness: 33.0,
    valley_thickness: 12.0,
    dynamics: 0.80,
    opacity: 0.80,
    rim_opacity: 0.20,
};

pub const HALO_BLUR_MIN: f32 = 16.0;
pub const HALO_BLUR_MAX: f32 = 96.0;
pub const HALO_SPEED_MIN: f32 = 0.1;
pub const HALO_SPEED_MAX: f32 = 3.0;
pub const HALO_THICKNESS_MIN: f32 = 4.0;
pub const HALO_THICKNESS_MAX: f32 = 90.0;

/// 悬浮条 / 悬浮球二选一。
///
/// 为什么是互斥的：两个都画会互相压、抢视线。用户明确要求「一次最多只能显示一个」。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Mode {
    /// 细条（已停用注释：仅保留变体供旧测试兼容）
    #[default]
    Bar,
    /// 悬浮声波：3 条流体弦波 + 亮白芯丝（无框无外晕）。
    Orb,
    /// 纯白流体圆环：中心全透明、仅保留一根微微发光并随声音柔和流变的纯白闭环。
    Ring,
    /// 屏幕边缘光（已停用注释：仅保留变体供旧测试兼容）
    Edge,
    /// 流光光晕：双层高斯漫射沿屏幕四边波浪式柔和流动（无硬边）。
    Halo,
}

/// 悬浮声波 / 白环的大小预设（小 / 中 / 大）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum IndicatorSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl IndicatorSize {
    pub fn from_conf(s: &str) -> IndicatorSize {
        match s.trim().to_ascii_lowercase().as_str() {
            "small" | "s" => IndicatorSize::Small,
            "large" | "l" => IndicatorSize::Large,
            _ => IndicatorSize::Medium,
        }
    }
    pub fn as_conf(self) -> &'static str {
        match self {
            IndicatorSize::Small => "small",
            IndicatorSize::Medium => "medium",
            IndicatorSize::Large => "large",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            IndicatorSize::Small => "小",
            IndicatorSize::Medium => "中",
            IndicatorSize::Large => "大",
        }
    }
    pub fn idx(self) -> usize {
        match self {
            IndicatorSize::Small => 0,
            IndicatorSize::Medium => 1,
            IndicatorSize::Large => 2,
        }
    }
    pub fn from_idx(i: usize) -> IndicatorSize {
        match i {
            0 => IndicatorSize::Small,
            2 => IndicatorSize::Large,
            _ => IndicatorSize::Medium,
        }
    }
}

/// 纯白流体圆环灵敏度预设（低 / 中 / 高）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum RingSens {
    Low,
    #[default]
    Medium,
    High,
}

impl RingSens {
    pub fn from_conf(s: &str) -> RingSens {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" | "l" | "0" | "低" => RingSens::Low,
            "high" | "h" | "2" | "高" => RingSens::High,
            _ => RingSens::Medium,
        }
    }
    pub fn as_conf(self) -> &'static str {
        match self {
            RingSens::Low => "low",
            RingSens::Medium => "medium",
            RingSens::High => "high",
        }
    }
    pub fn idx(self) -> usize {
        match self {
            RingSens::Low => 0,
            RingSens::Medium => 1,
            RingSens::High => 2,
        }
    }
    pub fn from_idx(i: usize) -> RingSens {
        match i {
            0 => RingSens::Low,
            2 => RingSens::High,
            _ => RingSens::Medium,
        }
    }
}

#[derive(Clone, Default)]
struct State {
    visible: bool,
    mode: Mode,
    status: String,
    text: String,
    seq: u64,
    at: Option<(i32, i32)>,
    /// 悬浮声波 / 白环：实时原始音量、自适应底噪/峰值 AGC 与归一化驱动（0..1）。
    orb_level_raw: f32,
    orb_level_norm: f32,
    orb_noise_floor: f32,
    orb_peak_agc: f32,
    orb_env: f32,
    orb_bass: f32,
    orb_mid: f32,
    orb_treble: f32,
    /// 悬浮声波/白环当前不透明度（0..1）：按住快捷键期间淡入并保持可见，松开后平滑淡出。
    orb_alpha: f32,
    orb_holding: bool,
    orb_last_voice: Option<std::time::Instant>,
    orb_preview_until: Option<std::time::Instant>,
    orb_fading: bool,
    /// 边缘光的当前强度（0..1），每帧朝 `edge_target` 缓动。
    edge_level: f32,
    edge_target: f32,
    edge_tint: EdgeTint,
}

/// 用户拖动后的声波位置（设计像素坐标 (x_dp, y_dp)）；None = 默认任务栏上方居中。
static ORB_POS: OnceLock<Mutex<Option<(i32, i32)>>> = OnceLock::new();
/// 拖动完成/双击归位后待落盘的新位置（外层 Some 表示有变更，内层 Option 表示坐标或 auto）。
static ORB_POS_DIRTY: OnceLock<Mutex<Option<Option<(i32, i32)>>>> = OnceLock::new();
/// 当前帧悬浮声波在屏幕上的物理像素矩形，供鼠标命中/拖动。
static CURRENT_ORB_RECT: OnceLock<Mutex<Option<Rect>>> = OnceLock::new();
static ORB_PASS_THROUGH: AtomicBool = AtomicBool::new(true);
static ORB_LAST_TICK: OnceLock<Mutex<std::time::Instant>> = OnceLock::new();

#[derive(Clone, Copy, Default)]
struct DragState {
    active: bool,
    start_cursor_x: i32,
    start_cursor_y: i32,
    start_panel_x: i32,
    start_panel_y: i32,
}
static ORB_DRAG: OnceLock<Mutex<DragState>> = OnceLock::new();

fn orb_pos_slot() -> &'static Mutex<Option<(i32, i32)>> {
    ORB_POS.get_or_init(|| Mutex::new(None))
}

fn orb_pos_dirty_slot() -> &'static Mutex<Option<Option<(i32, i32)>>> {
    ORB_POS_DIRTY.get_or_init(|| Mutex::new(None))
}

pub fn set_orb_pos(pos: Option<(i32, i32)>) {
    if let Ok(mut g) = orb_pos_slot().lock() {
        *g = pos;
    }
}

pub fn orb_pos() -> Option<(i32, i32)> {
    orb_pos_slot().lock().ok().and_then(|g| *g)
}

pub fn reset_orb_pos() {
    set_orb_pos(None);
    if let Ok(mut d) = orb_pos_dirty_slot().lock() {
        *d = Some(None);
    }
}

pub fn take_orb_pos_dirty() -> Option<Option<(i32, i32)>> {
    orb_pos_dirty_slot().lock().ok().and_then(|mut g| g.take())
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// 当前光色。用原子量而不是只放 State 里：
/// 托盘换色时不需要 seq 变化（不重定位），只要下一帧重画。
static EDGE_TINT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn tint_code(t: EdgeTint) -> u8 {
    match t {
        EdgeTint::White => 0,
        EdgeTint::Rainbow => 1,
        EdgeTint::Amber => 2,
        EdgeTint::Cyan => 3,
        EdgeTint::Aurora => 4,
    }
}

pub fn set_edge_tint(t: EdgeTint) {
    EDGE_TINT.store(tint_code(t), Ordering::Relaxed);
    if let Ok(mut g) = state().lock() {
        g.edge_tint = t;
    }
}

pub fn edge_tint() -> EdgeTint {
    match EDGE_TINT.load(Ordering::Relaxed) {
        1 => EdgeTint::Rainbow,
        2 => EdgeTint::Amber,
        3 => EdgeTint::Cyan,
        4 => EdgeTint::Aurora,
        _ => EdgeTint::White,
    }
}
static ORB_SIZE_IDX: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);
static RING_SIZE_IDX: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

pub fn orb_size_preset() -> IndicatorSize {
    IndicatorSize::from_idx(ORB_SIZE_IDX.load(Ordering::Relaxed) as usize)
}

pub fn set_orb_size(sz: IndicatorSize) {
    ORB_SIZE_IDX.store(sz.idx() as u8, Ordering::Relaxed);
    if let Ok(mut g) = state().lock() {
        g.seq += 1;
    }
}

pub fn ring_size_preset() -> IndicatorSize {
    IndicatorSize::from_idx(RING_SIZE_IDX.load(Ordering::Relaxed) as usize)
}

pub fn set_ring_size(sz: IndicatorSize) {
    RING_SIZE_IDX.store(sz.idx() as u8, Ordering::Relaxed);
    if let Ok(mut g) = state().lock() {
        g.seq += 1;
    }
}
static RING_SENS_IDX: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

pub fn ring_sens_preset() -> RingSens {
    RingSens::from_idx(RING_SENS_IDX.load(Ordering::Relaxed) as usize)
}

pub fn set_ring_sens(sens: RingSens) {
    RING_SENS_IDX.store(sens.idx() as u8, Ordering::Relaxed);
    if let Ok(mut g) = state().lock() {
        g.seq += 1;
    }
}

static HALO_TINT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static HALO_BLUR_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.blur_radius * 1000.0) as u32);
static HALO_SPEED_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.flow_speed * 1000.0) as u32);
static HALO_PEAK_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.peak_thickness * 1000.0) as u32);
static HALO_VALLEY_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.valley_thickness * 1000.0) as u32);
static HALO_DYNAMICS_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.dynamics * 1000.0) as u32);
static HALO_OPACITY_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.opacity * 1000.0) as u32);
static HALO_RIM_MILLI: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new((GLOW_CONFIG.rim_opacity * 1000.0) as u32);

pub fn halo_tint_code(t: HaloTint) -> u8 {
    match t {
        HaloTint::Blue => 0,
        HaloTint::Cyan => 1,
        HaloTint::Violet => 2,
        HaloTint::Siri => 3,
        HaloTint::Rainbow => 4,
        HaloTint::Cyberpunk => 5,
        HaloTint::Emerald => 6,
        HaloTint::Sunset => 7,
    }
}

pub fn set_halo_tint(t: HaloTint) {
    HALO_TINT.store(halo_tint_code(t), Ordering::Relaxed);
}

pub fn halo_tint() -> HaloTint {
    match HALO_TINT.load(Ordering::Relaxed) {
        1 => HaloTint::Cyan,
        2 => HaloTint::Violet,
        3 => HaloTint::Siri,
        4 => HaloTint::Rainbow,
        5 => HaloTint::Cyberpunk,
        6 => HaloTint::Emerald,
        7 => HaloTint::Sunset,
        _ => HaloTint::Blue,
    }
}

pub fn set_halo_config(c: GlowConfig) {
    let blur = c.blur_radius.clamp(HALO_BLUR_MIN, HALO_BLUR_MAX);
    let speed = c.flow_speed.clamp(HALO_SPEED_MIN, HALO_SPEED_MAX);
    let valley = c.valley_thickness.clamp(HALO_THICKNESS_MIN, HALO_THICKNESS_MAX);
    let peak = c.peak_thickness.clamp(valley, HALO_THICKNESS_MAX);
    let dynamics = c.dynamics.clamp(0.0, 1.0);
    let opacity = c.opacity.clamp(0.05, 1.0);
    let rim = c.rim_opacity.clamp(0.0, 1.0);
    HALO_BLUR_MILLI.store((blur * 1000.0).round() as u32, Ordering::Relaxed);
    HALO_SPEED_MILLI.store((speed * 1000.0).round() as u32, Ordering::Relaxed);
    HALO_PEAK_MILLI.store((peak * 1000.0).round() as u32, Ordering::Relaxed);
    HALO_VALLEY_MILLI.store((valley * 1000.0).round() as u32, Ordering::Relaxed);
    HALO_DYNAMICS_MILLI.store((dynamics * 1000.0).round() as u32, Ordering::Relaxed);
    HALO_OPACITY_MILLI.store((opacity * 1000.0).round() as u32, Ordering::Relaxed);
    HALO_RIM_MILLI.store((rim * 1000.0).round() as u32, Ordering::Relaxed);
}

pub fn halo_config() -> GlowConfig {
    let valley = (HALO_VALLEY_MILLI.load(Ordering::Relaxed) as f32 / 1000.0)
        .clamp(HALO_THICKNESS_MIN, HALO_THICKNESS_MAX);
    let peak = (HALO_PEAK_MILLI.load(Ordering::Relaxed) as f32 / 1000.0)
        .clamp(valley, HALO_THICKNESS_MAX);
    GlowConfig {
        blur_radius: (HALO_BLUR_MILLI.load(Ordering::Relaxed) as f32 / 1000.0)
            .clamp(HALO_BLUR_MIN, HALO_BLUR_MAX),
        flow_speed: (HALO_SPEED_MILLI.load(Ordering::Relaxed) as f32 / 1000.0)
            .clamp(HALO_SPEED_MIN, HALO_SPEED_MAX),
        peak_thickness: peak,
        valley_thickness: valley,
        dynamics: (HALO_DYNAMICS_MILLI.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.0, 1.0),
        opacity: (HALO_OPACITY_MILLI.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.05, 1.0),
        rim_opacity: (HALO_RIM_MILLI.load(Ordering::Relaxed) as f32 / 1000.0).clamp(0.0, 1.0),
    }
}
static LAST_SEQ: OnceLock<Mutex<u64>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 悬浮状态条的句柄。
///
/// `start()` 是**启动时就开**；`ensure()` 是**运行时按需开**（托盘开关用）。
/// 两者共用同一份全局窗口状态（`STATE`），所以重复调用不会创建第二个窗口 ——
/// 窗口线程已经活着时，`ensure()` 直接返回一个新句柄。
#[derive(Clone)]
pub struct Overlay {
    enabled: bool,
}

/// 窗口线程是否已经起来了（`start()`/`ensure()` 成功过一次就为 true）。
static THREAD_UP: AtomicBool = AtomicBool::new(false);

/// 窗口是否已经摆好。窗口**只 ShowWindow 一次**,之后永不 hide/show ——
/// 显隐靠 alpha(见 sync 的注释:分层窗口尺寸/显隐变化会让系统重建表面,而那份表面不回收)。
static WINDOW_PLACED: AtomicBool = AtomicBool::new(false);
/// 上一帧是不是"有内容"的帧。隐藏时据此补一帧全透明(否则最后那帧会留在屏幕上)。
static PAINTED: AtomicBool = AtomicBool::new(false);

impl Overlay {
    /// 运行时按需启动：已经起来就直接返回句柄，没起来就起一次。
    ///
    /// 为什么需要它：托盘里打开「悬浮条」只改了一个原子量，
    /// 而窗口是启动时决定要不要创建的 —— 不补这一条，开关点了永远没反应。
    pub fn ensure() -> Option<Overlay> {
        if THREAD_UP.load(Ordering::Relaxed) {
            return Some(Overlay { enabled: true });
        }
        Overlay::start()
    }

    pub fn start() -> Option<Overlay> {
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("ouuutput-overlay".into())
            .spawn(move || {
                // 体检报告 §5.2(线程优先级):这条线程只负责显示,降优先级 ——
                // 它晚一帧没人看得出来(悬浮条/边缘光本来就是提示性的),
                // 而识别线程晚一帧就是掉字。放在进消息循环之前设一次即可,
                // 保持普通优先级，避免 60fps 全屏烟雾动画被后台任务抢占造成掉帧
                unsafe {
                    SetThreadPriority(GetCurrentThread(), 0);
                }
                let hwnd = unsafe { create_window() };
                match hwnd {
                    Some(h) => {
                        unsafe {
                            SetTimer(h, 1, 15, 0);
                        }
                        let _ = tx.send(true);
                        unsafe {
                            let mut msg: Msg = std::mem::zeroed();
                            while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                                TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                            set_high_res_timer(false);
                        }
                    }
                    None => {
                        let _ = tx.send(false);
                    }
                }
            })
            .ok()?;
        match rx.recv_timeout(Duration::from_millis(2500)) {
            Ok(true) => {
                THREAD_UP.store(true, Ordering::Relaxed);
                Some(Overlay { enabled: true })
            }
            _ => None,
        }
    }

    pub fn show(&self, status: &str, text: &str) {
        self.show_at(None, status, text);
    }

    /// 切到「细条」模式并显示文字（不显示状态行 —— 用户要求撤掉）。
    ///
    /// 🔴 空文字时**不显示**，直接隐藏。
    ///
    /// 为什么必须这样：尾巴收完时主循环会调 `show_bar("")` 占位，等 final 回来。
    /// 早先它真的会画出一个空面板 —— 而且如果 final 因为打断/空结果没来，
    /// 这个空面板就**永远挂在屏幕上**（用户实测报过）。
    /// 空内容没有任何信息量，显示它只会碍事。
    pub fn show_bar(&self, text: &str) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            s.mode = Mode::Bar;
            s.status.clear();
            s.text = text.to_string();
            s.at = None;
            s.seq += 1;
            if text.is_empty() {
                s.visible = false;
            } else {
                s.visible = true;
            }
        }
    }

    /// 切到「悬浮声波」模式：按住热键期间淡入并保持可见，松手淡出。
    pub fn show_orb(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            if s.mode != Mode::Orb {
                s.orb_alpha = 0.0;
            }
            if s.orb_peak_agc <= 0.0 {
                s.orb_noise_floor = 0.0035;
                s.orb_peak_agc = 0.075;
            }
            s.visible = true;
            s.mode = Mode::Orb;
            s.status.clear();
            s.text.clear();
            s.at = None;
            s.orb_holding = true;
            s.orb_fading = false;
            s.orb_preview_until = None;
            s.orb_last_voice = None;
            s.orb_level_raw = 0.0;
            s.orb_level_norm = 0.0;
            s.seq += 1;
        }
    }

    /// 切到「纯白流体圆环」模式：按住热键期间淡入并保持可见，松手淡出。
    pub fn show_ring(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            if s.mode != Mode::Ring {
                s.orb_alpha = 0.0;
            }
            if s.orb_peak_agc <= 0.0 {
                s.orb_noise_floor = 0.0035;
                s.orb_peak_agc = 0.075;
            }
            s.visible = true;
            s.mode = Mode::Ring;
            s.status.clear();
            s.text.clear();
            s.at = None;
            s.orb_holding = true;
            s.orb_fading = false;
            s.orb_preview_until = None;
            s.orb_last_voice = None;
            s.orb_level_raw = 0.0;
            s.orb_level_norm = 0.0;
            s.seq += 1;
        }
    }

    /// 托盘切换/改配色/调大小预览悬浮声波：稳定显示 hold_ms 毫秒后平滑淡出。
    pub fn preview_orb(&self, hold_ms: u64) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            if s.mode != Mode::Orb {
                s.orb_alpha = 0.0;
            }
            s.visible = true;
            s.mode = Mode::Orb;
            s.status.clear();
            s.text.clear();
            s.at = None;
            s.orb_holding = false;
            s.orb_fading = false;
            s.orb_preview_until = Some(std::time::Instant::now() + Duration::from_millis(hold_ms));
            s.seq += 1;
        }
    }

    /// 托盘切换/调大小预览纯白流体圆环：稳定显示 hold_ms 毫秒后平滑淡出。
    pub fn preview_ring(&self, hold_ms: u64) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            if s.mode != Mode::Ring {
                s.orb_alpha = 0.0;
            }
            s.visible = true;
            s.mode = Mode::Ring;
            s.status.clear();
            s.text.clear();
            s.at = None;
            s.orb_holding = false;
            s.orb_fading = false;
            s.orb_preview_until = Some(std::time::Instant::now() + Duration::from_millis(hold_ms));
            s.seq += 1;
        }
    }

    /// 让悬浮声波或白环平滑淡出（松手或预览结束时调用）。
    pub fn fade_orb(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            if matches!(s.mode, Mode::Orb | Mode::Ring) {
                s.orb_holding = false;
                s.orb_fading = true;
                s.orb_preview_until = None;
                s.orb_last_voice = None;
                s.orb_level_raw = 0.0;
                s.orb_level_norm = 0.0;
                s.seq += 1;
            }
        }
    }

    /// 让纯白流体圆环平滑淡出（松手或预览结束时调用）。
    pub fn fade_ring(&self) {
        self.fade_orb();
    }

    /// 切到「屏幕边缘光」模式并开始亮起（录音开始）。
    ///
    /// 注意：这里**不直接设 edge_level**，只设目标值 ——
    /// 真正的亮度由窗口线程按帧缓动，否则会"啪"一下亮起来，很突兀。
    pub fn show_edge(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            s.visible = true;
            s.mode = Mode::Edge;
            s.status.clear();
            s.text.clear();
            s.at = None;
            s.edge_target = 1.0;
            s.seq += 1;
        }
    }

    /// 让边缘光淡出（松手后调用）。
    ///
    /// 不直接 hide：淡出要几百毫秒，期间窗口必须还在、还得继续重画。
    /// 亮度降到 0 时窗口线程自己会把它藏起来。
    pub fn fade_edge(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            s.edge_target = 0.0;
            s.seq += 1;
        }
    }

    /// 切到「流光光晕」模式并开始亮起（录音开始）。
    pub fn show_halo(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            if s.mode != Mode::Halo {
                s.edge_level = 0.0;
            }
            s.visible = true;
            s.mode = Mode::Halo;
            s.status.clear();
            s.text.clear();
            s.at = None;
            s.edge_target = 1.0;
            s.seq += 1;
        }
    }

    /// 让流光光晕淡出（松手后调用）。
    pub fn fade_halo(&self) {
        self.fade_edge();
    }

    /// 推一帧音量（0..1）给悬浮声波。
    ///
    /// 只在 Orb 模式下有意义；超过语音门限时自动触发淡入，静音持续后平滑淡出。
    pub fn push_level(&self, level: f32) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            push_level_into(&mut s, level);
        }
    }

    pub fn show_at(&self, at: Option<(i32, i32)>, status: &str, text: &str) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            s.visible = true;
            s.status = status.to_string();
            s.text = text.to_string();
            s.at = at;
            s.seq += 1;
        }
    }

    pub fn hide(&self) {
        if !self.enabled {
            return;
        }
        if let Ok(mut s) = state().lock() {
            s.visible = false;
            s.orb_alpha = 0.0;
            s.orb_env = 0.0;
            s.orb_fading = false;
            s.orb_preview_until = None;
            s.orb_last_voice = None;
            s.seq += 1;
        }
    }

    pub fn hide_after(&self, ms: u64) {
        if !self.enabled {
            return;
        }
        let cur = state().lock().map(|s| s.seq).unwrap_or(0);
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            let now = state().lock().map(|s| s.seq).unwrap_or(0);
            if now == cur {
                me.hide();
            }
        });
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}
fn push_level_into(s: &mut State, level: f32) {
    push_level_with_sens(s, level, ring_sens_preset());
}

fn push_level_with_sens(s: &mut State, level: f32, ring_sens: RingSens) {
    if !matches!(s.mode, Mode::Orb | Mode::Ring) {
        return;
    }
    let (gain, agc_floor, span_floor, gamma, gate_floor) = if s.mode == Mode::Ring {
        match ring_sens {
            RingSens::Low => (1.0f32, 0.048f32, 0.036f32, 0.58f32, ORB_VOICE_THRESH_MIN),
            RingSens::Medium => (2.2f32, 0.026f32, 0.018f32, 0.44f32, 0.0072f32),
            RingSens::High => (3.4f32, 0.016f32, 0.012f32, 0.36f32, 0.0065f32),
        }
    } else {
        (1.0f32, 0.045f32, 0.035f32, 0.55f32, ORB_VOICE_THRESH_MIN)
    };
    if s.orb_peak_agc <= 0.0 {
        s.orb_noise_floor = 0.0035;
        s.orb_peak_agc = agc_floor.max(0.036);
    }
    let raw = level.clamp(0.0, 1.0);
    let v = (raw * gain).clamp(0.0, 1.0);
    s.orb_level_raw = v;
    if v < s.orb_noise_floor {
        s.orb_noise_floor += (v - s.orb_noise_floor) * 0.25;
    } else if v < s.orb_noise_floor * 1.8 {
        s.orb_noise_floor += (v - s.orb_noise_floor) * 0.015;
    }
    s.orb_noise_floor = s.orb_noise_floor.clamp(0.0015, 0.045);
    let gate = (s.orb_noise_floor * 2.4).max(gate_floor);
    if v >= gate {
        s.orb_last_voice = Some(std::time::Instant::now());
        s.orb_peak_agc = (s.orb_peak_agc * 0.986).max(v).max(agc_floor);
    } else {
        s.orb_peak_agc = (s.orb_peak_agc * 0.992).max(agc_floor);
    }
    let floor_ref = gate * 0.42;
    let span = (s.orb_peak_agc - floor_ref).max(span_floor);
    let norm = ((v - floor_ref) / span).clamp(0.0, 1.0);
    s.orb_level_norm = norm.powf(gamma);
}

unsafe fn set_dpi_aware() {
    if SetProcessDpiAwarenessContext(-4isize) == 0 {
        SetProcessDPIAware();
    }
}

unsafe fn create_window() -> Option<isize> {
    set_dpi_aware();
    let inst = GetModuleHandleW(ptr::null());
    let cls = wide(OVERLAY_CLASS);
    let wc = WndClassW {
        style: CS_DBLCLKS,
        lpfn_wnd_proc: Some(wnd_proc),
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        h_instance: inst,
        h_icon: 0,
        h_cursor: 0,
        hbr_background: 0,
        menu_name: ptr::null(),
        class_name: cls.as_ptr(),
    };
    let atom = RegisterClassW(&wc);
    if atom == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(1410) {
            return None;
        }
    }
    let title = wide("");
    // 窗口**一次性铺满整屏**(物理像素)并且之后永不改尺寸、永不 hide:
    // 分层窗口的尺寸一变,系统就会重建它的表面,而那份表面在 NVIDIA 驱动上不回收
    // (见 sync 的注释:每说一句漏一块 32 MiB)。细条/悬浮球改成画在画布内的对应位置。
    // WS_EX_TRANSPARENT 保证这块全屏窗口不吃鼠标点击。
    let sw = GetSystemMetrics(0).max(1);
    let sh = GetSystemMetrics(1).max(1);
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        cls.as_ptr(),
        title.as_ptr(),
        WS_POPUP,
        0,
        0,
        sw,
        sh,
        0,
        0,
        inst,
        0,
    );
    if hwnd == 0 {
        return None;
    }
    Some(hwnd)
}

unsafe fn set_click_through(hwnd: isize, pass_through: bool) {
    let prev = ORB_PASS_THROUGH.swap(pass_through, Ordering::Relaxed);
    if prev == pass_through {
        return;
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
    let next = if pass_through {
        ex | (WS_EX_TRANSPARENT as i32)
    } else {
        ex & !(WS_EX_TRANSPARENT as i32)
    };
    if next != ex {
        SetWindowLongW(hwnd, GWL_EXSTYLE, next);
    }
}

unsafe fn cursor_screen_pos() -> Option<(i32, i32)> {
    let mut pt = Point::default();
    if GetCursorPos(&mut pt) != 0 {
        Some((pt.x, pt.y))
    } else {
        None
    }
}

unsafe extern "system" fn wnd_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_NCHITTEST => {
            let dragging = ORB_DRAG
                .get_or_init(|| Mutex::new(DragState::default()))
                .lock()
                .map(|d| d.active)
                .unwrap_or(false);
            if dragging {
                return HTCLIENT;
            }
            let can_hit = state()
                .lock()
                .map(|s| s.visible && matches!(s.mode, Mode::Orb | Mode::Ring) && s.orb_alpha > 0.02)
                .unwrap_or(false);
            if can_hit {
                let x = (lp as i32) as i16 as i32;
                let y = ((lp as i32) >> 16) as i16 as i32;
                let pr = CURRENT_ORB_RECT
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .ok()
                    .and_then(|g| *g);
                if let Some(r) = pr {
                    if x >= r.left && x <= r.right && y >= r.top && y <= r.bottom {
                        return HTCLIENT;
                    }
                }
            }
            HTTRANSPARENT
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_ERASEBKGND => 1, // Direct2D 自己画，不让系统擦
        WM_SETCURSOR => {
            let can_drag = state()
                .lock()
                .map(|s| s.visible && matches!(s.mode, Mode::Orb | Mode::Ring) && s.orb_alpha > 0.02)
                .unwrap_or(false);
            if can_drag {
                let hc = LoadCursorW(0, IDC_SIZEALL);
                if hc != 0 {
                    SetCursor(hc);
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_LBUTTONDOWN => {
            let panel_rect = CURRENT_ORB_RECT
                .get_or_init(|| Mutex::new(None))
                .lock()
                .ok()
                .and_then(|g| *g);
            let can_drag = state()
                .lock()
                .map(|s| s.visible && matches!(s.mode, Mode::Orb | Mode::Ring) && s.orb_alpha > 0.02)
                .unwrap_or(false);
            if can_drag {
                if let (Some(pr), Some((cx, cy))) = (panel_rect, cursor_screen_pos()) {
                    if cx >= pr.left && cx <= pr.right && cy >= pr.top && cy <= pr.bottom {
                        if let Ok(mut d) = ORB_DRAG.get_or_init(|| Mutex::new(DragState::default())).lock() {
                            *d = DragState {
                                active: true,
                                start_cursor_x: cx,
                                start_cursor_y: cy,
                                start_panel_x: pr.left,
                                start_panel_y: pr.top,
                            };
                        }
                        SetCapture(hwnd);
                        return 0;
                    }
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            let drag = ORB_DRAG
                .get_or_init(|| Mutex::new(DragState::default()))
                .lock()
                .map(|g| *g)
                .unwrap_or_default();
            if drag.active {
                if let Some((cx, cy)) = cursor_screen_pos() {
                    let sw = GetSystemMetrics(0).max(1);
                    let sh = GetSystemMetrics(1).max(1);
                    let cur_mode = state().lock().map(|s| s.mode).unwrap_or(Mode::Orb);
                    let (w, h) = indicator_size(cur_mode);
                    let nx = (drag.start_panel_x + (cx - drag.start_cursor_x)).clamp(4, (sw - w - 4).max(4));
                    let ny = (drag.start_panel_y + (cy - drag.start_cursor_y)).clamp(4, (sh - h - 4).max(4));
                    let s = scale().max(0.5);
                    let dp_x = (nx as f32 / s).round() as i32;
                    let dp_y = (ny as f32 / s).round() as i32;
                    set_orb_pos(Some((dp_x, dp_y)));
                }
                return 0;
            }
            0
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            let was_active = ORB_DRAG
                .get_or_init(|| Mutex::new(DragState::default()))
                .lock()
                .map(|mut d| {
                    let a = d.active;
                    d.active = false;
                    a
                })
                .unwrap_or(false);
            if was_active {
                if msg == WM_LBUTTONUP {
                    ReleaseCapture();
                }
                if let Some(pos) = orb_pos() {
                    if let Ok(mut dirty) = orb_pos_dirty_slot().lock() {
                        *dirty = Some(Some(pos));
                    }
                }
            }
            0
        }
        WM_LBUTTONDBLCLK => {
            let can_reset = state()
                .lock()
                .map(|s| s.visible && matches!(s.mode, Mode::Orb | Mode::Ring) && s.orb_alpha > 0.02)
                .unwrap_or(false);
            if can_reset {
                reset_orb_pos();
            }
            0
        }
        WM_PAINT => {
            // UpdateLayeredWindow 自己推位图，这里只需把窗口内容标脏
            let mut ps: PaintStruct = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let _ = hdc;
            EndPaint(hwnd, &ps);
            0
        }
        WM_TIMER => {
            sync(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 从共享状态同步窗口:内容、位置、显隐。
///
/// 🔴 窗口**尺寸恒定 = 整屏**,而且只在第一次 ShowWindow,之后永不 hide/show。
unsafe fn sync(hwnd: isize) {
    let (visible, seq, mode, status, text, at) = match state().lock() {
        Ok(s) => (
            s.visible,
            s.seq,
            s.mode,
            s.status.clone(),
            s.text.clone(),
            s.at,
        ),
        Err(_) => return,
    };

    let dragging = ORB_DRAG
        .get_or_init(|| Mutex::new(DragState::default()))
        .lock()
        .map(|d| d.active)
        .unwrap_or(false);

    // 边缘光/流光：按真实帧间隔缓动
    let mut edge_done = false;
    let is_fullscreen_anim = matches!(mode, Mode::Edge | Mode::Halo);
    if is_fullscreen_anim {
        let mut g = match state().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let target = g.edge_target;
        let cur = g.edge_level;
        let dt_ms = {
            let mut last = EDGE_LAST_TICK
                .get_or_init(|| Mutex::new(std::time::Instant::now()))
                .lock()
                .unwrap();
            let dt = last.elapsed().as_secs_f32() * 1000.0;
            *last = std::time::Instant::now();
            dt.clamp(1.0, 100.0)
        };
        let k = 1.0 - (1.0 - 0.06f32).powf(dt_ms / 16.0);
        let next = if (target - cur).abs() < 0.006 {
            target
        } else {
            cur + (target - cur) * k
        };
        g.edge_level = next;
        if target == 0.0 && next <= 0.01 {
            g.edge_level = 0.0;
            g.visible = false;
            edge_done = true;
        }
    }

    // 悬浮声波 / 白环（Mode::Orb | Mode::Ring）：按住热键淡入并持续可见，松手平滑淡出
    let mut orb_alpha_now = 0.0f32;
    let mut orb_target_now = 0.0f32;
    let mut orb_done = false;
    let is_indicator = matches!(mode, Mode::Orb | Mode::Ring);
    if is_indicator && visible {
        let mut g = match state().lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let dt_ms = {
            let mut last = ORB_LAST_TICK
                .get_or_init(|| Mutex::new(std::time::Instant::now()))
                .lock()
                .unwrap();
            let dt = last.elapsed().as_secs_f32() * 1000.0;
            *last = std::time::Instant::now();
            dt.clamp(1.0, 100.0)
        };
        let t_sec = halo_clock_secs() as f32;
        let (alpha_now, target_alpha, done) =
            tick_indicator_state(&mut g, dt_ms, t_sec, dragging, std::time::Instant::now());
        orb_alpha_now = alpha_now;
        orb_target_now = target_alpha;
        orb_done = done;
    }

    let visible = if edge_done || orb_done {
        match state().lock() {
            Ok(g) => g.visible,
            Err(_) => return,
        }
    } else {
        visible
    };

    let changed = {
        let mut last = LAST_SEQ.get_or_init(|| Mutex::new(u64::MAX)).lock().unwrap();
        if *last != seq {
            *last = seq;
            true
        } else {
            false
        }
    };

    let orb_active_anim = visible && is_indicator && (orb_alpha_now > 0.004 || orb_target_now > 0.0);
    set_high_res_timer(visible && (is_fullscreen_anim || orb_active_anim));
    set_click_through(hwnd, !(visible && is_indicator && (orb_alpha_now > 0.02 || dragging)));

    let edge_due = is_fullscreen_anim && {
        let fps = edge_fps().max(1);
        let budget_ms = if fps >= 60 { 11 } else { 28 };
        let budget = std::time::Duration::from_millis(budget_ms);
        let mut last = EDGE_LAST_PAINT
            .get_or_init(|| Mutex::new(std::time::Instant::now() - budget))
            .lock()
            .unwrap();
        if last.elapsed() >= budget {
            *last = std::time::Instant::now();
            true
        } else {
            false
        }
    };
    let repaint = changed || edge_due || (is_indicator && orb_alpha_now > 0.004);

    let sw = GetSystemMetrics(0);
    let sh = GetSystemMetrics(1);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let (w, h) = match mode {
        Mode::Bar => measure(&status, &text),
        Mode::Orb | Mode::Ring => indicator_size(mode),
        Mode::Edge | Mode::Halo => (sw, sh),
    };
    let (mut x, mut y) = match mode {
        Mode::Edge | Mode::Halo => (0, 0),
        Mode::Orb | Mode::Ring => match at {
            Some((ax, ay)) => (ax, ay),
            None => match orb_pos() {
                Some((dx, dy)) => (px(dx), px(dy)),
                None => anchor_pos_orb(w, h),
            },
        },
        Mode::Bar => match at {
            Some((ax, ay)) => (ax, ay),
            None => anchor_pos(w, h),
        },
    };

    // 屏幕边界钳制
    if x + w > sw {
        x = sw - w - 4;
    }
    if y + h > sh {
        y = sh - h - 4;
    }
    if x < 0 {
        x = 4;
    }
    if y < 0 {
        y = 4;
    }
    let panel = Rect {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    if let Ok(mut slot) = CURRENT_ORB_RECT.get_or_init(|| Mutex::new(None)).lock() {
        *slot = if visible && is_indicator { Some(panel) } else { None };
    }

    if !WINDOW_PLACED.load(Ordering::Relaxed) {
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, sw, sh, SWP_NOACTIVATE);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        WINDOW_PLACED.store(true, Ordering::Relaxed);
    }

    if !visible || (is_indicator && orb_alpha_now <= 0.004) {
        if PAINTED.load(Ordering::Relaxed) {
            paint(hwnd, &panel, false);
            PAINTED.store(false, Ordering::Relaxed);
        }
        return;
    }
    if repaint {
        paint(hwnd, &panel, true);
        PAINTED.store(true, Ordering::Relaxed);
    }
}

fn tick_indicator_state(
    g: &mut State,
    dt_ms: f32,
    t_sec: f32,
    dragging: bool,
    now: std::time::Instant,
) -> (f32, f32, bool) {
    let preview_on = match g.orb_preview_until {
        Some(until) if now < until => true,
        Some(_) => {
            g.orb_preview_until = None;
            if !g.orb_holding {
                g.orb_fading = true;
            }
            false
        }
        None => false,
    };
    let speaking = g
        .orb_last_voice
        .map(|t| now.saturating_duration_since(t).as_millis() <= ORB_SILENCE_HOLD_MS as u128)
        .unwrap_or(false);
    let held_active = g.orb_holding && !g.orb_fading;
    let target_alpha = if dragging || preview_on || held_active {
        1.0f32
    } else {
        0.0f32
    };
    let tau_alpha = if target_alpha > g.orb_alpha { 85.0 } else { 210.0 };
    let k_alpha = 1.0 - (-dt_ms / tau_alpha).exp();
    g.orb_alpha += (target_alpha - g.orb_alpha) * k_alpha;
    if target_alpha == 0.0 && g.orb_alpha < 0.005 {
        g.orb_alpha = 0.0;
    } else if target_alpha == 1.0 && (1.0 - g.orb_alpha) < 0.004 {
        g.orb_alpha = 1.0;
    }
    let mut orb_done = false;
    if g.orb_fading && g.orb_alpha == 0.0 && !dragging && !held_active {
        g.visible = false;
        g.orb_fading = false;
        orb_done = true;
    }

    let is_ring = g.mode == Mode::Ring;
    let raw_energy = if preview_on || (dragging && !speaking) {
        if is_ring {
            (0.30 + 0.16 * (t_sec * 2.1).sin().abs() + 0.08 * (t_sec * 3.7).cos().abs())
                .clamp(0.0, 1.0)
        } else {
            (0.36 + 0.26 * (t_sec * 3.1).sin().abs() + 0.16 * (t_sec * 7.3).cos().abs())
                .clamp(0.0, 1.0)
        }
    } else if held_active || g.orb_alpha > 0.01 {
        if speaking {
            if is_ring {
                (0.15 + 0.85 * g.orb_level_norm).clamp(0.0, 1.0)
            } else {
                (0.24 + 0.76 * g.orb_level_norm).clamp(0.0, 1.0)
            }
        } else {
            // 按着快捷键但暂未开口/句间停顿：保持轻柔活体呼吸，绝不消失
            if is_ring { 0.08 } else { 0.14 }
        }
    } else {
        0.0
    };
    let (tau_up, tau_down) = if is_ring { (55.0, 185.0) } else { (48.0, 150.0) };
    let tau_env = if raw_energy > g.orb_env { tau_up } else { tau_down };
    let k_env = 1.0 - (-dt_ms / tau_env).exp();
    g.orb_env += (raw_energy - g.orb_env) * k_env;
    let k_b = 1.0 - (-dt_ms / 115.0).exp();
    let k_m = 1.0 - (-dt_ms / 70.0).exp();
    let k_t = 1.0 - (-dt_ms / 42.0).exp();
    g.orb_bass += (raw_energy * 0.92 - g.orb_bass) * k_b;
    g.orb_mid += ((raw_energy * (0.88 + 0.14 * (t_sec * 4.5).sin())).clamp(0.0, 1.0) - g.orb_mid) * k_m;
    g.orb_treble += ((raw_energy * (0.82 + 0.22 * (t_sec * 8.2).cos())).clamp(0.0, 1.0) - g.orb_treble) * k_t;

    (g.orb_alpha, target_alpha, orb_done)
}

/// 细条尺寸：**只有一行正文**，没有状态行、没有分隔线。
fn measure(_status: &str, text: &str) -> (i32, i32) {
    let estimate = |s: &str| -> i32 {
        s.chars()
            .map(|c| if c.is_ascii() { px(BAR_CHAR_ASCII) } else { px(BAR_CHAR_CJK) })
            .sum::<i32>()
    };
    let text_w = estimate(text);
    let w = (text_w + px(BAR_PAD_X) * 2).clamp(px(BAR_MIN_W), px(BAR_MAX_W));

    let usable = (w - px(BAR_PAD_X) * 2).max(px(80));
    let lines = if text_w <= usable {
        1
    } else {
        (text_w as f32 / usable as f32).ceil() as i32
    };
    let h = px(BAR_PAD_Y) * 2 + lines * px(BAR_LINE_H);
    (w, h.max(px(BAR_MIN_H)))
}

/// 悬浮声波 / 白环尺寸（设计像素按大中小预设缩放）。
fn orb_dims(sz: IndicatorSize) -> (i32, i32) {
    ORB_DIMS[sz.idx()]
}

fn ring_dims(sz: IndicatorSize) -> (i32, i32) {
    RING_DIMS[sz.idx()]
}

fn indicator_size(mode: Mode) -> (i32, i32) {
    let (dw, dh) = match mode {
        Mode::Ring => ring_dims(ring_size_preset()),
        _ => orb_dims(orb_size_preset()),
    };
    (px(dw), px(dh))
}

fn work_area_bottom(sh: i32) -> i32 {
    unsafe {
        let mut r: Rect = std::mem::zeroed();
        if SystemParametersInfoW(0x0030, 0, &mut r as *mut Rect as *mut _, 0) != 0 {
            r.bottom
        } else {
            sh
        }
    }
}

fn anchor_pos_orb(w: i32, h: i32) -> (i32, i32) {
    let sw = unsafe { GetSystemMetrics(0) };
    let sh = unsafe { GetSystemMetrics(1) };
    let x = (sw - w) / 2;
    let y = work_area_bottom(sh) - h - px(ORB_BOTTOM_GAP);
    (x.max(0), y.max(0))
}

/// 屏幕中下方、任务栏正上方（细条默认锚点）。
fn anchor_pos(w: i32, h: i32) -> (i32, i32) {
    let sw = unsafe { GetSystemMetrics(0) };
    let sh = unsafe { GetSystemMetrics(1) };
    let x = (sw - w) / 2;
    let y = work_area_bottom(sh) - h - BOTTOM_GAP;
    (x.max(0), y.max(0))
}

/// 贴着插入符定位；拿不到就贴鼠标。
///
/// ⚠️ **没有任何调用者**(全仓 grep `caret_pos` 只有这一处定义)。
/// 为什么还留着:定位那套算术(前台窗口 → 线程 → 插入符矩形 → 客户区转屏幕,
/// 退化成鼠标位置)是"跟随模式"的完整参考实现,删了以后要重推一遍;
/// 它也不参与任何编译产物之外的路径,`#[allow(dead_code)]` 明确标着"有意保留"。
/// 现在的正常路径是 **屏幕中下方固定位置**(见 `anchor_pos`)。
#[allow(dead_code)]
unsafe fn caret_pos(hwnd: isize, _w: i32, _h: i32) -> (i32, i32) {
    let mut anchor: Option<Point> = None;
    let fg = GetForegroundWindow();
    if fg != 0 {
        let tid = GetWindowThreadProcessId(fg, ptr::null_mut());
        let mut gi = GuiThreadInfo {
            cb_size: std::mem::size_of::<GuiThreadInfo>() as u32,
            ..Default::default()
        };
        if tid != 0 && GetGUIThreadInfo(tid, &mut gi) != 0 && gi.hwnd_caret != 0 {
            let mut pt = Point {
                x: gi.rc_caret.left,
                y: gi.rc_caret.bottom,
            };
            if ClientToScreen(gi.hwnd_caret, &mut pt) != 0 {
                anchor = Some(pt);
            }
        }
    }
    let mut pt = anchor.unwrap_or_else(|| {
        let mut p = Point { x: 0, y: 0 };
        GetCursorPos(&mut p);
        p
    });
    // 放在插入符右下方，留 6px 间隙
    pt.x += 6;
    pt.y += 6;
    let _ = hwnd;
    (pt.x, pt.y)
}

// ------------------------------------------------------------------ 绘制
/// Direct2D 绘制。
///
/// ⚠️ 这里刻意用**最小可用**的 D2D 调用面：
///   · 圆角矩形（真抗锯齿）
///   · 两层圆角矩形模拟阴影（D2D 没有内建 drop shadow，用偏移+低透明度叠）
///   · DirectWrite 画文字
///
/// 如果 D2D 初始化失败（老系统/被禁用），**降级成什么也不画** ——
/// 悬浮条是锦上添花，绝不能因为它把主功能搞挂。
/// 绘制入口：走 **UpdateLayeredWindow**（分层窗口的正确画法）。
///
/// 为什么不能用 BeginPaint + Window DC：实测（examples/gdiprobe.rs + 本轮 D2D 版）
/// 在这条路径上 FillRect / RoundRect / TextOut / D2D 绑 HDC **全都不落笔** ——
/// 窗口存在、IsWindowVisible=1，但屏幕上什么都没有。
/// WS_EX_LAYERED 窗口要么被 alpha 机制接管（UpdateLayeredWindow），要么全隐形。
///
/// UpdateLayeredWindow 的做法:
///   1. 建一个 32bpp 的 DIBSection(自己的画布,不碰屏幕 DC)——**整屏一块,常驻复用**
///   2. 把 D2D 绑到这个 DIB 的 HDC 上画(绑定矩形恒定 = 整块画布,内容用平移定位)
///   3. UpdateLayeredWindow 把整块位图推给窗口(带 per-pixel alpha)
///
/// 把当前模式的内容画进**整屏画布**,再一次性推给窗口。
///
/// `panel`:内容在画布(= 屏幕)坐标系里的矩形。细条/悬浮球只占其中一小块,
/// 边缘光是整块 —— 但**画布和窗口的尺寸永远不变**。
/// `draw=false`:推一帧全透明,用来"隐藏"(窗口一直留着,只是看不见)。
///
/// ## 🔴 脏矩形只清该清的(2026-10-31,AUDIT §5.1)
///
/// 以前这里是 `ptr::write_bytes(bits, 0, sw*sh*4)`:4K 下 33MB,文档实测 **7ms/帧**,
/// 而且只要在录音就每 16ms 付一次 —— 相当于一个逻辑核在空转(另一个核还要跑 ASR)。
/// 但一帧里真正非透明的只有「上一帧画过的那几块」和「本帧要画的那几块」:
/// 边缘光是沿边的四条光带(4K 下 ≈ 6MB),细条/悬浮球是一小块(几十 KB)。
/// 所以改成只清 **上一帧区域 ∪ 本帧区域**:
///   · 上一帧区域必须清 —— 画布是复用的(DIB 不重建,见 `Canvas`),本帧画到别处时
///     旧位置不会自己消失,屏幕上就留鬼影;
///   · 本帧区域也要清 —— 那里可能残留着更早的帧(内容移动过、模式换过),
///     不能假设它本来就透明,否则残留会一帧帧攒下去。
/// 不变量只有一句:**画布上非透明的像素一定落在 `DIRTY` 里**。
///
/// 清的是「哪些」由 `content_rects` 给(和真正绘制用的是同一份几何),
/// 窗口表面那边不用管:每帧仍把**整张**位图推给 `UpdateLayeredWindow`,
/// 画布上透明的部分自然把它盖成透明。
///
/// ⚠️ 尺寸唯一真源 = 屏幕:以前这里跟着「细条/悬浮球/边缘光」算 (w,h),
/// 于是每句话都在整屏 ↔ 小面板之间重建位图、同时窗口也跟着改尺寸 ——
/// 那正是"一句一块 32 MiB"的来源(见 sync 的注释)。
unsafe fn paint(hwnd: isize, panel: &Rect, draw: bool) {
    let (mode, _status, text, orb_alpha, orb_env, orb_bass, orb_mid, orb_treble) = match state().lock() {
        Ok(s) => (
            s.mode,
            s.status.clone(),
            s.text.clone(),
            s.orb_alpha,
            s.orb_env,
            s.orb_bass,
            s.orb_mid,
            s.orb_treble,
        ),
        Err(_) => return,
    };
    let sw = GetSystemMetrics(0);
    let sh = GetSystemMetrics(1);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let w = (panel.right - panel.left).max(0);
    let h = (panel.bottom - panel.top).max(0);
    if w <= 0 || h <= 0 {
        return;
    }
    // 绘制函数按「内容自己的坐标系」画(左上角 = 0,0),位置由 with_target 里的平移给出。
    let layout = Rect {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    let offset = (panel.left as f32, panel.top as f32);
    // 画布 = 整屏(窗口就铺在 (0,0)),所以画布坐标 == 屏幕坐标
    let canvas = Rect {
        left: 0,
        top: 0,
        right: sw,
        bottom: sh,
    };

    // 本帧内容会落到画布上的区域(画布坐标 = 屏幕坐标)。
    // `draw == false` 是「隐藏」那一帧:本帧什么都不画,于是「本帧区域」是空集,
    // 下面的并集正好退化成「只把上一帧的残留抹掉」——正是隐藏要的效果。
    let halo_cfg_snap = halo_config();
    let scale_snap = scale().max(0.5);
    let cur = if draw {
        content_rects_with_cfg(mode, panel, sw, sh, &halo_cfg_snap, scale_snap)
    } else {
        [EMPTY_RECT; DIRTY_MAX]
    };
    // 画布:按**屏幕**尺寸缓存(整进程就这一块;屏幕变了才重建)。
    //
    // 这里只借一下 CANVAS 把 (mem_dc, bits) 拿出来就还回去 —— 后面的清零/绘制只用这两个值。
    // (画布本身归缓存管,拿裸指针出去不会被回收,Drop 只在尺寸变化或线程结束时跑。)
    let canvas_slot = CANVAS.with(|slot| unsafe {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().map(|c| (c.w, c.h)) != Some((sw, sh)) {
            // 先把 D2D 的绑定作废(它绑的是旧 DC),再丢旧画布
            RT.with(|rt| {
                if let Some(c) = rt.borrow_mut().as_mut() {
                    c.bound = None;
                }
            });
            *slot = Canvas::create(sw, sh);
            // 换了一块新内存:旧账本在新画布上毫无意义,直接作废
            // (否则下一帧会在新画布上空清一块从没画过的区域 —— 无害,但说明不清不楚)。
            // 顺手整块清一次:屏幕尺寸变化是极罕见事件,这一次全屏清零无所谓,
            // 换来的是不必去假设 CreateDIBSection 给的位图一定是零。
            if let Some(c) = slot.as_ref() {
                ptr::write_bytes(c.bits, 0, (sw * sh * 4) as usize);
            }
            DIRTY.with(|d| *d.borrow_mut() = [EMPTY_RECT; DIRTY_MAX]);
        }
        slot.as_ref().map(|c| (c.mem_dc, c.bits))
    });
    let Some((mem_dc, bits)) = canvas_slot else {
        return; // 画布造不出来:这一帧不画(账本原样留着,下一帧照上面的规则继续清)
    };

    // 把「上一帧区域」挪出来,顺手把「本帧区域」记进去 —— 下一帧要清的正是它。
    // 必须等确认这一帧画得成之后才动账本:提前记下而中途 return,账本就和画布对不上,
    // 那部分残留以后再也清不掉了。
    let prev = DIRTY.with(|d| std::mem::replace(&mut *d.borrow_mut(), cur));

    // 要抹成透明的区域 = **上一帧区域 ∪ 本帧区域**,合并进复用缓冲(稳态零分配)。
    CLEAR.with(|cs| {
        let mut buf = cs.borrow_mut();
        buf.clear();
        buf.extend(prev.iter().chain(cur.iter()).copied());
        let clear: &[Rect] = buf.as_slice();

        // 全透明打底 —— 只清这几块(以前是整屏 33MB,见函数开头的「脏矩形」)。
        for r in clear {
            clear_rect(bits, sw, sh, r);
        }

        if draw {
            match mode {
                Mode::Bar => d2d_paint_bar(mem_dc, &canvas, clear, &layout, offset, &text),
                Mode::Orb => paint_orb_silk_wave(
                    bits,
                    sw,
                    sh,
                    panel,
                    orb_alpha,
                    orb_env,
                    orb_bass,
                    orb_mid,
                    orb_treble,
                    scale_snap,
                ),
                Mode::Ring => paint_orb_white_ring(
                    bits,
                    sw,
                    sh,
                    panel,
                    orb_alpha,
                    orb_env,
                    scale_snap,
                ),
                Mode::Edge => {
                    let lv = state().lock().map(|g| g.edge_level).unwrap_or(0.0);
                    d2d_paint_edge(mem_dc, &canvas, clear, &layout, offset, lv);
                }
                Mode::Halo => {
                    let lv = state().lock().map(|g| g.edge_level).unwrap_or(0.0);
                    paint_halo_with_cfg(bits, sw, sh, lv, &halo_cfg_snap, scale_snap);
                }
            }
        }
    });

    // 推给窗口(带 alpha)。**尺寸恒等于整屏** → 系统不需要重建窗口表面。
    let mut pt_src = Point { x: 0, y: 0 };
    let mut sz = Size { cx: sw, cy: sh };
    let blend = BlendFunction {
        blend_op: 0, // AC_SRC_OVER
        blend_flags: 0,
        source_constant_alpha: 255,
        alpha_format: 1, // AC_SRC_ALPHA
    };
    let mut pt_dst = Point { x: 0, y: 0 }; // 窗口就摆在 (0,0),整屏
    UpdateLayeredWindow(
        hwnd,
        0,
        &mut pt_dst,
        &mut sz,
        mem_dc,
        &mut pt_src,
        0,
        &blend,
        2, // ULW_ALPHA
    );

    // 画布不在这里删:它归 CANVAS 缓存管(屏幕尺寸变了或线程退出时才回收)。
}

/// 本帧内容会覆盖到的矩形(画布坐标 = 屏幕坐标)—— 脏矩形的「本帧区域」。
///
/// · 细条 / 悬浮球:整个内容面板。D2D 是把内容**平移**进这块矩形里画的
///   (见 `with_target` 的 offset),圆角矩形/圆/文字都严格落在它内部。
/// · 边缘光:四条光带。这里直接复用 `edge_geometry` 的 band(和 `EdgeBrushes`
///   拿去 PushLayer 当 contentBounds 的是同一组数),几何以后改了也不会两边对不上。
///
/// 每块都往外放 1px:抗锯齿和驱动取整理论上都不会越出几何边界,但万一越出去,
/// 后果是屏幕上留一条**永远清不掉**的亮线 —— 多清 1px 的成本可以忽略,不值当赌。
///
/// 定长 4(悬浮球/细条只用第 0 个),没用到的槽位是空矩形,由空判断统一过滤。
fn content_rects_with_cfg(
    mode: Mode,
    panel: &Rect,
    sw: i32,
    sh: i32,
    halo_cfg: &GlowConfig,
    s: f32,
) -> [Rect; DIRTY_MAX] {
    let mut out = [EMPTY_RECT; DIRTY_MAX];
    match mode {
        Mode::Bar | Mode::Orb | Mode::Ring => out[0] = pad1(panel),
        Mode::Edge => {
            let spread = px(edge_spread()) as f32;
            for (i, g) in edge_geometry(sw as f32, sh as f32, spread).iter().enumerate() {
                let (l, t, r, b) = g.4;
                out[i] = pad1(&Rect {
                    left: l as i32,
                    top: t as i32,
                    right: r as i32,
                    bottom: b as i32,
                });
            }
        }
        Mode::Halo => {
            let reach = halo_reach_px_cfg(halo_cfg, sw, sh, s);
            let bands = [
                Rect { left: 0, top: 0, right: sw, bottom: reach },
                Rect { left: sw - reach, top: 0, right: sw, bottom: sh },
                Rect { left: 0, top: sh - reach, right: sw, bottom: sh },
                Rect { left: 0, top: 0, right: reach, bottom: sh },
            ];
            for (i, r) in bands.iter().enumerate() {
                out[i] = pad1(r);
            }
        }
    }
    out
}

#[cfg(test)]
fn content_rects(mode: Mode, panel: &Rect, sw: i32, sh: i32) -> [Rect; DIRTY_MAX] {
    content_rects_with_cfg(mode, panel, sw, sh, &halo_config(), scale().max(0.5))
}

/// 往外扩 1px(见 `content_rects` 的说明)。
fn pad1(r: &Rect) -> Rect {
    Rect {
        left: r.left - 1,
        top: r.top - 1,
        right: r.right + 1,
        bottom: r.bottom + 1,
    }
}

/// 把画布上的矩形 `r` 抹成全透明(按行 `write_bytes`,不是逐像素)。
///
/// `cw`/`ch` = 画布宽高。矩形先按画布裁一次:内容可以被屏幕边界钳过,
/// 屏幕也可能变小过,越界的行/列必须挡住(`write_bytes` 越界是踩内存,不是画错)。
unsafe fn clear_rect(bits: *mut u8, cw: i32, ch: i32, r: &Rect) {
    let l = r.left.clamp(0, cw);
    let t = r.top.clamp(0, ch);
    let rr = r.right.clamp(0, cw);
    let bt = r.bottom.clamp(0, ch);
    if rr <= l || bt <= t {
        return;
    }
    let row_bytes = ((rr - l) * 4) as usize;
    for y in t..bt {
        let off = (y as usize * cw as usize + l as usize) * 4;
        ptr::write_bytes(bits.add(off), 0, row_bytes);
    }
}

/// D2D 渲染目标,外加「当前绑到哪儿」——用来免得每帧都 BindDC。
struct RtCache {
    /// 工厂本身不用来画,只**持住**它:render target 是它的产物,留一份引用最稳
    /// (所以编译器会说它"never read" —— 那是有意的,不是漏删)。
    _factory: windows::Win32::Graphics::Direct2D::ID2D1Factory,
    rt: windows::Win32::Graphics::Direct2D::ID2D1DCRenderTarget,
    target: windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    /// 当前绑定:(hdc, left, top, right, bottom)。None = 还没绑/需要重绑。
    bound: Option<(isize, i32, i32, i32, i32)>,
}

/// 一帧画布:32bpp DIB + 装着它的内存 DC。
///
/// 🔴 为什么必须缓存(2026-10-30 实测):原来**每帧**都
/// `CreateCompatibleDC` + `CreateDIBSection(w*h*4)` + `DeleteObject`。
/// 边缘光模式下 w*h*4 就是整块屏幕(3840x2160 = 33 MB),60fps 下每帧
/// 都要系统给一块屏幕大小的内存再还回去;实测这一路上每次给的块在
/// 4K 下正好是 2^25 字节,而且**进程里会越攒越多**(一次听写一块,半天
/// 涨到 2 GB 以上,把整机提交额度吃光 —— 用户报的"pebrel 闪退 + 黑屏"
/// 就是它把 DWM 挤死的)。尺寸没变就没必要重建,留着复用。
struct Canvas {
    w: i32,
    h: i32,
    mem_dc: isize,
    dib: isize,
    bits: *mut u8,
    /// CreateCompatibleDC 自带的那张 1x1 位图,收尾时要换回去。
    old_bmp: isize,
}

impl Canvas {
    /// 造一块 w×h 的 32bpp 画布;任一步失败都自己清理干净并返回 None。
    unsafe fn create(w: i32, h: i32) -> Option<Canvas> {
        let bmi = BitmapInfo {
            header: BitmapInfoHeader {
                bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                bi_width: w,
                bi_height: -h, // 负 = 自上而下
                bi_planes: 1,
                bi_bit_count: 32,
                bi_compression: 0, // BI_RGB
                bi_size_image: (w * h * 4) as u32,
                ..Default::default()
            },
            colors: [0; 3],
        };
        let mem_dc = CreateCompatibleDC(0);
        if mem_dc == 0 {
            return None;
        }
        let mut bits: *mut u8 = ptr::null_mut();
        let dib = CreateDIBSection(mem_dc, &bmi, 0, &mut bits, 0, 0);
        if dib == 0 || bits.is_null() {
            DeleteDC(mem_dc);
            return None;
        }
        let old_bmp = SelectObject(mem_dc, dib);
        Some(Canvas { w, h, mem_dc, dib, bits, old_bmp })
    }

    /// 放掉画布。**绝不能**在 D2D 还绑着这个 DC 的时候调用 ——
    /// 调用的地方(尺寸变化/线程退出)都先丢了 RT 缓存。
    unsafe fn free(&self) {
        SelectObject(self.mem_dc, self.old_bmp);
        DeleteObject(self.dib);
        DeleteDC(self.mem_dc);
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe { self.free() };
    }
}

/// 一帧最多几块脏矩形:细条/悬浮球是 1 块,边缘光是 4 条光带。
const DIRTY_MAX: usize = 4;
/// 空矩形(`left == right`)—— 数组里没用到的槽位都用它,统一由空判断过滤掉,
/// 这样「本帧不画任何东西」不用换一种数据结构来表达。
const EMPTY_RECT: Rect = Rect {
    left: 0,
    top: 0,
    right: 0,
    bottom: 0,
};

thread_local! {
    /// D2D 渲染目标缓存(窗口线程专用;COM 对象不是 Send)。
    ///
    /// `bound` 记住**当前绑的是哪个 DC、哪个矩形**:DC 和矩形都没变就不必
    /// 再 `BindDC`(画布缓存之后 DC 每帧都一样了)。Direct2D 每次 BindDC
    /// 都会为绑上的 DC 重新准备一份内部表面,能省就省。
    static RT: std::cell::RefCell<Option<RtCache>> = const { std::cell::RefCell::new(None) };

    /// 每帧画布缓存(按尺寸复用)。
    static CANVAS: std::cell::RefCell<Option<Canvas>> = const { std::cell::RefCell::new(None) };

    /// 🔴 脏矩形的账本:画布上**当前还残留着非透明内容**的矩形(画布坐标 = 屏幕坐标)。
    ///
    /// 为什么会需要这本账:画布是**复用**的(DIB 不重建,见 `Canvas` 的注释),
    /// 上一帧画出去的东西不会自己消失 —— 本帧若不在同一位置重画,屏幕上就留鬼影。
    /// 完整的推导见 `paint` 里「清哪些」那段注释。
    static DIRTY: std::cell::RefCell<[Rect; DIRTY_MAX]> =
        const { std::cell::RefCell::new([EMPTY_RECT; DIRTY_MAX]) };

    /// 「上一帧区域 ∪ 本帧区域」的复用缓冲 —— 免得每帧为这几个矩形分配一次 Vec(Vec 稳态不扩容)。
    static CLEAR: std::cell::RefCell<Vec<Rect>> = const { std::cell::RefCell::new(Vec::new()) };
    /// 上一次 D2D 报错的时刻和内容。
    /// 窗口线程在设备丢失时会按 60fps 重试，不设限流会把控制台/日志刷爆。
    static LAST_D2D_ERR: std::cell::RefCell<Option<(std::time::Instant, String)>> =
        const { std::cell::RefCell::new(None) };
}

/// 记录 D2D 错误：同一条错误最多 2 秒输出一次。
fn log_d2d_err(stage: &str, e: &windows::core::Error) {
    LAST_D2D_ERR.with(|slot| {
        let mut slot = slot.borrow_mut();
        let msg = format!("{stage}: {e:?}");
        let now = std::time::Instant::now();
        let should = match slot.as_ref() {
            Some((at, prev)) => {
                prev != &msg || now.duration_since(*at) >= std::time::Duration::from_secs(2)
            }
            None => true,
        };
        if should {
            eprintln!("[ov] D2D {msg}");
            *slot = Some((now, msg));
        }
    });
}

/// 建一个绑到 DIB 上的 D2D render target,跑 `f`,收尾。
///
/// 抽出来是因为细条和悬浮球用的是**同一套** D2D 初始化 ——
/// 复制两遍的话,以后改 alpha 模式/像素格式要改两处,容易漏。
///
/// `clear`:本次要抹成透明的矩形(画布坐标)= 上一帧 ∪ 本帧的脏矩形,见 `paint`。
fn with_target(
    hdc: isize,
    canvas: &Rect,
    clear: &[Rect],
    offset: (f32, f32),
    f: impl FnOnce(&d2d_target::Target),
) {
    use windows::core::Interface;
    use windows::Foundation::Numerics::Matrix3x2;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;
    use windows::Win32::Graphics::Gdi::HDC;

    // 绑定矩形 = **整块画布**(恒定,由 paint 传进来)。
    //
    // 🔴 为什么绑整块画布、而不是绑内容所在的那块小矩形:绑定矩形一变,
    // Direct2D/驱动就要为新的绑定重新准备内部表面(4K 下 33 MB 一份),而且旧的
    // 不会回收 —— 实测一次听写漏一块。绑死整块画布、用平移把内容挪过去,绑定从不变。

    // 🔴 性能关键：factory 和 render target **必须复用**。
    //
    // 早先每次都新建，实测在 3840x2160 下 `draw` 阶段要 **247ms**（≈4fps）——
    // 用户看到的就是"边缘光啪一下出现"，因为淡入的十几帧被拖成了好几百毫秒。
    // 复用后同一段绘制降到毫秒级。
    //
    // 复用缓存；每帧只需要重新 BindDC（尺寸/位置可能变）
    RT.with(|slot| unsafe {
    let mut slot = slot.borrow_mut();
    let mut dead = false;
    // 「这一帧的目标里那份内部位图是新的、内容不可知」——要整块清一次(见下面的打底)。
    // 两种情况都算:一种是刚建的 target,另一种是这一帧重新 BindDC 了(换了 DC 或换了绑定矩形,
    // 对 D2D 来说内部位图同样作废)。
    let mut fresh = slot.is_none();
    // 这一帧要绑的 DC + 矩形:和上次一样就不必再 BindDC
    // (画布缓存之后 DC 每帧都相同,所以这句大多数帧都成立)。
    let want = (hdc, canvas.left, canvas.top, canvas.right, canvas.bottom);
    if slot.is_none() {
        let factory: d2d::ID2D1Factory =
            match d2d::D2D1CreateFactory(d2d::D2D1_FACTORY_TYPE_SINGLE_THREADED, None) {
                Ok(f) => f,
                Err(e) => {
                    log_d2d_err("D2D1CreateFactory", &e);
                    return;
                }
            };

        let props = d2d::D2D1_RENDER_TARGET_PROPERTIES {
            r#type: d2d::D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: d2dc::D2D1_PIXEL_FORMAT {
                format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                // 必须 PREMULTIPLIED：IGNORE 会让整块变不透明（圆角/半透明全丢）
                alphaMode: d2dc::D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            usage: d2d::D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: d2d::D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt: d2d::ID2D1DCRenderTarget = match factory.CreateDCRenderTarget(&props) {
            Ok(r) => r,
            Err(e) => {
                log_d2d_err("CreateDCRenderTarget", &e);
                return;
            }
        };

        let sub = RECT {
            left: canvas.left,
            top: canvas.top,
            right: canvas.right,
            bottom: canvas.bottom,
        };
        if let Err(e) = rt.BindDC(HDC(hdc), &sub) {
            log_d2d_err("BindDC(create)", &e);
            return;
        }
        let target: d2d::ID2D1RenderTarget = match rt.cast() {
            Ok(t) => t,
            Err(e) => {
                log_d2d_err("cast<ID2D1RenderTarget>", &e);
                return;
            }
        };
        *slot = Some(RtCache { _factory: factory, rt, target, bound: Some(want) });
        // 新目标 = 旧目标上的画刷作废(画刷绑在目标上,拿旧的去画会画不出来或直接报错)。
        // 只在**新建**这一支丢;单纯重新 BindDC 不动画刷对象。
        invalidate_dwrite_brush();
    }

    // 只有「绑的东西变了」才重新 BindDC —— DC 和矩形都没变就不动它。
    let need_bind = slot.as_ref().and_then(|c| c.bound) != Some(want);
    // 重新绑过之后,内部位图对这块 DC 来说是新的(内容和旧绑定无关)—— 按"新 target"处理。
    fresh |= need_bind;
    if need_bind {
        let sub = RECT {
            left: canvas.left,
            top: canvas.top,
            right: canvas.right,
            bottom: canvas.bottom,
        };
        if let Err(e) = slot.as_ref().unwrap().rt.BindDC(HDC(hdc), &sub) {
            log_d2d_err("BindDC", &e);
            dead = true;
        }
    }

    if !dead {
        let c = slot.as_ref().unwrap();
        // 🔴 设备丢失的唯一出口。
        //
        // GPU 驱动报错 / 睡眠唤醒后,Direct2D 设备会被重置:旧 target 的
        // EndDraw 返回 D2DERR_RECREATE_TARGET(BindDC 也可能直接失败)。
        // 以前这里只打印一行、继续用同一个死 target —— 之后每一帧都画进
        // 不存在的设备,DIB 保持全透明,用户看到的就是「彩虹边缘光突然
        // 消失,但语音输入一切正常」,只能重启进程。
        // 现在只要出错就丢弃缓存,下一帧用新 target 重建,最多丢一帧。
        c.target.BeginDraw();
        // 打底:把 `clear` 给的那几块抹成全透明 —— 就是「上一帧 ∪ 本帧」的脏矩形(见 paint)。
        //
        // 🔴 为什么不能像以前那样无条件清**整块**画布:4K 下 33MB/帧,是本次要省的主角。
        // 裁剪(PushAxisAlignedClip)压在**设平移之前**,所以这里给的是画布坐标;
        // 空矩形要跳过(D2D 对退化矩形会直接失败)。
        //
        // 刚建出来的 target 例外:它内部那份位图是新的,内容不可知,而它每帧都会
        // (在 EndDraw 时)整块贴回 DC —— 只要里面还留着"不可知"的像素,就会一直
        // 盖在我们的账本上。所以建新 target 的这一帧整块清一次,把
        // 「内部位图 = 全透明」这条前提立起来;之后每帧只清脏矩形就是完备的
        // (归纳:上一帧的非透明区域一定在 prev 里,而 prev 每帧都被清)。
        if fresh {
            c.target.Clear(Some(&color(0, 0, 0, 0.0)));
        } else {
            for r in clear {
                if r.left >= r.right || r.top >= r.bottom {
                    continue;
                }
                let clip = d2dc::D2D_RECT_F {
                    left: r.left as f32,
                    top: r.top as f32,
                    right: r.right as f32,
                    bottom: r.bottom as f32,
                };
                c.target
                    .PushAxisAlignedClip(&clip, d2d::D2D1_ANTIALIAS_MODE_ALIASED);
                c.target.Clear(Some(&color(0, 0, 0, 0.0)));
                c.target.PopAxisAlignedClip();
            }
        }
        // 内容整体平移到它在画布里的位置(细条/悬浮球不贴左上角)。
        // 平移而不是"把绑定矩形挪过去" —— 见函数开头的注释。
        c.target.SetTransform(&Matrix3x2::translation(offset.0, offset.1));
        f(&d2d_target::Target { target: &c.target });
        if let Err(e) = c.target.EndDraw(None, None) {
            log_d2d_err("EndDraw", &e);
            dead = true;
        } else {
            slot.as_mut().unwrap().bound = Some(want);
        }
    }
    if dead {
        *slot = None;
    }
    });
}

/// 把 render target 包一层，避免每个绘制函数都写一长串 windows crate 路径。
mod d2d_target {
    use windows::Win32::Graphics::Direct2D as d2d;

    pub struct Target<'a> {
        pub target: &'a d2d::ID2D1RenderTarget,
    }
}

/// 细条：**只有正文**，半透明圆角矩形。
///
/// 用户要求撤掉「录音中…」状态行 —— 状态交给悬浮球，细条只管文字。
fn d2d_paint_bar(hdc: isize, canvas: &Rect, clear: &[Rect], rc: &Rect, offset: (f32, f32), text: &str) {
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;

    with_target(hdc, canvas, clear, offset, |t| unsafe {
        let target = t.target;
        let w = (rc.right - rc.left) as f32;
        let h = (rc.bottom - rc.top) as f32;

        let body = d2d::D2D1_ROUNDED_RECT {
            rect: d2dc::D2D_RECT_F {
                left: 0.0,
                top: 0.0,
                right: w,
                bottom: h,
            },
            radiusX: px(BAR_RADIUS) as f32,
            radiusY: px(BAR_RADIUS) as f32,
        };
        if let Ok(b) = target.CreateSolidColorBrush(&argb(BAR_BG) as *const _, None) {
            target.FillRoundedRectangle(&body, &b);
        }
        if let Ok(b) = target.CreateSolidColorBrush(&argb(BAR_BORDER) as *const _, None) {
            target.DrawRoundedRectangle(&body, &b, 1.0, None);
        }

        draw_bar_text(target, &body.rect, text);
    });
}

/// 悬浮声波（方案 04 纯净版）：无黑底、无胶囊边框、无外圈光雾，仅保留中央 3 条流体弦波 + 亮白芯丝。
/// 左右两端平滑收束归零，说话淡入、停说淡出，支持改配色（仅改弦波自身色彩）与左键拖动位置。
fn orb_silk_palette(tint: HaloTint, t_sec: f32) -> [(f32, f32, f32); 3] {
    match tint {
        HaloTint::Rainbow => {
            let h = t_sec * 0.18;
            [
                hue_to_rgb(h.rem_euclid(1.0)),
                hue_to_rgb((h + 0.33).rem_euclid(1.0)),
                hue_to_rgb((h + 0.66).rem_euclid(1.0)),
            ]
        }
        HaloTint::Siri => [
            (56.0 / 255.0, 189.0 / 255.0, 248.0 / 255.0),
            (244.0 / 255.0, 114.0 / 255.0, 182.0 / 255.0),
            (163.0 / 255.0, 230.0 / 255.0, 53.0 / 255.0),
        ],
        HaloTint::Cyberpunk => [
            (0.0 / 255.0, 245.0 / 255.0, 212.0 / 255.0),
            (255.0 / 255.0, 42.0 / 255.0, 133.0 / 255.0),
            (168.0 / 255.0, 85.0 / 255.0, 247.0 / 255.0),
        ],
        HaloTint::Sunset => [
            (251.0 / 255.0, 191.0 / 255.0, 36.0 / 255.0),
            (244.0 / 255.0, 63.0 / 255.0, 94.0 / 255.0),
            (249.0 / 255.0, 115.0 / 255.0, 22.0 / 255.0),
        ],
        HaloTint::Emerald => [
            (110.0 / 255.0, 231.0 / 255.0, 183.0 / 255.0),
            (16.0 / 255.0, 185.0 / 255.0, 129.0 / 255.0),
            (6.0 / 255.0, 182.0 / 255.0, 212.0 / 255.0),
        ],
        HaloTint::Violet => [
            (192.0 / 255.0, 132.0 / 255.0, 252.0 / 255.0),
            (217.0 / 255.0, 70.0 / 255.0, 239.0 / 255.0),
            (99.0 / 255.0, 102.0 / 255.0, 241.0 / 255.0),
        ],
        HaloTint::Cyan => [
            (34.0 / 255.0, 211.0 / 255.0, 238.0 / 255.0),
            (45.0 / 255.0, 212.0 / 255.0, 191.0 / 255.0),
            (59.0 / 255.0, 130.0 / 255.0, 246.0 / 255.0),
        ],
        HaloTint::Blue => [
            (56.0 / 255.0, 189.0 / 255.0, 248.0 / 255.0),
            (59.0 / 255.0, 130.0 / 255.0, 246.0 / 255.0),
            (129.0 / 255.0, 140.0 / 255.0, 248.0 / 255.0),
        ],
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn paint_orb_silk_wave(
    bits: *mut u8,
    sw: i32,
    sh: i32,
    panel: &Rect,
    orb_alpha: f32,
    env: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    s: f32,
) {
    paint_orb_silk_wave_at(
        bits,
        sw,
        sh,
        panel,
        orb_alpha,
        env,
        bass,
        mid,
        treble,
        halo_clock_secs() as f32,
        halo_tint(),
        s,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn paint_orb_silk_wave_at(
    bits: *mut u8,
    sw: i32,
    sh: i32,
    panel: &Rect,
    orb_alpha: f32,
    env: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    t_sec: f32,
    tint: HaloTint,
    s: f32,
) {
    let orb_alpha = orb_alpha.clamp(0.0, 1.0);
    if orb_alpha <= 0.004 || bits.is_null() || sw <= 0 || sh <= 0 {
        return;
    }
    let pw = (panel.right - panel.left).max(0);
    let ph = (panel.bottom - panel.top).max(0);
    if pw <= 0 || ph <= 0 {
        return;
    }
    let x_lo = panel.left.clamp(0, sw);
    let x_hi = panel.right.clamp(0, sw);
    let y_lo = panel.top.clamp(0, sh);
    let y_hi = panel.bottom.clamp(0, sh);
    if x_hi <= x_lo || y_hi <= y_lo {
        return;
    }

    let s = s.max(0.5);
    let cols = orb_silk_palette(tint, t_sec);
    let cy = panel.top as f32 + ph as f32 * 0.5;
    let max_amp = ph as f32 * 0.44;
    let amp_drive = 0.08 + 0.92 * env.clamp(0.0, 1.0).sqrt();

    let ribbons = [
        (cols[0], 2.55f32, 5.2f32, 0.0f32, max_amp * (0.28 + bass.clamp(0.0, 1.0) * 0.72) * amp_drive),
        (cols[1], 3.75f32, -6.7f32, 1.75f32, max_amp * (0.25 + mid.clamp(0.0, 1.0) * 0.75) * amp_drive),
        (cols[2], 5.15f32, 8.1f32, 3.45f32, max_amp * (0.22 + treble.clamp(0.0, 1.0) * 0.78) * amp_drive),
    ];
    let core_amp = max_amp * (0.12 + env.clamp(0.0, 1.0) * 0.64) * amp_drive;
    let core_freq = 4.0f32;
    let core_speed = -9.0f32;

    let col_count = (x_hi - x_lo) as usize;
    // 栈复用（常见宽度 <= 1024px，超出才堆分配）：预计算每列各弦波的 y 与斜率法线缩放
    thread_local! {
        static WAVE_COLS: std::cell::RefCell<Vec<[f32; 9]>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    WAVE_COLS.with(|slot| {
        let mut buf = slot.borrow_mut();
        buf.clear();
        buf.reserve(col_count);
        let inv_pw = 1.0 / pw as f32;
        let pi = std::f32::consts::PI;
        let eval_wave = |u: f32, freq: f32, speed: f32, phase: f32, amp: f32| -> f32 {
            let s_u = (u * pi).sin().max(0.0);
            let env_u = s_u.powf(1.45);
            let osc = (u * pi * freq + t_sec * speed + phase).sin();
            cy + osc * env_u * amp
        };
        for px in x_lo..x_hi {
            let u = ((px - panel.left) as f32 + 0.5) * inv_pw;
            let taper = (u * pi).sin().max(0.0).powf(1.35);
            let u_next = (u + inv_pw).min(1.0);

            let y0 = eval_wave(u, ribbons[0].1, ribbons[0].2, ribbons[0].3, ribbons[0].4);
            let y0n = eval_wave(u_next, ribbons[0].1, ribbons[0].2, ribbons[0].3, ribbons[0].4);
            let n0 = 1.0 / (1.0 + (y0n - y0) * (y0n - y0)).sqrt();

            let y1 = eval_wave(u, ribbons[1].1, ribbons[1].2, ribbons[1].3, ribbons[1].4);
            let y1n = eval_wave(u_next, ribbons[1].1, ribbons[1].2, ribbons[1].3, ribbons[1].4);
            let n1 = 1.0 / (1.0 + (y1n - y1) * (y1n - y1)).sqrt();

            let y2 = eval_wave(u, ribbons[2].1, ribbons[2].2, ribbons[2].3, ribbons[2].4);
            let y2n = eval_wave(u_next, ribbons[2].1, ribbons[2].2, ribbons[2].3, ribbons[2].4);
            let n2 = 1.0 / (1.0 + (y2n - y2) * (y2n - y2)).sqrt();

            let yc = eval_wave(u, core_freq, core_speed, 0.5, core_amp);
            let ycn = eval_wave(u_next, core_freq, core_speed, 0.5, core_amp);
            let nc = 1.0 / (1.0 + (ycn - yc) * (ycn - yc)).sqrt();

            buf.push([taper, y0, n0, y1, n1, y2, n2, yc, nc]);
        }

        let inv_s = 1.0 / s;
        let sw_u = sw as usize;
        let curtain_base = 0.25 + env * 0.25;
        let strand_base = 0.92 + env * 0.08;
        let glow_base = 0.22 + env * 0.24;
        let half_h = ph as f32 * 0.5;
        let half_w = pw as f32 * 0.5;
        let cx_abs = panel.left as f32 + half_w;

        for py in y_lo..y_hi {
            let fy = py as f32 + 0.5;
            let dy_norm = (fy - cy) / half_h.max(1.0);
            let row_ptr = bits.add((py as usize) * sw_u * 4) as *mut u32;
            for (idx, px) in (x_lo..x_hi).enumerate() {
                let [taper, y0, n0, y1, n1, y2, n2, yc, nc] = *buf.get_unchecked(idx);
                let dx_norm = ((px as f32 + 0.5) - cx_abs) / half_w.max(1.0);
                let in_hit_ellipse = dx_norm * dx_norm + dy_norm * dy_norm <= 0.96;
                if taper <= 0.002 {
                    continue;
                }

                let mut acc_r = 0.0f32;
                let mut acc_g = 0.0f32;
                let mut acc_b = 0.0f32;
                let mut acc_a = 0.0f32;
                let mut shadow_a = 0.0f32;

                let ys = [(ribbons[0].0, y0, n0), (ribbons[1].0, y1, n1), (ribbons[2].0, y2, n2)];
                for (col, yk, nk) in ys {
                    // 1) 弦波与中心轴之间的半透明丝绸垂幕
                    let lo = yk.min(cy);
                    let hi = yk.max(cy);
                    let span = (hi - lo).max(0.5);
                    if fy >= lo - 1.0 && fy <= hi + 1.0 && span > 0.8 {
                        let aa_lo = (fy - (lo - 0.8)).clamp(0.0, 1.0);
                        let aa_hi = ((hi + 0.8) - fy).clamp(0.0, 1.0);
                        let curtain = aa_lo.min(aa_hi) * curtain_base * taper;
                        acc_r += col.0 * curtain;
                        acc_g += col.1 * curtain;
                        acc_b += col.2 * curtain;
                        acc_a = (acc_a + curtain * 0.72).min(1.0);
                    }

                    // 2) 弦波主体 + 紧贴波峰的柔和自发光（无外框/无大椭圆晕）
                    let perp_dp = (fy - yk).abs() * nk * inv_s;
                    if perp_dp < 5.5 {
                        let strand = smoothstep(1.55, 0.12, perp_dp) * strand_base * taper;
                        let self_glow = (-perp_dp * perp_dp * 0.24).exp() * glow_base * taper;
                        let w_c = strand + self_glow;
                        acc_r += col.0 * w_c;
                        acc_g += col.1 * w_c;
                        acc_b += col.2 * w_c;
                        acc_a = (acc_a + strand * 0.92 + self_glow * 0.55).min(1.0);
                        // 极微弱的紧贴丝线暗衬，保证在纯白 Word 上依然清晰锐利（绝不形成框）
                        shadow_a = shadow_a.max((-perp_dp * perp_dp * 0.42).exp() * 0.16 * taper);
                    }
                }

                // 3) 穿行中心的亮白载波细丝
                let perp_c = (fy - yc).abs() * nc * inv_s;
                if perp_c < 3.6 {
                    let c_line = smoothstep(1.12, 0.06, perp_c) * (0.72 + 0.28 * env) * taper;
                    let c_glow = (-perp_c * perp_c * 0.55).exp() * (0.20 + 0.24 * env) * taper;
                    let cw = c_line + c_glow;
                    acc_r += 1.0 * cw;
                    acc_g += 0.99 * cw;
                    acc_b += 0.98 * cw;
                    acc_a = (acc_a + c_line * 0.95 + c_glow * 0.55).min(1.0);
                    shadow_a = shadow_a.max((-perp_c * perp_c * 0.50).exp() * 0.14 * taper);
                }

                // 仅在有效透明度足够高时写色；拖动命中椭圆内保底 alpha=1
                let total_a = (acc_a.max(shadow_a) * orb_alpha).clamp(0.0, 1.0);
                let a_floor = if orb_alpha > 0.04 && in_hit_ellipse { 1u8 } else { 0u8 };
                let a_u8 = ((total_a * 255.0 + 0.5) as u8).max(a_floor);
                if a_u8 == 0 {
                    continue;
                }
                // 把略超 1.0 的叠加色温和压回 0..total_a，保留高光饱和芯同时满足预乘 BGRA 约束
                let max_c = acc_r.max(acc_g).max(acc_b);
                let norm = if max_c > 1.0 { 1.0 / max_c } else { 1.0 };
                let r_u8 = ((acc_r * norm * orb_alpha * 255.0 + 0.5).clamp(0.0, a_u8 as f32)) as u8;
                let g_u8 = ((acc_g * norm * orb_alpha * 255.0 + 0.5).clamp(0.0, a_u8 as f32)) as u8;
                let b_u8 = ((acc_b * norm * orb_alpha * 255.0 + 0.5).clamp(0.0, a_u8 as f32)) as u8;
                *row_ptr.add(px as usize) = ((a_u8 as u32) << 24)
                    | ((r_u8 as u32) << 16)
                    | ((g_u8 as u32) << 8)
                    | (b_u8 as u32);
            }
        }
    });
}

unsafe fn paint_orb_white_ring(
    bits: *mut u8,
    sw: i32,
    sh: i32,
    panel: &Rect,
    orb_alpha: f32,
    env: f32,
    s: f32,
) {
    paint_orb_white_ring_at(
        bits,
        sw,
        sh,
        panel,
        orb_alpha,
        env,
        halo_clock_secs() as f32,
        s,
    );
}

/// 纯白流体圆环（Mode::Ring）：无底色、无反色、中心 100% 透明穿透，
/// 仅保留一根微微发光、随声音柔和流体形变的纯白细丝闭环。
#[allow(clippy::too_many_arguments)]
unsafe fn paint_orb_white_ring_at(
    bits: *mut u8,
    sw: i32,
    sh: i32,
    panel: &Rect,
    orb_alpha: f32,
    env: f32,
    t_sec: f32,
    s: f32,
) {
    let orb_alpha = orb_alpha.clamp(0.0, 1.0);
    if orb_alpha <= 0.004 || bits.is_null() || sw <= 0 || sh <= 0 {
        return;
    }
    let pw = (panel.right - panel.left).max(0);
    let ph = (panel.bottom - panel.top).max(0);
    if pw <= 0 || ph <= 0 {
        return;
    }
    let x_lo = panel.left.clamp(0, sw);
    let x_hi = panel.right.clamp(0, sw);
    let y_lo = panel.top.clamp(0, sh);
    let y_hi = panel.bottom.clamp(0, sh);
    if x_hi <= x_lo || y_hi <= y_lo {
        return;
    }

    let s = s.max(0.5);
    let inv_s = 1.0 / s;
    let env_s = env.clamp(0.0, 1.0);
    let cx = panel.left as f32 + pw as f32 * 0.5;
    let cy = panel.top as f32 + ph as f32 * 0.5;
    let min_dim = pw.min(ph) as f32;
    let base_r = min_dim * 0.27;
    let amp = base_r * (0.038 + 0.22 * env_s);
    let breath = (t_sec * 1.65).sin() * base_r * 0.025 + env_s * base_r * 0.16;
    let hit_r = (base_r * 1.60).min(min_dim * 0.48);

    // 预计算 256 个极角采样点 (r_theta, norm_k)，每像素零三角函数
    const N: usize = 256;
    let mut lut = [(0.0f32, 1.0f32); N + 1];
    let two_pi = std::f32::consts::TAU;
    for i in 0..=N {
        let th = (i as f32 / N as f32) * two_pi;
        let w2 = (2.0 * th + t_sec * 1.35).sin() * 0.48;
        let w3 = (3.0 * th - t_sec * 1.85 + 1.1).sin() * 0.30;
        let w4 = (4.0 * th + t_sec * 2.45 + 2.4).sin() * 0.15;
        let w5 = (5.0 * th - t_sec * 3.0 + 0.7).sin() * 0.07 * env_s;
        let r_th = (base_r + breath + amp * (w2 + w3 + w4 + w5)).clamp(base_r * 0.62, min_dim * 0.45);
        let dr = amp
            * (2.0 * (2.0 * th + t_sec * 1.35).cos() * 0.48
                + 3.0 * (3.0 * th - t_sec * 1.85 + 1.1).cos() * 0.30
                + 4.0 * (4.0 * th + t_sec * 2.45 + 2.4).cos() * 0.15
                + 5.0 * (5.0 * th - t_sec * 3.0 + 0.7).cos() * 0.07 * env_s);
        let norm_k = r_th / (r_th * r_th + dr * dr).sqrt();
        lut[i] = (r_th, norm_k);
    }

    let sw_u = sw as usize;
    let core_boost = 0.86 + 0.14 * env_s;
    let glow_boost = 0.28 + 0.42 * env_s;
    for py in y_lo..y_hi {
        let dy = (py as f32 + 0.5) - cy;
        let row_ptr = bits.add((py as usize) * sw_u * 4) as *mut u32;
        for px in x_lo..x_hi {
            let dx = (px as f32 + 0.5) - cx;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > hit_r + 6.0 * s {
                continue;
            }
            let mut th = dy.atan2(dx);
            if th < 0.0 {
                th += two_pi;
            }
            let f_idx = (th / two_pi) * N as f32;
            let i0 = (f_idx as usize) & (N - 1);
            let frac = f_idx - (f_idx as usize) as f32;
            let (r0, k0) = *lut.get_unchecked(i0);
            let (r1, k1) = *lut.get_unchecked(i0 + 1);
            let r_th = r0 + (r1 - r0) * frac;
            let norm_k = k0 + (k1 - k0) * frac;
            let perp_dp = ((dist - r_th).abs() * norm_k) * inv_s;

            let (white_w, shadow_a) = if perp_dp < 6.8 {
                let core = smoothstep(1.38, 0.16, perp_dp) * core_boost;
                let glow = (-perp_dp * perp_dp * 0.15).exp() * glow_boost;
                let shadow = (-perp_dp * perp_dp * 0.55).exp() * 0.11;
                ((core + glow).min(1.0), shadow)
            } else {
                (0.0, 0.0)
            };

            let a_floor = if orb_alpha > 0.04 && dist <= hit_r { 1u8 } else { 0u8 };
            let total_a = (white_w.max(shadow_a) * orb_alpha).clamp(0.0, 1.0);
            let a_u8 = ((total_a * 255.0 + 0.5) as u8).max(a_floor);
            if a_u8 == 0 {
                continue;
            }
            let w_u8 = ((white_w * orb_alpha * 255.0 + 0.5).clamp(0.0, a_u8 as f32)) as u8;
            *row_ptr.add(px as usize) = ((a_u8 as u32) << 24)
                | ((w_u8 as u32) << 16)
                | ((w_u8 as u32) << 8)
                | (w_u8 as u32);
        }
    }
}

fn argb(c: u32) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
    let a = ((c >> 24) & 0xFF) as f32 / 255.0;
    let r = ((c >> 16) & 0xFF) as f32 / 255.0;
    let g = ((c >> 8) & 0xFF) as f32 / 255.0;
    let b = (c & 0xFF) as f32 / 255.0;
    windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F { r, g, b, a }
}

fn color(r: u8, g: u8, b: u8, a: f32) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
    windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

thread_local! {
    /// 边缘光的笔刷/图层缓存(窗口线程专用;COM 对象不是 Send)。
    static EDGE_BRUSHES: std::cell::RefCell<Option<EdgeBrushes>> =
        const { std::cell::RefCell::new(None) };
}

/// 把 `windows` crate 的 D2D 类型收一收,免得字段写成天书。
mod d2d_brush {
    pub use windows::Win32::Graphics::Direct2D::{
        ID2D1Layer as Layer, ID2D1LinearGradientBrush as LinearBrush,
    };
}

/// 边缘光的 D2D 对象集合:**整个进程只建一次**(画布尺寸或色调变了才重建)。
///
/// 🔴 为什么必须缓存(2026-10-30 实测,别改回去):`d2d_paint_edge` 原来**每帧**都
/// `CreateGradientStopCollection` + `CreateLinearGradientBrush`(4 条边 + 4 个蒙版),
/// 外加 `CreateLayer`。渐变刷/图层要占 **D2D+驱动侧的设备资源**,而设备侧资源池是
/// **按 32 MiB 段增长、只增不减**的 —— 实测每说一句话就长一段(4K 下正好 2^25 字节),
/// 半天 2 GB+,最后把整机提交额度吃光 → DWM 被 `STATUS_FATAL_MEMORY_EXHAUSTION`
/// 打死 → 全屏黑 + 程序闪退。受控实验口径(100 秒 / 10-12 句,同一进程内复现):
///   · 只画纯色矩形(无渐变、无图层)      → 0 块
///   · 完整绘制但跳过 PushLayer           → 每句一块
///   · 完整绘制但用纯色刷顶替色彩刷        → 每句一块
/// ⇒ 元凶是**每帧新建渐变刷**;图层与窗口尺寸都被证伪。
/// 现在:对象只建一次 —— **流动** = 平移色带刷的渐变几何,**亮度** = `SetOpacity`,
/// 于是每帧零资源创建。
struct EdgeBrushes {
    w: i32,
    h: i32,
    tint: u8,
    /// 四条边(上/右/下/左)。数组顺序与 `edge_geometry()` 一致。
    color: [d2d_brush::LinearBrush; 4],
    fade: [d2d_brush::LinearBrush; 4],
    layer: d2d_brush::Layer,
    /// 色带刷的几何基线(已按 ±1 个周长延长),每帧在此基础上平移。
    start: [(f32, f32); 4],
    end: [(f32, f32); 4],
    /// t 增大的方向(单位向量);平移量 = flow * per * dir。
    dir: [(f32, f32); 4],
    /// 每条边的光带矩形。
    band: [(f32, f32, f32, f32); 4],
}

/// 四条边的几何(与绘制顺序一致):起点、终点、t 区间、光带矩形、向内渐变起止、方向。
#[allow(clippy::type_complexity)]
fn edge_geometry(w: f32, h: f32, spread: f32) -> [((f32, f32), (f32, f32), f32, f32, (f32, f32, f32, f32), (f32, f32), (f32, f32), (f32, f32)); 4] {
    let per = 2.0 * (w + h);
    let tw = w / per;
    let th = h / per;
    [
        // 上(左→右)
        ((0.0, 0.0), (w, 0.0), 0.0, tw,
         (0.0, 0.0, w, spread), (0.0, 0.0), (0.0, spread), (1.0, 0.0)),
        // 右(上→下)
        ((w, 0.0), (w, h), tw, tw + th,
         (w - spread, 0.0, w, h), (w, 0.0), (w - spread, 0.0), (0.0, 1.0)),
        // 下(右→左)
        ((w, h), (0.0, h), tw + th, tw + th + tw,
         (0.0, h - spread, w, h), (0.0, h), (0.0, h - spread), (-1.0, 0.0)),
        // 左(下→上)
        ((0.0, h), (0.0, 0.0), tw + th + tw, 1.0,
         (0.0, 0.0, spread, h), (0.0, 0.0), (spread, 0.0), (0.0, -1.0)),
    ]
}

impl EdgeBrushes {
    /// 建一整套。任何一步失败返回 None(悬浮层降级成"不画",绝不影响主功能)。
    unsafe fn create(
        target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
        w: f32,
        h: f32,
        tint: EdgeTint,
    ) -> Option<EdgeBrushes> {
        use windows::Win32::Graphics::Direct2D as d2d;
        use windows::Win32::Graphics::Direct2D::Common as d2dc;

        let per = 2.0 * (w + h);
        let spread = px(edge_spread()) as f32;

        // 蒙版色标:边缘不透明 → 向内透明。**alpha 烘焙成 1**,
        // 整体亮度每帧用 SetOpacity 给 —— 这样这条色标集合是常量,只建一次。
        let fade_stops = [
            d2dc::D2D1_GRADIENT_STOP {
                position: 0.0,
                color: d2dc::D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            },
            d2dc::D2D1_GRADIENT_STOP {
                position: 1.0,
                color: d2dc::D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.0 },
            },
        ];
        let fade_coll = target
            .CreateGradientStopCollection(
                &fade_stops,
                d2d::D2D1_GAMMA_2_2,
                d2d::D2D1_EXTEND_MODE_CLAMP,
            )
            .ok()?;

        let mut colors = Vec::with_capacity(4);
        let mut fades = Vec::with_capacity(4);
        let mut starts = [(0.0f32, 0.0f32); 4];
        let mut ends = [(0.0f32, 0.0f32); 4];
        let mut dirs = [(0.0f32, 0.0f32); 4];
        let mut bands = [(0.0f32, 0.0f32, 0.0f32, 0.0f32); 4];

        for (i, g) in edge_geometry(w, h, spread).iter().enumerate() {
            let (s, e, t0, t1, band, f0, f1, dir) = *g;

            // 色带刷:色标覆盖 t ∈ [t0-1, t1+1](±1 个周长)—— 平移几何时颜色不会滑出边界。
            // flow 烘焙成 0,流动完全交给几何平移。
            const N: usize = 3 * 17;
            let span = (t1 - t0) + 2.0;
            let mut stops: [d2dc::D2D1_GRADIENT_STOP; N] = [d2dc::D2D1_GRADIENT_STOP {
                position: 0.0,
                color: d2dc::D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
            }; N];
            for (k, slot) in stops.iter_mut().enumerate() {
                let u = k as f32 / (N - 1) as f32;
                let (r, gg, b) = edge_color_at(tint, (t0 - 1.0) + span * u, 0.0);
                slot.position = u;
                slot.color = d2dc::D2D1_COLOR_F { r, g: gg, b, a: 1.0 };
            }
            let ccoll = target
                .CreateGradientStopCollection(
                    &stops,
                    d2d::D2D1_GAMMA_2_2,
                    d2d::D2D1_EXTEND_MODE_CLAMP,
                )
                .ok()?;

            // 几何按 ±1 个周长延长:1 个 t 单位 = per 像素(与边长的比例关系对四条边都成立)
            let sx = s.0 - dir.0 * per;
            let sy = s.1 - dir.1 * per;
            let ex = e.0 + dir.0 * per;
            let ey = e.1 + dir.1 * per;
            let color = target
                .CreateLinearGradientBrush(
                    &d2d::D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: d2dc::D2D_POINT_2F { x: sx, y: sy },
                        endPoint: d2dc::D2D_POINT_2F { x: ex, y: ey },
                    },
                    None,
                    &ccoll,
                )
                .ok()?;

            // 蒙版刷:几何固定(边缘 → 向内)
            let fade = target
                .CreateLinearGradientBrush(
                    &d2d::D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: d2dc::D2D_POINT_2F { x: f0.0, y: f0.1 },
                        endPoint: d2dc::D2D_POINT_2F { x: f1.0, y: f1.1 },
                    },
                    None,
                    &fade_coll,
                )
                .ok()?;

            colors.push(color);
            fades.push(fade);
            starts[i] = (sx, sy);
            ends[i] = (ex, ey);
            dirs[i] = dir;
            bands[i] = band;
        }

        let layer = target.CreateLayer(None).ok()?;
        Some(EdgeBrushes {
            w: w as i32,
            h: h as i32,
            tint: tint_code(tint),
            color: colors.try_into().ok()?,
            fade: fades.try_into().ok()?,
            layer,
            start: starts,
            end: ends,
            dir: dirs,
            band: bands,
        })
    }
}

/// 屏幕边缘光:四条边各画一道从边缘向内的渐变。
///
/// ## 怎么做出「流动的彩色」
///
/// 难点:颜色要**沿边缘**变化(彩虹),亮度要**向内**衰减(光带)——
/// 这两个方向互相垂直,一个线性渐变刷做不到。
///
/// 做法是 **PushLayer + opacityBrush**(蒙版):先用「边缘不透明 → 向内全透明」的渐变
/// 压入图层,再在图层里用**沿边缘的色带刷**填满整块矩形,最后 PopLayer。
///
/// ## 每帧只改属性,不建对象(见 EdgeBrushes 的注释)
///
/// · 流动 = 把色带刷的几何沿边平移(`SetStartPoint`/`SetEndPoint`)
/// · 亮度 = 蒙版刷的 `SetOpacity`
/// · 颜色变化 = 色带刷的色标只在**建一次**时烘焙(flow=0),之后靠平移
fn d2d_paint_edge(hdc: isize, canvas: &Rect, clear: &[Rect], rc: &Rect, offset: (f32, f32), level: f32) {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;

    if level <= 0.001 {
        return;
    }

    let tint = edge_tint();

    with_target(hdc, canvas, clear, offset, |t| unsafe {
        let target = t.target;
        let w = (rc.right - rc.left) as f32;
        let h = (rc.bottom - rc.top) as f32;

        // 呼吸:0.86~1.0 之间缓慢起伏。相位都在 0..1,见 edge_clock。
        let (breath_ph, flow_ph) = edge_clock();
        let breath = 0.86 + 0.14 * (breath_ph * std::f32::consts::TAU).sin();

        // 饱和色比白色"显眼"得多,所以不用提 alpha 就能看清 —— 反而要略降。
        let tint_boost = match tint {
            EdgeTint::White => 1.0,
            _ => 0.92,
        };
        let peak = edge_peak() * level * breath * tint_boost;
        if peak <= 0.002 {
            return;
        }

        EDGE_BRUSHES.with(|slot| {
            let mut slot = slot.borrow_mut();
            let stale = match slot.as_ref() {
                Some(b) => b.w != w as i32 || b.h != h as i32 || b.tint != tint_code(tint),
                None => true,
            };
            if stale {
                *slot = EdgeBrushes::create(target, w, h, tint);
            }
            let Some(b) = slot.as_ref() else {
                return; // 建不出来:这一帧不画边缘光(降级,不影响主功能)
            };

            let per = 2.0 * (w + h);
            let off = flow_ph * per; // 流动量(像素)
            for i in 0..4 {
                let (dx, dy) = b.dir[i];
                b.color[i].SetStartPoint(d2dc::D2D_POINT_2F {
                    x: b.start[i].0 - dx * off,
                    y: b.start[i].1 - dy * off,
                });
                b.color[i].SetEndPoint(d2dc::D2D_POINT_2F {
                    x: b.end[i].0 - dx * off,
                    y: b.end[i].1 - dy * off,
                });
                b.fade[i].SetOpacity(peak);
            }

            for i in 0..4 {
                let (l, tp, r, bt) = b.band[i];
                let rect = d2dc::D2D_RECT_F { left: l, top: tp, right: r, bottom: bt };
                let params = windows::Win32::Graphics::Direct2D::D2D1_LAYER_PARAMETERS {
                    contentBounds: rect,
                    geometricMask: std::mem::ManuallyDrop::new(None),
                    maskAntialiasMode:
                        windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    maskTransform: windows::Foundation::Numerics::Matrix3x2::identity(),
                    opacity: 1.0,
                    // ⚠️ 这里是**借用一个裸指针**,不是交出所有权:
                    // `opacityBrush` 在 C 结构里只是个指针,D2D 不会接管引用。
                    // 老代码写的是 `fbrush.clone().into()` —— clone 加的引用**永远没人 Release**
                    // (每边每帧一个),这正是「每说一句长一段 32 MiB 设备侧资源」的两个来源之一
                    // (另一个是每帧新建渐变刷,见 EdgeBrushes 的注释)。
                    // 这里用 from_raw 借一下:不加引用,ManuallyDrop 也不会去释放它。
                    opacityBrush: std::mem::ManuallyDrop::new(Some(
                        windows::Win32::Graphics::Direct2D::ID2D1Brush::from_raw(b.fade[i].as_raw())
                    )),
                    layerOptions: windows::Win32::Graphics::Direct2D::D2D1_LAYER_OPTIONS_NONE,
                };
                // 图层对象复用:每帧只 Push/Pop,不再 CreateLayer
                target.PushLayer(&params, &b.layer);
                target.FillRectangle(&rect, &b.color[i]);
                target.PopLayer();
            }
        });
    });
}
/// 按周长参数 `t`（0..1）和流动相位 `flow`（0..1）取颜色。
///
/// 单色方案忽略 `t`（整条边一个色）；彩虹/极光沿 `t` 连续变化，
/// 再叠加 `flow` 让整条彩带**沿周长滑动** —— 这就是"流动的彩色"。
/// `flow` 已经在 0..1，直接加上去即可（一整圈）。
fn edge_color_at(tint: EdgeTint, t: f32, flow: f32) -> (f32, f32, f32) {
    match tint {
        EdgeTint::White => (1.0, 1.0, 1.0),
        EdgeTint::Amber => (1.0, 0.62, 0.26),
        EdgeTint::Cyan => (0.28, 0.82, 1.0),
        EdgeTint::Rainbow => {
            // 整条彩虹绕屏幕一圈，随时间滑动
            hue_to_rgb(t + flow)
        }
        EdgeTint::Aurora => {
            // 极光：只在青(0.47)~紫(0.77)之间流动，比全色相彩虹柔和
            let u = (t + flow) * std::f32::consts::TAU;
            let hue = 0.62 + 0.15 * u.sin();
            // 再叠一层亮度起伏，像极光的"帘幕"
            let (r, g, b) = hue_to_rgb(hue);
            let v = 0.82 + 0.18 * (u * 1.7).sin();
            (r * v, g * v, b * v)
        }
    }
}

/// HSV(h, s=1, v=1) -> RGB。h 取 0..1。
fn hue_to_rgb(h: f32) -> (f32, f32, f32) {
    let h = (h.fract() + 1.0).fract() * 6.0;
    let i = h.floor() as i32;
    let f = h - i as f32;
    match i.rem_euclid(6) {
        0 => (1.0, f, 0.0),
        1 => (1.0 - f, 1.0, 0.0),
        2 => (0.0, 1.0, f),
        3 => (0.0, 1.0 - f, 1.0),
        4 => (f, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - f),
    }
}

// ------------------------------------------------------------------ 流光光晕 (Halo)

const HALO_CELL_DP: f32 = 7.0;

struct HaloCache {
    gw: usize,
    gh: usize,
    sw_lut: i32,
    s_lut: f32,
    pad_lut: usize,
    reach_lut: usize,
    dot_grid: Vec<[f32; 2]>,
    l1_u64: Vec<u64>,
    row_mix: Vec<u64>,
    w_lut: Vec<(u16, u16)>,
    dot_x_cell: Vec<i32>,
    dot_spacing_px: usize,
    dot_size_px: usize,
    full_runs: Vec<(u16, u16, u16)>,
    left_runs: Vec<(u16, u16, u16)>,
    right_runs: Vec<(u16, u16, u16)>,
}

fn build_gx_runs(gx_of_px: &[u16], x_lo: usize, x_hi: usize, out: &mut Vec<(u16, u16, u16)>) {
    out.clear();
    if x_lo >= x_hi {
        return;
    }
    let mut cur = gx_of_px[x_lo];
    let mut start = x_lo;
    for px in (x_lo + 1)..x_hi {
        let g = gx_of_px[px];
        if g != cur {
            out.push((cur, start as u16, px as u16));
            cur = g;
            start = px;
        }
    }
    out.push((cur, start as u16, x_hi as u16));
}

impl HaloCache {
    fn prepare(&mut self, gw: usize, gh: usize, sw: usize, pad_c: usize, reach_u: usize, s: f32) {
        let need = gw * gh;
        if self.gw != gw || self.gh != gh || self.dot_grid.len() != need {
            self.gw = gw;
            self.gh = gh;
            self.dot_grid.resize(need, [0.0; 2]);
            self.l1_u64.resize(need, 0);
            self.row_mix.resize(gw, 0);
        }
        unsafe {
            std::ptr::write_bytes(self.dot_grid.as_mut_ptr(), 0, need);
            std::ptr::write_bytes(self.l1_u64.as_mut_ptr(), 0, need);
        }
        if self.sw_lut != sw as i32
            || (self.s_lut - s).abs() > 1e-4
            || self.pad_lut != pad_c
            || self.reach_lut != reach_u
            || self.w_lut.len() != sw
        {
            self.sw_lut = sw as i32;
            self.s_lut = s;
            self.pad_lut = pad_c;
            self.reach_lut = reach_u;
            let inv_cell = 1.0 / (HALO_CELL_DP * s);
            let pad_f = pad_c as f32 - 0.5;
            let max_gx = (gw.saturating_sub(2)) as f32;
            self.w_lut.clear();
            self.w_lut.reserve(sw);
            let mut gx_of_px = Vec::with_capacity(sw);
            for px in 0..sw {
                let gxf = (px as f32 * inv_cell + pad_f).clamp(0.0, max_gx);
                let gx0 = gxf as usize;
                let fx = gxf - gx0 as f32;
                let w1 = (fx * 64.0 + 0.5) as u16;
                let w0 = 64 - w1;
                self.w_lut.push((w0, w1));
                gx_of_px.push(gx0 as u16);
            }
            let spacing_px = (8.0 * s).round().max(4.0) as usize;
            let dot_px = (2.0 * s).round().max(1.0) as usize;
            self.dot_spacing_px = spacing_px;
            self.dot_size_px = dot_px;
            self.dot_x_cell.clear();
            self.dot_x_cell.reserve(sw);
            for px in 0..sw {
                if (px % spacing_px) < dot_px {
                    self.dot_x_cell.push((px / spacing_px) as i32);
                } else {
                    self.dot_x_cell.push(-1);
                }
            }
            build_gx_runs(&gx_of_px, 0, sw, &mut self.full_runs);
            build_gx_runs(&gx_of_px, 0, reach_u, &mut self.left_runs);
            build_gx_runs(
                &gx_of_px,
                sw.saturating_sub(reach_u),
                sw,
                &mut self.right_runs,
            );
        }
    }
}

thread_local! {
    static HALO_CACHE: std::cell::RefCell<Option<HaloCache>> =
        const { std::cell::RefCell::new(None) };
}

fn halo_reach_dp(cfg: &GlowConfig) -> f32 {
    (1.15 * cfg.peak_thickness + 1.65 * cfg.blur_radius + 2.0 * HALO_CELL_DP).clamp(48.0, 260.0)
}

fn halo_reach_px_cfg(cfg: &GlowConfig, sw: i32, sh: i32, s: f32) -> i32 {
    let s = s.max(0.5);
    let reach_dp = halo_reach_dp(cfg);
    ((reach_dp * s).ceil() as i32).min(sw / 2).min(sh / 2).max(0)
}

fn halo_clock_secs() -> f64 {
    use std::sync::LazyLock;
    use std::time::Instant;
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    START.elapsed().as_secs_f64()
}

#[inline]
fn lerp3(a: (f32, f32, f32), b: (f32, f32, f32), t: f32) -> (f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t)
}

#[inline]
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
fn hash2i(ix: i32, iy: i32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(374_761_393)
        ^ (iy as u32).wrapping_mul(668_265_263);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0x00FF_FFFF) as f32 * (1.0 / 16_777_215.0)
}

#[inline]
fn vnoise2(x: f32, y: f32) -> f32 {
    let ix = x.floor() as i32;
    let iy = y.floor() as i32;
    let fx = x - ix as f32;
    let fy = y - iy as f32;
    let ux = fx * fx * (3.0 - 2.0 * fx);
    let uy = fy * fy * (3.0 - 2.0 * fy);
    let a = hash2i(ix, iy);
    let b = hash2i(ix + 1, iy);
    let c = hash2i(ix, iy + 1);
    let d = hash2i(ix + 1, iy + 1);
    let ab = a + (b - a) * ux;
    let cd = c + (d - c) * ux;
    ab + (cd - ab) * uy
}

/// 多倍频旋转 FBM 噪声（旋转八度消除轴向晶格感，保留烟雾卷曲轮廓）。
fn fbm_smoke(mut x: f32, mut y: f32, octaves: usize) -> f32 {
    let mut v = 0.0f32;
    let mut amp = 0.5f32;
    let mut total = 0.0f32;
    for _ in 0..octaves {
        v += amp * vnoise2(x, y);
        total += amp;
        let nx = (0.8 * x - 0.6 * y) * 2.02 + 11.3;
        let ny = (0.6 * x + 0.8 * y) * 2.02 + 7.7;
        x = nx;
        y = ny;
        amp *= 0.5;
    }
    v / total
}

/// 在整屏 DIB (`bits`, 32bpp premultiplied BGRA) 里绘制三层烟雾缭绕流光（域扭曲 FBM 烟雾 + 微点阵 + 高光细线）。
unsafe fn paint_halo_with_cfg(
    bits: *mut u8,
    sw: i32,
    sh: i32,
    level: f32,
    cfg: &GlowConfig,
    s: f32,
) {
    paint_halo_with_time_cfg(bits, sw, sh, level, halo_clock_secs(), cfg, s);
}

#[cfg(test)]
unsafe fn paint_halo_with_time(bits: *mut u8, sw: i32, sh: i32, level: f32, raw_secs: f64) {
    paint_halo_with_time_cfg(bits, sw, sh, level, raw_secs, &halo_config(), scale().max(0.5));
}

unsafe fn paint_halo_with_time_cfg(
    bits: *mut u8,
    sw: i32,
    sh: i32,
    level: f32,
    raw_secs: f64,
    cfg: &GlowConfig,
    s: f32,
) {
    if level <= 0.002 || sw <= 0 || sh <= 0 || bits.is_null() {
        return;
    }
    let s = s.max(0.5);
    let tint = halo_tint();
    let pal = tint.palette();
    let (_, hi_col, _) = pal;
    let eff_opacity = (cfg.opacity * level).clamp(0.0, 1.0);
    if eff_opacity <= 0.002 {
        return;
    }

    let reach_px = halo_reach_px_cfg(cfg, sw, sh, s);
    if reach_px <= 0 {
        return;
    }

    let dw = sw as f32 / s;
    let dh = sh as f32 / s;
    if dw <= 1.0 || dh <= 1.0 {
        return;
    }

    // 用 f64 取模避免长时间运行后 f32 相位丢失精度；采用闭合圆周涡旋 + 沿四边循环滚动烟波（杜绝单向西北飘移）
    let t_s = raw_secs * cfg.flow_speed as f64;
    let t_f = (t_s.rem_euclid(std::f64::consts::TAU * 120.0)) as f32;
    let orbit_ax = (t_f * 0.65).cos() * 0.78;
    let orbit_ay = (t_f * 0.65).sin() * 0.78;
    let orbit_bx = (t_f * 0.48 + 1.8).sin() * 0.64;
    let orbit_by = (t_f * 0.54 + 0.7).cos() * 0.64;
    let ph_cw = t_f * 1.75;
    let ph_ccw = t_f * 1.20;
    let ph_rad = t_f * 2.50;
    let ph_conic = ((t_s * 0.5).rem_euclid(std::f64::consts::TAU)) as f32;

    let pad_c = 2usize;
    let reach_dp = halo_reach_dp(&cfg);
    let reach_c = (reach_dp / HALO_CELL_DP).ceil() as usize + 2;
    let band = pad_c + reach_c;

    let cw = (dw / HALO_CELL_DP).ceil() as usize;
    let ch = (dh / HALO_CELL_DP).ceil() as usize;
    let gw = cw + 2 * pad_c;
    let gh = ch + 2 * pad_c;
    let sw_u = sw as usize;
    let sh_u = sh as usize;
    let reach_u = (reach_px as usize).min(sw_u / 2).min(sh_u / 2);

    // 烟雾 3 色阶：暗谷深蓝 → 主体电光蓝 → 烟丝亮芯（保持高饱和冰青，浅深色桌面都不发白灰）
    let c_deep = pal.2;
    let c_mid = pal.0;
    let c_bright = lerp3(pal.1, (0.75, 0.94, 1.0), 0.28);
    let dot_col = lerp3(hi_col, (0.90, 0.97, 1.0), 0.55);
    let dot_rgb_soft = (
        (dot_col.0 * 255.0 + 0.5) as u32,
        (dot_col.1 * 255.0 + 0.5) as u32,
        (dot_col.2 * 255.0 + 0.5) as u32,
    );
    let aspect = (dw / dh).max(0.5);
    let noise_scale = 5.0f32;
    let dyn_mix = cfg.dynamics.clamp(0.0, 1.0);
    let valley_ratio = (cfg.valley_thickness / reach_dp).clamp(0.06, 0.40);
    let peak_ratio = (cfg.peak_thickness / 33.0).clamp(0.55, 1.65);
    let half_dw = dw * 0.5;
    let half_dh = dh * 0.5;

    HALO_CACHE.with(|slot| {
        let mut guard = slot.borrow_mut();
        let cache = guard.get_or_insert_with(|| HaloCache {
            gw: 0,
            gh: 0,
            sw_lut: 0,
            s_lut: 0.0,
            pad_lut: 0,
            reach_lut: 0,
            dot_grid: Vec::new(),
            l1_u64: Vec::new(),
            row_mix: Vec::new(),
            w_lut: Vec::new(),
            dot_x_cell: Vec::new(),
            dot_spacing_px: 8,
            dot_size_px: 2,
            full_runs: Vec::new(),
            left_runs: Vec::new(),
            right_runs: Vec::new(),
        });
        cache.prepare(gw, gh, sw_u, pad_c, reach_u, s);

        // 1. 粗网格计算域扭曲 FBM 烟雾（仅扫边框 band，跳过屏中 ~80% 空白）
        let bx_m = band.min(gw);
        let by_m = band.min(gh);
        let full_m = 2 * bx_m >= gw;
        let inv_dw = 1.0 / dw;
        let inv_dh = 1.0 / dh;
        for gy in 0..gh {
            let y = (gy as f32 - pad_c as f32 + 0.5) * HALO_CELL_DP;
            let y_clamped = y.clamp(0.0, dh);
            let my = y_clamped.min(dh - y_clamped);
            let dy_c = y_clamped - half_dh;
            let full_row = full_m || gy < by_m || gy >= gh.saturating_sub(by_m);
            let row_start = gy * gw;

            let eval_cell = |gx: usize, dot_grid: &mut [[f32; 2]], l1_u64: &mut [u64]| {
                let x = (gx as f32 - pad_c as f32 + 0.5) * HALO_CELL_DP;
                let x_clamped = x.clamp(0.0, dw);
                let mx = x_clamped.min(dw - x_clamped);
                if mx >= reach_dp && my >= reach_dp {
                    return;
                }
                let d_edge = if mx < reach_dp && my < reach_dp {
                    let rx = reach_dp - mx;
                    let ry = reach_dp - my;
                    (reach_dp - (rx * rx + ry * ry).sqrt()).max(0.0)
                } else {
                    mx.min(my)
                };
                if d_edge >= reach_dp {
                    return;
                }
                let corner_boost = ((1.0 - (mx / reach_dp).clamp(0.0, 1.0))
                    * (1.0 - (my / reach_dp).clamp(0.0, 1.0)))
                .sqrt()
                    * 0.25;

                // 闭合圆周涡旋 + 绕四边顺/逆时针循环滚动烟波 + 径向呼吸波
                let ang = dy_c.atan2(x_clamped - half_dw);
                let px = x_clamped * inv_dw * noise_scale * aspect + orbit_ax;
                let py = y_clamped * inv_dh * noise_scale + orbit_ay;
                let qx = fbm_smoke(px, py, 2) - 0.5;
                let qy = fbm_smoke(px + 5.2, py + 1.3, 2) - 0.5;
                let n_warp = fbm_smoke(px + qx * 1.85 + orbit_bx, py + qy * 1.85 + orbit_by, 3);
                let n_wisp = fbm_smoke(
                    px * 2.20 - qx * 0.95 - orbit_bx * 1.1,
                    py * 2.20 - qy * 0.95 - orbit_by * 1.1,
                    2,
                );
                let w_cw = 0.5 + 0.5 * (ang * 3.0 - ph_cw + n_warp * 3.2).sin();
                let w_ccw = 0.5 + 0.5 * (ang * 2.0 + ph_ccw - n_wisp * 2.4).sin();
                let w_rad = 0.5 + 0.5 * (d_edge * 0.068 - ph_rad + ang).sin();
                let wave_mod = 0.48 + 0.32 * w_cw + 0.20 * w_ccw;
                let smoke_raw = (n_warp * 0.62 + n_wisp * 0.24 + w_rad * 0.14) * (0.72 + 0.42 * wave_mod);
                let t_smoke = ((smoke_raw - 0.44) * (1.95 + 0.95 * dyn_mix) + 0.50).clamp(0.0, 1.0);

                let local_reach = reach_dp
                    * (valley_ratio + (0.54 * peak_ratio - valley_ratio) * (0.24 + 0.76 * t_smoke))
                    * (1.0 + corner_boost);
                let norm_d = (d_edge / local_reach.max(6.0)).clamp(0.0, 4.0);
                let env_mist = (-norm_d.powf(1.45)).exp();
                let env_plume = (-(d_edge / (local_reach * 0.42).max(4.0)).powf(1.25)).exp()
                    * smoothstep(0.30, 0.88, t_smoke);
                let hard_cut = smoothstep(reach_dp, reach_dp * 0.64, d_edge);
                let a_smoke = ((env_mist * (0.18 + 0.44 * t_smoke) + env_plume * 0.42) * hard_cut * eff_opacity)
                    .clamp(0.0, 0.84);

                if a_smoke > 0.002 {
                    let col = if tint == HaloTint::Rainbow {
                        let h = (ang / std::f32::consts::TAU - ph_conic / std::f32::consts::TAU + t_smoke * 0.16).rem_euclid(1.0);
                        let base = hue_to_rgb(h);
                        lerp3(base, (0.90, 0.97, 1.0), env_plume * 0.48)
                    } else if tint == HaloTint::Cyberpunk {
                        // 赛博霓虹：电光青 ↔ 紫外紫 ↔ 热品红 沿四边巡游碰撞，亮芯注入冰青白光
                        let wave = 0.5 + 0.5 * (ang * 2.0 - ph_conic * 1.4 + n_warp * 2.6).sin();
                        let neon = if wave < 0.45 {
                            lerp3(c_deep, c_mid, wave / 0.45)
                        } else {
                            lerp3(c_mid, hi_col, (wave - 0.45) / 0.55)
                        };
                        lerp3(neon, (0.72, 0.98, 1.0), env_plume * 0.45)
                    } else {
                        let ct = (t_smoke * 0.56 + env_plume * 0.52).clamp(0.0, 1.0);
                        if ct < 0.5 {
                            lerp3(c_deep, c_mid, ct * 2.0)
                        } else {
                            lerp3(c_mid, c_bright, (ct - 0.5) * 2.0)
                        }
                    };
                    let a_q = (a_smoke * 1020.0 + 0.5) as u64;
                    let r_q = ((col.0 * a_smoke * 1020.0 + 0.5) as u64).min(a_q);
                    let g_q = ((col.1 * a_smoke * 1020.0 + 0.5) as u64).min(a_q);
                    let b_q = ((col.2 * a_smoke * 1020.0 + 0.5) as u64).min(a_q);
                    l1_u64[row_start + gx] = (a_q << 48) | (r_q << 32) | (g_q << 16) | b_q;
                } else {
                    l1_u64[row_start + gx] = 0;
                }
                let dot_edge = smoothstep(reach_dp * 0.78, 0.0, d_edge) * (0.60 + 0.40 * corner_boost * 4.0);
                dot_grid[row_start + gx] = [t_smoke, dot_edge.clamp(0.0, 1.0)];
            };

            if full_row {
                for gx in 0..gw {
                    eval_cell(gx, &mut cache.dot_grid, &mut cache.l1_u64);
                }
            } else {
                for gx in 0..bx_m {
                    eval_cell(gx, &mut cache.dot_grid, &mut cache.l1_u64);
                }
                for gx in (gw - bx_m)..gw {
                    eval_cell(gx, &mut cache.dot_grid, &mut cache.l1_u64);
                }
            }
        }

        // 3. 纯整型 SWAR 垂直混合 + gx 块双线性上采样直写 DIB（零像素级零区分支）
        let inv_cell = 1.0 / (HALO_CELL_DP * s);
        let pad_f = pad_c as f32 - 0.5;
        let max_gy = (gh - 2) as f32;
        let bx_mix = cache
            .left_runs
            .last()
            .map(|r| (r.0 as usize + 2).min(gw))
            .unwrap_or(0);
        let x_right_gx_lo = cache
            .right_runs
            .first()
            .map(|r| (r.0 as usize).min(gw.saturating_sub(2)))
            .unwrap_or(0);
        let l1_u64 = &cache.l1_u64;
        let dot_grid = &cache.dot_grid;
        let w_lut = &cache.w_lut;
        let dot_x_cell = &cache.dot_x_cell;
        let spacing_px = cache.dot_spacing_px.max(4);
        let dot_px = cache.dot_size_px.max(1);
        let full_runs = &cache.full_runs;
        let left_runs = &cache.left_runs;
        let right_runs = &cache.right_runs;
        let mix = &mut cache.row_mix;

        let paint_row_runs = |py: usize, runs: &[(u16, u16, u16)], is_full: bool, mix: &mut [u64]| {
            let gyf = (py as f32 * inv_cell + pad_f).clamp(0.0, max_gy);
            let gy0 = gyf as usize;
            let fy = gyf - gy0 as f32;
            let wy1 = (fy * 64.0 + 0.5) as u64;
            let wy0 = 64 - wy1;
            let r0 = &l1_u64[gy0 * gw..(gy0 + 1) * gw];
            let r1 = &l1_u64[(gy0 + 1) * gw..(gy0 + 2) * gw];
            let mix_range = |m: &mut [u64], lo: usize, hi: usize| {
                for gx in lo..hi {
                    let c0 = *r0.get_unchecked(gx);
                    let c1 = *r1.get_unchecked(gx);
                    let v = (c0
                        .wrapping_mul(wy0)
                        .wrapping_add(c1.wrapping_mul(wy1))
                        .wrapping_add(0x0020_0020_0020_0020)
                        >> 6)
                        & 0x03FF_03FF_03FF_03FF;
                    *m.get_unchecked_mut(gx) = v;
                }
            };
            if is_full {
                mix_range(mix, 0, gw);
            } else {
                mix_range(mix, 0, bx_mix);
                mix_range(mix, x_right_gx_lo, gw);
            }

            let row_ptr = (bits.add(py * sw_u * 4)) as *mut u32;
            for &(gx, px0, px1) in runs {
                let c0 = *mix.get_unchecked(gx as usize);
                let c1 = *mix.get_unchecked(gx as usize + 1);
                if (c0 | c1) == 0 {
                    continue;
                }
                for px in (px0 as usize)..(px1 as usize) {
                    let (w0, w1) = *w_lut.get_unchecked(px);
                    let sum = c0
                        .wrapping_mul(w0 as u64)
                        .wrapping_add(c1.wrapping_mul(w1 as u64))
                        .wrapping_add(0x0080_0080_0080_0080);
                    let x = (sum >> 8) & 0x00FF_00FF_00FF_00FF;
                    let lo = x | (x >> 8);
                    *row_ptr.add(px) = ((lo & 0xFFFF) | ((lo >> 16) & 0xFFFF_0000)) as u32;
                }
            }

            // 2. 纯整型定点叠加微点阵（仅命中点阵行 ~25% 且仅命中 dot_x_cell 列，零逐像素浮点转换）
            if (py % spacing_px) < dot_px {
                let cell_y = (py / spacing_px) as i32;
                let dot_row = &dot_grid[gy0 * gw..(gy0 + 1) * gw];
                for &(gx, px0, px1) in runs {
                    let [t_smoke, dot_edge] = *dot_row.get_unchecked(gx as usize);
                    if dot_edge <= 0.01 || t_smoke <= 0.24 {
                        continue;
                    }
                    let a_soft = smoothstep(0.24, 0.78, t_smoke) * dot_edge * 0.28 * eff_opacity;
                    let da_soft = (a_soft * 255.0 + 0.5) as u32;
                    if da_soft == 0 {
                        continue;
                    }
                    let da_spark = ((a_soft.max(dot_edge * 0.56 * eff_opacity)).clamp(0.0, 0.88) * 255.0 + 0.5) as u32;
                    for px in (px0 as usize)..(px1 as usize) {
                        let cell_x = *dot_x_cell.get_unchecked(px);
                        if cell_x < 0 {
                            continue;
                        }
                        let spark = t_smoke > 0.60
                            && hash2i(cell_x + 17, cell_y + 31) < (t_smoke - 0.55) * 0.85;
                        let da = if spark { da_spark } else { da_soft };
                        let (dr, dg, db) = if spark { (240, 252, 255) } else { dot_rgb_soft };
                        let inv_da = 255 - da;
                        let p = (row_ptr.add(px)) as *mut u8;
                        let b0 = *p as u32;
                        let g0 = *p.add(1) as u32;
                        let r0 = *p.add(2) as u32;
                        let a0 = *p.add(3) as u32;
                        let a_out = (da + ((a0 * inv_da + 127) / 255)).min(255);
                        *p = (((db * da + b0 * inv_da + 127) / 255).min(a_out)) as u8;
                        *p.add(1) = (((dg * da + g0 * inv_da + 127) / 255).min(a_out)) as u8;
                        *p.add(2) = (((dr * da + r0 * inv_da + 127) / 255).min(a_out)) as u8;
                        *p.add(3) = a_out as u8;
                    }
                }
            }
        };

        if 2 * reach_u >= sh_u {
            for py in 0..sh_u {
                paint_row_runs(py, full_runs, true, mix);
            }
        } else {
            for py in 0..reach_u {
                paint_row_runs(py, full_runs, true, mix);
            }
            for py in (sh_u - reach_u)..sh_u {
                paint_row_runs(py, full_runs, true, mix);
            }
            if 2 * reach_u >= sw_u {
                for py in reach_u..(sh_u - reach_u) {
                    paint_row_runs(py, full_runs, true, mix);
                }
            } else {
                for py in reach_u..(sh_u - reach_u) {
                    paint_row_runs(py, left_runs, false, mix);
                    paint_row_runs(py, right_runs, false, mix);
                }
            }
        }

        // 4. 贴边 1.5dp 柔高光线单独后叠加（仅 ~1% 边框像素，不拖慢主采样循环）
        let rim_alpha_base = eff_opacity * cfg.rim_opacity;
        if rim_alpha_base > 0.002 {
            let rim_px = ((1.5 * s).ceil() as usize).min(sw_u / 2).min(sh_u / 2);
            let inv_255 = 1.0 / 255.0;
            let blend_rim_px = |py: usize, px: usize| {
                let dy_edge = (py as f32).min((sh_u - 1 - py) as f32) / s;
                let dx_edge = (px as f32).min((sw_u - 1 - px) as f32) / s;
                let d_edge = dy_edge.min(dx_edge);
                if d_edge >= 1.5 {
                    return;
                }
                let ra = rim_alpha_base * (1.5 - d_edge).min(1.0);
                let (rh, gh_c, bh) = if tint == HaloTint::Rainbow {
                    hue_to_rgb((px as f32 / sw_u.max(1) as f32 - ph_conic / std::f32::consts::TAU).rem_euclid(1.0))
                } else {
                    hi_col
                };
                let rr = rh * ra;
                let rg = gh_c * ra;
                let rb = bh * ra;
                let p = bits.add((py * sw_u + px) * 4);
                let b0 = *p as f32 * inv_255;
                let g0 = *p.add(1) as f32 * inv_255;
                let r0 = *p.add(2) as f32 * inv_255;
                let a0 = *p.add(3) as f32 * inv_255;
                let a = a0 + ra - a0 * ra;
                let a_u8 = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                if a_u8 == 0 {
                    return;
                }
                let b_u8 = (((b0 + rb - b0 * rb).clamp(0.0, 1.0) * 255.0 + 0.5) as u8).min(a_u8);
                let g_u8 = (((g0 + rg - g0 * rg).clamp(0.0, 1.0) * 255.0 + 0.5) as u8).min(a_u8);
                let r_u8 = (((r0 + rr - r0 * rr).clamp(0.0, 1.0) * 255.0 + 0.5) as u8).min(a_u8);
                *p = b_u8;
                *p.add(1) = g_u8;
                *p.add(2) = r_u8;
                *p.add(3) = a_u8;
            };
            for py in 0..rim_px {
                for px in 0..sw_u {
                    blend_rim_px(py, px);
                }
            }
            for py in (sh_u - rim_px)..sh_u {
                for px in 0..sw_u {
                    blend_rim_px(py, px);
                }
            }
            for py in rim_px..(sh_u - rim_px) {
                for px in 0..rim_px {
                    blend_rim_px(py, px);
                }
                for px in (sw_u - rim_px)..sw_u {
                    blend_rim_px(py, px);
                }
            }
        }
    });
}

/// 细条的文字绘制：**只有正文一行**，垂直居中。
/// DWrite 资源缓存(T6.4)。
///
/// D2D 的 factory / render target 早就缓存了(见 `with_target` 里那段 247ms/帧的教训),
/// 但**文字**这条路上每帧还在重建三样:`IDWriteFactory`、`IDWriteTextFormat`、那支画刷。
/// 上屏是逐帧发生的,这三样白付的开销加起来就是"每次上屏多花的钱"。
///
/// 缓存口径:
/// - factory:进程级共享(`DWRITE_FACTORY_TYPE_SHARED`),建一次就够;
/// - TextFormat:**按字号缓存**(字号 = `BAR_FONT × DPI 缩放`),缩放变了才重建 ——
///   插拔显示器/改 DPI 之后自动跟着变,不会拿旧字号糊上去;
/// - 画刷:**绑在 D2D 目标上**,目标重建时必须丢掉(见 `invalidate_dwrite_brush`)。
///
/// 计数是可机检判据:"画 N 帧之后只应该是 (1,1,1)"。
///
/// ⚠️ 计数是**全局原子**,不能放进 `DwCache`(那是 thread_local):绘制跑在 overlay
/// 自己的窗口线程上,而读计数的人在主线程 —— 用 thread_local 读到的永远是 0,
/// 探针会给出一个"0 ≤ 1 所以 PASS"的假绿(踩过)。
static N_FACTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static N_FORMAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static N_BRUSH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct DwCache {
    factory: Option<windows::Win32::Graphics::DirectWrite::IDWriteFactory>,
    format: Option<(i32, windows::Win32::Graphics::DirectWrite::IDWriteTextFormat)>,
    ink: Option<windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush>,
}

impl Default for DwCache {
    fn default() -> Self {
        Self {
            factory: None,
            format: None,
            ink: None,
        }
    }
}

thread_local! {
    static DW: std::cell::RefCell<DwCache> = std::cell::RefCell::new(DwCache::default());
}

/// 三类资源的创建次数(factory / TextFormat / 画刷)—— 测试与排障用。
pub fn dwrite_create_counts() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering;
    (
        N_FACTORY.load(Ordering::Relaxed),
        N_FORMAT.load(Ordering::Relaxed),
        N_BRUSH.load(Ordering::Relaxed),
    )
}

/// 丢掉画刷缓存:D2D 目标重建时调 —— 画刷绑在目标上,旧目标那份不能用。
fn invalidate_dwrite_brush() {
    DW.with(|d| d.borrow_mut().ink = None);
}

/// 请求**绘制线程**在下一次绘制前丢掉三样缓存(计数不动)。
///
/// 为什么是"请求"而不是直接清:缓存和计数一样住在 **overlay 自己的线程**里
/// (`thread_local`),从别的线程直接清只会清到调用者自己那份空缓存(踩过两次)。
/// 这个标志是跨线程让绘制线程丢弃缓存的标准做法,探针自校准就用它。
pub fn dwrite_schedule_drop_cache() {
    DROP_NEXT.store(true, Ordering::Relaxed);
}

/// 绘制线程侧:该丢就在下一次绘制前丢掉。
fn take_drop_request() {
    if DROP_NEXT.swap(false, Ordering::Relaxed) {
        DW.with(|d| {
            let mut g = d.borrow_mut();
            g.factory = None;
            g.format = None;
            g.ink = None;
        });
    }
}

static DROP_NEXT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 计数归零(缓存不动)。给探针把"两段测试"分开记账。
pub fn dwrite_reset_counts() {
    use std::sync::atomic::Ordering;
    N_FACTORY.store(0, Ordering::Relaxed);
    N_FORMAT.store(0, Ordering::Relaxed);
    N_BRUSH.store(0, Ordering::Relaxed);
}

fn draw_bar_text(
    target: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    body: &windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F,
    text: &str,
) {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Direct2D as d2d;
    use windows::Win32::Graphics::Direct2D::Common as d2dc;
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, DWRITE_FACTORY_TYPE_SHARED,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
        DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER,
    };

    take_drop_request();
    // 字号随 DPI 缩放变;它变了才需要换 TextFormat。
    let size_key = (BAR_FONT * scale()).round() as i32;

    // ⚠️ 从缓存里**克隆出来用**,不要握着 `RefCell` 的借用去调 D2D:
    // 绘制路径里再碰一次这个 thread_local 就是借用冲突(panick)。
    // COM 克隆只加一次引用计数,比每帧重建便宜得多。
    let pair = DW.with(|d| {
        let mut g = d.borrow_mut();
        if g.factory.is_none() {
            match unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) } {
                Ok(f) => {
                    g.factory = Some(f);
                    N_FACTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("[ov] DWriteCreateFactory failed: {e:?}");
                    return None;
                }
            }
        }
        let need_format = !matches!(&g.format, Some((k, _)) if *k == size_key);
        if need_format {
            let dwrite = g.factory.as_ref()?;
            let face = wide("Microsoft YaHei UI");
            match unsafe {
                dwrite.CreateTextFormat(
                    PCWSTR(face.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    BAR_FONT * scale(),
                    PCWSTR(wide("zh-CN").as_ptr()),
                )
            } {
                Ok(f) => {
                    // ⚠️ DrawText 默认是**左上对齐**。不设这两项,文字会贴着面板上边往右排
                    // ——看起来就是"歪的"(用户实测指出)。
                    unsafe {
                        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                    }
                    g.format = Some((size_key, f));
                    N_FORMAT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    eprintln!("[ov] CreateTextFormat failed: {e:?}");
                    return None;
                }
            }
        }
        if g.ink.is_none() {
            match unsafe { target.CreateSolidColorBrush(&argb(BAR_INK) as *const _, None) } {
                Ok(b) => {
                    g.ink = Some(b);
                    N_BRUSH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(_) => return None,
            }
        }
        Some((g.format.as_ref()?.1.clone(), g.ink.as_ref()?.clone()))
    });

    let Some((f_text, br)) = pair else { return };
    let tx = wide(text);
    let rect = d2dc::D2D_RECT_F {
        left: body.left + px(BAR_PAD_X) as f32,
        top: body.top,
        right: body.right - px(BAR_PAD_X) as f32,
        bottom: body.bottom,
    };
    unsafe {
        let _ = target.DrawText(
            &tx[..tx.len().saturating_sub(1)],
            &f_text,
            &rect,
            &br,
            d2d::D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T6.4:画 N 帧,DWrite 的 factory / TextFormat / 画刷只该各建**一次**。
    ///
    /// 判据用**计数**而不是计时:计时在这台机器上会被调度噪声淹没(CI 上更飘),
    /// 而"重建了几次"是确定的整数 —— 一旦退化回"每帧重建",这条立刻红。
    // T6.4 的判据**不做成单测**:这个文件里能建的只是内存 DC + D2D 目标,而 DWrite 的
    // **文字**绘制在那条路上会挂住(实测:测试名打印出来之后 240s 超时)。生产里画字是在
    // 有窗口、有消息循环的那条线程上 —— 判据因此改成 `examples/dwritecache.rs` 探针:
    // 真窗口里推 N 帧,再读 `dwrite_create_counts()`。
    // (这里原来挂着一个 `#[test]`,函数体已被上面那段结论取代 → 它成了重复属性,
    //  会让下面那个用例被注册两次(实测:同一行 test 结果打印两遍)。已删。)

    /// 回归：D2D 目标失效后必须丢掉缓存，并在下一帧重建。
    ///
    /// 真实触发条件是 GPU 驱动报错 / 睡眠唤醒后的设备丢失：旧代码只
    /// eprintln、继续复用死 target，边缘光会永久消失（用户实测报过）。
    /// 这里用「已删除的 DC」当替身，让 BindDC 必然失败。
    #[test]
    fn d2d_target_cache_recovers_after_failure() {
        unsafe {
            let mem_dc = CreateCompatibleDC(0);
            assert!(mem_dc != 0, "CreateCompatibleDC failed");
            let bmi = BitmapInfo {
                header: BitmapInfoHeader {
                    bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    bi_width: 64,
                    bi_height: -64,
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: 0,
                    bi_size_image: 64 * 64 * 4,
                    ..Default::default()
                },
                colors: [0; 3],
            };
            let mut bits: *mut u8 = ptr::null_mut();
            let dib = CreateDIBSection(mem_dc, &bmi, 0, &mut bits, 0, 0);
            assert!(dib != 0 && !bits.is_null(), "CreateDIBSection failed");
            let old_bmp = SelectObject(mem_dc, dib);

            let rc = Rect {
                left: 0,
                top: 0,
                right: 64,
                bottom: 64,
            };

            let mut drew = false;
            with_target(mem_dc, &rc, std::slice::from_ref(&rc), (0.0, 0.0), |_| drew = true);
            assert!(drew, "健康 target 应执行绘制");
            assert!(
                RT.with(|s| s.borrow().is_some()),
                "健康绘制后应留下缓存"
            );

            // 失败替身：先建再删，留下的句柄保证 BindDC 失败
            let stale = CreateCompatibleDC(0);
            assert!(stale != 0);
            DeleteDC(stale);
            with_target(stale, &rc, std::slice::from_ref(&rc), (0.0, 0.0), |_| {});
            assert!(
                RT.with(|s| s.borrow().is_none()),
                "BindDC 失败后必须丢弃缓存（旧 bug：继续复用死 target）"
            );

            // 下一帧用健康 DC：必须重建并继续画
            let mut drew_again = false;
            with_target(mem_dc, &rc, std::slice::from_ref(&rc), (0.0, 0.0), |_| drew_again = true);
            assert!(drew_again, "缓存重建后应继续绘制");
            assert!(RT.with(|s| s.borrow().is_some()));

            // 先清缓存再收尾,别让 target 绑着已删除的 DC
            RT.with(|s| *s.borrow_mut() = None);
            SelectObject(mem_dc, old_bmp);
            DeleteObject(dib);
            DeleteDC(mem_dc);
        }
    }

    /// 回归:`clear_rect` 只抹给定矩形 —— 脏矩形的正确性全押在它身上。
    ///
    /// 关键的一条是**裁剪**:矩形可能被屏幕边界钳成负坐标或超出画布,
    /// 越界写不是"画错"而是踩内存。
    #[test]
    fn clear_rect_zeroes_only_that_rect() {
        let (w, h) = (8i32, 4i32);
        let mut buf = vec![0xFFu8; (w * h * 4) as usize];
        unsafe {
            clear_rect(
                buf.as_mut_ptr(),
                w,
                h,
                &Rect { left: -2, top: 1, right: 3, bottom: 3 },
            );
        }
        for y in 0..h {
            for x in 0..w {
                let a = buf[((y * w + x) * 4 + 3) as usize];
                let inside = (1..3).contains(&y) && (0..3).contains(&x);
                assert_eq!(a == 0, inside, "({x},{y}) 该清/不该清判断错了");
            }
        }
        // 空矩形(账本里的"没用到的槽位")必须什么都不做
        let mut buf2 = vec![0xFFu8; (w * h * 4) as usize];
        unsafe { clear_rect(buf2.as_mut_ptr(), w, h, &EMPTY_RECT) };
        assert!(buf2.iter().all(|b| *b == 0xFF));
    }

    /// 回归:`content_rects` 必须把内容**圈得住**(圈不住 = 屏幕上留残留)。
    #[test]
    fn content_rects_covers_drawn_area() {
        let (sw, sh) = (1920, 1080);
        // 细条/悬浮球:内容面板本身
        let panel = Rect { left: 100, top: 200, right: 400, bottom: 260 };
        let bar = content_rects(Mode::Bar, &panel, sw, sh);
        assert!(
            bar[0].left <= panel.left
                && bar[0].top <= panel.top
                && bar[0].right >= panel.right
                && bar[0].bottom >= panel.bottom
        );
        assert!(
            bar[1..].iter().all(|r| r.left >= r.right),
            "没用到的槽位应该是空矩形"
        );
        let orb = content_rects(Mode::Orb, &panel, sw, sh);
        assert_eq!(
            (orb[0].left, orb[0].top, orb[0].right, orb[0].bottom),
            (bar[0].left, bar[0].top, bar[0].right, bar[0].bottom)
        );

        // 边缘光:四条光带(顺序同 edge_geometry:上/右/下/左),贴边且向内延伸
        let e = content_rects(Mode::Edge, &Rect::default(), sw, sh);
        assert!(e[0].left <= 0 && e[0].top <= 0 && e[0].right >= sw && e[0].bottom > 0);
        assert!(e[1].left > 0 && e[1].left < sw && e[1].right >= sw && e[1].bottom >= sh);
        assert!(e[2].left <= 0 && e[2].right >= sw && e[2].top > 0 && e[2].bottom >= sh);
        assert!(e[3].left <= 0 && e[3].right > 0 && e[3].top <= 0 && e[3].bottom >= sh);

        // 流光光晕:同样覆盖四条贴边带
        let h = content_rects(Mode::Halo, &Rect::default(), sw, sh);
        assert!(h[0].left <= 0 && h[0].top <= 0 && h[0].right >= sw && h[0].bottom > 0);
        assert!(h[1].left > 0 && h[1].left < sw && h[1].right >= sw && h[1].bottom >= sh);
        assert!(h[2].left <= 0 && h[2].right >= sw && h[2].top > 0 && h[2].bottom >= sh);
        assert!(h[3].left <= 0 && h[3].right > 0 && h[3].top <= 0 && h[3].bottom >= sh);
    }

    /// 回归:Halo 渲染的像素必须严格落在 content_rects 内、满足预乘 BGRA 约束、且默认参数与电光蓝生效。
    #[test]
    fn halo_pixels_stay_inside_content_rects_and_valid_premultiplied() {
        let (sw, sh) = (960i32, 600i32);
        let mut buf = vec![0u8; (sw * sh * 4) as usize];
        set_halo_tint(HaloTint::Blue);
        set_halo_config(GLOW_CONFIG);
        unsafe {
            paint_halo_with_time(buf.as_mut_ptr(), sw, sh, 1.0, 1.5);
        }
        let rects = content_rects(Mode::Halo, &Rect::default(), sw, sh);
        let in_rects = |x: i32, y: i32| {
            rects.iter().any(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        };
        let mut max_edge_a = 0u8;
        let mut edge_blue_gt_red = false;
        for y in 0..sh {
            for x in 0..sw {
                let off = ((y * sw + x) * 4) as usize;
                let (b, g, r, a) = (buf[off], buf[off + 1], buf[off + 2], buf[off + 3]);
                assert!(b <= a && g <= a && r <= a, "预乘 alpha 越界 ({x},{y}): [{b},{g},{r},{a}]");
                if !in_rects(x, y) {
                    assert_eq!((b, g, r, a), (0, 0, 0, 0), "({x},{y}) 超出 content_rects 脏区");
                }
                if y == 4 && x == sw / 2 {
                    max_edge_a = max_edge_a.max(a);
                    edge_blue_gt_red = b > r;
                }
            }
        }
        assert!(max_edge_a > 60, "顶部烟雾光晕太暗: a={max_edge_a}");
        assert!(edge_blue_gt_red, "默认电光蓝应该蓝通道大于红通道");

        // verify organic FBM smoke contrast along top edge (not a flat uniform gradient)
        let mut row_min = 255u8;
        let mut row_max = 0u8;
        for x in 100..(sw - 100) {
            let a = buf[((14 * sw + x) * 4 + 3) as usize];
            row_min = row_min.min(a);
            row_max = row_max.max(a);
        }
        assert!(row_max.saturating_sub(row_min) > 25, "烟雾应具有明显明暗云团起伏: min={row_min}, max={row_max}");

        // center must be completely untouched
        let c_off = (((sh / 2) * sw + (sw / 2)) * 4) as usize;
        assert_eq!(&buf[c_off..c_off + 4], &[0, 0, 0, 0]);

        // level == 0.0 must draw nothing
        let mut zero_buf = vec![0u8; (sw * sh * 4) as usize];
        unsafe {
            paint_halo_with_time(zero_buf.as_mut_ptr(), sw, sh, 0.0, 1.5);
        }
        assert!(zero_buf.iter().all(|&v| v == 0));
    }

    #[test]
    fn halo_new_palettes_and_spread_presets_render_distinctly() {
        let (sw, sh) = (640i32, 400i32);
        let mut cyber = vec![0u8; (sw * sh * 4) as usize];
        set_halo_tint(HaloTint::Cyberpunk);
        let mut cfg = GLOW_CONFIG;
        cfg.blur_radius = 28.0;
        cfg.peak_thickness = 20.0;
        cfg.valley_thickness = 8.0;
        set_halo_config(cfg);
        unsafe {
            paint_halo_with_time(cyber.as_mut_ptr(), sw, sh, 1.0, 1.5);
        }
        // 赛博霓虹应在边缘呈现出高饱和品红/青色像素，且不能发黑或全零
        let non_zero_compact = cyber.chunks_exact(4).filter(|p| p[3] > 12).count();
        assert!(non_zero_compact > 2000, "赛博霓虹渲染面积过小: {non_zero_compact}");

        let mut wide = vec![0u8; (sw * sh * 4) as usize];
        set_halo_tint(HaloTint::Sunset);
        cfg.blur_radius = 86.0;
        cfg.peak_thickness = 54.0;
        cfg.valley_thickness = 20.0;
        set_halo_config(cfg);
        unsafe {
            paint_halo_with_time(wide.as_mut_ptr(), sw, sh, 1.0, 1.5);
        }
        let non_zero_wide = wide.chunks_exact(4).filter(|p| p[3] > 12).count();
        assert!(
            non_zero_wide > non_zero_compact,
            "宽广范围({non_zero_wide})应显著大于紧凑范围({non_zero_compact})"
        );
        // 恢复默认，避免污染同进程其他测试
        set_halo_tint(HaloTint::Blue);
        set_halo_config(GLOW_CONFIG);
    }

    #[test]
    fn halo_spread_transitions_never_leave_inner_border_line() {
        // 回归：切换 紧凑 ↔ 标准 ↔ 宽广 或不同分辨率时，l1_u64 不可残留旧帧亮边导致内侧出现竖线
        let mut wide = GLOW_CONFIG;
        wide.blur_radius = 86.0;
        wide.peak_thickness = 54.0;
        wide.valley_thickness = 20.0;
        wide.opacity = 0.98;
        let mut compact = GLOW_CONFIG;
        compact.blur_radius = 28.0;
        compact.peak_thickness = 20.0;
        compact.valley_thickness = 8.0;
        for &(sw, sh, s) in &[
            (1920i32, 1080i32, 1.0f32),
            (2560i32, 1440i32, 1.25f32),
            (2560i32, 1600i32, 1.50f32),
            (1440i32, 1088i32, 1.25f32),
        ] {
            let mut buf = vec![0u8; (sw * sh * 4) as usize];
            set_halo_tint(HaloTint::Cyberpunk);
            for &(cfg, t) in &[(GLOW_CONFIG, 1.0), (wide, 1.2), (compact, 1.4), (GLOW_CONFIG, 1.6), (wide, 1.8)] {
                set_halo_config(cfg);
                buf.fill(0);
                unsafe { paint_halo_with_time_cfg(buf.as_mut_ptr(), sw, sh, 1.0, t, &cfg, s); }
                let reach = halo_reach_px_cfg(&cfg, sw, sh, s);
                let py = sh / 2;
                for px in (sw - reach - 12).max(0)..(sw - reach + 4).min(sw) {
                    let a = buf[((py * sw + px) * 4 + 3) as usize];
                    assert!(
                        a <= 2,
                        "内边界出现亮线残留: sw={sw} s={s} reach={reach} px={px} (offset={}) alpha={a}",
                        px - (sw - reach)
                    );
                }
                for px in (reach - 4).max(0)..(reach + 12).min(sw) {
                    let a = buf[((py * sw + px) * 4 + 3) as usize];
                    assert!(a <= 2, "左侧内边界出现亮线残留: sw={sw} s={s} reach={reach} px={px} alpha={a}");
                }
                let px_mid = sw / 2;
                for y in (reach - 4).max(0)..(reach + 12).min(sh) {
                    let a = buf[((y * sw + px_mid) * 4 + 3) as usize];
                    assert!(a <= 2, "顶部内边界出现亮线残留: sw={sw} s={s} reach={reach} py={y} alpha={a}");
                }
            }
        }
        set_halo_tint(HaloTint::Blue);
        set_halo_config(GLOW_CONFIG);
    }
    #[test]
    fn orb_silk_wave_naked_premultiplied_and_palette_sensitive() {
        let (sw, sh) = (400i32, 160i32);
        let panel = Rect { left: 100, top: 50, right: 296, bottom: 102 };
        let mut buf = vec![0u8; (sw * sh * 4) as usize];
        unsafe {
            paint_orb_silk_wave_at(
                buf.as_mut_ptr(),
                sw,
                sh,
                &panel,
                1.0,
                0.75,
                0.70,
                0.78,
                0.72,
                1.2,
                HaloTint::Cyberpunk,
                1.0,
            );
        }
        let mut bright_px = 0usize;
        for y in 0..sh {
            for x in 0..sw {
                let off = ((y * sw + x) * 4) as usize;
                let (b, g, r, a) = (buf[off], buf[off + 1], buf[off + 2], buf[off + 3]);
                assert!(b <= a && g <= a && r <= a, "预乘 alpha 越界 ({x},{y}): [{b},{g},{r},{a}]");
                let inside = x >= panel.left && x < panel.right && y >= panel.top && y < panel.bottom;
                if !inside {
                    assert_eq!((b, g, r, a), (0, 0, 0, 0), "不应画到 panel 之外");
                }
                if a > 80 {
                    bright_px += 1;
                }
            }
        }
        assert!(bright_px > 200, "说话时应画出清晰弦波: bright_px={bright_px}");
        // 顶部与左右边界不能有胶囊框线（border-free 验收）
        for x in panel.left..panel.right {
            let top_a = buf[((panel.top * sw + x) * 4 + 3) as usize];
            assert!(top_a <= 2, "顶部不应有胶囊边框: x={x} a={top_a}");
        }
        let left_mid_a = buf[(((panel.top + 26) * sw + panel.left) * 4 + 3) as usize];
        assert!(left_mid_a <= 2, "左侧不应有胶囊边框: a={left_mid_a}");

        // orb_alpha == 0.0（不说话默认消失）必须全零
        let mut zero_buf = vec![0u8; (sw * sh * 4) as usize];
        unsafe {
            paint_orb_silk_wave_at(
                zero_buf.as_mut_ptr(),
                sw,
                sh,
                &panel,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.2,
                HaloTint::Cyberpunk,
                1.0,
            );
        }
        assert!(zero_buf.iter().all(|&v| v == 0));

        // 换配色只改波的颜色：翡翠绿应该绿通道显著高于赛博霓虹
        let mut emerald_buf = vec![0u8; (sw * sh * 4) as usize];
        unsafe {
            paint_orb_silk_wave_at(
                emerald_buf.as_mut_ptr(),
                sw,
                sh,
                &panel,
                1.0,
                0.75,
                0.70,
                0.78,
                0.72,
                1.2,
                HaloTint::Emerald,
                1.0,
            );
        }
        let sum_g_em: u64 = emerald_buf.chunks_exact(4).map(|p| p[1] as u64).sum();
        let sum_r_em: u64 = emerald_buf.chunks_exact(4).map(|p| p[2] as u64).sum();
        assert!(sum_g_em > sum_r_em, "翡翠幽光波应绿大于红: g={sum_g_em} r={sum_r_em}");
    }

    #[test]
    fn orb_pos_set_and_reset_dirty_roundtrip() {
        let _ = take_orb_pos_dirty();
        set_orb_pos(Some((320, 900)));
        assert_eq!(orb_pos(), Some((320, 900)));
        reset_orb_pos();
        assert_eq!(orb_pos(), None);
        assert_eq!(take_orb_pos_dirty(), Some(None));
        assert_eq!(take_orb_pos_dirty(), None);
    }

    #[test]
    fn orb_adaptive_voice_gate_handles_quiet_mic_and_rejects_floor() {
        let mut s = State {
            mode: Mode::Orb,
            visible: true,
            orb_noise_floor: 0.0035,
            orb_peak_agc: 0.075,
            ..State::default()
        };
        // 实机底噪 RMS*6 ≈ 0.0012：不得误触发声波出现
        for _ in 0..10 {
            push_level_into(&mut s, 0.0012);
        }
        assert!(s.orb_last_voice.is_none(), "底噪不应触发声波淡入");
        // 实机低增益麦说话 RMS*6 ≈ 0.025（低于旧固定门限 0.055）：应立刻唤醒且归一化高度充沛
        push_level_into(&mut s, 0.025);
        assert!(s.orb_last_voice.is_some(), "真实低增益说话帧(0.025)必须唤醒声波");
        assert!(
            s.orb_level_norm > 0.35,
            "AGC 归一化应让柔和语音也有明显起伏: norm={}",
            s.orb_level_norm
        );
    }
    #[test]
    fn orb_size_and_ring_size_presets_match_spec() {
        assert!(orb_dims(IndicatorSize::Small).0 < orb_dims(IndicatorSize::Medium).0);
        assert!(orb_dims(IndicatorSize::Medium).0 < orb_dims(IndicatorSize::Large).0);
        assert!(orb_dims(IndicatorSize::Medium).0 <= 160, "默认声波应比旧版更小巧");
        assert!(ring_dims(IndicatorSize::Small).0 < ring_dims(IndicatorSize::Medium).0);
        assert!(ring_dims(IndicatorSize::Medium).0 < ring_dims(IndicatorSize::Large).0);
        assert_eq!(IndicatorSize::from_conf("small"), IndicatorSize::Small);
        assert_eq!(IndicatorSize::from_conf("large"), IndicatorSize::Large);
        assert_eq!(IndicatorSize::from_conf("anything"), IndicatorSize::Medium);
    }

    #[test]
    fn orb_and_ring_stay_visible_while_hotkey_held_even_when_silent() {
        let now = std::time::Instant::now();
        for mode in [Mode::Orb, Mode::Ring] {
            let mut s = State {
                mode,
                visible: true,
                orb_holding: true,
                orb_fading: false,
                orb_noise_floor: 0.0035,
                orb_peak_agc: 0.075,
                ..State::default()
            };
            // 没有任何声音输入，仅按着快捷键推进多帧：必须淡入到可见且保持 target=1.0
            for i in 0..25 {
                let (a, tgt, done) = tick_indicator_state(&mut s, 16.0, i as f32 * 0.016, false, now);
                assert_eq!(tgt, 1.0, "{mode:?} 按住快捷键即使静音也应保持 target_alpha=1.0");
                assert!(!done);
                if i == 24 {
                    assert!(a > 0.9, "{mode:?} 按住后应平滑淡入接近 1.0: alpha={a}");
                }
            }
            assert!(s.orb_env > 0.06, "{mode:?} 按住时应维持柔和活体呼吸底噪包络: {}", s.orb_env);
            // 松开快捷键（fade_orb/fade_ring）：应切换为淡出并在结束后归零隐藏
            s.orb_holding = false;
            s.orb_fading = true;
            for _ in 0..80 {
                let _ = tick_indicator_state(&mut s, 20.0, 1.0, false, now);
            }
            assert_eq!(s.orb_alpha, 0.0, "{mode:?} 松开快捷键后必须淡出归零");
            assert!(!s.visible, "{mode:?} 淡出完成后必须隐藏");
        }
    }

    #[test]
    fn ring_white_luminous_loop_has_transparent_center_and_premultiplied_alpha() {
        let (sw, sh) = (120i32, 120i32);
        let panel = Rect { left: 31, top: 31, right: 89, bottom: 89 }; // 58x58 中号白环
        let mut buf = vec![0u8; (sw * sh * 4) as usize];
        unsafe {
            paint_orb_white_ring_at(buf.as_mut_ptr(), sw, sh, &panel, 1.0, 0.45, 1.1, 1.0);
        }
        // 中心像素（60, 60）：视觉完全透明（仅允许 <=1 的热区命中保底 alpha，RGB=0）
        let c_off = ((60 * sw + 60) * 4) as usize;
        assert_eq!(
            (buf[c_off], buf[c_off + 1], buf[c_off + 2]),
            (0, 0, 0),
            "白环中心必须无任何填充色"
        );
        assert!(buf[c_off + 3] <= 1, "白环中心必须完全透明: a={}", buf[c_off + 3]);

        let mut bright_white_px = 0usize;
        for y in 0..sh {
            for x in 0..sw {
                let off = ((y * sw + x) * 4) as usize;
                let (b, g, r, a) = (buf[off], buf[off + 1], buf[off + 2], buf[off + 3]);
                assert!(b <= a && g <= a && r <= a, "预乘 alpha 越界 ({x},{y}): [{b},{g},{r},{a}]");
                let inside = x >= panel.left && x < panel.right && y >= panel.top && y < panel.bottom;
                if !inside {
                    assert_eq!((b, g, r, a), (0, 0, 0, 0), "不应画到 panel 之外");
                }
                if a > 120 {
                    assert_eq!((b, g, r), (b, b, b), "白环应该是纯粹白色");
                    bright_white_px += 1;
                }
            }
        }
        assert!(bright_white_px > 60, "白环应该画出清晰连续的发光白线: bright={bright_white_px}");
    }

    #[test]
    fn ring_sensitivity_presets_scale_quiet_speech_and_reject_floor() {
        let mut norms = [0.0f32; 3];
        for (idx, sens) in [RingSens::Low, RingSens::Medium, RingSens::High].iter().enumerate() {
            let mut s = State {
                mode: Mode::Ring,
                visible: true,
                orb_holding: true,
                orb_noise_floor: 0.0025,
                orb_peak_agc: 0.0,
                ..State::default()
            };
            for _ in 0..8 {
                push_level_with_sens(&mut s, 0.0012, *sens);
            }
            assert!(s.orb_last_voice.is_none(), "{sens:?} 不应把底噪 0.0012 误判为说话");
            push_level_with_sens(&mut s, 0.015, *sens);
            assert!(s.orb_last_voice.is_some(), "{sens:?} 柔和说话 0.015 应触发");
            norms[idx] = s.orb_level_norm;
        }
        assert!(
            norms[0] < norms[1] && norms[1] < norms[2],
            "白环灵敏度应严格递增: low={} < med={} < high={}",
            norms[0],
            norms[1],
            norms[2]
        );
        assert!(norms[1] >= 0.45, "中档白环在柔和说话 0.015 下应有充沛起伏: med={}", norms[1]);
    }

    /// 🔴 实测:被裁剪的 `Clear` 会不会动到画布上**没被清到**的地方。
    ///
    /// 这条决定了「脏矩形清零」能不能做。`ID2D1DCRenderTarget` 的官方说明是
    /// 「先画进一张内部位图,再用 GDI 贴回 DC」,但**没说贴的范围**:
    /// 万一它每帧把整张内部位图原样贴回来,那"只清脏矩形"就是错的 ——
    /// 内部位图里上一帧的旧内容会被贴回来,在屏幕上留鬼影,而且会盖掉我们
    /// 按账本刚擦干净的区域(那样只能用"整块清"或改成累计区域,方案完全不同)。
    ///
    /// 做法:第 1 帧在 A 画东西(此后 D2D 内部 A 处有内容);按真实流程把 A
    /// **自己擦干净**(模拟账本里"清上一帧区域"这一步);第 2 帧只在 B 画、
    /// 且只让 D2D 清 B —— 然后回头看 A 是不是又被贴上了内容。
    ///
    /// 2026-10-31 本机(Direct2D 软件路径)结果:A 保持全透明 ⇒ 没被清到的地方
    /// 不会被 D2D 改写,裁剪清零成立(AUDIT §5.1 的前提)。
    #[test]
    fn clipped_clear_keeps_untouched_area() {
        use windows::Win32::Graphics::Direct2D::Common as d2dc;
        unsafe {
            let n = 64;
            let mem_dc = CreateCompatibleDC(0);
            assert!(mem_dc != 0, "CreateCompatibleDC failed");
            let bmi = BitmapInfo {
                header: BitmapInfoHeader {
                    bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                    bi_width: n,
                    bi_height: -n,
                    bi_planes: 1,
                    bi_bit_count: 32,
                    bi_compression: 0,
                    bi_size_image: (n * n * 4) as u32,
                    ..Default::default()
                },
                colors: [0; 3],
            };
            let mut bits: *mut u8 = ptr::null_mut();
            let dib = CreateDIBSection(mem_dc, &bmi, 0, &mut bits, 0, 0);
            assert!(dib != 0 && !bits.is_null(), "CreateDIBSection failed");
            let old_bmp = SelectObject(mem_dc, dib);
            ptr::write_bytes(bits, 0, (n * n * 4) as usize);

            let canvas = Rect {
                left: 0,
                top: 0,
                right: n,
                bottom: n,
            };
            let a = Rect { left: 8, top: 8, right: 24, bottom: 24 };
            let b = Rect { left: 40, top: 40, right: 56, bottom: 56 };
            // 画布上 (x,y) 处的 4 个字节(BGRA),第 4 个是 alpha
            let at = |x: i32, y: i32| -> [u8; 4] {
                let mut out = [0u8; 4];
                for (k, slot) in out.iter_mut().enumerate() {
                    *slot = *bits.add((y as usize * n as usize + x as usize) * 4 + k);
                }
                out
            };
            let fill = |r: &Rect, color_word: u32| {
                with_target(mem_dc, &canvas, std::slice::from_ref(r), (0.0, 0.0), |t| {
                    if let Ok(br) =
                        t.target.CreateSolidColorBrush(&argb(color_word) as *const _, None)
                    {
                        t.target.FillRectangle(
                            &d2dc::D2D_RECT_F {
                                left: r.left as f32,
                                top: r.top as f32,
                                right: r.right as f32,
                                bottom: r.bottom as f32,
                            },
                            &br,
                        );
                    }
                });
            };

            // 第 1 帧:清整块(把 A 写进 D2D 的内部位图),在 A 里画一块不透明的红
            fill(&a, 0xFF_FF0000);
            assert!(at(16, 16)[3] > 0, "第 1 帧后 A 应该有内容,否则这条测试没有意义");

            // 真实流程里「账本把上一帧区域擦掉」这一步
            for y in a.top..a.bottom {
                ptr::write_bytes(
                    bits.add((y as usize * n as usize + a.left as usize) * 4),
                    0,
                    ((a.right - a.left) * 4) as usize,
                );
            }
            assert_eq!(at(16, 16)[3], 0, "A 应该已被账本擦干净");

            // 第 2 帧:只让 D2D 清 B,并在 B 里画东西(保证这一帧确实有绘制、有回贴)
            fill(&b, 0xFF_00FF00);
            assert_eq!(
                at(16, 16)[3], 0,
                "A 又被贴上了内容 —— D2D 把整张内部位图回贴了,脏矩形清零不成立"
            );
            assert_eq!(at(48, 48), [0, 255, 0, 255], "B 里应该画上了不透明的绿");

            RT.with(|s| *s.borrow_mut() = None);
            SelectObject(mem_dc, old_bmp);
            DeleteObject(dib);
            DeleteDC(mem_dc);
        }
    }
}
