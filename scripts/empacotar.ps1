<#
Monta uma pasta pronta para levar a outro PC: dist\winshare (e dist\winshare.zip).

Conteúdo: screencast.exe, as DLLs do MinGW que ele usa, config.toml (modelo) e LEIA-ME.txt.
Compila em target\pacote para não esbarrar num screencast.exe em execução (que trava o
arquivo em target\release).

Uso, na raiz do projeto:  powershell -ExecutionPolicy Bypass -File scripts\empacotar.ps1
#>
$ErrorActionPreference = 'Stop'
$raiz = Split-Path -Parent $PSScriptRoot
Set-Location $raiz

$alvo = Join-Path $raiz 'target\pacote'
Write-Host 'Compilando (release)...'
cargo build --release --target-dir $alvo
if ($LASTEXITCODE -ne 0) { throw 'cargo build falhou' }
$exe = Join-Path $alvo 'release\screencast.exe'

# O Rust "gnu" liga o openh264 (C++) à libstdc++ dinâmica do MinGW que fez o link.
# Copiamos exatamente as DLLs desse MinGW, para não misturar versões.
$gcc = (Get-Command x86_64-w64-mingw32-gcc -ErrorAction Stop).Source
$mingwBin = Split-Path -Parent $gcc
$dlls = 'libstdc++-6.dll', 'libgcc_s_seh-1.dll', 'libwinpthread-1.dll'

$destino = Join-Path $raiz 'dist\winshare'
if (Test-Path $destino) { Remove-Item -Recurse -Force $destino }
New-Item -ItemType Directory -Force $destino | Out-Null

Copy-Item $exe $destino
foreach ($dll in $dlls) {
    $origem = Join-Path $mingwBin $dll
    if (-not (Test-Path $origem)) { throw "DLL não encontrada: $origem" }
    Copy-Item $origem $destino
}
Copy-Item (Join-Path $raiz 'config.toml.example') (Join-Path $destino 'config.toml')
Copy-Item (Join-Path $raiz 'docs\LEIA-ME-pacote.txt') (Join-Path $destino 'LEIA-ME.txt')

$zip = Join-Path $raiz 'dist\winshare.zip'
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $destino '*') -DestinationPath $zip

Write-Host "Pronto: $destino"
Write-Host "Zip:    $zip"
Write-Host 'Importante: o config.toml do pacote é o MODELO, sem seus tokens. Preencha no PC de destino.'
