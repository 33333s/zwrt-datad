# U30PRO（U30 Pro，原生 OpenWrt）

匹配：`system board` 的 `board_name = zte,u30pro`（启动时判定一次；`ZWRT_DATAD_BACKEND=u30pro` 可强制，其它值强制走中兴后端）。
`device.model_name` 取自 `system board` 的型号（`ZTE U30 Pro (MU5358)` → `MU5358`），`device.api_template = U30PRO`。

这台机器跑的是原生 OpenWrt 25.12（[U30pro-openwrt](https://github.com/33333s/U30pro-openwrt)），没有任何中兴原厂对象
（`zwrt_*`、`zte_nwinfo_api` 等）。后端代码在 `rust/src/u30pro.rs`，**只通过 ubus（含 `uci` 对象）读写**：不发 AT、不读厂商文件、不读 sysfs。
设备守护进程有自己的 ubus 对象的直接调用它（拨号管理 `u30pro.cell`），其余经设备的 rpcd 插件（`u30pro-modem`、`u30pro-native-control` 等）。
基带经 `u30pro-modem` 访问，它的 `snapshot` 方法与设备屏幕共用一份缓存查询，datad 每秒轮询也不增加 AT 流量。
需要固件 p535-s90 或更新（`u30pro.cell`、`charger_bypass`）。

## 数据来源

| 块 | 来源 |
|---|---|
| `device` | `system board` |
| `system` | `system info`（内存、运行时间）、`u30pro-cpu get`（`usage.total_tenths`）、`u30pro-native-control get`（SoC 温度）、`u30pro-firmware status`（运行槽位的固件版本 → `sw_version`）；`imei` 固定为空 |
| `runtime` | `u30pro-cpu get`（每核占用与频率）、`system info`（内存/交换）、`u30pro-storage get`（数据分区）、`network.device status br-lan`（吞吐）；`connections`（本机套接字数）与中兴机型相同；`link_rates` 是两个上行口 `sipa_eth0`（蜂窝）、`wlan1`（Wi-Fi 中继） |
| `thermal` | `u30pro-native-control get`：SoC、主板各点、电池温度；`protection` 取自保护服务 `u30pro.safety` 的判定（`raw` 是 ok/warning/critical，它只暂停充电或关机，`speed_limited`、`network_restricted` 恒为 false；没有中兴的 `level`） |
| `battery` / `power.direct_supply` | `u30pro-native-control get` 的 `battery`、`charger`；`charge_protection.active` = 保护服务暂停了充电（没有中兴的 `mode`）；直供电 = 充电策略 `inhibit-charge`；没有 `charger_type`、`time_to_full` |
| `sim` | `u30pro-modem snapshot` 的 `cellular.sim`（`cellular` 就是 `u30pro.cell status`；只有状态，不读 ICCID/IMSI/号码）；单卡：`current_slot` 1、`dual_sim` 0 |
| `net` | `u30pro-modem snapshot`（`serving`：CEREG/C5GREG/COPS/CESQ/CSQ/SPPHYCHINFO/SPENDC，最长 10 秒缓存；`sinr_db`：LTE 服务小区 SINR）、`u30pro-modem bands_get`（5 分钟缓存：支持频段、当前频段、锁小区） |
| `traffic` | `network.interface.wan status` + `network.device status <l3_device>` 的计数器；没有日/月/累计统计 |
| `qos` | `u30pro-modem qos`（1 分钟缓存；AMBR 由 kbps 换算成 Mbps） |
| `interfaces` | `network.interface.lan/wan status`；`cellular` 用中兴数据服务（`zwrt_data get_wwaniface`）的字段名：`enable`（拨号管理要求连接）、`connect_mode` 固定 `auto`（开机自动拨号）、`connect_status`（`ipv4_ipv6_connected` / `ipv4_connected` / `ipv6_connected` / `disconnected`）、`ipv4_dev_name`/`ipv6_dev_name`、`pdp_type`（`IP` / `IPV6` / `IPV4V6`）、`ipv4_address`/`ipv6_address`，另有 `apn`、`dns`；没有漫游开关和 APN 配置表，所以没有 `roam_enable`、`profile_id` |
| `clients` | `hostapd.wlan0 get_clients` + `luci-rpc getDHCPLeases`；只有 Wi-Fi 客户端，不做黑白名单 |
| `wlan` | `uci get wireless`：标准 `/etc/config/wireless` 里 radio0 和第一个 mode ap 的接口（单频段热点） |
| `dhcp` | `uci get network` / `uci get dhcp` |
| `sms` | `u30pro-modem sms_list`（1 分钟缓存；`id` 是首段存储序号，多段短信删除时一起删）；每条有中兴的 `tag`（收到未读 1、已读 0，发出 2、未发 3），`truncated` 恒为 false（列出全部存储） |
| `usb` | datad 通用的 USB 块（sysfs），与中兴机型相同 |

`net.type`：NR 注册（SA）为 `SA`，EN-DC 为 `LTE-NSA`（测量值来自 LTE 锚点），其余 `LTE`/`WCDMA`/`GSM`。
`net.band` 写作 `LTE BAND 8` 或 `n78`；`net.signal_age_s` 是信号数据的缓存年龄（秒）。

## 控制

| 动作 | 实现 |
|---|---|
| `device.reboot` | `u30pro-native-control reboot`（PMIC 看门狗重启；PSCI 重启会让 Type-C 和基带状态错乱） |
| `cellular.connect` / `cellular.disconnect` | `u30pro.cell reconnect` / `disconnect` |
| `band.set_lte` / `band.set_nr_sa` / `band.set_nr_nsa` | `u30pro-modem lock_apply`（SA/NSA 共用一组 NR 频段；空串 = 全部支持频段） |
| `cell.lock_lte` / `cell.lock_nr` / `cell.unlock_all` | `lock_apply`（`cell`）/ `lock_reset` |
| `wifi.configure` / `wifi.status` / `wifi.state` | `uci set` + `uci commit` wireless（热点那一段：ssid、encryption、key、ieee80211w、maxassoc、hidden、isolate、disabled），再 `network reload` / `uci get wireless` |
| `client.kick` | `hostapd.wlan0 del_client`（deauth，不封禁） |
| `sms.send_raw` / `sms.delete` | `u30pro-modem sms_send` / `sms_delete` |
| `power.direct_supply.set` / `.status` | `u30pro-native-control charger_bypass`（与屏幕上的开关相同：只切换充电器的 `inhibit-charge`，重启后恢复充电；不改 uci 里的充电限流 `u30pro_native_control.charger`），由插件写后读回确认 / `get` |
| `qos.reload` | 让 QoS 缓存失效 |

锁频段/锁小区：固件在 120 秒内没收到确认就自动撤销（`lock_apply` 的安全网）。datad 在下发后 10 秒起每 5 秒查一次注册状态，
重新注册就确认；锁错导致掉网时由固件撤销。

datad 自己的通用动作（hosts、time、schedule、speedtest、nat、state 等）照常可用；中兴专有动作（APN、流量统计、NFC、睡眠、USB、
多 WAN、散热、访客 Wi-Fi、黑白名单等）返回 `unknown_action`。`/ubus/call` 透传照常开放（例如 `u30pro-modem at`）。

## 登录与更新

`POST /auth/login`：用路由器自己的账号，经 rpcd `session login` 校验（与 LuCI 相同）。为中兴固件写的 App 固定用 `admin`，
在这台设备上 `admin` 按 `root` 校验（密码是 root 的，即 LuCI 的密码）。静态令牌照常可用。
datad 的自更新在这台设备上关闭（`ota.check`/`install` 直接拒绝）：新版本通过设备软件源的 apk 包 `zwrt-datad` 安装。

## 构建

ARM64 release 需要 `DATAD_NO_KEYMASTER=1`（这台机器没有中兴 Keymaster 库，身份接口返回 `identity_backend_unavailable`）：

```sh
DATAD_NO_KEYMASTER=1 cargo zigbuild --manifest-path rust/Cargo.toml --locked --release --target aarch64-unknown-linux-musl --bin zwrt-datad
```

打包见 U30pro-openwrt 的 `tools/build_zwrt_datad.sh` 与 `feed/packages/zwrt-datad`（procd 服务、`/etc/config/zwrt-datad`）。
