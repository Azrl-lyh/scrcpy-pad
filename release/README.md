# release —— 发布构建目录

本目录用于把源码编译成可直接对外发布的二进制包。所有产物都生成在本目录内。

## 目录结构

```
release/
├── build.sh                  Linux/macOS 上的构建脚本
├── build.bat                 Windows 上的构建脚本
├── Makefile                  make 入口(封装 build.sh)
├── README.md                 本文件
├── linux-x86_64/             构建产物:可直接发布的 Linux 包
├── windows-x86_64/           构建产物:可直接发布的 Windows 包
├── scrcpy-pad-<版本>-linux-x86_64.tar.gz
└── scrcpy-pad-<版本>-windows-x86_64.zip
```

`linux-x86_64/` 与 `windows-x86_64/` 均为自包含目录，内含可执行文件、`icons/`、`README.md`、`LICENSE` 与启动脚本，压缩包由这两个目录打包而成。

## 构建

### Linux / macOS 主机

```bash
cd release
./build.sh            # 构建 Linux + Windows 并打包
./build.sh linux      # 只构建 Linux
./build.sh windows    # 只构建 Windows
./build.sh clean      # 删除所有构建产物
```

也可以使用 make：

```bash
cd release
make            # 等价于 ./build.sh all
make linux
make windows
make clean
```

### Windows 主机

```bat
cd release
build.bat
```

Windows 主机只能编译 Windows 版；Linux 版请在 Linux 上构建。

## 交叉编译 Windows 版的依赖

在 Linux 上构建 Windows 版需要 mingw-w64 工具链与对应的 Rust 编译目标。脚本会自动检测并给出提示。

```bash
# Fedora
sudo dnf install mingw64-gcc mingw64-binutils

# Debian / Ubuntu
sudo apt install gcc-mingw-w64-x86-64

# Arch
sudo pacman -S mingw-w64-gcc

# Rust 编译目标(脚本会自动安装)
rustup target add x86_64-pc-windows-gnu
```

未安装工具链时，`./build.sh` 会跳过 Windows 部分并继续完成 Linux 构建，不会中断。

## 产物说明

| 文件 | 说明 |
|---|---|
| `linux-x86_64/scrcpy-pad` | Linux 可执行文件 |
| `linux-x86_64/启动.sh` | Linux 启动脚本，自动处理 `/dev/input` 读取权限 |
| `windows-x86_64/scrcpy-pad.exe` | Windows 可执行文件 |
| `windows-x86_64/启动.bat` | Windows 启动脚本 |

发布时直接上传 `scrcpy-pad-<版本>-linux-x86_64.tar.gz` 与 `scrcpy-pad-<版本>-windows-x86_64.zip` 即可；使用者解压后运行启动脚本，无需安装 Rust。

程序运行仍需目标机器具备 `adb` 与 `scrcpy`，详见项目根目录 `README.md`。

## 备注

- 本目录的构建脚本只会向本目录写入产物，不会改动项目根目录中的任何文件。
- 构建过程复用项目根目录的 `target/`，切换平台不会相互覆盖。
- 版本号自动从 `../Cargo.toml` 读取，用于命名压缩包。
