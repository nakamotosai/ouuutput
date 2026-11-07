#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""跨句:上一句的定稿**迟到**,而下一句**已经出字** —— 两句话都得完好,屏幕还不许回退。

用户报的现场(2026-11-01):上一句的定稿还在路上(离线整段重解那版能晚到约 2 秒),
用户已经开口说下一句了。屏幕上的表现是「一句话被贴上两遍」,或者「后半句直接没了」,
还有人看到的是「**回退再重打**」(屏幕先跳回上一句,再把新句补一遍)。

真 bug 在 `crates/ouuutput-cli/src/main.rs` 的 worker:`defer_reset` 只记住
「上一句的定稿还没回来」,**没记住「新句已经出字」**。两条不同的路径都会踩它:

  · 新句的字写进上一句**还没归零的账**里 —— 差额算术把新句的字当成上一句的尾巴改来改去
    (`inject.rs` 的 `finalize` 是 common_prefix + 退格 + 补尾巴,账错了屏幕上就是重字/串字);
  · 上一句的定稿一到,旧代码顺手把 `stage=Idle` / `finalized=true` 按在**新一句**头上,
    于是新句后面的帧被当迟到帧丢掉,连新句**自己的定稿**也一并被丢(后半句没了)。

**第二条路径与定稿的 `reason` 无关**:上一句除了我们收尾时要的那条 `finish` 回执,
侧车自己按静音判出来的 `endpoint`(`tools/asr_sidecar.py` 的 VAD 那条路)也会迟到。
只要它进了 `handle_msg`,就会走 `main.rs:1073` 那次
`finalize(text, max_bs*4)`:`endpoint` 带的是**上一句**的文本,而账这时是**新句**的
—— common_prefix 归零、退格新句打出去的字、再把上一句重打一遍 = 屏幕回退;
紧接着它自己那句 `reason == "endpoint"` 还把账归零(`main.rs:1101`),于是新句的
`finish` 又变成"往干净的账里补打",两句话最后拼在一起 —— 用户看到的就是「回退再重打」。

本用例把这条时间线在**真 worker + 真注入链路**上跑两遍(假侧车按脚本安排时间线,私有桌面,
不需要麦克风、不需要合成按键、用户看不见):
  A 组:上一句迟到的是 `finish`(我们收尾要的那条);
  B 组:上一句迟到的是 `endpoint` **和** `finish`(真侧车就是这么发的:VAD 在前、回执在后)。

时间线(毫秒,从"第一帧 audio 到达"= 第一句按下那一刻起算):
  300/700     第一句两帧 partial → 上屏
  3200/3600   第二句两帧 partial(第二句按下 ≈ 2700,即"上一句定稿未回"的状态)
  4500        上一句的定稿**迟到**(此时第二句已松开、尾巴收完、在等定稿)
  4700        B 组多一条:上一句的 `finish` 回执也迟到
  5000        第二句的 `f:<第二句>:finish`

判据(A/B 是"用户看得见的结果",C/D/E 是"机制上不许发生的事"):
  A 靶子窗口里的文本**恰好**是第一句全文 + 第二句全文,各出现一次;
  B 第二句的定稿**仍然生效** —— 语料 `index.jsonl` 里有它的 finish 行
    (它没被上一句迟到的定稿当成"这一句已经定稿"而丢弃);
  C 引擎日志里有「上一句的定稿未回而新句已出字」与「丢弃上一句迟到的定稿」;
  D 宿主记下的**屏幕时间线只增不减** —— 每一步都是上一步的延长。本用例的脚本只做逐字追加,
    所以屏幕上任何一次变短 = 有人把已经上屏的字擦掉了(用户看到的"回退");
  E 第二句开始出字之后,引擎日志里**再没有**把上一句的文字交给上屏器的那条 `F ...` 记录
    —— 这是判据 D 的确定性版本(D 靠宿主每 20ms 采样一次,极短的回退可能采样不到;
    E 看的是"它有没有被交给 `finalize`",不依赖采样)。

⚠️ 单实例:引擎带 `OUUUTPUT_SINGLE_NS` 跑,只认自己的锁与自己的进程名 ——
**绝不会**去接管(更不会杀掉)用户正在用的那个实例。日志走**独立目录**(`--log-dir`),
不跟用户自己那份混在一起。

已知限制 / 踩过的坑(2026-11-01 实跑记录):
  · 时间线锚在引擎**自己的**诊断热键节奏上(`OUUUTPUT_DIAG_AUTOSTART`:按住 HOLD_MS、间隔 1500ms),
    侧车脚本从"第一帧 audio"起算,所以不依赖绝对墙钟。机器很忙时若 `DEADLINE_S`(16s)内还没等到
    文本,脚本会按超时读现状 —— 这时**可能**报假红:先看打印出来的"引擎/宿主二进制路径"和证据里的
    语料行、日志行,再重跑一次即可。
  · **跑的是哪一份二进制必须看打印**:`target-test/` 下同时存在 `release/`(别的 target 编的)与
    `x86_64-pc-windows-gnu/release/` 时,红/绿对照有可能跑成同一份 —— 第一次做对照就踩到了,
    两次输出一模一样。所以本用例开头会把引擎/宿主的完整路径打出来。
  · 日志/语料目录与宿主日志都**带本进程 pid**(`tmp/engine-cross-<pid>/`):两遍并行跑时,
    各自"开跑先清目录"不会再删掉对方正在写的那份(踩过:判据 C 读到空日志)。
  · 私有桌面上没有前台窗口,目标窗口由 `OUUUTPUT_DIAG_TARGET_HWND` 显式给出,上屏走
    `--ime-wmchar on`(**逐帧流式**那条消息通道,和生产里"窗口线程挂着输入法"时同路)。

跑法:
    cargo build --release --target-dir target-test -p ouuutput-cli -p ouuutput-tsf-host
    python tools/test_cross_sentence.py
"""
import json
import os
import pathlib
import re
import shutil
import sys
import time
import wave

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import hdesk  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
# 与 `tools/test_race_guards.py` 同一份顺序。本仓库的 `.cargo/config.toml` 把默认 target
# 钉在 `x86_64-pc-windows-gnu`(见那里的注释),所以**文档里那条构建命令**
# (`cargo build --release --target-dir target-test ...`)产出的就是 gnu 那一份;
# ⚠️ `target-test/release/` 下可能是别的 target 编出来的东西 —— 跑之前先确认跑的是哪一份
# (下面的打印会给出完整路径)。
DIRS = ["target-test/x86_64-pc-windows-gnu/release", "target-test/release",
        "target/x86_64-pc-windows-gnu/release", "target/release"]
DESKTOP = "SAI-CrossSentence"
NS = "ouuutput-crosstest"
ENV_KEYS = ("OUUUTPUT_SINGLE_NS", "OUUUTPUT_DIAG_AUTOSTART", "FAKE_ASR_SCRIPT",
            "OUUUTPUT_DIAG_TARGET_HWND")

# 两句的正文。第二句的 final 与它的最后一帧 partial **故意不一样**:模型定稿时会把尾巴
# 改得更准,也正好让「屏幕上的字什么时候变成 A 判据要的样子」= 第二句定稿真的落地了。
S1 = "今天天气不错啊"
S1_PARTIALS = ["今天天气不错", "今天天气不错啊"]
S2 = "明天估计要下雨了"
S2_PARTIALS = ["明天估计", "明天估计要下雨"]
EXPECT = S1 + S2

# 诊断自动热键:每句按住 HOLD_MS,松开后间隔 1500ms 再按下(见 main.rs 的
# `OUUUTPUT_DIAG_AUTOSTART`)。所以相对"第一句按下"这一刻:
#   第二句按下 ≈ HOLD_MS + 1500 = 2700ms,第二句松开 ≈ 2*HOLD_MS + 1500 = 3900ms。
HOLD_MS = 1200
LATE_REASONS = ("finish", "endpoint")
DEADLINE_S = 16.0  # 等 A 判据的文本出现(等不到就按超时收工,照样读证据)
LOG_TAIL = 40
HOST_TAIL = 14


def script_for(late_reason):
    """上一句的定稿**迟到**。

    `endpoint` 那一组要**两条都迟到** —— 真侧车就是这么发的:`endpoint` 是它自己按静音
    判出来的(VAD 那条路,在收尾命令**之前**就形成了),`finish` 是答复我们那次收尾的。
    两条一前一后到达,正好把「只挡 finish」那个洞照出来。
    """
    steps = [f"w:300;p:{S1_PARTIALS[0]}", f"w:400;p:{S1_PARTIALS[1]}",
             f"w:2500;p:{S2_PARTIALS[0]}", f"w:400;p:{S2_PARTIALS[1]}"]
    if late_reason == "endpoint":
        steps += [f"w:900;f:{S1}:endpoint", f"w:200;f:{S1}:finish"]
    else:
        steps += [f"w:900;f:{S1}:finish"]
    steps += [f"w:300;f:{S2}:finish"]
    return ";".join(steps)


def find(rel, what):
    for d in DIRS:
        p = REPO / d / rel
        if p.exists():
            return p
    print(f"FAIL: {what} not built")
    sys.exit(2)


def engine_log_dir():
    """引擎的日志开关是 **`--log-dir <目录>`**(不是 `--log <文件>`)—— 传错了它一声不吭地不写。

    独立目录:既拿得到日志(判据 C/E 全靠它),也不跟用户自己那份混在一起。
    目录名**带本进程 pid**:同一台机器上并行跑两遍这个脚本(或前后脚跑)时,各自的
    "开跑前先把上一轮的日志删掉"会互相删掉对方**正在写**的那份 —— 实测踩到:
    判据 C 读到空日志(引擎 stdout 里明明写着这一轮的 `ouuutput-<stamp>.log`)。
    """
    d = REPO / "tmp" / f"engine-cross-{os.getpid()}"
    if d.exists():
        shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True, exist_ok=True)
    return d


def read_engine_log(d):
    """目录里最新的那份会话日志(引擎自己按时间戳命名)。Logger 是直接 writeln 到 File,
    没有用户态缓冲 —— 引擎还在跑的时候就能读到已经写下的行。"""
    files = sorted(d.glob("ouuutput-*.log"))
    if not files:
        return ""
    return files[-1].read_text(encoding="utf-8", errors="replace")


def read_corpus_rows(d):
    """语料 `index.jsonl`:一句 finish 一行(判据 B 的证据)。"""
    p = d / "corpus" / "index.jsonl"
    if not p.exists():
        return []
    rows = []
    for line in p.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rows.append(json.loads(line))
        except Exception:
            rows.append({"text": f"<坏行 {line[:60]!r}>"})
    return rows


def read_timeline(p):
    """宿主记下的屏幕时间线:每 `--dump-every` ms 读一次 EDIT,变了才记一行。

    只取**本进程这一遍**的(宿主日志按 pid 命名,开跑前删掉),所以两条记录之间没有
    别的进程插进来 —— 之前用一个共用文件名时,两遍并行跑会把彼此的行混在一起,
    看起来就像"屏幕回退了"(实测踩到)。
    """
    out = []
    try:
        text = pathlib.Path(p).read_text(encoding="utf-8", errors="replace")
    except Exception:
        return out
    for line in text.splitlines():
        m = re.search(r"text changed -> \"(.*)\"$", line)
        if m:
            out.append(m.group(1))
    return out


def tail_lines(text, n):
    return text.splitlines()[-n:]


def make_wav(path):
    """400ms 静音(16k 单声道 16bit):引擎要真音频才肯走采集那条路,内容无所谓。"""
    if path.exists():
        return path
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(b"\x00\x00" * 6400)
    return path


def run(host, ouuutput, wav, script):
    """起宿主(靶子)→ 起引擎(假侧车脚本)→ 等文本 → 读回全部证据 → 收摊。

    返回 dict(text / log / rows / timeline / host_log)。判据在 check() 里算 —— 这里只取证。
    """
    h = hdesk.open_desktop(DESKTOP)
    saved = {k: os.environ.get(k) for k in ENV_KEYS}
    hh = hp = None
    hpid = epid = 0
    host_log = REPO / "tmp" / f"cross-host-{os.getpid()}.log"
    host_log.unlink(missing_ok=True)
    try:
        # 环境变量由父进程继承 —— CreateProcess 默认就把整份环境传下去
        os.environ["OUUUTPUT_SINGLE_NS"] = NS
        os.environ["OUUUTPUT_DIAG_AUTOSTART"] = str(HOLD_MS)  # 进程内部触发热键,不合成按键
        os.environ["FAKE_ASR_SCRIPT"] = script

        # `--dump-every 20`:宿主每 20ms 采一次 EDIT 内容(默认 250ms)。屏幕上那次
        # "回退再重打"很短,默认采样很可能整段漏过去 —— 判据 D 就废了。
        hh, ht, hpid = hdesk.launch(
            str(host), f"--life 40000 --dump-every 20 --log {host_log}", DESKTOP)
        edit = None
        t0 = time.time()
        while time.time() - t0 < 10:
            edit = hdesk.find_edit(hpid, h)
            if edit:
                break
            time.sleep(0.2)
        if not edit:
            return {"text": "<宿主窗口没起来>", "log": "", "rows": [], "timeline": [],
                    "host_log": _read_text(host_log)}

        # 私有桌面上没有前台窗口,引擎得由我们把目标窗口直接给它
        os.environ["OUUUTPUT_DIAG_TARGET_HWND"] = hex(edit)
        logdir = engine_log_dir()
        hp, hpt, epid = hdesk.launch(
            str(ouuutput),
            "run --script " + str(REPO / "tools" / "fake_asr_sidecar.py")
            + f' --python "{sys.executable}"'
            + f" --feed {wav} --feed-restart"
            # 私有桌面上 **SendInput 根本进不去**(实测 0/6 事件被丢)。目标是个经典 EDIT,
            # 走 WM_CHAR 这条消息通道 —— 和生产里"窗口线程挂着输入法"时走的是同一条路,
            # 而且它是**逐帧流式**上屏的(`KeyMode::WmChar`,不是 `WmCharFinal`):
            # 跨句那张错账只有在流式下才写得出来,这正是要复现的东西。
            + " --ime-wmchar on"
            + " --no-leading-space --no-trailing-space --overlay-mode off"
            + f" --log-dir {logdir}",
            DESKTOP)

        # 等 A 判据要的文本出现(第二句的定稿落地才会出现),等不到就按超时读现状。
        deadline = time.time() + DEADLINE_S
        text = ""
        while time.time() < deadline:
            text = hdesk.read_edit(edit)
            if text == EXPECT:
                break
            time.sleep(0.25)
        # 再给一拍观察窗:定稿之后**还有没有**迟到的帧来改屏幕(判据 A 要求一个字都不许变)。
        time.sleep(0.8)
        text = hdesk.read_edit(edit)

        evidence = {
            "text": text,
            "log": read_engine_log(logdir),
            "rows": read_corpus_rows(logdir),
            "timeline": read_timeline(host_log),
            "host_log": _read_text(host_log),
        }
        hdesk.close(hpt)
        return evidence
    finally:
        if hp:
            hdesk.terminate(epid)
            hdesk.close(hp)
        if hh:
            hdesk.terminate(hpid)
            hdesk.close(ht)
            hdesk.close(hh)
        hdesk.close(h)
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def _read_text(p):
    try:
        return pathlib.Path(p).read_text(encoding="utf-8", errors="replace")
    except Exception:
        return ""


def dump(ev):
    """失败时把证据摊开:读回的文本 + 屏幕时间线 + 引擎日志 + 宿主日志 + 语料行。"""
    out = []
    out.append(f"    读回文本: {ev['text']!r}")
    out.append(f"    期望文本: {EXPECT!r}")
    out.append(f"    第一句 {S1!r} 出现 {ev['text'].count(S1)} 次;"
               f"第二句 {S2!r} 出现 {ev['text'].count(S2)} 次")
    out.append(f"    屏幕时间线({len(ev['timeline'])} 步):")
    for i, s in enumerate(ev["timeline"]):
        out.append(f"      {i}: {s!r}")
    out.append(f"    语料 finish 行({len(ev['rows'])} 条): "
               + ", ".join(repr(r.get("text")) for r in ev["rows"]))
    out.append(f"    --- 引擎日志(尾 {LOG_TAIL} 行)---")
    for l in tail_lines(ev["log"], LOG_TAIL):
        out.append("      " + l)
    out.append(f"    --- 宿主日志(尾 {HOST_TAIL} 行)---")
    for l in tail_lines(ev["host_log"], HOST_TAIL):
        out.append("      " + l)
    return "\n".join(out)


def check(ev):
    """五条判据。返回 (是否全过, [(代号, 是否过, 说明)...])。"""
    got = ev["text"]
    log = ev["log"]

    # A: 文本恰好 = 第一句 + 第二句,各一次。
    a_ok = got == EXPECT
    a_msg = (f"text={got!r} expect={EXPECT!r}"
             + ("" if a_ok else
                f"(第一句×{got.count(S1)},第二句×{got.count(S2)})"))

    # B: 第二句的定稿仍生效 —— 语料里必须有它的 finish 行。
    #    finish 行只在**真定稿走完全程**时才写(被丢弃的那条在 corpus 之前就 continue 了),
    #    所以这一行存在 = 这条定稿没被当成"上一句迟到的账"丢掉。
    hits = [r for r in ev["rows"] if r.get("text") == S2]
    b_ok = len(hits) == 1
    b_msg = (f"语料里第二句的 finish 行 {len(hits)} 条"
             f"(共 {len(ev['rows'])} 条:{[r.get('text') for r in ev['rows']]!r})")

    # C: 引擎日志里那两行痕迹。
    c1 = "上一句的定稿未回而新句已出字" in log
    c2 = "丢弃上一句迟到的定稿" in log
    c_ok = c1 and c2
    c_msg = f"「上一句...新句已出字」={c1} 「丢弃上一句迟到的定稿」={c2}"

    # D: 屏幕时间线只增不减(每一步都是上一步的延长)。
    tl = ev["timeline"]
    bad = [(i, tl[i - 1], tl[i]) for i in range(1, len(tl)) if not tl[i].startswith(tl[i - 1])]
    d_ok = len(tl) >= 3 and not bad and tl[-1] == EXPECT
    d_msg = (f"{len(tl)} 步,回退 {len(bad)} 处"
             + (f",末态={tl[-1]!r}" if tl else ",一条都没记到")
             + ("" if not bad else f";第一次回退: {bad[0][1]!r} → {bad[0][2]!r}"))

    # E: 第二句开始出字之后,上一句的文字再没被交给上屏器。
    #    `F ...ms  (reason)  <text>` 是 handle_msg 收到定稿时**先**写的那行 ——
    #    只要它出现,那条定稿就已经进了差额算术(会退格、会重打)。
    lines = log.splitlines()
    start = next((i for i, l in enumerate(lines) if l.startswith("P ") and S2_PARTIALS[0] in l), None)
    leaked = []
    if start is not None:
        leaked = [l for l in lines[start:] if S1 in l]
    e_ok = start is not None and not leaked
    e_msg = (f"第二句首帧在第 {start} 行" if start is not None else "日志里找不到第二句的首帧")
    if leaked:
        e_msg += f";之后仍有 {len(leaked)} 行带着上一句的文字:{leaked[0][:70]!r}"

    return (a_ok and b_ok and c_ok and d_ok and e_ok), [
        ("A", a_ok, a_msg), ("B", b_ok, b_msg), ("C", c_ok, c_msg),
        ("D", d_ok, d_msg), ("E", e_ok, e_msg)]


def main():
    host = find("ouuutput-tsf-host.exe", "host")
    ouuutput = find("ouuutput.exe", "engine")
    tmp = REPO / "tmp"
    tmp.mkdir(exist_ok=True)
    wav = make_wav(tmp / "cross-silence.wav")

    print("用例:第一句的 finish 迟到、第二句已经出字 —— 两句都要完好,屏幕还不许回退")
    # 跑的是哪一份二进制必须一眼可见:`find()` 按 DIRS 顺序取第一个存在的 ——
    # target-test 下同时有 `release` 和 `x86_64-pc-windows-gnu/release` 时,
    # 取值可能不是你刚编的那份(实测踩过:红/绿对照跑成了同一份二进制)。
    print(f"  引擎 {ouuutput}")
    print(f"  宿主 {host}")
    print(f"  第一句 {S1!r} / 第二句 {S2!r}")

    fails = []
    for reason in LATE_REASONS:
        script = script_for(reason)
        print(f"\n[组 {reason}] 上一句迟到的定稿 reason={reason!r}")
        print(f"  侧车脚本 {script}")
        ev = run(host, ouuutput, wav, script)
        ok, results = check(ev)
        for code, good, msg in results:
            print(f"[{'PASS' if good else 'FAIL'}] {reason}/{code} {msg}")
            if not good:
                fails.append(f"{reason}/{code}")
        if not ok:
            print("  证据:")
            print(dump(ev))

    print("\n" + ("ALL PASS" if not fails else "FAILED: " + ", ".join(fails)))
    return 0 if not fails else 1


if __name__ == "__main__":
    sys.exit(main())
