# NMS datad 更新协议

云端连接启用后，上报 `datad.update` 和 `datad.remote` 能力。升级请求沿用设备专属 MQTT `command/request` 与 `command/result`。保留消息不执行。

`datad.update.check` 与 `datad.update.install` 需要 `protocol_version: 1`、唯一 `request_id`、设备 `identity`、当前 `boot_id` 和未来不超过 300 秒的 `expires_uptime`。安装还要求前次检查得到的 `target_version` 与 `binary_sha256`。不接受下载地址、命令或固件刷写参数。

检查使用设备已有 OTA 更新源并验证 Ed25519 签名。安装再次校验候选版本及二进制摘要，复用 datad 原生升级安全检查与安装器。结果持久化到受保护的数据目录，服务重连后报告实际运行版本和文件 SHA-256。NMS 只有核对匹配后才能显示完成。

远程后台仍需本地 `remote_enabled` 开关和服务端口白名单；不开放 datad 管理端口，不处理旧代理的多端口免密交接。

在线心跳固定每 10 秒上报，与可配置的数据上报周期独立，兼容 NMS 的 30 秒离线判定。断开或关闭连接仍发送离线状态，异常断线由 MQTT 遗嘱通知。
