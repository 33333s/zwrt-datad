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
ubus calls + key.log latest-match parse -> u60-datad -> /tmp/u60-datad/state.json -> consumers
```

当前实现要点：

- 通过命令行调用 `ubus`，不直接链接 `libubus`
- 用 `popen()` 读取 `ubus` 输出
- 对 `key.log` 只读取 QoS 相关的最后匹配行，避免每轮全量扫大文件
- 默认 1 Hz 生成一次完整快照
- 快照写入 tmpfs，并通过 `rename()` 原子替换

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
- `system board` 变化很少，按小时刷新缓存即可
- 日志补充字段应优先走“读取最后匹配行”的方式，不要每轮全量扫 `key.log`
- `qci` 与 `apn_ambr_*` 不能只共用一个简单尾窗：`AMBR` 日志通常比 `qci` 稀疏，实机上很容易出现 `qci` 还在更新、但 `AMBR` 已被其它新日志挤出窗口的情况。更稳的做法是分别取最后一条并缓存最后已知值。
- 短信列表一次最多取 32 条，并缓存解码后的列表；状态快照缓冲需要足够容纳这部分 JSON，避免长短信或多条短信时被截断。

## 后续方向

仍可继续补的字段包括：

- 月流量 / 总流量
- SIM / IMEI 相关信息
- 更多 WiFi 细节

扩展时继续保持“单一聚合器 + 文件快照”模式即可。
