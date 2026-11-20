# 可选内嵌 Web 控制台

范围为 `UFIPanel-datad-Web-reference-20261120-154921.zip` 中 App 已接入的 datad 分支。逐操作清单及未验证项见 [功能覆盖表](WEB_APP_COVERAGE.zh-CN.md)。

以 Cargo `web-ui` feature 构建的版本，默认在既有鉴权 LAN listener 提供 `/ui/`，传入 `--disable-web-ui` 才禁用。未包含该 feature 的纯 API 构建保持不变；`/` 横幅、健康检查和已有客户端接口保持原契约。版本读取仓库 `version.json`，沿用上游 OTA 和 NMS 更新机制；网页不根据版本号改变更新策略。

## 构建与产物

前端使用 React、Mantine Core/Charts、Motion、Lucide、xterm、Virtuoso 和 Vite；版本锁在 `web/package-lock.json`。设备不需要 Node.js、CDN、外部字体或独立网页进程。

```powershell
cd D:\Git\zwrt-datad
npm ci --prefix web
npm run build --prefix web
```

前端生成 `web/dist/embedded.rs` 与 gzip 资源表。开启 feature 时缺少前端产物会编译失败，避免交付空界面。纯 API 构建不引用网页资源。

Linux / WSL 主机构建：

```sh
cargo +1.89.0 build --manifest-path rust/Cargo.toml --locked --features web-ui
```

U50 ARMv7 交叉构建（先完成前端构建）：

```sh
cd /mnt/d/Git/zwrt-datad
ZIG=~/zig/zig bash scripts/build-arm32-web.sh
```

输出单独放在：

- `build/web-ui/armv7/zwrt-datad-armv7-web`
- `build/web-ui/armv7/SHA256SUMS`

该脚本同时内嵌 DIAG worker、streamer、EFS worker。默认仅生成独立产物；使用 `--candidate` 可同步打包输入及 provenance，再运行本机 `build/refresh-deploy-package.py --require-web` 整套刷新 `build/deploy_kano`。当前已按用户要求完成这一步，Shell/tiny/Go/ADB及校验文件一致。完整流程见 [ARM32 编译打包说明](ARM32_BUILD_PACKAGE.md)。设备实测和配置写入验收边界见验证记录。

保持现有启动参数，无需追加网页开关，访问 `http://设备IP:9461/ui/`。普通平台需要配置已鉴权 `--lan-bind`；U50 使用其鉴权 LAN listener，`--u50-loopback-only` 下网页与 LAN API 一起关闭。启动参数不会由构建脚本偷偷改写。

## 功能组织

五个主要入口：概览、信号参数、频段与小区、短信、设置。信令、EFS、NV 另有快捷入口。

- 概览：设备/网络/SIM/电池、当前驻网与 NSA 锚点、RSRP/速率趋势切换和折叠、今日/月流量、实时速率、资源状态与详情跳转。
- 信号：服务小区参数、质量评分、全部已确认 PCC/PSCell/SCC、未激活载波、邻区与 SA 扫描、锁定带入、QoS 桥接。不在日常页面提供载波模拟/JSON回放入口。
- 锁定：自动/4G/SA/NSA、三类频段选择/解除、重拨/等待/回读；LTE/NR PCI/频道手动或带入锁定、全局解除、必要模式协调与可选重启。频段锁定不提示重启。
- 短信：收件/发件、实际存在的草稿/未知分类、排序/搜索/分页/全文/复制/回复、能力驱动的发送卡槽、UTF16BE、真实发送状态与删除。结果未知不自动重发。
- 设置：设备信息/重启、蜂窝网络、热点双频/合一/访客、APN、LAN/DHCP/MTU、流量管理/历史、四渠道短信转发/过滤/电源通知、15类定时任务、执行记录、断网恢复、Root Shell、datad 更新、NMS、官方后台避让与登录提醒。
- EFS/NV：五秒风险确认、根及直属条目只读、路径/排序/筛选/分页/属性、UTF8/HEX、下载/上传/文本与HEX编辑/移动/mkdir/空目录删除、任意NV只读、IMEI转换与INI生成/预览/保存/确认上传。

具体可编辑字段、参数类型、回读及测试对应在覆盖表中逐项登记；后端有但 App 尚未接入的 Hosts、普通文件、系统时间等没有扩进来。参考投影明确省略的原生应用入口不伪造网页版本。

## 信令解码链路

`GET signal/stream`（Bearer）→ `ReadableStream` → 增量 `FrameReader` → 独立 Worker → App 现有协议解码 → 上下文/卡片归并 → 卡片、表格和详情。

已移植 App 活动路由中的全部 NR ML1、LTE、NAS 与 RRC UPER 解码方法、数据承载关联、工程信息、CSI-RS/链路配置、NR功率时隙关联和TA公式。未匹配长度/版本或校验失败的布局保留原始记录，不用猜值填满卡片。App 中已停用的旧 TemplateDecoder/PerDecoder 不重新启用。

卡片包含服务小区、运营商/所选PLMN、厂商证据、安全算法、邻区/SSB/模三、QoS、BLER/MCS/RB/grants/layers/天线、工程参数/高铁识别、PUSCH主卡与分开的PUCCH详情、TA估算。来源、统计口径、缺失值及历史测量标记保留；厂商/模三/TA仍按原实现的推断或估算表达。

RRC树和完整NDJSON使用虚拟列表；日志保留近期300条/16MiB，原包独立保留最近24MiB。导出BIN与NDJSON使用全部仍在原包缓存中的帧；近期JSON菜单明确标出近期事件数量。导出有处理进度，不把“已请求浏览器下载”冒充“已保存到磁盘”。

进入信令页按 App 行为自动抓取；停止/离页取消读取和重连任务，离页终止Worker及定时器。网络/会话/小区变化按原数据桥接规则清理；回放不混用当前设备的RAT/测量，也不写入实时QoS缓存。

## 鉴权、生命周期与写入

登录壳静态资源公开；HTTP数据、EFS、终端、信令继续鉴权。密码/Bearer仅保存在内存，不放URL、Cookie或浏览器持久存储。浏览器本地仅保存连接名称/地址与有设备归属的精简信令摘要，忘记设备清除对应摘要。

所有资源/API路径保留反向代理前缀。客户端代际及页面AbortController阻止旧回包污染新会话；网络重驻留事务离页后完成一次必要重连，退出登录则取消旧会话事务。普通写操作不因超时/断网自动重放。

终端使用现有服务端的 `datad-webshell-v1` + `datad-auth.<token>` 子协议握手；URL不带Token。仅返回主子协议，不回显鉴权项；PTY有初始化超时、输入/输出队列限额，离页关闭。静态资源allowlist、MIME、gzip、nosniff、CSP仅放行同源脚本/Worker，Mantine/Motion样式可动态设置。

表单有未保存提示、局部loading、上次结果清除、确认、真实业务成功/已提交/未确认分支。EFS另外检查后端完整保护、名称/路径/类型/链接数，编辑前重读比对原始文件；不递归删除。写结果未知时两分钟内禁止继续修改，先读取核对。EFS下载按流限制大小；普通HTTP浏览器缺少Clipboard API时有复制回退。

## 回归命令

```sh
npm test --prefix web
python3 tests/web_browser_test.py
python3 tests/web_app_browser_test.py
cargo +1.89.0 fmt --manifest-path rust/Cargo.toml --check
cargo +1.89.0 test --manifest-path rust/Cargo.toml --locked --features web-ui
cargo +1.89.0 build --manifest-path rust/Cargo.toml --locked --features web-ui
python3 tests/web_ui_http_test.py rust/target/debug/zwrt-datad
cargo +1.89.0 clippy --manifest-path rust/Cargo.toml --locked --all-targets --features web-ui -- -D warnings
```

浏览器测试需Playwright/Chromium，使用实际构建产物和本地假后端，所有发送、配置和EFS写入都只发生在内存夹具。CI新增 `web-ui` job，同时保留原纯API测试job。

可选本地差分，不把抓包或私有源包放进Git：

```sh
node tests/web_capture_audit.mjs /path/golden.json.gz /path/cap4g.bin /path/cap5g.bin
python3 tests/web_reference_replay.py /path/reference-root /path/cap4g.bin /path/cap5g.bin --output local/reference-replay.ndjson
node tests/web_reference_diff.mjs local/reference-replay.ndjson /path/cap4g.bin /path/cap5g.bin
```

JVM审计需要本地Gradle缓存中的Kotlin2.3.21与kotlinx-serialization1.6.3，编译原参考解码器及原卡片hook。只替代Android宿主，报告只输出计数和差异路径。现有三份真实抓包74,457帧、439棵RRC树结果及测试边界见覆盖表。

依赖原始许可证随静态资源打包，位于 `/ui/licenses/dependencies`；RRC许可证与来源另置 `/ui/licenses/rrc` 和 `/ui/licenses/notice`。字典和标签可复现导入：

```sh
python3 scripts/import-web-rrc-schema.py /path/reference.zip
python3 scripts/import-web-reference-metadata.py /path/reference.zip --check
```

## 验收边界

自动化回归、交叉编译与设备实测分开。尚未执行真实设备上的短信发送、网络/APN/Wi-Fi/LAN/EFS/NV写入、OTA安装、NMS平台远程链路与长时间运行；现有抓包没有出现的变体不能宣称实包全覆盖。真实TA距离没有仪表/站点位置校准。完整待验收清单见覆盖表末尾。

本轮实际构建、回归计数及产物校验见 [验证记录](WEB_UI_VALIDATION.zh-CN.md)。

## 控件与操作反馈

明暗主题共用输入框、下拉框、按钮和分段控件的状态配色；禁用状态保留可辨识的文字与边框。操作成功、失败及结果未确认通过统一 Mantine Toast 展示，桌面右上、手机顶部居中；支持去重、关闭和自动消失，不占页面布局空间。字段校验、持续故障和数据失效仍在对应区域展示。

## 概览与移动导航布局

首页将当前网络、RSRP、SINR和电量合并成一张分区卡片；流量、热点和SIM概览合并为紧凑2×2分区，与趋势图和运行状态同排。小屏按内容优先级重排，驻网参数使用响应式分列。移动导航使用Mantine Drawer，支持关闭按钮、遮罩、Esc和点击当前页关闭，保留焦点恢复和小屏独立滚动。

弹窗使用细滚动条，文本预览行高1.4，保留原始换行与字节；HEX仅在预览区横向滚动，不撑开弹窗操作区。分页或切换文本/HEX模式时回到预览开头。

## 信令详情的显示调度

打开信令弹窗后，接收、协议解码、上下文合并和原始帧记录继续运行；背景列表和卡片停止发布画面更新，详情以500ms合并更新。退出动画完成后恢复背景最新状态。完整记录仍保持打开时的快照及原有列表暂停行为。复用已生成的列表节点，避免弹窗状态变化时重复构建300行表格；小型跨页摘要随显示批次持久化，避免每个网络块都同步写入浏览器存储。信令只读详情使用遮罩而不模糊重绘整个背景。

日常主页已移除完整状态快照和开发用载波回放入口，真实信令抓包导入和完整记录导出保留。

## 保存登录与项目入口

“记住密码”“自动登录”默认关闭。勾选自动登录会同时勾选记住密码；通过密码认证后，将账号、密码及选项保存到当前浏览器本地存储，按来源与API代理前缀隔离。服务Token和会话Bearer不写入这些记录。可在登录页清除，忘记设备时也清除。主动退出会阻止当前标签页立即自动重登；取消自动登录会中止旧请求，错误密码不会循环尝试。浏览器不允许保存时仍可手动登录，并提示保存失败。

登录页标题为“高级管理控制台”，说明为“面向中兴 ARM64 5G 路由设备的统一数据与控制服务。”登录页及侧栏底部均提供上游GitHub项目卡片；静态链接在新窗口打开，不请求外部接口，不把登录信息放入链接。

## 首页标识显示

首页当前驻网的小区ID与TAC默认模糊，分别点击可显示或隐藏，支持键盘操作。离页后重新进入、切换服务小区或值改变时恢复模糊；普通信号/流量轮询保留当前显示选择。

首页趋势图首次默认速率，切换RSRP/速率后按当前设备管理地址保存浏览器偏好，刷新及离页返回时恢复选择。

实时采集面板默认收起，单个启停按钮、暂停列表、网络模式与包数、RRC/NAS统计、每秒速率常驻。导入/导出/缓存信息展开后操作；导入导出进度与回放来源提示始终可见，折叠不会停止采集。
