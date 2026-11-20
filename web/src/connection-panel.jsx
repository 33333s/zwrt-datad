import React, { useState } from "react";
import {
  Alert,
  Button,
  Group,
  Modal,
  Stack,
  Text,
  TextInput,
} from "@mantine/core";
import { createLoginPreferences } from "../login-preferences.js";
import { Radio } from "lucide-react";
import {
  deviceProfiles,
  forgetDeviceProfile,
  managementUrl,
  saveDeviceProfile,
} from "../device-profiles.js";
import { useServices, InfoGrid, Panel } from "./ui.jsx";

export function ConnectionPanel() {
  const {
      api,
      connection,
      ask,
      logout,
      renameConnection,
      signalCache,
      notify,
    } = useServices(),
    current = new URL("ui/", api.base).href;
  const [editor, setEditor] = useState({
      opened: false,
      kind: "name",
      value: "",
    }),
    [error, setError] = useState(""),
    [profiles, setProfiles] = useState(deviceProfiles);
  function edit(kind) {
    setError("");
    setEditor({
      opened: true,
      kind,
      value: kind === "name" ? (connection.displayName ?? "") : current,
    });
  }
  async function switchTo(input) {
    setError("");
    try {
      const target = managementUrl(input);
      if (target === current) {
        setEditor((e) => ({ ...e, opened: false }));
        return;
      }
      if (
        !(await ask({
          title: "切换管理设备？",
          body: (
            <Text size="sm" style={{ overflowWrap: "anywhere" }}>
              {target}。将结束当前会话，在目标设备重新登录。
            </Text>
          ),
        }))
      )
        return;
      logout();
      location.assign(target);
    } catch (e) {
      setError(e.message);
    }
  }
  function saveName() {
    const next = saveDeviceProfile(current, editor.value);
    setProfiles(next);
    renameConnection(editor.value.trim());
    setEditor((e) => ({ ...e, opened: false }));
  }
  async function forget() {
    if (
      await ask({
        title: "忘记此设备？",
        body: (
          <Text size="sm">
            删除此浏览器中的连接记录并退出登录；不修改设备配置。
          </Text>
        ),
        danger: true,
      })
    ) {
      const cleared = createLoginPreferences(api.base).clear();
      forgetDeviceProfile(current);
      signalCache.clear();
      logout();
      if (!cleared) notify("已退出登录，但浏览器未能清除保存的密码", true);
    }
  }
  return (
    <>
      <Panel title="当前设备" icon={Radio}>
        <Stack>
          <InfoGrid
            cols={2}
            items={[
              [
                "名称",
                connection.displayName ||
                  connection.snapshot.device?.market_name ||
                  connection.snapshot.device?.model_name,
              ],
              ["管理地址", current],
            ]}
          />
          <Group>
            <Button size="xs" variant="light" onClick={() => edit("name")}>
              编辑名称
            </Button>
            <Button size="xs" variant="default" onClick={() => edit("switch")}>
              切换设备
            </Button>
            <Button size="xs" variant="subtle" color="red" onClick={forget}>
              忘记设备
            </Button>
          </Group>
        </Stack>
      </Panel>
      <Modal
        opened={editor.opened}
        onClose={() => setEditor((e) => ({ ...e, opened: false }))}
        title={editor.kind === "name" ? "编辑连接名称" : "切换管理设备"}
        centered
      >
        <Stack>
          {error && <Alert color="red">{error}</Alert>}
          <TextInput
            label={
              editor.kind === "name"
                ? "名称 · 仅保存在当前浏览器"
                : "网页管理地址 · 包含端口和代理前缀"
            }
            value={editor.value}
            onChange={(e) =>
              setEditor((s) => ({ ...s, value: e.target.value }))
            }
            maxLength={editor.kind === "name" ? 80 : 2048}
          />
          {editor.kind === "switch" &&
            profiles
              .filter((p) => p.url !== current)
              .map((p) => (
                <Button
                  variant="subtle"
                  key={p.url}
                  onClick={() => switchTo(p.url)}
                >
                  {p.name || p.url}
                </Button>
              ))}
          <Group justify="flex-end">
            <Button
              variant="default"
              onClick={() => setEditor((e) => ({ ...e, opened: false }))}
            >
              取消
            </Button>
            <Button
              onClick={
                editor.kind === "name" ? saveName : () => switchTo(editor.value)
              }
            >
              {editor.kind === "name" ? "保存" : "前往登录"}
            </Button>
          </Group>
        </Stack>
      </Modal>
    </>
  );
}
