# DeepSeek Harness Desktop

把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`）封装成**一个独立桌面应用**：
用户机器上**不需要**预装 Node.js、不需要 npm、不需要 pnpm、不需要任何命令行工具——双击即用，
同时 **DSH 自身的更新通道保持原样可用**。

外壳使用 [Tauri](https://github.com/tauri-apps/tauri) v2（Rust + 系统原生 WebView），
Windows / macOS / Linux 三平台适配，安装包体积远小于 Electron 方案（不需要打包一份 Chromium）。

```
┌─ DeepSeek Harness Desktop（Tauri 外壳，系统 WebView，约 10 MB）────────┐
│  splash 窗口（解包/启动进度）                                             │
│  main  窗口  ──► http://127.0.0.1:<port>/?token=…  （DSH Web UI，原样加载）│
│                          ▲                                              │
│                          │ 标准输出里的 "dsh web: <url>"                  │
│  ┌───────────────────────┴──────────────────────────────────────────┐   │
│  │ 受管运行时目录（用户可写）  <app-data>/                              │   │
│  │   runtime/node/node      ← 官方 Node.js 运行时（随包分发）           │   │
│  │   runtime/pnpm/          ← 随包分发的 pnpm（DSH 插件管理要用）        │   │
│  │   runtime/bin/pnpm       ← 垫片：让 PATH 上的 pnpm 指向上面这份        │   │
│  │   vendor/node_modules/@deepseek-ai/dsh/**  ← 原版 DSH + 完整依赖闭包  │   │
│  └──────────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## 一、这个外壳做了什么（以及刻意没做什么）

**只做三件事**：

1. 随安装包分发一份**官方 Node.js 运行时**；
2. 随安装包分发一份**原版 `@deepseek-ai/dsh` 及其完整依赖闭包**（未打任何补丁、未删任何文件）；
3. 把上面两者解包到用户可写目录，执行
   `node vendor/node_modules/@deepseek-ai/dsh/lib/bin.js --profile web --no-open`，
   解析它在标准输出打印的 `dsh web: <带 token 的 loopback URL>`，把系统 WebView 指过去。

**刻意不做的事**：

| 没做的事 | 原因 |
| --- | --- |
| 不 fork / 不 patch DSH 源码 | 保持"原样封装"：上游行为＝应用行为，升级不依赖我们 |
| 不共用本机的 `~/.dsh` | 默认使用 APP 私有的 `<app-data>/home`：与机器上其他 DSH 实例（CLI、别的 GUI）完全隔离——孤儿锁、profile、会话互不影响 |
| 不改 DSH 的更新机制 | 见下节：更新走的就是官方那条 npm/pnpm 路径 |
| 不把运行时塞进安装目录 | macOS `.app` 签名只读、Windows `Program Files` 普通用户不可写，且更新必须能改写 `node_modules` |
| 不给自己做私有更新通道 | 外壳与 DSH 版本解耦：外壳升级不影响 DSH 的版本与更新 |

---

## 二、体积：这一版做了什么、省了多少（2026-09-17 实测）

安装包体积（同一套构建，数字取 Actions 产物字节数）：

| 平台 | 改前 | 改后 | 降幅 |
| --- | ---: | ---: | ---: |
| macOS arm64 | 83,933,694 | 51,524,060 | −38.6% |
| macOS x64 | 86,098,897 | 53,731,901 | −37.6% |
| Windows x64 | 162,124,166 | 52,680,603 | −67.5% |
| Windows arm64 | 151,383,874 | 48,472,049 | −68.0% |
| Linux arm64 | 336,651,130 | 52,591,132 | −84.4% |
| Linux x64 | 341,746,501 | 53,264,748 | −84.4% |

"改前"是 Tauri 默认把每个平台能打的格式全打一遍的结果，"改后"每端只发一种格式。

运行时载荷（每平台一份，含 Node 运行时 + DSH 闭包 + pnpm）：

| 指标 | 改前 | 改后 |
| --- | ---: | ---: |
| 归档字节 | 86,417,022 | **48,465,448**（−43.9%） |
| 归档条目 | 30,583 | 12,767 |
| 内含源码映射 / 类型声明 / npm | 4,365 / 6,568 / 2,381 条 | **0 / 0 / 0** |

省下来的部分（都在 `scripts/vendor-runtime.sh` 里做，并且**带断言**）：

1. **非运行期文件剪枝**：`*.map`、`*.d.ts`、原始 `.ts`、`test/`、`docs/`、`examples/`、`benchmark/`、`tsconfig*.json` 等共 14,229 个文件（解压后约 89 MB）——运行时永远读不到它们。
2. **node_modules 里的外来平台预编译件**：`node-pty`、`@img` 这类包会带全部 6 个平台的二进制，只留本平台（约 23 MB/平台）。
3. **Node 自带的 npm**：应用自带随包 pnpm 负责插件安装与更新，Node 自带的 npm（约 16 MB）不再需要；同时去掉调试符号并重新签名（macOS 上 138.5 MB → 101.9 MB）。
4. **压缩级别**：载荷归档用 `zstd -19`。
5. **每端只发一种安装包**：Linux 由 `deb + rpm + AppImage` 收敛为 `deb`；Windows 由 `msi + NSIS` 收敛为 `NSIS`；macOS 只发 `dmg`。
6. **构建前清掉缓存里的旧安装包**：CI 缓存了 `src-tauri/target`，不清理就会把上一轮其它格式的安装包当成"本次产物"一起上传——这正是早先"Linux 300 MB"的另一半原因。

> **为什么 Linux 不发 AppImage**：AppImage 会把 WebKitGTK 整条依赖链打包进去（`libwebkit2gtk` 98 MB + `libjavascriptcoregtk` 31 MB + ICU 30 MB …），实测 165,456,392 字节；而 `deb` 只有 52,608,722 字节（相差 3.1 倍），桌面系统本来就有 GTK/WebKit。需要便携版可自行 `cargo tauri build --bundles appimage`。
>
> **瘦身不牺牲功能**：载荷里的 DSH 闭包是应用真正运行的部分，删掉的是运行时读不到的文件；删除后运行时自检与三平台各自的启动自检都必须通过，否则不出包。

## 三、下载与安装

安装包在仓库的 **Releases** 页面，每端一份、按指令集区分：

| 平台 | 文件 | 安装方式 |
| --- | --- | --- |
| macOS（Apple 芯片） | `DeepSeek Harness Desktop_x.y.z_aarch64.dmg` | 打开后把应用拖进"应用程序" |
| macOS（Intel） | `DeepSeek Harness Desktop_x.y.z_x64.dmg` | 同上 |
| Windows x64 | `DeepSeek Harness Desktop_x.y.z_x64-setup.exe` | 双击安装，无需管理员权限 |
| Windows arm64 | `DeepSeek Harness Desktop_x.y.z_aarch64-setup.exe` | 同上 |
| Linux x64 | `DeepSeek Harness Desktop_x.y.z_amd64.deb` | `sudo apt install ./DeepSeek*.deb` |
| Linux arm64 | `DeepSeek Harness Desktop_x.y.z_arm64.deb` | 同上 |

**每次出包都会跑两级自检，两级都通过才会发布**：

1. 构建树里的程序启动一次：定位/校验/解包载荷 → 起 DSH → 换取会话令牌 → 取回 Harness 界面（HTTP 200）。
2. **从"发布出去的那个安装包"里取出程序，再跑一次同样的自检**：macOS 挂载 dmg 后运行包内二进制、Linux 把 deb 解包后运行、Windows 静默安装到临时目录后运行。也就是说，"能编译"和"用户装完能用"是同一条判据。

## 四、"不影响 DSH 自动更新"是怎么保证的

DSH 没有内置的自更新命令。它的更新方式就是**包管理器更新 `@deepseek-ai/dsh` 这个包**
（`dsh` 的发行物本身就是 npm 包，`lib/bin.js` 只负责起 profile）。
所以"不影响更新"＝**不要挡住、不要代理、不要复刻这条路径**。本项目的做法：

1. **运行时目录是可写的普通 npm/pnpm 工程**
   `<app-data>/vendor/` 里有标准的 `package.json`、`pnpm-workspace.yaml`、`.npmrc`、`node_modules/`。
   因此这些命令都能正常工作，且改的是**同一棵树**：

   ```bash
   # 用随包分发的 pnpm（已自动置于 PATH 首位）
   pnpm add @deepseek-ai/dsh@latest --dir <app-data>/vendor
   # 或用系统里任意 npm
   npm install @deepseek-ai/dsh@latest --prefix <app-data>/vendor
   ```

2. **不劫持包管理器**：应用自己更新时也是 shell out 到**真正的 pnpm / npm**（优先随包分发、
   其次系统 PATH 上的、最后是随 Node 发行的 npm），没有私有通道、没有镜像替换。

3. **失败不可能弄坏现场**：更新在 `<app-data>/vendor.staging/` 里完整装好、校验过
   `@deepseek-ai/dsh/package.json` 存在之后，才把旧目录挪成 `vendor.previous` 并换入新目录；
   任何一步失败都会回滚并保留旧版本。

4. **不碰其他实例的状态**：更新只动 `vendor/` 与 APP 私有的 `$DSH_HOME`（`<app-data>/home`）；
   本机 `~/.dsh`（若用户装过 CLI）里的 session、credentials、插件、profile 一律不动。

5. **外壳与 DSH 版本无关**：外壳不带 DSH 版本号假设，`dsh` 升到任何新版本都被同一个外壳原样加载。

---

## 五、"完整、不依赖其他程序"是怎么保证的

| 依赖 | 随包分发？ | 说明 |
| --- | --- | --- |
| Node.js 运行时 | ✅ | 官方 `nodejs.org/dist` 二进制，放进 payload |
| DSH 及其全部依赖 | ✅ | pnpm hoisted 链接器物化的**真实文件**（无 symlink 到全局 store） |
| pnpm | ✅ | 随包分发 + `runtime/bin/pnpm` 垫片，`dsh plugin add` 也能用 |
| WebView | ⚠️ 系统自带 | macOS 用 WKWebView、Linux 用 WebKitGTK、Windows 用 WebView2（Tauri 配置为 `downloadBootstrapper`，可离线部署时改 `embedBootstrapper`/`offlineInstaller`） |
| 系统运行时库 | ⚠️ 系统自带 | 与任何原生应用相同（Linux 需 WebKitGTK 系软件包，见安装包依赖） |
| 网络 | ❌ 非必需 | 首次启动解包即可用；只有更新/调用模型 API 时才需要网络 |

**同机验证过的证据**（macOS arm64，外壳真实代码路径，`--self-check` 模式）：

```
[self-check] start    dsh-desktop 0.1.0
[self-check] extract  Unpacking the DeepSeek Harness runtime…
[self-check] resolve  DSH 0.1.5-rc.1 · node v26.7.0 (vendored)
[self-check] boot     Starting the DeepSeek Harness server…
[self-check] runtime  self-contained: true
[self-check] runtime  package manager: <data>/runtime/bin/pnpm   ← 随包 pnpm
[self-check] http     token exchange: HTTP 303 with session cookie (true)
[self-check] http     UI served: HTTP 200, 27660 bytes, Harness bootstrap present
[self-check] PASS in 12066 ms
```

`--self-check` 是无头（不开窗口）的完整验收：走的就是 GUI 启动的同一条
`boot_core` 代码路径——定位/校验/解包 payload → 解析运行时 → 启动 DSH →
token 换 Cookie → 拉取 UI HTML 并断言 Harness 引导代码存在。CI 在每个平台的
构建后都会跑它作为验收门槛：

```bash
DSH_DESKTOP_DATA=/tmp/dsh-check DSH_DESKTOP_PAYLOAD=./src-tauri/payload \
    src-tauri/target/release/dsh-desktop --self-check
```

---

## 六、跨平台说明（必须知道的一件事）

payload 里有**平台相关的原生模块**（`koffi`、`node-pty`、`sharp`、`ripgrep` …），
还有平台相关的 Node 二进制。因此：

> **一个 payload 只能服务一个平台架构，不能在操作系统之间复用。**

这是所有 Node 桌面应用（Electron 也一样）的固有约束。项目用
`.github/workflows/build.yml` 的构建矩阵解决：每个平台各自运行一次
`scripts/vendor-runtime.sh`，产出各自的安装包。

矩阵覆盖：

| 平台 | Runner | 产物 |
| --- | --- | --- |
| macOS arm64 | `macos-14` | `.dmg` / `.app` |
| macOS x64 | `macos-13` | `.dmg` / `.app` |
| Linux x64 | `ubuntu-22.04` | `.AppImage` / `.deb` / `.rpm` |
| Windows x64 | `windows-latest` | `.msi` / `.exe` |

### 分发方式（拖进应用程序就能用吗？）

- **macOS**：把 `DeepSeek Harness Desktop.app` 拖进 `/Applications` 即完成"安装"，双击即用——
  无安装器、无依赖。**但分发给别人的机器时有一个平台硬限制**：APP 默认只有 ad-hoc 签名，
  在 Gatekeeper 开启（系统默认）的机器上，从网络下载来的 APP 首次打开会被拦。
  接收方的三种解法（由易到正规）：
  1. 右键 APP → "打开"（首次放行一次即可，之后正常双击）；
  2. 系统设置 → 隐私与安全性 → 找到被拦提示 → "仍要打开"；
  3. 正式分发：用 Apple Developer 账号签名（`codesign` + `notarytool` 公证）后，Gatekeeper 直接放行。
  开发者本机构建的 APP（无 quarantine 属性）不受此限制。
- **Windows**：CI 产出 `.msi` 安装器（双击安装）——这是 Tauri 在 Windows 上的标准形态；
  需要免安装便携版时可自行改为 NSIS portable 目标。
- **Linux**：`.AppImage` 天然免安装（下载加执行权限即用）；`.deb`/`.rpm` 走包管理器。

### 更新能力的实测结论（2026-09-17）

随包工具链在隔离副本上实测：`pnpm view @deepseek-ai/dsh version` 能查到官方 registry
当前最新版（当时为 0.1.5-rc.1，与随包版本一致）；对 vendor 目录做同版本
`pnpm install` 全量重装成功（1m22s，全部 postinstall 通过，闭包完整）。
即：官方发布新版本后，APP 内更新通道（查版本 → staging 安装 → 校验 → 原子换入）
所需的每一段链路都是通的。

---

## 七、目录结构

```
.
├── scripts/
│   ├── vendor-runtime.sh     # 生成自包含 payload（Node + DSH 闭包 + pnpm）
│   ├── make-icons.mjs        # 零依赖生成图标（png/ico/icns）
│   ├── smoke-payload.mjs     # 端到端验证 payload：解包→起服→取 UI
│   ├── dev.sh                # 本地一键：准备 payload + 编译 + 运行
│   └── debug-token.mjs       # 排查 token/会话交换（开发辅助）
├── src-tauri/
│   ├── src/
│   │   ├── lib.rs            # Tauri 应用：splash 事件、窗口创建、IPC 命令
│   │   ├── config.rs         # 路径解析（数据目录、payload、DSH_HOME、设置）
│   │   ├── payload.rs        # payload 定位/校验/解包（tar+zstd/gzip，带 swap 与回滚）
│   │   ├── runtime.rs        # node 定位、环境构造、启动 DSH、解析 URL
│   │   └── update.rs         # 版本检查与更新（staging + swap + 回滚）
│   ├── capabilities/         # 权限：只给 splash 窗口，DSH 窗口零 Tauri 权限
│   ├── icons/                # 生成物
│   ├── payload/              # 构建产物（.gitignore）
│   └── tauri.conf.json
├── ui/                       # splash 界面（纯 HTML/JS，无框架）
└── .github/workflows/build.yml
```

### 运行时目录（首次启动自动生成）

| 平台 | 数据目录 `<app-data>` |
| --- | --- |
| macOS | `~/Library/Application Support/ai.deepseek.harness.desktop/` |
| Windows | `%APPDATA%\ai.deepseek.harness.desktop\` |
| Linux | `~/.local/share/ai.deepseek.harness.desktop/` |

```
<app-data>/
├── runtime/node/       # 随包 Node
├── runtime/pnpm/       # 随包 pnpm
├── runtime/bin/pnpm    # 垫片
├── runtime/.extracted  # 解包版本戳（payload 变了才重解包）
├── vendor/             # DSH 安装（可被任意包管理器更新）
└── settings.json       # 可选：覆盖 profile/端口等
```

---

## 八、本地开发与构建

前置：Node.js ≥ 20、pnpm、Rust（rustup）、平台编译工具链（macOS 需接受 Xcode 许可：
`sudo xcodebuild -license accept`）。

```bash
# 一键：准备 payload → 生成图标 → 编译 → 运行
bash scripts/dev.sh

# 只准备产物（payload + 图标）
bash scripts/dev.sh --build

# 出安装包
bash scripts/dev.sh --release
```

单独的分步命令：

```bash
node scripts/make-icons.mjs                     # 图标（首次）
bash scripts/vendor-runtime.sh                  # payload → dist/
node scripts/smoke-payload.mjs                  # 端到端验证 payload（不起 GUI）
mkdir -p src-tauri/payload && cp dist/dsh-payload-* src-tauri/payload/
cd src-tauri && cargo tauri build               # 安装包
```

常用参数：

```bash
# 指定 DSH / Node 版本与目标平台
bash scripts/vendor-runtime.sh --version 0.1.5-rc.1 --node 26.7.0 \
     --platform linux --arch x64 --out dist

# 不随包分发 Node（改用系统 node，体积更小、但不再"完全独立"）
bash scripts/vendor-runtime.sh --skip-node

# 使用国内 Node 镜像
DSH_NODE_MIRROR=https://npmmirror.com/mirrors/node bash scripts/vendor-runtime.sh
```

### 环境变量

| 变量 | 作用 |
| --- | --- |
| `DSH_DESKTOP_PAYLOAD` | 指定 payload 目录（开发/CI 用，覆盖资源目录） |
| `DSH_DESKTOP_DATA` | 指定数据目录（测试隔离用，不动真实用户目录） |
| `DSH_NODE_MIRROR` | 替换 Node 下载源 |
| `DSH_SKIP_PNPM=1` | 生成 payload 时不打包 pnpm |

---

## 九、运行时行为

**启动顺序**：解析路径 → 校验 payload（有 `.json` 校验和时核对 sha256）→ 需要时解包到 staging 再换入
→ 定位 node/DSH → （可选）检查更新 → 启动 DSH 并等它打印 URL → 创建 main 窗口加载该 URL → 关闭 splash。

**安全边界**：

- DSH 服务器只绑 `127.0.0.1`（`--host 0.0.0.0` 被上游主动拒绝）；UI 需要带一次性 token 换取
  `HttpOnly; SameSite=Strict` 会话 Cookie，无 token 请求返回 401 —— 外壳只是把浏览器的这段流程交给系统 WebView。
- **main 窗口不获得任何 Tauri 权限**（`capabilities/splash.json` 只覆盖 `splash`），
  且窗口导航被限制在 loopback：Harness 页面里出现的任何外部链接都无法把应用窗口带到站外。
- **关窗 ≠ 退出**：点红色按钮只是隐藏窗口（服务与长任务继续跑），
  ⌘Q / Dock 退出才会优雅关停 DSH 子进程；Dock 点图标（或再次启动）会重开窗口。

**窗口与高分屏（重要）**：

窗口几何（尺寸/位置/是否最大化）记在 `<app-data>/window.json` 里，**单位是逻辑点**，
还原前按当前显示器夹取（换屏、换缩放、拔插外接屏都不会再压扁界面）。

- 刻意**不使用** `tauri-plugin-window-state`：它把 `inner_size()`（**物理像素**）原样存盘，
  再用 `set_size(PhysicalSize)` 还原。于是在 2x 高分屏上，"上次在 1x 屏记下的 1778×1200"
  会被还原成 889×600 **逻辑点**——界面挤压、启动页底部被裁。逻辑点存储从根上消除了这类问题。
- 主窗口最小逻辑尺寸 960×640（更窄时 DSH 界面会明显挤压），默认 1280×860。
- 同一份数据目录**只允许一个实例**（`<app-data>/app.pid`）：两个实例抢同一个 `$DSH_HOME`
  会互相覆盖会话索引/设置/凭据。第二个实例会把已有窗口带到前台后自己退出；
  自测需要双开时设 `DSH_DESKTOP_ALLOW_MULTI=1`。

**桌面端适配层**：主窗口页面加载完成后，外壳会 `eval` 一段 CSS（`DESKTOP_CSS`），
只修 DSH Web UI 在桌面窗口里暴露出来的确切问题。目前一条：设置面板左侧导航
（`<div role="dialog">` 的 `<nav>`）在插件较多时会溢出，而父级是 `overflow: hidden`——
后装插件贡献的设置项（自动续跑/侧边卡片/使用统计/会话归档管理…）既看不到也点不到；
补 `overflow-y: auto` 后即可滚动。放在外壳里而不是改 DSH 的文件，是因为 DSH 会自更新，
改它的 assets 会被覆盖，而外壳的注入每次加载都重新执行、能跟着 DSH 一起升级。

**设置**（`<app-data>/settings.json`，可选，全部有默认值）：

```json
{
  "profile": "web",
  "port": 0,
  "host": "127.0.0.1",
  "autoUpdate": true,
  "updateOnFirstBoot": false,
  "shareDshHome": false,
  "extraArgs": []
}
```

- `updateOnFirstBoot`：默认 `false`。开启后首次启动会**先**尝试更新再起服务（仅首次解包时）；
  平时更新由界面/命令触发，避免每次启动都等网络。
- `shareDshHome`：默认 `false`——APP 完全独立，使用私有的 `<app-data>/home` 作为 `$DSH_HOME`，
  与本机其他 DSH 实例零耦合（端口由 `port: 0` 让 OS 分配空闲端口，锁/profile 各自独立）。
  改为 `true` 才会共用 CLI 的 `~/.dsh`（共享会话与凭据，但也会共享锁竞争：
  若某 DSH 进程崩溃留下 `~/.dsh/profiles/node_modules.lock` 孤儿锁，需手动删除该文件才能再启动，
  这是 `dsh-atomic-write` 的设计——竞争方永不替他人清锁）。
- 首次使用私有 home 需在 APP 内重新配置模型凭据（与 CLI 登录态不互通，这是"完全独立"的代价）。

**应用内命令**（供界面使用，只在 splash 窗口可用）：

| 命令 | 作用 |
| --- | --- |
| `get_status` | phase / URL / DSH 版本 / node 版本与来源 / 是否完全自包含 / 当前包管理器 / 数据目录 / DSH_HOME / 日志尾 |
| `get_log` | 诊断日志尾 |
| `check_for_update` | 查 npm registry，比较版本 |
| `install_update` | 安装最新版（staging + swap + 回滚） |
| `restart` | 重启应用（换用新版本） |
| `open_data_dir` | 打开数据目录 |

---

## 十、已知取舍与后续项

1. **安装包体积**：约 100 MB/平台（Node ~50 MB + DSH 闭包压缩后 ~45 MB）。
   想瘦身可换成系统 Node（`--skip-node`，但会失去"零依赖"）或按需剔除 DSH 闭包里用不到的重依赖。
2. **Windows/macOS 签名**：CI 目前产出未签名包。分发前需配置
   Apple Developer ID + 公证、Windows 代码签名；`tauri.conf.json` 已预留位置。
3. **payload 不进版本库**：可复现、体积大，由 CI 每次构建（可选加构建缓存）。
4. **DSH 自己的 "desktop profile"**：上游 `lib/bin.js` 保留了一个由官方 Electron 应用托管的
   `desktop` profile 名，本外壳不使用它（用 `web` profile + WebView 嵌入），以免与上游实现耦合。
5. **更新触发时机**：目前是"检查后由界面/命令触发"。如需无人值守自动更新，
   可在 `bootstrap` 里把 `updateOnFirstBoot` 默认打开，或在 main 窗口关闭时后台更新。

---

## 十一、许可

外壳代码 MIT。随包分发的 Node.js（MIT）与 `@deepseek-ai/dsh` 及其依赖
各自遵循其原始许可，见 payload 内各包的 `LICENSE`。
