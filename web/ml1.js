// Port of Ml1Decoder.kt: lengths/version allowlists are deliberate, not heuristics.
// 64-bit counters remain decimal strings at the public boundary.
const inRange = (n, a, b) => n >= a && n <= b;
const signed12 = (n) => (n << 20) >> 20;
const B8A1 = [
  36, 84, 104, 132, 172, 180, 200, 220, 228, 240, 248, 260, 268, 276, 280, 288,
  304, 308, 316, 324, 336, 344, 356, 364, 372, 384, 392, 404, 412, 420, 424,
  432, 440, 452, 460, 472, 480, 488, 492, 500, 508, 520, 528, 548, 568,
];
const B883 = [
  40, 52, 68, 96, 112, 128, 140, 156, 172, 184, 188, 200, 216, 228, 232, 244,
  260,
];
const B887 = [36, 56, 64, 84, 92, 112, 120, 132, 140, 148, 176, 204];
const B890 = [
  64, 92, 120, 148, 176, 204, 232, 260, 288, 316, 344, 372, 400, 428, 456, 484,
  512, 540, 624, 680,
];
function reader(b) {
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    u8: (o) => v.getUint8(o),
    u16: (o) => v.getUint16(o, true),
    i16: (o) => v.getInt16(o, true),
    u32: (o) => v.getUint32(o, true),
    i32: (o) => v.getInt32(o, true),
    u64: (o) => v.getBigInt64(o, true),
  };
}
function mac(b, r, version, padding = false) {
  return (
    b.length >= 8 &&
    r.u32(0) === version &&
    ((r.u8(4) === 0 && r.u16(5) === 0) ||
      (padding && r.u8(4) === 204 && r.u16(5) === 0xcccc))
  );
}
function time(r, o) {
  const numerology = r.u8(o + 1) & 15,
    slot = r.u8(o);
  return inRange(numerology, 0, 1) && slot < 10 << numerology
    ? { sfn: r.u16(o + 2) & 1023, slot, numerology }
    : null;
}
const safe =
  (f) =>
  (...args) => {
    try {
      return f(...args);
    } catch {
      return null;
    }
  };

export const ulTb = safe((b) => {
  const r = reader(b);
  if (
    b.length !== 92 ||
    r.u32(0) !== 0x30001 ||
    r.u32(4) !== 0x1000000 ||
    r.u32(8) !== 0 ||
    8 + r.u8(7) * 84 !== b.length
  )
    return null;
  const newBytes = r.u64(12),
    retransmitBytes = r.u64(20),
    newTb = r.u32(56),
    retransmitTb = r.u32(60),
    total = BigInt(newTb + retransmitTb),
    mcsSum = r.u64(28),
    prbSum = r.u64(36),
    scheduled = r.u32(80);
  if (
    newBytes < BigInt(newTb) ||
    retransmitBytes < BigInt(retransmitTb) ||
    (!newTb && newBytes !== 0n) ||
    (!retransmitTb && retransmitBytes !== 0n) ||
    mcsSum < 0n ||
    mcsSum > 31n * total ||
    prbSum < 0n ||
    prbSum > 275n * total ||
    BigInt(scheduled) > total
  )
    return null;
  return {
    newBytes: String(newBytes),
    retransmitBytes: String(retransmitBytes),
    newTb,
    retransmitTb,
    mcsSum: String(mcsSum),
    prbSum: String(prbSum),
    scheduled,
    retransmitPercent: total ? (100 * retransmitTb) / Number(total) : null,
  };
});
export const pdschStats = safe((b) => {
  const r = reader(b);
  let count;
  if (
    b.length === 92 &&
    r.u32(4) === 0x1000000 &&
    r.u8(11) === 0x10 &&
    r.u32(12) === 1 &&
    r.u32(16) === 0
  )
    count = 1;
  else if (
    b.length === 380 &&
    r.u32(4) === 0x1cccccc &&
    r.u8(11) === 0x50 &&
    r.u32(12) === 0xcccccccd &&
    r.u32(16) === 0xcccccccc
  )
    count = 5;
  else return null;
  if (
    r.u32(0) !== 0x30000 ||
    (r.u8(8) & 15) !== 0 ||
    20 + count * 72 !== b.length
  )
    return null;
  const rows = [],
    ids = new Set();
  for (let i = 0; i < count; i++) {
    const o = 20 + i * 72,
      id = r.i32(o),
      decoded = r.u32(o + 8),
      crcPass = r.u32(o + 12),
      crcFail = r.u32(o + 16),
      passBytes = r.u64(o + 32),
      failBytes = r.u64(o + 40),
      totalBytes = r.u64(o + 48),
      retransmissions = r.u32(o + 20),
      ackAsNack = r.u32(o + 24),
      harqFailures = r.u32(o + 28),
      paddingBytes = r.u64(o + 56),
      retransmitBytes = r.u64(o + 64);
    if (
      !inRange(id, 0, 7) ||
      ids.has(id) ||
      (count === 1 && id !== 0) ||
      crcPass + crcFail !== decoded ||
      passBytes < BigInt(crcPass) ||
      failBytes < BigInt(crcFail) ||
      totalBytes < passBytes ||
      totalBytes - passBytes !== failBytes ||
      retransmissions > decoded ||
      ackAsNack > decoded ||
      harqFailures > decoded ||
      paddingBytes < 0n ||
      paddingBytes > passBytes ||
      retransmitBytes < 0n ||
      retransmitBytes > totalBytes ||
      (retransmissions === 0) !== (retransmitBytes === 0n)
    )
      return null;
    ids.add(id);
    rows.push({
      slots: r.u32(o + 4),
      decoded,
      crcPass,
      crcFail,
      passBytes,
      failBytes,
      totalBytes,
      retransmissions,
      ackAsNack,
      harqFailures,
      paddingBytes,
      retransmitBytes,
    });
  }
  const out = { carrierIds: [...ids] };
  for (const key of Object.keys(rows[0])) {
    const big = typeof rows[0][key] === "bigint",
      sum = rows.reduce((s, row) => s + row[key], big ? 0n : 0);
    if (big && sum > 0x7fffffffffffffffn) return null;
    out[key] = big ? String(sum) : sum;
  }
  out.bler = out.decoded ? (100 * out.crcFail) / out.decoded : null;
  return out;
});
export const arbitration = safe((b) => {
  const r = reader(b);
  if (!B8A1.includes(b.length) || !mac(b, r, 0x30001)) return null;
  const count = r.u8(7);
  if (!inRange(count, 1, 10)) return null;
  let o = 8;
  const records = [];
  for (let i = 0; i < count; i++) {
    if (o + 8 > b.length) return null;
    const t = time(r, o),
      n = r.i32(o + 4);
    if (!t || !inRange(n, 1, 3) || o + 8 + n * 20 > b.length) return null;
    const symbols = [];
    for (let j = 0; j < n; j++) {
      const p = o + 8 + j * 20,
        a = r.u32(p),
        bb = r.u32(p + 4),
        c = r.u32(p + 8),
        d = r.u32(p + 12),
        e = r.u32(p + 16),
        info = {
          rfGroup: a & 15,
          updateType: (a >>> 4) & 7,
          evaluatedSymbols: (a >>> 7) & 15,
          ulAvailable: (a >>> 11) & 0x3fff,
          ulExclusiveType: (a >>> 25) & 15,
          ulExclusive: bb & 0x3fff,
          dlAggregated: (bb >>> 14) & 0x3fff,
          guard: c & 0x3fff,
          ulCommitted: (c >>> 14) & 0x3fff,
          ulRxIu: d & 0x3fff,
          dlUnavailable: (d >>> 14) & 0x3fff,
          dlUnavailableType: d >>> 28,
          ssb: e & 0x3fff,
          vstmr: e >>> 14,
        };
      if (
        info.rfGroup !== 0 ||
        info.updateType > 2 ||
        info.evaluatedSymbols > 14 ||
        ![0, 4, 5].includes(info.ulExclusiveType) ||
        info.dlUnavailable ||
        info.dlUnavailableType ||
        info.ulAvailable & info.dlAggregated ||
        info.guard & info.dlAggregated
      )
        return null;
      symbols.push(info);
    }
    records.push({ time: t, entries: n, symbols });
    o += 8 + n * 20;
  }
  return o === b.length ? records : null;
});
export const powerControlTimes = safe((b) => {
  const r = reader(b);
  if (![40, 72, 104, 136].includes(b.length) || !mac(b, r, 0x30003, true))
    return null;
  const n = r.u8(7);
  if (!inRange(n, 1, 4) || 8 + n * 32 !== b.length) return null;
  const rows = [];
  for (let i = 0; i < n; i++) {
    const o = 8 + i * 32,
      t = time(r, o);
    if (
      !t ||
      ![1, 0xcccccc01].includes(r.u32(o + 4)) ||
      ![1, 0xcccccc01].includes(r.u32(o + 12)) ||
      r.u32(o + 8) !== t.sfn * (10 << t.numerology) + t.slot
    )
      return null;
    rows.push(t);
  }
  return rows;
});
export const powerControl = safe((b) => {
  const times = powerControlTimes(b);
  if (!times) return null;
  const r = reader(b),
    rows = [];
  for (const [i, t] of times.entries()) {
    const o = 24 + i * 32,
      w = r.u32(o),
      channel = { 1: "PRACH", 3: "SRS", 4: "PUSCH", 5: "PUCCH" }[
        (w >>> 4) & 15
      ],
      requestedDbm = signed12((w >>> 8) & 4095) / 10,
      limitDbm = signed12((r.u32(o + 4) >>> 12) & 4095) / 10;
    if (
      w & 15 ||
      !channel ||
      !inRange(requestedDbm, -100, 100) ||
      !inRange(limitDbm, -50, 35)
    )
      return null;
    rows.push({ time: t, channel, requestedDbm, limitDbm });
  }
  return rows;
});
export const txRf = safe((b) => {
  const r = reader(b);
  if (
    ![24, 228, 436, 444, 644, 652, 860].includes(b.length) ||
    r.u32(0) !== 0x30005
  )
    return null;
  let o = 12;
  const commands = [];
  while (o < b.length) {
    if (o + 4 > b.length) return null;
    const h = r.u32(o),
      length = (h >>> 8) & 0xfff;
    if (h !== (length << 8) + 0xfc || length < 12 || o + length > b.length)
      return null;
    const end = o + length;
    o += 4;
    while (o < end) {
      const tag = r.u32(o);
      if (tag === 0x10cc00) {
        if (o + 204 > end || r.u32(o + 4) !== 0x100) return null;
        const symbolBoundary = r.u8(o + 12),
          chainMask = r.u8(o + 13);
        if (
          !inRange(symbolBoundary, 0, 14) ||
          !inRange(chainMask, 1, 3) ||
          r.u16(o + 14) !== 0
        )
          return null;
        const chain = chainMask === 1 ? 0 : chainMask === 2 ? 1 : null;
        let power = null;
        if (
          chain !== null &&
          r.i32(o + 16) === 330 &&
          r.i32(o + 32) === 330 &&
          r.u16(o + 60 + chain * 68) === 1
        ) {
          const p = o + 62 + chain * 68,
            dbm = r.i16(p) / 10,
            limitDbm = r.i16(p + 2) / 10;
          if (
            inRange(dbm, -50, 35) &&
            inRange(limitDbm, -50, 35) &&
            dbm <= limitDbm
          )
            power = { chain, dbm, limitDbm };
        }
        const radio = r.u32(o + 8);
        commands.push({
          sfn: (radio >>> 10) & 1023,
          slot: (radio >>> 5) & 31,
          symbolBoundary,
          chainMask,
          power,
        });
        o += 204;
      } else if (tag === 0x33004fe || tag === 0x1004fe) o += 4;
      else return null;
    }
    if (o !== end || r.u32(end - 4) !== 0x1004fe) return null;
  }
  return o === b.length ? commands : null;
});
export const ulSchedule = safe((b) => {
  const r = reader(b);
  if (!B883.includes(b.length) || !mac(b, r, 0x3000f, true)) return null;
  const n = r.u8(7);
  if (!inRange(n, 1, 5)) return null;
  let o = 8;
  const rows = [];
  for (let i = 0; i < n; i++) {
    if (o + 24 > b.length || r.u32(o) >>> 26 !== 1) return null;
    const t = time(r, o),
      kind = r.u8(o + 5) & 15,
      size = { 1: 36, 2: 20, 4: 20, 8: 8 }[kind];
    if (
      !t ||
      !size ||
      o + 24 + size > b.length ||
      (r.u8(o + 4) & 15) !== 0 ||
      ![1, 128, 129].includes(r.i32(o + 16)) ||
      r.u32(o + 20) !== 0x7c000
    )
      return null;
    let grant = null;
    if (kind === 1) {
      const word = r.u32(o + 24),
        rbStart = word >>> 23,
        rb = r.u8(o + 28);
      if (!inRange(rb, 1, 255) || rbStart + rb > 275) return null;
      grant = { rbStart, rb, mcs: (word >>> 16) & 31 };
    }
    rows.push({
      time: t,
      kind,
      rntiCandidateA: r.u16(o + 12),
      rntiCandidateB: r.u16(o + 14),
      grant,
    });
    o += 24 + size;
  }
  return o === b.length ? rows : null;
});
export const pdschStatus = safe((b) => {
  const r = reader(b);
  if (!B887.includes(b.length) || !mac(b, r, 0x30003, true)) return null;
  const count = r.u8(7);
  if (!inRange(count, 1, 7)) return null;
  let o = 8;
  const rows = [];
  for (let i = 0; i < count; i++) {
    if (o + 8 > b.length) return null;
    const t = time(r, o),
      n = r.u8(o + 4);
    if (!t || !inRange(n, 1, 2) || o + 8 + n * 20 > b.length) return null;
    const readings = [];
    for (let j = 0; j < n; j++) {
      const p = o + 8 + j * 20,
        cell = r.u32(p + 4),
        pci = cell & 1023,
        arfcn = cell >>> 10,
        rb = r.u16(p + 12) & 511;
      if (pci > 1007 || arfcn > 3279165 || !inRange(rb, 1, 275)) return null;
      const modulationCode = r.u8(p + 18) & 15;
      readings.push({
        pci,
        arfcn,
        mcs: (r.u8(p + 11) >>> 2) & 31,
        modulationCode,
        modulation:
          { 1: "QPSK", 5: "16QAM", 9: "64QAM" }[modulationCode] ?? null,
        rb,
        layers: ((r.u16(p + 14) >>> 13) & 3) + 1,
        rxChains: { 1: 2, 3: 4 }[r.u8(p + 18) >>> 6] ?? null,
      });
    }
    rows.push({ time: t, statusCount: n, readings });
    o += 8 + n * 20;
  }
  return o === b.length ? rows : null;
});
export const cdrx = safe((b) => {
  const r = reader(b);
  if (!B890.includes(b.length) || !mac(b, r, 0x30005)) return null;
  const count = r.u8(7),
    enabled = r.u16(8);
  if (
    !inRange(count, 1, 23) ||
    36 + count * 28 !== b.length ||
    enabled > 1 ||
    r.u16(10) !== 10
  )
    return null;
  if (!enabled && b.slice(12, 36).some((v) => v !== 0)) return null;
  if (
    enabled &&
    (r.u16(12) !== 80 ||
      r.u16(14) !== 0x404 ||
      r.u16(16) !== 16 ||
      r.u16(18) !== 16 ||
      r.u16(24) !== 160 ||
      r.u16(26) !== 0x201 ||
      r.u16(28) !== 20 ||
      ![0, 16].includes(r.u16(30)) ||
      r.i32(32) !== 0 ||
      r.u32(20) >= 160)
  )
    return null;
  const events = [];
  for (let i = 0; i < count; i++) {
    const o = 36 + i * 28,
      w = r.u32(o),
      mu = w & 7,
      slot = (w >>> 13) & 127,
      previousState = (w >>> 20) & 15,
      currentState = (w >>> 24) & 15,
      reasonCode = r.i32(o + 4);
    if (
      mu !== 1 ||
      slot >= 20 ||
      w >>> 28 !== 0 ||
      ![0, 1, 2, 4].includes(previousState) ||
      ![1, 2, 4].includes(currentState) ||
      ![1, 2, 3, 4, 7, 8].includes(reasonCode)
    )
      return null;
    events.push({
      time: { sfn: (w >>> 3) & 1023, slot, numerology: mu },
      previousState,
      currentState,
      reasonCode,
    });
  }
  return {
    enabled: enabled === 1,
    onDurationRaw: r.u16(10),
    inactivityRaw: r.u16(12),
    longCycleRaw: r.u16(24),
    shortCycleRaw: r.u16(28),
    startOffsetRaw: r.u32(20),
    events,
  };
});
export const servingFtl = safe((b) => {
  const r = reader(b);
  if (b.length !== 644 || r.u32(0) !== 0x3000b || ![0, 2048].includes(r.i32(8)))
    return null;
  const word = r.u32(4),
    pci = (word >>> 14) & 1023,
    rfKHz = r.u32(128),
    arfcn = r.u32(136);
  if (
    pci > 1007 ||
    !inRange(rfKHz, 300000, 7125000) ||
    r.u32(132) !== rfKHz ||
    !inRange(arfcn, 1, 875000) ||
    Math.abs(
      rfKHz - (arfcn < 600000 ? arfcn * 5 : 3000000 + (arfcn - 600000) * 15),
    ) > 100000
  )
    return null;
  return { sfn: word & 1023, pci, rfKHz, arfcn };
});
export const nrServing = safe((b) => {
  const r = reader(b);
  if (b.length !== 46 || r.u32(0) !== 0x30000) return null;
  const pci = r.u16(4),
    dlArfcn = r.u32(14),
    ulArfcn = r.u32(18),
    band = r.u16(44),
    cellId = BigInt(r.u32(26)) + (BigInt(r.u8(30)) << 32n),
    digits = r.u8(36),
    mnc = r.u16(37),
    mcc = r.u16(34),
    tac = r.u32(40),
    dlBwMhz = r.u16(22),
    ulBwMhz = r.u16(24),
    bw = [10, 15, 20, 25, 30, 40, 50, 60, 70, 80, 90, 100];
  if (
    pci > 1007 ||
    !inRange(dlArfcn, 620000, 653333) ||
    !inRange(ulArfcn, 620000, 653333) ||
    band !== 78 ||
    cellId <= 0n ||
    cellId >= 1n << 36n ||
    ![2, 3].includes(digits) ||
    !inRange(mcc, 100, 999) ||
    mnc >= (digits === 2 ? 100 : 1000) ||
    tac > 0xffffff ||
    !bw.includes(dlBwMhz) ||
    !bw.includes(ulBwMhz)
  )
    return null;
  return {
    pci,
    dlArfcn,
    ulArfcn,
    band,
    cellId: String(cellId),
    mcc: String(mcc),
    mnc: String(mnc).padStart(digits, "0"),
    tac,
    dlBwMhz,
    ulBwMhz,
  };
});

// LTE timing profiles. Payloads explicitly stay partial where App has no unit/field evidence.
const LTE_RECORDS = {
  0xb111: {
    version: 164,
    head: 4,
    stride: 128,
    lengths: [132, 260, 388, 516, 644, 772, 900, 1028, 1156, 1284],
    count: (r) => (r.u8(1) >>> 1) & 15,
    shift: 2,
    scope: "推导 AGC 记录时序；AGC/RSSI 数值及增益未解码",
  },
  0xb114: {
    version: 161,
    head: 16,
    stride: 48,
    lengths: [
      112, 160, 208, 256, 304, 352, 400, 448, 496, 544, 592, 688, 784, 832, 880,
      928, 976,
    ],
    count: (r) => r.u8(1),
    consecutive: true,
    scope: "推导帧时序记录；时序调整量及其单位未解码",
  },
  0xb11b: {
    version: 161,
    head: 28,
    stride: 92,
    lengths: [
      120, 212, 304, 396, 488, 580, 672, 764, 856, 948, 1040, 1132, 1224, 1316,
      1592, 1684, 1776, 1868,
    ],
    count: (r) => (r.u16(4) >>> 4) & 31,
    separate: true,
    scope: "推导 LTE 记录时序；0xB11B 载荷含义未解码",
  },
  0xb11d: {
    version: 143,
    head: 16,
    stride: 40,
    lengths: [
      56, 96, 136, 176, 216, 256, 296, 336, 376, 416, 456, 496, 536, 576, 696,
      736, 776, 816,
    ],
    count: (r) => r.u8(3) >>> 1,
    consecutive: true,
    scope: "推导 TTL 记录时序；跟踪环路调整量未解码",
  },
};
export const lteLog = safe((code, b) => {
  if (b.length < 4) return null;
  const r = reader(b),
    profile = LTE_RECORDS[code];
  if (profile) {
    const {
        head,
        stride,
        version,
        lengths,
        shift = 0,
        separate,
        consecutive,
        scope,
      } = profile,
      count = profile.count(r);
    if (
      !lengths.includes(b.length) ||
      r.u8(0) !== version ||
      !inRange(count, 1, 20) ||
      head + count * stride !== b.length
    )
      return null;
    const times = [];
    for (let i = 0; i < count; i++) {
      const off = head + i * stride,
        w = r.u32(off) >>> shift,
        subframe = separate ? r.u16(off + 2) : (w >>> 10) & 15;
      if (subframe > 9) return null;
      const t = { sfn: w & 1023, subframe },
        last = times.at(-1);
      if (last) {
        const delta =
          (t.sfn * 10 + t.subframe - last.sfn * 10 - last.subframe + 10240) %
          10240;
        if (
          ((separate || consecutive) && delta !== 1) ||
          (shift === 2 && !inRange(delta, 0, 2))
        )
          return null;
      }
      times.push(t);
    }
    return { times, recordBytes: stride, scope };
  }
  if (code === 0xb134) {
    if (
      ![
        172, 236, 324, 452, 628, 668, 780, 884, 932, 1084, 1236, 1388, 1540,
        1748, 2180,
      ].includes(b.length) ||
      b[0] !== 162
    )
      return null;
    const variant = b[12] & 7,
      count = b[12] >>> 3,
      stride = 88 + 32 * variant,
      pci = r.u16(2) & 511,
      startSfn = r.u16(16) & 1023;
    if (
      ![2, 4].includes(variant) ||
      !inRange(count, 1, 10) ||
      20 + count * stride !== b.length ||
      pci > 503
    )
      return null;
    const times = [];
    for (let i = 0; i < count; i++) {
      const w = r.u16(20 + i * stride),
        subframe = (w >>> 4) & 15;
      if (subframe > 9) return null;
      times.push({ sfn: (startSfn + (w & 15)) % 1024, subframe });
    }
    return {
      times,
      pci,
      recordBytes: stride,
      scope: "推导 UERS 小区、时序和分组边界；SNR 数值及缩放未解码",
    };
  }
  if (code === 0xb193) {
    if (
      ![180, 344, 508, 672].includes(b.length) ||
      b[0] !== 1 ||
      b[1] !== 1 ||
      b[4] !== 25 ||
      b[5] !== 57 ||
      r.u16(6) !== b.length - 4
    )
      return null;
    const count = r.u32(12),
      earfcn = r.u32(8);
    if (
      !inRange(count, 1, 4) ||
      16 + count * 164 !== b.length ||
      !inRange(earfcn, 1, 262143)
    )
      return null;
    const cells = [];
    for (let i = 0; i < count; i++) {
      const o = 16 + i * 164,
        mask = r.i32(o),
        pci = r.u16(o + 8) & 511,
        w = r.u16(o + 12),
        t = { sfn: w & 1023, subframe: (w >>> 10) & 15 };
      if (![3, 15].includes(mask) || pci > 503 || t.subframe > 9) return null;
      const rsrp = [
          ((r.u32(o + 24) >>> 10) & 4095) / 16 - 180,
          ((r.u32(o + 28) >>> 12) & 4095) / 16 - 180,
          ((r.u32(o + 32) >>> 12) & 4095) / 16 - 180,
          (r.u32(o + 40) & 4095) / 16 - 180,
        ].slice(0, mask === 3 ? 2 : 4),
        rsrq = [
          (r.u32(o + 48) & 1023) / 16 - 30,
          ((r.u32(o + 48) >>> 20) & 1023) / 16 - 30,
        ];
      if (
        rsrp.some((p) => !inRange(p, -156, -31)) ||
        rsrq.some((q) => !inRange(q, -30, -3))
      )
        return null;
      cells.push({ pci, time: t, rsrp, rsrq });
    }
    return {
      cells,
      times: cells.map((c) => c.time),
      earfcn,
      recordBytes: 164,
      scope:
        "推导 v57 小区测量；RSRP/RSRQ 位域参照 v40 并经实包值域检查，未获对照解码确认",
    };
  }
  if (code === 0xb12a) {
    if (b.length !== 176 || b[0] !== 161 || b.slice(8, 16).some((v) => v !== 0))
      return null;
    const w = r.u16(1),
      pci = w & 511,
      sfn = r.u32(4),
      count = (w >>> 9) & 31;
    if (pci > 503 || sfn > 1023 || !inRange(count, 1, 20)) return null;
    let previous = -1;
    const times = [];
    for (let i = 0; i < count; i++) {
      const off = r.u16(16 + i * 8);
      if (!inRange(off, 0, 30) || off <= previous || (i === 0 && off !== 0))
        return null;
      previous = off;
      times.push({
        sfn: (sfn + Math.floor(off / 10)) % 1024,
        subframe: off % 10,
      });
    }
    return {
      times,
      pci,
      recordBytes: 8,
      scope: "推导 PCFICH 小区、帧号和子帧偏移；CFI/控制位域未解码",
    };
  }
  if (code === 0xb123) {
    const pci = r.u16(1) & 511;
    return b.length === 160 && b[0] === 41 && b[3] === 0 && pci <= 503
      ? { pci, scope: "仅推导邻区 PCI；CER 原始数组及能量单位未解码" }
      : null;
  }
  if (code === 0xb1dc) {
    if (
      b.length !== 136 ||
      r.u32(0) !== 0x130 ||
      r.u32(4) > 1023 ||
      r.i32(12) !== 0 ||
      ![1, 2].includes(r.i32(24)) ||
      [40, 48, 56, 60, 68, 76, 80, 88, 96, 108, 112, 128].some(
        (o) => r.i32(o) !== 0,
      )
    )
      return null;
    return { sfn: r.u32(4), scope: "仅推导 SFN；0xB1DC 状态载荷未解码" };
  }
  return null;
});
