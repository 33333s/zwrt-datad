# UFIPanel datad 网页移植覆盖表

参考包：`UFIPanel-datad-Web-reference-20261120-154921.zip`（144 项清单校验通过）。
范围是参考包中 App 已接入的 datad 分支；后端额外能力不自动纳入。本文按入口、操作、参数和解码链路登记实现。实现覆盖与硬件验收分开记录；假后端操作成功不等于已在设备上写入验证。

源码路径缩写：`D` = `source/.../data/zwrt/`，`S` = `D/signal/`，`U` = `source/.../ui/`，`P` = `ui-reference/.../ui/`，`I` = `integration/`。完整路径见资料包 `SOURCE-INDEX.md`。

状态：已实现 = 有业务逻辑、界面和对应流程；测试列区分自动化验证、源码核对与尚未执行的实机操作。无数据和不支持状态来自能力/有效性判断，不用占位数据代替。

## 实施顺序

1. 核对全部参考资料、UI、业务摘录和测试；登记每个可见操作。
2. 共用状态、鉴权、生命周期、业务结果判定、五个主入口和设置导航。
3. 网络模式、频段、邻区扫描、小区锁定及回读/重连流程。
4. 短信、APN、热点、LAN、流量管理与历史。
5. 转发、定时任务、记录、恢复、设备信息、NMS、避让、终端和更新。
6. 信令全部解码器、上下文、卡片、详情与跨页面数据桥接。
7. EFS/NV 逐项对照及边界回归；最后从 App 入口反向验收全部条目。

## 主页面

| 功能 / 子操作 | 参考文件 | 接口、动作与参数 | 网页入口 | 当前覆盖 / 缺口 | 验证 |
|---|---|---|---|---|---|
| 登录、退出、会话过期、代理路径前缀 | D/ZwrtDatadClient、I/SessionRepository | POST auth/login；Bearer；state/capabilities | 登录 | 已实现；Bearer 仅在内存，代际隔离，代理前缀，退出取消任务 | 单测 / 浏览器 / 真实 Rust HTTP |
| 设备、运营商、RAT、SIM、电量与充电 | P/home/HomeScreen、HomeComponents、I/DeviceStateRepository | GET state | 概览 | 已实现；PLMN 优先映射，充电枚举与缺失值保护 | 源码核对 / 浏览器状态夹具 |
| 当前驻网与跳转、LTE 锚点 | P/home、P/params、util/ServingCell | state.net | 概览 / 信号 | 已实现；完整参数、NSA 锚点、详情及锁定带入 | 协议单测 / 浏览器 |
| RSRP 趋势、速率趋势、切换/折叠、均值 | P/home/HomeComponents | state 连续样本 | 概览 | 已实现；双趋势、折叠、60 点及 RSRP 均值，离页停止采样 | 浏览器构建与响应式回归；连续实机采样未验收 |
| 今日/月流量、速率、CPU/内存/温度与详情跳转 | P/home、I/DeviceStateRepository | state | 概览 | 已实现；今日/月用量、速率、资源与各详情跳转 | 浏览器 / 源码核对 |
| PCC、PSCell、全部 SCC、激活状态 | D/ZwrtCarriers、U/signal/ZwrtCarrierCard | state.net 的 LTE/NR 多变体 | 概览 / 信号 | 已实现；最多 32 描述记录，非激活 SCC 仍显示；按用户要求不在日常界面开放开发用JSON回放 | 载波变体单测 / 非聚合 SCC 浏览器 |
| 信号质量、服务小区参数与完整详情 | P/params/ParamsScreen、util/SignalQuality | state | 信号 | 已实现；完整服务参数、加权质量评分、NSA 锚点、手动/当前小区带入 | 协议单测 / 浏览器状态夹具 |
| 邻区分类、排序、详情、锁定标记 | P/params、P/band/CellLockProductScreen | state；neighbor.scan_sa | 信号 / 锁小区 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| SA 扫描、临时切网与恢复原模式 | I/NetworkRepository、P/params | neighbor.scan_sa（后端负责临时切网与恢复） | 信号 | 已实现；只调用 neighbor.scan_sa，临时模式恢复由后端执行，不重复切网 | 调用契约单测；实机扫描未执行 |
| QoS 摘要与全部承载详情 | S/SignalQosBridge、DataBearerQos | NAS / RRC 解码及 state 关联 | 信号 / 信令详情 | 已实现；数据 APN 选择、全部承载/flow 详情、EPS↔5GS、跨页缓存与会话隔离 | NAS/QoS 状态单测 / Worker 卡片 / 实包卡片对拍 |

## 网络模式、频段和小区

| 功能 / 子操作 | 参考文件 | 接口、动作与参数 | 网页入口 | 当前覆盖 / 缺口 | 验证 |
|---|---|---|---|---|---|
| 当前模式、自动/4G/SA/NSA 选择、应用与回读 | P/band/BandScreen、I/NetworkRepository、D/ZwrtDatadProtocol | network.set_mode {mode}；state.net | 频段 / 蜂窝网络 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 支持频段、当前驻网标记、已锁频段读取 | 同上 | state.bandlock、state.net | 频段 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| LTE / NR SA / NR NSA 选择、取消编辑、应用确认 | 同上 | band.set_lte / band.set_nr_sa / band.set_nr_nsa {bands} | 频段 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 解除所有频段锁、重拨、等待上线、回读 | I/NetworkRepository | 三类 bands 空串；cellular.disconnect/connect | 频段 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| LTE/NR 手动 PCI/频点、NR band 校验 | P/band/CellLockProductScreen、I/NetworkRepository | cell.lock_lte {pci,earfcn} / cell.lock_nr {pci,arfcn,band} | 锁小区 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 当前小区/邻区带入、列表暂停/恢复、锁定确认 | 同上 | state / 扫描结果 | 锁小区 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 锁后模式协调、全局解除、回读、可选重启 | 同上 | cell.unlock_all；network.set_mode；device.reboot | 锁小区 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |

## 短信与配置

| 功能 / 子操作 | 参考文件 | 接口、动作与参数 | 网页入口 | 当前覆盖 / 缺口 | 验证 |
|---|---|---|---|---|---|
| 收件箱/发件箱、实际存在的草稿和未知分类 | D/ZwrtSms、P/sms/SmsTabScreen | state.sms.list；tag 0/1 收件、2/3 发件 | 短信 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 刷新、排序、去重、时间串、全文、复制、回复 | 同上、util/SmsDate、I/MoreRepositories | state | 短信 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 新建、UTF-16BE 正文、能力驱动卡槽、结果未知 | P/sms/SmsComposeScreen、I/MoreRepositories | sms.send_raw {sender,number,message_hex,sms_time,ID,encode_type} | 短信 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 删除确认、删除后刷新 | 同上 | sms.delete {ids} | 短信 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| APN 自动/手动、当前档案、自动只读 | D/ZwrtApn、P/network/ApnListScreen、I/ApnRepository | apn.list、apn.set_mode {mode} | 设置 / APN | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| APN 新建/编辑、认证、PDP、凭据保留 | P/network/ApnEditScreen、I/ApnRepository | apn.add / apn.modify | 设置 / APN | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| APN 启用/删除、先重读档案、保存后回读 | 同上 | apn.enable / apn.delete {profile_id} | 设置 / APN | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| Wi-Fi 总开关、双频合一、2.4/5 GHz 开关 | P/tools/HotspotScreen、I/ToolsRepository | wifi.status、wifi.set_module、wifi.set_dual_band、wifi.configure | 设置 / 热点 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 双频 SSID/安全/密码/隐藏/人数/信道 | 同上 | wifi.configure {section,ssid,encryption,key,hidden,maxassoc,channel} | 设置 / 热点 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 访客网络状态与开关 | 同上 | wifi.status；wifi.configure {section,enabled} | 设置 / 热点 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| LAN 网关/掩码、DHCP 池/租期、独立 MTU | D/ZwrtLanUsage、U/more/ZwrtLanScreen | GET/POST lan；POST lan/mtu | 设置 / LAN | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 流量限额开关、GB 输入、今日/月统计与进度 | P/settings/SettingsDetailScreens、I/ToolsRepository | traffic.set_limit {enabled,type:1,value:MB}；state | 设置 / 流量 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 历史日期范围/近7天/30天/本月、日图表/列表 | D/ZwrtLanUsage、U/more/ZwrtUsageScreen | GET traffic/history | 设置 / 历史流量 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |

## 设置子页与设备功能

| 功能 / 子操作 | 参考文件 | 接口、动作与参数 | 网页入口 | 当前覆盖 / 缺口 | 验证 |
|---|---|---|---|---|---|
| 设备信息、状态刷新、重启确认 | P/more/MoreDeviceScreens | state；device.reboot | 设置 / 设备信息 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 蜂窝网络模式、连接状态与重拨 | P/settings/SettingsDetailScreens、I/NetworkRepository | network.set_mode；cellular.disconnect/connect | 设置 / 蜂窝网络 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 转发总开关、SMTP/Webhook/钉钉/SMS 全部配置 | D/ZwrtSmsForward、U/more/ZwrtSmsForwardScreen | sms/forward/config | 设置 / 短信转发 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 黑名单、关键词、备注、附带设备信息、电源通知 | 同上 | 仅发送变化字段 | 设置 / 短信转发 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 转发状态轮询、保存、测试确认与真实投递结果 | 同上 | sms/forward/status、test | 设置 / 短信转发 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 任务列表、新建/编辑/删除、每日/一次、设备时间 | D/ZwrtTasks、U/more/ZwrtTasksScreen | GET/POST tasks；tasks/remove | 设置 / 定时任务 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 每类受支持任务的专属参数与校验 | 同上 | 返回 actions/network_modes 与 buildZwrtTask | 设置 / 定时任务 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 执行记录分类/失败筛选、分页、清空确认 | D/ZwrtAutomation、U/more/ZwrtActivityScreen | activity；activity/clear | 设置 / 执行记录 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 自动恢复配置、状态、检测一次、恢复检测 | D/ZwrtAutomation、U/more/ZwrtRecoveryScreen | network/recovery、check、resume | 设置 / 自动恢复 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| Root Shell 连接/输入/输出、控制键、清空/重连 | D/ZwrtWebShell、ZwrtShellOutput、U/tools/ZwrtShellScreen | 鉴权 WebSocket webshell | 设置 / 终端 | 已实现；xterm、鉴权子协议、二进制输入、resize、超时/队列限制、退出关闭 | 浏览器 PTY 夹具 / 现有 Rust 终端回归 |
| 更新状态/进度/候选说明/签名、检查、安装确认 | D/ZwrtOta、U/more/ZwrtOtaScreen | ota/app、check、install {candidate_id} | 设置 / datad 更新 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 官方/自定义更新源、保存与回读 | 同上 | ota/app/config | 设置 / datad 更新 | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| NMS 开关/远程网页/面板控制、连接状态 | D/ZwrtNms、U/settings/ZwrtNmsScreen | cloud/app/config、status | 设置 / NMS | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| NMS 接入码、服务器/凭据/身份/间隔/CA/备用源 | 同上 | quick-connect；config | 设置 / NMS | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| NMS 网页后台增删改、终端条目保留 | 同上 | config.web_services | 设置 / NMS | 已实现；输入校验、确认、提交及结果回读按对应业务处理 | 协议单测 / 假后端浏览器；实机配置写入未执行 |
| 官方后台避让、时长、重计时、恢复、登录提醒 | D/ZwrtOemAccess、U/settings/ZwrtOemAccessScreen、U/root/ZwrtOemAccessLoginReminder | oem.access.status / set {paused,duration_seconds} | 设置 / 官方后台 | 已实现；配置/重计时/恢复；每会话能力检测和登录提醒 | 浏览器设置操作；提醒按源码实现，实机未验收 |

## EFS / NV

| 功能 / 子操作 | 参考文件 | 接口、动作与参数 | 网页入口 | 当前覆盖 / 缺口 | 验证 |
|---|---|---|---|---|---|
| 五秒风险确认、状态与根目录只读 | D/ZwrtEfs、ZwrtEfsClient、U/more/ZwrtEfsScreen | efs/status | EFS | 已实现；五秒确认、退出复位、根目录与直属条目只读 | 浏览器倒计时 / 路径和写保护单测 |
| 路径导航、筛选、列表分页、详情、HEX/UTF8 分页 | 同上、U/more/EfsComponents | efs/list、read | EFS | 已实现；跳转、名称/大小/时间排序、筛选、50 项分页、权限/链接、UTF8/HEX 4096B 分页 | 浏览器目录/正文 / 分页单测 |
| 上传/下载、编辑、新建目录、移动、删除空目录 | 同上 | efs 对应操作；受保护路径检查 | EFS | 已实现；文本及 HEX、原文件比对、取消保留输入、空目录删除、未知结果两分钟写冷却 | 假后端编辑、失效比对与取消 / 写保护单测；实机写入未执行 |
| IMEI/NV 任意项只读、失败转码保留 HEX | D/ZwrtNvRead、U/more/EfsComponents | nvread | NV | 已实现；精确半字节序列化，转码失败保留 HEX；上传独立确认 | 单测 / 浏览器查询和生成；实机 NV/上传未执行 |
| IMEI 校验/序列化、INI 预览/下载/确认上传 | D/ZwrtNvConfig、U/more/EfsComponents | efs/write /nvefs | NV | 已实现；精确半字节序列化，转码失败保留 HEX；上传独立确认 | 单测 / 浏览器查询和生成；实机 NV/上传未执行 |

## 信令解析（每项均需真实结果与详情）

| 功能 / 子操作 | 参考文件 | 协议 / 状态来源 | 网页入口 | 当前覆盖 / 缺口 | 验证 |
|---|---|---|---|---|---|
| 鉴权流、增量拆帧、包头/长度校验、停止清理 | S/AuthenticatedSignalStream、SignalCaptureEngine | signal/stream | 信令分析 | 已实现；Bearer、增量帧校验、断流重连、停止释放，绝不回退裸端口 | 所有拆分边界单测 / 浏览器真实 Worker 流与停止 / Rust HTTP 鉴权 |
| RRC UPER 完整树、详情、保留尾部数据 | S/RrcUperDecoder、RrcSchemaData、U/signal/RrcTreeDialog | B821/B0C0 | 信令分析 | 已实现；完整 schema、扩展/截断容错、虚拟树、原始记录 | 439 树逐字段对拍；浏览器 Worker |
| NR ML1 全部已接入布局及字段 | S/Ml1Decoder、SignalDecoder | B97F/B8C9/B8D8/B881/B888/B8A1/B884/B8D1/B883/B887/B890/B8DD/B823 | 信令分析 | 已实现；App 所有活动方法与长度/版本/数值约束，未匹配返回空 | 三份实包 JVM 对拍（见验证口径）；未出现的变体仅源码核对 |
| LTE 全部已接入布局及字段 | S/Ml1Decoder、LteSignalDecoder | B111/B114/B11B/B11D/B123/B12A/B134/B193/B1DC/B0C1/B0C2/B16E/B16F/B173 | 信令分析 | 已实现；App 所有活动 LTE 布局，保留推导证据标签 | 三份实包 JVM 对拍；长度/截断/噪声回归 |
| 明文 NAS、EPS/5GS 承载、QCI/5QI、AMBR/IP | S/NasDecoder、LteSignalDecoder、DataBearerQos | B800/B801/B80A/B80B/B0E2/B0E3/B0EC/B0ED；嵌入 NAS | 信令分析 | 已实现；5GMM/SM、EMM/ESM、DNN/APN、QoS flow、EPS 映射、AMBR/IP；密文不猜 | 实包含 2 个 NAS；另有构造 NAS 与 QoS 生命周期单测 |
| PLMN 选择/共享网络、TAC、安全算法、厂商 | S/RrcPlmn、VendorFingerprint、SignalDecoder | 已解码证据；不能把首广播运营商当驻网 | 信令分析 | 已实现；所选 PLMN、共享网络、安全算法、LTE/NR 厂商证据 | JVM 实包卡片对拍；推断保留证据与置信度 |
| 服务小区、测量与历史邻区区分、模三、SSB 波束 | S/CellMeasurementSnapshot、SignalDetailRows、U/signal/ServingCellContent、SsbBeamTable | 当前上下文与同一次测量 | 信令分析 | 已实现；测量快照/历史邻区区分、同次测量模三、SSB 稳定排序与详情 | 实包解码对拍 / 渲染源码核对；当前设备射频状态未验收 |
| 工程参数分组、NR/LTE SIB、高铁识别 | S/CellEngineering、HighSpeedRail、U/signal/CellEngineeringContent | RRC schema 树 | 信令分析 | 已实现；完整工程标签/排除表、搜索分组、来源、高铁 NR SIB1 规则 | JVM 实包 9 次广播工程状态对比；标签重生成校验 |
| LTE 测量对象→测量报告→频道关联 | S/LteMeasurementContext | RRC 测量配置及释放 | 信令分析 | 已实现；增改/移除/释放、measId 与频道关联；无关联不填频道 | 上下文生命周期单测 / 实包解码 |
| BLER/MCS、RB/grants/layers、CSI-RS、Tx/Rx | S/SignalCardUpdates、NrLinkConfig、SignalDetailRows | 结构校验后的 ML1 与 RRC | 信令分析 | 已实现；NR 固定行、LTE 紧凑详情，保留累计/本包口径；缺失不填0 | 实包 JVM 卡片对拍 / 浏览器真实 Worker 解析卡片 |
| PUSCH 主卡、PUSCH/PUCCH 独立详情与历史 | S/NrUplinkPower、SignalCardUpdates | B884/B8D1 同时隙关联；LTE 独立功率 | 信令分析 | 已实现；时戳/时隙/请求功率交叉关联，PUSCH/PUCCH 分开，历史与来源 | 实包状态对拍 / 模糊关联拒绝单测；非仪表测功 |
| 自动 TA 距离、FDD/TDD/NR 规则与来源 | S/AutomaticTimingAdvance、TimingAdvanceDistance、U/signal/TimingAdvanceDistanceDialog | B062/B88A | 信令分析 | 已实现；RAR 自动值、LTE/NR/numerology 与 FDD/TDD 偏移、手工核对输入 | 单位/偏移边界单测；实机距离未测量验证 |
| QoS 数据 APN 选择、全部会话详情、跨页桥接 | S/DataBearerQos、SignalQosBridge、SignalCardStore | 注册制式与承载关联、失效清理 | 信令分析 / 信号 | 已实现；数据 APN 选择、全部承载/flow 详情、EPS↔5GS、跨页缓存与会话隔离 | NAS/QoS 状态单测 / Worker 卡片 / 实包卡片对拍 |
| 筛选/暂停/清空、完整 NDJSON、导出进度/记录 | S/SignalEventJson、SignalExportWriter、NdjsonEmitter、U/signal/SignalCaptureScreen | 保留原包及解码证据 | 信令分析 | 已实现；列表/完整 NDJSON 虚拟视图，独立原包缓存，BIN/JSON/NDJSON 导出进度 | 浏览器导入、解析、列表暂停、全帧 NDJSON 导出 |
| 换网/换小区/断流/切设备清理、时间与时效 | S/SignalCaptureEngine、SignalCardUpdates | 会话代际、RAT、PCI、ARFCN | 全部信令展示 | 已实现；RAT/小区/会话与来源隔离，重连重置，回放不混入实时状态 | 状态单测 / 两个浏览器回归；长时间实机断流未验收 |

## 可编辑字段与独立操作复核

下表补充容易被“页面完成”掩盖的子操作；每组均有初始化、校验、独立提交状态、错误与回读路径。

| 网页文件 | 逐项核对内容 | 参数/保护 |
|---|---|---|
| `connection-panel.jsx` | 编辑连接名称、查看地址、历史连接、切换、忘记、退出 | 浏览器本地记录无密码/Token；切换目标重新登录，保留代理前缀 |
| `wifi-page.jsx` | 总开关、双频合一、2.4/5 GHz 独立开关；SSID、安全、密码、信道、隐藏、人数、取消；两组访客开关 | `section`、`enabled`；`ssid/encryption/key/channel/hidden/maxassoc`；空密码保留；访客 SSID/安全只读 |
| `apn-page.jsx` | 自动/手动、自动只读、全部手动档案；名称、APN、PDP、认证、用户名、密码；新建、编辑、启用、删除、恢复自动 | 原 `profile_id`；新建不启用；空编辑凭据保留；保存前重读，保存后核对档案字段 |
| `settings-pages.jsx` | LAN 网关/掩码、DHCP 开关/池/租期、独立 MTU；流量限额开关/GB；历史日期及三种预设 | 网段/地址/租期校验；`verified` 分支；保留另一节未保存输入；400 日范围，缺日不补零 |
| `forward-page.jsx` | 总开关、渠道选择；SMTP 域名/端口/发件邮箱/授权码/收件邮箱/清除；Webhook；钉钉地址/密钥/清除；短信收件列表；各渠道设备信息 | HTTPS 与号码/长度校验；密码留空保留；只提交变化字段；测试请求不重发 |
| `forward-page.jsx` | 电源通知、号码黑名单、关键词、设备备注；状态、最近结果、测试 | 后端提供的电池能力和真实剩余额度按条件显示，不在 Web 自建额度 |
| `automation-pages.jsx` | 任务名称、设备时间+5分钟默认、一次/每日；编辑、删除；状态与执行日期 | 名称80 UTF8字节、短信280 UTF16单元、设备 `actions/network_modes` 限定 |
| `automation-models.js` | 重启、关机、连接、断开、网络模式、SIM、漫游、Wi-Fi、访客、NFC、短信、设备信息、LTE/NR锁定、全解锁共15类 | `slot/roaming/enabled` 整数，NFC布尔，访客 `guest_enabled:0`，小区字段字符串；不会添加单小区解除 |
| `automation-pages.jsx` | 记录分类/失败/分页/清空；恢复开关/间隔/失败门限/冷却/拨号次数/可选重启；检测一次、恢复检测、跳转记录 | requested 与 success 区分；设备限频语义保留；保存回读核对 |
| `nms-page.jsx` | 总开关、远程网页、面板控制；MQTT/平台/用户名/密码/清除、间隔；厂商/型号/标识/类型/平台；CA、备用地址 | `web_services` 仅网页，端口校验和防重；保留终端条目；接入码不泄露凭据 |
| `nms-page.jsx` | 网页后台新增/修改/移除、接入码导入、保存、状态、最近上报、未保存提示 | 局部编辑先校验条目、整页保存再校验完整配置；空密码保留 |
| `device-tools.jsx` | OTA 官方/自定义/优先源、来源目录、保存、自动更新开关、手动检查、签名状态/候选/说明/等待原因/进度、确认安装 | 使用既有签名机制；候选ID校验；更新策略沿用后端，自动更新开关按能力标志禁用 |
| `device-tools.jsx` | OEM 暂停开关、5/10/30分钟/手动、重新计时、恢复、刷新；终端重连/清空/Ctrl+C/Ctrl+D/回车 | 设备状态决定避让结束；终端鉴权、初始化、resize、输入/输出背压、退出释放 |
| `oem-reminder.jsx` | 每登录会话提示、忽略、立即关闭避让、失败回读 | 能力检测、串行请求；退出登录取消，忽略不修改设备 |
| `efs-pages.jsx` | 路径/排序/分页/属性、文本/HEX、下载/上传、mkdir、rename、file/empty-dir delete、IMEI/NV/INI | 根及直属只读、完整后端保护、类型/硬链接、原文件比对、未知结果写冷却；没有递归删除 |

## 验证口径与排除项

自动化验证分层：

- `web_unit_test.mjs`：通用请求、帧、EFS、UPER 边界。
- `web_app_ports_test.mjs`：短信、APN、载波、LAN/日期、全部任务参数、转发/NMS 和网络操作序列。
- `web_signal_state_test.mjs`：NAS、QoS 切网/释放/QFI、测量关联、CSI-RS 释放、TA、RF 功率关联、缓存隔离、畸形包及下载边界。
- `web_browser_test.py`：生产资源、明暗/桌面/手机、EFS 风险/编辑/比对/取消、NV、Worker。
- `web_app_browser_test.py`：完整设置操作链路、HTTP 200 业务失败/结果未知不重发、真实 Worker 卡片/导出/停止、PTY 释放。
- `web_ui_http_test.py`：真实 Rust 静态资源 allowlist、压缩、路径、开关、API 鉴权。
- JVM 差分：未修改参考纯解码器与原卡片 hook；三份本地抓包共 **74,457 帧**，**32,541** 组解析模型、**74,457** 次卡片比较和 **9** 次工程参数变化，**0 差异**。NAS 实包只含 **2** 个，不能据此声称覆盖所有 NAS 变体。功率关联内部缓存、测量关联内部表等未作为差分输出，另由单测核对。
- RRC **439** 棵参考树逐字段一致。差分仅统一 Kotlin/JS 整数末尾 `.0` 的显示差别，不忽略字段值。
- 最终构建、Rust/Node 数量和实际产物见 `WEB_UI.md` 及 `build/reports/web-*.log`。本地抓包、JVM 中间文件和个人设备数据不提交。

不纳入：App 原生激活/签名保护/桌面组件/原生应用更新/壁纸触感；App datad 分支不可达的普通文件/系统时间/Hosts 等后端额外能力。开发用载波模拟/JSON回放与完整状态快照按用户要求不在日常页面提供；真实信令抓包导入保留且不混入实时状态。

已完成按专属页面、共享页 datad 投影、integration 方法与活动解码路由的反向核对。没有用后端额外接口扩大范围。

尚未完成/未验证：未在真实设备发送短信、写入 APN/频段/小区/Wi-Fi/LAN/EFS/NV、执行重启/OTA/NMS 远程链路；未测量真实 TA 距离或长时间运行内存。各机型/未在现有实包出现的布局仍需设备验收。ARMv7 交叉构建不等于已部署运行。原生测速等被参考投影明确标作 `native App entry omitted` 的入口不纳入；CA 固定模拟场景仅为 App DEBUG 代码，Web 日常页面不提供该开发入口，不放固定演示数据进实时卡片。


默认开启 Web 版本已部署到 MU5120，真实页面读取、终端鉴权握手、231帧信令接收及停止通过；根分区维持只读、原启动参数和自启保留。完整摘要见 `WEB_UI_VALIDATION.zh-CN.md` 实机补充，配置写入与其他未验收项仍保留。
