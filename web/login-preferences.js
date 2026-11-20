// Saved login belongs to one origin and API mount path. Service/session tokens stay in memory.
const blocked = new Set();
export function createLoginPreferences(base, stores = {}) {
  const url = new URL(base);
  const key = "datad.web.login.v1:" + url.origin + url.pathname;
  const pauseKey = key + ":paused";
  const persistent = () => stores.local ?? globalThis.localStorage;
  const session = () => stores.session ?? globalThis.sessionStorage;
  function read() {
    try {
      const p = JSON.parse(persistent().getItem(key));
      if (
        p?.version !== 1 ||
        typeof p.user !== "string" ||
        !p.user ||
        p.user.length > 128 ||
        typeof p.password !== "string" ||
        !p.password ||
        p.password.length > 512 ||
        typeof p.auto !== "boolean"
      )
        return null;
      return { user: p.user, password: p.password, auto: p.auto };
    } catch {
      return null;
    }
  }
  function clear() {
    try {
      persistent().removeItem(key);
      return true;
    } catch {
      return false;
    }
  }
  function save(user, password, auto) {
    if (
      typeof user !== "string" ||
      !user ||
      user.length > 128 ||
      typeof password !== "string" ||
      !password ||
      password.length > 512
    )
      return false;
    try {
      persistent().setItem(
        key,
        JSON.stringify({ version: 1, user, password, auto: !!auto }),
      );
      return true;
    } catch {
      return false;
    }
  }
  function setAutomatic(auto) {
    const p = read();
    return p ? save(p.user, p.password, auto) : false;
  }
  function suppressAuto() {
    blocked.add(key);
    try {
      session().setItem(pauseKey, "1");
    } catch {}
  }
  function allowAuto() {
    blocked.delete(key);
    try {
      session().removeItem(pauseKey);
    } catch {}
  }
  function canAutoLogin() {
    if (blocked.has(key)) return false;
    try {
      if (session().getItem(pauseKey) === "1") return false;
    } catch {}
    return !!read()?.auto;
  }
  return {
    read,
    save,
    clear,
    setAutomatic,
    suppressAuto,
    allowAuto,
    canAutoLogin,
  };
}
