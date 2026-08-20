# zwrt-datad 开发说明

这份文档描述当前实现。datad 是 UFI 下方的设备数据与控制层，不承载 UUID、OTA、插件、数据库或前端业务。

## 项目目标

`zwrt-datad` 是一个 clean-room 设备数据与控制服务，目标是在不依赖厂商私有库的前提下，把机型差异和设备操作收口到模板层，对上层 UFI 输出稳定状态和白名单控制接口。

核心原则：

- 单一生产者轮询 `ubus`
- 需要日志补充的字段也由同一个生产者统一读取
- 消费者只走统一后端接口
- 不把 `ubus` 压力和日志扫描扩散到每个 UI、网页或脚本
- 所有写操作使用编译期白名单，不提供任意 Shell 或任意 `ubus` 透传

## 机型范围

当前先按“模板是否已实现”理解：

- 已实现模板：
  - `MU5250`
  - 匹配 `model_name = MU5250`
  - `MC8532B`
  - 匹配 `model_name = MC8532B`
- 待后续拆分适配：
  - 其他机型

因此，字段模型仍然是统一对外契约，但设备内部取数路径现在明确按模板分开维护，不再把多机型回退默认混在一条主路径里。

## 当前架构

```text
device sources -> template select -> state/runtime caches -> /state + /events
                                              control allow-list -> /control
```

当前实现要点：

- 通过 `fork/exec` 参数数组调用 `ubus/uci`，不直接链接 `libubus`，控制参数不经过 Shell
- 启动时对 `key.log` 做一次全量扫描，提取 QoS 缓存
- 平时只输出缓存；手动刷新或换卡时再重读日志
- 默认 1 Hz 生成一次完整快照
- `GET /state` 提供当前完整 JSON
- `GET /events` 提供持续 SSE 推送
- `GET /capabilities` 提供协议版本和动作白名单
- `POST /control` 提供设备控制；成功后立即重采样
- `--auth-token-file` 为内部数据和控制接口提供 Token 鉴权
- 每核心 CPU 与频率按周期实时读取，吞吐使用 16 样本滚动窗口
- 新增 `device.*` 机型识别层，适配优先看 `device.model_name`
- 后端根据 `device.model_name` 选择 `device.api_template`
- 当前已把 `MU5250` / `MC8532B` 模板作为正式适配路径
- 原来为其他机型加的宽松回退收进兼容模板，不再算正式支持

## 传输层约定

当前内部接口：

- `GET /state`
- `GET /events`
- `GET /healthz`
- `GET /capabilities`
- `POST /control`

默认监听：

- 地址：`127.0.0.1`
- 端口：`9460`

当前 SSE 策略：

- 新连接建立后立即推送一份当前快照
- 后续只有在快照内容变化时才再次推送
- `ts` 不单独作为“变化”依据，避免无意义高频推送

## 实机联调状态

`2026-06-26` 已完成一轮真实设备联调，验证组合为：

- `/data/plugins/zwrt-datad/zwrt-datad`：`HTTP + SSE` 版本
- `/data/plugins/u60pro-devui/u60pro-devui`：改为消费 `/state + /events` 的新版前端

设备侧确认点：

- `127.0.0.1:9460` 正常监听
- `GET /state` 正常返回完整 JSON
- `GET /events` 正常返回 `retry: 1000` 和连续的 `event: state`
- `u60pro-devui` 与 `zwrt-datad` 在本机建立了稳定 SSE 长连接

## 启动链路

当前实现依然沿用设备上现有的启动习惯，不主动改变 bring-up 方式。设备自启方案应作为单独变更验证。

如果以后要把它投入实际设备验证，建议继续沿用现有稳定拉起路径，再单独验证 HTTP/SSE 消费端。

## 模板文档

设备侧数据源不再统一写在一张总表里，而是按模板拆开维护：

- 模板索引：[`models/README.md`](models/README.md)
- 当前已实现：[`models/MU5250.md`](models/MU5250.md)
- 当前已实现：[`models/MC8532B.md`](models/MC8532B.md)

## 已知约定

- WebUI / 脚本不应再各自直接打 `ubus`
- 轮询频率不宜过高，默认 `1000ms` 是当前平衡点
- `system board` / `common info` / `imei` 变化很少，按小时刷新缓存即可
- `qos` 日志缓存策略仍然是：启动按 `key.log.0` -> `key.log` 全量扫一次，之后只显示缓存；`key.log` 是当前轮转文件，具有高于 `key.log.0` 的候选优先级，旧日志仅在当前日志缺少对应字段时补缺；同一文件内再优先采用当前 `rmcc/rmnc` 匹配的 APN 承载。非 IMS `dnn=` 可作为数据承载候选，IMS / emergency 不覆盖主数据 AMBR。
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

完整严格检查：

```sh
cc -std=c11 -Wall -Wextra -Werror -D_GNU_SOURCE -Iinclude \
  src/json.c src/device_exec.c src/system_ext.c src/control.c src/main.c \
  -o zwrt-datad-test
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

本仓库当前只处理三件事：

1. 保留统一字段模型，并把设备侧差异收口到模板层
2. 通过 `HTTP + SSE` 向上层输出状态
3. 通过受 Token 保护的白名单接口执行设备控制

机型适配继续按“先把已确认模板做实，再逐个补充”的方式推进，不在主路径里堆隐式兼容分支。UUID、OTA、插件、数据库、调度任务和前端鉴权继续留在 UFI。
