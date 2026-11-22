# Installs the giong command on Windows:
#   powershell -c "irm https://giong.run/install.ps1 | iex"
#
# Downloads the prebuilt binary of the latest release from GitHub, checks its
# SHA-256, puts it in %USERPROFILE%\.giong\bin (or $env:GIONG_INSTALL) and adds
# that folder to the user's PATH. Where no binary is published (an ARM PC, or
# a release from before binaries), it builds the command with cargo instead.
# Never asks for administrator rights.
#
#   $env:GIONG_VERSION = "1.5.0"   a given version instead of the latest
#   $env:GIONG_INSTALL = "C:\x"    where the binary goes

# Everything sits in a function: `irm ... | iex` typed into an open PowerShell
# runs the script in that session, where `exit` would close the window.
function Install-Giong {
    $ErrorActionPreference = "Stop"
    # Invoke-WebRequest draws a progress bar per block in Windows PowerShell
    # 5.1, which makes downloads many times slower.
    $ProgressPreference = "SilentlyContinue"
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $repo = "giong-run/Giong"
    $dir = if ($env:GIONG_INSTALL) { $env:GIONG_INSTALL } else { Join-Path $env:USERPROFILE ".giong\bin" }
    $target = "x86_64-pc-windows-msvc"

    # A 32-bit PowerShell on 64-bit Windows reports x86 in the first and
    # AMD64 in the second.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne "AMD64") {
        Install-WithCargo $dir "no prebuilt giong for $arch"
        return
    }

    if ($env:GIONG_VERSION) {
        $tag = "v" + $env:GIONG_VERSION.TrimStart("v")
    } else {
        $tag = Get-LatestTag $repo
    }
    $version = $tag.TrimStart("v")
    $name = "giong-$version-$target"
    $base = "https://github.com/$repo/releases/download/$tag"
    $work = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid())
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        try {
            Invoke-WebRequest "$base/$name.zip" -OutFile "$work\$name.zip" -UseBasicParsing
        } catch {
            # A release from before the command had binaries.
            Install-WithCargo $dir "no prebuilt giong $version for Windows" $version
            return
        }
        Invoke-WebRequest "$base/$name.zip.sha256" -OutFile "$work\sum" -UseBasicParsing
        $want = (Get-Content "$work\sum" -Raw).Split(" ")[0].Trim().ToLower()
        $got = (Get-FileHash "$work\$name.zip" -Algorithm SHA256).Hash.ToLower()
        if ($want -ne $got) { throw "giong install: the download of $name is damaged (SHA-256 $got, expected $want): run this again" }
        Expand-Archive "$work\$name.zip" -DestinationPath $work
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Copy-Item "$work\$name\giong.exe" (Join-Path $dir "giong.exe") -Force
    } finally {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
    Add-ToPath $dir
    Write-Host "giong install: installed giong $version in $dir; run: giong doctor"
}

function Get-LatestTag($repo) {
    try {
        $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
    } catch {
        # The API allows 60 calls an hour per address; the release page's
        # redirect has no such limit: .../releases/latest -> .../releases/tag/vX.Y.Z
        $r = Invoke-WebRequest "https://github.com/$repo/releases/latest" -Method Head -UseBasicParsing
        # Windows PowerShell 5.1 and PowerShell 7 keep the final address in
        # different places.
        $uri = if ($r.BaseResponse.ResponseUri) { $r.BaseResponse.ResponseUri } else { $r.BaseResponse.RequestMessage.RequestUri }
        $tag = ($uri.AbsoluteUri -split "/")[-1]
    }
    if ($tag -notmatch "^v\d") {
        throw "giong install: cannot find the latest release on GitHub (set `$env:GIONG_VERSION = ""x.y.z"" to pick one)"
    }
    $tag
}

function Install-WithCargo($dir, $why, $version) {
    if (-not $version -and $env:GIONG_VERSION) { $version = $env:GIONG_VERSION.TrimStart("v") }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "giong install: $why and no cargo to build it: install Rust from https://rustup.rs, then run this again"
    }
    Write-Host "giong install: $why; building it with cargo (a minute or two)"
    $root = Split-Path $dir
    if ($version) {
        cargo install giong-cli --locked --version $version --root $root
    } else {
        cargo install giong-cli --locked --root $root
    }
    if ($LASTEXITCODE -ne 0) { throw "giong install: cargo install giong-cli failed" }
    Add-ToPath (Join-Path $root "bin")
    Write-Host "giong install: installed giong; run: giong doctor"
}

function Add-ToPath($dir) {
    $path = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $path) { $path = "" }
    if (-not ($path -split ";" | Where-Object { $_.TrimEnd("\") -eq $dir.TrimEnd("\") })) {
        $new = if ($path) { "$($path.TrimEnd(';'));$dir" } else { $dir }
        [Environment]::SetEnvironmentVariable("Path", $new, "User")
        $env:Path = "$env:Path;$dir"
        Write-Host "giong install: added $dir to your PATH (new terminals see it)"
    }
}

Install-Giong
