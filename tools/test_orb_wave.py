# -*- coding: utf-8 -*-
"""悬浮声波（Mode::Orb 方案 04 纯净弦波）实机窗口 + 配色 + 拖动 + 归位 E2E 测试（独立私有桌面）。"""
import ctypes, ctypes.wintypes as wt, os, pathlib, re, shutil, sys, time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import hdesk  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
EXE = next((REPO / d / "ouuutput.exe" for d in [
    "target/x86_64-pc-windows-gnu/release", "target/release"
] if (REPO / d / "ouuutput.exe").exists()), None)
DESKTOP, NS = "SAI-OrbWave", "ouuutput-orbwavetest"
WM_TRAY, WM_RBUTTONUP, WM_LBUTTONUP = 0x0464, 0x0205, 0x0202
WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_LBUTTONDBLCLK = 0x0201, 0x0200, 0x0203
GWL_EXSTYLE, WS_EX_TRANSPARENT = -20, 0x00000020


class RECT(ctypes.Structure):
    _fields_ = [("left", wt.LONG), ("top", wt.LONG), ("right", wt.LONG), ("bottom", wt.LONG)]


class POINT(ctypes.Structure):
    _fields_ = [("x", wt.LONG), ("y", wt.LONG)]


def conf_value(path, key):
    try:
        txt = path.read_text(encoding="utf-8")
    except PermissionError:
        return None
    m = re.search(rf"(?m)^\s*{re.escape(key)}\s*=\s*(.+?)\s*$", txt)
    return m.group(1) if m else None


def find_win(d, pid, cls):
    for h, c, _, vis in hdesk.enum_windows(pid, d):
        if c == cls:
            return h, vis
    return None, False


def win_rect(hwnd):
    r = RECT()
    hdesk.u32.GetWindowRect(hwnd, ctypes.byref(r))
    return r.right - r.left, r.bottom - r.top


def probe_indicator_width(ov_hwnd):
    w, h = win_rect(ov_hwnd)
    WM_NCHITTEST, HTCLIENT = 0x0084, 1
    cx = w // 2
    hit_y = None
    for y in range(max(0, h - 260), h - 10, 6):
        lp = ((y & 0xFFFF) << 16) | (cx & 0xFFFF)
        if hdesk.u32.SendMessageW(ov_hwnd, WM_NCHITTEST, 0, lp) == HTCLIENT:
            hit_y = y
            break
    if hit_y is None:
        return 0
    xs_hit = []
    for x in range(max(0, cx - 220), min(w, cx + 220), 2):
        lp = ((hit_y & 0xFFFF) << 16) | (x & 0xFFFF)
        if hdesk.u32.SendMessageW(ov_hwnd, WM_NCHITTEST, 0, lp) == HTCLIENT:
            xs_hit.append(x)
    return (max(xs_hit) - min(xs_hit)) + 2 if xs_hit else 0
def click_dp(hwnd, scale, dp_x, dp_y):
    px = int((16.0 + dp_x) * scale)
    py = int((16.0 + dp_y) * scale)
    lp = ((py & 0xFFFF) << 16) | (px & 0xFFFF)
    hdesk.u32.SendMessageW(hwnd, WM_LBUTTONUP, 0, lp)


def main():
    assert EXE, "FAIL: 找不到 ouuutput.exe"
    conf, bak = REPO / "ouuutput.conf", REPO / "ouuutput.conf.orbtest.bak"
    had_conf = conf.exists()
    d = hdesk.open_desktop(DESKTOP)
    saved_ns = os.environ.get("OUUUTPUT_SINGLE_NS")
    hp = hpt = None
    epid = 0
    try:
        if had_conf:
            shutil.copy2(conf, bak)
            clean = re.sub(r"(?m)^\s*(overlay_mode|halo_tint|orb_pos|orb_size|ring_size|ring_sens)\s*=.*\n?", "", conf.read_text(encoding="utf-8"))
            conf.write_text(clean, encoding="utf-8")
        else:
            conf.write_text("", encoding="utf-8")
        os.environ["OUUUTPUT_SINGLE_NS"] = NS
        hp, hpt, epid = hdesk.launch(str(EXE), "run", DESKTOP)
        tray_hwnd, t0 = None, time.time()
        while time.time() - t0 < 35 and not tray_hwnd:
            tray_hwnd, _ = find_win(d, epid, "OuuutputTrayWnd")
            if not tray_hwnd:
                time.sleep(0.4)
        assert tray_hwnd, "FAIL: 35s 内未找到 OuuutputTrayWnd"

        # 1. 打开托盘面板并切换到「声波」（第 2 段，x≈135dp, y≈156dp）
        hdesk.u32.PostMessageW(tray_hwnd, WM_TRAY, 0, WM_RBUTTONUP)
        menu_hwnd, vis, deadline = None, False, time.time() + 5.0
        while time.time() < deadline and not vis:
            menu_hwnd, vis = find_win(d, epid, "OuuutputTrayMenuWnd")
            if not vis:
                time.sleep(0.1)
        assert menu_hwnd and vis, "FAIL: OuuutputTrayMenuWnd 未弹出"
        w0, _ = win_rect(menu_hwnd)
        scale = w0 / 376.0

        click_dp(menu_hwnd, scale, 135.0, 156.0)
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "overlay_mode") != "orb":
            time.sleep(0.1)
        assert conf_value(conf, "overlay_mode") == "orb", f"overlay_mode={conf_value(conf, 'overlay_mode')}"
        time.sleep(0.25)

        # 1b. 验证默认中号声波更紧凑（<= 145dp 宽）且切换「小」/「大」会真实改变声波矩形尺寸
        ov_hwnd, ov_vis = find_win(d, epid, "OuuutputOverlayWnd")
        assert ov_hwnd and ov_vis, "FAIL: 未找到 OuuutputOverlayWnd"
        orb_w_med = probe_indicator_width(ov_hwnd)
        assert 40 <= orb_w_med <= int(145 * scale) + 8, f"默认声波应更小: orb_w_med={orb_w_med}, scale={scale}"
        click_dp(menu_hwnd, scale, 181.0, 192.0)  # 声波「小」
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "orb_size") != "small":
            time.sleep(0.1)
        assert conf_value(conf, "orb_size") == "small", f"orb_size={conf_value(conf, 'orb_size')}"
        time.sleep(0.25)
        orb_w_small = probe_indicator_width(ov_hwnd)
        click_dp(menu_hwnd, scale, 249.0, 192.0)  # 声波「大」
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "orb_size") != "large":
            time.sleep(0.1)
        assert conf_value(conf, "orb_size") == "large", f"orb_size={conf_value(conf, 'orb_size')}"
        time.sleep(0.25)
        orb_w_large = probe_indicator_width(ov_hwnd)
        assert 0 < orb_w_small < orb_w_med < orb_w_large, f"声波大小档未拉开: {orb_w_small} < {orb_w_med} < {orb_w_large}"
        print(f"[PASS] 声波大小档实机生效: 小={orb_w_small}px < 中={orb_w_med}px < 大={orb_w_large}px")

        # 2. 在悬浮声波模式下点「赛博霓虹」色点（y=219dp, x=228dp），确认保留 orb 模式而非跳走
        click_dp(menu_hwnd, scale, 228.0, 219.0)
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "halo_tint") != "cyberpunk":
            time.sleep(0.1)
        assert conf_value(conf, "halo_tint") == "cyberpunk", f"halo_tint={conf_value(conf, 'halo_tint')}"
        assert conf_value(conf, "overlay_mode") == "orb", "切配色不应改变 overlay_mode=orb"
        print("[PASS] 悬浮声波模式下切换配色: halo_tint=cyberpunk 且保持 overlay_mode=orb")

        # 3. 确认预览窗口期间取消了 WS_EX_TRANSPARENT（可被左键拖动）
        ex_style = hdesk.u32.GetWindowLongW(ov_hwnd, GWL_EXSTYLE)
        assert (ex_style & WS_EX_TRANSPARENT) == 0, f"预览显示期间应允许命中拖动 (ex={ex_style:#x})"
        print("[PASS] 预览期间 OuuutputOverlayWnd 已开放命中拖动（非点击穿透）")

        # 4. 点击托盘「归位」按钮（x=298dp, y=192dp）确认 orb_pos=auto 落盘
        click_dp(menu_hwnd, scale, 298.0, 192.0)
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "orb_pos") != "auto":
            time.sleep(0.1)
        assert conf_value(conf, "orb_pos") == "auto", f"orb_pos={conf_value(conf, 'orb_pos')}"
        print("[PASS] 点击「归位」: orb_pos=auto 落盘")

        # 5. 切换到「白环」（第 3 段，x≈208dp, y≈156dp），验证 overlay_mode=ring、三档尺寸与三档灵敏度
        click_dp(menu_hwnd, scale, 208.0, 156.0)
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "overlay_mode") != "ring":
            time.sleep(0.1)
        assert conf_value(conf, "overlay_mode") == "ring", f"overlay_mode={conf_value(conf, 'overlay_mode')}"
        time.sleep(0.25)
        ring_w_med = probe_indicator_width(ov_hwnd)
        click_dp(menu_hwnd, scale, 93.0, 192.0)   # 白环「小」
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "ring_size") != "small":
            time.sleep(0.1)
        assert conf_value(conf, "ring_size") == "small", f"ring_size={conf_value(conf, 'ring_size')}"
        time.sleep(0.25)
        ring_w_small = probe_indicator_width(ov_hwnd)
        click_dp(menu_hwnd, scale, 189.0, 192.0)  # 白环「大」
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "ring_size") != "large":
            time.sleep(0.1)
        assert conf_value(conf, "ring_size") == "large", f"ring_size={conf_value(conf, 'ring_size')}"
        time.sleep(0.25)
        ring_w_large = probe_indicator_width(ov_hwnd)
        assert 0 < ring_w_small < ring_w_med < ring_w_large, f"白环大小档未拉开: {ring_w_small} < {ring_w_med} < {ring_w_large}"
        print(f"[PASS] 纯白流体圆环大小档实机生效: 小={ring_w_small}px < 中={ring_w_med}px < 大={ring_w_large}px")

        click_dp(menu_hwnd, scale, 189.0, 218.0)  # 白环灵敏度「高」
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "ring_sens") != "high":
            time.sleep(0.1)
        assert conf_value(conf, "ring_sens") == "high", f"ring_sens={conf_value(conf, 'ring_sens')}"
        click_dp(menu_hwnd, scale, 93.0, 218.0)   # 白环灵敏度「低」
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "ring_sens") != "low":
            time.sleep(0.1)
        assert conf_value(conf, "ring_sens") == "low", f"ring_sens={conf_value(conf, 'ring_sens')}"
        assert conf_value(conf, "overlay_mode") == "ring", "切灵敏度不应改变 overlay_mode=ring"
        print("[PASS] 纯白流体圆环灵敏度三档切换实机生效并落盘: high / low")
        deadline = time.time() + 5.0
        while time.time() < deadline:
            ex_after = hdesk.u32.GetWindowLongW(ov_hwnd, GWL_EXSTYLE)
            if (ex_after & WS_EX_TRANSPARENT) != 0:
                break
            time.sleep(0.15)
        ex_after = hdesk.u32.GetWindowLongW(ov_hwnd, GWL_EXSTYLE)
        assert (ex_after & WS_EX_TRANSPARENT) != 0, f"淡出结束后未恢复 WS_EX_TRANSPARENT (ex={ex_after:#x})"
        print("[PASS] 预览结束后自动平滑淡出并恢复 WS_EX_TRANSPARENT 穿透")
        print("\nALL PASS")
        return 0
    finally:
        if hpt:
            hdesk.close(hpt)
        if hp:
            hdesk.terminate(epid)
            hdesk.close(hp)
        hdesk.close(d)
        if saved_ns is None:
            os.environ.pop("OUUUTPUT_SINGLE_NS", None)
        else:
            os.environ["OUUUTPUT_SINGLE_NS"] = saved_ns
        shutil.move(str(bak), str(conf)) if (had_conf and bak.exists()) else (conf.unlink() if (not had_conf and conf.exists()) else None)


if __name__ == "__main__":
    sys.exit(main())
