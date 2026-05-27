$ErrorActionPreference = "Stop"

$Repo = "Abyss116/IndexSearch"
$InstallDir = if ($env:INDEXSEARCH_INSTALL_DIR) {
    $env:INDEXSEARCH_INSTALL_DIR
} else {
    Join-Path $HOME ".local\bin"
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "IndexSearch currently publishes Windows x86_64 builds only."
}

$Asset = "indexsearch-windows-x86_64.zip"
$Url = "https://github.com/$Repo/releases/latest/download/$Asset"
$Temp = Join-Path ([System.IO.Path]::GetTempPath()) ("indexsearch-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Path $Temp | Out-Null

try {
    $Archive = Join-Path $Temp $Asset
    Write-Host "downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Archive
    Expand-Archive -Path $Archive -DestinationPath $Temp -Force
    $Payload = Get-ChildItem -Path $Temp -Directory -Filter "indexsearch-*" | Select-Object -First 1
    if (-not $Payload) {
        throw "archive layout changed"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    & (Join-Path $Payload.FullName "indexsearch.exe") install --dir $InstallDir
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathParts = @()
    if ($UserPath) {
        $PathParts = $UserPath -split ';' | Where-Object { $_ }
    }
    if ($PathParts -notcontains $InstallDir) {
        $NewPath = (@($PathParts) + $InstallDir) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "added $InstallDir to the user PATH"
    }

    Write-Host ""
    & (Join-Path $InstallDir "indexsearch.exe") --version
    Write-Host "installed indexsearch.exe, is.exe, and is-daemon.exe to $InstallDir"
} finally {
    Remove-Item -Recurse -Force $Temp -ErrorAction SilentlyContinue
}
