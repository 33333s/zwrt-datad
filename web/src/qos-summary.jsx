import React, { useSyncExternalStore } from "react";
import { Badge, Group, Text } from "@mantine/core";
import { Network } from "lucide-react";
import { InfoGrid, Panel, useServices } from "./ui.jsx";
export function QosSummary({ state }) {
  const { signalCache, navigate } = useServices(),
    cached = useSyncExternalStore(
      signalCache.subscribe,
      signalCache.getSnapshot,
    ),
    q = state?.qos,
    live = !!q?.available,
    hasCache = !!cached?.qosApn;
  if (!live && !hasCache) return null;
  return (
    <Panel
      title="上网承载 QoS"
      icon={Network}
      right={
        <Badge variant="light" size="xs">
          {live ? "设备状态" : "最近信令"}
        </Badge>
      }
    >
      <InfoGrid
        items={[
          [
            live ? "QCI" : (cached.qiLabel ?? "QCI"),
            live ? (q.qci > 0 ? q.qci : null) : cached.qci,
          ],
          ["AMBR 下行", live ? q.ambr_dl : cached.ambr?.dl],
          ["AMBR 上行", live ? q.ambr_ul : cached.ambr?.ul],
        ]}
      />
      {!live && cached.updatedAt > 0 && (
        <Group justify="space-between" mt="md">
          <Text size="xs" c="dimmed">
            最近采集 {new Date(cached.updatedAt).toLocaleString()} ·
            进入信令页可重新捕获
          </Text>
          <Text
            component="button"
            className="inline-action"
            size="xs"
            c="blue"
            onClick={() => navigate("signal")}
          >
            查看信令
          </Text>
        </Group>
      )}
    </Panel>
  );
}
