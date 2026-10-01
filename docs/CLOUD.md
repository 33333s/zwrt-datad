# NMS 云端管理

## 仅凭 MQTT 凭据接入（0.10.23）

NMS 创建待接入凭据后，只把一次显示的 MQTT 用户名（`enr_` 前缀）和密码填入设备。UFI 的简易接入表单调用本机 `POST /cloud/quick-connect`；datad 只在该路径下自动选用内置 NMS HTTPS/WSS 地址，并从本机状态读取厂商、型号、固件平台和稳定标识。已有高级配置不会因加载而改写；已启用的云端账户须在高级配置中更换。从 0.10.53 起，简易接入同时开启远程面板控制和云端终端：NMS 侧任何运维账户（含超级管理员）仍须取得设备 Owner 限时授权（最长 7 天）才能打开，Owner 可随时在本机关闭任一开关。

datad 优先使用 modem MSN 生成与 UFI 相同的稳定 UUID；若没有 MSN，依次使用设备序列号、IMEI，生成后固定保存在 `cloud.json`，后续状态变化不会重算。若取不到可靠标识或型号，保存失败，不注册随机身份。首次连接仅向 `onboard/<用户名>/hello` 发送签名身份报告；NMS 校验凭据、签名、账户和设备占用后返回绑定结果，datad 才订阅正式命令并发送正常遥测。待接入凭据只能绑定首台设备，失效、撤销或已被占用的身份不能继续接入。旧 `dev_` 凭据继续按原流程连接。

0.10.24 起，快速接入即使从曾经关闭的旧高级配置切换，也会明确把远程面板控制和 WebShell 设为关闭；若需要这些能力，设备所有者必须随后在高级配置中单独开启。

默认关闭。首次没有 `cloud.json` 时，NMS 地址预填 `https://nms.ericsfj.com`，MQTT 地址预填 `wss://nms.ericsfj.com/mqtt`；用户名、密码和设备标识仍需独立填写，datad 不会自行启用或连接。已有配置文件中的自定义地址、凭据、开关及后台列表保持原值，不在加载时重写。云端 TLS/MQTT/WebSocket 运行时由 Rust datad 进程直接提供，共享同一二进制和 PID。`scripts/build.sh` 只从 `rust/Cargo.toml` 构建最终 ARM64 musl 静态二进制；设备不再安装或启动 `zwrt-datad-cloud`、`cloud-service.sh` 或 `cloud.sock`。

UFI 通过本机 9460 的 GET/POST /cloud/config 和 GET /cloud/status 管理；9461 禁止访问。请求由 datad 进程内直接处理。配置原子保存为 0600 的 cloud.json，GET 不返回密码。POST 提交完整配置，空密码保留，clear_password:true 清除。保存后取消旧连接/会话并重新连接；关闭不影响本地管理。

配置字段：enabled、broker（`ssl://主机:端口` 或 `wss://主机[:端口]/mqtt`，WSS 默认 443）、platform_url（https://主机:端口）、username/password（设备 MQTT 凭据）、ca_pem（可选 CA）、vendor/model/identity_type/identity/platform、report_interval_seconds（10–3600）、remote_enabled、services:[{name,port,kind}]（kind=web/terminal）。默认后台端口 80/2333/8899，最多 8 项，禁止 9460/9461。UFI 传入已有持久 UUID，不能每次启用生成新身份。

发布协议 v1 非 retained 的 telemetry/device、telemetry/system、telemetry/network、status（含离线遗嘱），订阅本设备 command/request，只接受 `remote.open` 和已签名的 datad 自更新命令（见 `docs/NMS.md`）。拒绝 retained、重复会话、非平台 WSS、未授权端口和超限会话，只连接 127.0.0.1。保留后台认证，不执行免密 handoff 或额外端口。最多 4 会话，每个 4 流；到期、配置关闭或平台拒绝后退出。不开放云端 ubus 或系统固件 OTA；可选原生终端另需下文的独立授权。

`telemetry/network` 包含 `upstream.ipv4/ipv6` 与服务小区标识 `cell`：`rat`、`mcc`、`mnc`、`lte_tac`、`lte_cell_id`、`lte_pci`、`nr_tac`、`nr_cell_id`、`nr_pci`，供 NMS 做基站定位。数值字段只上报大于 0 的值（`mnc` 在 `mcc` 有效时保留 `0`）；原厂可能在切换制式后短暂保留旧小区值，消费端还须核对 `rat` 与上报时间。不包含 IMSI、ICCID 等 SIM 标识。

端口清单使用 `remote_services` 扩展字段，自定义入口需要 NMS 服务端同时支持。`connected` 只证明 MQTT 连接，不能证明设备准入、绑定或网页访问成功。上报仅包含选定资源字段，不上传完整 `/state`、短信或认证信息。

必须安装系统根证书或配置私有 CA，不支持跳过 TLS 校验。无 NMS 凭据时保持禁用。状态包括 disabled/connecting/connected/retrying/error。

## MQTT over WSS / Cloudflare Tunnel

`broker` 可填写 `wss://nms.example.com/mqtt`，`platform_url` 填写 `https://nms.example.com`。WSS 的路径原样用于 WebSocket 握手，显式端口可选；不支持明文 `ws://`，地址不得包含用户名、密码、查询参数或片段。MQTT 用户名和密码仍放在独立配置字段中，主题、ACL、QoS、心跳、遗嘱和重连流程保持不变。

NMS 入口将 `/mqtt` 反向代理到 EMQX 仅监听回环地址的 WebSocket 服务，其余路径继续进入 NMS。Tunnel 可沿用现有 HTTPS 回源，公网只需标准 HTTPS 443，不必在设备安装 cloudflared。UFI 的云端管理 MQTT 地址输入框可直接保存 WSS 地址。

WSS 使用内嵌公共根证书与可选 `ca_pem` 验证证书及主机名，不提供跳过验证选项。保留 `ssl://` 以兼容原有 MQTT TLS 部署。Cloudflare 会终止客户端 TLS；MQTT 账号和 Broker ACL 仍需启用。代理连接中断后由 datad 自动重连。

## 原生云端 WebShell

从 0.10.11 起，`remote_webshell_enabled` 为独立布尔配置，`cloud.json` 缺省值为 false。从 0.10.53 起，简易接入时直接开启；升级前已启用云端与远程访问的设备，在首次以 0.10.53 加载时一次性开启远程面板控制与云端终端，并写入 `cloud-feature-defaults-v1` 标记，此后本机关闭会被保留。还需启用 `enabled`、`remote_enabled` 和进程的 `--webshell` 选项。通过已鉴权的本机 `/cloud/config` 保存完整配置即可切换；关闭或任何云端配置变更会终止旧终端。遥测仅在本地终端可用时声明 `datad.webshell`，并上报实际生效的 `remote_webshell_enabled`。

`remote.open` 可使用 `target_service: "webshell"`、`target_port: 0`、1–1800 秒 TTL；不允许 `target_ports`，票据为 64 位十六进制。设备 WSS 地址必须严格匹配配置的平台与当前 request_id；保留 TLS 证书/主机名校验、MQTT 主题 ACL、非 retained 与会话去重。该逻辑服务直接使用进程内 PTY，不连接 TCP 端口、不代理本地管理 API，也不向云端发送 datad Token。

WSS 必须协商 `nms-webshell-v1`。设备使用 `Authorization: Bearer <session-ticket>`、`X-NMS-Target-Port: 0`；每条 binary 消息为一个 `kind:u8 + length:u32be + payload` 记录，kind 0 原始 stdin/stdout，kind 1 UTF-8 控制 JSON，payload 上限 16 KiB。首条输出为 `{"type":"ready","cols":80,"rows":24}`；文本输入仅接受 resize，范围 20–500 列、5–300 行。

本地和云端共用四个 PTY 名额。空闲 15 分钟、TTL 到期、连接断开、平台撤销或云端关闭后回收终端和前台进程组；不自动重连或重放命令。PTY 以 datad 用户执行，通常具有 root 权限；平台应绑定当前登录并持续复核终端权限。手动脱离终端的后台任务不提供恢复管理。MQTT 不执行命令文本，系统固件 OTA 仍不支持。

该通道复用平台 HTTPS/WSS 入口，无需新增端口。NMS 以及终止 TLS 的代理可以访问终端流量；不能宣称浏览器到设备的端到端或已验证的后量子加密。


## 备用远程入口（0.10.12）

`remote_origins` 是可选的 HTTPS 站点地址列表，默认空，最多 4 个。主 `platform_url` 始终保留。示例：`platform_url: "https://nms.example.com"`、`remote_origins: ["https://relay.example.com:16001"]`。仅接受明确的主机及端口，禁止通配符、用户信息、路径、查询参数、片段、空白和重复地址；不提供跳过 TLS 校验选项。

当 `platform_url` 与 `broker` 都使用上面的内置免费地址时，datad 还把 `https://a.ericsfj.com:16001` 加入**运行时有效的备用远程来源**，即使旧 `cloud.json` 的 `remote_origins` 仍为空。该地址只用于已启用远程访问且经 NMS 会话票据授权的 WebSocket，不改变 MQTT/心跳入口，也不表示会员资格。NMS 遥测报告包含这个有效来源；`GET /cloud/config` 和 UFI 自定义表单保持用户保存的 `remote_origins` 原值。只要主 NMS 或 MQTT 地址改为自定义值，隐式会员来源即不再加入；用户显式配置的备用来源仍照常生效。

`remote.open` 可连接主地址或已配置备用地址对应的 WSS，其他主机、端口和会话路径仍被拒绝。遥测声明 `datad.remote_origins` 并上报规范化后的全部允许地址。空列表保持旧版只允许主平台的行为。保存配置会关闭旧会话并重新上报；原有本地认证、WebShell 开关、资源上限和签名更新规则保持不变。

NMS 双线路部署可让 MQTT 继续使用稳定的免费控制入口，仅将设备 WebUI/WebShell 数据通道切换至已授权中转。两个远程地址都在设备白名单中时，会员过期后可以回到免费入口，无需重新修改设备配置。会员资格和限速由 NMS 执行，设备备用地址本身不是付费凭据。

## 按需远程面板状态通道

从 0.10.29 起，已授权的按需远程面板会话还可读取 `cloud_management`：仅含云连接状态、非秘密配置字段、密码与自定义 CA 是否存在的布尔值，以及最多 8 个本机后台名称/端口/类型。MQTT 密码与 CA 内容不会上传。远程写操作 `cloud.remote_features.set` 只接收 `remote_panel_control_enabled`、`remote_webshell_enabled` 和完整 `services` 列表；不能改写 MQTT 目标、设备身份、凭据或证书。NMS 和 datad 双层校验，v2 控制帧必须 `confirmed=true`，保存后先返回控制结果，再重连云端会话；远程面板控制被关闭后，后续会话恢复只读。

从 0.10.30 起，原版远程 UFI 的定时任务在设备本地按设备时钟执行：只接受经 NMS 和 datad 双层审查的已支持动作及精确参数，不执行原版的任意 JSON/命令。最多 16 项，一次性或每天 `HH:MM`；执行日期先持久化到 0600 文件，重启后不重复触发同日任务。任务列表和结果只在已授权的按需面板帧中提供，不加入普通 MQTT 遥测或 `/state`；新增、修改、删除均要求 v2 控制确认，NMS 记录管理动作审计。设备时间不可用时拒绝新增，原厂计划重启开启或同一时刻已有 datad 每日重启时拒绝重复安排重启。此前明确跳过的 ADB 调试不在动作范围内。

从 0.10.31 起，可单独启用设备端 HTTPS 短信转发，支持固定 JSON POST Webhook 与钉钉机器人。从 0.10.32 起，还支持用本机 SIM 向最多三个号码转发收到的短信；跳过原发件人和已发送短信，每日最多尝试发送 60 条，短信过长时拒绝发送。短信方式和发送测试消息均可能产生运营商费用。从 0.10.33 起，还支持 SMTP 邮件：仅接受公网域名、465 隐式 TLS 或 587 强制 STARTTLS，固定解析出的公网地址并验证服务器证书；邮箱应用密码留在设备 0600 配置中，不随状态回传。配置默认关闭，原版任意 CURL 命令仍未接入。启用时先将现有短信指纹记作基线，只处理后续新短信；指纹在投递前以 0600 文件持久化，投递失败不自动重发，避免进程重启后重复发送。HTTPS 转发目标必须为有有效证书的公网域名，禁用代理/跳转，DNS 地址在连接前过滤并固定，响应严格限长。钉钉加签时间从已配置 NMS 的 HTTPS Date 响应取得；校时失败不改用可能偏移的设备时钟。密码、目标 URL、手机号、邮箱地址和短信内容均不在普通 MQTT 遥测或云端转发状态中出现；只有经授权的设备本地进程执行投递。若设备本地 UFI 已启用短信转发，应先关闭其中一方以免重复发送。

从 0.10.34 起，有有效电池读数的设备可单独开启电源状态通知。充电状态变化或电量百分比变化会通过当前短信转发目的地投递；首次启用只记录基线，总开关关闭期间继续更新基线，不补发旧变化。原厂 `charge_status` 的 1/2/3/4 分别视为充电中/放电中/未在充电/已充满，未知读数不产生通知；MU5250 的状态 4 已由设备 `/sys/class/power_supply/battery/status=Full` 对照。事件和每日最多 60 次的额度在投递前以 0600 文件持久化，失败不自动重发；本机短信方式仍同时受自身每日 60 条限制。此功能默认关闭，发送到外部目的地前需由设备 Owner 或授权管理员在 NMS 二次确认。

从 0.10.35 起，原版“转发规则”可配置最多 64 个号码与 32 个关键词作为短信转发黑名单。号码允许纯数字或开头单个 `+`，比较时忽略这个 `+`；关键词区分大小写并做正文子串匹配。命中的新短信在设备端跳过目的地投递，但指纹仍以 0600 文件记录，之后清空规则不会补发。规则内容只保存在设备端，按需远程面板仅上报是否支持和条数；规则不影响电源状态通知。配置及清除走既有 NMS Owner/管理员权限、二次确认与审计。

从 0.10.37 起，原版“设置别名”可将最多 255 字节的设备别名持久保存在同一 0600 配置中，空串恢复默认名称。只有已授权的按需远程面板会收到别名以显示标题；不进入普通遥测，也不改变短信发送开关、目的地或历史基线。

从 0.10.38 起，原版 SMTP、钉钉、本机短信方式可分别选择“同时转发设备信息”，默认均关闭，按方式独立保存于设备的 0600 配置。datad 仅在向已配置目的地投递短信或电源通知时，从设备本地快照提取有界的名称、固件、流量、CPU/内存/电池与运行时长；SMTP/钉钉附状态块，本机短信只附型号、电量、本机号码且仍受 280 个 UTF-16 单元上限约束。设备资料及目的地不进入普通 MQTT 遥测，授权按需面板只收到各方式的开关状态。原版可执行任意 CURL 命令仍不开放。

受限定时动作 `sms.forward.device_info` 可按设备本地时间向当前已配置的转发目的地发送一次设备状态，参数必须是空对象。它要求总转发开关已开启；本机短信方式先持久化扣除每日配额，任务执行前记录尝试结果，失败不会自动重试。该动作不接受任意目标地址或消息正文。

从 0.10.40 起，远程原版定时任务可保存 `sms.send_scheduled` 的目标号码和有界短信正文。它只在设备本地到期时通过当前主卡发送，不接受指定 SIM/厂商命令、外部 URL 或任意控制参数；任务尝试与共用的每日 60 条本机 SIM 配额均在发送前持久化，失败不重试。号码和正文仅存设备 0600 任务文件，并只在授权按需面板流中回显，不进入普通 MQTT 遥测、NMS 审计参数或常规状态接口。

从 0.10.41 起，NMS 原版远程面板可在设备报告本地采集器可用时，经 Owner/管理员权限与二次确认启停临时邻区采集。datad 仍默认关闭、只管理自己启动的进程，并在停止时清理私有临时 capture；原始诊断帧和采集日志不上传 NMS，远程面板只读取有界状态及筛选后的邻区观测。采集启动成功不代表当前固件的报告已适配，界面须区分 `no_supported_reports` 与确实没有邻区。

从 0.10.25 起，datad 在设备本地每五分钟记录一次蜂窝日流量计数变化，最多保留 400 个自然日，文件 `traffic-history.json` 以 0600 权限原子保存。首次安装从设备当前“今日流量”计数开始，不回填此前日期；设备日期未就绪时不记样本。计数在同一天归零时按新计数继续累计；跨日时若原厂计数尚未归零，先扣除上一日的末次计数，避免把昨日用量记入新一天。

只有已授权的 `datad_panel` / `datad_panel_control` 按需会话接收 `traffic_history:[{date,bytes}]`；常规 MQTT 遥测、本地 `/state` 与 SSE 不含历史文件内容。该数组只含日期和上下行总字节，不含原始 UCI、IP、SIM 标识或每秒速率。NMS 再对日期、顺序、长度和字节范围校验，浏览器只在打开原版“流量历史”弹窗时按日期筛选显示。

从 0.10.27 起，`reboot_schedule` 状态块只随本地 `/state`/SSE 与已授权的按需面板会话发送，不进入常规 MQTT。`schedule.reboot.set` 在 v2 面板要求 `confirmed=true`，每日时间按设备本地时钟和时区解释；datad 将当天的触发日期以 0600 文件持久化后才调用已有 `device.reboot`，同一天至多触发一次。默认关闭；检测到原厂周/间隔日计划重启已启用时，拒绝启用 datad 的每日计划，避免两套计划重复重启。无效配置或设备时间不可用时不触发。

从 0.10.28 起，`speedtest.start` 由设备默认 IPv4 出口向 [Cloudflare 官方测速组件](https://github.com/cloudflare/speedtest#api-reference)使用的固定 `https://speed.cloudflare.com/__down` 发起 HTTPS 下载，沿用证书与主机名校验、不跟随重定向、不读取浏览器提供的 URL。一次操作的总下载量限制为 1–50 MiB、1–5 线程、1 或 3 轮，每轮最长约 18 秒；同一时刻只允许一个测试，可用 `speedtest.stop` 取消。NMS v2 控制通道启动时要求 `confirmed=true`；网页只收到 `speedtest` 状态与平均速率，不传输下载内容，也不把测速结果加入普通 MQTT 遥测。该值只描述设备到 Cloudflare 边缘的单次测试，不代表设备蜂窝链路峰值或聚合吞吐。

云端连接且 `remote_enabled` 开启时，datad 报告 `datad.panel` 能力。NMS 可用现有 `remote.open` 命令建立 `target_service: "datad_panel"`、`target_port: 0` 的独立 WSS 会话，最长一小时，不占用 9460/9461 TCP 管理端口，也不需要设备本地 UFI 进程。设备必须验证配置的平台/备用 HTTPS 来源、TLS 主机名、一次性会话票据和 `nms-datad-panel-v1` WebSocket 子协议。

连接后设备先发送 `ready`，再以最多每秒一次的 `state` 文本帧发送当前 datad 快照。只允许本模块显式列出的状态块；每帧至多 192 KiB，不上传 datad Bearer Token、云配置或未来新增的未知块。状态只在远程面板会话期间传输，不加入常规 MQTT 遥测或设备历史记录。会话到期、远程开关关闭、配置变化或 WSS 中断即停止。此协议第一阶段只读，设备拒绝浏览器/平台发来的数据帧；后续控制需另行逐项授权和审计。

0.10.22 起，面板按需附带 `uci_device_info` 中的 ICCID/IMEI/IMSI/MSISDN/MAC/MSN，以及 `interfaces` 中最多四个有效 WAN IPv4/IPv6 地址。设备端先逐字段筛选，NMS 再做第二层筛选；原始 UCI、蜂窝接口配置、DNS 与其他字段不随面板流发送，也不进入常规 MQTT 遥测。

`clients` 状态包含在线设备列表和经过 MAC 格式校验、去重并限制为 128 条的 `blocked` 黑名单，用于 NMS 托管的原版接入设备弹窗；不会把 Wi-Fi 密钥或原厂管理凭据放入该状态块。

远程面板会话开启后按需读取 `wifi_config`：仅包含双频合一状态、各频段 SSID、加密模式、隐藏/PMF/最大接入、国家、信道与设备允许的选项；不读取或传输已保存的 Wi-Fi 密钥。改动 Wi-Fi 后重新读取这些字段。`wireless.config` 作为 v2 受确认的频段配置动作，仍由 datad 验证国家和信道并执行读回；普通 MQTT 遥测不含该配置。

云端 v2 的 LTE/NR 锁频动作另行拒绝空频段、0、重复或越界频段。设备原版实测空串可能清空允许频段导致断网；NMS 恢复自动模式必须使用设备报告的完整支持频段列表，不发送空串。

APN 配置同样只在远程面板会话期间按需读取，输出仅限配置 ID、名称、APN、鉴权/PDP 类型与启用状态；原厂回包中的 APN 用户名和密码不会上传。修改既有手动 APN 时，前端留空的用户名、密码和其他未改动可选字段由 datad 在设备本地读取并保留。云端 v2 的 APN 模式、添加、修改、启用、删除都要求显式确认。

链路聚合切换及 `multiwan.interface/member/policy/rule.set` 均属于可能改写路由的操作，云端 v2 要求确认；设备仍使用各动作原有的 section 类型和参数范围校验，并在 MULTIWAN 模式下应用 mwan3 配置。

风扇、液冷的启停、模式及曲线写入可能改变散热行为，云端 v2 均要求显式确认；设备端原有温度上限保护和曲线参数校验保持有效。

散热设备存在时，面板会话按需读取当前风扇/液冷模式与 datad 有效保存的风扇曲线 `cooling_config`；无散热设备不提供该块。NMS 只接收 20–80℃、PWM 0–255、温度递增且 PWM 不下降的 2–8 个控制点。该配置不进入普通 MQTT 遥测。

## 可选面板控制通道（0.10.21）

0.10.51 新增独立 `datad_files` 文件会话，复用远程面板控制开关、单次票据和
TLS 来源验证，不开放设备本地文件端口；文件正文不进入普通遥测。其有界分块
传输不改变现有面板 8 KiB 控制帧限制。详见 [`FILES.md`](FILES.md)。

0.10.49 新增按需 `hosts_config` 和确认后的 `hosts.save` / `hosts.restore`。
内容只进入授权面板会话；成功的 Hosts 控制结果额外携带已读回验证的 SHA-256
`revision`，NMS 等待匹配状态，不把接收请求当作完成。详见
[`HOSTS_CONTROL.md`](HOSTS_CONTROL.md)。

原 `datad_panel` / `nms-datad-panel-v1` 继续严格只读。新增 `remote_panel_control_enabled` 默认 false，只有设备已启用云端和远程访问且明确打开此独立开关时，才声明 `datad.panel.control` 并接受 `datad_panel_control` / `nms-datad-panel-v2` 会话。它仍使用 NMS 下发的单次票据、已配置 HTTPS 来源、TLS 主机名和最长一小时的 0 端口 WSS；不开放 datad HTTP Token 或设备本地 UFI 代理。

V2 的状态帧带 `protocol_version:2`，仍只含显式允许的快照块。来自 NMS 的文本控制帧只接受结构化的 `type/control`、32 位十六进制 request_id、已列入 datad 控制动作白名单的 action、JSON 对象 params 和 confirmed 布尔值，最大 8 KiB；每会话最多 128 个不同 request_id，按顺序执行，每个动作最多 20 秒。重启、每日定时重启、关机、断网、锁频锁小区、SIM/Wi-Fi/LAN/DNS/供电/休眠/聚合变更、流量计数及限制、客户端解禁、QoS 日志清理、短信发送删除等动作要求 confirmed=true。结果只包含成功布尔值或有限错误码，不回传原厂响应或可能含密钥的原始内容。NMS 必须在每个动作前验证当前设备 Owner/管理员权限、CSRF 与会话有效性，记录不含秘密参数的审计；设备本身不会从浏览器直接接收控制帧。

休眠状态来自设备 UCI `zwrt_sleep.ztmp_time.SysIdTime`，`sleep` 快照仅提供 `supported` 和经过校验的 `minutes`（`-1/5/10/20/30/60/120`）；缺失或异常值不作为可配置档位显示。NMS 的 `sleep.set` 仍经设备控制白名单、确认和审计。
