# ARM32 编译与 deploy_kano 打包流程

本文记录当前 U50 系列 ARM32 后端的本地编译、打包和交付流程。统一交付目录是 **`build/deploy_kano/`**。

编译成功、打包成功、设备部署成功是三个独立状态。生成 candidate 不代表 `deploy_kano` 已更新；生成部署包也不代表设备已经更新。

## 1. 工作目录与产物归属

本机源码仓库为 `D:\Git\zwrt-datad`，当前开发分支为 `arm32`；WSL 对应路径为 `/mnt/d/Git/zwrt-datad`。
从这份源码构建，不使用 WSL 家目录下可能残留的另一份 `~/zwrt-datad`。

| 路径 | 用途 | 清理原则 |
| --- | --- | --- |
| `rust/`、`scripts/`、`tools/`、`tests/`、`docs/` | 源码、模板、测试、文档 | 保留，包括尚未提交及暂存的改动 |
| `rust/target/` | Cargo 编译输出和缓存 | 可在无构建进程时删除，下次重新编译 |
| `build/arm32-toolchain/` | ARM32 构建脚本生成的编译器包装脚本 | 可重新生成 |
| `build/u50-diag-worker`、`build/u50-diag-streamer`、`build/efs-worker-armv7` | 嵌入主程序的辅助进程 | 构建中间产物，不单独作为整套更新交付 |
| `build/zwrt-datad-armv7-candidate` | 本次编译出的主程序 | 打包输入，不是已更新的部署包 |
| `build/rust-release-provenance-armv7.json` | candidate 的版本、提交号、目标和 SHA256 | 必须与 candidate 配套 |
| **`build/deploy_kano/`** | **统一部署交付目录** | **更新时整套重新生成，不当作缓存清理** |
| `build/u50pro-go-installer/` | Go 安装器的生成源码和内嵌资源 | 可重新生成 |
| `build/reports/` | 构建、测试及核验日志 | 集中存放，避免在 `build/` 根目录堆日志 |
| `build/sigcap/`、`build/sigweb*`、其他逆向参考目录 | 真实抓包和参考资料 | 不能按编译垃圾删除 |
| `local/workspace-archive/` | 历史补丁、试验源码和清理记录 | 本地归档，不作为打包输入 |

`build/`、`rust/target/`、`local/` 被 Git 忽略。不能用“未跟踪”作为判断文件是否可以删除的依据。

## 2. 构建前检查

在 Windows PowerShell 中进入仓库，确认分支和改动：

```powershell
Set-Location D:\Git\zwrt-datad
git branch --show-current
git status --short
```

日常本地验证可以编译未提交的工作区，但必须知道本次包含哪些改动，不要覆盖其他人的暂存内容。
`rust-release-provenance-armv7.json` 的 `commit` 仅记录 HEAD，**不记录未提交源码的完整状态**，不能据此声称候选包来自干净提交。

本机需要：

- WSL Linux：Bash、Python 3、`file`、`shasum`、Rust/rustup，以及构建脚本指定的 **Rust 1.89.0**。
- Linux Zig；本机使用 `/home/kano/zig/zig`，不是 Windows `zig.exe`。
- Windows Python 3，用于现有本地整包刷新入口。
- WSL Go 1.25+、公共 CA 证书包；生成压缩 Go 安装器时还需要 UPX。
- Android SDK platform-tools。现有本地刷新入口使用 `D:\Android\Sdk\platform-tools`，ADB 包会复制其中的 ADB、DLL 和 NOTICE 文件。
- tiny 下载器，以及当前本地部署目录的地址配置，见第 4 节。

## 3. 编译 ARM32 后端

统一使用 [build-arm32-candidate.sh](../scripts/build-arm32-candidate.sh)，不要只执行普通 `cargo build` 后把主机版本拿去部署。

PowerShell：

```powershell
Set-Location D:\Git\zwrt-datad
New-Item -ItemType Directory -Force build/reports | Out-Null
wsl bash -lc 'cd /mnt/d/Git/zwrt-datad && ZIG=/home/kano/zig/zig bash scripts/build-arm32-candidate.sh > build/reports/arm32-build.log 2>&1'
if ($LASTEXITCODE -ne 0) { throw 'ARM32 build failed; inspect build/reports/arm32-build.log' }
Get-Content build/reports/arm32-build.log -Tail 12
```

脚本依次执行：

1. 生成 ARM32 编译器包装脚本。
2. 编译 DIAG worker、信令 streamer 和 EFS worker，并通过环境变量供 `rust/build.rs` 嵌入主程序。
3. 用 Rust 1.89.0 构建 `armv7-unknown-linux-musleabihf` release 主程序。
4. 检查主程序为 **ELF32 ARM、静态链接、已 strip**。
5. 生成 candidate 和配套构建信息。

主程序为静态二进制，EFS worker 是依赖设备厂商库的独立动态进程；两者的链接方式不同是正常的。

版本以根目录 [version.json](../version.json) 的 `datad.version` 为准。修改版本后必须重新编译再打包，不能只修改安装脚本中的版本或哈希。

## 4. 整套刷新 deploy_kano

### 当前本机统一入口

当前已有本地辅助脚本：

```powershell
Set-Location D:\Git\zwrt-datad
python build/refresh-deploy-package.py
if ($LASTEXITCODE -ne 0) { throw 'Packaging failed; do not upload this package' }
```

**该辅助脚本位于被 Git 忽略的 `build/` 中，目前不是仓库跟踪的正式脚本。**
它在本机保留，但全新 clone 不会自带；清理时不能把它当普通日志删除。本文记录现状，没有把它描述成已有的通用 CI 入口。

运行前确认以下本地文件存在：

- `build/refresh-deploy-package.py`
- `build/deploy_kano/u50get/tiny/u50get-tiny`
- `build/deploy_kano/tiny-bootstrap/install-steps.txt`
- 第 3 节生成的 candidate 和 provenance 文件。

刷新入口从已有 `tiny-bootstrap/install-steps.txt` 中读取部署服务器目录，并使用现有 tiny 下载器。
私人服务器地址只保留在本地生成物中，不写入公共仓库文档或模板。

该入口会：

1. 调用 [package-u50pro.py](../scripts/package-u50pro.py)，更新根部署包与 `tiny-bootstrap/` 中的二进制、安装脚本、bootstrap、校验文件。
2. 重新生成分段安装命令。当前格式是 **6 条命令：准备 1 条、压缩载荷 4 段、校验执行 1 条**；命令文件内容为英文，每条长度小于 2000 字符。
3. 检查生成的 shell 安装脚本语法。
4. 调用 [package-u50pro-adb.py](../scripts/package-u50pro-adb.py)，生成离线 ADB 目录和当前版本 ZIP。
5. 调用 [package-u50pro-go.py](../scripts/package-u50pro-go.py)，重新编译内嵌新版本哈希的 Go 安装器，包括 UPX 版和未压缩备用版。
6. 核对各份二进制、脚本、bootstrap、Go 生成资源、校验清单和 ADB ZIP，输出 `package-verification.json`。

刷新不是原子操作；中途失败时，目录里可能混有新旧文件。必须修复错误并完整重跑成功，不能从失败的半成品中挑文件上传。

### 仓库内的基础打包脚本

以下是当前入口调用的基础能力，便于迁移环境时核对。示例服务器地址必须替换，不包含私人配置。

WSL 中生成 shell 安装包和 tiny bootstrap：

```sh
cd /mnt/d/Git/zwrt-datad
python3 scripts/package-u50pro.py \
  --output build/deploy_kano \
  --base-url https://YOUR-SERVER/datad/u50pro \
  --tiny-downloader build/deploy_kano/u50get/tiny/u50get-tiny
```

PowerShell 中生成离线 ADB 包：

```powershell
python scripts/package-u50pro-adb.py --output build/deploy_kano --adb-dir D:\Android\Sdk\platform-tools
```

WSL 中生成 Go 安装器：

```sh
cd /mnt/d/Git/zwrt-datad
python3 scripts/package-u50pro-go.py \
  --output build/deploy_kano \
  --base-url https://YOUR-SERVER/datad/u50pro \
  --ca-bundle /etc/ssl/certs/ca-certificates.crt --upx
```

这三条基础命令**不等于整包刷新**：它们本身不会完成本地辅助脚本中的分段命令生成、第二份 `tiny-bootstrap/` 同步和整包交叉核验。

## 5. 最终交付文件

统一从 `build/deploy_kano/` 取文件：

| 文件或目录 | 用途 |
| --- | --- |
| `zwrt-datad-armv7` | 上传服务器的 datad 主程序 |
| `install-datad.sh` | shell 安装入口，内嵌该主程序的版本、大小和 SHA256 |
| `bootstrap-u50pro.sh` | 内嵌 tiny 下载器，并固定校验安装脚本 |
| `install-steps.txt`、`一键安装命令.txt` | 同一套分段粘贴命令，固定校验 bootstrap |
| `install-datad-armv7` | Go 安装器，内嵌主程序版本、大小、SHA256 和部署脚本 |
| `install-datad-armv7.unpacked` | 未压缩的 Go 安装器备用版 |
| `U50Pro_ADB/`、`U50Pro_ADB_<版本>.zip` | 无网络的本地 ADB 部署包 |
| `tiny-bootstrap/` | tiny 部署方案的同步副本 |
| `SHA256SUMS`、`INSTALLER-SHA256SUMS` | 文件校验清单 |
| `build-info.json`、`installer-build-info.json` | 主程序和安装器的构建信息 |
| `package-verification.json` | 本次整包交叉核验结果 |

旧版本 ZIP 可能仍在目录内；交付前按当前版本确认，不凭文件列表里的排列顺序选择。

## 6. 上传前核验

至少确认：

- 构建、打包命令均以退出码 0 完成；本次相关测试通过。
- `package-verification.json` 是本次成功生成的，不是之前遗留的文件。
- candidate、部署目录主程序、tiny 副本和 ADB payload/ZIP 内主程序 SHA256 一致。
- 安装脚本内的 SHA256、字节数与主程序一致；bootstrap 对应本次安装脚本；分段命令对应本次 bootstrap。
- Go 安装器已重新编译，不能只替换旁边的 datad 二进制。
- 各校验清单通过，版本与 `version.json` 一致。

本机可快速查看：

```powershell
Get-Content build/deploy_kano/package-verification.json
Get-FileHash build/zwrt-datad-armv7-candidate, build/deploy_kano/zwrt-datad-armv7 -Algorithm SHA256
```

SHA256 用于确认文件匹配，**不等于 Ed25519 发布签名**。当前自建部署包流程不会自动生成 App 签名更新所需的清单签名。
GitHub ARM32 发布及签名更新另见 [publish-arm32-release.sh](../scripts/publish-arm32-release.sh)；不在日常本地打包时顺带发布或签名。

## 7. 上传与实机验证顺序

1. 完成本机测试、ARM32 编译和整包核验。
2. 将同一套 `zwrt-datad-armv7` 和 `install-datad.sh` 上传到同一个服务器目录；对外提供的 bootstrap 和 Go 安装器也同步更新。
3. 所有文件就绪后再发送新版分段命令；上传过程中不通知用户开始安装。
4. 在目标设备部署，等待明确的成功结果，确认运行版本和对应接口能力。
5. 后端部署成功后，再连接 App 验证新功能，不能拿未更新的后端判断 App 功能是否正常。

EFS 的实机验证只读目录、读取文件；写入、删除和异常情况先由模拟测试验证，不拿用户基带文件做写入试验。

**禁止混用旧安装器与新二进制，也不要手改 SHA256 绕过不匹配。遇到校验错误应重新核对并生成整套部署包。**
