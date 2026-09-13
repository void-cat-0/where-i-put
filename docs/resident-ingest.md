# 常驻 ingest 设计草案（resident ingest）

> 状态：**草案，未实现**。评审通过后再动代码。
> 上位依据：`README.md` 的 Roadmap 第 1 条把常驻化写成"前置条件，不是可选项"
> ——消失时刻与覆盖事件是只有连续观测才能产生的时间事实；`AGENT-MEMORIES/`
> 的进度快照记「下一步开发 = 常驻化」。
> 本文所有对现状的判断都以代码为准，行文里的 `file:line` 是评审时的锚点。

## 0. 目标与非目标

**要做成的**：一个能连续跑一周以上、不需要人管、且"还活着"这件事可被外部读到的
ingest 进程。它是数据模型 v2 与 query 侧规则引擎的**数据前提**。

**全平台是一等约束，不是事后移植**：本项目定位为完整的跨平台项目（README 已写明），
所以本文所有设计都必须能在 Windows / Linux / macOS 上落地，平台差异要**显式写出**
而不是默认其中一个。凡涉及平台行为的段落（§1 锁、§3 信号与路径、§5/§6 原子写、
§7/§8 服务化、§9 缺口）都自带平台注记；代码里只允许在**真正平台特定**处写
`#[cfg(...)]`（信号类型、锁实现等），不接受"Windows 能跑就行"的捷径。

**明确不做**（留给后续阶段，避免一次改太多）：

- 不落库逐帧检测、不加 events 表、不改 `observations` schema —— 那是 Roadmap
  第 2 步（数据模型 v2）。本阶段只在**内存里**做事件识别 + 旁路日志（§6），
  为 v2 攒证据。
- 不做覆盖/容纳推断规则本身（Roadmap 第 3 步）。
- 不做 WebUI 证据卡（Roadmap 第 4 步）。
- 不引入数据库服务化（不换 Postgres/不做独立 DB 进程）：单机单写者 SQLite 足够。

## 1. 现状盘点：为什么现在不能常驻

| 现状 | 位置 | 常驻时的后果 |
|---|---|---|
| `--rtsp` / `--webcam` / webhook 三个模式 `return` 互斥 | `crates/item-ingest/src/main.rs` `main()` 里的模式分派（`--demo`/`--detect`/`--rtsp`/`--webcam` 依次 `return`，都没命中才落到 Frigate webhook） | webhook 与相机闭环**不能同进程**；Frigate 相机与自接 RTSP 相机不能合并成一个 daemon |
| 相机闭环是前台死循环，无退出条件（`--frames 0` 默认） | `crates/item-ingest/src/main.rs` 的 `camera_pump` | 无法优雅停机；杀进程=硬断，正在写的 observation 无收尾 |
| 无单实例保护 | — | 误起第二个进程 → 两个写者对同一 WAL 库；`--listen` 会 bind 失败，但纯相机模式会静默双写。**跨平台手段**：原子创建 `data/ingest.lock`（`OpenOptions::new().create_new(true)`，底层分别是 Unix `O_EXCL` / Windows `CREATE_NEW`，语义一致；**不用文件锁**——`flock` 与 `LockFileEx` 语义不同且 Windows 侧是强制锁），文件内写 pid 与启动时间；已存在则启动即拒绝并报出持锁 pid。`--maintenance` 离线子命令要能明确跳过它 |
| 只有 `tracing` 日志，无"活着的读数" | 全局 | 外部无法回答"它还在工作吗"；断流/写失败只能靠翻日志 |
| 无保留策略 | — | `data/items.db` + `data/snapshots/` 单调增长（每新 observation 一张 JPEG，q75） |
| 每次启动靠长命令行 | `crates/item-ingest/src/main.rs` 的 `struct Args` | 多相机、区域、检测参数无法固化；`config.rs` 只有 `id/url/regions` 三个字段，`CameraConfig.url` 目前**根本没被消费**（`seed_regions` 只用 `regions`） |
| 断线重连已有，但退避是固定 2s | `camera_pump` 内三处 `std::thread::sleep(Duration::from_secs(2))`（建连失败 / detector 失败 / 重连前） | 摄像头长期拔线时每 2s 重试刷屏；需要指数退避 + 上限 |

已有的**好底子**（不用重做）：source 层的 reconnect 循环、detector 错误只跳帧不杀
循环（`camera_pump` 里 `detector.detect` 的 `Err` 分支）、`Store` 的 WAL 打开与幂等 `seed_regions`、写库路径全部
收敛在 `ingest_detections` / `Store::record_sighting` 一个点上。

## 2. 架构分层：先拆三种任务

现在 `camera_pump` 一个函数同时承载"拉帧""检测""落库""快照"四件事，且和 CLI
`Args` 强耦合（直接读 `args.camera_id` / `args.snapshots_dir` / `args.frames`）。
常驻化第一步是**把任务描述与任务执行分离**：

```
CameraTask {            // 纯数据，来自 config/CLI，无副作用
    camera_id, source: SourceSpec, detector: DetectorSpec,
    detect_fps, regions(仅校验用，真正播种在 store),
    snapshot_dir, enabled
}
        ↓ build
CameraRunner {          // 拥有一个 FrameSource + 一个 Detector
    open() -> Result<()>            // 建 source（COM 亲和：必须在跑它的线程上调用）
    step(&mut self) -> StepOutcome  // 拉一帧/节流/检测/落库，返回本步事实
    health(&self) -> CameraHealth   // 没帧多久了、连续失败几次、累计帧数/记录数
}
        ↓ 被
Supervisor              // 一相机一线程 + 退避重启 + 优雅停机标志
```

要点：

1. **`step()` 必须是可被反复调用的短步骤**，不允许内部无限循环——这样停机标志
   检查点、健康上报点、事件检查点都在同一个位置（现在这些都散在 `camera_pump`
   的双层 `loop` 里）。
2. **一个相机一个 OS 线程**，沿用现有约定：`make_source` 与每次 `next_frame` 都
   必须在同一个线程上（nokhwa 的 COM 亲和，`source::nokhwa` 已注明）。
3. **进程内不做多相机共享线程池**。检测本身是阻塞重活（YOLO 720p ~280ms、VLM
   数秒），并行相机只是互相抢 CPU；一相机一线程最直白，也最容易定位问题。
4. `Supervisor` 只做三件事：拉起线程、看 `stop` 标志、按退避策略重启退出的线程。
   线程**不 panic 逃逸**：`step()` 的错误分类后返回，致命错误（如模型文件缺失）
   让该相机进 `failed` 状态并停止重启，而不是无限刷日志。

## 3. 统一 daemon 与并发模型

目标形态（**一个进程同时干三件事**，这是当前最大的结构性改动）：

```
item-ingest (常驻)
├── tokio 多线程 runtime（主线程）
│   ├── axum: /frigate/webhook   (8477，可关)
│   ├── axum: /preview, /preview.mjpg, /preview.jpg  (同端口，rtsp feature，可关)
│   └── 健康文件写入任务（每 15s，§5）
└── 相机 supervisor 线程（每个 enabled 的 camera 一条）
    └── CameraRunner.step() 循环 → ingest_detections → Store
```

**共享 `Store` 用 `Arc<Mutex<Store>>`**（就是 `crates/item-ingest/src/frigate.rs`
的 `pub type State`）。理由与约束：

- `rusqlite::Connection` 是 `Send` 非 `Sync`，`Mutex` 是既定解法；两条写入路径
  （webhook 异步任务 / 相机线程）合流到一个 `Mutex` 上。
- **绝不在持锁期间做重活**：JPEG 编码、标注、快照落盘现在就在锁外
  （`ingest_detections` 里 `annotate` + `write_snapshot` 是纯内存/IO，只把
  `set_sample_snapshot` 那个 UPDATE 进锁），这个性质必须保持。
- **绝不在持锁期间 `.await`**：`frigate.rs` 的 `pub type State` 上方已有此注释，常驻化后由 review 守住。
- 写入频率的现实上界：yolo 1 fps/相机、VLM 0.2 fps/相机。**单帧落库是毫秒级**，
  几条相机串行在 `Mutex` 上完全够用；真要上到几十路再谈 `Store` 连接池，现在
  属于过早优化（记在 §9 待观察项）。

**停机的正确形态**：主线程捕获停机信号（需自己处理平台差异，见下表；tokio
`signal` feature 已在 `Cargo.toml` 里），置 `Arc<AtomicBool> stop`，然后：

1. 停止接受新请求（`axum::serve` 用 `with_graceful_shutdown`）；
2. 等相机线程**在 `step()` 边界**退出（给 10s 上限，超时则记 warn 并放弃）；
   Unix 上第二次信号 = 立即强杀（留给运维的逃生门）；
3. 收尾：最后扫一次事件表，flush 并 close `health.json` / `events.jsonl`，再写一次
   最终健康快照 + `PRAGMA wal_checkpoint(TRUNCATE)`；
4. 退出码 0。

**信号是平台差异点，不能一句 `Ctrl-C` 带过**：

| 平台 | 优雅停机信号 | 说明 |
|---|---|---|
| Windows | `Ctrl-C`、`Ctrl-Break`（控制台事件） | **没有 `SIGTERM`**；`tokio::signal::unix` 在非 Unix 上根本不存在，必须 `#[cfg(unix)]` 分开写 |
| Unix（Linux/macOS） | `SIGTERM`（服务管理器发的就是这个）、`SIGINT` | 额外处理 `SIGHUP`（终端断开）；systemd 默认发 `SIGTERM`，超时后才 `SIGKILL` |

因此实现应**双路 select**：`tokio::signal::ctrl_c()`（全平台）+ `#[cfg(unix)]`
`tokio::signal::unix::signal(SignalKind::terminate())`，而不是只等 Ctrl-C。

**cwd 与相对路径（三平台都会踩）**：服务管理器（Task Scheduler / systemd /
launchd）决定进程 cwd，而 `docs/detection-flow.md` 已记录"换目录启动 web 会导致
`has_snapshot=false`"。常驻版必须在启动时把 `db` / `snapshots_dir` / `health_file`
/ `events.jsonl` 全部**规范化为绝对路径**（相对路径按**配置文件所在目录**解析，
而不是按 cwd）再进任何子系统——这同时修掉既有的相对路径坑。

## 4. 配置：从 `Args` 到 `config.toml`

现有 `config.rs` 太小（`CameraConfig` 只有 `id/url/regions`，且 **`url` 是死字段**：
`seed_regions` 只读 `regions`）。常驻化的配置面要覆盖"跑一周"所需的一切。

目标形状（向后兼容：现有 `detection-flow.md` / README 里的 config 片段继续有效）：

```toml
[daemon]
db = "data/items.db"
snapshots_dir = "data/snapshots"
health_file = "data/health.json"
rescan_config_secs = 0          # 0 = 不热加载（阶段 2 先不做热加载）
checkpoint_secs = 300

[webhook]
enabled = true
listen = "127.0.0.1:8477"
preview_url = ""                # 非空则开 /preview（rtsp feature）

[[camera]]
id = "living"
enabled = true
url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"  # 空=仅 webhook 供给
detector = "yolo"               # 覆盖全局默认
detect_fps = 1.0                # 覆盖全局默认（vlm 仍是 0.2）
targets = ""                    # detector = "vlm" 时生效
[camera.regions]
desk = [0.0, 360.0, 1280.0, 720.0]
```

**优先级（从低到高）**：内置默认 → `config.toml` → 环境变量
（`ITEM_VLM_BASE_URL`/`ITEM_VLM_MODEL`）→ **显式 CLI 参数**。

这条规则是常驻化的关键：现在 CLI 参数一律 `default_value` 写死（如
`--detect-fps` 是 `Option` 但 `--cameras-id` 默认 `rtsp-0`），配置文件一旦引入就
必须能分辨"用户显式给了"和"用了默认"。实现手段：把 `Args` 里需要被 config 覆盖
的项全部改成 `Option<T>`，`main` 里做一次三方合并，产出 `RuntimeConfig`（纯数据、
可打日志、可单测）。**CLI 与 config 的合并逻辑必须有单测**——这是最容易悄悄写错的
地方。

CLI 形态的变化（保持旧用法可用）：

- 保留 `--rtsp/--webcam/--camera-id/--detect-fps/...`：**单相机一次跑**的旧用法
  继续工作（等价于构造一个临时 `CameraTask`）。
- 新增 `--config-file <path>` 之外的常驻入口：`item-ingest --daemon
  --config config.toml`（名字待定，见 §8 待定项）。
- `--demo` / `--detect <img>` 保持"跑完即退"的一次性语义，不参与常驻。

## 5. 外部可观测：健康快照文件

**选文件不选 HTTP 端点**，理由是硬的：`reqwest` 是 `item-ingest` 的 `vlm` feature
可选依赖（`crates/item-ingest/Cargo.toml` 的 `[dependencies.reqwest]`），`item-web` 侧没有 HTTP 客户端；而
健康检查是给"人和 `item-web`"看的，15s 粒度足够。文件方案零新依赖、零端口、
只读方随时可读。

`data/health.json`（每 15s 原子重写：写 `.tmp` → rename；**原子写的平台陷阱见下**）：

```json
{
  "schema": 1,
  "pid": 12345,
  "started_at": "2026-09-09T02:00:00Z",
  "updated_at": "2026-09-09T02:37:15Z",
  "seq": 148,
  "detector_default": "yolo",
  "cameras": [
    {
      "id": "living",
      "state": "running",
      "source": "rtsp://192.168.1.64:554/Streaming/Channels/102",
      "reconnects": 3,
      "frames": 210344,
      "detections": 21033,
      "recorded": 87,
      "last_frame_at": "2026-09-09T02:37:14Z",
      "last_frame_age_s": 1,
      "last_error": null,
      "inference_ms_ewma": 284.1
    }
  ]
}
```

判活协议（写进 README，别让使用者猜）：

- `updated_at` 与当前时间差 > **90s** ⇒ 进程死了或被挂起。这是**唯一**的判活依据。
- `pid` 字段**只作辅助信息，不参与判活**：Windows 要 `OpenProcess`、Unix 是
  `kill(pid, 0)`，没有可移植的 std API；且 Unix 的 pid 会被复用，"pid 还在"可能
  指向另一个进程，据此判定会误报"活着"。
- `cameras[].state`：`starting`（建 source 中）/ `running` / `reconnecting`
  / `failed`（放弃重启，需人工介入）/ `stopped`。
- `last_frame_age_s` 持续大于该相机 `detect_fps` 的 3 倍 ⇒ 解码侧卡住。

**原子写的平台陷阱**：`.tmp` → rename 覆盖，在 Windows 上若目标 `health.json`
正被 `item-web` 打开（哪怕只读）会失败（共享冲突），Unix 不受影响。实现要求：
rename 失败时短退避重试（如 3 次），仍失败只 warn 且**不阻塞主循环**；也可先试
删除目标再重命名。契约是"文件可能短暂缺失或偏旧"，读取方（含 `item-web`）必须
容忍读不到/读到旧值，而不是当成 fatal。

`item-web` 侧只做**最小**接入（可选，第二阶段）：读 health 文件，在页头显示
"ingest 活/死 + 最后更新"，死了给醒目提示。不做实时推送、不做进程管理。

顺带一项长期缺失的可观测性：**当前 `recorded > 0` 才打日志**
（`crates/item-ingest/src/main.rs` 的 `camera_pump`），意味着"跑了 6 小时但一次都没记录"和"进程正常工作"在日志里
长得一样。常驻版必须每 N 分钟打一条心跳（含 `frames/detections/recorded` 与
各相机 `last_frame_age_s`），让"静默"永远不等于"正常"。

## 6. 事件观测（本阶段的内存态，为 v2 铺路）

目标：**在不改 schema 的前提下产出真实的时间事实序列**，这样 v2 的 events 表
设计能拿真数据验证，而不是拍脑袋。

做法（全部在 `Supervisor`/`CameraRunner` 内存里，`item-core` 不动）：

- 每次 `step()` 之后，用 `ingest_detections` 的返回值和 store 的最近状态维护一张
  内存表：`(camera_id, zone, label) -> {obs_id, last_hit_at, hit_count_at_last_hit}`。
- **appeared**：`record_sighting` 返回 `is_new = true` 时。
- **disappeared**：某键在 `last_hit_at + gap`（默认 `max(30s, 3/detect_fps)`）内
  没有新命中。判定发生在"下一次扫描时"，因此**停机前必须扫一次**（§3 收尾步骤 3
  之前插一步），否则最后一次真实消失会丢。
- **covered/moved**：**本阶段不产出**。它们需要空间关系（谁挡住了谁、容器位移），
  属于 v2 + 规则引擎。这里只保证事件流格式能容纳它们。

落地形式：追加式 JSONL `data/events.jsonl`，一行一事件、可被 jq/grep 直接消费：

```json
{"ts":"2026-09-09T02:31:02Z","camera":"living","zone":"desk","label":"keys",
 "obs_id":87,"event":"disappeared","hits":64,"seen_for_s":128.4}
```

轮转：单文件超 64MB 就 rename 为 `events.jsonl.1`（最多留 2 个历史文件）——常驻
一周的事件量必须先有界，再谈入表。rename 的 Windows 占用陷阱与重试要求同 §5
（日志文件常被 `tail`/编辑器占用）。**这个文件是"观测"，不是"真相"**：v2 落地后，
events 表是权威，JSONL 降级为调试产物。

风险提示：`record_sighting` 现在把"是否新行"作为 `is_new` 返回，但**合并路径不
返回"这次命中的是哪一行"** 之外的线索（`crates/core/src/store.rs` 的 `Store::record_sighting`）。若内存表与 DB 状态
不一致（例如手工删过行、或同 label 在 dedup 窗口边缘反复），事件会漂。缓解：
内存表以 `obs_id` 为准，`obs_id` 变了就当"旧的 disappeared + 新的 appeared"
两条事件——语义仍然正确，只是粒度变粗。

## 7. 数据体量与保留策略

常驻一周后必须回答"库会不会撑爆"。策略（默认值都可配）：

| 项 | 策略 | 默认 |
|---|---|---|
| observations | 按 `last_seen` 保留窗口；过期行删除，**连带删除其快照 JPEG** | 90 天 |
| snapshots 目录 | 不单独扫描孤儿文件；删除行时精确删对应文件；每 7 天做一次孤儿对账（DB 无该 id 就删文件） | 开启 |
| WAL | 每 `checkpoint_secs` 一次 `PRAGMA wal_checkpoint(TRUNCATE)` | 300s |
| VACUUM | **不自动做**（会长时间持写锁）。提供 `item-ingest --maintenance` 手动子命令 | 手动 |
| events.jsonl | 见 §6 轮转 | 64MB×3 |
| 平台注记 | **Windows** 无法删除/重命名被打开的文件，所以"删行+删图"（本节）与 §5/§6 的 rename 在该平台都要"重试 + 失败不致命"；`--maintenance` 在 Windows 上要求先停 daemon（无 `fork`），Unix 则可在 WAL 仍开着时做 checkpoint | — |

保留策略的**硬约束**：清理必须发生在推断规则用到这些数据之后。所以 v2 的规则引擎
上线前，保留窗口默认取大（90 天）；规则引擎上线后按"最长容器滞留时间"反推。
这一条要写进代码注释，否则将来有人把默认值调小会静默毁掉推断证据。

删除必须**事务内先 DELETE 行再删文件**（文件删除失败只 warn）：宁可留孤儿 JPEG，
不可留"指向已删文件的行"。

## 8. 服务化：三平台开机自启

三平台的服务形态完全不同，**不存在一个跨平台的"服务化"实现**；但"怎么让进程活
着"这一层之上，daemon 自身的行为（单实例锁、优雅停机、健康文件、异常退出痕迹）
三平台一致。本节给三份模板，路径/workdir/账号按部署填。

### 8.1 三平台对照

| | Linux | macOS | Windows（本机，见 `AGENT-MEMORIES/`） |
|---|---|---|---|
| 服务管理者 | systemd | launchd | Task Scheduler / `sc` / NSSM |
| 配置文件 | `/etc/systemd/system/item-ingest.service` | `~/Library/LaunchAgents/*.plist`（登录级）或 `/Library/LaunchDaemons/*.plist`（系统级） | `schtasks /Create` 或 `sc create` |
| 开机自启 | `systemctl enable`（`WantedBy=multi-user.target`） | `RunAtLoad` + `launchctl bootstrap` | `/SC ONSTART` 或服务启动类型 auto |
| 崩溃拉起 | `Restart=on-failure` + `RestartSec` | `KeepAlive` | 任务"失败后重启"，或 NSSM 自带 |
| 优雅停机信号 | `SIGTERM`（默认） | `SIGTERM`（宽限期由 `ExitTimeOut` 控制） | 无 `SIGTERM`；走控制台事件（NSSM 会转发） |
| 账号/权限 | `User=`（建议专用系统用户） | Daemon=root；Agent=登录用户 | 服务账号或登录用户 |

### 8.2 Linux（systemd）

```ini
# /etc/systemd/system/item-ingest.service
[Unit]
Description=where-i-put resident ingest (cameras -> observations)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=whereiput
WorkingDirectory=/opt/where-i-put
ExecStart=/opt/where-i-put/item-ingest --daemon --config /etc/where-i-put/config.toml
Restart=on-failure
RestartSec=5
# 给 §3 的 10s 相机退出 + 收尾留余量
TimeoutStopSec=30
Environment=RUST_LOG=info
ProtectSystem=strict
ReadWritePaths=/opt/where-i-put/data

[Install]
WantedBy=multi-user.target
```

启用：`systemctl daemon-reload && systemctl enable --now item-ingest`；看日志
`journalctl -u item-ingest -f`（**不必**重定向到文件，journald 就够）。

### 8.3 macOS（launchd）

```xml
<!-- ~/Library/LaunchAgents/com.whereiput.ingest.plist（登录级，推荐） -->
<!-- 需开机即起、不依赖登录 → 改放 /Library/LaunchDaemons/ 并用 sudo 加载 -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.whereiput.ingest</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/where-i-put/item-ingest</string>
    <string>--daemon</string>
    <string>--config</string>
    <string>/opt/where-i-put/config.toml</string>
  </array>
  <key>WorkingDirectory</key><string>/opt/where-i-put</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ExitTimeOut</key><integer>30</integer>
  <key>StandardOutPath</key><string>/opt/where-i-put/data/ingest.out.log</string>
  <key>StandardErrorPath</key><string>/opt/where-i-put/data/ingest.err.log</string>
</dict>
</plist>
```

加载：`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.whereiput.ingest.plist`；
卸载 `launchctl bootout …`。两个坑：launchd **不会创建日志目录**（`data/` 必须先
存在）；`KeepAlive` 下崩溃会被立刻拉起，失败原因只能看 `StandardErrorPath`。

### 8.4 Windows

- **本机推荐 A：计划任务**。零安装、无需常驻服务，可直接用现有 `cargo run`/
  release 二进制路径；配"失败后重启（1 分钟间隔，最多 3 次）"，触发方式按需求选
  "用户登录时"或"计算机启动时"（后者需管理员 + 专用账号）。
- **备选 B：NSSM / `sc create`**。真正的服务语义（开机即起、不依赖登录），但引入
  外部依赖、需要管理员权限、日志重定向要额外配。机器上未验证过 NSSM 是否可用，
  所以默认走 A。

### 8.5 三平台共用的约束

- **两级兜底，各自独立**：服务管理器拉起整个 daemon；daemon 内的 `Supervisor`
  拉起单个相机。不要指望一级。
- **自愈前提已满足**：`seed_regions` 幂等、`Store::open` 会跑 WAL 恢复。
- **"上次异常退出"的痕迹**：启动时若 `health.json` 的 `updated_at` 很新、但**本次
  进程未持有锁**（§1）⇒ 记 `tracing::warn!("previous run died without shutdown")`。
  判定必须用锁文件，不能用 `kill(pid, 0)`（§5：pid 不可移植且会被复用）。
- **日志去向各平台不同**（journald / launchd 文件 / Windows 重定向），但**内容契约
  一致**（心跳、重连、异常退出），别让某个平台的日志缺一块。

## 9. 已知限制 / 本阶段不解决的

- **单写者**：`Mutex<Store>` 串行化所有写入。相机路数到两位数时要重新评估
  （见 §3 待观察项）。
- **VLM 检测期间不读帧**（`docs/vlm-sidecar.md §7`）：`step()` 是同步的，
  grounding 在途时 RTSP 会积压。常驻化**不会**顺手修这个——异步化需要
  `Detector` trait 变 async 或加解码缓冲，是独立议题。常驻版的缓解是：VLM 相机
  的 `detect_fps` 天生低（0.2），积压由 `RtspSource` 的丢帧行为兜住。
- **不热加载配置**：改 `config.toml` 要重启进程（`rescan_config_secs = 0`）。
  阶段 2 再考虑；先让"重启"变成一条命令的事。
- **不做多机/分布式**：一个 daemon 一台机。跨机聚合留给未来的读取侧。
- **不含 Frigate 事件的时间事实**：Frigate 路径只有离散事件，无法观测"消失"
  （没有连续帧）。同一相机**要么**走 Frigate **要么**走本地检测，不要都开
  ——这条要写进 README，否则会出现"同一物体两条 observation 互相打架"。
- **平台缺口（与 §0 的全平台约束一并看）**：
  - **全平台 CI 尚未覆盖常驻**：现有 CI 只跑 `cargo check`/`test`（三平台 runner），
    §10 的 P1–P5 验收需在 Windows / Linux / macOS 各跑一遍才算真的全平台。
  - **`rtsp` 在 macOS 需另走 setup**：BtbN 预编译包只有 Windows/Linux，macOS 要
    `brew install ffmpeg` + 系统 LLVM（README 已写，`cargo xtask setup` 会打提示），
    版本仍需 ≤ 7.x（rust-ffmpeg 9.x 拒绝 FFmpeg 8）。即"常驻 + RTSP"在 macOS 上
    依赖外部 FFmpeg。
  - **`camera`（nokhwa）三平台可用但设备索引不可移植**：Windows DirectShow /
    Linux V4L2 / macOS AVFoundation，"`--webcam 0`"在三平台指向不同物理设备，
    配置里不要把它当可移植值。

## 10. 分阶段实施与验收

| 阶段 | 内容 | 验收（可机械核对） |
|---|---|---|
| P0 纯重构 | 拆 `CameraTask`/`CameraRunner`/`Supervisor`；`Args` → `RuntimeConfig` 合并 + 单测 | 现有 `cargo test` 全绿；`--rtsp` 旧命令行为不变；`cargo run -p item-ingest -- --demo` 仍通过 |
| P1 常驻骨架 | 单进程同时跑 webhook + preview + 多相机；优雅停机（10s 内退出、退出码 0）；单实例锁；路径绝对化 | 起两个实例，第二个拒绝启动并报出持锁 pid；停机信号后无残留进程、`health.json` 的 `updated_at` 是最终值；**从任意 cwd 启动**快照仍能被 item-web 显示 |
| P2 可观测 | health.json 原子写（含 rename 重试）+ 心跳日志 + 退避重连（指数退避，上限 60s） | 拔掉摄像头 5 分钟：health 显示 `reconnecting`、`reconnects` 递增、日志不刷屏；插回后 `state` 回 `running`、`last_frame_age_s` 归零；**Windows 上** item-web 持续打开 health 时写入仍不报错（重试生效） |
| P3 有界增长 | 保留窗口清理 + 快照对账 + 定期 checkpoint + `--maintenance` | 造 1 万行 + 1 万张图，跑清理后行数与文件数同时下降且无"指向已删文件的 sample_snapshot"；**Windows 下**遇到被占用文件不中断清理 |
| P4 事件观测 | §6 的 appeared/disappeared + events.jsonl | 实测一次"物体离开画面"：JSONL 出现 `appeared` 后出现 `disappeared`，`seen_for_s` 与 `last_seen - first_seen` 一致（容差 1 个 detect 周期） |
| P5 服务化 | §8.2/8.3/8.4 的 systemd unit / launchd plist / Windows 任务三份模板 + 异常退出痕迹 | **三平台各验一次**：安装后重启机器 → 无需登录手工操作即恢复写入；强杀进程 → 被拉起，日志出现 `previous run died without shutdown`；服务管理器发停机信号 → 10s 内干净退出（Windows 走控制台事件路径） |

P0 与 P1 之间**不要夹带功能**：这一步是纯结构调整，混入行为改动会让"行为不变"
这个唯一的安全网失效。

## 11. 需要先拍板的决策点

1. **单进程 vs 一相机一 daemon**：本文推荐**单进程多相机**（共享 `Store`、
   一个 health 文件、一处停机逻辑）。代价是模型常驻内存、一个相机的崩溃面
   波及同进程其他相机（用 `Supervisor` 的 catch/隔离来缓解）。
2. **常驻入口的 CLI 形态**：`--daemon --config <file>`（推荐）还是"给了
   `--config` 就自动常驻"（会悄悄改变现有命令语义，不推荐）。
3. **保留窗口默认值**：90 天（推荐，为 v2 留足证据）还是更激进（如 30 天）。
4. **事件观测（P4）是否纳入本轮**：纳入就要接受 §6 的"内存表可能与 DB 漂移"
   风险；不纳入则 v2 的 events 表设计缺少真数据验证。
5. **服务化形式**：日常用临时启动器（前台/手动）够吗，还是一开始就装到
   systemd / launchd / 计划任务？三份模板已在 §8 写好，三种都可选；只影响"谁负责
   开机拉起"，不影响 daemon 自身设计。
