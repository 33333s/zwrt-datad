import { parseJson } from "./core.js";
import { number } from "./datad-models.js";
const bandwidths = new Set([
  1.4, 3, 5, 10, 15, 20, 25, 30, 40, 50, 60, 70, 80, 90, 100, 200, 400,
]);
const activity = (raw) =>
  raw === "2" ? "active" : ["0", "1"].includes(raw) ? "inactive" : "unknown";
const uplink = (raw) => (raw === "1" ? true : raw === "0" ? false : null);
const int = (s, min, max) =>
  /^\d+$/.test(String(s)) ? number(s, min, max) : null;
function columns(raw) {
  if (!raw?.trim() || raw.length > 16384) return null;
  const rows = raw.trim().replace(/;+$/, "").split(";");
  return rows.length <= 32
    ? rows.map((r) => r.split(",").map((c) => c.trim()))
    : null;
}
const empty = (c) => c.length === 1 && !c[0];
const unique = (rows) =>
  new Set(rows.map((c) => `${c.pci}:${c.channel}`)).size === rows.length
    ? rows
    : null;
function lteRows(raw, signals) {
  const rows = columns(raw);
  if (!rows || rows.some((c) => !empty(c) && c.length !== 6)) return null;
  const s = columns(signals),
    sig = s?.length === rows.length ? s : null,
    result = [];
  for (const [i, c] of rows.entries()) {
    if (empty(c)) continue;
    const [id, pci, state, band, channel, bw] = c;
    if (
      int(id, 0, Infinity) === null ||
      int(pci, 0, 503) === null ||
      !/^[-+]?\d+$/.test(state) ||
      int(band, 1, 256) === null ||
      int(channel, 0, 262143) === null ||
      !bandwidths.has(Number(bw)) ||
      Number(bw) > 20
    )
      return null;
    const v = sig?.[i]?.length === 5 ? sig[i] : [];
    result.push({
      rat: "LTE",
      role: `SCC${i}`,
      band: `B${band}`,
      pci: Number(pci),
      channel: Number(channel),
      bandwidth: `${Number(bw)} MHz`,
      activity: activity(state),
      ulConfigured: uplink(v[4]),
      rsrp: number(v[0], -156, -31),
      rsrq: number(v[1], -43, 20),
      sinr: number(v[2], -30, 40),
      rssi: number(v[3], -140, -10),
    });
  }
  return unique(result);
}
function descriptors(raw, primary, oem, nwinfo) {
  const rows = columns(raw);
  if (!rows) return null;
  const nr = primary.rat === "NR",
    numeric = nr && nwinfo,
    u50 =
      nr &&
      !numeric &&
      (oem ||
        rows
          .filter((c) => !empty(c))
          .every(
            (c) =>
              c.length >= 10 &&
              c.length <= 11 &&
              /^[nN]\d+$/.test(c[3]) &&
              /MHz$/i.test(c[5]),
          ));
  if (
    rows.some(
      (c) =>
        !empty(c) && (u50 ? c.length < 10 || c.length > 11 : c.length !== 11),
    )
  )
    return null;
  const ids = new Set(),
    result = [];
  for (const [i, c] of rows.entries()) {
    if (empty(c)) continue;
    const id = int(c[0], 0, Infinity),
      pci = int(c[1], 0, nr ? 1007 : 503),
      band = int(u50 ? c[3].replace(/^[nN]/, "") : c[3], 1, nr ? 1024 : 256),
      channel = int(c[4], 0, nr ? 3279165 : 262143),
      bw = Number(u50 ? c[5].replace(/MHz$/i, "").trim() : c[5]);
    if (
      id === null ||
      (!u50 && ids.has(id)) ||
      !/^[-+]?\d+$/.test(c[2]) ||
      !/^[-+]?\d+$/.test(c[6]) ||
      pci === null ||
      band === null ||
      channel === null ||
      !bandwidths.has(bw) ||
      (!nr && bw > 20)
    )
      return null;
    ids.add(id);
    const metrics = {
      rsrp: number(c[7], -156, -31),
      rsrq: number(c[8], -43, 20),
      sinr: number(c[9], -30, 40),
      rssi: number(c[10], -140, -10),
    };
    if (!u50 && !numeric && Object.values(metrics).some((v) => v === null))
      return null;
    result.push({
      rat: primary.rat,
      role: `SCC${i}`,
      band: `${nr ? "n" : "B"}${band}`,
      pci,
      channel,
      bandwidth: `${bw} MHz`,
      ...metrics,
      activity: u50 || numeric ? activity(c[2]) : "unknown",
      ulConfigured: u50 || numeric ? uplink(c[6]) : null,
    });
  }
  if (
    !u50 &&
    !numeric &&
    !result.some((c) => c.pci === primary.pci && c.channel === primary.channel)
  )
    return null;
  return unique(result);
}
export function carrierSnapshot(net = {}, model = "") {
  const type = String(net.type ?? "").toUpperCase(),
    nsa = ["ENDC", "LTE-NSA", "NSA"].includes(type),
    nr = nsa || ["SA", "NR", "NR5G", "5G"].includes(type),
    lte = nsa || ["LTE", "LTE_CA", "LTE_A"].includes(type),
    nwinfo =
      net.ca_format === "mu5250_nwinfo" || model.toUpperCase() === "MU5250";
  const carriers = [];
  let unrecognized = false;
  const lteCa = String(
    net.lte_ca_state || ((net.lteca ?? "").startsWith("ca_") ? net.lteca : ""),
  );
  function add(primary, raw = "", alternate = "") {
    const hasPrimary = primary.pci !== null && primary.channel !== null,
      released = raw.trim() === "ca_deconfigured",
      payload = released ? "" : raw,
      signal = released ? "" : alternate;
    let rows =
      primary.rat === "LTE" &&
      payload
        .split(";")
        .find((x) => x.trim())
        ?.split(",").length === 6
        ? lteRows(payload, signal)
        : (descriptors(payload, primary, net.ca_format === "u50_oem", nwinfo) ??
          descriptors(signal, primary, false, false));
    if (
      ((payload.trim() && !payload.startsWith("ca_")) || signal.trim()) &&
      (!rows || !hasPrimary)
    )
      unrecognized = true;
    if (hasPrimary)
      carriers.push({ ...primary, activity: "active", ulConfigured: true });
    if (rows)
      carriers.push(
        ...rows.filter(
          (c) =>
            !hasPrimary ||
            c.pci !== primary.pci ||
            c.channel !== primary.channel,
        ),
      );
  }
  function primary(rat) {
    const p = rat === "NR" ? "nr" : "lte";
    return {
      rat,
      role: rat === "NR" && nsa ? "PSCell" : "PCC",
      band: rat === "NR" ? net.nr_band : net.band,
      pci: number(net[`${p}_pci`], 0, rat === "NR" ? 1007 : 503),
      channel: number(net[`${p}_channel`], 0, rat === "NR" ? 3279165 : 262143),
      bandwidth: net[`${p}_bw`],
      rsrp: number(net[`${p}_rsrp`], -156, -31),
      rsrq: number(net[`${p}_rsrq`], -43, 20),
      sinr: number(net[`${p}_snr`], -30, 40),
      rssi: number(net[`${p}_rssi`], -140, -10),
    };
  }
  if (lte) add(primary("LTE"), net.lteca, net.ltecasig);
  if (nr) add(primary("NR"), net.nrca);
  const state = (raw) =>
    /^\d{1,3}$/.test(String(raw ?? "").trim()) ? String(raw).trim() : "";
  let nrDownlinkCa = nr ? state(net.nr_ca_dl_state) : "",
    nrUplinkCa = nr ? state(net.nr_ca_ul_state) : "";
  const secondary = (rat) =>
    carriers.filter((c) => c.rat === rat && c.role.startsWith("SCC"));
  if (!nrDownlinkCa && nwinfo) {
    const rows = secondary("NR");
    nrDownlinkCa = rows.some((c) => c.activity === "active")
      ? "2"
      : rows.length && rows.every((c) => c.activity === "inactive")
        ? "1"
        : "";
  }
  const caLabel = (raw) =>
    raw === "2"
      ? "已激活"
      : ["0", "1"].includes(raw)
        ? "未激活"
        : raw
          ? `未知（${raw}）`
          : "未知";
  function lteDl(rows) {
    if (["ca_deactivated", "ca_deconfigured"].includes(lteCa)) return "未激活";
    if (rows.some((c) => c.activity === "active")) return "已激活";
    if (rows.length && rows.every((c) => c.activity === "inactive"))
      return "未激活";
    return lteCa === "ca_activated" ? "已激活" : "未知";
  }
  const groups = [...new Set(carriers.map((c) => c.rat))];
  const activation = [];
  for (const rat of groups) {
    const all = carriers.filter((c) => c.rat === rat),
      rows = secondary(rat);
    if (!rows.length) continue;
    if (rat === "LTE")
      activation.push(
        `LTE 下行 CA：${lteDl(rows)} · 上行 CA：${rows.some((c) => c.ulConfigured === true) ? "已配置" : rows.every((c) => c.ulConfigured === false) ? "未配置" : "未知"}`,
      );
    else
      activation.push(
        `NR 下行 CA：${caLabel(nrDownlinkCa)} · 上行 CA：${caLabel(nrUplinkCa)}`,
      );
    if (all.every((c) => ["active", "inactive"].includes(c.activity)))
      activation.push(
        `${rat} 已配置 ${all.length}CC · 激活 ${all.filter((c) => c.activity === "active").length}CC`,
      );
  }
  let summary;
  if (!carriers.length) summary = "等待载波数据";
  else if (unrecognized) summary = `载波待确认 · 已上报 ${carriers.length} 条`;
  else if (nsa)
    summary =
      groups.length < 2
        ? `NSA · 等待${groups.includes("LTE") ? "NR" : "LTE"}载波`
        : groups.every((r) => carriers.filter((c) => c.rat === r).length === 1)
          ? "NSA · 双连接"
          : "NSA · " +
            groups
              .map(
                (r) => `${r} ${carriers.filter((c) => c.rat === r).length}CC`,
              )
              .join(" + ");
  else if (carriers.length === 1)
    summary =
      lteCa === "ca_activated"
        ? "CA 已激活 · SCC 待上报"
        : lteCa === "ca_deactivated"
          ? "CA 未激活 · SCC 待上报"
          : "单载波";
  else {
    const status =
      groups.length === 1 && groups[0] === "LTE"
        ? lteDl(secondary("LTE"))
        : nrDownlinkCa === "2" || nrUplinkCa === "2"
          ? "已激活"
          : ["0", "1"].includes(nrDownlinkCa) && ["0", "1"].includes(nrUplinkCa)
            ? "未激活"
            : ["", "0", "1"].includes(nrDownlinkCa) &&
                ["", "0", "1"].includes(nrUplinkCa) &&
                secondary("NR").length &&
                secondary("NR").every((c) => c.activity === "inactive")
              ? "未激活"
              : "未知";
    summary = `${carriers.length}CC 配置 · ${status === "未知" ? "激活未知" : status === "未激活" ? "未聚合" : status}`;
  }
  return {
    carriers,
    unrecognized,
    nsa,
    nrDownlinkCa,
    nrUplinkCa,
    lteCa,
    summary,
    activation,
  };
}

export function carrierReplay(text) {
  if (text.length > 2000000) throw new Error("回放文件不能超过 2 MB");
  const root = parseJson(text),
    frames = Array.isArray(root) ? root : [root];
  if (!frames.length || frames.length > 500)
    throw new Error("需要 1–500 帧 /state 数据");
  return frames.map((f) => {
    if (!f?.net || typeof f.net.type !== "string")
      throw new Error("每帧需要包含 net.type 的 /state 数据");
    return carrierSnapshot(f.net, f.device?.model_name);
  });
}
