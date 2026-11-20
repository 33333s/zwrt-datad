import React, { useEffect, useRef, useState } from "react";
import {
  Alert,
  Badge,
  Box,
  Button,
  Checkbox,
  Group,
  NumberInput,
  Select,
  SimpleGrid,
  Stack,
  Table,
  Tabs,
  Text,
  TextInput,
} from "@mantine/core";
import { Antenna, LockKeyhole, Radio, TowerControl } from "lucide-react";
import {
  MODES,
  localizedOperator,
  bandList,
  cellLockPayload,
  lockedBands,
  modeOf,
  neighbors,
  networkScore,
  number,
  serving,
} from "../datad-models.js";
import {
  lockCell,
  readNetworkBack,
  redial,
  scanNeighbors,
  setBands,
  setMode,
  unlockBands,
  unlockCells,
} from "../network-operations.js";
import { display } from "../core.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { InfoGrid, Panel, useServices } from "./ui.jsx";
import { CarrierPanel } from "./carrier-panel.jsx";
import { QosSummary } from "./qos-summary.jsx";

const resultMessage = (r) =>
  r.verified ? "已保存并回读确认" : "设置已提交，回读尚未一致，请稍后刷新";
export function ModePanel({ resource, api, operation: op }) {
  const { ask } = useServices(),
    [target, setTarget] = useState(""),
    observed = modeOf(resource.data?.net?.net_select);
  useEffect(() => {
    setTarget(observed ?? "");
  }, [observed]);
  const modes = MODES.filter(
    (m) => m.value !== "LTE_AND_5G" || resource.data?.sim?.current_slot !== 2,
  );
  async function apply() {
    if (
      !(await ask({
        title: "切换网络模式？",
        body: (
          <Text size="sm">
            切换至 {MODES.find((m) => m.value === target)?.label}
            ，网络可能短暂中断。
          </Text>
        ),
      }))
    )
      return;
    await op.run(
      async () => {
        const r = await api.transaction("network", (tx) => setMode(tx, target));
        if (r.state) resource.setData(r.state);
        return r;
      },
      { write: true, message: resultMessage },
    );
  }
  return (
    <Panel title="蜂窝网络模式" icon={Radio}>
      <Stack>
        <Group gap="sm">
          <Badge variant="light">
            {MODES.find((m) => m.value === observed)?.label ??
              resource.data?.net?.net_select ??
              "状态未上报"}
          </Badge>
          <Text size="xs" c="dimmed">
            {resource.data?.net?.type || "驻网未上报"} ·{" "}
            {resource.data?.net?.wan_status || "连接状态未上报"}
          </Text>
        </Group>
        <Group align="end">
          <Select
            label="网络模式"
            data={modes}
            value={target || null}
            onChange={setTarget}
            allowDeselect={false}
            disabled={op.busy}
            style={{ flex: 1, minWidth: 160 }}
          />
          <Button
            loading={op.busy}
            disabled={!target || target === observed}
            onClick={apply}
          >
            应用
          </Button>
        </Group>
      </Stack>
    </Panel>
  );
}
export function BandPage() {
  const api = usePageApi(),
    { ask, navigate } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/state")),
    net = resource.data?.net ?? {};
  const [key, setKey] = useState("lte"),
    [selected, setSelected] = useState({ lte: [], nr_sa: [], nr_nsa: [] }),
    [edited, setEdited] = useState(false);
  useDirtyGuard(edited);
  useEffect(() => {
    if (resource.data && !edited)
      setSelected(
        Object.fromEntries(
          ["lte", "nr_sa", "nr_nsa"].map((k) => [k, lockedBands(net, k)]),
        ),
      );
  }, [resource.data, edited]);
  const supported = net.band_capabilities?.[key] ?? [],
    current = serving(resource.data),
    actual = lockedBands(net, key),
    selection = selected[key] ?? [],
    pending = selection.join(",") !== actual.join(",");
  async function apply(reset = false) {
    if (
      !(await ask({
        title: reset ? "恢复全部频段？" : "应用频段选择？",
        body: (
          <Text size="sm">
            {reset
              ? "LTE、NR SA、NR NSA 都恢复自动选择。"
              : `${key === "lte" ? "LTE" : key === "nr_sa" ? "NR SA" : "NR NSA"}：${selection.join(", ") || "自动选择"}`}{" "}
            将重新拨号并回读，无需重启设备。
          </Text>
        ),
      }))
    )
      return;
    await op.run(
      async () => {
        const r = await api.transaction("network", (tx) =>
          reset ? unlockBands(tx) : setBands(tx, key, selection, supported),
        );
        setEdited(false);
        if (r.state) resource.setData(r.state);
        return r;
      },
      { write: true, message: resultMessage },
    );
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        <ModePanel resource={resource} api={api} operation={op} />
        <Panel
          title="频段锁定"
          icon={Antenna}
          right={
            <RefreshButton resource={resource} disabled={op.busy || edited} />
          }
        >
          <Stack>
            <Group justify="space-between">
              <Text size="xs" c="dimmed">
                当前驻网 {current?.band || "未上报"} ·{" "}
                {net.band_capabilities?.complete
                  ? "设备完整能力表"
                  : "设备返回的能力表可能不完整"}
              </Text>
              <Button
                variant="light"
                size="xs"
                onClick={() => navigate("cell-lock")}
              >
                锁定小区
              </Button>
            </Group>
            <Tabs value={key} onChange={setKey}>
              <Tabs.List>
                {[
                  ["lte", "LTE"],
                  ["nr_sa", "NR SA"],
                  ["nr_nsa", "NR NSA"],
                ].map(([k, l]) => (
                  <Tabs.Tab value={k} key={k}>
                    {l}
                  </Tabs.Tab>
                ))}
              </Tabs.List>
            </Tabs>
            <Text size="xs" c="dimmed">
              设备配置：{actual.join(", ") || "自动选择"}
              {pending ? " · 有未应用选择" : ""}
            </Text>
            {supported.length ? (
              <SimpleGrid cols={{ base: 3, sm: 5, lg: 8 }}>
                {supported.map((b) => (
                  <Checkbox.Card
                    key={b}
                    checked={selection.includes(b)}
                    disabled={op.busy}
                    onClick={() => {
                      if (op.busy) return;
                      setSelected((s) => ({
                        ...s,
                        [key]: s[key].includes(b)
                          ? s[key].filter((x) => x !== b)
                          : [...s[key], b].sort((a, b) => a - b),
                      }));
                      setEdited(true);
                    }}
                    p="sm"
                    radius="md"
                  >
                    <Group gap={6}>
                      <Checkbox.Indicator />
                      <Text size="sm">
                        {key === "lte" ? "B" : "n"}
                        {b}
                      </Text>
                    </Group>
                    {number(
                      String(current?.band ?? "").replace(/^[Bn]/i, ""),
                    ) === b &&
                      (key === "lte"
                        ? !current?.nsa && current?.rat === "LTE"
                        : current?.rat === "NR") && (
                        <Badge mt={6} size="xs">
                          当前驻网
                        </Badge>
                      )}
                  </Checkbox.Card>
                ))}
              </SimpleGrid>
            ) : (
              <Alert color="orange">
                设备未返回此类支持频段，请刷新确认；不会用固定频段表代替设备能力。
              </Alert>
            )}
            <Group justify="space-between">
              <Group>
                <Button
                  size="xs"
                  variant="default"
                  disabled={op.busy || !edited}
                  onClick={() => {
                    setEdited(false);
                    op.clear();
                  }}
                >
                  取消选择
                </Button>
                <Button
                  size="xs"
                  variant="light"
                  disabled={op.busy || !supported.length}
                  onClick={() => {
                    setSelected((s) => ({
                      ...s,
                      [key]: supported.slice().sort((a, b) => a - b),
                    }));
                    setEdited(true);
                  }}
                >
                  全选
                </Button>
              </Group>
              <Group>
                <Button
                  variant="default"
                  disabled={op.busy}
                  onClick={() => apply(true)}
                >
                  解除全部频段锁
                </Button>
                <Button
                  loading={op.busy}
                  disabled={!pending || !supported.length}
                  onClick={() => apply()}
                >
                  应用频段
                </Button>
              </Group>
            </Group>
          </Stack>
        </Panel>
      </LoadState>
    </Stack>
  );
}
export function NeighborTable({ rows, net, onLock }) {
  return (
    <Table.ScrollContainer minWidth={620}>
      <Table striped highlightOnHover fz="xs">
        <Table.Thead>
          <Table.Tr>
            {[
              "制式",
              "频段",
              "PCI",
              "频道",
              "RSRP · dBm",
              "SINR · dB",
              "操作",
            ].map((k) => (
              <Table.Th key={k}>{k}</Table.Th>
            ))}
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {rows.map((c, i) => {
            const lock =
                net?.[c.rat === "NR" ? "nr_cell_lock" : "lte_cell_lock"],
              locked =
                lock &&
                Number(lock.pci) === Number(c.pci) &&
                Number(lock[c.rat === "NR" ? "arfcn" : "earfcn"]) ===
                  Number(c.arfcn);
            return (
              <Table.Tr key={`${c.rat}:${c.arfcn}:${c.pci}:${i}`}>
                <Table.Td>{c.rat}</Table.Td>
                <Table.Td>
                  {c.band ? `${c.rat === "NR" ? "n" : "B"}${c.band}` : "—"}
                </Table.Td>
                <Table.Td>{display(c.pci)}</Table.Td>
                <Table.Td>{display(c.arfcn)}</Table.Td>
                <Table.Td>{display(c.rsrp)}</Table.Td>
                <Table.Td>{display(c.sinr)}</Table.Td>
                <Table.Td>
                  {locked ? (
                    <Badge size="xs">已配置锁定</Badge>
                  ) : (
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      disabled={c.pci == null || c.arfcn == null}
                      onClick={() => onLock(c)}
                    >
                      带入锁定
                    </Button>
                  )}
                </Table.Td>
              </Table.Tr>
            );
          })}
        </Table.Tbody>
      </Table>
      {!rows.length && (
        <Text size="sm" c="dimmed" ta="center" py="lg">
          等待邻区上报
        </Text>
      )}
    </Table.ScrollContainer>
  );
}
export function CellLockPage() {
  const api = usePageApi(),
    { ask, networkTarget, setNetworkTarget } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/state"));
  const [form, setForm] = useState({ rat: "NR", pci: "", arfcn: "", band: "" }),
    [paused, setPaused] = useState(false),
    [scan, setScan] = useState(null),
    [dirty, setDirty] = useState(false);
  useDirtyGuard(dirty);
  useEffect(() => {
    if (networkTarget) {
      setForm(networkTarget);
      setPaused(true);
      setDirty(true);
      setNetworkTarget(null);
    }
  }, [networkTarget]);
  useEffect(() => {
    if (paused || op.busy) return;
    const timer = setInterval(() => void resource.refresh(), 5000);
    return () => clearInterval(timer);
  }, [paused, op.busy]);
  function fill(c) {
    setForm({
      rat: c.rat,
      pci: String(c.pci ?? ""),
      arfcn: String(c.arfcn ?? ""),
      band: String(c.band ?? "").replace(/^[Bn]/i, ""),
    });
    setDirty(true);
    op.clear();
    setPaused(true);
  }
  async function apply(unlock = false) {
    let params;
    if (!unlock)
      try {
        params = cellLockPayload(form);
      } catch (e) {
        await op.run(() => {
          throw e;
        });
        return;
      }
    if (
      !(await ask({
        title: unlock ? "解除全部小区锁定？" : "保存小区锁定？",
        body: (
          <Text size="sm">
            {unlock
              ? "解除 LTE 和 NR 的全部小区锁定。"
              : `${form.rat} PCI ${params.pci} · 频道 ${params.arfcn ?? params.earfcn}`}{" "}
            U50 系列通常重启设备后生效。
          </Text>
        ),
      }))
    )
      return;
    const result = await op.run(
      async () => {
        const r = await api.transaction("network", (tx) =>
          unlock ? unlockCells(tx) : lockCell(tx, form),
        );
        if (r.state) resource.setData(r.state);
        setDirty(false);
        return r;
      },
      {
        write: true,
        message: (r) =>
          (r.modeError ? r.modeError + "。" : "") +
          resultMessage(r) +
          "；是否已驻留目标小区需重启后确认",
      },
    );
    if (
      result &&
      (await ask({
        title: "重启使小区锁定生效？",
        body: (
          <Text size="sm">
            配置已提交。现在重启会断开连接，也可以稍后手动重启。
          </Text>
        ),
        confirm: "重启",
        cancel: "稍后",
      }))
    )
      await op.run(() => api.control("device.reboot"), {
        write: true,
        message: "重启请求已提交，请等待设备恢复后重新登录",
      });
  }
  async function scanNow() {
    if (
      !(await ask({
        title: "扫描 SA 邻区？",
        body: (
          <Text size="sm">
            设备会在必要时临时切至仅 5G，扫描后恢复原模式；最长约三分钟。
          </Text>
        ),
      }))
    )
      return;
    await op.run(
      async () => {
        const result = await api.transaction("network", (tx) =>
          scanNeighbors(tx),
        );
        setScan(
          (result.cells ?? []).map((c) => ({
            ...c,
            rat: "NR",
            rsrp: c.rsrp_dbm,
            sinr: c.sinr_db,
          })),
        );
        await resource.refresh();
        return result;
      },
      {
        write: true,
        message: (r) => `扫描完成，返回 ${r.cells?.length ?? 0} 个小区`,
      },
    );
  }
  const state = resource.data,
    current = serving(state),
    net = state?.net ?? {},
    rows = scan ?? neighbors(state);
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title="小区锁定"
          icon={LockKeyhole}
          right={<RefreshButton resource={resource} disabled={op.busy} />}
        >
          <Stack>
            <InfoGrid
              items={[
                [
                  "LTE 锁定",
                  net.lte_cell_lock
                    ? `PCI ${net.lte_cell_lock.pci} · ${net.lte_cell_lock.earfcn}`
                    : "未配置",
                ],
                [
                  "NR 锁定",
                  net.nr_cell_lock
                    ? `PCI ${net.nr_cell_lock.pci} · ${net.nr_cell_lock.arfcn}`
                    : "未配置",
                ],
              ]}
              cols={2}
            />
            <SimpleGrid cols={{ base: 2, md: 4 }}>
              <Select
                label="制式"
                value={form.rat}
                data={["LTE", "NR"]}
                allowDeselect={false}
                disabled={op.busy}
                onChange={(v) => {
                  setForm((f) => ({ ...f, rat: v }));
                  setDirty(true);
                }}
              />
              {[
                ["pci", form.rat === "NR" ? "PCI · 0–1007" : "PCI · 0–503"],
                ["arfcn", form.rat === "NR" ? "NR-ARFCN" : "EARFCN"],
                ...(form.rat === "NR" ? [["band", "NR 频段 · 如 78"]] : []),
              ].map(([k, l]) => (
                <TextInput
                  key={k}
                  label={l}
                  inputMode="numeric"
                  value={form[k]}
                  disabled={op.busy}
                  onChange={(e) => {
                    setForm((f) => ({ ...f, [k]: e.target.value }));
                    setDirty(true);
                  }}
                />
              ))}
            </SimpleGrid>
            <Group justify="space-between">
              <Group>
                <Button
                  size="xs"
                  variant="light"
                  disabled={!current || op.busy}
                  onClick={() => fill(current)}
                >
                  带入当前小区
                </Button>
                <Button
                  size="xs"
                  variant="default"
                  disabled={op.busy}
                  onClick={() => {
                    setForm((f) => ({ ...f, pci: "", arfcn: "", band: "" }));
                    setDirty(false);
                  }}
                >
                  清空输入
                </Button>
              </Group>
              <Group>
                <Button
                  variant="default"
                  disabled={op.busy}
                  onClick={() => apply(true)}
                >
                  解除全部小区锁
                </Button>
                <Button loading={op.busy} onClick={() => apply()}>
                  保存锁定
                </Button>
              </Group>
            </Group>
          </Stack>
        </Panel>
        <Panel
          title="邻区列表"
          icon={TowerControl}
          right={
            <Group gap="xs">
              <Button
                size="xs"
                variant="default"
                onClick={() => setPaused(!paused)}
                disabled={op.busy}
              >
                {paused ? "恢复刷新" : "暂停刷新"}
              </Button>
              <Button
                size="xs"
                variant="light"
                loading={op.busy}
                disabled={
                  !(
                    String(net.type).toUpperCase() === "SA" ||
                    net.net_select === "Only_5G"
                  )
                }
                onClick={scanNow}
              >
                SA 扫描
              </Button>
            </Group>
          }
        >
          <NeighborTable rows={rows} net={net} onLock={fill} />
        </Panel>
      </LoadState>
    </Stack>
  );
}
export function ParamsPage() {
  const api = usePageApi(),
    { navigate, setNetworkTarget, ask } = useServices(),
    op = useOperation(),
    [scan, setScan] = useState(null),
    resource = useResource(() => api.request("/state")),
    state = resource.data,
    cell = serving(state),
    score = networkScore(cell);
  useEffect(() => {
    if (op.busy) return;
    const timer = setInterval(() => void resource.refresh(), 5000);
    return () => clearInterval(timer);
  }, [op.busy]);
  useEffect(() => setScan(null), [cell?.pci, cell?.arfcn, state?.net?.type]);
  function lock(c) {
    setNetworkTarget({
      rat: c.rat,
      pci: String(c.pci ?? ""),
      arfcn: String(c.arfcn ?? ""),
      band: String(c.band ?? "").replace(/^[Bn]/i, ""),
    });
    navigate("cell-lock");
  }
  async function scanNow() {
    if (
      !(await ask({
        title: "扫描 SA 邻区？",
        body: (
          <Text size="sm">
            扫描可能暂时影响数据连接，设备会自行恢复原模式。
          </Text>
        ),
      }))
    )
      return;
    await op.run(
      async () => {
        const result = await api.transaction("network", scanNeighbors);
        setScan(
          (result.cells ?? []).map((c) => ({
            ...c,
            rat: "NR",
            rsrp: c.rsrp_dbm,
            sinr: c.sinr_db,
          })),
        );
        await resource.refresh();
        return result;
      },
      {
        write: true,
        message: (r) => `扫描完成，返回 ${r.cells?.length ?? 0} 个小区`,
      },
    );
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title="当前服务小区"
          icon={Radio}
          right={
            <Group>
              <Badge variant="light">{cell?.label ?? "等待驻网"}</Badge>
              <RefreshButton resource={resource} />
            </Group>
          }
        >
          <Stack>
            <InfoGrid
              items={[
                ["运营商", localizedOperator(state?.net)],
                [
                  "SIM",
                  state?.sim?.current_slot
                    ? `SIM ${state.sim.current_slot}`
                    : null,
                ],
                [
                  "网络质量",
                  score === null ? null : `${score.toFixed(1)} / 10`,
                ],
                ["频段", cell?.band],
                ["PCI", cell?.pci],
                ["频道", cell?.arfcn],
                ["Cell ID", cell?.cellId],
                ["带宽", cell?.bandwidth],
                ["RSRP", cell?.rsrp == null ? null : `${cell?.rsrp} dBm`],
                ["RSRQ", cell?.rsrq == null ? null : `${cell?.rsrq} dB`],
                ["SINR", cell?.sinr == null ? null : `${cell?.sinr} dB`],
                ["RSSI", cell?.rssi == null ? null : `${cell?.rssi} dBm`],
              ]}
            />
            {cell?.anchor && (
              <>
                <Text size="xs" c="dimmed">
                  LTE 锚点
                </Text>
                <InfoGrid
                  items={Object.entries(cell.anchor).map(([k, v]) => [k, v])}
                />
              </>
            )}
            <Group>
              <Button
                variant="light"
                size="xs"
                disabled={!cell}
                onClick={() => lock(cell)}
              >
                锁定当前小区
              </Button>
              <Button
                variant="default"
                size="xs"
                onClick={() => navigate("cell-lock")}
              >
                手动锁小区
              </Button>
              <Button
                variant="light"
                size="xs"
                onClick={() => navigate("signal")}
              >
                信令解析卡片
              </Button>
            </Group>
          </Stack>
        </Panel>
        <QosSummary state={state} />
        <CarrierPanel state={state} />
        <Panel
          title="邻区"
          icon={TowerControl}
          right={
            <Button
              size="xs"
              variant="light"
              loading={op.busy}
              disabled={
                !(
                  String(state?.net?.type).toUpperCase() === "SA" ||
                  state?.net?.net_select === "Only_5G"
                )
              }
              onClick={scanNow}
            >
              SA 扫描
            </Button>
          }
        >
          <NeighborTable
            rows={scan ?? neighbors(state)}
            net={state?.net}
            onLock={lock}
          />
        </Panel>
      </LoadState>
    </Stack>
  );
}
