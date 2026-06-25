# u60-datad

`u60-datad` 是一个面向 ZTE 便携式 5G 路由设备的状态聚合器。它会统一轮询 `ubus`、按需扫描 `key.log`，把结果归一化成一份稳定 JSON，再通过轻量 HTTP 服务对外提供。

`dev` 分支当前的传输层已经彻底切到 `HTTP + SSE`：

- `GET /state`：返回当前完整 JSON
- `GET /events`：返回 `text/event-stream`，持续推送最新快照
- `GET /healthz`：返回 `ok`

默认监听地址：

- `http://127.0.0.1:9460`
- `http://127.0.0.1:9460/state`
- `http://127.0.0.1:9460/events`

> 这是一个 clean-room 实现，只依赖标准 OpenWRT 能力，不链接厂商私有库。

当前这条 `dev` 线开始把“设备侧 API 模板选择”收口到后端：后端会先识别机型，再选择对应模板和那套设备接口。当前默认先把 `MU5250` 这条模板做实，原先混在主路径里的宽松兼容回退不再算正式机型适配。

`2026-06-26` 又补做了一轮和新版 `u60pro-devui` 的实机联调：正式设备上的 `/data/u60pro/u60-datad` 已切到这条 `HTTP + SSE` 线，`/state` 与 `/events` 均可正常读取，前端也已通过本机 `127.0.0.1:9460` 长连接订阅。

## 当前模板

当前只把后端模板明确分成“已实现”和“待拆分适配”两类：

- 已实现并启用：
  - `MU5250`
  - 匹配机型：`model_name = MU5250`
  - 对应设备：`U60 Pro`
- 待后续单独适配：
  - `G5Pro` 及其他机型
  - 不再继续复用 `U60` 模板冒充“通用支持”

## 为什么需要它

如果每个 UI、脚本、网页都自己反复执行 `ubus call`，或者自己去扫 `key.log`，设备上的服务和 I/O 会被打得很碎。`u60-datad` 把这些读取统一收口：

- `ubus` 只被单个进程按固定频率轮询
- `key.log` 只由单个进程按需读取
- WebUI / 脚本 / 其他本地消费者都只走统一 HTTP 接口
- 传输层统一后，前端不需要再自己处理文件轮询和 mtime 判定

## 构建

需要 POSIX shell 和 aarch64 musl 工具链：

```sh
bash scripts/build.sh
```

主机侧语法检查：

```sh
cc -std=c11 -Wall -Wextra -Werror -Iinclude -c src/json.c src/main.c
```

## 运行

手动运行：

```sh
./u60-datad -i 1000
```

单次采样：

```sh
./u60-datad --once
```

修改监听地址和端口：

```sh
./u60-datad -b 0.0.0.0 -p 9460
```

作为 OpenWRT 服务安装：

```sh
adb push scripts/u60-datad.init /etc/init.d/u60-datad
adb shell 'chmod 755 /etc/init.d/u60-datad &&
           /etc/init.d/u60-datad enable &&
           /etc/init.d/u60-datad start'
```

## 读取方式

当前 `dev` 分支消费者统一走 HTTP / SSE：

```sh
curl http://127.0.0.1:9460/state
```

```sh
curl -N http://127.0.0.1:9460/events
```

浏览器侧最小示例：

```javascript
const es = new EventSource("http://127.0.0.1:9460/events");
es.addEventListener("state", (ev) => {
  const state = JSON.parse(ev.data);
  console.log(state);
});
```

后端会先根据 `state.device.model_name` 选择设备侧 API 模板，并把结果写进 `state.device.api_template`。如果前端还需要切自己的 UI 模板，优先使用 `state.device.model_name` 或 `state.device.api_template`，不要再用 `market_name` / `alias_name` 做判断。

## QoS / 短信说明

QoS 相关有一个容易踩的点：`qci` 往往更新得比 `apn_ambr_*` 更频繁，而且最新一条日志不一定同时带齐所有字段。当前实现改成：

- 进程启动时全量扫描一次 `key.log`
- 逐行提取最近一次有效的 `qci` / `apn_ambr_*`
- 后续只显示缓存，不在每轮快照里反复扫日志
- 收到 `SIGUSR1` 时立即重读
- 检测到 `sim_iccid/current_sim_slot` 变化时清空旧缓存，并在新日志写入后自动补读

运行中的 `u60-datad` 支持：

```sh
kill -USR1 $(pidof u60-datad)
```

这会立刻触发一次 QoS 日志重读，供 DevUI 的“刷新 AMBR 缓存”按钮复用。

## 文档

- 接口说明：[`docs/API.md`](docs/API.md)
- 字段契约：[`docs/STATE_SCHEMA.md`](docs/STATE_SCHEMA.md)
- 机型模板索引：[`docs/models/README.md`](docs/models/README.md)
- MU5250 模板：[`docs/models/MU5250.md`](docs/models/MU5250.md)
- 开发说明：[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)

## 许可

[MIT](LICENSE)
