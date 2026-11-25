#!/usr/bin/env python3
"""publish-r2.py - Upload ouuutput release archives to Cloudflare R2 and verify public delivery.

Environment variables (for CI / portable usage):
  OUUUT_CF_ACCOUNT_ID    Cloudflare account ID
  OUUUT_CF_EMAIL         Cloudflare account email (with OUUUT_CF_GLOBAL_KEY)
  OUUUT_CF_GLOBAL_KEY    Cloudflare Global API Key
  OUUUT_R2_API_TOKEN     Cloudflare R2 Bearer token (optional alternative to email+global key)
  OUUUT_R2_BUCKET        Target R2 bucket (default: ouuut-media)
  OUUUT_R2_PUBLIC_URL    Public base URL (default: https://media.ouuut.com)
  OUUUT_SECRETS_MANAGER  Optional path to local secrets_manager.py fallback
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

CF_API_HOST = "api.cloudflare.com"
CF_API_BASE = f"https://{CF_API_HOST}/client/v4"
USER_AGENT = "ouuutput-release/1.0"
REPO_ROOT = Path(__file__).resolve().parent.parent


def read_cargo_version(repo_root: Path = REPO_ROOT) -> str:
    cargo = (repo_root / "Cargo.toml").read_text(encoding="utf-8")
    m = re.search(r'(?m)^version\s*=\s*"([^"]+)"', cargo)
    if not m:
        raise RuntimeError(f"Cannot find workspace version in {repo_root / 'Cargo.toml'}")
    return m.group(1).strip()


def _find_secrets_manager() -> Path | None:
    env_path = os.environ.get("OUUUT_SECRETS_MANAGER", "").strip()
    if env_path:
        p = Path(env_path)
        return p if p.is_file() else None
    candidate = Path.home() / ".omp" / "agent" / "scripts" / "secrets_manager.py"
    return candidate if candidate.is_file() else None


def _read_secret(category: str, key: str) -> str:
    sm = _find_secrets_manager()
    if not sm:
        return ""
    proc = subprocess.run(
        [sys.executable, str(sm), "read", category, key],
        capture_output=True,
        text=True,
        timeout=20,
    )
    if proc.returncode != 0:
        return ""
    try:
        return str(json.loads(proc.stdout).get("value") or "").strip()
    except Exception:
        return ""


def get_r2_config() -> dict[str, str]:
    return {
        "account_id": os.environ.get("OUUUT_CF_ACCOUNT_ID", "").strip() or _read_secret("custom", "ouuut_cf_account_id"),
        "email": os.environ.get("OUUUT_CF_EMAIL", "").strip() or _read_secret("custom", "ouuut_cf_email"),
        "global_key": os.environ.get("OUUUT_CF_GLOBAL_KEY", "").strip() or _read_secret("custom", "ouuut_cf_global"),
        "api_token": os.environ.get("OUUUT_R2_API_TOKEN", "").strip(),
        "bucket": os.environ.get("OUUUT_R2_BUCKET", "").strip() or _read_secret("custom", "ouuut_r2_bucket") or "ouuut-media",
        "public_url": (
            os.environ.get("OUUUT_R2_PUBLIC_URL", "").strip()
            or _read_secret("custom", "ouuut_r2_public_url")
            or "https://media.ouuut.com"
        ).rstrip("/"),
    }


def _auth_headers(cfg: dict[str, str]) -> dict[str, str]:
    if cfg.get("api_token"):
        return {"Authorization": f"Bearer {cfg['api_token']}"}
    if cfg.get("email") and cfg.get("global_key"):
        return {"X-Auth-Email": cfg["email"], "X-Auth-Key": cfg["global_key"]}
    raise RuntimeError(
        "Missing Cloudflare R2 credentials. Set OUUUT_CF_ACCOUNT_ID + (OUUUT_R2_API_TOKEN or OUUUT_CF_EMAIL + OUUUT_CF_GLOBAL_KEY)."
    )


def file_sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def _stream_put_cf_api(
    fp: Path,
    clean_key: str,
    cfg: dict[str, str],
    ctype: str,
    cache_control: str,
    timeout: float = 900.0,
) -> tuple[int, str]:
    acct, bucket = cfg["account_id"], cfg["bucket"]
    if not acct:
        raise RuntimeError("Missing OUUUT_CF_ACCOUNT_ID / ouuut_cf_account_id")
    quoted_key = "/".join(urllib.parse.quote(seg, safe="") for seg in clean_key.split("/"))
    path = f"/client/v4/accounts/{acct}/r2/buckets/{bucket}/objects/{quoted_key}"
    size = fp.stat().st_size

    headers = {
        **_auth_headers(cfg),
        "Content-Type": ctype,
        "Content-Length": str(size),
        "Cache-Control": cache_control,
        "User-Agent": USER_AGENT,
    }

    conn = http.client.HTTPSConnection(CF_API_HOST, timeout=timeout)
    try:
        conn.putrequest("PUT", path)
        for k, v in headers.items():
            conn.putheader(k, v)
        conn.endheaders()

        sent = 0
        next_report = 25 * 1024 * 1024
        with fp.open("rb") as f:
            while True:
                buf = f.read(2 * 1024 * 1024)
                if not buf:
                    break
                conn.send(buf)
                sent += len(buf)
                if size >= 25 * 1024 * 1024 and sent >= next_report:
                    print(f"    ... uploaded {sent / 1024 / 1024:.1f} / {size / 1024 / 1024:.1f} MB", flush=True)
                    next_report += 25 * 1024 * 1024

        resp = conn.getresponse()
        body = resp.read().decode("utf-8", errors="replace")
        return resp.status, body
    finally:
        conn.close()


def _wrangler_r2_put(
    fp: Path,
    clean_key: str,
    cfg: dict[str, str],
    ctype: str,
    cache_control: str,
) -> None:
    env = dict(os.environ, CLOUDFLARE_ACCOUNT_ID=cfg["account_id"])
    env.pop("CLOUDFLARE_API_TOKEN", None)
    if cfg.get("api_token"):
        env["CLOUDFLARE_API_TOKEN"] = cfg["api_token"]
    else:
        env["CLOUDFLARE_EMAIL"] = cfg["email"]
        env["CLOUDFLARE_API_KEY"] = cfg["global_key"]
    cmd = [
        "npx",
        "--yes",
        "wrangler@4",
        "r2",
        "object",
        "put",
        f"{cfg['bucket']}/{clean_key}",
        "--file",
        str(fp),
        "--content-type",
        ctype,
        "--cache-control",
        cache_control,
        "--remote",
    ]
    proc = subprocess.run(cmd, env=env, shell=(os.name == "nt"), capture_output=True, text=True, timeout=900)
    if proc.returncode != 0:
        raise RuntimeError(f"wrangler r2 object put --remote failed ({proc.returncode}): {proc.stdout}\n{proc.stderr}")


def put_object(
    fp: Path,
    clean_key: str,
    cfg: dict[str, str],
    ctype: str,
    cache_control: str,
) -> str:
    size_mb = fp.stat().st_size / 1024 / 1024
    print(f"[r2] PUT {fp.name} ({size_mb:.2f} MB) -> {cfg['bucket']}/{clean_key}", flush=True)
    status, body = _stream_put_cf_api(fp, clean_key, cfg, ctype, cache_control)
    if status in (200, 201):
        return "cf_rest_api"
    print(f"[r2] warn: CF REST API returned HTTP {status}: {body[:240]}; falling back to wrangler --remote", flush=True)
    _wrangler_r2_put(fp, clean_key, cfg, ctype, cache_control)
    return "wrangler_r2_put_remote"


def verify_public_zip(
    url: str,
    expect_size: int,
    expect_sha256: str,
    *,
    full_sha: bool = False,
) -> dict[str, Any]:
    req_h = urllib.request.Request(url, method="HEAD", headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req_h, timeout=30) as resp:
        head_status = resp.status
        clen = int(resp.headers.get("Content-Length") or 0)
        ctype = resp.headers.get("Content-Type") or ""
        ar = resp.headers.get("Accept-Ranges") or ""
    if head_status != 200 or clen != expect_size:
        raise RuntimeError(f"Public R2 HEAD check failed for {url}: status={head_status}, len={clen} != {expect_size}")

    req_r = urllib.request.Request(
        url,
        method="GET",
        headers={"Range": "bytes=0-65535", "User-Agent": USER_AGENT},
    )
    with urllib.request.urlopen(req_r, timeout=30) as resp:
        range_status = resp.status
        first_chunk = resp.read()
    expected_range_len = min(65536, expect_size)
    if range_status != 206 or len(first_chunk) != expected_range_len:
        raise RuntimeError(f"Public R2 Range 206 check failed for {url}: status={range_status}, len={len(first_chunk)}")
    if not first_chunk.startswith(b"PK\x03\x04"):
        raise RuntimeError(f"Public R2 object does not start with ZIP magic PK\\x03\\x04: {first_chunk[:8]!r}")

    verified_sha = None
    if full_sha:
        print(f"[r2] verifying full download SHA-256 from {url} ...", flush=True)
        h = hashlib.sha256()
        total = 0
        req_g = urllib.request.Request(url, method="GET", headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(req_g, timeout=600) as resp:
            if resp.status != 200:
                raise RuntimeError(f"Full GET returned HTTP {resp.status} for {url}")
            while True:
                buf = resp.read(1024 * 1024)
                if not buf:
                    break
                h.update(buf)
                total += len(buf)
        verified_sha = h.hexdigest()
        if total != expect_size or verified_sha != expect_sha256:
            raise RuntimeError(
                f"Full download mismatch for {url}: bytes={total}/{expect_size}, sha={verified_sha}/{expect_sha256}"
            )

    return {
        "head_status": head_status,
        "content_length": clen,
        "content_type": ctype,
        "accept_ranges": ar,
        "range_status": range_status,
        "full_sha_verified": full_sha,
        "sha256": verified_sha or expect_sha256,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="Upload ouuutput portable zip to Cloudflare R2 and verify public URL.")
    ap.add_argument("--version", default="", help="Release version (default: derived from Cargo.toml)")
    ap.add_argument("--file", default="", help="Path to zip archive (default: dist/ouuutput-<ver>-win64.zip)")
    ap.add_argument("--key-prefix", default="ouuutput/releases", help="R2 key prefix (default: ouuutput/releases)")
    ap.add_argument("--no-latest", action="store_true", help="Do not update <key-prefix>/latest.json")
    ap.add_argument("--verify-full-sha", action="store_true", help="Download full public zip and verify SHA-256")
    ap.add_argument("--json", action="store_true", help="Print JSON summary on final line")
    args = ap.parse_args()

    ver = args.version.strip().lstrip("v") or read_cargo_version()
    zip_path = Path(args.file).resolve() if args.file else (REPO_ROOT / "dist" / f"ouuutput-{ver}-win64.zip").resolve()
    if not zip_path.is_file() or zip_path.stat().st_size == 0:
        raise FileNotFoundError(f"Release zip not found or empty: {zip_path}")

    cfg = get_r2_config()
    size = zip_path.stat().st_size
    sha = file_sha256(zip_path)
    prefix = args.key_prefix.strip("/")
    zip_key = f"{prefix}/v{ver}/{zip_path.name}"
    r2_url = f"{cfg['public_url']}/{zip_key}"
    gh_url = f"https://github.com/nakamotosai/ouuutput/releases/download/v{ver}/{zip_path.name}"

    method = put_object(zip_path, zip_key, cfg, "application/zip", "public, max-age=31536000, immutable")
    probe = verify_public_zip(r2_url, size, sha, full_sha=args.verify_full_sha)

    latest_url = ""
    if not args.no_latest:
        latest_key = f"{prefix}/latest.json"
        latest_payload = {
            "version": ver,
            "tag": f"v{ver}",
            "filename": zip_path.name,
            "r2_url": r2_url,
            "github_url": gh_url,
            "bytes": size,
            "sha256": sha,
            "updated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        with tempfile.TemporaryDirectory() as td:
            lp = Path(td) / "latest.json"
            lp.write_text(json.dumps(latest_payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            put_object(lp, latest_key, cfg, "application/json; charset=utf-8", "public, max-age=60")
        latest_url = f"{cfg['public_url']}/{latest_key}"
        req_l = urllib.request.Request(latest_url, method="HEAD", headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(req_l, timeout=20) as resp:
            if resp.status != 200:
                raise RuntimeError(f"latest.json HEAD check failed: {resp.status}")

    out = {
        "ok": True,
        "version": ver,
        "bucket": cfg["bucket"],
        "key": zip_key,
        "r2_url": r2_url,
        "github_url": gh_url,
        "latest_url": latest_url,
        "bytes": size,
        "sha256": sha,
        "method": method,
        "probe": probe,
    }
    if args.json:
        print(json.dumps(out, ensure_ascii=False))
    else:
        print(f"[r2] OK: {r2_url} ({size} bytes, sha256={sha})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
