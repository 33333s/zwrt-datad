# zwrt-datad

面向中兴 ARM64 5G 路由设备的统一数据与控制服务。

[English](README_EN.md) · [最新版本](https://github.com/33333s/zwrt-datad/releases/latest) · [API 文档](docs/API.md)

## 项目介绍

`zwrt-datad` 运行在设备本机，统一读取 `ubus`、`uci`、`sysfs` 和必要的设备日志，把不同机型的底层接口整理成稳定的 JSON 状态，并通过 HTTP 与 SSE 提供给 UFI、WebUI、脚本或其他本机服务。

项目使用 Rust 实现，并通过机型模板隔离固件差异。上层应用不需要为每台设备重复轮询厂商接口，也不需要自行解析日志。datad 只负责设备数据、设备控制和自身更新，不包含前端页面、插件系统或 UFI 业务。旧 C/Go 实现保留在 `c` 分支，不再用于新版本发布。

## 主要功能

- 聚合设备、系统、CPU、内存、存储、温度、电池和运行状态
- 聚合 SIM、移动网络、信号、频段、流量、QoS、Wi-Fi、客户端和短信数据
- `GET /state` 提供完整 JSON 快照，`GET /events` 通过 SSE 推送变化
- 按机型模板规范化字段，并通过 `/capabilities` 暴露当前能力
- 通过 `POST /control` 执行经过约束的蜂窝、Wi-Fi、APN、短信、电源和设备控制
- 提供设备当前注册的 ubus 查询与调用接口，供受信任的管理应用使用
- 可选邻小区采集，具有独立 worker、容量限制、过期处理和进程隔离
- 可选 NMS 云端连接与远程服务入口
- 内置 datad 自更新，使用 Ed25519 签名和 SHA-256 校验更新清单与二进制
- 单进程 Rust、静态 ARM64 发布，默认每秒生成一次状态快照

## 当前已适配设备

| 设备型号 | 产品名称 |
| --- | --- |
| `MU5250` | U60 Pro |
| `MC8532B` | G5 Pro |
| `MU5252` | TopFlow |
| `MC7523` | G5 Max WiFi |

运行时只有 `device.api_template_supported = 1` 才代表识别到正式模板。不同设备只输出实际支持的状态块；调用方应通过字段是否存在判断能力，不要为缺失功能补 `0`、`-1` 或空对象。

各机型的数据来源和差异见 [`docs/models/`](docs/models/)。其他机型可能进入兼容模板，但不代表已经完成适配。

## 一键安装或升级

要求设备为 ARM64/aarch64、使用 root 执行，并可写入 `/data`：

```sh
curl -4fL --retry 3 \
  'https://github.com/33333s/zwrt-datad/releases/latest/download/install-datad.sh' \
  -o /tmp/install-datad.sh && \
sh /tmp/install-datad.sh
```

重复执行同一命令即可升级到最新版。安装器会：

1. 下载发布二进制并校验固定的 SHA-256。
2. 在临时端口启动候选版本，检查 `/healthz` 和 `/state`。
3. 备份已有安装，原子写入 `/data/zwrt-datad`。
4. 清理旧版重复启动项，并在 `/etc/rc.local` 写入唯一启动命令。
5. 启动正式服务，检查 9460/9461 健康状态和单进程状态。
6. 任一步骤失败时恢复原文件和原服务。

datad 不安装自己的 `/etc/init.d` 脚本。安装器需要设备提供 `curl`、`sha256sum`、`awk`、`cmp`、`stat`、`flock`、`mktemp`、`readlink` 和 `od`。

## 服务管理

```sh
sh /data/zwrt-datad/service.sh status
sh /data/zwrt-datad/service.sh start
sh /data/zwrt-datad/service.sh restart
sh /data/zwrt-datad/service.sh stop
```

默认路径：

- 程序：`/data/zwrt-datad/zwrt-datad`
- 日志：`/data/zwrt-datad/zwrt-datad.log`
- PID：`/data/zwrt-datad/zwrt-datad.pid`
- 本机 API：`http://127.0.0.1:9460`
- 内网 API：`http://<设备 IP>:9461`

## 快速检查

```sh
curl -fsS http://127.0.0.1:9460/healthz
curl -fsS http://127.0.0.1:9460/version
curl -fsS http://127.0.0.1:9460/state
curl -N http://127.0.0.1:9460/events
```

9460 是设备本机接口。9461 是内网接口，读取数据前需要通过 `/auth/login` 或 `/auth/exchange` 获取 Bearer Token；详细鉴权方式见 [`docs/API.md`](docs/API.md)。

> **安全提示：** `POST /ubus/call` 可以访问运行时注册的 ubus 方法，其中可能包含修改网络、断开连接或重启设备的写操作。只应向受信任的管理程序开放，并由调用方限制入口和进行必要确认。

## 文档

- [`docs/API.md`](docs/API.md)：HTTP、SSE、鉴权与命令行参数
- [`docs/STATE_SCHEMA.md`](docs/STATE_SCHEMA.md)：状态字段契约
- [`docs/CONTROL_API.md`](docs/CONTROL_API.md)：设备控制动作与安全边界
- [`docs/models/`](docs/models/)：已适配设备模板
- [`docs/RUNTIME.md`](docs/RUNTIME.md)：运行、日志与服务管理
- [`docs/NEIGHBOR.md`](docs/NEIGHBOR.md)：可选邻区采集
- [`docs/CLOUD.md`](docs/CLOUD.md)：可选 NMS 云端连接

## 许可与贡献者

项目使用 [MIT License](LICENSE)，项目署名见 [`CONTRIBUTORS.md`](CONTRIBUTORS.md)。
