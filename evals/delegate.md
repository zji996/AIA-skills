# delegate

## 应触发

- 让 Pi 并行审查一下 `src/api/` 和 `src/db/` 的错误处理，你继续做前端。
- 这个缓存方案帮我找个便宜模型问问第二意见。
- 这个动画页面我来做浏览器检查，你让 Pi 先审一下交互与小屏布局，给出最重要的三条意见。
- Delegate the migration of these three config files to a background agent and review the diff afterwards.
- 让 Pi 只读审查存储布局，控制台别刷读取步骤，只给我最终结论和错误。
- 把这个 bug 交给 Pi 修，跑 `npm test` 通过才算完成，我先去做别的。
- 让 GPT 用 Codex 独立审一下这个迁移方案，和 Gemini 的意见对照看看。
- （主控自发）我继续改代码，同时让 Codex 分运行时、API/调度、前端三路只读审查这批提交。
- 这是移动端设置页的截图，找个同事看看布局哪里别扭。
- 这个锁释放的并发 bug 交给 GPT 修，`go test ./...` 通过为准。
- （主控自发）要审 api、worker、web 三块的错误处理，各自独立，分给同事并行做。
- 让 Codex 在单独的 worktree 里改上传接口，别碰我现在的工作区，改完给我看改了哪些文件再合并。
- 刚才那个任务让 Pi 接着把测试补上，就在它原来的会话里继续。

## 不应触发

- 把这个函数的变量名改成驼峰。（委派与验收的开销高于直接修改）
- 帮我配置 Pi 的默认模型。（是 Pi 配置问题，不是委派任务）
