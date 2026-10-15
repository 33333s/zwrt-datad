# EFS 文件管理 API

datad 移植了 `diag_efs_tool_once` 的文件操作，通过独立的 `efs-worker` 加载设备上的
`libdiag.so.1` / `libdiag.so`。Rust 主程序仍为静态二进制，厂商库的崩溃不会直接影响 datad。
每个请求只初始化、注册一次 DCI 客户端，完成操作后释放。

## 鉴权

所有 `/efs/*` 接口，包括能力查询，在 9460 本机端口与 9461 内网端口上都必须携带：

```http
Authorization: Bearer <datad token>
```

复用 datad 当前的服务 Token 或 `/auth/login`、`/auth/exchange` 发放的会话 Token，
没有独立 EFS 密码或登录入口。不接受 URL 的 `access_token`、Cookie、`X-Auth-Token`
或 Basic 作为 EFS 鉴权。认证在解析参数、保存上传文件和调用 worker 之前完成。
这些操作没有加入 `/control` 或云端面板动作白名单。
成功和已认证错误响应带 `Cache-Control: no-store`。

## 接口

| 方法与路径 | 参数 | 用途 |
|---|---|---|
| `GET /efs/status` | 无 | 构建能力、限制及操作列表；不会连接调制解调器 |
| `GET /efs/nvread` | Query: `item`（0–65535） | 读取任意指定 NV 项，返回完整 DIAG 响应 HEX |
| `GET /efs/list` | Query: `path` | 列目录 |
| `GET /efs/stat` | Query: `path` | 文件、目录信息 |
| `GET /efs/read` | Query: `path`, `length?`, `offset?` | 分页读文件，返回 hex |
| `POST /efs/write` | JSON: `path`, `hex`, `offset?` | 写入 hex 内容 |
| `POST /efs/upload` | Query: `path`；Body: 原始二进制 | 上传完整文件 |
| `GET /efs/download` | Query: `path` | 下载完整文件 |
| `POST /efs/delete` | JSON: `path`, `directory?` | 删除文件；`directory:true` 删除空目录 |
| `POST /efs/mkdir` | JSON: `path`, `mode?` | 创建目录，mode 为八进制字符串，默认 `"0755"` |
| `POST /efs/rename` | JSON: `path`, `destination` | 重命名或移动 |

EFS 路径属于调制解调器文件系统，不是设备 Linux 的路径。必须以 `/` 开头，不能有
控制字符或 `.` / `..` 路径分量，最多 1011 字节；重命名的两个路径合计最多 1018 字节。
参数中的路径按原值传给 EFS，不通过 shell；空格及合法 Unicode 可以使用。
未声明的请求参数和 JSON 字段均拒绝。

### 写保护（App 与后端共同执行）

`/` 以及根目录的直属条目均只读。例如 `/file`、`/nvefs` 不能修改、删除或移动，
不能在 `/` 下创建新条目；`/nvefs/file` 可以在检查通过后操作。移动同时检查源、目标。
不接受重复斜杠、尾部斜杠（根目录除外）或任何递归删除参数；删除目录仅调用原生 rmdir。

worker 对每次修改逐级枚举父目录，拒绝符号链接、特殊文件、类型不明的条目和多重硬链接文件。
重命名/移动的目标必须不存在。HTTP 始终通过 `--guarded` 调用 worker，不能通过 API 关闭保护。
这不是 EFS 事务：其他 DIAG 客户端或调制解调器的并发修改仍可能与检查产生竞争，
操作期间应避免其他工具同时修改 EFS。中途失败可能已写入部分数据，不自动重试。

`GET /efs/status` 增加以下能力声明；旧后端没有这些能力时，App 只提供读取：

```json
{"safety":{"version":1,"root_readonly":true,"recursive_delete":false,"no_symlink_mutations":true}}
```

直接运行 worker CLI 是独立的维修工具；其未带 `--guarded` 的旧命令不属于 App/API 的保护范围。

### JSON 响应

成功示例：

```json
{"ok":true,"command":"list","data":{"path":"/nvefs","entries":[{"name":"example","name_hex":"6578616d706c65","type":"file","mode":33188,"size":3,"atime":0,"mtime":0,"ctime":0}]}}
```

条目的 `type` 为 `file`、`directory`、`symlink`、`item` 或 `other`，`item` 表示 Item File。
`mode` 为包含类型位的十进制整数，时间为设备报告的 Unix 秒。
`name_hex` 保留 EFS 原始名称字节；UTF-8 名称正常输出，非法 UTF-8 字节转义为 `\u00xx`。
`stat` 的 data 包含 `path/type/mode/size/links/atime/mtime/ctime`。

失败示例：

```json
{"ok":false,"command":"write","error":{"code":"EFS_ERROR","message":"WRITE: EFS errno 28","efs_errno":28,"diag_status":0,"local_errno":0,"bytes_transferred":150}}
```

错误不伪装成空目录或成功。`efs_errno` 保留设备原始值，`bytes_transferred` 保留已确认的
字节数。写入失败或超时可能已经修改部分内容，不自动回滚，不应盲目自动重试。
libdiag 日志不进入 API 响应，也不记录文件内容、完整路径或 Token 到活动日志。

常见 HTTP 状态：`400` 参数错误、`401` 鉴权失败、`403` EFS 权限错误、`404` EFS 不存在、
`409` 文件/目录冲突或 `efs_busy`、`413` 超限、`500` 私有工作目录错误、`502` DIAG/协议错误、
`503` 未嵌入 worker 或厂商库不可用、`504` 超时、`507` EFS 空间不足。

### NV 只读查询

`GET /efs/nvread?item=550` 对应 worker 的 `nvread 550`。`item` 为必填十进制
无符号 16 位编号，不限定 550；未声明参数、负数和超范围值被拒绝。
此接口仅发出 `DIAG_NV_READ_F`（0x26），没有 NV 写入或任意 raw DIAG 接口。
鉴权、无缓存、并发槽和超时限制与其他 EFS 接口相同。

返回 `{"ok":true,"command":"nvread","data":{"encoding":"hex","bytes":133,"hex":"…"}}`。
`hex` 是完整原始响应，不裁剪、不转换 IMEI。典型响应为：命令 1 字节、NV 编号
2 字节小端、NV 数据 128 字节、NV 状态 2 字节小端。
`ok:true` 仅代表取得响应，不代表 NV 状态为成功；异常状态或非标准长度同样保留原始 HEX，
由客户端解释。没有响应、厂商库或传输失败仍返回 API 错误。

App 的 IMEI 工具默认读取 550，在 App 内转换 Qualcomm BCD；无法转换时显示原始 HEX。
NV 内容不写入活动日志。

### 分页读取与编辑

```sh
curl -H "Authorization: Bearer $DATAD_TOKEN" --get \
  --data-urlencode 'path=/nvefs/example' --data-urlencode 'length=4096' \
  --data-urlencode 'offset=0' http://127.0.0.1:9460/efs/read

curl -H "Authorization: Bearer $DATAD_TOKEN" -H 'Content-Type: application/json' \
  --data '{"path":"/nvefs/example","hex":"010203"}' \
  http://127.0.0.1:9460/efs/write
```

读取默认 length=4096、offset=0，返回 `hex/bytes/offset/next_offset/eof`。
单次读取最多 64 KiB。`eof:true` 表示读到了零字节响应，恰好达到请求长度时为 false；
前端继续使用 `next_offset` 请求下一页。

写入最多 32 KiB 解码后的内容，只接受连续 hex 字符。
**省略 offset 时创建或截断并替换整个文件**；提供 offset（包括 0）则保留其余内容。
空 hex 且省略 offset 会清空或创建文件。offset 为 32 位无符号整数，范围不能溢出。
内容先保存到私有临时文件，不出现在 worker 的命令行参数中。

### 上传下载

```sh
curl -H "Authorization: Bearer $DATAD_TOKEN" -H 'Content-Type: application/octet-stream' \
  --data-binary @example.bin 'http://127.0.0.1:9460/efs/upload?path=%2Fnvefs%2Fexample'

curl -f -H "Authorization: Bearer $DATAD_TOKEN" \
  'http://127.0.0.1:9460/efs/download?path=%2Fnvefs%2Fexample' -o example.bin
```

每个完整文件最多 1 MiB。上传不接收 JSON 包装或客户端本地文件名，只将 HTTP body
作为原始文件字节。下载成功返回 `application/octet-stream` 和附件头；失败返回 JSON。
上传目标必须是完整 EFS 文件路径，不会自动拼接文件名或创建父目录。
本机临时路径由 datad 生成，API 不能指定服务器本地读写路径。
工作目录 `<data_dir>/efs` 为 0700，临时文件为 0600，操作结束清理；worker 为 0500。
普通下载失败保留原目标，worker 写完并关闭远端文件后才原子提交。

### 并发与能力

同一个 datad 的两个监听端口共用一个 EFS 操作槽。正在执行操作时新的操作返回
`409/efs_busy`；HTTP 调用方断开不会提前释放槽或启动另一项操作。
每个 DIAG 请求等待最多 5 秒，整项 API 操作最多 120 秒。超时结束并回收 worker；
强制停止 datad 后，遗留的传输临时文件可能需要清理。
目录列表上限为 4096 条，超过返回明确错误。

`status.available` 表示 worker 已编译进 datad，不代表已验证设备的 DIAG 或每项 EFS 操作。
`reason:not_probed`、`device_verified:false` 明确标识未探测；首次操作的真实结果才能确认设备支持。
无 worker 的主机/构建返回 `available:false/reason:worker_not_built`，其他操作返回 503。
除文件管理外，提供通用只读 NV 查询；未加入任意原始 DIAG 收发、NV 写入或递归删除。

## 构建与测试

`scripts/build-arm32-candidate.sh` 编译 glibc 2.31 ARMv7 worker 并嵌入 ARM32 datad。
`scripts/build.sh` 使用原有 Bootlin 工具链编译动态 musl AArch64 worker 并嵌入 ARM64 datad。
主程序保持静态；worker 使用设备的厂商库，不分发 libdiag。
构建时也可以显式传入 `DATAD_EFS_WORKER=<matching ELF>`，build.rs 检查其架构。
ARM32 保留 `-fPIC -pie`，避免曾发生过的 libc 数据 copy relocation 错误。

```sh
python3 tests/efs_worker_test.py
cargo test --manifest-path rust/Cargo.toml --locked
cargo build --manifest-path rust/Cargo.toml --locked
python3 tests/efs_http_test.py rust/target/debug/zwrt-datad
```

HTTP 集成测试使用 debug 构建的测试 worker 注入。release 构建不会读取
`ZWRT_DATAD_EFS_TEST_WORKER`、测试配置或 mock 环境变量。
测试验证两个端口的鉴权、参数校验、JSON/binary 传输、错误、串行执行及私有存储；不接触真实调制解调器。
