#Requires -Version 5.1
<#
.SYNOPSIS
  ime_repair.ps1 —— 只修 ouuutput **自己**的 TSF 薄壳留在用户级输入法设置里的残留。
  用户的日常输入法与它无关,本脚本绝不碰。

.DESCRIPTION
  为什么会有残留:我们的 TSF 薄壳 ouuutput_tsf.dll 在做组合态实验时,
  `ActivateProfile(..., TF_IPPMF_FORPROCESS)` 会**连带把该语言的默认输入法改成自己**
  (crates/ouuutput-tsf-host/src/profile.rs 有记录;PLAN-NEXT 的 R1 判决据此把 TSF 降级为
  "可选模式、产品不主动激活")。残留只会落在两处**用户级(HKCU)**设置里:
    1) HKCU\Control Panel\International\User Profile\InputMethodOverride
       —— 默认输入法覆盖;Get-WinDefaultInputMethodOverride 读的就是它
    2) zh-Hans-CN 用户语言列表里的那条 tip(Get-WinUserLanguageList)
  后果:中文语境下激活的是那个壳,注入的字母被当成本地输入法的输入吃掉、整句丢失。

  本项目认识的"我们"**只有一个**(仓库里的权威常量):
    CLSID   {FCC45A5E-09C4-410A-B696-BDD2DA66A21F}   crates/ouuutput-tsf/src/lib.rs:92 (CLSID_OUUUTPUT_TSF)
    profile {6A1F2B7C-3D4E-4F50-9A1B-2C3D4E5F6071}   crates/ouuutput-tsf-host/src/profile.rs:23 (PROFILE_GUID)
    (tools/tsf_register.ps1 里的默认值与此同源)

  [!] 身份闸 —— 这是这次事故的全部教训:
      -Fix / -Register 动手之前**必须先确认**目标 CLSID 的 InProcServer32 指向的是**我们的 DLL**
      (路径里含 "ouuutput")。**对不上就硬拒绝、一个字都不改**,绝不"猜它是我们的"。
      反面教材:本机默认输入法其实是另一套**独立第三方输入法** Metasequoia 水杉输入法
      (CLSID {E3062E9A-D834-4637-8958-ED8CFA427D01},
       DLL C:\WINDOWS\system32\IME\MetasequoiaIME\msime_v0.11.1\MetasequoiaImeTsf.dll)。
      那是**用户自己在用的**输入法,不是我们的残留 —— 身份闸就是用来挡住"把它当成我们的删掉"。
      本机当前的 zh-CN 默认就是它,且我们的 TSF 根本没注册;因此 -Check 会直接说明
      "目标不是我们、身份闸不通过",-Fix / -Register 会被拒绝。

  边界:只写 HKCU 的上述两项,不碰 HKLM、不碰其它任何系统位置;幂等;先报告后动手;可回退。

.PARAMETER Check
  默认动作。只报告当前状态(含身份闸结论),一个字都不改。

.PARAMETER Fix
  清**我们自己的**残留,两步:
    1) 清默认输入法覆盖(先打印原值 + 落盘备份 → 删 HKCU 的 InputMethodOverride 值 →
       立刻用 Get-WinDefaultInputMethodOverride 复读核验,必要时回落到
       Set-WinDefaultInputMethodOverride -InputTip "")
    2) 把目标 tip 从用户语言列表里移除(先读出**整份**列表并打印 → 只删这一条 →
       Set-WinUserLanguageList -Force → 复读核验)。别处语言 / 别家 tip 一个不丢。
  身份闸不通过 → 直接拒绝,什么都不改(exit 5)。
  安全闸:若删完会让那个语言一条 tip 都不剩,默认**拒绝**并说明原因
  (那等于把用户的中文输入整个删掉)。确实要这么做得显式加 -AllowEmptyLanguage。
  最后重打一次全部状态。

.PARAMETER Register
  反向操作:把**我们自己的** tip 加回 zh-Hans-CN 语言列表(若不在),并把默认覆盖设回它
  —— 给以后真要跑 TSF 组合态时用。同样先过身份闸。

.PARAMETER AllowEmptyLanguage
  允许 -Fix 把一个语言的 tip 删空。默认关(保护性)。

.PARAMETER DryRun
  只演练:把 -Fix / -Register 会做的事打印出来,一个字都不写。

.EXAMPLE
  powershell -File tools/ime_repair.ps1 -Check
  powershell -File tools/ime_repair.ps1 -Fix
  powershell -File tools/ime_repair.ps1 -Register
#>
[CmdletBinding()]
param(
  [switch]$Check,
  [switch]$Fix,
  [switch]$Register,

  # **我们自己的** TSF(仓库权威常量;改这里前先改 crates/ouuutput-tsf/src/lib.rs)。
  [string]$TipClsid     = '{FCC45A5E-09C4-410A-B696-BDD2DA66A21F}',
  [string]$TipProfile   = '{6A1F2B7C-3D4E-4F50-9A1B-2C3D4E5F6071}',
  [string]$LangId       = '0804',
  [string]$LangTag      = 'zh-Hans-CN',

  # 身份闸:目标 CLSID 的 InProcServer32 必须匹配这条正则,才算"是我们的"。
  [string]$OwnDllPattern = 'ouuutput',

  # 允许把一个语言的 tip 删空(默认关)。
  [switch]$AllowEmptyLanguage,

  # 只演练:把 -Fix / -Register 会做的事打印出来,但一个字都不写。
  [switch]$DryRun,

  # 备份文件落这里(默认 TEMP,不往仓库里扔状态文件)。
  [string]$BackupDir = ''
)

$ErrorActionPreference = 'Stop'

$UserProfileKey = 'HKCU:\Control Panel\International\User Profile'
$Tip            = "$LangId`:$TipClsid$TipProfile"
# '0804' -> '0x00000804'(注册表里 LanguageProfile 的键名就是这种写法)
$LangIdKey      = '0x' + [Convert]::ToInt32($LangId, 16).ToString('X8')

if (-not $BackupDir) { $BackupDir = $env:TEMP }
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'

# 本机已知的"别人的输入法"样例:仅用于在报告里点名提醒,不参与任何判定。
$KnownForeignClsid = '{E3062E9A-D834-4637-8958-ED8CFA427D01}'

# ── 小工具 ────────────────────────────────────────────────────────────────────

function Say([string]$s = '') { Write-Output $s }
function Rule([string]$title) { Say ''; Say ("==== " + $title + " ====") }

function Test-Admin {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Read-Override {
  # Get-WinDefaultInputMethodOverride 未设置时返回 $null 或空串,两种都归一成 ''
  try {
    $o = Get-WinDefaultInputMethodOverride -ErrorAction Stop
    if ($null -eq $o) { return '' }
    return [string]$o.InputMethodTip
  } catch {
    return ''
  }
}

function Get-Prop([string]$Path, [string]$Name) {
  if (-not (Test-Path -LiteralPath $Path)) { return '' }
  $v = Get-ItemProperty -LiteralPath $Path -Name $Name -ErrorAction SilentlyContinue
  if ($null -eq $v) { return '' }
  $val = $v.$Name
  if ($null -eq $val) { return '' }
  return [string]$val
}

function Get-TipInfo([string]$Clsid, [string]$Profile, [string]$LangIdKey) {
  $tipKey  = "HKLM:\SOFTWARE\Microsoft\CTF\TIP\$Clsid"
  $comKey  = "HKLM:\SOFTWARE\Classes\CLSID\$Clsid"
  $profKey = "$tipKey\LanguageProfile\$LangIdKey\$Profile"
  [pscustomobject]@{
    TipKeyExists     = (Test-Path -LiteralPath $tipKey)
    ProfileKeyExists = (Test-Path -LiteralPath $profKey)
    DisplayName      = (Get-Prop $comKey '(default)')
    Description      = (Get-Prop $profKey 'Description')
    Dll              = (Get-Prop "$comKey\InProcServer32" '(default)')
  }
}

# 身份闸:目标 CLSID 的 InProcServer32 是否指向**我们的** DLL。
function Get-IdentityGate {
  $info = Get-TipInfo $TipClsid $TipProfile $LangIdKey
  $ok   = $false
  $why  = ''
  if (-not $info.TipKeyExists) {
    $why = "HKLM\SOFTWARE\Microsoft\CTF\TIP\$TipClsid 不存在 —— 这个 CLSID 根本没注册。"
  } elseif (-not $info.Dll) {
    $why = "读不到 $TipClsid 的 InProcServer32(DLL 路径为空)。"
  } elseif ($info.Dll -notmatch ('(?i)' + [regex]::Escape($OwnDllPattern))) {
    $why = ("InProcServer32 = '" + $info.Dll + "' —— 路径里没有 '" + $OwnDllPattern + "',这不是我们的 DLL。")
  } else {
    $ok  = $true
    $why = ("InProcServer32 含 '" + $OwnDllPattern + "',认定是我们自己的 TSF。")
  }
  [pscustomobject]@{ Ok = $ok; Reason = $why; Dll = $info.Dll; Info = $info }
}

function Dump-Languages($LangList) {
  $out = New-Object System.Collections.Generic.List[string]
  $i = 0
  foreach ($l in $LangList) {
    $tips = @($l.InputMethodTips)
    $out.Add(("[{0}] {1}  ({2} 条 tip)" -f $i, [string]$l.LanguageTag, $tips.Count))
    foreach ($t in $tips) { $out.Add("        $t") }
    $i++
  }
  return $out
}

function Show-Languages($LangList) {
  foreach ($line in (Dump-Languages $LangList)) { Say $line }
}

function Find-IndexByTag($LangList, [string]$Tag) {
  for ($i = 0; $i -lt @($LangList).Count; $i++) {
    if ([string]@($LangList)[$i].LanguageTag -eq $Tag) { return $i }
  }
  return -1
}

function Test-InList($LangList, [string]$Tip) {
  $hit = @()
  foreach ($l in $LangList) {
    if (@($l.InputMethodTips) -contains $Tip) { $hit += [string]$l.LanguageTag }
  }
  return $hit
}

# ── 状态报告(Check / Fix 末尾 / Register 末尾都用它)──────────────────────────

function Show-Status {
  $ov      = Read-Override
  $langs   = Get-WinUserLanguageList
  $info    = Get-TipInfo $TipClsid $TipProfile $LangIdKey
  $gate    = Get-IdentityGate
  $holders = @(Test-InList $langs $Tip)

  Rule '1) 默认输入法覆盖 (HKCU\Control Panel\International\User Profile\InputMethodOverride)'
  Say ("  Get-WinDefaultInputMethodOverride = " + $(if ($ov) { $ov } else { '<空 / 未设置>' }))
  if ($ov -eq $Tip) {
    Say '  -> 是我们的 tip(残留在这里)'
  } elseif ($ov) {
    Say '  -> 不是我们的 tip(指向别家输入法;本脚本不动它)'
  } else {
    Say '  -> 未设置'
  }

  Rule '2) 用户语言列表 (Get-WinUserLanguageList)'
  Show-Languages $langs
  if ($holders.Count -gt 0) {
    Say ("  -> 我们的 tip 出现在: " + ($holders -join ', '))
  } else {
    Say '  -> 我们的 tip 不在任何语言列表里'
  }

  Rule '3) 目标 CLSID 的注册身份 + 身份闸'
  Say ("  目标 CLSID : " + $TipClsid)
  Say ("  目标 tip   : " + $Tip)
  Say ("    HKLM\SOFTWARE\Microsoft\CTF\TIP\{0}                : {1}" -f $TipClsid, $(if ($info.TipKeyExists) { '存在' } else { '不存在' }))
  Say ("    ...\LanguageProfile\{0}\{1} : {2}" -f $LangIdKey, $TipProfile, $(if ($info.ProfileKeyExists) { '存在' } else { '不存在' }))
  Say ("    DisplayName     : " + $(if ($info.DisplayName) { $info.DisplayName } else { '<无>' }))
  Say ("    Description     : " + $(if ($info.Description) { $info.Description } else { '<无>' }))
  Say ("    InProcServer32  : " + $(if ($info.Dll) { $info.Dll } else { '<无>' }))
  if ($gate.Ok) {
    Say '  身份闸      : [通过] —— 目标是我们自己的 TSF'
  } else {
    Say '  身份闸      : [不通过] —— 目标不是我们自己的 TSF'
  }
  Say ("    理由      : " + $gate.Reason)

  Rule '4) 对照:仓库里写死的 ouuutput TSF 常量'
  Say ("  CLSID   {FCC45A5E-09C4-410A-B696-BDD2DA66A21F}   (crates/ouuutput-tsf/src/lib.rs:92)")
  Say ("  profile {6A1F2B7C-3D4E-4F50-9A1B-2C3D4E5F6071}   (crates/ouuutput-tsf-host/src/profile.rs:23)")
  if ($TipClsid -ieq $KnownForeignClsid) {
    Say ("  [!] 你传入的 -TipClsid " + $TipClsid + ' 是**别的输入法**(本机是 Metasequoia 水杉输入法),')
    Say '      不是我们的 TSF —— 身份闸会拒绝 -Fix / -Register。'
  }

  Rule '结论'
  if (-not $gate.Ok) {
    Say '  身份闸不通过:目标 CLSID 不是我们的 TSF。'
    Say '  因此 -Fix / -Register 会被**拒绝**,本脚本不会做任何修改。'
    Say '  你要修的是"我们自己的 TSF 残留"—— 当前系统里它根本没注册,也就没有残留可清。'
  } else {
    $dirty = ($ov -eq $Tip) -or ($holders.Count -gt 0)
    if ($dirty) {
      Say '  身份闸通过;状态:有我们自己的残留(默认覆盖 / 语言列表里有我们的 tip)。要清就 -Fix。'
    } else {
      Say '  身份闸通过;状态:干净 —— 我们的 tip 既不是默认,也不在语言列表里。'
    }
  }
}

# ── 动作 ──────────────────────────────────────────────────────────────────────

function Assert-UserScope {
  # 本脚本只写 HKCU / 用户语言列表;万一被用管理员跑,警告一下免得误会动了系统级设置。
  if (Test-Admin) {
    Say '  提示:当前是管理员会话。本脚本只写 HKCU(用户级),不需要提权;'
    Say '        但管理员上下文里的"用户"可能不是你日常登录的那个用户。'
  }
}

# 身份闸硬拒绝:-Fix / -Register 的第一道关,过了才可能动手。
function Assert-IdentityGate {
  $gate = Get-IdentityGate
  if ($gate.Ok) { return }
  Rule '身份闸:硬拒绝(动手之前,什么都没改)'
  Say ("  目标 CLSID     : " + $TipClsid)
  Say ("  InProcServer32 : " + $(if ($gate.Dll) { $gate.Dll } else { '<无>' }))
  Say ("  理由           : " + $gate.Reason)
  Say '  本脚本只修 ouuutput **自己**的 TSF 残留;目标不是我们的 → 拒绝执行。'
  Say '  用户日常在用的输入法(本机是 Metasequoia 水杉输入法)与它无关,不在本脚本职责内。'
  Say '  要修别的输入法请另行处理;本脚本不做"猜它是我们的"这种事。'
  exit 5
}

function Invoke-Fix {
  Assert-UserScope
  Assert-IdentityGate
  $origOv = Read-Override
  $langs  = Get-WinUserLanguageList

  # ── 预检:先把"要改什么"算出来;该拒绝的在动手之前拒绝 ──
  $hadValue = $false
  if (Test-Path -LiteralPath $UserProfileKey) {
    $p = Get-ItemProperty -LiteralPath $UserProfileKey -Name 'InputMethodOverride' -ErrorAction SilentlyContinue
    if ($null -ne $p) { $hadValue = $true }
  }
  $holders       = @(Test-InList $langs $Tip)
  $idx           = Find-IndexByTag $langs $LangTag
  $willEditLangs = ($holders.Count -gt 0 -and $idx -ge 0)
  $langBefore    = @()
  $langRemain    = @()
  if ($willEditLangs) {
    $langBefore = @($langs[$idx].InputMethodTips)
    $langRemain = @($langBefore | Where-Object { $_ -ne $Tip })
  }

  if ($willEditLangs -and $langRemain.Count -eq 0 -and -not $AllowEmptyLanguage) {
    Rule '安全闸:拒绝(动手之前,什么都没改)'
    Say  ("  " + $LangTag + ' 只有这一条 tip:')
    Say  ("    " + $Tip)
    Say  '  删掉它 = 这个语言一条输入法都不剩(= 把用户的中文输入整个删掉)。'
    Say  '  那不是"清残留",是改用户偏好 —— 默认拒绝。'
    Say  '  确实要这么干:加 -AllowEmptyLanguage;或先给该语言加一条别的输入法(如微软拼音)。'
    exit 3
  }

  if ($DryRun) { Rule 'DRY RUN —— 一个字都不会写' }

  # ── 备份(原默认覆盖 + 整份语言列表):落盘 + 打印 ──
  $backupPath = Join-Path $BackupDir "ime_repair_backup-$Stamp.txt"
  Rule '备份(改动前)'
  if ($DryRun) {
    Say ("  [DryRun] 将写入备份文件: " + $backupPath)
  } else {
    if (-not (Test-Path -LiteralPath $BackupDir)) {
      New-Item -ItemType Directory -Path $BackupDir -Force | Out-Null
    }
    $bk = New-Object System.Collections.Generic.List[string]
    $bk.Add("# ime_repair.ps1 备份  $Stamp")
    $bk.Add("# 目标 tip(我们自己的): $Tip")
    $bk.Add('# 回退:把 InputMethodOverride 写回下面的原值,并把这个 tip 加回语言列表')
    $bk.Add('#       (或直接跑: powershell -File tools/ime_repair.ps1 -Register)')
    $bk.Add('')
    $bk.Add('[原值] InputMethodOverride = ' + $(if ($origOv) { $origOv } else { '<空>' }))
    $bk.Add('[原值] Get-WinUserLanguageList:')
    foreach ($line in (Dump-Languages $langs)) { $bk.Add('    ' + $line) }
    Set-Content -LiteralPath $backupPath -Value ($bk -join "`r`n") -Encoding UTF8
    Say ("  文件: " + $backupPath)
  }
  Say ("  InputMethodOverride = " + $(if ($origOv) { $origOv } else { '<空 / 未设置>' }))
  Say  '  Get-WinUserLanguageList:'
  Show-Languages $langs

  $failures = 0

  # ── 步骤 1:默认输入法覆盖 ──
  Rule '步骤 1/2:清默认输入法覆盖'
  Say ("  改哪个: HKCU 的 " + $UserProfileKey + "  ->  值名 InputMethodOverride")
  Say ("  原值  : " + $(if ($origOv) { $origOv } else { '<空 / 未设置>' }))
  if (-not $hadValue) {
    Say '  动作  : 无 —— 本来就没有这个值(幂等)'
  } elseif ($DryRun) {
    Say '  动作  : [DryRun] 将 Remove-ItemProperty 删掉该值'
  } else {
    Remove-ItemProperty -LiteralPath $UserProfileKey -Name 'InputMethodOverride' -Force
    Say '  动作  : 已删除 HKCU 的 InputMethodOverride 值'
    if ((Read-Override) -eq $Tip) {
      Say "  复读  : 仍指向我们的 tip —— 回落到 Set-WinDefaultInputMethodOverride -InputTip (空)"
      try { Set-WinDefaultInputMethodOverride -InputTip "" -ErrorAction Stop } catch {
        Say ("          回落失败: " + $_.Exception.Message)
      }
    }
  }
  if ($DryRun) {
    Say '  核验  : [DryRun] 未改动;真实 -Fix 此步后会复读确认不再指向我们的 tip'
  } else {
    $afterOv = Read-Override
    Say ("  复读  : " + $(if ($afterOv) { $afterOv } else { '<空 / 未设置>' }))
    if ($afterOv -eq $Tip) {
      Say '  核验  : [FAIL] 失败 —— 默认覆盖仍指向我们的 tip'
      $failures++
    } else {
      Say '  核验  : [OK] 通过 —— 不再指向我们的 tip'
    }
  }

  # ── 步骤 2:用户语言列表 ──
  Rule '步骤 2/2:从用户语言列表移除我们的 tip'
  Say ("  改哪个: " + $LangTag + " 的 InputMethodTips(Set-WinUserLanguageList -Force)")
  if (-not $willEditLangs) {
    Say '  动作  : 无 —— 我们的 tip 本来就不在那个语言列表里(幂等)'
  } else {
    Say ("  原值  : " + ($langBefore -join ' , '))
    if ($DryRun) {
      Say ("  新值  : [DryRun] 将变成: " + $(if ($langRemain.Count) { ($langRemain -join ' , ') } else { '<空>' }))
    } else {
      $langs[$idx].InputMethodTips = $langRemain
      Set-WinUserLanguageList $langs -Force
      Say ("  新值  : " + $(if ($langRemain.Count) { ($langRemain -join ' , ') } else { '<空>' }))
    }
  }
  if ($DryRun) {
    Say '  核验  : [DryRun] 未改动;真实 -Fix 此步后会复读确认 tip 已不在任何语言列表里'
  } else {
    $afterHolders = @(Test-InList (Get-WinUserLanguageList) $Tip)
    Say ("  复读  : " + $(if ($afterHolders.Count) { '仍出现在 ' + ($afterHolders -join ', ') } else { '我们的 tip 已不在任何语言列表里' }))
    if ($afterHolders.Count -gt 0) {
      Say '  核验  : [FAIL] 失败 —— 我们的 tip 还在列表里'
      $failures++
    } else {
      Say '  核验  : [OK] 通过'
    }
  }

  Rule '改完之后(完整状态)'
  if ($DryRun) { Say '  [DryRun] 状态本应未变(仍为原样):' }
  Show-Status
  Rule '小结'
  if ($DryRun) {
    Say '  结果:[DryRun] 上面就是真实 -Fix 会做的事;本次未写任何东西。'
    exit 0
  }
  Say ("  失败步骤数: " + $failures)
  Say ("  备份文件  : " + $backupPath)
  if ($failures -gt 0) {
    Say '  结果:有步骤没成(见上面的"核验 [FAIL]")。原值在上面和备份文件里,可回退。'
    exit 4
  }
  Say '  结果:清干净了。'
  exit 0
}

function Invoke-Register {
  Assert-UserScope
  Assert-IdentityGate
  if ($DryRun) { Rule 'DRY RUN —— 一个字都不会写' }
  $origOv = Read-Override
  $langs  = Get-WinUserLanguageList
  $info   = Get-TipInfo $TipClsid $TipProfile $LangIdKey

  Rule '备份(改动前)'
  Say ("  InputMethodOverride = " + $(if ($origOv) { $origOv } else { '<空 / 未设置>' }))
  Say '  Get-WinUserLanguageList:'
  Show-Languages $langs

  $failures = 0

  Rule '步骤 1/2:把我们的 tip 加回用户语言列表'
  $idx = Find-IndexByTag $langs $LangTag
  if ($idx -lt 0) {
    Say ("  失败  : 用户语言列表里没有 " + $LangTag + ' 语言 —— 本脚本不替你新加语言(那会改用户偏好)。')
    Say '          请先在「设置 → 时间和语言 → 语言和区域」里把该语言加回来。'
    exit 2
  }
  $target = $langs[$idx]
  $before = @($target.InputMethodTips)
  Say ("  改哪个: " + $LangTag + " 的 InputMethodTips(Set-WinUserLanguageList -Force)")
  Say ("  原值  : " + ($before -join ' , '))
  if ($before -contains $Tip) {
    Say '  动作  : 无 —— 本来就在(幂等)'
  } elseif ($DryRun) {
    Say ("  新值  : [DryRun] 将变成: " + (@($before + $Tip) -join ' , '))
  } else {
    $target.InputMethodTips = @($before + $Tip)
    Set-WinUserLanguageList $langs -Force
    Say ("  新值  : " + (@($target.InputMethodTips) -join ' , '))
  }
  if ($DryRun) {
    Say '  核验  : [DryRun] 未改动'
  } else {
    $afterHolders = @(Test-InList (Get-WinUserLanguageList) $Tip)
    if ($afterHolders.Count -gt 0) {
      Say ("  核验  : [OK] 已回到 " + ($afterHolders -join ', '))
    } else {
      Say '  核验  : [FAIL] 失败 —— 复读列表里没有它'
      $failures++
    }
  }
  if (-not $info.TipKeyExists) {
    Say ("  [!] 注意:HKLM\SOFTWARE\Microsoft\CTF\TIP\$TipClsid 不存在 —— 这条 tip 只会是个")
    Say '          "点不动的摆设"。要它真能用,先跑 tools/tsf_register.ps1 -Action register。'
  }

  Rule '步骤 2/2:把默认输入法覆盖设回我们的 tip'
  Say ("  改哪个: " + $UserProfileKey + "  ->  InputMethodOverride(经 Set-WinDefaultInputMethodOverride)")
  Say ("  原值  : " + $(if ($origOv) { $origOv } else { '<空 / 未设置>' }))
  if ($origOv -eq $Tip) {
    Say '  动作  : 无 —— 本来指向它(幂等)'
  } elseif ($DryRun) {
    Say '  动作  : [DryRun] 将 Set-WinDefaultInputMethodOverride -InputTip <我们的 tip>'
  } else {
    Set-WinDefaultInputMethodOverride -InputTip $Tip
    Say '  动作  : 已设置'
  }
  if ($DryRun) {
    Say '  核验  : [DryRun] 未改动'
  } else {
    $afterOv = Read-Override
    Say ("  复读  : " + $(if ($afterOv) { $afterOv } else { '<空 / 未设置>' }))
    if ($afterOv -eq $Tip) {
      Say '  核验  : [OK] 通过'
    } else {
      Say '  核验  : [FAIL] 失败 —— 默认覆盖没有指向我们的 tip'
      $failures++
    }
  }

  Rule '改完之后(完整状态)'
  Show-Status
  Rule '小结'
  if ($DryRun) {
    Say '  结果:[DryRun] 上面就是真实 -Register 会做的事;本次未写任何东西。'
    exit 0
  }
  Say ("  失败步骤数: " + $failures)
  if ($failures -gt 0) { exit 4 }
  Say '  结果:已还原成"默认指向我们自己的 tip"。'
  exit 0
}

# ── 主流程:默认 -Check;一次只允许一个动作 ────────────────────────────────────

$actions = @()
if ($Check)    { $actions += 'Check' }
if ($Fix)      { $actions += 'Fix' }
if ($Register) { $actions += 'Register' }
if ($actions.Count -gt 1) {
  Say ("ERROR: 一次只能给一个动作,收到: " + ($actions -join ', '))
  exit 2
}
$action = if ($actions.Count -eq 1) { $actions[0] } else { 'Check' }

Rule "ime_repair.ps1 -$action"
Say '  定位:只修 ouuutput **自己**的 TSF 残留;用户的日常输入法与本脚本无关,绝不碰。'
Say '  只动: 默认输入法覆盖(InputMethodOverride)+ 用户语言列表里的那条 tip。只写 HKCU。'
Say ("  我们的 tip: " + $Tip)
if ($action -eq 'Check') {
  Say '  模式: 只读(什么都不改)。要清 → -Fix;要还原 → -Register。'
}

switch ($action) {
  'Check'    { Show-Status; exit 0 }
  'Fix'      { Invoke-Fix }
  'Register' { Invoke-Register }
}