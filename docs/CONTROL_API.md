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
| `lan.set` | `ip/netmask/dhcp_disabled/dhcp_start/dhcp_end/lease_seconds` |
| `lan.set_mtu` | `mtu` |
| `dns.set` | `primary/secondary/manual_ipv4/manual_ipv6` |
| `client.access` | 无，返回访问策略和设备列表 |
| `client.block` | `mac` |
| `client.unblock` | `mac` |
| `client.kick` | `macs`，逗号分隔 |
| `client.rename` | `mac`, `hostname` |

`wifi.configure.key` 是设备 WiFi 明文密码，只能在本机受 Token 保护的接口中传输，不应写入日志。

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

## SMS

| action | params |
|---|---|
| `sms.send_raw` | `number`, `message_hex`, `sms_time` |
| `sms.delete` | `ids`，使用设备要求的分号格式 |
| `sms.mark_read` | `ids`, `tag?` |

`sms.send_raw` 接受已经编码的 UCS-2 hex。文本编码、转发、黑名单和业务去重继续由 UFI 负责。

## MU5252 Aggregation And Cooling

以下动作只应在 `/state` 实际输出 `aggregation` / `cooling` 的 MU5252 模板上显示：

| action | params | 说明 |
|---|---|---|
| `aggregation.set` | `enabled` | 开启时通过 `router_set_wan_mode` 切到 `SMULTIWAN`；关闭时调用 `router_stop_agg_mode` |
| `cooling.fan.set_enabled` | `enabled` | 风扇总开关；同步厂商 `fan_switch_status` |
| `cooling.fan.set_speed` | `percent`，`0..100` | 切到手动模式并直接设置 PWM |
| `cooling.fan.set_curve` | `points:[{temperature,pwm},...]` | 启用 datad 自定义曲线；2–8 点，温度严格递增、PWM 不递减 |
| `cooling.liquid.set_enabled` | `enabled` | 驱动压电液冷并同步厂商 `liquid_cooling_switch_status` |

风扇/液冷配置持久化在 `/data/zwrt-datad/cooling.conf`，datad 重启时恢复。未启用自定义曲线时仍上报设备内核现有四档（约 `0/30/50/70%`）和三个阈值；手动/自定义模式关闭并持续压制厂商 `thermal_enable`，否则厂商控制器会把 PWM 0 拉回最低档 76。datad 每秒读取 `sys-therm-4` 温度、在控制点之间线性插值并直接写 PWM；80℃ 始终强制 PWM 255。datad 正常退出时会把 `thermal_enable` 交还厂商驱动作为停服保护。不另装 `/etc/init.d` 或外部风扇脚本。

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
