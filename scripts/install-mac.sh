#!/bin/sh
# 构建并安装到 /Applications。Tauri 打出来的包只有链接器的临时签名，Info.plist 没绑定，
# macOS 的“本地网络”授权（访问局域网里的 Ollama 需要）不会生效，所以安装后要重新做完整签名。
set -e
cd "$(dirname "$0")/.."
npm run tauri build -- --bundles app
osascript -e 'quit app "Crewforge"' 2>/dev/null || true
sleep 3
rm -rf /Applications/Crewforge.app
cp -R target/release/bundle/macos/Crewforge.app /Applications/
codesign --force --deep --sign - --identifier com.crewforge.app /Applications/Crewforge.app
codesign --verify --deep --strict /Applications/Crewforge.app
open -a /Applications/Crewforge.app
