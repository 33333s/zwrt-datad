# zwrt-datad

`zwrt-datad` 是面向中兴便携式 5G 路由设备的数据与控制服务。它统一采集
`ubus`、`uci`、`sysfs` 和必要的设备日志，输出稳定的 JSON 状态，并通过
HTTP/SSE 向 UFI 等本机应用提供数据与受控操作。

## 功能

- `GET /state`：读取完整状态快照
- `GET /events`：通过 SSE 订阅状态变化
- `GET /healthz`：健康检查
- `GET /capabilities`：读取当前机型支持的操作
- `POST /control`：执行白名单控制动作
- `GET /ubus`、`POST /ubus/call`：访问当前设备注册的 ubus 接口
- `/ota/*`：检查和安装经过 Ed25519 与 SHA-256 校验的 datad 更新

目前包含以下正式设备模板：

| 型号 | 产品 |
| --- | --- |
| `MU5250` | U60 Pro |
| `MC8532B` | G5 Pro |
| `MU5252` | TopFlow |
| `MC7523` | G5 Max WiFi |

模板只输出设备实际支持的状态块。调用方应通过字段是否存在判断能力，不应给
不支持的功能补 `0`、`-1` 或空对象。

## 安装

在 ARM64 设备上以 root 执行：

```sh
curl -4fL --retry 3 \
  'https://github.com/33333s/zwrt-datad/releases/latest/download/install-datad.sh' \
  -o /tmp/install-datad.sh
sh /tmp/install-datad.sh
```

安装目录固定为 `/data/zwrt-datad`。服务由
`/data/zwrt-datad/service.sh` 管理，并从 `/etc/rc.local` 启动；不会安装
datad 自己的 `/etc/init.d` 脚本。

```sh
sh /data/zwrt-datad/service.sh start
sh /data/zwrt-datad/service.sh status
sh /data/zwrt-datad/service.sh restart
```

更完整的运行和日志说明见 [`docs/RUNTIME.md`](docs/RUNTIME.md)。

## 访问

默认监听：

- `127.0.0.1:9460`：本机接口
- `<设备内网 IP>:9461`：需要登录后取得 Bearer Token 的内网接口

```sh
curl -fsS http://127.0.0.1:9460/healthz
curl -fsS http://127.0.0.1:9460/state
curl -N http://127.0.0.1:9460/events
```

内网调用方可通过 `POST /auth/login` 或 `POST /auth/exchange` 获取 Token。
详细路由和鉴权方式见 [`docs/API.md`](docs/API.md)。

> `POST /ubus/call` 可以调用设备注册的写方法，包括可能导致断网、重启或配置
> 变化的方法。面向用户的应用必须自行限制入口并进行必要确认。

## 构建与测试

需要 POSIX shell 和 aarch64 musl 工具链：

```sh
bash scripts/build.sh
```

GitHub Actions 会执行完整检查。本地开发和测试入口见仓库内的 `tests/`。

## 文档

- [`docs/API.md`](docs/API.md)：HTTP/SSE 接口
- [`docs/CONTROL_API.md`](docs/CONTROL_API.md)：控制动作
- [`docs/STATE_SCHEMA.md`](docs/STATE_SCHEMA.md)：状态字段契约
- [`docs/models/`](docs/models/)：设备模板与字段来源
- [`docs/CLOUD.md`](docs/CLOUD.md)：可选 NMS 云端连接
- [`docs/NEIGHBOR.md`](docs/NEIGHBOR.md)：可选邻区采集
- [`docs/RUNTIME.md`](docs/RUNTIME.md)：运行、日志与服务管理

## 许可

项目使用 [MIT License](LICENSE)。静态发布中包含的 OpenSSL 许可见
[`OPENSSL-LICENSE.txt`](OPENSSL-LICENSE.txt)。
