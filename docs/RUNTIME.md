# 运行与日志

`zwrt-datad` 读取设备 `ubus/uci/sysfs` 与 QoS 日志，并在本机 `127.0.0.1:9460` 提供 HTTP/SSE 和白名单控制接口。它不包含 modem signaling capture/decode、DCI 或厂商 DIAG 工具；这些能力不属于本仓库的公开边界，详见 [REPO_BOUNDARY.md](REPO_BOUNDARY.md)。

## 推荐启动方式

在 OpenWrt 上优先使用 [`scripts/zwrt-datad.init`](../scripts/zwrt-datad.init) 交给 `procd` 管理。临时后台启动可用：

```sh
nohup /data/plugins/zwrt-datad/zwrt-datad -i 1000 \
  --auth-token-file /data/plugins/zwrt-datad/auth.token \
  >/dev/null 2>&1 </dev/null &
```

`--auth-token-file` 现在是有效运行参数。文件首行去除首尾空白后作为 Bearer Token；指定了该参数但文件不存在或为空时，进程拒绝启动。`/healthz` 保持公开，其余数据和控制接口要求：

```http
Authorization: Bearer <token>
```

也兼容仅供本机服务间调用的 `X-Auth-Token` 请求头。不要把 Token 写入前端静态文件。

不要把长期、无轮转的输出重定向到 `/tmp/*.log`。在常见 OpenWrt 设备中，`/tmp` 位于 tmpfs；如果某个扩展构建或诊断后端输出高频调试信息，日志文件会直接占用 RAM，表现为“可用内存持续下降”，并不等同于进程 RSS 泄漏。

若确实需要保留诊断日志，应使用具有容量上限和轮转策略的持久化目录；诊断结束后及时停用高频输出并清理旧文件。

## 运行检查

```sh
curl -fsS http://127.0.0.1:9460/healthz
curl -fsS -H "Authorization: Bearer $(cat /data/plugins/zwrt-datad/auth.token)" \
  http://127.0.0.1:9460/state
```

`/healthz` 返回 `ok` 表示服务监听正常；`/state` 用于检查最新聚合快照。
