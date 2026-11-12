#!/usr/bin/env python3
"""生成 THIRD_PARTY_LICENSES.md：列出 Rust 和前端第三方依赖及其声明的许可证。
用法：在仓库根目录运行 `python3 scripts/gen-third-party.py`（需要已 `npm install`）。
只读取各包自己声明的许可证字段，不核对许可证文本。"""
import collections, json, os, subprocess, sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
os.chdir(root)


def rust():
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"], capture_output=True, text=True, check=True).stdout
    pk = json.loads(out)["packages"]
    seen = {}
    for p in pk:
        if p["source"] is None:  # 本仓库自己的 crate
            continue
        seen[(p["name"], p["version"])] = (p.get("license") or "未声明", p.get("repository") or "")
    return seen


def node(dev):
    cmd = ["npm", "ls", "--all", "--parseable"] + ([] if dev else ["--omit=dev"])
    out = subprocess.run(cmd, capture_output=True, text=True).stdout.split()[1:]
    seen = {}
    for p in out:
        f = os.path.join(p, "package.json")
        if os.path.exists(f):
            j = json.load(open(f))
            lic = j.get("license") or "未声明"
            repo = j.get("repository", "")
            repo = repo.get("url", "") if isinstance(repo, dict) else repo
            seen[(j["name"], j.get("version", ""))] = (str(lic), repo)
    return seen


def table(items):
    lines = ["| 包 | 版本 | 许可证 |", "|---|---|---|"]
    for (n, v), (lic, _) in sorted(items.items()):
        lines.append(f"| {n} | {v} | {lic} |")
    return "\n".join(lines)


def summary(items):
    c = collections.Counter(l for l, _ in items.values())
    return "\n".join(f"- `{l}`：{n} 个" for l, n in c.most_common())


r = rust()
n_run = node(False)
n_all = node(True)
n_dev = {k: v for k, v in n_all.items() if k not in n_run}
md = f"""# 第三方许可证清单

Crewforge 本身采用 [MIT 许可证](LICENSE)。本项目依赖的第三方包各自遵循其许可证，下面列出它们声明的许可证。
本文件由 `scripts/gen-third-party.py` 生成，只读取各包自己声明的许可证字段，不核对许可证文本；以各包随源码发布的 LICENSE 文件为准。

## 需要说明的几类

- **MPL-2.0**（文件级弱 copyleft）：Rust 依赖里有 `cssparser`、`cssparser-macros`、`dtoa-short`、`option-ext`、`selectors`；前端构建期工具 `lightningcss`。它们只作为依赖使用，未修改其源码；MPL-2.0 要求的是：对这些文件本身的修改要以 MPL-2.0 公开。
- **多重许可（`A OR B`）：** 使用者可以任选其一，我们按其中的宽松许可使用（例如 `r-efi` 的 `LGPL-2.1-or-later` 只是三个选项之一，我们不选它）。
- **SQLite：** `rusqlite` 开启 `bundled` 特性，内置 SQLite，SQLite 本身属于公共领域。

## 汇总

### Rust 依赖（{len(r)} 个）
{summary(r)}

### 前端运行时依赖（{len(n_run)} 个）
{summary(n_run)}

### 前端构建期依赖（{len(n_dev)} 个，不会进入最终应用）
{summary(n_dev)}

## 明细

### Rust

{table(r)}

### 前端运行时

{table(n_run)}

### 前端构建期

{table(n_dev)}
"""
open("THIRD_PARTY_LICENSES.md", "w").write(md)
print("Rust", len(r), "前端运行时", len(n_run), "构建期", len(n_dev))
