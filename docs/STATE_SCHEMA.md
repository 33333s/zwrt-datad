# zwrt-datad state schema

`dev` 分支当前通过 HTTP / SSE 暴露统一状态快照：

```text
GET  /state
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
  "device": {
    "profile": "mu5250",
    "profile_source": "model_name",
    "api_template": "MU5250",
    "api_template_label": "MU5250",
    "api_template_supported": 1,
    "vendor": "ZTE",
    "model_name": "MU5250",
    "hardware_version": "MU5250_HW1.0",
    "market_name": "U60 Pro",
    "alias_name": "U60 Pro",
    "board_name": "qcom,sdxpinn-idp"
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
    "model": "ZTE Device Name",
    "hostname": "zte-device",
    "fw": "OpenWrt 23.05.4 r24012-d8dd03c46f"
  }
}
```

## Template Docs

设备侧取数接口不再混写在一张总表里，而是按后端选中的模板拆分：

- `device.api_template = MU5250`
  - 见 [`models/MU5250.md`](models/MU5250.md)
- `device.api_template = MC8532B`
  - 见 [`models/MC8532B.md`](models/MC8532B.md)
- 其他模板
  - 待后续逐个补充

## Notes

- `GET /state` 返回的是完整 JSON 快照。
- `GET /events` 通过 `event: state` 推送完整 JSON；只有内容变化时才推送新快照。
- 后端会优先根据 `device.model_name` 选择 `device.api_template`；设备侧接口选择已经由后端完成，不需要前端再猜设备应该打哪套 `ubus/uci/sysfs`。
- `device.api_template_supported = 1` 表示当前机型已有明确模板；`0` 表示只落到了内部兼容模板，不应视为正式适配完成。
- 机型适配应优先使用 `device.model_name`；`device.profile` 是基于它生成的规范化键，便于模板映射。
- `device.market_name` / `device.alias_name` 只适合展示，不应作为模板切换主键，因为同名产品可能对应不同 `model_name` / 基带方案。
- `system.model` / `hostname` 是设备自报字段，消费端不应把示例值当成固定机型常量。
- `traffic` is realtime session data from `type:1`.
- `qos.ambr_*` 为 Mbps 字符串，保留 3 位小数；空串表示当前还没从日志里读到有效值。
- `net.nrca` / `net.lteca`：载波聚合描述符，`;` 分隔载波、`,` 分隔字段，每个载波 11 个字段 `idx,PCI,?,band,arfcn,bw,?,rsrp,rsrq,sinr,rssi`。没有载波聚合时为空串。
- WiFi 段的键名用 `wlan` 而不是 `wifi`，避免消费端按子串查找时先命中 `clients.wifi` 计数。`wlan.key` 为明文密码，消费端应自行决定是否打码显示。
- `qos.usb_mode`：`debug` 表示 ADB 开启，`user` 表示关闭；切换可调用 `ubus call zwrt_bsp.usb set '{"mode":"user|debug"}'`。
- `qos.qci` / `qos.ambr_*`：来自 `key.log` 的 PDU 建立日志（偶发行）。后端会在启动时**全量扫描整份日志**，逐行提取最近一次有效的 `qci` / `apn_ambr_*`，然后只对外暴露缓存；收到 `SIGUSR1` 或检测到 `sim_iccid/current_sim_slot` 变化时，会清空当前 QoS 缓存并重读。
- `clients.list` 上限 32 条；消费端可自行截断显示。NFC 切换用 `ubus call zwrt_nfc zwrt_nfc_wifi_set '{"switch":0|1,"flag":2}'`。
- `sms`：只读。`sms.unread` 每轮刷新；`sms.list` 每 10 轮重读一次，**或在未读数变化时立即重读**（新短信到达、或界面标记已读后，红点/图标能马上更新），最多 32 条。`text` 解码支持代理对（emoji）。本工程不实现发送/删除。
- **标记已读**（界面行为，非本快照字段）：`ubus call zwrt_wms zwrt_wms_modify_tag '{"id":"<id1>;<id2>;","tag":0}'`（id 分号分隔、末尾带分号；`tag:0`=已读）。
