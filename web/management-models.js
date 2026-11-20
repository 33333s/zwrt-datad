import { integer } from "./datad-models.js";
const controls = /[\x00-\x1f\x7f-\x9f]/;
export const lines = (text) =>
  String(text ?? "")
    .split(/\r?\n/)
    .map((x) => x.trim())
    .filter(Boolean);
export const bytes = (text) => new TextEncoder().encode(text).length;
export function secureUrl(raw, schemes = ["https:"], origin = false) {
  if (!raw) return;
  let url;
  try {
    url = new URL(raw);
  } catch {
    throw new Error("地址格式无效");
  }
  if (
    !schemes.includes(url.protocol) ||
    !url.hostname ||
    url.username ||
    url.password ||
    url.hash ||
    (origin && (url.search || !["", "/"].includes(url.pathname)))
  )
    throw new Error("地址协议、凭据或路径不符合要求");
}
export function forwardPatch(
  config,
  original,
  {
    smtpPassword = "",
    secret = "",
    clearSmtp = false,
    clearSecret = false,
  } = {},
) {
  if (!["smtp", "webhook", "dingtalk", "sms"].includes(config.method))
    throw new Error("请选择转发方式");
  if (
    bytes(config.nickname ?? "") > 255 ||
    controls.test(config.nickname ?? "")
  )
    throw new Error("备注过长或包含控制字符");
  secureUrl(config.webhook_url);
  secureUrl(config.dingtalk_webhook);
  function phones(values, max) {
    if (
      values.length > max ||
      values.some((v) => !/^\+?\d{3,32}$/.test(v) || v.length > 32) ||
      new Set(values.map((v) => v.replace(/^\+/, ""))).size !== values.length
    )
      throw new Error(`号码无效、重复或超过 ${max} 个`);
  }
  phones(config.sms_to_phone ?? [], 3);
  phones(config.blacklist_phone ?? [], 64);
  const keywords = config.blacklist_keywords ?? [];
  if (
    keywords.length > 32 ||
    new Set(keywords).size !== keywords.length ||
    keywords.some((v) => !v.trim() || bytes(v) > 128 || controls.test(v))
  )
    throw new Error("关键词最多 32 个，每项不超过 128 字节且不能重复");
  if (
    smtpPassword.length > 512 ||
    controls.test(smtpPassword) ||
    bytes(secret) > 256 ||
    controls.test(secret)
  )
    throw new Error("渠道凭据格式无效");
  const s = config.smtp ?? {},
    smtpPresent = !clearSmtp && (s.host || s.username || s.to || smtpPassword);
  if (
    smtpPresent &&
    (!s.host?.includes(".") ||
      s.host.includes("/") ||
      ![465, 587].includes(Number(s.port)) ||
      !s.username?.includes("@") ||
      !s.to?.includes("@") ||
      !(s.password_configured || smtpPassword))
  )
    throw new Error("SMTP 须填写服务器域名、465/587 端口、完整邮箱和授权码");
  if (
    config.enabled &&
    !{
      smtp: smtpPresent,
      webhook: !!config.webhook_url,
      dingtalk: !!config.dingtalk_webhook,
      sms: !!config.sms_to_phone?.length,
    }[config.method]
  )
    throw new Error("请先配置当前转发渠道");
  const patch = { enabled: !!config.enabled, method: config.method };
  for (const k of [
    "webhook_url",
    "dingtalk_webhook",
    "sms_to_phone",
    "power_forward_enabled",
    "blacklist_phone",
    "blacklist_keywords",
    "nickname",
    "smtp_forward_device_info",
    "dingtalk_forward_device_info",
    "sms_forward_device_info",
  ])
    if (JSON.stringify(config[k]) !== JSON.stringify(original[k]))
      patch[k] = config[k];
  if (clearSecret || secret) patch.dingtalk_secret = clearSecret ? "" : secret;
  if (
    clearSmtp ||
    JSON.stringify(s) !== JSON.stringify(original.smtp) ||
    smtpPassword
  ) {
    patch.smtp = clearSmtp
      ? { host: "", port: 0, username: "", to: "", password: "" }
      : { host: s.host, port: Number(s.port), username: s.username, to: s.to };
    if (!clearSmtp && smtpPassword) patch.smtp.password = smtpPassword;
  }
  return patch;
}
export function nmsEnrollment(input) {
  try {
    if (!input.trim() || input.length > 16384) throw Error();
    const raw = input.replace(/\s/g, "").replace(/-/g, "+").replace(/_/g, "/");
    if (!/^[A-Za-z0-9+/]+={0,2}$/.test(raw)) throw Error();
    const data = Uint8Array.from(atob(raw), (c) => c.charCodeAt(0));
    if (data.length > 8192) throw Error();
    const obj = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(data),
    );
    if (
      !/^enr_[0-9a-f]{32}$/.test(obj.username) ||
      typeof obj.password !== "string" ||
      !obj.password.trim() ||
      obj.password.length > 4096 ||
      controls.test(obj.password)
    )
      throw Error();
    return { username: obj.username, password: obj.password };
  } catch {
    throw new Error("接入码格式无效，请完整复制 NMS 提供的 Base64 接入码");
  }
}
export function validateWebServices(services) {
  if (
    services.length > 8 ||
    new Set(services.map((v) => Number(v.port))).size !== services.length
  )
    throw new Error("后台最多 8 个，端口不能重复");
  for (const s of services.filter((v) => v.kind === "web")) {
    if (!s.name?.trim() || bytes(s.name) > 80 || controls.test(s.name))
      throw new Error("后台名称无效");
    integer(s.port, 1, 65535, "后台端口");
    if ([9460, 9461].includes(Number(s.port)))
      throw new Error("不能使用 9460/9461 端口");
  }
}
export function nmsPayload(
  config,
  { password = "", clearPassword = false, passwordConfigured = false } = {},
) {
  const interval = integer(
    config.report_interval_seconds,
    10,
    3600,
    "上报间隔",
  );
  secureUrl(config.broker, ["wss:", "ssl:"]);
  secureUrl(config.platform_url, ["https:"], true);
  if (config.remote_origins.length > 4) throw new Error("备用地址最多 4 个");
  config.remote_origins.forEach((v) => secureUrl(v, ["https:"], true));
  const services = config.services ?? [];
  validateWebServices(services);
  if (config.enabled) {
    if (
      !config.broker ||
      !config.username ||
      !(password || passwordConfigured) ||
      clearPassword
    )
      throw new Error("启用前请填写 MQTT 地址、用户名和密码");
    for (const k of ["vendor", "model", "identity"])
      if (!config[k] || /[\s/+#]/.test(config[k]))
        throw new Error("请填写有效的厂商、型号和设备标识");
    if (config.remote_enabled && !config.platform_url)
      throw new Error("远程访问需要平台地址");
  }
  return Object.fromEntries([
    ...[
      "enabled",
      "broker",
      "platform_url",
      "remote_origins",
      "username",
      "ca_pem",
      "vendor",
      "model",
      "identity_type",
      "identity",
      "platform",
      "remote_enabled",
      "remote_panel_control_enabled",
    ].map((k) => [k, config[k]]),
    ["report_interval_seconds", interval],
    ["password", password],
    ["clear_password", clearPassword],
    [
      "web_services",
      services
        .filter((v) => v.kind === "web")
        .map((v) => ({ name: v.name, port: Number(v.port), kind: "web" })),
    ],
  ]);
}
