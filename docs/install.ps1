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
if (-not $version) {
    Write-Error "Failed to fetch latest version"; exit 1
}

$archive = "$tool-$version-$target.zip"
$url = "https://github.com/$repo/releases/download/$version/$archive"

Write-Host "Installing $tool $version for $target..."

$tmp = Join-Path $env:TEMP $archive
$tmpSha = "$tmp.sha256"
$stage = Join-Path $env:TEMP "$tool-$version-stage"
try {
    Invoke-WebRequest -Uri $url -OutFile $tmp
    Invoke-WebRequest -Uri "$url.sha256" -OutFile $tmpSha

    # `<64 hex>  <archive name>`
    $fields = (Get-Content -Raw $tmpSha).Trim() -split '\s+'
    if ($fields.Count -ne 2 -or $fields[1] -ne $archive) {
        Write-Error "Malformed checksum file for $archive"; exit 1
    }
    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash
    if ($actual -ne $fields[0]) {
        Write-Error "Checksum mismatch for ${archive}: expected $($fields[0]), got $actual"; exit 1
    }

    # Extract aside first so a bad archive never overwrites the installed binary.
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    Expand-Archive -Path $tmp -DestinationPath $stage
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Move-Item -Force -Path (Join-Path $stage "$tool.exe") -Destination $installDir
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp, $tmpSha, $stage
}

# Add to PATH if not already there
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$installDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$installDir;$userPath", "User")
    Write-Host ""
    Write-Host "Added $installDir to your PATH. Restart your terminal to use '$tool'."
}

Write-Host "Installed $tool to $installDir\$tool.exe"
