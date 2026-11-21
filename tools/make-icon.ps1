# Generates assets/icon.png (1024x1024) and assets/icon.ico (16..256) - macOS-style squircle app icon.
# Usage: powershell -File tools\make-icon.ps1
Add-Type -AssemblyName System.Drawing
$root = Split-Path $PSScriptRoot -Parent
$out = Join-Path $root 'assets'
New-Item -ItemType Directory -Force $out | Out-Null

function New-Squircle([float]$cx, [float]$cy, [float]$half, [float]$n) {
    $pts = New-Object 'System.Collections.Generic.List[System.Drawing.PointF]'
    for ($i = 0; $i -lt 720; $i++) {
        $t = 2 * [Math]::PI * $i / 720
        $c = [Math]::Cos($t); $s = [Math]::Sin($t)
        $x = $cx + $half * [Math]::Sign($c) * [Math]::Pow([Math]::Abs($c), 2 / $n)
        $y = $cy + $half * [Math]::Sign($s) * [Math]::Pow([Math]::Abs($s), 2 / $n)
        $pts.Add((New-Object System.Drawing.PointF $x, $y))
    }
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $p.AddPolygon($pts.ToArray())
    return $p
}

function Color($a, $r, $g, $b) { [System.Drawing.Color]::FromArgb($a, $r, $g, $b) }

$S = 1024
$bmp = New-Object System.Drawing.Bitmap $S, $S, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = 'AntiAlias'; $g.InterpolationMode = 'HighQualityBicubic'; $g.PixelOffsetMode = 'HighQuality'
$g.Clear([System.Drawing.Color]::Transparent)

$body = New-Squircle 512 512 412 5.0          # macOS grid: 824px body on a 1024 canvas
$rect = $body.GetBounds()

# soft drop shadow
for ($i = 14; $i -ge 1; $i--) {
    $sh = New-Squircle 512 (512 + 18) (412 + $i * 2.2) 5.0
    $br = New-Object System.Drawing.SolidBrush (Color ([int](2 + (15 - $i) * 0.45)) 0 0 0)
    $g.FillPath($br, $sh); $br.Dispose(); $sh.Dispose()
}

# body gradient (deep blue-black)
$bg = New-Object System.Drawing.Drawing2D.LinearGradientBrush $rect, (Color 255 44 52 72), (Color 255 12 15 24), 90
$g.FillPath($bg, $body)

# clip to the body for everything else
$g.SetClip($body)

# subtle radial glow behind the graph
$glow = New-Object System.Drawing.Drawing2D.GraphicsPath
$glow.AddEllipse(130, 330, 764, 520)
$pg = New-Object System.Drawing.Drawing2D.PathGradientBrush $glow
$pg.CenterColor = Color 70 30 150 255
$pg.SurroundColors = @((Color 0 30 150 255))
$g.FillPath($pg, $glow)

# faint grid
$gridPen = New-Object System.Drawing.Pen (Color 22 255 255 255), 3
foreach ($y in 300, 430, 560, 690) { $g.DrawLine($gridPen, 40, $y, 984, $y) }

# activity waveform
$ptsRaw = @(@(20, 640), @(250, 640), @(310, 560), @(370, 700), @(445, 330), @(525, 780), @(600, 480), @(660, 600), @(720, 520), @(780, 590), @(1004, 590))
$curve = New-Object 'System.Collections.Generic.List[System.Drawing.PointF]'
foreach ($p in $ptsRaw) { $curve.Add((New-Object System.Drawing.PointF $p[0], $p[1])) }
$line = New-Object System.Drawing.Drawing2D.GraphicsPath
$line.AddCurve($curve.ToArray(), 0.35)

# area under the curve
$area = New-Object System.Drawing.Drawing2D.GraphicsPath
$area.AddPath($line, $false)
$area.AddLine(1004, 590, 1004, 960); $area.AddLine(1004, 960, 20, 960); $area.CloseFigure()
$areaRect = New-Object System.Drawing.RectangleF 20, 330, 984, 630
$ab = New-Object System.Drawing.Drawing2D.LinearGradientBrush $areaRect, (Color 120 40 190 255), (Color 0 40 120 255), 90
$g.FillPath($ab, $area)

# glow strokes then the crisp gradient line
$lineRect = New-Object System.Drawing.RectangleF 20, 300, 984, 500
$lg = New-Object System.Drawing.Drawing2D.LinearGradientBrush $lineRect, (Color 255 64 224 208), (Color 255 70 140 255), 0
$cb = New-Object System.Drawing.Drawing2D.ColorBlend 3
$cb.Colors = @((Color 255 56 230 190), (Color 255 60 190 255), (Color 255 130 110 255))
$cb.Positions = @(0.0, 0.55, 1.0)
$lg.InterpolationColors = $cb
$lg.WrapMode = 'TileFlipXY'
foreach ($w in 64, 40, 24) {
    $a = if ($w -eq 64) { 22 } elseif ($w -eq 40) { 40 } else { 70 }
    $gp = New-Object System.Drawing.Pen (Color $a 80 200 255), $w
    $gp.LineJoin = 'Round'; $gp.StartCap = 'Round'; $gp.EndCap = 'Round'
    $g.DrawPath($gp, $line); $gp.Dispose()
}
$pen = New-Object System.Drawing.Pen $lg, 22
$pen.LineJoin = 'Round'; $pen.StartCap = 'Round'; $pen.EndCap = 'Round'
$g.DrawPath($pen, $line)

# tiny meter dots (the app's accent colours)
$dots = @((Color 255 36 165 255), (Color 255 255 92 92), (Color 255 236 214 84), (Color 255 198 76 240), (Color 255 72 224 92), (Color 255 255 160 32))
for ($i = 0; $i -lt $dots.Count; $i++) {
    $b = New-Object System.Drawing.SolidBrush $dots[$i]
    $g.FillEllipse($b, 232 + $i * 112, 800, 44, 44); $b.Dispose()
}

# glossy top highlight
$g.ResetClip(); $g.SetClip($body)
$hl = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.RectangleF 0, 100, 1024, 420), (Color 70 255 255 255), (Color 0 255 255 255), 90
$g.FillRectangle($hl, 0, 100, 1024, 420)
$g.ResetClip()

# thin inner rim
$rim = New-Object System.Drawing.Pen (Color 60 255 255 255), 4
$inner = New-Squircle 512 512 410 5.0
$g.DrawPath($rim, $inner)
$g.Dispose()

$bmp.Save((Join-Path $out 'icon.png'), [System.Drawing.Imaging.ImageFormat]::Png)

# ICO with PNG-compressed frames
$sizes = 16, 24, 32, 48, 64, 128, 256
$frames = @()
foreach ($sz in $sizes) {
    $b = New-Object System.Drawing.Bitmap $sz, $sz, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $gg = [System.Drawing.Graphics]::FromImage($b)
    $gg.InterpolationMode = 'HighQualityBicubic'; $gg.SmoothingMode = 'HighQuality'; $gg.PixelOffsetMode = 'HighQuality'
    $gg.Clear([System.Drawing.Color]::Transparent)
    $gg.DrawImage($bmp, 0, 0, $sz, $sz)
    $gg.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $frames += , @($sz, $ms.ToArray())
    $b.Dispose(); $ms.Dispose()
}
$fs = [System.IO.File]::Create((Join-Path $out 'icon.ico'))
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$frames.Count)
$offset = 6 + 16 * $frames.Count
foreach ($f in $frames) {
    $sz = $f[0]; $data = $f[1]
    $bw.Write([byte]($(if ($sz -ge 256) { 0 } else { $sz }))); $bw.Write([byte]($(if ($sz -ge 256) { 0 } else { $sz })))
    $bw.Write([byte]0); $bw.Write([byte]0); $bw.Write([UInt16]1); $bw.Write([UInt16]32)
    $bw.Write([UInt32]$data.Length); $bw.Write([UInt32]$offset)
    $offset += $data.Length
}
foreach ($f in $frames) { $bw.Write($f[1]) }
$bw.Close(); $fs.Close()
$bmp.Dispose()
"icon written to $out"
