@echo off
echo ==================================================
echo   AgentOS Native Bare-Metal Kernel Build & Boot
echo ==================================================

cd /d "%~dp0"
echo [1/4] Compiling Bare-Metal Rust Kernel (x86_64-unknown-none)...
cargo build --release

if %ERRORLEVEL% NEQ 0 (
    echo [ERROR] Cargo build failed!
    exit /b %ERRORLEVEL%
)

echo [2/4] Verifying generated ELF binary...
if exist "target\x86_64-unknown-none\release\agentos-kernel" (
    echo [OK] Binary generated: target\x86_64-unknown-none\release\agentos-kernel
) else (
    echo [ERROR] Binary not found!
    exit /b 1
)

echo [3/4] Packaging bootable disk image (kernel-runner + bootloader crate)...
if not exist "target\disk-image" mkdir "target\disk-image"
pushd "..\kernel-runner"
cargo build --release
if %ERRORLEVEL% NEQ 0 (
    echo [ERROR] kernel-runner build failed!
    popd
    exit /b %ERRORLEVEL%
)
".\target\release\kernel-runner.exe" "..\agentos-kernel\target\x86_64-unknown-none\release\agentos-kernel" "..\agentos-kernel\target\disk-image"
if %ERRORLEVEL% NEQ 0 (
    echo [ERROR] Disk image packaging failed!
    popd
    exit /b %ERRORLEVEL%
)
popd

echo [4/4] Launching AgentOS Kernel...
where qemu-system-x86_64 >nul 2>nul
if %ERRORLEVEL% EQU 0 (
    echo Launching QEMU...
    qemu-system-x86_64 -drive format=raw,file=target\disk-image\agentos-bios.img -serial stdio -netdev user,id=e1000net -device e1000,netdev=e1000net -netdev user,id=virtionet -device virtio-net-pci,netdev=virtionet
) else (
    echo [NOTE] QEMU is not installed in system PATH.
    echo To run the kernel in a virtual machine, execute:
    echo     qemu-system-x86_64 -drive format=raw,file=target\disk-image\agentos-bios.img -serial stdio -netdev user,id=e1000net -device e1000,netdev=e1000net -netdev user,id=virtionet -device virtio-net-pci,netdev=virtionet
)

echo ==================================================
echo Build process complete.
