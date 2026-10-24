# U30PRO（U30 Pro，原生 OpenWrt）

匹配：`system board` 的 `board_name = zte,u30pro`（启动时判定一次；`ZWRT_DATAD_BACKEND=u30pro` 可强制，其它值强制走中兴后端）。
`device.model_name` 取自 `system board` 的型号（`ZTE U30 Pro (MU5358)` → `MU5358`），`device.api_template = U30PRO`。

这台机器跑的是原生 OpenWrt 25.12（[U30pro-openwrt](https://github.com/33333s/U30pro-openwrt)），没有任何中兴原厂对象
（`zwrt_*`、`zte_nwinfo_api` 等）。后端代码在 `rust/src/u30pro.rs`，**只通过 ubus（含 `uci` 对象）读写**：不发 AT、不读厂商文件、不读 sysfs。
基带经 `u30pro-modem` 访问，它的 `snapshot` 方法与设备屏幕共用一份缓存查询，datad 每秒轮询也不增加 AT 流量。
信号与 CPU 占用需要固件 p535-s80 或更新。

## 数据来源

| 块 | 来源 |
|---|---|
| `device` | `system board` |
| `system` | `system info`（内存、运行时间）、`u30pro-cpu get`（`usage.total_tenths`）、`u30pro-native-control get`（SoC 温度）、`u30pro-firmware status`（运行槽位的固件版本 → `sw_version`）；`imei` 固定为空 |
| `runtime` | `u30pro-cpu get`（每核占用与频率）、`system info`（内存/交换）、`u30pro-storage get`（数据分区）、`network.device status br-lan`（吞吐）；无 `connections` |
| `thermal` | `u30pro-native-control get`：SoC、主板各点、电池温度 |
| `battery` / `power.direct_supply` | `u30pro-native-control get` 的 `battery`、`charger`；直供电 = 充电策略 `inhibit-charge` |
| `sim` | `u30pro-modem snapshot` 的 `cellular.sim`（只有状态，不读 ICCID/IMSI/号码） |
| `net` | `u30pro-modem snapshot`（`serving`：CEREG/C5GREG/COPS/CESQ/CSQ/SPPHYCHINFO/SPENDC，最长 10 秒缓存；`sinr_db`：LTE 服务小区 SINR）、`u30pro-modem bands_get`（5 分钟缓存：支持频段、当前频段、锁小区） |
| `traffic` | `network.interface.wan status` + `network.device status <l3_device>` 的计数器；没有日/月/累计统计 |
| `qos` | `u30pro-modem qos`（1 分钟缓存；AMBR 由 kbps 换算成 Mbps） |
| `interfaces` | `network.interface.lan/wan status`；`cellular` 来自 `snapshot.cellular` |
| `clients` | `hostapd.wlan0 get_clients` + `luci-rpc getDHCPLeases`；只有 Wi-Fi 客户端，不做黑白名单 |
| `wlan` | `u30pro-native-control wifi_get`（单频段热点） |
| `dhcp` | `uci get network` / `uci get dhcp` |
| `sms` | `u30pro-modem sms_list`（1 分钟缓存；`id` 是首段存储序号，多段短信删除时一起删） |

`net.type`：NR 注册（SA）为 `SA`，EN-DC 为 `LTE-NSA`（测量值来自 LTE 锚点），其余 `LTE`/`WCDMA`/`GSM`。
`net.band` 写作 `LTE BAND 8` 或 `n78`；`net.signal_age_s` 是信号数据的缓存年龄（秒）。

## 控制

| 动作 | 实现 |
|---|---|
| `device.reboot` | `u30pro-native-control reboot`（PMIC 看门狗重启；PSCI 重启会让 Type-C 和基带状态错乱） |
| `cellular.connect` / `cellular.disconnect` | `u30pro-native-control cellular_action` |
| `band.set_lte` / `band.set_nr_sa` / `band.set_nr_nsa` | `u30pro-modem lock_apply`（SA/NSA 共用一组 NR 频段；空串 = 全部支持频段） |
| `cell.lock_lte` / `cell.lock_nr` / `cell.unlock_all` | `lock_apply`（`cell`）/ `lock_reset` |
| `wifi.configure` / `wifi.status` / `wifi.state` | `u30pro-native-control wifi_apply` / `wifi_get` / `wifi_toggle` |
| `client.kick` | `u30pro-native-control wifi_kick` |
| `sms.send_raw` / `sms.delete` | `u30pro-modem sms_send` / `sms_delete` |
| `power.direct_supply.set` / `.status` | `u30pro-native-control set_charger_policy`（读回确认） |
| `qos.reload` | 让 QoS 缓存失效 |

锁频段/锁小区：固件在 120 秒内没收到确认就自动撤销（`lock_apply` 的安全网）。datad 在下发后 10 秒起每 5 秒查一次注册状态，
重新注册就确认；锁错导致掉网时由固件撤销。

datad 自己的通用动作（hosts、time、schedule、speedtest、nat、state 等）照常可用；中兴专有动作（APN、流量统计、NFC、睡眠、USB、
多 WAN、散热、访客 Wi-Fi、黑白名单等）返回 `unknown_action`。`/ubus/call` 透传照常开放（例如 `u30pro-modem at`）。

## 登录与更新

`POST /auth/login`：用路由器自己的账号，经 rpcd `session login` 校验（与 LuCI 相同）。静态令牌照常可用。
datad 的自更新在这台设备上关闭（`ota.check`/`install` 直接拒绝）：新版本通过设备软件源的 apk 包 `zwrt-datad` 安装。

## 构建

ARM64 release 需要 `DATAD_NO_KEYMASTER=1`（这台机器没有中兴 Keymaster 库，身份接口返回 `identity_backend_unavailable`）：

```sh
DATAD_NO_KEYMASTER=1 cargo zigbuild --manifest-path rust/Cargo.toml --locked --release --target aarch64-unknown-linux-musl --bin zwrt-datad
```

打包见 U30pro-openwrt 的 `tools/build_zwrt_datad.sh` 与 `feed/packages/zwrt-datad`（procd 服务、`/etc/config/zwrt-datad`）。
