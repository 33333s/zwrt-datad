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
| `nat.detect` | 可选 `servers`（1–6 个 `stun:host[:port]`） | 立即重新检测设备默认出口的 IPv4 NAT 类型（运营商侧），同步返回结果，通常 1–3 秒，最长 20 秒；结果同样写入缓存。见下文 |
| `nat.status` | 无 | 返回 `running`、当前 WAN IPv4 `wan_ipv4` 和缓存的检测结果 `last`（还没有结果时为 `null`）；datad 在 WAN IPv4 变化时自动检测，见下文 |
| `cloud.remote_features.set` | `remote_panel_control_enabled`、`remote_webshell_enabled`（布尔），`services`（名称、1–65535 端口、`web`/`terminal` 类型的完整列表） | 仅修改云端远程功能；需已启用云端连接及远程后台，不能修改连接地址、凭据或 CA。9460/9461 不可用，最多 8 项且端口不重复；远程 v2 调用要求 `confirmed=true` |
| `schedule.task.put` | `id`、设备本地 `time`（`HH:MM`）、`repeat_daily`、受限 `action`、`params` | 新增或修改本地任务；仅接受已支持的控制动作及精确参数，最多 16 项。0.10.39 起授权面板上报 `cell_lock_supported=true`，定时 `cell.lock_lte` / `cell.lock_nr` 仅接受有界十进制 PCI、频率及 NR 频段。0.10.40 起上报 `sms_send_supported=true`，定时 `sms.send_scheduled` 仅接受 `number`（3–32 位、可带前导 `+`）及 `text`（1–280 UTF-16 单元），正文仅存设备私有 0600 文件并进入授权按需面板流；执行前持久化尝试和与短信转发共用的每日 60 条本机 SIM 配额，失败不重试。重复任务每日最多尝试一次；远程 v2 要求 `confirmed=true` |
| `schedule.task.remove` | `id` | 删除本地任务；远程 v2 要求 `confirmed=true` |
| `sms.forward.set` | `enabled`、`method=webhook|dingtalk|sms|smtp`，可选 `webhook_url` / `dingtalk_webhook` / `dingtalk_secret` / `sms_to_phone` / `smtp:{host,port,username,password?,to}` / `power_forward_enabled` / `blacklist_phone` / `blacklist_keywords` / `nickname` / `smtp_forward_device_info` / `dingtalk_forward_device_info` / `sms_forward_device_info` | 保存受限转发设置；省略目标字段则保留，URL 设为空串、手机号数组设为空或 SMTP 字段全部置空可在关闭该方式时清除；SMTP 只接受证书验证的 TLS 465 或 STARTTLS 587，省略密码表示保留设备上现有密码；有电池且读数可用的设备可单独启用电源状态通知，开启时记录当前基线，总开关关闭时不投递。号码/关键词黑名单是可清空的数组，命中的新短信只登记指纹不投递，规则不影响电源通知。设备别名最多 255 字节，空串清除，省略则保留。三个设备信息布尔开关按转发方式独立保存、默认关闭，只改变向当前已配置目的地发送的正文，不进入常规遥测；首次启用总开关先强制刷新短信列表并建立基线；远程 v2 要求 `confirmed=true` |
| `sms.forward.test` | 无 | 向当前已配置目的地发送固定测试消息，不含真实收到的短信；短信方式会使用本机 SIM 向目标号码发短信并可能产生费用，SMTP 方式会发送一封测试邮件；远程 v2 要求 `confirmed=true` |
| `device.poweroff` | 无 | 关闭设备 |

UFI 自己的登录口令、HTTP 签名和浏览器会话不属于这里。

## Hosts

| action | params | 说明 |
| --- | --- | --- |
| `hosts.status` | 无 | 按需读取固定 `/etc/hosts`、精确 SHA-256 版本、备份与 DNS 服务状态；读取失败不返回伪造的空内容 |
| `hosts.save` | `content`、`expected_revision` | 最多 4096 字节 UTF-8；要求 `confirmed=true`，校验版本、首次备份、原子保存、读回并刷新 DNS |
| `hosts.restore` | `expected_revision` | 要求 `confirmed=true`；版本校验后恢复首次备份并刷新 DNS |

不接受任意文件路径或 shell 命令。云端控制帧仍限制 8 KiB，不截断超限内容。
详情见 [`HOSTS_CONTROL.md`](HOSTS_CONTROL.md)。

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

## NAT 检测

`nat.detect` 按 RFC 5780 测映射行为（mapping）和过滤行为（filtering），再换算成经典名称：

| `type` | `nat_level` | 含义 |
|---|---|---|
| `full_cone` | 1 | 映射与目标无关，任何外部地址都能回包（NAT1） |
| `restricted_cone` | 2 | 映射与目标无关，只接受访问过的 IP 回包（NAT2） |
| `port_restricted_cone` | 3 | 映射与目标无关，只接受访问过的 IP+端口回包（NAT3） |
| `symmetric` | 4 | 不同目标得到不同映射（NAT4） |
| `open` / `firewalled` / `no_nat` | `null` | WAN 地址就是公网地址；分别为不过滤、有过滤、过滤未知 |
| `udp_blocked` | `null` | 所有 STUN 服务器都没有应答（UDP 被封或服务器不可达） |
| `unknown` | `null` | 服务器不支持 RFC 5780 等原因，无法判断 |

返回字段：`mapping`/`filtering`（`endpoint_independent`、`address_dependent`、
`address_and_port_dependent`、`dependent`、`unknown`）、`nat`、`carrier_nat`（WAN 地址是私网或
100.64.0.0/10，说明 NAT 在运营商侧）、`measured_at`、`device`、`public_address`、`local_address`、
`port_preserved`、`server`、`rfc5780`、`duration_ms`、`checked_at`、`trigger`（`wan_change` 自动检测 /
`request` 调用 `nat.detect`）、`wan_ipv4`（检测时的 WAN IPv4 地址，排序），失败时另有 `error`（`dns_failed`、
`no_stun_answer`、`timeout`）。

0.10.78 起 datad 自动检测，不做定时检测：每 5 秒读取 `/state.interfaces.wan4.ipv4`，地址集合变化（启动后第一次拿到
IPv4 也算）并稳定 30 秒后检测一次，30 秒内再变就按新地址重新计时。结果连同当时的地址缓存到下一次 WAN IPv4 变化
（datad 重启后重新检测一次）。IPv4 消失时保留旧结果，之后拿回同一地址不算变化。检测失败（带 `error`，含
`udp_blocked`）时，同一地址下在 1、5、15 分钟后最多重试 3 次，仍失败就保留失败结果。已有检测或测速在进行时，
自动检测推迟到它们结束后。手动 `nat.detect` 的结果写入同一份缓存，绑定当时的 WAN IPv4。`nat.status.last.wan_ipv4`
与 `nat.status.wan_ipv4` 不同时，说明地址刚变、新结果还没出来。缓存摘要随 MQTT `telemetry/device.nat` 上报（见 CLOUD.md）。

主结果 `type`/`nat_level`/`filtering` 是**运营商侧**的 NAT 行为（`measured_at="carrier"`）。设备自身的
INPUT 防火墙只放行已建立连接的回包，会丢掉 STUN 服务器从另一个地址发回的探测包，普通套接字因此只能看到
“运营商 NAT + 设备防火墙”的结果。datad 为每个探测端口另开一个 AF_PACKET 抓包套接字（内核 BPF 只收发往该端口的
未分片 UDP），在 netfilter 之前看到运营商实际送到设备的回包；同一轮测试同时得出两种结果，不修改防火墙。
`device` 为 `{type, nat_level, filtering}`，是经过设备防火墙后、设备本身与局域网设备实际能得到的效果。
无法打开抓包套接字（非 root、非 Linux）时退回设备视角，`measured_at="device"`，主结果与 `device` 相同。

实测（0.10.74）：U60、MC7523、TopFlow 的移动网 CGNAT 为 `type=full_cone`（NAT1），`device.type=port_restricted_cone`
（NAT3）；MC8532B 直接获得公网 IPv4，`nat=false`、`type=open`。检测约 1.5–2 秒。

默认服务器依次为 `stun.miwifi.com:3478`、`stun.voipgate.com:3478`、`stun.fitauto.ru:3478`、
`stun.nextcloud.com:443`（以上支持 RFC 5780）、`stun.cloudflare.com:3478`、
`stun.chat.bilibili.com:3478`。部分服务器从不回应“只换端口”的请求，所以这一步会再用其他
RFC 5780 服务器确认。只提供不支持 RFC 5780 的服务器时，只能比较两台服务器看到的映射，
过滤行为为 `unknown`。同一时间只允许一次检测，并发请求返回 409 `operation_busy`。只测 IPv4。

## WiFi, LAN And Clients

| action | params |
|---|---|
| `wifi.status` | 无，返回 `main_2g/main_5g` 配置；有 6 GHz 的机型（MC8531）同时返回 `main_6g` |
| `wireless.config` | 无参数时返回各频段（2g/5g，有 6 GHz 的机型另有 6g）国家码、信道、带宽、设备国家列表和当前监管域合法信道；写入时传 `band`，并可传 `country/channel` |
| `wifi.dual_band_status` | 无，返回双频合一能力和开关状态 |
| `wifi.set_dual_band` | `enabled`，布尔值 |
| `wifi.state` | 无；分别返回 Wi-Fi 模块、各频段射频、主 AP、访客 AP 的开关和实际状态，见下文 |
| `wifi.set_module` | `enabled`，布尔或 `0/1`；Wi-Fi 模块总开关（原厂 `zte_mbb.wifi_onoff`） |
| `wifi.radio.set` | `band`=`2g`/`5g`（有 6 GHz 的机型含 `6g`），`enabled`；单个频段的开关（与原厂网页一致，通过该频段的 SSID 实现，见下文） |
| `wifi.guest.status` | 无；访客 SSID 开关与实际状态、访问时限、剩余时间、管理页访问、在线人数 |
| `wifi.bandwidth.set` | `band`、`width`（MHz）| 设置频段信道带宽：2.4 GHz 20/40，5 GHz 20/40/80/160（11n 只有 20/40），6 GHz Wi-Fi 7 另有 320；见下文 |
| `wifi.guest.set` | `enabled`（全部访客频段）、`bands`（如 `{"5g":true}`）、`active_time`（分钟）、`management_access` 至少一项 |
| `wifi.set_chip` | 已不支持，固定返回 400；改用 `wifi.radio.set` / `wifi.guest.set` |
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
| `wifi.acl.status` | 无，返回 Wi-Fi MAC 过滤模式和黑白名单 |
| `wifi.acl.set` | `mode`=`allow`/`deny`/`disable`，可选 `allow`、`deny`（MAC 数组） |
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

`wifi.configure` 的 `section` 可取 `main_2g/main_5g/guest_2g/guest_5g`；设备有 6 GHz AP（UCI
存在 `main_6g`）时还可取 `main_6g/guest_6g`，没有 6 GHz 的机型对这两个段返回 400，不会凭空创建。
6 GHz 强制 WPA3，`encryption` 只接受 `sae`。`client.block/unblock` 的黑名单同样写入 6 GHz 的主/访客
段（仅当它们存在）。

### Wi-Fi 开关与状态

原厂把 Wi-Fi 开关分成三层：模块总开关 `wireless.zte_mbb.wifi_onoff`、每个射频（wifi-device）的
`disabled`、每个 SSID（wifi-iface，`main_*`/`guest_*`）的 `disabled`。三层都打开且接口真正起来，
SSID 才在广播。

`wifi.state` 返回：

- `busy`：原厂 `zwrt_wlan.status` 的 `app_status`/`driver_status` 不是 `idle`。原厂重启 Wi-Fi 的几十秒内
  `module.enabled` 会暂时读到 `false`，`busy=true` 时不要把它当成用户关闭了 Wi-Fi。
- `module`：`enabled`（`wifi_onoff`，当前状态）和 `user_enabled`（`wifi_onoff_by_user`，用户设置）。
- `radios`：`[{band, device, enabled, radio_enabled}]`。`enabled` 是 `wifi.radio.set` 控制的频段开关：射频和该频段
  主 SSID 都打开才为 `true`；`radio_enabled` 只是射频（wifi-device）本身的配置。射频与频段的对应关系按机型从
  `wireless.main_<band>.device` 读取。`width` 是当前信道带宽（MHz，取自 `wifiN.htmode`
  的数字，读不到为 `null`），`widths` 是该频段可选的带宽，`htmode` 为原始值。
- `aps`：每个主/访客 SSID 一项 `{section, kind, band, ifname, enabled, state, active}`。`enabled` 是配置，
  `state` 是内核接口的 `operstate`（接口不存在为 `absent`），`active` 仅在 `state=up` 时为 `true`；
  接口启动过程中会短暂出现 `down`/`unknown`。没有 `ifname` 时 `state/active` 为 `null`。

读不到的字段一律为 `null`，不补 0 或 `false`。

`wifi.set_module` 写原厂 `zwrt_wlan.set {"zte_mbb":{"wifi_onoff":"0|1"}}`，并读回 `wifi_onoff_by_user`
确认。旧版本发送的 `SwitchOption` 会被所有已适配固件以 Invalid argument 拒绝，0.10.74 起已改正。
原厂行为：关闭模块会把所有射频置为关闭，打开模块会把所有射频重新打开，因此先前单独关掉的频段会被
重新打开。`wifi.radio.set` 按固件区分：

- **高通 qcacld32 单芯片固件**（射频 `type=qcacld32`：U60 Pro、MC7523、TopFlow）：原厂只剩一个 AP 时以单 AP 模式启动
  QCMAP，而 QCMAP 只在 `wlan0` 上起这个 AP，且所有接口都经 wlan0 创建。因此关闭一个频段采用 TopFlow 固件自身的布局：
  该频段射频 `disabled=1`，它的 main/guest/backhaul SSID 的 `ifname` 改为 `wlanx`，留下的频段主 SSID 改用 `wlan0`、
  访客/回传改用 `wlan1`；打开时启用射频，所有频段恢复原厂接口名（`wifiN` → 主 SSID `wlan(2N)`、访客/回传 `wlan(2N+1)`），
  主 SSID 关着的一并打开。写 UCI 后执行 `zwrt_wlan.reload`，原厂约一分钟完成重启。不会关掉最后一个开着的频段（返回 400，
  请用 `wifi.set_module`）；Mesh（`zte_mbb.mesh_onoff=1`）打开时不允许关闭频段（返回 400），因为 Mesh 下原厂用两个频段的
  回传 AP 经 ezmesh 启动，单频布局起不来（MC7523 实测），打开频段不受限。返回的 `options` 列出实际改动的 UCI 项。`wifi.configure` 关掉占用 `wlan0` 的
  主 SSID 时（只关主 SSID、保留访客）：该频段访客 SSID 开着时，访客改用 `wlan0`、主 SSID `disabled=1` 且 `ifname=wlanx`
  （U60 Pro 实测 QCMAP 保持两个 AP 正常启动），返回 `options`；访客关着时返回 400（请用 `wifi.radio.set`），因为只关这个
  SSID 时剩下的 AP 不在 wlan0 上，Wi-Fi 会整体起不来。主 SSID 停放在 `wlanx` 期间，单独关掉占着 `wlan0` 的访客 SSID 会被拒绝
  （400）；用 `wifi.configure` 打开主 SSID 时恢复原厂接口名。
- **U60 Pro（MU5250）最多同时运行两个 AP**：主 2.4 + 主 5 + 任一访客会让 QCMAP 以 `mode=4` 启动并报 `unsupported wifi
  mode`，Wi-Fi 整体起不来（0.10.79 实测）。`wifi.guest.set`、`wifi.configure` 打开 SSID、`wifi.radio.set` 打开频段时，
  结果超过两个 AP 会返回 400，不写配置。
- **其他固件**（MC8532B、MC8531 等 ath 驱动）：与原厂网页的单频段开关一致，只改 SSID：关闭写
  `{"main_<band>":{"disabled":"1"},"guest_<band>":{"disabled":"1"}}`（只包含开着的段），打开写主 SSID `disabled:"0"`，
  访客 SSID 仍由 `wifi.guest.set` 控制；射频被旧版关掉时一并恢复。返回的 `sections` 列出写入的段。

0.10.74–0.10.76 只写射频 `disabled`；在 U60 Pro / MC7523 上关 2.4 GHz 后 Wi-Fi 起不来，重启可能卡死。
两者都会让原厂重启全部
Wi-Fi，其他频段的 SSID 也会断开约 10–40 秒；返回成功只表示配置已确认，客户端应轮询 `wifi.state`
等待 `busy=false` 且 `active=true`。

`wifi.bandwidth.set` 与原厂“高级设置”一致，写 `zwrt_wlan.set {"<wifiN>":{"htmode":"<前缀><带宽>"}}`，前缀按射频
`hwmode` 取：`11bea/11beg` 为 `EHT`，`11axa/11axg` 为 `HE`，`11ac` 为 `VHT`，其他为 `HT`。带宽不在该频段可选列表内返回
400；与当前相同返回 `changed=false` 且不重启 Wi-Fi；否则读回 `htmode` 确认，返回 `{band, device, width, htmode,
previous, changed}`。改带宽会重启该射频上的 AP，客户端会短暂断开。

旧的 `wifi.set_chip`（`ChipEnum`/`GuestEnable`）在现有固件上没有对应字段，现在固定返回 400，
不再假装成功；已有的同名定时任务执行时会得到同样的错误。

### 访客网络

SSID、加密和密码继续用 `wifi.configure`（`section=guest_2g/guest_5g`）。其余访客设置用：

`wifi.guest.status` 返回 `guests` 数组，每项除 `wifi.state` 的 AP 字段外还有 `supported`（原厂
`wireless.readonly.wifi_attr_chip1/2_support_guest_ssid`，分别对应 2.4/5 GHz，没有该标记为 `null`）、
`ssid`、`encryption`、`active_time_minutes`、`left_seconds`、`clients`；以及顶层的
`active_time_minutes`（所有访客段一致时的值）、`active_time_supported`（`wifi_attr_support_guest_ssid_active_time`）、
`active_time_options`（`[0,120,240,480,720]`）和 `management_access`。

| 字段 | 原厂来源 | 单位/取值 |
|---|---|---|
| `enabled` | `guest_*.disabled` | 布尔 |
| `active_time_minutes` | `guest_*.guest_active_time` | 分钟，0 表示不限时 |
| `left_seconds` | `zwrt_wlan.wlan_get_guest_access_left_time` → `guest_left_time` | 秒；访客关闭时为 `null`（原厂关闭后仍会继续倒数一段时间） |
| `management_access` | `router_get_wifi_isolate` → `guest_access_restrict_enable` | `0` 为允许（`true`），`1` 为禁止（`false`） |
| `clients` | `router_get_user_list_num` → `guest_num_24g/5g/6g` | 人数；读不到为 `null` |

剩余时间和人数只在调用时读取，datad 不会周期轮询。

`wifi.guest.set` 只改传入的字段，其余保持不变：`enabled` 作用于所有支持访客的频段，`bands` 按频段开关
（与 `enabled` 同时给出时以 `bands` 为准）；`active_time` 只能取 0/120/240/480/720，写到所有访客段；
`management_access` 写 `router_set_wifi_isolate`，只发送 `guest_access_restrict_enable` 这一个字段
（原厂只改传入字段；部分机型的隔离设置里有读接口不返回的字段，整组写回会丢失它们）。SSID 相关字段合并
为一次 `zwrt_wlan.set`，与当前值相同的字段不写，全部未变时返回 `changed=false` 且不重启 Wi-Fi。写入后
读回确认；读不到隔离设置时 `management_access` 写入返回 502。开启访客会触发原厂重启 Wi-Fi。

### Wi-Fi 黑白名单

原厂把过滤模式写在每个 SSID 段的 `macfilter`（`allow` 白名单、`deny` 黑名单、`disable`
或未设置为不过滤），名单分别存在 `allowmaclist` 和 `denymaclist`；`zwrt_wlan.reload`
按当前模式把对应名单同步成驱动实际使用的 `maclist`，并断开新策略不允许的设备。两份名单
互相独立，切换模式不会丢失另一份。

`wifi.acl.status` 返回 `mode/guest_mode/allow/deny/consistent/max_entries`。模式和名单取自 `main_2g`
（原厂以它为整机策略），`guest_mode` 是访客 SSID 应有的模式；`consistent=false` 表示某个 SSID 段与此不一致（例如手工改过
UCI 或用原厂网页保存过）。
MAC 统一为小写并去重。

`wifi.acl.set` 把模式和名单写到所有主/访客 SSID 段（有 6 GHz 的机型含 6 GHz 段）。0.10.79 起白名单只作用于
主 SSID：`mode=allow` 时访客 SSID 写成黑名单模式（`macfilter=deny`，名单相同），访客无需加入白名单即可连接访客 Wi-Fi，
黑名单仍对访客生效；`deny`/`disable` 时访客与主 SSID 相同。原厂按每个 SSID 自己的 `macfilter` 给对应接口下发
（`qcacld32.sh`、`zte_post_wifi.sh` 的 `iwpriv setAclMode`），U60 Pro 实测两个 SSID 可分别生效。原厂网页保存白名单时仍会
写到所有段，此时 `wifi.acl.status.consistent=false`，再调用一次 `wifi.acl.set` 即可恢复。省略 `allow` 或 `deny` 时保留现有名单，传空数组表示清空。每份名单最多 64 条，
MAC 格式非法、重复项会被拒绝或合并。`mode=allow` 时白名单不能为空，否则所有设备都会被断开；
调用方应确认当前操作设备的 MAC 已在白名单内，否则保存后会被立即断开。配置与现有完全相同
时返回 `changed=false` 且不重载 Wi-Fi；否则提交后重载一次并读回核对，`verified=true`。

`client.block/unblock` 只维护黑名单。白名单模式下调用它们会返回 HTTP 409，错误码
`wifi_acl_allow_mode`，不会改动任何配置；旧版本会把模式强制改成 `deny`，悄悄关闭白名单。
黑名单为空或未开启过滤时，`client.block` 会开启黑名单模式。

TopFlow 的额外 SSID 由 datad 自己的 hostapd 承载，原厂脚本不会刷新它们的过滤列表。datad
按同频段主 AP 的模式和名单给额外 SSID 生成独立的 `macaddr_acl` 与名单文件；名单或模式
变化（包括在原厂页面修改）后，下一次巡检（约 5 秒）会重启该额外 AP 以应用新名单。

`wifi.configure` 检查原厂 reload 的业务结果和提交后的字段读回；原厂拒绝或
读回不一致返回错误。`verified=true` 只表示配置已核对，不表示 AP 广播已恢复；
原厂异步无线重载可能还需要数十秒。失败时配置可能已经提交，应重新读取确认。

频段与射频（wifi-device）的对应关系因机型而异（MC8531：`wifi2`=2.4G、`wifi0`=5G、`wifi1`=6G），
datad 读取 `wireless.main_<频段>.device` 确定，只有该段没有给出时才回退到 `wifi0`（2g）/`wifi1`（5g）。
`wireless.config` 的国家码作用于整台无线芯片，因此写入任一频段时会同步所有射频的
`country`（2g/5g，有 6 GHz 的机型含 6g）。信道列表按频率判定频段：iwinfo 不返回 `band` 字段时
（MC8531）按中心频率区分 2.4/5/6 GHz，因为 5 GHz 与 6 GHz 的信道号会重复。信道 `0` 或 `auto` 表示自动。
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

### 多基带 APN 目标（0.10.54，MU5252）

五个 `apn.*` 写动作可带整数 `slot_id`。`1/101/201` 是原厂 APN 配置目标：`1` → 5G 主基带，
`101` → 4G2，`201` → 4G1，**不是**物理 SIM 卡槽（`sim.set_slot` 仍只接受 1/2）。
datad 校验后把它翻译成设备接口的 `slotId`：

- 只有模板带外挂基带（目前 MU5252）才接受 `1/101/201`；`1` 是任何机型的主基带，所以单基带机型上
  `slot_id: 1` 等同于不带该参数（始终发送目标的客户端照常可用，0.10.54–0.10.56 曾误拒）。其他值、
  非整数、范围外或未知目标直接 `invalid_parameter`，不会回退去改主基带。不带 `slot_id` 时保持原单基带行为。
- 写入前先读该目标；读取失败的目标（面板中 `writable:false`）拒绝写入。
- 外挂基带（101/201）只有一套固定配置，datad 自身拒绝 `apn.add` / `apn.delete`。
- 修改已有配置时读取的是同一目标的原账号字段；写入后读回该目标，不一致返回 `device_call_failed`。
- 面板流 `apn_targets` 见 [`STATE_SCHEMA.md`](STATE_SCHEMA.md)。

## 设备后台会话与自动更新（0.10.54，面板/本机控制）

| action | params |
|---|---|
| `device.session.login` | `{"password":"..."}`，最长 1024 字节，保留首尾空格，拒绝换行和 NUL |
| `datad.ota.set` | `{"enabled":true/false}`，面板要求 `confirmed=true` |

`device.session.login` 按原厂网页的哈希流程调用 `zwrt_web.web_login`，用 `session list`
确认会话后才返回 `{"active":true}`。密码只用于这一次调用，不保存、不进入快照、错误、日志或
审计；datad 另限每分钟最多 3 次尝试，原厂锁定期内不发起登录。失败码：`invalid_credentials`、
`device_session_rate_limited`、`device_session_unavailable`。注意原厂后台是单会话，成功登录会
顶替网页端已有会话。

由该动作建立的会话之后若被替换或过期，`sms.send_raw` 与全部 `apn.*` 写动作会在发送/修改前返回
`device_session_expired`（本机 HTTP 409），不自动重试登录，也不会补发短信。从未登录过的设备保持
旧行为。`datad.ota.set` 只改自动更新开关（保留服务器和来源），读回配置文件确认后才报告成功。

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

`sms.send_raw` 接受已经编码的 UCS-2 hex。主卡发送是否加密由固件决定：UCI `zwrt_wms.config.sms_no_need_encryption_flag` 为 `0`（MU5252）时按原厂网页的方式用 AES-GCM 信封加密号码和正文；没有这个标志的固件（MU5250、MC7523、MC8532B）其短信服务接收明文，datad 直接发送明文号码和正文（0.10.56 及以前在这些机型上误发了加密信封，短信无法发出）。`sender` 为空或 `host` 时使用当前主卡；TopFlow 可选 `5G`（即主卡），不区分大小写；`4G1`（V3E2）、`4G2`（V3E1）及 0.10.52 前的别名 `v3e1`/`v3e2` 暂不支持发送，datad 直接返回参数错误（HTTP 400，`4G1 SMS sending is not supported`），不会发出任何请求——外挂模块的网页接口需要 datad 不持有的厂商登录，AT 通道又由模块自身守护进程占用；`x75` 仍作为 `5G` 的别名接受；普通双卡机型可选 `sim1`、`sim2`，datad 会先用原厂 provisioning 接口激活目标卡槽并等待切换完成。主机 WMS 发送会使用 datad 已注册的厂商 AES-GCM Web 会话加密号码和正文，并轮询 `sms_cmd=4`，只有状态 3 才返回成功。文本编码、转发、黑名单和业务去重继续由 UFI 负责。

## MU5252 Aggregation And Cooling

已配置 datad 散热后，采样同步原厂常开开关的变化：风扇开启为128、关闭恢复自定义曲线，液冷开启为最高档、关闭交还 thermal。液冷始终保持驱动 `thermal_enable=1`，常开写原厂高档波形，关闭写 `0 0 0`；驱动在 `thermal_enable=0` 时只保存波形参数而不播放。`cooling.vendor_sync_error` 报告同步失败。旧采样不会覆盖较新的 datad 控制请求；复用已有字段读取，不增加轮询。80°C 过热保护保持不变。

以下动作只应在 `/state` 实际输出 `aggregation` / `cooling` 的 MU5252 模板上显示：

| action | params | 说明 |
|---|---|---|
| `aggregation.set` | `enabled` | 开启时切到 `SMULTIWAN` 并停止 mwan3；关闭时停止 ICG、切到 `MULTIWAN` 并重启 mwan3 |
| `multiwan.status` | 无 | 只读，按需读取 mwan3：返回 `mode`、`active`、`service_running`、`sections[]`（mwan3 安全配置字段，按 `globals`/`interface`/`member`/`policy`/`rule` 标明类型，list 为数组）和 `paths[]`/`path_count`/`online_path_count`（X75/V3E1/V3E2/启用时的 Ethernet 承载链路的接口、跟踪/在线状态、探测延迟、丢包、时长和探测目标；摘要优先选 `up/online` 目标、忽略 `skipped`；mwan3 未运行时 `online` 表示底层接口已连接）。会调用约 1 秒的 `mwan3 status`，只在打开聚合详情或多 WAN 配置时请求；非 MU5252 返回参数错误 |
| `multiwan.interface.set` | `section` 与探测字段 | 修改已存在 interface 的启用、Ping 目标、次数、包大小、TTL、超时、间隔和上下线阈值 |
| `multiwan.member.set` | `section,metric,weight` | 修改已存在 member 的优先级与权重 |
| `multiwan.policy.set` | `section,last_resort,use_member` | 修改已存在 policy 的成员列表与无可用链路时动作 |
| `multiwan.rule.set` | `section,use_policy,sticky,logging` | 修改已存在 rule 使用的策略、会话保持与日志开关 |
| `cooling.fan.set_enabled` | `enabled` | 兼容 action 名；`true` 切到常开，`false` 切到自定义曲线 |
| `cooling.fan.set_mode` | `mode` | `automatic` 使用原厂内核三档曲线，`custom` 使用保存的 2–8 点线性曲线，`always_on` 固定 PWM 128 |
| `cooling.fan.set_curve` | `points:[{temperature,pwm},...]` | 保存曲线；常开已开启时不退出常开，关闭后使用保存的曲线，否则立即启用曲线。2–8 点，温度严格递增、PWM 不递减 |
| `cooling.liquid.set_enabled` | `enabled` | 兼容 action 名；`true` 固定高档 `1023 90 200`（幅值 90），即使此前选过低档也切到高档；`false` 交还 thermal 控制 |
| `cooling.liquid.set_mode` | `mode` | MU5252 液冷模式：`automatic` 交还内核 thermal；`low` 使用原厂低档幅度 60；`high` 使用幅度 90（aw86320 驱动接受的上限，0.10.79 前写 200 会被驱动静默丢弃、泵不转）。两档均保持频率 200，不伪造连续百分比 |

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


### App forwarding settings

Both listeners expose the following routes with an explicit Bearer token check,
including the loopback listener. Query-string tokens are not accepted.

- `GET /sms/forward/config`: private editor response `{config,status}` with
  `Cache-Control: no-store`. Targets and filter rules are included; SMTP passwords
  and DingTalk signing secrets are replaced by `password_configured` (inside
  `smtp`) and `dingtalk_secret_configured`. Internal fingerprints are excluded.
- `POST /sms/forward/config`: the same validated payload as `sms.forward.set`;
  returns status. Omitted secrets retain their previous values. An explicit empty
  DingTalk secret clears signing. To clear SMTP, disable/switch away from SMTP and
  submit empty host/username/password/to and port 0. Body limit: 32 KiB.
- `GET /sms/forward/status`: destination-free capabilities and delivery status.
- `POST /sms/forward/test` with `{}`: sends the existing fixed test message to the
  saved destination, including any applicable SIM charges. It does not accept
  an arbitrary recipient or body. Body limit: 1 KiB.

These editor fields are not added to `/state`, SSE or ordinary MQTT telemetry.
App edits do not require enabling NMS. DingTalk signing still uses the configured
NMS platform's HTTPS Date source for its trusted timestamp.


## App 定时任务

`GET /tasks` 返回任务列表、设备本地时间、`max_tasks`、当前支持的 `actions` 与 `network_modes`。
`POST /tasks` 使用与 `schedule.task.put` 相同的 `id/time/repeat_daily/action/params`，按任务名新增或更新；
`POST /tasks/remove` 仅接受 `{"id":"任务名"}`。三者在 9460、9461 均要求请求头中的有效令牌，
不接受查询字符串令牌；响应标记 `Cache-Control: no-store`。短信正文只进入此鉴权接口和设备私有文件，
不进入 `/state` 或普通 SSE。客户端应按返回的动作、网络模式和卡槽能力提供选项，不能默认设备支持双卡。

任务按设备本地 `HH:MM` 执行；`repeat_daily=false` 为下次到达该时间执行一次，`true` 为每日执行。
关机期间错过的时刻不补执行。结果 `requested` 表示已提交执行，`ok/failed` 为执行成功/失败；
一次性任务在执行前持久化触发标记，重启不重放。原有 NMS 与 App 共用同一任务库。


# App history and LAN settings

These routes require a Bearer token in the `Authorization` header on both
listeners (including loopback). Query-string tokens are not accepted. Successful
and application-error replies use `Cache-Control: no-store`.

- `GET /traffic/history`: returns `days: [{date, bytes}]`, `device_date`,
  `oldest_date`, `sample_interval_seconds: 300`, and `max_days: 400`.
  Optional inclusive `start` and `end` use `YYYY-MM-DD` in the device's local
  calendar. Only observed days are returned. Bytes combine cellular upload and
  download; today's total is provisional. History persists across datad restarts
  and cannot reconstruct days before collection. Daily counter resets accumulate
  within the day; a new day starts from its own daily counter, never yesterday's.
- `GET /lan`: normalized current settings and limits. Unknown values are `null`.
  `dhcp_enabled`, `dhcp_start`, `dhcp_end`, `lease_seconds`, and `mtu` are
  exposed separately. MTU is the device network/WAN MTU, not a client-specific MTU.
- `POST /lan`: optional `ip`, `netmask`, `dhcp_enabled`, `dhcp_start`, `dhcp_end`,
  `lease_seconds`; unspecified fields use the current settings. Enabled DHCP
  requires a valid same-subnet pool excluding the network, broadcast and gateway
  addresses, and a whole-hour lease from 1 to 720 hours. Disabling DHCP preserves
  the saved pool and lease. Existing firmware validation and readback remain authoritative.
- `POST /lan/mtu`: `{ "mtu": 1400 }`, integer 576 through 1500. Saved separately
  from DHCP to avoid partially applying two unrelated settings.

LAN writes return `{settings, changed, verified}`. Identical settings avoid OEM
writes. `verified` requires matching readback after the control operation; a
successful submission alone does not mean verified. Changing LAN settings may
disconnect clients. Clients must not automatically retry writes after a timeout
or report an unconfirmed change as successful.
# Local activity history and network recovery

All routes below require a Bearer header on both listeners and return private,
`no-store` responses. They are independent of NMS enrollment and the App staying
open. Recovery is disabled by default and never runs an unrestricted command.

- `GET /activity`: newest-first execution history, up to 100 entries per page.
  Optional `before` (exclusive ID), `category` (`notification`, `task`, `recovery`)
  and `failed=true` filter the result. `next_before` is the next-page cursor.
  `max_entries` is 300. `storage_ok=false` means persistence failed.
- `POST /activity/clear` with `{}` clears all categories without changing tasks,
  forwarding configuration, recovery limits or the history level. IDs do not reset.
- `POST /activity/level` with `{"level": "off" | "basic" | "standard" | "detailed"}`
  selects how much is recorded (below) and returns the same view as `GET /activity`.
  `GET /activity` also reports `level`, `levels`, `flush_interval_seconds` (`null`
  at `detailed`) and `unsaved` (entries still only in memory).

History levels (default `standard`; the choice is saved and survives restarts):

| level | records | written to flash |
|---|---|---|
| `off` | nothing | – |
| `basic` | failures, and what recovery does to the network (redial, reboot, back online) | on an interval |
| `standard` | `basic` plus successful deliveries, task runs and recovery settings changes | on an interval |
| `detailed` | `standard` plus each change in the recovery probe result | every entry at once |

Below `detailed`, entries are kept in memory and written together every 300 seconds
when something changed, so a busy device makes at most one write per interval. They
are also written when the daemon stops normally and before a reboot or poweroff
that goes through datad. A crash or power loss can lose the entries of the last
interval; use `detailed` while investigating something that restarts the device.
`GET /activity` always shows what is in memory. A failed write keeps the entries
buffered and is retried at the next interval. The recovery intent and action budgets
are separate safety state and are always written before an action.
- `GET /network/recovery`: saved configuration plus runtime status, consecutive
  failure count, redial count, next permissible action time and manual-pause state.
- `POST /network/recovery`: saves `enabled` (default false), `interval_seconds`
  (30/60/120, default 60), `failure_threshold` (3–10, default 3),
  `cooldown_seconds` (180–3600, default 300), `max_redials` (1–5, default 3), and
  `reboot_after_failures` (default false), and `probe_urls` (1–4 HTTPS URLs, default
  `https://www.baidu.com/`, `https://www.qq.com/`, `https://cp.cloudflare.com/generate_204`).
  A probe URL must use port 443 (or none), carry no credentials, query or fragment, and
  name a public host: loopback, private, link-local and carrier-grade-NAT addresses and
  `localhost`/`.local`/`.lan`/`.internal`/`.home.arpa` names are refused, and duplicates
  are rejected. A saved configuration from before 0.10.72 gets the default list. Unknown
  properties are rejected. `probe_hosts` in the status is read-only and lists the hosts.
- `POST /network/recovery/resume` with `{}` explicitly releases a manual pause
  and starts a new redial round after a grace period. Hourly/reboot limits remain.
- `POST /network/recovery/check` with `{}` performs one read-only connectivity
  check and returns `online` and `checked_at`. It does not increment failure
  counters, enable recovery or perform recovery actions; limited to once per 10s.

Detection sends bounded HTTPS HEAD requests to every configured `probe_urls` entry at
once, with normal certificate validation, no proxy and no redirect following. The first
authenticated HTTPS response, including an HTTP error, proves reachability and ends the
round; errors must not be mistaken for a broken cellular connection. The default list
pairs two sites that answer in mainland China with a Cloudflare address that answers
almost everywhere, so an overseas SIM, roaming or a VPN is not read as an outage when
the Chinese sites are unreachable. Every probe failing for the configured number of
rounds triggers a bearer reconnect;
optional reboot follows exhausted redials. DNS/TLS/remote-host failures can affect
this check; it measures reachability, not bandwidth. It does not change bands,
cell locks, APNs or network mode. A submitted action is recorded as such; only a
later successful connectivity check reports network recovery.

Recovery requires a fresh, recognized cellular snapshot, excludes explicitly
disabled radio, and waits at least 120 seconds on startup/configuration changes.
Network-changing controls through datad (including scheduled/NMS controls) cancel
in-flight recovery plans and reset the failure streak. Explicit disconnect and
poweroff persist a manual pause; explicit connect or `/resume` releases it.
Changes made entirely outside datad cannot always be attributed to user intent;
disable recovery before intentionally disconnecting through the original UI.

Attempts are reserved on disk before device actions: maximum five actions in a
rolling hour and one automatic reboot per six hours, including after restarts.
Two successful checks reset the current outage's redial count, but do not reset
hourly/reboot budgets. Invalid storage, stale snapshots or a regressed clock stop
recovery. Saving or probing never bypasses those limits.

History is stored privately in `activity.json`; recovery intent and budgets in
`network-recovery.json` (0600, atomic replacement). History records bounded reason
codes and task names, never message bodies, recipients, control parameters,
authentication headers or credentials. Identical repeated failure events are
coalesced for five minutes. Existing history cannot be reconstructed retroactively.


### App datad updates (private on 9460 and 9461)

All routes require an Authorization header; query tokens are not accepted. Responses are `Cache-Control: no-store`.

- `GET /ota/app`: `{status, config, busy, platform, candidate}`. Status remains readable during network downloads. `candidate` is null or `{id, manifest}`; manifest optionally contains signed plaintext `notes`.
- `POST /ota/app/config`: existing OTA `{enabled, servers, sources}` configuration. `enabled` controls automatic idle installation. Custom directories require HTTPS and the **same official Ed25519 signature** as built-in sources; this is mirror selection, not arbitrary binary installation. Configuration changes invalidate the previous candidate.
- `POST /ota/app/check` with `{}`: 202 schedules a check; poll status. 409 indicates another job is running.
- `POST /ota/app/install` with `{candidate_id: "<id returned above>"}`: 202 schedules installation of exactly that checked manifest and source. Stale/missing candidates or concurrent jobs return 409. No URL, hash or shell command is accepted from the App.

App updates use the existing signed `update.json` and `update.json.sig`, sources, installer and built-in verification key. Publish artifacts before the signed manifest. `scripts/sign-update.py --notes release-notes.txt` adds signed plaintext release notes.

`installing`/95% means the installer is running, not success. After service restart, `succeeded` requires the installer success marker and the expected running datad version. A missing result after 30 minutes is reported as unconfirmed. The result is read when the new process starts and then every minute, also with `ZWRT_DATAD_OTA_DISABLE_AUTO=1` (which only turns off automatic checks and installs), and before any check or install requested by the App or NMS, so a finished installation never keeps reporting "update in progress". The App refreshes its saved ZWRT credentials when the daemon loses in-memory sessions; without saved credentials, reconnect manually. Older daemons require one manual upgrade to expose these routes.
