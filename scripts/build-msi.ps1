# alc-cam.msi を作る (scripts/alc-cam.wxs + 拡張のフォルダから作る部品)。pack-release.ps1 と check-msi.ps1 から呼ぶ。
# 要るもの: dotnet tool の wix 5.0.2 と、その WixToolset.UI.wixext / WixToolset.Util.wixext 5.0.2 (CI の windows job が入れる)。
#   pwsh scripts/build-msi.ps1 -Version 0.4.0 -Exe dist/alc-cam-host.exe -Out dist/alc-cam.msi
param([Parameter(Mandatory)][string]$Version, [Parameter(Mandatory)][string]$Exe, [Parameter(Mandatory)][string]$Out)

$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding $false
$work = Join-Path ([IO.Path]::GetTempPath()) "alc-cam-msi-$PID"
Remove-Item -Recurse -Force -Path $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $work | Out-Null

# native messaging の manifest。Chrome は BOM 付きを読めない。Windows では path を manifest の場所からの相対で書ける
$manifest = [ordered]@{
    name            = 'org.ippoan.alc_cam_host'
    description     = 'alc-cam: 拠点カメラを WebRTC で届ける'
    path            = 'alc-cam-host.exe'
    type            = 'stdio'
    allowed_origins = @('chrome-extension://icmmpdjddlphkbokdalofdjpkcgfiggd/')
}
$json = Join-Path $work 'org.ippoan.alc_cam_host.json'
[IO.File]::WriteAllText($json, ($manifest | ConvertTo-Json), $utf8)

# 拡張のフォルダごとに部品を 1 つ (KeyPath は HKCU の印、NeverOverwrite。理由は alc-cam.wxs の頭)
$root = (Resolve-Path extension).Path
$esc = { param($s) [Security.SecurityElement]::Escape($s) }
$dirs = @($root) + @(Get-ChildItem -Recurse -Directory -Path $root | ForEach-Object FullName)
$ids = @{}
foreach ($d in $dirs) {
    $rel = [IO.Path]::GetRelativePath($root, $d).Replace('\', '/')
    $ids[$d] = if ($rel -eq '.') { 'EXTDIR' } else { 'Ext_' + ($rel -replace '[^A-Za-z0-9]', '_') }
}
function Tree([string]$d) {
    $kids = (Get-ChildItem -Directory -Path $d | ForEach-Object { Tree $_.FullName }) -join ''
    "<Directory Id=`"$($ids[$d])`" Name=`"$(& $esc (Split-Path -Leaf $d))`">$kids</Directory>"
}
# HKCU の KeyPath の部品は WiX が GUID を自動で決めないので、フォルダの相対パスから決まる GUID にする (版をまたいで同じ)
$md5 = [Security.Cryptography.MD5]::Create()
$comps = foreach ($d in $dirs) {
    $id = $ids[$d]
    $rel = [IO.Path]::GetRelativePath($root, $d).Replace('\', '/')
    $guid = [guid]::new($md5.ComputeHash([Text.Encoding]::UTF8.GetBytes("alc-cam/extension/$rel"))).ToString().ToUpper()
    $files = (Get-ChildItem -File -Path $d | ForEach-Object { "<File Source=`"$(& $esc $_.FullName)`" />" }) -join ''
    "<Component Id=`"C_$id`" Directory=`"$id`" Guid=`"$guid`" NeverOverwrite=`"yes`">" +
    "<RegistryValue Root=`"HKCU`" Key=`"Software\ippoan\alc-cam\Installed`" Name=`"extension:$(& $esc $rel) [ProductVersion]`" Type=`"integer`" Value=`"1`" KeyPath=`"yes`" />" +
    "<RemoveFolder Id=`"R_$id`" On=`"uninstall`" />$files</Component>"
}
$ext = Join-Path $work 'extension.wxs'
$wxs = "<Wix xmlns=`"http://wixtoolset.org/schemas/v4/wxs`"><Fragment><DirectoryRef Id=`"INSTALLFOLDER`">$(Tree $root)</DirectoryRef>" +
    "<ComponentGroup Id=`"Extension`">$($comps -join '')</ComponentGroup></Fragment></Wix>"
[IO.File]::WriteAllText($ext, $wxs, $utf8)

New-Item -ItemType Directory -Force -Path (Split-Path -Parent ([IO.Path]::GetFullPath($Out))) | Out-Null
wix build -arch x64 -culture ja-JP -ext WixToolset.UI.wixext -ext WixToolset.Util.wixext `
    -d "Version=$Version" -d "HostExe=$((Resolve-Path $Exe).Path)" -d "HostManifest=$json" `
    (Join-Path $PSScriptRoot 'alc-cam.wxs') $ext -o $Out
if ($LASTEXITCODE) { throw "wix build が失敗しました ($LASTEXITCODE)" }
Remove-Item -Recurse -Force -Path $work -ErrorAction SilentlyContinue
