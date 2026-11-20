import {
  bandList,
  cellLockPayload,
  lockedBands,
  modeOf,
  MODES,
} from "./datad-models.js";

export async function readNetworkBack(
  api,
  predicate,
  { attempts = 12, delay = 1000 } = {},
) {
  let state = null,
    lastError = null;
  for (let i = 0; i < attempts; i++) {
    try {
      state = await api.request("/state");
      lastError = null;
      if (predicate(state)) return { state, verified: true };
    } catch (e) {
      if (e.name === "AbortError" || e.status === 401 || e.status === 403)
        throw e;
      lastError = e;
    }
    if (i + 1 < attempts) await api.sleep(delay);
  }
  if (!state && lastError) throw lastError;
  return { state, verified: false };
}
export async function setMode(api, mode) {
  if (!MODES.some((m) => m.value === mode))
    throw new Error("请选择支持的网络模式");
  await api.control("network.set_mode", { mode });
  await api.sleep(1500);
  return readNetworkBack(api, (s) => modeOf(s.net?.net_select) === mode);
}
// One connect attempt even if the disconnect reply is lost. Never replay either write.
export async function redial(api, pause = 600) {
  let failure;
  try {
    await api.control("cellular.disconnect");
    await api.sleep(pause);
  } catch (e) {
    failure = e;
  } finally {
    api.check();
    try {
      await api.control("cellular.connect");
    } catch (e) {
      failure ??= e;
    }
  }
  if (failure) throw failure;
}
export async function setBands(api, key, selected, supported) {
  if (!["lte", "nr_sa", "nr_nsa"].includes(key))
    throw new Error("未知频段类型");
  const caps = bandList(supported.join(",")),
    bands = bandList(selected.join(","));
  if (!caps.length || bands.some((b) => !caps.includes(b)))
    throw new Error("所选频段不在设备支持列表");
  const automatic = !bands.length || bands.length === caps.length;
  await api.control(`band.set_${key}`, {
    bands: automatic ? "" : bands.join(","),
  });
  await redial(api);
  return readNetworkBack(api, (s) => {
    const actual = lockedBands(s.net ?? {}, key);
    return automatic
      ? actual.length === 0 || actual.join(",") === caps.join(",")
      : actual.join(",") === bands.join(",");
  });
}
export async function unlockBands(api) {
  for (const key of ["lte", "nr_sa", "nr_nsa"])
    await api.control(`band.set_${key}`, { bands: "" });
  await redial(api);
  return readNetworkBack(api, (s) =>
    ["lte", "nr_sa", "nr_nsa"].every((k) => {
      const current = lockedBands(s.net ?? {}, k),
        cap = bandList((s.net?.band_capabilities?.[k] ?? []).join(","));
      return (
        current.length === 0 ||
        (cap.length && current.join(",") === cap.join(","))
      );
    }),
  );
}
export async function lockCell(api, form) {
  const params = cellLockPayload(form),
    nr = form.rat === "NR",
    current = await api.request("/state");
  await api.control(nr ? "cell.lock_nr" : "cell.lock_lte", params);
  let modeError = "";
  const mode = current.net?.net_select;
  if (mode) {
    const target = nr ? "Only_5G" : mode === "Only_5G" ? "WL_AND_5G" : mode;
    try {
      await api.control("network.set_mode", { mode: target });
    } catch (e) {
      if (e.name === "AbortError") throw e;
      modeError = "小区锁已保存，但网络模式匹配未确认：" + e.message;
    }
  }
  const result = await readNetworkBack(
    api,
    (s) => {
      const lock = s.net?.[nr ? "nr_cell_lock" : "lte_cell_lock"];
      return (
        lock &&
        Number(lock.pci) === params.pci &&
        Number(lock[nr ? "arfcn" : "earfcn"]) ===
          params[nr ? "arfcn" : "earfcn"]
      );
    },
    { attempts: 3 },
  );
  return { ...result, modeError };
}
export async function unlockCells(api) {
  const previous = await api.request("/state");
  await api.control("cell.unlock_all");
  if (previous.net?.net_select)
    await api.control("network.set_mode", { mode: previous.net.net_select });
  return readNetworkBack(
    api,
    (s) => !s.net?.nr_cell_lock && !s.net?.lte_cell_lock,
    { attempts: 3 },
  );
}
export async function scanNeighbors(api) {
  const state = await api.request("/state");
  if (
    String(state.net?.type).toUpperCase() !== "SA" &&
    state.net?.net_select !== "Only_5G"
  )
    throw new Error("当前模式的邻区自动刷新，无需 SA 扫描");
  return api.control("neighbor.scan_sa", {}, { timeout: 210000 });
}
