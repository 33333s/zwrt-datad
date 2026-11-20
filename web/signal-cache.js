import { isDataApn } from "./nas.js";
import { dataSession } from "./qos.js";
const family = (type) =>
  String(type).toUpperCase() === "SA"
    ? "NR"
    : ["LTE", "LTE_CA", "LTE_A", "ENDC", "LTE-NSA", "NSA"].includes(
          String(type).toUpperCase(),
        )
      ? "LTE"
      : null;
// Mirrors SignalCardStore's small cross-page summary, never raw packets or credentials.
export class SignalSummaryStore {
  constructor(base) {
    this.key = "datad.signal-summary.v1:" + base;
    this.listeners = new Set();
    this.current = null;
    this.owner = null;
    this.lastMode = null;
    this.lastFamily = null;
    this.lastSim = null;
    this.bound = false;
  }
  subscribe = (listener) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };
  getSnapshot = () => this.current;
  emit() {
    for (const listener of this.listeners) listener();
  }
  identity(state, previous = null) {
    let old = [];
    try {
      old = JSON.parse(previous) || [];
    } catch {}
    const imei = String(
        state?.system?.imei || state?.modem?.imei || old[1] || "",
      ),
      iccid = String(state?.sim?.iccid || old[2] || "");
    return imei || iccid
      ? JSON.stringify([state?.device?.model_name || old[0] || "", imei, iccid])
      : null;
  }
  attach(state) {
    this.current = null;
    this.owner = this.identity(state);
    this.lastMode = state.net?.net_select ?? null;
    this.lastSim = state.sim?.iccid ?? null;
    this.lastFamily = family(state.net?.type);
    this.bound = true;
    try {
      const saved = JSON.parse(localStorage.getItem(this.key));
      if (
        this.owner &&
        saved?.owner === this.owner &&
        saved.summary?.updatedAt > 0 &&
        ["LTE", "NR"].includes(saved.summary.radio)
      ) {
        this.current = this.sanitize(saved.summary);
        if (this.current.qosFamily !== this.lastFamily)
          this.current = {
            ...this.current,
            qci: null,
            ambr: null,
            qosApn: null,
          };
      }
    } catch {}
    this.emit();
  }
  sanitize(summary) {
    return {
      ...summary,
      ...(!summary.qosApn || !isDataApn(summary.qosApn)
        ? { qci: null, ambr: null, qosApn: null }
        : {}),
    };
  }
  observe(state) {
    if (!this.bound) return;
    const owner = this.identity(state, this.owner),
      mode = state.net?.net_select,
      sim = state.sim?.iccid,
      nextFamily = family(state.net?.type),
      differentOwner = this.owner && owner && this.owner !== owner;
    if (
      differentOwner ||
      (this.lastMode && mode && this.lastMode !== mode) ||
      (this.lastSim && sim && this.lastSim !== sim) ||
      (nextFamily &&
        nextFamily !== this.lastFamily &&
        this.current?.qosFamily !== nextFamily)
    )
      this.clear();
    if (owner) this.owner = owner;
    if (mode) this.lastMode = mode;
    if (sim) this.lastSim = sim;
    if (nextFamily) this.lastFamily = nextFamily;
  }
  publish(cards) {
    if (!this.bound) return;
    const session = dataSession(cards.bearerQos);
    this.current = this.sanitize({
      qci: session?.qci ?? null,
      qiLabel: session?.qiLabel ?? "QCI",
      ambr: session?.ambr ?? null,
      qosApn: session?.apn ?? null,
      qosFamily: cards.bearerQos.selected?.split(":")[0] ?? null,
      plmn: cards.plmn,
      cell: cards.cell,
      security: cards.security,
      vendor: cards.vendor,
      vendorDetail: cards.vendorDetail,
      radio: cards.radio,
      updatedAt: cards.updatedAt,
    });
    try {
      if (this.owner)
        localStorage.setItem(
          this.key,
          JSON.stringify({ owner: this.owner, summary: this.current }),
        );
    } catch {}
    this.emit();
  }
  clear() {
    this.current = null;
    try {
      localStorage.removeItem(this.key);
    } catch {}
    this.emit();
  }
  detach() {
    this.bound = false;
    this.current = null;
    this.owner = null;
    this.emit();
  }
  captureSeed() {
    const c = this.current;
    if (!c) return {};
    return {
      cell: c.cell,
      plmn: c.plmn,
      vendor: c.vendor,
      vendorDetail: c.vendorDetail,
      security: c.security,
      updatedAt: c.updatedAt,
      radio: c.radio,
    };
  }
}
