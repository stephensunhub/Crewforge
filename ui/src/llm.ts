import type { Endpoint, EndpointInput } from "./types";

/** 把已有端点转成保存用的输入（令牌传 null 表示不改）；可以同时换掉模型清单。 */
export const endpointToInput = (ep: Endpoint, models: string[] = ep.models): EndpointInput => ({
  name: ep.name, base_url: ep.base_url, models, extra_env: ep.extra_env, max_concurrency: ep.max_concurrency,
  context_tokens: ep.context_tokens, limit_budget: ep.limit_budget, token: null,
});

/** 员工的「LLM 提供方」合并成一个选项：claude（本机订阅）、copilot，或 ep:<端点 id>。 */
export const srcOf = (provider: string, endpointId: string | null | undefined) => (endpointId ? `ep:${endpointId}` : provider);
export const providerOfSrc = (src: string) => (src === "copilot" ? "copilot" : "claude");
export const endpointOfSrc = (src: string) => (src.startsWith("ep:") ? src.slice(3) : "");
