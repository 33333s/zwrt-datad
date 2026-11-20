import React, { useEffect, useState } from "react";
import { AreaChart } from "@mantine/charts";
import {
  Alert,
  Badge,
  Box,
  Button,
  Card,
  Center,
  Divider,
  Group,
  Progress,
  SegmentedControl,
  SimpleGrid,
  Stack,
  Text,
  ThemeIcon,
} from "@mantine/core";
import {
  ArrowDown,
  ArrowUp,
  Battery,
  ChartNoAxesCombined,
  ChevronRight,
  Cpu,
  HardDrive,
  Monitor,
  Radio,
  Signal,
  Wifi,
} from "lucide-react";
import { display, formatBytes } from "../core.js";
import { InfoGrid, Panel, useServices } from "./ui.jsx";
import { CarrierPanel } from "./carrier-panel.jsx";
import { localizedOperator } from "../datad-models.js";
const physical = (value, lo, hi) =>
  value !== null &&
  value !== undefined &&
  value !== "" &&
  Number.isFinite(Number(value)) &&
  Number(value) >= lo &&
  Number(value) <= hi
    ? Number(value)
    : undefined;
const isNr = (net) =>
  ["SA", "NR", "NR5G", "5G", "ENDC", "LTE-NSA", "NSA"].includes(
    net.type?.toUpperCase(),
  );
const cellKey = (net) =>
  `${net.type}/${net[isNr(net) ? "nr_pci" : "lte_pci"]}/${net[isNr(net) ? "nr_channel" : "lte_channel"]}`;
const batteryLabel = (raw) =>
  ({
    Discharging: "正在放电",
    Charging: "正在充电",
    Full: "已充满",
    NotCharging: "未充电",
    "Not charging": "未充电",
  })[raw] ?? raw;
function point(state) {
  return {
    label: new Date().toLocaleTimeString("zh-CN", { hour12: false }),
    RSRP: physical(
      state.net?.[isNr(state.net ?? {}) ? "nr_rsrp" : "lte_rsrp"],
      -156,
      -31,
    ),
    下行: physical(state.traffic?.rx_speed, 0, 1e12),
    上行: physical(state.traffic?.tx_speed, 0, 1e12),
  };
}
function Metric({
  label,
  value,
  unit,
  subtitle,
  icon: Icon,
  color = "blue",
  primary = false,
  footer,
}) {
  return (
    <Box className={"metric-card " + (primary ? "metric-primary" : "")}>
      <Group justify="space-between" align="flex-start">
        <Text size="xs" className="metric-label">
          {label}
        </Text>
        <ThemeIcon
          size={24}
          variant="light"
          color={color}
          radius="md"
          className="metric-icon"
        >
          <Icon size={16} strokeWidth={1.7} />
        </ThemeIcon>
      </Group>
      <Group align="baseline" gap={6} mt={8}>
        <Text fz={32} fw={650} className="metric">
          {display(value)}
        </Text>
        <Text size="xs" className="metric-unit">
          {value === undefined ? "" : unit}
        </Text>
      </Group>
      <Group justify="space-between" mt={6} gap={8} wrap="nowrap">
        <Text fz={11} className="metric-caption" lineClamp={1}>
          {subtitle}
        </Text>
        {footer}
      </Group>
    </Box>
  );
}
function SummaryTile({ title, value, subtitle, action, onClick }) {
  return (
    <Box
      className="overview-summary-tile"
      component={onClick ? "button" : "div"}
      type={onClick ? "button" : undefined}
      onClick={onClick}
      aria-label={onClick ? `${title} · ${action}` : undefined}
    >
      <Group justify="space-between" wrap="nowrap" gap={4}>
        <Text size="xs" c="dimmed">
          {title}
        </Text>
        {onClick && <ChevronRight size={13} aria-hidden="true" />}
      </Group>
      <Text className="overview-summary-value" fw={650}>
        {value}
      </Text>
      <Text fz={11} c="dimmed" className="overview-summary-caption">
        {subtitle || action}
      </Text>
    </Box>
  );
}
function TrendPanel({ points }) {
  const { api } = useServices(),
    preferenceKey = "datad.web.trend-mode.v1:" + api.base.href;
  const [mode, setMode] = useState(() => {
      try {
        return localStorage.getItem(preferenceKey) === "signal"
          ? "signal"
          : "speed";
      } catch {
        return "speed";
      }
    }),
    speed = mode === "speed",
    samples = points.filter((p) =>
      speed
        ? p.下行 !== undefined || p.上行 !== undefined
        : p.RSRP !== undefined,
    ),
    values = samples.map((p) => p.RSRP),
    lo =
      values.length && !speed
        ? Math.max(-160, Math.floor((Math.min(...values) - 10) / 10) * 10)
        : -130,
    hi =
      values.length && !speed
        ? Math.min(-30, Math.ceil((Math.max(...values) + 10) / 10) * 10)
        : -40;
  return (
    <Panel
      title={speed ? "速率趋势" : "信号趋势"}
      icon={ChartNoAxesCombined}
      collapsible
      right={
        <SegmentedControl
          size="xs"
          value={mode}
          onChange={(value) => {
            if (!["signal", "speed"].includes(value)) return;
            setMode(value);
            try {
              localStorage.setItem(preferenceKey, value);
            } catch {}
          }}
          data={[
            { value: "signal", label: "RSRP" },
            { value: "speed", label: "速率" },
          ]}
        />
      }
      className="trend-panel"
    >
      <Box className="chart-wrap">
        {samples.length >= 2 ? (
          <AreaChart
            h={160}
            data={points}
            dataKey="label"
            series={
              speed
                ? [
                    { name: "下行", color: "blue.5" },
                    { name: "上行", color: "teal.5" },
                  ]
                : [{ name: "RSRP", color: "blue.5" }]
            }
            curveType="monotone"
            withDots={false}
            fillOpacity={0.12}
            strokeWidth={2}
            gridAxis="y"
            tickLine="none"
            withLegend={speed}
            withTooltip
            valueFormatter={(value) =>
              speed ? formatBytes(value) + "/s" : value + " dBm"
            }
            yAxisProps={{
              domain: speed ? [0, "auto"] : [lo, hi],
              width: 75,
              tickCount: 4,
            }}
            xAxisProps={{ minTickGap: 55, tickMargin: 10 }}
            areaProps={{ isAnimationActive: false, baseValue: speed ? 0 : lo }}
          />
        ) : (
          <Center h={160}>
            <Text size="xs" c="dimmed">
              正在积累{speed ? "速率" : "信号"}采样
            </Text>
          </Center>
        )}
      </Box>
      <Group justify="space-between" mt={12}>
        <Text fz={10} c="dimmed">
          每 3 秒采样 · 最近 60 次
        </Text>
        <Text fz={10} c="dimmed">
          {!speed && values.length
            ? `平均 ${(values.reduce((a, b) => a + b, 0) / values.length).toFixed(1)} dBm · `
            : ""}
          {samples.length} 个有效采样
        </Text>
      </Group>
    </Panel>
  );
}
const sum = (a, b) => (a == null || b == null ? null : Number(a) + Number(b));
export function OverviewPage() {
  const { api, connection, refreshTick, setOnline, navigate, observeSnapshot } =
      useServices(),
    [state, setState] = useState(connection.snapshot),
    [error, setError] = useState(""),
    [points, setPoints] = useState([point(connection.snapshot)]);
  useEffect(() => {
    let alive = true,
      timer,
      lastCell = cellKey(state.net ?? {});
    const controller = new AbortController();
    async function tick() {
      if (!alive) return;
      if (!document.hidden) {
        try {
          const next = await api.request("/state", {
            signal: controller.signal,
          });
          if (alive) {
            setState(next);
            observeSnapshot?.(next);
            setError("");
            setOnline(true);
            const nextKey = cellKey(next.net ?? {}),
              changed = nextKey !== lastCell;
            setPoints((previous) =>
              (changed ? [] : previous).concat(point(next)).slice(-60),
            );
            lastCell = nextKey;
          }
        } catch (e) {
          if (alive) {
            setError(e.message);
            setOnline(false);
          }
        }
      }
      if (alive) timer = setTimeout(tick, 3000);
    }
    void tick();
    return () => {
      alive = false;
      controller.abort();
      clearTimeout(timer);
    };
  }, [api, refreshTick]);
  const net = state.net ?? {},
    nr = isNr(net),
    nsa = ["ENDC", "LTE-NSA", "NSA"].includes(net.type?.toUpperCase()),
    prefix = nr ? "nr" : "lte",
    battery = state.battery ?? {},
    traffic = state.traffic ?? {},
    system = state.system ?? {},
    runtime = state.runtime ?? {};
  const rsrp = physical(net[`${prefix}_rsrp`], -156, -31),
    sinr = physical(net[`${prefix}_snr`], -30, 40),
    rsrq = physical(net[`${prefix}_rsrq`], -43, 20),
    cpu = physical(
      system.cpu_usage ??
        (runtime.cpu_usage_tenths === undefined
          ? undefined
          : runtime.cpu_usage_tenths / 10),
      0,
      100,
    ),
    memory = physical(
      system.mem_used_pct ??
        (runtime.memory_kb?.total > 0 && runtime.memory_kb?.available != null
          ? ((runtime.memory_kb.total - runtime.memory_kb.available) * 100) /
            runtime.memory_kb.total
          : undefined),
      0,
      100,
    );

  function serving(which) {
    const p = which === "NR" ? "nr" : "lte";
    const fields = [
      ["频段", p === "nr" ? net.nr_band : net.band],
      ["PCI", net[`${p}_pci`]],
      ["频道", net[`${p}_channel`]],
      ["带宽", net[`${p}_bw`]],
      [
        "RSRP",
        physical(net[`${p}_rsrp`], -156, -31) === undefined
          ? undefined
          : net[`${p}_rsrp`] + " dBm",
      ],
      [
        "RSRQ",
        physical(net[`${p}_rsrq`], -43, 20) === undefined
          ? undefined
          : net[`${p}_rsrq`] + " dB",
      ],
      [
        "SINR",
        physical(net[`${p}_snr`], -30, 40) === undefined
          ? undefined
          : net[`${p}_snr`] + " dB",
      ],
      ["小区 ID", net[`${p}_cell_id`]],
      ["TAC", net[`${p}_tac`]],
    ];
    return (
      <InfoGrid
        key={`${p}:${net[`${p}_cell_id`]}:${net[`${p}_channel`]}:${net[`${p}_pci`]}`}
        sensitive={["小区 ID", "TAC"]}
        cols={{ base: 2, sm: 3, lg: 5 }}
        items={fields.filter(
          ([, v], i) => i < 4 || (v !== undefined && v !== null && v !== ""),
        )}
      />
    );
  }
  function usage(title, value, Icon, color) {
    return (
      <div className="resource-row">
        <Group justify="space-between" mb={11}>
          <Group gap={8}>
            <Icon size={15} />
            <Text size="xs" c="dimmed">
              {title}
            </Text>
          </Group>
          <Text size="sm" fw={600} className="value">
            {value === undefined ? "—" : `${value.toFixed(0)}%`}
          </Text>
        </Group>
        <Progress
          value={value ?? 0}
          color={color}
          size={5}
          radius="xl"
          aria-label={title}
        />
      </div>
    );
  }
  return (
    <Stack gap={14} className="overview-page">
      {error && (
        <Alert color="orange" title="状态暂未更新">
          {error}。以下保留上次成功读取的状态。
        </Alert>
      )}
      <Card withBorder padding={0} radius="lg" className="overview-metrics">
        <SimpleGrid cols={{ base: 2, lg: 4 }} spacing={0}>
          <Metric
            label="当前网络"
            value={net.type}
            subtitle={localizedOperator(net) || "等待驻网"}
            icon={Wifi}
            primary
            footer={
              <span className="network-mini-label">
                {display(nr ? net.nr_band : net.band)}
              </span>
            }
          />
          <Metric
            label="参考信号功率"
            value={rsrp}
            unit="dBm"
            subtitle={`${nr ? "NR" : "LTE"} · PCI ${display(net[`${prefix}_pci`])}`}
            icon={Signal}
          />
          <Metric
            label="信噪比"
            value={sinr}
            unit="dB"
            subtitle={rsrq === undefined ? "当前服务小区" : `RSRQ ${rsrq} dB`}
            icon={Radio}
            color="teal"
          />
          <Metric
            label="设备电量"
            value={physical(battery.percent, 0, 100)}
            unit="%"
            subtitle={
              { 1: "正在充电", 2: "正在放电", 3: "未充电", 4: "已充满" }[
                battery.charging
              ] ||
              batteryLabel(battery.status) ||
              "电源状态未知"
            }
            icon={Battery}
            color="orange"
          />
        </SimpleGrid>
      </Card>
      <div className="overview-main-grid">
        <TrendPanel points={points} />
        <section
          className="overview-summary"
          aria-label="流量、热点与 SIM 概览"
        >
          <SummaryTile
            title="今日流量"
            value={formatBytes(
              sum(traffic.today_rx_bytes, traffic.today_tx_bytes),
            )}
            action="流量详情"
            onClick={() => navigate("traffic")}
          />
          <SummaryTile
            title="本月已用"
            value={formatBytes(
              sum(traffic.month_rx_bytes, traffic.month_tx_bytes),
            )}
            action="历史统计"
            onClick={() => navigate("usage")}
          />
          <SummaryTile
            title="热点接入"
            value={display(state.clients?.total ?? state.clients?.wifi)}
            action="热点设置"
            onClick={() => navigate("wifi")}
          />
          <SummaryTile
            title="SIM 信息"
            value={
              state.sim?.current_slot == null
                ? "当前 SIM"
                : `SIM ${state.sim.current_slot}`
            }
            subtitle={localizedOperator(net) || "运营商未上报"}
          />
        </section>
        <Panel
          title="运行状态"
          icon={Cpu}
          className="resources-panel"
          right={
            <Button
              size="compact-xs"
              variant="subtle"
              onClick={() => navigate("device")}
            >
              详情
            </Button>
          }
        >
          <Stack gap={22}>
            {usage("CPU 占用", cpu, Cpu, "blue")}
            {usage("内存占用", memory, HardDrive, "teal")}
            <Divider />
            <Group justify="space-between">
              <Box>
                <Text size="xs" c="dimmed" mb={6}>
                  运行时间
                </Text>
                <Text size="sm" fw={500}>
                  {system.uptime === undefined
                    ? "—"
                    : `${Math.floor(system.uptime / 3600)} 小时 ${Math.floor((system.uptime % 3600) / 60)} 分钟`}
                </Text>
              </Box>
              <Box ta="right">
                <Text size="xs" c="dimmed" mb={6}>
                  CPU 温度
                </Text>
                <Text size="sm" fw={500}>
                  {system.cpu_temp === undefined
                    ? "—"
                    : `${system.cpu_temp} °C`}
                </Text>
              </Box>
            </Group>
          </Stack>
        </Panel>
      </div>
      <Panel
        title="当前驻网"
        icon={Radio}
        right={
          <Group gap={6}>
            <Button
              size="compact-xs"
              variant="subtle"
              onClick={() => navigate("params")}
            >
              详情
            </Button>
            <Badge variant="light" color="blue" size="sm">
              {display(net.type)}
            </Badge>
            <Badge variant="light" color="gray" size="sm">
              {nsa ? "NR / PSCell" : "PCC"}
            </Badge>
          </Group>
        }
      >
        <Stack gap={20}>
          {serving(nr ? "NR" : "LTE")}
          {nsa && (
            <>
              <Divider label="LTE 锚点 / PCC" />
              {serving("LTE")}
            </>
          )}
        </Stack>
      </Panel>
      <CarrierPanel state={state} />
      <SimpleGrid cols={{ base: 1, lg: 2 }} spacing={18}>
        <Panel title="实时流量" icon={ArrowDown}>
          <SimpleGrid cols={2} spacing="lg">
            <Box className="throughput-field">
              <Group gap={6} mb={8}>
                <ArrowDown size={13} color="var(--mantine-color-blue-5)" />
                <Text size="xs" c="dimmed">
                  下行速率
                </Text>
              </Group>
              <Text fw={600} size="lg" className="value">
                {traffic.rx_speed === undefined
                  ? "—"
                  : formatBytes(traffic.rx_speed)}
                <Text span size="xs" c="dimmed">
                  {traffic.rx_speed === undefined ? "" : " /s"}
                </Text>
              </Text>
              <Text fz={10} c="dimmed" mt={7}>
                会话接收 {formatBytes(traffic.rx_bytes)}
              </Text>
            </Box>
            <Box>
              <Group gap={6} mb={8}>
                <ArrowUp size={13} color="var(--mantine-color-teal-5)" />
                <Text size="xs" c="dimmed">
                  上行速率
                </Text>
              </Group>
              <Text fw={600} size="lg" className="value">
                {traffic.tx_speed === undefined
                  ? "—"
                  : formatBytes(traffic.tx_speed)}
                <Text span size="xs" c="dimmed">
                  {traffic.tx_speed === undefined ? "" : " /s"}
                </Text>
              </Text>
              <Text fz={10} c="dimmed" mt={7}>
                会话发送 {formatBytes(traffic.tx_bytes)}
              </Text>
            </Box>
          </SimpleGrid>
        </Panel>
        <Panel title="设备信息" icon={Monitor} collapsible>
          <InfoGrid
            cols={2}
            items={[
              ["型号", state.device?.model_name],
              ["datad", state.datad?.version],
              ["固件", state.device?.sw_version || system.sw_version],
              ["硬件", state.device?.hardware_version],
            ]}
          />
        </Panel>
      </SimpleGrid>
    </Stack>
  );
}
