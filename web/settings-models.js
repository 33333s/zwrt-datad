import { integer, number } from "./datad-models.js";
export const changed = (a, b) => JSON.stringify(a) !== JSON.stringify(b);
export function ipv4(raw) {
  const parts = String(raw).split(".");
  if (
    parts.length !== 4 ||
    parts.some((x) => !/^(0|[1-9]\d{0,2})$/.test(x) || Number(x) > 255)
  )
    throw new Error("IPv4 地址格式不正确");
  return parts.reduce((a, b) => a * 256 + Number(b), 0);
}
export function lanPayload(current, form) {
  if (!current.supported) throw new Error("设备尚未提供 LAN 配置");
  if (
    !current.address_writable &&
    (form.ip !== current.ip || form.netmask !== current.netmask)
  )
    throw new Error("此设备的网关和掩码只读");
  const gateway = ipv4(form.ip),
    mask = ipv4(form.netmask),
    host = 0xffffffff - mask,
    network = (gateway & mask) >>> 0,
    broadcast = network + host;
  if (mask === 0 || host < 3 || (host & (host + 1)) !== 0)
    throw new Error("子网掩码必须连续，前缀长度为 1–30");
  if (
    gateway === network ||
    gateway === broadcast ||
    gateway / 16777216 < 1 ||
    gateway / 16777216 >= 224 ||
    Math.floor(gateway / 16777216) === 127
  )
    throw new Error("网关地址无效");
  const result = {
    ip: form.ip,
    netmask: form.netmask,
    dhcp_enabled: form.dhcp_enabled,
  };
  if (form.dhcp_enabled) {
    const a = ipv4(form.dhcp_start),
      b = ipv4(form.dhcp_end);
    if (
      a > b ||
      (a & mask) >>> 0 !== network ||
      (b & mask) >>> 0 !== network ||
      a <= network ||
      b >= broadcast ||
      (gateway >= a && gateway <= b)
    )
      throw new Error(
        "地址池须在同一子网内，且不能包含网关、网络地址或广播地址",
      );
    const hours = integer(form.hours, 1, 720, "租期");
    if (hours * 3600 > (current.lease_max_seconds ?? 2592000))
      throw new Error("租期超出设备限制");
    Object.assign(result, {
      dhcp_start: form.dhcp_start,
      dhcp_end: form.dhcp_end,
      lease_seconds: hours * 3600,
    });
  }
  return result;
}
export function lanForm(settings) {
  return {
    ip: settings.ip ?? "",
    netmask: settings.netmask ?? "",
    dhcp_enabled: settings.dhcp_enabled ?? false,
    dhcp_start: settings.dhcp_start ?? "",
    dhcp_end: settings.dhcp_end ?? "",
    hours:
      settings.lease_seconds && settings.lease_seconds % 3600 === 0
        ? String(settings.lease_seconds / 3600)
        : "",
    mtu: settings.mtu == null ? "" : String(settings.mtu),
  };
}
export function limitBytes(raw) {
  const m = String(raw ?? "").match(/^(\d+(?:\.\d+)?)(?:_(\d+(?:\.\d+)?))?$/);
  return m ? Number(m[1]) * Number(m[2] ?? 1) * 1048576 : null;
}
export function usageRows(history, start, end) {
  const day = (raw) =>
    /^\d{4}-\d{2}-\d{2}$/.test(raw) &&
    new Date(raw + "T00:00:00Z").toISOString().slice(0, 10) === raw;
  if (
    !day(start) ||
    !day(end) ||
    start > end ||
    (Date.parse(end) - Date.parse(start)) / 86400000 >= 400
  )
    throw new Error("请选择有效范围，最多 400 天");
  const rows = new Map();
  for (const d of history.days ?? []) {
    if (
      day(d.date) &&
      d.date >= start &&
      d.date <= end &&
      number(d.bytes, 0, 109951162777600) !== null &&
      !rows.has(d.date)
    )
      rows.set(d.date, d);
  }
  return [...rows.values()].sort((a, b) => a.date.localeCompare(b.date));
}
