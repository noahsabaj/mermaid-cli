<#
.SYNOPSIS
  Mermaid installer (Windows). Downloads the latest (or a pinned) release for
  this machine from GitHub Releases, verifies it against SHA256SUMS, and
  installs mermaid.exe + mermaidd.exe, adding the install dir to your user
  PATH. No Rust or cargo required.

    irm https://noahsabaj.github.io/mermaid-cli/install.ps1 | iex

.NOTES
  Environment overrides:
    $env:MERMAID_VERSION      = 'v0.11.0'   # install a specific tag (default: latest)
    $env:MERMAID_INSTALL_DIR  = 'C:\path'   # install location (default: %LOCALAPPDATA%\Programs\mermaid)
    $env:MERMAID_NO_MODIFY_PATH = '1'       # skip adding the dir to PATH
#>
$ErrorActionPreference = 'Stop'
$repo = 'noahsabaj/mermaid-cli'

$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64') {
    throw "Mermaid currently ships only x86_64 Windows builds (this machine is $arch). Build from source with cargo instead."
}
$asset = 'mermaid-windows-x86_64.zip'

if ($env:MERMAID_VERSION) {
    $base = "https://github.com/$repo/releases/download/$($env:MERMAID_VERSION)"
} else {
    $base = "https://github.com/$repo/releases/latest/download"
}

$dir = if ($env:MERMAID_INSTALL_DIR) { $env:MERMAID_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\mermaid' }
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("mermaid-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    Write-Host "Downloading $asset..."
    $zip = Join-Path $tmp $asset
    $sums = Join-Path $tmp 'SHA256SUMS'
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing

    # Verify against SHA256SUMS ("<hash>  <filename>").
    $line = (Select-String -Path $sums -Pattern ("\s" + [regex]::Escape($asset) + "$") | Select-Object -First 1).Line
    if (-not $line) { throw "no checksum listed for $asset" }
    $want = ($line.Trim() -split '\s+')[0]
    $got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash
    if ($want.ToLower() -ne $got.ToLower()) {
        throw "checksum mismatch for ${asset}: expected $want, got $got"
    }
    Write-Host "Checksum verified."

    # Extract.
    $extract = Join-Path $tmp 'x'
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    New-Item -ItemType Directory -Force -Path $dir | Out-Null

    foreach ($bin in @('mermaid.exe', 'mermaidd.exe')) {
        $src = Join-Path $extract $bin
        if (-not (Test-Path $src)) { throw "archive is missing $bin" }
        $dest = Join-Path $dir $bin
        # Windows won't overwrite a running .exe, but it WILL let you rename one.
        # Move the current binary aside (so an in-place `mermaid update` works),
        # then drop the new one in.
        if (Test-Path $dest) {
            $old = "$dest.old"
            Remove-Item -Force $old -ErrorAction SilentlyContinue
            try { Move-Item -Force $dest $old } catch {}
        }
        Copy-Item -Force $src $dest
    }
    # Best-effort cleanup of binaries renamed aside on a previous update.
    Get-ChildItem -Path $dir -Filter '*.exe.old' -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -Force $_.FullName -ErrorAction SilentlyContinue }

    Write-Host "Installed mermaid + mermaidd to $dir"

    if (-not $env:MERMAID_NO_MODIFY_PATH) {
        # Prepend our dir to the *user* PATH so this install wins over any older
        # mermaid earlier on PATH (e.g. a stale `cargo install` in ~\.cargo\bin).
        # Drop any existing entry for our dir first to keep it at the front and
        # avoid duplicates.
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $parts = @()
        if (-not [string]::IsNullOrEmpty($userPath)) {
            $parts = @($userPath -split ';' | Where-Object { $_ -and $_ -ne $dir })
        }
        [Environment]::SetEnvironmentVariable('Path', ((@($dir) + $parts) -join ';'), 'User')
        # Take effect in the current session too, at the front.
        $env:Path = "$dir;" + (($env:Path -split ';' | Where-Object { $_ -and $_ -ne $dir }) -join ';')
        Write-Host "Added $dir to the front of your user PATH."
    }

    # Warn if another mermaid elsewhere on PATH could shadow this install.
    $others = @(Get-Command mermaid.exe -All -ErrorAction SilentlyContinue |
        Where-Object { $_.Source -and (Split-Path $_.Source) -ne $dir })
    if ($others.Count -gt 0) {
        Write-Host ""
        Write-Host "Note: another 'mermaid' is also on your PATH and may shadow this one:"
        $others | ForEach-Object { Write-Host "  $($_.Source)" }
        Write-Host "If 'mermaid' keeps running an old version, remove that copy (e.g. 'cargo uninstall mermaid-cli')."
    }

    try { & (Join-Path $dir 'mermaid.exe') --version } catch {}
    Write-Host ""
    Write-Host "Done. Run 'mermaid' to start, or 'mermaid update' to upgrade later."
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
