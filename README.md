# u60-datad

`u60-datad` 是一个很小的单静态二进制状态聚合器，面向 ZTE U60Pro 和类似的 SDX 系列 OpenWRT MiFi 设备。

它的职责很单一：

- 按固定频率轮询 `ubus`
- 启动时全量扫描一次 `/data/logfs/key.log`，提取 QoS 相关缓存
- 在手动刷新或换卡后重读日志，并继续只对外暴露缓存
- 把结果归一化成一份稳定 JSON 快照
- 原子写入 `/tmp/u60-datad/state.json`

所有消费者都只读这个快照文件，不再各自直接打 `ubus` 或自己扫日志。

> 这是一个 clean-room 实现，只依赖标准 OpenWRT 能力，不链接厂商私有库。

## 为什么需要它

如果每个 UI、脚本、网页都自己反复执行 `ubus call`，或者自己去扫 `key.log`，设备上的服务和 I/O 会被打得很碎。`u60-datad` 把这些读取统一收口：

- `ubus` 只被单个进程按固定频率轮询
- `key.log` 也只由单个进程按需读取：启动全量扫一次，之后只在手动刷新或换卡时重读
- 其他消费者只读取一个很小的 tmpfs 文件
- 读文件比重复打 `ubus` / 扫日志便宜得多，也更稳定

```text
zte_nwinfo_api   zwrt_bsp.*   zwrt_router.api   zwrt_data   key.log
       \             |              |               |          /
        \            |              |               |         /
         +-----------+--------------+---------------+--------+
                                [ u60-datad ]
                                      |
                         /tmp/u60-datad/state.json
                                      |
             +------------------------+------------------------+
             |                        |                        |
           devui                     web                     scripts
```

## 构建

需要 POSIX shell 和 aarch64 musl 工具链：

```sh
bash scripts/build.sh
```

## 运行

手动运行：

```sh
adb push u60-datad /usr/bin/ && adb shell '
  chmod 755 /usr/bin/u60-datad
  /usr/bin/u60-datad -i 1000 &
'

./u60-datad --once
```

作为 OpenWRT 服务安装：

```sh
adb push scripts/u60-datad.init /etc/init.d/u60-datad
adb shell 'chmod 755 /etc/init.d/u60-datad &&
           /etc/init.d/u60-datad enable &&
           /etc/init.d/u60-datad start'
```

当前 init 脚本会把 `u60-datad` 固定为 `/data/u60pro/u60-datad`，并在正常开机时以 `START=47` 提前启动；关机充电路径会直接跳过，避免离线充电时额外拉起轮询后端。

## 读取方式

消费者只读：

```text
/tmp/u60-datad/state.json
```

字段契约见 [docs/STATE_SCHEMA.md](docs/STATE_SCHEMA.md)。

QoS 相关有一个容易踩的点：`qci` 往往更新得比 `apn_ambr_*` 更频繁，而且最新一条日志不一定同时带齐所有字段。当前实现改成：

- 进程启动时全量扫描一次 `key.log`
- 逐行提取最近一次有效的 `qci` / `apn_ambr_*`
- 后续只显示缓存，不在每轮快照里反复扫日志
- 收到 `SIGUSR1` 时立即重读
- 检测到 `sim_iccid/current_sim_slot` 变化时清空旧缓存，并在新日志写入后自动补读

示例：

```sh
RX=$(jsonfilter -i /tmp/u60-datad/state.json -e '@.traffic.rx_speed')
echo "down: $RX B/s"
```

## 参数

| 参数 | 说明 |
|------|------|
| `-i <ms>` | 轮询间隔，单位毫秒，默认 `1000` |
| `--once` | 只轮询一次，输出到标准输出后退出 |

运行中的 `u60-datad` 还支持：

```sh
kill -USR1 $(pidof u60-datad)
```

这会立刻触发一次 QoS 日志重读，供 DevUI 的“刷新 AMBR 缓存”按钮复用。

## 发布与版本

每个 release 都会附两份固定资产：

- `u60-datad-aarch64`
- `version.json`

`version.json` 由设备侧管理插件读取，用来判断 datad 是否可单独更新：

```jsonc
{ "schema": 1,
  "datad": { "version": "0.4.1", "asset": "u60-datad-aarch64" } }
```

发版时保持资产文件名不变，只更新版本号与内容即可。当前 U60Pro 发布线固定走 `u60pro` 分支，更新源以 GitHub latest release 为准。

## 许可

[MIT](LICENSE)
