# u60-datad state schema

`u60-datad` publishes a single normalized snapshot at:

```text
/tmp/u60-datad/state.json
```

Consumers read this file. They must not call `ubus` directly.

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
    "charger_type": 4
  },
  "clients": {
    "total": 0,
    "wifi": 0,
    "lan": 0,
    "list": [ { "name": "phone", "ip": "192.168.0.31", "mac": "ce:07:c3:6e:e1:76" } ]
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
    "ambr_dl": 300.0,
    "ambr_ul": 100.0,
    "ambr_dl_raw": 300,
    "ambr_ul_raw": 100,
    "ambr_dl_unit": 6,
    "ambr_ul_unit": 6,
    "usb_mode": "debug"
  },
  "system": {
    "uptime": 11202,
    "cpu_temp": 41,
    "cpu_usage": 17,
    "mem_used_pct": 52,
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
| `clients.total/wifi/lan` | `zwrt_router.api router_get_user_list_num` |
| `clients.list` | `/tmp/dhcp.leases`（每行 `expiry mac ip name clientid`） |
| `nfc.switch` | `zwrt_nfc zwrt_nfc_wifi_get`（1=开） |
| `wlan.enabled` | `uci get wireless.main_2g.disabled`（取反） |
| `dhcp.*` | `uci`：`network.lan.ipaddr`、`dhcp.lan.zte_start/limit/leasetime` |
| `system.cpu_usage` | `/proc/stat`（相邻轮询的占用率差值） |
| `system.sw_version` | `uci get zwrt_common_info.common_config.wa_inner_version` |
| `system.imei` | `zwrt_zte_mdm.api get_imei`（一次性） |
| `traffic.*` | `zwrt_data get_wwandst {"source_module":"deviceui","cid":1,"type":1}` |
| `qos.*` | tail-read `/data/logfs/key.log` and parse latest `qci` / `session_ambr_*` |
| `system.uptime/mem` | `system info` |
| `system.cpu_temp` | `zwrt_bsp.thermal get_cpu_temp` (`cpuss_temp`) |
| `system.model/fw` | `system board` |

## Notes

- `traffic` is realtime session data from `type:1`.
- `qos.ambr_*` is normalized to Mbps.
- `qos.ambr_*_raw` plus `qos.ambr_*_unit` are kept so consumers do not need to duplicate the vendor unit table.
- `net.nrca` / `net.lteca`：载波聚合描述符，`;` 分隔载波、`,` 分隔字段，每个载波 11 个字段 `idx,PCI,?,band,arfcn,bw,?,rsrp,rsrq,sinr,rssi`。没有载波聚合时为空串。
- WiFi 段的键名用 `wlan` 而不是 `wifi`，避免消费端按子串查找时先命中 `clients.wifi` 计数。`wlan.key` 为明文密码，消费端应自行决定是否打码显示。
- `qos.usb_mode`：`debug` 表示 ADB 开启，`user` 表示关闭；切换可调用 `ubus call zwrt_bsp.usb set '{"mode":"user|debug"}'`。
- `qos.qci` / `qos.ambr_*`：来自 `key.log` 的 PDU 建立日志（偶发行）。后端读取较大的日志尾窗（6000 行）并**缓存最后一次读到的值**，因为这些行会随日志增长滚出窗口；一旦读到就保留，直到下次 PDU 重建。
- `clients.list` 上限 32 条；消费端可自行截断显示。NFC 切换用 `ubus call zwrt_nfc zwrt_nfc_wifi_set '{"switch":0|1,"flag":2}'`。
