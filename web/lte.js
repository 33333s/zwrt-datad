const safe =
  (f) =>
  (...args) => {
    try {
      return f(...args);
    } catch {
      return null;
    }
  };
function reader(b) {
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    u8: (o) => v.getUint8(o),
    u16: (o) => v.getUint16(o, true),
    u32: (o) => v.getUint32(o, true),
  };
}
const BANDWIDTH = {
  6: "1.4MHz",
  15: "3MHz",
  25: "5MHz",
  50: "10MHz",
  75: "15MHz",
  100: "20MHz",
};
const BANDS = [
  [1, 0, 599, 2110],
  [2, 600, 1199, 1930],
  [3, 1200, 1949, 1805],
  [4, 1950, 2399, 2110],
  [5, 2400, 2649, 869],
  [7, 2750, 3449, 2620],
  [8, 3450, 3799, 925],
  [20, 6150, 6449, 791],
  [28, 9210, 9659, 758],
  [34, 36200, 36349, 2010],
  [38, 37750, 38249, 2570],
  [39, 38250, 38649, 1880],
  [40, 38650, 39649, 2300],
  [41, 39650, 41589, 2496],
];
export function lteCell(pci, arfcn, explicit) {
  const b = BANDS.find((b) => arfcn >= b[1] && arfcn <= b[2]);
  return {
    pci,
    arfcn,
    band: explicit ? `B${explicit}` : b ? `B${b[0]}` : "LTE",
    freq: b ? Math.trunc(b[3] + (arfcn - b[1]) / 10) : 0,
    radio: "LTE",
  };
}
export const lteServing = safe((b) => {
  const r = reader(b);
  if (b.length !== 29 || b[0] !== 3) return null;
  const pci = r.u16(1),
    dl = r.u32(3),
    ul = r.u32(7),
    dlBw = BANDWIDTH[b[11]],
    ulBw = BANDWIDTH[b[12]],
    cid = r.u32(13),
    tac = r.u16(17),
    band = r.u32(19),
    mcc = r.u16(23),
    digits = b[25],
    mnc = r.u16(26);
  if (
    pci > 503 ||
    dl > 262143 ||
    ul > 262143 ||
    !dlBw ||
    !ulBw ||
    cid > 0xfffffff ||
    band < 1 ||
    band > 88 ||
    mcc < 1 ||
    mcc > 999 ||
    ![2, 3].includes(digits) ||
    mnc >= (digits === 2 ? 100 : 1000)
  )
    return null;
  return {
    pci,
    dl,
    ul,
    dlBw,
    ulBw,
    cid: String(cid),
    tac,
    band,
    mcc: String(mcc).padStart(3, "0"),
    mnc: String(mnc).padStart(digits, "0"),
  };
});
export const lteMib = safe((b) => {
  const r = reader(b);
  if (b.length !== 11 || b[0] !== 2) return null;
  const pci = r.u16(1),
    arfcn = r.u32(3),
    sfn = r.u16(7),
    antennas = b[9],
    bandwidth = BANDWIDTH[b[10]];
  return pci <= 503 &&
    arfcn <= 262143 &&
    sfn <= 1023 &&
    [1, 2, 4].includes(antennas) &&
    bandwidth
    ? { pci, arfcn, sfn, antennas, bandwidth }
    : null;
});
const signed = (n) => (n >= 128 ? n - 256 : n);
export const ltePower = safe((code, b) => {
  const pusch = code === 0xb16e;
  if (!pusch && code !== 0xb16f) return null;
  const r = reader(b),
    count = b[3];
  if (
    b.length < 16 ||
    b[0] !== (pusch ? 48 : 49) ||
    count < 1 ||
    count > 50 ||
    b.length !== 4 + count * 12
  )
    return null;
  const rows = [];
  for (let i = 0; i < count; i++) {
    const o = 4 + i * 12,
      w = r.u32(o),
      last = r.u32(o + 8),
      sfn = (pusch ? w >>> 3 : w) & 1023,
      subframe = (w >>> (pusch ? 13 : 10)) & 15,
      dbm = signed(pusch ? (last >>> 15) & 255 : b[o + 11]);
    if (subframe > 9 || dbm < -50 || dbm > 33) return null;
    if (pusch) {
      const max = signed((last >>> 23) & 255);
      if (max < 0 || max > 33 || dbm > max) return null;
    } else {
      const req = signed(b[o + 10]);
      if (req < -50 || req > 40 || dbm !== Math.min(req, 23)) return null;
    }
    rows.push({ channel: pusch ? "PUSCH" : "PUCCH", dbm, sfn, subframe });
  }
  return rows;
});
export const ltePdsch = safe((b) => {
  const r = reader(b),
    count = b[1];
  if (
    b.length < 44 ||
    b[0] !== 48 ||
    count < 1 ||
    count > 50 ||
    b.length !== 4 + count * 40
  )
    return null;
  const blocks = [];
  for (let i = 0; i < count; i++) {
    const o = 4 + i * 40,
      t = r.u16(o),
      n = b[o + 4];
    if (
      (t & 15) > 9 ||
      t >>> 4 > 1023 ||
      n < 1 ||
      n > 2 ||
      b[o + 2] < 1 ||
      b[o + 2] > 110
    )
      return null;
    for (let j = 0; j < n; j++) {
      const k = o + 12 + j * 12,
        mcs = b[k + 6],
        rb = b[k + 7],
        modulation = { 2: "QPSK", 4: "16QAM", 6: "64QAM", 8: "256QAM" }[
          b[k + 8]
        ];
      if (mcs > 31 || rb < 1 || rb > 110 || !modulation) return null;
      blocks.push({
        crc: !!(b[k] & 128),
        bytes: r.u16(k + 4),
        mcs,
        rb,
        modulation,
      });
    }
  }
  return blocks;
});
