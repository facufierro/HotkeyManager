param()
$ErrorActionPreference = 'Stop'
$projectPath = Split-Path -Parent $PSScriptRoot
$ahkPath = Join-Path $projectPath 'src-tauri/resources/autohotkey/AutoHotkey64.exe'
$source = [IO.File]::ReadAllText((Join-Path $projectPath 'src-tauri/src/ahk.rs'))
$engine = [regex]::Match($source, '(?s)const BEHAVIOR_ENGINE: &str = r###"(.*?)"###;')
if (-not $engine.Success) { throw 'AutoHotkey behavior engine not found' }

# Exercise the production interpreter with input delivery replaced by an in-memory test sink.
# No keyboard/mouse hooks or application processes are needed.
$isolatedEngine = [regex]::Replace($engine.Groups[1].Value, '\bSend(Input|Event)\b', 'TestSend')
$fixture = [IO.File]::ReadAllText((Join-Path $projectPath 'tests/ahk/behavior.test.ahk'))
$testPath = Join-Path ([IO.Path]::GetTempPath()) ('MacroToolbox-behavior-' + [Guid]::NewGuid().ToString('N') + '.ahk')
$process = $null
try {
    [IO.File]::WriteAllText($testPath, $fixture + [Environment]::NewLine + $isolatedEngine, [Text.UTF8Encoding]::new($true))
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $ahkPath
    $start.Arguments = '/ErrorStdOut "' + $testPath + '"'
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($start)
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit(15000)) {
        $process.Kill()
        $process.WaitForExit()
        throw 'Behavior tests timed out waiting for a simulated trigger release'
    }
    $output = $stdout.GetAwaiter().GetResult()
    $errors = $stderr.GetAwaiter().GetResult()
    if ($output) { Write-Output $output.TrimEnd() }
    if ($errors) { Write-Output $errors.TrimEnd() }
    if ($process.ExitCode -ne 0) { throw "Behavior tests failed (exit $($process.ExitCode))" }
} finally {
    if ($process) { $process.Dispose() }
    Remove-Item -LiteralPath $testPath -ErrorAction SilentlyContinue
}
