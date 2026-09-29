# 发布与安装渠道

## 当前渠道：私有 GitHub Release

仓库为 `cdcRyan/mutlink`。Release 与源码继承相同的可见性，用户需要仓库读取权限。README 中的 `gh release download` + `MUTLINK_USE_GH=1` 可以通过 GitHub 登录下载。

每个版本包含五种平台压缩包、`SHA256SUMS`、下载脚本以及基于实际包哈希生成的 `mutlink.rb`。脚本安装只写用户目录；先校验并试运行新程序，再替换旧程序。

Linux 预编译包基于 Ubuntu 22.04，要求 glibc 2.35+，不支持直接在 musl/Alpine 上运行；这类系统请从源码构建。Windows 当前仅提供 x64。

## 开放匿名安装

需要仓库所有者决定公开范围：

1. 公开源码及 Release：将主仓库改为 public，即可使用下面的命令。
2. 保持源码私有：单独创建公开的发行仓库，上传压缩包、脚本和校验清单。复制构建产物前需审核许可与内容；包内包含 README、Cargo.lock 及许可文件。在该仓库使用 `scripts/release_manifest.py --version v0.1.0 --repo OWNER/PUBLIC-REPO` 重新生成公式和校验清单。安装时设置 `MUTLINK_REPO=OWNER/PUBLIC-REPO`。

主仓库公开后，macOS / Linux：

```sh
curl -fsSL https://github.com/cdcRyan/mutlink/releases/latest/download/install.sh -o /tmp/mutlink-install.sh
sh /tmp/mutlink-install.sh
```

Windows PowerShell：

```powershell
Invoke-WebRequest https://github.com/cdcRyan/mutlink/releases/latest/download/install.ps1 -OutFile "$env:TEMP\mutlink-install.ps1"
& "$env:TEMP\mutlink-install.ps1"
```

已设置 `MUTLINK_USE_GH=1` 的终端会继续使用 GitHub CLI；公开下载可清除该变量。SHA256 用于校验文件完整性，不能替代发布者签名或 macOS 公证。

## Homebrew

Release 中的 `mutlink.rb` 已根据四个 Unix 包的真实 SHA256 生成。**普通 Homebrew 无法直接下载私有 Release**，需先公开对应发行资产。

公开后，维护者创建 GitHub 仓库 `cdcRyan/homebrew-tap`，将该版本的 `mutlink.rb` 放到 `Formula/mutlink.rb` 并提交，用户即可运行：

```sh
brew install cdcRyan/tap/mutlink
mutlink --version

# 新版本公式提交后
brew update
brew upgrade mutlink
```

这是自有 tap，不需要申请 Homebrew 官方核心仓库收录。当前尚未创建或公开该 tap，以上命令是启用后的入口。后续每次发版下载新的 `mutlink.rb` 覆盖 tap 中的公式并提交即可。

## 维护者发版

1. 更新 `Cargo.toml` 版本，同步 `Cargo.lock`，检查更新说明 `docs/release-notes.md`。
2. 将变更提交到 main；确认 CI 通过。
3. 推送对应版本 tag：

```sh
git tag v0.1.0
git push origin v0.1.0
```

`.github/workflows/release.yml` 将在五种原生平台构建、测试、打包并验证安装。全部通过后自动发布 Release；tag 必须与 Cargo 版本一致。发布权限仅授予最后一个 job。

如果构建失败，可以从 Actions 重跑失败的 job；也可执行 `gh workflow run release.yml -f tag=v0.1.0`。已发布版本不会被工作流覆盖，需要修复时使用新版本 tag。

本地验证当前平台：

```sh
cargo build --release --locked
python3 scripts/package.py aarch64-apple-darwin
python3 tests/install_smoke.py
```

目标参数按当前平台替换；交叉构建时第二个参数可指定二进制路径。`package.py` 从 Cargo 自动读取版本并输出每个包的 `.sha256`。集齐五个包后执行 `python3 scripts/release_manifest.py --version v0.1.0` 生成汇总清单与 Homebrew 公式。
