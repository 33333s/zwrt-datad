import React, { useEffect, useState } from "react";
import {
  Alert,
  Badge,
  Button,
  Group,
  PasswordInput,
  Select,
  SimpleGrid,
  Stack,
  Switch,
  Text,
  TextInput,
} from "@mantine/core";
import { Wifi } from "lucide-react";
import { integer } from "../datad-models.js";
import { changed } from "../settings-models.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { Panel, useServices } from "./ui.jsx";
const SECURITY = [
  { value: "none", label: "开放网络" },
  { value: "psk2", label: "WPA2" },
  { value: "sae", label: "WPA3" },
  { value: "sae-mixed", label: "WPA2 / WPA3" },
];
const canonical = (s) =>
  s === "psk2+ccmp" ? "psk2" : s === "psk-mixed+tkip+ccmp" ? "psk-mixed" : s;
async function loadWifi(api) {
  const [state, wifi] = await Promise.all([
    api.request("/state"),
    api.control("wifi.status"),
  ]);
  return { state, wifi };
}
export function WifiPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => loadWifi(api));
  async function apply(action, params, verify) {
    if (
      !(await ask({
        title: "应用热点设置？",
        body: (
          <Text size="sm">
            热点可能重启，当前连接可能中断。请确认能重新连接设备。
          </Text>
        ),
      }))
    )
      return;
    await op.run(
      async () => {
        await api.control(action, params);
        const next = await loadWifi(api);
        resource.setData(next);
        return verify(next);
      },
      {
        write: true,
        message: (yes) =>
          yes
            ? "设备配置已回读确认，热点正在应用"
            : "修改已提交，回读未确认，请重新连接设备后刷新",
      },
    );
  }
  const state = resource.data?.state,
    wifi = resource.data?.wifi;
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title="热点控制"
          icon={Wifi}
          right={<RefreshButton resource={resource} disabled={op.busy} />}
        >
          <Stack>
            <Switch
              label="Wi-Fi 总开关"
              checked={state?.wlan?.enabled === 1}
              disabled={op.busy || state?.wlan?.enabled == null}
              onChange={(e) => {
                const enabled = e.target.checked;
                void apply(
                  "wifi.set_module",
                  { enabled: enabled ? 1 : 0 },
                  (r) => r.state.wlan?.enabled === (enabled ? 1 : 0),
                );
              }}
            />
            {state?.wlan?.band_steering_enabled != null && (
              <Switch
                label="双频合一"
                description="合并双频网络，由设备引导终端连接"
                checked={state.wlan.band_steering_enabled === 1}
                disabled={op.busy}
                onChange={(e) => {
                  const enabled = e.target.checked;
                  void apply(
                    "wifi.set_dual_band",
                    { enabled },
                    (r) =>
                      r.state.wlan?.band_steering_enabled === (enabled ? 1 : 0),
                  );
                }}
              />
            )}
          </Stack>
        </Panel>
        <SimpleGrid cols={{ base: 1, lg: 2 }}>
          {["main_2g", "main_5g"].map((section) =>
            wifi?.[section] ? (
              <WifiBand
                key={section}
                api={api}
                section={section}
                current={wifi[section]}
                moduleOn={state?.wlan?.enabled === 1}
                refresh={resource.refresh}
              />
            ) : (
              <Panel
                key={section}
                title={section === "main_2g" ? "2.4 GHz" : "5 GHz"}
              >
                <Text size="sm" c="dimmed">
                  设备未返回该频段配置
                </Text>
              </Panel>
            ),
          )}
        </SimpleGrid>
        {["guest_2g", "guest_5g"]
          .filter((section) => wifi?.[section])
          .map((section) => (
            <Panel
              key={section}
              title={`${section === "guest_2g" ? "2.4" : "5"} GHz 访客网络`}
            >
              <Group justify="space-between">
                <Stack gap={3}>
                  <Text size="sm">{wifi[section].ssid || "未设置 SSID"}</Text>
                  <Text size="xs" c="dimmed">
                    {wifi[section].encryption || "安全模式未上报"}
                  </Text>
                </Stack>
                <Switch
                  label="启用"
                  checked={
                    state?.wlan?.enabled === 1 &&
                    String(wifi[section].disabled) === "0"
                  }
                  disabled={op.busy || state?.wlan?.enabled !== 1}
                  onChange={(e) => {
                    const enabled = e.target.checked;
                    void apply(
                      "wifi.configure",
                      { section, enabled },
                      (r) =>
                        String(r.wifi[section]?.disabled) ===
                        (enabled ? "0" : "1"),
                    );
                  }}
                />
              </Group>
            </Panel>
          ))}
      </LoadState>
    </Stack>
  );
}
function WifiBand({ api, section, current, moduleOn, refresh }) {
  const { ask } = useServices(),
    op = useOperation(),
    [form, setForm] = useState(null),
    [initial, setInitial] = useState(null),
    five = section === "main_5g";
  useEffect(() => {
    if (form && changed(form, initial)) return;
    const f = {
      ssid: current.ssid ?? "",
      encryption: canonical(current.encryption ?? ""),
      key: "",
      hidden: String(current.hidden) === "1",
      maxassoc: String(current.maxassoc ?? ""),
      channel: String(current.channel || "0"),
    };
    setForm(f);
    setInitial(f);
  }, [current]);
  const dirty = !!form && changed(form, initial);
  useDirtyGuard(dirty);
  async function save(toggle) {
    await op.run(
      async () => {
        let params;
        if (toggle !== undefined) params = { section, enabled: toggle };
        else {
          if (
            !form.ssid.trim() ||
            new TextEncoder().encode(form.ssid.trim()).length > 32
          )
            throw new Error("SSID 须为 1–32 字节");
          if (
            form.key &&
            (new TextEncoder().encode(form.key).length < 8 ||
              new TextEncoder().encode(form.key).length > 63 ||
              /[\r\n]/.test(form.key))
          )
            throw new Error("密码须为 8–63 字节且不含换行");
          if (form.encryption !== "none" && !form.key && !current.key)
            throw new Error("请先填写热点密码");
          params = {
            section,
            ssid: form.ssid.trim(),
            encryption: form.encryption,
            hidden: form.hidden ? "1" : "0",
            maxassoc: String(integer(form.maxassoc, 1, 128, "最大客户端数")),
            channel: form.channel,
          };
          if (form.key) params.key = form.key;
        }
        if (
          !(await ask({
            title: `应用 ${five ? "5" : "2.4"} GHz 设置？`,
            body: (
              <Text size="sm">热点可能重启，请准备好以新配置重新连接。</Text>
            ),
          }))
        )
          return null;
        await api.control("wifi.configure", params);
        const next = await api.control("wifi.status"),
          read = next[section];
        const verified =
          toggle !== undefined
            ? String(read?.disabled) === (toggle ? "0" : "1")
            : read &&
              read.ssid === params.ssid &&
              canonical(read.encryption) === canonical(params.encryption) &&
              String(read.hidden) === params.hidden &&
              String(read.maxassoc) === params.maxassoc &&
              String(read.channel || "0") === params.channel;
        if (verified) {
          const nextForm = { ...form, key: "" };
          setForm(nextForm);
          setInitial(nextForm);
        }
        await refresh();
        return verified;
      },
      {
        write: true,
        message: (verified) =>
          verified === null
            ? ""
            : verified
              ? "配置已回读确认，热点正在应用"
              : "修改已提交，尚未回读一致，请重新连接后刷新",
      },
    );
  }
  const input = (key, label, props = {}) => (
    <TextInput
      label={label}
      value={form?.[key] ?? ""}
      onChange={(e) => setForm((f) => ({ ...f, [key]: e.target.value }))}
      disabled={op.busy}
      {...props}
    />
  );
  let security = SECURITY;
  if (form?.encryption && !security.some((x) => x.value === form.encryption))
    security = [
      ...security,
      { value: form.encryption, label: form.encryption },
    ];
  return (
    <Panel
      title={`${five ? "5" : "2.4"} GHz 主网络`}
      right={
        <Badge size="xs" variant="light">
          {moduleOn && String(current.disabled) === "0" ? "已开启" : "已关闭"}
        </Badge>
      }
    >
      {form && (
        <Stack>
          <Switch
            label="启用此频段"
            checked={moduleOn && String(current.disabled) === "0"}
            disabled={op.busy || !moduleOn}
            onChange={(e) => void save(e.target.checked)}
          />
          {input("ssid", "SSID")}
          <Select
            label="安全模式"
            data={security}
            value={form.encryption || null}
            onChange={(v) => setForm((f) => ({ ...f, encryption: v }))}
            allowDeselect={false}
            disabled={op.busy}
          />
          {form.encryption !== "none" && (
            <PasswordInput
              label="新密码 · 留空保留"
              autoComplete="new-password"
              value={form.key}
              onChange={(e) => setForm((f) => ({ ...f, key: e.target.value }))}
              disabled={op.busy}
            />
          )}
          <SimpleGrid cols={2}>
            <Select
              label="信道"
              value={form.channel}
              allowDeselect={false}
              disabled={op.busy}
              data={[
                { value: "0", label: "自动" },
                ...(five
                  ? [36, 40, 44, 48, 149, 153, 157, 161, 165]
                  : Array.from({ length: 13 }, (_, i) => i + 1)
                ).map((n) => ({ value: String(n), label: String(n) })),
              ]}
              onChange={(v) => setForm((f) => ({ ...f, channel: v }))}
            />
            {input("maxassoc", "最大客户端 · 1–128", { inputMode: "numeric" })}
          </SimpleGrid>
          <Switch
            label="隐藏 SSID"
            checked={form.hidden}
            onChange={(e) => {
              const hidden = e.target.checked;
              setForm((f) => ({ ...f, hidden }));
            }}
            disabled={op.busy}
          />
          <Group justify="flex-end">
            <Button
              variant="default"
              disabled={op.busy || !dirty}
              onClick={() => setForm(initial)}
            >
              取消修改
            </Button>
            <Button loading={op.busy} disabled={!dirty} onClick={() => save()}>
              保存此频段
            </Button>
          </Group>
        </Stack>
      )}
    </Panel>
  );
}
