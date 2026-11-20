// Optional offline verification against a user-supplied UFIPanel corpus; never commits captures.
import { readFileSync, writeFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { RrcDecoder } from "../web/rrc.js";
import { FrameReader, decodePacket } from "../web/signal.js";
import { bytesFromHex, parseJson } from "../web/core.js";
const schema = JSON.parse(
    gunzipSync(
      readFileSync(new URL("../web/rrc-schema.json.gz", import.meta.url)),
    ),
  ),
  decoder = new RrcDecoder(schema);
const canonical = (x) =>
  Array.isArray(x)
    ? x.map(canonical)
    : x && typeof x === "object"
      ? Object.fromEntries(
          Object.keys(x)
            .sort()
            .map((k) => [k, canonical(x[k])]),
        )
      : x;
const report = {
  golden: { matched: 0, rejected: 0, mismatched: 0 },
  captures: [],
};
for (const item of parseJson(
  gunzipSync(readFileSync(process.argv[2])).toString(),
)) {
  const result = decoder.decode(
    item.radio,
    item.channel,
    bytesFromHex(item.payload),
  );
  if (!result) {
    report.golden.rejected++;
    continue;
  }
  if (
    JSON.stringify(canonical(result.tree)) ===
    JSON.stringify(canonical(item.expected))
  )
    report.golden.matched++;
  else report.golden.mismatched++;
}
for (const path of process.argv.slice(3)) {
  const reader = new FrameReader(false),
    result = { frames: 0, decoded: 0, rrc: 0, codes: {} };
  reader.push(readFileSync(path), (code, packet) => {
    result.frames++;
    const event = decodePacket(code, packet, decoder);
    if (event.fields.length || event.tree) {
      result.decoded++;
      result.codes[event.code] = (result.codes[event.code] ?? 0) + 1;
    }
    if (event.tree) result.rrc++;
  });
  reader.finish();
  report.captures.push(result);
}
console.log(JSON.stringify(report, null, 2));
if (report.golden.mismatched) process.exitCode = 1;
