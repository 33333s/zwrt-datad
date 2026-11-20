import { copyText } from "./clipboard.js";
import React, { useMemo, useState } from "react";
import {
  ActionIcon,
  Alert,
  Badge,
  Box,
  Button,
  Card,
  Group,
  Modal,
  Pagination,
  ScrollArea,
  Select,
  Stack,
  Tabs,
  Text,
  Textarea,
  TextInput,
  Tooltip,
} from "@mantine/core";
import {
  Copy,
  MessageSquare,
  Plus,
  Reply,
  Search,
  Send,
  Trash2,
} from "lucide-react";
import { smsPayload, smsRows, smsSenderOptions } from "../datad-models.js";
import {
  useDirtyGuard,
  useOperation,
  usePageApi,
  useResource,
} from "./hooks.jsx";
import { LoadState, RefreshButton } from "./forms.jsx";
import { Panel, useServices } from "./ui.jsx";

export function SmsPage() {
  const api = usePageApi(),
    { ask, notify } = useServices(),
    resource = useResource(() => api.request("/state"), [], 5000),
    op = useOperation();
  const [mailbox, setMailbox] = useState("inbox"),
    [query, setQuery] = useState(""),
    [page, setPage] = useState(1),
    [detail, setDetail] = useState(null),
    [detailOpen, setDetailOpen] = useState(false),
    [compose, setCompose] = useState({
      opened: false,
      number: "",
      text: "",
      sender: "host",
    });
  const all = useMemo(() => smsRows(resource.data), [resource.data]),
    rows = all.filter(
      (r) =>
        r.mailbox === mailbox &&
        `${r.number} ${r.content}`.includes(query.trim()),
    ),
    tabs = [
      ["inbox", "收件箱"],
      ["sent", "发件箱"],
      ...["draft", "unknown"]
        .filter((k) => all.some((r) => r.mailbox === k))
        .map((k) => [k, k === "draft" ? "草稿" : "其他"]),
    ];
  const dirty = compose.opened && (!!compose.text || !!compose.number);
  useDirtyGuard(dirty);
  async function closeCompose() {
    if (op.busy) return;
    if (
      dirty &&
      !(await ask({
        title: "放弃未发送的短信？",
        body: <Text size="sm">内容尚未提交。</Text>,
        confirm: "放弃",
      }))
    )
      return;
    setCompose((c) => ({ ...c, opened: false }));
  }
  function edit(row = null) {
    op.clear();
    setDetailOpen(false);
    setCompose({
      opened: true,
      number: row?.number ?? "",
      text: row?.mailbox === "draft" ? row.content : "",
      sender: "host",
    });
  }
  async function remove(row) {
    if (
      !(await ask({
        title: "删除短信？",
        body: (
          <Text size="sm">将删除来自 {row.number} 的这条记录，无法撤销。</Text>
        ),
        danger: true,
        confirm: "删除",
      }))
    )
      return;
    await op.run(
      async () => {
        await api.control("sms.delete", { ids: `${row.id};` });
        setDetailOpen(false);
        const next = await api.request("/state");
        resource.setData(next);
        return next;
      },
      {
        write: true,
        message: (r) =>
          smsRows(r).some((v) => v.id === row.id)
            ? "删除已提交，列表尚未更新，请稍后刷新"
            : "短信已删除",
      },
    );
  }
  async function send() {
    await op.run(
      async () => {
        const [state, caps] = await Promise.all([
          api.request("/state"),
          api.request("/capabilities"),
        ]);
        const payload = smsPayload(
          compose.number,
          compose.text,
          compose.sender,
          state,
          caps,
        );
        if (
          !(await ask({
            title: "发送短信？",
            body: (
              <Stack>
                <Text size="sm">
                  发送至 {payload.number}，运营商可能收取短信费用。
                </Text>
                {payload.sender !== "host" && (
                  <Text size="sm">指定 SIM 可能触发卡槽切换及网络中断。</Text>
                )}
              </Stack>
            ),
            confirm: "发送",
          }))
        )
          return false;
        api.check();
        const result = await api.control("sms.send_raw", payload);
        if (Number(result.status) !== 3)
          throw new Error(
            "请求已提交，设备尚未确认发送结果；请检查发件箱，勿直接重发",
          );
        setCompose((c) => ({ ...c, opened: false, text: "", number: "" }));
        notify("短信发送成功");
        await resource.refresh();
        return true;
      },
      { write: true },
    );
  }
  const showSlot = resource.data?.sim?.dual_sim === 2;
  const options = smsSenderOptions(
    resource.data,
    useServices().connection.capabilities,
  );
  return (
    <Stack gap="md">
      <Panel
        title="短信"
        icon={MessageSquare}
        right={
          <Group gap="xs">
            <RefreshButton resource={resource} disabled={op.busy} />
            <Button
              size="xs"
              leftSection={<Plus size={14} />}
              onClick={() => edit()}
              disabled={op.busy}
            >
              新建短信
            </Button>
          </Group>
        }
      >
        <LoadState resource={resource}>
          <Stack gap="md">
            {resource.data?.sms?.stale && (
              <Alert color="orange">
                短信数据是缓存，官方后台避让或读取失败可能导致刷新延迟。
              </Alert>
            )}
            <Tabs
              value={mailbox}
              onChange={(v) => {
                setMailbox(v);
                setPage(1);
              }}
            >
              <Tabs.List>
                {tabs.map(([id, label]) => (
                  <Tabs.Tab key={id} value={id}>
                    {label}{" "}
                    <Badge size="xs" variant="light" ml={5}>
                      {all.filter((r) => r.mailbox === id).length}
                    </Badge>
                  </Tabs.Tab>
                ))}
              </Tabs.List>
            </Tabs>
            <TextInput
              aria-label="搜索短信"
              placeholder="搜索号码或正文"
              leftSection={<Search size={15} />}
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                setPage(1);
              }}
            />
            {!rows.length ? (
              <Text size="sm" c="dimmed" py="xl" ta="center">
                {query ? "没有匹配的短信" : "暂无短信"}
              </Text>
            ) : (
              rows.slice((page - 1) * 20, page * 20).map((row) => (
                <Card key={row.id} withBorder padding="md" radius="md">
                  <Group wrap="nowrap" align="start">
                    <Box
                      style={{ flex: 1, minWidth: 0, cursor: "pointer" }}
                      role="button"
                      tabIndex={0}
                      aria-label={`查看短信 ${row.number}`}
                      onClick={() => {
                        setDetail(row);
                        setDetailOpen(true);
                      }}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") {
                          setDetail(row);
                          setDetailOpen(true);
                        }
                      }}
                    >
                      <Group justify="space-between" mb={8}>
                        <Text size="sm" fw={600}>
                          {row.number || "未知号码"}
                        </Text>
                        <Text size="xs" c="dimmed">
                          {row.date || "时间未上报"}
                        </Text>
                      </Group>
                      <Text
                        size="sm"
                        lineClamp={3}
                        style={{ whiteSpace: "pre-wrap" }}
                      >
                        {row.content || "（空正文）"}
                      </Text>
                      {(showSlot || row.mailbox === "sent") && (
                        <Group mt={8} gap="sm">
                          {row.failed && (
                            <Badge size="xs" color="red">
                              发送失败
                            </Badge>
                          )}
                          {row.mailbox === "sent" && !row.failed && (
                            <Badge size="xs" color="green">
                              已发送
                            </Badge>
                          )}
                          {showSlot && (
                            <Text size="xs" c="dimmed">
                              卡槽未记录
                            </Text>
                          )}
                        </Group>
                      )}
                    </Box>
                    <Tooltip label="删除">
                      <ActionIcon
                        variant="subtle"
                        color="red"
                        aria-label="删除短信"
                        disabled={op.busy}
                        onClick={() => remove(row)}
                      >
                        <Trash2 size={15} />
                      </ActionIcon>
                    </Tooltip>
                  </Group>
                </Card>
              ))
            )}
            {rows.length > 20 && (
              <Pagination
                total={Math.ceil(rows.length / 20)}
                value={Math.min(page, Math.ceil(rows.length / 20))}
                onChange={setPage}
                size="sm"
              />
            )}
          </Stack>
        </LoadState>
      </Panel>
      <Modal
        opened={detailOpen}
        onClose={() => setDetailOpen(false)}
        title="短信详情"
        centered
        radius="lg"
        onExitTransitionEnd={() => {
          if (!detailOpen) setDetail(null);
        }}
      >
        {detail && (
          <Stack>
            <Group justify="space-between">
              <Text fw={600}>{detail.number}</Text>
              {detail.failed && <Badge color="red">发送失败</Badge>}
            </Group>
            <Text size="xs" c="dimmed">
              {detail.date || "时间未上报"}
              {showSlot ? " · 卡槽未记录" : ""}
            </Text>
            <ScrollArea.Autosize mah="50vh">
              <Text
                size="sm"
                style={{
                  whiteSpace: "pre-wrap",
                  overflowWrap: "anywhere",
                  userSelect: "text",
                }}
              >
                {detail.content}
              </Text>
            </ScrollArea.Autosize>
            <Group justify="flex-end">
              <Button
                variant="default"
                size="xs"
                leftSection={<Copy size={13} />}
                onClick={() =>
                  void copyText(detail.content)
                    .then(() => notify("正文已复制"))
                    .catch(() =>
                      notify("浏览器未允许复制，请选择正文复制", true),
                    )
                }
              >
                复制
              </Button>
              <Button
                variant="light"
                size="xs"
                leftSection={<Reply size={13} />}
                onClick={() => edit(detail)}
              >
                {detail.mailbox === "draft" ? "继续编辑" : "回复"}
              </Button>
              <Button
                color="red"
                variant="light"
                size="xs"
                disabled={op.busy}
                onClick={() => remove(detail)}
              >
                删除
              </Button>
            </Group>
          </Stack>
        )}
      </Modal>
      <Modal
        opened={compose.opened}
        onClose={closeCompose}
        title="新建短信"
        centered
        radius="lg"
        closeOnClickOutside={!op.busy}
        withCloseButton={!op.busy}
      >
        <Stack gap="md">
          <TextInput
            label="收件号码"
            value={compose.number}
            onChange={(e) =>
              setCompose((c) => ({ ...c, number: e.target.value }))
            }
            disabled={op.busy}
          />
          {options.length > 1 && (
            <Select
              label="发送卡槽"
              value={compose.sender}
              data={options}
              onChange={(v) => setCompose((c) => ({ ...c, sender: v }))}
              disabled={op.busy}
              allowDeselect={false}
            />
          )}
          <Textarea
            label="正文"
            autosize
            minRows={5}
            maxRows={12}
            value={compose.text}
            onChange={(e) =>
              setCompose((c) => ({ ...c, text: e.target.value }))
            }
            disabled={op.busy}
          />
          <Text size="xs" c="dimmed">
            {compose.text.length} 个 UTF-16 单元 · 长短信由设备处理分段
          </Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={closeCompose} disabled={op.busy}>
              取消
            </Button>
            <Button
              leftSection={<Send size={15} />}
              loading={op.busy}
              onClick={send}
              disabled={!compose.number.trim() || !compose.text.trim()}
            >
              发送
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Stack>
  );
}
