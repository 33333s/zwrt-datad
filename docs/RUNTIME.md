# 运行与日志

`zwrt-datad` 读取设备 `ubus/uci/sysfs` 与 QoS 日志，并在本机 `127.0.0.1:9460` 提供 HTTP/SSE 和白名单控制接口。云端 TLS/MQTT/WebSocket 运行时静态链接在同一个 `zwrt-datad` 二进制和进程中；`cloud.json` 仍留在正式数据目录。

## 推荐启动方式

正式启动脚本是项目内的 [`scripts/service.sh`](../scripts/service.sh)，部署后固定放在 `/data/zwrt-datad/service.sh`：

```sh
sh /data/zwrt-datad/service.sh start
sh /data/zwrt-datad/service.sh status
```

开机自启只允许在 `/etc/rc.local` 的 `exit 0` 之前调用 `sh /data/zwrt-datad/service.sh start`，不向 `/etc/init.d` 安装 datad 脚本。除 `rc.local` 外，二进制、脚本、PID、日志、Token 与配置全部留在 `/data/zwrt-datad`。

临时调试若确实需要绕过正式脚本，可用：

```sh
nohup /data/zwrt-datad/zwrt-datad -i 1000 \
  --auth-token-file /data/zwrt-datad/auth.token \
  --webshell \
  >/dev/null 2>&1 </dev/null &
```

`--auth-token-file` 现在是有效运行参数。文件首行去除首尾空白后作为 Bearer Token；指定了该参数但文件不存在或为空时，进程拒绝启动。`/healthz` 保持公开，其余数据和控制接口要求：

主监听若配置为非回环地址但未启用鉴权，datad 会拒绝启动，避免误把控制接口暴露到网络。对外提供内网访问时使用 `--lan-bind`，该监听始终要求鉴权。

```http
Authorization: Bearer <token>
```

也兼容仅供本机服务间调用的 `X-Auth-Token` 请求头。不要把 Token 写入前端静态文件。

正式 `service.sh` 使用 `--webshell` 启用 WebShell。9460 和 9461 上的 WebShell
都必须携带有效 Token；9461 同时受 LAN 来源过滤约束。浏览器 WebSocket 使用
`datad-webshell-v1` 和 `datad-auth.<token>` 两个子协议完成鉴权，查询参数 Token
始终拒绝。首次启动会从 `/dev/urandom` 生成 32 字节随机 Token，以 `0600`
原子保存；异常 Token 文件会令启动失败。

PTY 子进程优先使用 `/bin/ash -l -i`，固定从 `/` 启动；登录 shell 按 OpenWrt
标准流程加载 `/etc/profile` 并显示设备自己的 `/etc/banner`。

不要把长期、无轮转的输出重定向到 `/tmp/*.log`。在常见 OpenWrt 设备中，`/tmp` 位于 tmpfs；如果某个扩展构建或诊断后端输出高频调试信息，日志文件会直接占用 RAM，表现为“可用内存持续下降”，并不等同于进程 RSS 泄漏。

若确实需要保留诊断日志，应使用具有容量上限和轮转策略的持久化目录；诊断结束后及时停用高频输出并清理旧文件。

## ubus 访问（0.10.56）

datad 直接连接 ubusd 的 unix socket（`/var/run/ubus/ubus.sock`）发起请求，不再为每次调用启动一个 `ubus call` 进程；返回值和错误文字与 `ubus call` 保持一致。同一时刻只有一个请求在途（单一执行器）：

- 状态采集是低优先级通道；控制、远程面板、定时任务等交互请求排队时，采集调用会让出连接，交互请求不会排在整轮采集后面。
- 采集中的单次调用 5 秒超时，超时的对象 30 秒内不再参与采集（交互请求仍照常调用，超时 8 秒）；采集时发现未注册的对象 10 秒内不再重复查找。
- 交互请求最多排队 8 个，再多立即返回 `ubus busy`。调用方中途取消时该连接作废、下次重新连接，不会读错应答。
- socket 不可用（请求尚未发出）时自动改用 `ubus call`，30 秒后再试 socket；已发出的请求超时或失败一律不重发。

环境变量：`ZWRT_DATAD_UBUS=cli` 强制使用 `ubus call`；`ZWRT_DATAD_UBUS=socket` 只用 socket、不回退；`ZWRT_DATAD_UBUS_SOCKET` 指定 socket 路径；`ZWRT_DATAD_UBUS_STATS=1` 在 `/state` 增加 `ubus_stats`（调用数、错误、超时、跳过、回退、排队与忙碌毫秒、采集轮耗时），仅用于排查。

`zwrt-datad --ubus-compare 对象:方法 ['{json参数}'] …` 把只读调用分别经 socket 和 `ubus call` 执行并比较，只输出是否一致与不一致的字段路径，不输出字段值；上新机型或固件前可先用它核对。部分原厂接口每次调用都会重新加密敏感字段（如 APN `password`、MU5252 的 `msisdn`/`sim_imsi`），这些路径本来就会不同。

## 运行检查

### 设备签名身份

身份不会在启动时自动创建，也不会自动上传或登记到任何服务器。`POST /identity/init` 首次显式创建；私有目录为 `$ZWRT_DATAD_DIR/identity`（0700），其中 `key.json`（0600）保存硬件绑定的加密 key blob 与公钥，`initialized-key-id` 保存本机初始化完成后的公钥指纹。它不是原始私钥文件，不能复制到其他设备使用。OTA 应保留整个 identity 目录；损坏、外机 blob、已初始化但密钥文件丢失或硬件不可用只报错，不自动换钥匙。初始化被中断而密钥文件完整时，可以在验证同一公钥后补齐标记，不重新生成。删除整个目录或恢复出厂可能丢失原身份，再初始化产生新公钥，必须由后台受控换绑。

主程序仍为静态 Rust；`scripts/build.sh` 通过同一 `rust/Cargo.toml` 构建并内嵌一个短生命周期 Rust worker，它只动态加载设备已有的 `/usr/lib/libKeyMaster.so.0.0.0`。不分发、不替换原厂库，不调用 attestation keybox provision、清空全部密钥、设备ID或熔丝接口。worker 缓存在私有目录，单次运行结束即回收原厂库的进程资源；硬件接口不可用时不退回软件密钥。首次来源仍未远程证明，见 [API.md](API.md)。

本机管理员可运行 `zwrt-datad --identity init`、`--identity public-key`，或通过 stdin 提交相同请求 JSON 给 `--identity sign`。这些诊断命令不启动采样/云连接/散热控制；`init` 会在指定 `--data-dir` 内持久化密钥，其他两项不会初始化身份。普通读取 USB 的 `--usb-status` 不会触碰身份。`keymaster-worker` Cargo feature 和 debug-only 的 `ZWRT_DATAD_IDENTITY_TEST_WORKER` 仅用于构建/回归，正式 main build 不启用测试替身。

`zwrt-datad --usb-status` 只读输出 USB 协商速率 JSON，然后退出；不初始化云连接、状态采样、风扇控制或配置文件，可在正式服务运行时用于核对。HTTP 接口为 `GET /usb/status`，状态/SSE 顶层字段为 `usb`。测试可通过 `ZWRT_DATAD_USB_UDC_ROOT` 与 `ZWRT_DATAD_USB_HOST_ROOT` 指定隔离 sysfs 目录；这些路径不能由 HTTP 请求设置。

```sh
curl -fsS http://127.0.0.1:9460/healthz
curl -fsS -H "Authorization: Bearer $(cat /data/zwrt-datad/auth.token)" \
  http://127.0.0.1:9460/state
```

`/healthz` 返回 `ok` 表示服务监听正常；`/state` 用于检查最新聚合快照。
