# u60-datad state schema

`dev` 分支当前通过 HTTP / SSE 暴露统一状态快照：

```text
GET  /state
GET  /state.json
SSE  /events
```

消费者不应直接打 `ubus`，也不应自己扫 `key.log`。

## Shape

```json
{
  "ts": 1781201029,
  "net": {
    "type": "SA",
    "bars": 5,
    "operator": "China Mobile",
    "band": "n28",
    "nr_rsrp": -87,
    "nr_rsrq": -11,
    "nr_snr": "12.7",
    "nr_rssi": -74,
    "lte_rsrp": 0,
    "lte_rsrq": 0,
    "lte_rssi": 0,
    "lte_snr": "",
    "rssi": 0,
    "mcc": 460,
    "mnc": 0,
    "nr_pci": 0,
    "nr_cell_id": 0,
    "nr_channel": 0,
    "nr_bw": "100MHz",
    "nrca": "0,273,1,41,504990,100,0,-140.0,-43.0,-23.0,-120.0;",
    "lteca": "",
    "net_select": "WL_AND_5G",
    "sa_bands": "1,28,41,78",
    "nsa_bands": "1,28,41,78",
    "lte_bands": "1,3,8,40,41",
    "wan_status": "ipv4_ipv6_connected"
  },
  "wlan": {
    "ssid": "MyWiFi",
    "key": "password123",
    "enc": "sae-mixed",
    "enabled": 1
  },
  "battery": {
    "percent": 66,
    "temp": 32,
    "online": 1,
    "health": 1,
    "time_to_full": 390,
    "charging": 1,
    "charger_connect": 1,
    "charger_type": 4,
    "chg_uv": 4794000,
    "chg_ua": 456000,
    "bat_uv": 4502765,
    "bat_ua": 59327
  },
  "clients": {
    "total": 0,
    "wifi": 0,
    "lan": 0,
    "list": [ { "name": "phone", "ip": "192.168.0.31", "mac": "ce:07:c3:6e:e1:76" } ]
  },
  "sms": {
    "unread": 2,
    "list": [ { "id": 53, "num": "10086", "date": "06-15 14:57", "unread": 0, "text": "正文…" } ]
  },
  "nfc": { "switch": 1 },
  "dhcp": { "ip": "192.168.0.1", "start": "192.168.0.2", "limit": "252", "leasetime": "86400" },
  "traffic": {
    "rx_speed": 1260,
    "tx_speed": 1081,
    "max_rx_speed": 15879,
    "max_tx_speed": 13243,
    "rx_bytes": 11569922,
    "tx_bytes": 10832964,
    "session_time": 11162
  },
  "qos": {
    "qci": 9,
    "ambr_dl": "20008.641",
    "ambr_ul": "10008.640",
    "usb_mode": "debug"
  },
  "system": {
    "uptime": 11202,
    "cpu_temp": 41,
    "cpu_usage": 17,
    "mem_used_pct": 52,
    "mem_total": 1667604480,
    "mem_avail": 789684224,
    "sw_version": "BD_FLYMODEMMU5250V1.0.0B27",
    "imei": "863500074315883",
    "model": "ZTE U60Pro",
    "hostname": "ZTE-U60Pro",
    "fw": "OpenWrt 23.05.4 r24012-d8dd03c46f"
  }
}
```

## Source mapping

| snapshot | source |
|----------|--------|
| `net.*` | `zte_nwinfo_api nwinfo_get_netinfo` |
| `net.nrca` / `net.lteca` | `zte_nwinfo_api nwinfo_get_netinfo`（载波聚合描述符，原样透传） |
| `net.wan_status` | `zwrt_router.api router_get_status_no_auth` |
| `wlan.*` | `uci get wireless.main_2g.{ssid,key,encryption}` |
| `qos.usb_mode` | `zwrt_bsp.usb list`（`mode`：`debug`=ADB 开，`user`=ADB 关） |
| `battery.*` | `zwrt_bsp.battery list` |
| `battery.charging/charger_*` | `zwrt_bsp.charger list` |
| `battery.chg_uv/chg_ua` | `/sys/class/power_supply/usb/voltage_now`、`current_now`（充电器输入 µV/µA） |
| `battery.bat_uv/bat_ua` | `/sys/class/power_supply/battery/voltage_now`、`current_now`（电池 µV/µA） |
| `clients.total/wifi/lan` | `zwrt_router.api router_get_user_list_num` |
| `clients.list` | `/tmp/dhcp.leases`（每行 `expiry mac ip name clientid`） |
| `sms.unread` | `zwrt_wms_get_wms_capacity` 的 `sms_dev_unread_num + sms_sim_unread_num` |
| `sms.list` | `zte_libwms_get_sms_data {page:0,data_per_page:32,mem_store:1,tags:10,order_by:"order by id desc"}`；`id` 为消息 ID（标记已读用），`text` 为 UTF-16BE 十六进制后端解成 UTF-8，`date` 由 `YY,MM,DD,HH,MM,SS,+TZ` 格式化为 `MM-DD HH:MM`，`unread` 取自每条 `tag`（**"1"=未读，"0"=已读**） |
| `nfc.switch` | `zwrt_nfc zwrt_nfc_wifi_get`（1=开） |
| `net.net_select` | `nwinfo_get_netinfo`（选网模式；锁频页可写回 `nwinfo_set_netselect`） |
| `net.sa_bands`/`nsa_bands`/`lte_bands` | `nwinfo_get_netinfo` 的 `nr5g_sa_band_lock`/`nr5g_nsa_band_lock`/`lte_band`（可用/已锁频段，逗号列表） |
| `system.mem_total`/`mem_avail` | `system info` 的 `memory.total`/`memory.available`（字节） |
| `wlan.enabled` | `uci get wireless.main_2g.disabled`（取反） |
| `dhcp.*` | `uci`：`network.lan.ipaddr`、`dhcp.lan.zte_start/limit/leasetime` |
| `system.cpu_usage` | `/proc/stat`（相邻轮询的占用率差值） |
| `system.sw_version` | `uci get zwrt_common_info.common_config.wa_inner_version` |
| `system.imei` | `zwrt_zte_mdm.api get_imei`（一次性） |
| `traffic.*` | `zwrt_data get_wwandst {"source_module":"deviceui","cid":1,"type":1}` |
| `qos.*` | 启动时全量扫描 `/data/logfs/key.log` 的 QoS `[DATA]` 日志并缓存；收到 `SIGUSR1` 或检测到换卡后重读 |
| `system.uptime/mem` | `system info` |
| `system.cpu_temp` | `zwrt_bsp.thermal get_cpu_temp` (`cpuss_temp`) |
| `system.model/fw` | `system board` |

## Notes

- `GET /state` / `GET /state.json` 返回的是完整 JSON 快照。
- `GET /events` 通过 `event: state` 推送完整 JSON；只有内容变化时才推送新快照。
- `traffic` is realtime session data from `type:1`.
- `qos.ambr_*` 为 Mbps 字符串，保留 3 位小数；空串表示当前还没从日志里读到有效值。
- `net.nrca` / `net.lteca`：载波聚合描述符，`;` 分隔载波、`,` 分隔字段，每个载波 11 个字段 `idx,PCI,?,band,arfcn,bw,?,rsrp,rsrq,sinr,rssi`。没有载波聚合时为空串。
- WiFi 段的键名用 `wlan` 而不是 `wifi`，避免消费端按子串查找时先命中 `clients.wifi` 计数。`wlan.key` 为明文密码，消费端应自行决定是否打码显示。
- `qos.usb_mode`：`debug` 表示 ADB 开启，`user` 表示关闭；切换可调用 `ubus call zwrt_bsp.usb set '{"mode":"user|debug"}'`。
- `qos.qci` / `qos.ambr_*`：来自 `key.log` 的 PDU 建立日志（偶发行）。后端会在启动时**全量扫描整份日志**，逐行提取最近一次有效的 `qci` / `apn_ambr_*`，然后只对外暴露缓存；收到 `SIGUSR1` 或检测到 `sim_iccid/current_sim_slot` 变化时，会清空当前 QoS 缓存并重读。
- `clients.list` 上限 32 条；消费端可自行截断显示。NFC 切换用 `ubus call zwrt_nfc zwrt_nfc_wifi_set '{"switch":0|1,"flag":2}'`。
- `sms`：只读。`sms.unread` 每轮刷新；`sms.list` 每 10 轮重读一次，**或在未读数变化时立即重读**（新短信到达、或界面标记已读后，红点/图标能马上更新），最多 32 条。`text` 解码支持代理对（emoji）。本工程不实现发送/删除。
- **标记已读**（界面行为，非本快照字段）：`ubus call zwrt_wms zwrt_wms_modify_tag '{"id":"<id1>;<id2>;","tag":0}'`（id 分号分隔、末尾带分号；`tag:0`=已读）。
