# iteration-speed

## 应触发

- 新机器上的 Rust 项目要初始化，先量构建时间，再配置用户级缓存和链接器。
- 多个 worktree 同时编译，每次门禁都等一分钟，查哪里在重复工作。
- 下游改一行很快，上游改一行全仓库重编，拆 crate 前先比较收益。
- Decide which checks each parallel agent should run and batch the full gate after integration.
- Our pnpm installs and TypeScript tests got slower; inspect stores and incremental outputs.
- Python 测试启动很慢，量一下导入时间、上次失败用例和并行测试的耗时。

## 不应触发

- 修复这条业务断言失败。（没有反馈速度问题）
- 把这个类拆成两个，只为提高可读性。（没有重编或重测的测量需求）
- 帮我部署镜像到生产。（部署操作不属于本技能）

## 可执行验收

- 假混合仓库运行 `detect`，报告 Rust/Node/Python/Go/Docker 且仓库快照不变。
- `time -- sh -c 'exit 7'` 返回 7，用户状态目录记录耗时、退出码和命令；`history` 可读取。
- 指定临时 HOME 后 `rust-setup --write` 只写用户配置，保留无关配置且二次写入生成备份；`--print` 不写文件。
