$ErrorActionPreference = "Stop"

$repo = "home-still/home"
$tool = "hs"
$installDir = "$env:LOCALAPPDATA\Programs\$tool"

$arch = if ([Environment]::Is64BitOperatingSystem) { "x86_64" } else {
    Write-Error "Unsupported: 32-bit Windows"; exit 1
}
$target = "$arch-pc-windows-msvc"

$release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$version = $release.tag_name

$archive = "$tool-$version-$target.zip"
$url = "https://github.com/$repo/releases/download/$version/$archive"

Write-Host "Installing $tool $version for $target..."

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
$tmp = Join-Path $env:TEMP $archive
Invoke-WebRequest -Uri $url -OutFile $tmp
Expand-Archive -Path $tmp -DestinationPath $installDir -Force
Remove-Item $tmp

# Add to PATH if not already there
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$installDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$installDir;$userPath", "User")
    Write-Host ""
    Write-Host "Added $installDir to your PATH. Restart your terminal to use '$tool'."
}

Write-Host "Installed $tool to $installDir\$tool.exe"