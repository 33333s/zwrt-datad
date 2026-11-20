import { FrameReader, decodePacket, ndjsonRecord } from "./signal.js";
import { RrcDecoder } from "./rrc.js";
import {
  createCards,
  applyCards,
  registerCardsNetwork,
  reconnectCards,
} from "./signal-cards.js";
import { measurementNeighbors } from "./rrc-context.js";
import { emptyQos } from "./qos.js";
let reader,
  decoder,
  count = 0,
  decoded = 0,
  rrcCount = 0,
  nasCount = 0,
  cards = createCards(),
  lastMode = null,
  lastSim = null;
const publicCards = () => {
  const { nrUplinkPower, lteMeasurements, ...visible } = cards;
  return visible;
};
function network(state) {
  const mode = state?.net?.net_select,
    sim = state?.sim?.iccid;
  if (
    (lastMode && mode && lastMode !== mode) ||
    (lastSim && sim && lastSim !== sim)
  )
    cards = {
      ...cards,
      bearerQos: emptyQos(cards.bearerQos.activeFamily),
      qosApn: null,
      qci: null,
      ambr: null,
      ambrName: null,
    };
  if (mode) lastMode = mode;
  if (sim) lastSim = sim;
  cards = registerCardsNetwork(cards, state?.net?.type ?? "");
}
self.onmessage = async ({ data }) => {
  const reply = (value, transfer = []) =>
    self.postMessage({ id: data.id, ...value }, transfer);
  try {
    if (data.kind === "reset") {
      const url = new URL(data.schemaUrl);
      if (url.origin !== self.location.origin)
        throw new Error("RRC 字典必须来自同源服务");
      const response = await fetch(url, {
        cache: "no-store",
        credentials: "omit",
        redirect: "error",
      });
      if (!response.ok) throw new Error("RRC 字典加载失败");
      decoder = new RrcDecoder(await response.json());
      reader = new FrameReader(data.wire);
      count = 0;
      decoded = 0;
      rrcCount = 0;
      nasCount = 0;
      cards = createCards(data.cached ?? {});
      cards = reconnectCards(cards);
      network(data.state);
      reply({ kind: "ready", cards: publicCards() });
      return;
    }
    if (!decoder || !reader) throw new Error("解码器尚未初始化");
    if (data.kind === "chunk") {
      if (data.state) network(data.state);
      const events = [],
        frames = [];
      reader.push(new Uint8Array(data.buffer), (code, packet) => {
        const event = decodePacket(code, packet, decoder);
        count++;
        if (event.tree || event.fields.length || event.nas) decoded++;
        if (event.tree) rrcCount++;
        if (event.nas) nasCount++;
        cards = applyCards(cards, code, event);
        if (code === 0xb0c0 && cards.radio === "LTE" && event.tree)
          event.neighbors = measurementNeighbors(
            cards.lteMeasurements,
            event.tree,
          );
        if (event.neighbors && cards.cell)
          event.neighbors = event.neighbors.filter(
            (n) =>
              n.pci !== cards.cell.pci ||
              (n.arfcn != null && n.arfcn !== cards.cell.arfcn),
          );
        events.push(event);
        frames.push(packet.slice().buffer);
      });
      reply(
        {
          kind: "events",
          events,
          frames,
          count,
          decoded,
          rrcCount,
          nasCount,
          cards: publicCards(),
        },
        frames,
      );
    } else if (data.kind === "network") {
      network(data.state);
      reply({ kind: "state", cards: publicCards() });
    } else if (data.kind === "reconnect") {
      reader = new FrameReader(data.wire);
      cards = reconnectCards(cards);
      network(data.state);
      reply({ kind: "ready", cards: publicCards() });
    } else if (data.kind === "clear") {
      count = 0;
      decoded = 0;
      rrcCount = 0;
      nasCount = 0;
      cards = createCards({
        bearerQos: emptyQos(cards.bearerQos.activeFamily),
      });
      reply({ kind: "cleared", cards: publicCards() });
    } else if (data.kind === "finish") {
      reader.finish();
      reply({ kind: "finished", count, decoded, cards: publicCards() });
    } else if (data.kind === "export") {
      const rows = data.frames.map((frame) => {
        const packet = new Uint8Array(frame.buffer),
          v = new DataView(packet.buffer, packet.byteOffset, packet.byteLength);
        return JSON.stringify(
          ndjsonRecord(
            decodePacket(
              v.getUint16(2, true),
              packet,
              decoder,
              frame.observedAt ?? 0,
            ),
            frame.observedAt,
          ),
        );
      });
      reply({ kind: "exported", rows });
    } else throw new Error("未知 Worker 请求");
  } catch (error) {
    reply({ kind: "error", message: error.message });
  }
};
