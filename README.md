# Netodia

[![License: AGPL-3.0-or-later](https://img.shields.io/badge/License-AGPL--3.0--or--later-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Built%20with-Rust-orange.svg)](https://www.rust-lang.org/)

**Netodia 是一款基于 [Melodia](https://github.com/KenanSalar/Melodia) 修改的第三方网易云音乐桌面客户端。**它使用 Rust 和 Slint 构建，在 Melodia 的桌面音乐播放器基础上整合了网易云音乐账号、曲库浏览和在线播放功能，同时保留本地音乐管理与播放能力。

> **第三方项目声明**：Netodia 由社区独立开发，与网易云音乐及其关联公司无隶属、授权或官方支持关系。网易云音乐的账号、接口、曲库和播放权限由相关服务提供方管理；服务规则或接口变化可能影响部分功能。请遵守相关服务条款并合理使用。

## 功能

- **网易云账号**：支持二维码、手机号和邮箱登录。
- **在线音乐**：搜索歌曲，浏览歌单、专辑和歌手，并播放网易云音乐曲目；可在设置中选择云端播放音质。
- **个人音乐**：浏览收藏和创建的歌单、喜欢的歌曲、最近播放与每日推荐；提供私人漫游、心动模式和相似歌曲等播放入口。
- **歌词**：显示在线曲目歌词，也支持本地歌词文件和音乐文件内嵌歌词。
- **本地音乐**：扫描和管理本地曲库，支持搜索、歌单、标签编辑及多种常见音频格式。
- **桌面播放体验**：包含播放队列、均衡器、ReplayGain、无缝播放、迷你播放器、系统媒体按键和多种主题；播放模式通过单个按钮按顺序播放、列表循环、单曲循环、随机播放依次切换，首次启动默认为顺序播放，之后记住上次选择。
- **跨平台**：支持 Linux 与 Windows，面向 `x86_64` 和 `aarch64` 架构提供构建。

网易云音乐功能需要网络连接。部分歌曲、音质或账号功能可能受账号状态、会员权限、地区及服务端策略限制。

## 下载

前往 [GitHub Releases](https://github.com/HELPMEEADICE/Netodia/releases) 下载适用于你系统的版本。Linux 提供 `.deb`、`.rpm`、AppImage 或压缩包等构建；Windows 提供 `.msi` 安装包和 `.zip` 便携版（以对应版本的发布附件为准）。

## 从源码构建

项目使用 Rust 2024 edition，工具链版本见 [`rust-toolchain.toml`](rust-toolchain.toml)。Linux 构建还需要 Slint/FemtoVG 所需的系统开发依赖；Windows 无需额外的项目专用依赖。

```bash
git clone https://github.com/HELPMEEADICE/Netodia.git
cd Netodia
cargo run -p netodia
```

构建发布版本：

```bash
cargo build --release -p netodia
```

Linux 常见构建依赖：

```bash
# Debian / Ubuntu
sudo apt install libfontconfig1-dev libfreetype6-dev libasound2-dev \
                 libxkbcommon-dev mesa-vulkan-drivers libwayland-dev

# Fedora
sudo dnf install fontconfig-devel freetype-devel alsa-lib-devel \
                 libxkbcommon-devel vulkan-loader wayland-devel
```

## 项目来源与许可

Netodia 基于 [Kenan Salar 的 Melodia](https://github.com/KenanSalar/Melodia) 修改。感谢 Melodia 项目及其贡献者提供的基础播放器实现。

Netodia 按 [GNU Affero 通用公共许可证 v3 或更高版本（AGPL-3.0-or-later）](LICENSE) 发布。分发或修改本项目时，请遵守许可证要求；随项目提供的第三方组件许可文本见 [`licenses/`](licenses/)。

网易云音乐及相关商标、服务和内容归其各自权利人所有，本项目不主张相关权利。
