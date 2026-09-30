# scroll-gesture

## 应触发

- 过界切换会话在远程桌面里划不动，看看是什么原因。
- Windows 上用鼠标滚轮，下拉翻页一格一格地跳，想做得平滑些。
- 自定义下拉刷新的动画打断不了，想做成像 iOS 那样随时能接住。
- Build an overscroll-to-next-page gesture that works with trackpad, mouse wheel and touch.

## 不应触发

- 给列表加个“回到顶部”按钮。（普通原生滚动，不涉及手势）
- 虚拟列表渲染一万行时太卡。（渲染性能，不是输入手势）
