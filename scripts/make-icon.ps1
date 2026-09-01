Add-Type -AssemblyName System.Drawing

function New-IconBitmap([int]$s) {
  $bmp = New-Object System.Drawing.Bitmap($s, $s, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = 'AntiAlias'
  $g.PixelOffsetMode = 'HighQuality'
  $g.Clear([System.Drawing.Color]::Transparent)

  $pad = [math]::Max(1, [int]($s * 0.05))
  $r   = [math]::Max(2, [int]($s * 0.24))
  $rect = New-Object System.Drawing.Rectangle($pad, $pad, ($s - 2*$pad), ($s - 2*$pad))

  $path = New-Object System.Drawing.Drawing2D.GraphicsPath
  $d = $r * 2
  $path.AddArc($rect.X, $rect.Y, $d, $d, 180, 90)
  $path.AddArc($rect.Right - $d, $rect.Y, $d, $d, 270, 90)
  $path.AddArc($rect.Right - $d, $rect.Bottom - $d, $d, $d, 0, 90)
  $path.AddArc($rect.X, $rect.Bottom - $d, $d, $d, 90, 90)
  $path.CloseFigure()

  $c1 = [System.Drawing.Color]::FromArgb(255, 109, 92, 255)
  $c2 = [System.Drawing.Color]::FromArgb(255, 24, 190, 205)
  $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush($rect, $c1, $c2, 50.0)
  $g.FillPath($brush, $path)

  $cx = $s / 2.0; $cy = $s / 2.0
  $rad = $s * 0.255
  $penW = [math]::Max(1.4, $s * 0.105)
  $pen = New-Object System.Drawing.Pen([System.Drawing.Color]::White, $penW)
  $pen.StartCap = 'Round'; $pen.EndCap = 'Flat'
  $arc = New-Object System.Drawing.RectangleF(($cx-$rad), ($cy-$rad), (2*$rad), (2*$rad))
  $g.DrawArc($pen, $arc, 130, 250)

  $ah = $s * 0.23
  $ang = 128.0 * [math]::PI / 180.0
  $tipx = $cx + $rad * [math]::Cos($ang); $tipy = $cy + $rad * [math]::Sin($ang)
  $p1 = New-Object System.Drawing.PointF(($tipx - $ah*0.50), ($tipy - $ah*0.22))
  $p2 = New-Object System.Drawing.PointF(($tipx + $ah*0.46), ($tipy - $ah*0.42))
  $p3 = New-Object System.Drawing.PointF(($tipx + $ah*0.02), ($tipy + $ah*0.56))
  $g.FillPolygon([System.Drawing.Brushes]::White, @($p1,$p2,$p3))

  $g.Dispose()
  return $bmp
}

function Get-Bgra([System.Drawing.Bitmap]$bmp) {
  $w = $bmp.Width; $h = $bmp.Height
  $rect = New-Object System.Drawing.Rectangle(0, 0, $w, $h)
  $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $stride = $data.Stride
  $buf = New-Object byte[] ($stride * $h)
  [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buf, 0, $buf.Length)
  $bmp.UnlockBits($data)
  # repack to tightly packed bottom-up BGRA
  $out = New-Object byte[] ($w * $h * 4)
  for ($y = 0; $y -lt $h; $y++) {
    $src = ($h - 1 - $y) * $stride
    [System.Array]::Copy($buf, $src, $out, $y * $w * 4, $w * 4)
  }
  return $out
}

$sizes = 16,20,24,32,40,48,64,96,128,256
$entries = @()
foreach ($s in $sizes) {
  $bmp = New-IconBitmap $s
  $bgra = Get-Bgra $bmp
  $maskStride = [int](([math]::Floor(($s + 31) / 32)) * 4)
  $mask = New-Object byte[] ($maskStride * $s)   # all zero = fully opaque

  $ms = New-Object System.IO.MemoryStream
  $bw = New-Object System.IO.BinaryWriter($ms)
  $bw.Write([UInt32]40); $bw.Write([Int32]$s); $bw.Write([Int32]($s * 2))
  $bw.Write([UInt16]1); $bw.Write([UInt16]32); $bw.Write([UInt32]0)
  $bw.Write([UInt32]($bgra.Length + $mask.Length))
  $bw.Write([Int32]0); $bw.Write([Int32]0); $bw.Write([UInt32]0); $bw.Write([UInt32]0)
  $bw.Write($bgra); $bw.Write($mask); $bw.Flush()
  $entries += ,@($s, $ms.ToArray())
  $bw.Dispose(); $ms.Dispose(); $bmp.Dispose()
}

$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($out)
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$entries.Count)
$offset = 6 + 16 * $entries.Count
foreach ($e in $entries) {
  $sz = $e[0]; $data = $e[1]
  $b = if ($sz -ge 256) { 0 } else { $sz }
  $bw.Write([Byte]$b); $bw.Write([Byte]$b); $bw.Write([Byte]0); $bw.Write([Byte]0)
  $bw.Write([UInt16]1); $bw.Write([UInt16]32)
  $bw.Write([UInt32]$data.Length); $bw.Write([UInt32]$offset)
  $offset += $data.Length
}
foreach ($e in $entries) { $bw.Write($e[1]) }
$bw.Flush()
[System.IO.File]::WriteAllBytes("$PSScriptRoot\icon.ico", $out.ToArray())
$bw.Dispose(); $out.Dispose()
Write-Output ("icon.ico written, {0} entries" -f $entries.Count)
