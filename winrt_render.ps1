# Render page 0 of a PDF to PNG via WinRT Windows.Data.Pdf.
# Run with powershell.exe (5.1); pwsh 7 cannot project WinRT types.
# Usage: powershell -ExecutionPolicy Bypass -File winrt_render.ps1 <in.pdf> <out.png> [scale]
param(
    [Parameter(Mandatory = $true)][string]$InPdf,
    [Parameter(Mandatory = $true)][string]$OutPng,
    [double]$Scale = 2.0
)

Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Data.Pdf.PdfDocument, Windows.Data.Pdf, ContentType = WindowsRuntime]
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.RandomAccessStream, Windows.Storage.Streams, ContentType = WindowsRuntime]

$asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
        $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
    })[0]
function Await($WinRtTask, $ResultType) {
    $asTask = $asTaskGeneric.MakeGenericMethod($ResultType)
    $netTask = $asTask.Invoke($null, @($WinRtTask))
    $netTask.Wait(-1) | Out-Null
    $netTask.Result
}
$asTaskAction = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
        $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncAction'
    })[0]
function AwaitAction($WinRtAction) {
    $netTask = $asTaskAction.Invoke($null, @($WinRtAction))
    $netTask.Wait(-1) | Out-Null
}

$inPath = (Resolve-Path $InPdf).Path
$outPath = Join-Path (Get-Location) $OutPng

$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync($inPath)) ([Windows.Storage.StorageFile])
$pdf = Await ([Windows.Data.Pdf.PdfDocument]::LoadFromFileAsync($file)) ([Windows.Data.Pdf.PdfDocument])
$page = $pdf.GetPage(0)

$opts = New-Object Windows.Data.Pdf.PdfPageRenderOptions
$opts.DestinationWidth = [uint32]([math]::Round($page.Size.Width * $Scale))
$opts.DestinationHeight = [uint32]([math]::Round($page.Size.Height * $Scale))

$stream = New-Object Windows.Storage.Streams.InMemoryRandomAccessStream
AwaitAction ($page.RenderToStreamAsync($stream, $opts))

$size = $stream.Size
$reader = New-Object Windows.Storage.Streams.DataReader($stream.GetInputStreamAt(0))
Await ($reader.LoadAsync([uint32]$size)) ([uint32]) | Out-Null
$bytes = New-Object byte[] $size
$reader.ReadBytes($bytes)
[System.IO.File]::WriteAllBytes($outPath, $bytes)
Write-Host "rendered: $outPath ($($opts.DestinationWidth)x$($opts.DestinationHeight))"
