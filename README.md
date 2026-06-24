# u60-datad

`u60-datad` 是一个很小的单静态二进制状态聚合器，面向 ZTE U60Pro 和类似的 SDX 系列 OpenWRT MiFi 设备。

它的职责很单一：

- 按固定频率轮询 `ubus`
- 读取 `/data/logfs/key.log` 里 QoS 相关的最后匹配行（`qci` 与 `apn_ambr_*` 分开抓取）
- 把结果归一化成一份稳定 JSON 快照
- 原子写入 `/tmp/u60-datad/state.json`

所有消费者都只读这个快照文件，不再各自直接打 `ubus` 或自己扫日志。

> 这是一个 clean-room 实现，只依赖标准 OpenWRT 能力，不链接厂商私有库。

## 为什么需要它

如果每个 UI、脚本、网页都自己反复执行 `ubus call`，或者自己去扫 `key.log`，设备上的服务和 I/O 会被打得很碎。`u60-datad` 把这些读取统一收口：

- `ubus` 只被单个进程按固定频率轮询
- `key.log` 也只由单个进程读取少量相关日志，不做每轮全量扫
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

## 读取方式

消费者只读：

```text
/tmp/u60-datad/state.json
```

字段契约见 [docs/STATE_SCHEMA.md](docs/STATE_SCHEMA.md)。

QoS 相关有一个容易踩的点：`qci` 往往更新得比 `apn_ambr_*` 更频繁，所以不能只靠同一个简单尾窗去“顺便读到” AMBR。当前实现会分别取**最后一条 `qci`** 和 **最后一条 `apn_ambr_*`**，再缓存最后已知值。

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

## 许可

[MIT](LICENSE)
