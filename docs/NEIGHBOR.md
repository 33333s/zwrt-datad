# Neighbor cell adapter

`neighbor` 是可选的邻区采集模块，默认关闭。启用后，状态会出现在 `/state`
及其 SSE 快照中；`--once` 不会启动诊断采集器。

通过受鉴权保护的控制接口启用：

```json
{"action":"neighbor.set","params":{"enabled":true}}
```

使用 `neighbor.status` 读取当前状态。设置为关闭时，datad 会等待自己启动的工作
进程退出并清理它拥有的临时目录。
启动失败会清理本次私有状态并恢复 `enabled=false`，以免后台反复尝试占用诊断采集器。

## 状态解释

`status` 可能为：

- `disabled`、`stopping`、`starting`
- `collecting`、`ready`、`empty`、`stale`
- `blocked`、`dependency_missing`、`error`

`empty` 且 `reason=no_supported_reports` 表示已经收到诊断帧，但当前解析器没有
识别到受支持的报告；它不表示附近没有邻区。`ready` 表示经过服务小区和载波聚合
过滤后，至少仍有一个未过期结果。

未知的 ARFCN、频段和 RSRP 使用 JSON `null`。调用方不得根据服务频点猜测邻区
频点，也不应把缺少频点证据的同 PCI 记录合并成一个已确认小区。

`partial=true` 表示当前结果时间窗口内发生过截断、损坏或丢弃，调用方应明确
显示结果可能不完整。邻区测量是观测结果，不代表小区一定可以锁定或注册。

完整状态字段见 [`STATE_SCHEMA.md`](STATE_SCHEMA.md)，控制动作见
[`CONTROL_API.md`](CONTROL_API.md)。

## 4G（LTE）邻区

4G 邻区不需要 DIAG 采集：厂商 `zte_nwinfo` 服务在设备驻留 LTE（含 NSA 的 LTE 锚点）时，
每隔几秒把调制解调器自己的扫描结果写进
`zte_nwinfo.manual_scan.lteg_nbr_content`，格式为 `PCI,EARFCN,B<频段>,RSRP,RSRQ;`，
第一条是服务小区，其后是同频和异频邻区。datad 在每次状态刷新时读取它，**不管 `neighbor`
是否启用**，结果输出在 `neighbor.lte`：

```json
{"supported":true,"status":"ready","reason":"none","source":"vendor_scan",
 "sampled_at":1791000000,"age_ms":4200,
 "cells":[{"rat":"LTE","pci":490,"arfcn":2850,"band":7,"rsrp_dbm":-93,"rsrq_db":-18,
           "frequency_relation":"intra","frequency_evidence":"explicit",
           "samples":1,"direct_hits":0,"source":"vendor_scan"}]}
```

- `status`：`ready`、`empty`（没有邻区）、`stale`、`unavailable`。
- `unavailable` 的 `reason`：`not_reported`（固件没有这个字段，`supported=false`）、
  `not_on_lte`（网络类型为空或独立组网 SA，没有 LTE 锚点，遗留列表不输出）。
- 已去掉服务小区和载波聚合成员，`frequency_relation` 为 `intra/inter`。
- `rsrp_dbm` 取整数 dBm，`rsrq_db` 取整数 dB，缺失或越界为 `null`；EARFCN 0 是合法值；
  PCI 超过 503、RSRP 不在 -140..-30 的记录直接丢弃，不修补。同一 PCI+EARFCN 取最强读数，
  最多 32 个小区，从强到弱排序。
- 厂商列表没有时间戳，datad 记录列表**内容最后一次变化**的时间；超过 300 秒不变视为
  `stale`，此时不输出小区。`age_ms` 是自上次变化起的毫秒数。
- 已启用 DIAG 采集（`neighbor.enabled=true`）且状态为 `ready/empty` 时，4G 小区同时并入
  `neighbor.cells`，并替换 DIAG 解析出的 LTE 行（调制解调器的直接测量更可靠）；此时仅有 4G
  邻区也会使 `status=ready`。固件没有该字段时保留 DIAG 的 LTE 行。
- 已在 MC7523（LTE-NSA）、MC8532B 和 TopFlow（ENDC）上确认该列表约每 10 秒变化，且包含
  同频与异频邻区；SA 驻留时的行为按上面规则不输出。

## 隔离与资源限制

采集和解析在独立工作进程中执行，不阻塞 datad 主循环。模块只管理自己启动的
进程，不会终止其他诊断采集器；检测到 DIAG 被占用时会返回阻塞状态。

默认临时目录为 `/tmp/zwrt-datad-neighbor`，仅清理其中由本模块创建的目录。
采集文件数量、总大小、目录深度、单次读取量和返回小区数量均有上限。输入停滞、
工作进程异常和父进程退出均有明确的超时与清理路径。

邻区签名与解析布局依赖设备固件。未知布局保持未解析，不会用频率推断或空结果
冒充成功测量。

MU5250 `BD_FLYMODEMMU5250V1.0.0B29` 的签名是已支持记录的构建偏移别名。该固件带
频点的身份记录比信号快照稀疏，短距离锚点找不到时，B29 快照可以使用同一采集文件
中该 PCI 唯一出现过的频点；同一 PCI 出现多个频点时保持未解析并计入 `ambiguous`。
RSRP 原始值 -19968（-156 dBm）是“未测量”标记，不作为信号值，小区仍按身份记录列出。

## 离线解析与测试

```sh
./zwrt-datad --neighbor-parse capture.qmdl [another.qmdl ...]
python3 tests/neighbor_parser_test.py ./zwrt-datad
python3 tests/neighbor_http_test.py ./zwrt-datad
```

离线输入必须是普通文件且不能是符号链接，并受相同的数量与容量限制。合成测试
用于覆盖解析、生命周期、资源限制和鉴权边界，不能替代针对具体固件的实机验证。
