# scrcpy-pad 发布构建入口
#
#   make            构建 Linux + Windows 并打包(等价于 ./build.sh all)
#   make linux      只构建 Linux
#   make windows    只构建 Windows(需要 mingw-w64 交叉编译工具链)
#   make clean      删除所有构建产物
#
# 产物位于本目录:linux-x86_64/、windows-x86_64/ 与对应的压缩包。

.PHONY: all linux windows clean

all:
	./build.sh all

linux:
	./build.sh linux

windows:
	./build.sh windows

clean:
	./build.sh clean
