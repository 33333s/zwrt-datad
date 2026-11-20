import { emptyQos, updateQos, registeredNetwork, dataSession } from "./qos.js";
import {
  emptyMeasurements,
  updateMeasurements,
  measurementNeighbors,
  updateEngineering,
  updateLinkConfig,
  nrCellIdentity,
  highSpeedRail,
} from "./rrc-context.js";
import { emptyNrPower, correlateNrPower } from "./radio-context.js";
import { lteCell } from "./lte.js";
import { plmn } from "./nas.js";
import { serving } from "./datad-models.js";
const fmt = (n) => (n == null ? "—" : String(n)),
  pct = (n) => (n == null ? null : n.toFixed(2) + "%");
export const createCards = (patch = {}) => ({
  cell: null,
  plmn: null,
  broadcastPlmns: [],
  selectedPlmnIndex: null,
  vendor: null,
  vendorDetail: null,
  vendorCell: null,
  ambr: null,
  ambrName: null,
  qci: null,
  qosApn: null,
  bearerQos: emptyQos(),
  bler: null,
  mcs: null,
  modulation: null,
  sector: null,
  band: null,
  dlBw: null,
  ulBw: null,
  dlArfcn: null,
  ulArfcn: null,
  cellParams: [],
  engineering: {},
  caState: null,
  txPower: null,
  pusch: null,
  pucch: null,
  radio: null,
  lastNrAt: 0,
  security: null,
  neighbors: [],
  lteMeasurements: emptyMeasurements(),
  nrLinkConfig: { ports: {}, cellGroupId: null },
  nrUplinkPower: emptyNrPower(),
  servingMeasurement: null,
  measurementSnapshot: null,
  timingAdvance: null,
  pendingTimingAdvance: null,
  linkParams: {},
  updatedAt: 0,
  ...patch,
});
function qosState(c, state) {
  const selected = dataSession(state);
  return {
    ...c,
    bearerQos: state,
    qosApn: selected?.apn ?? null,
    qci: selected?.qci ?? null,
    ambr: selected?.ambr ?? null,
    ambrName: selected?.source ?? null,
  };
}
export const registerCardsNetwork = (c, type) =>
  qosState(c, registeredNetwork(c.bearerQos, type));
export const reconnectCards = (c) =>
  qosState(
    {
      ...c,
      lteMeasurements: emptyMeasurements(),
      nrLinkConfig: { ports: {}, cellGroupId: null },
      nrUplinkPower: emptyNrPower(),
      measurementSnapshot: null,
      timingAdvance: null,
      pendingTimingAdvance: null,
      pusch: null,
      pucch: null,
      txPower: null,
    },
    emptyQos(c.bearerQos.activeFamily),
  );
export function mergeNeighbors(c, incoming) {
  const cells = new Map();
  for (const cell of [...c.neighbors, ...incoming]) {
    if (
      cell.pci === c.cell?.pci &&
      (cell.arfcn == null || cell.arfcn === c.cell.arfcn)
    )
      continue;
    const key = `${cell.pci}:${cell.arfcn}`,
      prior = cells.get(key);
    cells.set(key, {
      ...cell,
      rsrp: cell.rsrp ?? prior?.rsrp ?? null,
      rsrq: cell.rsrq ?? prior?.rsrq ?? null,
      beams: cell.beams ?? [],
    });
  }
  return [...cells.values()];
}
function power(c, channel, dbm, source, details, now, timestamp = null) {
  if (
    !["PUSCH", "PUCCH"].includes(channel) ||
    !Number.isFinite(dbm) ||
    dbm < -50 ||
    dbm > 35
  )
    return c;
  const key = channel === "PUSCH" ? "pusch" : "pucch",
    previous = c[key];
  if (
    timestamp != null &&
    previous?.diagTimestamp != null &&
    BigInt(timestamp) < BigInt(previous.diagTimestamp)
  )
    return c;
  return {
    ...c,
    [key]: {
      dbm,
      source,
      details,
      history: [dbm, ...(previous?.history ?? [])].slice(0, 6),
      updatedAt: now,
      diagTimestamp: timestamp,
    },
    ...(channel === "PUSCH" ? { txPower: `${dbm} dBm` } : {}),
  };
}
const LTE_EVENTS = new Set([
  0xb0c0, 0xb0c1, 0xb0c2, 0xb16e, 0xb16f, 0xb173, 0xb193, 0xb0e2, 0xb0e3,
  0xb0ec, 0xb0ed,
]);
export function applyCards(previous, code, event) {
  if (!event.diag_valid) return previous;
  let c = previous;
  const now = event.ts,
    kind = event.m?.kind,
    value = event.m?.value,
    tree = event.tree,
    lte = !!event.decodeEvidence && LTE_EVENTS.has(code),
    nr = kind === "nrServing" || (code === 0xb821 && !!tree);
  if (nr)
    c =
      c.radio === "LTE"
        ? qosState(
            createCards({
              radio: "NR",
              lastNrAt: now,
              pendingTimingAdvance: c.pendingTimingAdvance,
            }),
            c.bearerQos,
          )
        : { ...c, radio: "NR", lastNrAt: now };
  if (lte) {
    if (c.radio === "NR" && now - c.lastNrAt < 10000) {
      if (tree)
        c = { ...c, nrLinkConfig: updateLinkConfig(c.nrLinkConfig, tree) };
      if (event.nas) c = qosState(c, updateQos(c.bearerQos, event.nas));
      return c === previous ? c : { ...c, updatedAt: now };
    }
    if (c.radio !== "LTE")
      c = qosState(
        createCards({
          radio: "LTE",
          pendingTimingAdvance: c.pendingTimingAdvance,
        }),
        c.bearerQos,
      );
  }
  let cell = event.cell;
  if (
    cell?.band === "NR" &&
    c.cell?.arfcn === cell.arfcn &&
    c.cell?.pci === cell.pci
  )
    cell = { ...cell, band: c.cell.band };
  if (cell && !(c.radio === "NR" && code >= 0xb000 && code <= 0xb0ff))
    c = { ...c, cell };
  if (
    cell &&
    (((code === 0xb821 || code === 0xb823) && c.radio === "NR") ||
      ([0xb0c0, 0xb0c1, 0xb0c2].includes(code) && c.radio === "LTE"))
  ) {
    const key = `${c.radio}:${cell.arfcn}:${cell.pci}`;
    if (c.vendorCell !== key)
      c = {
        ...c,
        timingAdvance: c.vendorCell === null ? c.timingAdvance : null,
        vendor: null,
        vendorDetail: null,
        vendorCell: key,
        servingMeasurement: null,
        measurementSnapshot: null,
        neighbors: [],
        cellParams: [],
        sector: null,
        dlBw: null,
        ulBw: null,
        dlArfcn: null,
        ulArfcn: null,
        caState: null,
        pusch: null,
        pucch: null,
        txPower: null,
        bler: null,
        mcs: null,
        modulation: null,
        linkParams: {},
        lteMeasurements: emptyMeasurements(),
        nrLinkConfig: { ports: {}, cellGroupId: null },
        nrUplinkPower: emptyNrPower(),
        engineering: {},
        band: cell.band,
        plmn: null,
        broadcastPlmns: [],
        selectedPlmnIndex: null,
      };
  }
  if (event.vendorFingerprint?.radio === c.radio)
    c = {
      ...c,
      vendor: event.vendorFingerprint.vendor,
      vendorDetail: event.vendorFingerprint.detail,
    };
  if (event.broadcastPlmns) {
    const listChanged =
      c.broadcastPlmns.length &&
      JSON.stringify(c.broadcastPlmns) !== JSON.stringify(event.broadcastPlmns);
    c = {
      ...c,
      broadcastPlmns: event.broadcastPlmns,
      selectedPlmnIndex: listChanged ? null : c.selectedPlmnIndex,
    };
    if (
      c.plmn === null &&
      c.selectedPlmnIndex &&
      c.broadcastPlmns[c.selectedPlmnIndex - 1]
    )
      c = { ...c, plmn: c.broadcastPlmns[c.selectedPlmnIndex - 1] };
  }
  if (event.selectedPlmnIndex != null) {
    const selected = c.broadcastPlmns[event.selectedPlmnIndex - 1],
      old = c.plmn;
    c = {
      ...c,
      selectedPlmnIndex: event.selectedPlmnIndex,
      plmn: selected
        ? {
            ...selected,
            tac:
              old?.mcc === selected.mcc && old?.mnc === selected.mnc
                ? old.tac
                : null,
          }
        : null,
    };
  }
  if (event.plmn) {
    const p = event.plmn;
    c = {
      ...c,
      plmn: {
        ...p,
        tac:
          p.tac ??
          (c.plmn?.mcc === p.mcc && c.plmn?.mnc === p.mnc ? c.plmn.tac : null),
      },
    };
  }
  if (event.nas) c = qosState(c, updateQos(c.bearerQos, event.nas));
  if (c.pendingTimingAdvance) {
    const pending = c.pendingTimingAdvance;
    if (now - pending.updatedAt > 60000)
      c = { ...c, pendingTimingAdvance: null };
    else if (pending.radio === c.radio)
      c = {
        ...c,
        timingAdvance:
          !c.timingAdvance ||
          BigInt(pending.diagTimestamp) >= BigInt(c.timingAdvance.diagTimestamp)
            ? pending
            : c.timingAdvance,
        pendingTimingAdvance: null,
      };
  }
  if (kind === "ta") {
    if (value.radio !== c.radio) c = { ...c, pendingTimingAdvance: value };
    else if (
      !c.timingAdvance ||
      BigInt(value.diagTimestamp) >= BigInt(c.timingAdvance.diagTimestamp)
    )
      c = { ...c, timingAdvance: value };
  }
  if (event.security) c = { ...c, security: event.security };
  if (kind === "ltePower")
    for (const reading of value)
      c = power(
        c,
        reading.channel,
        reading.dbm,
        `LTE ${event.code} · 推导`,
        [["SFN / 子帧", `${reading.sfn} / ${reading.subframe}`]],
        now,
      );
  if (kind === "lteServing")
    c = {
      ...c,
      cell: lteCell(value.pci, value.dl, value.band),
      plmn: event.plmn,
      band: `B${value.band}`,
      dlBw: value.dlBw,
      ulBw: value.ulBw,
      dlArfcn: String(value.dl),
      ulArfcn: String(value.ul),
      sector: `ECI ${value.cid}`,
      caState: null,
      cellParams: [
        ["制式", "LTE"],
        ["PCI", value.pci],
        ["频段", `B${value.band}`],
        ["下行 EARFCN", value.dl],
        ["上行 EARFCN", value.ul],
        ["下行带宽", value.dlBw],
        ["上行带宽", value.ulBw],
        ["ECI", value.cid],
        ["TAC", event.plmn.tac],
        ["PLMN", `${value.mcc}-${value.mnc}`],
      ],
    };
  if (kind === "lteMib")
    c = { ...c, cell: lteCell(value.pci, value.arfcn), dlBw: value.bandwidth };
  if (code === 0xb0c0 && event.cell)
    c = { ...c, cell: event.cell, band: event.cell.band };
  if (code === 0xb193 && kind === "lteLog") {
    const cells = (value.cells ?? []).map((r) => ({
      pci: r.pci,
      rsrp: r.rsrp[0] ?? null,
      rsrq: r.rsrq[0] ?? null,
      arfcn: value.earfcn,
      beams: [],
    }));
    if (c.radio === "LTE")
      c = {
        ...c,
        measurementSnapshot: { radio: "LTE", cells, updatedAt: now },
      };
    if (!c.cell && value.earfcn != null && cells.length === 1)
      c = { ...c, cell: lteCell(cells[0].pci, value.earfcn) };
    const measured =
      value.earfcn === c.cell?.arfcn
        ? cells.find((r) => r.pci === c.cell.pci)
        : null;
    if (measured)
      c = {
        ...c,
        servingMeasurement: {
          rsrp: measured.rsrp,
          rsrq: measured.rsrq,
          source: "LTE B193 位域推导",
          updatedAt: now,
        },
      };
    const neighbors = cells.filter(
      (r) => r.pci !== c.cell?.pci || r.arfcn !== c.cell?.arfcn,
    );
    if (neighbors.length) c = { ...c, neighbors: mergeNeighbors(c, neighbors) };
  }
  if (c.radio === "NR") {
    if (code === 0xb97f && event.neighbors)
      c = {
        ...c,
        measurementSnapshot: {
          radio: "NR",
          cells: event.neighbors,
          updatedAt: now,
        },
      };
    const measured = event.neighbors?.find(
      (r) => r.pci === c.cell?.pci && r.arfcn === c.cell?.arfcn,
    );
    if (measured)
      c = {
        ...c,
        servingMeasurement: {
          rsrp: measured.rsrp,
          rsrq: measured.rsrq,
          snr: c.servingMeasurement?.snr ?? null,
          source: "NR 测量",
          updatedAt: now,
        },
      };
    if (event.servingSnr)
      c = {
        ...c,
        servingMeasurement: {
          rsrp: c.servingMeasurement?.rsrp ?? null,
          rsrq: c.servingMeasurement?.rsrq ?? null,
          snr: event.servingSnr.snr,
          source: "NR B8D8 SNR / 小区测量",
          updatedAt: now,
        },
      };
  }
  if (kind === "pdschStats" && c.radio === "NR")
    c = {
      ...c,
      bler: pct(value.bler),
      linkParams: {
        ...c.linkParams,
        "累计 BLER": pct(value.bler) ?? "—",
        "统计载波 ID": value.carrierIds.join(", "),
        "PDSCH 解码数": value.decoded,
        "CRC 成功 TB": value.crcPass,
        "CRC 失败 TB": value.crcFail,
        "成功 TB 字节": value.passBytes,
        "失败 TB 字节": value.failBytes,
        重传计数: value.retransmissions,
        "ACK 视为 NACK": value.ackAsNack,
        "HARQ 失败": value.harqFailures,
        累计时隙: value.slots,
      },
    };
  if (kind === "pdschStatus" && c.radio !== "LTE") {
    const reading = value
      .flatMap((r) => r.readings)
      .findLast((r) => r.pci === c.cell?.pci && r.arfcn === c.cell?.arfcn);
    if (reading)
      c = {
        ...c,
        mcs: String(reading.mcs),
        modulation: reading.modulation,
        linkParams: {
          ...c.linkParams,
          "MCS 来源": "NR B887 PDSCH 状态",
          调制编码: reading.modulationCode,
          "DL RB": reading.rb,
          "DL layers": reading.layers,
          "Rx 接收链（UE）": reading.rxChains ?? "未识别",
        },
      };
  }
  if (kind === "ulSchedule" && c.radio === "NR") {
    const grants = value.flatMap((r) => (r.grant ? [r.grant] : [])),
      last = grants.at(-1);
    c = {
      ...c,
      linkParams: {
        ...c.linkParams,
        "UL grants（本包）": grants.length,
        ...(last
          ? { "UL RB": last.rb, "UL RB 起点": last.rbStart, "UL MCS": last.mcs }
          : {}),
      },
    };
  }
  if (
    [0xb884, 0xb8d1].includes(code) &&
    c.radio === "NR" &&
    event.diag_timestamp
  ) {
    const update = correlateNrPower(
      c.nrUplinkPower,
      code === 0xb884 ? "control" : "rf",
      event.diag_timestamp,
      kind === "powerControl" || kind === "txRf" ? value : [],
    );
    c = { ...c, nrUplinkPower: update.state };
    for (const r of update.readings) {
      const p = r.rf.power;
      c = power(
        c,
        r.channel,
        p.dbm,
        "NR B8D1 RF · B884 时隙关联",
        [
          ["RF 链功率上限", `${p.limitDbm} dBm`],
          ["MAC 功控上限", `${r.mac.limitDbm} dBm`],
          ["限幅前请求", `${r.mac.requestedDbm} dBm`],
          ["SFN / 时隙", `${r.rf.sfn} / ${r.rf.slot}`],
          ["发射链", p.chain],
          ["说明", "RF目标功率，非仪表实测"],
        ],
        now,
        r.timestamp,
      );
    }
  }
  if (kind === "txRf" && c.radio === "NR" && value.length) {
    const mask = value.at(-1).chainMask;
    c = {
      ...c,
      linkParams: {
        ...c.linkParams,
        "Tx 发射链（UE）": (mask & 1) + ((mask >>> 1) & 1),
        "Tx 链掩码": `0x${mask.toString(16)} · RF 命令`,
      },
    };
  }
  if (c.radio === "NR" && tree && event.decodeEvidence === "ASN1_SCHEMA")
    c = { ...c, nrLinkConfig: updateLinkConfig(c.nrLinkConfig, tree) };
  if (kind === "ltePdsch") {
    const fail = value.filter((r) => !r.crc).length,
      bler = pct((100 * fail) / value.length),
      last = value.at(-1);
    c = {
      ...c,
      bler,
      mcs: String(last.mcs),
      modulation: last.modulation,
      linkParams: {
        来源: "LTE B173 v48 · 推导",
        "本包 BLER": bler,
        "本包 TB": value.length,
        "CRC 失败": fail,
        "最近 MCS": last.mcs,
        调制: last.modulation,
        RB: last.rb,
        说明: "MCS 29–31 为重传保留索引；BLER 为本包统计",
      },
    };
  }
  let incoming = event.neighbors;
  if (code === 0xb0c0 && c.radio === "LTE" && tree) {
    const context = updateMeasurements(c.lteMeasurements, tree);
    c = { ...c, lteMeasurements: context };
    incoming = measurementNeighbors(context, tree);
  }
  if (incoming && !(c.radio === "NR" && code === 0xb0c0))
    c = { ...c, neighbors: mergeNeighbors(c, incoming) };
  if (
    tree &&
    event.decodeEvidence === "ASN1_SCHEMA" &&
    ((code === 0xb0c0 && c.radio === "LTE") ||
      (code === 0xb821 && c.radio === "NR"))
  )
    c = { ...c, engineering: updateEngineering(c.engineering, c.radio, tree) };
  if (kind === "ulTb" && c.radio === "NR")
    c = {
      ...c,
      linkParams: {
        ...c.linkParams,
        "上行新传 TB": value.newTb,
        "上行重传 TB": value.retransmitTb,
        上行新传字节: value.newBytes,
        上行重传字节: value.retransmitBytes,
        上行累计调度次数: value.scheduled,
        上行重传占比: pct(value.retransmitPercent) ?? "—",
      },
    };
  if (kind === "nrServing")
    c = {
      ...c,
      sector: `NCI ${value.cellId}`,
      band: `n${value.band}`,
      dlBw: `${value.dlBwMhz}MHz`,
      ulBw: `${value.ulBwMhz}MHz`,
      dlArfcn: String(value.dlArfcn),
      ulArfcn: String(value.ulArfcn),
      cellParams: [
        ["频段", `n${value.band}`],
        ["下行", `ARFCN ${value.dlArfcn} · ${value.dlBwMhz}MHz`],
        ["上行", `ARFCN ${value.ulArfcn} · ${value.ulBwMhz}MHz`],
        [
          "Cell ID",
          `${value.cellId} (0x${BigInt(value.cellId).toString(16).toUpperCase()})`,
        ],
        ["PLMN", `${value.mcc}-${value.mnc}`],
        ["TAC", `${value.tac} (0x${value.tac.toString(16).toUpperCase()})`],
        ["PCI", value.pci],
        ["载波状态", c.caState ?? "—"],
      ],
    };
  if (code === 0xb821 && event.decodeEvidence === "ASN1_SCHEMA") {
    const nci = nrCellIdentity(tree);
    if (nci) c = { ...c, sector: `NCI ${nci}` };
  }
  return c === previous ? previous : { ...c, updatedAt: now };
}
export const measuredServing = (c) =>
  c.measurementSnapshot?.radio === c.radio
    ? c.measurementSnapshot.cells.find(
        (r) => r.pci === c.cell?.pci && r.arfcn === c.cell?.arfcn,
      )
    : null;
export function sameModulo3(c) {
  const serving = measuredServing(c);
  return serving
    ? c.measurementSnapshot.cells.filter(
        (n) =>
          n.pci !== serving.pci &&
          n.arfcn != null &&
          n.arfcn === serving.arfcn &&
          n.pci % 3 === serving.pci % 3,
      )
    : [];
}
export function beamRows(beams = []) {
  if (!beams.length) return [];
  const strongest = beams.reduce((a, b) => (a.rsrp >= b.rsrp ? a : b));
  return [
    ["已测 SSB / beam", beams.length],
    ["最强已测 SSB", `#${strongest.index} · ${strongest.rsrp} dBm`],
    ...beams
      .slice()
      .sort((a, b) => a.index - b.index)
      .flatMap((b) => [
        [`SSB #${b.index}`, `RSRP ${b.rsrp} dBm · RSRQ ${b.rsrq} dB`],
        [
          `#${b.index} 分支 RSRP`,
          b.branchRsrp.map((p, i) => `${i}: ${p}`).join(" / ") + " dBm",
        ],
      ]),
  ];
}
export function neighborRows(c, cell) {
  const snapshot =
      c.measurementSnapshot?.radio === c.radio ? c.measurementSnapshot : null,
    reference = measuredServing(c),
    measured = snapshot?.cells.find(
      (r) => r.pci === cell.pci && r.arfcn === cell.arfcn,
    ),
    reason = !c.cell
      ? "等待服务小区"
      : cell.arfcn == null
        ? "缺少频道，无法判断同频"
        : cell.arfcn !== c.cell.arfcn
          ? "异频，不作同频模三比较"
          : !reference || !measured
            ? "缺少同一次测量，仅保留历史邻区"
            : cell.pci % 3 === c.cell.pci % 3
              ? "同频同模3 · 潜在干扰线索"
              : "同频不同模3";
  return [
    ["频道", cell.arfcn ?? "未解析（缺少频点或测量配置）"],
    ["RSRP", cell.rsrp == null ? "未上报" : `${cell.rsrp} dBm`],
    ["RSRQ", cell.rsrq == null ? "未上报" : `${cell.rsrq} dB`],
    ["PCI 模3", cell.pci % 3],
    ["模三对比", reason],
    ...(reference?.rsrp != null &&
    measured?.rsrp != null &&
    reference.arfcn === measured.arfcn
      ? [
          [
            "服务 − 邻区",
            `${Math.round((reference.rsrp - measured.rsrp) * 10) / 10} dB`,
          ],
        ]
      : []),
    ...beamRows(cell.beams),
  ];
}
const NR_LINK_ROWS = [
  "DL RB",
  "DL layers",
  "UL grants（本包）",
  "UL RB",
  "UL RB 起点",
  "UL MCS",
  "Tx 发射链（UE）",
  "Rx 接收链（UE）",
  "Tx 链掩码",
  "累计 BLER",
  "PDSCH 解码数",
  "CRC 成功 TB",
  "CRC 失败 TB",
  "成功 TB 字节",
  "失败 TB 字节",
  "重传计数",
  "ACK 视为 NACK",
  "HARQ 失败",
  "累计时隙",
  "上行新传 TB",
  "上行重传 TB",
  "上行新传字节",
  "上行重传字节",
  "上行重传占比",
  "上行累计调度次数",
  "MCS 来源",
  "调制编码",
];
export function linkRows(c) {
  const ports = c.nrLinkConfig.ports,
    entries = Object.entries(ports).sort(([a], [b]) => Number(a) - Number(b));
  return [
    ["MCS", c.mcs ?? "—"],
    ["调制方式", c.modulation ?? "—"],
    ...(c.radio === "NR"
      ? [
          [
            "CSI-RS 端口",
            entries.length
              ? [...new Set(Object.values(ports))]
                  .sort((a, b) => a - b)
                  .join(" / ")
              : "尚未收到专用配置",
          ],
          [
            "NZP 资源 → 端口",
            entries.map(([id, p]) => `${id} → ${p}`).join("；") || "—",
          ],
        ]
      : []),
    ...(c.radio === "LTE"
      ? ["本包 BLER", "本包 TB", "CRC 失败", "RB", "来源", "说明"]
      : NR_LINK_ROWS
    ).map((k) => [k, c.linkParams[k] ?? "—"]),
    ...(c.radio === "NR"
      ? [
          [
            "统计口径",
            "CSI-RS：逻辑端口；Tx/Rx：终端收发链；grants：本包调度数。",
          ],
        ]
      : []),
  ];
}
export function servingRows(c, state) {
  const live = serving(state),
    cell = c.cell,
    matches =
      live &&
      (!cell ||
        (live.pci === cell.pci &&
          live.arfcn === cell.arfcn &&
          live.rat === c.radio)),
    current = matches ? live : null,
    rows = [["制式", c.radio ?? current?.label ?? "等待上报"]];
  if (c.radio === "NR") {
    const rail = highSpeedRail(c.radio, c.engineering);
    rows.push([
      "高铁专网识别",
      rail
        ? "高铁专网 · SIB1 指纹"
        : c.engineering["NR SIB1"]
          ? "未命中"
          : "等待 NR SIB1",
    ]);
    if (rail) rows.push(["识别依据", rail.detail]);
  }
  rows.push(["PCI", cell?.pci ?? current?.pci ?? "—"]);
  if (cell) rows.push(["PCI 模3", cell.pci % 3]);
  rows.push(
    ["频段", cell?.band ?? current?.band ?? "—"],
    ["EARFCN / NR-ARFCN", cell?.arfcn ?? current?.arfcn ?? "—"],
  );
  if (cell?.freq > 0) rows.push(["频道对应频率", `${cell.freq} MHz`]);
  for (const [key, label, unit] of [
    ["rsrp", "RSRP", "dBm"],
    ["rsrq", "RSRQ", "dB"],
    ["sinr", "SINR", "dB"],
  ])
    rows.push([
      label,
      current?.[key] != null
        ? `${current[key]} ${unit} · 设备状态`
        : key !== "sinr" && c.servingMeasurement?.[key] != null
          ? `${c.servingMeasurement[key]} ${unit} · 信令`
          : "尚未上报",
    ]);
  if (c.servingMeasurement?.snr != null)
    rows.push(["SNR（信令）", `${c.servingMeasurement.snr} dB`]);
  if (current?.rssi != null)
    rows.push(["RSSI", `${current.rssi} dBm · 设备状态`]);
  if (live && !matches)
    rows.push(["设备状态", "当前状态与信令小区不一致，等待同小区测量"]);
  if (c.servingMeasurement)
    rows.push(["信令测量来源", c.servingMeasurement.source]);
  const measured = measuredServing(c);
  if (measured) {
    rows.push(
      ...beamRows(measured.beams),
      [
        "同频同模3邻区",
        sameModulo3(c)
          .map((n) => `PCI ${n.pci}`)
          .join(", ") || "本次测量未发现",
      ],
      ["模三判读", "同模3仅为线索，不能据此判定干扰。"],
    );
  }
  rows.push(["下行带宽", c.dlBw ?? current?.bandwidth ?? "—"]);
  for (const [k, label] of [
    ["ulBw", "上行带宽"],
    ["ulArfcn", "上行频道"],
  ])
    if (c[k] != null) rows.push([label, c[k]]);
  if (current?.cellId != null) rows.push(["Cell ID", current.cellId]);
  if (current?.rat === "NR" && current.tac != null)
    rows.push(["TAC", current.tac]);
  if (c.plmn) {
    rows.push(["运营商", c.plmn.name], ["PLMN", `${c.plmn.mcc}-${c.plmn.mnc}`]);
    if (c.plmn.tac != null) rows.push(["TAC（信令）", c.plmn.tac]);
  }
  if (c.broadcastPlmns.length)
    rows.push([
      "广播 PLMN",
      c.broadcastPlmns.map((p) => `${p.mcc}-${p.mnc}`).join(" / "),
    ]);
  if (c.selectedPlmnIndex != null)
    rows.push(["所选 PLMN 序号", c.selectedPlmnIndex]);
  for (const [k, label] of [
    ["sector", "小区标识（信令）"],
    ["caState", "载波聚合"],
    ["bler", "BLER"],
    ["mcs", "MCS"],
    ["modulation", "调制"],
    ["txPower", "PUSCH 功率"],
    ["qci", "QCI"],
    ["vendor", "厂商（推断）"],
  ])
    if (c[k] != null) rows.push([label, c[k]]);
  if (c.ambr) rows.push(["AMBR 下行 / 上行", `${c.ambr.dl} / ${c.ambr.ul}`]);
  if (c.security)
    rows.push(["安全算法", `${c.security.cipher} / ${c.security.integrity}`]);
  const known = new Set(rows.map(([k]) => k));
  rows.push(...c.cellParams.filter(([k]) => !known.has(k)));
  return rows;
}
