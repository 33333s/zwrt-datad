# NMS 云端管理

默认关闭。zwrt-datad-cloud 是同仓库的可选工作进程，使用 Go TLS/MQTT/WebSocket，C 采样器独立运行。构建使用 scripts/build-cloud.sh；Release 增加 zwrt-datad-cloud-aarch64，安装名为 /data/zwrt-datad/zwrt-datad-cloud。将 scripts/cloud-service.sh 安装至同目录并更新 service.sh，原 datad 服务负责启停，无需新 init.d 或 rc.local 项。

UFI 通过本机 9460 的 GET/POST /cloud/config 和 GET /cloud/status 管理；9461 禁止访问。worker 只监听 0600 的 cloud.sock。配置原子保存为 0600 的 cloud.json，GET 不返回密码。POST 提交完整配置，空密码保留，clear_password:true 清除。保存后取消旧连接/会话并重新连接；关闭不影响本地管理。测试可通过 ZWRT_DATAD_CLOUD_SOCKET 改写采样器 Unix socket 目标。

配置字段：enabled、broker（ssl://主机:端口）、platform_url（https://主机:端口）、username/password（设备 MQTT 凭据）、ca_pem（可选 CA）、vendor/model/identity_type/identity/platform、report_interval_seconds（10–3600）、remote_enabled、services:[{name,port,kind}]（kind=web/terminal）。默认后台端口 80/2333/8899，最多 8 项，禁止 9460/9461。UFI 传入已有持久 UUID，不能每次启用生成新身份。

发布协议 v1 非 retained 的 telemetry/device、telemetry/system、status（含离线遗嘱），订阅本设备 command/request，只接受 remote.open。拒绝 retained、重复会话、非平台 WSS、未授权端口和超限会话，只连接 127.0.0.1。保留后台认证，不执行免密 handoff 或额外端口。最多 4 会话，每个 4 流；到期、配置关闭或平台拒绝后退出。不开放云端任意控制、ubus 或 OTA。

端口清单为 remote_services 扩展字段，自定义入口仍需 NMS 支持。目前 NMS 管理员 router_web 请求包含 80/81 双端口 handoff，会被拒绝；使用无 handoff 的 owner 入口或等待服务端适配。connected 只证明 MQTT 连接，不能证明设备准入、绑定或网页访问成功。上报仅包含选定资源字段，不上传完整 /state、短信或认证信息。

必须安装系统根证书或配置私有 CA，不支持跳过 TLS 校验。无 NMS 凭据时保持禁用。状态包括 disabled/connecting/connected/retrying/error。
