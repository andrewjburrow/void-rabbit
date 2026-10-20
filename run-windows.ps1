$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "Build failed. Check Rust and Visual Studio C++ build tools." }
& .\target\release\void-rabbit.exe
