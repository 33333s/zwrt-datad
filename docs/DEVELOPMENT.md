# u60-datad 开发说明

这份文档记录 `u60-datad` 当前最重要的实现事实，方便后续维护、移植和公开分享。它的定位是把那些容易忘、但又会直接影响开发和部署的经验保存在仓库里，而不是只留在本地记忆中。

## 项目目标

`u60-datad` 是一个 clean-room 的设备状态聚合器，目标是在不依赖厂商私有库的前提下，把 U60Pro 以及类似 SDX 系列 OpenWRT MiFi 设备上的状态统一采集起来，输出成一份稳定的 JSON 快照。

设计原则很简单：

- 单一生产者轮询 `ubus`
- 所有消费者只读缓存文件
- 不把 `ubus` 压力扩散到每一个 UI 或脚本里
- 保持可独立构建、可独立开源

## 当前状态

截至 2026-06-12，`u60-datad` 已在真机上验证可用：

- 可以稳定轮询 `ubus`
- 可以把状态写成 `/tmp/u60-datad/state.json`
- `u60pro-devui` 已经能直接读取这份快照
- 网络、电池、客户端、流量、系统信息都已接入
- `--once` 和 `-i <ms>` 参数可用
- 以 procd 服务方式运行也可用

## 架构

`u60-datad` 的职责是把多个 `ubus` 服务整合成一份平面 JSON：

```text
ubus calls -> u60-datad -> /tmp/u60-datad/state.json -> consumers
```

当前实现是：

- 通过命令行调用 `ubus`，不是直接链接 `libubus`
- 用 `popen` 读取输出
- 定时轮询，默认 1 秒一次
- 每次生成完整快照后，原子性写入 tmpfs 文件

这样做的好处是：

- 静态二进制更容易保持简单
- 不需要把厂商库或额外依赖带进来
- 读侧可以始终按“只读文件”处理

## 数据模型

真正的字段契约见 [docs/STATE_SCHEMA.md](STATE_SCHEMA.md)。这里记录的是实现和维护上的约束：

- `state.json` 是后端和所有消费者之间的唯一公共接口
- 任何字段增删改名，都要同时更新 schema 和读取逻辑
- 快照应该尽量保持扁平、稳定、易解析

已确认可用的 `ubus` 数据源包括：

- `zte_nwinfo_api nwinfo_get_netinfo`
- `zwrt_bsp.battery list`
- `zwrt_bsp.charger list`
- `zwrt_bsp.thermal get_cpu_temp`
- `zwrt_router.api router_get_user_list_num`
- `zwrt_router.api router_get_status_no_auth`
- `zwrt_data get_wwandst`
- `system info`
- `system board`

## 构建

当前已验证的构建方式是不依赖宿主机完整开发环境，直接使用仓库内脚本和 aarch64 musl 工具链：

```sh
bash scripts/build.sh
```

生成的是一个静态的 `aarch64` 二进制。

## 部署与运行

推荐的运行方式是作为 OpenWRT `procd` 服务启动。脚本位于 `scripts/u60-datad.init`，安装后可以通过标准 init 流程管理。

如果手动运行，默认轮询间隔是 1000ms：

```sh
/usr/bin/u60-datad -i 1000
```

调试时也可以只执行一次：

```sh
./u60-datad --once
```

## 已知约定

- `u60-datad` 不直接被 UI 频繁调用，而是由 UI 读取缓存文件
- `state.json` 放在 tmpfs 下，读写都应当轻量
- 轮询频率不宜过高，默认 1 Hz 是当前已验证的平衡点
- 如果要扩展字段，优先在后端统一映射，不要让消费者各自适配不同的 `ubus` 输出

## 后续方向

当前还没完全补齐的字段主要有：

- 月/总流量计数
- 短信未读数
- SIM / IMEI 相关信息
- 更多 WiFi 细节

这些后续都应该继续保持“单一聚合器 + 文件快照”的模式，不回到多消费者各自轮询 `ubus` 的方式。

