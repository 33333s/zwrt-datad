# zwrt-datad API

`zwrt-datad` 通过本机 HTTP 服务向上层 UFI 提供统一状态、状态事件和设备控制。

## Endpoint

- Local Base URL：`http://127.0.0.1:9460`
- LAN Base URL：`http://<device-lan-ip>:9461`
- LAN Source Filter：仅允许 `10/8`、`172.16/12`、`192.168/16`、`100.64/10`、`169.254/16` 和 `127/8`
- TLS：默认不提供；如需跨设备安全传输，应在外层补 HTTPS
- Content-Type：JSON 接口使用 `application/json; charset=utf-8`

本机端口保持免鉴权，供设备上的 UFI 和脚本使用。内网端口通过 `POST /auth/login` 或 `POST /auth/exchange` 获取临时 Token；若配置了非空静态 Token 文件，也兼容该 Token。

需要鉴权的接口接受以下请求头：

```http
Authorization: Bearer <token>
```

```http
X-Auth-Token: <token>
```

原生 `EventSource` 无法设置请求头时，也可以使用 `?access_token=<token>`。

## Routes

### `POST /auth/login`

仅内网鉴权端口提供。使用 HTTP Basic 传递中兴后台用户名和密码：

```sh
curl -s -u admin:your_web_password -X POST \
  http://<device-lan-ip>:9461/auth/login
```

成功后返回有效期 12 小时的 Bearer Token；每次成功使用会刷新有效期：

```json
{"ok":true,"token_type":"Bearer","access_token":"...","expires_in":43200,"expires_at":1783500000}
```

### `POST /auth/exchange`

使用 vendor webtoken 换取 datad Token：

```sh
curl -s -X POST \
  -H 'X-Web-Token: <vendor_webtoken>' \
  -H 'X-Z-Mode: 0' \
  -H 'X-Z-Tag: zwrt-datad' \
  http://<device-lan-ip>:9461/auth/exchange
```

### `GET /state`

返回当前完整 JSON 快照。字段结构见 [`STATE_SCHEMA.md`](STATE_SCHEMA.md)。

### `GET /events`

建立 SSE 长连接：

1. 连接建立后立即推送当前快照
2. 只有状态内容变化时才推送下一份完整快照
3. `ts` 单独变化不会产生事件
4. 控制成功会触发立即重采样，变化后的状态通过此连接推送

```text
retry: 1000

event: state
data: {"ts":1782396733,...}

```

### `GET /capabilities`

返回内部协议版本、支持的控制动作和事件类型。

### `GET /ubus`

返回设备当前注册的完整 ubus 对象清单。传入 `?verbose=1` 时等价于设备侧 `ubus -v list`，同时返回所有方法及参数签名：

```sh
curl http://127.0.0.1:9460/ubus
curl 'http://127.0.0.1:9460/ubus?verbose=1'
```

该接口本身只做只读发现，不接受对象名、方法名或参数；实际调用使用下面独立的 `/ubus/call`。LAN 端口访问时仍需要 Bearer Token。

### `POST /ubus/call`

所有设备模板默认提供完整 ubus 调用能力，设备当前注册的对象和方法均可调用：

```json
{
  "service": "system",
  "method": "info",
  "args": {}
}
```

成功响应保留设备原始 JSON：

```json
{
  "ok": true,
  "service": "system",
  "method": "info",
  "result": { "uptime": 123 }
}
```

- `args` 可省略；提供时必须是完整 JSON 对象。
- 服务名、方法名和参数通过 `fork/exec` 参数数组传递，不经过 Shell。
- 完整 ubus 能力由设备模板的 `full_ubus` 位控制，现有正式模板和兼容模板默认均开启。
- 回环来源可使用本机免鉴权口；非回环来源必须走现有鉴权监听口。
- 该入口不会区分 getter 和 setter。`reboot`、`sysupgrade`、网络重载、服务管理及厂商写接口都可能造成中断或数据丢失，调用方对目标和参数负责。

### `POST /control`

执行白名单内的设备操作。接口不接受任意 Shell、任意命令名或任意 `ubus` 服务名。

```json
{
  "action": "network.set_mode",
  "params": {
    "mode": "Only_LTE"
  }
}
```

成功响应：

```json
{
  "ok": true,
  "action": "network.set_mode",
  "result": {
    "result": "success"
  }
}
```

失败响应：

```json
{
  "ok": false,
  "action": "network.set_mode",
  "error": {
    "code": "device_call_failed",
    "message": "zte_nwinfo_api.nwinfo_set_netselect failed"
  }
}
```

完整动作和参数见 [`CONTROL_API.md`](CONTROL_API.md)。

### `GET /healthz`

始终返回 `ok`。该接口只代表 HTTP 进程正在监听，不代表每个设备子模块都可用。

## Status Codes

- `200`：读取或控制成功
- `400`：请求体或参数错误
- `401`：Token 缺失或错误
- `404`：路径或控制动作不存在
- `405`：请求方法错误
- `413`：请求体超过限制
- `502`：设备侧 `ubus/uci` 调用失败
- `503`：SSE 客户端达到上限

## Command Line

- `--once`：采样一次并把 JSON 输出到标准输出
- `-i <ms>`：采样间隔，默认 `1000`
- `-b <addr>` / `--bind <addr>`：监听地址，默认 `127.0.0.1`
- `-p <port>` / `--port <port>`：监听端口，默认 `9460`
- `--lan-bind <addr>`：额外开启需要鉴权的内网监听口
- `--lan-port <port>`：内网监听端口，默认 `9461`
- `--auth-token-file <path>`：兼容静态 Token 文件

```sh
/data/zwrt-datad/zwrt-datad -i 1000 -b 127.0.0.1 -p 9460 \
  --lan-bind 0.0.0.0 --lan-port 9461 \
  --auth-token-file /data/zwrt-datad/auth.token
```

## Integration Boundary

上层 UFI 继续提供原有 `/api/*`、`/api/goform/*` 和 `/goform/*`，负责用户鉴权、UUID、OTA、插件、数据库和业务逻辑。UFI 将旧接口翻译为 datad 的内部控制动作，浏览器不应直接连接 datad。

内网读取与 SSE 示例：

```sh
curl -H 'Authorization: Bearer <token>' http://<device-lan-ip>:9461/state
curl -N 'http://<device-lan-ip>:9461/events?access_token=<token>'
```
