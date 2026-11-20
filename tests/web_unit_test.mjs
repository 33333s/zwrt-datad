import test from "node:test";
import assert from "node:assert/strict";
import {
  Client,
  parseJson,
  validPath,
  mutablePath,
  childPath,
  hex,
  bytesFromHex,
  imeiFromNv,
  nvConfig,
} from "../web/core.js";
import { EfsClient, checkedPage, checkedListing } from "../web/efs.js";
import { FrameReader, decodePacket, neighbors, agc } from "../web/signal.js";
import { RrcDecoder } from "../web/rrc.js";

const packet = (code = 0xb8d8, size = 40) => {
  const b = new Uint8Array(size),
    v = new DataView(b.buffer);
  v.setUint16(0, size, true);
  v.setUint16(2, code, true);
  return b;
};
const frame = (p, wire = true) => {
  const b = new Uint8Array(p.length + (wire ? 8 : 4)),
    v = new DataView(b.buffer);
  v.setUint32(0, p.length, true);
  if (wire) v.setUint32(4, new DataView(p.buffer).getUint16(2, true), true);
  b.set(p, wire ? 8 : 4);
  return b;
};

test("JSON preserves exact large integers without touching strings or safe/decimal values", () => {
  const data = parseJson(
    '{"id":9007199254740993,"negative":-9007199254740993,"safe":9007199254740991,"s":"\\\"9007199254740993","n":0.15,"e":1e3}',
  );
  assert.equal(data.id, "9007199254740993");
  assert.equal(data.negative, "-9007199254740993");
  assert.equal(data.safe, 9007199254740991);
  assert.equal(data.s, '"9007199254740993');
  assert.equal(data.n, 0.15);
  assert.equal(data.e, 1000);
  assert.throws(() => parseJson('{"a":01}'));
});
test("EFS path rules protect root, raw names and both move endpoints", () => {
  for (const path of ["/", "/nvefs"]) {
    assert.equal(validPath(path), true);
    assert.equal(mutablePath(path), false);
  }
  assert.equal(mutablePath("/nvefs/nvconfig.ini"), true);
  for (const path of [
    "relative",
    "/a/../b",
    "/a/./b",
    "/a//b",
    "/a/",
    "/a\u0085b",
    "/a\ud800",
    "/" + "a".repeat(1011),
  ])
    assert.equal(validPath(path), false, path);
  assert.equal(childPath("/nvefs", "a..b"), "/nvefs/a..b");
  assert.throws(() => childPath("/nvefs", "../b"));
  const entries = checkedListing(
    {
      path: "/nvefs",
      entries: [
        { name: "é", name_hex: "c3a9" },
        { name: "ÿ", name_hex: "ff" },
      ],
    },
    "/nvefs",
  );
  assert.equal(entries[0].nameValid, true);
  assert.equal(entries[1].nameValid, false);
  assert.throws(() =>
    checkedListing({ path: "/other", entries: [] }, "/nvefs"),
  );
});
test("NV serialization matches UFIPanel and malformed replies retain raw HEX", () => {
  const config = nvConfig("866834080042000");
  assert.match(config, /par=0x08,0x8A,0x66,0x38,0x04,0x08,0x40,0x02,0x00\n$/);
  const bytes = new Uint8Array(133);
  bytes.set([0x26, 0x26, 2, 8, 0x8a, 0x66, 0x38, 4, 8, 0x40, 2, 0]);
  assert.equal(
    imeiFromNv({ encoding: "hex", bytes: 133, hex: hex(bytes) }),
    "866834080042000",
  );
  bytes[131] = 1;
  assert.equal(
    imeiFromNv({ encoding: "hex", bytes: 133, hex: hex(bytes) }),
    null,
  );
  for (const value of [
    "000000000000000",
    "86683408004200",
    "866834080042000a",
    "８６６８３４０８００４２０００",
  ])
    assert.throws(() => nvConfig(value));
  assert.throws(() => bytesFromHex("abc"));
  assert.throws(() => bytesFromHex("0g"));
});
test("EFS page validation refuses wrong offsets, lengths and stalled reads", () => {
  assert.equal(
    checkedPage(
      { hex: "0102", offset: 4, bytes: 2, next_offset: 6, eof: false },
      4,
      4096,
    ).bytes,
    2,
  );
  for (const data of [
    { hex: "01", offset: 0, bytes: 1, next_offset: 2, eof: false },
    { hex: "", offset: 0, bytes: 0, next_offset: 0, eof: false },
    { hex: "01", offset: 1, bytes: 1, next_offset: 2, eof: true },
  ])
    assert.throws(() => checkedPage(data, 0, 4096));
});
test("wire and exported frames survive every split, and reject corrupt/truncated envelopes", () => {
  for (const wire of [true, false]) {
    const p = packet(),
      f = frame(p, wire);
    for (let split = 0; split <= f.length; split++) {
      const reader = new FrameReader(wire),
        out = [];
      reader.push(f.subarray(0, split), (code, bytes) =>
        out.push([code, hex(bytes)]),
      );
      reader.push(f.subarray(split), (code, bytes) =>
        out.push([code, hex(bytes)]),
      );
      reader.finish();
      assert.deepEqual(out, [[0xb8d8, hex(p)]]);
    }
    const reader = new FrameReader(wire);
    for (const byte of f) reader.push(Uint8Array.of(byte), () => {});
    reader.finish();
    const truncated = new FrameReader(wire);
    truncated.push(f.subarray(0, f.length - 1), () => {});
    assert.throws(() => truncated.finish());
  }
  const bad = frame(packet());
  bad[4] = 0;
  assert.throws(() => new FrameReader().push(bad, () => {}));
  assert.throws(() =>
    new FrameReader().push(Uint8Array.of(255, 255, 255, 255), () => {}),
  );
});
test("known SNR is strict about layout and physical range; unsupported packets stay raw", () => {
  const p = packet(),
    v = new DataView(p.buffer);
  v.setUint32(12, 0x30002, true);
  v.setInt32(24, 128 * 20, true);
  assert.equal(decodePacket(0xb8d8, p).snr, 20);
  v.setInt32(24, 128 * 50, true);
  assert.equal(decodePacket(0xb8d8, p).snr, undefined);
  assert.equal(decodePacket(0xb8d8, p).payload_hex, hex(p));
  assert.equal(neighbors(new Uint8Array(52)), null);
  assert.equal(agc(new Uint8Array(180)), null);
});
test("UPER constraints, unknown extensions and truncation fail closed", () => {
  const schema = {
      roots: { "NR:X": 0 },
      nodes: [
        { t: "SEQUENCE", root: [["value", 1, false]], ext: true, extra: [] },
        { t: "INTEGER", min: -2, max: 2 },
      ],
    },
    decoder = new RrcDecoder(schema);
  assert.equal(decoder.decode("NR", "X", Uint8Array.of(0x20)).tree.value, 0);
  assert.equal(decoder.decode("NR", "X", Uint8Array.of(0x70)), null); // Invalid constrained integer.
  assert.equal(decoder.decode("NR", "X", Uint8Array.of(0x80)), null); // Truncated extension bitmap.
  assert.equal(decoder.decode("NR", "unknown", Uint8Array.of(0)), null);
  assert.equal(decoder.decode("NR", "X", new Uint8Array()), null);
});
test("API keeps proxy prefix and Bearer headers; logout rejects stale responses", async () => {
  const original = globalThis.fetch;
  let seen, release;
  try {
    globalThis.fetch = async (url, options) => {
      seen = { url: String(url), options };
      return new Response('{"id":9007199254740993}', { status: 200 });
    };
    const api = new Client("http://127.0.0.1/proxy/device/ui/");
    api.token = "unit-test-token";
    const data = await api.request("/state");
    assert.equal(data.id, "9007199254740993");
    assert.equal(seen.url, "http://127.0.0.1/proxy/device/state");
    assert.equal(
      seen.options.headers.get("Authorization"),
      "Bearer unit-test-token",
    );
    assert.equal(seen.options.credentials, "omit");
    await api.request("/efs/list?path=%2Fnvefs%2Fa..b");
    assert.ok(!seen.url.includes("unit-test-token"));
    await assert.rejects(api.request("//evil.test/x"));
    globalThis.fetch = () => new Promise((resolve) => (release = resolve));
    const pending = api.request("/state");
    api.reset();
    release(new Response("{}"));
    await assert.rejects(pending, /旧会话/);
  } finally {
    globalThis.fetch = original;
  }
});
test("uncertain EFS writes are never replayed and protected paths never reach backend", async () => {
  let calls = 0;
  const api = {
      request: async (path) => {
        calls++;
        if (path === "/efs/status")
          return {
            available: true,
            safety: {
              version: 1,
              root_readonly: true,
              recursive_delete: false,
              no_symlink_mutations: true,
            },
            limits: { write_bytes: 32768 },
          };
        throw new Error("connection lost after request");
      },
    },
    client = new EfsClient(api);
  await assert.rejects(client.mutate("delete", "/nvefs", {}), /只读/);
  assert.equal(calls, 0);
  await assert.rejects(
    client.mutate("write", "/nvefs/file", { hex: "01" }, 1),
    /connection lost/,
  );
  assert.equal(calls, 2);
  await assert.rejects(
    client.mutate("write", "/nvefs/file", { hex: "01" }, 1),
    /两分钟/,
  );
  assert.equal(calls, 2);
});
