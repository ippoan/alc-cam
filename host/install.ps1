# alc-cam を拠点 PC に入れる (初回の 1 回だけ。管理者権限は要らない。以後は GitHub の Release を出すだけで host も拡張も更新される)。
#   install.cmd をダブルクリック          # = powershell -ExecutionPolicy Bypass -File install.ps1
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall
#
# 入れる物 (どれも今のユーザーの物。Chrome を使う Windows のユーザーで実行する):
#   %LOCALAPPDATA%\alc-cam\alc-cam-host.exe   host。HKCU に native messaging として登録する
#   %LOCALAPPDATA%\alc-cam\extension\         拡張。chrome://extensions でこのフォルダを 1 回だけ読み込む。
#                                             以後は host がこのフォルダを Release の版に書き換え、拡張が自分で読み込み直す
# 拡張の ID は extension/manifest.json の "key" で固定されている (icmmpdjddlphkbokdalofdjpkcgfiggd)。
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$name = 'org.ippoan.alc_cam_host'
$extId = 'icmmpdjddlphkbokdalofdjpkcgfiggd'
$dir = Join-Path $env:LOCALAPPDATA 'alc-cam'
$exe = Join-Path $dir 'alc-cam-host.exe'
$extDir = Join-Path $dir 'extension'
$regKey = "HKCU:\Software\Google\Chrome\NativeMessagingHosts\$name"

if ($Uninstall) {
    Remove-Item -Path $regKey -Force -ErrorAction SilentlyContinue
    # 動いている host の exe は消せないので、消せた物だけ消す (残りは Chrome を閉じてから)
    Remove-Item -Path $dir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "alc-cam の host を外しました。chrome://extensions で「alc-cam 拠点中継」を削除してください"
    exit 0
}

# Chrome のプロファイルに読み込まれている alc-cam の拡張の場所 (手で読み込んだ物だけ)
function Find-Loaded {
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
                if ($s -and $s.location -eq 4 -and $s.path) { $found += [pscustomobject]@{ Profile = $f.Name; Path = $s.path } }
            } catch {}
        }
    }
    return $found
}

foreach ($f in 'alc-cam-host.exe', 'extension\manifest.json') {
    if (-not (Test-Path (Join-Path $PSScriptRoot $f))) { throw "$f が $PSScriptRoot にありません (zip を全部展開してから実行してください)" }
}

New-Item -ItemType Directory -Force -Path $dir | Out-Null
# host が動いていても入れ替えられるよう、今の exe は .old に退けてから置く (.old は host の次の起動で消える)
if (Test-Path $exe) {
    Remove-Item -Path "$exe.old" -Force -ErrorAction SilentlyContinue
    if (Test-Path "$exe.old") { Rename-Item -Path $exe -NewName "alc-cam-host.exe.old-$PID" }
    else { Rename-Item -Path $exe -NewName 'alc-cam-host.exe.old' }
}
Copy-Item -Path (Join-Path $PSScriptRoot 'alc-cam-host.exe') -Destination $exe -Force
Remove-Item -Path $extDir -Recurse -Force -ErrorAction SilentlyContinue
Copy-Item -Path (Join-Path $PSScriptRoot 'extension') -Destination $extDir -Recurse -Force

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
Write-Host "host を登録し、拡張を置きました: $dir"
Write-Host ""

$loaded = @(Find-Loaded)
$here = @($loaded | Where-Object { $_.Path -eq $extDir })
$elsewhere = @($loaded | Where-Object { $_.Path -ne $extDir })
if ($elsewhere) {
    Write-Warning "別の場所から読み込んだ alc-cam の拡張が Chrome に入っています:"
    $elsewhere | ForEach-Object { Write-Warning ("  {0}: {1}" -f $_.Profile, $_.Path) }
    Write-Warning "そこは自動で更新されません。拠点 ID と RTSP URL を控えてから chrome://extensions で「削除」し、下の手順で読み込み直してください。"
}
if ($here -and -not $elsewhere) {
    Write-Host "拡張はもう $extDir から読み込まれています。chrome://extensions の alc-cam 拠点中継の ↻ を押すと新しい版になります。"
    exit 0
}
try { Set-Clipboard -Value $extDir } catch {}
Write-Host "Chrome で次を 1 回だけ行ってください (以後の更新は自動です):"
Write-Host "  1. アドレス欄に chrome://extensions を入れて開く"
Write-Host "  2. 右上の「デベロッパー モード」を ON にする (以後も OFF に戻さない。OFF だと自動更新で拡張が止まる)"
Write-Host "  3. 「パッケージ化されていない拡張機能を読み込む」で次のフォルダを選ぶ (パスはクリップボードに入っています)"
Write-Host "     $extDir"
Write-Host "  4. ID が $extId であることを確かめ、ツールバーのアイコンからパネルを開いて拠点 ID と RTSP URL を保存する"
