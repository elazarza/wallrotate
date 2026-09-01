<#
.SYNOPSIS
  Builds the categorized wallpaper gallery WallRotate was designed around.

.DESCRIPTION
  Downloads the wallpaper collection from https://github.com/dharmx/walls into
  your backgrounds folder, keeping its category subfolders (abstract, anime,
  nature, ...) and skipping a few. Uses a sparse partial clone, so the skipped
  folders are never downloaded at all.

  Optionally normalizes every video in the animated folder to 1080p / max 30fps
  (needs ffmpeg). Strongly recommended: 4K/60fps clips make the GPU's video
  decoder work 4-8x harder for no visible gain on a 1080p wallpaper. Originals
  are kept in animated\.originals, which WallRotate ignores.

.PARAMETER Destination
  Where the gallery goes. Default: %USERPROFILE%\Pictures\backgrounds
  (WallRotate's default wallpaper_dir, so it works with zero configuration).

.PARAMETER Exclude
  Category folders to skip. Edit to taste.

.PARAMETER NormalizeVideos
  Transcode animated\*.mp4 (etc.) to 1080p/30fps with ffmpeg after download.

.EXAMPLE
  .\get-backgrounds.ps1
  .\get-backgrounds.ps1 -NormalizeVideos
  .\get-backgrounds.ps1 -Destination D:\walls -Exclude girl,boccha
#>
param(
  [string]  $Destination = (Join-Path $env:USERPROFILE "Pictures\backgrounds"),
  [string[]]$Exclude = @("girl", "boccha", "decay", "devicons", "weirdcore"),
  [switch]  $NormalizeVideos
)

$ErrorActionPreference = "Stop"
$repo = "https://github.com/dharmx/walls"

if (-not (Get-Command git -EA SilentlyContinue)) { throw "git is required." }

$stage = Join-Path $env:TEMP ("walls-" + [guid]::NewGuid().ToString("n").Substring(0, 8))
Write-Host "Cloning $repo (sparse, ~3 GB for the kept folders)..." -ForegroundColor Cyan
git clone --depth 1 --filter=blob:none --sparse $repo $stage

# Every top-level folder except the excluded ones.
Push-Location $stage
$folders = git ls-tree -d --name-only HEAD |
  Where-Object { $_ -notin $Exclude -and $_ -ne ".github" }
git sparse-checkout set @folders
Pop-Location

New-Item -ItemType Directory -Force $Destination | Out-Null
Write-Host "Moving into $Destination ..." -ForegroundColor Cyan
Get-ChildItem $stage -Directory | Where-Object Name -ne ".git" | ForEach-Object {
  $target = Join-Path $Destination $_.Name
  if (Test-Path $target) { Remove-Item $target -Recurse -Force }
  Move-Item $_.FullName $target
}
Remove-Item $stage -Recurse -Force

if ($NormalizeVideos) {
  if (-not (Get-Command ffmpeg -EA SilentlyContinue)) {
    Write-Warning "ffmpeg not found (winget install Gyan.FFmpeg) - skipping video normalization."
  } else {
    $anim = Join-Path $Destination "animated"
    $orig = Join-Path $anim ".originals"
    New-Item -ItemType Directory -Force $orig | Out-Null
    Get-ChildItem "$anim\*" -File -Include *.mp4, *.m4v, *.mov, *.webm, *.mkv | ForEach-Object {
      $probe = ffprobe -v quiet -print_format json -show_streams -select_streams v:0 $_.FullName | ConvertFrom-Json
      $s = $probe.streams[0]
      $fps = 30
      if ($s.avg_frame_rate -match '(\d+)/(\d+)') { $fps = [int]$Matches[1] / [math]::Max(1, [int]$Matches[2]) }
      $vf = @()
      if ($s.width -gt 1920) { $vf += "scale=1920:-2:flags=lanczos" }
      if ($fps -gt 31)       { $vf += "fps=30" }
      if (-not $vf) { Write-Host "  ok      $($_.Name)"; return }
      Write-Host "  encode  $($_.Name)  [$($vf -join ',')]"
      $tmp = Join-Path $anim ("~tmp_" + $_.BaseName + ".mp4")
      ffmpeg -v error -y -i $_.FullName -vf ($vf -join ",") `
        -c:v libx264 -crf 20 -preset medium -pix_fmt yuv420p -movflags +faststart -an $tmp
      if ($LASTEXITCODE -eq 0 -and (Get-Item $tmp).Length -gt 100KB) {
        Move-Item $_.FullName (Join-Path $orig $_.Name) -Force
        Move-Item $tmp (Join-Path $anim ($_.BaseName + ".mp4"))
      } else {
        if (Test-Path $tmp) { Remove-Item $tmp -Force }
        Write-Warning "  failed  $($_.Name) - keeping original"
      }
    }
  }
}

Write-Host "`nDone. WallRotate's default wallpaper_dir already points here." -ForegroundColor Green
Write-Host "If it is running: tray icon -> Rescan wallpaper folder."