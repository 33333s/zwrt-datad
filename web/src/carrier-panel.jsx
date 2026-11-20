import React from "react";
import { Alert, Badge, Button, Stack, Table, Text } from "@mantine/core";
import { Antenna } from "lucide-react";
import { carrierSnapshot } from "../carriers.js";
import { display } from "../core.js";
import { Panel, useServices } from "./ui.jsx";
const labels = {
  active: "已激活",
  inactive: "已停用",
  configured: "已配置",
  unknown: "激活未知",
};
function CarrierTable({ data }) {
  return (
    <Table.ScrollContainer minWidth={720}>
      <Table striped fz="xs">
        <Table.Thead>
          <Table.Tr>
            {[
              "角色",
              "制式 / 频段",
              "PCI",
              "频道",
              "带宽",
              "RSRP · dBm",
              "RSRQ · dB",
              "SINR · dB",
              "RSSI · dBm",
              "状态",
              "上行",
            ].map((k) => (
              <Table.Th key={k}>{k}</Table.Th>
            ))}
          </Table.Tr>
        </Table.Thead>
        <Table.Tbody>
          {data.carriers.map((c) => (
            <Table.Tr key={`${c.rat}:${c.role}`}>
              <Table.Td>
                <Badge size="xs" variant="light">
                  {c.role}
                </Badge>
              </Table.Td>
              <Table.Td>
                {c.rat} {c.band}
              </Table.Td>
              {[
                c.pci,
                c.channel,
                c.bandwidth,
                c.rsrp,
                c.rsrq,
                c.sinr,
                c.rssi,
                labels[c.activity],
                c.ulConfigured === null
                  ? "未知"
                  : c.ulConfigured
                    ? "已配置"
                    : "未配置",
              ].map((v, i) => (
                <Table.Td key={i}>{display(v)}</Table.Td>
              ))}
            </Table.Tr>
          ))}
        </Table.Tbody>
      </Table>
      {!data.carriers.length && (
        <Text c="dimmed" size="sm" ta="center" py="lg">
          当前未上报载波
        </Text>
      )}
    </Table.ScrollContainer>
  );
}
export function CarrierPanel({ state }) {
  const { ask } = useServices(),
    data = carrierSnapshot(state?.net, state?.device?.model_name);
  return (
    <Panel
      title="载波聚合"
      icon={Antenna}
      collapsible
      right={
        <Button
          variant="subtle"
          size="compact-xs"
          onClick={() =>
            void ask({
              title: "载波详情",
              wide: true,
              confirm: null,
              cancel: "关闭",
              body: (
                <Stack>
                  <Text size="sm">{data.summary}</Text>
                  {data.activation.map((x) => (
                    <Text size="xs" key={x}>
                      {x}
                    </Text>
                  ))}
                  <CarrierTable data={data} />
                </Stack>
              ),
            })
          }
        >
          {data.summary}
        </Button>
      }
    >
      <Stack gap="sm">
        {data.activation
          .filter((x) => x.includes("下行 CA"))
          .map((x) => (
            <Text c="dimmed" size="xs" key={x}>
              {x}
            </Text>
          ))}
        <CarrierTable data={data} />
        {data.unrecognized && (
          <Alert color="orange">
            附加载波格式待确认，已确认载波仍保留显示。
          </Alert>
        )}
      </Stack>
    </Panel>
  );
}
