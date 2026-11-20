export function timingAdvanceDistance(
  value,
  radio,
  duplex = "FDD",
  input = "RAR_COMMAND",
  numerology = 0,
  nrOffset = null,
  lteOffsetOverride = null,
) {
  if (
    !Number.isSafeInteger(value) ||
    value < 0 ||
    !Number.isInteger(numerology) ||
    numerology < 0 ||
    numerology > 4 ||
    !["LTE", "NR"].includes(radio)
  )
    return null;
  const sample = radio === "LTE" ? 1 / 30720000 : 1 / 1966080000,
    step = radio === "LTE" ? 16 : 1024 >> numerology,
    offset =
      input !== "TOTAL_ADVANCE"
        ? 0
        : radio === "LTE"
          ? (lteOffsetOverride ?? (duplex === "TDD" ? 624 : 0))
          : nrOffset;
  if (
    offset === null ||
    !(radio === "LTE" ? [0, 624] : [0, 25600, 39936, 13792]).includes(offset)
  )
    return null;
  let units;
  if (input === "RAR_COMMAND") {
    if (value > (radio === "LTE" ? 1282 : 3846)) return null;
    units = value * step;
  } else if (input === "N_TA") units = value;
  else if (input === "TOTAL_ADVANCE") units = value - offset;
  else return null;
  if (units < 0 || units > (radio === "LTE" ? 1282 : 3846) * step) return null;
  const seconds = units * sample;
  return {
    meters: (seconds * 299792458) / 2,
    delayMicroseconds: seconds * 1e6,
    deductedOffset: offset,
    sampleSeconds: sample,
    commandStepMeters: (step * sample * 299792458) / 2,
  };
}
export function automaticTa(code, packet, now) {
  try {
    const p = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
    if (
      packet.length < 12 ||
      p.getUint16(0, true) !== packet.length ||
      p.getUint16(2, true) !== code
    )
      return null;
    const b = packet.subarray(12),
      v = new DataView(b.buffer, b.byteOffset, b.byteLength),
      w = (o) => v.getUint16(o, true);
    let command, numerology, radio;
    if (code === 0xb88a) {
      if (
        b.length !== 160 ||
        w(0) !== 8 ||
        w(2) !== 3 ||
        w(4) !== 0 ||
        b[6] !== 0 ||
        b[7] !== 1 ||
        b[14] !== 0 ||
        b[15] !== 1 ||
        b[16] !== 15 ||
        b[106] !== 1 ||
        b[107] !== 1 ||
        w(102) === 0 ||
        w(102) !== w(156) ||
        b[62] > 63 ||
        b[62] !== b[116] ||
        [55, 79, 83, 99].some((i) => b[i] !== 1)
      )
        return null;
      command = w(104);
      numerology = 1;
      radio = "NR";
      if (command > 3846) return null;
    } else if (code === 0xb062) {
      if (
        b.length !== 60 ||
        b[0] !== 1 ||
        b[1] !== 1 ||
        b[4] !== 6 ||
        b[5] !== 50 ||
        w(6) !== 56 ||
        b[9] !== 0 ||
        b[11] !== 0 ||
        ![3, 7].includes(b[13]) ||
        b[14] > 63 ||
        b[23] !== 1 ||
        w(24) === 0
      )
        return null;
      command = w(26);
      numerology = 0;
      radio = "LTE";
      if (command > 1282) return null;
    } else return null;
    const ts = p.getBigInt64(4, true);
    if (ts <= 0n) return null;
    return {
      radio,
      command,
      numerology,
      source: `${code.toString(16).toUpperCase()} · RAR`,
      updatedAt: now,
      diagTimestamp: String(ts),
      estimate: timingAdvanceDistance(
        command,
        radio,
        "FDD",
        "RAR_COMMAND",
        numerology,
      ),
    };
  } catch {
    return null;
  }
}
const abs = (n) => (n < 0n ? -n : n);
export const emptyNrPower = () => ({ controls: [], commands: [] });
export function correlateNrPower(old, code, timestamp, values) {
  if (!["control", "rf"].includes(code)) return { state: old, readings: [] };
  const ts = BigInt(timestamp);
  if (ts <= 0n) return { state: old, readings: [] };
  const close = (v) => abs(BigInt(v.timestamp) - ts) <= 2000000000n,
    controls = [
      ...old.controls.filter(close),
      ...(code === "control"
        ? values.map((value) => ({ timestamp, value }))
        : []),
    ].slice(-4096),
    commands = [
      ...old.commands.filter(close),
      ...(code === "rf"
        ? values
            .filter((value) => value.power)
            .map((value) => ({ timestamp, value }))
        : []),
    ].slice(-4096),
    bySlot = new Map();
  for (const c of controls) {
    const k = `${c.value.time.sfn}:${c.value.time.slot}`;
    if (!bySlot.has(k)) bySlot.set(k, []);
    bySlot.get(k).push(c);
  }
  const pending = [],
    readings = [];
  for (const command of commands) {
    const c = command.value,
      p = c.power;
    if (!p) continue;
    const candidates = (bySlot.get(`${c.sfn}:${c.slot}`) ?? []).filter(
      (c) =>
        c.value.time.numerology === 1 &&
        abs(BigInt(c.timestamp) - BigInt(command.timestamp)) <= 1000000n,
    );
    let match = null;
    if (new Set(candidates.map((c) => c.value.channel)).size === 1)
      match = candidates.reduce((a, b) =>
        abs(BigInt(a.timestamp) - BigInt(command.timestamp)) <=
        abs(BigInt(b.timestamp) - BigInt(command.timestamp))
          ? a
          : b,
      ).value;
    if (!match) {
      pending.push(command);
      continue;
    }
    if (!["PUSCH", "PUCCH"].includes(match.channel)) continue;
    if (Math.abs(p.dbm - Math.min(match.requestedDbm, match.limitDbm)) > 0.11) {
      pending.push(command);
      continue;
    }
    readings.push({
      channel: match.channel,
      rf: c,
      mac: match,
      timestamp: command.timestamp,
    });
  }
  return { state: { controls, commands: pending }, readings };
}
