# JPEG テストフィクスチャ生成スクリプト（baseline DCT デコーダ用）。
#
# 再生成手順:
#   powershell.exe -ExecutionPolicy Bypass -File tests\fixtures\gen_jpeg_fixtures.ps1
#
# .NET の System.Drawing でテスト画像を生成し、各画像を JPEG（品質指定つき）で
# 保存したのち、同じ画像を Bitmap として読み戻して全ピクセルの RGB を
# 生バイト列（R,G,B 順、行優先）として .rgb にダンプする。.rgb が期待値となる。
#
# 注意:
#   - System.Drawing では CMYK JPEG / グレースケール JPEG を素直に作れないため、
#     カラー（4:4:4 / 4:2:0 などサブサンプリングは品質依存）のみを生成する。
#     低品質（quality 50）にすると JPEG エンコーダがクロマサブサンプリング
#     （4:2:0）を行うことが多く、端 MCU・アップサンプリング経路の検証になる。
#   - 期待値 .rgb はエンコード後の JPEG を「.NET 自身で」デコードした結果なので、
#     IDCT 実装差で数値が完全一致はしない。テスト側で誤差許容（チャネル絶対差 <=10、
#     平均絶対差 <= 2.0）で比較する。
#   - 奇数サイズ（17x13）で右端・下端の半端 MCU を検証する。

Add-Type -AssemblyName System.Drawing

$ErrorActionPreference = "Stop"
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path

function Save-Jpeg([System.Drawing.Bitmap]$bmp, [string]$path, [int]$quality) {
    $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() |
        Where-Object { $_.MimeType -eq "image/jpeg" } | Select-Object -First 1
    $params = New-Object System.Drawing.Imaging.EncoderParameters 1
    $params.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter(
        [System.Drawing.Imaging.Encoder]::Quality, [long]$quality)
    $bmp.Save($path, $codec, $params)
}

function Dump-Rgb([string]$jpgPath, [string]$rgbPath) {
    # JPEG を読み戻して RGB を生ダンプ（これが期待値）
    $img = [System.Drawing.Bitmap]::FromFile($jpgPath)
    try {
        $w = $img.Width
        $h = $img.Height
        $bytes = New-Object byte[] ($w * $h * 3)
        $idx = 0
        for ($y = 0; $y -lt $h; $y++) {
            for ($x = 0; $x -lt $w; $x++) {
                $c = $img.GetPixel($x, $y)
                $bytes[$idx]     = $c.R
                $bytes[$idx + 1] = $c.G
                $bytes[$idx + 2] = $c.B
                $idx += 3
            }
        }
        [System.IO.File]::WriteAllBytes($rgbPath, $bytes)
    } finally {
        $img.Dispose()
    }
}

# 画像生成ヘルパ群 -----------------------------------------------------------

function New-Solid([int]$w, [int]$h, [int]$r, [int]$g, [int]$b) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    for ($y = 0; $y -lt $h; $y++) {
        for ($x = 0; $x -lt $w; $x++) {
            $bmp.SetPixel($x, $y, [System.Drawing.Color]::FromArgb($r, $g, $b))
        }
    }
    return $bmp
}

function New-Gradient([int]$w, [int]$h) {
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    for ($y = 0; $y -lt $h; $y++) {
        for ($x = 0; $x -lt $w; $x++) {
            $r = [int](255 * $x / [Math]::Max(1, $w - 1))
            $g = [int](255 * $y / [Math]::Max(1, $h - 1))
            $b = [int](128)
            $bmp.SetPixel($x, $y, [System.Drawing.Color]::FromArgb($r, $g, $b))
        }
    }
    return $bmp
}

function New-Blocks([int]$w, [int]$h) {
    # 4 色のカラーブロック（クロマ境界を作る）
    $bmp = New-Object System.Drawing.Bitmap($w, $h)
    for ($y = 0; $y -lt $h; $y++) {
        for ($x = 0; $x -lt $w; $x++) {
            $left = $x -lt ($w / 2)
            $top  = $y -lt ($h / 2)
            if ($top -and $left)        { $c = [System.Drawing.Color]::FromArgb(220, 30, 30) }
            elseif ($top -and -not $left) { $c = [System.Drawing.Color]::FromArgb(30, 200, 40) }
            elseif (-not $top -and $left) { $c = [System.Drawing.Color]::FromArgb(40, 60, 230) }
            else                        { $c = [System.Drawing.Color]::FromArgb(240, 230, 20) }
            $bmp.SetPixel($x, $y, $c)
        }
    }
    return $bmp
}

# フィクスチャ定義: name, generator, size, quality ---------------------------

$specs = @(
    @{ name = "solid_16_q90";    gen = { New-Solid 16 16 120 60 200 }; q = 90 },
    @{ name = "gradient_16_q90"; gen = { New-Gradient 16 16 };         q = 90 },
    @{ name = "blocks_16_q90";   gen = { New-Blocks 16 16 };           q = 90 },
    @{ name = "blocks_16_q50";   gen = { New-Blocks 16 16 };           q = 50 },
    @{ name = "gradient_17x13_q90"; gen = { New-Gradient 17 13 };      q = 90 },
    @{ name = "blocks_17x13_q50";   gen = { New-Blocks 17 13 };        q = 50 },
    @{ name = "gradient_32_q50";    gen = { New-Gradient 32 32 };      q = 50 }
)

foreach ($s in $specs) {
    $bmp = & $s.gen
    try {
        $jpg = Join-Path $dir ($s.name + ".jpg")
        $rgb = Join-Path $dir ($s.name + ".rgb")
        Save-Jpeg $bmp $jpg $s.q
        Dump-Rgb $jpg $rgb
        Write-Host ("生成: {0}.jpg ({1}x{2}, q={3})" -f $s.name, $bmp.Width, $bmp.Height, $s.q)
    } finally {
        $bmp.Dispose()
    }
}

Write-Host "完了"
