import React, { useEffect, useState } from "react";
import {
  Alert,
  Badge,
  Button,
  Card,
  Group,
  Modal,
  PasswordInput,
  Select,
  SimpleGrid,
  Stack,
  Text,
  TextInput,
} from "@mantine/core";
import { Check, Network, Plus } from "lucide-react";
import { apnPayload, apnRows, manualApn } from "../datad-models.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { Panel, useServices } from "./ui.jsx";

export function ApnPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    resource = useResource(() => api.control("apn.list")),
    op = useOperation();
  const [editor, setEditor] = useState({
      opened: false,
      id: null,
      name: "",
      apn: "",
      username: "",
      password: "",
      auth_mode: "0",
      pdp_type: "3",
    }),
    [initial, setInitial] = useState("");
  const dirty =
    editor.opened &&
    JSON.stringify({ ...editor, opened: undefined }) !== initial;
  useDirtyGuard(dirty);
  const view = resource.data,
    rows = view ? apnRows(view) : [],
    selected = view?.enabled?.profileId;
  function edit(row = null) {
    op.clear();
    const next = {
      opened: true,
      id: row?.profileId ?? null,
      name: row?.profilename ?? "",
      apn: row?.wanapn ?? "",
      username: "",
      password: "",
      auth_mode: row ? "-1" : "0",
      pdp_type: String(row?.pdpType ?? 3),
    };
    setEditor(next);
    setInitial(JSON.stringify({ ...next, opened: undefined }));
  }
  async function close() {
    if (op.busy) return;
    if (
      dirty &&
      !(await ask({
        title: "放弃未保存的 APN？",
        body: <Text size="sm">修改尚未提交。</Text>,
        confirm: "放弃",
      }))
    )
      return;
    setEditor((c) => ({ ...c, opened: false }));
  }
  async function mutate(action, row = null) {
    if (
      !(await ask({
        title:
          action === "apn.delete"
            ? "删除 APN？"
            : action === "apn.enable"
              ? "使用这个 APN？"
              : "恢复自动 APN？",
        body: (
          <Text size="sm">
            {action === "apn.delete"
              ? "此操作会删除手动档案。"
              : "设备可能重新注册或短暂断网。"}{" "}
            {row?.profilename || row?.wanapn}
          </Text>
        ),
        danger: action === "apn.delete",
      }))
    )
      return;
    await op.run(
      async () => {
        const current = await api.control("apn.list");
        if (current.writable === false) throw new Error("当前 APN 配置只读");
        const params = row
          ? { profile_id: manualApn(current, row.profileId) }
          : { mode: 0 };
        await api.control(action, params);
        const next = await api.control("apn.list");
        resource.setData(next);
        const confirmed =
          action === "apn.delete"
            ? !apnRows(next).some((r) => r.profileId === row.profileId)
            : action === "apn.enable"
              ? next.enabled?.profileId === row.profileId
              : String(next.mode?.apn_mode) === "0";
        return confirmed;
      },
      {
        write: true,
        message: (confirmed) =>
          confirmed
            ? "已保存并回读确认"
            : "修改已提交，回读尚未一致，请稍后刷新",
      },
    );
  }
  async function save() {
    await op.run(
      async () => {
        const current = await api.control("apn.list");
        if (current.writable === false) throw new Error("当前 APN 配置只读");
        const id = editor.id ? manualApn(current, editor.id) : undefined,
          payload = apnPayload(editor, id);
        if (
          !(current.auth_modes ?? [0, 1, 2, 3]).includes(
            payload.auth_mode ?? 0,
          ) &&
          payload.auth_mode !== undefined
        )
          throw new Error("当前设备不支持所选认证方式");
        await api.control(id ? "apn.modify" : "apn.add", payload);
        setEditor((c) => ({ ...c, opened: false }));
        const next = await api.control("apn.list");
        resource.setData(next);
        const priorIds = new Set(
          (current.manual?.apnListArray ?? []).map((r) => r.profileId),
        );
        return (next.manual?.apnListArray ?? []).some(
          (r) =>
            (id ? r.profileId === id : !priorIds.has(r.profileId)) &&
            r.profilename === payload.name &&
            r.wanapn === payload.apn &&
            Number(r.pdpType) === payload.pdp_type &&
            (payload.auth_mode === undefined ||
              Number(r.pppAuthMode) === payload.auth_mode),
        );
      },
      {
        write: true,
        message: (verified) =>
          verified
            ? "APN 已保存；新建档案不会自动启用"
            : "APN 修改已提交，回读尚未确认，请重新读取",
      },
    );
  }
  const field = (key, label, props = {}) => (
    <TextInput
      label={label}
      value={editor[key]}
      onChange={(e) => setEditor((c) => ({ ...c, [key]: e.target.value }))}
      disabled={op.busy}
      {...props}
    />
  );
  return (
    <Stack>
      <Panel
        title="APN 管理"
        icon={Network}
        right={
          <Group gap="xs">
            <RefreshButton
              resource={resource}
              disabled={op.busy || editor.opened}
            />
            <Button
              size="xs"
              leftSection={<Plus size={13} />}
              disabled={!view || view.writable === false || op.busy}
              onClick={() => edit()}
            >
              新建
            </Button>
          </Group>
        }
      >
        <LoadState resource={resource}>
          <Stack gap="md">
            <Group justify="space-between">
              <Group>
                <Badge variant="light">
                  {String(view?.mode?.apn_mode) === "1"
                    ? "手动模式"
                    : "自动模式"}
                </Badge>
                <Text size="xs" c="dimmed">
                  当前档案 {selected || "未上报"}
                </Text>
              </Group>
              <Button
                size="xs"
                variant="light"
                disabled={
                  op.busy ||
                  view?.writable === false ||
                  String(view?.mode?.apn_mode) === "0"
                }
                onClick={() => mutate("apn.set_mode")}
              >
                恢复自动
              </Button>
            </Group>
            {view?.writable === false && (
              <Alert color="orange">
                当前设备的 APN 配置只读，请检查后端版本和官方后台访问状态。
              </Alert>
            )}
            {!rows.length && (
              <Text c="dimmed" size="sm" py="lg">
                设备未返回 APN 档案
              </Text>
            )}
            <SimpleGrid cols={{ base: 1, md: 2 }}>
              {rows.map((row) => (
                <Card withBorder radius="md" padding="md" key={row.profileId}>
                  <Stack gap="sm">
                    <Group justify="space-between">
                      <Text fw={600} size="sm">
                        {row.profilename || row.wanapn || row.profileId}
                      </Text>
                      <Badge
                        size="xs"
                        color={selected === row.profileId ? "green" : "gray"}
                        leftSection={
                          selected === row.profileId ? (
                            <Check size={11} />
                          ) : null
                        }
                      >
                        {selected === row.profileId
                          ? "当前使用"
                          : row.automatic
                            ? "自动档案"
                            : "手动档案"}
                      </Badge>
                    </Group>
                    <Text size="sm">{row.wanapn || "APN 未上报"}</Text>
                    <Text size="xs" c="dimmed">
                      {{ 1: "IPv4", 2: "IPv6", 3: "IPv4 / IPv6" }[
                        row.pdpType
                      ] ?? "协议未上报"}{" "}
                      · {row.profileId}
                    </Text>
                    {!row.automatic && (
                      <Group gap="xs" mt={4}>
                        <Button
                          size="xs"
                          variant="light"
                          disabled={
                            op.busy ||
                            view.writable === false ||
                            selected === row.profileId
                          }
                          onClick={() => mutate("apn.enable", row)}
                        >
                          启用
                        </Button>
                        <Button
                          size="xs"
                          variant="default"
                          disabled={op.busy || view.writable === false}
                          onClick={() => edit(row)}
                        >
                          编辑
                        </Button>
                        <Button
                          size="xs"
                          color="red"
                          variant="subtle"
                          disabled={op.busy || view.writable === false}
                          onClick={() => mutate("apn.delete", row)}
                        >
                          删除
                        </Button>
                      </Group>
                    )}
                  </Stack>
                </Card>
              ))}
            </SimpleGrid>
          </Stack>
        </LoadState>
      </Panel>
      <Modal
        opened={editor.opened}
        onClose={close}
        title={editor.id ? "编辑 APN" : "新建 APN"}
        centered
        closeOnClickOutside={!op.busy}
      >
        <Stack>
          {field("name", "档案名称")}
          {field("apn", "APN", { required: true })}
          <SimpleGrid cols={{ base: 1, sm: 2 }}>
            <Select
              label="PDP 协议"
              value={editor.pdp_type}
              data={[
                { value: "1", label: "IPv4" },
                { value: "2", label: "IPv6" },
                { value: "3", label: "IPv4 / IPv6" },
              ]}
              allowDeselect={false}
              onChange={(v) => setEditor((c) => ({ ...c, pdp_type: v }))}
              disabled={op.busy}
            />
            <Select
              label="认证方式"
              value={editor.auth_mode}
              data={[
                ...(editor.id ? [{ value: "-1", label: "保留原设置" }] : []),
                ...(view?.auth_modes ?? [0, 1, 2, 3]).map((v) => ({
                  value: String(v),
                  label:
                    ["无认证", "PAP", "CHAP", "PAP / CHAP"][v] ?? String(v),
                })),
              ]}
              allowDeselect={false}
              onChange={(v) => setEditor((c) => ({ ...c, auth_mode: v }))}
              disabled={op.busy}
            />
          </SimpleGrid>
          {field("username", editor.id ? "用户名 · 留空保留" : "用户名", {
            autoComplete: "off",
          })}
          <PasswordInput
            label={editor.id ? "密码 · 留空保留" : "密码"}
            value={editor.password}
            onChange={(e) =>
              setEditor((c) => ({ ...c, password: e.target.value }))
            }
            disabled={op.busy}
            autoComplete="new-password"
          />
          <Group justify="flex-end">
            <Button variant="default" onClick={close} disabled={op.busy}>
              取消
            </Button>
            <Button
              loading={op.busy}
              disabled={!editor.apn.trim() || !dirty}
              onClick={save}
            >
              保存
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
