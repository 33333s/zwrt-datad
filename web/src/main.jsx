import React, {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { createRoot } from "react-dom/client";
import {
  ActionIcon,
  Alert,
  AppShell,
  Avatar,
  Badge,
  Box,
  Breadcrumbs,
  Burger,
  Button,
  Card,
  Center,
  Checkbox,
  Container,
  Divider,
  Drawer,
  ScrollArea,
  Group,
  Loader,
  MantineProvider,
  Modal,
  NavLink,
  PasswordInput,
  Skeleton,
  Stack,
  Text,
  TextInput,
  Title,
  Tooltip,
  UnstyledButton,
  createTheme,
  useMantineColorScheme,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import { GithubCard } from "./github-card.jsx";
import { ToastHost } from "./toast.jsx";
import { toast } from "../toasts.js";
import {
  AnimatePresence,
  MotionConfig,
  motion,
  useReducedMotion,
} from "motion/react";
import {
  Activity,
  ArrowRight,
  Check,
  ChevronRight,
  Database,
  FolderOpen,
  LayoutDashboard,
  LogOut,
  Radio,
  RefreshCw,
  ShieldAlert,
  SunMoon,
  Wifi,
  Zap,
  Settings,
  Antenna,
  MessageSquare,
  Network,
} from "lucide-react";
import { createLoginPreferences } from "../login-preferences.js";
import { Client, display } from "../core.js";
import { usePageApi } from "./hooks.jsx";
import { EfsClient } from "../efs.js";
import { deviceProfiles, saveDeviceProfile } from "../device-profiles.js";
import { SignalSummaryStore } from "../signal-cache.js";
import {
  CountdownButton,
  EfsStatus,
  Panel,
  Services,
  notify,
  useServices,
} from "./ui.jsx";
import "@mantine/core/styles.css";
import "@mantine/notifications/styles.css";
import "@mantine/charts/styles.css";
import "./style.css";
import { OemReminder } from "./oem-reminder.jsx";

const OverviewPage = lazy(() =>
  import("./overview.jsx").then((m) => ({ default: m.OverviewPage })),
);
const SignalPage = lazy(() =>
  import("./signal-page.jsx").then((m) => ({ default: m.SignalPage })),
);
const EfsPage = lazy(() =>
  import("./efs-pages.jsx").then((m) => ({ default: m.EfsPage })),
);
const NvPage = lazy(() =>
  import("./efs-pages.jsx").then((m) => ({ default: m.NvPage })),
);
const lazyExport = (path, name) =>
  lazy(() => path().then((m) => ({ default: m[name] })));
const ParamsPage = lazyExport(
  () => import("./network-pages.jsx"),
  "ParamsPage",
);
const BandPage = lazyExport(() => import("./network-pages.jsx"), "BandPage");
const CellLockPage = lazyExport(
  () => import("./network-pages.jsx"),
  "CellLockPage",
);
const SmsPage = lazyExport(() => import("./sms-page.jsx"), "SmsPage");
const ApnPage = lazyExport(() => import("./apn-page.jsx"), "ApnPage");
const WifiPage = lazyExport(() => import("./wifi-page.jsx"), "WifiPage");
const settingsPages = Object.fromEntries(
  ["Settings", "Cellular", "Lan", "Usage", "Traffic", "Device"].map((name) => [
    name.toLowerCase(),
    lazyExport(() => import("./settings-pages.jsx"), name + "Page"),
  ]),
);
const automationPages = Object.fromEntries(
  ["Tasks", "Activity", "Recovery"].map((name) => [
    name.toLowerCase(),
    lazyExport(() => import("./automation-pages.jsx"), name + "Page"),
  ]),
);
const ForwardPage = lazyExport(
  () => import("./forward-page.jsx"),
  "ForwardPage",
);
const NmsPage = lazyExport(() => import("./nms-page.jsx"), "NmsPage");
const toolsPages = Object.fromEntries(
  ["Shell", "Ota", "Oem"].map((name) => [
    name.toLowerCase(),
    lazyExport(() => import("./device-tools.jsx"), name + "Page"),
  ]),
);
const pages = [
  {
    id: "overview",
    label: "设备概览",
    icon: LayoutDashboard,
    caption: "当前连接、无线信号与设备运行状态。",
  },
  {
    id: "params",
    label: "信号参数",
    icon: Radio,
    caption: "服务小区、邻区与全部已上报载波。",
  },
  {
    id: "bands",
    label: "频段与小区",
    icon: Antenna,
    caption: "读取网络模式与频段，设置频段和小区锁定。",
  },
  {
    id: "sms",
    label: "短信",
    icon: MessageSquare,
    caption: "查看收发记录、发送和管理短信。",
  },
  {
    id: "settings",
    label: "设置",
    icon: Settings,
    caption: "网络、设备和自动化管理。",
  },
  {
    id: "signal",
    label: "信令分析",
    icon: Activity,
    caption: "实时信令、协议树与无线解析卡片。",
  },
  {
    id: "efs",
    label: "EFS 管理器",
    icon: FolderOpen,
    caption: "安全浏览与管理基带文件系统。",
  },
  {
    id: "nv",
    label: "NV 工具",
    icon: Database,
    caption: "查询 NV 参数，生成和管理设备配置。",
  },
  ...[
    ["cell-lock", "小区锁定"],
    ["apn", "APN 管理"],
    ["wifi", "热点设置"],
    ["cellular", "蜂窝网络"],
    ["lan", "LAN 设置"],
    ["traffic", "流量管理"],
    ["usage", "历史流量统计"],
    ["device", "设备信息"],
    ["forward", "短信转发"],
    ["tasks", "定时任务"],
    ["activity", "通知与执行记录"],
    ["recovery", "断网自动恢复"],
    ["shell", "Root Shell"],
    ["ota", "datad 更新"],
    ["nms", "NMS 远程管理"],
    ["oem", "官方后台避让"],
  ].map(([id, label]) => ({
    id,
    label,
    icon: Settings,
    caption: "设备配置与运行状态。",
  })),
];
function PageContent({ page }) {
  const component = {
    overview: OverviewPage,
    params: ParamsPage,
    bands: BandPage,
    "cell-lock": CellLockPage,
    sms: SmsPage,
    apn: ApnPage,
    wifi: WifiPage,
    signal: SignalPage,
    forward: ForwardPage,
    nms: NmsPage,
    ...settingsPages,
    ...automationPages,
    ...toolsPages,
  }[page];
  if (component) return React.createElement(component);
  return (
    <EfsGate key={page}>{page === "efs" ? <EfsPage /> : <NvPage />}</EfsGate>
  );
}
function useDialogs() {
  const [config, setConfig] = useState(null),
    pending = useRef(null),
    navigationPending = useRef(null),
    current = useRef(config),
    reduce = useReducedMotion();
  current.current = config;
  function resumeEditing() {
    navigationPending.current?.(false);
    navigationPending.current = null;
    setConfig((c) => ({ ...c, discard: false }));
  }
  function close(value = false, force = false, discardConfirmed = false) {
    const c = current.current;
    if (!force && c?.opened) {
      if (!value && c.discard && !discardConfirmed) {
        resumeEditing();
        return;
      }
      if (!value && c.dirty?.() && !discardConfirmed) {
        setConfig((c) => ({ ...c, discard: true }));
        return;
      }
      if (value && c.validate) {
        try {
          c.validate();
        } catch (e) {
          setConfig((c) => ({ ...c, error: e.message }));
          return;
        }
      }
    }
    navigationPending.current?.(discardConfirmed && !force);
    navigationPending.current = null;
    pending.current?.(value);
    pending.current = null;
    setConfig((c) => (c ? { ...c, opened: false } : null));
  }
  async function allowNavigation() {
    const c = current.current;
    if (!c?.opened) return true;
    if (c.dirty?.()) {
      navigationPending.current?.(false);
      setConfig((c) => ({ ...c, discard: true }));
      return new Promise((resolve) => {
        navigationPending.current = resolve;
      });
    }
    close(false, true);
    return true;
  }
  function ask(options) {
    navigationPending.current?.(false);
    navigationPending.current = null;
    pending.current?.(false);
    return new Promise((resolve) => {
      pending.current = resolve;
      setConfig({ ...options, key: Date.now(), opened: true });
    });
  }
  useEffect(
    () => () => {
      pending.current?.(false);
      navigationPending.current?.(false);
    },
    [],
  );
  const modal = (
    <Modal
      zIndex={500}
      opened={!!config?.opened}
      onClose={() => close(false)}
      title={<Text fw={650}>{config?.title}</Text>}
      size={config?.wide ? "xl" : "md"}
      centered
      radius="lg"
      padding="lg"
      overlayProps={{ backgroundOpacity: 0.38, blur: config?.overlayBlur ?? 4 }}
      transitionProps={{ transition: "pop", duration: reduce ? 0 : 180 }}
      onExitTransitionEnd={() => setConfig((c) => (c?.opened ? c : null))}
    >
      {config && (
        <Stack gap="md">
          {config.subtitle && (
            <Text size="xs" c="dimmed" className="value">
              {config.subtitle}
            </Text>
          )}
          <Box style={{ display: config.discard ? "none" : undefined }}>
            {config.body}
          </Box>
          {config.error && !config.discard && (
            <Alert color="red">{config.error}</Alert>
          )}
          {config.discard && <Alert color="orange">放弃未保存的修改？</Alert>}
          <Divider />
          <Group justify="flex-end">
            {config.discard ? (
              <>
                <Button variant="default" onClick={resumeEditing}>
                  继续编辑
                </Button>
                <Button color="red" onClick={() => close(false, false, true)}>
                  放弃修改
                </Button>
              </>
            ) : (
              <>
                <Button variant="default" onClick={() => close(false)}>
                  {config.cancel ?? "取消"}
                </Button>
                {config.confirm !== null && (
                  <CountdownButton
                    key={config.key}
                    seconds={config.countdown ?? 0}
                    color={config.danger ? "red" : "blue"}
                    onClick={() => close(true)}
                  >
                    {config.confirm ?? "确认"}
                  </CountdownButton>
                )}
              </>
            )}
          </Group>
        </Stack>
      )}
    </Modal>
  );
  return { ask, modal, close, allowNavigation, visible: config !== null };
}
function Brand({ large = false }) {
  return (
    <Group gap={11} wrap="nowrap">
      <span className={large ? "brand-icon large" : "brand-icon"}>
        <Zap size={large ? 24 : 20} fill="currentColor" strokeWidth={1.4} />
      </span>
      <Box>
        <Text className="brand-name" fw={700} fz={large ? 24 : 21}>
          datad<span className="brand-period">.</span>
        </Text>
        {large && (
          <Text size="xs" c="dimmed" mt={2}>
            设备管理控制台
          </Text>
        )}
      </Box>
    </Group>
  );
}
function Login({ api, onConnected }) {
  const preferences = useMemo(() => createLoginPreferences(api.base), [api]);
  const [saved] = useState(() => preferences.read());
  const [user, setUser] = useState(saved?.user ?? "admin"),
    [password, setPassword] = useState(saved?.password ?? ""),
    [remember, setRemember] = useState(!!saved),
    [automatic, setAutomatic] = useState(saved?.auto ?? false),
    [hasSaved, setHasSaved] = useState(!!saved),
    [autoBusy, setAutoBusy] = useState(false),
    [token, setToken] = useState(""),
    [advanced, setAdvanced] = useState(false),
    [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const attempt = useRef(null),
    alive = useRef(true),
    autoAttempted = useRef(false);
  const { toggleColorScheme } = useMantineColorScheme();
  async function submit(event, service = false, auto = false) {
    event?.preventDefault();
    if (attempt.current) return;
    const controller = new AbortController();
    attempt.current = controller;
    setBusy(true);
    setAutoBusy(auto);
    setError("");
    try {
      api.reset();
      if (service) {
        api.token = token.trim();
        if (!api.token) throw new Error("请输入服务 Token");
      } else {
        await api.login(user, password, { signal: controller.signal });
        if (remember) {
          const stored = preferences.save(user, password, automatic);
          if (alive.current) setHasSaved(stored);
          if (!stored) {
            preferences.clear();
            notify("浏览器未能保存密码，本次仍可登录", true);
          }
        } else preferences.clear();
        preferences.allowAuto();
      }
      await onConnected({ signal: controller.signal });
      if (alive.current) {
        setPassword("");
        setToken("");
      }
    } catch (e) {
      if (attempt.current !== controller || !alive.current) return;
      api.reset();
      if (e.status === 401 && auto) {
        preferences.setAutomatic(false);
        setAutomatic(false);
      }
      setError(
        (auto ? "自动登录失败：" : "") +
          (e.status === 401 ? "请检查用户名和密码。" : e.message),
      );
    } finally {
      if (attempt.current === controller) {
        attempt.current = null;
        if (alive.current) {
          setBusy(false);
          setAutoBusy(false);
        }
      }
    }
  }
  function cancelAuto() {
    const running = attempt.current;
    attempt.current = null;
    running?.abort();
    api.reset();
    preferences.suppressAuto();
    setBusy(false);
    setAutoBusy(false);
    setError("");
  }
  function forgetSaved() {
    if (attempt.current) cancelAuto();
    if (!preferences.clear()) {
      notify("浏览器未能清除已保存的登录信息", true);
      return;
    }
    preferences.suppressAuto();
    setUser("admin");
    setPassword("");
    setRemember(false);
    setAutomatic(false);
    setHasSaved(false);
    setError("");
  }
  useEffect(() => {
    alive.current = true;
    if (!autoAttempted.current && preferences.canAutoLogin()) {
      autoAttempted.current = true;
      void submit(null, false, true);
    }
    return () => {
      alive.current = false;
      attempt.current?.abort();
    };
  }, []);
  return (
    <div className="login-shell">
      <div className="login-story">
        <Brand large />
        <div className="login-art">
          <div className="orbit orbit-one" />
          <div className="orbit orbit-two" />
          <div className="orbit orbit-three" />
          <div className="radio-orb">
            <Radio size={48} strokeWidth={1} />
          </div>
          <span className="orbit-dot dot-one" />
          <span className="orbit-dot dot-two" />
        </div>
        <Box>
          <Badge variant="outline" color="blue" mb="lg">
            datad 网页版
          </Badge>
          <Title order={1} fw={550} fz={34} lh={1.5}>
            高级管理控制台
          </Title>
          <Text size="sm" mt="lg" className="story-caption">
            面向中兴 ARM64 5G 路由设备的统一数据与控制服务。
          </Text>
        </Box>
        <Text size="xs" className="story-footer">
          适用于支持 datad 的设备
        </Text>
      </div>
      <Center className="login-form-area" p="lg">
        <motion.div
          initial={{ opacity: 0, y: 12 }}
          animate={{ opacity: 1, y: 0 }}
          className="login-container"
        >
          <Stack gap={28}>
            <Group justify="space-between">
              <Box hiddenFrom="md">
                <Brand />
              </Box>
              <Text size="xs" c="dimmed" visibleFrom="md">
                欢迎使用 datad
              </Text>
              <ActionIcon
                variant="subtle"
                color="gray"
                aria-label="切换明暗主题"
                onClick={toggleColorScheme}
              >
                <SunMoon size={19} />
              </ActionIcon>
            </Group>
            <Box>
              <Title order={2} fw={600} fz={27}>
                高级管理控制台
              </Title>
              <Text c="dimmed" size="sm" mt={9}>
                输入设备的管理账号和密码。
              </Text>
            </Box>
            <form onSubmit={submit}>
              <Stack gap="lg">
                <TextInput
                  label="用户名"
                  value={user}
                  onChange={(e) => setUser(e.target.value)}
                  autoComplete="username"
                  disabled={busy}
                  required
                  maxLength={128}
                  size="md"
                />
                <PasswordInput
                  label="管理密码"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="current-password"
                  disabled={busy}
                  required
                  maxLength={512}
                  size="md"
                />
                <Stack gap={9}>
                  <Group justify="space-between">
                    <Checkbox
                      label="记住密码"
                      checked={remember}
                      disabled={busy}
                      onChange={(e) => {
                        const checked = e.currentTarget.checked;
                        if (!checked && !preferences.clear()) {
                          notify("浏览器未能清除保存的密码", true);
                          return;
                        }
                        setRemember(checked);
                        if (!checked) {
                          setHasSaved(false);
                          setAutomatic(false);
                        }
                      }}
                    />
                    <Checkbox
                      label="自动登录"
                      checked={automatic}
                      disabled={busy}
                      onChange={(e) => {
                        const checked = e.currentTarget.checked;
                        if (hasSaved && !preferences.setAutomatic(checked)) {
                          notify("浏览器未能保存登录选项", true);
                          return;
                        }
                        setAutomatic(checked);
                        if (checked) setRemember(true);
                      }}
                    />
                  </Group>
                  <Group justify="space-between" gap={6}>
                    <Text fz={11} c="dimmed">
                      仅保存在当前浏览器
                    </Text>
                    {hasSaved && (
                      <Button
                        variant="subtle"
                        size="compact-xs"
                        onClick={forgetSaved}
                        disabled={busy}
                      >
                        清除登录信息
                      </Button>
                    )}
                  </Group>
                </Stack>
                {error && (
                  <Alert color="red" role="alert">
                    {error}
                  </Alert>
                )}
                <Button
                  type="submit"
                  loading={busy}
                  fullWidth
                  size="md"
                  mt={5}
                  rightSection={<ArrowRight size={16} />}
                >
                  {autoBusy ? "正在自动登录" : "登录设备"}
                </Button>
                {autoBusy && (
                  <Button variant="default" onClick={cancelAuto}>
                    取消自动登录
                  </Button>
                )}
              </Stack>
            </form>
            <UnstyledButton
              disabled={busy}
              onClick={() => setAdvanced(!advanced)}
            >
              <Group gap={5}>
                <Text size="xs" c="dimmed">
                  {advanced ? "收起服务 Token 登录" : "使用服务 Token 登录"}
                </Text>
                <ChevronRight size={12} />
              </Group>
            </UnstyledButton>
            <AnimatePresence>
              {advanced && (
                <motion.div
                  initial={{ height: 0, opacity: 0 }}
                  animate={{ height: "auto", opacity: 1 }}
                  exit={{ height: 0, opacity: 0 }}
                  className="panel-body"
                >
                  <Stack gap="sm">
                    <PasswordInput
                      label="服务 Token"
                      autoComplete="off"
                      value={token}
                      onChange={(e) => setToken(e.target.value)}
                    />
                    <Button
                      variant="light"
                      loading={busy}
                      onClick={() => void submit(null, true)}
                    >
                      连接
                    </Button>
                  </Stack>
                </motion.div>
              )}
            </AnimatePresence>
            <GithubCard />
          </Stack>
        </motion.div>
      </Center>
    </div>
  );
}
function App() {
  const [connection, setConnection] = useState(null),
    [page, setPage] = useState(
      pages.find((x) => x.id === location.hash.slice(1))?.id ?? "overview",
    ),
    [mobile, setMobile] = useState(false),
    [dangerAccepted, setDangerAccepted] = useState(false),
    [refreshTick, setRefreshTick] = useState(0),
    [online, setOnline] = useState(true),
    [networkTarget, setNetworkTarget] = useState(null);
  const desktop = useMediaQuery("(min-width: 48em)"),
    reducedMotion = useReducedMotion();
  useEffect(() => {
    if (desktop) setMobile(false);
  }, [desktop]);
  const {
      ask,
      modal,
      close,
      allowNavigation,
      visible: dialogVisible,
    } = useDialogs(),
    services = useRef(),
    { toggleColorScheme } = useMantineColorScheme();
  if (!services.current) {
    const api = new Client(new URL("./", location.href), () =>
      logout("会话已过期，请重新登录"),
    );
    services.current = { api, efs: new EfsClient(api) };
  }
  const { api, efs } = services.current;
  const signalCache = (services.current.signalCache ??= new SignalSummaryStore(
    api.base.href,
  ));
  const observeSnapshot = useCallback(
    (snapshot) => signalCache.observe(snapshot),
    [signalCache],
  );
  const guards = useRef(new Set()),
    pageRef = useRef(page),
    bypass = useRef(null),
    navigationRevision = useRef(0);
  pageRef.current = page;
  useEffect(() => {
    window.scrollTo({ top: 0, left: 0, behavior: "instant" });
  }, [page]);
  const registerLeaveGuard = useCallback((check) => {
    guards.current.add(check);
    return () => guards.current.delete(check);
  }, []);
  async function canLeave() {
    if (!(await allowNavigation())) return false;
    return (
      ![...guards.current].some((check) => check()) ||
      (await ask({
        title: "放弃未保存的修改？",
        body: <Text size="sm">页面中的修改尚未提交。</Text>,
        confirm: "放弃修改",
      }))
    );
  }

  function logout(message) {
    createLoginPreferences(api.base).suppressAuto();
    setMobile(false);
    toast.clear();
    close(false, true);
    api.reset();
    signalCache.detach();
    guards.current.clear();
    setConnection(null);
    setDangerAccepted(false);
    setNetworkTarget(null);
    if (message) notify(message, true);
  }
  async function connected(options = {}) {
    const [snapshot, capabilities] = await Promise.all([
      api.request("/state", options),
      api.request("/capabilities", options),
    ]);
    signalCache.attach(snapshot);
    const url = new URL("ui/", api.base).href,
      displayName = deviceProfiles().find((p) => p.url === url)?.name || "";
    saveDeviceProfile(
      url,
      displayName ||
        snapshot.device?.market_name ||
        snapshot.device?.model_name ||
        "datad",
    );
    setConnection({ snapshot, capabilities, displayName });
    setOnline(true);
  }
  async function navigate(id) {
    if (id === pageRef.current) {
      setMobile(false);
      return;
    }
    if (!pages.some((x) => x.id === id)) return;
    const ticket = ++navigationRevision.current;
    const allowed = await canLeave();
    if (ticket !== navigationRevision.current || !allowed) return;
    if (["efs", "nv"].includes(pageRef.current) && !["efs", "nv"].includes(id))
      setDangerAccepted(false);
    bypass.current = id;
    location.hash = id;
    setPage(id);
    setMobile(false);
    close(false, true);
  }
  useEffect(() => {
    const update = async () => {
      const id = pages.some((x) => x.id === location.hash.slice(1))
        ? location.hash.slice(1)
        : "overview";
      if (bypass.current === id) {
        bypass.current = null;
        return;
      }
      const ticket = ++navigationRevision.current;
      if (id !== pageRef.current) {
        const allowed = await canLeave();
        if (ticket !== navigationRevision.current) return;
        if (!allowed) {
          history.replaceState(null, "", "#" + pageRef.current);
          return;
        }
      }
      if (
        ["efs", "nv"].includes(pageRef.current) &&
        !["efs", "nv"].includes(id)
      )
        setDangerAccepted(false);
      setPage(id);
    };
    addEventListener("hashchange", update);
    const exit = () => api.reset(),
      restore = (e) => {
        if (e.persisted) logout();
      };
    addEventListener("pagehide", exit);
    addEventListener("pageshow", restore);
    return () => {
      removeEventListener("hashchange", update);
      removeEventListener("pagehide", exit);
      removeEventListener("pageshow", restore);
    };
  }, []);
  const item = pages.find((x) => x.id === page),
    model =
      connection?.displayName ||
      connection?.snapshot.device?.market_name ||
      connection?.snapshot.device?.model_name ||
      "已连接设备";
  const context = {
    api,
    efs,
    ask,
    dialogVisible,
    notify,
    connection,
    logout,
    renameConnection: (name) =>
      setConnection((c) => ({ ...c, displayName: name })),
    dangerAccepted,
    acceptDanger: () => setDangerAccepted(true),
    navigate,
    refreshTick,
    setOnline,
    registerLeaveGuard,
    networkTarget,
    setNetworkTarget,
    signalCache,
    observeSnapshot,
  };
  const navigationContent = (
    <Stack className="navigation-content" h="100%" gap={20}>
      <ScrollArea className="navigation-links" type="auto" offsetScrollbars>
        <Stack gap={7}>
          <Text className="nav-caption" size="10px" lts={1.5} px={12} mb={6}>
            工作空间
          </Text>
          {pages
            .filter((x) =>
              [
                "overview",
                "params",
                "bands",
                "sms",
                "settings",
                "signal",
                "efs",
                "nv",
              ].includes(x.id),
            )
            .map(({ id, label, icon: Icon }) => (
              <NavLink
                key={id}
                component="button"
                label={label}
                active={page === id}
                onClick={() => navigate(id)}
                leftSection={<Icon size={18} strokeWidth={1.7} />}
                rightSection={page === id ? <ChevronRight size={13} /> : null}
                variant="light"
                className="nav-link"
              />
            ))}
        </Stack>
      </ScrollArea>
      <Box mt="auto">
        <div className="sidebar-device">
          <Group gap={10} wrap="nowrap">
            <div className="device-glyph">
              <Radio size={18} />
            </div>
            <Box miw={0}>
              <Text size="sm" fw={500} className="value">
                {model}
              </Text>
              <Group gap={5} mt={5}>
                <span className={"status-dot " + (!online ? "offline" : "")} />
                <Text fz={11} className="sidebar-muted">
                  {online ? "设备已连接" : "连接中断"}
                </Text>
              </Group>
            </Box>
          </Group>
        </div>
        <GithubCard compact />
        <Group
          justify="space-between"
          className="sidebar-footer"
          mt={17}
          px={5}
        >
          <Text fz={10}>
            datad {display(connection?.snapshot.datad?.version)}
          </Text>
          <Text fz={10}>本地管理</Text>
        </Group>
      </Box>
    </Stack>
  );
  return (
    <Services.Provider value={context}>
      {!connection ? (
        <Login api={api} onConnected={connected} />
      ) : (
        <AppShell
          layout="alt"
          header={{ height: 65 }}
          navbar={{
            width: 236,
            breakpoint: "sm",
            collapsed: { mobile: true },
          }}
          padding={0}
          transitionDuration={180}
          className="app-shell"
        >
          <AppShell.Navbar className="app-sidebar" p={20} visibleFrom="sm">
            <Stack h="100%" gap={28}>
              <Box className="sidebar-brand" pt={8} px={8}>
                <Brand />
              </Box>
              <Box style={{ flex: 1, minHeight: 0 }}>{navigationContent}</Box>
            </Stack>
          </AppShell.Navbar>
          <Drawer
            opened={mobile && !desktop}
            onClose={() => setMobile(false)}
            title={
              <Box className="sidebar-brand">
                <Brand />
              </Box>
            }
            position="left"
            size="min(320px, calc(100vw - 48px))"
            closeButtonProps={{ "aria-label": "关闭导航" }}
            overlayProps={{ backgroundOpacity: 0.4, blur: 2 }}
            transitionProps={{ duration: reducedMotion ? 0 : 180 }}
            classNames={{
              content: "app-sidebar mobile-navigation",
              header: "mobile-nav-header",
              body: "mobile-nav-body",
              close: "mobile-nav-close",
            }}
          >
            {navigationContent}
          </Drawer>
          <AppShell.Header px={{ base: "md", md: 32 }} className="app-header">
            <Group justify="space-between" h="100%">
              <Group gap="sm">
                <Burger
                  opened={mobile}
                  onClick={() => setMobile(!mobile)}
                  hiddenFrom="sm"
                  size="sm"
                  aria-label="打开导航"
                  aria-expanded={mobile}
                />
                <Breadcrumbs
                  separator={<ChevronRight size={12} />}
                  separatorMargin={10}
                >
                  <Text size="xs" c="dimmed" visibleFrom="sm">
                    工作空间
                  </Text>
                  <Text size="xs" fw={500}>
                    {item.label}
                  </Text>
                </Breadcrumbs>
              </Group>
              <Group gap={14}>
                <Group gap={6} visibleFrom="xs">
                  <span
                    className={"status-dot " + (!online ? "offline" : "")}
                  />
                  <Text fz={11} c="dimmed">
                    {online ? "设备已连接" : "连接中断"}
                  </Text>
                </Group>
                <div className="header-divider" />
                <Tooltip label="切换明暗主题">
                  <ActionIcon
                    variant="subtle"
                    color="gray"
                    aria-label="切换明暗主题"
                    onClick={toggleColorScheme}
                  >
                    <SunMoon size={17} />
                  </ActionIcon>
                </Tooltip>
                <Tooltip label="退出登录">
                  <ActionIcon
                    variant="subtle"
                    color="gray"
                    aria-label="退出登录"
                    onClick={() => logout()}
                  >
                    <LogOut size={17} />
                  </ActionIcon>
                </Tooltip>
              </Group>
            </Group>
          </AppShell.Header>
          <AppShell.Main>
            <Container size={1450} className="main-container">
              <Group justify="space-between" align="center" mb={25} gap="sm">
                <Box>
                  <Title
                    order={1}
                    fz={{ base: 25, sm: 27 }}
                    fw={650}
                    className="page-title"
                  >
                    {item.label}
                  </Title>
                  <Text size="sm" c="dimmed" mt={7}>
                    {item.caption}
                  </Text>
                </Box>
                {page === "overview" && (
                  <Button
                    variant="default"
                    size="xs"
                    leftSection={<RefreshCw size={13} />}
                    onClick={() => setRefreshTick((n) => n + 1)}
                  >
                    刷新状态
                  </Button>
                )}
              </Group>
              <Suspense
                fallback={
                  <Stack>
                    <Skeleton h={125} radius="lg" />
                    <Skeleton h={300} radius="lg" />
                  </Stack>
                }
              >
                <AnimatePresence mode="wait" initial={false}>
                  <motion.div
                    key={page}
                    initial={{ opacity: 0, y: 7 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -4 }}
                    transition={{ duration: 0.16, ease: "easeOut" }}
                  >
                    <PageContent page={page} />
                  </motion.div>
                </AnimatePresence>
              </Suspense>
              <Group
                justify="space-between"
                className="workspace-footer"
                mt={25}
              >
                <Text size="10px" c="dimmed">
                  ZWRT DATAD
                </Text>
                <Text size="10px" c="dimmed">
                  {model} · 设备管理控制台
                </Text>
              </Group>
            </Container>
          </AppShell.Main>
        </AppShell>
      )}
      {connection && <OemReminder key={api.generation} />}
      {modal}
    </Services.Provider>
  );
}
function EfsGate({ children }) {
  const {
      dangerAccepted,
      acceptDanger,
      efs: rootEfs,
      navigate,
    } = useServices(),
    pageApi = usePageApi(),
    efs = useMemo(() => rootEfs.scoped(pageApi), [rootEfs, pageApi]),
    [status, setStatus] = useState(null),
    [error, setError] = useState(""),
    [attempt, setAttempt] = useState(0),
    [riskOpen, setRiskOpen] = useState(true),
    accepted = useRef(false),
    reduce = useReducedMotion();
  useEffect(() => {
    if (!dangerAccepted) return;
    let active = true;
    efs
      .exclusive(() => efs.request("status", null))
      .then((data) => {
        if (active) {
          if (!data.available) throw new Error("后端未包含 EFS worker");
          setStatus(data);
        }
      })
      .catch((e) => {
        if (active) setError(e.message);
      });
    return () => {
      active = false;
    };
  }, [dangerAccepted, attempt]);
  function finishRisk(yes) {
    accepted.current = yes;
    setRiskOpen(false);
  }
  if (!dangerAccepted)
    return (
      <>
        <Panel title="EFS / NV" icon={ShieldAlert}>
          <Text size="sm" c="dimmed">
            确认操作风险后，连接基带文件服务。
          </Text>
        </Panel>
        <Modal
          opened={riskOpen}
          onClose={() => finishRisk(false)}
          onExitTransitionEnd={() =>
            accepted.current ? acceptDanger() : navigate("overview")
          }
          title={<Text fw={650}>EFS 操作风险提示</Text>}
          centered
          radius="lg"
          padding="xl"
          overlayProps={{ backgroundOpacity: 0.4, blur: 5 }}
          transitionProps={{ transition: "pop", duration: reduce ? 0 : 180 }}
        >
          <Stack gap="lg">
            <Center>
              <span className="risk-icon">
                <ShieldAlert size={27} strokeWidth={1.5} />
              </span>
            </Center>
            <Text size="sm" lh={1.9}>
              EFS
              包含基带关键配置。错误修改可能导致网络异常、数据丢失，甚至会损坏设备。
            </Text>
            <Alert color="orange" variant="light" radius="md">
              <Text size="xs">
                根目录及直属条目只读。删除目录仅支持空目录，不提供递归删除。
              </Text>
            </Alert>
            <Group justify="flex-end">
              <Button variant="default" onClick={() => finishRisk(false)}>
                取消
              </Button>
              <CountdownButton
                seconds={5}
                color="red"
                onClick={() => finishRisk(true)}
              >
                我已了解
              </CountdownButton>
            </Group>
          </Stack>
        </Modal>
      </>
    );
  if (error)
    return (
      <Alert color="red" title="EFS 服务不可用">
        <Stack>
          <Text size="sm">{error}</Text>
          <Button
            variant="light"
            onClick={() => {
              setError("");
              setAttempt((n) => n + 1);
            }}
          >
            重试
          </Button>
        </Stack>
      </Alert>
    );
  if (!status)
    return (
      <Center py={70}>
        <Group>
          <Loader size="sm" />
          <Text c="dimmed" size="sm">
            正在连接 EFS 服务
          </Text>
        </Group>
      </Center>
    );
  return <EfsStatus.Provider value={status}>{children}</EfsStatus.Provider>;
}
function ModalScrollArea(props) {
  return (
    <ScrollArea.Autosize
      {...props}
      type="auto"
      scrollbars="y"
      scrollbarSize={5}
      classNames={{ content: "dialog-scroll-content" }}
    />
  );
}
const theme = createTheme({
  primaryColor: "blue",
  primaryShade: 6,
  colors: {
    dark: [
      "#dce4ef",
      "#b6c2d3",
      "#929fb3",
      "#75869d",
      "#435168",
      "#344157",
      "#273346",
      "#202a39",
      "#192230",
      "#111821",
    ],
  },
  defaultRadius: "md",
  fontFamily:
    '"Segoe UI", "Microsoft YaHei", "PingFang SC", "Noto Sans CJK SC", Arial, sans-serif',
  fontSizes: { xs: "11px", sm: "13px", md: "14px", lg: "16px", xl: "19px" },
  radius: { xs: "5px", sm: "7px", md: "10px", lg: "15px", xl: "22px" },
  headings: {
    fontFamily:
      '"Segoe UI", "Microsoft YaHei", "PingFang SC", Arial, sans-serif',
    fontWeight: "600",
  },
  components: {
    Button: Button.extend({ defaultProps: { radius: "md", fw: 500 } }),
    Card: Card.extend({ defaultProps: { radius: "lg" } }),
    Modal: Modal.extend({
      defaultProps: { padding: "lg", scrollAreaComponent: ModalScrollArea },
    }),
    TextInput: TextInput.extend({ defaultProps: { radius: "md" } }),
  },
});
createRoot(document.getElementById("root")).render(
  <MantineProvider theme={theme} defaultColorScheme="auto">
    <MotionConfig
      reducedMotion="user"
      transition={{ type: "spring", stiffness: 380, damping: 32 }}
    >
      <ToastHost />
      <App />
    </MotionConfig>
  </MantineProvider>,
);
