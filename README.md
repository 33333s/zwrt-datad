# zwrt-datad

`zwrt-datad` 是一个面向 ZTE 便携式 5G 路由设备的设备数据与控制服务。它会统一轮询 `ubus`、按需扫描 `key.log`，把结果归一化成稳定 JSON，并通过轻量 HTTP/SSE 与上层 UFI 服务交换数据。

当前传输层使用 `HTTP + SSE`：

- `GET /state`：返回当前完整 JSON
- `GET /events`：返回 `text/event-stream`，持续推送最新快照
- `GET /healthz`：返回 `ok`
- `GET /capabilities`：返回允许的设备操作
- `GET /ubus`：返回全部 ubus 对象；`?verbose=1` 同时返回方法签名
- `POST /ubus/call`：完整 ubus 调用入口（所有模板默认开放）
- `POST /control`：执行白名单内的设备控制

默认监听地址：

- 本机免鉴权口：`http://127.0.0.1:9460`
- 内网鉴权口：`http://<设备内网IP>:9461`
  - init 脚本默认开启；本机历史接口保持不变
  - 只接受内网、CGNAT、link-local 和 loopback 来源
  - 通过 `POST /auth/login` 或 `POST /auth/exchange` 换取 Bearer Token
  - 支持 `Authorization`、`X-Auth-Token`，以及 SSE 使用的 `?access_token=`
  - 非空 `auth.token` 继续作为兼容静态 Token

> 这是一个 clean-room 实现，只依赖标准 OpenWrt 能力，不链接厂商私有库。内网 Token 只保护 datad 传输层；UUID、OTA、插件、数据库、短信转发和面向用户的前端会话鉴权仍属于上层 UFI。

公开仓库的文件边界见 [`docs/REPO_BOUNDARY.md`](docs/REPO_BOUNDARY.md)；本仓库不包含 modem signaling capture/decode、qmdl/DCI 工具或本地设备工作流记录。

设备侧 API 模板选择已经收口到后端：后端会先识别机型，再选择对应模板和那套设备接口。当前已经把 `MU5250`、`MC8532B`、`MU5252` 和 `MC7523` 四条模板做实，原先混在主路径里的宽松兼容回退不再算正式机型适配。

`2026-06-26` 又补做了一轮和新版 `u60pro-devui` 的实机联调：后端已按 `HTTP + SSE` 方式跑通，`/state` 与 `/events` 均可正常读取，前端也已通过本机 `127.0.0.1:9460` 长连接订阅。

## 当前模板

当前只把后端模板明确分成“已实现”和“待拆分适配”两类：

- 目前适配机型：
  - `MU5250`
  - 匹配机型：`model_name = MU5250`
  - 对应设备：`U60 Pro`
  - `MC8532B`
  - 匹配机型：`model_name = MC8532B`
  - 对应设备：`G5 Pro`
  - `MU5252`
  - 匹配机型：`model_name = MU5252`
  - 对应设备：`TopFlow`
  - `MC7523`
  - 匹配机型：`model_name = MC7523`
  - 对应设备：`G5 Max WiFi`

## 为什么需要它

如果每个 UI、脚本、网页都自己反复执行 `ubus call`，或者自己去扫 `key.log`，设备上的服务和 I/O 会被打得很碎。`zwrt-datad` 把这些读取统一收口：

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
cc -std=c11 -Wall -Wextra -Werror -D_GNU_SOURCE -Iinclude \
  src/json.c src/device_exec.c src/system_ext.c src/control.c src/main.c \
  -o zwrt-datad-test
```

## 运行

手动运行：

```sh
./zwrt-datad -i 1000
```

后台运行时，建议把常规输出交给服务管理器；若必须使用 `nohup`，不要把无上限日志写到 `/tmp`（多数设备的 `/tmp` 是内存文件系统）：

```sh
nohup ./zwrt-datad -i 1000 >/dev/null 2>&1 </dev/null &
```

运行边界和日志建议见 [`docs/RUNTIME.md`](docs/RUNTIME.md)。

单次采样：

```sh
./zwrt-datad --once
```

额外开启内网鉴权口：

```sh
./zwrt-datad -i 1000 --lan-bind 0.0.0.0 --lan-port 9461
```

作为 OpenWRT 常驻服务安装：

```sh
adb shell 'mkdir -p /data/zwrt-datad'
adb push zwrt-datad-aarch64 /data/zwrt-datad/zwrt-datad
adb push scripts/service.sh /data/zwrt-datad/service.sh
adb shell 'chmod 755 /data/zwrt-datad/zwrt-datad /data/zwrt-datad/service.sh &&
           sh /data/zwrt-datad/service.sh start'
```

`service.sh` 会保留本机 `127.0.0.1:9460`，同时开启 `0.0.0.0:9461` 内网鉴权口。若静态 Token 文件不存在，内网口仍可通过动态登录发放临时 Token。PID 与日志分别写在 `/data/zwrt-datad/zwrt-datad.pid` 和 `/data/zwrt-datad/zwrt-datad.log`。

开机自启只修改 `/etc/rc.local`，在原有 `exit 0` 之前加入：

```sh
sh /data/zwrt-datad/service.sh start
```

不要把 datad 启动脚本放进 `/etc/init.d`。除 `/etc/rc.local` 外，datad 的安装和运行文件全部放在 `/data/zwrt-datad`；如果同机还运行 UFI，应让 `rc.local` 先启动 datad，再调用 `/data/ufi-tools/service.sh start`。

## 读取方式

消费者统一走 HTTP / SSE：

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

内网调用方先登录获取 Token：

```sh
curl -s -u admin:your_web_password -X POST http://<设备内网IP>:9461/auth/login
```

随后使用请求头访问状态和控制接口：

```sh
curl -H 'Authorization: Bearer <token>' http://<设备内网IP>:9461/state
curl -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -d '{"action":"band.set_nr_nsa","params":{"bands":"41,78"}}' \
  http://<设备内网IP>:9461/control
```

通用 `/control` 仍只接受明确列入白名单的动作，不提供 Shell 透传。所有设备模板默认提供完整 `/ubus/call`，能力位保存在模板定义中；该入口只使用参数数组执行 `ubus`，但可以调用设备注册的写方法和危险方法，调用方必须自行约束。完整契约见 [`docs/CONTROL_API.md`](docs/CONTROL_API.md) 和 [`docs/API.md`](docs/API.md)。

后端会先根据 `state.device.model_name` 选择设备侧 API 模板，并把结果写进 `state.device.api_template`。如果前端还需要切自己的 UI 模板，优先使用 `state.device.model_name` 或 `state.device.api_template`，不要再用 `market_name` / `alias_name` 做判断。

模板只输出设备实际支持的可选状态块。无电池设备不会输出 `battery`，无 NFC
设备不会输出 `nfc`；不会用 `-1`、`0` 或空对象冒充“不支持”。调用方应根据
块/字段是否存在决定是否显示功能，同时把 `0%`、关闭状态和 `0mA` 视为有效值。

## QoS / 短信说明

QoS 相关有一个容易踩的点：`qci` / `session_ambr` 往往更新得比 `apn_ambr_*` 更频繁，而且最新一条日志不一定同时带齐所有字段。当前实现改成：

- 进程启动时按 `key.log.0`、`key.log` 的顺序全量扫描；当前 `key.log` 的有效候选优先，旧轮转日志仅补缺
- 优先提取带 `access_point=` 或非 IMS `dnn=` 上下文的数据承载 `qci` / `AMBR`
- 忽略 `dnn=ims` / emergency 承载，避免 IMS 的 256/256 覆盖主数据 AMBR
- 裸 `qci = ...` 只在紧跟有效数据承载上下文，或完全没有更可信值时兜底
- 后续只显示缓存，不在每轮快照里反复扫日志
- 收到 `SIGUSR1` 时立即重读
- 检测到 `sim_iccid/current_sim_slot` 变化时清空旧缓存，并在新日志写入后自动补读

运行中的 `zwrt-datad` 支持：

```sh
kill -USR1 $(pidof zwrt-datad)
```

这会立刻触发一次 QoS 日志重读，供 DevUI 的“刷新 AMBR 缓存”按钮复用。

## 文档

- 接口说明：[`docs/API.md`](docs/API.md)
- 控制接口：[`docs/CONTROL_API.md`](docs/CONTROL_API.md)
- 字段契约：[`docs/STATE_SCHEMA.md`](docs/STATE_SCHEMA.md)
- 仓库边界：[`docs/REPO_BOUNDARY.md`](docs/REPO_BOUNDARY.md)
- 机型模板索引：[`docs/models/README.md`](docs/models/README.md)
- MU5250 模板：[`docs/models/MU5250.md`](docs/models/MU5250.md)
- MC8532B 模板：[`docs/models/MC8532B.md`](docs/models/MC8532B.md)
- MU5252 模板：[`docs/models/MU5252.md`](docs/models/MU5252.md)
- MC7523 模板：[`docs/models/MC7523.md`](docs/models/MC7523.md)
- 开发说明：[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)

## 许可

[MIT](LICENSE)
