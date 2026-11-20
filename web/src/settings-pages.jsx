import React, { useEffect, useState } from "react";
import {
  Alert,
  Badge,
  Box,
  Button,
  Group,
  NavLink,
  NumberInput,
  Progress,
  Select,
  SimpleGrid,
  Stack,
  Switch,
  Table,
  Text,
  TextInput,
} from "@mantine/core";
import { BarChart } from "@mantine/charts";
import {
  ArrowRight,
  Database,
  FileClock,
  Globe,
  HardDrive,
  History,
  Info,
  Cable,
  MessageSquare,
  Network,
  Radio,
  RefreshCw,
  Settings,
  Shield,
  Terminal,
  Wifi,
  ChartNoAxesCombined,
} from "lucide-react";
import { display, formatBytes } from "../core.js";
import { integer, localizedOperator, serving } from "../datad-models.js";
import {
  changed,
  lanForm,
  lanPayload,
  limitBytes,
  usageRows,
} from "../settings-models.js";
import { redial } from "../network-operations.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { InfoGrid, Panel, useServices } from "./ui.jsx";
import { ConnectionPanel } from "./connection-panel.jsx";
import { ModePanel } from "./network-pages.jsx";

export const SETTINGS_GROUPS = [
  [
    "网络与连接",
    [
      ["wifi", "热点设置", Wifi],
      ["cellular", "蜂窝网络", Radio],
      ["apn", "APN 管理", Network],
      ["lan", "LAN / DHCP / MTU", Cable],
      ["traffic", "流量管理", ChartNoAxesCombined],
      ["usage", "历史流量统计", History],
    ],
  ],
  [
    "设备与自动化",
    [
      ["device", "设备信息", Info],
      ["forward", "短信转发", MessageSquare],
      ["tasks", "定时任务", FileClock],
      ["activity", "通知与执行记录", History],
      ["recovery", "断网自动恢复", RefreshCw],
    ],
  ],
  [
    "管理工具",
    [
      ["shell", "Root Shell", Terminal],
      ["ota", "datad 更新", HardDrive],
      ["efs", "EFS 管理器", Database],
      ["nv", "NV 工具", Database],
      ["nms", "NMS 远程管理", Globe],
      ["oem", "官方后台避让", Shield],
      ["signal", "信令分析", Radio],
    ],
  ],
];
export function SettingsPage() {
  const { navigate } = useServices();
  return (
    <Stack>
      <ConnectionPanel />
      <SimpleGrid cols={{ base: 1, lg: 3 }}>
        {SETTINGS_GROUPS.map(([title, items]) => (
          <Panel key={title} title={title} icon={Settings}>
            <Stack gap={3}>
              {items.map(([id, label, Icon]) => (
                <NavLink
                  key={id}
                  label={label}
                  leftSection={<Icon size={17} />}
                  rightSection={<ArrowRight size={14} />}
                  onClick={() => navigate(id)}
                  className="settings-entry"
                />
              ))}
            </Stack>
          </Panel>
        ))}
      </SimpleGrid>
    </Stack>
  );
}
export function CellularPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/state"));
  async function reconnect() {
    if (
      await ask({
        title: "重新拨号？",
        body: <Text size="sm">数据连接会短暂中断，随后尝试重新连接一次。</Text>,
      })
    )
      await op.run(
        async () => {
          await api.transaction("network", (tx) => redial(tx, 5000));
          await resource.refresh();
        },
        { write: true, message: "已提交重新连接，请以驻网和连接状态为准" },
      );
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        <ModePanel resource={resource} api={api} operation={op} />
        <Panel title="数据连接">
          <InfoGrid
            items={[
              ["当前状态", resource.data?.net?.wan_status],
              ["SIM 卡槽", resource.data?.sim?.current_slot],
              ["SIM 状态", resource.data?.sim?.state],
              ["当前驻网", resource.data?.net?.type],
            ]}
          />
          <Group mt="lg">
            <Button variant="light" loading={op.busy} onClick={reconnect}>
              重新拨号
            </Button>
            <RefreshButton resource={resource} disabled={op.busy} />
          </Group>
        </Panel>
      </LoadState>
    </Stack>
  );
}
export function LanPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/lan")),
    [form, setForm] = useState(null),
    [initial, setInitial] = useState(null);
  useEffect(() => {
    if (resource.data) {
      const f = lanForm(resource.data);
      setForm(f);
      setInitial(f);
    }
  }, [resource.data]);
  const dirty = !!form && changed(form, initial);
  useDirtyGuard(dirty);
  const setting = resource.data;
  async function save(mtuOnly) {
    let payload;
    try {
      payload = mtuOnly
        ? {
            mtu: integer(
              form.mtu,
              setting.mtu_min ?? 576,
              setting.mtu_max ?? 1500,
              "MTU",
            ),
          }
        : lanPayload(setting, form);
    } catch (e) {
      await op.run(() => {
        throw e;
      });
      return;
    }
    if (
      !(await ask({
        title: mtuOnly ? "保存 MTU？" : "保存 LAN 设置？",
        body: (
          <Stack>
            <Text size="sm">
              {mtuOnly
                ? `MTU → ${payload.mtu}`
                : `网关 ${payload.ip} · DHCP ${payload.dhcp_enabled ? "开启" : "关闭"}`}
            </Text>
            <Text size="sm">
              修改可能断开当前连接。关闭 DHCP 后，终端可能需要手动配置地址。
            </Text>
          </Stack>
        ),
      }))
    )
      return;
    await op.run(
      async () => {
        const result = await api.request(mtuOnly ? "/lan/mtu" : "/lan", {
          method: "POST",
          body: payload,
          timeout: 45000,
        });
        if (!result.settings || typeof result.verified !== "boolean")
          throw new Error("修改结果未确认，请重新连接后读取配置");
        const next = lanForm(result.settings);
        setInitial(next);
        setForm((old) =>
          mtuOnly ? { ...old, mtu: next.mtu } : { ...next, mtu: old.mtu },
        );
        return result;
      },
      {
        write: true,
        message: (r) =>
          r.verified
            ? r.changed === false
              ? "配置未变化"
              : "已保存并回读确认"
            : "修改已提交，回读尚未一致，请重新读取",
      },
    );
  }
  const field = (k, label, readonly = false) => (
    <TextInput
      label={label}
      value={form?.[k] ?? ""}
      readOnly={readonly}
      disabled={op.busy}
      onChange={(e) => setForm((f) => ({ ...f, [k]: e.target.value }))}
    />
  );
  return (
    <Stack>
      <LoadState resource={resource}>
        {setting?.supported && form ? (
          <>
            <Panel
              title="网关与地址分配"
              icon={Cable}
              right={
                <RefreshButton
                  resource={resource}
                  disabled={op.busy || dirty}
                />
              }
            >
              <Stack>
                <SimpleGrid cols={{ base: 1, sm: 2 }}>
                  {field("ip", "网关地址", !setting.address_writable)}
                  {field("netmask", "子网掩码", !setting.address_writable)}
                </SimpleGrid>
                {!setting.address_writable && (
                  <Text size="xs" c="dimmed">
                    此设备的网关与掩码只读
                  </Text>
                )}
                <Switch
                  label="DHCP 自动分配"
                  checked={form.dhcp_enabled}
                  disabled={op.busy || setting.dhcp_enabled == null}
                  onChange={(e) =>
                    setForm((f) => ({ ...f, dhcp_enabled: e.target.checked }))
                  }
                />
                {form.dhcp_enabled && (
                  <SimpleGrid cols={{ base: 1, sm: 3 }}>
                    {field("dhcp_start", "起始地址")}
                    {field("dhcp_end", "结束地址")}
                    {field("hours", "租期 · 小时")}
                  </SimpleGrid>
                )}
                <Group justify="flex-end">
                  <Button
                    loading={op.busy}
                    disabled={setting.dhcp_enabled == null || !dirty}
                    onClick={() => save(false)}
                  >
                    保存 LAN 设置
                  </Button>
                </Group>
              </Stack>
            </Panel>
            <Panel title="网络 MTU">
              <Group align="end">
                {field(
                  "mtu",
                  `MTU · ${setting.mtu_min ?? 576}–${setting.mtu_max ?? 1500}`,
                )}
                <Button
                  loading={op.busy}
                  disabled={form.mtu === initial?.mtu}
                  onClick={() => save(true)}
                >
                  保存 MTU
                </Button>
              </Group>
            </Panel>
          </>
        ) : (
          <Alert color="orange">设备尚未提供 LAN 配置</Alert>
        )}
      </LoadState>
    </Stack>
  );
}
export function UsagePage() {
  const api = usePageApi(),
    resource = useResource(() => api.request("/traffic/history")),
    [start, setStart] = useState(""),
    [end, setEnd] = useState(""),
    [preset, setPreset] = useState("month");
  function range(key, history = resource.data) {
    const today = history?.device_date ?? new Date().toISOString().slice(0, 10),
      date = new Date(today + "T00:00:00Z");
    setEnd(today);
    if (key === "month") date.setUTCDate(1);
    else date.setUTCDate(date.getUTCDate() - Number(key) + 1);
    setStart(date.toISOString().slice(0, 10));
    setPreset(key);
  }
  useEffect(() => {
    if (resource.data && preset) range(preset, resource.data);
  }, [resource.data]);
  let rows = [],
    error = "";
  try {
    if (start && end && resource.data)
      rows = usageRows(resource.data, start, end);
  } catch (e) {
    error = e.message;
  }
  return (
    <LoadState resource={resource}>
      <Stack>
        <Panel
          title="时间范围"
          icon={History}
          right={<RefreshButton resource={resource} />}
        >
          <Stack>
            <SimpleGrid cols={{ base: 1, sm: 2 }}>
              <TextInput
                type="date"
                label="开始"
                value={start}
                onChange={(e) => {
                  setStart(e.target.value);
                  setPreset("");
                }}
              />
              <TextInput
                type="date"
                label="结束"
                value={end}
                onChange={(e) => {
                  setEnd(e.target.value);
                  setPreset("");
                }}
              />
            </SimpleGrid>
            <Group>
              {[
                ["7", "近 7 天"],
                ["30", "近 30 天"],
                ["month", "本月"],
              ].map(([key, label]) => (
                <Button
                  key={key}
                  variant={preset === key ? "light" : "default"}
                  size="xs"
                  onClick={() => range(key)}
                >
                  {label}
                </Button>
              ))}
            </Group>
            {error ? (
              <Alert color="red">{error}</Alert>
            ) : (
              <>
                <Text fz={26} fw={600}>
                  {rows.length
                    ? formatBytes(rows.reduce((s, d) => s + Number(d.bytes), 0))
                    : "—"}
                </Text>
                <Text size="xs" c="dimmed">
                  {rows.length} 天记录 ·
                  上传与下载合计；按设备日期统计，缺失日期不补零。
                </Text>
              </>
            )}
            <Text size="xs" c="dimmed">
              最早记录 {resource.data?.oldest_date || "未上报"} · 每{" "}
              {Number(resource.data?.sample_interval_seconds ?? 300) / 60}{" "}
              分钟采样
            </Text>
          </Stack>
        </Panel>
        <Panel title="按日用量">
          {rows.length ? (
            <Stack>
              <BarChart
                h={230}
                data={rows.map((d) => ({
                  ...d,
                  MiB: Number(d.bytes) / 1048576,
                }))}
                dataKey="date"
                series={[{ name: "MiB", color: "blue.6" }]}
                valueFormatter={(n) => formatBytes(n * 1048576)}
                withLegend={false}
              />
              <Table fz="sm">
                <Table.Tbody>
                  {rows
                    .slice()
                    .reverse()
                    .map((d) => (
                      <Table.Tr key={d.date}>
                        <Table.Td>
                          {d.date}
                          {d.date === resource.data?.device_date
                            ? " · 今日"
                            : ""}
                        </Table.Td>
                        <Table.Td ta="right">{formatBytes(d.bytes)}</Table.Td>
                      </Table.Tr>
                    ))}
                </Table.Tbody>
              </Table>
            </Stack>
          ) : (
            <Text size="sm" c="dimmed">
              所选范围暂无记录
            </Text>
          )}
        </Panel>
      </Stack>
    </LoadState>
  );
}
export function TrafficPage() {
  const api = usePageApi(),
    { ask, navigate } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/state")),
    [enabled, setEnabled] = useState(false),
    [gb, setGb] = useState(""),
    [initial, setInitial] = useState("");
  useEffect(() => {
    if (resource.data) {
      const limit = resource.data.traffic?.limit;
      setEnabled(Number(limit?.enable) === 1);
      const amount = limitBytes(limit?.value);
      const v = amount ? String(amount / 1073741824) : "";
      setGb(v);
      setInitial(JSON.stringify([Number(limit?.enable) === 1, v]));
    }
  }, [resource.data]);
  const dirty = JSON.stringify([enabled, gb]) !== initial;
  useDirtyGuard(!!initial && dirty);
  const t = resource.data?.traffic,
    used = t
      ? Number(t.month_rx_bytes ?? 0) + Number(t.month_tx_bytes ?? 0)
      : null,
    max = limitBytes(t?.limit?.value);
  async function save() {
    await op.run(
      async () => {
        const value = Number(gb);
        if (
          enabled &&
          (!Number.isFinite(value) ||
            value <= 0 ||
            !Number.isSafeInteger(Math.round(value * 1024)))
        )
          throw new Error("请输入有效流量限额");
        if (
          !(await ask({
            title: "保存流量限额？",
            body: (
              <Text size="sm">
                {enabled ? `限额 ${gb} GB` : "关闭流量限额"}
                ，达到限额后设备可能断开网络。
              </Text>
            ),
          }))
        )
          return null;
        await api.control(
          "traffic.set_limit",
          enabled
            ? { enabled: 1, type: 1, value: String(Math.round(value * 1024)) }
            : { enabled: 0 },
        );
        const next = await api.request("/state");
        resource.setData(next);
        return (
          Number(next.traffic?.limit?.enable) === (enabled ? 1 : 0) &&
          (!enabled ||
            limitBytes(next.traffic?.limit?.value) ===
              Math.round(value * 1024) * 1048576)
        );
      },
      {
        write: true,
        message: (ok) =>
          ok === null
            ? ""
            : ok
              ? "已保存并回读确认"
              : "修改已提交，回读尚未一致，请刷新确认",
      },
    );
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title="流量管理"
          right={
            <RefreshButton resource={resource} disabled={dirty || op.busy} />
          }
        >
          <Stack>
            <InfoGrid
              items={[
                [
                  "今日用量",
                  t
                    ? formatBytes(
                        Number(t.today_rx_bytes ?? 0) +
                          Number(t.today_tx_bytes ?? 0),
                      )
                    : null,
                ],
                ["本月用量", formatBytes(used)],
                ["已配置限额", formatBytes(max)],
              ]}
            />
            {max > 0 && used !== null && (
              <Progress
                value={Math.min(100, (100 * used) / max)}
                color={used >= max ? "red" : "blue"}
              />
            )}
            <Switch
              label="启用流量限额"
              checked={enabled}
              onChange={(e) => setEnabled(e.target.checked)}
              disabled={op.busy}
            />
            {enabled && (
              <TextInput
                label="限额 · GB"
                inputMode="decimal"
                value={gb}
                onChange={(e) => setGb(e.target.value)}
                disabled={op.busy}
              />
            )}
            <Group justify="space-between">
              <Button variant="light" onClick={() => navigate("usage")}>
                历史流量统计
              </Button>
              <Button loading={op.busy} disabled={!dirty} onClick={save}>
                保存
              </Button>
            </Group>
          </Stack>
        </Panel>
      </LoadState>
    </Stack>
  );
}
export function DevicePage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/state"), [], 5000),
    s = resource.data,
    d = s?.device ?? {},
    system = s?.system ?? {},
    b = s?.battery ?? {};
  async function reboot() {
    if (
      await ask({
        title: "重启设备？",
        body: <Text size="sm">连接会中断，请等待设备启动后重新登录。</Text>,
        confirm: "重启",
      })
    )
      await op.run(() => api.control("device.reboot"), {
        write: true,
        message: "重启请求已提交，等待设备恢复",
      });
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title="设备信息"
          icon={Info}
          right={<RefreshButton resource={resource} />}
        >
          <InfoGrid
            items={[
              ["设备名称", d.nickname || d.alias_name],
              ["机型", d.market_name || d.model_name || system.model],
              ["硬件版本", d.hardware_version],
              ["固件版本", system.sw_version || d.sw_version],
              ["datad 版本", s?.datad?.version],
              [
                "运行时长",
                system.uptime == null
                  ? null
                  : `${Math.floor(system.uptime / 86400)} 天 ${Math.floor((system.uptime % 86400) / 3600)} 时 ${Math.floor((system.uptime % 3600) / 60)} 分`,
              ],
              [
                "CPU 温度",
                system.cpu_temp == null ? null : `${system.cpu_temp} ℃`,
              ],
              [
                "CPU 使用率",
                system.cpu_usage == null ? null : `${system.cpu_usage}%`,
              ],
              [
                "内存使用率",
                system.mem_used_pct == null ? null : `${system.mem_used_pct}%`,
              ],
              [
                "电量",
                b.percent == null || b.percent < 0 ? null : `${b.percent}%`,
              ],
              [
                "充电状态",
                { 1: "正在充电", 2: "放电中", 3: "未充电", 4: "已充满" }[
                  b.charging
                ],
              ],
              ["电池温度", b.temp == null ? null : `${b.temp} ℃`],
              [
                "电压",
                b.bat_uv == null
                  ? null
                  : `${(b.bat_uv / 1000000).toFixed(2)} V`,
              ],
              [
                "电流",
                b.bat_ua == null ? null : `${(b.bat_ua / 1000).toFixed(0)} mA`,
              ],
              ["运营商", localizedOperator(s?.net)],
              ["网络", s?.net?.type],
              ["SIM 卡槽", s?.sim?.current_slot],
              ["IMEI", s?.modem?.imei || system.imei],
              ["ICCID", s?.sim?.iccid],
              ["IMSI", s?.sim?.imsi],
            ]}
          />
          <Group mt="lg">
            <Button
              variant="light"
              color="red"
              loading={op.busy}
              onClick={reboot}
            >
              重启设备
            </Button>
          </Group>
        </Panel>
      </LoadState>
    </Stack>
  );
}
