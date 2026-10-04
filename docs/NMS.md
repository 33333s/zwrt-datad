# NMS datad 更新协议

云端连接启用后，上报 `datad.update` 和 `datad.remote` 能力。升级请求沿用设备专属 MQTT `command/request` 与 `command/result`。保留消息不执行。

`datad.update.check` 与 `datad.update.install` 需要 `protocol_version: 1`、唯一 `request_id`、设备 `identity`、当前 `boot_id` 和未来不超过 300 秒的 `expires_uptime`。安装还要求前次检查得到的 `target_version` 与 `binary_sha256`。不接受下载地址、命令或固件刷写参数。

检查使用设备已有 OTA 更新源并验证 Ed25519 签名。安装再次校验候选版本及二进制摘要，复用 datad 原生升级安全检查与安装器。结果持久化到受保护的数据目录，服务重连后报告实际运行版本和文件 SHA-256。NMS 只有核对匹配后才能显示完成。

远程后台仍需本地 `remote_enabled` 开关和服务端口白名单；不开放 datad 管理端口，不处理旧代理的多端口免密交接。

在线心跳固定每 20 秒上报（0.10.62 起，原为 10 秒），与可配置的数据上报周期独立；NMS 的离线判定是 60 秒，保留 3 倍余量。每次连接后的第一次心跳落在 20 秒内的随机时刻，避免大量设备同步心跳。断开或关闭连接仍发送离线状态，异常断线由 MQTT 遗嘱通知。

## 上报节奏与抖动（0.10.62）

- `telemetry/system` 仍按数据上报周期（默认 30 秒）定时发送。
- `telemetry/device` 和 `telemetry/network` 只在内容变化时发送，没有变化则每小时补发一次；每次（重新）连接后的第一轮完整上报三类载荷都发，并发送 `status` 在线。周期性上报不再重复发 `status`，在线状态由心跳承担。
- 进程启动时的第一次连接随机延后 0 到 5 秒；连接失败后的重试间隔在 0.5 到 1.5 倍的退避值内随机，避免 NMS 重启或网络恢复时上千台设备同时重连、同时上报。

## 设备名称与真实机型（0.10.62）

`telemetry/device` 增加三个可选字段，值缺失、为空、超过 64 个字符或含控制字符时不输出：

| 字段 | 来源 | 说明 |
|---|---|---|
| `hardware_model` | `/state.device.model_name`（回退 `uci_device_info.common_model_name`） | 真实硬件机型，例如 `MU5002D`。`model` 仍是设备身份和 MQTT 主题使用的值，不改变 |
| `device_market_name` | `uci_device_info.device_market_name` | 厂商营销名称 |
| `device_alias_name` | `uci_device_info.device_alias_name` | 厂商别名 |

## `remote.close`（0.10.62）

遥测声明 `datad.remote_close` 能力后，NMS 在远程会话结束时向设备发送：

```json
{"protocol_version":1,"request_id":"<32 位十六进制>","action":"remote.close","target_session_id":"<对应 remote.open 的 request_id>"}
```

设备立即断开该会话的 WebSocket 通道，不等 TTL；其他会话不受影响，重复关闭无副作用，没有应答。收到尚未见过的 `target_session_id` 时只记录该编号，之后重发的 `remote.open` 会被当作已处理而忽略，不会重新打开已结束的会话。字段格式不对或协议版本不是 1 的关闭请求被丢弃。

