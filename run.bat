@echo off
rem AMBER TERMINAL - 原生 Rust 终端
setlocal
cd /d "%~dp0"

if not exist "target\release\amber-gui.exe" (
  echo [1/2] 首次运行，正在编译（需要几分钟）...
  cargo build --release
  if errorlevel 1 goto fail
)

echo [2/2] 启动 AMBER TERMINAL
start "" "target\release\amber-gui.exe"
exit /b 0

:fail
echo.
echo 编译失败，请检查上方 cargo 输出。
pause
exit /b 1
