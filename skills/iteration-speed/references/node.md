# Node：共享依赖与增量输出

## pnpm store

- `pnpm store path` 查询当前配置的内容寻址 store。
- 同一文件系统上可硬链接；支持克隆时可能采用 reflink。
- 跨文件系统会复制，路径近不代表文件系统相同。
- 用户级 `pnpm config set store-dir <路径> --global` 设置 store。
- 不把本机绝对路径提交到仓库 `.npmrc`。
- `pnpm install --offline --frozen-lockfile` 适合已缓存的 worktree 初始化。
- 缺包时离线安装失败，需要先补齐缓存；生命周期脚本仍可能执行。
- 热 store 的安装可到秒级，原生模块或脚本多时须实测。
- 把这种轻量安装放到排队的重任务队列可能增加等待，按实测决定。
- store 复用包内容，node_modules 的目录映射仍属于各 worktree。

## TypeScript

```json
{
  "compilerOptions": {
    "incremental": true,
    "tsBuildInfoFile": "./.cache/typecheck.tsbuildinfo"
  }
}
```

`tsc -b` 按项目引用构建过期项目；被引用项目使用 `composite`。
增量状态记录在 `.tsbuildinfo`，配置、版本或输入改变会影响复用。
noEmit 类型检查与 emit 构建使用不同的 `tsBuildInfoFile` 和输出位置。
不要让两种模式互相覆盖，也不要让两个 Agent 同时写同一增量文件。
项目引用只在确有测量收益时拆分；增加边界也增加配置维护。

## Vite / Vitest / Playwright

- Vite 默认把预构建依赖缓存到 `node_modules/.vite`。
- 锁文件、相关配置等变化会使依赖重新预构建。
- `vite --force` 强制重建缓存，用于排障，不放进每次迭代入口。
- `vitest run path/to/test.ts` 按文件运行。
- `vitest run --changed` 基于 Git 改动筛选，依赖分析不是完整门禁证明。
- 需要比较基准时使用 `vitest run --changed HEAD~1`。
- Playwright 全量浏览器门禁按批合入后跑一次；仓库要求优先。
- 浏览器改动可先跑相关文件：`playwright test path/to/spec.ts`。
- CPU 超卖会拖慢调度，对时序敏感的用例可能偶发失败。
- 先限制并行、检查等待条件和资源占用，再判断是否业务回归。

## 跨目录边界

store 可共享依赖内容，TS 增量和 Vite 缓存不保证跨目录命中。
不要共享正在并行写入的 node_modules 或 `.tsbuildinfo`。
分别测安装、类型检查、单测和浏览器启动，定位主要等待段。

## 核对来源

- [pnpm store 设置](https://pnpm.io/settings#store-dir)
- [TS 项目引用](https://www.typescriptlang.org/docs/handbook/project-references.html)
- [TS incremental](https://www.typescriptlang.org/tsconfig/incremental.html)
- [Vite 预构建](https://vite.dev/guide/dep-pre-bundling)
- [Vitest CLI](https://vitest.dev/guide/cli.html)
