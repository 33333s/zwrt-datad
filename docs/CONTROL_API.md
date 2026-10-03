# Device Control API

控制接口只用于 UFI 与 datad 的本机服务间通信。

```http
POST /control
Authorization: Bearer <token>
Content-Type: application/json
```

统一请求：

```json
{"action":"<action>","params":{}}
```

## Device Session

| action | params | 说明 |
|---|---|---|
| `device.login_info` | 无 | 获取设备登录 challenge |
| `device.login` | `password_hash` | 64 位 SHA-256 hex，调用 `zwrt_web.web_login` |
| `device.logout` | 无 | 清除 datad 内存中的设备会话 |
| `device.session_status` | 无 | 返回 datad 当前设备会话状态 |
| `device.change_password` | `old_hash`, `new_hash` | 修改某兴后台密码 |
| `device.reboot` | 无 | 重启设备 |
| `schedule.reboot.set` | `enabled`（布尔）、`time`（设备本地 `HH:MM`） | 保存 datad 每日定时重启；原厂周/间隔日计划开启时拒绝启用 |
| `speedtest.start` | `bytes`（1–50 MiB）、`threads`（1–5）、`runs`（1 或 3） | 设备默认出口直连固定 Cloudflare HTTPS 下载端点，异步返回状态；总量不超过 `bytes` |
| `speedtest.stop` | 无 | 取消正在运行的下载测试 |
| `cloud.remote_features.set` | `remote_panel_control_enabled`、`remote_webshell_enabled`（布尔），`services`（名称、1–65535 端口、`web`/`terminal` 类型的完整列表） | 仅修改云端远程功能；需已启用云端连接及远程后台，不能修改连接地址、凭据或 CA。9460/9461 不可用，最多 8 项且端口不重复；远程 v2 调用要求 `confirmed=true` |
| `schedule.task.put` | `id`、设备本地 `time`（`HH:MM`）、`repeat_daily`、受限 `action`、`params` | 新增或修改本地任务；仅接受已支持的控制动作及精确参数，最多 16 项。0.10.39 起授权面板上报 `cell_lock_supported=true`，定时 `cell.lock_lte` / `cell.lock_nr` 仅接受有界十进制 PCI、频率及 NR 频段。0.10.40 起上报 `sms_send_supported=true`，定时 `sms.send_scheduled` 仅接受 `number`（3–32 位、可带前导 `+`）及 `text`（1–280 UTF-16 单元），正文仅存设备私有 0600 文件并进入授权按需面板流；执行前持久化尝试和与短信转发共用的每日 60 条本机 SIM 配额，失败不重试。重复任务每日最多尝试一次；远程 v2 要求 `confirmed=true` |
| `schedule.task.remove` | `id` | 删除本地任务；远程 v2 要求 `confirmed=true` |
| `sms.forward.set` | `enabled`、`method=webhook|dingtalk|sms|smtp`，可选 `webhook_url` / `dingtalk_webhook` / `dingtalk_secret` / `sms_to_phone` / `smtp:{host,port,username,password?,to}` / `power_forward_enabled` / `blacklist_phone` / `blacklist_keywords` / `nickname` / `smtp_forward_device_info` / `dingtalk_forward_device_info` / `sms_forward_device_info` | 保存受限转发设置；省略目标字段则保留，URL 设为空串、手机号数组设为空或 SMTP 字段全部置空可在关闭该方式时清除；SMTP 只接受证书验证的 TLS 465 或 STARTTLS 587，省略密码表示保留设备上现有密码；有电池且读数可用的设备可单独启用电源状态通知，开启时记录当前基线，总开关关闭时不投递。号码/关键词黑名单是可清空的数组，命中的新短信只登记指纹不投递，规则不影响电源通知。设备别名最多 255 字节，空串清除，省略则保留。三个设备信息布尔开关按转发方式独立保存、默认关闭，只改变向当前已配置目的地发送的正文，不进入常规遥测；首次启用总开关先强制刷新短信列表并建立基线；远程 v2 要求 `confirmed=true` |
| `sms.forward.test` | 无 | 向当前已配置目的地发送固定测试消息，不含真实收到的短信；短信方式会使用本机 SIM 向目标号码发短信并可能产生费用，SMTP 方式会发送一封测试邮件；远程 v2 要求 `confirmed=true` |
| `device.poweroff` | 无 | 关闭设备 |

UFI 自己的登录口令、HTTP 签名和浏览器会话不属于这里。

## Cellular And Radio

| action | params |
|---|---|
| `cellular.connect` | 无 |
| `cellular.disconnect` | 无 |
| `cellular.set` | `enabled?`, `roaming?`, `connect_mode?` |

`cellular.set` 只提交调用方指定的字段及 `source_module/cid`。`get_wwaniface` 的
`enable` 是状态响应中的写入参数，可能在已连接时仍为 0；不得将整个读取结果
作为写入模板，否则仅修改漫游设置也会断开数据连接。
| `network.set_mode` | `mode` |
| `band.set_lte` | `bands`，逗号分隔；空串表示自动 |
| `band.set_nr_sa` | `bands` |
| `band.set_nr_nsa` | `bands` |
| `cell.lock_lte` | `pci`, `earfcn` |
| `cell.lock_nr` | `pci`, `arfcn`, `band` |
| `cell.unlock_all` | 无 |
| `sim.set_slot` | `slot`，设备侧编号 `1/2` |

`cellular.set` 会先读取完整 `get_wwaniface` 对象，再覆盖调用方提供的字段，避免固件清空未指定的 PDP、配置档案等属性。

## WiFi, LAN And Clients

| action | params |
|---|---|
| `wifi.status` | 无，返回 `main_2g/main_5g` 配置 |
| `wireless.config` | 无参数时返回两频段国家码、信道、带宽、设备国家列表和当前监管域合法信道；写入时传 `band`，并可传 `country/channel` |
| `wifi.dual_band_status` | 无，返回双频合一能力和开关状态 |
| `wifi.set_dual_band` | `enabled`，布尔值 |
| `wifi.set_module` | `enabled`，`0/1` |
| `wifi.set_chip` | `chip`, `guest_enabled?` |
| `wifi.configure` | `section` 与 `ssid/encryption/key/pmf/maxassoc/hidden/isolate/enabled` 可选字段 |
| `wifi.txpower.status` | 无；返回两频段的启用状态、功率百分比、配置功率、配置上限和原厂上限 |
| `wifi.txpower.apply` | `band`=`2g`/`5g`，以及 `percent`/`limit_dbm` 至少一项；一次提交并只重载一次 WiFi |
| `wifi.txpower.set_percent` | `band`=`2g`/`5g`，`percent`=10–100（10% 步进） |
| `wifi.txpower.set_limit` | `band`=`2g`/`5g`，`limit_dbm`=1–30；同时修改 `txpower` 与 `max_power` |
| `wifi.txpower.restore_limit` | `band`=`2g`/`5g`；分别恢复为 MU5252 原厂 19/18 dBm |
| `lan.set` | `ip/netmask/dhcp_disabled/dhcp_start/dhcp_end/lease_seconds` |
| `lan.set_mtu` | `mtu` |
| `dns.set` | `primary/secondary/manual_ipv4/manual_ipv6` |
| `client.access` | 无，返回访问策略和设备列表 |
| `client.block` | `mac` |
| `client.unblock` | `mac` |
| `client.kick` | `macs`，逗号分隔 |
| `client.rename` | `mac`, `hostname` |

`wifi.configure.key` 是设备 WiFi 明文密码，只能在本机受 Token 保护的接口中传输，不应写入日志。

`wifi.status` 的 `main_2g/main_5g` 均返回原始字符串配置：
`ssid/key/encryption/disabled/hidden/isolate/pmf/maxassoc`。不要把缺失字段直接当成
关闭状态再提交；`hidden=1` 表示隐藏 SSID，而不是开启广播。

`wifi.dual_band_status` 从原厂 `zwrt_wlan.wlan_uci_get_section` 的 `zte_mbb.lbd`
读取双频合一。返回 `supported/enabled` 布尔值，并保留
`WiFiDualBandSupported/WiFiDualBandEnabled/BandSteeringSwitch` 字符串兼容字段。
读取失败或未知状态返回错误，不伪装成关闭。`wifi.set_dual_band` 只写
`zwrt_wlan.set` 的 `zte_mbb.lbd` 并检查读回，返回 `changed/verified`；不修改网络隔离。
客户端应调用这个控制动作，不要用 `router_set_wifi_isolate` 代替。
通用 ubus 透传保持原厂语义，不会替客户端改写错误的方法调用。

`wifi.configure` 检查原厂 reload 的业务结果和提交后的字段读回；原厂拒绝或
读回不一致返回错误。`verified=true` 只表示配置已核对，不表示 AP 广播已恢复；
原厂异步无线重载可能还需要数十秒。失败时配置可能已经提交，应重新读取确认。

`wireless.config` 的国家码作用于整台无线芯片，因此写入任一频段时会同步
`wireless.wifi0.country` 与 `wireless.wifi1.country`。信道 `0` 或 `auto` 表示自动。
国家码变更后，datad 会先让厂商 `zwrt_wlan.reload` 应用监管域，再读取
`iwinfo.freqlist` 校验目标信道；因此原厂静态 `channellist` 未列出的 100-144
只有在目标国家的设备驱动实际开放时才能写入。设备重载期间会等待最长 20 秒让
`iwinfo` 恢复；校验或 reload 失败会恢复原国家码和信道。`wifi.configure` 收到的
字段与 UCI 当前值完全相同时不会重载 WiFi，避免一次页面提交重复触发无线重启。

`wifi.txpower.*` 只在 MU5252 上执行。触摸屏使用 `apply` 把百分比和上限一次提交；
`set_limit`、`restore_limit` 与 `apply.limit_dbm` 都会同时设置 radio 的 `txpower` 和
`max_power`。目标值没有变化时返回 `changed=false`，不重载 WiFi；发生变化时只提交一次
`wireless` 并重载一次 WiFi，重载失败会尝试恢复旧配置。这里返回的是配置/驱动目标，
不是天线端实测射频功率。

## APN

| action | params |
|---|---|
| `apn.list` | 无，返回模式、自动列表、手动列表和已启用 ID |
| `apn.set_mode` | `mode`，设备侧整数 |
| `apn.add` | `name`, `apn`，以及可选认证字段 |
| `apn.modify` | `profile_id`, `name`, `apn`，以及可选认证字段 |
| `apn.delete` | `profile_id` |
| `apn.enable` | `profile_id` |

认证字段为 `username/password/auth_mode/pdp_type/roaming_pdp_type`。

## USB, Sleep And NFC

| action | params |
|---|---|
| `usb.status` | 无 |
| `usb.set` | `mode/port_switch/network_protocol` |
| `sleep.status` | 无 |
| `sleep.set` | `seconds` |
| `nfc.set` | `enabled`（布尔或 0/1），`flag?`（1–6；省略时保留原厂当前 NFC Wi-Fi 目标，无法读取时回退 2） |

`usb.status` 保留原有 `result.typec`、`result.usb` 厂商字段，新增 `result.link`，内容等同于 `GET /usb/status` 与 `/state.usb`。仅查询握手速率推荐直接使用只读 `GET /usb/status`，不依赖厂商 UBus 查询成功。

## Traffic And QoS

| action | params |
|---|---|
| `traffic.set_limit` | `enabled`, `value?`, `type?`, `ratio?` |
| `traffic.set_clear_day` | `day`（1–31），`enabled?`（0/1，省略时为 1） |
| `traffic.calibrate` | `value` |
| `qos.reload` | 无，重新扫描 QoS 日志 |
| `qos.clear` | 无，截断已有的 `key.log/key.log.0` 并重读；轮转文件不存在不算失败 |
| `state.refresh` | 无，立即重采样 |
| `state.set_interval` | `milliseconds`，`500..5000`；运行时切换全局采样/SSE 推送周期，不重启 datad |

## SMS

| action | params |
|---|---|
| `sms.send_raw` | `number`, `message_hex`, `sms_time`，可选 `sender` |
| `sms.delete` | `ids`，使用设备要求的分号格式 |
| `sms.mark_read` | `ids`, `tag?` |

`sms.send_raw` 接受已经编码的 UCS-2 hex。`sender` 为空或 `host` 时使用当前主卡；TopFlow 可选 `x75`、`v3e1`、`v3e2`，其中 V3E 通过各自内网管理接口发送；普通双卡机型可选 `sim1`、`sim2`，datad 会先用原厂 provisioning 接口激活目标卡槽并等待切换完成。主机 WMS 发送会使用 datad 已注册的厂商 AES-GCM Web 会话加密号码和正文，并轮询 `sms_cmd=4`，只有状态 3 才返回成功。文本编码、转发、黑名单和业务去重继续由 UFI 负责。

## MU5252 Aggregation And Cooling

已配置 datad 散热后，采样同步原厂常开开关的变化：风扇开启为128、关闭恢复自定义曲线，液冷开启为最高档、关闭交还 thermal。液冷始终保持驱动 `thermal_enable=1`，常开写原厂高档波形，关闭写 `0 0 0`；驱动在 `thermal_enable=0` 时只保存波形参数而不播放。`cooling.vendor_sync_error` 报告同步失败。旧采样不会覆盖较新的 datad 控制请求；复用已有字段读取，不增加轮询。80°C 过热保护保持不变。

以下动作只应在 `/state` 实际输出 `aggregation` / `cooling` 的 MU5252 模板上显示：

| action | params | 说明 |
|---|---|---|
| `aggregation.set` | `enabled` | 开启时切到 `SMULTIWAN` 并停止 mwan3；关闭时停止 ICG、切到 `MULTIWAN` 并重启 mwan3 |
| `multiwan.interface.set` | `section` 与探测字段 | 修改已存在 interface 的启用、Ping 目标、次数、包大小、TTL、超时、间隔和上下线阈值 |
| `multiwan.member.set` | `section,metric,weight` | 修改已存在 member 的优先级与权重 |
| `multiwan.policy.set` | `section,last_resort,use_member` | 修改已存在 policy 的成员列表与无可用链路时动作 |
| `multiwan.rule.set` | `section,use_policy,sticky,logging` | 修改已存在 rule 使用的策略、会话保持与日志开关 |
| `cooling.fan.set_enabled` | `enabled` | 兼容 action 名；`true` 切到常开，`false` 切到自定义曲线 |
| `cooling.fan.set_mode` | `mode` | `automatic` 使用原厂内核三档曲线，`custom` 使用保存的 2–8 点线性曲线，`always_on` 固定 PWM 128 |
| `cooling.fan.set_curve` | `points:[{temperature,pwm},...]` | 保存曲线；常开已开启时不退出常开，关闭后使用保存的曲线，否则立即启用曲线。2–8 点，温度严格递增、PWM 不递减 |
| `cooling.liquid.set_enabled` | `enabled` | 兼容 action 名；`true` 固定原厂最高档 `1023 200 200`（幅值200），即使此前选过低档也切到高档；`false` 交还 thermal 控制 |
| `cooling.liquid.set_mode` | `mode` | MU5252 液冷模式：`automatic` 交还内核 thermal；`low` 使用原厂低档幅度 60；`high` 使用原厂高档幅度 200。两档均保持频率 200，不伪造连续百分比 |

风扇/液冷配置持久化在 `/data/zwrt-datad/cooling.conf`，datad 重启时恢复。状态中的 `always_on` 表示常开，`enabled` 仅作为同值兼容别名。`automatic` 会重新启用 `sys-therm-4` 并使用设备树的 44/48/53℃、30/50/70% 三档曲线；`custom` 会禁用该 thermal zone、清零 `pwm-fan` 的锁存 state，并每秒按保存曲线线性插值写 PWM；`always_on` 采用同一用户态控制路径持续写 PWM 128。三种模式都保持风扇 `thermal_enable=1`，且 80℃ 始终强制 PWM 255。`factory_curve` 与 `custom_curve` 分别返回原厂和自定义曲线；`curve` 保留为旧消费者兼容字段。液冷自动模式恢复其 `thermal_enable`，低/高档使用原厂固定硬件参数。datad 正常退出时会把风扇 thermal 控制交还厂商驱动作为停服保护。不另装 `/etc/init.d` 或外部风扇脚本。

温控节点按 `type=sys-therm-4`、风扇 cooling device 按 `type=pwm-fan` 动态发现，绝不回退到可能属于 CPU 的编号 0。用户态曲线/常开接管时，若驱动提供 `low_speed_mode`，会解除该限速，避免目标 PWM 被截断；写入后读回 PWM，不一致按失败处理。自定义曲线温度不可用，或控制写入失败时恢复原厂风扇温控，保留已保存曲线；`cooling.fan.policy` 明确报告应用是否成功与错误。PWM 读回不等同于物理转速反馈。

`multiwan.*.set` 只能修改已存在且类型匹配的 mwan3 section，不提供任意 UCI 路径写入。datad 会先校验所有 section、数值范围、IP 地址和引用关系再提交；`use_member` 与 `track_ip` 以受限列表替换。`MULTIWAN` 模式保存后重启 mwan3 并返回 `applied=true`，`SMULTIWAN` 模式只保存并返回 `applied=false`。

示例：

```json
{"action":"cooling.fan.set_curve","params":{"points":[
  {"temperature":40,"pwm":0},
  {"temperature":50,"pwm":76},
  {"temperature":60,"pwm":128},
  {"temperature":70,"pwm":255}
]}}
```

## Safety

- 所有动作必须存在于编译期白名单。
- 服务名和方法名不能由请求指定。
- `ubus/uci` 使用 `fork/exec` 参数数组执行，不经过 Shell。
- WiFi、APN 和密码字段不得写入运行日志。
- 重启、关机和密码修改应由 UFI 再做用户确认。
- 切换 `SMULTIWAN` 会重配 WAN，远程设备可能短暂断线；UFI 应明确提示用户。


### Topflow advanced wireless

`wifi.advanced.status` returns two radio entries and the configured SSIDs, including
live interface names, band, readiness, driver-reported power and PSM. Passwords
are omitted. Power readback is a driver/firmware value, not an RF measurement.

- `wifi.txpower.set_dbm`: `{band:"2g"|"5g", dbm:1..30}` stores a 1 dBm-step policy;
  the current channel's reported limit is enforced when available. `{band,mode:"oem"}`
  removes it and reapplies the OEM configured power. The legacy percentage setter
  rejects changes while a dBm policy is active.
- `wifi.psm.set`: `{section,mode:"on"|"off"|"default"}` persists an SSID-specific
  policy. `default` releases ownership and retains the current driver state until
  the next driver reset. Inactive interfaces receive saved policies when ready.
- `wifi.interface.configure`: `{section,ssid?,encryption?,key?,enabled?,hidden?,isolate?}`
  edits a stock main/guest interface. A blank key retains the existing password.
  `enabled`, `hidden` and `isolate` are 0 or 1. OEM changes can restart Wi-Fi;
  callers must poll readiness instead of treating save success as an active AP.
- `wifi.interface.create`: `{band,ssid,encryption,key,enabled?,hidden?,isolate?}`
  allocates one of two additional SSID slots. `wifi.interface.configure` also
  accepts those returned section IDs, and an optional `band` for them.
- `wifi.interface.delete`: `{section}` deletes only an additional SSID.

Supported additional-SSID encryption values are `none`, `psk2+ccmp`, `sae-mixed`
and `sae`. SSIDs contain 1-32 UTF-8 bytes; encrypted passwords contain 8-63 bytes.
Additional SSIDs bridge to the existing LAN; AP isolation does not provide a
separate guest subnet or block access to other LAN devices.

The OEM QCMAP loader accepts its four predefined sections. Additional SSIDs live
in the separate UCI package `datad_wifi` and use independently managed hostapd
processes in `/data/zwrt-datad/wifi/`, with private config files and reserved
interfaces wlan4/wlan5. Creation waits for an active stock AP on the same band.
The runtime restores these APs after a radio restart without installing hotplug
scripts or modifying the OEM loader. Ownership checks use PID command lines plus
boot ID and interface index before cleanup. PSM policies apply after readiness, once per interface generation or explicit edit.
Power is reapplied once after an atomic wireless config revision has settled,
because the OEM post-DFS workflow commits configuration before resetting power.
This does not reassert PSM, and it does not continuously fight arbitrary live
changes. Additional APs inherit the OEM computed radio power when no dBm override
is selected.

## Neighbor collection

| action | params | Result |
|---|---|---|
| `neighbor.status` | none | Current cached neighbor block |
| `neighbor.set` | `enabled`: boolean or 0/1 locally; exact boolean remotely | Process-scoped enablement and current lifecycle state. Remote v2 requires `confirmed=true`, an executable collector, and panel-control permission |

Collection is off by default and enabling lasts only until datad restarts. Older
saved `enabled:true` values are reset to false during startup. Disabling waits for
the owned collector to stop and removes its capture logs before returning success.
A successful enable request confirms startup, not compatible firmware reports; inspect `status`, `reason`
and `cells`. See [NEIGHBOR.md](NEIGHBOR.md) for the supported signature limits.

## Charger direct supply

| action | params | Result |
|---|---|---|
| `power.direct_supply.status` | none | `supported`, `enabled`, `mode` |
| `power.direct_supply.set` | `enabled`: boolean or 0/1 | Same fields plus `changed`, `verified` |

The adapter reads `zwrt_bsp.charger.list.direct_power_supply_mode` and writes only
that field through `zwrt_bsp.charger.set`, mapping true/false to enable/disable.
A missing field returns `supported:false,enabled:null,mode:null`. An unknown enum
returns `supported:true,enabled:null,mode:null`. Neither is treated as disabled,
and setting either fails with HTTP 502 before any write. Invalid parameters
return HTTP 400. Unchanged requests return `changed:false,verified:true` without
issuing a write. Changed requests return success only after bounded readback
confirms the requested enum. B20 returns an empty body on a successful write;
this is accepted only with confirmed readback. Command errors and unconfirmed writes return 502.
State is refreshed after uncertain writes as the hardware may have changed.
`verified` confirms the firmware setting, not an electrical current measurement.
No extra datad startup policy rewrites the mode; reboot persistence is whatever
the device firmware provides. Both actions use the existing token authentication.

## U50S ARM32 兼容说明

- `apn.list` 与按需云端面板优先读取原厂 HTTP 配置；接口不可用时只读本地 `cfg` 的白名单标量。兜底仅报告当前 APN（`current`）、模式、名称和认证/PDP 类型，不报告完整配置目录，不读取 APN 账号、密码或原始序列化配置，保持 `writable:false`。模式未知、字段过长或读取超时明确失败。
- `network.set_mode` 支持 `4G_AND_5G`（也接受 `auto`）、`Only_LTE`（`4G` / `LTE`）、`Only_5G`（`5G` / `NR`）。模式偏好与实际驻留网络是不同字段，选择 5G 不保证当地有 5G 覆盖。
- 频段 `bands` 可以是逗号分隔的数字字符串或整数数组。**空字符串 / 空数组是解除锁定（自动）**，与官方 WebUI 取消全选一致：LTE 写掩码 `0`、NR 写 `nr5g_band_mask=0`，成功返回 `mode:"auto"`。它不是“锁定全部出厂频段”——锁定全部仍会禁止调制解调器选用表外频段，与自动不同。
- 显式频段先对照设备出厂频段表（`lte_band_1_64_factory` / `nr5g_{sa,nsa}_band_factory`）校验：表外频段直接 `400` 拒绝且不发写。固件本身不校验频段位（实测会原样入库），锁上不支持的频段会导致失网，因此由 datad 挡在前面。
- LTE 掩码为官方调试页同款 16 位零填充十六进制（如 `0x000001e2080800d5`）；NR 为逗号分隔十进制频段表（如 `1,3,5,8,28,41,77,78`），SA/NSA 以 `type` 区分。U50S 使用 `SET_NETWORK_BAND_LOCK`；U50Pro 使用 `BAND_SELECT` 与 `WAN_PERFORM_NR5G_SANSA_BAND_LOCK`——这两个 goform 位于固件 developer 门禁清单内，OEM 桥接层会用本机管理员哈希自动完成 `DEVELOPER_OPTION_LOGIN` 提权（会话内一次，凭据不落盘），调用方无感知。
- 频段和网络模式写入仅在有界回读匹配后返回 `result.result="success"` 与 `verified=true`；原厂拒绝或回读不一致返回失败。
- 锁频类写入不需要切网；仅 SA 手动邻区扫描（`neighbor.scan_sa`）需要临时切到 `Only_5G`，扫完自动还原原模式。

### U50 Wi-Fi（主线契约）

U50 的 Wi-Fi 动作与主线（U60pro）同名同形：

- `wifi.status`：返回 `main_2g / main_5g / guest_2g / guest_5g` 各段的完整设置
  `ssid / key / encryption / disabled / hidden / isolate / pmf / maxassoc / writable`。
  `key` 为解码后的明文密码——与主线一致，受同一 token 鉴权保护；云端 NMS 面板视图仍然不含密码。
- `wifi.configure`：官方 `setAccessPointInfo` 通道。字段校验与主线一致
  （`ssid` ≤32、`key` 8–63 或空=保留原密码、`encryption` 枚举、`hidden/isolate/pmf` 0/1/2、
  `maxassoc` 1–128、`enabled` 布尔；`disabled` 字段与主线一样拒收，请用 `enabled`）。
  无变化不写（`changed:false`）；仅开关变化时只发 `AccessPointSwitchStatus`（官方 WebUI 同款
  最小写）；写后有界回读重试（最长 25 秒）等 AP 自重启。加密名映射：`psk2→WPA2PSK`、
  `psk-mixed→WPAPSKWPA2PSK`、`sae→WPA3PSK`、`sae-mixed→WPA2PSKWPA3PSK`；`none` 清空密码。
- `wifi.configure` 额外接受 **`channel`**（`main_5g` 与 `main_2g`）：`"0"` 恢复自动选信道，
  5G 为标准信道号（36–165），2.4G 为 1–13。写入走官方 `setWiFiChipAdvancedInfo`，重发当前无线模式/
  国家码/带宽仅改信道，回读校验。**DFS 信道（52–64、100–144）开台前有法规强制的
  约 60 秒 CAC 静默**——自动模式挤到 DFS 信道就是 5G 开得慢的根源；钉死
  36–48 或 149–165 等**非 DFS 信道**即可秒开。信道是整颗射频的设置，只在主段（`main_2g`/`main_5g`）暴露，访客段拒绝；
  `wifi.status` 的主段 `channel` 返回 `queryWiFiChipAdvancedInfo` 里的固定信道（`0` 表示自动）。
  `band.set_lte`/`band.set_nr_*` 传空列表（解锁）改为写入完整工厂频段表，因为 U50 Pro 固件的掩码 `0` 表示“不允许任何频段”。
- `wifi.set_dual_band`：官方双频合一开关走 `switchWiFiModule` 携带当前开关与 LAN 标志，
  仅写 `wifi_lbd_enable`，回读校验；无变化 `changed:false`。
- `wifi.set_module`：Wi-Fi 总开关（`switchWiFiModule` + `SwitchOption`），与主线动作名一致。

### U50 邻区

- `neighbor.set {enabled}` / `neighbor.status`：与主线同名同形状（见 `NEIGHBOR.md` 的 U50 章节）。
  会话级开关、默认关闭；`/state.neighbor` 在 U50 上为请求时实时读取，切换后立即生效。
- `neighbor.list`（U50 扩展，只读）：一次返回 `network_type`、`primary`、`lte[]`、`sa[]`
  两个结构化列表（字段 `rat/pci/arfcn/band/rsrp_dbm/sinr_db`，`band` 为数值）。
  LTE/NSA 邻区由固件注册后自动刷新；`sa[]` 只在手动扫描后变化。
- `neighbor.scan_sa`（U50 扩展）：SA 手动扫描，镜像官方隐藏调试页——固件仅在
  `net_select=Only_5G` 时接受扫描（`SCAN_NR5G_NEIGHBOR_CELL`，同样走自动提权），流程为
  切 `Only_5G`（25 秒上限）→ 扫描 → 轮询 `m_netselect_status`（150 秒上限）→ 取 `sa[]` →
  还原原模式。真机全程约 13 秒；锁小区不需要此流程。
