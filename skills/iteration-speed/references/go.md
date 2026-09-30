# Go

Go 自带内容寻址构建缓存；`go env GOCACHE` 查询用户缓存位置。
不同 worktree 可命中相同输入，工具链、flags、源码变动影响命中。
`go test ./path/to/package` 先验证受影响包，公共接口变动补下游。
成功测试结果也可能缓存；`go test -count=1 ./path/to/package` 禁用测试结果缓存。
`-count=1` 仍可复用构建缓存；需要真实执行时使用，会牺牲热重跑速度。
不要每轮 `go clean -cache`，它会丢掉构建复用。
本机容量或绝对缓存路径只在用户环境配置，不提交仓库。

来源：[Go 命令与缓存](https://pkg.go.dev/cmd/go)。
