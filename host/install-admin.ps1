# alc-cam の拡張を Chrome のポリシーで入れる (管理者で実行する。普段は install.ps1 が昇格して呼ぶ)。
#   powershell -ExecutionPolicy Bypass -File install-admin.ps1
#   powershell -ExecutionPolicy Bypass -File install-admin.ps1 -EnrollmentToken <token>
#   powershell -ExecutionPolicy Bypass -File install-admin.ps1 -Uninstall
#
# HKLM\SOFTWARE\Policies\Google\Chrome\ExtensionSettings\<拡張 ID> に
#   installation_mode = force_installed
#   update_url        = https://github.com/ippoan/alc-cam/releases/latest/download/update.xml
# を書く。HKCU\Software\Policies はユーザー権限では書けないので HKLM (管理者 1 回)。
# 64bit の Chrome が読むよう 64bit の view に書く (WOW6432Node に落とさない)。
# ExtensionInstallForcelist は 1 からの連番で他製品と番号がぶつかるので使わない。
#
# 注意: Windows の Chrome が Web Store 外の拡張を force_installed で入れるのは、管理された PC
# (AD / Entra に参加、または Chrome Enterprise Core に登録) だけ。それ以外では chrome://policy に
# [BLOCKED] と出て入らない。Chrome Enterprise Core は Google 管理コンソールの登録トークン
# (-EnrollmentToken) を CloudManagementEnrollmentToken に書けば登録できる。
#Requires -RunAsAdministrator
param([switch]$Uninstall, [string]$EnrollmentToken)

$ErrorActionPreference = 'Stop'
$extId = 'icmmpdjddlphkbokdalofdjpkcgfiggd'
$updateUrl = 'https://github.com/ippoan/alc-cam/releases/latest/download/update.xml'
$chromeKey = 'SOFTWARE\Policies\Google\Chrome'
$settingsKey = "$chromeKey\ExtensionSettings"

$hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)

if ($Uninstall) {
    $hklm.DeleteSubKeyTree("$settingsKey\$extId", $false)
    $s = $hklm.OpenSubKey($settingsKey)
    if ($s) {
        $empty = $s.SubKeyCount -eq 0 -and $s.ValueCount -eq 0
        $s.Close()
        if ($empty) { $hklm.DeleteSubKey($settingsKey, $false) }
    }
    Write-Host "Chrome のポリシーから alc-cam を外しました (Chrome を起動し直すと拡張が消えます)"
    exit 0
}

$k = $hklm.CreateSubKey("$settingsKey\$extId")
$k.SetValue('installation_mode', 'force_installed', [Microsoft.Win32.RegistryValueKind]::String)
$k.SetValue('update_url', $updateUrl, [Microsoft.Win32.RegistryValueKind]::String)
$k.Close()
Write-Host "Chrome のポリシーに書きました: HKLM\$settingsKey\$extId"

if ($EnrollmentToken) {
    $c = $hklm.CreateSubKey($chromeKey)
    $c.SetValue('CloudManagementEnrollmentToken', $EnrollmentToken, [Microsoft.Win32.RegistryValueKind]::String)
    $c.Close()
    Write-Host "Chrome Enterprise Core の登録トークンを書きました (Chrome の次の起動で登録されます)"
}

# 管理された PC か (でなければ force_installed は [BLOCKED] になる)
$joined = $false
try { $joined = [bool]((dsregcmd /status) -match '^\s*(DomainJoined|AzureAdJoined|EnterpriseJoined)\s*:\s*YES') } catch {}
$c = $hklm.OpenSubKey($chromeKey)
$enrolled = $c -and $c.GetValue('CloudManagementEnrollmentToken')
if ($c) { $c.Close() }
if (-not $joined -and -not $enrolled) {
    Write-Warning "この PC はドメイン / Entra に参加しておらず、Chrome Enterprise Core にも登録されていません。"
    Write-Warning "このままでは Chrome が Web Store 外の拡張の強制インストールを [BLOCKED] にします (chrome://policy で確認できます)。"
    Write-Warning "-EnrollmentToken <Google 管理コンソールの登録トークン> を付けて入れ直してください。"
}
