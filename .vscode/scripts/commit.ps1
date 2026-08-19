$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location -LiteralPath $repoRoot

$codexPath = (Get-Command codex -ErrorAction SilentlyContinue | Select-Object -First 1).Source
if (-not $codexPath) {
    $extensionRoot = Join-Path $env:USERPROFILE '.vscode\extensions'
    $codexPath = Get-ChildItem -LiteralPath $extensionRoot -Directory -Filter 'openai.chatgpt-*' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending |
        ForEach-Object {
            $candidate = Join-Path $_.FullName 'bin\windows-x86_64\codex.exe'
            if (Test-Path -LiteralPath $candidate) {
                $candidate
            }
        } |
        Select-Object -First 1
}

if (-not $codexPath) {
    throw 'Codex CLI not found. Install or enable the OpenAI Codex VS Code extension, or add codex to PATH.'
}

& $codexPath exec --ephemeral --model gpt-5.6-terra --config 'model_reasoning_effort="medium"' --approve-for-me '$commit'
exit $LASTEXITCODE
