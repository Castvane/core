$ErrorActionPreference = "Stop"
choco install ffmpeg -y --no-progress
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Add-Content $env:GITHUB_PATH 'C:\Program Files\Git\usr\bin'
$ffmpeg = Get-ChildItem "$env:ChocolateyInstall/lib/ffmpeg/tools" -Recurse -File -Filter ffmpeg.exe | Select-Object -First 1
if (-not $ffmpeg) { throw 'The FFmpeg package did not contain its native executable' }
Add-Content $env:GITHUB_PATH $ffmpeg.DirectoryName
