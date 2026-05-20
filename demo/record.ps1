# demo/record.ps1
#
# Run the heso agent demo and record the desktop with ffmpeg.
# Produces demo/demo.mp4 and an optimized demo/demo.gif for the README.
#
# Usage:
#   pwsh demo/record.ps1                                # interactive prompt
#   pwsh demo/record.ps1 "your query here"              # one-shot
#   pwsh demo/record.ps1 -Query "..." -Mp4Only          # skip gif conversion
#
# Tips:
# - Maximize your terminal window first; gdigrab captures the whole desktop.
# - Set ANTHROPIC_API_KEY in env for the live-agent path; otherwise the
#   offline (scripted) demo runs the same heso pipeline.

[CmdletBinding()]
param(
    [Parameter(Position=0)]
    [string]$Query,
    [switch]$Mp4Only,
    [int]$Fps = 20,
    [int]$GifWidth = 900,
    [int]$GifFps = 12
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
$demoDir = Join-Path $repoRoot "demo"
$mp4 = Join-Path $demoDir "demo.mp4"
$gif = Join-Path $demoDir "demo.gif"
$palette = Join-Path $demoDir ".palette.png"

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
    Write-Error "ffmpeg not in PATH. Install: winget install Gyan.FFmpeg"
}
if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
    Write-Error "python not in PATH."
}

$heso = Join-Path $repoRoot "target\release\heso.exe"
if (-not (Test-Path $heso)) {
    Write-Host "Building heso (release) first..." -ForegroundColor Yellow
    & cargo build --release -p heso-cli
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

if (-not $Query) {
    $Query = Read-Host "Query for the agent"
}
if (-not $Query) {
    Write-Error "no query"
}

# Remove old recording
Remove-Item -Path $mp4, $gif, $palette -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "Recording starts in 3 seconds. Make sure your terminal is visible." -ForegroundColor Cyan
Write-Host "  Output -> $mp4" -ForegroundColor DarkGray
Start-Sleep -Seconds 3

# Start ffmpeg gdigrab in background. -draw_mouse 0 hides the cursor.
$ffmpegArgs = @(
    "-y",
    "-f", "gdigrab",
    "-framerate", $Fps,
    "-draw_mouse", "0",
    "-i", "desktop",
    "-vcodec", "libx264",
    "-pix_fmt", "yuv420p",
    "-preset", "ultrafast",
    "-tune", "zerolatency",
    $mp4
)
$ffmpeg = Start-Process -FilePath "ffmpeg" -ArgumentList $ffmpegArgs -PassThru -WindowStyle Hidden
Start-Sleep -Milliseconds 600

try {
    & python (Join-Path $demoDir "agent.py") --query $Query
} finally {
    # Give the final frame a moment to settle, then stop ffmpeg cleanly.
    Start-Sleep -Milliseconds 800
    # Send 'q' to ffmpeg via stdin would be cleaner; gdigrab + Start-Process
    # doesn't expose a clean stdin, so SIGINT via taskkill is the pragmatic
    # path on Windows.
    if (-not $ffmpeg.HasExited) {
        Stop-Process -Id $ffmpeg.Id -ErrorAction SilentlyContinue
    }
    Wait-Process -Id $ffmpeg.Id -ErrorAction SilentlyContinue
}

if (-not (Test-Path $mp4)) {
    Write-Error "recording failed - no $mp4 produced"
}

Write-Host ""
Write-Host "MP4 saved: $mp4 ($([math]::Round((Get-Item $mp4).Length / 1MB, 1)) MB)" -ForegroundColor Green

if ($Mp4Only) {
    return
}

# Convert to a palette-optimized GIF for the README.
Write-Host "Converting to GIF (this takes a few seconds)..." -ForegroundColor Cyan

$paletteArgs = @(
    "-y",
    "-i", $mp4,
    "-vf", "fps=$GifFps,scale=${GifWidth}:-1:flags=lanczos,palettegen=stats_mode=diff",
    $palette
)
& ffmpeg @paletteArgs 2>$null | Out-Null

$gifArgs = @(
    "-y",
    "-i", $mp4,
    "-i", $palette,
    "-filter_complex", "fps=$GifFps,scale=${GifWidth}:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=5",
    $gif
)
& ffmpeg @gifArgs 2>$null | Out-Null

Remove-Item -Path $palette -ErrorAction SilentlyContinue

if (Test-Path $gif) {
    Write-Host "GIF saved:  $gif ($([math]::Round((Get-Item $gif).Length / 1MB, 1)) MB)" -ForegroundColor Green
} else {
    Write-Warning "GIF conversion failed (mp4 is still there)."
}
