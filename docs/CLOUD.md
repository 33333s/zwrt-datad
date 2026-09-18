# NMS 云端管理

默认关闭。云端 TLS/MQTT/WebSocket 运行时由 Rust datad 进程直接提供，共享同一二进制和 PID。`scripts/build.sh` 只从 `rust/Cargo.toml` 构建最终 ARM64 musl 静态二进制；设备不再安装或启动 `zwrt-datad-cloud`、`cloud-service.sh` 或 `cloud.sock`。

UFI 通过本机 9460 的 GET/POST /cloud/config 和 GET /cloud/status 管理；9461 禁止访问。请求由 datad 进程内直接处理。配置原子保存为 0600 的 cloud.json，GET 不返回密码。POST 提交完整配置，空密码保留，clear_password:true 清除。保存后取消旧连接/会话并重新连接；关闭不影响本地管理。

配置字段：enabled、broker（`ssl://主机:端口` 或 `wss://主机[:端口]/mqtt`，WSS 默认 443）、platform_url（https://主机:端口）、username/password（设备 MQTT 凭据）、ca_pem（可选 CA）、vendor/model/identity_type/identity/platform、report_interval_seconds（10–3600）、remote_enabled、services:[{name,port,kind}]（kind=web/terminal）。默认后台端口 80/2333/8899，最多 8 项，禁止 9460/9461。UFI 传入已有持久 UUID，不能每次启用生成新身份。

发布协议 v1 非 retained 的 telemetry/device、telemetry/system、status（含离线遗嘱），订阅本设备 command/request，只接受 `remote.open` 和已签名的 datad 自更新命令（见 `docs/NMS.md`）。拒绝 retained、重复会话、非平台 WSS、未授权端口和超限会话，只连接 127.0.0.1。保留后台认证，不执行免密 handoff 或额外端口。最多 4 会话，每个 4 流；到期、配置关闭或平台拒绝后退出。不开放云端 ubus 或系统固件 OTA；可选原生终端另需下文的独立授权。

端口清单使用 `remote_services` 扩展字段，自定义入口需要 NMS 服务端同时支持。`connected` 只证明 MQTT 连接，不能证明设备准入、绑定或网页访问成功。上报仅包含选定资源字段，不上传完整 `/state`、短信或认证信息。

必须安装系统根证书或配置私有 CA，不支持跳过 TLS 校验。无 NMS 凭据时保持禁用。状态包括 disabled/connecting/connected/retrying/error。

## MQTT over WSS / Cloudflare Tunnel

`broker` 可填写 `wss://nms.example.com/mqtt`，`platform_url` 填写 `https://nms.example.com`。WSS 的路径原样用于 WebSocket 握手，显式端口可选；不支持明文 `ws://`，地址不得包含用户名、密码、查询参数或片段。MQTT 用户名和密码仍放在独立配置字段中，主题、ACL、QoS、心跳、遗嘱和重连流程保持不变。

NMS 入口将 `/mqtt` 反向代理到 EMQX 仅监听回环地址的 WebSocket 服务，其余路径继续进入 NMS。Tunnel 可沿用现有 HTTPS 回源，公网只需标准 HTTPS 443，不必在设备安装 cloudflared。UFI 的云端管理 MQTT 地址输入框可直接保存 WSS 地址。

WSS 使用内嵌公共根证书与可选 `ca_pem` 验证证书及主机名，不提供跳过验证选项。保留 `ssl://` 以兼容原有 MQTT TLS 部署。Cloudflare 会终止客户端 TLS；MQTT 账号和 Broker ACL 仍需启用。代理连接中断后由 datad 自动重连。

## 原生云端 WebShell

从 0.10.11 起，`remote_webshell_enabled` 为独立布尔配置，默认 false；旧配置升级后不会自动开放终端。还需启用 `enabled`、`remote_enabled` 和进程的 `--webshell` 选项。通过已鉴权的本机 `/cloud/config` 保存完整配置即可切换；关闭或任何云端配置变更会终止旧终端。遥测仅在本地终端可用时声明 `datad.webshell`，并上报实际生效的 `remote_webshell_enabled`。

`remote.open` 可使用 `target_service: "webshell"`、`target_port: 0`、1–1800 秒 TTL；不允许 `target_ports`，票据为 64 位十六进制。设备 WSS 地址必须严格匹配配置的平台与当前 request_id；保留 TLS 证书/主机名校验、MQTT 主题 ACL、非 retained 与会话去重。该逻辑服务直接使用进程内 PTY，不连接 TCP 端口、不代理本地管理 API，也不向云端发送 datad Token。

WSS 必须协商 `nms-webshell-v1`。设备使用 `Authorization: Bearer <session-ticket>`、`X-NMS-Target-Port: 0`；每条 binary 消息为一个 `kind:u8 + length:u32be + payload` 记录，kind 0 原始 stdin/stdout，kind 1 UTF-8 控制 JSON，payload 上限 16 KiB。首条输出为 `{"type":"ready","cols":80,"rows":24}`；文本输入仅接受 resize，范围 20–500 列、5–300 行。

本地和云端共用四个 PTY 名额。空闲 15 分钟、TTL 到期、连接断开、平台撤销或云端关闭后回收终端和前台进程组；不自动重连或重放命令。PTY 以 datad 用户执行，通常具有 root 权限；平台应绑定当前登录并持续复核终端权限。手动脱离终端的后台任务不提供恢复管理。MQTT 不执行命令文本，系统固件 OTA 仍不支持。

该通道复用平台 HTTPS/WSS 入口，无需新增端口。NMS 以及终止 TLS 的代理可以访问终端流量；不能宣称浏览器到设备的端到端或已验证的后量子加密。
