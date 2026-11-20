import React, {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  Accordion,
  ActionIcon,
  Alert,
  Badge,
  Box,
  Button,
  Center,
  FileButton,
  Group,
  Menu,
  Progress,
  Select,
  Stack,
  Table,
  Text,
  TextInput,
  Tooltip,
  UnstyledButton,
} from "@mantine/core";
import {
  Activity,
  ArrowDownToLine,
  ChevronDown,
  FileJson,
  FolderOpen,
  Pause,
  Play,
  Search,
  Square,
  Trash2,
} from "lucide-react";
import { download, formatBytes } from "../core.js";
import { MODES, modeOf } from "../datad-models.js";
import { redial, setMode } from "../network-operations.js";
import { CaptureBuffer } from "../capture-buffer.js";
import { createCards } from "../signal-cards.js";
import { SignalCards, DetailRows, BeamTable } from "./signal-card-ui.jsx";
import { NdjsonView, RrcTree } from "./signal-views.jsx";
import { useOperation, usePageApi } from "./hooks.jsx";
import { Panel, useServices } from "./ui.jsx";

function decoderSession() {
  const worker = new Worker(new URL("../signal-worker.js", import.meta.url), {
      type: "module",
    }),
    pending = new Map();
  let id = 0,
    closed = false;
  worker.onmessage = ({ data }) => {
    const p = pending.get(data.id);
    if (!p) return;
    pending.delete(data.id);
    data.kind === "error" ? p.reject(new Error(data.message)) : p.resolve(data);
  };
  worker.onerror = () => {
    for (const p of pending.values())
      p.reject(new Error("解码 Worker 运行失败"));
    pending.clear();
  };
  return {
    request(data, transfer = []) {
      if (closed)
        return Promise.reject(new DOMException("解码任务已结束", "AbortError"));
      return new Promise((resolve, reject) => {
        const key = ++id;
        pending.set(key, { resolve, reject });
        worker.postMessage({ id: key, ...data }, transfer);
      });
    },
    close() {
      closed = true;
      worker.terminate();
      for (const p of pending.values())
        p.reject(new DOMException("解码任务已结束", "AbortError"));
      pending.clear();
    },
  };
}
const schemaUrl = () =>
  new URL("rrc-schema.json", new URL("./", document.baseURI)).href;
const showEvent = (event, level, query) => {
  if (level === "imp" && !event.nas && !event.security && !event.p0)
    return false;
  if (level === "nas" && !event.nas) return false;
  if (level === "rrc" && !event.tree) return false;
  if (level === "meas" && !event.neighbors?.length && !event.servingSnr)
    return false;
  return (
    !query ||
    `${event.code} ${event.name} ${JSON.stringify(event.fields)} ${JSON.stringify(event.nas ?? {})} ${JSON.stringify(event.tree ?? {})}`
      .toLowerCase()
      .includes(query)
  );
};

export function SignalPage() {
  const {
      api,
      ask,
      notify,
      signalCache,
      observeSnapshot,
      connection,
      dialogVisible,
    } = useServices(),
    pageApi = usePageApi(),
    networkOp = useOperation();
  const [revision, setRevision] = useState(0),
    [filter, setFilter] = useState(""),
    [level, setLevel] = useState("all"),
    [paused, setPaused] = useState(false),
    [progress, setProgress] = useState(null),
    [exporting, setExporting] = useState(false),
    [exportPhase, setExportPhase] = useState("");
  const runtime = useRef(null);
  if (!runtime.current) {
    const r = {
      alive: true,
      job: 0,
      events: [],
      pausedEvents: [],
      raw: new CaptureBuffer(),
      eventBytes: 0,
      count: 0,
      decoded: 0,
      rrcCount: 0,
      nasCount: 0,
      rate: 0,
      rateAt: performance.now(),
      rateCount: 0,
      cards: createCards(signalCache?.captureSeed()),
      state: connection.snapshot,
      status: "尚未抓取",
      running: false,
      replay: false,
      decoder: null,
      exportDecoder: null,
      reader: null,
      abort: null,
      timer: null,
      listeners: new Set(),
      detailListeners: new Set(),
      dialogVisible: false,
      frozen: null,
    };
    r.snapshot = {
      cards: r.cards,
      state: r.replay ? null : r.state,
      status: r.status,
      running: r.running,
      replay: r.replay,
    };
    r.detailSnapshot = r.snapshot;
    r.detailStore = {
      subscribe: (fn) => {
        r.detailListeners.add(fn);
        return () => r.detailListeners.delete(fn);
      },
      getSnapshot: () => r.frozen ?? r.detailSnapshot,
    };
    r.store = {
      subscribe: (fn) => {
        r.listeners.add(fn);
        return () => r.listeners.delete(fn);
      },
      getSnapshot: () => r.frozen ?? r.snapshot,
    };
    runtime.current = r;
  }
  const r = runtime.current;
  function update(immediate = false) {
    if (!r.alive) return;
    const commit = () => {
      r.timer = null;
      const next = {
        cards: r.cards,
        state: r.replay ? null : r.state,
        status: r.status,
        running: r.running,
        replay: r.replay,
      };
      r.detailSnapshot = next;
      if (!r.replay) signalCache?.publish(r.cards);
      if (r.dialogVisible) {
        for (const fn of r.detailListeners) fn();
      } else {
        r.snapshot = next;
        for (const fn of r.listeners) fn();
        setRevision((n) => n + 1);
      }
    };
    if (immediate) {
      clearTimeout(r.timer);
      commit();
    } else if (!r.timer)
      r.timer = setTimeout(commit, r.dialogVisible ? 500 : 250);
  }
  useLayoutEffect(() => {
    r.dialogVisible = dialogVisible;
    update(true);
  }, [dialogVisible]);
  function publish(reply) {
    if (reply.cards) {
      r.cards = reply.cards;
    }
    for (const k of ["count", "decoded", "rrcCount", "nasCount"])
      if (reply[k] !== undefined) r[k] = reply[k];
    if (performance.now() - r.rateAt >= 1000) {
      r.rate =
        ((r.count - r.rateCount) * 1000) / (performance.now() - r.rateAt);
      r.rateCount = r.count;
      r.rateAt = performance.now();
    }
    update();
  }
  function stop(status = "已停止", sendStop = true) {
    const wasLive = r.running && !r.replay;
    r.job++;
    r.abort?.abort();
    void r.reader?.cancel().catch(() => {});
    r.decoder?.close();
    r.decoder = null;
    r.reader = null;
    r.running = false;
    r.status = status;
    if (sendStop && wasLive && api.token)
      void api.control("signal.stream.stop").catch(() => {});
    if (r.alive) {
      setProgress(null);
      update(true);
    }
  }
  useEffect(() => {
    r.alive = true;
    return () => {
      if (!r.replay) signalCache?.publish(r.cards);
      r.alive = false;
      stop("已停止");
      r.exportDecoder?.close();
      r.exportDecoder = null;
      r.raw.clear();
      r.events = [];
      r.pausedEvents = [];
      clearTimeout(r.timer);
      r.listeners.clear();
      r.detailListeners.clear();
    };
  }, []);
  useEffect(() => {
    let active = true,
      timer;
    const controller = new AbortController();
    async function tick() {
      if (!document.hidden) {
        try {
          const state = await pageApi.request("/state", {
            signal: controller.signal,
          });
          if (!active) return;
          r.state = state;
          if (r.decoder && !r.replay) {
            const reply = await r.decoder.request({ kind: "network", state });
            if (active) publish(reply);
          } else update();
        } catch (e) {
          if (active && e.name !== "AbortError") update();
        }
      }
      if (active && r.running) timer = setTimeout(tick, 2000);
    }
    void tick();
    return () => {
      active = false;
      controller.abort();
      clearTimeout(timer);
    };
  }, [r.running]);
  function append(reply) {
    publish(reply);
    for (let i = 0; i < reply.events.length; i++) {
      const event = reply.events[i],
        observedAt = r.replay ? null : event.ts,
        entry = { event, observedAt, cost: JSON.stringify(event).length * 2 };
      r.raw.push(new Uint8Array(reply.frames[i]), observedAt);
      r.events.push(entry);
      r.eventBytes += entry.cost;
      while (r.events.length > 300 || r.eventBytes > 16 * 1024 * 1024)
        r.eventBytes -= r.events.shift().cost;
    }
  }
  async function capture(file) {
    if (r.running) return;
    stop("正在准备", false);
    r.events = [];
    r.pausedEvents = [];
    r.raw.clear();
    r.eventBytes = 0;
    r.count = 0;
    r.decoded = 0;
    r.rrcCount = 0;
    r.nasCount = 0;
    r.rate = 0;
    r.rateCount = 0;
    r.rateAt = performance.now();
    r.replay = !!file;
    r.running = true;
    r.status = file ? "正在导入…" : "正在连接…";
    setPaused(false);
    r.frozen = null;
    const job = ++r.job;
    r.abort = new AbortController();
    r.decoder = decoderSession();
    update(true);
    try {
      publish(
        await r.decoder.request({
          kind: "reset",
          wire: !file,
          schemaUrl: schemaUrl(),
          cached: file ? {} : signalCache?.captureSeed(),
          state: file ? null : r.state,
        }),
      );
      if (file) {
        r.reader = file.stream().getReader();
        let consumed = 0;
        setProgress(0);
        while (r.alive && job === r.job) {
          const { value, done } = await r.reader.read();
          if (done) break;
          consumed += value.length;
          for (let at = 0; at < value.length; at += 65536) {
            if (job !== r.job) return;
            const chunk = value.slice(at, at + 65536),
              reply = await r.decoder.request(
                {
                  kind: "chunk",
                  buffer: chunk.buffer,
                  state: r.replay ? null : r.state,
                },
                [chunk.buffer],
              );
            if (job !== r.job) return;
            append(reply);
          }
          setProgress((consumed / file.size) * 100);
        }
        if (job === r.job) {
          publish(await r.decoder.request({ kind: "finish" }));
          stop("导入完成", false);
        }
        return;
      }
      let backoff = 500;
      while (r.alive && job === r.job) {
        try {
          r.status = "正在连接…";
          update();
          const response = await api.request("/signal/stream", {
            stream: true,
            signal: r.abort.signal,
          });
          if (
            response.headers.get("x-signal-framing") !== "diag-le32-v1" ||
            !(response.headers.get("content-type") ?? "").startsWith(
              "application/octet-stream",
            )
          )
            throw new Error("信令协议不匹配，请更新 datad");
          if (job !== r.job) {
            await response.body.cancel();
            return;
          }
          publish(
            await r.decoder.request({
              kind: "reconnect",
              wire: true,
              state: r.state,
            }),
          );
          r.reader = response.body.getReader();
          r.status = "抓取中";
          backoff = 500;
          update(true);
          while (r.alive && job === r.job) {
            const { value, done } = await r.reader.read();
            if (done) break;
            for (let at = 0; at < value.length; at += 65536) {
              if (job !== r.job) return;
              const chunk = value.slice(at, at + 65536),
                reply = await r.decoder.request(
                  {
                    kind: "chunk",
                    buffer: chunk.buffer,
                    state: r.replay ? null : r.state,
                  },
                  [chunk.buffer],
                );
              if (job !== r.job) return;
              append(reply);
            }
          }
          if (job === r.job) await r.decoder.request({ kind: "finish" });
        } catch (e) {
          if (job !== r.job || !r.alive || e.name === "AbortError") return;
          if (
            [400, 401, 403, 404, 405, 426].includes(e.status) ||
            e.message.includes("协议不匹配")
          )
            throw e;
          r.status = "连接中断，正在重连";
          update(true);
        } finally {
          try {
            await r.reader?.cancel();
          } catch {}
          r.reader = null;
        }
        if (job !== r.job) return;
        r.status = "等待重连";
        update();
        await pageApi.sleep(backoff);
        backoff = Math.min(backoff * 2, 8000);
      }
    } catch (error) {
      if (job === r.job && r.alive) {
        stop(file ? "导入失败" : "抓取失败", false);
        if (error.name !== "AbortError") notify(error.message, true);
      }
    }
  }
  useEffect(() => {
    void capture();
  }, []);
  function togglePause() {
    if (!paused) r.pausedEvents = r.events.slice();
    setPaused(!paused);
  }
  async function inspect(entry) {
    if (!paused) {
      r.pausedEvents = r.events.slice();
      setPaused(true);
    }
    r.frozen = r.snapshot;
    update(true);
    const event = entry.event;
    try {
      await ask({
        title: event.name,
        overlayBlur: 0,
        subtitle: `${event.code} · ${event.len} B · 列表已暂停`,
        confirm: null,
        cancel: "关闭",
        wide: true,
        body: (
          <Stack gap="md">
            {event.decodeEvidence && (
              <Badge variant="light" size="xs">
                {event.evidence}
              </Badge>
            )}
            {event.decodeScope && (
              <Text size="xs" c="dimmed">
                {event.decodeScope}
              </Text>
            )}
            {event.fields.length ? (
              <DetailRows rows={event.fields} />
            ) : (
              <Alert color="gray">
                未通过此布局的解码校验，完整原始记录已保留。
              </Alert>
            )}
            {event.neighbors?.length > 0 && (
              <Accordion variant="contained">
                {event.neighbors.map((c) => (
                  <Accordion.Item
                    key={`${c.pci}:${c.arfcn}`}
                    value={`${c.pci}:${c.arfcn}`}
                  >
                    <Accordion.Control>
                      PCI {c.pci} · {c.arfcn ?? "频道未关联"}
                    </Accordion.Control>
                    <Accordion.Panel>
                      <DetailRows
                        rows={[
                          ["RSRP", c.rsrp],
                          ["RSRQ", c.rsrq],
                        ]}
                      />
                      <BeamTable beams={c.beams} />
                    </Accordion.Panel>
                  </Accordion.Item>
                ))}
              </Accordion>
            )}
            <Accordion
              variant="contained"
              multiple
              defaultValue={event.tree ? ["tree"] : []}
              radius="md"
            >
              {event.tree && (
                <Accordion.Item value="tree">
                  <Accordion.Control>RRC 协议树</Accordion.Control>
                  <Accordion.Panel>
                    <RrcTree value={event.tree} />
                  </Accordion.Panel>
                </Accordion.Item>
              )}
              <Accordion.Item value="ndjson">
                <Accordion.Control>完整 NDJSON 记录</Accordion.Control>
                <Accordion.Panel>
                  <NdjsonView event={event} observedAt={entry.observedAt} />
                </Accordion.Panel>
              </Accordion.Item>
            </Accordion>
          </Stack>
        ),
      });
    } finally {
      r.frozen = null;
      update(true);
    }
  }
  async function clear() {
    if (
      !(await ask({
        title: "清空信令记录？",
        body: (
          <Text size="sm">清空消息流、原始帧缓冲和卡片上下文，无法恢复。</Text>
        ),
        confirm: "清空",
        danger: true,
      }))
    )
      return;
    const reply = r.decoder ? await r.decoder.request({ kind: "clear" }) : null;
    r.events = [];
    r.pausedEvents = [];
    r.raw.clear();
    r.eventBytes = 0;
    r.count = 0;
    r.decoded = 0;
    r.rrcCount = 0;
    r.nasCount = 0;
    r.cards = reply?.cards ?? createCards();
    signalCache?.clear();
    update(true);
  }
  async function exportRecords(kind) {
    if (exporting || !r.raw.count) return;
    setExporting(true);
    setProgress(0);
    setExportPhase("正在准备");
    const raw = r.raw.snapshot(),
      events = r.events.slice(),
      parts = [];
    let session;
    try {
      if (kind === "ndjson") {
        session = decoderSession();
        r.exportDecoder = session;
        await session.request({
          kind: "reset",
          wire: false,
          schemaUrl: schemaUrl(),
        });
      }
      const total = kind === "json" ? events.length : raw.length;
      for (let i = 0; i < total; i += 20) {
        if (!r.alive) return;
        const end = Math.min(total, i + 20);
        if (kind === "ndjson") {
          const frames = raw.slice(i, end).map((f) => ({
              buffer: f.packet.slice().buffer,
              observedAt: f.observedAt,
            })),
            reply = await session.request(
              { kind: "export", frames },
              frames.map((f) => f.buffer),
            );
          parts.push(...reply.rows.map((line) => line + "\n"));
        } else if (kind === "bin") {
          for (let j = i; j < end; j++) {
            const header = new Uint8Array(4);
            new DataView(header.buffer).setUint32(
              0,
              raw[j].packet.byteLength,
              true,
            );
            parts.push(header, raw[j].packet);
          }
        } else
          for (let j = i; j < end; j++) {
            const { m, payload_hex, diag_timestamp, diag_valid, ...event } =
              events[j].event;
            if (j) parts.push(",");
            parts.push(JSON.stringify(event));
          }
        setProgress(total ? (end / total) * 99 : 99);
        setExportPhase(`正在生成 · ${end} / ${total} 条`);
        await new Promise((resolve) => setTimeout(resolve, 0));
      }
      if (!r.alive) return;
      setExportPhase("正在请求浏览器下载");
      if (kind === "json") {
        parts.unshift("[");
        parts.push("]");
      }
      download(
        new Blob(parts),
        `sig${kind === "bin" ? "cap" : kind === "json" ? "events" : "ndjson"}-${Date.now()}.${kind}`,
        kind === "bin"
          ? "application/octet-stream"
          : kind === "json"
            ? "application/json"
            : "application/x-ndjson",
      );
      setProgress(100);
      notify(`已生成 ${total} 条记录并请求浏览器下载`);
    } catch (error) {
      if (r.alive && error.name !== "AbortError") notify(error.message, true);
    } finally {
      session?.close();
      r.exportDecoder = null;
      if (r.alive) {
        setExporting(false);
        setProgress(null);
        setExportPhase("");
      }
    }
  }
  async function changeMode(mode) {
    if (
      !(await ask({
        title: "切换网络模式？",
        body: (
          <Text size="sm">
            切换至 {MODES.find((m) => m.value === mode)?.label}
            ，网络可能短暂中断，信令抓取继续。
          </Text>
        ),
      }))
    )
      return;
    await networkOp.run(
      async () => {
        const result = await pageApi.transaction("network", (tx) =>
          setMode(tx, mode),
        );
        if (result.state) {
          r.state = result.state;
          observeSnapshot?.(result.state);
        }
        update(true);
        return result;
      },
      {
        write: true,
        message: (r) =>
          r.verified ? "网络模式已回读确认" : "切换已提交，等待设备应用",
      },
    );
  }
  async function reconnect() {
    if (
      !(await ask({
        title: "重新拨号？",
        body: (
          <Text size="sm">网络会短暂中断，抓取继续，用于补齐承载信息。</Text>
        ),
        confirm: "重新拨号",
      }))
    )
      return;
    await networkOp.run(
      () => pageApi.transaction("network", (tx) => redial(tx, 5000)),
      { write: true, message: "已请求重新连接，继续捕获承载消息" },
    );
  }
  const snapshot = r.frozen ?? r.snapshot;
  const records = useMemo(() => {
    const source = paused ? r.pausedEvents : r.events,
      needle = filter.trim().toLowerCase();
    return source
      .filter(({ event }) => showEvent(event, level, needle))
      .slice()
      .reverse();
  }, [revision, paused, filter, level]);
  const recordTable = useMemo(
    () =>
      records.length ? (
        <Table.ScrollContainer minWidth={670}>
          <Table verticalSpacing="sm" highlightOnHover className="record-table">
            <Table.Thead>
              <Table.Tr>
                {[
                  "时间",
                  "类型",
                  "DIAG 代码",
                  "记录名称",
                  "长度",
                  "解析状态",
                ].map((label) => (
                  <Table.Th key={label}>{label}</Table.Th>
                ))}
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {records.map((entry, i) => {
                const event = entry.event;
                return (
                  <Table.Tr
                    key={`${event.ts}:${event.code}:${i}`}
                    onClick={() => void inspect(entry)}
                  >
                    <Table.Td>
                      <Text size="xs" c="dimmed" className="value">
                        {new Date(event.ts).toLocaleTimeString("zh-CN", {
                          hour12: false,
                        })}
                      </Text>
                    </Table.Td>
                    <Table.Td>
                      <Badge
                        size="xs"
                        variant="light"
                        color={
                          event.tree ? "violet" : event.nas ? "teal" : "blue"
                        }
                      >
                        {event.tree
                          ? "RRC"
                          : event.nas
                            ? "NAS"
                            : event.fields.length
                              ? "PHY"
                              : "DIAG"}
                      </Badge>
                    </Table.Td>
                    <Table.Td>
                      <span className="table-tag">{event.code}</span>
                    </Table.Td>
                    <Table.Td>
                      <UnstyledButton
                        onClick={(e) => {
                          e.stopPropagation();
                          void inspect(entry);
                        }}
                      >
                        <Text size="sm" className="value">
                          {event.name}
                        </Text>
                        {event.cell && (
                          <Text fz={10} c="dimmed" mt={3}>
                            PCI {event.cell.pci} · {event.cell.arfcn}
                          </Text>
                        )}
                      </UnstyledButton>
                    </Table.Td>
                    <Table.Td>
                      <Text size="xs" c="dimmed">
                        {event.len} B
                      </Text>
                    </Table.Td>
                    <Table.Td>
                      {event.evidence ? (
                        <Badge size="xs" variant="light" color="teal">
                          {event.evidence}
                        </Badge>
                      ) : (
                        <Text size="xs" c="dimmed">
                          原始记录
                        </Text>
                      )}
                    </Table.Td>
                  </Table.Tr>
                );
              })}
            </Table.Tbody>
          </Table>
        </Table.ScrollContainer>
      ) : (
        <Center py={50}>
          <Stack align="center" gap="sm">
            <Activity size={25} />
            <Text size="sm">等待信令记录</Text>
            <Text size="xs" c="dimmed">
              开始实时抓取，或导入 .bin 文件
            </Text>
          </Stack>
        </Center>
      ),
    [records],
  );
  return (
    <Stack gap={18}>
      <Panel
        title="实时采集"
        icon={Activity}
        collapsible
        defaultOpen={false}
        className="capture-panel"
        right={
          <Badge
            variant="light"
            color={snapshot.running ? "teal" : "gray"}
            size="sm"
          >
            {snapshot.status}
          </Badge>
        }
        summary={
          <Stack gap="sm">
            <Group justify="space-between" gap="sm">
              <Group gap={6}>
                <Button
                  size="xs"
                  px={9}
                  w={92}
                  variant={snapshot.running ? "default" : "filled"}
                  leftSection={
                    snapshot.running ? <Square size={14} /> : <Play size={14} />
                  }
                  onClick={() => (r.running ? stop() : void capture())}
                >
                  {snapshot.running ? "停止" : "开始抓取"}
                </Button>
                <Button
                  size="xs"
                  px={9}
                  variant={paused ? "light" : "default"}
                  leftSection={
                    paused ? <Play size={13} /> : <Pause size={13} />
                  }
                  onClick={togglePause}
                >
                  {paused ? "继续更新" : "暂停列表"}
                </Button>
                <Select
                  aria-label="信令页网络模式"
                  size="xs"
                  w={{ base: 112, sm: 145 }}
                  data={MODES.filter(
                    (m) =>
                      m.value !== "LTE_AND_5G" ||
                      r.state?.sim?.current_slot !== 2,
                  )}
                  value={modeOf(r.state?.net?.net_select)}
                  allowDeselect={false}
                  disabled={networkOp.busy || r.replay}
                  onChange={(v) => void changeMode(v)}
                />
              </Group>
              <Text size="xs" c="dimmed">
                包 {r.count.toLocaleString()} · RRC {r.rrcCount} · NAS{" "}
                {r.nasCount} · {snapshot.running ? r.rate.toFixed(0) : "0"}/s
              </Text>
            </Group>
            {progress !== null && (
              <Stack gap={5}>
                <Progress
                  value={progress}
                  size={5}
                  aria-label={exporting ? "导出进度" : "导入进度"}
                />
                <Text size="xs" c="dimmed">
                  {progress.toFixed(0)}% · {exportPhase || "正在导入"}
                </Text>
              </Stack>
            )}
            {r.replay && (
              <Alert color="gray" py={7}>
                抓包回放；下方卡片来自文件解析，列表时间为本机处理时间，DIAG
                原始时钟保留在完整记录中。
              </Alert>
            )}
          </Stack>
        }
      >
        <Stack gap="sm">
          <Group justify="space-between" gap="sm">
            <Group gap={8}>
              <FileButton
                accept=".bin"
                onChange={(file) => {
                  if (file) void capture(file);
                }}
              >
                {(props) => (
                  <Button
                    {...props}
                    size="xs"
                    variant="default"
                    leftSection={<FolderOpen size={14} />}
                    disabled={r.running}
                  >
                    导入抓包
                  </Button>
                )}
              </FileButton>
              <Menu position="bottom-end">
                <Menu.Target>
                  <Button
                    size="xs"
                    variant="default"
                    leftSection={<ArrowDownToLine size={14} />}
                    rightSection={<ChevronDown size={12} />}
                    loading={exporting}
                    disabled={!r.raw.count}
                  >
                    导出
                  </Button>
                </Menu.Target>
                <Menu.Dropdown>
                  <Menu.Item onClick={() => void exportRecords("bin")}>
                    原始 BIN · {r.raw.count} 帧
                  </Menu.Item>
                  <Menu.Item onClick={() => void exportRecords("json")}>
                    近期事件 JSON · {r.events.length} 条
                  </Menu.Item>
                  <Menu.Item onClick={() => void exportRecords("ndjson")}>
                    完整帧与解码 NDJSON · {r.raw.count} 帧
                  </Menu.Item>
                </Menu.Dropdown>
              </Menu>
            </Group>
          </Group>
          <Text size="xs" c="dimmed">
            已解析 {r.decoded} · 缓存 {formatBytes(r.raw.bytes)}
          </Text>
        </Stack>
      </Panel>
      <SignalCards
        store={r.store}
        detailStore={r.detailStore}
        onRedial={reconnect}
        networkBusy={networkOp.busy}
      />
      <Panel
        title="信令记录"
        icon={FileJson}
        right={
          <Group gap={8}>
            {paused && (
              <Badge color="orange" variant="light" size="sm">
                列表已暂停
              </Badge>
            )}
            <Tooltip label="清空记录">
              <ActionIcon
                variant="subtle"
                color="gray"
                aria-label="清空记录"
                onClick={() => void clear()}
              >
                <Trash2 size={15} />
              </ActionIcon>
            </Tooltip>
          </Group>
        }
      >
        <Stack gap="md">
          <Group justify="space-between">
            <TextInput
              placeholder="筛选名称 / 代码 / 字段内容"
              aria-label="筛选信令"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              leftSection={<Search size={14} />}
              w={{ base: "100%", sm: 340 }}
            />
            <Select
              aria-label="信令分类"
              size="xs"
              value={level}
              onChange={setLevel}
              allowDeselect={false}
              data={[
                { value: "all", label: "全部" },
                { value: "imp", label: "重要" },
                { value: "nas", label: "NAS" },
                { value: "rrc", label: "RRC" },
                { value: "meas", label: "测量" },
              ]}
              w={120}
            />
          </Group>
          {recordTable}
          <Text fz={10} c="dimmed">
            近期事件最多 300 条 / 16 MiB；原始帧独立保留最近 24
            MiB。详情暂停显示，解码与记录继续。
          </Text>
        </Stack>
      </Panel>
    </Stack>
  );
}
