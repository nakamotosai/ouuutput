# -*- coding: utf-8 -*-
"""Fluent 托盘浮窗（OuuutputTrayMenuWnd）实机窗口 + 点击交互回归测试（独立私有桌面）。"""
import ctypes, ctypes.wintypes as wt, os, pathlib, re, shutil, sys, time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import hdesk  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
EXE = next((REPO / d / "ouuutput.exe" for d in [
    "target/x86_64-pc-windows-gnu/release", "target/release"
] if (REPO / d / "ouuutput.exe").exists()), None)
DESKTOP, NS = "SAI-TrayPopover", "ouuutput-popovertest"
WM_TRAY, WM_RBUTTONUP, WM_LBUTTONUP, WM_KEYDOWN, VK_ESCAPE = 0x0464, 0x0205, 0x0202, 0x0100, 0x1B


class RECT(ctypes.Structure):
    _fields_ = [("left", wt.LONG), ("top", wt.LONG), ("right", wt.LONG), ("bottom", wt.LONG)]


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


def click_dp(hwnd, scale, dp_x, dp_y):
    # 窗口坐标 = (SHADOW_PAD_DP + dp) * scale
    px = int((16.0 + dp_x) * scale)
    py = int((16.0 + dp_y) * scale)
    lp = ((py & 0xFFFF) << 16) | (px & 0xFFFF)
    hdesk.u32.SendMessageW(hwnd, WM_LBUTTONUP, 0, lp)


def main():
    assert EXE, "FAIL: 找不到 ouuutput.exe"
    conf, bak = REPO / "ouuutput.conf", REPO / "ouuutput.conf.popover.bak"
    had_conf = conf.exists()
    if had_conf:
        shutil.copy2(conf, bak)
        clean = re.sub(r"(?m)^\s*(halo_blur|halo_peak|halo_valley|halo_speed|halo_opacity)\s*=.*\n?", "", conf.read_text(encoding="utf-8"))
        conf.write_text(clean, encoding="utf-8")
    else:
        conf.write_text("", encoding="utf-8")

    d = hdesk.open_desktop(DESKTOP)
    saved_ns = os.environ.get("OUUUTPUT_SINGLE_NS")
    hp = hpt = None
    epid = fails = 0
    try:
        os.environ["OUUUTPUT_SINGLE_NS"] = NS
        hp, hpt, epid = hdesk.launch(str(EXE), "run", DESKTOP)
        tray_hwnd, t0 = None, time.time()
        while time.time() - t0 < 35 and not tray_hwnd:
            tray_hwnd, _ = find_win(d, epid, "OuuutputTrayWnd")
            if not tray_hwnd:
                time.sleep(0.4)
        assert tray_hwnd, "FAIL: 35s 内未找到 OuuutputTrayWnd"

        # 1. 打开 Fluent 托盘面板
        hdesk.u32.PostMessageW(tray_hwnd, WM_TRAY, 0, WM_RBUTTONUP)
        menu_hwnd, vis, deadline = None, False, time.time() + 5.0
        while time.time() < deadline and not vis:
            menu_hwnd, vis = find_win(d, epid, "OuuutputTrayMenuWnd")
            if not vis:
                time.sleep(0.1)
        assert menu_hwnd and vis, f"FAIL: OuuutputTrayMenuWnd 未弹出 (hwnd={menu_hwnd}, vis={vis})"
        w0, h0 = win_rect(menu_hwnd)
        scale = w0 / 376.0
        print(f"[PASS] OuuutputTrayMenuWnd 弹出成功: {w0}x{h0} (scale={scale:.2f})")

        # 2. 点击「关闭」段（overlay_mode = off），确认面板高度收窄（隐藏情境色板）
        click_dp(menu_hwnd, scale, 48.0, 156.0)
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "overlay_mode") != "off":
            time.sleep(0.1)
        assert conf_value(conf, "overlay_mode") == "off", f"overlay_mode={conf_value(conf, 'overlay_mode')}"
        time.sleep(0.2)
        _, h_off = win_rect(menu_hwnd)
        assert h_off < h0, f"期望关闭模式高度缩小: h_off={h_off}, h0={h0}"
        print(f"[PASS] 点击「关闭」段: overlay_mode=off, 高度自动收拢 {h0} -> {h_off}")

        # 3. 点击「流光」段（overlay_mode = halo），确认高度恢复并出现流光色点与调参胶囊
        click_dp(menu_hwnd, scale, 290.0, 156.0)
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "overlay_mode") != "halo":
            time.sleep(0.1)
        assert conf_value(conf, "overlay_mode") == "halo", f"overlay_mode={conf_value(conf, 'overlay_mode')}"
        time.sleep(0.2)
        _, h_halo = win_rect(menu_hwnd)
        assert h_halo > h_off, f"期望流光模式展开色点行: h_halo={h_halo}, h_off={h_off}"
        print(f"[PASS] 点击「流光」段: overlay_mode=halo, 情境区展开 {h_off} -> {h_halo}")

        # 4. 连续快速切换配色到「赛博霓虹」并验证预览不会过早消失（预览窗口应保持可见 >= 2s）
        click_dp(menu_hwnd, scale, 150.0, 213.0)  # Siri 绚烂
        time.sleep(0.25)
        click_dp(menu_hwnd, scale, 228.0, 213.0)  # 赛博霓虹
        deadline = time.time() + 4.0
        while time.time() < deadline and conf_value(conf, "halo_tint") != "cyberpunk":
            time.sleep(0.1)
        assert conf_value(conf, "halo_tint") == "cyberpunk", f"halo_tint={conf_value(conf, 'halo_tint')}"
        ov_hwnd, ov_vis = find_win(d, epid, "OuuutputOverlayWnd")
        assert ov_hwnd and ov_vis, "FAIL: 切换配色后未弹出流光预览"
        time.sleep(1.9)
        _, ov_vis_late = find_win(d, epid, "OuuutputOverlayWnd")
        assert ov_vis_late, "FAIL: 流光预览未满 2 秒就被提前隐藏"
        print("[PASS] 连续点击「赛博霓虹」: halo_tint=cyberpunk 落盘，且预览稳定保持 >= 2s")

        # 5. 点击「范围」「流速」「浓淡」胶囊并确认写入 ouuutput.conf
        click_dp(menu_hwnd, scale, 62.0, 243.0)   # 范围: 标准 -> 宽广
        click_dp(menu_hwnd, scale, 144.0, 243.0)  # 流速: 中 -> 快
        click_dp(menu_hwnd, scale, 220.0, 243.0)  # 浓淡: 中 -> 浓
        deadline = time.time() + 4.0
        while time.time() < deadline and (
            conf_value(conf, "halo_blur") != "86"
            or conf_value(conf, "halo_speed") != "0.95"
            or conf_value(conf, "halo_opacity") != "0.98"
        ):
            time.sleep(0.1)
        assert conf_value(conf, "halo_blur") == "86", f"halo_blur={conf_value(conf, 'halo_blur')}"
        assert conf_value(conf, "halo_speed") == "0.95", f"halo_speed={conf_value(conf, 'halo_speed')}"
        assert conf_value(conf, "halo_opacity") == "0.98", f"halo_opacity={conf_value(conf, 'halo_opacity')}"
        print("[PASS] 流光调参胶囊（范围/流速/浓淡）全部实时生效并落盘")

        # 6. 点击「排版与断句细节」折叠抽屉（y=371dp），确认面板内展开
        click_dp(menu_hwnd, scale, 170.0, 381.0)
        time.sleep(0.2)
        _, h_exp = win_rect(menu_hwnd)
        assert h_exp > h_halo + 120 * scale, f"期望抽屉展开: h_exp={h_exp}, h_halo={h_halo}"
        print(f"[PASS] 展开「排版与断句细节」抽屉: 高度 {h_halo} -> {h_exp}")
        hdesk.u32.SendMessageW(menu_hwnd, WM_KEYDOWN, VK_ESCAPE, 0)
        time.sleep(0.15)
        _, vis_after = find_win(d, epid, "OuuutputTrayMenuWnd")
        assert not vis_after, "FAIL: Esc 后 OuuutputTrayMenuWnd 未收起"
        print("[PASS] 按 Esc 干净收起浮窗")
    except AssertionError as e:
        print(e)
        fails += 1
    finally:
        if hpt:
            hdesk.close(hpt)
        if hp:
            hdesk.terminate(epid)
            hdesk.close(hp)
        hdesk.close(d)
        os.environ.pop("OUUUTPUT_SINGLE_NS", None) if saved_ns is None else os.environ.__setitem__("OUUUTPUT_SINGLE_NS", saved_ns)
        shutil.move(str(bak), str(conf)) if (had_conf and bak.exists()) else (conf.unlink() if (not had_conf and conf.exists()) else None)
    print("\n" + ("ALL PASS" if fails == 0 else f"FAILED: {fails}"))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
