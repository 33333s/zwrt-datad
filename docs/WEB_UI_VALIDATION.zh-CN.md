# Web 移植验证结果（2026-11-20）

以下为原 Web 移植分支的历史验证记录，包含本地定制构建和实机记录；这些产物不作为上游版本发布件。上游整理分支沿用 `version.json` 和原 OTA/NMS 更新机制，当前提交的验证结果以对应 Actions 为准。范围和逐操作入口见 [覆盖表](WEB_APP_COVERAGE.zh-CN.md)，构建/启动见 [Web说明](WEB_UI.md)。未修改 App 仓库；Web 产物经用户确认已整套同步到 `deploy_kano`，见末节。

| 检查 | 结果 | 本地证据 |
|---|---|---|
| 前端生产构建 | 通过；53个内嵌gzip资源，无CDN | `build/reports/web-build.log` |
| Node单测 | 38通过、0失败（含通知和登录选项隔离） | `build/reports/web-login-unit.log` |
| Rust（web-ui feature） | 254通过、0失败 | `build/reports/web-default-on-tests.log` |
| Rust（默认纯API feature集） | 254通过、0失败 | `build/reports/web-default-on-pure-tests.log` |
| Clippy | feature与默认均 `-D warnings` 通过 | `web-default-on-clippy.log` / `web-default-on-pure-clippy.log` |
| 实际Rust HTTP内嵌/鉴权 | 通过；开关、所有资源、gzip、路径、现有鉴权 | `build/reports/web-default-on-http.log` |
| EFS/基础浏览器回归 | 通过；风险确认、目录/分页、HEX编辑、原文件比对、取消/后退保留与放弃、NV、明暗/手机 | `build/reports/web-browser.log` |
| App业务浏览器回归 | 通过；短信业务失败/未知不重放、APN、模式/频段/小区、WiFi/LAN、任务/记录/恢复、转发/NMS/OEM/OTA、PTY、历史、信令Worker真实解析卡片/全帧导出/停止 | `build/reports/web-app-browser.log` |
| 原有客户端API兼容 | 9组全部通过：U50、信令鉴权、终端、EFS、LAN/CORS、短信、NMS、OTA、WiFi | `build/reports/web-api-compat.log` |
| RRC参考树 | 439匹配，0拒绝，0差异 | `build/reports/web-capture-audit.log` |
| 两份基准抓包JVM对拍 | 35,677帧，13,255组解析模型，2条NAS，35,677次卡片比较，5次工程变化；0差异 | `build/reports/web-reference-base-diff.log` |
| 另一份本地抓包JVM对拍 | 38,780帧，19,286组解析模型，0条NAS，38,780次卡片比较，4次工程变化；0差异 | `build/reports/web-reference-qos-diff.log` |
| 字典/标签 | RRC原SHA与许可保留；NAS/代码名/工参标签重生成一致 | `scripts/import-web-*.py` |
| ARMv7静态交叉构建 | 成功；ARM EABI5、静态、stripped | `build/reports/web-arm-build.log` |

对拍只统一 Kotlin/JS 整数末尾 `.0` 的显示差别，不忽略字段值。它证明与参考实现一致，不意味着把原 App 标注“推导”的字段升级为射频实测结论。未出现的布局仍需补实包；NAS实包较少，额外使用独立构造消息验证QoS切网/释放/关联。内部射频/测量关联缓存未作为JVM报告输出，另有状态单测。

所有浏览器写操作均针对本地内存假后端。9组API兼容测试也使用独立临时目录及假设备端点，未操作真实基带。CI新增Web job，远端执行状态以对应提交的Actions记录为准。

## 可交付产物

- 文件：`build/web-ui/armv7/zwrt-datad-armv7-web`
- 大小：6313320字节
- SHA256：`5ab0d265e19d7df8798f17e4b731e7319dece4d07bfdd32e63167c532499a437`
- 校验文件：同目录 `SHA256SUMS`
- 保留既有启动参数默认开启 Web；用 `--disable-web-ui` 禁用，访问 `/ui/`；见构建说明。

ARM构建有两项既有 `libc::time_t` / `suseconds_t` 弃用警告；构建成功，主机Clippy无警告。此处产物使用本地定制版本与更新策略；上游提交不包含这些定制。

## 尚未验收

未在真实设备执行短信发送、APN/频段/小区/WiFi/LAN配置、EFS/NV写入、重启和OTA安装；NMS平台远程链路、TA实际距离和长时间内存/断流行为未做设备验收。ARM交叉构建不等于已在机运行。各项在覆盖表中保留这些限制，没有把入口、原始JSON或固定演示值当成实现。

## 默认开启与实机部署补充

- CLI 改为默认在既有鉴权 LAN listener 开启网页；`--disable-web-ui` 关闭。无鉴权的本机 API listener 不提供网页；纯 API feature 集保持不变。
- 本轮 Web feature Rust 回归 **254通过、0失败**；HTTP 验证覆盖默认开启、显式禁用、旧的本机监听启动、路径/压缩/原API鉴权；Clippy `-D warnings` 通过。
- 新 ARMv7 二进制 SHA256：`6a5919ee54985afb684b3f4cd816223483eba83a91aaec01b0cc4a3c6f928771`；大小 6308480 字节，校验文件已同步。
- 已部署到连接的 MU5120。仅替换 `/cache/zwrt-datad/zwrt-datad`，保留原服务参数和开机自启；稳定健康10秒，安装与运行哈希一致，根分区保持只读。
- 实机浏览器管理密码登录成功；概览、信号、频段、小区入口、短信、APN、热点、LAN、历史、NMS、更新等读取页面正常。终端通过带鉴权握手；信令接收 **231帧、15条RRC**，展示9张卡片，停止成功；移动端无横向溢出、无CSP/JS异常。
- 原 `/` 横幅保留，`/ui/` 返回网页；未登录的状态、控制、EFS、信令继续拒绝访问；授权后原状态及管理读取接口返回200。
- 此次仍未执行短信发送、锁频/锁小区、网络/APN/Wi-Fi/LAN修改、EFS/NV写入、OTA或NMS远程链路。早先“未实机部署”的记录已由本补充更新，其他待验收项保留。

实机自动化报告：`build/reports/web-live-device-smoke.json`（只保存非敏感摘要，不含口令、Token、短信正文或SIM身份）。

## 控件配色与统一 Toast 回归

- 完成明暗主题禁用色和默认控件配色统一，Select 不再使用原先的暗灰背景。
- 页面、弹窗和 EFS/NV 操作结果统一进入 Mantine 通知管理器；移除重复的页面结果条，保留字段校验及持续状态提示。
- 34项 Node 单测通过；两组生产包浏览器回归通过。新增检查覆盖结果去重、自动消失、通知层不阻挡登录、手机定位、操作不挤动页面、明暗主题实际计算颜色及禁用状态。浏览器写操作仍只运行于内存假后端。
- 报告：`build/reports/web-toast-unit.log`、`web-toast-browser.log`、`web-toast-app-browser.log`；视觉截图：`web-toast-light.png`、`web-toast-dark.png`、`web-toast-mobile.png`。

- 本轮 ARMv7 与宿主机编译、真实 Rust HTTP 静态资源/鉴权验证通过（`web-toast-arm.log`、`web-toast-rust-build.log`、`web-toast-http.log`）。最新产物 SHA256：`e2f9a0687adf0b149858f9cb1802186509c51ba81aad2382047f0dd906369091`；大小 6309376 字节。

- 已将本轮产物部署至 MU5120；服务保持开机自启、根分区只读、原启动参数及鉴权不变，健康检查通过。实机读页面与终端握手正常；本次信令接收7590帧、29条RRC、9张解析卡片。明暗控件颜色、完整NDJSON复制后的浮动Toast、关闭通知及手机无横向溢出均验证通过；无JS/CSP错误（`web-live-device-smoke.json`）。未执行基带/网络配置写入。

## 短信卡槽展示修正

- 移除短信列表与详情中写死的卡槽提示；仅明确双卡时提示历史短信未记录卡槽，单卡不显示，避免把缺少来源字段误呈现为设备故障。只有能力允许多卡发送时才显示发送卡槽选择。
- 34项单测、App业务浏览器单/双卡场景、宿主机与ARMv7编译、HTTP资源及鉴权回归通过。报告：`web-sms-slot-unit.log`、`web-sms-slot-browser.log`、`web-sms-slot-arm.log`、`web-sms-slot-http.log`。
- 已部署MU5120并验证列表、详情及新建短信无卡槽提示/选择，无JS异常。实机仅打开与取消新建，未发送或删除短信。报告：`web-sms-slot-live.json`。
- 最新产物SHA256：`2e890e1cbbcc4d161b4ce4fa67cda52a1faf4a5fbc264123b99e3c0b2f8c0f71`；大小6309376字节，`SHA256SUMS`已同步。

## 概览密度、移动导航与弹窗预览

- 网络指标合并为分区卡片；四项流量/连接信息移至趋势图右侧，去掉页面下方原四张大卡；驻网信息加密分列。
- 移动导航采用Mantine Drawer，四种关闭路径均有浏览器回归：关闭按钮、遮罩、Esc及当前页。覆盖焦点恢复、320×568小屏侧栏滚动及320/390/900/1100/1360/1600宽度检查。
- 弹窗使用统一细滚动条和较紧行距；长HEX的水平滚动限制在预览区内，避免将下载/翻页/关闭按钮挤出屏幕。文本预览内容及换行逐字匹配夹具，模式切换重置滚动位置。
- 34项单测、两组生产包浏览器回归通过。报告：`web-overview-layout-unit.log`、`web-overview-layout-browser.log`、`web-overview-layout-app-browser.log`。新EFS预览回归及写操作均针对内存假后端，未对真实基带执行写入。
- 截图：`web-overview-layout-dark.png`、`web-overview-layout-mobile.png`、`web-navigation-small-mobile.png`、`web-efs-preview-text.png`、`web-efs-preview-hex.png`、`web-efs-preview-mobile.png`。

- ARMv7与宿主机构建、真实Rust HTTP资源/开关/鉴权回归通过；已部署MU5120并通过10秒健康检查。实机确认桌面首页合并分区及排列，390px侧栏四种关闭方式/焦点恢复、320×568侧栏滚动正常，无JS异常（`web-overview-layout-live.json`）。本轮真实设备只读查看，未执行EFS/NV写入或网络配置。
- 最新产物SHA256：`1df18afa83c15484d762e13bbcec331a38068e593c4c874646d873dac16a4836`；大小6311552字节；原服务参数、开机自启和根分区只读状态保留。

## 信令弹窗性能与开发入口清理

- 弹窗存续期间（含退出动画）暂停背景列表及卡片的画面发布，接收、协议解码、上下文清理和原始帧缓存继续。详情按500ms发布最新状态；关闭后恢复背景。原完整记录快照/手动暂停语义不变。
- 列表节点与筛选结果复用，避免弹窗状态变更重建300行；跨页摘要持久化合并到显示批次。信令只读详情取消全背景模糊，保留遮罩。
- 主页移除完整状态快照；概览/信号页载波移除开发用JSON回放及其文件入口。真实信令BIN导入、完整记录和导出保留。
- 34项单测、两组生产包浏览器回归、ARMv7/宿主机构建、Rust HTTP鉴权与资源检查通过。新高流量浏览器回归验证弹窗内背景DOM不变、详情继续更新、关闭后计数追上，以及24MiB缓存窗口内导出帧数等于已采集计数。报告：`web-signal-modal-unit.log`、`web-signal-modal-browser.log`、`web-signal-modal-base-browser.log`、`web-signal-modal-arm.log`、`web-signal-modal-http.log`。
- 已部署MU5120，保持原服务参数与根分区只读，健康检查10秒通过。连接实机的桌面Chromium各取6秒功率详情采样：背景DOM变动8258→0；长任务14→0；帧间隔P95约100→16.7ms。此为本机这两次样本，未宣称覆盖所有浏览器或相同射频流量。新样本详情仍发生13次DOM更新，无JS错误。报告：`web-signal-perf-before.json` / `web-signal-perf-after.json`。
- 最新产物SHA256：`a34e29278314247c2e43ef01d66d0f0c600e98089ae80cefdedf682919c2b6b1`；大小6310912字节，校验文件同步。

## 登录选项、文案与GitHub项目卡片

- 记住密码/自动登录默认关闭，自动登录依赖保存密码。密码认证成功才写入浏览器本地记录，按来源/API前缀隔离，不存服务Token或Bearer。可清除，忘记设备同步清除；主动退出阻止本标签页自动重登。
- 登录标题/说明按用户给定文本替换；登录页、桌面侧栏与移动抽屉提供GitHub上游项目卡片，使用独立新窗口与noopener/noreferrer，不向GitHub发送设备凭据。
- 38项单测及两组浏览器回归通过。新增覆盖默认不保存、错误密码不保存、记住密码、刷新自动登录、退出后刷新不重登、记住但不自动、取消慢请求、错误自动登录只尝试一次、清除记录、服务Token不落盘、存储失败和跨地址/代理隔离。报告：`web-login-unit.log`、`web-login-browser.log`、`web-login-app-browser.log`。
- ARMv7/宿主机构建与Rust静态资源/开关/鉴权检查通过（`web-login-arm.log`、`web-login-http.log`）。已部署MU5120并通过10秒健康检查，原服务参数、开机自启、根分区只读保留。实机验证登录、记住/刷新自动登录、主动退出、清除保存记录、GitHub卡片及手机无横向溢出，无JS异常；测试浏览器记录已清除（`web-login-live.json`）。
- 最新产物SHA256：`e1736dce36148c97fe18eff94d5bd5c0a7e8644ba39d2f6d79118191730d9d94`；大小6312808字节，校验文件同步。

## 首页小区ID与TAC默认模糊

- 首页当前驻网两项标识默认blur，分别点击/键盘切换显示，再次点击可隐藏；值改变、换小区及离页重新进入时恢复默认。显示时保持超大整数原值。
- 38项单测、两组浏览器回归、ARMv7/宿主构建及HTTP鉴权/资源回归通过；新增覆盖默认样式、独立显示、键盘切换、变更小区和重新进入页面后的恢复。报告：`web-private-cell-unit.log`、`web-private-cell-browser.log`、`web-private-cell-app-browser.log`、`web-private-cell-arm.log`、`web-private-cell-http.log`。
- 已部署MU5120并通过10秒健康检查；实机只读验证默认模糊、独立显示、再次隐藏、键盘操作及离页返回，无JS异常（`web-private-cell-live.json`）。原服务参数及根分区只读保留。
- 最新产物SHA256：`d28df8b98f2dad86def0e0457b96eca9ddb114b6a1023a79c8fa8f796c1af018`；大小6313064字节，校验文件同步。

## 趋势选项记忆与采集面板收起

- 首页首次默认速率，按设备管理地址保存RSRP/速率选择，离页返回和刷新恢复；浏览器回归及连接实机验证通过（`web-trend-preference-live.json`，对应构建和回归日志`web-trend-preference-*`）。
- 实时采集默认收起，常驻启停/暂停列表、包数/分类计数/每秒速率；导入/导出/网络模式/缓存详情展开后显示，操作进度和文件回放来源保持可见。折叠不停止接收或解码，停止后显示0/s。
- 38项单测、两组浏览器、ARMv7/宿主构建及HTTP静态资源/鉴权验证通过；覆盖默认收起、紧凑工具操作、展开导入导出、再次收起和移动端。报告：`web-capture-collapse-unit.log`、`web-capture-collapse-browser.log`、`web-capture-collapse-base-browser.log`、`web-capture-collapse-arm.log`、`web-capture-collapse-http.log`。
- 已部署MU5120并通过10秒健康检查；实机验证默认收起、展开/收起、折叠中继续接收、基本按钮、停止速率归零、手机紧凑布局，无JS异常（`web-capture-collapse-live.json`）。服务参数与根分区只读保留。
- 最新产物SHA256：`5ed04a404658829a79e427e8fceed4d36e6c543985fab3e44a8a4d502b06a77c`；大小6313320字节，校验文件同步。

## 采集启停合并与常驻网络模式

- 开始与停止合并为同一固定宽度按钮，随真实采集状态切换文字/图标；网络模式选择移到常驻摘要区，收起时也可选择4G/5G，原确认/提交/回读流程及回放时禁用规则保留。
- 38项单测、两组浏览器、ARMv7/宿主构建和HTTP资源鉴权检查通过。假后端回归执行收起状态4G→5G的确认和状态回读，并验证单按钮、停止归零及手机布局。报告：`web-capture-controls-unit.log`、`web-capture-controls-browser.log`、`web-capture-controls-base-browser.log`、`web-capture-controls-arm.log`、`web-capture-controls-http.log`。
- 已部署MU5120并通过10秒健康检查；实机验证单按钮启停、收起时模式入口和4G/5G选项、收起/展开、暂停列表、继续采集及手机布局，无JS异常（`web-capture-controls-live.json`）。实机仅打开模式选项后取消，未提交切网；切网写操作在假后端验证。
- 最新产物SHA256：`5ab0d265e19d7df8798f17e4b731e7319dece4d07bfdd32e63167c532499a437`；大小6313320字节，校验文件同步。

## 同步 deploy_kano 交付包

- 已使用 `scripts/build-arm32-web.sh --candidate` 构建并同步配套provenance；与当前MU5120运行的SHA256一致：`5ab0d265e19d7df8798f17e4b731e7319dece4d07bfdd32e63167c532499a437`，6313320字节，本地定制版本，feature为web-ui。元数据明确source_dirty=true，没有将未提交的Web代码宣称为干净HEAD。
- 使用本机整包入口 `build/refresh-deploy-package.py --require-web` 更新Shell安装器、tiny引导及6条英文粘贴命令、Go安装器（UPX及未压缩备份）、离线ADB目录与本地版本 ZIP；保留本地下载地址。原交付包已本地归档，旧0.10.71 ZIP移出交付目录。
- 全部SHA清单、各二进制副本、bootstrap固定哈希、Go生成的releaseSpec和ZIP内payload一致；ZIP CRC完整，说明包含/ui/入口，粘贴命令最长513字符。报告：`build/deploy_kano/package-verification.json`、`build/reports/web-deploy-package-verification.json`。
- 安装事务故障注入24项、下载流程13项、旧ARM32安装器交换/校验/回滚用例、Go安装器测试通过（`web-package-deploy-tests.log`、`web-package-download-tests.log`、`web-package-installer-tests.log`、`web-package-go-tests.log`）。UPX自检通过；本轮打包未重新执行真实设备安装事务，未上传服务器或发布签名更新。
