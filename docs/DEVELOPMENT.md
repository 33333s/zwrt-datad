# u60-datad 开发说明

这份文档描述 `dev` 分支的当前实现。和稳定发布线相比，`dev` 的主要变化是把传输层彻底切成了 `HTTP + SSE`，方便后续 WebUI 直接接入。

## 项目目标

`u60-datad` 是一个 clean-room 设备状态聚合器，目标是在不依赖厂商私有库的前提下，把 U60Pro 上的状态统一采集起来，对外输出成一份稳定 JSON。

核心原则：

- 单一生产者轮询 `ubus`
- 需要日志补充的字段也由同一个生产者统一读取
- 消费者只走统一后端接口
- 不把 `ubus` 压力和日志扫描扩散到每个 UI、网页或脚本

## 当前架构

```text
ubus calls + key.log cache -> u60-datad -> HTTP /state + SSE /events -> consumers
```

当前实现要点：

- 通过命令行调用 `ubus`，不直接链接 `libubus`
- 用 `popen()` 读取 `ubus` 输出
- 启动时对 `key.log` 做一次全量扫描，提取 QoS 缓存
- 平时只输出缓存；手动刷新或换卡时再重读日志
- 默认 1 Hz 生成一次完整快照
- `GET /state` 提供当前完整 JSON
- `GET /events` 提供持续 SSE 推送
- 对不同设备的 `thermal` / `wifi` / `client list` 数据源做回退适配

## 传输层约定

当前 `dev` 分支的对外接口：

- `GET /state`
- `GET /events`
- `GET /healthz`

默认监听：

- 地址：`127.0.0.1`
- 端口：`9460`

当前 SSE 策略：

- 新连接建立后立即推送一份当前快照
- 后续只有在快照内容变化时才再次推送
- `ts` 不单独作为“变化”依据，避免无意义高频推送

## 启动链路

当前 `dev` 依然沿用 U60Pro 现有的启动习惯，不主动改变设备上的 bring-up 方式。也就是说，这个分支只是先替换“后端对外通信方式”，不顺手重构设备自启方案。

如果以后要把它投入实际设备验证，建议继续沿用现有稳定拉起路径，再单独验证 HTTP/SSE 消费端。

## 已接入数据源

`ubus`：

- `zte_nwinfo_api nwinfo_get_netinfo`
- `zwrt_bsp.battery list`
- `zwrt_bsp.charger list`
- `zwrt_bsp.thermal get_cpu_temp`
- `zwrt_bsp.thermal list`（作为温度接口回退）
- `zwrt_router.api router_get_user_list_num`
- `zwrt_router.api router_get_status_no_auth`
- `zwrt_router.api router_lan_access_list` / `router_wireless_access_list`（当 DHCP lease 不可用时）
- `zwrt_data get_wwandst`
- `zwrt_zte_mdm.api get_zwrt_common_info`
- `zwrt_zte_mdm.api get_imei`
- `system info`
- `system board`

其他数据源：

- `/data/logfs/key.log` 的 QoS `[DATA]` 行
- `/tmp/dhcp.leases`
- `uci`
- `/proc/stat`
- `thermal_zone` sysfs（当 ubus 不直接给 CPU 温度时）
- `power_supply` sysfs

## 已知约定

- WebUI / 脚本不应再各自直接打 `ubus`
- 轮询频率不宜过高，默认 `1000ms` 是当前平衡点
- `system board` / `common info` / `imei` 变化很少，按小时刷新缓存即可
- `qos` 日志缓存策略仍然是：启动全量扫一次，之后只显示缓存
- 手动刷新通过 `SIGUSR1` 触发
- 换卡检测基于 `sim_iccid/current_sim_slot`
- 短信列表一次最多取 32 条，并缓存解码后的列表
- G5Pro 这类机型上如果 `/tmp/dhcp.leases` 为空，会自动回退到 `router_wireless_access_list`
- 如果 `zwrt_bsp.thermal get_cpu_temp` 不存在，会自动回退到 `thermal_zone` 里的 `cpuss*`

## 构建

交叉编译：

```sh
bash scripts/build.sh
```

主机侧检查：

```sh
cc -std=c11 -Wall -Wextra -Werror -Iinclude -c src/json.c src/main.c
```

## 运行

前台运行：

```sh
./u60-datad -i 1000
```

单次采样：

```sh
./u60-datad --once
```

改监听地址 / 端口：

```sh
./u60-datad -b 0.0.0.0 -p 9460
```

## 调试

直接看一份当前快照：

```sh
./u60-datad --once
```

读 HTTP：

```sh
curl http://127.0.0.1:9460/state
```

读 SSE：

```sh
curl -N http://127.0.0.1:9460/events
```

QoS 缓存重读：

```sh
kill -USR1 $(pidof u60-datad)
```

## 代码边界

这个 `dev` 分支当前只处理两件事：

1. 保留现有 U60Pro 采集逻辑和字段模型
2. 把外部通信方式改成 `HTTP + SSE`

也就是说，这个分支还不是“多设备主线”重构，只是先把传输层收口，给后续 WebUI 或适配层改造打基础。
