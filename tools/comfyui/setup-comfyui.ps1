<#
  setup-comfyui.ps1 - install ComfyUI (Windows portable, NVIDIA) and a model,
  then expose its HTTP API on the LAN so the laptop can drive it.

  ASCII-only on purpose: Windows PowerShell 5.1 reads BOM-less files as ANSI,
  and a single non-ASCII byte inside a string can break parsing.

  Idempotent: safe to re-run. Anything already downloaded is skipped.

  Examples:
    powershell -ExecutionPolicy Bypass -File setup-comfyui.ps1
    powershell -ExecutionPolicy Bypass -File setup-comfyui.ps1 -Variant compact
    powershell -ExecutionPolicy Bypass -File setup-comfyui.ps1 -ModelSet sdxl
    powershell -ExecutionPolicy Bypass -File setup-comfyui.ps1 -ModelSet none
#>
[CmdletBinding()]
param(
    # Where to install. Needs roughly 30-45 GB free.
    [string] $InstallRoot = "$env:USERPROFILE\ComfyUI",

    # Which checkpoint family to download.
    [ValidateSet('zimage', 'sdxl', 'none')]
    [string] $ModelSet = 'zimage',

    # zimage only: quality | balanced | compact  (trades VRAM/disk for speed)
    [ValidateSet('quality', 'balanced', 'compact')]
    [string] $Variant = 'balanced',

    [int] $Port = 8188,

    # Restrict the API to one host — the machine you drive it from. An empty
    # string allows the whole LAN, which is only sensible on a trusted network.
    [string] $AllowFrom = '',

    [switch] $SkipFirewall
)

$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'

function Step($m) { Write-Host "`n=== $m ===" -ForegroundColor Cyan }
function Ok($m)   { Write-Host "  ok    $m" -ForegroundColor Green }
function Note($m) { Write-Host "  ..    $m" }
function Warn($m) { Write-Host "  WARN  $m" -ForegroundColor Yellow }
function Die($m)  { Write-Host "  FAIL  $m" -ForegroundColor Red; exit 1 }

# curl.exe ships with Windows 10 1803+. Running it through cmd.exe keeps its
# progress meter and warnings out of PowerShell's error stream, otherwise they
# render as red error text that looks like a failure.
$Curl = $null
if ($env:SystemRoot) {
    $candidate = Join-Path $env:SystemRoot 'System32\curl.exe'
    if (Test-Path $candidate) { $Curl = $candidate }
}

function Fetch([string] $Url, [string] $Dest) {
    $leaf = Split-Path -Leaf $Dest
    if (Test-Path $Dest) {
        $sz = (Get-Item $Dest).Length
        if ($sz -gt 0) {
            Write-Host ("  ok    have {0} ({1:n2} GB) - skipping" -f $leaf, ($sz / 1GB)) -ForegroundColor Green
            return
        }
    }
    $dir = Split-Path -Parent $Dest
    if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }

    # Download into a .part file and only rename after curl exits 0, so an
    # interrupted transfer can never be mistaken for a finished one on re-run.
    $part = "$Dest.part"
    Write-Host "  ..    $leaf"
    if ($Curl) {
        $exe = if ($Curl -match ' ') { '"' + $Curl + '"' } else { $Curl }
        $resume = ''
        if (Test-Path $part) { $resume = '--continue-at -' }
        $line = '{0} -L --fail --retry 5 --retry-delay 3 --connect-timeout 30 {1} --output "{2}" "{3}"' -f $exe, $resume, $part, $Url
        & cmd.exe /c $line
        if ($LASTEXITCODE -ne 0) { Die "download failed (curl exit $LASTEXITCODE): $Url -- partial kept at $part" }
    } else {
        Warn "curl.exe not found - falling back to Invoke-WebRequest"
        Invoke-WebRequest -Uri $Url -OutFile $part -UseBasicParsing
    }
    if (-not (Test-Path $part)) { Die "download produced no file: $Url" }
    $final = (Get-Item $part).Length
    if ($final -le 0) { Die "downloaded a zero-byte file: $Url" }
    Move-Item -Force -Path $part -Destination $Dest
    Write-Host ("  ok    {0,8:n2} GB  {1}" -f ($final / 1GB), $leaf) -ForegroundColor Green
}

# ---------------------------------------------------------------- preflight
Step "Preflight"

if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) {
    & nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
} else {
    Warn "nvidia-smi is not on PATH - confirm the NVIDIA driver is installed."
}

$qual        = Split-Path -Qualifier $InstallRoot
$driveLetter = $qual.TrimEnd(':')
if (Get-PSDrive -Name $driveLetter -ErrorAction SilentlyContinue) {
    $freeGB = (Get-PSDrive -Name $driveLetter).Free / 1GB
    Note ("target {0}  ({1} - {2:n1} GB free)" -f $InstallRoot, $qual, $freeGB)
    if ($freeGB -lt 30) { Warn "under 30 GB free; ComfyUI plus models wants 30-45 GB" }
} else {
    Warn "could not determine free space for $qual"
}

# ---------------------------------------------------------------- extractor
Step "Extractor"
$SevenZip = Join-Path $env:TEMP '7zr.exe'
Fetch 'https://www.7-zip.org/a/7zr.exe' $SevenZip

# ---------------------------------------------------------------- comfyui
Step "ComfyUI portable v0.35.0 (NVIDIA)"
$Tag     = 'v0.35.0'
$Archive = Join-Path $env:TEMP "ComfyUI_windows_portable_nvidia_$Tag.7z"
Fetch "https://github.com/Comfy-Org/ComfyUI/releases/download/$Tag/ComfyUI_windows_portable_nvidia.7z" $Archive

$CF   = Join-Path $InstallRoot 'ComfyUI_windows_portable'
$Main = Join-Path $CF 'ComfyUI\main.py'

if (Test-Path $Main) {
    Ok "already extracted - skipping"
} else {
    Note "extracting (this takes a few minutes)"
    New-Item -ItemType Directory -Force -Path $InstallRoot | Out-Null
    & $SevenZip x $Archive "-o$InstallRoot" -y | Out-Null
    if (-not (Test-Path $Main)) { Die "extracted, but $Main is missing - the release layout may have changed" }
    Ok "extracted to $CF"
}

# ---------------------------------------------------------------- models
$Models = Join-Path $CF 'ComfyUI\models'

if ($ModelSet -eq 'zimage') {
    Step "Z-Image Turbo ($Variant) - Apache 2.0, approx 6B"
    $HF  = 'https://huggingface.co/Comfy-Org/z_image_turbo/resolve/main/split_files'
    $map = @{
        quality  = @('diffusion_models/z_image_turbo_bf16.safetensors',         'text_encoders/qwen_3_4b.safetensors')
        balanced = @('diffusion_models/z_image_turbo_bf16.safetensors',         'text_encoders/qwen_3_4b_fp8_mixed.safetensors')
        compact  = @('diffusion_models/z_image_turbo_int8_convrot.safetensors', 'text_encoders/qwen_3_4b_fp8_mixed.safetensors')
    }
    $unet = $map[$Variant][0]
    $text = $map[$Variant][1]

    Fetch "$HF/$unet" (Join-Path $Models "diffusion_models\$(Split-Path -Leaf $unet)")
    Fetch "$HF/$text" (Join-Path $Models "text_encoders\$(Split-Path -Leaf $text)")
    Fetch "$HF/vae/ae.safetensors" (Join-Path $Models 'vae\ae.safetensors')
    Fetch "$HF/loras/z_image_turbo_distill_patch_lora_bf16.safetensors" (Join-Path $Models 'loras\z_image_turbo_distill_patch_lora_bf16.safetensors')
}
elseif ($ModelSet -eq 'sdxl') {
    Step "SDXL base 1.0 - single-file checkpoint, approx 6.9 GB"
    Fetch 'https://huggingface.co/stabilityai/stable-diffusion-xl-base-1.0/resolve/main/sd_xl_base_1.0.safetensors' (Join-Path $Models 'checkpoints\sd_xl_base_1.0.safetensors')
}
else {
    Step "Models"
    Warn "ModelSet=none - no checkpoint installed. Put one in $Models\checkpoints"
}

# ---------------------------------------------------------------- launcher
Step "Launcher"
$Bat = Join-Path $CF 'run-listen.bat'
$batLines = @(
    '@echo off'
    'cd /d "%~dp0"'
    'echo.'
    "echo   ComfyUI API  -^>  http://0.0.0.0:$Port"
    'echo   Press Ctrl+C to stop.'
    'echo.'
    '".\python_embeded\python.exe" -s ComfyUI\main.py --listen 0.0.0.0 --port ' + $Port
    'pause'
)
Set-Content -Path $Bat -Value $batLines -Encoding ASCII
Ok "wrote $Bat"

# ---------------------------------------------------------------- firewall
Step "Firewall"
if ($SkipFirewall) {
    Warn "skipped (-SkipFirewall)"
} else {
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $ruleName = "ComfyUI $Port (LAN)"
    $remote = 'Any'
    if (-not [string]::IsNullOrWhiteSpace($AllowFrom)) { $remote = $AllowFrom }

    if (-not $admin) {
        Warn "not elevated - open an Administrator PowerShell and run this:"
        Write-Host ""
        Write-Host "  New-NetFirewallRule -DisplayName '$ruleName' -Direction Inbound -Action Allow -Protocol TCP -LocalPort $Port -RemoteAddress $remote -Profile Private,Domain"
        Write-Host ""
    } else {
        Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule -ErrorAction SilentlyContinue
        New-NetFirewallRule -DisplayName $ruleName -Direction Inbound -Action Allow -Protocol TCP -LocalPort $Port -RemoteAddress $remote -Profile Private,Domain | Out-Null
        Ok "inbound tcp/$Port allowed from $remote only"
    }
}

# ---------------------------------------------------------------- done
Step "Done"
Write-Host ""
Write-Host "  Start ComfyUI:    $Bat"
Write-Host "  Then open:        http://localhost:$Port"
Write-Host "  API:              http://<this-host>:$Port/system_stats"
Write-Host ""
Write-Host "  Nothing was installed system-wide. To remove everything, delete:"
Write-Host "    $InstallRoot"
Write-Host ""
