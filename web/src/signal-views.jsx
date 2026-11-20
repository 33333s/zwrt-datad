import { copyText } from "./clipboard.js";
import React, { useMemo, useState } from "react";
import { Box, Button, Group, Stack, Text } from "@mantine/core";
import { Virtuoso } from "react-virtuoso";
import { Copy, Download } from "lucide-react";
import { download } from "../core.js";
import { ndjsonRecord } from "../signal.js";
import { useServices } from "./ui.jsx";
export function RrcTree({ value }) {
  const [expanded, setExpanded] = useState(
    new Set(["", "/message", "/message/c1"]),
  );
  const rows = useMemo(() => {
    const out = [];
    function visit(node, path, name, depth) {
      const branch = node !== null && typeof node === "object";
      if (path) out.push({ node, path, name, depth, branch });
      if (!branch || !expanded.has(path)) return;
      for (const [key, v] of Object.entries(node))
        visit(
          v,
          path + "/" + key,
          Array.isArray(node) ? `[${key}]` : key,
          depth + 1,
        );
    }
    visit(value, "", "RRC", -1);
    return out;
  }, [value, expanded]);
  return (
    <Virtuoso
      style={{ height: 390 }}
      data={rows}
      itemContent={(_, r) => (
        <Box
          component="button"
          type="button"
          className="rrc-tree-row"
          style={{ paddingLeft: Math.min(r.depth, 8) * 11 + 8 }}
          onClick={() => {
            if (r.branch)
              setExpanded((old) => {
                const next = new Set(old);
                if (next.has(r.path)) next.delete(r.path);
                else next.add(r.path);
                return next;
              });
          }}
        >
          <Text
            size="xs"
            c={r.branch ? "blue" : undefined}
            style={{
              fontFamily: "Consolas,monospace",
              whiteSpace: "pre-wrap",
              overflowWrap: "anywhere",
              textAlign: "left",
              userSelect: "text",
            }}
          >
            {r.branch
              ? `${expanded.has(r.path) ? "▾" : "▸"} ${r.name} (${Object.keys(r.node).length})`
              : `${r.name}: ${JSON.stringify(r.node)}`}
          </Text>
        </Box>
      )}
    />
  );
}
export function NdjsonView({ event, observedAt }) {
  const { notify } = useServices(),
    text = useMemo(
      () => JSON.stringify(ndjsonRecord(event, observedAt), null, 2),
      [event, observedAt],
    ),
    rows = useMemo(
      () =>
        text
          .split("\n")
          .flatMap((line) => (line.length ? line.match(/.{1,160}/g) : [""])),
      [text],
    );
  return (
    <Stack gap="sm">
      <Group justify="space-between">
        <Text size="xs" c="dimmed">
          {text.length.toLocaleString()} 字符 · 完整记录
        </Text>
        <Group gap="xs">
          <Button
            variant="subtle"
            size="xs"
            leftSection={<Copy size={12} />}
            onClick={() =>
              void copyText(text)
                .then(() => notify("完整记录已复制"))
                .catch(() => notify("浏览器未允许复制，请下载完整记录", true))
            }
          >
            复制全部
          </Button>
          <Button
            variant="subtle"
            size="xs"
            leftSection={<Download size={12} />}
            onClick={() =>
              download(
                JSON.stringify(ndjsonRecord(event, observedAt)) + "\n",
                `signal-${event.code}-${Date.now()}.ndjson`,
                "application/x-ndjson",
              )
            }
          >
            下载
          </Button>
        </Group>
      </Group>
      <Virtuoso
        style={{ height: 390 }}
        data={rows}
        itemContent={(_, line) => (
          <Text
            fz={11}
            style={{
              fontFamily: "Consolas,monospace",
              whiteSpace: "pre-wrap",
              overflowWrap: "anywhere",
              userSelect: "text",
              lineHeight: 1.6,
            }}
          >
            {line || " "}
          </Text>
        )}
      />
    </Stack>
  );
}
