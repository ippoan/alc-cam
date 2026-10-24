# Release に付けるものを dist/ に作る (CI の windows job から呼ぶ)。拠点 PC の host は最新 Release の
# alc-cam.json を見て、新しければその版の Release から exe と拡張の zip を落として入れ替える。
#   alc-cam-host.exe        host
#   alc-cam-extension.zip   拡張 (zip の直下に manifest.json)。host が %LOCALAPPDATA%\alc-cam\extension に展開する
#   alc-cam.json            {"version","host_sha256","extension_sha256"}
#   alc-cam.msi             初回に拠点 PC へ入れる物 (scripts/build-msi.ps1。以後は host が上の 3 つで自分を更新する)
#
#   pwsh scripts/pack-release.ps1 -Version 0.3.1
# extension/manifest.json と host/Cargo.toml の版が -Version と揃っていなければ止まる。
param([Parameter(Mandatory)][string]$Version, [string]$Out = 'dist')

$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding $false

$m = (Get-Content -Raw extension/manifest.json | ConvertFrom-Json).version
if ($m -ne $Version) { throw "extension/manifest.json の版 ($m) が $Version と違います" }
$h = (Select-String -Path host/Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
if ($h -ne $Version) { throw "host/Cargo.toml の版 ($h) が $Version と違います" }

New-Item -ItemType Directory -Force -Path $Out | Out-Null
$exe = Join-Path $Out 'alc-cam-host.exe'
Copy-Item -Force -Path target/release/alc-cam-host.exe -Destination $exe
$zip = Join-Path $Out 'alc-cam-extension.zip'
Remove-Item -Force -Path $zip -ErrorAction SilentlyContinue
# Compress-Archive は Windows で入れ子のパスを \ 区切りで書くことがあるので、名前を / 区切りにして自分で詰める
Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
$root = (Resolve-Path extension).Path
$za = [IO.Compression.ZipFile]::Open($zip, 'Create')
try {
    foreach ($f in Get-ChildItem -Recurse -File -Path extension) {
        $rel = [IO.Path]::GetRelativePath($root, $f.FullName).Replace('\', '/')
        [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($za, $f.FullName, $rel, 'Optimal')
    }
} finally {
    $za.Dispose()
}

$sha = { param($p) (Get-FileHash -Algorithm SHA256 -Path $p).Hash.ToLower() }
$json = [ordered]@{ version = $Version; host_sha256 = (& $sha $exe); extension_sha256 = (& $sha $zip) } | ConvertTo-Json -Compress
[IO.File]::WriteAllText((Join-Path $Out 'alc-cam.json'), $json, $utf8)
& (Join-Path $PSScriptRoot 'build-msi.ps1') -Version $Version -Exe $exe -Out (Join-Path $Out 'alc-cam.msi')

Get-ChildItem $Out | Format-Table Name, Length
