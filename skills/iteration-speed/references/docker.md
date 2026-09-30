# Docker / BuildKit

先复制锁文件并安装依赖，再复制频繁变化的源码，减少层失效传播。
BuildKit 用缓存挂载复用下载内容；层失效后仍可利用包缓存：

```dockerfile
# syntax=docker/dockerfile:1
RUN --mount=type=cache,target=/root/.cache/pip pip install -r requirements.txt
```

缓存挂载属于 builder，不自动跨机器；多 builder 需配置缓存导入/导出。
安装步骤必须在空缓存时也能成功，缓存内容可被回收。
apt 等并发写入要求独占的缓存使用 `sharing=locked`。
用 `.dockerignore` 减少上下文 I/O；不要把整个本机缓存复制进镜像层。
测上下文传输、依赖安装和层构建各段，命中层不代表运行时测试已通过。

来源：[BuildKit 缓存与层顺序](https://docs.docker.com/build/cache/optimize/)。
