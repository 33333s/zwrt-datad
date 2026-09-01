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
| `device.poweroff` | 无 | 关闭设备 |

UFI 自己的登录口令、HTTP 签名和浏览器会话不属于这里。

## Cellular And Radio

| action | params |
|---|---|
| `cellular.connect` | 无 |
| `cellular.disconnect` | 无 |
| `cellular.set` | `enabled?`, `roaming?`, `connect_mode?` |
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
| `nfc.set` | `enabled`, `flag?` |

## Traffic And QoS

| action | params |
|---|---|
| `traffic.set_limit` | `enabled`, `value?`, `type?`, `ratio?` |
| `traffic.set_clear_day` | `day` |
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
| `cooling.fan.set_curve` | `points:[{temperature,pwm},...]` | 保存并启用 datad 自定义曲线，同时退出常开；2–8 点，温度严格递增、PWM 不递减 |
| `cooling.liquid.set_enabled` | `enabled` | 兼容 action 名；实际控制“液冷常开”。`true` 固定厂商参数 `1023 60 200`，`false` 解除强制并交还 thermal 控制 |
| `cooling.liquid.set_mode` | `mode` | MU5252 液冷模式：`automatic` 交还内核 thermal；`low` 使用原厂低档幅度 60；`high` 使用原厂高档幅度 200。两档均保持频率 200，不伪造连续百分比 |

风扇/液冷配置持久化在 `/data/zwrt-datad/cooling.conf`，datad 重启时恢复。状态中的 `always_on` 表示常开，`enabled` 仅作为同值兼容别名。`automatic` 会重新启用 `sys-therm-4` 并使用设备树的 44/48/53℃、30/50/70% 三档曲线；`custom` 会禁用该 thermal zone、清零 `pwm-fan` 的锁存 state，并每秒按保存曲线线性插值写 PWM；`always_on` 采用同一用户态控制路径持续写 PWM 128。三种模式都保持风扇 `thermal_enable=1`，且 80℃ 始终强制 PWM 255。`factory_curve` 与 `custom_curve` 分别返回原厂和自定义曲线；`curve` 保留为旧消费者兼容字段。液冷自动模式恢复其 `thermal_enable`，低/高档使用原厂固定硬件参数。datad 正常退出时会把风扇和液冷 thermal 控制交还厂商驱动作为停服保护。不另装 `/etc/init.d` 或外部风扇脚本。

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
