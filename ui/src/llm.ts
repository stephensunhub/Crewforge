import type { Endpoint, EndpointInput } from "./types";

/** 把已有端点转成保存用的输入（令牌传 null 表示不改）；可以同时换掉模型清单。 */
export const endpointToInput = (ep: Endpoint, models: string[] = ep.models): EndpointInput => ({
  name: ep.name, base_url: ep.base_url, models, extra_env: ep.extra_env, max_concurrency: ep.max_concurrency,
  context_tokens: ep.context_tokens, limit_budget: ep.limit_budget, token: null,
});

/** 员工的「LLM 通道」合并成一个选项：claude（本机订阅）、copilot，或 ep:<端点 id>。 */
export const srcOf = (provider: string, endpointId: string | null | undefined) => (endpointId ? `ep:${endpointId}` : provider);
export const providerOfSrc = (src: string) => (src === "copilot" ? "copilot" : "claude");
export const endpointOfSrc = (src: string) => (src.startsWith("ep:") ? src.slice(3) : "");

const isNameChar = (c: string | undefined) => !!c && /[A-Za-z0-9_-]/.test(c);
/** 把文字里的旧名字换成新名字；前后紧挨着英文字母、数字、-、_ 的不换（和后端 `replace_name` 一致）。 */
export function replaceName(text: string, oldName: string, newName: string): string {
  if (!oldName || oldName === newName) return text;
  let out = "";
  let i = 0;
  for (;;) {
    const j = text.indexOf(oldName, i);
    if (j < 0) { out += text.slice(i); return out; }
    const before = j > 0 ? text[j - 1] : undefined;
    const after = text[j + oldName.length];
    out += text.slice(i, j) + (isNameChar(before) || isNameChar(after) ? oldName : newName);
    i = j + oldName.length;
  }
}
