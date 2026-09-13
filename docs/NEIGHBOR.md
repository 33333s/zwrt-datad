# Neighbor cell adapter

`neighbor` 是可选的邻区采集模块，默认关闭。启用后，状态会出现在 `/state`
及其 SSE 快照中；`--once` 不会启动诊断采集器。

通过受鉴权保护的控制接口启用：

```json
{"action":"neighbor.set","params":{"enabled":true}}
```

使用 `neighbor.status` 读取当前状态。设置为关闭时，datad 会等待自己启动的工作
进程退出并清理它拥有的临时目录。

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

## 隔离与资源限制

采集和解析在独立工作进程中执行，不阻塞 datad 主循环。模块只管理自己启动的
进程，不会终止其他诊断采集器；检测到 DIAG 被占用时会返回阻塞状态。

默认临时目录为 `/tmp/zwrt-datad-neighbor`，仅清理其中由本模块创建的目录。
采集文件数量、总大小、目录深度、单次读取量和返回小区数量均有上限。输入停滞、
工作进程异常和父进程退出均有明确的超时与清理路径。

邻区签名与解析布局依赖设备固件。未知布局保持未解析，不会用频率推断或空结果
冒充成功测量。

## 离线解析与测试

```sh
./zwrt-datad --neighbor-parse capture.qmdl [another.qmdl ...]
python3 tests/neighbor_parser_test.py ./zwrt-datad
python3 tests/neighbor_http_test.py ./zwrt-datad
```

离线输入必须是普通文件且不能是符号链接，并受相同的数量与容量限制。合成测试
用于覆盖解析、生命周期、资源限制和鉴权边界，不能替代针对具体固件的实机验证。
