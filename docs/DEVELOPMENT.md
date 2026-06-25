# u60-datad 开发说明

这份文档记录 `u60-datad` 当前最重要的实现约束，方便后续维护、移植和继续扩展。

## 项目目标

`u60-datad` 是一个 clean-room 设备状态聚合器，目标是在不依赖厂商私有库的前提下，把 U60Pro 这类设备上的状态统一采集起来，输出成一份稳定的 JSON 快照。

核心原则：

- 单一生产者轮询 `ubus`
- 需要日志补充的字段也由同一个生产者统一读取
- 所有消费者只读缓存文件
- 不把 `ubus` 压力和日志扫描扩散到每一个 UI、网页或脚本

## 当前状态

截至 2026-06-19，后端已经接入这些类别：

- 网络与信号
- 电池与充电
- 客户端数
- 实时流量
- `qos.qci` / `qos.ambr_*`
- 短信未读数与最近短信列表
- 系统信息

## 架构

职责很简单：

```text
ubus calls + key.log full-scan-on-demand cache -> u60-datad -> /tmp/u60-datad/state.json -> consumers
```

当前实现要点：

- 通过命令行调用 `ubus`，不直接链接 `libubus`
- 用 `popen()` 读取 `ubus` 输出
- 启动时对 `key.log` 做一次全量扫描，提取 QoS 缓存
- 平时只输出缓存；手动刷新或换卡时再重读日志
- 默认 1 Hz 生成一次完整快照
- 快照写入 tmpfs，并通过 `rename()` 原子替换

## 启动链路约束

截至 **2026-06-25**，`u60-datad` 在这台 U60Pro 上的**稳定部署方式**仍然不是独立 `procd` 自启，而是由 `u60pro-devui` 的 `/data/u60pro/start.sh` 在 `rc.local` 阶段拉起：

```text
procd -> zte_topsw_devui -> rc.local -> /data/u60pro/start.sh -> u60-datad
```

原因不是 `u60-datad` 自己有问题，而是整条“完全禁用原厂 `zte_topsw_devui`、改成我们自己的 `procd` 自启”链路在这版固件上**不稳定**：

- 干净重启后，`u60pro-devui` / `u60-datad` 都出现过 `inactive`
- 原厂早期服务被禁用时，触摸节点 `event3` 有概率不出现
- 同时还观察到过 `u60pro-devui` 的 `drm: SETCRTC failed: Permission denied`

所以当前约束是：

- `u60-datad` 要容忍**稍晚一点**才启动
- 启动入口以 `/data/u60pro/start.sh` 为准，不要默认再额外启一份独立自启
- 若以后再尝试把 `u60-datad` 改回独立 `procd` 服务，必须和整条屏幕/触摸 bring-up 一起重新实机验证

## 数据模型

真正的字段定义见 [STATE_SCHEMA.md](STATE_SCHEMA.md)。

这里强调几个约束：

- `state.json` 是后端和所有消费者之间的唯一公共接口
- 任何字段的新增、删除、改名，都必须同步更新 schema
- 优先在后端统一做字段映射，不要让消费者分别适配 `ubus` 或日志格式

## 已接入数据源

`ubus`：

- `zte_nwinfo_api nwinfo_get_netinfo`
- `zwrt_bsp.battery list`
- `zwrt_bsp.charger list`
- `zwrt_bsp.thermal get_cpu_temp`
- `zwrt_router.api router_get_user_list_num`
- `zwrt_router.api router_get_status_no_auth`
- `zwrt_data get_wwandst`
- `system info`
- `system board`

日志：

- `/data/logfs/key.log` 的 QoS 相关 `[DATA]` 行
  - `qci`
  - `apn_ambr_dl`
  - `apn_ambr_dl_ext`
  - `apn_ambr_dl_ext2`
  - `apn_ambr_ul`
  - `apn_ambr_ul_ext`
  - `apn_ambr_ul_ext2`

## 已知约定

- UI 不应频繁直接调用后端做单项查询，而应直接读取快照文件
- `state.json` 放在 tmpfs 下，读写都应保持轻量
- 轮询频率不宜过高，默认 `1000ms` 是当前验证过的平衡点
- 当前稳定启动路径里，`u60-datad` 可能由 `start.sh` 重复尝试拉起，因此保持“单进程幂等”很重要；启动脚本里已经有 `pidof u60-datad` guard，后续不要再额外引入第二条自启链路。
- `system board` 变化很少，按小时刷新缓存即可
- `qos` 日志缓存的策略是：**启动全量扫一次，之后只显示缓存**；不要每轮快照都去重扫 `key.log`
- `qci` 与 `apn_ambr_*` 不能只依赖“最后一条日志”：`AMBR` 日志通常比 `qci` 稀疏，实机上很容易出现最新一条 `qci` 还在更新、但最新 `AMBR` 已经是更早的一行。更稳的做法是全量逐行提取最近一次有效值并缓存。
- 手动刷新通过 `SIGUSR1` 触发；DevUI 的“刷新 AMBR 缓存”按钮复用的就是这条通路。
- 换卡检测当前基于 `zwrt_zte_mdm.api get_sim_info` 的 `sim_iccid/current_sim_slot`。签名变化后会清空旧 QoS 缓存，并在新日志写入后自动补读。
- 短信列表一次最多取 32 条，并缓存解码后的列表；状态快照缓冲需要足够容纳这部分 JSON，避免长短信或多条短信时被截断。

## 发布与版本约定

- U60Pro 的 datad 发布线固定走 GitHub 仓库 `33333s/zwrt-datad` 的 `u60pro` 分支。
- 每个 release 只挂两份固定资产：`u60-datad-aarch64` 与 `version.json`。
- `version.json` 是设备侧管理插件真正读取的版本源，示例：

```jsonc
{ "schema": 1,
  "datad": { "version": "0.4.3", "asset": "u60-datad-aarch64" } }
```

- 发版时不要只打 tag；要把新的 `version.json` 和同版本二进制一起上传到 GitHub release。

## 后续方向

仍可继续补的字段包括：

- 月流量 / 总流量
- SIM / IMEI 相关信息
- 更多 WiFi 细节

扩展时继续保持“单一聚合器 + 文件快照”模式即可。

## 2026-06-25 设备 Smoke Test 补记（短信状态链路）

配合 UI 侧设备验证，本轮把 `u60-datad` 测试版临时推到设备 `/tmp/u60-datad.zigtest` 做运行态确认，没有覆盖正式 `/data/u60pro/u60-datad`。

- 启动方式：`/tmp/u60-datad.zigtest -i 1000`
- 运行结果：进程可正常拉起，采样时 RSS 约 `116 KB`，VSZ 约 `6596 KB`
- 状态文件：设备侧在 `/tmp/u60-datad/state.json` 观察到新鲜输出，并命中 `sms` 相关字段
- 结论：
  - “短信是否进入状态文件”这条链路在设备上是通的
  - 这次没有通过 UI 画面人工逐条点读短信，因此更准确地说，是先确认了 backend 产出与 UI 进程联动都已在设备上跑通
- 清理 / 恢复：
  - 测试完成后已删除 `/tmp/u60-datad.zigtest` 与临时日志
  - 正式 `/data/u60pro/u60-datad -i 1000` 已恢复运行

## 2026-06-25 短信列表出现大量空项（号码/正文为空）的根因

- 设备侧原始短信接口 `ubus call zwrt_wms zte_libwms_get_sms_data ...` 当前返回的字段名是：
  - `number`
  - `content`
  - `date`
  - `id`
  - `tag`
- 但 `parse_sms_list()` 还在按旧字段名读取：
  - `num`
  - `text`
- 这就导致现象很典型：
  - `id` / `date` 能正常显示
  - `num` / `text` 全部变成空串
  - 前端短信页于是出现“一大堆日期正常、但号码和正文都空白”的短信项
- 修正策略：
  - 后端兼容两套字段名，优先读旧名 `num` / `text`
  - 若旧名不存在，则回退到设备当前实际返回的 `number` / `content`
- 现场附带还观察到设备上有两条 `u60-datad` 同时在跑；这不会制造“字段名错位”本身，但会放大排查噪音。后续部署修复版时，应顺手确认设备上只保留一条 `u60-datad` 进程，避免旧进程继续抢写 `/tmp/u60-datad/state.json`。
- 已处理：
  - 已在本地重新编译修复后的 `u60-datad`
  - 已替换设备上的 `/data/u60pro/u60-datad`
  - 已运行 `install-autostart.sh` 清理旧的重复自启链路
  - 之后再次手动执行 `sh /data/u60pro/start.sh legacy`，把设备恢复到单 `u60-datad` + 单 `u60pro-devui` 的稳定运行态

## 2026-06-25 插件更新后复查：设备仍在跑旧 datad

- 复查时设备进程状态本身正常：
  - `u60-datad` 只剩一份
  - `u60pro-devui` 也在跑
  - `/tmp/u60-datad/state.json` 在持续刷新
- 但短信仍然是老问题：`id/date` 正常，`num/text` 全空。
- 进一步核对二进制 hash：
  - GitHub `v0.4.2` release 资产 `u60-datad-aarch64` 的 sha256 是 `fe978914ddeb97e25ccb22c04dc03ec4adaea9a155f9a4ee1635feed2774b563`
  - 设备 `/data/u60pro/u60-datad` 的 sha256 是 `354050596062d500fb25f575f1d65fd0c2875db6ba39249928c820f6e8ad346d`
- 结论：
  - 插件这次并没有把设备实际运行的 datad 更新到 `v0.4.2`
  - 设备当前跑的仍是旧版二进制，因此短信字段兼容修复没有生效

## 2026-06-25 用户样本 `key.log.txt`：AMBR 读不到的根因

- 用户提供的日志样本是 `/Users/y/Downloads/key.log.txt`。
- 这份样本里：
  - `apn_ambr_*` 只有 `2` 条，而且都停在 `2026-06-18 04:04:12` 之前
  - `session_ambr_*` 有 `76` 条，并且最近一条一直更新到 `2026-06-25 19:32:29`
- 当前样本的主流格式已经不是旧的：
  - `apn_ambr_dl=... apn_ambr_ul=... apn_ambr_dl_ext=...`
  - 而是新的：
    - `session_ambr_dl=3000`
    - `session_ambr_dl_unit=6(1Mbps)`
    - `session_ambr_ul=200`
    - `session_ambr_ul_unit=6(1Mbps)`
- 同时 `qci` 仍然单独存在，例如 `qci = 6 6`。
- 结论：
  - 如果后端 AMBR parser 只认 `apn_ambr_*`，这个用户就很容易表现成“QCI 能读到，但 AMBR 读不到”
  - 因为对他来说，真正持续更新的字段已经切到 `session_ambr_*`
- 后续修正方向：
  - 保留旧 `apn_ambr_*` 解析不动
  - 增加 `session_ambr_dl/session_ambr_ul + *_unit` 的兼容解析作为 fallback
  - 最稳的做法是优先取**最近一次有效**的 `session_ambr_*`；若完全没有，再退回旧 `apn_ambr_*`

## 2026-06-25 AMBR 双格式兼容补记

- 线上样本 `key.log.txt` 证明 AMBR 日志现在存在两套格式并存：
  - 旧格式：`apn_ambr_dl/apn_ambr_ul/apn_ambr_*_ext*`
  - 新格式：`session_ambr_dl/session_ambr_ul + session_ambr_*_unit`
- 当前修正策略已改成双兼容：
  - 扫到 `session_ambr_*` 时，按 `*_unit` 括号里的单位文本换算成 Mbps
  - 扫到 `apn_ambr_*` 时，继续沿用旧解析逻辑
  - 日志顺序扫描，最近一次有效值覆盖更早值
- 这样老用户和新用户都能出 AMBR，不要求固件日志格式一致。
