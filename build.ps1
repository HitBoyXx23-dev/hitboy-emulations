# Builds the three apps in release mode and packages them:
#   dist\<Brand> Emulations\<Brand> Emulations.exe
#   dist\<Brand>-Emulations-Windows.zip
#   websites\<brand>\  (download site with the zip inside)
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

# The browser player: cores compiled to WebAssembly plus the generated JavaScript glue.
# The C++ CPU cores need clang's WebAssembly backend (LLVM) for the browser build.
$llvm = Join-Path $env:ProgramFiles "LLVM\bin"
if (Test-Path "$llvm\clang.exe") { $env:PATH = "$llvm;$env:PATH" }
cargo build -p web --release --target wasm32-unknown-unknown
if ($LASTEXITCODE -ne 0) { throw "web build failed" }
wasm-bindgen --target web --no-typescript --out-dir target\webpkg target\wasm32-unknown-unknown\release\web.wasm
if ($LASTEXITCODE -ne 0) { throw "wasm-bindgen failed" }
$playTemplate = Get-Content "websites\play.html" -Raw -Encoding utf8
$iosTemplate  = Get-Content "websites\ios.html" -Raw -Encoding utf8

$version = (Select-String -Path "crates\frontend\Cargo.toml" -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
$brands = @(
    @{ id = "hitboy";  name = "HitBoy's"; primary = "#7851A9"; secondary = "#4169E1"; publisher = "HitBoyXx23"; repo = "https://github.com/HitBoyXx23-dev/hitboy-emulations" }
)
$template = Get-Content "websites\template.html" -Raw -Encoding utf8
New-Item -ItemType Directory -Force dist | Out-Null

# Code signing: each brand's exe is signed by its publisher. Uses a real certificate for that
# publisher if one is installed, otherwise creates a self-signed one (personal store only).
function Get-SigningCert([string]$publisher) {
    $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Where-Object { $_.Subject -eq "CN=$publisher" -and $_.NotAfter -gt (Get-Date) } | Sort-Object NotAfter -Descending | Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=$publisher" -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddYears(5)
    }
    return $cert
}

foreach ($b in $brands) {
    $full = "$($b.name) Emulations"
    $safe = ($b.name -replace "'", "") + "-Emulations"
    $appDir = Join-Path "dist" $full
    New-Item -ItemType Directory -Force $appDir | Out-Null
    $exePath = Join-Path $appDir "$full.exe"
    Copy-Item "target\release\$($b.id).exe" $exePath -Force
    $sig = Set-AuthenticodeSignature -FilePath $exePath -Certificate (Get-SigningCert $b.publisher) -HashAlgorithm SHA256
    if ($null -eq $sig.SignerCertificate) { throw "signing $exePath failed: $($sig.StatusMessage)" }

    # The dump helper (checks and verifies dumps from your own discs and consoles) and its guide.
    $dumpPath = Join-Path $appDir "Game Dump Helper.exe"
    Copy-Item "target\release\gamedump.exe" $dumpPath -Force
    $dsig = Set-AuthenticodeSignature -FilePath $dumpPath -Certificate (Get-SigningCert $b.publisher) -HashAlgorithm SHA256
    if ($null -eq $dsig.SignerCertificate) { throw "signing $dumpPath failed: $($dsig.StatusMessage)" }
    Copy-Item "crates\gamedump\src\guide.txt" (Join-Path $appDir "Get Your Games.txt") -Force

    $zip = "dist\$safe-Windows.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    $stage = Join-Path ([IO.Path]::GetTempPath()) ("stage-" + $b.id + "-" + $PID)
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    $stageInner = Join-Path $stage $full
    New-Item -ItemType Directory -Force $stageInner | Out-Null
    Copy-Item $exePath, $dumpPath, (Join-Path $appDir "Get Your Games.txt") $stageInner -Force
    Compress-Archive -Path $stageInner -DestinationPath $zip
    Remove-Item $stage -Recurse -Force

    $site = "websites\$($b.id)"
    New-Item -ItemType Directory -Force $site | Out-Null
    Copy-Item $zip $site -Force
    & "target\release\$($b.id).exe" --export-logo $site | Out-Null
    Move-Item -Force (Join-Path $site "$($b.id)-mark.png") (Join-Path $site "logo.png")
    Remove-Item (Join-Path $site "$($b.id)-icon.png") -ErrorAction SilentlyContinue

    $size = "{0:N1} MB" -f ((Get-Item $zip).Length / 1MB)
    $releasesUrl = "$($b.repo)/releases"
    $downloadUrl = "$($b.repo)/releases/latest/download/$safe-Windows.zip"
    $html = $template.Replace("{{FULL}}", $full).Replace("{{NAME}}", $b.name).Replace("{{PRIMARY}}", $b.primary).Replace("{{SECONDARY}}", $b.secondary)
    $html = $html.Replace("{{ZIP}}", "$safe-Windows.zip").Replace("{{DOWNLOAD_URL}}", $downloadUrl).Replace("{{RELEASES_URL}}", $releasesUrl)
    $html = $html.Replace("{{SIZE}}", $size).Replace("{{VERSION}}", $version).Replace("{{EXE}}", "$full.exe").Replace("{{YEAR}}", (Get-Date).Year)
    [IO.File]::WriteAllText((Join-Path $root "$site\index.html"), $html, (New-Object Text.UTF8Encoding $false))

    $play = Join-Path $site "play"
    New-Item -ItemType Directory -Force $play | Out-Null
    Copy-Item "target\webpkg\web.js", "target\webpkg\web_bg.wasm" $play -Force
    $phtml = $playTemplate.Replace("{{FULL}}", $full).Replace("{{NAME}}", $b.name).Replace("{{PRIMARY}}", $b.primary).Replace("{{SECONDARY}}", $b.secondary)
    [IO.File]::WriteAllText((Join-Path $root "$play\index.html"), $phtml, (New-Object Text.UTF8Encoding $false))
    $ihtml = $iosTemplate.Replace("{{FULL}}", $full).Replace("{{NAME}}", $b.name).Replace("{{PRIMARY}}", $b.primary).Replace("{{SECONDARY}}", $b.secondary)
    [IO.File]::WriteAllText((Join-Path $root "$site\ios.html"), $ihtml, (New-Object Text.UTF8Encoding $false))
    Write-Host "$full -> $appDir, $zip ($size), $site\index.html"
}

