# zwrt-datad API

`dev` 分支当前通过单个 HTTP 服务提供状态读取能力。

## Endpoint

- Base URL: `http://127.0.0.1:9460`
- Auth: 无业务鉴权
- TLS: 默认不提供；如需外部访问，请在外层反向代理上补 HTTPS

## Routes

### `GET /state`

返回当前完整 JSON 快照。

响应头：

```http
HTTP/1.1 200 OK
Content-Type: application/json; charset=utf-8
Cache-Control: no-store
```

### `GET /events`

建立 SSE 长连接。

响应头：

```http
HTTP/1.1 200 OK
Content-Type: text/event-stream
Cache-Control: no-cache
Connection: keep-alive
X-Accel-Buffering: no
```

连接建立后：

1. 先立即推送一份当前快照
2. 后续每次状态内容变化后，再推送一份新的快照
3. 服务端只负责推送；客户端如果主动往这个连接写数据，服务端会断开

SSE 消息格式：

```text
retry: 1000

event: state
data: {"ts":1782396733,...}

```

### `GET /healthz`

返回：

```text
ok
```

## Command Line

- `--once`
  - 只采样一次，把 JSON 输出到标准输出后退出
- `-i <ms>`
  - 轮询间隔，默认 `1000`
- `-b <addr>`, `--bind <addr>`
  - HTTP 监听地址，默认 `127.0.0.1`
- `-p <port>`, `--port <port>`
  - HTTP 监听端口，默认 `9460`

示例：

```sh
./u60-datad -i 1000 -b 127.0.0.1 -p 9460
```

## Snapshot Contract

字段结构见 [`STATE_SCHEMA.md`](STATE_SCHEMA.md)。

当前快照是完整对象，不是增量 patch。每次 `/state` 和每条 SSE `state` 事件都返回一份完整 JSON。

设备侧 API 模板由后端根据 `device.model_name` 选择，并暴露在 `device.api_template` / `device.api_template_supported`。

具体某个机型模板会调用哪些设备接口，请看：

- [`models/README.md`](models/README.md)
- [`models/MU5250.md`](models/MU5250.md)

## Examples

一次性读状态：

```sh
curl http://127.0.0.1:9460/state
```

持续订阅：

```sh
curl -N http://127.0.0.1:9460/events
```

浏览器：

```javascript
const es = new EventSource("http://127.0.0.1:9460/events");
es.addEventListener("state", (ev) => {
  const state = JSON.parse(ev.data);
  console.log(state.system.cpu_usage);
});
```
