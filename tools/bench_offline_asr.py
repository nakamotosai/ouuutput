# -*- coding: utf-8 -*-
"""离线/流式 ASR 多引擎对比打表:sherpa(现用流式) vs sherpa-offline SenseVoice vs funasr SenseVoiceSmall vs faster-whisper large-v3-turbo。

和 tools/bench_models.py 的分工:那边只比 sherpa 自家的一堆流式模型(看首字延迟),这边比**不同引擎**
在同一批音频上的「识别文本 + RTF + 显存」。默认素材 = logs/corpus/ 里用户真人录音 + tmp/ 下三句已知内容的合成音
(tmp/zh-en.wav 中英混说、tmp/code.wav 报验证码、tmp/digits.wav 纯数字)。合成音必须标注,别当真机结论。

引擎(前两条走 sherpa_onnx 运行时,不用 torch,装进现有产品不用换运行时):
    sherpa            现用 160ms 流式 zipformer(CPU)
    sensevoice-onnx   sherpa_onnx 自带的离线 SenseVoice(CPU)
    sensevoice        funasr SenseVoiceSmall(GPU,要 torch)
    whisper           faster-whisper large-v3-turbo(GPU,要 ctranslate2)

用法:
    python tools/bench_offline_asr.py --download                 # 只下权重(可与其他步骤并行跑)
    python tools/bench_offline_asr.py                            # 默认素材 + 全部引擎
    python tools/bench_offline_asr.py --wavs tmp/code.wav --engines sherpa,whisper
    python tools/bench_offline_asr.py --json tmp/asr_bench.json  # 附带落一份机器可读结果

权重来源(目录名即来源,下载到 models/ 下):
    models/SenseVoiceSmall-modelscope/                        ModelScope iic/SenseVoiceSmall
    models/SenseVoiceSmall-huggingface/                       HF FunAudioLLM/SenseVoiceSmall(备用)
    models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/  HF csukuangfj/…-sense-voice-…
    models/faster-whisper-large-v3-turbo-ct2/                 HF Systran/faster-whisper-large-v3-turbo
    models/faster-whisper-large-v3-turbo-ct2-mobiuslabsgmbh/  HF mobiuslabsgmbh/faster-whisper-large-v3-turbo
    models/faster-whisper-large-v3-turbo-ct2-modelscope/      ModelScope pengzhendong/faster-whisper-large-v3-turbo
"""
import sys, os, glob, re, json, time, argparse, subprocess
sys.stdout.reconfigure(encoding="utf-8")
import numpy as np, soundfile as sf

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MODELS = os.path.join(ROOT, "models")
RATE = 16000

# 现用模型:与 tools/bench_models.py、asr_sidecar 同一份 160ms 流式 zipformer
SHERPA_DIR = os.path.join(MODELS, "sherpa-onnx-x-asr-160ms-streaming-zipformer-transducer-zh-en-int8-2026-06-05")
# 离线 SenseVoice 的 sherpa-onnx 版(同权重、同运行时,CPU 可跑)
SENSE_VOICE_ONNX_DIR = os.path.join(MODELS, "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17")

# (引擎名, 预期目录, 下载来源说明) —— 按优先级排列,取第一个已存在的
ENGINE_DIRS = {
    "sensevoice": [
        ("SenseVoiceSmall-modelscope", "ModelScope:iic/SenseVoiceSmall"),
        ("SenseVoiceSmall-huggingface", "HF:FunAudioLLM/SenseVoiceSmall"),
    ],
    "whisper": [
        ("faster-whisper-large-v3-turbo-ct2", "HF:Systran/faster-whisper-large-v3-turbo"),
        ("faster-whisper-large-v3-turbo-ct2-mobiuslabsgmbh", "HF:mobiuslabsgmbh/faster-whisper-large-v3-turbo"),
        ("faster-whisper-large-v3-turbo-ct2-modelscope", "ModelScope:pengzhendong/faster-whisper-large-v3-turbo"),
    ],
    "sensevoice-onnx": [
        ("sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
         "HF:csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"),
    ],
}

# Whisper 解码参数:关掉 VAD/温度回退,保证 RTF 可复现
WHISPER_KW = dict(beam_size=5, vad_filter=False, temperature=0.0,
                  condition_on_previous_text=False, word_timestamps=False)


def need(rel_dir):
    """返回 engines 里第一个已就绪的权重目录,没有则 None。只看 >1MB 的权重文件 —— 0 字节的是没下完的残件。"""
    for d, _src in ENGINE_DIRS[rel_dir]:
        p = os.path.join(MODELS, d)
        if not os.path.isdir(p):
            continue
        for f in os.listdir(p):
            fp = os.path.join(p, f)
            if f.endswith((".onnx", ".pt", ".bin")) and os.path.getsize(fp) > 1024 * 1024:
                return p
    return None


# ---------------------------------------------------------------- 下载
def download_sensevoice():
    ok = []
    p = os.path.join(MODELS, "SenseVoiceSmall-modelscope")
    if need("sensevoice"):
        return "已存在,跳过"
    try:
        from modelscope import snapshot_download
        snapshot_download("iic/SenseVoiceSmall", local_dir=p)
        return f"ModelScope iic/SenseVoiceSmall -> {p}"
    except Exception as e:
        ok.append(f"ModelScope 失败: {type(e).__name__}: {str(e)[:160]}")
    try:
        from huggingface_hub import snapshot_download
        p2 = os.path.join(MODELS, "SenseVoiceSmall-huggingface")
        snapshot_download("FunAudioLLM/SenseVoiceSmall", local_dir=p2)
        return f"HF FunAudioLLM/SenseVoiceSmall -> {p2}"
    except Exception as e:
        ok.append(f"HF 失败: {type(e).__name__}: {str(e)[:160]}")
    return "全部来源失败 | " + " | ".join(ok)


def download_whisper():
    if need("whisper"):
        return "已存在,跳过"
    errs = []
    tries = [
        ("Systran/faster-whisper-large-v3-turbo", "hf", "faster-whisper-large-v3-turbo-ct2"),
        ("mobiuslabsgmbh/faster-whisper-large-v3-turbo", "hf", "faster-whisper-large-v3-turbo-ct2-mobiuslabsgmbh"),
        ("pengzhendong/faster-whisper-large-v3-turbo", "ms", "faster-whisper-large-v3-turbo-ct2-modelscope"),
    ]
    for repo, kind, d in tries:
        p = os.path.join(MODELS, d)
        try:
            if kind == "hf":
                from huggingface_hub import snapshot_download
                snapshot_download(repo, local_dir=p, allow_patterns=["*.bin", "*.json", "*.txt", "*.md"])
            else:
                from modelscope import snapshot_download
                snapshot_download(repo, local_dir=p)
            return f"{kind.upper()} {repo} -> {p}"
        except Exception as e:
            errs.append(f"{kind}:{repo} 失败 {type(e).__name__}: {str(e)[:140]}")
    return "全部来源失败 | " + " | ".join(errs)


def download_sensevoice_onnx():
    if need("sensevoice-onnx"):
        return "已存在,跳过"
    errs = []
    p = SENSE_VOICE_ONNX_DIR
    for repo in ("csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
                 "k2-fsa/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"):
        try:
            from huggingface_hub import snapshot_download
            snapshot_download(repo, local_dir=p, allow_patterns=["*.onnx", "*.txt", "*.md"])
            return f"HF {repo} -> {p}"
        except Exception as e:
            errs.append(f"HF:{repo} 失败 {type(e).__name__}: {str(e)[:140]}")
    return "全部来源失败 | " + " | ".join(errs)


# ---------------------------------------------------------------- 引擎
class SherpaEngine:
    """现用流式引擎:同一份 160ms zipformer,按 32ms 帧喂,整句结束后取 final 文本。
    itn=True 时额外用 tools/itn.py 把「九零三七二」规整成「90372」—— 和 sidecar 默认行为一致,
    这样和 whisper(自带 ITN)比数字才是同一口径。"""
    name = "sherpa-160ms(现用/流式)"
    vram = "CPU(不占显存)"

    def __init__(self, threads=2, itn=True):
        import sherpa_onnx
        md = SHERPA_DIR
        self.itn = itn

        def find(kind):
            g = sorted(glob.glob(os.path.join(md, f"{kind}*.int8.onnx")))
            if not g:
                g = sorted(glob.glob(os.path.join(md, f"{kind}*.onnx")))
            return g[0]

        self.rec = sherpa_onnx.OnlineRecognizer.from_transducer(
            tokens=os.path.join(md, "tokens.txt"), encoder=find("encoder"),
            decoder=find("decoder"), joiner=find("joiner"), num_threads=threads,
            sample_rate=RATE, feature_dim=80, decoding_method="greedy_search",
            enable_endpoint_detection=True, rule1_min_trailing_silence=2.4,
            rule2_min_trailing_silence=1.2, rule3_min_utterance_length=20, provider="cpu")

    def transcribe(self, x):
        rec, frame = self.rec, RATE * 32 // 1000
        st = rec.create_stream()
        t0 = time.perf_counter()
        for i in range(0, len(x), frame):
            st.accept_waveform(RATE, x[i:i + frame])
            while rec.is_ready(st):
                rec.decode_stream(st)
        st.input_finished()
        while rec.is_ready(st):
            rec.decode_stream(st)
        el = time.perf_counter() - t0
        raw = rec.get_result(st)
        extra = {}
        if self.itn:
            try:
                sys.path.insert(0, os.path.join(ROOT, "tools"))
                import itn as itn_mod
                fin = itn_mod.normalize(raw)
                if fin != raw:
                    extra["itn"] = fin
            except Exception as e:
                extra["itn_err"] = f"{type(e).__name__}: {e}"
        return raw, el, extra


class SherpaOfflineSVEngine:
    """sherpa_onnx 自带的**离线** SenseVoice(ONNX):和现用流式模型同一套运行时,
    不用 torch、CPU 就能跑 —— 装机成本最小的那条路。权重与 funasr 的 SenseVoiceSmall 同源。"""
    vram = "CPU(不占显存)"

    def __init__(self, threads=4, itn=True, provider="cpu"):
        import sherpa_onnx
        if not os.path.isdir(SENSE_VOICE_ONNX_DIR):
            raise RuntimeError(f"缺权重目录 {SENSE_VOICE_ONNX_DIR}(先跑 --download 或等下载完)")
        models = [x for x in sorted(glob.glob(os.path.join(SENSE_VOICE_ONNX_DIR, "model*.onnx")))
                  if os.path.getsize(x) > 1024 * 1024]
        if not models:
            raise RuntimeError(f"{SENSE_VOICE_ONNX_DIR} 里还没有可用的 model*.onnx(0 字节=没下完)")
        m = [x for x in models if "int8" in os.path.basename(x)] or models
        self.rec = sherpa_onnx.OfflineRecognizer.from_sense_voice(
            model=m[0], tokens=os.path.join(SENSE_VOICE_ONNX_DIR, "tokens.txt"),
            num_threads=threads, use_itn=itn, provider=provider, language="auto")
        self.name = f"sherpa-offline-SenseVoice({os.path.basename(m[0])}/{provider}/{threads}线程)"

    def transcribe(self, x):
        st = self.rec.create_stream()
        t0 = time.perf_counter()
        st.accept_waveform(RATE, x)
        self.rec.decode_stream(st)
        el = time.perf_counter() - t0
        return st.result.text, el, {}


class SenseVoiceEngine:
    """funasr SenseVoiceSmall:非流式、一次吃整段。use_itn 打开时中文数字会被规整成阿拉伯数字。"""
    name = "funasr-SenseVoiceSmall(离线)"
    vram = "见总结"

    def __init__(self, device="cuda:0", itn=True):
        from funasr import AutoModel
        self.itn = itn
        self.model = AutoModel(model=self.path(), device=device, disable_update=True, disable_pbar=True)

    @staticmethod
    def path():
        p = need("sensevoice")
        if not p:
            raise RuntimeError("找不到 SenseVoiceSmall 权重,先跑 --download")
        return p

    def transcribe(self, wav):
        t0 = time.perf_counter()
        res = self.model.generate(input=wav, cache={}, language="auto", use_itn=self.itn,
                                  batch_size_s=60, merge_vad=False)
        el = time.perf_counter() - t0
        raw = res[0]["text"] if res else ""
        clean = re.sub(r"<\|[^|]*\|>", "", raw).strip()
        extra = {} if clean == raw.strip() else {"raw": raw.strip()}
        return clean, el, extra


class WhisperEngine:
    """faster-whisper large-v3-turbo(CT2)。"""

    def __init__(self, device="cuda", compute_type="float16"):
        from faster_whisper import WhisperModel
        p = need("whisper")
        if not p:
            raise RuntimeError("找不到 faster-whisper 权重,先跑 --download")
        self.device, self.compute_type = device, compute_type
        self.model = WhisperModel(p, device=device, compute_type=compute_type)
        self.name = f"faster-whisper-large-v3-turbo(离线/{compute_type})"
        self.vram = "见总结"

    def transcribe(self, wav):
        t0 = time.perf_counter()
        segs, _info = self.model.transcribe(wav, **WHISPER_KW)
        text = "".join(s.text for s in segs).strip()
        el = time.perf_counter() - t0
        return text, el, {}


# ---------------------------------------------------------------- 素材
# 已知内容(合成素材才有;真人录音只能靠人耳朵判)
KNOWN = {
    "zh-en.wav": "我们现在就在微信里面说一句英文的 TOKEN 看看它还在不在 TOKEN",
    "code.wav": "我的验证码是九零三七二一(=903721)",
    "digits.wav": "一二三四五",
}


def tag_of(path):
    """素材标签:合成音要标出来,别和真人录音混为一谈。"""
    b = os.path.basename(path)
    p = path.replace("\\", "/")
    if "corpus" in p:
        return "真人"
    if b in KNOWN:
        kind = {"zh-en.wav": "中英混说", "code.wav": "验证码", "digits.wav": "数字"}[b]
        return f"合成音/{kind}"
    return "合成音"


def default_wavs():
    """默认素材:logs/corpus/ 真人录音(按文件名=时间排序)+ tmp/ 三句已知内容合成音。"""
    out = []
    for w in sorted(glob.glob(os.path.join(ROOT, "logs", "corpus", "*.wav"))):
        out.append((w, tag_of(w)))
    for n in ("zh-en.wav", "code.wav", "digits.wav"):
        w = os.path.join(ROOT, "tmp", n)
        if os.path.exists(w):
            out.append((w, tag_of(w)))
    return out


def load_wav(p):
    x, sr = sf.read(p, dtype="float32")
    if x.ndim > 1:
        x = x.mean(axis=1)
    if sr != RATE:
        idx = (np.arange(int(len(x) * RATE / sr)) * sr / RATE).astype(np.int64)
        x = x[np.clip(idx, 0, len(x) - 1)]
    return x


def gpu_used_mb():
    """整卡已用显存(MiB)。ctranslate2 / funasr 各有自己的分配器,torch 的 allocated 统计
    看不到 CT2 的占用,所以统一按「设备级已用」算增量。"""
    try:
        import torch
        if torch.cuda.is_available():
            free, total = torch.cuda.mem_get_info()
            return (total - free) / 2**20
    except Exception:
        pass
    return None


# ---------------------------------------------------------------- 主流程
def bench_engine(eng, items, args):
    """跑单个引擎,返回 {engine, rows, error, load_s, load_mb, peak_mb}。"""
    builders = {
        "sherpa": lambda: SherpaEngine(itn=not args.no_itn),
        "sensevoice-onnx": lambda: SherpaOfflineSVEngine(itn=not args.no_itn),
        "sensevoice": lambda: SenseVoiceEngine(args.device, not args.no_itn),
        "whisper": WhisperEngine,
    }
    if eng not in builders:
        return dict(engine=eng, rows=[], error=f"未知引擎 {eng}(可选 sherpa/sensevoice-onnx/sensevoice/whisper)",
                    load_s=None, load_mb=None, peak_mb=None)
    base = gpu_used_mb()
    try:
        t0 = time.perf_counter()
        e = builders[eng]()
        load_s = time.perf_counter() - t0
    except Exception as ex:
        msg = f"加载失败: {type(ex).__name__}: {ex}"
        print(f"\n########## {eng}  跳过 —— {msg}", flush=True)
        return dict(engine=eng, rows=[], error=msg, load_s=None, load_mb=None, peak_mb=None)

    after = gpu_used_mb()
    load_mb = None if (base is None or after is None) else after - base
    peak_mb = load_mb
    vt = "显存 n/a" if load_mb is None else f"显存 载入+{load_mb/1024:.2f}GB"
    print(f"\n########## {e.name}   载入 {load_s:.1f}s   {vt}", flush=True)

    rows = []
    for w, tag in items:
        base_n = os.path.basename(w)
        try:
            x = load_wav(w)
            dur = len(x) / RATE
            # 两个 sherpa 引擎吃 numpy 样本;funasr / faster-whisper 自己读文件
            arg = x if eng in ("sherpa", "sensevoice-onnx") else w
            text, el, extra = e.transcribe(arg)
            rtf = el / dur if dur else float("nan")
            now = gpu_used_mb()
            if now is not None and base is not None:
                peak_mb = max(peak_mb or 0, now - base)
            rows.append(dict(engine=eng, model=e.name, wav=base_n, tag=tag, dur=round(dur, 2),
                             rtf=round(rtf, 4), text=text, **extra))
            print(f"  {base_n:28s}[{tag}] dur={dur:5.2f}s RTF={rtf:6.3f}  {text}"
                  + (f"   |ITN: {extra['itn']}" if "itn" in extra else "")
                  + (f"   |raw: {extra['raw']}" if "raw" in extra else ""), flush=True)
        except Exception as ex:
            msg = " ".join(str(ex).split())[:180]
            print(f"  {base_n:28s}[{tag}] 失败: {type(ex).__name__}: {msg}", flush=True)
            rows.append(dict(engine=eng, model=getattr(e, "name", eng), wav=base_n, tag=tag,
                             error=f"{type(ex).__name__}: {msg}"))
        r = rows[-1]
        if not r.get("error"):
            r["load_s"], r["load_mb"], r["peak_mb"] = round(load_s, 1), load_mb, peak_mb
    return dict(engine=eng, rows=rows, error=None, load_s=round(load_s, 1), load_mb=load_mb, peak_mb=peak_mb)


def main():
    ap = argparse.ArgumentParser(description="多引擎 ASR 对比打表(sherpa 流式 / sherpa 离线 SenseVoice / funasr / faster-whisper)")
    ap.add_argument("--wavs", nargs="*", default=None, help="指定 wav;不给则用 logs/corpus + tmp 三句")
    ap.add_argument("--engines", default="sherpa,sensevoice-onnx,sensevoice,whisper")
    ap.add_argument("--json", default=None, help="把结果额外落一份 json")
    ap.add_argument("--download", action="store_true", help="只下载缺失权重然后退出")
    ap.add_argument("--device", default="cuda:0", help="sensevoice(funasr) 设备,CPU 用 cpu")
    ap.add_argument("--no-itn", action="store_true", help="关闭逆文本规整(SenseVoice/sherpa 都保留「九零三七二」原样)")
    ap.add_argument("--no-split", action="store_true", help="同一进程里串跑所有引擎(默认每个引擎单独起进程,见下)")
    ap.add_argument("--child", action="store_true", help="内部用:子进程只跑一个引擎并把结果写进 --json")
    args = ap.parse_args()

    if args.download:
        for tag, fn in (("SenseVoiceSmall(funasr)", download_sensevoice),
                        ("sherpa-onnx-SenseVoice", download_sensevoice_onnx),
                        ("faster-whisper-turbo", download_whisper)):
            t0 = time.perf_counter()
            print(f"[下载] {tag}: {fn()}  ({time.perf_counter()-t0:.0f}s)", flush=True)
        return

    if args.wavs:
        items = [(w if os.path.isabs(w) else os.path.join(ROOT, w), tag_of(w)) for w in args.wavs]
    else:
        items = default_wavs()

    if not args.child:
        print(f"########## 素材 {len(items)} 条(真人=logs/corpus 真实录音;合成音=tmp/ 下 SAPI 拼的,标了内容已知)")
        for w, tag in items:
            x = load_wav(w)
            truth = KNOWN.get(os.path.basename(w))
            print(f"  {os.path.basename(w):28s}[{tag}] {len(x)/RATE:5.2f}s"
                  + (f"  参考: {truth}" if truth else ""), flush=True)

    want = [e.strip() for e in args.engines.split(",") if e.strip()]
    # 每个引擎单独起一个进程:torch(cu128)+ctranslate2+sherpa_onnx 挤在同一进程里,
    # 后加载的那个会 mkl_malloc: Failed to allocate memory(实测 whisper 排在 funasr 之后起不来),
    # 显存统计也会被上一个引擎污染。--no-split 才是同进程串跑。
    split = len(want) > 1 and not args.no_split and not args.child
    if split:
        summaries = []
        for eng in want:
            tmp_json = os.path.join(ROOT, "tmp", f"asr_bench_{eng}.json")
            cmd = [sys.executable, os.path.abspath(__file__), "--child", "--engines", eng,
                   "--json", tmp_json, "--wavs"] + [w for w, _ in items]
            if args.no_itn:
                cmd.append("--no-itn")
            if args.device != "cuda:0":
                cmd += ["--device", args.device]
            rc = subprocess.run(cmd).returncode
            if os.path.exists(tmp_json):
                with open(tmp_json, encoding="utf-8") as f:
                    summaries.append(json.load(f))
            else:
                summaries.append(dict(engine=eng, rows=[], load_s=None, load_mb=None, peak_mb=None,
                                      error=f"子进程退出码 {rc},没产出 json"))
    else:
        summaries = [bench_engine(eng, items, args) for eng in want]

    if args.child:   # 子进程:结果进 json,父进程拼表
        if args.json:
            with open(args.json, "w", encoding="utf-8") as f:
                json.dump(summaries[0], f, ensure_ascii=False, indent=1)
        return

    rows = [r for s in summaries for r in s["rows"]]
    print("\n########## 汇总")
    for s in summaries:
        eng = s["engine"]
        rr = [r for r in s["rows"] if "rtf" in r]
        lm, pm = s.get("load_mb"), s.get("peak_mb")
        vt = "显存 n/a" if lm is None else f"显存 载入+{lm/1024:.2f}GB 峰值+{pm/1024:.2f}GB"
        if not rr:
            print(f"  {eng:16s} 无有效结果 {s.get('error') or ''}  {vt}")
            continue
        print(f"  {eng:16s} 平均RTF={sum(r['rtf'] for r in rr)/len(rr):.3f}  "
              f"最慢RTF={max(r['rtf'] for r in rr):.3f}  条数={len(rr)}  {vt}")
    if args.json:
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(rows, f, ensure_ascii=False, indent=1)
        print(f"  json -> {args.json}")


if __name__ == "__main__":
    main()
