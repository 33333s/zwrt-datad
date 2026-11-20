import test from "node:test";
import assert from "node:assert/strict";
import { createLoginPreferences } from "../web/login-preferences.js";
const memory = () => {
  const values = new Map();
  return {
    getItem: (k) => values.get(k) ?? null,
    setItem: (k, v) => values.set(k, v),
    removeItem: (k) => values.delete(k),
    values,
  };
};
test("saved login is opt-in, scoped to origin and proxy mount, and contains no token", () => {
  const local = memory(),
    session = memory(),
    a = createLoginPreferences("https://device.example/proxy/a/", {
      local,
      session,
    });
  assert.equal(a.read(), null);
  assert.equal(a.canAutoLogin(), false);
  assert.equal(a.save("admin", "fixture-password", true), true);
  assert.deepEqual(a.read(), {
    user: "admin",
    password: "fixture-password",
    auto: true,
  });
  assert.equal(a.canAutoLogin(), true);
  for (const url of [
    "https://device.example/proxy/b/",
    "http://device.example/proxy/a/",
    "https://another.example/proxy/a/",
  ])
    assert.equal(createLoginPreferences(url, { local, session }).read(), null);
  assert.deepEqual(
    Object.keys(JSON.parse([...local.values.values()][0])).sort(),
    ["auto", "password", "user", "version"],
  );
  a.clear();
  assert.equal(a.read(), null);
  assert.equal(a.canAutoLogin(), false);
});
test("explicit logout suppresses automatic re-entry across managers until a successful login", () => {
  const local = memory(),
    session = memory(),
    url = "https://logout.example/";
  const a = createLoginPreferences(url, { local, session });
  a.save("admin", "fixture", true);
  a.suppressAuto();
  const b = createLoginPreferences(url, { local, session });
  assert.equal(b.canAutoLogin(), false);
  assert.equal(b.read().auto, true);
  b.allowAuto();
  assert.equal(a.canAutoLogin(), true);
  b.setAutomatic(false);
  assert.equal(a.canAutoLogin(), false);
  assert.equal(a.read().password, "fixture");
});
test("corrupt or invalid saved credentials never trigger auto login", () => {
  const local = memory(),
    session = memory(),
    a = createLoginPreferences("https://invalid.example/", { local, session });
  a.save("admin", "fixture", true);
  const key = [...local.values.keys()][0];
  for (const value of [
    "not json",
    "null",
    JSON.stringify({ version: 1, user: "admin", password: "", auto: true }),
    JSON.stringify({
      version: 1,
      user: "admin",
      password: "fixture",
      auto: "yes",
    }),
  ]) {
    local.setItem(key, value);
    assert.equal(a.read(), null);
    assert.equal(a.canAutoLogin(), false);
  }
  assert.equal(a.save("admin", "x".repeat(513), true), false);
});
test("denied browser storage does not block manual login or crash cleanup", () => {
  const denied = {
    getItem() {
      throw Error("denied");
    },
    setItem() {
      throw Error("denied");
    },
    removeItem() {
      throw Error("denied");
    },
  };
  const a = createLoginPreferences("http://private.example/", {
    local: denied,
    session: denied,
  });
  assert.equal(a.read(), null);
  assert.equal(a.save("admin", "fixture", true), false);
  assert.equal(a.clear(), false);
  a.suppressAuto();
  assert.equal(a.canAutoLogin(), false);
  a.allowAuto();
  assert.equal(a.canAutoLogin(), false);
});
