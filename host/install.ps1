# alc-cam-host を Chrome の native messaging host として登録する (管理者権限は要らない。今のユーザーにだけ入る)。
#   powershell -ExecutionPolicy Bypass -File install.ps1            # 同じフォルダの alc-cam-host.exe を入れる
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
# 拡張の ID は extension/manifest.json の "key" で固定されている (icmmpdjddlphkbokdalofdjpkcgfiggd)。
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$name = 'org.ippoan.alc_cam_host'
$extId = 'icmmpdjddlphkbokdalofdjpkcgfiggd'
$dir = Join-Path $env:LOCALAPPDATA 'alc-cam'
$regKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$name"

if ($Uninstall) {
    Remove-Item -Path $regKey -Force -ErrorAction SilentlyContinue
    Remove-Item -Path $dir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "alc-cam-host を外しました"
    exit 0
}

$src = Join-Path $PSScriptRoot 'alc-cam-host.exe'
if (-not (Test-Path $src)) { throw "alc-cam-host.exe が $PSScriptRoot にありません" }

New-Item -ItemType Directory -Force -Path $dir | Out-Null
# Chrome が起動中の exe は上書きできないので、拡張のパネルを閉じてから入れ直す
Copy-Item -Path $src -Destination (Join-Path $dir 'alc-cam-host.exe') -Force

$manifest = [ordered]@{
    name            = $name
    description     = 'alc-cam: 拠点カメラを WebRTC で届ける'
    path            = (Join-Path $dir 'alc-cam-host.exe')
    type            = 'stdio'
    allowed_origins = @("chrome-extension://$extId/")
}
$manifestPath = Join-Path $dir "$name.json"
# Chrome は BOM 付きの JSON を読めないので BOM なしで書く (5.1 の -Encoding UTF8 は BOM を付ける)
[System.IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))

New-Item -Path $regKey -Force | Out-Null
Set-ItemProperty -Path $regKey -Name '(default)' -Value $manifestPath
Write-Host "登録しました: $manifestPath"
Write-Host "Chrome で拡張のサイドパネルを開き直すと使えます"
