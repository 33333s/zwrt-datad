import React from "react";
import {
  Alert,
  Button,
  Center,
  Group,
  Loader,
  Stack,
  Text,
} from "@mantine/core";
import { RefreshCw } from "lucide-react";

export function LoadState({ resource, children }) {
  return (
    <>
      {resource.error && (
        <Alert color="red" role="alert">
          <Stack gap="sm">
            <Text size="sm">{resource.error}</Text>
            <Button variant="light" size="xs" onClick={resource.refresh}>
              重新读取
            </Button>
          </Stack>
        </Alert>
      )}
      {!resource.data && resource.loading ? (
        <Center py={45}>
          <Group gap="sm">
            <Loader size="sm" />
            <Text size="sm" c="dimmed">
              正在读取设备数据
            </Text>
          </Group>
        </Center>
      ) : resource.data ? (
        children
      ) : null}
    </>
  );
}
export function RefreshButton({ resource, disabled = false }) {
  return (
    <Button
      variant="default"
      size="xs"
      loading={resource.loading}
      disabled={disabled}
      leftSection={<RefreshCw size={13} />}
      onClick={resource.refresh}
    >
      刷新
    </Button>
  );
}
