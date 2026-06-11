# u60-datad

`u60-datad` 是一个很小的、单静态二进制的设备状态聚合器，面向 ZTE U60Pro 以及类似的 SDX 系列 OpenWRT MiFi 设备。它只做一件事：固定频率轮询 `ubus`，把结果归一化成一份扁平的 JSON 快照，然后写到 tmpfs 文件里。所有消费者都只读这个文件，不再各自去打 `ubus`。

> 这是一个独立的 clean-room 实现，只依赖标准 OpenWRT `ubus`。不链接厂商库，也不隶属于 ZTE。

## 为什么要有它

如果每个消费者都自己反复执行 `ubus call`，就会把面向调制解调器的服务压得很厉害，尤其是信号类查询常常会继续往下走到 AT 命令。`u60-datad` 把 `ubus` 的负载和消费者数量解耦了：

- `ubus` 只被单个进程按固定频率轮询
- 其他所有消费者只读取一个很小的文件
- 读文件比重复打 `ubus` 便宜得多，也更稳定

```text
zte_nwinfo_api   zwrt_bsp.*   zwrt_router.api   zwrt_data
       \             |              |               /
        \            |              |              /
         +-----------+--------------+-------------+
                         ubus
                           |
                       [ u60-datad ]
                           |
                 /tmp/u60-datad/state.json
                           |
      +--------------------+--------------------+
      |                    |                    |
   devui                 web                 scripts
```

## 构建

需要一个 POSIX shell 和 aarch64 musl 工具链。构建脚本会直接调用交叉编译器，不依赖宿主机的完整开发环境。

```sh
bash scripts/build.sh
```

生成结果是一个静态二进制，典型大小约为 136K，strip 后约 58K。

## 运行

手动运行：

```sh
adb push u60-datad /usr/bin/ && adb shell '
  chmod 755 /usr/bin/u60-datad
  /usr/bin/u60-datad -i 1000 &
'

# 只跑一次并输出到 stdout，方便调试
./u60-datad --once
```

安装成 OpenWRT 服务：

```sh
adb push scripts/u60-datad.init /etc/init.d/u60-datad
adb shell 'chmod 755 /etc/init.d/u60-datad &&
           /etc/init.d/u60-datad enable && /etc/init.d/u60-datad start'
```

## 读取方式

消费者读取 `/tmp/u60-datad/state.json` 并解析 JSON。完整字段契约见 [docs/STATE_SCHEMA.md](docs/STATE_SCHEMA.md)。

```sh
. /dev/stdin <<'EOF'
RX=$(jsonfilter -i /tmp/u60-datad/state.json -e '@.traffic.rx_speed')
echo "down: $RX B/s"
EOF
```

## 参数

| 参数 | 说明 |
|------|------|
| `-i <ms>` | 轮询间隔，单位毫秒，默认 `1000` |
| `--once` | 轮询一次，把快照打印到标准输出后退出 |

## 许可证

[MIT](LICENSE)

