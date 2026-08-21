# zwrt-datad API

`zwrt-datad` 通过本机 HTTP 服务向上层 UFI 提供统一状态、状态事件和设备控制。

## Endpoint

- Base URL：`http://127.0.0.1:9460`
- TLS：默认不提供；推荐只监听回环地址
- Auth：可通过 `--auth-token-file <path>` 启用
- Content-Type：JSON 接口使用 `application/json; charset=utf-8`

启用 Token 后，除 `/healthz` 和根路径说明外，接口要求以下任一请求头：

```http
Authorization: Bearer <token>
```

```http
X-Auth-Token: <token>
```

## Routes

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
- `--auth-token-file <path>`：从文件加载私有 API Token

```sh
./zwrt-datad -i 1000 -b 127.0.0.1 -p 9460 \
  --auth-token-file /data/plugins/zwrt-datad/auth.token
```

## Integration Boundary

上层 UFI 继续提供原有 `/api/*`、`/api/goform/*` 和 `/goform/*`，负责用户鉴权、UUID、OTA、插件、数据库和业务逻辑。UFI 将旧接口翻译为 datad 的内部控制动作，浏览器不应直接连接 datad。
