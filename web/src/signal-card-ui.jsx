import React, { useMemo, useState, useSyncExternalStore } from "react";
import {
  Accordion,
  Alert,
  Badge,
  Box,
  Button,
  Card,
  Group,
  ScrollArea,
  Select,
  SimpleGrid,
  Stack,
  Table,
  Text,
  TextInput,
} from "@mantine/core";
import {
  Activity,
  Antenna,
  ArrowUp,
  Building2,
  Clock,
  MapPin,
  Network,
  Radio,
  Search,
  Shield,
} from "lucide-react";
import { display } from "../core.js";
import { dataSession, isDataSession } from "../qos.js";
import { highSpeedRail } from "../rrc-context.js";
import {
  linkRows,
  neighborRows,
  servingRows,
  measuredServing,
} from "../signal-cards.js";
import { Panel, useServices } from "./ui.jsx";

const when = (ts) =>
  ts ? new Date(ts).toLocaleTimeString("zh-CN", { hour12: false }) : "尚未上报";
export function DetailRows({ rows }) {
  return (
    <Table
      fz="xs"
      verticalSpacing={6}
      withRowBorders={false}
      style={{ tableLayout: "fixed" }}
    >
      <Table.Tbody>
        {rows.map(([label, value], i) => (
          <Table.Tr key={label + ":" + i}>
            <Table.Td w="38%" c="dimmed" style={{ verticalAlign: "top" }}>
              {label}
            </Table.Td>
            <Table.Td style={{ overflowWrap: "anywhere", userSelect: "text" }}>
              {display(value)}
            </Table.Td>
          </Table.Tr>
        ))}
      </Table.Tbody>
    </Table>
  );
}
export function BeamTable({ beams = [] }) {
  if (!beams.length) return null;
  const ordered = beams.slice().sort((a, b) => a.index - b.index),
    strongest = ordered.reduce((a, b) => (a.rsrp >= b.rsrp ? a : b)),
    branches = Math.max(...beams.map((b) => b.branchRsrp.length));
  return (
    <Stack gap="xs">
      <Group justify="space-between">
        <Text size="xs" c="dimmed">
          {beams.length} 个已测 SSB
        </Text>
        <Badge size="xs" variant="light">
          最强已测 #{strongest.index}
        </Badge>
      </Group>
      <Table.ScrollContainer minWidth={330}>
        <Table fz="xs" striped verticalSpacing={6}>
          <Table.Thead>
            <Table.Tr>
              {[
                "SSB",
                "RSRP · dBm",
                "RSRQ · dB",
                ...Array.from(
                  { length: branches },
                  (_, i) => `分支 ${i} · dBm`,
                ),
              ].map((k) => (
                <Table.Th key={k} ta={k === "SSB" ? "left" : "right"}>
                  {k}
                </Table.Th>
              ))}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {ordered.map((b) => (
              <Table.Tr
                key={b.index}
                bg={
                  b.index === strongest.index
                    ? "var(--mantine-color-blue-light)"
                    : undefined
                }
              >
                <Table.Td fw={b.index === strongest.index ? 650 : 400}>
                  #{b.index}
                </Table.Td>
                {[
                  b.rsrp,
                  b.rsrq,
                  ...Array.from(
                    { length: branches },
                    (_, i) => b.branchRsrp[i],
                  ),
                ].map((n, i) => (
                  <Table.Td
                    key={i}
                    ta="right"
                    style={{ fontVariantNumeric: "tabular-nums" }}
                  >
                    {n == null ? "—" : n.toFixed(1)}
                  </Table.Td>
                ))}
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      </Table.ScrollContainer>
      <Text fz={10} c="dimmed">
        最强 SSB ≠ 业务波束；-156 dBm 为测量下限。
      </Text>
    </Stack>
  );
}
const isBeamRow = (k) =>
  ["已测 SSB / beam", "最强已测 SSB"].includes(k) ||
  k.startsWith("SSB #") ||
  (k.startsWith("#") && k.endsWith(" 分支 RSRP"));
function ServingContent({ cards, state }) {
  const rows = servingRows(cards, state),
    metrics = ["RSRP", "RSRQ", "SINR"],
    groups = new Map();
  for (const row of rows) {
    const k = row[0];
    if (metrics.includes(k)) continue;
    const title = ["RSSI", "SNR（信令）", "信令测量来源", "设备状态"].includes(
      k,
    )
      ? "测量来源"
      : ["高铁专网识别", "识别依据", "厂商（推断）", "运营商", "PLMN"].includes(
            k,
          )
        ? "网络识别"
        : isBeamRow(k)
          ? "SSB 波束"
          : ["同频同模3邻区", "模三判读", "PCI 模3"].includes(k)
            ? "模三对比"
            : [
                  "BLER",
                  "MCS",
                  "调制",
                  "PUSCH 功率",
                  "AMBR 下行 / 上行",
                  "QCI",
                  "安全算法",
                ].includes(k)
              ? "链路与承载"
              : "小区与频点";
    if (!groups.has(title)) groups.set(title, []);
    groups.get(title).push(row);
  }
  return (
    <Stack gap="md">
      <SimpleGrid cols={3}>
        {metrics.map((key) => {
          const value = rows.find((r) => r[0] === key)?.[1] ?? "尚未上报";
          return (
            <Card key={key} withBorder radius="md" padding="sm">
              <Text fz={10} c="dimmed">
                {key}
              </Text>
              <Text size="sm" fw={600} my={5}>
                {String(value).split(" · ")[0]}
              </Text>
              <Text fz={10} c="dimmed">
                {String(value).split(" · ")[1] ?? "—"}
              </Text>
            </Card>
          );
        })}
      </SimpleGrid>
      {[
        "小区与频点",
        "网络识别",
        "测量来源",
        "SSB 波束",
        "模三对比",
        "链路与承载",
      ]
        .filter((g) => groups.has(g))
        .map((g) => (
          <Box key={g}>
            <Text size="xs" c="blue" fw={600} mb={7}>
              {g}
            </Text>
            {g === "SSB 波束" && measuredServing(cards)?.beams?.length ? (
              <BeamTable beams={measuredServing(cards).beams} />
            ) : (
              <DetailRows rows={groups.get(g)} />
            )}
          </Box>
        ))}
    </Stack>
  );
}
function EngineeringContent({ groups }) {
  const [query, setQuery] = useState(""),
    [source, setSource] = useState("all"),
    [selected, setSelected] = useState(null),
    sources = Object.keys(groups).sort(),
    all = sources.flatMap((k) => groups[k]),
    needle = query.trim().toLowerCase(),
    sections = all
      .filter((s) => source === "all" || s.source === source)
      .map((s) => ({
        ...s,
        rows: s.rows.filter(
          (r) =>
            !needle ||
            `${s.title} ${s.source} ${r.label} ${r.path} ${r.value}`
              .toLowerCase()
              .includes(needle),
        ),
      }))
      .filter((s) => s.rows.length);
  return (
    <Stack gap="md">
      <Group>
        <TextInput
          placeholder="搜索工参、字段或来源"
          aria-label="搜索工参"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          leftSection={<Search size={14} />}
          style={{ flex: 1 }}
        />
        {sources.length > 1 && (
          <Select
            aria-label="工参来源"
            value={source}
            data={[
              { value: "all", label: "全部来源" },
              ...sources.map((v) => ({ value: v, label: v })),
            ]}
            allowDeselect={false}
            onChange={setSource}
          />
        )}
      </Group>
      <Text size="xs" c="dimmed">
        {sections.reduce((n, s) => n + s.rows.length, 0)} 项参数 ·
        点按参数查看来源
      </Text>
      {!sections.length ? (
        <Text size="sm" c="dimmed" py="lg">
          {all.length ? "没有匹配的参数" : "等待广播配置，收到 SIB 后自动显示"}
        </Text>
      ) : (
        <Accordion
          key={needle ? needle : source}
          multiple={!!needle}
          defaultValue={needle ? sections.map((s) => s.id) : sections[0]?.id}
          radius="md"
          variant="contained"
        >
          {sections.map((s) => (
            <Accordion.Item key={s.id} value={s.id}>
              <Accordion.Control>
                <Group justify="space-between" wrap="nowrap">
                  <Box>
                    <Text size="sm" fw={550}>
                      {s.title}
                    </Text>
                    <Text fz={10} c="dimmed">
                      {s.source}
                    </Text>
                  </Box>
                  <Badge size="xs" variant="light">
                    {s.rows.length}
                  </Badge>
                </Group>
              </Accordion.Control>
              <Accordion.Panel>
                <Stack gap={0}>
                  {s.rows.map((r) => (
                    <Box
                      key={r.path}
                      component="button"
                      type="button"
                      className="engineering-parameter"
                      onClick={() =>
                        setSelected(selected === r.path ? null : r.path)
                      }
                    >
                      <Group justify="space-between" wrap="nowrap" gap="md">
                        <Text
                          size="xs"
                          c="dimmed"
                          ta="left"
                          style={{ flex: 1 }}
                        >
                          {r.label.split(" · ").at(-1)}
                        </Text>
                        <Text
                          size="sm"
                          fw={550}
                          ta="right"
                          style={{ maxWidth: "55%", overflowWrap: "anywhere" }}
                        >
                          {r.value.split("（编码 ")[0]}
                        </Text>
                      </Group>
                      {selected === r.path && (
                        <Text
                          mt={9}
                          fz={10}
                          c="dimmed"
                          ta="left"
                          style={{
                            overflowWrap: "anywhere",
                            userSelect: "text",
                          }}
                        >
                          {s.source}
                          <br />
                          {r.path}
                          <br />
                          原始值：{JSON.stringify(r.raw)}
                        </Text>
                      )}
                    </Box>
                  ))}
                </Stack>
              </Accordion.Panel>
            </Accordion.Item>
          ))}
        </Accordion>
      )}
    </Stack>
  );
}
function QosContent({ cards }) {
  const qos = cards.bearerQos,
    entries = Object.entries(qos.sessions).sort(([a], [b]) =>
      a === qos.selected ? -1 : b === qos.selected ? 1 : a.localeCompare(b),
    );
  return (
    <Stack gap="md">
      {!entries.length && (
        <Text size="sm" c="dimmed">
          等待捕获承载建立消息
        </Text>
      )}
      {entries.map(([id, s]) => (
        <Card withBorder radius="md" padding="md" key={id}>
          <Stack gap="xs">
            <Group justify="space-between">
              <Text size="sm" fw={600}>
                {s.apn ?? "APN 未关联"}
              </Text>
              {id === qos.selected && (
                <Badge size="xs" variant="light">
                  主卡片
                </Badge>
              )}
            </Group>
            <Text size="xs" c="dimmed">
              {id === qos.selected
                ? "上网业务"
                : isDataSession(s)
                  ? "数据业务"
                  : s.apn?.split(".")[0] === "ims"
                    ? "IMS 语音 / 信令"
                    : s.apn === null
                      ? "业务未确认"
                      : "其他业务"}
              {qos.activeFamily && !id.startsWith(qos.activeFamily + ":")
                ? " · 非当前核心网络会话"
                : ""}
            </Text>
            <DetailRows
              rows={[
                [
                  id.startsWith("LTE:") ? "EPS 承载 ID" : "PDU 会话 ID",
                  id.split(":")[1],
                ],
                ...(s.linkedBearer
                  ? [["关联默认承载", s.linkedBearer.split(":")[1]]]
                  : []),
                [s.qiLabel, s.qci ?? "—"],
                ...(s.mappedEpsBearerId == null
                  ? []
                  : [["EPS 映射承载", s.mappedEpsBearerId]]),
                ["AMBR 下行", s.ambr?.dl ?? "—"],
                ["AMBR 上行", s.ambr?.ul ?? "—"],
                ["来源", s.source],
              ]}
            />
            {s.apn === null && s.ambr && (
              <Text size="xs" c="dimmed">
                仅捕获会话更新；需捕获完整建立消息以确认 APN / QoS。
              </Text>
            )}
            {Object.values(s.flows).length > 0 && (
              <DetailRows
                rows={Object.values(s.flows)
                  .sort((a, b) => a.qfi - b.qfi)
                  .map((f) => [
                    `QFI ${f.qfi}`,
                    `${f.qi == null ? "QoS 等级未上报" : `5QI ${f.qi}`}${f.epsBearerId == null ? "" : ` · EPS ${f.epsBearerId}`}`,
                  ])}
              />
            )}
          </Stack>
        </Card>
      ))}
    </Stack>
  );
}
function PowerContent({ cards }) {
  return (
    <Stack gap="md">
      {[
        ["PUSCH", cards.pusch],
        ["PUCCH", cards.pucch],
      ].map(([channel, r]) => (
        <Card key={channel} withBorder radius="md" padding="md">
          <Text fw={600} size="sm" mb={10}>
            {channel}
          </Text>
          {r ? (
            <>
              <Text fz={22} fw={600} mb={10}>
                {r.dbm} dBm
              </Text>
              <DetailRows
                rows={[
                  ["来源", r.source],
                  ...r.details,
                  ["历史", r.history.join(" → ") + " dBm"],
                  ["更新时间", when(r.updatedAt)],
                ]}
              />
            </>
          ) : (
            <Text size="xs" c="dimmed">
              {cards.radio === "NR"
                ? `等待 ${channel} 发射与功控记录；上行有流量时更新`
                : `尚未收到 ${channel} 功率记录`}
            </Text>
          )}
        </Card>
      ))}
    </Stack>
  );
}
function NeighborsContent({ cards }) {
  return (
    <Stack gap="md">
      <Text size="xs" c="dimmed">
        共 {cards.neighbors.length} 个已捕获邻区，不含当前服务小区。最近测量{" "}
        {when(cards.measurementSnapshot?.updatedAt)}。
      </Text>
      <Text size="xs" c="dimmed">
        同模3仅为线索，不能据此判定干扰。历史邻区不代表同时测得。
      </Text>
      {cards.neighbors.length ? (
        <Accordion variant="contained" radius="md">
          {cards.neighbors
            .slice()
            .sort((a, b) => a.pci - b.pci || (a.arfcn ?? -1) - (b.arfcn ?? -1))
            .map((c) => (
              <Accordion.Item
                key={`${c.pci}:${c.arfcn}`}
                value={`${c.pci}:${c.arfcn}`}
              >
                <Accordion.Control>
                  <Group justify="space-between">
                    <Text size="sm" fw={550}>
                      PCI {c.pci}
                    </Text>
                    <Text size="xs" c="dimmed">
                      {c.arfcn ?? "频道未关联"} ·{" "}
                      {c.rsrp == null ? "RSRP 未上报" : `${c.rsrp} dBm`}
                    </Text>
                  </Group>
                </Accordion.Control>
                <Accordion.Panel>
                  <Stack gap="md">
                    <DetailRows
                      rows={neighborRows(cards, c).filter(
                        ([k]) => !isBeamRow(k),
                      )}
                    />
                    <BeamTable beams={c.beams} />
                  </Stack>
                </Accordion.Panel>
              </Accordion.Item>
            ))}
        </Accordion>
      ) : (
        <Text size="sm" c="dimmed">
          等待邻区测量上报
        </Text>
      )}
    </Stack>
  );
}
export function SignalDetails({ store, kind }) {
  const snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot),
    c = snapshot.cards;
  return (
    <ScrollArea.Autosize mah="65vh" offsetScrollbars>
      {kind === "serving" ? (
        <ServingContent cards={c} state={snapshot.state} />
      ) : kind === "neighbors" ? (
        <NeighborsContent cards={c} />
      ) : kind === "engineering" ? (
        <EngineeringContent groups={c.engineering} />
      ) : kind === "qos" ? (
        <QosContent cards={c} />
      ) : kind === "link" ? (
        <DetailRows rows={linkRows(c)} />
      ) : kind === "power" ? (
        <PowerContent cards={c} />
      ) : kind === "vendor" ? (
        <Stack>
          <Text fw={600}>{c.vendor ?? "未知"}</Text>
          <Text size="sm" style={{ whiteSpace: "pre-wrap" }}>
            {c.vendorDetail ??
              (c.radio === "LTE"
                ? "等待 LTE SIB2 的 deltaFList-PUCCH"
                : "等待 NR SIB1 的 IMS / p0 证据")}
          </Text>
        </Stack>
      ) : kind === "ta" ? (
        c.timingAdvance ? (
          <Stack>
            <Text fz={22} fw={600}>
              约 {c.timingAdvance.estimate.meters.toFixed(0)} 米
            </Text>
            <DetailRows
              rows={[
                ["制式", c.timingAdvance.radio],
                ["绝对 RAR TA", c.timingAdvance.command],
                ...(c.timingAdvance.radio === "NR"
                  ? [["子载波间隔", `${15 << c.timingAdvance.numerology} kHz`]]
                  : []),
                ["来源", c.timingAdvance.source],
                ["采集时间", when(c.timingAdvance.updatedAt)],
                [
                  "距离步长",
                  `约 ${c.timingAdvance.estimate.commandStepMeters.toFixed(0)} 米`,
                ],
              ]}
            />
            <Text size="xs" c="dimmed">
              绝对 RAR TA 已排除双工固定偏移。此为传播距离估算，非地图直线距离。
            </Text>
          </Stack>
        ) : (
          <Stack>
            <Text size="sm">等待绝对 TA 上报</Text>
            <Text size="xs" c="dimmed">
              入网或重连后自动计算，无需输入。
            </Text>
          </Stack>
        )
      ) : (
        <DetailRows
          rows={
            c.security
              ? [
                  ["加密算法", c.security.cipher],
                  ["完整性算法", c.security.integrity],
                ]
              : [["安全算法", "等待安全模式消息"]]
          }
        />
      )}
    </ScrollArea.Autosize>
  );
}
export function SignalCards({
  store,
  detailStore = store,
  onRedial,
  networkBusy,
}) {
  const { ask } = useServices(),
    snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot),
    c = snapshot.cards,
    selected = dataSession(c.bearerQos),
    rail = highSpeedRail(c.radio, c.engineering),
    count = Object.values(c.engineering)
      .flat()
      .reduce((n, s) => n + s.rows.length, 0);
  function open(kind, title) {
    void ask({
      title,
      overlayBlur: 0,
      confirm: null,
      cancel: "关闭",
      wide: ["serving", "engineering", "neighbors"].includes(kind),
      body: <SignalDetails store={detailStore} kind={kind} />,
    });
  }
  const cards = [
    [
      "serving",
      "服务小区",
      c.cell ? `PCI ${c.cell.pci}` : "—",
      c.cell
        ? `${c.cell.band} · ${c.cell.freq ? c.cell.freq + " MHz" : c.cell.arfcn}`
        : "等待服务小区",
      Radio,
    ],
    [
      "operator",
      "运营商",
      c.plmn?.name ?? "—",
      c.plmn?.tac
        ? `TAC ${c.plmn.tac}`
        : c.plmn
          ? `${c.plmn.mcc}-${c.plmn.mnc}`
          : "等待所选 PLMN",
      Network,
    ],
    [
      "vendor",
      "基站厂商",
      c.vendor ?? "—",
      c.vendor
        ? "指纹推断 · 查看证据"
        : c.radio === "LTE"
          ? "等待 SIB2 指纹"
          : "等待 SIB1 指纹",
      Building2,
    ],
    [
      "qos",
      "AMBR / QCI",
      selected ? `${selected.qiLabel} ${c.qci ?? "待上报"}` : "等待上网承载",
      c.ambr ? `↓ ${c.ambr.dl}  ↑ ${c.ambr.ul}` : "↓ —   ↑ —",
      Network,
    ],
    [
      "security",
      "安全算法",
      c.security
        ? `${c.security.cipher ?? "?"} / ${c.security.integrity ?? "?"}`
        : "—",
      "加密 / 完整性",
      Shield,
    ],
    [
      "neighbors",
      `邻区 ${c.neighbors.length}`,
      c.neighbors
        .slice(0, 8)
        .map((n) => n.pci)
        .join(", ") || "—",
      "完整邻区、SSB 与模三线索",
      Antenna,
    ],
    [
      "link",
      "BLER / MCS",
      c.bler ?? "—",
      `MCS ${c.mcs ?? "—"}${c.modulation ? " · " + c.modulation : ""}`,
      Activity,
    ],
    [
      "engineering",
      "信令分析",
      count ? `${count} 项广播配置` : "等待 SIB",
      Object.keys(c.engineering)
        .filter((k) => c.engineering[k].length)
        .sort()
        .join(" / ") || "接入 / 重选 / 公共信道",
      Building2,
    ],
    ["power", "上行功率", c.txPower ?? "—", "PUSCH 数据上行", ArrowUp],
  ];
  return (
    <Panel
      title="小区信息卡片"
      icon={Radio}
      collapsible
      right={
        <Group gap="xs">
          <Button
            size="compact-xs"
            variant="subtle"
            disabled={networkBusy || !snapshot.running || snapshot.replay}
            onClick={onRedial}
          >
            重新拨号
          </Button>
          <Button
            size="compact-xs"
            variant="subtle"
            onClick={() => open("ta", "基站距离估算")}
          >
            TA 距离
          </Button>
        </Group>
      }
    >
      <Stack gap="sm">
        <SimpleGrid cols={{ base: 2, md: 3 }} spacing="sm">
          {cards.map(([kind, title, value, sub, Icon]) => (
            <Card
              component="button"
              type="button"
              key={kind}
              withBorder
              radius="md"
              padding="md"
              className="signal-metric-card"
              onClick={() =>
                open(kind === "operator" ? "serving" : kind, title)
              }
            >
              <Group gap={6}>
                <Icon size={13} strokeWidth={1.6} />
                <Text fz={10} c="dimmed">
                  {title}
                </Text>
              </Group>
              <Text
                fz={kind === "qos" ? 14 : 16}
                fw={600}
                mt={10}
                mb={5}
                lineClamp={2}
                className="value"
              >
                {value}
              </Text>
              <Text size="xs" c="dimmed" lineClamp={2}>
                {sub}
              </Text>
              {kind === "serving" && rail && (
                <Badge mt={5} size="xs" variant="light">
                  高铁专网
                </Badge>
              )}
            </Card>
          ))}
        </SimpleGrid>
        <Text fz={10} c="dimmed">
          {snapshot.replay
            ? "回放数据"
            : snapshot.running
              ? "当前抓取"
              : "已停止 · 最近记录"}{" "}
          · 卡片更新 {when(c.updatedAt)}
        </Text>
      </Stack>
    </Panel>
  );
}
