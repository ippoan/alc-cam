# alc-cam を拠点 PC に入れる (初回の 1 回だけ。以後は GitHub の Release を出すだけで拡張も host も更新される)。
#   install.cmd をダブルクリック                        # = 下の 1 行目 (管理者の確認が 1 回出る)
#   powershell -ExecutionPolicy Bypass -File install.ps1
#   powershell -ExecutionPolicy Bypass -File install.ps1 -EnrollmentToken <token>   # 非管理の PC を Chrome Enterprise Core に登録する
#   powershell -ExecutionPolicy Bypass -File install.ps1 -NoPolicy                  # host だけ (拡張は手で読み込む開発用)
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#
# 2 段になっている:
#   1. このスクリプト (今のユーザーの権限): host を %LOCALAPPDATA%\alc-cam に置き、HKCU に native messaging として登録する。
#      HKCU と %LOCALAPPDATA% は「Chrome を使う人」のものなので、管理者に昇格する前にここで書く
#   2. install-admin.ps1 (管理者に昇格して): HKLM の Chrome ポリシー ExtensionSettings に拡張を force_installed で書く。
#      Chrome は update_url (Release の update.xml) から拡張を入れ、以後も自分で更新する
# 拡張の ID は extension/manifest.json の "key" で固定されている (icmmpdjddlphkbokdalofdjpkcgfiggd)。
param([switch]$Uninstall, [switch]$NoPolicy, [string]$EnrollmentToken)

$ErrorActionPreference = 'Stop'
$name = 'org.ippoan.alc_cam_host'
$extId = 'icmmpdjddlphkbokdalofdjpkcgfiggd'
$dir = Join-Path $env:LOCALAPPDATA 'alc-cam'
$exe = Join-Path $dir 'alc-cam-host.exe'
$regKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$name"

function Invoke-Admin([string[]]$extra) {
    if ($NoPolicy) { return }
    $admin = Join-Path $PSScriptRoot 'install-admin.ps1'
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$admin`"") + $extra
    Write-Host "Chrome のポリシーを書くため、管理者の確認が出ます"
    $p = Start-Process -FilePath powershell.exe -ArgumentList $argList -Verb RunAs -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "install-admin.ps1 が失敗しました (exit $($p.ExitCode))" }
}

if ($Uninstall) {
    Invoke-Admin @('-Uninstall')
    Remove-Item -Path $regKey -Force -ErrorAction SilentlyContinue
    # 動いている host の exe は消せないので、消せた物だけ消す (残りは Chrome を閉じてから入れ直すか手で)
    Remove-Item -Path $dir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "alc-cam を外しました"
    exit 0
}

# パッケージ化されていない (手で読み込んだ) 拡張が同じ ID で入っていると、ポリシーの版と取り合いになる
function Find-Unpacked {
    $root = Join-Path $env:LOCALAPPDATA 'Google\Chrome\User Data'
    if (-not (Test-Path $root)) { return @() }
    $found = @()
    foreach ($f in Get-ChildItem -Path $root -Directory -ErrorAction SilentlyContinue) {
        foreach ($p in 'Secure Preferences', 'Preferences') {
            $path = Join-Path $f.FullName $p
            if (-not (Test-Path $path)) { continue }
            try {
                $s = (Get-Content -Raw -Path $path | ConvertFrom-Json).extensions.settings.$extId
                # location 4 = パッケージ化されていない拡張
                if ($s -and $s.location -eq 4) { $found += $f.Name }
            } catch {}
        }
    }
    return $found | Select-Object -Unique
}

$src = Join-Path $PSScriptRoot 'alc-cam-host.exe'
if (-not (Test-Path $src)) { throw "alc-cam-host.exe が $PSScriptRoot にありません" }

New-Item -ItemType Directory -Force -Path $dir | Out-Null
# host が動いていても入れ替えられるよう、今の exe は .old に退けてから置く (.old は host の次の起動で消える)
if (Test-Path $exe) {
    Remove-Item -Path "$exe.old" -Force -ErrorAction SilentlyContinue
    if (Test-Path "$exe.old") { Rename-Item -Path $exe -NewName "alc-cam-host.exe.old-$PID" }
    else { Rename-Item -Path $exe -NewName 'alc-cam-host.exe.old' }
}
Copy-Item -Path $src -Destination $exe -Force

$manifest = [ordered]@{
    name            = $name
    description     = 'alc-cam: 拠点カメラを WebRTC で届ける'
    path            = $exe
    type            = 'stdio'
    allowed_origins = @("chrome-extension://$extId/")
}
$manifestPath = Join-Path $dir "$name.json"
# Chrome は BOM 付きの JSON を読めないので BOM なしで書く (5.1 の -Encoding UTF8 は BOM を付ける)
[System.IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json), (New-Object System.Text.UTF8Encoding $false))

New-Item -Path $regKey -Force | Out-Null
Set-ItemProperty -Path $regKey -Name '(default)' -Value $manifestPath
Write-Host "host を登録しました: $manifestPath"

if ($NoPolicy) {
    Write-Host "拡張は chrome://extensions で手で読み込んでください (-NoPolicy)"
    exit 0
}

$unpacked = Find-Unpacked
if ($unpacked) {
    Write-Warning ("手で読み込んだ alc-cam の拡張が Chrome に入っています (プロファイル: {0})。" -f ($unpacked -join ', '))
    Write-Warning "chrome://extensions を開き、alc-cam 拠点中継の「削除」を押してから続けてください。"
    Read-Host "外したら Enter" | Out-Null
} else {
    Write-Host "手で読み込んだ alc-cam の拡張が入っている場合は、先に chrome://extensions で「削除」してください"
}

$extra = @()
if ($EnrollmentToken) { $extra += @('-EnrollmentToken', $EnrollmentToken) }
Invoke-Admin $extra
Write-Host ""
Write-Host "Chrome を完全に終了して起動し直すと、拡張が自動で入ります (以後の更新も自動)。"
Write-Host "入らないときは chrome://policy で ExtensionSettings の状態を見てください。"
