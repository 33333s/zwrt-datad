import { hex } from "./core.js";
import appNames from "./signal-names.json" with { type: "json" };
import * as ml1 from "./ml1.js";
import { lteCell, lteServing, lteMib, ltePower, ltePdsch } from "./lte.js";
import { decodeNas, decodeEps, plmn } from "./nas.js";
import {
  nrBroadcastBand,
  rrcEvidence,
  lteNeighbors,
  vendorEvidence,
  vendorVerdict,
  findP0,
} from "./rrc-context.js";
import { automaticTa } from "./radio-context.js";

// TCP stream frames include a 32-bit code; exported .bin captures do not.
export class FrameReader {
  constructor(wire = true) {
    this.wire = wire;
    this.header = new Uint8Array(4);
    this.headerAt = 0;
    this.body = null;
    this.at = 0;
  }
  push(chunk, emit) {
    let offset = 0;
    while (offset < chunk.length) {
      if (!this.body) {
        const n = Math.min(4 - this.headerAt, chunk.length - offset);
        this.header.set(chunk.subarray(offset, offset + n), this.headerAt);
        this.headerAt += n;
        offset += n;
        if (this.headerAt !== 4) continue;
        const length = new DataView(this.header.buffer).getUint32(0, true),
          extra = this.wire ? 4 : 0;
        if (length < 12 || length > 65535)
          throw new Error(`无效信令帧长度 ${length}`);
        this.body = new Uint8Array(length + extra);
        this.at = 0;
        this.headerAt = 0;
      }
      const n = Math.min(this.body.length - this.at, chunk.length - offset);
      this.body.set(chunk.subarray(offset, offset + n), this.at);
      this.at += n;
      offset += n;
      if (this.at === this.body.length) {
        const extra = this.wire ? 4 : 0,
          packet = this.body.subarray(extra),
          v = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
        const code = v.getUint16(2, true);
        if (
          v.getUint16(0, true) !== packet.length ||
          (this.wire &&
            new DataView(this.body.buffer).getUint32(0, true) !== code)
        )
          throw new Error("DIAG 内外帧长度或代码不一致");
        emit(code, packet);
        this.body = null;
        this.at = 0;
      }
    }
  }
  finish() {
    if (this.body || this.headerAt) throw new Error("信令末尾存在截断帧");
  }
}

const names = {
  ...appNames,
  0xb821: "NR RRC OTA",
  0xb0c0: "LTE RRC OTA",
  0xb97f: "NR 邻区与 SSB",
  0xb8d8: "NR 服务 SNR",
  0xb8c9: "NR RX AGC",
  0xb0c1: "LTE 服务小区",
  0xb0c2: "LTE MIB",
  0xb173: "LTE PDSCH",
  0xb800: "5G NAS SM DL",
  0xb801: "5G NAS SM UL",
  0xb80a: "5G NAS MM DL",
  0xb80b: "5G NAS MM UL",
  0xb0e2: "LTE NAS ESM DL",
  0xb0e3: "LTE NAS ESM UL",
};
const round = (x) => Math.round(x * 10) / 10;
const range = (x, lo, hi) => x >= lo && x <= hi;

export function neighbors(b) {
  if (b.length < 52) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength),
    cells = [];
  if (v.getUint32(0, true) !== 0x20009 || !range(b[8], 1, 8)) return null;
  let offset = 20;
  for (let carrier = 0; carrier < b[8]; carrier++) {
    if (offset + 32 > b.length) return null;
    const arfcn = v.getUint32(offset, true),
      count = b[offset + 5];
    if (arfcn > 3279165 || !range(count, 1, 32)) return null;
    offset += 32;
    for (let i = 0; i < count; i++) {
      if (offset + 16 > b.length) return null;
      const pci = v.getUint16(offset, true),
        number = b[offset + 4],
        rsrp = v.getInt32(offset + 8, true) / 128,
        rsrq = v.getInt32(offset + 12, true) / 128;
      if (pci > 1007 || number > 64 || offset + 16 + number * 44 > b.length)
        return null;
      const beams = [];
      if (number) {
        if (!range(rsrp, -156, -31) || !range(rsrq, -43, 20)) return null;
        for (let j = 0; j < number; j++) {
          const o = offset + 16 + j * 44,
            index = b[o];
          const branch = [20, 24].map((d) => v.getInt32(o + d, true) / 128),
            p = v.getInt32(o + 28, true) / 128,
            q = v.getInt32(o + 32, true) / 128;
          if (
            index > 63 ||
            beams.some((x) => x.index === index) ||
            branch.some((x) => !range(x, -156, -31)) ||
            !range(p, -156, -31) ||
            !range(q, -43, 20)
          )
            return null;
          beams.push({
            index,
            rsrp: round(p),
            rsrq: round(q),
            branchRsrp: branch.map(round),
          });
        }
        cells.push({ pci, arfcn, rsrp: round(rsrp), rsrq: round(rsrq), beams });
      }
      offset += 16 + number * 44;
    }
  }
  return offset === b.length ? cells : null;
}

export function agc(b) {
  const layout = { 100: [1, 2], 180: [2, 2], 164: [1, 4], 308: [2, 4] }[
    b.length
  ];
  if (!layout) return null;
  const [count, rx] = layout,
    v = new DataView(b.buffer, b.byteOffset, b.byteLength),
    stride = 16 + rx * 32;
  if (
    v.getUint32(0, true) !== 0x20000 ||
    v.getUint32(12, true) !== (b.length - 12) * 256 + 252 ||
    16 + count * stride + 4 !== b.length ||
    v.getUint32(b.length - 4, true) !== (rx === 2 ? 0x014004fe : 0x024004fe) ||
    b[11] & 128
  )
    return null;
  const records = [];
  for (let r = 0; r < count; r++) {
    const o = 16 + r * stride,
      tag = r === 0 ? 0x00100004 : rx === 2 ? 0x01400004 : 0x02400004;
    if (
      v.getUint32(o, true) !== tag + stride * 256 ||
      v.getUint32(o + 4, true) !== 80 ||
      b[o + 15] >> 4 !== (1 << rx) - 1
    )
      return null;
    const raw = Array.from({ length: rx }, (_, i) =>
      v.getInt16(o + 16 + i * 32 + 14, true),
    );
    const dbm = raw.map((x) =>
      x === 0 || x === -32768 ? null : round(x / 128),
    );
    if (raw.some((x) => x !== 0 && x !== -32768 && !range(x / 128, -110, -20)))
      return null;
    records.push({ raw, dbm });
  }
  return { timestamp: v.getBigUint64(4, true).toString(), records };
}

function decodePacketBase(code, packet, rrc, now) {
  const event = {
    code: "0x" + code.toString(16).toUpperCase().padStart(4, "0"),
    name: names[code] ?? "DIAG 日志",
    ts: now,
    len: packet.length,
    fields: [],
    payload_hex: hex(packet),
  };
  if (packet.length < 12) return event;
  const v = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
  if (v.getUint16(0, true) !== packet.length || v.getUint16(2, true) !== code)
    return event;
  event.diag_timestamp = v.getBigUint64(4, true).toString();
  const b = packet.subarray(12),
    view = new DataView(b.buffer, b.byteOffset, b.byteLength);
  if ((code === 0xb821 || code === 0xb0c0) && rrc) {
    const nr = code === 0xb821,
      head = nr ? 31 : 21;
    if (
      b.length < head ||
      b[0] !== (nr ? 17 : 27) ||
      !view.getUint16(head - 2, true) ||
      head + view.getUint16(head - 2, true) !== b.length
    )
      return event;
    const pci = view.getUint16(nr ? 7 : 6, true),
      arfcn = view.getUint32(nr ? 17 : 8, true),
      pdu = b[nr ? 24 : 14];
    const channel = (
      nr
        ? {
            1: "BCCH_BCH",
            2: "BCCH_DL_SCH",
            3: "DL_CCCH",
            4: "DL_DCCH",
            5: "PCCH",
            6: "UL_CCCH",
            8: "UL_DCCH",
          }
        : {
            1: "BCCH_BCH",
            3: "BCCH_DL_SCH",
            7: "PCCH",
            8: "DL_CCCH",
            9: "DL_DCCH",
            10: "UL_CCCH",
            11: "UL_DCCH",
          }
    )[pdu];
    if (!channel || pci > (nr ? 1007 : 503) || arfcn > (nr ? 3279165 : 262143))
      return event;
    const result = rrc.decode(nr ? "NR" : "LTE", channel, b.subarray(head));
    if (result) {
      event.tree = result.tree;
      event.evidence = "ASN.1";
      event.cell = { pci, arfcn, radio: nr ? "NR" : "LTE" };
      const message = result.tree.message,
        c1 = message?.c1;
      event.name = `${nr ? "NR" : "LTE"} RRC ${Object.keys(c1 ?? message ?? {})[0] ?? channel}`;
      event.fields = [
        ["逻辑信道", channel],
        ["PCI", pci],
        [nr ? "NR-ARFCN" : "EARFCN", arfcn],
        ["已读取位数", result.consumedBits],
        ["尾部位数", result.trailingBits],
      ];
    }
  } else if (code === 0xb97f) {
    const cells = neighbors(b);
    if (cells) {
      event.neighbors = cells;
      event.evidence = "实包布局";
      event.fields = [
        ["测量小区", cells.length],
        ["SSB 总数", cells.reduce((n, c) => n + c.beams.length, 0)],
      ];
    }
  } else if (code === 0xb8c9) {
    const value = agc(b);
    if (value) {
      event.agc = value;
      event.evidence = "实包布局";
      event.fields = value.records.flatMap((r, i) =>
        r.dbm.map((d, j) => [
          `记录 ${i + 1} / RX ${j + 1}`,
          d === null ? "不可用" : `${d} dBm`,
        ]),
      );
    }
  } else if (
    code === 0xb8d8 &&
    b.length === 28 &&
    view.getUint32(0, true) === 0x30002
  ) {
    const snr = view.getInt32(12, true) / 128;
    if (range(snr, -30, 40)) {
      event.snr = snr;
      event.evidence = "实包布局";
      event.fields = [["SNR", `${snr} dB`]];
    }
  }
  return event;
}

export function nrFrequency(arfcn) {
  return Math.round(
    arfcn < 600000
      ? arfcn * 0.005
      : arfcn < 2016667
        ? 3000 + (arfcn - 600000) * 0.015
        : 24250.08 + (arfcn - 2016667) * 0.06,
  );
}
const pct = (n) => (n == null ? "—" : n.toFixed(2) + "%");
const timeLabel = (t) =>
  `SFN ${t.sfn} / Slot ${t.slot} / ${15 << t.numerology} kHz`;
const symbols = (mask) =>
  mask === 0
    ? "无"
    : mask === 0x3fff
      ? "0–13"
      : Array.from({ length: 14 }, (_, i) => i)
          .filter((i) => mask & (1 << i))
          .join(",");
export function decodePacket(code, packet, rrc, now = Date.now()) {
  const event = decodePacketBase(code, packet, rrc, now);
  if (packet.length < 12) return event;
  const v = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
  if (v.getUint16(0, true) !== packet.length || v.getUint16(2, true) !== code)
    return event;
  event.diag_valid = true;
  const b = packet.subarray(12);
  event.hexPreview = hex(b.subarray(0, 32));
  function decoded(kind, value, scope, evidence = "STRUCTURE_INFERRED") {
    event.m = { kind, value };
    event.decodeScope = scope;
    event.decodeEvidence = evidence;
    event.evidence =
      evidence === "ASN1_SCHEMA"
        ? "ASN.1"
        : evidence === "CAPTURE_VALIDATED"
          ? "实包校验"
          : "推导布局 · 部分解码";
    delete event.hexPreview;
  }
  try {
    if (code === 0xb821 || code === 0xb0c0) {
      const nr = code === 0xb821;
      if (event.tree) {
        event.rrcType = Object.keys(
          event.tree.message?.c1 ?? event.tree.message ?? {},
        )[0];
        event.cell = nr
          ? {
              ...event.cell,
              band: nrBroadcastBand(event.tree) ?? "NR",
              freq: nrFrequency(event.cell.arfcn),
            }
          : lteCell(event.cell.pci, event.cell.arfcn);
        Object.assign(event, rrcEvidence(event.tree, nr ? "NR" : "LTE"));
        if (!nr) event.neighbors = lteNeighbors(event.tree);
        const evidence = vendorEvidence(nr ? "NR" : "LTE", event.tree);
        if (evidence) {
          event.vendorFingerprint = {
            ...evidence,
            ...vendorVerdict(evidence),
            heuristic: true,
          };
          event.imsBit = evidence.ims ?? null;
          event.p0 =
            evidence.pusch != null && evidence.pucch != null
              ? [evidence.pusch, evidence.pucch]
              : null;
        }
        decoded(
          "rrc",
          null,
          "3GPP ASN.1 UPER 树；嵌入的未展开载荷和尾部保留原始数据；LTE 邻区测量显示量化区间下界。",
          "ASN1_SCHEMA",
        );
      } else if (
        nr &&
        b.length >= 31 &&
        b[0] === 17 &&
        b[24] === 2 &&
        new DataView(b.buffer, b.byteOffset, b.byteLength).getUint16(
          29,
          true,
        ) ===
          b.length - 31
      ) {
        const payload = b.subarray(31),
          pair =
            payload.length > 20 && ((payload[0] >>> 6) & 3) === 2
              ? findP0(payload)
              : null;
        if (pair) {
          event.p0 = pair;
          const pci = v.getUint16(19, true),
            arfcn = v.getUint32(29, true);
          if (pci <= 1007 && arfcn <= 3279165)
            event.cell = {
              pci,
              arfcn,
              band: "NR",
              freq: nrFrequency(arfcn),
              radio: "NR",
            };
          const e = {
            radio: "NR",
            source: "SIB1 比特模式候选（未通过树解码）",
            pusch: pair[0],
            pucch: pair[1],
          };
          event.vendorFingerprint = {
            ...e,
            ...vendorVerdict(e),
            heuristic: true,
          };
          event.decodeScope = "功控对仅为比特模式候选，保留原包供核对";
        }
      }
      if (event.nas) {
        event.ambr = event.nas.ambr ?? null;
        event.ueIp = event.nas.ueIp ?? null;
      }
      return event;
    }
    if (code === 0xb97f && event.neighbors) {
      decoded(
        "neighbors",
        event.neighbors,
        "小区、逐 SSB RSRP/RSRQ 与接收分支；最强已测 SSB 不等于业务波束",
      );
      return event;
    }
    if (code === 0xb8c9 && event.agc) {
      event.fields = [
        ["STMR（原始计数）", event.agc.timestamp],
        ["记录数", event.agc.records.length],
        ["每记录 RX 数", event.agc.records[0].raw.length],
        ...event.fields,
      ];
      decoded(
        "agc",
        event.agc,
        "RX AGC；0/0x8000 保留为不可用状态，不换算为 dBm",
        event.agc.records.some((r) => r.dbm.some((v) => v === null))
          ? "STRUCTURE_INFERRED"
          : "CAPTURE_VALIDATED",
      );
      return event;
    }
    if (code === 0xb8d8 && event.snr !== undefined) {
      event.servingSnr = { snr: event.snr, sfn: null };
      decoded(
        "snr",
        event.servingSnr,
        "NR B8D8 v3.2 服务 SNR",
        "CAPTURE_VALIDATED",
      );
      return event;
    }
    if ([0xb800, 0xb801, 0xb80a, 0xb80b].includes(code)) {
      if (
        packet.length < 22 ||
        ![1, 0, 0, 0, 15, 4, 0].every((n, i) => b[i] === n)
      )
        return event;
      const nas = decodeNas(b.subarray(7));
      if (nas) {
        event.nas = nas;
        event.name = nas.name;
        event.ambr = nas.ambr ?? null;
        event.ueIp = nas.ueIp ?? null;
        event.plmn = nas.plmn ?? null;
        event.fields = nasFields(nas);
        decoded(
          "nas",
          nas,
          "5GS 明文 NAS；未知可选 IE 和加密载荷保留原包",
          "CAPTURE_VALIDATED",
        );
      }
      return event;
    }
    if ([0xb0e2, 0xb0e3, 0xb0ec, 0xb0ed].includes(code)) {
      if (b.length < 6 || b[0] !== 1) return event;
      const eps = decodeEps(b.subarray(4));
      if (eps) {
        event.nas = eps.nas;
        event.name = eps.nas.name;
        event.ambr = eps.nas.ambr ?? null;
        event.ueIp = eps.nas.ueIp ?? null;
        event.security = eps.security ?? null;
        event.fields = nasFields(eps.nas);
        decoded(
          "nas",
          eps.nas,
          "LTE 明文 EMM/ESM；加密内容不解码，未知可选 IE 保留原包",
          "CAPTURE_VALIDATED",
        );
      }
      return event;
    }
    if (code === 0xb0c2) {
      const c = lteServing(b);
      if (c) {
        event.name = "LTE RRC 服务小区";
        event.cell = lteCell(c.pci, c.dl, c.band);
        event.plmn = plmn(
          c.mcc,
          c.mnc,
          c.tac.toString(16).toUpperCase().padStart(4, "0"),
        );
        event.fields = [
          ["频段", `B${c.band}`],
          ["DL EARFCN", c.dl],
          ["UL EARFCN", c.ul],
          ["DL 带宽", c.dlBw],
          ["UL 带宽", c.ulBw],
          ["ECI", c.cid],
          ["TAC", c.tac],
        ];
        decoded(
          "lteServing",
          c,
          "LTE 服务小区 v3 固定 29B 布局",
          "CAPTURE_VALIDATED",
        );
      }
      return event;
    }
    if (code === 0xb0c1) {
      const c = lteMib(b);
      if (c) {
        event.name = "LTE RRC MIB";
        event.cell = lteCell(c.pci, c.arfcn);
        event.fields = [
          ["SFN", c.sfn],
          ["下行带宽", c.bandwidth],
          ["天线数", c.antennas],
        ];
        decoded("lteMib", c, "LTE MIB v2 固定 11B 布局", "CAPTURE_VALIDATED");
      }
      return event;
    }
    if (code === 0xb16e || code === 0xb16f) {
      const rows = ltePower(code, b);
      if (rows) {
        event.fields = [
          ["信道", rows.at(-1).channel],
          ...rows.map((r, i) => [
            `记录${i + 1} 实际功率（推导）`,
            `${r.dbm} dBm · SFN ${r.sfn}/${r.subframe}`,
          ]),
        ];
        decoded(
          "ltePower",
          rows,
          "LTE 新版位域推导；长度/记录数/时序/值域自检",
        );
      }
      return event;
    }
    if (code === 0xb173) {
      const rows = ltePdsch(b);
      if (rows) {
        const fail = rows.filter((r) => !r.crc).length;
        event.fields = [
          ["TB 数", rows.length],
          ["CRC 失败 TB", fail],
          ["本包 BLER（推导）", pct((100 * fail) / rows.length)],
          ...rows.map((t, i) => [
            `TB ${i + 1}（推导）`,
            `MCS ${t.mcs} / ${t.modulation} / ${t.rb} RB / ${t.bytes} B / CRC ${t.crc ? "成功" : "失败"}`,
          ]),
        ];
        decoded(
          "ltePdsch",
          rows,
          "LTE v48 位域推导；BLER 为本包 TB 统计，非累计值",
        );
      }
      return event;
    }
    if (
      [
        0xb111, 0xb114, 0xb11b, 0xb11d, 0xb123, 0xb12a, 0xb134, 0xb193, 0xb1dc,
      ].includes(code)
    ) {
      const info = ml1.lteLog(code, b);
      if (info) {
        event.fields = [];
        for (const [key, label] of [
          ["pci", "PCI（推导）"],
          ["earfcn", "EARFCN（推导）"],
          ["sfn", "SFN（推导）"],
          ["recordBytes", "记录长度"],
        ])
          if (info[key] != null) event.fields.push([label, info[key]]);
        if (info.times?.length) {
          event.fields.push(["记录数", info.times.length]);
          info.times.forEach((t, i) =>
            event.fields.push([
              `记录${i + 1} 时序（推导）`,
              `SFN ${t.sfn} / Subframe ${t.subframe}`,
            ]),
          );
        }
        for (const [i, c] of (info.cells ?? []).entries()) {
          event.fields.push([`小区${i + 1} PCI（推导）`, c.pci]);
          for (const [rx, p] of c.rsrp.entries())
            event.fields.push([
              `小区${i + 1} RSRP RX${rx}（推导）`,
              `${p} dBm`,
            ]);
          for (const [rx, q] of c.rsrq.entries())
            event.fields.push([`小区${i + 1} RSRQ RX${rx}（推导）`, `${q} dB`]);
        }
        decoded("lteLog", info, info.scope);
      }
      return event;
    }
    if (code === 0xb823) {
      const c = ml1.nrServing(b);
      if (c) {
        event.cell = {
          pci: c.pci,
          arfcn: c.dlArfcn,
          band: `n${c.band}`,
          freq: nrFrequency(c.dlArfcn),
          radio: "NR",
        };
        event.plmn = plmn(c.mcc, c.mnc, c.tac.toString(16).toUpperCase());
        event.fields = [
          ["频段", `n${c.band}`],
          ["下行 ARFCN", c.dlArfcn],
          ["上行 ARFCN", c.ulArfcn],
          ["下行带宽", c.dlBwMhz + "MHz"],
          ["上行带宽", c.ulBwMhz + "MHz"],
          ["Cell ID", c.cellId],
          ["PLMN", `${c.mcc}-${c.mnc}`],
          ["TAC", c.tac],
          ["PCI", c.pci],
        ];
        decoded(
          "nrServing",
          c,
          "NR 服务小区 v3 固定 46B、实测 n78 布局",
          "CAPTURE_VALIDATED",
        );
      }
      return event;
    }
    if (code === 0xb062 || code === 0xb88a) {
      const ta = automaticTa(code, packet, now);
      if (ta) {
        event.fields = [
          ["RAR 绝对 TA", ta.command],
          ["TA 估算距离", `${ta.estimate.meters.toFixed(0)} m`],
          ["TA 步长", `${ta.estimate.commandStepMeters.toFixed(1)} m`],
        ];
        decoded("ta", ta, "绝对 RAR TA；已排除双工固定偏移，不是地图直线距离");
      }
      return event;
    }
    const route = {
      0xb881: [
        "ulTb",
        ml1.ulTb,
        "上行 TB 累计统计 3.1；MCS/PRB/调度为推导，功率/PHR 未解码",
      ],
      0xb888: [
        "pdschStats",
        ml1.pdschStats,
        "PDSCH 累计统计 3.0；BLER 非瞬时值",
      ],
      0xb8a1: [
        "arbitration",
        ml1.arbitration,
        "14 符号仲裁位图推导；类型枚举与 VSTMR 时间未换算",
      ],
      0xb884: [
        "powerControl",
        ml1.powerControl,
        "3.3 signed12 限幅前请求/上限；RF 功率来自 B8D1",
      ],
      0xb8d1: [
        "txRf",
        ml1.txRf,
        "3.5 单链 RF 目标功率；需要 B884 同时隙区分信道；非仪表实测",
      ],
      0xb883: [
        "ulSchedule",
        ml1.ulSchedule,
        "3.15 PUSCH RB/MCS；RNTI 候选角色未确认",
      ],
      0xb887: [
        "pdschStatus",
        ml1.pdschStatus,
        "PDSCH 时序、小区、MCS、独立调制编码、RB、layers 及 UE Rx 链；CRC 未解码",
      ],
      0xb890: [
        "cdrx",
        ml1.cdrx,
        "DRX 配置和状态时序推导；计时单位与状态/原因枚举未解码",
      ],
      0xb8dd: [
        "servingFtl",
        ml1.servingFtl,
        "服务小区时序与频率；相位/频偏环路字段未解码",
      ],
    }[code];
    if (route) {
      let value = route[1](b);
      if (value !== null) {
        event.fields = ml1Fields(route[0], value);
        decoded(route[0], value, route[2]);
      } else if (code === 0xb884) {
        const times = ml1.powerControlTimes(b);
        if (times) {
          event.fields = [
            ["功控记录数", times.length],
            ...times.map((t, i) => [`记录${i + 1}`, timeLabel(t)]),
          ];
          decoded("powerControlTimes", times, "仅功控时序；未通过功率字段校验");
        }
      }
    }
  } catch (error) {
    event.decodeError = String(error).slice(0, 80);
  }
  return event;
}
function nasFields(n) {
  const fields = [["协议", n.type]];
  for (const [key, label] of [
    ["pduSessionId", "PDU 会话 ID"],
    ["epsBearerId", "EPS 承载 ID"],
    ["linkedEpsBearerId", "关联 EPS 承载"],
    ["pti", "PTI"],
    ["dnn", "DNN / APN"],
    ["qci", "EPS QCI"],
    ["ueIp", "UE IP"],
    ["pduType", "PDU 类型"],
    ["ssc", "SSC 模式"],
  ])
    if (n[key] != null) fields.push([label, n[key]]);
  if (n.ambr) fields.push(["AMBR 下行", n.ambr.dl], ["AMBR 上行", n.ambr.ul]);
  for (const f of n.qosFlows ?? [])
    fields.push([
      `QFI ${f.qfi}`,
      `5QI ${f.qi ?? "未上报"} · 操作 ${f.operation}${f.epsBearerId == null ? "" : ` · EPS ${f.epsBearerId}`}`,
    ]);
  for (const m of n.mappedEpsBearers ?? [])
    fields.push([
      `EPS 映射 ${m.id}`,
      `QCI ${m.qci ?? "未上报"} · 操作 ${m.operation}`,
    ]);
  return fields;
}
function ml1Fields(kind, a) {
  if (kind === "ulTb")
    return [
      ["新传 TB 字节", a.newBytes],
      ["重传 TB 字节", a.retransmitBytes],
      ["新传 TB 数", a.newTb],
      ["重传 TB 数", a.retransmitTb],
      ["MCS 累计和（推导）", a.mcsSum],
      ["PRB 累计和（推导）", a.prbSum],
      ["ULSCH 调度计数（推导）", a.scheduled],
      ...(a.retransmitPercent == null
        ? []
        : [["累计重传 TB 占比", pct(a.retransmitPercent)]]),
    ];
  if (kind === "pdschStats")
    return [
      ["统计载波 ID", a.carrierIds.join(", ")],
      ...Object.entries({
        slots: "累计时隙",
        decoded: "PDSCH 解码数",
        crcPass: "CRC 成功 TB",
        crcFail: "CRC 失败 TB",
        passBytes: "成功 TB 字节",
        failBytes: "失败 TB 字节",
        totalBytes: "总 TB 字节",
        retransmissions: "重传计数（推导）",
        ackAsNack: "ACK 被视为 NACK（推导）",
        harqFailures: "HARQ 失败（推导）",
        paddingBytes: "填充字节（推导）",
        retransmitBytes: "重传字节（推导）",
      }).map(([k, label]) => [label, a[k]]),
      ...(a.bler == null ? [] : [["累计 BLER", pct(a.bler)]]),
    ];
  if (kind === "arbitration")
    return [
      ["仲裁记录数", a.length],
      ...a.flatMap((r, i) => [
        [`记录${i + 1}`, `${timeLabel(r.time)} / ${r.entries} 项`],
        ...r.symbols.flatMap((s, j) => {
          const p = `${i + 1}.${j + 1}`;
          return [
            [
              `仲裁项 ${p}（推导）`,
              `RF 组 ${s.rfGroup} / 更新码 ${s.updateType} / 评估 ${s.evaluatedSymbols} 符号`,
            ],
            [
              `${p} UL 可用 / 已承诺符号`,
              `${symbols(s.ulAvailable)} / ${symbols(s.ulCommitted)}`,
            ],
            [
              `${p} DL 聚合 / 保护符号`,
              `${symbols(s.dlAggregated)} / ${symbols(s.guard)}`,
            ],
            [
              `${p} UL 排他 / RX IU 符号`,
              `${symbols(s.ulExclusive)}（类型 ${s.ulExclusiveType}） / ${symbols(s.ulRxIu)}`,
            ],
            [
              `${p} DL 不可用 / SSB 符号`,
              `${symbols(s.dlUnavailable)}（类型 ${s.dlUnavailableType}） / ${symbols(s.ssb)}`,
            ],
            [`${p} VSTMR（原始计数）`, s.vstmr],
          ];
        }),
      ]),
    ];
  if (kind === "powerControl")
    return [
      ["功控记录数", a.length],
      ...a.flatMap((r, i) => [
        [`记录${i + 1}`, timeLabel(r.time)],
        [`记录${i + 1} ${r.channel} 限幅前请求`, `${r.requestedDbm} dBm`],
        [`记录${i + 1} 功率上限`, `${r.limitDbm} dBm`],
      ]),
    ];
  if (kind === "txRf")
    return [
      ["TX RF 命令数", a.length],
      ...a.flatMap((r, i) => [
        [
          `命令${i + 1}`,
          `SFN ${r.sfn} / Slot ${r.slot} / 符号边界 ${r.symbolBoundary} / TX 链 ${(r.chainMask & 1) + ((r.chainMask >>> 1) & 1)} / 掩码 0x${r.chainMask.toString(16)}`,
        ],
        ...(r.power
          ? [
              [
                `命令${i + 1} RF 功率 / 上限`,
                `${r.power.dbm} / ${r.power.limitDbm} dBm（链${r.power.chain}）`,
              ],
            ]
          : []),
      ]),
    ];
  if (kind === "ulSchedule")
    return [
      ["上行调度记录数", a.length],
      ...a.flatMap((r, i) => [
        [
          `记录${i + 1}`,
          `${timeLabel(r.time)} / 类型 0x${r.kind.toString(16)}`,
        ],
        [
          `记录${i + 1} RNTI 候选 A/B`,
          `0x${r.rntiCandidateA.toString(16)} / 0x${r.rntiCandidateB.toString(16)}`,
        ],
        ...(r.grant
          ? [
              [
                `记录${i + 1} PUSCH grant`,
                `RB ${r.grant.rb} / 起点 ${r.grant.rbStart} / MCS ${r.grant.mcs}`,
              ],
            ]
          : []),
      ]),
    ];
  if (kind === "pdschStatus")
    return [
      ["PDSCH 状态项数", a.reduce((n, r) => n + r.statusCount, 0)],
      ...a.flatMap((r, i) => [
        [`记录${i + 1}`, `${timeLabel(r.time)} / ${r.statusCount} 项`],
        ...r.readings.map((s, j) => [
          `记录${i + 1} 状态${j + 1}`,
          `PCI ${s.pci} / NR-ARFCN ${s.arfcn} / MCS ${s.mcs} / ${s.modulation ?? `未识别调制（编码 ${s.modulationCode}）`} / RB ${s.rb} / layers ${s.layers} / Rx ${s.rxChains ?? "未识别"}`,
        ]),
      ]),
    ];
  if (kind === "cdrx")
    return [
      ["DRX 启用（推导）", a.enabled ? "是" : "否"],
      ["On-duration（原始计数）", a.onDurationRaw],
      ["Inactivity（原始计数）", a.inactivityRaw],
      ["长/短周期（原始计数）", `${a.longCycleRaw} / ${a.shortCycleRaw}`],
      ["起始偏移（原始计数）", a.startOffsetRaw],
      ["CDRX 事件数", a.events.length],
      ...a.events.map((r, i) => [
        `事件${i + 1}`,
        `${timeLabel(r.time)} / 状态 ${r.previousState}→${r.currentState} / 原因码 ${r.reasonCode}`,
      ]),
    ];
  if (kind === "servingFtl")
    return [
      ["SFN", a.sfn],
      ["PCI", a.pci],
      ["NR-ARFCN（推导）", a.arfcn],
      ["RF 频率（推导）", `${(a.rfKHz / 1000).toFixed(3)} MHz`],
    ];
  return [];
}

export function ndjsonRecord(event, observedAt = null) {
  const packetHex = event.payload_hex ?? "",
    decoded = Object.fromEntries(
      Object.entries(event).filter(
        ([k]) =>
          !["payload_hex", "ts", "diag_timestamp", "diag_valid", "m"].includes(
            k,
          ),
      ),
    );
  const semantic =
    !!event.tree ||
    !!event.nas ||
    !!event.fields?.length ||
    !!event.neighbors?.length ||
    event.snr !== undefined;
  const out = {
    schema: "ufi.signal.ndjson.v1",
    source_event: "modem.packet",
    diag_packet_hex: packetHex,
    diag_packet_len: event.len,
  };
  if (observedAt !== null) out.observed_at_ms = observedAt;
  if (!event.diag_valid)
    return { ...out, decoder_status: "invalid_diag_header" };
  return {
    ...out,
    log_id: parseInt(event.code, 16),
    log_id_hex: event.code,
    qxdm_ts: event.diag_timestamp,
    payload_len: event.len - 12,
    payload_hex: packetHex.slice(24),
    decoder_status: event.decodeError
      ? "decode_error"
      : !semantic
        ? "raw_preserved"
        : event.decodeEvidence === "ASN1_SCHEMA"
          ? "asn1_tree"
          : event.decodeEvidence === "STRUCTURE_INFERRED"
            ? "inferred_partial"
            : "partial_fields",
    parser: "ufi_signal",
    summary: event.name,
    decoded,
  };
}
