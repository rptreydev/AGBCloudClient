@echo off
echo ========================================
echo  AGB Cloud Client Installer Builder
echo  v0.1.0-beta
echo ========================================
echo.

echo [1/2] Building Rust release binary...
cargo build --release
if %ERRORLEVEL% neq 0 (
    echo ERROR: Cargo build failed!
    pause
    exit /b 1
)
echo Build successful: target\release\agb-cloud-client.exe
echo.

echo [2/2] Building NSIS installer...
where makensis >nul 2>&1
if %ERRORLEVEL% neq 0 (
    echo ERROR: makensis not found!
    echo Please install NSIS from https://nsis.sourceforge.io/
    echo Then add it to your PATH or run from NSIS directory.
    pause
    exit /b 1
)

cd installer
makensis installer.nsi
if %ERRORLEVEL% neq 0 (
    echo ERROR: NSIS build failed!
    cd ..
    pause
    exit /b 1
)
cd ..

echo.
echo ========================================
echo  SUCCESS!
echo  Installer: installer\AGBCloudClient-0.1.0-beta-Setup.exe
echo ========================================
pause
