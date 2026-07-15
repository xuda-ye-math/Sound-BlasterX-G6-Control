# Sound BlasterX G6 控制工具

[English](README.md) | **简体中文**

适用于 Linux 的 [Creative Sound BlasterX G6](https://www.amazon.com/BlasterX-External-Surround-Sidetone-Consoles/dp/B0G6DS1RZV)（USB `041e:3256`）控制工具，提供命令行程序 `g6-cli` 与原生图形界面 `g6-gui`。

主要功能包括：

- 自动检测并初始化扬声器与麦克风，包括 ALSA / PipeWire 路由、External Mic 选择和 udev 规则。
- 一键切换内置的 Default、Scout 与 SBX 配置，也可保存和加载自定义配置。
- 直接调节麦克风输入与扬声器输出音量，并显示实时电平。
- 调节 SBX 效果、10 段均衡器、输出模式和 DAC 滤波器，并实时显示均衡器频响曲线。
- `g6-gui` 同时支持原生 Wayland 与 X11，会在启动时自动选择当前会话可用的显示后端。

> **仅正式支持 Arch Linux。** 本项目已在 Arch Linux 的 Hyprland（Wayland）和 LXQt（X11）环境下测试，两者均使用 PipeWire-Pulse。推荐通过项目自带的 [PKGBUILD](PKGBUILD) 安装；其他发行版尚未测试。

![g6-gui 界面截图](g6-gui.png)

## 安装（Arch Linux）

使用 AUR 助手一行安装：

```sh
yay -S sound-blasterx-g6-control-git
```

或手动 `makepkg`：

```sh
git clone https://github.com/xuda-ye-math/Sound-BlasterX-G6-Control.git
cd Sound-BlasterX-G6-Control
makepkg -si
```

## 首次设置

插入 G6 后运行一次：

```sh
g6-cli init
```

初始化完成后即可启动图形界面：

```sh
g6-gui
```

也可以完全通过命令行使用。运行 `g6-cli --help` 查看全部命令。
