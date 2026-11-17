# -*- coding: utf-8 -*-
"""流光光晕(Halo)模式进程外回归测试（运行于独立桌面，备份并还原 ouuutput.conf）。"""
import os, pathlib, re, shutil, subprocess, sys, time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import hdesk  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
DIRS = ["target-test/x86_64-pc-windows-gnu/release", "target-test/release",
        "target/x86_64-pc-windows-gnu/release", "target/release"]
DESKTOP, NS, WM_COMMAND, TRAY_CLASS = "SAI-HaloMode", "ouuutput-halotest", 0x0111, "OuuutputTrayWnd"


def conf_value(path, key):
    m = re.search(rf"(?m)^\s*{re.escape(key)}\s*=\s*(.+?)\s*$", path.read_text(encoding="utf-8"))
    return m.group(1) if m else None


def main():
    exe = next((REPO / d / "ouuutput.exe" for d in DIRS if (REPO / d / "ouuutput.exe").exists()), None)
    assert exe, "FAIL: 找不到 ouuutput.exe"
    conf, bak = REPO / "ouuutput.conf", REPO / "ouuutput.conf.halotest.bak"
    had_conf = conf.exists()
    d = hdesk.open_desktop(DESKTOP)
    saved_ns = os.environ.get("OUUUTPUT_SINGLE_NS")
    hp = hpt = None
    epid = fails = 0
    try:
        if had_conf:
            shutil.copy2(conf, bak)
            clean = re.sub(r"(?m)^\s*(halo_blur|halo_peak|halo_valley|halo_speed|halo_opacity|halo_tint)\s*=.*\n?", "", conf.read_text(encoding="utf-8"))
            conf.write_text(clean, encoding="utf-8")
        else:
            conf.write_text("", encoding="utf-8")
        cfg_out = subprocess.check_output([str(exe), "config", "--overlay-mode", "halo"], cwd=str(REPO), text=True, encoding="utf-8")
        assert all(k in cfg_out for k in ("流光光晕", "模糊 52px", "波峰 33px", "波谷 12px")), cfg_out
        print("[PASS] ouuutput config --overlay-mode halo 默认参数输出正确")
        os.environ["OUUUTPUT_SINGLE_NS"] = NS
        hp, hpt, epid = hdesk.launch(str(exe), "run", DESKTOP)
        tray_hwnd, t0 = None, time.time()
        while time.time() - t0 < 35 and not tray_hwnd:
            tray_hwnd = next((h for h, c, _, _ in hdesk.enum_windows(epid, d) if c == TRAY_CLASS), None)
            time.sleep(0.4) if not tray_hwnd else None
        if not tray_hwnd:
            return 1
        print(f"托盘窗口 = {tray_hwnd:#x} (pid {epid})")
        for cmd_id, key, expect, label in [
            (304, "overlay_mode", "halo", "悬浮显示 → 流光光晕"),
            (381, "halo_tint", "cyan", "配色 → 天青水色"),
            (382, "halo_tint", "violet", "配色 → 蓝紫极光"),
            (383, "halo_tint", "siri", "配色 → Siri 绚烂"),
            (384, "halo_tint", "rainbow", "配色 → 流光彩虹"),
            (385, "halo_tint", "cyberpunk", "配色 → 赛博霓虹"),
            (386, "halo_tint", "emerald", "配色 → 翡翠幽光"),
            (387, "halo_tint", "sunset", "配色 → 落日熔金"),
            (380, "halo_tint", "blue", "配色 → 电光蓝（默认）"),
            (392, "halo_blur", "86", "范围 → 宽广(blur=86)"),
            (397, "halo_speed", "0.95", "流速 → 快(0.95x)"),
            (402, "halo_opacity", "0.98", "浓淡 → 浓(0.98)"),
        ]:
            hdesk.u32.SendMessageW(tray_hwnd, WM_COMMAND, cmd_id, 0)
            got, deadline = None, time.time() + 5.0
            while time.time() < deadline and got != expect:
                got = conf_value(conf, key)
                time.sleep(0.15) if got != expect else None
            print(f"[{'PASS' if got == expect else 'FAIL'}] {label}: {key} = {got!r}(期望 {expect!r})")
            fails += int(got != expect)
    finally:
        hdesk.close(hpt) if hpt else None
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
