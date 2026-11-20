// Pure ports of ZwrtDatadProtocol, ZwrtSms and ZwrtApn; no UI or network side effects.
export const MODES = [
  { value: "auto", read: ["AUTO", "WL_AND_5G", "4G_AND_5G"], label: "自动" },
  { value: "4G", read: ["Only_LTE"], label: "仅 4G" },
  { value: "5G", read: ["Only_5G"], label: "仅 5G SA" },
  { value: "LTE_AND_5G", read: ["LTE_AND_5G"], label: "仅 5G NSA" },
];
export const modeOf = (raw) =>
  MODES.find((m) => m.read.includes(raw) || m.value === raw)?.value ?? null;
export function number(value, min = -Infinity, max = Infinity) {
  if (value === null || value === undefined || String(value).trim() === "")
    return null;
  const n = Number(value);
  return Number.isFinite(n) && n >= min && n <= max ? n : null;
}
export function integer(value, min, max, label = "数值") {
  const raw = String(value ?? "");
  if (
    !/^\d+$/.test(raw) ||
    !Number.isSafeInteger(Number(raw)) ||
    Number(raw) < min ||
    Number(raw) > max
  )
    throw new Error(`${label}须为 ${min}–${max} 的整数`);
  return Number(raw);
}
export function serving(state) {
  const n = state?.net ?? {},
    type = String(n.type ?? "").toUpperCase(),
    nsa = ["ENDC", "LTE-NSA", "NSA"].includes(type),
    nr = nsa || ["SA", "NR", "NR5G", "5G"].includes(type);
  if (!type) return null;
  const prefix = nr ? "nr" : "lte";
  return {
    rat: nr ? "NR" : "LTE",
    label: nsa
      ? "5G NSA"
      : nr
        ? "5G NR"
        : type.startsWith("LTE")
          ? "4G LTE"
          : type,
    nsa,
    band: nr ? n.nr_band : n.band,
    bandwidth: n[`${prefix}_bw`],
    pci: number(n[`${prefix}_pci`], 0, nr ? 1007 : 503),
    arfcn: number(n[`${prefix}_channel`], 0, nr ? 3279165 : 262143),
    cellId: n[`${prefix}_cell_id`],
    tac: n.nr_tac,
    rsrp: number(n[`${prefix}_rsrp`], -156, -31),
    rsrq: number(n[`${prefix}_rsrq`], -43, 20),
    sinr: number(n[`${prefix}_snr`], -30, 40),
    rssi: number(n[`${prefix}_rssi`], -140, -10),
    anchor: nsa
      ? {
          rat: "LTE",
          band: n.band,
          pci: n.lte_pci,
          arfcn: n.lte_channel,
          bandwidth: n.lte_bw,
          rsrp: n.lte_rsrp,
          rsrq: n.lte_rsrq,
          sinr: n.lte_snr,
          cellId: n.lte_cell_id,
        }
      : null,
  };
}
export function neighbors(state) {
  const net = state?.net ?? {},
    type = String(net.type ?? "").toUpperCase(),
    nr = ["SA", "NR", "NR5G", "5G", "ENDC", "LTE-NSA", "NSA"].includes(type),
    cell = serving(state);
  return (state?.neighbor?.cells ?? [])
    .map((c) => ({
      ...c,
      rat: String(c.rat).toUpperCase().includes("NR") ? "NR" : "LTE",
      rsrp: number(c.rsrp_dbm, -156, -31),
      sinr: number(c.sinr_db, -30, 40),
    }))
    .filter(
      (c) =>
        (nr || c.rat === "LTE") &&
        !(c.rat === cell?.rat && c.pci === cell.pci && c.arfcn === cell.arfcn),
    )
    .sort(
      (a, b) =>
        (a.rat === b.rat ? 0 : a.rat === "NR" ? -1 : 1) ||
        (b.rsrp ?? -Infinity) - (a.rsrp ?? -Infinity),
    );
}
export const bandList = (raw) =>
  [
    ...new Set(
      String(raw ?? "")
        .split(/[,\s]+/)
        .filter((x) => /^\d+$/.test(x))
        .map(Number),
    ),
  ].sort((a, b) => a - b);
export function lockedBands(net, key) {
  return bandList(
    key === "lte"
      ? net.lte_band || net.lte_bands
      : key === "nr_sa"
        ? net.nr5g_sa_band_lock || net.sa_bands
        : net.nr5g_nsa_band_lock || net.nsa_bands,
  );
}
export function smsRows(state) {
  const rows = state?.sms?.list ?? [],
    unique = new Map();
  for (const r of rows) {
    const id = String(r.id ?? "");
    if (!id) continue;
    const tag = r.tag === null || r.tag === undefined ? null : Number(r.tag);
    const date = String(r.date ?? "")
      .trim()
      .replace(
        /^(\d{2}),(\d{1,2}),(\d{1,2}),(\d{1,2}),(\d{1,2}),(\d{1,2}),.+$/,
        (_, y, m, d, h, i, s) =>
          `20${y}-${m.padStart(2, "0")}-${d.padStart(2, "0")} ${h.padStart(2, "0")}:${i.padStart(2, "0")}:${s.padStart(2, "0")}`,
      );
    unique.set(id, {
      ...r,
      id,
      date,
      content: String(r.text ?? ""),
      number: String(r.num ?? ""),
      mailbox:
        tag === 0 || tag === 1
          ? "inbox"
          : tag === 2 || tag === 3
            ? "sent"
            : tag === 4
              ? "draft"
              : "unknown",
      failed: tag === 3,
    });
  }
  return [...unique.values()].sort(
    (a, b) =>
      b.date.localeCompare(a.date) ||
      b.id.localeCompare(a.id, undefined, { numeric: true }),
  );
}
export function smsSenderOptions(state, caps) {
  return state?.sim?.dual_sim === 2 && caps?.controls?.includes("sim.set_slot")
    ? [
        { value: "host", label: "当前 SIM" },
        { value: "sim1", label: "SIM 1" },
        { value: "sim2", label: "SIM 2" },
      ]
    : [{ value: "host", label: "当前 SIM" }];
}
export function smsPayload(
  number,
  text,
  sender,
  state,
  caps,
  now = new Date(),
) {
  if (!smsSenderOptions(state, caps).some((s) => s.value === sender))
    throw new Error("当前设备不支持所选发送卡槽，请重新选择");
  if (!/^\+?[\d*#]+$/.test(number.trim()))
    throw new Error("请输入有效收件号码");
  if (!text.trim()) throw new Error("请输入短信正文");
  let encoded = "";
  for (let i = 0; i < text.length; i++)
    encoded += text.charCodeAt(i).toString(16).padStart(4, "0");
  const pad = (n) => String(n).padStart(2, "0");
  return {
    sender,
    number: number.trim(),
    message_hex: encoded.toUpperCase(),
    sms_time: [
      pad(now.getFullYear() % 100),
      pad(now.getMonth() + 1),
      pad(now.getDate()),
      pad(now.getHours()),
      pad(now.getMinutes()),
      pad(now.getSeconds()),
      "+",
      "0",
    ].join(";"),
    ID: "-1",
    encode_type: "UNICODE",
  };
}
export function apnRows(view) {
  return [
    ...(String(view.mode?.apn_mode) === "1"
      ? []
      : (view.automatic?.apnListArray ?? [])
    ).map((row) => ({ ...row, automatic: true })),
    ...(view.manual?.apnListArray ?? []).map((row) => ({
      ...row,
      automatic: false,
    })),
  ];
}
export function apnPayload(form, id) {
  if (!form.apn?.trim()) throw new Error("APN 不能为空");
  const p = {
    name: form.name?.trim() || form.apn.trim(),
    apn: form.apn.trim(),
    pdp_type: integer(form.pdp_type, 1, 3, "PDP 类型"),
  };
  if (id) p.profile_id = id;
  for (const k of ["username", "password"])
    if (!id || form[k]) p[k] = form[k] ?? "";
  if (Number(form.auth_mode) >= 0)
    p.auth_mode = integer(form.auth_mode, 0, 3, "认证方式");
  else if (!id) p.auth_mode = 0;
  return p;
}
export function manualApn(view, id) {
  if (view.writable === false) throw new Error("当前 APN 配置只读");
  const row = (view.manual?.apnListArray ?? []).find((r) => r.profileId === id);
  if (!row) throw new Error("手动 APN 已变化，请刷新列表");
  return row.profileId;
}
export function cellLockPayload(form) {
  const nr = form.rat === "NR";
  const p = {
    pci: integer(form.pci, 0, nr ? 1007 : 503, "PCI"),
    [nr ? "arfcn" : "earfcn"]: integer(
      form.arfcn,
      1,
      nr ? 3279165 : 262143,
      "频点",
    ),
  };
  if (nr) p.band = integer(form.band, 1, 512, "NR 频段");
  return p;
}
export function networkScore(cell) {
  if (!cell) return null;
  const points = [
    [
      "rsrp",
      0.4,
      [
        [-120, 0],
        [-110, 3.5],
        [-100, 6.5],
        [-90, 8.5],
        [-80, 10],
      ],
    ],
    [
      "rsrq",
      0.25,
      [
        [-20, 0],
        [-14, 3.5],
        [-11, 6.5],
        [-8, 8.5],
        [-3, 10],
      ],
    ],
    [
      "sinr",
      0.35,
      [
        [-5, 0],
        [0, 3.5],
        [13, 6.5],
        [20, 8.5],
        [30, 10],
      ],
    ],
  ];
  let total = 0,
    weight = 0;
  for (const [key, w, p] of points) {
    const n = number(cell[key]);
    if (n === null) continue;
    let score = n <= p[0][0] ? p[0][1] : p.at(-1)[1];
    for (let i = 1; i < p.length; i++)
      if (n > p[i - 1][0] && n <= p[i][0])
        score =
          p[i - 1][1] +
          ((n - p[i - 1][0]) / (p[i][0] - p[i - 1][0])) *
            (p[i][1] - p[i - 1][1]);
    total += score * w;
    weight += w;
  }
  return weight ? total / weight : null;
}

export function localizedOperator(net) {
  const plmn = String(net?.plmn ?? "").slice(0, 5),
    op = String(net?.operator ?? "");
  return (
    {
      46000: "中国移动",
      46002: "中国移动",
      46004: "中国移动",
      46007: "中国移动",
      46008: "中国移动",
      46001: "中国联通",
      46006: "中国联通",
      46009: "中国联通",
      46003: "中国电信",
      46005: "中国电信",
      46011: "中国电信",
      46015: "中国广电",
    }[plmn] ??
    {
      CMCC: "中国移动",
      CHINAMOBILE: "中国移动",
      MOBILE: "中国移动",
      UNICOM: "中国联通",
      CUCC: "中国联通",
      CHINAUNICOM: "中国联通",
      CHINANET: "中国电信",
      CTCC: "中国电信",
      TELECOM: "中国电信",
      CHINATELECOM: "中国电信",
      CHNCT: "中国电信",
    }[op.toUpperCase().replace(/[- ]/g, "")] ??
    (op || null)
  );
}
