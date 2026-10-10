# strata-tray

GNOME 顶栏上的 [Strata](https://github.com/Niko1221/Strata) 状态指示器：**只有一个彩色圆点**，颜色就是状态，点开是详情。

![五个状态在深色面板上的样子](preview-dark-32.png)

| 状态 | 颜色 | 含义 |
| --- | --- | --- |
| Idle | 灰 `#8e979f` | 空闲 |
| Reading | 蓝 `#3aa9ff` | 在读 prompt（还没出第一个 token） |
| Generating | 绿 `#34d399` | 正在生成 |
| Error | 红 `#ff5a6e` | 连不上服务器 / key 不对 / 响应异常 |

另外，只要有请求在排队，圆点**右上角会多一个琥珀小点** `#f5b53d`（状态颜色不变）。

这些颜色就是 Strata 自己的状态色，取自 `serve/web/tokens.css` 的 dark 一套，和
`serve/web/components.css` 里 `.st-pill[data-state=...]` 一一对应：
generating=`--st-accent`、reading=`--st-info`、queued=`--st-warn`、error=`--st-danger`、
其余=`--st-ink-muted`（monitor 页是 `data-theme="dark"`，所以取 dark 的值）。

判定方式和 Strata 自己的 `/metrics` 一致：`busy` 且还没有第一个 token 是 Reading，
`busy` 且已经在出 token 是 Generating，其余 Idle。

面板上不显示文字（`XAyatanaLabel` 不设置），所以用的就是 crates.io 上的正式 `ksni 0.3.6`。

## 数据来源

- `/status`（每秒，几百字节）：`busy`、`queued`、`phase`、`prompt_tokens`、`generated`、`elapsed_s`、`tokens_per_s`
- `/v1/status`（每 5 秒）：模型、engine 版本、上下文、uptime、请求数、上一个请求的耗时、GPU 利用率/温度/功耗、内存
- `/metrics`（每 30 秒）：`totals` 里的累计 token（prompt / reused / output）
- 没有 API key 时退化为 `/health`（无需认证，但只有模型名和是否加载）

API key 的查找顺序：环境变量 `STRATA_API_KEY` → `~/.config/strata/service.env`。

## 菜单（点击圆点）

详情若干行（不可点）：状态、tok/s（即时与均值）、已生成、排队数、上下文占用、模型与 engine、
运行时长与请求数、**tokens 总计**（服务器启动至今的 prompt / 复用 / 输出，用 K/M/B 简写）、GPU、内存、
上一个请求的耗时。

然后是：打开网页界面（`http://127.0.0.1:8080/#monitor`）、重启服务、停止服务（服务器不通时变成启动服务）、退出托盘。
服务操作走 `systemctl --user <verb> strata`。

> GNOME 的 appindicator 扩展不渲染 SNI 的 `ToolTip`，所以悬浮提示只有 KDE 等宿主能看到；
> 在 GNOME 上详情在点击菜单里。

## 需要什么

- Strata 服务器在 `127.0.0.1:8080`（本工具读它的 `/status` 和 `/v1/status`）
- GNOME 加 AppIndicator 扩展（`appindicatorsupport@rgcjonas.gmail.com`），Wayland / X11 都行
- 编译需要 Rust 1.80+

## 构建与运行

```sh
cargo build --release
./target/release/strata-tray          # 前台跑，Ctrl-C 退出
```

装成用户服务（登录后自启）：先把 `strata-tray.service` 里 `ExecStart` 的路径改成你这边的
`target/release/strata-tray`（默认写的是 `%h/git/Strata-data/strata-tray/...`，`%h` 是家目录），然后

```sh
cp strata-tray.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now strata-tray     # 启动 + 自启
systemctl --user status strata-tray           # 看状态
journalctl --user -u strata-tray -f           # 看日志
systemctl --user disable --now strata-tray    # 关掉
```

## 实现说明

- 图标不用文件，直接按状态生成 ARGB32 像素（16×16 和 32×32），`IconName` 留空让扩展用 pixmap。
  右上角的排队角标是画进同一张 pixmap 的，没用 SNI 的 `OverlayIconPixmap`——
  那个被 GNOME 扩展当成 GIcon emblem 合成在**右下角**，尺寸也由宿主决定（16px 图标时约 10px）。
- 服务器地址默认 `127.0.0.1:8080`，可以用 `STRATA_TRAY_HOST` / `STRATA_TRAY_PORT` 覆盖。
- HTTP 只用 `std::net::TcpStream` 手写（本机 127.0.0.1，不需要 TLS 和 HTTP 库）。
- 想改颜色或角标位置/大小：`State::rgb()` 和 `dot_icon()`。

## 预览

`preview-{dark,light}-{16,32}.png` 是深色/浅色面板底色下的样子，左起
Idle、Reading、Reading+排队角标、Generating、Generating+排队角标。
像素来自真实负载（三个并发的模型请求）时托盘实际发出的 `IconPixmap`，不是另画的示意图。
Error 的红色没法用真实负载造出来（要停掉服务器），只有 token 值。

![深色面板](preview-dark-16.png)
![浅色面板](preview-light-16.png)
