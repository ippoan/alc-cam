# alc-cam.msi の検査 (CI の windows job)。中身の表を読んだあと、この PC の今のユーザーに実際に入れて
# 自己更新の書き換え → 修復しても戻らない → 新しい版の MSI で上がる → 古い MSI では下がらない → 外すと全部消える、を確かめる。
#   pwsh scripts/check-msi.ps1 -Msi dist/alc-cam.msi -Exe dist/alc-cam-host.exe -Version 0.4.0
param([Parameter(Mandatory)][string]$Msi, [Parameter(Mandatory)][string]$Exe, [Parameter(Mandatory)][string]$Version)

$ErrorActionPreference = 'Stop'
$Msi = (Resolve-Path $Msi).Path
$bad = 0
function Check([bool]$ok, [string]$what) {
    if ($ok) { Write-Host "ok   $what" } else { Write-Host "::error::$what"; $script:bad++ }
}

# --- 表 (Windows Installer の COM で読む) ---
$wi = New-Object -ComObject WindowsInstaller.Installer
function Call($o, [string]$m, $a) { $o.GetType().InvokeMember($m, 'InvokeMethod', $null, $o, $a) }
function Prop($o, [string]$m, $a) { $o.GetType().InvokeMember($m, 'GetProperty', $null, $o, $a) }
$db = Call $wi 'OpenDatabase' @($Msi, 0)
function Rows([string]$sql, [int]$cols) {
    try { $v = Call $db 'OpenView' @($sql) } catch { return @() }  # 表が無い
    [void](Call $v 'Execute' $null)
    $out = @()
    while ($r = Call $v 'Fetch' $null) { $out += , @(1..$cols | ForEach-Object { Prop $r 'StringData' @([int]$_) }) }
    [void](Call $v 'Close' $null)
    return , $out
}
$props = @{}
foreach ($r in Rows 'SELECT `Property`, `Value` FROM `Property`' 2) { $props[$r[0]] = $r[1] }
$words = Prop (Prop $db 'SummaryInformation' @(0)) 'Property' @(15)
Check (-not $props['ALLUSERS'] -and ($words -band 8)) "perUser (ALLUSERS 無し・昇格不要の印。Word Count=$words)"
Check ($props['ProductVersion'] -eq $Version) "ProductVersion = $Version ($($props['ProductVersion']))"
Check ($props['ARPNOREPAIR'] -eq '1' -and $props['ARPNOMODIFY'] -eq '1') 'アプリと機能に修復・変更を出さない'

$names = @(Rows 'SELECT `FileName` FROM `File`' 1 | ForEach-Object { ($_[0] -split '\|')[-1] })
$want = @('alc-cam-host.exe', 'org.ippoan.alc_cam_host.json') + @(Get-ChildItem -Recurse -File extension | ForEach-Object Name)
Check (-not (Compare-Object ($names | Sort-Object) ($want | Sort-Object))) "ファイル = host・manifest・拡張の $((Get-ChildItem -Recurse -File extension).Count) 個 ($($names.Count) 個)"

$reg = Rows 'SELECT `Root`, `Key`, `Name`, `Value` FROM `Registry`' 4
Check (@($reg | Where-Object { $_[0] -ne '1' }).Count -eq 0) 'レジストリは全部 HKCU'
Check (@($reg | Where-Object { $_[1] -eq 'Software\Google\Chrome\NativeMessagingHosts\org.ippoan.alc_cam_host' -and -not $_[2] -and $_[3] -eq '[#HostManifest]' }).Count -eq 1) 'native messaging の登録'

$comps = Rows 'SELECT `Component`, `Attributes`, `Directory_` FROM `Component`' 3
Check (@($comps | Where-Object { -not ([int]$_[1] -band 4) }).Count -eq 0) '部品の KeyPath は全部レジストリ'
Check (@($comps | Where-Object { $_[0] -ne 'Registration' -and -not ([int]$_[1] -band 128) }).Count -eq 0) 'exe と拡張の部品は NeverOverwrite'
Check ((Rows 'SELECT `Shortcut` FROM `Shortcut`' 1).Count -eq 0) 'ショートカット (広告) なし'
Check (@(Rows 'SELECT `Action`, `Target` FROM `CustomAction`' 2 | Where-Object { $_[1] -match 'powershell' }).Count -eq 0) 'PowerShell の custom action なし'
[void][Runtime.InteropServices.Marshal]::ReleaseComObject($db)
$db = $null
[GC]::Collect(); [GC]::WaitForPendingFinalizers()

# --- 実際に入れる ---
$dir = Join-Path $env:LOCALAPPDATA 'alc-cam'
$hostExe = Join-Path $dir 'alc-cam-host.exe'
$extDir = Join-Path $dir 'extension'
$nm = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\org.ippoan.alc_cam_host'
$mark = 'HKCU:\Software\ippoan\alc-cam\Installed'
function Msiexec([string[]]$a, [string]$name) {
    $log = Join-Path ([IO.Path]::GetTempPath()) "alc-cam-$name.log"
    $p = Start-Process msiexec.exe -ArgumentList ($a + @('/qn', '/l*v', "`"$log`"")) -Wait -PassThru
    if ($p.ExitCode -notin 0, 1603) { Get-Content $log -Tail 40 | Write-Host }
    return $p.ExitCode
}
$sha = { param($p) (Get-FileHash -Algorithm SHA256 -Path $p).Hash }

Check ((Msiexec @('/i', "`"$Msi`"") 'install') -eq 0) '入れる'
Check ((& $sha $hostExe) -eq (& $sha $Exe)) "host = $hostExe"
Check (Test-Path (Join-Path $extDir 'manifest.json')) "拡張 = $extDir"
$jsonPath = Join-Path $dir 'org.ippoan.alc_cam_host.json'
$b = [IO.File]::ReadAllBytes($jsonPath)
Check (-not ($b[0] -eq 0xEF -and $b[1] -eq 0xBB -and $b[2] -eq 0xBF)) 'manifest は BOM なし'
$j = [Text.Encoding]::UTF8.GetString($b) | ConvertFrom-Json
Check ($j.path -eq 'alc-cam-host.exe' -and $j.allowed_origins[0] -eq 'chrome-extension://icmmpdjddlphkbokdalofdjpkcgfiggd/') 'manifest の path・allowed_origins'
Check ((Get-ItemProperty $nm).'(default)' -eq $jsonPath) "HKCU の登録 → $jsonPath"

# 自己更新の書き換えを真似る (update.rs と同じ名前の残りも作る)
Rename-Item $hostExe 'alc-cam-host.exe.old'
Set-Content -Path $hostExe -Value 'self-updated'
Set-Content -Path (Join-Path $extDir 'manifest.json') -Value '{"version":"9.9.9"}'
Set-Content -Path (Join-Path $extDir 'added.js') -Value '//'
New-Item -ItemType Directory -Force (Join-Path $dir 'extension.old-123') | Out-Null
$updated = & $sha $hostExe

Check ((Msiexec @('/fa', "`"$Msi`"") 'repair') -eq 0) '修復 (msiexec /fa = 全部入れ直し) が通る'
Check ((& $sha $hostExe) -eq $updated) '修復で host が戻らない'
Check ((Get-Content -Raw (Join-Path $extDir 'manifest.json')) -match '9\.9\.9') '修復で拡張が戻らない'
Check ((Msiexec @('/i', "`"$Msi`"") 'again') -eq 0) '同じ MSI をもう一度 (保守モード)'
Check ((& $sha $hostExe) -eq $updated) '同じ MSI の入れ直しで host が戻らない'

# 新しい版の MSI (中身は同じで版だけ上げる) で上げる
$v = [version]$Version
$up = "$($v.Major).$($v.Minor).$($v.Build + 1)"
$upMsi = Join-Path ([IO.Path]::GetTempPath()) 'alc-cam-up.msi'
& (Join-Path $PSScriptRoot 'build-msi.ps1') -Version $up -Exe $Exe -Out $upMsi
Check ((Msiexec @('/i', "`"$upMsi`"") 'upgrade') -eq 0) "$up の MSI で上げる"
Check ((& $sha $hostExe) -eq (& $sha $Exe)) '上げると host が MSI の物になる'
Check (-not (Test-Path (Join-Path $extDir 'added.js')) -and -not (Test-Path "$hostExe.old") -and -not (Test-Path (Join-Path $dir 'extension.old-123'))) '上げると前の版の残り (.old・足されたファイル) が消える'
Check ($null -ne (Get-ItemProperty $mark -Name "host $up" -ErrorAction SilentlyContinue) -and $null -eq (Get-ItemProperty $mark -Name "host $Version" -ErrorAction SilentlyContinue)) "入れた印が $up だけ"
Check ((Msiexec @('/i', "`"$Msi`"") 'downgrade') -ne 0) "$Version の MSI では下げられない"
Check ((& $sha $hostExe) -eq (& $sha $Exe) -and (Test-Path (Join-Path $extDir 'manifest.json'))) '下げ損ねても今の版のまま'

# 外す (自己更新の残りも含めて全部消える)
New-Item -ItemType Directory -Force (Join-Path $dir 'extension.new-456\lib') | Out-Null
Set-Content -Path "$hostExe.old-789" -Value 'x'
Check ((Msiexec @('/x', "`"$upMsi`"") 'uninstall') -eq 0) '外す'
Check (-not (Test-Path $dir)) "$dir が消える"
Check (-not (Test-Path $nm) -and -not (Test-Path 'HKCU:\Software\ippoan\alc-cam')) 'HKCU の登録と印が消える'

if ($bad) { throw "MSI の検査で $bad 件の失敗" }
