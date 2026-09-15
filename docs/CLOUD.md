# NMS 云端管理

默认关闭。云端 TLS/MQTT/WebSocket 运行时由 Rust datad 进程直接提供，共享同一二进制和 PID。`scripts/build.sh` 只从 `rust/Cargo.toml` 构建最终 ARM64 musl 静态二进制；设备不再安装或启动 `zwrt-datad-cloud`、`cloud-service.sh` 或 `cloud.sock`。

UFI 通过本机 9460 的 GET/POST /cloud/config 和 GET /cloud/status 管理；9461 禁止访问。请求由 datad 进程内直接处理。配置原子保存为 0600 的 cloud.json，GET 不返回密码。POST 提交完整配置，空密码保留，clear_password:true 清除。保存后取消旧连接/会话并重新连接；关闭不影响本地管理。

配置字段：enabled、broker（ssl://主机:端口）、platform_url（https://主机:端口）、username/password（设备 MQTT 凭据）、ca_pem（可选 CA）、vendor/model/identity_type/identity/platform、report_interval_seconds（10–3600）、remote_enabled、services:[{name,port,kind}]（kind=web/terminal）。默认后台端口 80/2333/8899，最多 8 项，禁止 9460/9461。UFI 传入已有持久 UUID，不能每次启用生成新身份。

发布协议 v1 非 retained 的 telemetry/device、telemetry/system、status（含离线遗嘱），订阅本设备 command/request，只接受 remote.open。拒绝 retained、重复会话、非平台 WSS、未授权端口和超限会话，只连接 127.0.0.1。保留后台认证，不执行免密 handoff 或额外端口。最多 4 会话，每个 4 流；到期、配置关闭或平台拒绝后退出。不开放云端任意控制、ubus 或 OTA。

端口清单使用 `remote_services` 扩展字段，自定义入口需要 NMS 服务端同时支持。`connected` 只证明 MQTT 连接，不能证明设备准入、绑定或网页访问成功。上报仅包含选定资源字段，不上传完整 `/state`、短信或认证信息。

必须安装系统根证书或配置私有 CA，不支持跳过 TLS 校验。无 NMS 凭据时保持禁用。状态包括 disabled/connecting/connected/retrying/error。
