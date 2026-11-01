@echo off
rem P3 - 琥珀磷光终端（原生 Rust）
setlocal
cd /d "%~dp0"

if not exist "target\release\p3.exe" (
  echo [1/2] 首次运行，正在编译（需要几分钟）...
  cargo build --release
  if errorlevel 1 goto fail
)

echo [2/2] 启动 P3
start "" "target\release\p3.exe"
exit /b 0

:fail
echo.
echo 编译失败，请检查上方 cargo 输出。
pause
exit /b 1
