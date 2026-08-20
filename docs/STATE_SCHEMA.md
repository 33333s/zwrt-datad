# zwrt-datad state schema

服务通过 HTTP / SSE 暴露统一状态快照：

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
    "wan_status": "ipv4_ipv6_connected",
    "HSR": false
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
  "interfaces": {
    "lan": { "up": true, "proto": "static", "device": "br-lan", "ipv4": [], "ipv6": [], "dns": [] },
    "wan4": { "up": true, "proto": "dhcp", "device": "rmnet_data0", "ipv4": [], "ipv6": [], "dns": [] },
    "wan6": { "up": true, "proto": "dhcpv6", "device": "rmnet_data0", "ipv4": [], "ipv6": [], "dns": [] },
    "lan_config": {},
    "cellular": {}
  },
  "sim": {
    "iccid": "8986000000000000000",
    "imsi": "460000000000001",
    "msisdn": "10086",
    "state": "ready",
    "modem_state": "online",
    "pin_status": "disabled",
    "current_slot": 1,
    "dual_sim": 1,
    "sim1_provision": 1,
    "sim2_provision": 0
  },
  "dhcp": { "ip": "192.168.0.1", "start": "192.168.0.2", "limit": "252", "leasetime": "86400" },
  "traffic": {
    "rx_speed": 1260,
    "tx_speed": 1081,
    "max_rx_speed": 15879,
    "max_tx_speed": 13243,
    "rx_bytes": 11569922,
    "tx_bytes": 10832964,
    "session_time": 11162,
    "day_rx_bytes": 11569922,
    "day_tx_bytes": 10832964,
    "month_rx_bytes": 111569922,
    "month_tx_bytes": 101083296,
    "total_rx_bytes": 511569922,
    "total_tx_bytes": 410832964,
    "limit": {},
    "clear_day": {}
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
  },
  "runtime": {
    "cpu_usage_tenths": 172,
    "cpu_cores": { "cpu0": 180, "cpu1": 164 },
    "cpu_freq_mhz": { "cpu0": { "cur": 691, "max": 1728 } },
    "thermal_zones": [ { "type": "cpuss-0", "temp_milli": 41000 } ],
    "memory_kb": { "total": 1628520, "free": 120000, "available": 771176, "buffers": 4096, "cached": 420000, "swap_total": 0, "swap_free": 0 },
    "storage": { "total": 1946157056, "used": 95105856, "available": 1851051200 },
    "connections": { "tcp4": 12, "tcp6": 2, "udp4": 8, "udp6": 2, "unix": 91 },
    "throughput": { "rx_bps": 126000, "tx_bps": 108100, "window_ms": 15000 }
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
- `traffic.rx_speed/tx_speed` 和会话计数来自 `type:1`；日/月/累计值、限额与清零日由低频设备状态刷新补充。
- `interfaces` 每 5 秒刷新一次，控制成功时强制立即刷新。地址数组沿用 OpenWrt `network.interface.* status` 的对象结构。
- `sim` 每 5 秒刷新一次；检测到 ICCID、卡槽或 SIM 状态变化时同时刷新 QoS 缓存。
- `runtime.cpu_usage_tenths` 与 `runtime.cpu_cores.*` 单位为百分比的十分之一，例如 `172` 表示 `17.2%`。每核心占用与频率按采样周期实时读取，不缓存计算结果。
- `runtime.cpu_freq_mhz` 单位为 MHz；`thermal_zones.temp_milli` 单位为毫摄氏度；`memory_kb` 单位为 KiB；`storage` 和 `throughput` 单位分别为字节和字节/秒。
- `runtime.throughput` 优先使用 `br-lan`，回退到 WiFi 接口，最后才使用 rmnet，并采用最多 16 个样本的滚动窗口平滑 IPA 批量刷新。
- `qos.ambr_*` 为 Mbps 字符串，保留 3 位小数；空串表示当前还没从日志里读到有效值。
- `net.nrca` / `net.lteca`：载波聚合描述符，`;` 分隔载波、`,` 分隔字段，每个载波 11 个字段 `idx,PCI,?,band,arfcn,bw,?,rsrp,rsrq,sinr,rssi`。没有载波聚合时为空串。
- `net.HSR`：高铁专网确认结果。该值只应来自信令确认，不按 ARFCN/EARFCN 直接判定。当前公开版尚未实现 modem SIB1 信令确认链路，因此该字段保持 `false`，直到公开实现具备同等确认能力。
- WiFi 段的键名用 `wlan` 而不是 `wifi`，避免消费端按子串查找时先命中 `clients.wifi` 计数。`wlan.key` 为明文密码，消费端应自行决定是否打码显示。
- `qos.usb_mode`：`debug` 表示 ADB 开启，`user` 表示关闭；切换可调用 `ubus call zwrt_bsp.usb set '{"mode":"user|debug"}'`。
- `qos.qci` / `qos.ambr_*`：来自 `key.log.0` / `key.log` 的 PDU/EPS 建立日志（偶发行）。后端启动时按轮转旧日志到当前日志的顺序扫描，并按日志上下文缓存多个候选；当前 `key.log` 的有效候选优先于 `key.log.0`，后者只补充当前日志缺失的字段；同一日志内再优先采用当前 `net.mcc/net.mnc` 匹配的 `access_point=*.mncXXX.mccYYY.*` 承载。`dnn=ims` / emergency 这类信令承载不会覆盖主数据 AMBR；无 PLMN 的非 IMS `dnn=` 可作为主数据候选。裸 `qci = ...` 只有在紧跟有效数据承载上下文，或完全没有更可信 QCI 时才作为兜底；收到 `SIGUSR1` 或检测到 `sim_iccid/current_sim_slot` 变化时，会清空当前 QoS 缓存并重读。
- `clients.list` 上限 32 条；消费端可自行截断显示。NFC 切换用 `ubus call zwrt_nfc zwrt_nfc_wifi_set '{"switch":0|1,"flag":2}'`。
- `sms.unread` 每轮刷新；`sms.list` 每 10 轮重读一次，或在未读数变化时立即重读，最多 32 条。发送、删除和标记已读通过私有 [`CONTROL_API.md`](CONTROL_API.md) 执行。
