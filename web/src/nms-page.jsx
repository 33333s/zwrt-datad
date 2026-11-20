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
  Switch,
  Text,
  Textarea,
  TextInput,
} from "@mantine/core";
import { Globe, Plus, Trash2 } from "lucide-react";
import {
  nmsEnrollment,
  nmsPayload,
  validateWebServices,
  lines,
} from "../management-models.js";
import { changed } from "../settings-models.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { Panel, useServices } from "./ui.jsx";
const EMPTY = {
  enabled: false,
  broker: "",
  platform_url: "",
  remote_origins: [],
  username: "",
  ca_pem: "",
  vendor: "",
  model: "",
  identity_type: "uuid",
  identity: "",
  platform: "generic",
  report_interval_seconds: 30,
  remote_enabled: false,
  remote_panel_control_enabled: false,
  services: [],
};
const NMS_STATES = {
  disabled: "未启用",
  starting: "连接中",
  connecting: "连接中",
  reconfiguring: "正在应用配置",
  connected: "MQTT 已连接",
  retrying: "正在重试",
  error: "连接异常",
};
export function NmsPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/cloud/app/config")),
    status = useResource(() => api.request("/cloud/app/status"), [], 5000),
    [form, setForm] = useState(null),
    [initial, setInitial] = useState(null),
    [quick, setQuick] = useState({ opened: false, code: "" }),
    [service, setService] = useState({
      opened: false,
      index: -1,
      name: "",
      port: "",
    });
  useEffect(() => {
    if (resource.data) {
      const c = { ...EMPTY, ...resource.data.config },
        f = {
          config: c,
          password: "",
          clearPassword: false,
          origins: c.remote_origins.join("\n"),
        };
      setForm(f);
      setInitial(f);
    }
  }, [resource.data]);
  const dirty = !!form && changed(form, initial);
  useDirtyGuard(
    dirty ||
      (quick.opened && !!quick.code) ||
      (service.opened && (!!service.name || !!service.port)),
  );
  const c = form?.config;
  function set(key, value) {
    setForm((f) => ({ ...f, config: { ...f.config, [key]: value } }));
  }
  async function save() {
    await op.run(
      async () => {
        const config = { ...c, remote_origins: lines(form.origins) },
          payload = nmsPayload(config, {
            password: form.password,
            clearPassword: form.clearPassword,
            passwordConfigured: resource.data.password_configured,
          });
        const result = await api.request("/cloud/app/config", {
          method: "POST",
          body: payload,
        });
        if (!result.config) throw new Error("配置提交后未收到有效回读");
        resource.setData(result);
        await status.refresh();
      },
      { write: true, message: "配置已保存，正在更新云端连接" },
    );
  }
  async function enroll() {
    await op.run(
      async () => {
        if (dirty) throw new Error("请先保存页面修改，再快速接入");
        const credentials = nmsEnrollment(quick.code);
        const result = await api.request("/cloud/app/quick-connect", {
          method: "POST",
          body: credentials,
        });
        if (!result.config) throw new Error("设备未确认接入配置");
        resource.setData(result);
        setQuick({ opened: false, code: "" });
        await status.refresh();
      },
      { write: true, message: "接入配置已保存，正在等待云端连接" },
    );
  }
  async function closeQuick() {
    if (op.busy) return;
    if (
      quick.code &&
      !(await ask({
        title: "放弃接入码？",
        body: <Text size="sm">接入码尚未提交。</Text>,
        confirm: "放弃",
      }))
    )
      return;
    setQuick((q) => ({ ...q, opened: false }));
  }
  function addService() {
    void op.run(() => {
      const list = c.services.slice(),
        value = {
          name: service.name.trim(),
          port: Number(service.port),
          kind: "web",
        };
      if (service.index < 0) list.push(value);
      else list[service.index] = value;
      validateWebServices(list);
      set("services", list);
      setService((s) => ({ ...s, opened: false }));
    });
  }
  const field = (key, label, props = {}) => (
    <TextInput
      label={label}
      value={String(c?.[key] ?? "")}
      disabled={op.busy}
      onChange={(e) => set(key, e.target.value)}
      {...props}
    />
  );
  const sw = (key, label) => (
    <Switch
      label={label}
      checked={!!c?.[key]}
      disabled={op.busy}
      onChange={(e) => set(key, e.target.checked)}
    />
  );
  return (
    <Stack>
      <LoadState resource={resource}>
        {c && (
          <>
            <Panel
              title="NMS 远程管理"
              icon={Globe}
              right={
                <Group>
                  <Badge
                    color={
                      status.data?.state === "connected" ? "green" : "gray"
                    }
                    variant="light"
                  >
                    {NMS_STATES[status.data?.state] || "等待状态"}
                  </Badge>
                  <RefreshButton
                    resource={resource}
                    disabled={op.busy || dirty}
                  />
                </Group>
              }
            >
              <Stack>
                {status.error && <Alert color="orange">{status.error}</Alert>}
                {status.data?.error && (
                  <Text size="xs" c="red">
                    {status.data.error}
                  </Text>
                )}
                {Number(status.data?.last_report_at) > 0 && (
                  <Text size="xs" c="dimmed">
                    最近上报{" "}
                    {new Date(
                      Number(status.data.last_report_at) * 1000,
                    ).toLocaleString()}
                  </Text>
                )}
                {!resource.data.config?.enabled && (
                  <Button
                    variant="light"
                    disabled={op.busy || dirty}
                    onClick={() => setQuick({ opened: true, code: "" })}
                  >
                    快速接入 · Base64 接入码
                  </Button>
                )}
                <SimpleGrid cols={{ base: 1, sm: 3 }}>
                  {sw("enabled", "启用 NMS")}
                  {sw("remote_enabled", "远程网页访问")}
                  {sw("remote_panel_control_enabled", "面板控制")}
                </SimpleGrid>
              </Stack>
            </Panel>
            <Panel title="服务器与账户">
              <Stack>
                <SimpleGrid cols={{ base: 1, sm: 2 }}>
                  {field("platform_url", "平台地址 · HTTPS")}
                  {field("broker", "MQTT 地址 · WSS / SSL")}
                  {field("username", "MQTT 用户名", { autoComplete: "off" })}
                  <PasswordInput
                    label={
                      resource.data.password_configured
                        ? "新密码 · 留空保留"
                        : "MQTT 密码"
                    }
                    value={form.password}
                    disabled={op.busy || form.clearPassword}
                    autoComplete="new-password"
                    onChange={(e) =>
                      setForm((f) => ({ ...f, password: e.target.value }))
                    }
                  />
                </SimpleGrid>
                {resource.data.password_configured && (
                  <Switch
                    label="清除已保存密码"
                    checked={form.clearPassword}
                    disabled={op.busy}
                    onChange={(e) => {
                      const clearPassword = e.target.checked;
                      setForm((f) => ({ ...f, clearPassword }));
                    }}
                  />
                )}
                {field("report_interval_seconds", "上报间隔 · 10–3600 秒", {
                  inputMode: "numeric",
                })}
              </Stack>
            </Panel>
            <Panel
              title="网页后台"
              right={
                <Button
                  size="xs"
                  variant="light"
                  leftSection={<Plus size={13} />}
                  disabled={op.busy || c.services.length >= 8}
                  onClick={() =>
                    setService({ opened: true, index: -1, name: "", port: "" })
                  }
                >
                  添加后台
                </Button>
              }
            >
              <Stack>
                {c.services.map((s, i) =>
                  s.kind === "web" ? (
                    <Card withBorder radius="md" padding="sm" key={i}>
                      <Group justify="space-between">
                        <Text size="sm">
                          {s.name} · {s.port}
                        </Text>
                        <Group gap="xs">
                          <Button
                            size="xs"
                            variant="subtle"
                            disabled={op.busy}
                            onClick={() =>
                              setService({
                                opened: true,
                                index: i,
                                name: s.name,
                                port: String(s.port),
                              })
                            }
                          >
                            编辑
                          </Button>
                          <Button
                            size="xs"
                            variant="subtle"
                            color="red"
                            disabled={op.busy}
                            onClick={() =>
                              void ask({
                                title: "移除此网页后台？",
                                body: (
                                  <Text size="sm">
                                    保存页面后生效。已有远程终端条目保持原状。
                                  </Text>
                                ),
                              }).then((ok) => {
                                if (ok)
                                  set(
                                    "services",
                                    c.services.filter(
                                      (_, index) => index !== i,
                                    ),
                                  );
                              })
                            }
                          >
                            移除
                          </Button>
                        </Group>
                      </Group>
                    </Card>
                  ) : null,
                )}
                {!c.services.some((s) => s.kind === "web") && (
                  <Text size="sm" c="dimmed">
                    暂无网页后台
                  </Text>
                )}
              </Stack>
            </Panel>
            <Panel title="设备身份" collapsible>
              <Stack>
                <SimpleGrid cols={{ base: 1, sm: 3 }}>
                  {field("vendor", "厂商")}
                  {field("model", "型号")}
                  {field("identity", "稳定设备标识")}
                </SimpleGrid>
                <SimpleGrid cols={2}>
                  <Select
                    label="标识类型"
                    value={c.identity_type}
                    data={[
                      { value: "uuid", label: "UUID" },
                      { value: "sn", label: "序列号" },
                    ]}
                    disabled={op.busy}
                    onChange={(v) => set("identity_type", v)}
                    allowDeselect={false}
                  />
                  <Select
                    label="固件平台"
                    value={c.platform}
                    data={[
                      { value: "generic", label: "通用" },
                      { value: "qualcomm", label: "高通" },
                      { value: "mediatek", label: "联发科" },
                      { value: "quecopen", label: "QuecOpen" },
                    ]}
                    disabled={op.busy}
                    onChange={(v) => set("platform", v)}
                    allowDeselect={false}
                  />
                </SimpleGrid>
                <Text size="xs" c="dimmed">
                  已绑定的设备请保留原标识。
                </Text>
              </Stack>
            </Panel>
            <Panel title="证书与备用地址" collapsible defaultOpen={false}>
              <Stack>
                <Textarea
                  label="自定义 CA · PEM，留空使用系统证书"
                  value={c.ca_pem}
                  autosize
                  minRows={3}
                  maxRows={10}
                  disabled={op.busy}
                  onChange={(e) => set("ca_pem", e.target.value)}
                />
                <Textarea
                  label="备用 HTTPS 地址 · 每行一个，最多 4 个"
                  value={form.origins}
                  autosize
                  minRows={3}
                  disabled={op.busy}
                  onChange={(e) =>
                    setForm((f) => ({ ...f, origins: e.target.value }))
                  }
                />
              </Stack>
            </Panel>
            <Group justify="flex-end">
              <Button loading={op.busy} disabled={!dirty} onClick={save}>
                保存配置
              </Button>
            </Group>
          </>
        )}
      </LoadState>
      <Modal
        opened={quick.opened}
        onClose={closeQuick}
        title="NMS 快速接入"
        centered
        closeOnClickOutside={!op.busy}
      >
        <Stack>
          <Text size="sm" c="dimmed">
            接入码包含凭据，仅导入用户名和密码。
          </Text>
          <PasswordInput
            label="Base64 接入码"
            value={quick.code}
            autoComplete="off"
            disabled={op.busy}
            onChange={(e) => setQuick((q) => ({ ...q, code: e.target.value }))}
          />
          <Group justify="flex-end">
            <Button variant="default" disabled={op.busy} onClick={closeQuick}>
              取消
            </Button>
            <Button loading={op.busy} disabled={!quick.code} onClick={enroll}>
              接入
            </Button>
          </Group>
        </Stack>
      </Modal>
      <Modal
        opened={service.opened}
        onClose={() => setService((s) => ({ ...s, opened: false }))}
        title="网页后台"
        centered
      >
        <Stack>
          <TextInput
            label="名称"
            value={service.name}
            onChange={(e) =>
              setService((s) => ({ ...s, name: e.target.value }))
            }
          />
          <TextInput
            label="端口 · 1–65535，不能使用 9460/9461"
            value={service.port}
            inputMode="numeric"
            onChange={(e) =>
              setService((s) => ({ ...s, port: e.target.value }))
            }
          />
          <Group justify="flex-end">
            <Button
              variant="default"
              onClick={() => setService((s) => ({ ...s, opened: false }))}
            >
              取消
            </Button>
            <Button onClick={addService}>确定</Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
