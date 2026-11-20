import React, { useEffect, useRef, useState } from "react";
import {
  Alert,
  Badge,
  Button,
  Group,
  Progress,
  Select,
  Stack,
  Switch,
  Text,
  Textarea,
} from "@mantine/core";
import { Download, Shield, Terminal as TerminalIcon } from "lucide-react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { lines, secureUrl } from "../management-models.js";
import { changed } from "../settings-models.js";
import { formatBytes } from "../core.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { InfoGrid, Panel, useServices } from "./ui.jsx";
const OTA_STATES = {
  idle: "暂无可用更新",
  checking: "正在检查更新",
  available: "发现新版本",
  downloading: "正在下载并校验",
  installing: "正在安装，等待服务恢复",
  succeeded: "更新完成",
  waiting_idle: "等待安装条件",
  error: "更新未完成",
  blocked: "自动更新已暂停，可手动重试",
};
export function OtaPage() {
  const api = usePageApi(),
    { ask } = useServices(),
    op = useOperation(),
    resource = useResource(() => api.request("/ota/app")),
    [form, setForm] = useState(null),
    [initial, setInitial] = useState(null),
    dirty = !!form && changed(form, initial);
  useDirtyGuard(dirty);
  useEffect(() => {
    if (!resource.data?.config || dirty) return;
    const c = resource.data.config,
      f = {
        enabled: !!c.enabled,
        mode:
          c.sources?.length === 1 && c.sources[0] === "custom"
            ? "custom"
            : !c.sources?.includes("custom")
              ? "official"
              : "both",
        servers: (c.servers ?? []).join("\n"),
      };
    setForm(f);
    setInitial(f);
  }, [resource.data]);
  useEffect(() => {
    if (op.busy) return;
    const timer = setInterval(
      () => void resource.refresh(),
      resource.data?.busy ? 2000 : 10000,
    );
    return () => clearInterval(timer);
  }, [op.busy, resource.data?.busy]);
  const v = resource.data,
    s = v?.status,
    c = v?.candidate,
    locked = op.busy || v?.busy,
    autoDisabled =
      v?.auto_updates_supported === false ||
      v?.automatic_updates_allowed === false;
  async function check() {
    await op.run(
      async () => {
        const result = await api.request("/ota/app/check", {
          method: "POST",
          body: {},
        });
        await resource.refresh();
        return result;
      },
      { write: true, message: "检查请求已提交，请查看设备更新状态" },
    );
  }
  async function save() {
    await op.run(
      async () => {
        const servers = lines(form.servers).map((x) => x.replace(/\/$/, ""));
        if (form.mode === "custom" && !servers.length)
          throw new Error("请填写自定义更新源");
        servers.forEach((u) => secureUrl(u));
        const payload = {
          enabled: autoDisabled ? false : form.enabled,
          servers,
          sources:
            form.mode === "custom"
              ? ["custom"]
              : form.mode === "official"
                ? ["netdisk", "github"]
                : ["custom", "netdisk", "github"],
        };
        const next = await api.request("/ota/app/config", {
          method: "POST",
          body: payload,
        });
        if (!next.config) throw new Error("设备未确认配置");
        resource.setData(next);
        setForm(null);
        setInitial(null);
      },
      { write: true, message: "更新源配置已保存" },
    );
  }
  async function install() {
    if (!c || !s.signature_verified || v.busy || !/^[0-9a-f]{64}$/.test(c.id))
      return;
    if (
      !(await ask({
        title: `更新至 ${c.manifest?.version}？`,
        body: (
          <Text size="sm">
            已通过签名校验。将重启 datad
            服务，请保持供电，恢复后重新登录并核对运行版本。
          </Text>
        ),
        confirm: "安装更新",
      }))
    )
      return;
    await op.run(
      async () => {
        await api.request("/ota/app/install", {
          method: "POST",
          body: { candidate_id: c.id },
        });
        await resource.refresh();
      },
      { write: true, message: "安装已请求，以设备进度和恢复后的版本为准" },
    );
  }
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title={`当前版本 ${s?.current_version || "—"}`}
          icon={Download}
          right={<RefreshButton resource={resource} disabled={op.busy} />}
        >
          <Stack>
            <Text size="sm">
              {!s?.last_check_at && !v?.busy
                ? "尚未检查更新"
                : OTA_STATES[s?.state] || s?.state}
            </Text>
            <InfoGrid
              items={[
                ["最新版本", s?.latest_version],
                ["来源", s?.source],
              ]}
            />
            {v?.busy && (
              <Stack gap="xs">
                {s?.state === "downloading" ? (
                  <Progress
                    value={Math.max(0, Math.min(100, Number(s.progress) || 0))}
                    animated
                  />
                ) : (
                  <Progress indeterminate />
                )}
                <Text size="xs" c="dimmed">
                  {s?.state === "downloading"
                    ? `${s.progress}% · 下载校验阶段`
                    : "正在处理，请保持供电"}
                </Text>
              </Stack>
            )}
            {s?.error && <Alert color="red">{s.error}</Alert>}
            {s?.wait_reasons?.length > 0 && (
              <Alert color="orange">{s.wait_reasons.join(" · ")}</Alert>
            )}
            <Button
              variant="light"
              disabled={locked || dirty}
              loading={op.busy}
              onClick={check}
            >
              检查更新
            </Button>
          </Stack>
        </Panel>
        {c && (
          <Panel title={`新版本 ${c.manifest?.version}`}>
            <Stack>
              <Badge
                variant="light"
                color={s?.signature_verified ? "green" : "orange"}
              >
                {s?.signature_verified ? "签名已验证" : "等待签名校验"}
              </Badge>
              <Text size="xs" c="dimmed">
                下载大小 {formatBytes(c.manifest?.artifacts?.binary?.size)}
              </Text>
              <Text size="sm" style={{ whiteSpace: "pre-wrap" }}>
                {c.manifest?.notes || "未提供更新说明"}
              </Text>
              <Button
                disabled={
                  locked ||
                  dirty ||
                  !s?.signature_verified ||
                  !/^[0-9a-f]{64}$/.test(c.id)
                }
                onClick={install}
              >
                安装更新
              </Button>
            </Stack>
          </Panel>
        )}
        {form && (
          <Panel title="更新设置">
            <Stack>
              <Switch
                label="空闲时自动更新"
                checked={!autoDisabled && form.enabled}
                disabled={locked || autoDisabled}
                onChange={(e) => {
                  const enabled = e.target.checked;
                  setForm((f) => ({ ...f, enabled }));
                }}
              />
              {autoDisabled && (
                <Text size="xs" c="dimmed">
                  当前后端构建不允许自动更新，可手动检查和安装。
                </Text>
              )}
              <Select
                label="更新来源"
                value={form.mode}
                allowDeselect={false}
                disabled={locked}
                data={[
                  { value: "official", label: "官方源" },
                  { value: "both", label: "自定义优先" },
                  { value: "custom", label: "仅自定义" },
                ]}
                onChange={(v) => setForm((f) => ({ ...f, mode: v }))}
              />
              {form.mode !== "official" && (
                <>
                  <Textarea
                    label="更新目录 HTTPS 地址 · 每行一个"
                    value={form.servers}
                    autosize
                    minRows={3}
                    disabled={locked}
                    onChange={(e) =>
                      setForm((f) => ({ ...f, servers: e.target.value }))
                    }
                  />
                  <Text size="xs" c="dimmed">
                    需提供签名清单 {v?.platform} 和
                    .sig，以及清单中的脚本、二进制。
                  </Text>
                </>
              )}
              <Button
                loading={op.busy}
                disabled={locked || !dirty}
                onClick={save}
              >
                保存设置
              </Button>
            </Stack>
          </Panel>
        )}
      </LoadState>
    </Stack>
  );
}
export function OemPage() {
  const api = usePageApi(),
    { connection } = useServices(),
    controls = connection.capabilities?.controls ?? [],
    supported =
      controls.includes("oem.access.status") &&
      controls.includes("oem.access.set"),
    op = useOperation(),
    resource = useResource(async () =>
      supported ? api.control("oem.access.status") : { supported: false },
    ),
    [duration, setDuration] = useState("600");
  useEffect(() => {
    if (op.busy) return;
    const timer = setInterval(() => void resource.refresh(), 2000);
    return () => clearInterval(timer);
  }, [op.busy]);
  async function apply(paused) {
    await op.run(
      async () => {
        const next = await api.control("oem.access.set", {
          paused,
          duration_seconds: paused ? Number(duration) : 0,
        });
        if (typeof next.paused !== "boolean" || next.paused !== paused)
          throw new Error("设备尚未确认避让状态");
        resource.setData(next);
        return next;
      },
      {
        write: true,
        message: (r) =>
          r.paused
            ? "已暂停 datad 自动登录，可使用官方后台"
            : "已恢复 datad 按需访问",
      },
    );
  }
  const s = resource.data;
  return (
    <Stack>
      <LoadState resource={resource}>
        <Panel
          title="官方后台避让"
          icon={Shield}
          right={<RefreshButton resource={resource} disabled={op.busy} />}
        >
          <Stack>
            {s?.supported === false ? (
              <Text size="sm">当前设备或后端版本未提供此能力。</Text>
            ) : (
              <>
                <Switch
                  label="暂停 datad 自动登录"
                  checked={!!s?.paused}
                  disabled={op.busy || !!resource.error}
                  onChange={(e) => apply(e.target.checked)}
                />
                <Text size="sm">
                  {s?.paused
                    ? s.remaining_seconds == null
                      ? "手动恢复模式，不会自动结束避让"
                      : `设备返回剩余 ${s.remaining_seconds} 秒`
                    : "允许 datad 按需登录和访问"}
                </Text>
                <Select
                  label="暂停时长"
                  value={duration}
                  allowDeselect={false}
                  disabled={op.busy}
                  data={[
                    { value: "300", label: "5 分钟" },
                    { value: "600", label: "10 分钟" },
                    { value: "1800", label: "30 分钟" },
                    { value: "0", label: "手动恢复" },
                  ]}
                  onChange={setDuration}
                />
                {s?.paused && (
                  <Button
                    variant="light"
                    loading={op.busy}
                    onClick={() => apply(true)}
                  >
                    应用时长 / 重新计时
                  </Button>
                )}
              </>
            )}
            <Text size="xs" c="dimmed" lh={1.8}>
              暂停期间，datad
              不再登录或请求官方后台，依赖它的短信、转发及部分设置暂时不可用，基础流量统计继续运行。倒计时由设备执行；datad
              或设备重启后恢复访问。
            </Text>
          </Stack>
        </Panel>
      </LoadState>
    </Stack>
  );
}
export function ShellPage() {
  const { api: client } = useServices(),
    mount = useRef(null),
    connection = useRef(null),
    terminal = useRef(null),
    [attempt, setAttempt] = useState(0),
    [status, setStatus] = useState("正在连接终端"),
    [ready, setReady] = useState(false);
  useEffect(() => {
    let disposed = false,
      isReady = false,
      pendingOutput = 0,
      failure = null;
    const generation = client.generation,
      term = new Terminal({
        fontFamily: 'Consolas, "Cascadia Mono", monospace',
        fontSize: 13,
        scrollback: 300,
        convertEol: false,
        theme: { background: "#151c2b", foreground: "#dce5f5" },
      }),
      fit = new FitAddon();
    term.loadAddon(fit);
    term.open(mount.current);
    fit.fit();
    terminal.current = term;
    setReady(false);
    setStatus("正在连接终端");
    const url = new URL("webshell", client.base);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    let socket;
    try {
      if (!/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(client.token))
        throw new Error(
          "服务 Token 不符合浏览器终端协议要求，请使用管理账号登录",
        );
      socket = new WebSocket(url, [
        "datad-webshell-v1",
        `datad-auth.${client.token}`,
      ]);
    } catch (e) {
      setStatus(e.message);
      term.dispose();
      return;
    }
    socket.binaryType = "arraybuffer";
    connection.current = socket;
    const finish = (message) => {
      if (disposed) return;
      isReady = false;
      setReady(false);
      failure = message;
      setStatus(message);
    };
    const watchdog = setTimeout(() => {
      finish("终端连接超时，请手动重连");
      socket.close();
    }, 15000);
    const active = () => !disposed && generation === client.generation;
    socket.onmessage = (event) => {
      if (!active()) {
        socket.close();
        return;
      }
      if (typeof event.data === "string") {
        let msg;
        try {
          msg = JSON.parse(event.data);
        } catch {}
        if (msg?.type !== "ready" || isReady) {
          finish("终端协议不兼容");
          socket.close();
          return;
        }
        clearTimeout(watchdog);
        isReady = true;
        setReady(true);
        setStatus("已连接 · 设备 Shell");
        socket.send(
          JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }),
        );
      } else if (isReady) {
        const bytes = new Uint8Array(event.data);
        pendingOutput += bytes.length;
        if (pendingOutput > 2 * 1024 * 1024) {
          finish("终端输出过快，连接已关闭，请重新连接");
          socket.close();
          return;
        }
        term.write(bytes, () => {
          pendingOutput -= bytes.length;
        });
      } else {
        finish("终端未完成初始化");
        socket.close();
      }
    };
    socket.onerror = () =>
      finish("终端连接失败，请确认后端已启用 WebShell 和当前会话有效");
    socket.onclose = () => {
      clearTimeout(watchdog);
      finish(failure || "终端已断开，可手动重连");
    };
    const input = term.onData((text) => {
      if (!active() || !isReady) return;
      const bytes = new TextEncoder().encode(text);
      if (bytes.length > 16384 || socket.bufferedAmount > 65536) {
        finish("输入队列超限，请重连后检查命令结果");
        socket.close();
        return;
      }
      socket.send(bytes);
    });
    const resize = new ResizeObserver(() => {
      if (!active()) return;
      fit.fit();
      if (isReady && socket.readyState === WebSocket.OPEN)
        socket.send(
          JSON.stringify({ type: "resize", cols: term.cols, rows: term.rows }),
        );
    });
    resize.observe(mount.current);
    return () => {
      disposed = true;
      clearTimeout(watchdog);
      socket.onmessage = null;
      socket.onerror = null;
      socket.onclose = null;
      socket.close();
      input.dispose();
      resize.disconnect();
      term.dispose();
      terminal.current = null;
      connection.current = null;
    };
  }, [attempt, client.generation]);
  function send(text) {
    if (ready && connection.current?.readyState === WebSocket.OPEN)
      connection.current.send(new TextEncoder().encode(text));
  }
  return (
    <Panel title="Root Shell" icon={TerminalIcon}>
      <Stack>
        <Group justify="space-between">
          <Text size="sm" c={ready ? "blue" : "dimmed"}>
            {status}
          </Text>
          <Group gap="xs">
            {!ready && (
              <Button
                size="xs"
                variant="light"
                onClick={() => setAttempt((n) => n + 1)}
              >
                重连
              </Button>
            )}
            <Button
              size="xs"
              variant="default"
              disabled={!ready}
              onClick={() => send("\x03")}
            >
              Ctrl+C
            </Button>
            <Button
              size="xs"
              variant="default"
              disabled={!ready}
              onClick={() => send("\x04")}
            >
              Ctrl+D
            </Button>
            <Button
              size="xs"
              variant="default"
              disabled={!ready}
              onClick={() => send("\r")}
            >
              回车
            </Button>
            <Button
              size="xs"
              variant="default"
              onClick={() => terminal.current?.clear()}
            >
              清空
            </Button>
          </Group>
        </Group>
        <div
          ref={mount}
          className="terminal-container"
          style={{
            height: "min(65vh,600px)",
            padding: 12,
            borderRadius: 10,
            overflow: "hidden",
            background: "#151c2b",
          }}
        />
      </Stack>
    </Panel>
  );
}
