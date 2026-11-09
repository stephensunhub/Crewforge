import { useCallback, useEffect, useRef, useState } from "react";

/**
 * 全局前进 / 后退：记录“位置”的变化（切换范围、切换标签），像浏览器一样可以回退和前进。
 * 触发方式：顶栏的 ‹ › 按钮、鼠标侧键（后退 / 前进，如罗技 MX Master 3 的拇指侧键）、Cmd+[ 和 Cmd+]。
 * 回退到已经不存在的位置（比如被归档的项目）时自动跳过。
 */
export function useNavHistory<T>(current: T, keyOf: (v: T) => string, apply: (v: T) => void, valid: (v: T) => boolean, limit = 60) {
  const stack = useRef<T[]>([]);
  const idx = useRef(-1);
  const [, bump] = useState(0);
  const latest = useRef({ keyOf, apply, valid });
  latest.current = { keyOf, apply, valid };
  const key = keyOf(current);

  useEffect(() => {
    const cur = stack.current[idx.current];
    if (cur !== undefined && latest.current.keyOf(cur) === key) { stack.current[idx.current] = current; return; }
    stack.current = [...stack.current.slice(0, idx.current + 1), current].slice(-limit);
    idx.current = stack.current.length - 1;
    bump((n) => n + 1);
  }, [key]); // eslint-disable-line react-hooks/exhaustive-deps

  const target = useCallback((dir: -1 | 1): number => {
    let i = idx.current + dir;
    while (i >= 0 && i < stack.current.length && !latest.current.valid(stack.current[i])) i += dir;
    return i >= 0 && i < stack.current.length ? i : -1;
  }, []);
  const go = useCallback((dir: -1 | 1) => {
    const i = target(dir);
    if (i < 0) return;
    idx.current = i;
    latest.current.apply(stack.current[i]);
    bump((n) => n + 1);
  }, [target]);

  useEffect(() => {
    let last = 0;
    const fire = (dir: -1 | 1) => { const now = Date.now(); if (now - last < 150) return; last = now; go(dir); };
    // 鼠标侧键：button 3 = 后退，button 4 = 前进。mouseup 和 auxclick 都可能收到，150ms 内只处理一次
    const onMouse = (e: MouseEvent) => {
      if (e.button === 3 || e.button === 4) { e.preventDefault(); fire(e.button === 3 ? -1 : 1); }
    };
    const onKey = (e: KeyboardEvent) => {
      if (!e.metaKey || e.altKey || e.ctrlKey || e.shiftKey) return;
      if (e.key === "[") { e.preventDefault(); fire(-1); }
      if (e.key === "]") { e.preventDefault(); fire(1); }
    };
    window.addEventListener("mouseup", onMouse);
    window.addEventListener("auxclick", onMouse);
    window.addEventListener("keydown", onKey);
    return () => { window.removeEventListener("mouseup", onMouse); window.removeEventListener("auxclick", onMouse); window.removeEventListener("keydown", onKey); };
  }, [go]);

  return { back: () => go(-1), forward: () => go(1), canBack: target(-1) >= 0, canForward: target(1) >= 0 };
}
