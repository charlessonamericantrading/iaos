@echo off
echo ==================================================
echo   AgentOS Native Bare-Metal Kernel Build & Boot  
echo ==================================================

cd /d "%~dp0"
echo [1/3] Compiling Bare-Metal Rust Kernel (x86_64-unknown-none)...
cargo build --release

if %ERRORLEVEL% NEQ 0 (
    echo [ERROR] Cargo build failed!
    exit /b %ERRORLEVEL%
)

echo [2/3] Verifying generated ELF binary...
if exist "target\x86_64-unknown-none\release\agentos-kernel" (
    echo [OK] Binary generated: target\x86_64-unknown-none\release\agentos-kernel
) else (
    echo [ERROR] Binary not found!
    exit /b 1
)

echo [3/3] Launching AgentOS Kernel...
where qemu-system-x86_64 >nul 2>nul
if %ERRORLEVEL% EQU 0 (
    echo Launching QEMU...
    qemu-system-x86_64 -kernel target\x86_64-unknown-none\release\agentos-kernel -serial stdio
) else (
    echo [NOTE] QEMU is not installed in system PATH.
    echo To run the kernel in a virtual machine, execute:
    echo     qemu-system-x86_64 -kernel target\x86_64-unknown-none\release\agentos-kernel -serial stdio
)

echo ==================================================
echo Build process complete.
