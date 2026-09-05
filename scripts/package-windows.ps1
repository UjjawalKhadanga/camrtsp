[CmdletBinding()]
param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$Version = "0.1.0",
    [string]$OutputDirectory = "dist",
    [string]$CertificateThumbprint = $env:CAMRTSP_CERTIFICATE_THUMBPRINT,
    [string]$TimestampUrl = $env:CAMRTSP_TIMESTAMP_URL
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$output = Join-Path $repoRoot $OutputDirectory
$binary = Join-Path $repoRoot "target\$Target\release\camrtsp.exe"
$stage = Join-Path $output "camrtsp-$Version-windows-x64"
$zip = "$stage.zip"

Push-Location $repoRoot
try {
    cargo build --locked --release --target $Target -p camrtsp
    if (-not (Test-Path $binary)) {
        throw "Expected release binary was not produced: $binary"
    }

    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item -Force $binary (Join-Path $stage "camrtsp.exe")
    Copy-Item -Force (Join-Path $repoRoot "README.md") (Join-Path $stage "README.md")

    if ($CertificateThumbprint) {
        $signTool = Get-Command signtool.exe -ErrorAction Stop
        $arguments = @("sign", "/sha1", $CertificateThumbprint, "/fd", "SHA256")
        if ($TimestampUrl) {
            $arguments += @("/tr", $TimestampUrl, "/td", "SHA256")
        }
        $arguments += (Join-Path $stage "camrtsp.exe")
        & $signTool.Source @arguments
        if ($LASTEXITCODE -ne 0) { throw "signtool signing failed" }
        & $signTool.Source verify /pa /v (Join-Path $stage "camrtsp.exe")
        if ($LASTEXITCODE -ne 0) { throw "Authenticode verification failed" }
    }

    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path $stage -DestinationPath $zip
    $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
    Set-Content -NoNewline -Path "$zip.sha256" -Value "$hash  $(Split-Path -Leaf $zip)"
    Write-Host "Created $zip"
}
finally {
    Pop-Location
}
