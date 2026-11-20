import React from "react";
import { Card, Group, Text, Box } from "@mantine/core";
import { GitFork, ExternalLink } from "lucide-react";
export function GithubCard({ compact = false }) {
  return (
    <Card
      component="a"
      href="https://github.com/33333s/zwrt-datad"
      target="_blank"
      rel="noopener noreferrer"
      withBorder
      radius="md"
      padding={compact ? "sm" : "md"}
      className="github-card"
      aria-label="zwrt-datad 开源项目 · GitHub（新窗口打开）"
    >
      <Group gap={10} wrap="nowrap">
        <GitFork size={compact ? 18 : 21} aria-hidden="true" />
        <Box style={{ flex: 1, minWidth: 0 }}>
          <Text fz={12} fw={600}>
            zwrt-datad
          </Text>
          <Text fz={10} className="github-card-caption" mt={3}>
            开源项目 · GitHub
          </Text>
        </Box>
        <ExternalLink size={12} aria-hidden="true" />
      </Group>
    </Card>
  );
}
