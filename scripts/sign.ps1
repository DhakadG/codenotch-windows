<#
.SYNOPSIS
    Authenticode-signs one file, or does nothing when no certificate is configured.

.DESCRIPTION
    Called by .github/workflows/release.yml for the application binary and for the NSIS
    installer. When CODESIGN_PROVIDER is unset the script exits successfully without
    touching the file, so an unsigned build still completes on a fork or on a pull
    request from a contributor who has no access to the signing secrets.

    Enabling signing is therefore a repository-secrets change, not a workflow change.

    Every signature is timestamped (RFC 3161). Without a timestamp a signature stops
    validating the day the certificate expires; with one it stays valid for the lifetime
    of the timestamp authority's own certificate.

    Providers:
      (unset)  - no-op, exits 0.
      sslcom   - SSL.com eSigner CodeSignTool (cloud HSM).
      digicert - DigiCert KeyLocker / smctl (cloud HSM).
      azure    - Azure Trusted Signing.

    Only the provider actually purchased needs an implementation. The others throw
    rather than silently skipping, so a misconfigured secret fails the release build
    instead of publishing an unsigned installer that claims to be signed.

.PARAMETER Path
    The file to sign.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Path
)

$ErrorActionPreference = 'Stop'

$provider = $env:CODESIGN_PROVIDER
if ([string]::IsNullOrWhiteSpace($provider)) {
    Write-Host "sign.ps1: CODESIGN_PROVIDER is not set, leaving '$Path' unsigned."
    exit 0
}

if (-not (Test-Path $Path)) {
    throw "sign.ps1: file not found: $Path"
}

$timestampUrl = if ($env:CODESIGN_TIMESTAMP_URL) { $env:CODESIGN_TIMESTAMP_URL } else { 'http://ts.ssl.com' }

function Assert-Env {
    param([string[]]$Names)
    $missing = $Names | Where-Object { [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($_)) }
    if ($missing) {
        throw "sign.ps1: CODESIGN_PROVIDER=$provider requires these secrets, which are unset: $($missing -join ', ')"
    }
}

switch ($provider.ToLowerInvariant()) {
    'sslcom' {
        Assert-Env @('CODESIGN_USERNAME', 'CODESIGN_PASSWORD', 'CODESIGN_CREDENTIAL_ID', 'CODESIGN_TOTP_SECRET')
        throw @'
sign.ps1: the sslcom path is not implemented yet.

To implement: download SSL.com CodeSignTool, then invoke

  CodeSignTool sign -username=$env:CODESIGN_USERNAME -password=$env:CODESIGN_PASSWORD `
    -credential_id=$env:CODESIGN_CREDENTIAL_ID -totp_secret=$env:CODESIGN_TOTP_SECRET `
    -input_file_path=<Path> -override=true

CodeSignTool timestamps automatically. Fill this in once the certificate is issued.
'@
    }
    'digicert' {
        Assert-Env @('CODESIGN_CREDENTIAL_ID')
        throw 'sign.ps1: the digicert path is not implemented yet. Use smctl sign --keypair-alias=$env:CODESIGN_CREDENTIAL_ID --input <Path>.'
    }
    'azure' {
        throw 'sign.ps1: the azure path is not implemented yet. Use the Azure.CodeSigning.Dlib with signtool /dlib.'
    }
    default {
        throw "sign.ps1: unknown CODESIGN_PROVIDER '$provider'. Expected one of: sslcom, digicert, azure."
    }
}
