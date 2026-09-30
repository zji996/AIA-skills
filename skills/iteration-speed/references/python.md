# Python：安装、收集与执行

## uv 缓存与 venv

- `uv cache dir` 查询缓存位置，`UV_CACHE_DIR` 设置用户级位置。
- uv 缓存下载/构建的依赖；wheel 可复用不代表源码构建总能命中。
- 本地依赖变动要考虑缓存失效规则，不能把旧 wheel 当新源码。
- 缓存与环境同一文件系统可避免跨盘复制；链接模式受平台影响。
- 缓存完整时用 `uv sync --offline --frozen` 初始化 worktree。
- `--frozen` 使用现有锁文件，不验证它与项目配置是否一致。
- 启动已同步环境里的工具可用 `uv run --no-sync pytest`。
- 这把依赖同步从测试计时中分离；依赖变更后仍需先同步。
- 同一 worktree 复用 `.venv`，不同 worktree 各自建环境。
- venv 中脚本可能含绝对路径，不直接把旧目录搬到新 worktree。
- 多个 Agent 不同时修改同一环境；共享 uv 缓存即可。
- 活跃环境使用硬链接时，清缓存前考虑是否有并行操作。

## 迭代测试

```sh
pytest tests/test_api.py
pytest -x
pytest -k 'parse and not slow'
pytest --lf
```

`-x` 遇第一项失败就停止，适合快速返工。
`-k` 按名称表达式筛选，不代表覆盖所有受影响行为。
`--lf` 重跑上次失败；没有失败记录时默认仍运行全部。
`.pytest_cache` 是本地状态，不能代替当前版本完整验收。
fixtures 或公共模块改变时补相关下游用例。

## 并行与启动成本

安装 pytest-xdist 后用 `pytest -n auto`，按整机容量调整 worker 数。
多个 Agent 已在跑测试时，auto 各自占满核数会导致 CPU 超卖。
`pytest -n 2` 是限制 worker 的示例，不是固定推荐值。
共享数据库、端口、临时目录的测试需隔离或单独分组。
每个 worker 会重复收集/导入，短测试多时并行可能更慢。
先比较收集、fixture 初始化与测试体耗时，再选分组方式。
`python -X importtime -c 'import your_package'` 看导入自身/累计时间。
输出写 stderr；累计值包含子导入，不应逐行相加。
将慢依赖延迟导入前确认生命周期和错误处理没有变化。

## 跨目录边界

uv 缓存可跨环境复用依赖；venv 路径与 pytest 失败记录属于具体目录。
慢端到端测试按批合入后验收，公共 fixture 变动不可只跑上次失败。
按文件/名称筛选节省执行成本，无法自动证明没有遗漏。

## 核对来源

- [uv 缓存](https://docs.astral.sh/uv/concepts/cache/)
- [pytest 用法](https://docs.pytest.org/en/stable/how-to/usage.html)
- [xdist 调度](https://pytest-xdist.readthedocs.io/en/stable/distribution.html)
- [Python importtime](https://docs.python.org/3/using/cmdline.html#cmdoption-X)
