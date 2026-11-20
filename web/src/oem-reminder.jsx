import React, { useEffect, useRef, useState } from "react";
import { Alert, Button, Group, Modal, Stack, Text } from "@mantine/core";
import { usePageApi } from "./hooks.jsx";
import { useServices } from "./ui.jsx";

// One notice per authenticated connection. Device readback, not a local clock, ends it.
export function OemReminder() {
  const api = usePageApi(),
    { connection } = useServices(),
    controls = connection.capabilities?.controls ?? [];
  const [handled, setHandled] = useState(false),
    [status, setStatus] = useState(null),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
    lock = useRef(false),
    alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  useEffect(() => {
    if (
      handled ||
      !controls.includes("oem.access.status") ||
      !controls.includes("oem.access.set")
    )
      return;
    let disposed = false,
      timer;
    async function tick() {
      if (!document.hidden && !lock.current) {
        lock.current = true;
        try {
          const next = await api.control("oem.access.status");
          if (!disposed) {
            setStatus(next);
            if (!next.supported || !next.paused) setHandled(true);
            setError("");
          }
        } catch (e) {
          if (!disposed && e.name !== "AbortError")
            setError("暂时无法确认设备状态，可重试关闭避让，或忽略此提示。");
        } finally {
          lock.current = false;
        }
      }
      if (!disposed) timer = setTimeout(tick, 3000);
    }
    void tick();
    return () => {
      disposed = true;
      clearTimeout(timer);
    };
  }, [handled, api]);
  async function resume() {
    if (lock.current) return;
    lock.current = true;
    setBusy(true);
    setError("");
    try {
      const next = await api.control("oem.access.set", {
        paused: false,
        duration_seconds: 0,
      });
      if (!alive.current) return;
      setStatus(next);
      if (next.paused === false) setHandled(true);
      else setError("设备尚未关闭避让，请重新读取确认。");
    } catch (e) {
      if (alive.current && e.name !== "AbortError")
        setError("结果尚未确认，请等待状态回读后再操作。");
    } finally {
      lock.current = false;
      if (alive.current) setBusy(false);
    }
  }
  return (
    <Modal
      opened={!handled && status?.paused === true}
      onClose={() => {
        if (!busy) setHandled(true);
      }}
      title="当前处于避让模式"
      centered
      closeOnClickOutside={!busy}
      withCloseButton={!busy}
      zIndex={300}
    >
      <Stack>
        <Text size="sm" lh={1.8}>
          datad
          已暂停登录和访问官方后台。立即关闭后会恢复按需访问，可能使官方后台退出登录；忽略仅关闭本次提醒。
        </Text>
        <Text size="sm">
          {status?.remaining_seconds == null
            ? "手动恢复模式"
            : `设备返回剩余 ${status.remaining_seconds} 秒`}
        </Text>
        {error && <Alert color="orange">{error}</Alert>}
        <Group justify="flex-end">
          <Button
            variant="default"
            disabled={busy}
            onClick={() => setHandled(true)}
          >
            忽略
          </Button>
          <Button loading={busy} onClick={resume}>
            立即关闭避让
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}
