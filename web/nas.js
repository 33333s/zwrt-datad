import names from "./nas-names.json" with { type: "json" };
const safe =
  (f) =>
  (...args) => {
    try {
      return f(...args);
    } catch {
      return null;
    }
  };
export const plmn = (mcc, mnc, tac) => ({
  mcc,
  mnc,
  ...(tac == null ? {} : { tac }),
  name: names.PLMN[mcc + mnc] ?? `${mcc}-${mnc}`,
});
export function normalizeApn(value) {
  const v = String(value).trim().replace(/\.+$/, "").toLowerCase();
  return v.length >= 1 &&
    v.length <= 100 &&
    v
      .split(".")
      .every((s) => s.length >= 1 && s.length <= 63 && /^[a-z0-9-]+$/.test(s))
    ? v
    : null;
}
export const isDataApn = (value) => {
  const n = normalizeApn(value);
  return (
    n !== null &&
    !["ims", "sos", "emergency", "xcap", "mms"].includes(n.split(".")[0])
  );
};
export const decodeApn = safe((d) => {
  let p = 0;
  const labels = [];
  while (p < d.length) {
    const n = d[p++];
    if (n < 1 || n > 63 || p + n > d.length) return null;
    labels.push(new TextDecoder().decode(d.subarray(p, p + n)));
    p += n;
  }
  return normalizeApn(labels.join("."));
});
const be16 = (d, p) => d[p] * 256 + d[p + 1];
const AMBR_UNITS = [
  0n,
  1000n,
  4000n,
  16000n,
  64000n,
  256000n,
  1000000n,
  4000000n,
  16000000n,
  64000000n,
  256000000n,
  1000000000n,
  4000000000n,
  16000000000n,
  64000000000n,
  256000000000n,
];
function rateText(bps) {
  const [divisor, suffix] =
      bps >= 1000000000n
        ? [1000000000n, "Gbps"]
        : bps >= 1000000n
          ? [1000000n, "Mbps"]
          : [1000n, "Kbps"],
    fraction = String(bps % divisor)
      .padStart(String(divisor).length - 1, "0")
      .replace(/0+$/, "");
  return String(bps / divisor) + (fraction ? "." + fraction : "") + suffix;
}
export function sessionAmbr(d) {
  if (d.length !== 6 || d[0] < 1 || d[0] > 15 || d[3] < 1 || d[3] > 15)
    return null;
  return {
    dl: rateText(BigInt(be16(d, 1)) * AMBR_UNITS[d[0]]),
    ul: rateText(BigInt(be16(d, 4)) * AMBR_UNITS[d[3]]),
  };
}
export const qosFlows = safe((d) => {
  const rows = [];
  let p = 0;
  while (p < d.length) {
    if (p + 3 > d.length) return null;
    const qfi = d[p] & 63,
      operation = d[p + 1] >>> 5,
      count = d[p + 2] & 63;
    if (!qfi || operation < 1 || operation > 3) return null;
    p += 3;
    let qi = null,
      epsBearerId = null;
    for (let i = 0; i < count; i++) {
      if (p + 2 > d.length) return null;
      const id = d[p++],
        len = d[p++];
      if (p + len > d.length) return null;
      if (id === 1 && len === 1) qi = d[p] > 0 ? d[p] : null;
      if (id === 7 && len === 1) {
        const ebi = d[p] >>> 4;
        epsBearerId = ebi >= 5 && ebi <= 15 ? ebi : null;
      }
      p += len;
    }
    rows.push({ qfi, qi, operation, epsBearerId });
  }
  return rows;
});
const mappedEps = safe((d) => {
  let p = 0;
  const rows = [];
  while (p < d.length) {
    if (p + 4 > d.length) return null;
    const id = d[p] >>> 4,
      end = p + 3 + be16(d, p + 1),
      operation = d[p + 3] >>> 6,
      count = d[p + 3] & 15;
    if (
      id < 5 ||
      id > 15 ||
      d[p] & 15 ||
      end > d.length ||
      end < p + 4 ||
      operation < 1 ||
      operation > 3
    )
      return null;
    p += 4;
    let qci = null;
    for (let i = 0; i < count; i++) {
      if (p + 2 > end) return null;
      const type = d[p++],
        len = d[p++];
      if (p + len > end) return null;
      if (type === 1 && len > 0) qci = d[p] > 0 ? d[p] : null;
      p += len;
    }
    if (p !== end) return null;
    rows.push({ id, operation, qci });
  }
  return rows;
});
function registration(d, start) {
  let p = start;
  if (d[p] !== 1) return null;
  p += 2;
  while (p + 1 < d.length) {
    const ie = d[p];
    if (ie === 0x54) {
      const len = d[p + 1],
        end = Math.min(p + 2 + len, d.length);
      if (end - p - 2 < 7) return null;
      const base = p + 2,
        o1 = d[base + 1],
        o2 = d[base + 2],
        o3 = d[base + 3],
        mcc = `${o1 & 15}${(o1 >>> 4) & 15}${o2 & 15}`,
        m3 = (o2 >>> 4) & 15,
        mnc = `${o3 & 15}${(o3 >>> 4) & 15}` + (m3 !== 15 ? m3 : "");
      return plmn(
        mcc,
        mnc,
        ((d[base + 4] << 16) | (d[base + 5] << 8) | d[base + 6])
          .toString(16)
          .toUpperCase()
          .padStart(6, "0"),
      );
    }
    if ([0x77, 0x72, 0x78, 0x7a, 0x76, 0x74, 0x7b].includes(ie)) {
      if (p + 2 >= d.length) return null;
      p += 3 + be16(d, p + 1);
    } else p += 2 + d[p + 1];
  }
  return null;
}
export const decodeNas = safe((d) => {
  if (d[0] === 0x7e && d.length > 2 && d[1] === 0) {
    const mt = d[2];
    return {
      name: names.MM_NAMES[mt] ?? `5GMM 0x${mt.toString(16)}`,
      type: "5GMM",
      plmn: mt === 0x42 ? registration(d, 3) : null,
    };
  }
  if (d[0] !== 0x2e || d.length < 4) return null;
  const mt = d[3],
    out = {
      name: names.SM_NAMES[mt] ?? `5GSM 0x${mt.toString(16)}`,
      type: "5GSM",
      pduSessionId: d[1],
      pti: d[2],
    };
  if (mt !== 0xc2 && mt !== 0xcb) return out;
  let p = 4;
  if (mt === 0xc2) {
    if (p + 3 > d.length) return null;
    out.ssc = (d[p] >>> 4) & 7;
    out.pduType = d[p++] & 7;
    const n = be16(d, p);
    p += 2;
    if (p + n > d.length) return null;
    p += n;
    if (p >= d.length || d[p] !== 6 || p + 7 > d.length) return null;
    out.ambr = sessionAmbr(d.subarray(p + 1, p + 7));
    if (!out.ambr) return null;
    p += 7;
  }
  while (p < d.length) {
    const ie = d[p++];
    if (ie >= 128) continue;
    if (ie === 0x59 || ie === 0x56) {
      if (p >= d.length) return null;
      p++;
      continue;
    }
    const extended = [
      0x71, 0x72, 0x74, 0x75, 0x77, 0x78, 0x79, 0x7a, 0x7b,
    ].includes(ie);
    if (
      !extended &&
      ![0x17, 0x18, 0x1e, 0x1f, 0x22, 0x25, 0x29, 0x2a, 0x66].includes(ie)
    )
      break;
    const width = extended ? 2 : 1;
    if (p + width > d.length) return null;
    const len = extended ? be16(d, p) : d[p];
    p += width;
    if (p + len > d.length) {
      if (ie === 0x7b) break;
      return null;
    }
    const value = d.subarray(p, p + len);
    switch (ie) {
      case 0x2a:
        out.ambr = sessionAmbr(value);
        if (!out.ambr) return null;
        break;
      case 0x25:
        out.dnn = decodeApn(value);
        break;
      case 0x79:
        out.qosFlows = qosFlows(value);
        if (!out.qosFlows) return null;
        break;
      case 0x75:
        out.mappedEpsBearers = mappedEps(value);
        if (!out.mappedEpsBearers) return null;
        break;
      case 0x29:
        if (value.length) {
          out.pduType = value[0] & 7;
          out.ueIp =
            out.pduType === 1 && len === 5
              ? [...value.subarray(1)].join(".")
              : null;
        }
        break;
    }
    p += len;
  }
  return out;
});
function epsRate(base, ext, ext2) {
  let lower;
  if (ext === 0) {
    if (base >= 1 && base <= 63) lower = base;
    else if (base >= 64 && base <= 127) lower = 64 + (base - 64) * 8;
    else if (base >= 128 && base <= 254) lower = 576 + (base - 128) * 64;
    else if (base === 255) lower = 0;
    else return null;
  } else if (ext <= 74) lower = 8600 + ext * 100;
  else if (ext <= 186) lower = 16000 + (ext - 74) * 1000;
  else if (ext <= 250) lower = 128000 + (ext - 186) * 2000;
  else lower = 256000;
  const kbps = lower + (ext2 >= 1 && ext2 <= 254 ? ext2 * 256000 : 0);
  return kbps % 1000 === 0 ? `${kbps / 1000} Mbps` : `${kbps} kbps`;
}
const pcoFlows = safe((d) => {
  if (!d.length || !(d[0] & 128)) return null;
  let p = 1,
    flows = null;
  while (p < d.length) {
    if (p + 3 > d.length) return null;
    const id = be16(d, p),
      len = d[p + 2];
    p += 3;
    if (p + len > d.length) return null;
    if (id === 0x1f) {
      flows = qosFlows(d.subarray(p, p + len));
      if (!flows) return null;
    }
    p += len;
  }
  return flows;
});
const EMM = {
  0x41: "Attach Request",
  0x42: "Attach Accept",
  0x43: "Attach Complete",
  0x48: "TAU Request",
  0x49: "TAU Accept",
  0x52: "Authentication Request",
  0x53: "Authentication Response",
  0x55: "Identity Request",
  0x56: "Identity Response",
  0x5d: "Security Mode Command",
  0x5e: "Security Mode Complete",
  0x60: "EMM Status",
  0x61: "EMM Information",
};
const ESM = {
  0xc1: "Activate Default EPS Bearer",
  0xc2: "Default EPS Bearer Accept",
  0xc5: "Activate Dedicated EPS Bearer",
  0xc6: "Dedicated EPS Bearer Accept",
  0xc9: "Modify EPS Bearer",
  0xca: "Modify EPS Bearer Accept",
  0xcd: "Deactivate EPS Bearer",
  0xce: "Deactivate EPS Bearer Accept",
  0xd0: "PDN Connectivity Request",
  0xd1: "PDN Connectivity Reject",
  0xd2: "PDN Disconnect Request",
  0xd9: "ESM Information Request",
  0xda: "ESM Information Response",
};
export const decodeEps = safe((b) => {
  if (b.length < 2) return null;
  const pd = b[0] & 15;
  if (pd === 7) {
    if (b[0] >>> 4 !== 0) return null;
    const type = b[1],
      name = EMM[type];
    if (!name) return null;
    const nas = { name: `LTE ${name}`, type: "EMM" };
    if (type === 0x5d) {
      if (b.length < 5 || b[2] & 0x88) return null;
      return {
        nas,
        security: { cipher: `EEA${b[2] >>> 4}`, integrity: `EIA${b[2] & 7}` },
      };
    }
    if (type === 0x42) {
      if (b.length < 7) return null;
      const taiLen = b[4],
        next = 5 + taiLen;
      if (taiLen < 6 || next + 2 > b.length) return null;
      const len = be16(b, next);
      if (len < 3 || next + 2 + len > b.length) return null;
      const nested = decodeEps(b.subarray(next + 2, next + 2 + len))?.nas;
      for (const k of ["ambr", "ueIp", "dnn", "qci", "epsBearerId", "qosFlows"])
        nas[k] = nested?.[k] ?? null;
    }
    return { nas };
  }
  if (pd !== 2 || b.length < 3) return null;
  const type = b[2],
    ebi = b[0] >>> 4,
    name = ESM[type];
  if (!name) return null;
  const nas = { name: `LTE ${name}`, type: "ESM", pti: b[1], epsBearerId: ebi };
  if (![0xc1, 0xc5, 0xc9].includes(type)) return { nas };
  let p = 3;
  function lv() {
    if (p >= b.length) return null;
    const n = b[p++];
    if (p + n > b.length) return null;
    const value = b.subarray(p, p + n);
    p += n;
    return value;
  }
  if (type === 0xc5) {
    if (p >= b.length) return null;
    const linked = b[p++] & 15;
    if (linked < 5 || linked > 15) return null;
    const qos = lv(),
      tft = lv();
    if (!qos?.length || !qos[0] || !tft?.length) return null;
    return { nas: { ...nas, qci: qos[0], linkedEpsBearerId: linked } };
  }
  if (type === 0xc1) {
    const qos = lv();
    if (!qos?.length || !qos[0]) return null;
    const apn = lv(),
      address = lv();
    if (!apn || !address) return null;
    const labels = [];
    let at = 0;
    while (at < apn.length) {
      const n = apn[at++];
      if (!n || at + n > apn.length) return null;
      labels.push(new TextDecoder().decode(apn.subarray(at, at + n)));
      at += n;
    }
    nas.qci = qos[0];
    nas.dnn = labels.join(".");
    nas.ueIp =
      (address.length === 5 && (address[0] & 7) === 1) ||
      (address.length === 13 && (address[0] & 7) === 3)
        ? [...address.subarray(-4)].join(".")
        : null;
  }
  while (p < b.length) {
    const ie = b[p++];
    if (ie >= 128) continue;
    if (ie === 0x32 || ie === 0x58) {
      if (p >= b.length) return null;
      p++;
      continue;
    }
    if (ie === 0x7b) {
      if (p + 2 > b.length) return null;
      const len = be16(b, p);
      p += 2;
      if (p + len > b.length) return null;
      const flows = pcoFlows(b.subarray(p, p + len));
      if (flows) nas.qosFlows = flows;
      p += len;
      continue;
    }
    if (
      ![
        0x5e, 0x27, 0x30, 0x5d, 0x34, 0x36, 0x33, 0x66, 0x6e, 0x5f, 0x5b, 0x5c,
      ].includes(ie)
    )
      break;
    const value = lv();
    if (!value) return null;
    if (ie === 0x27) {
      const flows = pcoFlows(value);
      if (flows) nas.qosFlows = flows;
    }
    if (ie === 0x5e) {
      if (![2, 4, 6].includes(value.length)) return null;
      const dl = epsRate(
          value[0],
          value.length >= 4 ? value[2] : 0,
          value.length === 6 ? value[4] : 0,
        ),
        ul = epsRate(
          value[1],
          value.length >= 4 ? value[3] : 0,
          value.length === 6 ? value[5] : 0,
        );
      if (dl !== null && ul !== null) nas.ambr = { dl, ul };
    }
    if (ie === 0x5b && type === 0xc9 && value.length && value[0] > 0)
      nas.qci = value[0];
  }
  return { nas };
});
