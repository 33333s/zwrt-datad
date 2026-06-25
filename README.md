# u60-datad

`u60-datad` 是一个面向 ZTE U60Pro 的设备状态聚合器。它会统一轮询 `ubus`、按需扫描 `key.log`，把结果归一化成一份稳定 JSON，再通过轻量 HTTP 服务对外提供。

`dev` 分支当前的传输层已经彻底切到 `HTTP + SSE`：

- `GET /state`：返回当前完整 JSON
- `GET /events`：返回 `text/event-stream`，持续推送最新快照
- `GET /healthz`：返回 `ok`

默认监听地址：

- `http://127.0.0.1:9460`
- `http://127.0.0.1:9460/state`
- `http://127.0.0.1:9460/events`

> 这是一个 clean-room 实现，只依赖标准 OpenWRT 能力，不链接厂商私有库。

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
- 开发说明：[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)

## 许可

[MIT](LICENSE)
