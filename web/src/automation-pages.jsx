import React, { useEffect, useRef, useState } from "react";
import {
  Alert,
  Badge,
  Button,
  Card,
  Group,
  Modal,
  Select,
  SimpleGrid,
  Stack,
  Switch,
  Text,
  Textarea,
  TextInput,
} from "@mantine/core";
import { Clock, History, Plus, RefreshCw } from "lucide-react";
import {
  TASK_ACTIONS,
  TASK_MODES,
  taskPayload,
  recoveryPayload,
  RECOVERY_STATUS,
  REASONS,
} from "../automation-models.js";
import { changed } from "../settings-models.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { InfoGrid, Panel, useServices } from "./ui.jsx";

export function ActivityPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    [category, setCategory] = useState(""),
    [failed, setFailed] = useState(false),
    resource = useResource(
      () => api.request("/activity" + query()),
      [category, failed],
    );
  function query(before) {
    const q = new URLSearchParams();
    if (category) q.set("category", category);
    if (failed) q.set("failed", "true");
    if (before) q.set("before", before);
    return q.size ? "?" + q : "";
  }
  async function more() {
    await op.run(async () => {
      const next = await api.request(
        "/activity" + query(resource.data.next_before),
      );
      const all = new Map(
        [...resource.data.entries, ...next.entries].map((e) => [
          String(e.id),
          e,
        ]),
      );
      resource.setData({ ...next, entries: [...all.values()] });
    });
  }
  async function clear() {
    if (
      await ask({
        title: "清空全部记录？",
        body: (
          <Text size="sm">
            清空所有分类的通知与执行记录，不影响任务和转发配置。
          </Text>
        ),
        danger: true,
        confirm: "清空",
      })
    )
      await op.run(
        async () => {
          await api.request("/activity/clear", { method: "POST", body: {} });
          await resource.refresh();
        },
        { write: true, message: "清空请求已完成" },
      );
  }
  return (
    <Stack>
      <Panel
        title="通知与执行记录"
        icon={History}
        right={
          <Group>
            <RefreshButton resource={resource} disabled={op.busy} />
            <Button
              size="xs"
              variant="light"
              color="red"
              disabled={op.busy}
              onClick={clear}
            >
              清空
            </Button>
          </Group>
        }
      >
        <Stack>
          <Group>
            <Select
              aria-label="记录分类"
              value={category}
              allowDeselect={false}
              data={[
                { value: "", label: "全部" },
                { value: "notification", label: "通知" },
                { value: "task", label: "任务" },
                { value: "recovery", label: "恢复" },
              ]}
              onChange={setCategory}
              disabled={op.busy}
            />
            <Switch
              label="只看失败"
              checked={failed}
              disabled={op.busy}
              onChange={(e) => setFailed(e.target.checked)}
            />
          </Group>
          <LoadState resource={resource}>
            {resource.data?.storage_ok === false && (
              <Alert color="red">记录保存异常，请检查设备存储空间</Alert>
            )}
            <Text size="xs" c="dimmed">
              保留最近 {resource.data?.max_entries ?? 300} 条 · 不记录短信正文
            </Text>
            {!resource.data?.entries?.length && (
              <Text size="sm" c="dimmed">
                暂无记录
              </Text>
            )}
            {resource.data?.entries?.map((e) => (
              <Card withBorder radius="md" padding="md" key={String(e.id)}>
                <Group justify="space-between">
                  <Text size="sm" fw={600}>
                    {e.task ||
                      TASK_ACTIONS[e.action] ||
                      {
                        sms: "短信转发",
                        power: "电源通知",
                        test: "测试通知",
                        device_info: "设备信息通知",
                        redial: "自动重新拨号",
                        reboot: "自动重启",
                        online: "网络恢复",
                        enable: "开启自动恢复",
                        disable: "关闭自动恢复",
                        resume: "恢复检测",
                      }[e.action] ||
                      e.action}
                  </Text>
                  <Badge
                    size="xs"
                    color={
                      e.result === "failed"
                        ? "red"
                        : e.result === "success"
                          ? "green"
                          : "gray"
                    }
                  >
                    {{
                      success: "成功",
                      failed: "失败",
                      skipped: "已跳过",
                      requested:
                        e.reason === "awaiting_probe" ? "等待检测" : "已发起",
                    }[e.result] || "未知"}
                  </Badge>
                </Group>
                {e.reason && (
                  <Text mt={7} size="xs">
                    {REASONS[e.reason] || e.reason}
                  </Text>
                )}
                <Text size="xs" c="dimmed" mt={7}>
                  {e.device_time || "设备时间不可用"}
                </Text>
              </Card>
            ))}
            {resource.data?.next_before && (
              <Button variant="light" loading={op.busy} onClick={more}>
                加载更多
              </Button>
            )}
          </LoadState>
        </Stack>
      </Panel>
    </Stack>
  );
}
function defaultTaskTime(text) {
  const m = String(text ?? "").match(/(?:T|\s)(\d{2}):(\d{2})/);
  if (!m) return "08:00";
  const total = (Number(m[1]) * 60 + Number(m[2]) + 5) % 1440;
  return `${String(Math.floor(total / 60)).padStart(2, "0")}:${String(total % 60).padStart(2, "0")}`;
}
export function TasksPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/tasks")),
    [editor, setEditor] = useState({ opened: false }),
    [initial, setInitial] = useState("");
  const dirty = editor.opened && JSON.stringify(editor) !== initial;
  useDirtyGuard(dirty);
  useEffect(() => {
    if (editor.opened || op.busy) return;
    const timer = setInterval(() => void resource.refresh(), 10000);
    return () => clearInterval(timer);
  }, [editor.opened, op.busy]);
  function edit(task) {
    op.clear();
    const e = {
      opened: true,
      original: task?.id ?? null,
      id: task?.id ?? "",
      time: task?.time ?? defaultTaskTime(resource.data?.device_time),
      repeat_daily: task?.repeat_daily ?? false,
      action:
        task?.action ??
        Object.keys(TASK_ACTIONS).find((k) =>
          resource.data?.actions?.includes(k),
        ) ??
        "",
      params: {
        mode: resource.data?.network_modes?.[0] ?? "",
        slot: "1",
        enabled: "1",
        roaming: "0",
        chip: "chip1",
        ...task?.params,
      },
    };
    setEditor(e);
    setInitial(JSON.stringify(e));
  }
  async function close() {
    if (op.busy) return;
    if (
      dirty &&
      !(await ask({
        title: "放弃未保存的任务？",
        body: <Text size="sm">修改尚未提交。</Text>,
        confirm: "放弃",
      }))
    )
      return;
    setEditor((e) => ({ ...e, opened: false }));
  }
  async function save() {
    await op.run(
      async () => {
        const current = await api.request("/tasks"),
          payload = taskPayload(editor, current);
        if (!editor.original && current.tasks.some((t) => t.id === payload.id))
          throw new Error("任务名称已存在");
        if (
          !(await ask({
            title: "保存定时任务？",
            body: (
              <Stack>
                <Text size="sm">
                  {payload.id} · {payload.time} ·{" "}
                  {payload.repeat_daily ? "每天" : "下次到达时间执行一次"} ·{" "}
                  {TASK_ACTIONS[payload.action]}
                </Text>
                {payload.action === "device.poweroff" && (
                  <Text size="sm" c="red">
                    关机后无法远程开机，后续任务也不会执行。
                  </Text>
                )}
              </Stack>
            ),
          }))
        )
          return false;
        const result = await api.request("/tasks", {
          method: "POST",
          body: payload,
        });
        if (
          !result.tasks?.some(
            (t) =>
              t.id === payload.id &&
              t.action === payload.action &&
              t.time === payload.time &&
              t.repeat_daily === payload.repeat_daily &&
              Object.entries(payload.params).every(
                ([k, v]) => t.params?.[k] === v,
              ),
          )
        )
          throw new Error("任务已提交，列表回读尚未确认");
        resource.setData(result);
        setEditor((e) => ({ ...e, opened: false }));
        return true;
      },
      { write: true, message: (ok) => (ok ? "任务已保存" : "") },
    );
  }
  async function remove(t) {
    if (
      await ask({
        title: "删除定时任务？",
        body: <Text size="sm">删除“{t.id}”？</Text>,
        danger: true,
        confirm: "删除",
      })
    )
      await op.run(
        async () => {
          const result = await api.request("/tasks/remove", {
            method: "POST",
            body: { id: t.id },
          });
          if (result.tasks?.some((v) => v.id === t.id))
            throw new Error("删除结果未确认");
          resource.setData(result);
        },
        { write: true, message: "任务已删除" },
      );
  }
  const param = (k, v) =>
      setEditor((e) => ({ ...e, params: { ...e.params, [k]: v } })),
    field = (k, label, multiline = false) =>
      multiline ? (
        <Textarea
          label={label}
          value={editor.params?.[k] ?? ""}
          onChange={(e) => param(k, e.target.value)}
          disabled={op.busy}
          autosize
          minRows={3}
        />
      ) : (
        <TextInput
          label={label}
          value={String(editor.params?.[k] ?? "")}
          onChange={(e) => param(k, e.target.value)}
          disabled={op.busy}
        />
      );
  const choice = (k, label, data) => (
    <Select
      label={label}
      value={String(editor.params?.[k] ?? "")}
      data={data}
      onChange={(v) => param(k, v)}
      allowDeselect={false}
      disabled={op.busy}
    />
  );
  const v = resource.data;
  return (
    <Stack>
      <Panel
        title="定时任务"
        icon={Clock}
        right={
          <Group>
            <RefreshButton
              resource={resource}
              disabled={editor.opened || op.busy}
            />
            <Button
              size="xs"
              leftSection={<Plus size={13} />}
              disabled={
                !v?.supported ||
                op.busy ||
                (v?.tasks?.length ?? 0) >= (v?.max_tasks ?? 16)
              }
              onClick={() => edit()}
            >
              新建
            </Button>
          </Group>
        }
      >
        <LoadState resource={resource}>
          <Stack>
            <Text size="xs" c="dimmed">
              设备时间 {v?.device_time || "不可用"} · {v?.tasks?.length ?? 0}/
              {v?.max_tasks ?? 16} 项
            </Text>
            {v?.error && (
              <Alert color="red">{REASONS[v.error] || v.error}</Alert>
            )}
            {v?.supported === false && (
              <Alert color="orange">设备尚未提供定时任务</Alert>
            )}
            {!v?.tasks?.length && (
              <Text size="sm" c="dimmed">
                暂无定时任务
              </Text>
            )}
            {v?.tasks?.map((t) => (
              <Card withBorder padding="md" radius="md" key={t.id}>
                <Group justify="space-between">
                  <Stack gap={6}>
                    <Text size="sm" fw={600}>
                      {t.id}
                    </Text>
                    <Text size="lg" c="blue">
                      {t.time} · {t.repeat_daily ? "每天" : "仅一次"}
                    </Text>
                    <Text size="sm">{TASK_ACTIONS[t.action] || t.action}</Text>
                    <Text size="xs" c="dimmed">
                      {{
                        ok: "执行成功",
                        failed: "执行失败",
                        requested: "已提交执行",
                      }[t.last_result] ||
                        (t.has_triggered ? "已执行" : "待执行")}{" "}
                      {t.last_attempt_date}
                    </Text>
                  </Stack>
                  <Group>
                    <Button
                      size="xs"
                      variant="default"
                      disabled={op.busy}
                      onClick={() => edit(t)}
                    >
                      编辑
                    </Button>
                    <Button
                      size="xs"
                      variant="light"
                      color="red"
                      disabled={op.busy}
                      onClick={() => remove(t)}
                    >
                      删除
                    </Button>
                  </Group>
                </Group>
              </Card>
            ))}
            <Text size="xs" c="dimmed">
              任务由设备执行；设备离线时错过的任务不会补执行。
            </Text>
          </Stack>
        </LoadState>
      </Panel>
      <Modal
        opened={editor.opened}
        onClose={close}
        title={editor.original ? "编辑定时任务" : "新建定时任务"}
        centered
        closeOnClickOutside={!op.busy}
      >
        <Stack>
          <TextInput
            label="任务名称"
            value={editor.id ?? ""}
            disabled={op.busy || !!editor.original}
            onChange={(e) => setEditor((c) => ({ ...c, id: e.target.value }))}
          />
          <TextInput
            type="time"
            label="设备本地执行时间"
            value={editor.time ?? "08:00"}
            disabled={op.busy}
            onChange={(e) => setEditor((c) => ({ ...c, time: e.target.value }))}
          />
          <Switch
            label="每天重复"
            checked={editor.repeat_daily ?? false}
            disabled={op.busy}
            onChange={(e) => {
              const repeat_daily = e.target.checked;
              setEditor((c) => ({ ...c, repeat_daily }));
            }}
          />
          <Select
            label="任务类型"
            value={editor.action || null}
            data={Object.entries(TASK_ACTIONS)
              .filter(([a]) => v?.actions?.includes(a))
              .map(([value, label]) => ({ value, label }))}
            onChange={(action) =>
              setEditor((c) => ({
                ...c,
                action,
                params: {
                  ...c.params,
                  enabled: action === "nfc.set" ? "true" : "1",
                },
              }))
            }
            allowDeselect={false}
            disabled={op.busy}
          />
          {editor.action === "network.set_mode" &&
            choice(
              "mode",
              "网络模式",
              (v?.network_modes ?? []).map((value) => ({
                value,
                label: TASK_MODES[value] || value,
              })),
            )}
          {editor.action === "sim.set_slot" &&
            choice("slot", "SIM 卡槽", [
              { value: "1", label: "SIM 1" },
              { value: "2", label: "SIM 2" },
            ])}
          {editor.action === "cellular.set" &&
            choice("roaming", "漫游", [
              { value: "0", label: "禁止" },
              { value: "1", label: "允许" },
            ])}
          {["wifi.set_module", "nfc.set"].includes(editor.action) &&
            choice("enabled", "开关", [
              {
                value: editor.action === "nfc.set" ? "true" : "1",
                label: "开启",
              },
              {
                value: editor.action === "nfc.set" ? "false" : "0",
                label: "关闭",
              },
            ])}
          {editor.action === "wifi.set_chip" &&
            choice("chip", "访客频段", [
              { value: "chip1", label: "2.4 GHz" },
              { value: "chip2", label: "5 GHz" },
            ])}
          {editor.action === "sms.send_scheduled" && (
            <>
              {field("number", "收件号码")}
              {field("text", "短信正文 · 最多 280 字符", true)}
              <Text size="xs" c="dimmed">
                使用设备当前 SIM，可能产生短信费用。
              </Text>
            </>
          )}
          {["cell.lock_lte", "cell.lock_nr"].includes(editor.action) && (
            <>
              {field(
                "pci",
                editor.action === "cell.lock_nr"
                  ? "PCI · 0–1007"
                  : "PCI · 0–503",
              )}
              {editor.action === "cell.lock_nr" ? (
                <>
                  {field("arfcn", "NR-ARFCN")}
                  {field("band", "NR 频段")}
                </>
              ) : (
                field("earfcn", "EARFCN")
              )}
              <Text size="xs" c="dimmed">
                此任务不会自动重启；是否需重启生效由固件决定。
              </Text>
            </>
          )}
          {editor.action === "sms.forward.device_info" && (
            <Text size="xs" c="dimmed">
              先在短信转发中配置并启用通知渠道。
            </Text>
          )}
          <Group justify="flex-end">
            <Button variant="default" onClick={close} disabled={op.busy}>
              取消
            </Button>
            <Button loading={op.busy} disabled={!dirty} onClick={save}>
              保存
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
export function RecoveryPage() {
  const api = usePageApi(),
    { ask, navigate } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/network/recovery")),
    [form, setForm] = useState(null),
    [initial, setInitial] = useState(null),
    dirty = !!form && changed(form, initial);
  useDirtyGuard(dirty);
  useEffect(() => {
    if (!resource.data?.config || dirty) return;
    const c = resource.data.config,
      f = {
        ...c,
        interval_seconds: String(c.interval_seconds ?? 60),
        failure_threshold: String(c.failure_threshold ?? 3),
        cooldown_minutes: String((c.cooldown_seconds ?? 300) / 60),
        max_redials: String(c.max_redials ?? 3),
      };
    setForm(f);
    setInitial(f);
  }, [resource.data]);
  useEffect(() => {
    if (op.busy) return;
    const timer = setInterval(() => void resource.refresh(), 10000);
    return () => clearInterval(timer);
  }, [op.busy]);
  async function save() {
    await op.run(
      async () => {
        const payload = recoveryPayload(form);
        if (
          !(await ask({
            title: "保存自动恢复设置？",
            body: (
              <Text size="sm">
                {payload.enabled
                  ? `连续 ${payload.failure_threshold} 次失败后重新拨号，至少冷却 ${payload.cooldown_seconds / 60} 分钟。${payload.reboot_after_failures ? "多次拨号失败后可能自动重启设备。" : "不会自动重启。"}`
                  : "关闭断网自动恢复。"}
              </Text>
            ),
          }))
        )
          return false;
        const next = await api.request("/network/recovery", {
          method: "POST",
          body: payload,
        });
        if (
          !next.config ||
          Object.entries(payload).some(([k, v]) => next.config[k] !== v)
        )
          throw new Error("配置已提交，设备回读尚未一致，请重新读取核对");
        resource.setData(next);
        setInitial(null);
        setForm(null);
        return true;
      },
      { write: true, message: (ok) => (ok ? "配置已保存" : "") },
    );
  }
  async function check() {
    await op.run(
      async () => {
        const result = await api.request("/network/recovery/check", {
          method: "POST",
          body: {},
        });
        if (typeof result.online !== "boolean")
          throw new Error("检测结果未确认");
        return result;
      },
      {
        message: (r) =>
          r.online ? "本次检测：网络可用" : "本次检测：两个地址均未连通",
      },
    );
  }
  async function resume() {
    if (
      await ask({
        title: "恢复自动检测？",
        body: (
          <Text size="sm">
            重新开始一轮，至少等待两分钟后按保存配置恢复；频率限制仍有效。
          </Text>
        ),
      })
    )
      await op.run(
        async () => {
          const next = await api.request("/network/recovery/resume", {
            method: "POST",
            body: {},
          });
          resource.setData(next);
        },
        { write: true, message: "已恢复检测" },
      );
  }
  const v = resource.data,
    field = (k, label) => (
      <TextInput
        label={label}
        value={form?.[k] ?? ""}
        onChange={(e) => setForm((f) => ({ ...f, [k]: e.target.value }))}
        disabled={op.busy}
        inputMode="numeric"
      />
    );
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title={RECOVERY_STATUS[v?.status] || "等待检测"}
          icon={RefreshCw}
          right={<RefreshButton resource={resource} disabled={op.busy} />}
        >
          <Stack>
            <InfoGrid
              items={[
                ["连续失败", v?.consecutive_failures],
                ["本轮拨号", v?.redials],
                ["检测地址", v?.probe_hosts?.join(" / ")],
              ]}
            />
            {v?.storage_ok === false && (
              <Alert color="red">保存异常，请检查设备存储</Alert>
            )}
            <Group>
              <Button variant="light" loading={op.busy} onClick={check}>
                检测一次
              </Button>
              {v?.config?.enabled &&
                (v.manual_paused || v.status === "limit_reached") && (
                  <Button
                    variant="default"
                    disabled={op.busy || dirty}
                    onClick={resume}
                  >
                    恢复检测 / 重新开始一轮
                  </Button>
                )}
              <Button variant="subtle" onClick={() => navigate("activity")}>
                执行记录
              </Button>
            </Group>
          </Stack>
        </Panel>
        {form && (
          <Panel title="自动恢复设置">
            <Stack>
              <Switch
                label="断网自动恢复"
                checked={form.enabled ?? false}
                disabled={op.busy}
                onChange={(e) => {
                  const enabled = e.target.checked;
                  setForm((f) => ({ ...f, enabled }));
                }}
              />
              <Select
                label="检测间隔"
                value={form.interval_seconds}
                data={[
                  { value: "30", label: "每 30 秒" },
                  { value: "60", label: "每分钟" },
                  { value: "120", label: "每 2 分钟" },
                ]}
                disabled={op.busy}
                allowDeselect={false}
                onChange={(v) =>
                  setForm((f) => ({ ...f, interval_seconds: v }))
                }
              />
              <SimpleGrid cols={{ base: 1, sm: 3 }}>
                {field("failure_threshold", "连续失败 · 3–10 次")}
                {field("cooldown_minutes", "冷却 · 3–60 分钟")}
                {field("max_redials", "拨号上限 · 1–5 次")}
              </SimpleGrid>
              <Switch
                label="拨号仍失败时重启设备"
                checked={form.reboot_after_failures ?? false}
                disabled={op.busy}
                onChange={(e) => {
                  const reboot_after_failures = e.target.checked;
                  setForm((f) => ({ ...f, reboot_after_failures }));
                }}
              />
              <Text size="xs" c="dimmed">
                两个检测地址均失败才累计；手动断网后暂停。自动重启有设备端冷却和频率限制。
              </Text>
              <Group justify="flex-end">
                <Button loading={op.busy} disabled={!dirty} onClick={save}>
                  保存
                </Button>
              </Group>
            </Stack>
          </Panel>
        )}
      </LoadState>
    </Stack>
  );
}
