#!/bin/sh
# 构建并安装到 /Applications。Tauri 打出来的包只有链接器的临时签名，Info.plist 没绑定，
# macOS 的“本地网络”授权（访问局域网里的 Ollama 需要）不会生效，所以安装后要重新做完整签名。
set -e
cd "$(dirname "$0")/.."
npm run tauri build -- --bundles app
# 退出旧应用会打断正在干活的员工。构建要几十秒，期间自主推进可能又派了活，所以空闲检查放在构建之后：
# 连续 3 次（间隔 5 秒）都没有员工进程才继续；最多等 1 小时。CREWFORGE_FORCE=1 跳过检查。
if [ -z "$CREWFORGE_FORCE" ]; then
  zero=0; waited=0
  while [ $zero -lt 3 ]; do
    if pgrep -f 'claude -p' >/dev/null; then zero=0; else zero=$((zero+1)); fi
    [ $zero -ge 3 ] && break
    waited=$((waited+1))
    if [ $waited -gt 720 ]; then echo "等了 1 小时一直有员工在干活，没有安装（已构建好，可稍后重跑）"; exit 1; fi
    [ $((waited % 12)) -eq 1 ] && echo "有员工在干活，等他们收尾……"
    sleep 5
  done
fi
osascript -e 'quit app "Crewforge"' 2>/dev/null || true
sleep 3
rm -rf /Applications/Crewforge.app
cp -R target/release/bundle/macos/Crewforge.app /Applications/
codesign --force --deep --sign - --identifier com.crewforge.app /Applications/Crewforge.app
codesign --verify --deep --strict /Applications/Crewforge.app
open -a /Applications/Crewforge.app
