//! 系统托盘图标 —— 让 ouuutput 有「看得见的存在感」。
//!
//! 为什么需要它：ouuutput 本体是个控制台程序，用户双击 start.bat 之后只有一个黑窗口，
//! 关掉就没了、也看不出它到底在不在跑。托盘图标解决三件事：
//!   1) 一眼看到「它在运行」；
//!   2) 右键能退出（不用去黑窗口按 Ctrl+C）；
//!   3) 启动时弹一个气泡告诉用户怎么用。
//!
//! 图标是自己按像素画的（3x3 超采样成 32x32 带 alpha 的麦克风），不依赖任何外部资源。
//! 线程模型与 overlay 一致：独立线程持隐藏窗口 + 消息循环，主线程只读它的请求。

use std::collections::VecDeque;
use std::ptr;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use crate::hotkey::{DispatchMessageW, GetMessageW, Msg, TranslateMessage};

pub const TRAY_CLASS: &str = "OuuutputTrayWnd";

const WM_TRAY: u32 = 0x0400 + 100; // WM_APP + 100
const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_CONTEXTMENU: u32 = 0x007B;
const WM_COMMAND: u32 = 0x0111;
const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;

const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIF_TIP: u32 = 0x04;
const NIF_INFO: u32 = 0x10;

pub(crate) const ID_EXIT: usize = 2;
pub(crate) const ID_PUNCT_SPACE: usize = 10;
pub(crate) const ID_MERGE_TERMS: usize = 11;
pub(crate) const ID_CTX_BIAS: usize = 12;
pub(crate) const ID_CTX_VERBOSE: usize = 13;
pub(crate) const ID_LEADING_SPACE: usize = 14;
pub(crate) const ID_FIX_CASE: usize = 15;
/// 句尾补一个空格(和 ID_LEADING_SPACE 成对:一个管前面、一个管后面)
pub(crate) const ID_TRAILING_SPACE: usize = 16;
/// 静音断句子菜单的基址（5/10/15/30/不断句）
pub(crate) const ID_SILENCE_BASE: usize = 200;
pub(crate) const SILENCE_CHOICES: [u32; 5] = [5, 10, 15, 30, 0];
/// 查看运行日志（隐藏控制台后改为 ShellExecuteW 打开当天日志文件）
pub(crate) const ID_SHOW_CONSOLE: usize = 17;
/// 开机自启开关（写 HKCU\...\Run）
pub(crate) const ID_AUTOSTART: usize = 18;
/// 悬浮显示方式：关 / 悬浮声波 / 纯白流体圆环 / 流光光晕（细条 1 与 边缘光 3 已停用注释）
pub(crate) const ID_OVERLAY_BASE: usize = 300;
pub(crate) const ID_EDGE_TINT_BASE: usize = 320;
/// 边缘光**带宽**基址(细/中/宽)
pub(crate) const ID_EDGE_SPREAD_BASE: usize = 340;
/// 边缘光**亮度**基址(淡/中/亮)
pub(crate) const ID_EDGE_PEAK_BASE: usize = 350;
/// 边缘光**速度**基址(慢/中/快)
pub(crate) const ID_EDGE_SPEED_BASE: usize = 360;
/// 边缘光**重绘帧率**基址(60fps / 30fps 省电)
pub(crate) const ID_EDGE_FPS_BASE: usize = 370;
/// 流光光晕颜色基址（blue/cyan/violet/siri/rainbow/cyberpunk/emerald/sunset，共 8 项）
pub(crate) const ID_HALO_TINT_BASE: usize = 380;
/// 流光光晕**范围**基址（紧凑/标准/宽广）
pub(crate) const ID_HALO_SPREAD_BASE: usize = 390;
/// 流光光晕**流速**基址（缓/中/快）
pub(crate) const ID_HALO_SPEED_BASE: usize = 395;
/// 流光光晕**浓淡**基址（轻/中/浓）
pub(crate) const ID_HALO_OPACITY_BASE: usize = 400;
/// 悬浮声波 / 白环归位按钮
pub(crate) const ID_ORB_RESET: usize = 415;
/// 悬浮声波大小预设基址（0=小, 1=中, 2=大）
pub(crate) const ID_ORB_SIZE_BASE: usize = 420;
/// 纯白流体圆环大小预设基址（0=小, 1=中, 2=大）
pub(crate) const ID_RING_SIZE_BASE: usize = 430;
pub const INDICATOR_SIZE_NAMES: [&str; 3] = ["小", "中", "大"];
pub const INDICATOR_SIZE_CONF: [&str; 3] = ["small", "medium", "large"];
/// 纯白流体圆环灵敏度预设基址（0=低, 1=中, 2=高）
pub(crate) const ID_RING_SENS_BASE: usize = 440;
pub const RING_SENS_NAMES: [&str; 3] = ["低", "中", "高"];
pub const RING_SENS_CONF: [&str; 3] = ["low", "medium", "high"];
///
/// 为什么给三档而不是让用户填数字:托盘里没有输入框,而这三档覆盖了
/// "几乎看不见 / 刚好 / 很显眼"的实际需求;真要精确值就走配置项。
pub const EDGE_SPREAD_CHOICES: [i32; 3] = [40, 56, 76];
pub const EDGE_SPREAD_NAMES: [&str; 3] = ["细(40px)", "中(56px)", "宽(76px)"];
pub const EDGE_PEAK_CHOICES: [f32; 3] = [0.28, 0.42, 0.62];
pub const EDGE_PEAK_NAMES: [&str; 3] = ["淡", "中", "亮"];
pub const EDGE_SPEED_CHOICES: [f32; 3] = [0.6, 1.0, 1.8];
pub const EDGE_SPEED_NAMES: [&str; 3] = ["慢", "中", "快"];
/// 写回 `ouuutput.conf` 时用的字面量(必须是 `&'static str`:托盘那条路只吃静态串)。
pub const EDGE_SPREAD_CONF: [&str; 3] = ["40", "56", "76"];
pub const EDGE_PEAK_CONF: [&str; 3] = ["0.28", "0.42", "0.62"];
pub const EDGE_SPEED_CONF: [&str; 3] = ["0.6", "1.0", "1.8"];
/// 边缘光/流光重绘帧率(两档;实测 60fps 自身吃 0.578 核、30fps 吃 0.255 核 —— 省 56%)。
pub const EDGE_FPS_CHOICES: [u32; 2] = [60, 30];
pub const EDGE_FPS_NAMES: [&str; 2] = ["60fps(默认,最滑)", "30fps(省电,省一半绘制)"];
pub const EDGE_FPS_CONF: [&str; 2] = ["60", "30"];
/// 流光光晕范围/流速/浓淡三档预置：(blur_radius, peak_thickness, valley_thickness)。
pub const HALO_SPREAD_CHOICES: [(f32, f32, f32); 3] = [
    (28.0, 20.0, 8.0),
    (52.0, 33.0, 12.0),
    (86.0, 54.0, 20.0),
];
pub const HALO_SPREAD_SHORT: [&str; 3] = ["紧凑", "标准", "宽广"];
pub const HALO_BLUR_CONF: [&str; 3] = ["28", "52", "86"];
pub const HALO_PEAK_CONF: [&str; 3] = ["20", "33", "54"];
pub const HALO_VALLEY_CONF: [&str; 3] = ["8", "12", "20"];
pub const HALO_SPEED_CHOICES: [f32; 3] = [0.28, 0.50, 0.95];
pub const HALO_SPEED_NAMES: [&str; 3] = ["缓", "中", "快"];
pub const HALO_SPEED_CONF: [&str; 3] = ["0.28", "0.5", "0.95"];
pub const HALO_OPACITY_CHOICES: [f32; 3] = [0.52, 0.80, 0.98];
pub const HALO_OPACITY_NAMES: [&str; 3] = ["轻", "中", "浓"];
pub const HALO_OPACITY_CONF: [&str; 3] = ["0.52", "0.8", "0.98"];
pub(crate) const ID_RELEASE_KEYS: usize = 19;
pub(crate) const ID_UNDO_LAST: usize = 20;
pub(crate) const ID_RESTART: usize = 21;
pub(crate) const ID_HOTKEY_BASE: usize = 100;

/// 托盘菜单里可以点的开关。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayToggle {
    /// 中文标点转空格（问号保留）
    PunctToSpace,
    /// 英文拆字合并（"R E A" -> "REA"）
    MergeTerms,
    /// 上下文偏置
    CtxBias,
    /// 偏置生效时在控制台打提示
    CtxVerbose,
    /// 光标前有字时在前面补一个空格
    LeadingSpace,
    /// 句尾补一个空格(下一句接上来不会粘在一起)
    TrailingSpace,
    /// 英文大小写修复（模型吐不出小写）
    FixCase,
    /// 静音多少秒算一句说完
    SetSilence(u32),
    /// 换热键（换成候选表里第 n 个）
    SetHotkey(usize),
    /// 把控制台窗口叫出来
    ShowConsole,
    /// 把**上一句**从上屏过的那个窗口里收回去(T5.2)。焦点不在那儿就拒绝(见 worker)。
    UndoLast,
    /// 开机自启开关
    AutoStart,
    /// 悬浮显示方式：0=关 2=悬浮声波 4=流光光晕 5=纯白流体圆环（1=细条 与 3=边缘光 已注释停用）
    SetOverlayMode(usize),
    /// 悬浮声波大小（0=小 1=中 2=大）
    SetOrbSize(usize),
    /// 纯白流体圆环大小（0=小 1=中 2=大）
    SetRingSize(usize),
    /// 纯白流体圆环灵敏度（0=低 1=中 2=高）
    SetRingSens(usize),
    /// 流光光晕配色：0..7（电光蓝/天青水色/蓝紫极光/Siri/彩虹/赛博霓虹/翡翠幽光/落日熔金）
    SetHaloTint(usize),
    SetHaloSpread(usize),
    SetHaloSpeed(usize),
    SetHaloOpacity(usize),
    /// 边缘光颜色：0=白 1=彩虹 2=暖橙 3=青蓝 4=极光
    SetEdgeTint(usize),
    /// 边缘光带宽/亮度/速度:各三档(下标见 `EDGE_*_CHOICES`)
    SetEdgeSpread(usize),
    SetEdgePeak(usize),
    SetEdgeSpeed(usize),
    /// 边缘光重绘帧率:两档(下标见 `EDGE_FPS_CHOICES`)
    SetEdgeFps(usize),
    /// 释放卡住的修饰键（键盘失灵时自救）
    ReleaseKeys,
    /// 重置悬浮声波位置到默认任务栏上方居中
    ResetOrbPos,
}

/// 热键候选表 —— 托盘「换热键」子菜单就照这个列。
///
/// 为什么是"选"而不是"录入"：让用户录一次按键要抓键、要处理冲突、要弹窗，
/// 是一整个小功能；先给一份**覆盖绝大多数人**的候选，用起来最直接。
pub const HOTKEY_CANDIDATES: &[(&str, &str)] = &[
    ("mousex1", "鼠标侧键 X1（后侧）"),
    ("mousex2", "鼠标侧键 X2（前侧）"),
    ("rightctrl", "右 Ctrl"),
    ("rightalt", "右 Alt"),
    ("f8", "F8"),
    ("f9", "F9"),
    ("f10", "F10"),
];

/// 主循环每拍读一次：托盘点击排队队列（VecDeque 防快速连击丢事件）。
static PENDING: std::sync::Mutex<VecDeque<TrayToggle>> = std::sync::Mutex::new(VecDeque::new());
/// 托盘图标的数据块 —— 必须留着：Explorer 重启后要拿同一份数据重新 NIM_ADD。
static TRAY_NID: std::sync::Mutex<Option<NotifyIconDataW>> = std::sync::Mutex::new(None);
/// Explorer 重启时广播的消息 id（RegisterWindowMessageW 的返回值；0 = 没注册成功）。
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// 当前开关状态（由主循环推给托盘，用来画勾）。
static FLAGS: std::sync::Mutex<TrayFlags> = std::sync::Mutex::new(TrayFlags {
    punct_to_space: false,
    merge_terms: true,
    ctx_bias: true,
    ctx_verbose: false,
    leading_space: true,
    trailing_space: true,
    fix_case: true,
    silence_sec: 15,
    overlay_mode: 0,
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
    hotkey: "rightctrl",
    autostart: false,
});
/// 托盘要显示的开关状态。
#[derive(Clone, Copy)]
pub struct TrayFlags {
    pub punct_to_space: bool,
    pub merge_terms: bool,
    pub ctx_bias: bool,
    pub ctx_verbose: bool,
    /// 光标前有字时补一个前置空格。
    pub leading_space: bool,
    /// 句尾补一个空格(只补一个)。
    pub trailing_space: bool,
    /// 英文大小写修复。
    pub fix_case: bool,
    /// 当前静音断句门槛（秒；0 = 不断句）。
    pub silence_sec: u32,
    /// 悬浮显示方式：0=关 2=悬浮声波 4=流光光晕 5=纯白流体圆环（1=细条 与 3=边缘光 已注释停用）
    pub overlay_mode: usize,
    /// 流光光晕配色：0..7
    pub halo_tint: usize,
    /// 悬浮声波大小档位(0=小 1=中 2=大)
    pub orb_size_idx: usize,
    /// 纯白流体圆环大小档位(0=小 1=中 2=大)
    pub ring_size_idx: usize,
    /// 纯白流体圆环灵敏度档位(0=低 1=中 2=高)
    pub ring_sens_idx: usize,
    /// 流光光晕范围/流速/浓淡当前档位(0..2)
    pub halo_spread_idx: usize,
    pub halo_speed_idx: usize,
    pub halo_opacity_idx: usize,
    pub edge_tint: usize,
    /// 边缘光带宽/亮度/速度当前档位(0..2),用来在子菜单里打勾。
    pub edge_spread_idx: usize,
    pub edge_peak_idx: usize,
    pub edge_speed_idx: usize,
    /// 边缘光帧率档位下标(见 `EDGE_FPS_CHOICES`)
    pub edge_fps_idx: usize,
    /// 当前热键的配置写法（用来在子菜单里打勾），例如 "mousex1"。
    pub hotkey: &'static str,
    /// 是否已设置开机自启。
    pub autostart: bool,
}

/// 主循环把当前生效状态推给托盘（决定菜单里哪几项打勾）。
/// 托盘**图标**注册成功了吗(窗口本身与它无关 —— 见 `Tray::start` 里的说明)。
///
/// 为什么单独一个标志:没有 shell 的桌面(自动化用的私有桌面)上注册必然失败,
/// 但托盘窗口照样要跑起来 —— 它是菜单的宿主、也是 `WM_COMMAND` 的去处。
static ICON_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_flags(f: TrayFlags) {
    if let Ok(mut g) = FLAGS.lock() {
        *g = f;
    }
    crate::tray_menu::notify_refresh();
}

pub(crate) fn snapshot_flags() -> TrayFlags {
    FLAGS.lock().map(|g| *g).unwrap_or(TrayFlags {
        punct_to_space: false,
        merge_terms: true,
        ctx_bias: true,
        ctx_verbose: false,
        leading_space: true,
        trailing_space: true,
        fix_case: true,
        silence_sec: 15,
        overlay_mode: 0,
        edge_tint: 0,
        halo_tint: 0,
        orb_size_idx: 1,
        ring_size_idx: 1,
        ring_sens_idx: 1,
        halo_spread_idx: 1,
        halo_speed_idx: 1,
        halo_opacity_idx: 1,
        edge_spread_idx: 1,
        edge_peak_idx: 1,
        edge_speed_idx: 1,
        edge_fps_idx: 0,
        hotkey: "rightctrl",
        autostart: false,
    })
}

/// 设置托盘的悬停提示（带版本号与当前热键）。
pub fn set_title(t: &str) {
    if let Ok(mut g) = TRAY_NID.lock() {
        if let Some(nid) = g.as_mut() {
            set_fixed(&mut nid.sz_tip, t);
            if ICON_OK.load(Ordering::Relaxed) {
                nid.u_flags = NIF_TIP;
                unsafe { Shell_NotifyIconW(NIM_MODIFY, nid); }
            }
        }
    }
}

/// 把热键配置写法翻成人话（托盘菜单标题用）。
/// 只认托盘候选表里的那几种 + 常见键；认不出就原样显示，不假装认识。
pub fn key_label_of(conf: &str) -> &'static str {
    match conf {
        "mousex1" => "鼠标侧键 X1",
        "mousex2" => "鼠标侧键 X2",
        "mousemiddle" => "鼠标中键",
        "rightctrl" => "右 Ctrl",
        "leftctrl" => "左 Ctrl",
        "rightalt" => "右 Alt",
        "leftalt" => "左 Alt",
        "rightshift" => "右 Shift",
        "leftshift" => "左 Shift",
        "capslock" => "CapsLock",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "backquote" => "`",
        _ => "?",
    }
}

/// 取走一次托盘开关点击（主循环用；没有就返回 None）。
pub fn take_toggle() -> Option<TrayToggle> {
    PENDING.lock().ok().and_then(|mut g| g.pop_front())
}
/// 托盘窗口回调里记录「用户点了退出」。
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 需要在退出时唤醒的主线程（热键消息循环所在线程）。
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconDataW {
    cb_size: u32,
    hwnd: isize,
    u_id: u32,
    u_flags: u32,
    u_callback_message: u32,
    h_icon: isize,
    sz_tip: [u16; 128],
    dw_state: u32,
    dw_state_mask: u32,
    sz_info: [u16; 256],
    u_timeout_or_version: u32,
    sz_info_title: [u16; 64],
    dw_info_flags: u32,
    guid_item: [u8; 16],
    h_balloon_icon: isize,
}

#[repr(C)]
struct IconInfo {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: isize,
    hbm_color: isize,
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

#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(message: u32, data: *mut NotifyIconDataW) -> i32;
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
    fn PostMessageW(hwnd: isize, msg: u32, wp: usize, lp: isize) -> i32;
    fn PostQuitMessage(code: i32);
    fn PostThreadMessageW(thread: u32, msg: u32, wp: usize, lp: isize) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn GetModuleHandleW(name: *const u16) -> isize;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn EnumWindows(cb: Option<unsafe extern "system" fn(isize, isize) -> i32>, lp: isize) -> i32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
}

#[link(name = "user32")]
extern "system" {
    fn SetWindowTextW(hwnd: isize, text: *const u16) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn FreeConsole() -> i32;
    fn SetStdHandle(which: u32, handle: isize) -> i32;
    fn CreateFileW(
        name: *const u16, access: u32, share: u32, sec: *const u8,
        disposition: u32, flags: u32, template: isize,
    ) -> isize;
    fn GetConsoleWindow() -> isize;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateDIBSection(
        hdc: isize,
        bmi: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut u8,
        section: isize,
        offset: u32,
    ) -> isize;
    fn CreateBitmap(width: i32, height: i32, planes: u32, bits_per_pel: u32, bits: *const u8) -> isize;
    fn CreateIconIndirect(info: *const IconInfo) -> isize;
    fn DeleteObject(obj: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

fn set_fixed<const N: usize>(dst: &mut [u16; N], s: &str) {
    let w = wide(s);
    let n = w.len().min(N);
    dst[..n].copy_from_slice(&w[..n]);
    if n < N {
        dst[n] = 0;
    }
}

/// 托盘句柄。起不来时是 None（调用方按「没有也不影响录音」处理）。
#[derive(Clone)]
pub struct Tray {
    enabled: bool,
}

impl Tray {
    /// 记录主线程 id，退出时用它唤醒热键消息循环。
    /// 托盘图标注册成功了吗。`false` = 这个桌面上没有 shell(私有桌面/自动化),
    /// 窗口仍然在、菜单仍然能点,只是看不到图标。
    pub fn icon_ok() -> bool {
        ICON_OK.load(Ordering::Relaxed)
    }

    pub fn note_main_thread() {
        MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    }

    pub fn start(tooltip: &str) -> Option<Tray> {
        let tip = tooltip.to_string();
        let (tx, rx) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("ouuutput-tray".into())
            .spawn(move || unsafe {
                // Explorer 重启会广播这条消息。名字注册一次，id 全进程有效。
                TASKBAR_CREATED.store(
                    RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
                    Ordering::Relaxed,
                );
                let hwnd = create_window();
                if hwnd == 0 {
                    let _ = tx.send(false);
                    return;
                }
                let icon = make_icon();
                let mut nid = new_nid(hwnd);
                nid.u_flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
                nid.u_callback_message = WM_TRAY;
                nid.h_icon = icon;
                set_fixed(&mut nid.sz_tip, &tip);
                // 图标注册失败**不代表托盘窗口没用**:窗口是右键菜单的宿主,
                // 也是 `WM_COMMAND` 的去处(测试/诊断就靠它)。没有 shell 的桌面上
                // (例如自动化用的私有桌面)注册必然失败,以前这里直接 return 把整条
                // 都关掉了 —— 于是"点菜单"这件事实测不了。现在照样把窗口跑起来,
                // 只把"图标没注册"如实报出去。
                let icon_ok = Shell_NotifyIconW(NIM_ADD, &mut nid) != 0;
                ICON_OK.store(icon_ok, Ordering::Relaxed);
                if icon_ok {
                    if let Ok(mut g) = TRAY_NID.lock() {
                        *g = Some(nid);
                    }
                }
                let _ = tx.send(true);
                let mut msg: Msg = std::mem::zeroed();
                while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if ICON_OK.load(Ordering::Relaxed) {
                    Shell_NotifyIconW(NIM_DELETE, &mut nid);
                }
                if icon != 0 {
                    DeleteObject(icon);
                }
            })
            .ok()?;
        match rx.recv_timeout(Duration::from_millis(2500)) {
            Ok(true) => Some(Tray { enabled: true }),
            _ => None,
        }
    }

    /// 弹一个气泡通知（启动提示、状态变化）。
    pub fn balloon(&self, title: &str, text: &str) {
        if !self.enabled {
            return;
        }
        let hwnd = match HWND.get() {
            Some(h) => *h,
            None => return,
        };
        unsafe {
            balloon_now(hwnd, title, text);
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// 给控制台窗口起个能认出来的标题（默认是那串 exe 路径，用户看不出是什么）。
pub fn set_console_title(title: &str) {
    unsafe {
        let c = GetConsoleWindow();
        if c == 0 {
            return;
        }
        let t = wide(title);
        SetWindowTextW(c, t.as_ptr());
    }
}

/// 隐藏控制台。
///
/// **不是** `ShowWindow(SW_HIDE)` —— 那个在 Windows Terminal 承载下无效
/// （可见窗口属于 WindowsTerminal.exe，不属于本进程，`GetConsoleWindow()`
/// 拿到的是另一个隐藏句柄）。
///
/// 真正有效的是 `FreeConsole()`：把进程从控制台分离。
/// 分离后标准输出失效，所以先把 stdout/stderr/stdin 指向 NUL，
/// 避免后续 println! 触发写错误。
pub fn set_console_visible(v: bool) {
    unsafe {
        if v {
            // 「显示」在 FreeConsole 之后没法真的恢复控制台（不可逆）。
            // 用 SW_SHOW 尽力而为 —— 如果进程还在控制台上就有用。
            let c = GetConsoleWindow();
            if c != 0 {
                ShowWindow(c, 5); // SW_SHOW
            }
            return;
        }

        // 把三个标准句柄指向 NUL，避免 FreeConsole 之后写管道出错。
        let nul = wide("NUL");
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const GENERIC_READ: u32 = 0x8000_0000;
        const OPEN_EXISTING: u32 = 3;
        // STD_INPUT=-10 / STD_OUTPUT=-11 / STD_ERROR=-12
        let h_out = CreateFileW(
            nul.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, 0,
        );
        if h_out != -1isize {
            SetStdHandle(0xFFFF_FFF5, h_out); // -11 as u32
            SetStdHandle(0xFFFF_FFF4, h_out); // -12 as u32
        }
        let h_in = CreateFileW(
            nul.as_ptr(), GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, 0,
        );
        if h_in != -1isize {
            SetStdHandle(0xFFFF_FFF6, h_in); // -10 as u32
        }

        FreeConsole();
    }
}

/// 让指定进程走「托盘退出」路径：它的主循环收尾、sidecar 由 Drop 结束。
///
/// 找不到它的托盘窗口（旧版没起托盘 / 托盘线程已死）时返回 false，
/// 交给调用方硬杀。跨进程 PostMessage 不需要命名管道 —— 类名固定，找得到就能发。
pub fn request_exit_of(pid: u32) -> bool {
    struct Ctx {
        pid: u32,
        sent: bool,
    }

    unsafe extern "system" fn find_and_exit(hwnd: isize, lp: isize) -> i32 {
        let ctx = &mut *(lp as *mut Ctx);
        let mut wpid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut wpid);
        if wpid != ctx.pid {
            return 1;
        }
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, buf.as_mut_ptr(), 64);
        if n <= 0 || String::from_utf16_lossy(&buf[..n as usize]) != TRAY_CLASS {
            return 1;
        }
        PostMessageW(hwnd, WM_COMMAND, ID_EXIT, 0);
        ctx.sent = true;
        0
    }

    let mut ctx = Ctx { pid, sent: false };
    unsafe {
        EnumWindows(Some(find_and_exit), &mut ctx as *mut Ctx as isize);
    }
    ctx.sent
}

static HWND: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn new_nid(hwnd: isize) -> NotifyIconDataW {
    NotifyIconDataW {
        cb_size: std::mem::size_of::<NotifyIconDataW>() as u32,
        hwnd,
        u_id: 1,
        u_flags: 0,
        u_callback_message: 0,
        h_icon: 0,
        sz_tip: [0; 128],
        dw_state: 0,
        dw_state_mask: 0,
        sz_info: [0; 256],
        u_timeout_or_version: 0,
        sz_info_title: [0; 64],
        dw_info_flags: 0,
        guid_item: [0; 16],
        h_balloon_icon: 0,
    }
}

unsafe fn create_window() -> isize {
    let inst = GetModuleHandleW(ptr::null());
    let cls = wide(TRAY_CLASS);
    let wc = WndClassW {
        style: 0,
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
    if RegisterClassW(&wc) == 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(1410) {
            return 0;
        }
    }
    let title = wide("ouuutput 输入法");
    let hwnd = CreateWindowExW(0, cls.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, 0, 0, inst, 0);
    if hwnd != 0 {
        let _ = HWND.set(hwnd);
    }
    hwnd
}

/// Explorer 重启会把通知区域整个重建，托盘图标随之消失。
/// 拿启动时存下的 nid 重新 ADD 一次 —— 不重新 ADD，进程还在但用户永远看不到图标。
/// 图标本来就在时 ADD 会失败（同一 hwnd+uID 重复注册），这不是错误，走 MODIFY 更新即可。
unsafe fn restore_icon() {
    if let Ok(mut g) = TRAY_NID.lock() {
        if let Some(nid) = g.as_mut() {
            nid.u_flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            if Shell_NotifyIconW(NIM_ADD, nid) == 0 {
                Shell_NotifyIconW(NIM_MODIFY, nid);
            }
        }
    }
}

unsafe fn balloon_now(hwnd: isize, title: &str, text: &str) {
    let mut nid = new_nid(hwnd);
    nid.u_flags = NIF_INFO;
    set_fixed(&mut nid.sz_info_title, title);
    set_fixed(&mut nid.sz_info, text);
    nid.dw_info_flags = 1; // NIIF_INFO
    Shell_NotifyIconW(NIM_MODIFY, &mut nid);
}

unsafe extern "system" fn wnd_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    // Explorer 崩溃/更新/重启后广播 TaskbarCreated。收到就重新挂图标，
    // 否则图标永久消失：ouuutput 照常工作，用户却以为它没开。
    let tc = TASKBAR_CREATED.load(Ordering::Relaxed);
    if tc != 0 && msg == tc {
        restore_icon();
        return 0;
    }
    match msg {
        WM_TRAY => {
            // 传统版本的托盘回调：lParam 就是鼠标消息
            let m = lp as u32 & 0xFFFF;
            if m == WM_RBUTTONUP || m == WM_CONTEXTMENU || m == WM_LBUTTONUP {
                crate::tray_menu::toggle_or_show();
            }
            0
        }
        WM_COMMAND => {
            let id = wp & 0xFFFF;
            trigger_action(id);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            crate::tray_menu::destroy();
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub(crate) fn trigger_action(id: usize) {
    if id == ID_EXIT {
        unsafe { request_exit(); }
    } else {
        queue_toggle(id);
    }
}
/// 把菜单 id 翻成开关并排队（主循环取走）。
///
/// 特例:「重启输入法」是**当场执行**的动作,不排队。原因:
///   1. 用户点了就是要"现在读最新版",等下一拍 tick 没有意义;
///   2. 新实例的接管逻辑会给**本进程**发托盘退出命令(`request_exit_of`,
///      PostMessage WM_COMMAND/ID_EXIT),而处理那条命令要求托盘线程的消息循环
///      还在跑 —— 所以这里只 spawn 新进程、不碰自己的状态,让后续的
///      WM_COMMAND(ID_EXIT) 走原来的收尾路径(主循环退出 → Sidecar Drop → 子进程不孤儿)。
/// 新进程:拿 takeover 锁 → 请旧的正常退出 → 旧的退完再真正跑起来。
/// 用户看到的等价于"把 start.bat 再双击一遍",但他不需要知道 exe 在哪。
fn queue_toggle(id: usize) {
    if id == ID_RESTART {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // 别给用户弹第二个黑窗口(和 spawn guard 时同一个理由)。
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            if let Ok(exe) = std::env::current_exe() {
                // `run --hide-console` = start.bat 的那条命令。
                // 不设 cwd:sidecar 的路径由 exe 位置往上找(find_root),与 cwd 无关。
                if let Err(e) = std::process::Command::new(exe)
                    .args(["run", "--hide-console"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                {
                    // 起不来也不能静默:用户点了菜单什么都没发生是最糟的体验。
                    // 托盘气泡是隐藏控制台后唯一的提示通道(和 sidecar 看门狗同一招)。
                    let hwnd = HWND.get().copied().unwrap_or(0);
                    if hwnd != 0 {
                        unsafe {
                            balloon_now(hwnd, "ouuutput", &format!("重启失败:{e}"));
                        }
                    }
                }
            }
        }
        // 不排队:主循环无事可做 —— 新实例的接管会把自己请下线。
        return;
    }
    // 热键候选是连续 id：ID_HOTKEY_BASE + 序号
    // 热键与静音断句候选均复用 module-level SILENCE_CHOICES
    if id >= ID_SILENCE_BASE && id < ID_SILENCE_BASE + SILENCE_CHOICES.len() {
        if let Ok(mut g) = PENDING.lock() {
            g.push_back(TrayToggle::SetSilence(SILENCE_CHOICES[id - ID_SILENCE_BASE]));
        }
        return;
    }
    if id >= ID_HOTKEY_BASE && id < ID_HOTKEY_BASE + HOTKEY_CANDIDATES.len() {
        if let Ok(mut g) = PENDING.lock() {
            g.push_back(TrayToggle::SetHotkey(id - ID_HOTKEY_BASE));
        }
        return;
    }
    let t = match id {
        ID_PUNCT_SPACE => Some(TrayToggle::PunctToSpace),
        ID_MERGE_TERMS => Some(TrayToggle::MergeTerms),
        ID_CTX_BIAS => Some(TrayToggle::CtxBias),
        ID_CTX_VERBOSE => Some(TrayToggle::CtxVerbose),
        ID_LEADING_SPACE => Some(TrayToggle::LeadingSpace),
        ID_TRAILING_SPACE => Some(TrayToggle::TrailingSpace),
        ID_FIX_CASE => Some(TrayToggle::FixCase),
        ID_SHOW_CONSOLE => Some(TrayToggle::ShowConsole),
        ID_AUTOSTART => Some(TrayToggle::AutoStart),
        ID_RELEASE_KEYS => Some(TrayToggle::ReleaseKeys),
        ID_UNDO_LAST => Some(TrayToggle::UndoLast),
        ID_ORB_RESET => Some(TrayToggle::ResetOrbPos),
        _ if id >= ID_OVERLAY_BASE && id <= ID_OVERLAY_BASE + 5 => {
            Some(TrayToggle::SetOverlayMode(id - ID_OVERLAY_BASE))
        }
        _ if id >= ID_ORB_SIZE_BASE && id < ID_ORB_SIZE_BASE + 3 => {
            Some(TrayToggle::SetOrbSize(id - ID_ORB_SIZE_BASE))
        }
        _ if id >= ID_RING_SIZE_BASE && id < ID_RING_SIZE_BASE + 3 => {
            Some(TrayToggle::SetRingSize(id - ID_RING_SIZE_BASE))
        }
        _ if id >= ID_RING_SENS_BASE && id < ID_RING_SENS_BASE + 3 => {
            Some(TrayToggle::SetRingSens(id - ID_RING_SENS_BASE))
        }
        _ if id >= ID_HALO_TINT_BASE && id < ID_HALO_TINT_BASE + 8 => {
            Some(TrayToggle::SetHaloTint(id - ID_HALO_TINT_BASE))
        }
        _ if id >= ID_HALO_SPREAD_BASE && id < ID_HALO_SPREAD_BASE + 3 => {
            Some(TrayToggle::SetHaloSpread(id - ID_HALO_SPREAD_BASE))
        }
        _ if id >= ID_HALO_SPEED_BASE && id < ID_HALO_SPEED_BASE + 3 => {
            Some(TrayToggle::SetHaloSpeed(id - ID_HALO_SPEED_BASE))
        }
        _ if id >= ID_HALO_OPACITY_BASE && id < ID_HALO_OPACITY_BASE + 3 => {
            Some(TrayToggle::SetHaloOpacity(id - ID_HALO_OPACITY_BASE))
        }
        _ if id >= ID_EDGE_TINT_BASE && id < ID_EDGE_TINT_BASE + 5 => {
            Some(TrayToggle::SetEdgeTint(id - ID_EDGE_TINT_BASE))
        }
        _ if id >= ID_EDGE_SPREAD_BASE && id < ID_EDGE_SPREAD_BASE + 3 => {
            Some(TrayToggle::SetEdgeSpread(id - ID_EDGE_SPREAD_BASE))
        }
        _ if id >= ID_EDGE_PEAK_BASE && id < ID_EDGE_PEAK_BASE + 3 => {
            Some(TrayToggle::SetEdgePeak(id - ID_EDGE_PEAK_BASE))
        }
        _ if id >= ID_EDGE_SPEED_BASE && id < ID_EDGE_SPEED_BASE + 3 => {
            Some(TrayToggle::SetEdgeSpeed(id - ID_EDGE_SPEED_BASE))
        }
        _ if id >= ID_EDGE_FPS_BASE && id < ID_EDGE_FPS_BASE + 2 => {
            Some(TrayToggle::SetEdgeFps(id - ID_EDGE_FPS_BASE))
        }
        _ => None,
    };
    if let Some(t) = t {
        if let Ok(mut g) = PENDING.lock() {
            g.push_back(t);
        }
    }
}

/// 退出：先把控制台还原（用户可能还要看日志），再让主线程的消息循环结束，
/// 这样 Sidecar 的 Drop 才会跑、子进程不会变成孤儿。
unsafe fn request_exit() {
    if EXIT_REQUESTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let c = GetConsoleWindow();
    if c != 0 {
        ShowWindow(c, 5); // SW_SHOW
    }
    let tid = MAIN_THREAD.load(Ordering::Relaxed);
    if tid != 0 {
        PostThreadMessageW(tid, 0x0012, 0, 0); // WM_QUIT
    }
}

/// 自己画一个 32x32 的 ouuut 标记图标（3x3 超采样抗锯齿）。
///
/// 形状 = 品牌字标里那个手绘 `o`（`ouuut-mark-square.svg` 的轮廓，矢量化自桌面手稿，
/// 与品牌参考 PNG 的形状 IoU 0.983）。轮廓数据 `MARK` 是**唯一真相源**：
/// `tools/make_icon.py` 直接从这里解析，不另存一份，免得多一处要同步的手抄版。
unsafe fn make_icon() -> isize {
    const S: i32 = 32;
    let mut bits_ptr: *mut u8 = ptr::null_mut();
    let bmi = BitmapInfo {
        header: BitmapInfoHeader {
            bi_size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            bi_width: S,
            bi_height: -S,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: 0,
            bi_size_image: (S * S * 4) as u32,
            ..Default::default()
        },
        colors: [0; 3],
    };
    let dib = CreateDIBSection(0, &bmi, 0, &mut bits_ptr, 0, 0);
    if dib == 0 || bits_ptr.is_null() {
        return 0;
    }
    let poly = mark_outline();
    const SS: i32 = 3;
    for py in 0..S {
        for px in 0..S {
            let mut hit = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if in_poly(&poly, x, y) {
                        hit += 1;
                    }
                }
            }
            let a = (hit * 255 / (SS * SS)) as u8;
            // BGRA：纸白 #F5F3ED（品牌手绘标记的米白，深色任务栏上清晰）
            let o = ((py * S + px) * 4) as usize;
            *bits_ptr.add(o) = 237; // B
            *bits_ptr.add(o + 1) = 243; // G
            *bits_ptr.add(o + 2) = 245; // R
            *bits_ptr.add(o + 3) = a;
        }
    }
    let mask = CreateBitmap(S, S, 1, 1, ptr::null());
    let info = IconInfo {
        f_icon: 1,
        x_hotspot: 0,
        y_hotspot: 0,
        hbm_mask: mask,
        hbm_color: dib,
    };
    let icon = CreateIconIndirect(&info);
    if mask != 0 {
        DeleteObject(mask);
    }
    if dib != 0 {
        DeleteObject(dib);
    }
    icon
}

/// ouuut 标记的轮廓：1000×1000 坐标系、单一环形体（even-odd 判内外）。
/// 来源 `F:/projects/ouuut-brand/logo/ouuut-mark-square.svg`；改标记就重跑
/// `tools/ouuut_mark_to_code.py` 重新生成这一段。
const MARK: [(f32, f32); 112] = [
    (427.02, 120.00), (393.91, 140.80), (354.70, 173.52), (327.62, 203.68), (305.84, 234.55), (285.35, 270.51),
    (270.86, 304.96), (258.18, 351.46), (251.53, 392.39), (250.21, 437.27), (254.05, 480.73), (262.93, 522.99),
    (276.96, 564.72), (295.68, 603.12), (318.22, 636.91), (342.68, 664.16), (374.53, 690.64), (396.23, 704.73),
    (419.13, 716.73), (443.06, 726.50), (467.88, 733.86), (496.67, 739.20), (527.79, 742.47), (584.75, 740.93),
    (614.30, 735.98), (643.21, 728.11), (670.48, 717.53), (690.04, 706.45), (706.85, 693.39), (719.85, 680.55),
    (726.69, 671.13), (738.75, 648.85), (748.96, 613.38), (751.93, 574.46), (749.12, 534.30), (740.13, 484.94),
    (721.79, 427.94), (705.74, 394.33), (687.61, 364.89), (662.30, 332.80), (635.40, 308.65), (607.55, 290.95),
    (577.13, 278.69), (545.04, 271.89), (512.21, 270.57), (479.54, 274.75), (447.95, 284.46), (416.17, 300.06),
    (580.29, 154.00), (598.42, 141.09), (611.56, 136.30), (632.18, 135.58), (650.11, 140.96), (670.88, 150.60),
    (702.07, 168.04), (731.66, 188.13), (757.20, 208.95), (780.44, 232.08), (801.62, 257.14), (821.27, 284.10),
    (831.80, 301.07), (848.11, 333.41), (858.60, 359.09), (866.88, 385.48), (873.44, 416.31), (877.68, 445.78),
    (879.08, 472.63), (877.59, 499.24), (872.24, 527.67), (867.82, 542.46), (857.90, 567.16), (847.14, 587.28),
    (832.47, 608.60), (811.34, 634.71), (708.94, 741.85), (639.89, 809.31), (610.96, 834.49), (584.30, 852.40),
    (556.71, 865.98), (531.10, 874.45), (504.77, 878.93), (473.16, 879.81), (439.09, 876.34), (402.29, 868.48),
    (367.05, 857.33), (340.48, 846.06), (306.92, 828.38), (273.73, 805.85), (243.90, 781.34), (219.80, 757.05),
    (198.17, 730.52), (177.66, 699.87), (160.03, 667.47), (146.67, 637.16), (135.95, 604.71), (127.92, 569.44),
    (122.90, 533.57), (120.93, 499.31), (122.41, 464.41), (127.26, 429.83), (135.31, 395.83), (146.40, 362.65),
    (162.28, 326.61), (179.22, 296.01), (199.23, 266.22), (218.10, 243.00), (242.43, 217.39), (287.53, 180.28),
    (317.69, 161.56), (360.71, 140.68), (396.72, 127.82), (427.02, 120.00),
];

/// 把 1000 空间的轮廓缩放进 32×32 图标坐标（留边距，托盘里不顶到边）。
fn mark_outline() -> [(f32, f32); MARK.len()] {
    const S: f32 = 32.0;
    const PAD: f32 = 2.0;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in MARK.iter() {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let scale = ((S - 2.0 * PAD) / (x1 - x0)).min((S - 2.0 * PAD) / (y1 - y0));
    let ox = (S - (x1 - x0) * scale) * 0.5;
    let oy = (S - (y1 - y0) * scale) * 0.5;
    let mut out = [(0.0f32, 0.0f32); MARK.len()];
    for (i, &(x, y)) in MARK.iter().enumerate() {
        out[i] = (ox + (x - x0) * scale, oy + (y - y0) * scale);
    }
    out
}

/// even-odd 判点是否在多边形内（单环形体；自交处按奇偶翻转，正好是「环」）。
fn in_poly(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
    let n = poly.len();
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) {
            let xint = (xj - xi) * (y - yi) / (yj - yi) + xi;
            if x < xint {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}