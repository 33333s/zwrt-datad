# zwrt-datad 开发说明

这份文档描述 `dev` 分支的当前实现。和稳定发布线相比，`dev` 的主要变化是把传输层彻底切成了 `HTTP + SSE`，方便后续 WebUI 直接接入。

## 项目目标

`zwrt-datad` 是一个 clean-room 设备状态聚合器，目标是在不依赖厂商私有库的前提下，把机型差异收口到后端模板层，对外输出成一份稳定 JSON。

核心原则：

- 单一生产者轮询 `ubus`
- 需要日志补充的字段也由同一个生产者统一读取
- 消费者只走统一后端接口
- 不把 `ubus` 压力和日志扫描扩散到每个 UI、网页或脚本

## 机型范围

当前先按“模板是否已实现”理解：

- 已实现模板：
  - `MU5250`
  - 匹配 `model_name = MU5250`
- 待后续拆分适配：
  - `G5Pro` 和其他机型

因此，字段模型仍然是统一对外契约，但设备内部取数路径现在明确按模板分开维护，不再把多机型回退默认混在一条主路径里。

## 当前架构

```text
common caches + model_name detect -> template select -> template-specific sources -> HTTP /state + SSE /events
```

当前实现要点：

- 通过命令行调用 `ubus`，不直接链接 `libubus`
- 用 `popen()` 读取 `ubus` 输出
- 启动时对 `key.log` 做一次全量扫描，提取 QoS 缓存
- 平时只输出缓存；手动刷新或换卡时再重读日志
- 默认 1 Hz 生成一次完整快照
- `GET /state` 提供当前完整 JSON
- `GET /events` 提供持续 SSE 推送
- 新增 `device.*` 机型识别层，适配优先看 `device.model_name`
- 后端根据 `device.model_name` 选择 `device.api_template`
- 当前只把 `MU5250` 模板作为正式适配路径
- 原来为其他机型加的宽松回退收进兼容模板，不再算正式支持

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

## 实机联调状态

`2026-06-26` 已完成一轮真实设备联调，验证组合为：

- `/data/plugins/zwrt-datad/zwrt-datad`：当前 `dev` 线的 `HTTP + SSE` 版本
- `/data/plugins/u60pro-devui/u60pro-devui`：改为消费 `/state + /events` 的新版前端

设备侧确认点：

- `127.0.0.1:9460` 正常监听
- `GET /state` 正常返回完整 JSON
- `GET /events` 正常返回 `retry: 1000` 和连续的 `event: state`
- `u60pro-devui` 与 `zwrt-datad` 在本机建立了稳定 SSE 长连接

## 启动链路

当前 `dev` 依然沿用设备上现有的启动习惯，不主动改变 bring-up 方式。也就是说，这个分支只是先替换“后端对外通信方式”，不顺手重构设备自启方案。

如果以后要把它投入实际设备验证，建议继续沿用现有稳定拉起路径，再单独验证 HTTP/SSE 消费端。

## 模板文档

设备侧数据源不再统一写在一张总表里，而是按模板拆开维护：

- 模板索引：[`models/README.md`](models/README.md)
- 当前已实现：[`models/MU5250.md`](models/MU5250.md)

## 已知约定

- WebUI / 脚本不应再各自直接打 `ubus`
- 轮询频率不宜过高，默认 `1000ms` 是当前平衡点
- `system board` / `common info` / `imei` 变化很少，按小时刷新缓存即可
- `qos` 日志缓存策略仍然是：启动全量扫一次，之后只显示缓存
- 手动刷新通过 `SIGUSR1` 触发
- 换卡检测基于 `sim_iccid/current_sim_slot`
- 短信列表一次最多取 32 条，并缓存解码后的列表
- `device.api_template_supported = 1` 才代表当前机型已有明确模板；`0` 只表示落到了内部兼容模板

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
./zwrt-datad -i 1000
```

单次采样：

```sh
./zwrt-datad --once
```

改监听地址 / 端口：

```sh
./zwrt-datad -b 0.0.0.0 -p 9460
```

## 调试

直接看一份当前快照：

```sh
./zwrt-datad --once
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
kill -USR1 $(pidof zwrt-datad)
```

## 代码边界

这个 `dev` 分支当前只处理两件事：

1. 保留统一字段模型，并把设备侧差异收口到模板层
2. 把外部通信方式改成 `HTTP + SSE`

也就是说，这个分支现在是“先把 U60 模板做实，再逐个补别的机型模板”，而不是继续在主路径里堆隐式兼容分支。
