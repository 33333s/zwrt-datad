import test from "node:test";
import assert from "node:assert/strict";
import { decodeNas, decodeEps, sessionAmbr, qosFlows } from "../web/nas.js";
import {
  emptyQos,
  updateQos,
  registeredNetwork,
  dataSession,
} from "../web/qos.js";
import {
  emptyMeasurements,
  updateMeasurements,
  measurementNeighbors,
  updateLinkConfig,
  highSpeedRail,
} from "../web/rrc-context.js";
import {
  timingAdvanceDistance,
  emptyNrPower,
  correlateNrPower,
} from "../web/radio-context.js";
import { SignalSummaryStore } from "../web/signal-cache.js";
import { decodePacket } from "../web/signal.js";
import * as ml1 from "../web/ml1.js";
import * as lte from "../web/lte.js";
import { readBoundedBinary } from "../web/core.js";
import { managementUrl } from "../web/device-profiles.js";
import { carrierReplay } from "../web/carriers.js";

test("browser device addresses and carrier imports reject credentials, invalid frames and oversized input", () => {
  assert.equal(
    managementUrl("http://127.0.0.1:9461/proxy/device"),
    "http://127.0.0.1:9461/proxy/device/ui/",
  );
  assert.throws(() => managementUrl("http://admin:secret@example.com/"));
  assert.throws(() => managementUrl("javascript:alert(1)"));
  assert.throws(() => carrierReplay("[{}]"));
  assert.throws(() => carrierReplay(" ".repeat(2000001)));
  assert.equal(
    carrierReplay('{"net":{"type":"LTE","lte_pci":10,"lte_channel":1650}}')
      .length,
    1,
  );
});

test("EFS downloads enforce streamed bounds and declared length before allocating the complete file", async () => {
  let cancelled = false;
  const source = new ReadableStream({
    start(c) {
      c.enqueue(new Uint8Array(4));
      c.enqueue(new Uint8Array(4));
    },
    cancel() {
      cancelled = true;
    },
  });
  await assert.rejects(readBoundedBinary(new Response(source), 6), /限制/);
  assert.ok(cancelled);
  await assert.rejects(
    readBoundedBinary(
      new Response("abc", { headers: { "Content-Length": "4" } }),
      8,
    ),
    /长度/,
  );
  const progress = [];
  assert.deepEqual(
    await readBoundedBinary(
      new Response("abc", { headers: { "Content-Length": "3" } }),
      8,
      (done, total) => progress.push([done, total]),
    ),
    new TextEncoder().encode("abc"),
  );
  assert.deepEqual(progress, [[3, 3]]);
});

const bytes = (...v) => Uint8Array.from(v.flat());
const apn = (name) => [name.length, ...new TextEncoder().encode(name)];
const eps = (id, name, qci = 9) =>
  bytes(
    (id << 4) | 2,
    1,
    0xc1,
    1,
    qci,
    apn(name).length,
    apn(name),
    5,
    1,
    10,
    0,
    0,
    1,
    0x5e,
    2,
    0xfe,
    0xfe,
  );
const nr = (id, name) =>
  bytes(
    0x2e,
    id,
    1,
    0xc2,
    0x11,
    0,
    0,
    6,
    6,
    3,
    0xe8,
    6,
    0,
    0xc8,
    0x25,
    apn(name).length,
    apn(name),
    0x79,
    0,
    6,
    1,
    0x20,
    1,
    1,
    1,
    9,
  );

test("plain EPS and 5GS NAS decode real wire fields, not a guessed APN or borrowed AMBR", () => {
  const e = decodeEps(eps(5, "internet")).nas;
  assert.equal(e.dnn, "internet");
  assert.equal(e.qci, 9);
  assert.equal(e.ueIp, "10.0.0.1");
  assert.equal(e.ambr.dl, "8640 kbps");
  const n = decodeNas(nr(1, "internet"));
  assert.equal(n.dnn, "internet");
  assert.equal(n.pduSessionId, 1);
  assert.deepEqual(n.ambr, { dl: "1Gbps", ul: "200Mbps" });
  assert.equal(n.qosFlows[0].qi, 9);
  assert.equal(decodeNas(bytes(0x7e, 2, 0x42)), null); // encrypted MM is opaque
  assert.equal(decodeNas(nr(1, "internet").subarray(0, 13)), null);
  assert.equal(sessionAmbr(bytes(0, 0, 0, 6, 0, 1)), null);
  assert.equal(qosFlows(bytes(1, 0x20, 1, 1, 2, 9)), null);
});

test("QoS selects the Internet default bearer and isolates mode switches, releases and QFI edits", () => {
  let q = registeredNetwork(emptyQos(), "LTE");
  q = updateQos(q, decodeEps(eps(5, "ims", 5)).nas);
  assert.equal(dataSession(q), null);
  q = updateQos(q, decodeEps(eps(6, "internet")).nas);
  assert.equal(dataSession(q).qci, "9");
  q = registeredNetwork(q, "SA");
  assert.equal(dataSession(q), null);
  q = updateQos(q, {
    name: "PDU_SESSION_MODIFICATION_COMMAND",
    type: "5GSM",
    pduSessionId: 1,
    ambr: { dl: "1Gbps", ul: "200Mbps" },
  });
  assert.equal(dataSession(q), null);
  q = updateQos(q, decodeNas(nr(1, "internet")));
  assert.equal(dataSession(q).qiLabel, "5QI");
  assert.equal(dataSession(q).qci, "9");
  q = updateQos(q, {
    name: "PDU_SESSION_MODIFICATION_COMMAND",
    type: "5GSM",
    pduSessionId: 1,
    qosFlows: [{ qfi: 1, operation: 3, qi: null }],
  });
  assert.equal(dataSession(q).qci, "9");
  q = updateQos(q, {
    name: "PDU_SESSION_MODIFICATION_COMMAND",
    type: "5GSM",
    pduSessionId: 1,
    qosFlows: [{ qfi: 1, operation: 2, qi: null }],
  });
  assert.equal(dataSession(q).qci, null);
  q = updateQos(q, {
    name: "PDU_SESSION_RELEASE_COMMAND",
    type: "5GSM",
    pduSessionId: 1,
  });
  assert.equal(dataSession(q), null);
  q = registeredNetwork(q, "NSA");
  assert.equal(q.activeFamily, "LTE");
  assert.equal(dataSession(q), null);
});

test("LTE measurement report only receives a frequency from its current measurement identity", () => {
  const tree = (config) => ({
    message: {
      c1: {
        rrcConnectionReconfiguration: {
          criticalExtensions: {
            c1: { "rrcConnectionReconfiguration-r8": { measConfig: config } },
          },
        },
      },
    },
  });
  let context = updateMeasurements(
    emptyMeasurements(),
    tree({
      measObjectToAddModList: [
        {
          measObjectId: 2,
          measObject: { measObjectEUTRA: { carrierFreq: 1650 } },
        },
      ],
      measIdToAddModList: [{ measId: 4, measObjectId: 2, reportConfigId: 1 }],
    }),
  );
  const results = {
    measId: 4,
    measResultListEUTRA: [
      { physCellId: 10, measResult: { rsrpResult: 50, rsrqResult: 20 } },
    ],
  };
  const report = {
    message: {
      c1: {
        measurementReport: {
          criticalExtensions: {
            c1: { "measurementReport-r8": { measResults: results } },
          },
        },
      },
    },
  };
  assert.equal(measurementNeighbors(context, report)[0].arfcn, 1650);
  context = updateMeasurements(context, tree({ measObjectToRemoveList: [2] }));
  assert.equal(measurementNeighbors(context, report)[0].arfcn, null);
  assert.deepEqual(
    updateMeasurements(context, {
      message: { c1: { rrcConnectionRelease: {} } },
    }),
    emptyMeasurements(),
  );
});

test("CSI-RS release and cell-group transitions remove obsolete ports", () => {
  const tree = (group, csi) => ({
    CellGroupConfig: {
      cellGroupId: group,
      spCellConfig: { spCellConfigDedicated: { "csi-MeasConfig": csi } },
    },
  });
  let state = updateLinkConfig(
    { ports: {}, cellGroupId: null },
    tree(1, {
      setup: {
        "nzp-CSI-RS-ResourceToAddModList": [
          { "nzp-CSI-RS-ResourceId": 2, resourceMapping: { nrofPorts: "p8" } },
        ],
      },
    }),
  );
  assert.deepEqual(state.ports, { 2: 8 });
  state = updateLinkConfig(
    state,
    tree(1, { setup: { "nzp-CSI-RS-ResourceToReleaseList": [2] } }),
  );
  assert.deepEqual(state.ports, {});
  assert.deepEqual(
    updateLinkConfig(
      { ports: { 2: 8 }, cellGroupId: 1 },
      tree(2, { release: null }),
    ).ports,
    {},
  );
});

test("TA offsets and NR numerology are applied in the correct units", () => {
  const fdd = timingAdvanceDistance(16, "LTE", "FDD", "N_TA");
  assert.ok(Math.abs(fdd.meters - 78.0709526) < 0.001);
  assert.equal(
    timingAdvanceDistance(640, "LTE", "TDD", "TOTAL_ADVANCE").meters,
    fdd.meters,
  );
  assert.equal(
    timingAdvanceDistance(16, "LTE", "TDD", "N_TA").meters,
    fdd.meters,
  );
  assert.equal(
    timingAdvanceDistance(39936 + 512, "NR", "TDD", "TOTAL_ADVANCE", 1, 39936)
      .meters,
    fdd.meters / 2,
  );
  assert.equal(
    timingAdvanceDistance(512, "NR", "TDD", "TOTAL_ADVANCE", 1, null),
    null,
  );
  assert.equal(timingAdvanceDistance(1283, "LTE"), null);
});

test("NR RF power requires unambiguous channel, slot, time and clipped target agreement", () => {
  const mac = {
      time: { sfn: 1, slot: 2, numerology: 1 },
      channel: "PUSCH",
      requestedDbm: 20,
      limitDbm: 23,
    },
    rf = { sfn: 1, slot: 2, power: { dbm: 20, limitDbm: 23, chain: 1 } };
  const m = correlateNrPower(emptyNrPower(), "control", "1000000", [mac]).state;
  assert.equal(
    correlateNrPower(m, "rf", "1000001", [rf]).readings[0].channel,
    "PUSCH",
  );
  assert.equal(correlateNrPower(m, "rf", "3000001", [rf]).readings.length, 0);
  const ambiguous = correlateNrPower(m, "control", "1000000", [
    { ...mac, channel: "PUCCH" },
  ]).state;
  assert.equal(
    correlateNrPower(ambiguous, "rf", "1000001", [rf]).readings.length,
    0,
  );
  assert.equal(
    correlateNrPower(m, "rf", "1000001", [{ ...rf, power: { dbm: 21 } }])
      .readings.length,
    0,
  );
});

test("summary cache retains partial-state identity, clears on confirmed SIM or mode change, and never seeds bearer QoS", () => {
  const original = globalThis.localStorage,
    values = new Map();
  globalThis.localStorage = {
    getItem: (k) => values.get(k),
    setItem: (k, v) => values.set(k, v),
    removeItem: (k) => values.delete(k),
  };
  try {
    const store = new SignalSummaryStore("/fixture"),
      state = {
        device: { model_name: "MU5120" },
        system: { imei: "fixture" },
        sim: { iccid: "sim-a" },
        net: { type: "SA", net_select: "Only_5G" },
      };
    store.attach(state);
    const q = updateQos(
      registeredNetwork(emptyQos(), "SA"),
      decodeNas(nr(1, "internet")),
    );
    store.publish({ bearerQos: q, radio: "NR", updatedAt: 123 });
    assert.equal(store.current.qci, "9");
    store.observe({ sim: { iccid: "sim-a" }, net: { type: "SA" } });
    assert.equal(store.current.qci, "9");
    assert.equal(store.captureSeed().bearerQos, undefined);
    store.observe({ sim: { iccid: "sim-b" } });
    assert.equal(store.current, null);
    store.detach();
    assert.equal(store.bound, false);
  } finally {
    globalThis.localStorage = original;
  }
});

test("unsupported/truncated diagnostic layouts remain null and malformed packets never throw", () => {
  let seed = 123456789;
  const rand = () => {
    seed ^= seed << 13;
    seed ^= seed >>> 17;
    seed ^= seed << 5;
    return seed >>> 0;
  };
  const decoders = Object.values(ml1).filter((v) => typeof v === "function");
  for (let i = 0; i < 600; i++) {
    const body = Uint8Array.from({ length: rand() % 256 }, () => rand() & 255);
    for (const fn of decoders) assert.doesNotThrow(() => fn(body));
    for (const fn of [
      lte.lteServing,
      lte.lteMib,
      lte.ltePdsch,
      decodeNas,
      decodeEps,
    ])
      assert.doesNotThrow(() => fn(body));
    assert.doesNotThrow(() => decodePacket(0xb821, body));
  }
});
