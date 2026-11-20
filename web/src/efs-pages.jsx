import React, { useContext, useEffect, useMemo, useRef, useState } from "react";
import {
  ActionIcon,
  Alert,
  Anchor,
  Badge,
  Box,
  Breadcrumbs,
  Button,
  Center,
  Code,
  FileButton,
  Group,
  Loader,
  Menu,
  NumberInput,
  Pagination,
  ScrollArea,
  Select,
  SegmentedControl,
  SimpleGrid,
  Stack,
  Table,
  Text,
  Textarea,
  TextInput,
  Tooltip,
} from "@mantine/core";
import {
  ArrowDownToLine,
  ArrowUpFromLine,
  ChevronLeft,
  ChevronRight,
  Database,
  File,
  Folder,
  FolderPlus,
  MoreHorizontal,
  Pencil,
  RefreshCw,
  Search,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { toast } from "../toasts.js";
import {
  bytesFromHex,
  readBoundedBinary,
  childPath,
  download,
  formatBytes,
  hex,
  imeiFromNv,
  mutablePath,
  nvConfig,
  validPath,
} from "../core.js";
import { useDirtyGuard, usePageApi } from "./hooks.jsx";
import { checkedListing, checkedPage, protectedBackend } from "../efs.js";
import { EfsStatus, InfoGrid, Panel, useServices } from "./ui.jsx";

function useAlive() {
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  return alive;
}
function PromptInput({ initial = "", label, onValue, multiline = false }) {
  const [value, setValue] = useState(initial);
  const input = {
    label,
    value,
    onChange: (e) => {
      setValue(e.target.value);
      onValue(e.target.value);
    },
    "data-autofocus": true,
  };
  return multiline ? (
    <Textarea
      {...input}
      minRows={12}
      maxRows={24}
      autosize
      spellCheck={false}
      styles={{ input: { fontFamily: "monospace", fontSize: 12 } }}
    />
  ) : (
    <TextInput {...input} />
  );
}
export function EfsPage() {
  const { efs: rootEfs, ask, notify } = useServices(),
    pageApi = usePageApi(),
    efs = useMemo(() => rootEfs.scoped(pageApi), [rootEfs, pageApi]),
    status = useContext(EfsStatus),
    alive = useAlive();
  const [path, setPath] = useState("/"),
    [entries, setEntries] = useState([]),
    [filter, setFilter] = useState(""),
    [busy, setBusy] = useState(false),
    [error, setError] = useState(""),
    [refresh, setRefresh] = useState(0),
    [page, setPage] = useState(1),
    [sort, setSort] = useState("name"),
    [operationLabel, setOperationLabel] = useState(""),
    [transfer, setTransfer] = useState(null);
  useDirtyGuard(busy);
  useEffect(() => setPage(1), [path, filter]);
  useEffect(() => {
    let current = true;
    setBusy(true);
    setError("");
    efs
      .exclusive(() => efs.request("list", path))
      .then((reply) => {
        const list = checkedListing(reply.data, path);
        if (current) {
          setEntries(list);
          setFilter("");
        }
      })
      .catch((e) => {
        if (current) setError(e.message);
      })
      .finally(() => {
        if (current) setBusy(false);
      });
    return () => {
      current = false;
    };
  }, [path, refresh, efs]);
  const writable = protectedBackend(status),
    parts = path === "/" ? [] : path.slice(1).split("/");
  async function work(operation, label = "正在处理") {
    if (busy) return;
    setBusy(true);
    toast.clear();
    setOperationLabel(label);
    setTransfer(null);
    try {
      return await operation();
    } catch (e) {
      if (alive.current) {
        notify(
          e.message +
            (efs.uncertainUntil > Date.now()
              ? "。结果未确认，请先读取核对，勿重复提交。"
              : ""),
          true,
        );
      }
      return null;
    } finally {
      if (alive.current) {
        setBusy(false);
        setOperationLabel("");
        setTransfer(null);
      }
    }
  }
  async function mutate(kind, target, body, bytes) {
    return work(async () => {
      if (["rename", "delete"].includes(kind)) await checkEntry(target);
      await efs.mutate(kind, target, body, bytes);
      notify("操作成功");
      if (alive.current) setRefresh((n) => n + 1);
      return true;
    });
  }
  async function checkEntry(target) {
    const info = await efs.exclusive(() => efs.request("stat", target)),
      f = info.data;
    if (
      f?.path !== target ||
      !["directory", "file", "item"].includes(f.type) ||
      (f.type !== "directory" && Number(f.links) !== 1)
    )
      throw new Error("此条目类型或链接数不允许修改");
    return f;
  }
  async function getBytes(target) {
    return efs.exclusive(async () => {
      const response = await efs.request("download", target, { binary: true });
      if (
        !(response.headers.get("content-type") ?? "").startsWith(
          "application/octet-stream",
        )
      )
        throw new Error("下载响应不匹配");
      return readBoundedBinary(response, 1048576, (done, total) => {
        if (alive.current) setTransfer({ done, total });
      });
    });
  }
  async function saveFile(target) {
    await work(async () => {
      const bytes = await getBytes(target);
      if (alive.current) {
        download(bytes, target.split("/").at(-1));
        notify("下载已交给浏览器保存");
      }
    });
  }
  async function view(entry) {
    const target = childPath(path, entry.name);
    if (entry.type === "directory") {
      setPath(target);
      return;
    }
    await ask({
      title: entry.name,
      subtitle: target,
      body: (
        <FilePreview path={target} download={() => void saveFile(target)} />
      ),
      confirm: null,
      cancel: "关闭",
      wide: true,
    });
  }
  async function jump() {
    let target = path;
    if (
      await ask({
        title: "跳转目录",
        body: (
          <PromptInput
            initial={path}
            label="完整绝对路径"
            onValue={(v) => (target = v)}
          />
        ),
        validate: () => {
          if (!validPath(target)) throw new Error("EFS 路径无效");
        },
        confirm: "打开",
      })
    ) {
      if (!validPath(target)) {
        notify("EFS 路径无效", true);
        return;
      }
      setPath(target);
    }
  }
  async function edit(entry, mode = "text") {
    const target = childPath(path, entry.name);
    let value, original;
    const loaded = await work(async () => {
      const info = await checkEntry(target);
      if (info.type === "directory") throw new Error("目录不能作为文件编辑");
      const bytes = await getBytes(target);
      original = bytes;
      if (bytes.length > Math.min(32768, status.limits?.write_bytes ?? 0))
        throw new Error("完整文件超过单次编辑限制");
      if (mode === "hex") value = hex(bytes);
      else
        try {
          value = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
        } catch {
          throw new Error("此文件不是完整 UTF-8 文本，请使用 HEX 编辑");
        }
      return true;
    });
    if (!loaded || !alive.current) return;
    let changed = value;
    if (
      await ask({
        title: mode === "hex" ? "编辑 HEX" : "编辑文本文件",
        subtitle: target,
        body: (
          <PromptInput
            initial={value}
            label={
              mode === "hex"
                ? "连续 HEX（空内容会清空文件）"
                : "UTF-8 文本（保存会完整替换文件）"
            }
            onValue={(v) => (changed = v)}
            multiline
          />
        ),
        wide: true,
        dirty: () => changed !== value,
        validate: () => {
          const data =
            mode === "hex"
              ? bytesFromHex(changed)
              : new TextEncoder().encode(changed);
          if (data.length > Math.min(32768, status.limits?.write_bytes ?? 0))
            throw new Error("完整文件超过单次编辑限制");
        },
        confirm: "保存文件",
        danger: true,
      })
    ) {
      await work(async () => {
        const bytes =
          mode === "hex"
            ? bytesFromHex(changed)
            : new TextEncoder().encode(changed);
        const current = await getBytes(target);
        if (
          current.length !== original.length ||
          current.some((v, i) => v !== original[i])
        )
          throw new Error("文件内容已变化，请重新打开后编辑");
        await efs.mutate("write", target, { hex: hex(bytes) }, bytes.length);
        notify("文件已保存");
        if (alive.current) setRefresh((n) => n + 1);
      });
    }
  }
  async function rename(entry) {
    const target = childPath(path, entry.name);
    let destination = target;
    if (
      await ask({
        title: "重命名 / 移动",
        subtitle: target,
        body: (
          <PromptInput
            initial={target}
            label="目标绝对路径"
            onValue={(v) => (destination = v)}
          />
        ),
        dirty: () => destination !== target,
        validate: () => {
          if (
            !mutablePath(destination) ||
            destination === target ||
            destination.startsWith(target + "/")
          )
            throw new Error("移动目标无效");
        },
        confirm: "移动",
      })
    )
      await mutate("rename", target, { destination });
  }
  async function remove(entry) {
    const target = childPath(path, entry.name),
      directory = entry.type === "directory";
    if (
      await ask({
        title: directory ? "删除空目录？" : "删除文件？",
        subtitle: target,
        body: (
          <Alert color="red">
            删除无法撤销。目录仅支持为空时删除，不会递归删除。
          </Alert>
        ),
        confirm: "删除",
        danger: true,
      })
    )
      await mutate("delete", target, { directory });
  }
  async function upload(file) {
    if (!file) return;
    try {
      const target = childPath(path, file.name);
      if (file.size > Math.min(1048576, status.limits?.transfer_bytes ?? 0))
        throw new Error("上传超过设备限制（最大 1 MiB）");
      if (
        await ask({
          title: "上传文件？",
          subtitle: target,
          body: (
            <Text size="sm">
              {formatBytes(file.size)}。已有同名文件将被完整覆盖。
            </Text>
          ),
          confirm: "上传",
          danger: true,
        })
      )
        await mutate(
          "upload",
          target,
          new Uint8Array(await file.arrayBuffer()),
          file.size,
        );
    } catch (e) {
      notify(e.message, true);
    }
  }
  async function mkdir() {
    let name = "";
    if (
      await ask({
        title: "新建目录",
        subtitle: path,
        body: <PromptInput label="目录名称" onValue={(v) => (name = v)} />,
        dirty: () => !!name,
        validate: () => {
          childPath(path, name);
        },
        confirm: "创建",
      })
    ) {
      try {
        await mutate("mkdir", childPath(path, name), {});
      } catch (e) {
        notify(e.message, true);
      }
    }
  }
  const visible = entries
    .filter((e) => e.name?.toLowerCase().includes(filter.toLowerCase()))
    .sort(
      (a, b) =>
        (a.type === "directory" ? 0 : 1) - (b.type === "directory" ? 0 : 1) ||
        (sort === "size"
          ? Number(b.size) - Number(a.size)
          : sort === "time"
            ? Number(b.mtime) - Number(a.mtime)
            : 0) ||
        a.name.toLowerCase().localeCompare(b.name.toLowerCase()),
    );
  const pageCount = Math.max(1, Math.ceil(visible.length / 50)),
    currentPage = Math.min(page, pageCount),
    pageEntries = visible.slice((currentPage - 1) * 50, currentPage * 50);
  return (
    <Stack gap="lg">
      {!writable && (
        <Alert color="orange">后端缺少完整写保护能力，当前仅可读取。</Alert>
      )}
      <Panel
        title="文件浏览"
        icon={Folder}
        right={
          <Badge color={writable ? "teal" : "gray"} variant="light" size="sm">
            {writable ? "写保护已启用" : "只读"}
          </Badge>
        }
      >
        <Stack gap="md">
          <Group justify="space-between" gap="sm">
            <Breadcrumbs separator="/" separatorMargin="xs">
              <Anchor
                component="button"
                size="sm"
                onClick={() => setPath("/")}
                disabled={busy}
              >
                根目录
              </Anchor>
              {parts.map((part, i) => (
                <Anchor
                  component="button"
                  size="sm"
                  key={i}
                  onClick={() => setPath("/" + parts.slice(0, i + 1).join("/"))}
                  disabled={busy}
                >
                  {part}
                </Anchor>
              ))}
            </Breadcrumbs>
            {busy ? (
              <Group gap={6}>
                <Loader size={14} />
                <Text size="xs" c="dimmed">
                  {operationLabel || "正在读取"}
                  {transfer
                    ? ` · ${formatBytes(transfer.done)}${transfer.total ? " / " + formatBytes(transfer.total) : ""}`
                    : ""}
                </Text>
              </Group>
            ) : (
              <Text size="xs" c="dimmed">
                {entries.length} 项
              </Text>
            )}
          </Group>
          <Text size="xs" c="dimmed">
            根目录及直属条目只读 · 删除目录仅支持空目录
          </Text>
          <Group gap="sm" wrap="wrap">
            <Button
              variant="default"
              disabled={busy}
              onClick={() => void jump()}
            >
              跳转目录
            </Button>
            <Select
              aria-label="文件排序"
              w={120}
              size="sm"
              value={sort}
              allowDeselect={false}
              onChange={setSort}
              data={[
                { value: "name", label: "按名称" },
                { value: "size", label: "按大小" },
                { value: "time", label: "按时间" },
              ]}
            />
            <TextInput
              placeholder="筛选当前目录"
              aria-label="筛选当前目录"
              leftSection={<Search size={15} />}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              flex={1}
              miw={150}
            />
            <Tooltip label="刷新目录">
              <ActionIcon
                size="lg"
                variant="default"
                aria-label="刷新目录"
                disabled={busy}
                onClick={() => setRefresh((n) => n + 1)}
              >
                <RefreshCw size={16} />
              </ActionIcon>
            </Tooltip>
            <Button
              variant="default"
              leftSection={<FolderPlus size={16} />}
              onClick={() => void mkdir()}
              disabled={busy || path === "/" || !writable}
            >
              新建目录
            </Button>
            <FileButton onChange={(file) => void upload(file)}>
              {(props) => (
                <Button
                  {...props}
                  leftSection={<ArrowUpFromLine size={16} />}
                  disabled={busy || path === "/" || !writable}
                >
                  上传文件
                </Button>
              )}
            </FileButton>
          </Group>
          {error ? (
            <Alert color="red" title="目录读取失败">
              {error}
            </Alert>
          ) : (
            <Table.ScrollContainer minWidth={550}>
              <Table
                verticalSpacing="md"
                horizontalSpacing="md"
                highlightOnHover
                className="file-table"
              >
                <Table.Thead>
                  <Table.Tr>
                    <Table.Th>文件名称</Table.Th>
                    <Table.Th>大小</Table.Th>
                    <Table.Th>类型</Table.Th>
                    <Table.Th>修改时间</Table.Th>
                    <Table.Th w={44} />
                  </Table.Tr>
                </Table.Thead>
                <Table.Tbody>
                  {pageEntries.map((entry, i) => {
                    const safe =
                        entry.nameValid &&
                        ["directory", "file", "item"].includes(entry.type),
                      target = safe ? childPath(path, entry.name) : "",
                      canWrite =
                        safe &&
                        writable &&
                        mutablePath(target) &&
                        (entry.type === "directory" ||
                          entry.links == null ||
                          Number(entry.links) === 1),
                      Icon = entry.type === "directory" ? Folder : File;
                    return (
                      <Table.Tr key={entry.name_hex || i}>
                        <Table.Td>
                          <Group gap="sm" wrap="nowrap">
                            <span
                              className={
                                "file-type-icon " +
                                (entry.type === "directory" ? "folder" : "")
                              }
                            >
                              <Icon size={18} strokeWidth={1.6} />
                            </span>
                            <Box>
                              <Anchor
                                component="button"
                                className="file-name"
                                size="sm"
                                disabled={!safe || busy}
                                onClick={() => void view(entry)}
                              >
                                {entry.name}
                              </Anchor>
                              {!entry.nameValid && (
                                <Text size="xs" c="orange">
                                  名称无法安全寻址
                                </Text>
                              )}
                            </Box>
                          </Group>
                        </Table.Td>
                        <Table.Td>
                          <Text size="xs" c="dimmed">
                            {entry.type === "directory"
                              ? "—"
                              : formatBytes(entry.size)}
                          </Text>
                        </Table.Td>
                        <Table.Td>
                          <Badge
                            size="xs"
                            color="gray"
                            variant="light"
                            radius="sm"
                          >
                            {{
                              directory: "目录",
                              file: "文件",
                              item: "Item File",
                              symlink: "符号链接",
                            }[entry.type] ?? entry.type}
                          </Badge>
                        </Table.Td>
                        <Table.Td>
                          <Text size="xs" c="dimmed">
                            {Number(entry.mtime) > 0
                              ? new Date(
                                  Number(entry.mtime) * 1000,
                                ).toLocaleString()
                              : "—"}
                          </Text>
                        </Table.Td>
                        <Table.Td>
                          <Menu shadow="md" position="bottom-end" withinPortal>
                            <Menu.Target>
                              <ActionIcon
                                variant="subtle"
                                color="gray"
                                aria-label={"操作 " + entry.name}
                                disabled={!safe || busy}
                              >
                                <MoreHorizontal size={18} />
                              </ActionIcon>
                            </Menu.Target>
                            <Menu.Dropdown>
                              <Menu.Label>
                                {canWrite ? "条目操作" : "只读条目"}
                              </Menu.Label>
                              {entry.type !== "directory" && (
                                <Menu.Item
                                  leftSection={<ArrowDownToLine size={15} />}
                                  onClick={() => void saveFile(target)}
                                >
                                  下载文件
                                </Menu.Item>
                              )}
                              {canWrite && (
                                <>
                                  {entry.type !== "directory" &&
                                    Number(entry.size) <= 32768 && (
                                      <>
                                        <Menu.Item
                                          leftSection={<Pencil size={15} />}
                                          onClick={() => void edit(entry)}
                                        >
                                          编辑文本
                                        </Menu.Item>
                                        <Menu.Item
                                          leftSection={<Pencil size={15} />}
                                          onClick={() =>
                                            void edit(entry, "hex")
                                          }
                                        >
                                          编辑 HEX
                                        </Menu.Item>
                                      </>
                                    )}
                                  <Menu.Item
                                    leftSection={<Pencil size={15} />}
                                    onClick={() => void rename(entry)}
                                  >
                                    重命名 / 移动
                                  </Menu.Item>
                                  <Menu.Divider />
                                  <Menu.Item
                                    color="red"
                                    leftSection={<Trash2 size={15} />}
                                    onClick={() => void remove(entry)}
                                  >
                                    {entry.type === "directory"
                                      ? "删除空目录"
                                      : "删除文件"}
                                  </Menu.Item>
                                </>
                              )}
                            </Menu.Dropdown>
                          </Menu>
                        </Table.Td>
                      </Table.Tr>
                    );
                  })}
                </Table.Tbody>
              </Table>
            </Table.ScrollContainer>
          )}
          {!busy && !error && !visible.length && (
            <Center py="xl">
              <Text size="sm" c="dimmed">
                {entries.length ? "没有匹配的条目" : "此目录为空"}
              </Text>
            </Center>
          )}
          <Group justify="space-between">
            <Text fz={10} c="dimmed">
              {visible.length} 个条目{pageCount > 1 ? " · 每页 50 项" : ""}
            </Text>
            {pageCount > 1 && (
              <Pagination
                size="xs"
                value={currentPage}
                onChange={setPage}
                total={pageCount}
              />
            )}
          </Group>
        </Stack>
      </Panel>
    </Stack>
  );
}
function FilePreview({ path, download: save }) {
  const { efs: rootEfs } = useServices(),
    pageApi = usePageApi(),
    efs = useMemo(() => rootEfs.scoped(pageApi), [rootEfs, pageApi]),
    [meta, setMeta] = useState(null),
    [offset, setOffset] = useState(0),
    [page, setPage] = useState(null),
    [mode, setMode] = useState("text"),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false);
  const previewViewport = useRef(null);
  useEffect(() => {
    previewViewport.current?.scrollTo({ top: 0, left: 0, behavior: "instant" });
  }, [mode, page?.offset]);
  useEffect(() => {
    let active = true;
    setBusy(true);
    setError("");
    efs
      .exclusive(async () => {
        const reply = await efs.api.request(
          `/efs/read?path=${encodeURIComponent(path)}&offset=${offset}&length=4096`,
          { timeout: 125000 },
        );
        if (offset === 0) {
          try {
            const r = await efs.request("stat", path);
            if (active && r.data?.path === path) setMeta(r.data);
          } catch {}
        }
        return reply;
      })
      .then((reply) => {
        const data = checkedPage(reply.data, offset, 4096);
        if (active) setPage(data);
      })
      .catch((e) => {
        if (active) setError(e.message);
      })
      .finally(() => {
        if (active) setBusy(false);
      });
    return () => {
      active = false;
    };
  }, [path, offset, efs]);

  let text = "";
  if (page) {
    const bytes = bytesFromHex(page.hex);
    if (mode === "text") {
      try {
        text = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
      } catch {
        text = "当前分页无法完整转换为 UTF-8，请使用 HEX 查看。";
      }
    } else
      text = Array.from(
        { length: Math.ceil(bytes.length / 16) },
        (_, i) =>
          `${(page.offset + i * 16).toString(16).padStart(8, "0")}  ${
            hex(bytes.subarray(i * 16, i * 16 + 16))
              .match(/../g)
              ?.join(" ") ?? ""
          }`,
      ).join("\n");
  }
  return (
    <Stack gap="md">
      {meta && (
        <InfoGrid
          cols={3}
          items={[
            ["大小", formatBytes(meta.size)],
            [
              "权限",
              meta.mode == null
                ? null
                : (Number(meta.mode) & 511).toString(8).padStart(3, "0"),
            ],
            ["链接数", meta.links],
          ]}
        />
      )}
      <Group justify="space-between">
        <SegmentedControl
          value={mode}
          onChange={setMode}
          data={[
            { label: "UTF-8 文本", value: "text" },
            { label: "HEX", value: "hex" },
          ]}
        />
        <Button
          variant="light"
          leftSection={<ArrowDownToLine size={15} />}
          onClick={save}
          disabled={busy}
        >
          下载
        </Button>
      </Group>
      {error && <Alert color="red">{error}</Alert>}
      {busy ? (
        <Center py={45}>
          <Loader size="sm" />
        </Center>
      ) : (
        <ScrollArea.Autosize
          viewportRef={previewViewport}
          mah="clamp(120px, calc(100dvh - 380px), 400px)"
          scrollbarSize={5}
          type="auto"
          className="file-preview-scroll"
          offsetScrollbars
        >
          <pre className="code-block file-preview" data-mode={mode}>
            {text}
          </pre>
        </ScrollArea.Autosize>
      )}
      <Group justify="space-between">
        <Button
          variant="default"
          size="xs"
          leftSection={<ChevronLeft size={14} />}
          disabled={busy || offset === 0}
          onClick={() => setOffset(Math.max(0, offset - 4096))}
        >
          上一页
        </Button>
        <Text size="xs" c="dimmed">
          {page ? `${page.offset}–${page.next_offset} B` : ""}
        </Text>
        <Button
          variant="default"
          size="xs"
          rightSection={<ChevronRight size={14} />}
          disabled={busy || !page || page.eof}
          onClick={() => setOffset(page.next_offset)}
        >
          下一页
        </Button>
      </Group>
    </Stack>
  );
}
export function NvPage() {
  const { efs: rootEfs, ask, notify } = useServices(),
    pageApi = usePageApi(),
    efs = useMemo(() => rootEfs.scoped(pageApi), [rootEfs, pageApi]),
    status = useContext(EfsStatus),
    alive = useAlive();
  const [item, setItem] = useState(550),
    [result, setResult] = useState(null),
    [busy, setBusy] = useState(false),
    [imei, setImei] = useState(""),
    [config, setConfig] = useState("");
  useDirtyGuard(busy);
  async function read() {
    if (busy) return;
    toast.clear();
    if (!Number.isInteger(item) || item < 0 || item > 65535) {
      notify("NV 编号必须为 0–65535", true);
      return;
    }
    setBusy(true);
    const number = item;
    try {
      const reply = await efs.exclusive(() =>
        efs.api.request(`/efs/nvread?item=${number}`, { timeout: 125000 }),
      );
      if (alive.current) {
        setResult({
          item: number,
          data: reply.data,
          imei: number === 550 ? imeiFromNv(reply.data) : null,
        });
        notify("NV 读取完成");
      }
    } catch (e) {
      if (alive.current) {
        notify(e.message, true);
      }
    } finally {
      if (alive.current) setBusy(false);
    }
  }
  async function upload() {
    if (busy || !config) return;
    toast.clear();
    if (
      await ask({
        title: "上传 nvconfig.ini？",
        subtitle: "/nvefs/nvconfig.ini",
        body: <pre className="code-block">{config}</pre>,
        confirm: "上传配置",
        danger: true,
      })
    ) {
      setBusy(true);
      try {
        const bytes = new TextEncoder().encode(config);
        await efs.mutate("upload", "/nvefs/nvconfig.ini", bytes, bytes.length);
        notify("配置已上传");
      } catch (e) {
        notify(e.message, true);
      } finally {
        if (alive.current) setBusy(false);
      }
    }
  }
  return (
    <SimpleGrid cols={{ base: 1, lg: 2 }} spacing="lg">
      <Panel
        title="IMEI / NV 查询"
        icon={Database}
        right={
          <Badge color="teal" variant="light">
            只读
          </Badge>
        }
      >
        <Stack gap="lg">
          <Text size="sm" c="dimmed">
            读取 NV 550 时转换显示 IMEI；无法转换时保留完整 HEX。
          </Text>
          <NumberInput
            label="NV 编号"
            value={item}
            onChange={setItem}
            min={0}
            max={65535}
            allowDecimal={false}
            allowNegative={false}
          />
          <Group>
            <Button onClick={() => void read()} loading={busy}>
              读取 NV
            </Button>
            <Button
              variant="light"
              disabled={busy}
              onClick={() => setItem(550)}
            >
              IMEI · 550
            </Button>
          </Group>
          {result && (
            <Stack>
              {result.imei && <InfoGrid items={[["IMEI", result.imei]]} />}
              <Text size="xs" c="dimmed">
                NV {result.item} · 原始响应 HEX
              </Text>
              <pre className="code-block">
                {result.data?.hex ?? JSON.stringify(result.data, null, 2)}
              </pre>
            </Stack>
          )}
        </Stack>
      </Panel>
      <Panel title="生成 nvconfig.ini" icon={File}>
        <Stack gap="lg">
          <TextInput
            label="IMEI"
            placeholder="15 位 IMEI"
            inputMode="numeric"
            maxLength={15}
            autoComplete="off"
            value={imei}
            onChange={(e) => {
              setImei(e.target.value);
              setConfig("");
            }}
          />
          <Button
            onClick={() => {
              try {
                setConfig(nvConfig(imei));
              } catch (e) {
                notify(e.message, true);
              }
            }}
          >
            生成预览
          </Button>
          {config ? (
            <>
              <pre className="code-block">{config}</pre>
              <Group>
                <Button
                  variant="default"
                  leftSection={<ArrowDownToLine size={15} />}
                  onClick={() =>
                    download(config, "nvconfig.ini", "text/plain;charset=utf-8")
                  }
                >
                  下载配置
                </Button>
                <Button
                  variant="light"
                  leftSection={<ArrowUpFromLine size={15} />}
                  onClick={() => void upload()}
                  disabled={busy || !protectedBackend(status)}
                >
                  上传至 /nvefs
                </Button>
              </Group>
            </>
          ) : (
            <Center py={35}>
              <Text size="sm" c="dimmed">
                生成后可预览并下载配置
              </Text>
            </Center>
          )}
        </Stack>
      </Panel>
    </SimpleGrid>
  );
}
