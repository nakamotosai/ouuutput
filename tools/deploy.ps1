# 一条命令:编译 → 替换正在跑的那份 → 重启(接管旧实例)。**不需要用户先手动退出。**
#
# 为什么要这么写(踩过的坑,别简化):
#   1) Windows 不许**覆盖**正在运行的 exe / 已加载的 dll(实测报 `os error 5 拒绝访问`),
#      但**允许改名**。所以流程是:把在跑的 `sainput.exe` 改名腾位 → 再把新的拷到原路径。
#      在跑的那个进程照旧用改名后的镜像继续跑,新路径上已经是新版。
#   2) 编译**不能直接编到正在跑的那个目录**:cargo 换不上文件会以
#      `failed to remove file ...sainput.exe` 收尾 —— 那句看着像编译失败,其实是**换不上去**。
#      所以这里编到 staging(`target-build\...`),由本脚本负责搬运。
#   3) 重启用 `start.bat`(它带 `run --hide-console`)。**新实例会自己接管旧实例**
#      (引擎里的单实例逻辑:先请旧的退出,赖着不走才硬杀)—— 这正是"不用手动退出"的来源。
#
# 用法:
#   powershell -File tools/deploy.ps1                 # 编译 + 替换 + 重启(默认)
#   powershell -File tools/deploy.ps1 -SkipBuild      # 只用现有产物替换 + 重启
#   powershell -File tools/deploy.ps1 -NoRestart      # 只替换,不动正在跑的实例
#   powershell -File tools/deploy.ps1 -Staging <dir>  # 用别处的产物(默认 target-build)
param(
    [switch]$SkipBuild,
    [switch]$NoRestart,
    [string]$Staging = "target-build",
    [string]$Triple = "x86_64-pc-windows-gnu"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root

$stagingDir = Join-Path $root (Join-Path $Staging (Join-Path $Triple "release"))
$liveDir    = Join-Path $root (Join-Path "target" (Join-Path $Triple "release"))
# 需要搬运的三件:引擎本体、TSF 通道 dll(它像 exe 一样会被宿主进程加载锁住)、自检宿主
$artifacts  = @("sainput.exe", "sainput_tsf.dll", "sainput-tsf-host.exe")

if (-not $SkipBuild) {
    Write-Host "[deploy] 编译到 staging:$Staging(不碰正在跑的那份)" -ForegroundColor Cyan
    # CARGO_TARGET_DIR 指到 staging:cargo 永远不会尝试去覆盖 live 目录里被锁的文件
    $env:CARGO_TARGET_DIR = Join-Path $root $Staging
    # ⚠️ cargo 往 stderr 打 warning 是常态;PowerShell 5.1 会把原生命令的 stderr 当成
    # ErrorRecord,配上 `$ErrorActionPreference='Stop'` 就会**误判成失败**并中断脚本
    # (2026-11-01 实测:第一次跑就这样"假失败",好在它什么都没动)。
    # 所以这一小段单独放开,用真正的判据 —— `$LASTEXITCODE`。
    $eap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $out = (& cargo build --release 2>&1 | Out-String)
    $code = $LASTEXITCODE
    $ErrorActionPreference = $eap
    if ($code -ne 0) {
        ($out -split "`n" | Where-Object { $_ -match "^\s*error" } | Select-Object -First 8) |
            ForEach-Object { Write-Host $_ -ForegroundColor Red }
        Write-Host "[deploy] 编译失败(退出码 $code),没动任何东西" -ForegroundColor Red
        exit 1
    }
    ($out -split "`n" | Where-Object { $_ -match "Finished" } | Select-Object -First 1) |
        ForEach-Object { Write-Host "[deploy] $($_.Trim())" -ForegroundColor DarkGray }
}

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$copied = @()
foreach ($a in $artifacts) {
    $src = Join-Path $stagingDir $a
    if (-not (Test-Path $src)) { Write-Host "[deploy] staging 里没有 $a(跳过)" -ForegroundColor Yellow; continue }
    $dst = Join-Path $liveDir $a
    if (Test-Path $dst) {
        # 在跑/已加载 → 改名腾位(这一步 Windows 允许)。改出来的 .old-* 留作回退,
        # 下次部署时顺手清掉(那时候它对应的进程早退了,删得掉)。
        try { Move-Item -Force $dst "$dst.old-$stamp" }
        catch { Write-Host "[deploy] $a 改名失败:$($_.Exception.Message)" -ForegroundColor Red; continue }
    }
    Copy-Item -Force $src $dst
    $ok = (Get-FileHash $src).Hash -eq (Get-FileHash $dst).Hash
    Write-Host ("[deploy] {0}  ->  {1}" -f $a, $(if ($ok) { "OK" } else { "哈希不一致 ✗" })) -ForegroundColor $(if ($ok) { "Green" } else { "Red" })
    $copied += $a
}
if ($copied.Count -eq 0) { Write-Host "[deploy] 什么都没搬,检查 -Staging" -ForegroundColor Red; exit 2 }

# 清掉更早留下的 .old-*(此刻对应的进程已经退出,删得掉;删不掉就留着,不报错)
Get-ChildItem $liveDir -Filter "*.old-*" | Where-Object { $_.Name -notlike "*$stamp" } | ForEach-Object {
    try { Remove-Item -Force $_.FullName } catch {}
}

if ($NoRestart) { Write-Host "[deploy] 已替换,未重启(-NoRestart)。下次启动即新版。" -ForegroundColor Green; exit 0 }

$running = @(Get-CimInstance Win32_Process -Filter "Name='sainput.exe'" -ErrorAction SilentlyContinue)
Write-Host ("[deploy] 现在有 {0} 个实例在跑 → 启动新实例接管(引擎自己的单实例逻辑会请旧的退出)" -f $running.Count) -ForegroundColor Cyan
Start-Process -FilePath (Join-Path $root "start.bat") -WorkingDirectory $root -WindowStyle Hidden
Start-Sleep -Seconds 12
$after = @(Get-CimInstance Win32_Process -Filter "Name='sainput.exe'" -ErrorAction SilentlyContinue)
Write-Host ("[deploy] 装载完成,当前实例 pid {0}" -f (($after | ForEach-Object { $_.ProcessId }) -join ",")) -ForegroundColor Green
