import {
  validPath,
  mutablePath,
  childPath,
  bytesFromHex,
  hex,
} from "./core.js";

export const protectedBackend = (s) =>
  s.available === true &&
  s.safety?.version >= 1 &&
  s.safety.root_readonly === true &&
  s.safety.recursive_delete === false &&
  s.safety.no_symlink_mutations === true;
export function checkedPage(data, offset, length) {
  if (
    !data ||
    data.offset !== offset ||
    !Number.isInteger(data.bytes) ||
    data.bytes < 0 ||
    data.bytes > length ||
    bytesFromHex(data.hex).length !== data.bytes ||
    data.next_offset !== offset + data.bytes ||
    typeof data.eof !== "boolean" ||
    (!data.bytes && !data.eof)
  )
    throw new Error("分页响应不匹配");
  return data;
}
export function checkedListing(data, path) {
  if (
    data?.path !== path ||
    !Array.isArray(data.entries) ||
    data.entries.length > 4096
  )
    throw new Error("目录响应不匹配");
  return data.entries.map((entry) => {
    let nameValid = false;
    try {
      childPath(path, entry.name);
      nameValid =
        !!entry.name_hex &&
        hex(new TextEncoder().encode(entry.name)) ===
          entry.name_hex.toLowerCase();
    } catch {
      /* Keep unaddressable names visible, never act on lossy names. */
    }
    return { ...entry, nameValid };
  });
}
export class EfsClient {
  constructor(api) {
    this.api = api;
    this.busy = false;
    this.uncertainUntil = 0;
  }
  scoped(api) {
    const root = this,
      scope = Object.create(this);
    scope.api = api;
    scope.exclusive = (operation) => root.exclusive(operation);
    Object.defineProperty(scope, "uncertainUntil", {
      get: () => root.uncertainUntil,
      set: (value) => {
        root.uncertainUntil = value;
      },
    });
    return scope;
  }
  async exclusive(operation) {
    if (this.busy) throw new Error("请等待当前 EFS 操作完成");
    this.busy = true;
    try {
      return await operation();
    } finally {
      this.busy = false;
    }
  }
  request(action, path, options = {}) {
    if (path !== null && !validPath(path)) throw new Error("EFS 路径无效");
    return this.api.request(
      `/efs/${action}${path === null ? "" : "?path=" + encodeURIComponent(path)}`,
      { ...options, timeout: 125000 },
    );
  }
  async mutate(action, path, body, expectedBytes) {
    if (!["upload", "write", "rename", "delete", "mkdir"].includes(action))
      throw new Error("未知 EFS 修改操作");
    if (!mutablePath(path)) throw new Error("根目录及直属条目只读");
    if (Date.now() < this.uncertainUntil)
      throw new Error("上次写入结果未确认，请等待两分钟后重新读取核对");
    return this.exclusive(async () => {
      const status = await this.request("status", null);
      if (!protectedBackend(status))
        throw new Error("后端缺少 EFS 写保护，当前仅可读取");
      if (
        body instanceof Uint8Array &&
        body.length > Math.min(1048576, status.limits?.transfer_bytes ?? 0)
      )
        throw new Error("上传超过设备限制");
      if (
        action === "write" &&
        bytesFromHex(body.hex).length >
          Math.min(32768, status.limits?.write_bytes ?? 0)
      )
        throw new Error("编辑超过设备限制");
      if (
        action === "rename" &&
        (!mutablePath(body.destination) ||
          body.destination === path ||
          body.destination.startsWith(path + "/") ||
          new TextEncoder().encode(path + body.destination).length > 1018)
      )
        throw new Error("移动目标无效");
      try {
        const result =
          action === "upload"
            ? await this.request(action, path, {
                method: "POST",
                body,
                headers: { "Content-Type": "application/octet-stream" },
              })
            : await this.request(action, null, {
                method: "POST",
                body: { ...body, path },
              });
        if (result.ok !== true) throw new Error("设备未确认修改结果");
        if (expectedBytes !== undefined && result.data?.bytes !== expectedBytes)
          throw new Error("设备确认写入字节数不匹配");
        return result;
      } catch (error) {
        if (![400, 401, 403, 409].includes(error.status))
          this.uncertainUntil = Date.now() + 120000;
        throw error;
      }
    });
  }
}
