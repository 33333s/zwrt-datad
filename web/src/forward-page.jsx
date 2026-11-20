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
  Textarea,
  TextInput,
} from "@mantine/core";
import { Mail, MessageSquare } from "lucide-react";
import { forwardPatch, lines } from "../management-models.js";
import { changed } from "../settings-models.js";
import { REASONS } from "../automation-models.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { Panel, useServices } from "./ui.jsx";
const defaults = {
  enabled: false,
  method: "webhook",
  webhook_url: "",
  dingtalk_webhook: "",
  dingtalk_secret_configured: false,
  sms_to_phone: [],
  smtp: {
    host: "",
    port: 465,
    username: "",
    to: "",
    password_configured: false,
  },
  power_forward_enabled: false,
  blacklist_phone: [],
  blacklist_keywords: [],
  nickname: "",
  smtp_forward_device_info: false,
  dingtalk_forward_device_info: false,
  sms_forward_device_info: false,
};
export function ForwardPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/sms/forward/config")),
    [form, setForm] = useState(null),
    [initial, setInitial] = useState(null),
    status = useResource(() => api.request("/sms/forward/status"), [], 5000);
  useEffect(() => {
    if (resource.data) {
      const c = {
          ...defaults,
          ...resource.data.config,
          smtp: { ...defaults.smtp, ...resource.data.config?.smtp },
        },
        f = {
          config: c,
          smtpPassword: "",
          secret: "",
          clearSmtp: false,
          clearSecret: false,
          phones: c.sms_to_phone.join("\n"),
          blocked: c.blacklist_phone.join("\n"),
          keywords: c.blacklist_keywords.join("\n"),
        };
      setForm(f);
      setInitial(f);
      if (resource.data.status) status.setData(resource.data.status);
    }
  }, [resource.data]);
  const dirty = !!form && changed(form, initial);
  useDirtyGuard(dirty);
  const c = form?.config,
    current = status.data ?? resource.data?.status;
  function set(key, value) {
    setForm((f) => ({ ...f, config: { ...f.config, [key]: value } }));
  }
  const field = (key, label, props = {}) => (
    <TextInput
      label={label}
      value={c?.[key] ?? ""}
      disabled={op.busy}
      onChange={(e) => set(key, e.target.value)}
      {...props}
    />
  );
  async function save() {
    await op.run(
      async () => {
        const nextConfig = {
            ...c,
            sms_to_phone: lines(form.phones),
            blacklist_phone: lines(form.blocked),
            blacklist_keywords: lines(form.keywords),
          },
          payload = forwardPatch(nextConfig, initial.config, form);
        const result = await api.request("/sms/forward/config", {
          method: "POST",
          body: payload,
          timeout: 60000,
        });
        if (typeof result.enabled !== "boolean")
          throw new Error("设备未确认保存结果");
        status.setData(result);
        const saved = await api.request("/sms/forward/config");
        resource.setData(saved);
        return true;
      },
      { write: true, message: "转发配置已保存并重新读取" },
    );
  }
  async function test() {
    if (dirty) {
      op.clear();
      return;
    }
    if (
      !(await ask({
        title: "发送测试消息？",
        body: (
          <Text size="sm">
            向当前已保存渠道发送固定测试消息。短信渠道可能产生费用。
          </Text>
        ),
        confirm: "发送",
      }))
    )
      return;
    await op.run(
      async () => {
        const result = await api.request("/sms/forward/test", {
          method: "POST",
          body: {},
          timeout: 60000,
        });
        status.setData(result);
        if (result.last_result !== "sent")
          throw new Error(
            REASONS[result.last_result] ||
              `投递未确认（${result.last_result ?? "未知"}）`,
          );
        return result;
      },
      { write: true, message: "测试消息发送成功" },
    );
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        {c && (
          <>
            <Panel
              title="短信转发"
              icon={MessageSquare}
              right={
                <Group>
                  <Badge size="xs" color={current?.enabled ? "green" : "gray"}>
                    {current?.enabled ? "已开启" : "未开启"}
                  </Badge>
                  <RefreshButton
                    resource={resource}
                    disabled={op.busy || dirty}
                  />
                </Group>
              }
            >
              <Stack>
                {status.error && (
                  <Alert color="orange">{status.error} · 下方为上次状态</Alert>
                )}
                <Text size="xs" c="dimmed">
                  最近投递：
                  {REASONS[current?.last_result] ||
                    current?.last_result ||
                    "暂无投递"}
                </Text>
                {current?.power_forward_enabled && (
                  <Text size="xs" c="dimmed">
                    电源通知：
                    {REASONS[current.power_last_result] ||
                      current.power_last_result}
                  </Text>
                )}
                <Switch
                  label="短信转发"
                  checked={c.enabled}
                  disabled={op.busy}
                  onChange={(e) => set("enabled", e.target.checked)}
                />
                <Select
                  label="转发方式"
                  value={c.method}
                  data={[
                    { value: "smtp", label: "邮件 SMTP" },
                    { value: "webhook", label: "Webhook" },
                    { value: "dingtalk", label: "钉钉机器人" },
                    { value: "sms", label: "设备 SIM 短信" },
                  ]}
                  allowDeselect={false}
                  disabled={op.busy}
                  onChange={(v) => set("method", v)}
                />
              </Stack>
            </Panel>
            <Panel
              title={
                {
                  smtp: "邮件 SMTP",
                  webhook: "Webhook",
                  dingtalk: "钉钉机器人",
                  sms: "短信接收号码",
                }[c.method]
              }
              icon={Mail}
            >
              <Stack>
                {c.method === "smtp" && (
                  <>
                    <SimpleGrid cols={{ base: 1, sm: 2 }}>
                      {[
                        ["host", "SMTP 服务器域名"],
                        ["port", "端口 · 465 TLS / 587 STARTTLS"],
                        ["username", "发件邮箱"],
                        ["to", "收件邮箱"],
                      ].map(([k, label]) => (
                        <TextInput
                          label={label}
                          key={k}
                          value={String(c.smtp[k] ?? "")}
                          disabled={op.busy || form.clearSmtp}
                          onChange={(e) =>
                            setForm((f) => ({
                              ...f,
                              config: {
                                ...f.config,
                                smtp: { ...f.config.smtp, [k]: e.target.value },
                              },
                            }))
                          }
                        />
                      ))}
                    </SimpleGrid>
                    <PasswordInput
                      label={
                        c.smtp.password_configured
                          ? "授权码 / 密码 · 留空保留"
                          : "授权码 / 密码"
                      }
                      value={form.smtpPassword}
                      autoComplete="new-password"
                      disabled={op.busy || form.clearSmtp}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, smtpPassword: e.target.value }))
                      }
                    />
                    <Switch
                      label="附带设备信息"
                      checked={c.smtp_forward_device_info}
                      disabled={op.busy}
                      onChange={(e) =>
                        set("smtp_forward_device_info", e.target.checked)
                      }
                    />
                    {c.smtp.password_configured && (
                      <Switch
                        label="清除邮件配置"
                        checked={form.clearSmtp}
                        disabled={op.busy}
                        onChange={(e) => {
                          const clearSmtp = e.target.checked;
                          setForm((f) => ({ ...f, clearSmtp }));
                        }}
                      />
                    )}
                  </>
                )}
                {c.method === "webhook" &&
                  field("webhook_url", "公网 HTTPS 地址")}
                {c.method === "dingtalk" && (
                  <>
                    {field("dingtalk_webhook", "机器人 Webhook 地址")}
                    <PasswordInput
                      label={
                        c.dingtalk_secret_configured
                          ? "加签密钥 · 留空保留"
                          : "加签密钥 · 可选"
                      }
                      value={form.secret}
                      autoComplete="new-password"
                      disabled={op.busy || form.clearSecret}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, secret: e.target.value }))
                      }
                    />
                    <Switch
                      label="附带设备信息"
                      checked={c.dingtalk_forward_device_info}
                      disabled={op.busy}
                      onChange={(e) =>
                        set("dingtalk_forward_device_info", e.target.checked)
                      }
                    />
                    {c.dingtalk_secret_configured && (
                      <Switch
                        label="清除加签密钥"
                        checked={form.clearSecret}
                        disabled={op.busy}
                        onChange={(e) => {
                          const clearSecret = e.target.checked;
                          setForm((f) => ({ ...f, clearSecret }));
                        }}
                      />
                    )}
                  </>
                )}
                {c.method === "sms" && (
                  <>
                    <Textarea
                      label="每行一个号码，最多 3 个"
                      value={form.phones}
                      autosize
                      minRows={3}
                      disabled={op.busy}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, phones: e.target.value }))
                      }
                    />
                    <Switch
                      label="附带设备信息"
                      checked={c.sms_forward_device_info}
                      disabled={op.busy}
                      onChange={(e) =>
                        set("sms_forward_device_info", e.target.checked)
                      }
                    />
                    <Text size="xs" c="dimmed">
                      通过设备 SIM 发送，可能产生费用；发送限制以设备返回为准。
                    </Text>
                    {current?.sms_daily_remaining != null && (
                      <Text size="xs" c="dimmed">
                        今日剩余短信额度 {current.sms_daily_remaining}{" "}
                        条（设备返回，与定时短信共用）
                      </Text>
                    )}
                  </>
                )}
              </Stack>
            </Panel>
            <Panel title="电源通知">
              <Stack>
                <Switch
                  label="电源状态通知"
                  checked={c.power_forward_enabled}
                  disabled={
                    op.busy ||
                    !(current?.power_supported || c.power_forward_enabled)
                  }
                  onChange={(e) =>
                    set("power_forward_enabled", e.target.checked)
                  }
                />
                <Text size="xs" c="dimmed">
                  {current?.power_supported
                    ? "充电状态变化，或电量到达、跨过 5%、20%、40%、60%、80%、100% 时通知"
                    : "设备暂未提供有效电池状态"}
                </Text>
                {!c.enabled && c.power_forward_enabled && (
                  <Text size="xs" c="dimmed">
                    开启并保存短信转发后生效
                  </Text>
                )}
              </Stack>
            </Panel>
            <Panel title="过滤与备注">
              <Stack>
                <SimpleGrid cols={{ base: 1, sm: 2 }}>
                  <Textarea
                    label="号码黑名单 · 每行一个，最多 64 个"
                    value={form.blocked}
                    autosize
                    minRows={3}
                    disabled={op.busy}
                    onChange={(e) =>
                      setForm((f) => ({ ...f, blocked: e.target.value }))
                    }
                  />
                  <Textarea
                    label="关键词黑名单 · 每行一个，最多 32 个"
                    value={form.keywords}
                    autosize
                    minRows={3}
                    disabled={op.busy}
                    onChange={(e) =>
                      setForm((f) => ({ ...f, keywords: e.target.value }))
                    }
                  />
                </SimpleGrid>
                {field("nickname", "设备备注")}
                <Text size="xs" c="dimmed">
                  仅转发新短信，失败不自动重发；设备独立运行，关闭网页仍生效。
                </Text>
                <Group justify="space-between">
                  <Button
                    variant="default"
                    disabled={op.busy || dirty || !current?.enabled}
                    onClick={test}
                  >
                    发送测试消息
                  </Button>
                  <Button loading={op.busy} disabled={!dirty} onClick={save}>
                    保存配置
                  </Button>
                </Group>
              </Stack>
            </Panel>
          </>
        )}
      </LoadState>
    </Stack>
  );
}
