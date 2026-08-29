# zwrt-datad state schema

服务通过 HTTP / SSE 暴露统一状态快照：

```text
GET  /state
SSE  /events
```

消费者不应直接打 `ubus`，也不应自己扫 `key.log`。

状态块是可选的。设备模板只输出当前设备支持的块；例如无电池的 CPE
不会输出 `battery`，而不是用 `percent:-1`、`temp:0` 伪装成有效数据。
消费者必须用 JSON key 是否存在判断能力，不能用数值真假判断，因为
`battery.percent=0`、`charging=0` 和 `wlan.enabled=0` 都是合法状态。

## Shape

```json
{
  "ts": 1781201029,
  "net": {
    "type": "SA",
    "bars": 5,
    "roaming": "Home",
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
    "lte_supported_bands": "1,3,8,40,41",
    "nr_sa_supported_bands": "1,28,41,78",
    "nr_nsa_supported_bands": "1,28,41,78",
    "wan_status": "ipv4_ipv6_connected",
    "HSR": false
  },
  "wlan": {
    "ssid": "MyWiFi",
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
  "modems": [
    {
      "id": "x75",
      "role": "integrated_5g",
      "transport": "rmnet",
      "subid": 1,
      "ifname": "rmnet_data0",
      "wan_interface": "zte_mwan2",
      "net": { "bars": 5, "roaming": "Home" },
      "sim": {},
      "wwan": {},
      "interfaces": { "ipv4": {}, "ipv6": {} },
      "traffic": {},
      "qos": { "qci": 9, "ambr_dl": "1000.000", "ambr_ul": "200.000", "sampled_at": 0 }
    },
    {
      "id": "v3e1",
      "role": "external_4g",
      "transport": "cdc-ecm",
      "subid": 3,
      "ifname": "V3E1net0",
      "wan_interface": "zte_mwan3",
      "usb": { "path": "1-1", "id": "19d2:0581", "present": true, "carrier": 1 },
      "debug": { "transport": "adb", "serial": "V3E1T12345", "available": true },
      "net": {},
      "sim": {},
      "wwan": {},
      "interfaces": { "ipv4": {}, "ipv6": {} },
      "traffic": {},
      "qos": { "qci": 9, "ambr_dl": "100.000", "ambr_ul": "100.000", "sampled_at": 1787753189 }
    },
    {
      "id": "v3e2",
      "role": "external_4g",
      "transport": "cdc-ecm",
      "subid": 5,
      "ifname": "V3E2net0",
      "wan_interface": "zte_mwan4",
      "usb": { "path": "1-2", "id": "19d2:1716", "present": true, "carrier": 1 },
      "debug": { "transport": "adb", "serial": "V3E2T12345", "available": true },
      "net": {},
      "sim": {},
      "wwan": {},
      "interfaces": { "ipv4": {}, "ipv6": {} },
      "traffic": {},
      "qos": { "qci": 9, "ambr_dl": "150.000", "ambr_ul": "75.000", "sampled_at": 1787753189 }
    }
  ],
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
    "full_ubus": 1,
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
  "thermal": {
    "cpu_celsius": 41,
    "zones": [
      { "name": "battery", "celsius": 30.000 },
      { "name": "cpuss-0", "celsius": 41.250 }
    ],
    "modems": []
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
- `device.api_template = MU5252`
  - 见 [`models/MU5252.md`](models/MU5252.md)
- `device.api_template = MC7523`
  - 见 [`models/MC7523.md`](models/MC7523.md)
- 其他模板
  - 待后续逐个补充

## Notes

- `GET /state` 返回的是完整 JSON 快照。
- `GET /events` 通过 `event: state` 推送完整 JSON；只有内容变化时才推送新快照。
- 后端会优先根据 `device.model_name` 选择 `device.api_template`；设备侧接口选择已经由后端完成，不需要前端再猜设备应该打哪套 `ubus/uci/sysfs`。
- `device.api_template_supported = 1` 表示当前机型已有明确模板；`0` 表示只落到了内部兼容模板，不应视为正式适配完成。
- `battery`、`wlan`、`nfc`、`sms` 等可选块由模板决定是否输出；兼容模板会按实际接口探测结果输出。缺少整个块表示该接口不可用，块存在而字段值为 `0` 表示有效零值。
- 当模板不输出 `battery` 时，`uci_device_info` 内的 `battery_*` 与 `power_adapter` 厂商占位字段也会同步过滤，避免消费者从兼容缓存重新推断出不存在的电池。
- `/state.wlan` 不输出 Wi-Fi 密钥；密码只允许通过显式鉴权的 Wi-Fi 管理接口读取。
- `device.full_ubus = 1` 表示当前模板开放 `POST /ubus/call`；现有模板默认均为 `1`。
- 机型适配应优先使用 `device.model_name`；`device.profile` 是基于它生成的规范化键，便于模板映射。
- `device.market_name` / `device.alias_name` 只适合展示，不应作为模板切换主键，因为同名产品可能对应不同 `model_name` / 基带方案。
- `system.model` / `hostname` 是设备自报字段，消费端不应把示例值当成固定机型常量。
- `traffic.rx_speed/tx_speed` 和会话计数来自 `type:1`；日/月/累计值、限额与清零日由低频设备状态刷新补充。
- `interfaces` 每 5 秒刷新一次，控制成功时强制立即刷新。地址数组沿用 OpenWrt `network.interface.* status` 的对象结构。
- `sim` 每 5 秒刷新一次；检测到 ICCID、卡槽或 SIM 状态变化时同时刷新 QoS 缓存。
- `modems` 是多基带设备的规范化列表。MU5252 固定包含 `x75`、`v3e1`、`v3e2` 三项；外挂基带的活动 `subid` 分别由 `3/4`、`5/6` 加当前基带卡槽计算。MC7523 是单基带设备，和其他非 MU5252 模板一样返回空数组。
- `net.roaming` 与 `modems[*].net.roaming` 保留厂商当前注册状态字符串（例如 `Home`、`Roaming`）；它不是“允许数据漫游”开关。
- `modems[*].debug.available` 表示对应 USB ADB interface 已枚举，不会在每轮状态采样中启动或调用 ADB；调试口只用于联调。
- `runtime.cpu_usage_tenths` 与 `runtime.cpu_cores.*` 单位为百分比的十分之一，例如 `172` 表示 `17.2%`。每核心占用与频率按采样周期实时读取，不缓存计算结果。
- 顶层 `sample_interval_ms` 是当前 datad 全局采样与 SSE 推送周期。可通过 `state.set_interval` 在 `500..5000` 毫秒范围内运行时切换；所有 SSE 客户端共享同一周期。
- `net.lte_supported_bands`、`net.nr_sa_supported_bands`、`net.nr_nsa_supported_bands` 直接来自本轮 `nwinfo_get_netinfo`，消费者必须用它们生成锁频候选，不得合并其他机型的静态频段目录。
- `runtime.cpu_freq_mhz` 单位为 MHz；`thermal_zones.temp_milli` 单位为毫摄氏度；`memory_kb` 单位为 KiB；`storage` 固定统计 `/data` 文件系统，`storage` 和 `throughput` 单位分别为字节和字节/秒。
- `thermal` 是模板规范化后的温度接口，随 `/state` 与 SSE 的 `state` 事件一起发送。`thermal.cpu_celsius` 为模板 CPU 温度；MU5250、MC8532B、MC7523 和 MU5252 的 `thermal.zones` 来自主机 sysfs thermal zones，已过滤无效哨兵值和不可读 zone、按名称排序并转换为摄氏度。新消费者应使用该字段，不再自行解析 `runtime.thermal_zones[].temp_milli`。
- MU5252 的 `thermal.modems` 固定包含 `x75`、`v3e1`、`v3e2`。X75 温度来自主机 thermal ubus；V3E1/V3E2 每 30 秒通过固定 ADB serial 读取外挂系统的 `zte_power/adc2_temp` 并缓存。`available=false` 时 `celsius=null`；其他模板当前返回空 `modems`。
- MU5252 额外输出 `aggregation` 与 `cooling`；其他模板完全省略这两个块。`aggregation.enabled` 仅在 `zwrt_router.network.opms_wan_mode == SMULTIWAN` 时为 `true`，`aggregation.mode` 保留厂商当前模式文本。`aggregation.provisioned` 只表示厂商是否已下发 ICG 设备配置，不输出实际 ICG ID；`state` 为 `disabled` / `unprovisioned` / `waiting` / `online`，`online` 只在 `zte_icg_agg` 进程确实持有出站 `ESTABLISHED` TCP socket 时为真。datad 会从该进程的 `/proc/<pid>/fd` 反查 socket inode，排除其本地监听端口和入站管理连接，所以不依赖可能被云端运行时下发覆盖的 `/home/icg/icg.conf`；有隧道时 `server.source=runtime` 并显示占多数的实际远端，否则回退静态配置且 `source=config`。`controller.icg_process_running` / `mwan3_running` 分别标示两个控制层是否运行。`paths[]` 来自 `mwan3 status`，同时用 `network.interface.* status` 补充底层 `interface_up/available/pending`；按 X75/V3E1/V3E2（以及实际启用时的 Ethernet）分别输出接口、跟踪/在线状态、探测延迟、丢包、时长和探测目标。路径摘要优先采用状态为 `up` 或 `online` 的实际检测目标，忽略 `reliability=1` 产生的 `skipped` 备用目标；没有在线目标时回退第一个非 `skipped` 样本。目标项中的 `up` 与 `online` 均归一化为 `online=true`。mwan3 运行时路径 `online` 表示探测在线；ICG 模式下 mwan3 不运行，`online` 改为表示底层接口已连接，并继续用 `pending/available` 区分待拨号与可用。它描述承载链路状态，不等同于 ICG 内部 TCP 隧道；mwan3 未运行时不会伪造延迟或丢包。`traffic.remaining_bytes` / `today_used_bytes` 来自厂商落盘 UCI 字节值，`remaining_raw` / `today_used_raw` 继续保留同一原始文本以兼容旧消费者。云端接口以短超时每 60 秒低频触发刷新；接口超时或未返回有效字段时继续使用 UCI 真值，不能阻塞高频主采样。
- MU5252 还输出结构化 `multiwan`：`mode`、`active`、`service_running` 和 `sections[]`。`sections[]` 只包含 mwan3 的安全配置字段，并按 `globals` / `interface` / `member` / `policy` / `rule` 标明类型；list 选项输出为数组。不会输出原始 UCI 文本。固件只在 `mode=MULTIWAN` 时运行 mwan3；`SMULTIWAN` 是 ICG 模式，此时配置可编辑保存但不生效，延迟/丢包为空属于正常状态。
- `cooling.fan.mode` 为 `automatic` / `custom` / `always_on`。`automatic` 启用 `sys-therm-4` 原厂三档内核曲线；`custom` 禁用该 zone，并由 datad 每秒对保存的 2–8 点曲线线性插值；`always_on` 固定 PWM 128。`cooling.fan.kernel_zone_enabled` 给出 zone 的实际状态，`always_on` 表示常开，`enabled` 是与它同值的兼容别名。`cooling.fan` 还输出当前温度、实际 PWM/百分比、80℃ 满速保护、可选 RPM 和固定 cooling level；`manual_speed_percent` 只用于读取旧配置，不再提供手动调速 action。`cooling.factory_curve` 始终返回原厂 44/48/53℃ 三档，`cooling.custom_curve` 单独返回已保存的自定义曲线；`cooling.curve` 是兼容字段，优先返回自定义曲线，没有时回退原厂曲线。三种模式均保持风扇 `thermal_enable` 开启，用户态模式还会清零 `pwm-fan` 的锁存 state，使 PWM 0 能真正停转且非零值可驱动风扇。`cooling.liquid.mode` 为 `automatic` / `low` / `high`：自动模式交还厂商 thermal；低/高档分别使用设备树实际支持的幅度 60/200，`speed_percent` 只对应这两个硬件档位。`always_on`/`enabled` 在低档或高档时为真。消费者应根据整个块是否存在决定是否显示，不得给其他机型补默认值。
- `runtime.throughput` 优先使用 `br-lan`，回退到 WiFi 接口，最后才使用 rmnet，并采用最多 16 个样本的滚动窗口平滑 IPA 批量刷新。
- `qos.ambr_*` 为 Mbps 字符串，保留 3 位小数；空串表示当前还没从日志里读到有效值。
- `net.nrca` / `net.lteca`：载波聚合描述符，`;` 分隔载波、`,` 分隔字段，每个载波 11 个字段 `idx,PCI,?,band,arfcn,bw,?,rsrp,rsrq,sinr,rssi`。没有载波聚合时为空串。
- `net.HSR`：高铁专网确认结果。该值只应来自信令确认，不按 ARFCN/EARFCN 直接判定。当前公开版尚未实现 modem SIB1 信令确认链路，因此该字段保持 `false`，直到公开实现具备同等确认能力。
- WiFi 段的键名用 `wlan` 而不是 `wifi`，避免消费端按子串查找时先命中 `clients.wifi` 计数。`wlan.key` 为明文密码，消费端应自行决定是否打码显示。
- `qos.usb_mode`：`debug` 表示 ADB 开启，`user` 表示关闭；切换可调用 `ubus call zwrt_bsp.usb set '{"mode":"user|debug"}'`。
- `qos.qci` / `qos.ambr_*`：来自 `key.log.0` / `key.log` 的 PDU/EPS 建立日志（偶发行）。后端启动时按轮转旧日志到当前日志的顺序扫描，并按日志上下文缓存多个候选；当前 `key.log` 的有效候选优先于 `key.log.0`，后者只补充当前日志缺失的字段；同一日志内再优先采用当前 `net.mcc/net.mnc` 匹配的 `access_point=*.mncXXX.mccYYY.*` 承载。`dnn=ims` / emergency 这类信令承载不会覆盖主数据 AMBR；无 PLMN 的非 IMS `dnn=` 可作为主数据候选。裸 `qci = ...` 只有在紧跟有效数据承载上下文，或完全没有更可信 QCI 时才作为兜底；收到 `SIGUSR1` 或检测到 `sim_iccid/current_sim_slot` 变化时，会清空当前 QoS 缓存并重读。
- `clients.list` 上限 32 条；消费端可自行截断显示。NFC 切换用 `ubus call zwrt_nfc zwrt_nfc_wifi_set '{"switch":0|1,"flag":2}'`。
- `sms.unread` 默认每 5 秒读取一次；`sms.list` 启动时按 NV/SIM 各最多 32 条完整同步，之后未读数变化时各取最新 8 条并按消息 ID 合并。发送、删除和标记已读成功后会使缓存失效并在下一轮完整同步。TopFlow 厂商密文由 datad 使用设备 OpenSSL 3 解密，输出到该字段的号码和正文均为 UTF-8 明文；消费者不应再实现厂商会话密钥或解密逻辑。相关写操作通过私有 [`CONTROL_API.md`](CONTROL_API.md) 执行。
