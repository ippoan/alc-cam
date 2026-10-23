# Release に付けるものを dist/ に作る (CI の windows job から呼ぶ。windows-latest の Chrome と node を使う)。
#   alc-cam.crx         拡張 (署名済み)。Chrome のポリシー (update_url) が update.xml 経由で入れる
#   update.xml          gupdate 形式。codebase はその版の Release の alc-cam.crx (固定 URL)
#   alc-cam-host.exe    host。自己更新がその版の Release から落とす
#   alc-cam-host.json   {"version","sha256"}。host の自己更新が最新 Release から読む
#
#   pwsh scripts/pack-release.ps1 -Version 0.3.1 -KeyPath ext.pem   # タグ: 本物の鍵。ID が違えば止まる
#   pwsh scripts/pack-release.ps1 -Version 0.3.1                    # PR: 使い捨ての鍵 (梱包の手順だけ確かめる)
# タグ・extension/manifest.json・host/Cargo.toml の版が揃っていなければ止まる。
param([Parameter(Mandatory)][string]$Version, [string]$KeyPath, [string]$Out = 'dist')

$ErrorActionPreference = 'Stop'
$extId = 'icmmpdjddlphkbokdalofdjpkcgfiggd'
$base = "https://github.com/ippoan/alc-cam/releases/download/v$Version"
$utf8 = New-Object System.Text.UTF8Encoding $false

$m = (Get-Content -Raw extension/manifest.json | ConvertFrom-Json).version
if ($m -ne $Version) { throw "extension/manifest.json の版 ($m) が $Version と違います" }
$h = (Select-String -Path host/Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if ($h -ne $Version) { throw "host/Cargo.toml の版 ($h) が $Version と違います" }

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$work = Join-Path ([IO.Path]::GetTempPath()) ("alc-cam-pack-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $src = Join-Path $work 'alc-cam'
    Copy-Item -Recurse -Path extension -Destination $src
    $throwaway = -not $KeyPath
    if ($throwaway) {
        $KeyPath = Join-Path $work 'throwaway.pem'
        $rsa = [Security.Cryptography.RSA]::Create(2048)
        [IO.File]::WriteAllText($KeyPath, $rsa.ExportPkcs8PrivateKeyPem(), $utf8)
    }

    $chrome = @(
        'C:\Program Files\Google\Chrome\Application\chrome.exe',
        'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe'
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $chrome) { throw 'chrome.exe が見つかりません (runner の image が変わった?)' }
    & $chrome --pack-extension="$src" --pack-extension-key="$KeyPath" --no-message-box
    $crx = "$src.crx"
    for ($i = 0; $i -lt 120 -and -not (Test-Path $crx); $i++) { Start-Sleep -Milliseconds 500 }
    if (-not (Test-Path $crx)) { throw 'CRX ができませんでした' }

    # Chrome は鍵が manifest の key と違っても黙って梱包するので、ID をここで確かめる
    $id = (& node scripts/crx-id.mjs $crx).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'CRX の ID が読めません' }
    if (-not $throwaway -and $id -ne $extId) { throw "拡張 ID が $id です ($extId のはず)。署名鍵 (Secret ALC_CAM_EXTENSION_KEY) を確かめてください" }
    Write-Host "CRX: $id$(if ($throwaway) { ' (使い捨ての鍵。Release には使わない)' })"
    Move-Item -Force -Path $crx -Destination (Join-Path $Out 'alc-cam.crx')
} finally {
    Remove-Item -Recurse -Force -Path $work -ErrorAction SilentlyContinue
}

$xml = @"
<?xml version='1.0' encoding='UTF-8'?>
<gupdate xmlns='http://www.google.com/update2/response' protocol='2.0'>
  <app appid='$extId'>
    <updatecheck codebase='$base/alc-cam.crx' version='$Version' />
  </app>
</gupdate>
"@
[IO.File]::WriteAllText((Join-Path $Out 'update.xml'), $xml, $utf8)

$exe = Join-Path $Out 'alc-cam-host.exe'
Copy-Item -Force -Path target/release/alc-cam-host.exe -Destination $exe
$sha = (Get-FileHash -Algorithm SHA256 -Path $exe).Hash.ToLower()
$json = [ordered]@{ version = $Version; sha256 = $sha } | ConvertTo-Json -Compress
[IO.File]::WriteAllText((Join-Path $Out 'alc-cam-host.json'), $json, $utf8)

Get-ChildItem $Out | Format-Table Name, Length
