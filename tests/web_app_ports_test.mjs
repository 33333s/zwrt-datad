import test from "node:test";
import assert from "node:assert/strict";
import {
  apnPayload,
  apnRows,
  manualApn,
  smsPayload,
  smsRows,
  smsSenderOptions,
  serving,
  networkScore,
  neighbors,
} from "../web/datad-models.js";
import { carrierSnapshot } from "../web/carriers.js";
import { lanPayload, usageRows, limitBytes } from "../web/settings-models.js";
import {
  taskPayload,
  TASK_ACTIONS,
  recoveryPayload,
} from "../web/automation-models.js";
import {
  forwardPatch,
  nmsEnrollment,
  nmsPayload,
} from "../web/management-models.js";
import {
  lockCell,
  redial,
  setBands,
  setMode,
  unlockBands,
  unlockCells,
  scanNeighbors,
} from "../web/network-operations.js";

test("datad SMS uses num/text, exact IDs, explicit direction and sender capabilities", () => {
  const rows = smsRows({
    sms: {
      list: [
        {
          id: "9007199254740993",
          num: "10086",
          text: "不是 Base64 的原始正文",
          tag: 1,
          date: "26,09,07,12,00,00,+0800",
        },
        ...[0, 2, 3, 4, 99, null].map((tag, id) => ({ id: String(id), tag })),
      ],
    },
  });
  const first = rows.find((r) => r.id === "9007199254740993");
  assert.equal(first.number, "10086");
  assert.equal(first.content, "不是 Base64 的原始正文");
  assert.equal(first.date, "2026-09-07 12:00:00");
  assert.equal(first.mailbox, "inbox");
  assert.equal(rows.find((r) => r.id === "2").failed, true);
  assert.equal(rows.find((r) => r.id === "3").mailbox, "draft");
  assert.equal(rows.find((r) => r.id === "4").mailbox, "unknown");
  assert.equal(rows.find((r) => r.id === "5").mailbox, "unknown");
  const caps = { controls: ["sim.set_slot"] };
  assert.equal(smsSenderOptions({ sim: { dual_sim: 1 } }, caps).length, 1);
  assert.equal(smsSenderOptions({ sim: { dual_sim: 2 } }, caps).length, 3);
  assert.throws(() =>
    smsPayload("10086", "test", "sim2", { sim: { dual_sim: 1 } }, caps),
  );
  const p = smsPayload(
    "10086",
    " 中😀\n",
    "sim2",
    { sim: { dual_sim: 2 } },
    caps,
    new Date(2026, 8, 7, 12, 0, 0),
  );
  assert.equal(p.message_hex, "00204E2DD83DDE00000A");
  assert.equal(p.sender, "sim2");
  assert.equal(p.sms_time, "26;09;07;12;00;00;+;0");
  assert.equal(p.ID, "-1");
});
test("APN credential omission and fresh manual profile identity match App contract", () => {
  const form = {
      name: "",
      apn: "internet",
      pdp_type: "3",
      auth_mode: "-1",
      username: "",
      password: "",
    },
    edit = apnPayload(form, "manual-2"),
    create = apnPayload(form);
  assert.equal(edit.profile_id, "manual-2");
  assert.equal(edit.name, "internet");
  assert.ok(!("password" in edit));
  assert.ok(!("auth_mode" in edit));
  assert.equal(create.password, "");
  assert.equal(create.auth_mode, 0);
  assert.equal(create.pdp_type, 3);
  const view = {
    mode: { apn_mode: 0 },
    automatic: { apnListArray: [{ profileId: "automatic-1" }] },
    manual: { apnListArray: [{ profileId: "manual-2" }] },
  };
  assert.equal(apnRows(view).length, 2);
  assert.equal(manualApn(view, "manual-2"), "manual-2");
  assert.throws(() => manualApn(view, "2"));
  assert.throws(() => manualApn({ ...view, writable: false }, "manual-2"));
});
test("carrier port preserves inactive SCCs, exact U50/MU5250 layouts and 32-carrier lists", () => {
  const nr = {
    type: "SA",
    nr_band: "n78",
    nr_pci: 415,
    nr_channel: 633984,
    nr_bw: "100MHz",
    nrca: "0,801,2,n1,427210,20MHz,1,-69.9,-10.4,32.0,-59.6;",
    nr_ca_dl_state: "2",
    nr_ca_ul_state: "2",
  };
  let result = carrierSnapshot(nr);
  assert.equal(result.summary, "2CC 配置 · 已激活");
  assert.equal(result.carriers[1].band, "n1");
  assert.equal(result.carriers[1].rsrp, -69.9);
  assert.equal(result.carriers[1].bandwidth, "20 MHz");
  result = carrierSnapshot({ ...nr, nr_ca_dl_state: "1", nr_ca_ul_state: "1" });
  assert.equal(result.carriers.length, 2);
  assert.equal(result.summary, "2CC 配置 · 未聚合");
  const numeric =
    "0,485,2,78,633984,100,1,-86.1,-10.8,13,-75.3;1,414,1,78,627264,100,0,-140,-43,-23,-120;";
  result = carrierSnapshot(
    {
      ...nr,
      nr_pci: 801,
      nr_channel: 427210,
      nrca: numeric,
      nr_ca_dl_state: "",
      nr_ca_ul_state: "",
    },
    "MU5250",
  );
  assert.equal(result.carriers.length, 3);
  assert.equal(result.summary, "3CC 配置 · 已激活");
  assert.equal(result.carriers[2].activity, "inactive");
  assert.equal(
    carrierSnapshot({ ...nr, nrca: numeric }, "unknown").unrecognized,
    true,
  );
  const rows = Array.from(
    { length: 31 },
    (_, i) => `${i},${500 + i},1,n78,${640001 + i},100MHz,0,NaN,,-999;`,
  ).join("");
  result = carrierSnapshot({ ...nr, nrca: rows });
  assert.equal(result.carriers.length, 32);
  assert.equal(result.carriers.at(-1).role, "SCC30");
  assert.equal(result.carriers[1].rsrp, null);
  for (const bad of [
    nr.nrca + nr.nrca,
    nr.nrca + "broken;",
    nr.nrca.replace("20MHz", "20kHz"),
  ])
    assert.equal(carrierSnapshot({ ...nr, nrca: bad }).unrecognized, true);
});
test("LTE separate descriptor and signal tables join by slot, not by filtered position", () => {
  const lte = {
    type: "LTE",
    band: "B3",
    lte_pci: 223,
    lte_channel: 1650,
    lteca: "0,224,2,1,300,20;;2,225,1,8,3740,10;",
    ltecasig: "-98,-12,8,-65,1;;-100,-14,0,-70,0;",
    lte_ca_state: "ca_deactivated",
  };
  const r = carrierSnapshot(lte);
  assert.equal(r.carriers.length, 3);
  assert.equal(r.carriers[2].role, "SCC2");
  assert.equal(r.carriers[2].rsrp, -100);
  assert.equal(r.carriers[2].sinr, 0);
  assert.equal(r.summary, "3CC 配置 · 未聚合");
  assert.equal(
    carrierSnapshot({ ...lte, lteca: "ca_deconfigured" }).carriers.length,
    1,
  );
  assert.equal(carrierSnapshot({ ...lte, type: "SA" }).carriers.length, 0);
});
test("serving and measurements do not mix SA residue with LTE/NSA anchors", () => {
  const state = {
    net: {
      type: "SA",
      nr_pci: 1,
      nr_channel: 633984,
      nr_rsrp: 0,
      lte_pci: 2,
      lte_channel: 1650,
    },
  };
  assert.equal(serving(state).rsrp, null);
  assert.equal(serving(state).anchor, null);
  assert.equal(
    serving({ ...state, net: { ...state.net, type: "NSA" } }).anchor.pci,
    2,
  );
  assert.equal(networkScore({}), null);
  assert.equal(networkScore({ rsrp: -80 }), 10);
  assert.equal(
    neighbors({
      ...state,
      net: { ...state.net, type: "LTE" },
      neighbor: { cells: [{ rat: "NR5G", pci: 1, arfcn: 633984 }] },
    }).length,
    0,
  );
});
test("LAN subnet, readonly gateway, independent MTU and history missing days remain strict", () => {
  const current = {
      supported: true,
      address_writable: false,
      ip: "192.168.0.1",
      netmask: "255.255.255.0",
    },
    form = {
      ...current,
      dhcp_enabled: true,
      dhcp_start: "192.168.0.2",
      dhcp_end: "192.168.0.200",
      hours: "24",
    };
  assert.equal(lanPayload(current, form).lease_seconds, 86400);
  for (const patch of [
    { dhcp_start: "192.168.0.0" },
    { dhcp_end: "192.168.0.255" },
    { dhcp_start: "192.168.1.2" },
    { dhcp_start: "192.168.0.1" },
    { netmask: "255.0.255.0" },
    { hours: "721" },
    { ip: "192.168.1.1" },
  ])
    assert.throws(() => lanPayload(current, { ...form, ...patch }));
  assert.ok(
    !("lease_seconds" in lanPayload(current, { ...form, dhcp_enabled: false })),
  );
  const rows = usageRows(
    {
      days: [
        { date: "2026-10-01", bytes: 100 },
        { date: "2026-10-03", bytes: 200 },
        { date: "bad", bytes: 1 },
        { date: "2026-10-04", bytes: -1 },
      ],
    },
    "2026-10-01",
    "2026-10-04",
  );
  assert.deepEqual(
    rows.map((d) => d.date),
    ["2026-10-01", "2026-10-03"],
  );
  assert.equal(limitBytes("100_1024"), 100 * 1073741824);
});
test("scheduler only submits supported actions and exact typed params", () => {
  const view = {
      actions: Object.keys(TASK_ACTIONS),
      network_modes: ["Only_LTE"],
    },
    base = { id: "Task", time: "08:30", repeat_daily: false };
  assert.equal(
    taskPayload(
      { ...base, action: "sim.set_slot", params: { slot: "2" } },
      view,
    ).params.slot,
    2,
  );
  assert.equal(
    taskPayload(
      { ...base, action: "nfc.set", params: { enabled: "true" } },
      view,
    ).params.enabled,
    true,
  );
  assert.equal(
    taskPayload(
      {
        ...base,
        action: "cell.lock_nr",
        params: { pci: "1007", arfcn: "633984", band: "78" },
      },
      view,
    ).params.pci,
    "1007",
  );
  assert.equal(
    taskPayload(
      {
        ...base,
        action: "sms.send_scheduled",
        params: { number: "10086", text: " 一行\n二行 " },
      },
      view,
    ).params.text,
    " 一行\n二行 ",
  );
  for (const bad of [
    { time: "24:00", action: "device.reboot" },
    { action: "shell" },
    { action: "network.set_mode", params: { mode: "AUTO" } },
    { action: "sim.set_slot", params: { slot: "3" } },
  ])
    assert.throws(() => taskPayload({ ...base, ...bad }, view));
  assert.throws(() => recoveryPayload({ interval_seconds: "1" }));
});
test("forward changes preserve secrets and do not reset unrelated power baseline", () => {
  const c = {
    enabled: true,
    method: "smtp",
    nickname: "",
    webhook_url: "",
    dingtalk_webhook: "",
    smtp: {
      host: "smtp.example.com",
      port: 465,
      username: "a@example.com",
      to: "b@example.com",
      password_configured: true,
    },
    sms_to_phone: [],
    blacklist_phone: [],
    blacklist_keywords: [],
    power_forward_enabled: true,
  };
  const patch = forwardPatch({ ...c, nickname: "Pocket" }, c);
  assert.deepEqual(Object.keys(patch).sort(), [
    "enabled",
    "method",
    "nickname",
  ]);
  const changed = forwardPatch(
    { ...c, smtp: { ...c.smtp, to: "c@example.com" } },
    c,
  );
  assert.ok(!("password" in changed.smtp));
  assert.ok(!("password_configured" in changed.smtp));
  assert.throws(() =>
    forwardPatch({ ...c, webhook_url: "http://example.com" }, c),
  );
  assert.throws(() => forwardPatch(c, c, { clearSmtp: true }));
});
test("NMS enrollment sanitizes errors and never submits terminal permissions", () => {
  const raw = {
      username: "enr_" + "a".repeat(32),
      password: " secret_+/= 中文 ",
      remote_webshell_enabled: true,
    },
    code = Buffer.from(JSON.stringify(raw)).toString("base64url");
  assert.deepEqual(nmsEnrollment(code), {
    username: raw.username,
    password: raw.password,
  });
  for (const code of [
    "bad!",
    Buffer.from('{"password":"private-secret",broken}').toString("base64"),
  ]) {
    try {
      nmsEnrollment(code);
      assert.fail();
    } catch (e) {
      assert.ok(!e.message.includes("private-secret"));
      assert.equal(e.cause, undefined);
    }
  }
  const config = {
    enabled: false,
    broker: "wss://example.com/mqtt",
    platform_url: "https://example.com",
    remote_origins: [],
    username: "",
    ca_pem: "",
    vendor: "",
    model: "",
    identity: "",
    identity_type: "uuid",
    platform: "generic",
    report_interval_seconds: 30,
    remote_enabled: false,
    remote_panel_control_enabled: false,
    services: [
      { name: "Web", port: 80, kind: "web" },
      { name: "Keep terminal", port: 8899, kind: "terminal" },
    ],
    remote_webshell_enabled: true,
  };
  const payload = nmsPayload(config);
  assert.ok(!("services" in payload));
  assert.ok(!("remote_webshell_enabled" in payload));
  assert.equal(payload.web_services.length, 1);
  assert.equal(payload.password, "");
  assert.equal(payload.clear_password, false);
  assert.throws(() =>
    nmsPayload({
      ...config,
      services: [
        ...config.services,
        { name: "Duplicate", port: 8899, kind: "web" },
      ],
    }),
  );
});
function fake(states) {
  let i = 0;
  const calls = [];
  return {
    calls,
    check() {},
    async request(path) {
      calls.push(["GET", path]);
      return structuredClone(states[Math.min(i++, states.length - 1)]);
    },
    async control(action, params) {
      calls.push([action, params]);
      return {};
    },
    async sleep(ms) {
      calls.push(["sleep", ms]);
    },
  };
}
test("network writes follow App actions, reconnect once and verify real readback", async () => {
  const a = fake([{ net: { net_select: "Only_LTE" } }]);
  assert.equal((await setMode(a, "4G")).verified, true);
  assert.deepEqual(a.calls[0], ["network.set_mode", { mode: "4G" }]);
  const b = fake([{ net: { lte_band: "1,3" } }]);
  assert.equal((await setBands(b, "lte", [1, 3], [1, 3, 8])).verified, true);
  assert.deepEqual(b.calls.slice(0, 4), [
    ["band.set_lte", { bands: "1,3" }],
    ["cellular.disconnect", undefined],
    ["sleep", 600],
    ["cellular.connect", undefined],
  ]);
  const no = fake([{ net: { lte_band: "8" } }]);
  assert.equal((await setBands(no, "lte", [1], [1, 3, 8])).verified, false);
  assert.equal(no.calls.filter((c) => c[0] === "band.set_lte").length, 1);
});
test("cell locks use global unlock only and NR mode matching, scans delegate restoration", async () => {
  const a = fake([
    { net: { net_select: "Only_LTE" } },
    { net: { nr_cell_lock: { pci: 123, arfcn: 633984 } } },
  ]);
  const result = await lockCell(a, {
    rat: "NR",
    pci: "123",
    arfcn: "633984",
    band: "78",
  });
  assert.equal(result.verified, true);
  assert.deepEqual(
    a.calls.find((c) => c[0] === "cell.lock_nr"),
    ["cell.lock_nr", { pci: 123, arfcn: 633984, band: 78 }],
  );
  assert.deepEqual(
    a.calls.find((c) => c[0] === "network.set_mode"),
    ["network.set_mode", { mode: "Only_5G" }],
  );
  const b = fake([{ net: { net_select: "Only_LTE" } }, { net: {} }]);
  assert.equal((await unlockCells(b)).verified, true);
  assert.equal(
    b.calls
      .filter((c) => c[0].startsWith("cell.unlock"))
      .map((c) => c[0])
      .join(","),
    "cell.unlock_all",
  );
  const scan = fake([{ net: { type: "SA", net_select: "AUTO" } }]);
  await scanNeighbors(scan);
  assert.equal(scan.calls.at(-1)[0], "neighbor.scan_sa");
  assert.equal(scan.calls.filter((c) => c[0] === "network.set_mode").length, 0);
});
test("a lost disconnect reply still attempts a single connect, never retries the disconnect", async () => {
  const a = fake([]);
  a.control = async (action, params) => {
    a.calls.push([action, params]);
    if (action === "cellular.disconnect") throw new Error("lost reply");
    return {};
  };
  await assert.rejects(redial(a), /lost reply/);
  assert.deepEqual(
    a.calls.map((c) => c[0]),
    ["cellular.disconnect", "cellular.connect"],
  );
});
