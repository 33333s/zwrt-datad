// All requests stay relative to the mounted /ui/ prefix. Credentials never enter a URL.
export class ApiError extends Error {
  constructor(message, status = 0) {
    super(message);
    this.status = status;
  }
}

// Preserve integers outside IEEE-754's exact range before JSON.parse can round them.
export function parseJson(text) {
  let output = "",
    at = 0;
  while (at < text.length) {
    if (text[at] === '"') {
      const begin = at++;
      while (at < text.length) {
        if (text[at] === "\\") at += 2;
        else if (text[at++] === '"') break;
      }
      output += text.slice(begin, at);
    } else if (text[at] === "-" || /[0-9]/.test(text[at])) {
      const number = text
        .slice(at)
        .match(/^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/);
      if (!number) {
        output += text[at++];
        continue;
      }
      const raw = number[0];
      output +=
        /^-?\d+$/.test(raw) && !Number.isSafeInteger(Number(raw))
          ? `"${raw}"`
          : raw;
      at += raw.length;
    } else output += text[at++];
  }
  return JSON.parse(output);
}

export class Client {
  constructor(base, expired) {
    this.base = new URL("../", base);
    this.token = "";
    this.expired = expired;
    this.generation = 0;
    this.controllers = new Set();
  }
  reset() {
    this.generation++;
    this.token = "";
    for (const controller of this.controllers) controller.abort();
    this.controllers.clear();
  }
  async request(path, options = {}) {
    if (
      !path.startsWith("/") ||
      path.startsWith("//") ||
      path
        .split("?")[0]
        .split("/")
        .some((p) => p === ".." || p === ".")
    )
      throw new Error("Invalid API path");
    const url = new URL(path.slice(1), this.base);
    if (
      url.origin !== this.base.origin ||
      !url.pathname.startsWith(this.base.pathname)
    )
      throw new Error("Invalid API origin");
    const controller = new AbortController(),
      generation = this.generation;
    this.controllers.add(controller);
    const signal = options.signal
      ? AbortSignal.any([options.signal, controller.signal])
      : controller.signal;
    const timeout = options.stream
      ? null
      : setTimeout(
          () =>
            controller.abort(
              new Error("请求超时，操作结果可能未知，请刷新确认"),
            ),
          options.timeout ?? 20000,
        );
    const headers = new Headers(options.headers);
    if (this.token && !headers.has("Authorization"))
      headers.set("Authorization", `Bearer ${this.token}`);
    let body = options.body;
    if (
      body &&
      !(body instanceof Blob) &&
      !(body instanceof Uint8Array) &&
      typeof body === "object"
    ) {
      headers.set("Content-Type", "application/json");
      body = JSON.stringify(body);
    }
    try {
      const response = await fetch(url, {
        method: options.method ?? "GET",
        headers,
        body,
        signal,
        credentials: "omit",
        cache: "no-store",
        redirect: "error",
      });
      if (generation !== this.generation)
        throw new DOMException("旧会话已结束", "AbortError");
      if (response.status === 401 && this.token) {
        this.reset();
        this.expired?.();
      }
      if (!response.ok) {
        const text = (await response.text()).slice(0, 4096);
        let detail;
        try {
          const data = parseJson(text);
          detail = data.error?.message ?? data.error?.code ?? data.error;
        } catch {
          /* Plain-text upstream error. */
        }
        throw new ApiError(
          typeof detail === "string"
            ? detail
            : `请求失败（HTTP ${response.status}）`,
          response.status,
        );
      }
      if (options.stream || options.binary) return response;
      const data = parseJson(await response.text());
      if (generation !== this.generation)
        throw new DOMException("旧会话已结束", "AbortError");
      if (data.ok === false)
        throw new ApiError(
          data.error?.message ?? data.error?.code ?? "操作失败",
          response.status,
        );
      return data;
    } finally {
      clearTimeout(timeout);
      this.controllers.delete(controller);
    }
  }
  async login(user, password, options = {}) {
    const bytes = new TextEncoder().encode(`${user}:${password}`);
    const basic = btoa(String.fromCharCode(...bytes));
    const data = await this.request("/auth/login", {
      ...options,
      method: "POST",
      headers: { Authorization: `Basic ${basic}` },
      body: {},
    });
    if (
      data.ok !== true ||
      typeof data.access_token !== "string" ||
      !data.access_token
    )
      throw new Error("登录响应不匹配");
    this.token = data.access_token;
  }
  async control(action, params = {}, options = {}) {
    const data = await this.request("/control", {
      ...options,
      method: "POST",
      body: { action, params },
      timeout: options.timeout ?? (action === "sms.send_raw" ? 60000 : 20000),
    });
    if (
      data.ok !== true ||
      !data.result ||
      typeof data.result !== "object" ||
      Array.isArray(data.result)
    )
      throw new ApiError("设备未返回有效的操作确认");
    return data.result;
  }
  async transaction(key, work) {
    this.operations ??= new Set();
    if (this.operations.has(key))
      throw new Error("同类设备操作仍在处理，请稍后");
    this.operations.add(key);
    const generation = this.generation,
      controller = new AbortController();
    this.controllers.add(controller);
    const check = () => {
      if (controller.signal.aborted || generation !== this.generation)
        throw new DOMException("会话已结束", "AbortError");
    };
    const scoped = {
      check,
      request: async (path, options = {}) => {
        check();
        return this.request(path, { ...options, signal: controller.signal });
      },
      control: async (action, params = {}, options = {}) => {
        check();
        return this.control(action, params, {
          ...options,
          signal: controller.signal,
        });
      },
      sleep: (ms) =>
        new Promise((resolve, reject) => {
          check();
          const abort = () => {
            clearTimeout(timer);
            reject(new DOMException("会话已结束", "AbortError"));
          };
          const timer = setTimeout(() => {
            controller.signal.removeEventListener("abort", abort);
            try {
              check();
              resolve();
            } catch (e) {
              reject(e);
            }
          }, ms);
          controller.signal.addEventListener("abort", abort, { once: true });
        }),
    };
    try {
      return await work(scoped);
    } finally {
      this.controllers.delete(controller);
      this.operations.delete(key);
    }
  }
}

export async function readBoundedBinary(
  response,
  maximum,
  progress = () => {},
) {
  const length = Number(response.headers.get("Content-Length"));
  if (length > maximum) {
    await response.body?.cancel();
    throw new Error("下载超过设备传输限制");
  }
  const reader = response.body.getReader(),
    chunks = [];
  let total = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > maximum) throw new Error("下载超过设备传输限制");
      chunks.push(value);
      progress(total, length > 0 ? length : null);
    }
    if (length > 0 && total !== length)
      throw new Error("下载长度与设备声明不符");
    const result = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      result.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return result;
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

export function validPath(path) {
  return (
    typeof path === "string" &&
    path.startsWith("/") &&
    !/[\x00-\x1f\x7f-\x9f]/.test(path) &&
    new TextDecoder().decode(new TextEncoder().encode(path)) === path &&
    new TextEncoder().encode(path).length <= 1011 &&
    (path === "/" ||
      path
        .slice(1)
        .split("/")
        .every((x) => x && x !== "." && x !== ".."))
  );
}
export function mutablePath(path) {
  return (
    validPath(path) && path !== "/" && path.slice(1).split("/").length >= 2
  );
}
export function childPath(parent, name) {
  if (!name || name.includes("/") || !validPath(parent))
    throw new Error("名称不能包含斜杠");
  const path = (parent === "/" ? "" : parent) + "/" + name;
  if (!validPath(path)) throw new Error("路径无效");
  return path;
}
export function bytesFromHex(hex) {
  if (typeof hex !== "string" || hex.length % 2 || /[^0-9a-f]/i.test(hex))
    throw new Error("HEX 必须为完整的十六进制字节");
  return Uint8Array.from(hex.match(/../g) ?? [], (x) => parseInt(x, 16));
}
export function hex(bytes) {
  return Array.from(bytes, (x) => x.toString(16).padStart(2, "0")).join("");
}
export function imeiFromNv(data) {
  if (data.encoding !== "hex" || data.bytes !== 133 || data.hex?.length !== 266)
    return null;
  let b;
  try {
    b = bytesFromHex(data.hex);
  } catch {
    return null;
  }
  if (
    b[0] !== 0x26 ||
    b[1] !== 0x26 ||
    b[2] !== 2 ||
    b[131] ||
    b[132] ||
    b[3] !== 8 ||
    (b[4] & 15) !== 10 ||
    b[4] >> 4 > 9
  )
    return null;
  let value = String(b[4] >> 4);
  for (let i = 5; i <= 11; i++) {
    const lo = b[i] & 15,
      hi = b[i] >> 4;
    if (lo > 9 || hi > 9) return null;
    value += `${lo}${hi}`;
  }
  return value;
}
export function nvConfig(imei) {
  if (!/^[0-9]{15}$/.test(imei) || /^0+$/.test(imei))
    throw new Error("IMEI 必须为 15 位数字，且不能全为 0");
  const b = [8, (Number(imei[0]) << 4) | 10];
  for (let i = 0; i < 7; i++)
    b.push((Number(imei[i * 2 + 2]) << 4) | Number(imei[i * 2 + 1]));
  return `[NV_SYS0]\nset=1\ntype=2\nnvcode=550\nlen=9\npar=${b.map((x) => "0x" + x.toString(16).toUpperCase().padStart(2, "0")).join(",")}\n`;
}
export function download(data, name, type = "application/octet-stream") {
  const url = URL.createObjectURL(new Blob([data], { type }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 30000);
}
export const display = (value) =>
  value === null || value === undefined || value === "" ? "—" : String(value);
export function el(tag, properties = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(properties)) {
    if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (key === "class") node.className = value;
    else if (key.startsWith("aria-")) node.setAttribute(key, value);
    else node[key] = value;
  }
  for (const child of children.flat())
    if (child !== null && child !== undefined)
      node.append(
        child instanceof Node ? child : document.createTextNode(String(child)),
      );
  return node;
}
export function formatBytes(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || value === null || value === undefined) return "—";
  const unit = Math.min(
    4,
    Math.max(0, Math.floor(Math.log2(Math.max(n, 1)) / 10)),
  );
  return `${(n / 1024 ** unit).toFixed(unit ? 1 : 0)} ${["B", "KiB", "MiB", "GiB", "TiB"][unit]}`;
}
