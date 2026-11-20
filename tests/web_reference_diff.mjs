// Offline differential check against web_reference_replay.py's JVM output.
// Reports contain counts and field paths, never captured payloads or identities.
import { readFileSync, createReadStream, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
import { gunzipSync } from "node:zlib";
import { parseJson } from "../web/core.js";
import { RrcDecoder } from "../web/rrc.js";
import { decodePacket } from "../web/signal.js";
import { createCards, applyCards } from "../web/signal-cards.js";
const [reference, ...paths] = process.argv.slice(2);
if (!reference || !paths.length)
  throw Error("Usage: node tests/web_reference_diff.mjs JVM_NDJSON CAPTURE...");
const captures = paths.map((p) => readFileSync(p)),
  positions = captures.map(() => 0),
  decoder = new RrcDecoder(
    JSON.parse(
      gunzipSync(
        readFileSync(new URL("../web/rrc-schema.json.gz", import.meta.url)),
      ),
    ),
  );
const states = paths.map(() => createCards()),
  report = {
    frames: 0,
    parsed: 0,
    nas: 0,
    cardComparisons: 0,
    engineeringComparisons: 0,
    mismatches: 0,
    paths: {},
    examples: [],
  };
function mismatch(path, capture, index, code) {
  report.mismatches++;
  report.paths[path] = (report.paths[path] ?? 0) + 1;
  if (report.examples.length < 25)
    report.examples.push({
      capture,
      index,
      code: "0x" + code.toString(16),
      path,
    });
}
function compare(expected, actual, path, meta) {
  if (expected == null && actual == null) return;
  if (typeof expected === "string" && typeof actual === "string") {
    // Kotlin Double.toString keeps .0; JS Number.toString does not. Unit/value is unchanged.
    const canonical = (s) => s.replace(/(-?\d+)\.0+(?=[^\d]|$)/g, "$1");
    if (canonical(expected) !== canonical(actual)) mismatch(path, ...meta);
    return;
  }
  if (
    typeof expected === "number" &&
    typeof actual === "string" &&
    /^-?\d+$/.test(actual) &&
    Number.isInteger(expected)
  ) {
    if (BigInt(expected) !== BigInt(actual)) mismatch(path, ...meta);
    return;
  }
  if (
    typeof expected === "string" &&
    typeof actual === "number" &&
    Number.isInteger(actual) &&
    /^-?\d+$/.test(expected)
  ) {
    if (BigInt(expected) !== BigInt(actual)) mismatch(path, ...meta);
    return;
  }
  if (typeof expected === "number" && typeof actual === "number") {
    if (
      !Number.isFinite(actual) ||
      Math.abs(expected - actual) > 1e-8 * Math.max(1, Math.abs(expected))
    )
      mismatch(path, ...meta);
    return;
  }
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual) || expected.length !== actual.length) {
      mismatch(path + ".length", ...meta);
      return;
    }
    expected.forEach((v, i) =>
      compare(v, actual[i], path + "[" + i + "]", meta),
    );
    return;
  }
  if (expected && typeof expected === "object") {
    if (!actual || typeof actual !== "object") {
      mismatch(path, ...meta);
      return;
    }
    for (const [k, v] of Object.entries(expected))
      compare(v, actual[k], path + "." + k, meta);
    return;
  }
  if (expected !== actual) mismatch(path, ...meta);
}
for await (const line of createInterface({
  input: createReadStream(reference),
  crlfDelay: Infinity,
})) {
  const ref = parseJson(line),
    capture = ref.capture,
    index = ref.index,
    buf = captures[capture],
    offset = positions[capture],
    len = buf.readUInt32LE(offset);
  positions[capture] += 4 + len;
  const packet = new Uint8Array(buf.buffer, buf.byteOffset + offset + 4, len),
    event = decodePacket(ref.code, packet, decoder, 1000000 + index),
    meta = [capture, index, ref.code];
  report.frames++;
  const actual =
    event.m?.kind === "powerControlTimes" ? null : (event.m?.value ?? null);
  if (ref.parsed !== null) {
    report.parsed++;
    let value = actual;
    if (event.m?.kind === "lteServing") value = { ...actual, plmn: event.plmn };
    if (event.m?.kind === "lteLog")
      value = {
        times: [],
        pci: null,
        earfcn: null,
        cells: [],
        recordBytes: null,
        sfn: null,
        ...actual,
      };
    compare(ref.parsed, value, "parsed", meta);
  } else if (
    [
      "ulTb",
      "pdschStats",
      "arbitration",
      "powerControl",
      "txRf",
      "ulSchedule",
      "pdschStatus",
      "cdrx",
      "servingFtl",
      "nrServing",
      "lteLog",
      "lteServing",
      "lteMib",
      "ltePower",
      "ltePdsch",
    ].includes(event.m?.kind)
  )
    mismatch("unexpected_parsed", ...meta);
  const expectedNas = ref.event.nas ?? null;
  if (expectedNas || event.nas) {
    report.nas++;
    compare(expectedNas, event.nas ?? null, "nas", meta);
  }
  for (const key of ["security", "broadcastPlmns", "selectedPlmnIndex"])
    compare(ref.event[key] ?? null, event[key] ?? null, "event." + key, meta);
  states[capture] = applyCards(states[capture], ref.code, event);
  const cards = states[capture];
  report.cardComparisons++;
  const ignored = new Set(["updatedAt"]);
  for (const [key, value] of Object.entries(ref.cards))
    if (!ignored.has(key))
      compare(
        key === "linkParams" ? Object.fromEntries(value) : value,
        cards[key],
        "cards." + key,
        meta,
      );
  if (ref.engineering) {
    report.engineeringComparisons++;
    compare(ref.engineering, cards.engineering, "engineering", meta);
  }
}
for (const [i, b] of captures.entries())
  if (positions[i] !== b.length)
    throw Error("Reference/capture frame count mismatch");
writeFileSync(
  "build/reports/web-reference-diff.json",
  JSON.stringify(report, null, 2),
);
console.log(JSON.stringify(report, null, 2));
if (report.mismatches) process.exitCode = 1;
