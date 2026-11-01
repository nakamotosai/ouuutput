# -*- coding: utf-8 -*-
"""回归:组合态(Pebrel)的收尾兜底 —— 等不到真定稿时,把**已经显示出来的那一版**
提交进应用,而不是把整句丢掉(2026-11-02 用户要求:不要完全丢弃的方案)。

怎么验(全程不碰用户终端、不碰用户的 Pebrel):
  * 引擎跑在**私有桌面**上,靶子是 `sainput-tsf-host.exe`(只为提供前台窗口/进程名);
  * `--ime-wmchar pebrel:sainput-tsf-host.exe` 强制走 Pebrel 组合态;
  * `PEBREL_CONFIG_DIR` 指向 tmp 下的假数据目录,里面是一个**假 Pebrel 控制面**
    (TCP,一行一个 JSON):`runtime.snapshot` 给一个窗口/pane,`pane.compose` 记录收到的
    set/commit;
  * 假侧车只吐一条 partial、**不给定稿**(`FAKE_ASR_SCRIPT=p:…`)→ 引擎的收尾上限
    (`SAINPUT_FINAL_WAIT_MS`)到点后应该自己把这一版提交掉。

判据:假控制面收到一次 `commit=true` 且 text == 那条 partial;引擎日志里明确记了
「上屏改用 Pebrel 组合态缓冲层」和「收尾硬上限」。

跑法:
    cargo build --release --target-dir target-test -p sainput-cli -p sainput-tsf-host
    python tools/test_compose_cap.py
"""
import json
import os
import pathlib
import socket
import sys
import threading
import time
import wave

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import hdesk  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
DIRS = ["target-test/x86_64-pc-windows-gnu/release", "target-test/release",
        "target/x86_64-pc-windows-gnu/release", "target/release"]
DESKTOP = "SAI-ComposeCap"
NS = "sainput-captest"
TEXT = "说到一半的一句"
ENV_KEYS = ("SAINPUT_SINGLE_NS", "SAINPUT_DIAG_AUTOSTART", "SAINPUT_DIAG_TARGET_HWND",
            "FAKE_ASR_SCRIPT", "SAINPUT_FINAL_WAIT_MS", "PEBREL_CONFIG_DIR")


def find(rel, what):
    for d in DIRS:
        p = REPO / d / rel
        if p.exists():
            return p
    print(f"FAIL: {what} not built")
    sys.exit(2)


def make_wav(path):
    if path.exists():
        return path
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(b"\x00\x00" * 6400)
    return path


class FakePebrel(threading.Thread):
    """假控制面:只实现这条路上用到的两个方法,并把调用原样记下来。"""

    def __init__(self):
        super().__init__(daemon=True)
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(16)
        self.port = self.sock.getsockname()[1]
        self.token = "0123456789abcdef0123456789abcdef"
        self.host_pid = 0
        self.calls = []
        self.lock = threading.Lock()

    def run(self):
        while True:
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            try:
                data = b""
                while not data.endswith(b"\n"):
                    chunk = conn.recv(65536)
                    if not chunk:
                        break
                    data += chunk
                req = json.loads(data.decode("utf-8", "replace") or "{}")
                method = req.get("method")
                params = req.get("params")
                with self.lock:
                    self.calls.append((method, params))
                if method == "runtime.snapshot":
                    result = {
                        "process_id": self.host_pid,
                        "windows": [{"id": 1, "focused": True, "focused_pane_id": 1,
                                     "tabs": [{"active": True, "focused_pane_id": 1}]}],
                    }
                else:
                    result = {"window_id": 1, "pane_id": 1,
                              "committed": bool((params or {}).get("commit")),
                              "input": "compose"}
                conn.sendall((json.dumps({"ok": True, "result": result}) + "\n").encode())
            except Exception as e:  # 协议写错也要留痕,别让引擎等超时
                print(f"[fake-pebrel] {e}", flush=True)
            finally:
                conn.close()

    def commits(self):
        with self.lock:
            return [p for (m, p) in self.calls
                    if m == "pane.compose" and (p or {}).get("commit")]

    def stop(self):
        try:
            self.sock.close()
        except OSError:
            pass


def main():
    host_exe = find("sainput-tsf-host.exe", "sainput-tsf-host.exe")
    sainput = find("sainput.exe", "sainput.exe")
    tmp = REPO / "tmp"
    tmp.mkdir(exist_ok=True)
    wav = make_wav(tmp / "race-silence.wav")
    data_dir = tmp / "pebrel-fake"
    data_dir.mkdir(exist_ok=True)
    log_dir = tmp / "engine-cap"
    log_dir.mkdir(exist_ok=True)
    for f in log_dir.glob("sainput-*.log"):
        f.unlink()

    saved = {k: os.environ.get(k) for k in ENV_KEYS}
    h = hdesk.open_desktop(DESKTOP)
    hp = ep = None
    hpid = epid = 0
    pebrel = FakePebrel()
    try:
        hp, ht, hpid = hdesk.launch(
            str(host_exe), f"--life 30000 --log {tmp / 'cap-host.log'} --control edit", DESKTOP)
        edit = None
        t0 = time.time()
        while time.time() - t0 < 8:
            edit = hdesk.find_edit(hpid, h, "Edit")
            if edit:
                break
            time.sleep(0.2)
        if not edit:
            print("FAIL: 宿主壳的 EDIT 没出现")
            return 1

        pebrel.host_pid = hpid
        pebrel.start()
        (data_dir / "runtime.port").write_text(
            f"{pebrel.port} {pebrel.token} 1\n", encoding="utf-8")

        os.environ["SAINPUT_SINGLE_NS"] = NS
        os.environ["SAINPUT_DIAG_AUTOSTART"] = "4000"
        os.environ["SAINPUT_DIAG_TARGET_HWND"] = hex(edit)
        os.environ["FAKE_ASR_SCRIPT"] = f"p:{TEXT}"
        os.environ["SAINPUT_FINAL_WAIT_MS"] = "800"
        os.environ["PEBREL_CONFIG_DIR"] = str(data_dir)

        ep, et, epid = hdesk.launch(
            str(sainput),
            "run --script " + str(REPO / "tools" / "fake_asr_sidecar.py")
            + f' --python "{sys.executable}"'
            + f" --feed {wav} --feed-restart"
            + " --ime-wmchar pebrel:sainput-tsf-host.exe"
            + " --no-leading-space --no-trailing-space --overlay-mode off"
            + f" --log-dir {log_dir}",
            DESKTOP)

        # 时间线:1.5s 后按下 → 4s 后松开 → 尾巴 ~0.25s → 上限 0.8s。
        # 等到 ~8s:提交必须已经发生。
        deadline = time.time() + 9
        while time.time() < deadline and not pebrel.commits():
            time.sleep(0.1)

        logs = sorted(log_dir.glob("sainput-*.log"))
        log = logs[-1].read_text(encoding="utf-8", errors="replace") if logs else ""
        commits = pebrel.commits()
        ok_mode = "上屏改用 Pebrel 组合态缓冲层" in log
        ok_cap = "收尾硬上限" in log
        ok_commit = any((c or {}).get("text") == TEXT for c in commits)
        print(f"[{'PASS' if ok_mode else 'FAIL'}] 走了组合态: {ok_mode}")
        print(f"[{'PASS' if ok_cap else 'FAIL'}] 收尾上限到点: {ok_cap}")
        print(f"[{'PASS' if ok_commit else 'FAIL'}] 已显示的文本被提交: {commits!r}")
        if not (ok_mode and ok_cap and ok_commit):
            print("--- 引擎日志尾部 ---")
            print("\n".join(log.splitlines()[-20:]))
            return 1
        print("\nALL PASS")
        return 0
    finally:
        for pid in (epid, hpid):
            if pid:
                hdesk.terminate(pid)
        if ep:
            hdesk.close(ep)
        if hp:
            hdesk.close(hp)
        hdesk.close(h)
        pebrel.stop()
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


if __name__ == "__main__":
    sys.exit(main())
