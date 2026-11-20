const KEY = "datad.web.devices.v1";
export function managementUrl(input) {
  const url = new URL(input);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  )
    throw new Error("请输入不含账号、查询参数的 HTTP / HTTPS 管理地址");
  if (!url.pathname.endsWith("/")) url.pathname += "/";
  if (!url.pathname.endsWith("/ui/")) url.pathname += "ui/";
  return url.href;
}
export function deviceProfiles() {
  try {
    return JSON.parse(localStorage.getItem(KEY))
      .filter(
        (p) => typeof p.name === "string" && managementUrl(p.url) === p.url,
      )
      .slice(0, 12);
  } catch {
    return [];
  }
}
export function saveDeviceProfile(url, name) {
  const next = [
    { url: managementUrl(url), name: String(name).trim().slice(0, 80) },
    ...deviceProfiles().filter((p) => p.url !== managementUrl(url)),
  ].slice(0, 12);
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {}
  return next;
}
export function forgetDeviceProfile(url) {
  const next = deviceProfiles().filter((p) => p.url !== url);
  try {
    localStorage.setItem(KEY, JSON.stringify(next));
  } catch {}
  return next;
}
