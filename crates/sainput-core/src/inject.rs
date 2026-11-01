//! 上屏：SendInput + 流式改写。
//!
//! 手写 Win32 绑定而不是用 windows crate —— 这几个结构体的 ABI 十几年没变，
//! 手工声明比跟 crate 版本较劲更可靠。

use anyhow::Result;

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const VK_BACK: u16 = 0x08;

#[repr(C)]
#[derive(Clone, Copy)]
struct KEYBDINPUT {
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MOUSEINPUT {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HARDWAREINPUT {
    u_msg: u32,
    w_param_l: u16,
    w_param_h: u16,
}

#[repr(C)]
union InputUnion {
    mi: MOUSEINPUT,
    ki: KEYBDINPUT,
    hi: HARDWAREINPUT,
}

#[repr(C)]
struct Input {
    type_: u32,
    u: InputUnion,
}

#[link(name = "user32")]
extern "system" {
    fn SendInput(c_inputs: u32, p_inputs: *const Input, cb_size: i32) -> u32;
}

fn unicode_event(unit: u16, keyup: bool) -> Input {
    Input {
        type_: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KEYBDINPUT {
                w_vk: 0,
                w_scan: unit,
                dw_flags: KEYEVENTF_UNICODE | if keyup { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
}

fn vk_event(vk: u16, keyup: bool) -> Input {
    Input {
        type_: INPUT_KEYBOARD,
        u: InputUnion {
            ki: KEYBDINPUT {
                w_vk: vk,
                w_scan: 0,
                dw_flags: if keyup { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dw_extra_info: 0,
            },
        },
    }
}

fn send(inputs: &[Input]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<Input>() as i32) };
    if n as usize != inputs.len() {
        anyhow::bail!("SendInput 只接受了 {n}/{} 个事件（目标窗口可能拒绝注入）", inputs.len());
    }
    Ok(())
}

/// 用 KEYEVENTF_UNICODE 打一段文本（支持中文）。
pub fn type_text(s: &str) -> Result<()> {
    let mut v: Vec<Input> = Vec::with_capacity(s.len() * 2);
    for u in s.encode_utf16() {
        v.push(unicode_event(u, false));
        v.push(unicode_event(u, true));
    }
    send(&v)
}

/// 退格 n 次。
pub fn backspace(n: usize) -> Result<()> {
    let mut v: Vec<Input> = Vec::with_capacity(n * 2);
    for _ in 0..n {
        v.push(vk_event(VK_BACK, false));
        v.push(vk_event(VK_BACK, true));
    }
    send(&v)
}

// ------------------------------------------------------------------ 上屏方式
//
// 为什么需要第二条路(2026-10-28 实测):目标窗口所在线程挂着中文/日文输入法时,
// **注入的字母会被输入法当成它自己的输入** —— 落进它的组字串,弹出候选面板,
// 程序一个字都收不到;中文汉字反倒不受影响(输入法不吃非字母)。
// 于是「说英文它就被吃掉 / 全删掉」。这不是模型的问题:同一段音频,识别原文是完整的。
//
// 输入法的状态读不到也改不了:跨进程 `ImmGetContext(目标窗口)` 返回 NULL;
// 用 `WM_INPUTLANGCHANGEREQUEST` 把窗口切到别的键盘布局、或者改 IMM32 的
// open/conversion 状态,输入法照样把字母收走。所以对这类窗口改走
// **PostMessage(WM_CHAR)**:消息不经过键盘那条链,输入法看不到,
// 实测在经典文本控件上「无焦点也能完整落字 + 退格照常」。
//
// 2026-10-30 补:Qt 窗口是同一类现场(`class_needs_message_channel` 的 `qt*`),
// 微信 4.x = `Weixin.exe` / `Qt51514QWindowIcon` 实测:
//   SendInput 的 `helloTOKENabc` → 输入框**一个字符都没有**(字母全进了水杉输入法的
//   候选面板,用户截图里就是「O'K → 哦*/欧*/噢*」);剪贴板读回是空的。
//   同一窗口 `PostMessage(WM_CHAR)` → 逐字落地 `helloTOKENabc`;
//   `WM_KEYDOWN/UP VK_BACK` ×3 → `helloTOKEN`(退格认);中文混英文也一字不差。
//
// 2026-11-01 改:`auto` 从此**只看「目标线程挂不挂输入法」**,不再要求窗口类名落在
// 白名单里(`class_needs_message_channel` 降级为诊断参考)。用户实测:在一个**不在白名单**的
// 窗口里,说出的英文词(`bug`)被该窗口线程挂着的输入法当成它自己的输入吃掉 —— 用户看到
// "输入法跳到候选面板",后半句直接没了。这正是白名单策略的老毛病:**漏一个程序就丢一次字,
// 而丢字是静默的**(用户看不到报错,只看到字没了)。相比之下"换通道在个别窗口可能不适用"
// 是次要风险,而且有逃生门(`off:<exe>` / `keys`)兜。每句开始时选定一次通道,
// **句中途不换**(混发两条通道会乱序,见 `type_text_with`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyMode {
    /// SendInput + KEYEVENTF_UNICODE(默认路径,绝大多数窗口都对)
    Keys,
    /// 走窗口消息、**照常流式**:字符用 PostMessage(WM_CHAR),退格用
    /// PostMessage(WM_KEYDOWN/UP, VK_BACK)。同一个消息队列,先后顺序天然一致。
    WmChar(isize),
    /// 走窗口消息、**整句一次投递**(不流式):给「连消息里的退格都不认」的程序兜底。
    /// 代价是松手才出字,但没有退格就不存在「记账 vs 屏幕」的偏差。
    WmCharFinal(isize),
    /// **Pebrel 控制面**:一个按键都不发。整句塞进它自己的组合态缓冲层
    /// (`pane.compose` 的 preedit,可无限次整体替换),定稿时才作为一次写入提交给
    /// PTY。见 `crate::pebrel`。
    Pebrel,
    /// **TSF 组合态**:一个按键都不发。文本走命名管道交给目标进程里的 TSF 薄壳
    /// (`crates/sainput-tsf`),由它写进那个应用的编辑上下文。见 `crate::tsf`。
    /// 它和 `Pebrel` 是同一个形状(**整体替换 + 一次提交**),只是传输层不同。
    Tsf,
}

impl KeyMode {
    /// 走窗口消息(不管流不流式)
    pub fn is_wmchar(self) -> bool {
        matches!(self, KeyMode::WmChar(_) | KeyMode::WmCharFinal(_))
    }
    /// 只允许定稿时整句投递
    pub fn is_final_only(self) -> bool {
        matches!(self, KeyMode::WmCharFinal(_))
    }
    /// 走组合态(不产生按键):Pebrel 控制面,或 TSF 薄壳。
    pub fn is_compose(self) -> bool {
        matches!(self, KeyMode::Pebrel | KeyMode::Tsf)
    }
}

/// 日志用:当前方式打在哪个窗口上。
pub fn describe_target(mode: KeyMode) -> String {
    match mode {
        KeyMode::Keys => "键盘注入".to_string(),
        KeyMode::WmChar(h) => format!("消息通道(流式){}", target::describe(h)),
        KeyMode::WmCharFinal(h) => format!("消息通道(整句一次){}", target::describe(h)),
        KeyMode::Pebrel => "Pebrel 控制面(组合态缓冲层)".to_string(),
        KeyMode::Tsf => "TSF 组合态(写进应用的编辑上下文)".to_string(),
    }
}

/// 该给这个窗口用哪种上屏方式。纯函数,好测。
///
/// `policy` 是配置里的 `ime_wmchar`,写法:
///   `off` / `keys`            —— 永远走键盘注入(老行为)
///   `on`  / `wmchar`          —— 永远走 WM_CHAR(拿不到窗口句柄时退回键盘)
///   `on:pebrel.exe,zed.exe`   —— **只对这几个程序**走消息通道(流式)
///   `off:some.exe`            —— 这几个程序永远走键盘注入
///   `nostream[:<exe>,...]`    —— 走消息通道但**整句一次投递**(连消息退格都不认的程序用)
///   `pebrel[:<exe>,...]`      —— 走 Pebrel 控制面的**组合态缓冲层**(一个按键都不发)。
///                               端点在本机只有一份,所以不写名单时只对 Pebrel 自己的
///                               程序名(pebrel.exe / nebula.exe)生效。
///   `auto`(默认,等于没写)   —— **只要目标窗口所在线程挂着输入法(IME)** 就走 WM_CHAR。
///
/// `auto` 为什么不再看窗口类名(2026-11-01 改,现场见 §上屏方式那段的注释):
/// 字母被输入法吃掉是**静默丢字** —— 用户看到的是"输入法跳到候选面板"、后半句直接没了,
/// 而不是一次可见的报错;比"换通道可能在个别窗口不适用"严重得多。窗口类名白名单
/// (`class_needs_message_channel`)是事后补的坑:每上一个新程序就得再踩一次同样的现场
/// (`bug` 被吃掉),而漏掉的代价是**用户看不见的丢字**。所以改成按线程挂不挂 IME 一刀切,
/// 用逃生门(`off:<exe>` / `keys`)给确实不适用的程序留后路,而不是靠"猜哪些窗口要换"。
///
/// **名单是「覆盖」而不是「开关」**:名单形式只改名单里那几个程序怎么走,
/// 名单外的窗口一律按 `auto` 判 —— 把兜底一起关掉是最隐形的坑
/// (2026-10-30:配置写成 `ime_wmchar = pebrel` 之后,微信里的英文又被输入法吃掉,
/// 根因就是这个「名单外走键盘」把 auto 兜底顺手关了)。
///
/// `exe` 是目标进程的可执行文件名(小写,含 `.exe`),用来匹配名单。
pub fn decide_key_mode(
    policy: &str,
    hwnd: isize,
    thread_has_ime: bool,
    exe: &str,
    // 焦点窗口所在进程里**我们的 TSF 薄壳是不是活的**(端点能不能等到)。
    // 这一条本身就是"该不该走 TSF"的判据:薄壳只在 TSF 把我们激活时才有端点。
    tsf_available: bool,
) -> KeyMode {
    let exe = exe.to_ascii_lowercase();
    let (head, list) = match policy.split_once(':') {
        Some((h, l)) => (h.trim(), l.trim()),
        None => (policy.trim(), ""),
    };
    let listed = |list: &str| -> bool {
        list.split(',')
            .map(|x| x.trim().to_ascii_lowercase())
            .any(|x| !x.is_empty() && x == exe)
    };
    let only_listed = !list.is_empty();
    let wmchar = |yes: bool| {
        if yes && hwnd != 0 {
            KeyMode::WmChar(hwnd)
        } else {
            KeyMode::Keys
        }
    };
    let wmchar_final = |yes: bool| {
        if yes && hwnd != 0 {
            KeyMode::WmCharFinal(hwnd)
        } else {
            KeyMode::Keys
        }
    };
    // `auto` 的判据:**只看目标线程挂不挂输入法**。名单外的窗口按它判。
    // 窗口类名白名单(`class_needs_message_channel`)已**不再是判据** —— 见上面 `auto`
    // 那条 doc:白名单漏一个程序就是一次静默丢字,而"换通道在个别窗口不适用"要轻得多。
    let auto_mode = || wmchar(thread_has_ime);
    match head {
        "nostream" | "final" => {
            if !only_listed || listed(list) {
                wmchar_final(true)
            } else {
                auto_mode()
            }
        }
        "pebrel" | "compose" => {
            // 组合态那条路走的是 Pebrel 的控制面(发现文件只有一份),所以「不写名单」
            // = 只对 Pebrel 自己的程序名生效,而不是「对所有窗口生效」。
            // 名单外**不换控制面**(静默把别的程序的文本塞进它的缓冲层是最难查的错误),
            // 但也**不再一律走键盘** —— 按 auto 判,否则就是「配了 pebrel 之后
            // 微信里的英文又被输入法吃掉」。
            const PEBREL_EXES: [&str; 2] = ["pebrel.exe", "nebula.exe"];
            let want = if only_listed { listed(list) } else { PEBREL_EXES.contains(&exe.as_str()) };
            if want {
                KeyMode::Pebrel
            } else {
                auto_mode()
            }
        }
        "off" | "keys" | "input" => {
            // `off:foo.exe`:名单里的程序永远走键盘;名单外的按 auto,免得把兜底关掉
            if only_listed && !listed(list) {
                auto_mode()
            } else {
                KeyMode::Keys
            }
        }
        "tsf" => {
            // 名单外**不换**(和 pebrel 同一个道理:静默把别的程序的文本塞进它的编辑上下文
            // 是最难查的错误)。不写名单时靠 `tsf_available` 自己收窄 ——
            // 只有"我们真的被那个进程激活了"才会走这条。
            let want = if only_listed { listed(list) } else { true };
            if want && tsf_available {
                KeyMode::Tsf
            } else {
                auto_mode()
            }
        }
        "on" | "wmchar" => {
            if !only_listed || listed(list) {
                wmchar(true)
            } else {
                auto_mode()
            }
        }
        _ => {
            // auto(以及任何写错的值):输入法 + 那类文本面才换;
            // 带名单时**只有名单里的程序**吃这条规则(写 `auto:foo.exe` 就是在收紧它)
            let allow = !only_listed || listed(list);
            if allow {
                auto_mode()
            } else {
                KeyMode::Keys
            }
        }
    }
}

/// 窗口类名 → 「输入法会吃掉注入字母、要走 WM_CHAR」的那类文本面。纯函数,好测。
///
/// ⚠️ **仅供探针/诊断参考,已不再是 `auto` 的判据**(2026-11-01 起 `auto` 只看
/// 目标线程挂不挂输入法 —— 理由见 `decide_key_mode` 的 `auto` 那条 doc)。
/// 保留它是因为"哪些类在真机上确实吃过字母"仍是有用的现场记录,探针/日志要按它分类。
///
/// 记过的两类(都是**在真机上实测过**字母被吃掉 + WM_CHAR 能落字的):
///   - `*edit*`:经典文本控件(Edit / RichEdit / RichEdit20W...);
///   - `qt*`    :Qt 窗口(Qt5/Qt6 的 `QWindow`,微信 4.x 就是 `Qt51514QWindowIcon`)。
///
/// 想对某个具体程序强开/强关,用配置里的名单形式(如 `ime_wmchar = off:foo.exe`)。
pub fn class_needs_message_channel(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    c.contains("edit") || c.starts_with("qt")
}

#[cfg(windows)]
mod target {
    use super::KeyMode;

    #[repr(C)]
    struct GuiThreadInfo {
        cb_size: u32,
        flags: u32,
        hwnd_active: isize,
        hwnd_focus: isize,
        hwnd_capture: isize,
        hwnd_menu_owner: isize,
        hwnd_move_size: isize,
        hwnd_caret: isize,
        rc_caret: [i32; 4],
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn GetGUIThreadInfo(id_thread: u32, pgui: *mut GuiThreadInfo) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    }

    #[link(name = "imm32")]
    extern "system" {
        fn ImmIsIME(hkl: isize) -> i32;
    }

    /// 目标窗口所在线程挂的是不是输入法(IME)。
    fn thread_uses_ime(hwnd: isize) -> bool {
        if hwnd == 0 {
            return false;
        }
        let tid = unsafe { GetWindowThreadProcessId(hwnd, std::ptr::null_mut()) };
        if tid == 0 {
            return false;
        }
        let hkl = unsafe { GetKeyboardLayout(tid as u32) };
        hkl != 0 && unsafe { ImmIsIME(hkl) } != 0
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetKeyboardLayout(id_thread: u32) -> isize;
    }

    /// 目标窗口所属进程的可执行文件名(小写,含 `.exe`);拿不到就空串。
    fn exe_name(hwnd: isize) -> String {
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn QueryFullProcessImageNameW(h: isize, flags: u32, buf: *mut u16, size: *mut u32) -> i32;
            fn CloseHandle(h: isize) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        if hwnd == 0 {
            return String::new();
        }
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == 0 {
            return String::new();
        }
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h == 0 {
            return String::new();
        }
        let mut buf = [0u16; 512];
        let mut size = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
        unsafe { CloseHandle(h) };
        if ok == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    }

    /// 前台窗口所属进程 pid(拿不到就 0)。
    pub fn focused_pid() -> u32 {
        let hwnd = focused_hwnd();
        if hwnd == 0 {
            return 0;
        }
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        pid
    }

    /// 焦点控件(拿不到就退回前台窗口)。
    pub fn focused_hwnd() -> isize {
        // 【回归/诊断专用】`SAINPUT_DIAG_TARGET_HWND=0x…` 直接指定目标窗口。
        //
        // 为什么需要:私有桌面(自动化测试用的那种)上**谁都抢不到前台** ——
        // `SetForegroundWindow` 返回 0,于是 `GetForegroundWindow()` 也是 0,
        // 上屏方式按"没有目标"退化成 SendInput,而 SendInput 在非交互桌面上
        // 事件全被丢(实测 0/6)。要给"worker + 上屏算术"整条链路做无人值守回归,
        // 只能把目标窗口显式给出来。
        if let Some(h) = std::env::var("SAINPUT_DIAG_TARGET_HWND").ok().and_then(|v| {
            let v = v.trim().to_string();
            match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                Some(hex) => isize::from_str_radix(hex, 16).ok(),
                None => v.parse::<isize>().ok(),
            }
        }) {
            if h != 0 {
                return h;
            }
        }
        let mut info: GuiThreadInfo = unsafe { std::mem::zeroed() };
        info.cb_size = std::mem::size_of::<GuiThreadInfo>() as u32;
        if unsafe { GetGUIThreadInfo(0, &mut info) } != 0 && info.hwnd_focus != 0 {
            return info.hwnd_focus;
        }
        if info.hwnd_active != 0 {
            return info.hwnd_active;
        }
        unsafe { GetForegroundWindow() }
    }

    pub fn key_mode(policy: &str) -> KeyMode {
        let hwnd = focused_hwnd();
        // 只有策略里真写了 tsf 才去探端点 —— 别的策略下每句多一次系统调用是白花。
        let wants_tsf = policy.split(':').next().unwrap_or("").trim() == "tsf";
        let tsf_available =
            wants_tsf && crate::tsf::available(crate::tsf::pid_of_window(hwnd));
        super::decide_key_mode(
            policy,
            hwnd,
            thread_uses_ime(hwnd),
            &exe_name(hwnd),
            tsf_available,
        )
    }

    /// 只给日志用:一眼看出为什么走了 WM_CHAR。
    pub fn describe(hwnd: isize) -> String {
        if hwnd == 0 {
            return "无焦点窗口".into();
        }
        let mut buf = [0u16; 128];
        let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        let cls = if n > 0 { String::from_utf16_lossy(&buf[..n as usize]) } else { "?".into() };
        format!("{cls} 上有输入法,注入的字母会被它吃掉")
    }
}

#[cfg(not(windows))]
mod target {
    use super::KeyMode;
    pub fn focused_hwnd() -> isize {
        0
    }

    pub fn focused_pid() -> u32 {
        0
    }
    pub fn key_mode(_policy: &str) -> KeyMode {
        KeyMode::Keys
    }
    pub fn pid_of_window(_hwnd: isize) -> u32 {
        0
    }
    pub fn describe(_hwnd: isize) -> String {
        String::new()
    }
}

pub use target::key_mode as target_key_mode;

/// 前台窗口所属进程 pid(拿不到就 0)。
pub fn focused_pid() -> u32 {
    target::focused_pid()
}

/// 焦点窗口(拿不到就 0)。给探针/日志用 —— 上屏方式就是按它决定的。
pub fn focused_window() -> isize {
    target::focused_hwnd()
}

/// 按当前方式打一段文本。
///
/// WM_CHAR 模式下**整段都走消息**:不能一半走消息、一半走键盘。
/// 这两条通道在系统里的处理时机不一样(SendInput 要过输入子系统,PostMessage 直接进
/// 消息队列),混着发**先后顺序不保证** —— 真机上表现为字母插到汉字中间
/// (`说到TOKEN` 变成 `说TO到 说TOKE到N`)。所以选定一条路就一条路走到底。
fn type_text_with(mode: KeyMode, s: &str) -> Result<()> {
    match mode {
        KeyMode::Keys => type_text(s),
        // 消息通道:整段都走消息(两条通道抢顺序会把字母插到汉字中间,所以不混)
        KeyMode::WmChar(h) | KeyMode::WmCharFinal(h) => post_chars(h, s),
        // 组合态不发按键:文本走 `pane.compose`。走到这里说明模式/传输层接线错了,
        // 宁可明确报错,也不要静默退化成键盘注入(那正是这条模式要避开的失败)。
        KeyMode::Pebrel => anyhow::bail!("Pebrel 组合态不走按键通道"),
        KeyMode::Tsf => anyhow::bail!("TSF 组合态不走按键通道"),
    }
}

/// 按当前方式退格 n 次。
///
/// **消息通道下退格走哪条消息要看控件类** —— 这是 2026-10-31 在私有桌面靶子上量出来的
/// (探针 `tools/probe_backspace_flavor.py`),两个"edit 类"的答案正好相反:
///
/// | 控件类 | 投 `WM_KEYDOWN/UP VK_BACK` | 投 `WM_CHAR(0x08)` |
/// |---|---|---|
/// | 经典 `Edit` | **无效**(关掉应用的 `TranslateMessage` 后一次都不生效) | **有效** |
/// | `RICHEDIT50W` | **有效**(自己处理,不依赖 `TranslateMessage`) | 无效 |
/// | Qt(`qt*`) | 有效(工具链真机探针 `probe_wmchar_backspace.py`) | —— |
///
/// 经典控件那条尤其阴:开着 `TranslateMessage`(所有正常程序的默认)时看着"能用",
/// 但真正删除的是**应用自己把这条 KEYDOWN 翻译出来的 `WM_CHAR(0x08)`**,
/// 而那条消息会被排到"我们随后投出的补打字符"**之后** —— 屏幕上删掉的是句尾,
/// 不是该删的那几个字。`--no-translate` 下才看得见真相(见 `PLAN-NEXT.md` T4.5)。
fn backspace_with(mode: KeyMode, n: usize) -> Result<()> {
    match mode {
        KeyMode::Keys => backspace(n),
        KeyMode::WmChar(h) | KeyMode::WmCharFinal(h) => post_backspace(h, n),
        KeyMode::Pebrel => anyhow::bail!("Pebrel 组合态不做退格:整句是整体替换的"),
        KeyMode::Tsf => anyhow::bail!("TSF 组合态不做退格:整句是整体替换的"),
    }
}

/// 这个控件类该用 `WM_CHAR(0x08)` 退格吗?纯函数,判据见 `backspace_with` 的表。
///
/// 判据**故意写窄**:只有量过的类才改行为,没量过的一律保持"投 KEYDOWN/UP"
/// (与 2026-10-30 之前完全一致)—— 换错风味是**可见的丢字/乱改**,
/// 而"没量过的类维持现状"最坏也只是维持现状。
pub fn class_backspace_as_char(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    // RichEdit 一族自己处理按键消息(`richedit` 里也含 `edit`,必须先挡掉)。
    if c.contains("richedit") {
        return false;
    }
    c.contains("edit")
}

/// 目标窗口的类名(拿不到就返回空串)。
#[cfg(windows)]
fn class_name_of(hwnd: isize) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

#[cfg(windows)]
fn post_backspace(hwnd: isize, n: usize) -> Result<()> {
    const WM_CHAR: u32 = 0x0102;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const VK_BACK: usize = 0x08;
    if hwnd == 0 {
        return Ok(());
    }
    // lParam:重复次数 | 扫描码(0x0E=Backspace)<<16 | 抬起标志
    let scan: isize = 0x0E;
    let down = 1 | (scan << 16);
    let up = 1 | (scan << 16) | (1 << 30) | (1 << 31);
    let as_char = class_backspace_as_char(&class_name_of(hwnd));
    for _ in 0..n {
        if as_char {
            if unsafe { PostMessageW(hwnd, WM_CHAR, VK_BACK, 0) } == 0 {
                anyhow::bail!("PostMessage(WM_CHAR 0x08) 失败:窗口句柄 {hwnd:#x}");
            }
            continue;
        }
        if unsafe { PostMessageW(hwnd, WM_KEYDOWN, VK_BACK, down) } == 0 {
            anyhow::bail!("PostMessage(WM_KEYDOWN VK_BACK) 失败:窗口句柄 {hwnd:#x}");
        }
        if unsafe { PostMessageW(hwnd, WM_KEYUP, VK_BACK, up) } == 0 {
            anyhow::bail!("PostMessage(WM_KEYUP VK_BACK) 失败:窗口句柄 {hwnd:#x}");
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn post_backspace(_hwnd: isize, _n: usize) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn post_chars(hwnd: isize, s: &str) -> Result<()> {
    const WM_CHAR: u32 = 0x0102;
    if hwnd == 0 {
        return Ok(());
    }
    for unit in s.encode_utf16() {
        let ok = unsafe { PostMessageW(hwnd, WM_CHAR, unit as usize, 0) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            let alive = unsafe { IsWindow(hwnd) } != 0;
            anyhow::bail!(
                "PostMessage(WM_CHAR) 失败:窗口句柄 {hwnd:#x} win32_err={err} 句柄有效={alive} 字符={:?}",
                char::from_u32(unit as u32)
            );
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn post_chars(_hwnd: isize, _s: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn GetClassNameW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    /// 报错时带上 win32 错误码 —— 只给一个句柄等于没给现场(2026-10-31 加)。
    fn IsWindow(hwnd: isize) -> i32;
}

#[cfg(not(windows))]
unsafe fn IsWindow(_h: isize) -> i32 { 0 }

#[cfg(not(windows))]
unsafe fn GetLastError() -> u32 { 0 }

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetLastError() -> u32;
}

/// 消息通道里「退格之后要等多久才补打下一个字符」。
///
/// 真机实测(微信 4.x / Qt 5.15,挂水杉输入法;口径见 `tools/probe_wmchar_backspace.py`):
/// 退格之后**同一批**投出的字符 5/5 次全丢(屏幕上少一个字符 —— 用户报的
/// 「我的验证码是903721」变成「我的验证码是03721」就是这一下);
/// 中间隔 10ms 以上 5/5 全对。
///
/// 取 15ms 而不是最初的 40ms(体检报告 §5.2,2026-10-31):
/// - 10ms 是「5/5 全对」的实测点,而且 0ms 是必丢、10ms 起全对,阈值附近没有
///   「偶发丢字」的中间样本,所以 10ms 本身就不贴边;15ms 是在它基础上再留 50%
///   (1.5×)——「退格 + 补打」每句只有约 5% 的帧会走到,再往下压收益递减,
///   一旦压进阈值就是**可见的丢字**,所以余量不能省。
/// - 原来的 40ms 是 4× 余量,而这条路径每次白等 25~30ms(改口帧里唯一的固定开销),
///   属于体检报告点名的「性能可压项」里最便宜的一个。
#[cfg(windows)]
const WMCHAR_BACKSPACE_SETTLE: std::time::Duration = std::time::Duration::from_millis(15);
#[cfg(not(windows))]
const WMCHAR_BACKSPACE_SETTLE: std::time::Duration = std::time::Duration::from_millis(0);

/// 整句是不是一个阿拉伯数字串(读验证码 / 号码那种)。
///
/// 判据只看 ASCII 数字:**汉字数字已经在 ITN 那一步转成阿拉伯数字了**
/// (见 `tools/itn.py` 的 `whole_digits`),所以这里不需要再认「一二三」。
/// 空串不算 —— 空结果表示这一句还没听出东西。
///
/// 为什么要这条规则:用户的原话是「我想直接读验证码能贴上去」——
/// 前面多一个空格,数字框里就贴不进去(FROZEN-v0.2.5 之后那轮,2026-10-30)。
fn is_bare_number(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}


#[cfg(test)]
mod tests {
    /// 退格风味表(`backspace_with` 的那张表就是这几条量出来的结论)。
    #[test]
    fn backspace_flavor_by_class() {
        use super::class_backspace_as_char;
        // 经典控件:只认 WM_CHAR(0x08)
        assert!(class_backspace_as_char("Edit"));
        assert!(class_backspace_as_char("EDIT"));
        // RichEdit 一族:自己处理按键,且它的名字里也含 edit —— 必须先挡掉
        assert!(!class_backspace_as_char("RICHEDIT50W"));
        assert!(!class_backspace_as_char("RichEdit20W"));
        // Qt / 终端 / 浏览器那类:维持投 KEYDOWN/UP(各自真机量过或用的是别的通道)
        assert!(!class_backspace_as_char("Qt51514QWindowIcon"));
        assert!(!class_backspace_as_char("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(!class_backspace_as_char("Chrome_WidgetWin_1"));
        assert!(!class_backspace_as_char(""));
    }

    use super::*;

    /// 前缀（前置空格）必须进入和正文同一条算术。
    /// 这里不真的敲键盘（type_text 会 SendInput），只检查「该显示什么」的推导。
    #[test]
    fn want_includes_prefix() {
        let mut w = StreamWriter::new(8, true);
        assert_eq!(w.want("你好", false), "你好");
        w.set_prefix(" ");
        assert_eq!(w.want("你好", false), " 你好");
        w.set_prefix("");
        assert_eq!(w.want("你好", false), "你好");
    }

    /// 句尾空格(托盘「后面自动加空格」):定稿才补,而且**只补一个**。
    #[test]
    fn want_has_single_trailing_space() {
        let mut w = StreamWriter::new(8, true);
        // 关着的时候一个字都不动(默认状态)
        assert_eq!(w.want("你好", true), "你好");
        w.set_trailing_space(true);
        // 流式中间帧不补 —— 补了每一帧都要「退空格→补新字→再补空格」
        assert_eq!(w.want("你好", false), "你好");
        // 定稿才补,而且是**一个**
        assert_eq!(w.want("你好", true), "你好 ");
        // 已经是空格结尾(标点转空格那条路的尾巴)就不再补
        assert_eq!(w.want("你好 ", true), "你好 ", "不许出现两个空格");
        // 和前置空格共存:两边各一个
        w.set_prefix(" ");
        assert_eq!(w.want("你好", true), " 你好 ");
        // 纯数字句两边都不补 —— 和前置空格同一个理由(要能贴进数字框)
        assert_eq!(w.want("903721", true), "903721");
        // 空结果仍然是空:不能被补成一个空格(那会被当成一次上屏)
        let mut w2 = StreamWriter::new(8, true);
        w2.set_trailing_space(true);
        assert_eq!(w2.want("", true), "");
    }

    /// 句尾空格只在**定稿**落下去:流式阶段一个退格都不许多出来
    /// (多出来的退格在 WM_CHAR 那条路上还要白等一次 `WMCHAR_BACKSPACE_SETTLE`)。
    #[test]
    fn trailing_space_only_on_final() {
        let mut w = StreamWriter::new(8, true);
        w.set_trailing_space(true);
        w.set_mode(KeyMode::WmChar(0)); // 句柄 0:不真投递,只走记账
        w.update("你好").unwrap();
        assert_eq!(w.injected, "你好", "流式帧不带句尾空格");
        w.update("你好啊").unwrap();
        assert_eq!(w.injected, "你好啊");
        assert_eq!(w.backspaces, 0, "流式阶段不该出现退格");
        w.finalize("你好啊", 32).unwrap();
        assert_eq!(w.injected, "你好啊 ", "定稿补一个空格");
        assert_eq!(w.backspaces, 0, "定稿只是往后补一个空格,不退格");
        // 同样文本再定稿一次:不许又补一个
        w.finalize("你好啊", 32).unwrap();
        assert_eq!(w.injected, "你好啊 ");
    }

    /// reset 要同时清掉前缀 —— 前缀是"这一句"的决策,换句要重新判断。
    #[test]
    fn reset_clears_prefix() {
        let mut w = StreamWriter::new(8, true);
        w.set_prefix(" ");
        w.reset().unwrap();
        assert_eq!(w.want("下一句", false), "下一句");
    }

    /// 前缀变化不能影响"公共前缀"的正确性:
    /// 从" 你好"到" 你好啊" → 公共前缀 3 个字(空格+你好),只需补"啊"。
    #[test]
    fn prefix_participates_in_common_prefix() {
        let mut w = StreamWriter::new(8, true);
        w.set_prefix(" ");
        let a = w.want("你好", false);
        let b = w.want("你好啊", false);
        assert_eq!(common_prefix_chars(&a, &b), 3);
        assert_eq!(a.chars().count() - common_prefix_chars(&a, &b), 0); // 不用退格
    }

    /// 读验证码那种「整句一个数字串」:不补前置空格,流式第一帧也不补。
    ///
    /// 用户要的是「读出来能直接贴进数字框」—— 前面多一个空格就贴不进去。
    #[test]
    fn bare_number_gets_no_leading_space() {
        let mut w = StreamWriter::new(8, true);
        w.set_prefix(" ");
        assert_eq!(w.want("12345", false), "12345", "纯数字句不补前置空格");
        assert_eq!(w.want("12345个", false), " 12345个", "不是纯数字句就照旧补空格");
        assert_eq!(w.want("abc", false), " abc");

        // 流式:第一帧只有一位数字时也不许先补空格再退格(那会在屏幕上闪一下)
        let mut w = StreamWriter::new(8, true);
        w.set_prefix(" ");
        w.set_mode(KeyMode::WmChar(0)); // 句柄 0:消息函数直接返回,只走记账
        w.update("1").unwrap();
        assert_eq!(w.injected, "1", "第一帧就不许带前置空格");
        w.update("12345").unwrap();
        assert_eq!(w.injected, "12345");
        assert_eq!(w.backspaces, 0, "整句跑完不该出现退格");
        w.finalize("12345", 32).unwrap();
        assert_eq!(w.injected, "12345");
    }

    /// 🔴 2026-11-02 用户报的「长句卡住不动、松开时一下子把后面卡住的文字全吐出来」。
    ///
    /// 现场:标点模型在句中插了一个空格(「补充一下我发的那些指令现在是白的」→
    /// 「补充一下 我发的那些指令现在是白的」),公共前缀只有 4 字、要退 12 字 > `max_backspace`(默认 8)。
    /// 老代码在这里 `return`,而 `injected` 保持旧版 —— 后面每一条 partial 的公共前缀
    /// 都还停在那个空格的位置,改动距离只增不减,**整句冻到 finalize**。
    ///
    /// 真机 186 句语料里 21 句中招(最长冻 14.7 秒),所以这条必须是「真的重打」。
    #[test]
    fn big_revision_resyncs_instead_of_freezing_the_sentence() {
        let mut w = StreamWriter::new(8, true);
        w.set_mode(KeyMode::WmChar(0)); // 句柄 0:不真投递,只走记账
        w.update("补充一下我发的那些指令现在是白的").unwrap();
        assert_eq!(w.backspaces, 0, "第一版是纯追加");
        // 公共前缀 4 字,要退 12 字 > max_backspace(8)
        w.update("补充一下 我发的那些指令现在是白的").unwrap();
        assert_eq!(
            w.injected, "补充一下 我发的那些指令现在是白的",
            "改动距离超过 max_backspace 也必须真的重打 —— 冻住这一句就是用户报的 bug"
        );
        assert_eq!(w.backspaces, 12);
        // 之后照常追加:屏幕已经同步,不用再退格
        w.update("补充一下 我发的那些指令现在是白的啊").unwrap();
        assert_eq!(w.injected, "补充一下 我发的那些指令现在是白的啊");
        assert_eq!(w.backspaces, 12, "追加不该再退格");
        // 定稿也只是一次纯追加
        w.finalize("补充一下 我发的那些指令现在是白的啊", 32).unwrap();
        assert_eq!(w.injected, "补充一下 我发的那些指令现在是白的啊");
        assert_eq!(w.backspaces, 12);
    }

    /// 大改动要**限频**(模型每拍回头改一次 = 每拍重打整句),但限频期一过必须补上 ——
    /// 「限频」不是「丢弃」,这是老代码冻住整句的根源。
    #[test]
    fn big_revision_is_rate_limited_but_never_dropped() {
        let mut w = StreamWriter::new(8, true);
        w.set_mode(KeyMode::WmChar(0));
        w.update("一二三四五六七八九十").unwrap();
        w.update("零一二三四五六七八九十").unwrap(); // bs=10 > 8 → 第一笔立刻重打
        assert_eq!(w.injected, "零一二三四五六七八九十");
        let bs0 = w.backspaces;
        // 紧接着又一次大改:这一拍先不动屏幕(限频)
        w.update("〇一二三四五六七八九十").unwrap();
        assert_eq!(w.injected, "零一二三四五六七八九十", "限频期内先不动");
        assert_eq!(w.backspaces, bs0, "限频期内不许真退格");
        std::thread::sleep(BIG_RESYNC_MIN_INTERVAL + std::time::Duration::from_millis(20));
        w.update("〇一二三四五六七八九十").unwrap();
        assert_eq!(w.injected, "〇一二三四五六七八九十", "限频期过了必须真的补上");
        assert!(w.backspaces > bs0);
    }

    /// 换句要清掉限频钟:下一句的第一笔大改动必须立刻生效,不许被上一句的节流挡住。
    #[test]
    fn reset_clears_the_resync_clock() {
        let mut w = StreamWriter::new(8, true);
        w.set_mode(KeyMode::WmChar(0));
        w.update("一二三四五六七八九十").unwrap();
        w.update("零一二三四五六七八九十").unwrap(); // 记下重打时刻
        w.reset().unwrap();
        w.update("一二三四五六七八九十").unwrap();
        w.update("〇一二三四五六七八九十").unwrap();
        assert_eq!(w.injected, "〇一二三四五六七八九十", "新一句的大改动不许被上一句节流");
    }

    /// 上屏方式的选择逻辑(纯函数,不需要真窗口)。
    ///
    /// 这里钉的是**只在该换的时候换**:`auto` 现在**只看目标线程挂不挂输入法** ——
    /// 挂着就一律走 WM_CHAR(2026-11-01 起;白名单漏一个程序就是一次静默丢字)。
    ///
    /// 【调用侧契约,本文件不实现】通道**每句开始时只选一次、句中途不换** ——
    /// 混发 SendInput / PostMessage 两条通道先后顺序不保证(真机上字母会插进汉字中间)。
    /// "不换"由调用方保证(见 `install_injection_mode`),本函数只对某一时刻给一个答案。
    #[test]
    fn key_mode_policy() {
        const E: &str = "pebrel.exe";
        const OTHER: &str = "notepad.exe";
        // off:永远是键盘注入
        assert_eq!(decide_key_mode("off", 0x1234, true, E, false), KeyMode::Keys);
        // tsf:只有"薄壳在那个进程里活着"才换 —— 没端点就按 auto 判,不误伤别的窗口
        assert_eq!(decide_key_mode("tsf", 0x1234, true, E, true), KeyMode::Tsf);
        assert_eq!(decide_key_mode("tsf", 0x1234, true, E, false), KeyMode::WmChar(0x1234));
        // 连输入法都没有的窗口也照样能用 TSF(它根本不走键盘/消息)
        assert_eq!(decide_key_mode("tsf", 0x1234, false, E, true), KeyMode::Tsf);
        // 名单形式:名单外按 auto 判
        assert_eq!(decide_key_mode("tsf:pebrel.exe", 0x1234, true, OTHER, true), KeyMode::WmChar(0x1234));
        assert_eq!(decide_key_mode("tsf:pebrel.exe", 0x1234, true, E, true), KeyMode::Tsf);
        // on:强制 WM_CHAR
        assert_eq!(decide_key_mode("on", 0x1234, false, E, false), KeyMode::WmChar(0x1234));
        // on 但拿不到窗口句柄 → 退回键盘(有句柄才谈得上投递消息)
        assert_eq!(decide_key_mode("on", 0, true, E, false), KeyMode::Keys);
        // auto:只要目标线程挂着输入法就换 —— **与窗口类名(needs_channel)无关**
        // (2026-11-01 前这里要求 needs_channel=true;那正是"新程序再踩一次"的坑)
        assert_eq!(decide_key_mode("auto", 0x1234, true, E, false), KeyMode::WmChar(0x1234));
        assert_eq!(
            decide_key_mode("auto", 0x1234, true, E, false),
            KeyMode::WmChar(0x1234),
            "类名不在白名单里也一样换 —— 漏一个程序就是一次静默丢字"
        );
        // 没输入法就照旧键盘注入(不管类名真假)
        assert_eq!(decide_key_mode("auto", 0x1234, false, E, false), KeyMode::Keys);
        assert_eq!(decide_key_mode("auto", 0x1234, false, E, false), KeyMode::Keys);
        // 没窗口句柄 → 退回键盘
        assert_eq!(decide_key_mode("auto", 0, true, E, false), KeyMode::Keys);
        // 没写这个键(或写错)按 auto 走
        assert_eq!(decide_key_mode("", 0x1234, true, E, false), KeyMode::WmChar(0x1234));
        assert_eq!(decide_key_mode("whatever", 0x1234, true, E, false), KeyMode::WmChar(0x1234));
        // 名单形式:只对该程序生效(自绘终端就靠它试,不牵动别的窗口)
        assert_eq!(decide_key_mode("on:pebrel.exe", 0x1234, false, E, false), KeyMode::WmChar(0x1234));
        assert_eq!(decide_key_mode("on: Pebrel.EXE , zed.exe", 0x1234, false, E, false), KeyMode::WmChar(0x1234));
        assert_eq!(decide_key_mode("on:pebrel.exe", 0x1234, true, OTHER, false), KeyMode::WmChar(0x1234));
        // off 带名单:名单里的永远键盘,名单外仍享受 auto 兜底
        assert_eq!(decide_key_mode("off:pebrel.exe", 0x1234, true, E, false), KeyMode::Keys);
        assert_eq!(decide_key_mode("off:pebrel.exe", 0x1234, true, OTHER, false), KeyMode::WmChar(0x1234));
        // auto 带名单:只有名单里的程序吃 auto 规则(写名单 = 收紧这条规则)
        assert_eq!(decide_key_mode("auto:pebrel.exe", 0x1234, true, OTHER, false), KeyMode::Keys);
        assert_eq!(decide_key_mode("auto:pebrel.exe", 0x1234, true, E, false), KeyMode::WmChar(0x1234));
        // nostream:名单里走「整句一次投递」,名单外按 auto(不是一律键盘)
        assert_eq!(decide_key_mode("nostream:pebrel.exe", 0x1234, false, E, false), KeyMode::WmCharFinal(0x1234));
        assert_eq!(decide_key_mode("nostream:pebrel.exe", 0x1234, true, OTHER, false), KeyMode::WmChar(0x1234));
        assert_eq!(
            decide_key_mode("nostream:pebrel.exe", 0x1234, true, OTHER, false),
            KeyMode::WmChar(0x1234),
            "名单外按 auto:挂着输入法就换,不再要求类名在白名单里"
        );
        assert_eq!(decide_key_mode("nostream:pebrel.exe", 0x1234, false, OTHER, false), KeyMode::Keys);
        assert_eq!(decide_key_mode("nostream", 0x1234, false, E, false), KeyMode::WmCharFinal(0x1234));
        // pebrel:组合态缓冲层。**不写名单 = 只对 Pebrel 自己的程序名生效** ——
        // 控制面在这台机上只有一份,把别的程序的文本塞进它的缓冲层是最难查的错误。
        assert_eq!(decide_key_mode("pebrel", 0x1234, false, E, false), KeyMode::Pebrel);
        assert_eq!(
            decide_key_mode("pebrel", 0x1234, false, "nebula.exe", false),
            KeyMode::Pebrel,
            "旧程序名 nebula.exe 也认"
        );
        assert_eq!(
            decide_key_mode("pebrel:psql.exe", 0x1234, false, "psql.exe", false),
            KeyMode::Pebrel
        );
        // ⚠️ 回归(2026-10-30 微信「英文全被输入法吃掉」的现场):配了 `pebrel` 之后,
        // 名单外的窗口**必须继续按 auto 判** —— 用户日常配置就是 `ime_wmchar = pebrel`,
        // 一旦这里退回「一律键盘」,微信里说英文就又没了。
        assert_eq!(
            decide_key_mode("pebrel", 0x1234, true, OTHER, false),
            KeyMode::WmChar(0x1234),
            "配了 pebrel 也要保住名单外窗口的 auto 兜底"
        );
        assert_eq!(decide_key_mode("pebrel", 0x1234, false, OTHER, false), KeyMode::Keys, "没输入法就不用换");
        assert_eq!(
            decide_key_mode("pebrel:psql.exe", 0x1234, true, OTHER, false),
            KeyMode::WmChar(0x1234),
            "名单外也不许静默换控制面,但要保住 auto 兜底"
        );
    }

    /// 哪些窗口类在真机上确实吃过注入的字母(**诊断参考**,已不是 `auto` 的判据)。
    #[test]
    fn class_needs_message_channel_covers_edit_and_qt() {
        // 经典控件(2026-10-28 实测的那一类)
        assert!(class_needs_message_channel("Edit"));
        assert!(class_needs_message_channel("RichEdit20W"));
        assert!(class_needs_message_channel("EDIT"));
        // Qt 窗口(2026-10-30 微信实测的那一类)
        assert!(class_needs_message_channel("Qt51514QWindowIcon"));
        assert!(class_needs_message_channel("Qt6QWindowIcon"));
        // 别的自绘窗口:不动它(没实测过,保留原来的注入方式)
        assert!(!class_needs_message_channel("Zed::Window"));
        assert!(!class_needs_message_channel("ConsoleWindowClass"));
        assert!(!class_needs_message_channel("Chrome_WidgetWin_1"));
        assert!(!class_needs_message_channel(""));
    }

    /// 消息通道默认**照常流式**(退格走 PostMessage(WM_KEYDOWN/UP, VK_BACK))。
    /// 用 `WmChar(0)` 做这个测试:句柄 0 时消息函数直接返回,不会真的发消息。
    #[test]
    fn wmchar_mode_streams_on_the_message_channel() {
        let mut w = StreamWriter::new(8, true);
        w.set_mode(KeyMode::WmChar(0));
        w.update("我说到 TOKE").unwrap();
        assert_eq!(w.injected, "我说到 TOKE", "消息通道要照常逐帧上屏");
        let typed = w.typed_chars;
        w.update("我说到 TOKEN 的时候").unwrap();
        assert_eq!(w.injected, "我说到 TOKEN 的时候");
        assert!(w.typed_chars > typed, "新增部分要接着打出去");
        assert_eq!(w.backspaces, 0, "只往后加,不用退格");
        // 回退(ASR 改口)时才退格:这里退 1 个字再补一个字
        w.update("我说到 TOKEN 的时").unwrap();
        assert_eq!(w.backspaces, 1, "改口要退格一次");
        assert_eq!(w.injected, "我说到 TOKEN 的时");
    }

    /// `nostream:` 策略:一个字都不流式,定稿时整句一次投递(连消息退格都不认的程序用)。
    #[test]
    fn nostream_mode_types_only_the_whole_final() {
        let mut w = StreamWriter::new(8, true);
        w.set_mode(KeyMode::WmCharFinal(0));
        w.update("我").unwrap();
        w.update("我说到 TOKEN 的时候").unwrap();
        assert_eq!(w.injected, "", "整句一次模式不许流式上屏");
        w.finalize("我说到 TOKEN 的时候", 32).unwrap();
        assert_eq!(w.injected, "我说到 TOKEN 的时候", "定稿时整句一次投递");
        assert_eq!(w.typed_chars, "我说到 TOKEN 的时候".chars().count());
        assert_eq!(w.backspaces, 0, "整句投递不需要退格");
        let typed = w.typed_chars;
        w.finalize("我说到 TOKEN 的时候", 32).unwrap();
        assert_eq!(w.typed_chars, typed, "重复 final 不该重复打一遍");
    }

    /// `--no-stream`(`StreamWriter::new(.., false)`):流式阶段一个字都不打,
    /// **松手定稿时整句一次投递**。
    ///
    /// 这里用 `WmChar(0)`(句柄 0)当「不真投递」的替身:消息函数拿到 0 直接返回,
    /// 记账照走 —— 所以测试不会真的敲键盘。改前这一支是 `injected = want; return`,
    /// 一个字都不会上屏(即 `--no-stream` 是坏的),下面这几条断言就是钉这件事。
    #[test]
    fn no_stream_option_types_the_whole_sentence_on_final() {
        let mut w = StreamWriter::new(8, false);
        w.set_mode(KeyMode::WmChar(0));
        w.update("我说到 TOKE").unwrap();
        assert_eq!(w.injected, "", "`--no-stream` 的流式阶段不许上屏");
        w.finalize("我说到 TOKEN 的时候", 32).unwrap();
        assert_eq!(w.injected, "我说到 TOKEN 的时候", "定稿时整句一次投递");
        assert_eq!(w.typed_chars, "我说到 TOKEN 的时候".chars().count());
        assert_eq!(w.backspaces, 0, "整句投递不需要退格");
        let typed = w.typed_chars;
        // 同一句再来一次 final(比如 final 之后紧跟 finish):不许重复打一遍
        w.finalize("我说到 TOKEN 的时候", 32).unwrap();
        assert_eq!(w.typed_chars, typed, "重复 final 不该重复打一遍");
    }

    /// 安全阀放弃修正时,**记账必须留在屏幕上那一版**。
    ///
    /// 不许把 `injected` 推到屏幕前面 —— 一旦对不上,后面每次退格都是照
    /// 「屏幕上有这些字」算的,算出来的退格数会删掉用户真正的文字
    /// (2026-11-01 实测:「说英文的那一句整段不见了」)。
    ///
    /// 现在只剩**定稿**这一条放弃路径:流式那条改成了限频重打
    /// (见 `big_revision_resyncs_instead_of_freezing_the_sentence`),
    /// 它要么真的重打了(记账跟着走),要么这一拍不动(记账不动),不会出现
    /// 「记账说有、屏幕上没有」。
    #[test]
    fn giveup_keeps_bookkeeping_on_screen() {
        let mut w = StreamWriter::new(8, true);
        // 屏幕上已经有一句(测试里直接摆好状态,不真的敲键盘)。
        // 必须比 final 的 32 字上限长,否则就会走进「真的退格重打」那条路。
        w.injected =
            "已经上屏的这一句 hello world 后面还有一大截没说完还在继续说下去".to_string();
        assert!(w.injected.chars().count() > 32);
        let screen = w.injected.clone();

        // final 超限(> 32):整笔放弃,记账不动
        w.finalize("收口", 32).unwrap();
        assert_eq!(w.injected, screen, "final 放弃后 injected 必须还是屏幕上那一版");
        assert_eq!(w.backspaces, 0, "放弃就是一笔都不动");
    }

    /// 组合态模式的替身:只记录被要求设置/提交了什么,不碰任何窗口。
    #[derive(Clone, Default)]
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>>);

    impl Recorder {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().filter(|(_, commit)| !commit).map(|(t, _)| t.clone()).collect()
        }
        fn commits(&self) -> Vec<String> {
            self.0.lock().unwrap().iter().filter(|(_, commit)| *commit).map(|(t, _)| t.clone()).collect()
        }
    }

    impl crate::pebrel::Composer for Recorder {
        fn set(&mut self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push((text.to_string(), false));
            Ok(())
        }
        fn commit(&mut self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push((text.to_string(), true));
            Ok(())
        }
    }

    /// 组合态:**整体替换**缓冲层,不产生一个按键、不产生一次退格。
    /// (真机语料里约 5% 的帧会回头改前面的词;那些帧在按键模式下就是退格补打。)
    #[test]
    fn compose_mode_replaces_the_whole_buffer_and_never_backspaces() {
        let mut w = StreamWriter::new(8, true);
        let rec = Recorder::default();
        w.set_mode(KeyMode::Pebrel);
        w.set_composer(Some(Box::new(rec.clone())));
        w.update("我说到 TOKE").unwrap();
        w.update("我说到 TOKEN 的时候").unwrap();
        w.update("我说到 TOKEN 的时").unwrap(); // ← 改口那一帧
        assert_eq!(w.injected, "", "没提交就等于没上屏");
        assert_eq!(w.backspaces, 0, "组合态不做退格");
        assert_eq!(w.typed_chars, 0, "组合态不按键,所以不打字");
        assert_eq!(
            rec.calls(),
            vec!["我说到 TOKE", "我说到 TOKEN 的时候", "我说到 TOKEN 的时"],
            "每一帧都是整体替换"
        );
        assert!(rec.commits().is_empty(), "没定稿不许提交");
        w.update("我说到 TOKEN 的时").unwrap();
        assert_eq!(rec.calls().len(), 3, "同一段文本不重复发");
    }

    /// 定稿只提交一次,提交之后记账才等于屏幕。
    #[test]
    fn compose_mode_commits_once_on_finalize() {
        let mut w = StreamWriter::new(8, true);
        let rec = Recorder::default();
        w.set_mode(KeyMode::Pebrel);
        w.set_composer(Some(Box::new(rec.clone())));
        w.set_prefix(" ");
        w.update("我说到 TOKE").unwrap();
        w.finalize("我说到 TOKEN 的时候", 32).unwrap();
        assert_eq!(rec.commits(), vec![" 我说到 TOKEN 的时候"], "提交要带上前置空格");
        assert_eq!(w.injected, " 我说到 TOKEN 的时候");
        assert_eq!(w.typed_chars, " 我说到 TOKEN 的时候".chars().count());
        w.finalize("我说到 TOKEN 的时候", 32).unwrap();
        assert_eq!(rec.commits().len(), 1, "重复 final 不许重复提交(等于说了一遍进去两遍)");
    }

    /// 流水线中途结束(识别报错 / 空定稿 / 侧车挂了)时:**把已经显示出来的那一版
    /// 提交掉**,不许丢(2026-11-02 用户明确要求)。按键模式什么都不做。
    #[test]
    fn commit_shown_keeps_what_the_user_already_saw() {
        let mut w = StreamWriter::new(8, true);
        let rec = Recorder::default();
        w.set_mode(KeyMode::Pebrel);
        w.set_composer(Some(Box::new(rec.clone())));
        w.update("还没说完就被打断的一句").unwrap();
        w.commit_shown().unwrap();
        assert_eq!(rec.commits(), vec!["还没说完就被打断的一句"], "已显示的组合串要提交,不许丢");
        assert_eq!(w.injected, "还没说完就被打断的一句");
        assert_eq!(w.typed_chars, "还没说完就被打断的一句".chars().count());
        // 提交过之后:迟到的 partial / 定稿都不许再改屏幕(组合态没有差额补打)
        w.update("迟到的帧").unwrap();
        assert_eq!(rec.calls(), vec!["还没说完就被打断的一句"], "迟到帧不许再开组合串");
        w.finalize("迟到的定稿", 32).unwrap();
        assert_eq!(rec.commits().len(), 1, "迟到的定稿不许再提交一遍");
        // 按键模式:这条路径什么都不做(字是真按键打的,本来就在屏幕上)
        w.set_mode(KeyMode::Keys);
        w.injected = "屏幕上已经有的一句".into();
        w.commit_shown().unwrap();
        assert_eq!(w.injected, "屏幕上已经有的一句", "按键模式不许动记账");
        assert_eq!(rec.calls().len(), 1);
    }

    /// 换句时组合串还挂着 = 上一句没有正常定稿(定稿迟到 / 卡住 / 报错):
    /// **提交它**,不许把用户已经看见的字丢掉;提交锁不能带到下一句。
    #[test]
    fn compose_mode_commits_the_buffer_on_reset() {
        let mut w = StreamWriter::new(8, true);
        let rec = Recorder::default();
        w.set_mode(KeyMode::Pebrel);
        w.set_composer(Some(Box::new(rec.clone())));
        w.update("还没说完的一句").unwrap();
        w.reset().unwrap();
        assert_eq!(rec.calls(), vec!["还没说完的一句"], "换句不再发『清空组合串』");
        assert_eq!(rec.commits(), vec!["还没说完的一句"], "挂着的这一版要提交进应用");
        assert_eq!(w.injected, "", "换句后记账归零(新一句从干净的账开始)");
        w.reset().unwrap();
        assert_eq!(rec.calls().len(), 1, "没有挂着的组合串时不多发一条");
        assert_eq!(rec.commits().len(), 1);
    }

    /// 收尾上限到点(组合态):提交当前这一版并锁上这一句 —— 迟到的真定稿不许再提交
    /// 一遍(整段一次写入,再提交就是两遍);换句后锁要清掉,下一句照常提交。
    #[test]
    fn compose_cap_commits_once_and_locks_the_sentence() {
        let mut w = StreamWriter::new(8, true);
        let rec = Recorder::default();
        w.set_mode(KeyMode::Pebrel);
        w.set_composer(Some(Box::new(rec.clone())));
        w.update("说到一半的一句").unwrap();
        w.finalize_capped("说到一半的一句", 32).unwrap();
        assert_eq!(rec.commits(), vec!["说到一半的一句"], "上限到点先把已显示的这一版交出去");
        w.update("迟到的帧").unwrap();
        assert_eq!(rec.calls().len(), 1, "提交后不许再开组合串");
        w.finalize("说到一半的一句(修正版)", 32).unwrap();
        assert_eq!(rec.commits().len(), 1, "迟到的真定稿不许再提交");
        // 换句:锁清掉,下一句照常
        w.reset().unwrap();
        w.update("下一句").unwrap();
        w.finalize("下一句", 32).unwrap();
        assert_eq!(rec.commits(), vec!["说到一半的一句", "下一句"]);
    }

    /// 按键模式的收尾上限**不锁**:晚到的真定稿还要靠差额补打把最后几个字补上。
    #[test]
    fn key_mode_cap_still_allows_a_late_final() {
        let mut w = StreamWriter::new(8, true);
        w.set_mode(KeyMode::WmChar(0));
        w.update("说到一半").unwrap();
        w.finalize_capped("说到一半", 32).unwrap();
        assert_eq!(w.injected, "说到一半");
        w.finalize("说到一半的一句", 32).unwrap();
        assert_eq!(w.injected, "说到一半的一句", "按键模式晚到的定稿必须能补尾巴");
        assert_eq!(w.backspaces, 0);
    }

}

/// 两次「大改动重打」之间的最小间隔（改动距离超过 `max_backspace` 时）。
///
/// 为什么是 250ms：流式模型本身 160ms 才吐一拍，用户分不出 250ms 和 160ms 的差别；
/// 而模型要是每拍都在回头改，这条线把重打压到最多 4 次/秒 —— 既不会把目标窗口刷爆，
/// 也**绝不会**像老代码那样把整句冻到松手（见 `big_resync_at` 的注释）。
const BIG_RESYNC_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// 流式改写器：把「当前应显示的文本」增量地落到目标窗口。
///
/// 策略：算公共前缀 → 退格掉差异尾巴 → 补打新增部分。
/// `max_backspace` 是安全阀：改动超过这个字数时**降频重打**（最快
/// `BIG_RESYNC_MIN_INTERVAL` 一次），而不是永久放弃这一帧 —— 见 `big_resync_at`。
pub struct StreamWriter {
    injected: String,
    max_backspace: usize,
    enabled: bool,
    /// 这一句要不要在前面补一个空格（用户要的"光标前有字就插空格"）。
    ///
    /// 为什么放在 writer 里而不是在 typing 前拼一下：
    /// 流式改写靠「公共前缀 → 退格差异 → 补打新增」这套算术，**屏幕上的内容必须和
    /// `injected` 完全一致**。如果只是打的时候多敲一个空格、而 `injected` 里没有，
    /// 下一帧算前缀就会错位，退格数算错 → 删到用户自己的字。
    /// 所以空格作为 `prefix` 进入同一条算术：`injected` 始终等于 prefix + 逻辑文本。
    prefix: String,
    /// 这一句要不要在**句尾**补一个空格(托盘「后面自动加空格」)。
    ///
    /// 和 `prefix` 同一个理由:空格必须进同一条算术
    /// (`injected` = prefix + 正文 + 句尾空格),只多敲一个空格而不记账,
    /// 下一帧算公共前缀就会错位、退格数算错。
    /// 只补**一个** —— 已经是空格结尾(标点转空格那条路的尾巴)就不再补。
    trailing_space: bool,
    /// 这一句用哪种方式上屏(每句开始时按目标窗口决定,见 `decide_key_mode`)。
    /// 默认是键盘注入;目标线程挂着输入法时会切成 WM_CHAR(见 `auto` 那条 doc)。
    mode: KeyMode,
    /// 组合态模式(`KeyMode::Pebrel`)的传输层:设置/提交应用自己的缓冲层。
    /// 只在这个模式下才需要,别的模式是 `None`。
    composer: Option<Box<dyn crate::pebrel::Composer>>,
    /// 当前**已经放进组合串**的文本 —— 屏幕上还没提交的那一版。
    /// 组合态下 `injected` 始终是空的:没提交就是没上屏。
    composed: String,
    pub typed_chars: usize,
    pub backspaces: usize,
    /// 上一次「改动距离超过 `max_backspace` 的大重打」发生在什么时候。
    ///
    /// 🔴 这道阀门原来是**永久放弃**：一帧改动超过 `max_backspace` 就 `return`，
    /// 而 `injected` 保持旧值。于是后面每一条 partial 的公共前缀都还停在那个改动点，
    /// 改动距离只增不减，**整句从此不再更新**，直到松手 finalize 才一次补上。
    /// 用户原话（2026-11-02）：「说的稍微长一点就卡住不动，松开时一下子把后面卡住的
    /// 文字全吐出来」；真机 186 句语料里 21 句中招，最长冻了 14.7 秒。
    /// 诱因几乎都是标点模型在句中插一个空格（「补充一下我发的」→「补充一下 我发的」），
    /// 改动距离 9~29 字，而默认阈值只有 8。
    ///
    /// 现在改成限频：同样的改动离上次大重打超过 `BIG_RESYNC_MIN_INTERVAL` 就真的退格
    /// 重打（十几毫秒的事），屏幕永远跟得上；模型要是每拍都在来回改，这条线保证
    /// 不会每拍都重打整句。
    big_resync_at: Option<std::time::Instant>,
    /// 这一句已经**提交过**组合串(收尾上限到点,或收尾兜底 `commit_shown`)。
    ///
    /// 组合态是「整段一次写入」,没有差额补打 —— 提交过一次之后再提交一次,应用里就是
    /// 同一句出现两遍(2026-11-01 实测)。所以提交之后:迟到的 partial 不再改组合串、
    /// 迟到的真定稿也不再提交(它的字用户已经看见了);`reset()` 换句时清掉。
    compose_committed: bool,
}

impl StreamWriter {
    pub fn new(max_backspace: usize, enabled: bool) -> Self {
        Self {
            injected: String::new(),
            max_backspace,
            enabled,
            prefix: String::new(),
            trailing_space: false,
            mode: KeyMode::Keys,
            composer: None,
            composed: String::new(),
            typed_chars: 0,
            backspaces: 0,
            big_resync_at: None,
            compose_committed: false,
        }
    }

    /// 设定这一句的前缀(`" "` 或 `""`)。**每句开始时**由调用方按光标上下文决定一次。
    pub fn set_prefix(&mut self, p: &str) {
        self.prefix = p.to_string();
    }

    /// 设定这一句的**句尾**要不要补一个空格。和 `set_prefix` 一样,每句开始时定一次。
    pub fn set_trailing_space(&mut self, on: bool) {
        self.trailing_space = on;
    }

    /// 设定这一句的上屏方式。**每句开始时**决定一次:目标窗口可能已经换了。
    ///
    /// 【调用侧契约】**一句里只设一次、句中途绝不换** —— 两条通道(SendInput / PostMessage)
    /// 在系统里处理时机不同,混发先后顺序不保证(真机上表现为字母插进汉字中间)。
    /// 本函数不做任何拦截,"不换"由调用方(每句开始处 `install_injection_mode`)保证;
    /// 见 `type_text_with` 的说明。
    pub fn set_mode(&mut self, m: KeyMode) {
        self.mode = m;
    }

    pub fn mode(&self) -> KeyMode {
        self.mode
    }

    /// 装上/卸下组合态传输层。**每句开始时**由调用方按模式决定:
    /// 组合态需要一个真客户端(每句重解析目标窗口与活动 pane)。
    pub fn set_composer(&mut self, composer: Option<Box<dyn crate::pebrel::Composer>>) {
        self.composer = composer;
    }

    /// 收回已经上屏的 `n` 个字符(T5.2「收回上一句」)。
    ///
    /// 走 `backspace_with` —— **按本句当初用的那条通道退格**(Keys 走 SendInput、
    /// 消息通道走 `WM_KEYDOWN/UP VK_BACK`)。直接用 `inject::backspace` 是错的:
    /// 它只会 SendInput,在那些"必须走消息通道"的窗口里(微信 4.x、Qt、认字符不认消息的控件)
    /// 一个都退不掉 —— 那正是当初要开第二条通道的原因。
    ///
    /// ⚠️ 调用方**必须先确认焦点还在那一句的窗口里**:退格打在**当前焦点**上,
    /// 打错窗口就是删掉别人的内容。
    pub fn undo(&mut self, n: usize) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        backspace_with(self.mode, n)?;
        self.backspaces = self.backspaces.saturating_add(n);
        Ok(())
    }

    /// 把**已经显示出来的这一版**提交掉 —— 组合态的收尾兜底。
    ///
    /// 用在所有「这一句没有正常定稿」的收尾路径上:识别报错、空定稿、侧车挂了、
    /// 焦点守卫中止、换句时还挂着组合串。用户已经看见的字不许凭空消失
    /// (2026-11-02 用户明确要求:不要完全丢弃的方案)。
    ///
    /// 按键模式什么都不做:那些字是**真按键**打进去的,本来就在屏幕上。
    pub fn commit_shown(&mut self) -> Result<()> {
        if !self.mode.is_compose() || self.composed.is_empty() {
            return Ok(());
        }
        self.commit_composed()?;
        // 组合态没有差额补打:这一句已经落进应用,迟到的 partial / 定稿不许再改一遍
        self.compose_committed = true;
        Ok(())
    }

    /// 提交组合串(不碰提交锁)。`reset` 换句时用它 —— 那时锁属于上一句,不该带到下一句。
    fn commit_composed(&mut self) -> Result<()> {
        if self.composed.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(&mut self.composed);
        let Some(composer) = self.composer.as_mut() else {
            // 传输层已经被换掉(组合态失败后已退档到按键通道):这一版已经由那条路打出去了,
            // 没有组合串可提交,别把文本留在状态里。
            return Ok(());
        };
        match composer.commit(&text) {
            Ok(()) => {
                self.typed_chars += text.chars().count();
                self.injected = text;
                Ok(())
            }
            Err(e) => {
                // 提交失败:文本放回去,下一次收尾(reset)还能再试一次 —— 丢掉就真没了
                self.composed = text;
                Err(e)
            }
        }
    }

    /// 收尾硬上限到点时的定稿。
    ///
    /// 按键模式和 `finalize` 完全一样(晚到的真定稿还能差额补打尾巴);
    /// 组合态则**提交这一版并把这一句锁上** —— 它是整段一次写入,再提交一次就是两遍
    /// (2026-11-01 实测)。锁由 `reset` 在换句时清掉。
    pub fn finalize_capped(&mut self, target: &str, max_bs: usize) -> Result<()> {
        let r = self.finalize(target, max_bs);
        if r.is_ok() && self.mode.is_compose() && self.composed.is_empty() {
            self.compose_committed = true;
        }
        r
    }

    pub fn reset(&mut self) -> Result<()> {
        // 换句时组合串还挂着 = 上一句没有正常定稿(定稿迟到 / 卡住 / 报错)。
        // **提交它**,不许把用户已经看见的字丢掉。
        let flushed = self.commit_composed();
        self.injected.clear();
        // 前缀属于"这一句"的决策,换句要重新决定(光标位置可能已经变了)
        self.prefix.clear();
        self.trailing_space = false;
        // 限频钟也是"这一句"的状态:下一句的第一笔大改动必须立刻生效
        self.big_resync_at = None;
        // 提交锁只属于上一句
        self.compose_committed = false;
        flushed
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn injected(&self) -> &str {
        &self.injected
    }

    /// 这一句该显示成什么(含前置空格;`final_tail` 时还含句尾空格)。
    ///
    /// 例外:**整句就是一个数字串**时两边都不补空格(见 `is_bare_number`)。
    ///
    /// 为什么句尾空格只在**定稿**时补:它只是句与句之间的分隔,流式中间帧不需要;
    /// 而带上它,每一帧都得「退掉空格 → 补打新字 → 再补空格」走一遍 ——
    /// 白多一次退格,`WM_CHAR` 那条路上还要白等一次退格沉淀(见 `WMCHAR_BACKSPACE_SETTLE`)。
    fn want(&self, target: &str, final_tail: bool) -> String {
        let mut s = if self.prefix.is_empty() || is_bare_number(target) {
            target.to_string()
        } else {
            format!("{}{}", self.prefix, target)
        };
        // 句尾**只补一个**空格:标点转空格那条路本来就会把结尾标点变成空格,
        // 已经是空格结尾就不再补(用户口径:「只允许加一个就可以了」)。
        // 纯数字句不补 —— 和前置空格同一个理由:读出来的号码要能直接贴进数字框。
        if final_tail
            && self.trailing_space
            && !s.is_empty()
            && !s.ends_with(' ')
            && !is_bare_number(target)
        {
            s.push(' ');
        }
        s
    }

    /// 用 partial 结果更新屏幕。失败不致命,交给下一帧。
    ///
    /// ⚠️ WM_CHAR 模式**不流式**:一个字都不打,等定稿(`finalize`)整句一次投递。
    /// 为什么:这条路靠 PostMessage,而「消息里的退格」(WM_CHAR(0x08))各家程序语义
    /// 不一样 —— 真机上退格被忽略、插入照常,屏幕就成了同一句反复叠加
    /// (`说到TOKEN` 变成 `说TO到 说TOKE到N`)。整句一次投递既不需要退格,也不存在
    /// 「消息通道 vs 键盘通道抢先后」的问题。代价:这类窗口里没有逐字上屏。
    pub fn update(&mut self, target: &str) -> Result<()> {
        if self.mode.is_compose() {
            if self.compose_committed {
                // 这一句已经按收尾上限/兜底提交过:迟到帧不许再开一条组合串,
                // 否则屏幕上会多出一段没人收的 preedit,下一句 reset 一提交就是两遍
                return Ok(());
            }
            // 组合态:**整体替换**缓冲层,没有退格算术 —— 因为一个字节都还没上屏。
            // 这正是「边说边改」在终端里能做到的形态:应用自己那层 preedit 可以随便
            // 改写,屏幕(和 shell 历史)不会经过任何中间态。
            return self.compose_set(target);
        }
        if self.mode.is_final_only() {
            return Ok(());
        }
        if !self.enabled || target.is_empty() {
            // 空结果一律当成「这次没听清」,绝不把已经上屏的字擦掉。
            // final 走 finalize(),那条路径才允许改写/清空。
            // `!enabled`(`--no-stream`)同样在这里早退:流式阶段本就不该打字,
            // 整句留给 finalize() 一次投递(见那里的注释)。
            return Ok(());
        }
        let want = self.want(target, false);
        if want == self.injected {
            return Ok(());
        }
        let common = common_prefix_chars(&self.injected, &want);
        let bs = self.injected.chars().count().saturating_sub(common);
        if bs > self.max_backspace {
            // 大改动:限频重打,**绝不永久放弃** —— 见 `big_resync_at` 的注释。
            // 老写法是直接 `return`:injected 停在旧版,后面每一条 partial 的改动距离
            // 都还从那个点算起、只增不减,整句就冻到松手才一次吐出来。
            let now = std::time::Instant::now();
            let due = self
                .big_resync_at
                .is_none_or(|t| now.duration_since(t) >= BIG_RESYNC_MIN_INTERVAL);
            if !due {
                return Ok(()); // 刚重打过:这一拍先不动屏幕,下一拍再来(不会冻住整句)
            }
            self.big_resync_at = Some(now);
        }
        let tail: String = want.chars().skip(common).collect();
        self.backspace_then_type(bs, &tail)?;
        self.injected = want;
        Ok(())
    }

    /// 「退格 n 次 → 补打 tail」这两步,update / finalize 共用。
    ///
    /// 消息通道(`WM_CHAR`)下**两步之间必须停一下**:退格走的是
    /// `PostMessage(WM_KEYDOWN/UP, VK_BACK)`,紧接着投出去的字符会被目标程序
    /// 当成本次按键产生的字符丢掉 —— 微信 4.x(Qt 5.15)真机实测:
    /// 同一批消息里 `VK_BACK` 后面紧跟的那个字符 5/5 次全丢(读回少一个字符),
    /// 中间隔 10ms 以上 5/5 全对。所以这里等 `WMCHAR_BACKSPACE_SETTLE`。
    /// 键盘注入那条路不需要:SendInput 的事件本来就按真实的按键节奏进系统。
    fn backspace_then_type(&mut self, bs: usize, tail: &str) -> Result<()> {
        if bs > 0 {
            backspace_with(self.mode, bs)?;
            self.backspaces += bs;
            if self.mode.is_wmchar() && !tail.is_empty() {
                std::thread::sleep(WMCHAR_BACKSPACE_SETTLE);
            }
        }
        if !tail.is_empty() {
            type_text_with(self.mode, tail)?;
            self.typed_chars += tail.chars().count();
        }
        Ok(())
    }

    /// 组合态:把这一句(含前缀)放进应用的缓冲层。同样的文本不重复发。
    fn compose_set(&mut self, target: &str) -> Result<()> {
        if !self.enabled || target.is_empty() {
            // 空结果 = 这次没听清:不要把已经放进缓冲层的那一版擦掉,
            // 定稿或换句时才收尾。
            // `!enabled`(`--no-stream`)时流式阶段连缓冲层都不碰:整句在
            // finalize() 里一次提交(见那里的注释)。
            return Ok(());
        }
        let want = self.want(target, false);
        if want == self.composed {
            return Ok(());
        }
        let Some(composer) = self.composer.as_mut() else {
            anyhow::bail!("组合态模式没有装传输层(内部接线错误)");
        };
        composer.set(&want)?;
        self.composed = want;
        Ok(())
    }


    /// final 结果：允许更大的回退（final 通常只是把尾巴改得更准）。
    ///
    /// 走和 `update` **完全相同**的前缀算术（含 `prefix`），否则屏幕与 `injected` 会错位。
    pub fn finalize(&mut self, target: &str, max_bs: usize) -> Result<()> {
        if self.mode.is_compose() && self.compose_committed {
            // 这一句已经按收尾上限/兜底提交过:再提交一次 = 应用里同一句出现两遍
            return Ok(());
        }
        let want = self.want(target, true);
        if !self.enabled {
            // `--no-stream`:不流式,**松手定稿时整句一次性投递**(体检报告 §7-7)。
            //
            // 这一支原来只有 `self.injected = want; return` —— 一个字都不打;而
            // `update()` 在 `!enabled` 时同样早退,于是「不要逐字重打」这个开关被
            // 实现成了「一个字都不会上屏」,等于死代码(main.rs 的帮助文本当时也
            // 只好写着「当前不可用」)。
            //
            // 现在的语义就是下面 `is_final_only()`(`WmCharFinal`)那一支,只是由
            // **用户开关**触发而不是由目标窗口决定:整句(含前置空格)一次投递,
            // 没有退格,也就没有「记账 vs 屏幕」的偏差;代价是字等到松手才出现。
            //
            // `enabled` 的另一个用途(离线演练靠 `false` 抑制注入)不受影响:
            // `--dry-run` 那条路在主循环里就绕开了 `finalize`(main.rs 的 `handle_msg`
            // 在 dry 时只打印文本),根本走不到这里。
            if want == self.injected {
                return Ok(());
            }
            if self.mode.is_compose() {
                // 组合态本来就是「整句一次提交」,而且一个按键都不许发
                // (`type_text_with` 在 Pebrel 下是明确报错的)。
                let Some(composer) = self.composer.as_mut() else {
                    anyhow::bail!("组合态模式没有装传输层(内部接线错误)");
                };
                composer.commit(&want)?;
                self.composed.clear();
            } else {
                type_text_with(self.mode, &want)?;
            }
            self.typed_chars += want.chars().count();
            self.injected = want;
            return Ok(());
        }
        if self.mode.is_compose() {
            // 提交:清掉缓冲层,把整段作为**一次**写入交给应用(与输入法提交同一条路)。
            // `max_bs` 在这里没有意义 —— 缓冲层是整体替换的,不存在退格。
            if want == self.injected {
                return Ok(());
            }
            let Some(composer) = self.composer.as_mut() else {
                anyhow::bail!("组合态模式没有装传输层(内部接线错误)");
            };
            composer.commit(&want)?;
            self.typed_chars += want.chars().count();
            self.injected = want;
            self.composed.clear();
            return Ok(());
        }
        if self.mode.is_final_only() {
            // 不流式:整句(含前置空格)一次投递,没有退格,也就没有「记账 vs 屏幕」的偏差。
            if want == self.injected {
                return Ok(());
            }
            type_text_with(self.mode, &want)?;
            self.typed_chars += want.chars().count();
            self.injected = want;
            return Ok(());
        }
        if want == self.injected {
            return Ok(());
        }
        let common = common_prefix_chars(&self.injected, &want);
        let bs = self.injected.chars().count().saturating_sub(common);
        if bs > max_bs {
            // 超限:整笔放弃,**屏幕和记账都不动**。
            //
            // 这里原来会 `self.injected = want` —— 于是记账跑到了屏幕前面
            // (记账说屏幕上是新文本,屏幕上其实还是旧文本)。之后每一次退格都是
            // 照着「屏幕上有这些字」算的,算出来的退格数就会删掉用户真正的文字;
            // 真机上表现为「说英文的那一句整段不见了」。
            // 宁可这一笔不生效(屏幕留着上一版假设,反正下一句会 reset),
            // 也不能让记账和屏幕对不上。
            return Ok(());
        }
        let tail: String = want.chars().skip(common).collect();
        self.backspace_then_type(bs, &tail)?;
        self.injected = want;
        Ok(())
    }
}