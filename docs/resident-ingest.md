# 常驻 ingest 设计草案（resident ingest）

> 状态：**已定案，P0–P5 全部落地并实机验证**（2026-09-13 / 09-14 / 09-22 / 09-25 / 09-26）。§11 的决策
> 已拍板；P0 的两半（结构拆分、`Args → RuntimeConfig` 四层合并）、P1（`--daemon` 常驻
> 骨架：单实例锁、健康快照、路径绝对化、优雅停机）、P2（健康文件原子写 + 心跳日志 +
> 指数退避）、P3（保留窗口清理 + 快照对账 + 定期 checkpoint + `--maintenance`）、
> P4（§6 的 appeared/disappeared 事件 + `data/events.jsonl`）与 P5（§8 三平台服务化模板 +
> 锁的心跳/接手 + 完整停机信号面）都已实现，见 §4 的六条落地注记与四份实机验证记录
> （含尚未验证的缺口）。
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
retention_days = 90             # 0 = 永久保留（关掉清理）
sweep_secs = 3600               # 清理扫描周期；0 = 只在 --maintenance 时做
reconcile_secs = 604800         # 快照孤儿对账周期（7 天）；0 = 从不扫描

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

**P0 落地注记（2026-09-13）**：上面这套合并已实现于 `item-ingest/src/config.rs` ——
`CliOverrides`（只记用户真正给过的值，所以 `Args` 里可被覆盖的项一律是 `Option<T>`）、
`EnvOverrides`、`RuntimeConfig::resolve`，九个单测逐层锁住优先级。一点与本文原文有出入，
按代码为准：

- **`camera_id` 不来自 config.toml**：它是旧命令行为帧归属的身份，让配置去改它会在
  同一条命令行下把 observation 写到别的相机上。`[[camera]]` 行只在 id 已确定后用来取
  该相机的 `detector` / `detect_fps` / `targets`。

**P1 落地注记（2026-09-13）**：`item-ingest --daemon --config <file>` 已实现，落在三个新
模块 —— `daemon.rs`（编排与停机顺序）、`lock.rs`（单实例锁）、`health.rs`（健康快照与
注册表）；`supervisor.rs` 拆出 `start` / `join(timeout)` 以支持 10s 停机预算；
`[[camera]]` 的 `url` 现在被消费（每行一个 `CameraTask`），`enabled = false` 的行跳过。

与本文的差异，以及**尚未验证的部分**：

- **单实例锁放在数据库旁边**（`<db 目录>/ingest.lock`），不单独配置；默认库在 `data/`
  下，所以结果仍是 §1 写的 `data/ingest.lock`。
- **`--maintenance` 尚不存在**（§7，属 P3），因此还没有"离线子命令跳过锁"的入口。
  → P3 已补上，见下面的 P3 落地注记。
- **Windows 上「控制台事件 → 优雅停机」这条链没有端到端验证过**：代码走
  `tokio::signal::ctrl_c()`，但实测只做到「硬杀 → 留下过期锁文件并报出持锁 pid」。
  `Ctrl-Break` 需要裸的 `SetConsoleCtrlHandler`，tokio 没有封装，**未处理**；
  Unix 侧的 `SIGHUP` 也**未处理**（走默认终止）。
- **相机线程仍在粗粒度锁下跑**：`step()` 整体持 `Arc<Mutex<Store>>`，标注 + JPEG 编码
  因此落在锁内，与 §3 的不变量不符。多相机下这是真实争用点，仍待修。

**P2 落地注记（2026-09-13）**：

- **健康文件原子写**：序列化到 `<path>.tmp` → rename 覆盖；rename 失败按 50/100/150ms
  重试 3 次，仍失败就先删目标再 rename（文件会短暂缺失，正是 §5 让读取方必须容忍的
  那种情况），最后才 warn。**注意**：这条 Windows 共享冲突路径没有被测试真正走通过 ——
  Rust 的 `File::open` 自带 `FILE_SHARE_DELETE`，所以进程内「读者持有文件」并不会让
  rename 失败；真正会失败的是共享模式更严的外部读者（编辑器、别的运行时）。测试覆盖的是
  「有读者时写入必须成功」与「rename 不可能成功时报错并清理 .tmp」两条。
- **心跳日志**：每 5 分钟每个相机一条 `heartbeat`（`state` / `frames` / `detections` /
  `recorded` / `reconnects` / `last_frame_age_s`），消掉 §5 说的「静默 = 正常」这个盲区。
- **指数退避**：2s 起、每次翻倍、上限 60s（`Backoff`）；**只有真帧到达才重置**，所以
  拔线的相机一小时只花掉个位数日志行。重连、建连失败、detector 失败共用同一条退避。
- 退避的等待是可被打断的（50ms 粒度检查 stop），所以 60s 上限不会拖慢停机。

**P3 落地注记（2026-09-22）**：新增 `retention.rs`（保留窗口清理 + 孤儿对账 + 容忍被占用文件的删除）、
`Store::{expire_observations, observation_ids, count_observations, vacuum}`（`crates/core/src/store.rs`，
外加 `last_seen` 单列索引——清理谓词只按它筛，原来的复合索引带不动）、daemon 里的 maintenance 线程
（三档定时：checkpoint / 清理 / 对账）与 `item-ingest --maintenance [--force]` 离线子命令。
与 §7 原文的差异与补充：

- **删除键是"id 推出的文件名"，不是行里存的路径**：快照的契约是 `<snapshots_dir>/<id>.jpg`
  （`write_snapshot`），而行里的 `sample_snapshot` 是给**读方**用的，可能是相对路径（单相机 CLI 跑的），
  也可能是 `frigate://snapshot/...`（根本没有本地文件）。所以清理删的是 `snapshots_dir/{id}.jpg`，
  不是那个字符串——把 `frigate://` 当路径去删是错的，把相对路径当绝对路径去删更错。
  代价：换过 `snapshots_dir` 的旧文件不再被扫到（记在 §9）。
- **先删行、后删文件，删文件失败绝不上升为错误**：行在一条语句里删掉
  （`DELETE ... RETURNING id`，返回的 id 就是真被删掉的行，两者不会不一致），随后才按 id 删文件；
  删不掉的按 §5 的 rename 同款策略重试 3 次（50/100/150ms），仍失败就记 `files_kept` 继续。
  留孤儿 JPEG 可以，留"指向已删文件的行"不行。
- **对账多一道保护**：只删 **id ≤ max(id)** 且当前无行的 `{数字}.jpg`。超出最大 id 的文件不是本程序写的，
  所以把 `snapshots_dir` 指到一个放照片的目录（`~/Pictures/2024.jpg`）不会被清空；观测表为空时同理
  什么都不删——"库是空的"更可能是路径配错，而不是"全过期了"。代价：整表被清空后，最后一个 id 的残留
  文件要等**下一条** observation 落库（max id 变大）才会被扫掉。
- **周期与开关**：`retention_days = 0` 关清理、`sweep_secs = 0` 只留 `--maintenance`、
  `reconcile_secs = 0` 从不扫孤儿；`checkpoint_secs`（P0 起就在配置里、此前没人消费）现在真的接上了。
  **首次清理在 daemon 启动后的第一个 tick**（停机一个月再起来要立刻收），对账按自己的周期走。
  空转的清理打 debug，真删了东西才打 info。
- **`--maintenance` 默认抢锁**（与 daemon 同一个 `<db 目录>/ingest.lock`）：daemon 在跑就拒绝并报出持锁
  pid——这就是 §7 平台注记里"Windows 上要求先停 daemon"的可移植实现。`--force` 明确跳过锁（§1 要求的
  逃生门），跳过时读 health.json 给一句判断：`updated_at` 还新 ⇒ 警告"daemon 大概还活着，删它正在写的
  快照不安全"；已过期（§5 的 90s 规则）⇒ 说明"锁多半是被杀死的进程留下的"。
- **VACUUM 只在 `--maintenance` 里做**（§7 原文：会长时间持写锁），daemon 永不自动做。
  `--maintenance` 的顺序是：清理 → 对账 → checkpoint(TRUNCATE) → VACUUM → 打印报告。

**P4 落地注记（2026-09-25）**：新增 `events.rs`（`Event` / `EventLog` 落盘 + `Tracker` 内存表），
`Store::record_sighting` 的返回从 `(id, is_new)` 扩成 `(id, is_new, hit_count)`（合并分支用同一条
`UPDATE ... RETURNING hit_count` 回读，不新增往返），`ingest_detections` 的返回从 `usize` 改成
`Vec<Recorded>`（**交出以前被丢掉的"这次命中哪一行"**），`CameraTask` 增 `events_path`、
`RuntimeConfig` 增 `events_file`（`[daemon] events_file`，空串=关，与 `preview_url` 同款；
`absolutize` 按 config 目录解析，空串保持空串）。与 §6 原文的差异与补充：

- **`seen_for_s` 只在 disappeared 行出现**，appeared 行不带：§6 的样例本身如此，且内存表只记
  `last_hit_at` 与"上次命中时的 hits"——`first_seen` 在库里，为它回查一次是把查询放进帧路径。
  于是 `hits` 与 `seen_for_s` 都取自**最后一次命中**，而不是扫描时刻。
- **事件类型用字符串常量而不是枚举**（`"appeared"` / `"disappeared"` / `"daemon_started"`）：
  §6 要求本阶段就容纳将来的 `covered`/`moved`，加一个只改字符串，不动格式也不动模块形状。
- **每次写都"开→追加→flush→关"，不常开句柄**：常驻句柄正是 Windows 上让轮转 `rename` 失败的东西
  （§5/§7 反复打的那条）；这里写一行 flush 一行，读取方（jq/tail）永远看不到半行。
- **启动标记 `daemon_started`**：文件已存在时**追加而非截断**（§6 的意义就是能被 jq/grep 跨时段消费），
  并从 `metadata().len()` 采纳已有字节数，好让轮转把旧内容算进去。标记行的作用是不让 disappeared
  跨越重启边界被误读：标记之后表是空的。
- **判定跟着每个 `step()` 走**，不是独立的定时器：与 §6「每次 step() 之后」一致，也让停机补扫有天然的
  插入点。`Tracker::record` 先折叠本帧命中再 `sweep`，所以被这一帧命中的键会先刷新、不会被误判过期。
- **`obs_id` 变了就记"旧的 disappeared + 新的 appeared"**（§6 末段的漂移缓解），实现上不再信任
  `is_new` 作为"到达"信号——`is_new` 是 store 对行的看法，而表需要的是"这个键上我有没有见过这行"，
  比对 `obs_id` 恰好回答这个，且 store 的去重规则将来变了也不受影响。
- **停机补扫在 supervisor 的相机线程收尾处**（`drive` 末尾、`flush_events`），不在 daemon 主流程：
  一个相机一条线程各管各的表，主流程没有它们的内存表；放在 `drive` 里也让"相机失败退出"同样补扫
  ——它的键一样是废弃的。
- **webhook（Frigate）路径不产出事件**（本阶段决定）：它只发离散事件、没有连续帧，永远观测不到
  "消失"（正是 §9「一台相机要么走 Frigate 要么走本地检测」的根源），只能产出半条流、反而会污染
  v2 要验证的数据。`frigate.rs` 保持原样，注释写明这是有意的。
- **轮转失败只 warn**：文件继续增长，下一次写再试。60MB 上限、2 代历史，照 §6；重试策略（3 次、
  50ms 递增）与 §5 的 rename、§7 的 unlink 同源。

**实机验证记录（2026-09-25，P4）**：用真实二进制（`target/debug/item-ingest.exe`，stable 工具链构建
`--features camera,yolo`）驱动内置摄像头，config 放在仓库根、数据落沙箱（顺带又验一次 §3 的路径
绝对化：**模型路径按 config 目录解析，把 config 放进沙箱会让 `models/` 找不到**，这本身是设计行为、
不是缺陷，故 config 留在仓库根）：

- **§10 的 P4 验收成立**：daemon 观察到真实物体后，`events.jsonl` 依次出现
  `daemon_started` → 4 条 `appeared`（person / chair / laptop / dining table，各带 `obs_id`）→
  `disappeared`（laptop，`obs_id` 与它的 appeared 一致，`seen_for_s=30.5`，即 30s gap 下限加一点
  采样余量）。`hits` 与数据库一致：该行 `hits=1`，事件也报 `hits=1`。
- **Windows 停机链的缺口在 P4 时仍在**（P5 已补，见下面的 P5 记录）：从后台 shell 启动的 daemon
  收不到 Ctrl-C（没有可附加的控制台），`taskkill` 不带 `/F` 只是请求终止、进程不退。P4 时
  **优雅停机补扫的确定性验证落在测试里**（runner 层用注入检测器证明 appeared→disappeared 与
  `flush_events` 的语义；supervisor 层证明停机后日志无半行）。
- **强杀的代价被观察到**：`taskkill /F` 后进程留下的 `ingest.lock` 是陈旧的，`events.jsonl` 里
  最后一批仍在场物体的消失事件不会出现——这正是补扫存在的理由，也是 P5 锁接手要解决的问题。
- **P4 时 `seen_for_s` 的语义是错的，P5 期间修正**：当时实现为"最后一次命中 → 判定时刻"（静默时长），
  实机一看就露馅——shutdown 补扫把仍在场的 chair 报成 `seen_for_s: 0.7`。§6 的样例（`hits: 64` 配
  `seen_for_s: 128.4`，约 0.5s 采样下的驻留时长）与 §10 的验收（与 `last_seen - first_seen` 一致）
  都说明它应当是**驻留时长**（首次命中 → 最后命中）。已改为后者，见 §6 的 P5 注记与 P5 验证记录。

**P5 落地注记（2026-09-26）**：§8 的三份模板落成 `deploy/`，daemon 侧补上两件事——锁的**心跳与
接手**（`lock.rs`：`HEARTBEAT=15s`、`STALE_AFTER=45s`、`acquire_or_recover`）与**完整停机信号面**
（`daemon.rs::wait_for_signal`）。与 §8 原文的差异与补充：

- **锁文件自带心跳，陈旧锁按证据接手**。§8.5 只说要"记 previous run died without shutdown"，但没说
  陈旧锁该怎么办——而"强杀 → 被服务管理器拉起"这条验收的前提是**下一次启动必须能起来**：原来的
  `acquire` 遇到任何已存在的文件都拒绝，一个被杀死的 daemon 会让服务永远起不来。现在锁文件在运行中
  每 15s 刷新一次 mtime（并写入 `heartbeat=<ts>`），启动时发现它已静默 45s 就认定为残骸、删除重建，
  并打 §8 要的那句 warn。**两条证据共同判定**：心跳停了 **且** health.json 的 `updated_at` 也过了
  §5 的 90s 窗口。health 新鲜 ⇒ 拒绝接手（防"卡住但还活着"的 daemon 被偷锁）。`--maintenance`
  仍用严格的 `acquire`（人在旁边，从不接手）。这同时消掉了 §9 里"陈旧锁要人工删"那条。
- **接手要等 ~90s 而不是 45s**：两个窗口取大者，因为 health 否决比心跳陈旧更保守。服务管理器的
  重启间隔（systemd `RestartSec=5` 会连续重试；Task Scheduler 一分钟一次）足够跨过它。
- **Windows 信号面从"只有 Ctrl-C"扩到五个**。§8 表格说 Windows 走"控制台事件"，P1 的注记说
  Ctrl-Break "需要裸的 `SetConsoleCtrlHandler`，tokio 没有封装"——**那个判断过时了**：tokio 1.53
  已提供 `signal::windows::{ctrl_c, ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown}`。现在五个
  全接，分别是交互中断、服务包装器/`GenerateConsoleCtrlEvent` 常发的、控制台窗口关闭、注销、关机。
  Unix 侧同时补上 `SIGHUP`（`SIGTERM`/`SIGINT` 原有）。**§9 那条"Windows 控制台事件→优雅停机未
  端到端验证"因此关闭**：实机用 `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT)` 打进去，日志出现
  `stop signal: Ctrl-Break` → 10s 预算内 `camera stopped` → `wal checkpointed` → 退出码 0。
- **模板放进 `deploy/`，按 §4 注记修正了原文的路径坑**：§8.2/8.3 把 config 放 `/etc/where-i-put`、
  WorkingDirectory 设 `/opt/where-i-put`，而相对路径按 **config 目录**解析（§3），于是 `data/` 会落到
  `/etc` 下。三份模板统一改为"config 与数据同树或写绝对路径"，并把这条写进 `deploy/README.md` 的
  "安装前必读"；systemd unit 的 `ReadWritePaths` 只放开数据目录，`ProtectSystem=strict` 下配置保持只读。
- **Windows 模板用计划任务（§8.4 的推荐 A）**：`install-windows-task.ps1` 注册任务、配失败重启
  （3 次 / 1 分钟）、`ExecutionTimeLimit` 设 0（这活儿要跑几周），并在脚本里就写明
  `Stop-ScheduledTask` 是**无控制台的硬停**，所以"干净停"要走控制台窗口关闭或 Ctrl-C——两句话都
  与 daemon 实际行为对齐。
- **锁的心跳线程与停机的先后**：心跳线程按 `TICK`（200ms）轮询 stop 标志，停机时与 health/maintenance
  线程一同 join，所以锁的释放仍由 `SingleInstance::drop` 在最后完成（干净的停机不留锁文件）。

**实机验证记录（2026-09-26，P5）**：release 二进制（`target/release/item-ingest.exe`，stable +
`camera,yolo`），config 在仓库根、数据在沙箱，内置摄像头：

- **第二个 daemon 被拒且错误信息带心跳**：`another item-ingest is already running: pid=10472 …
  heartbeat=2026-09-26T12:07:28Z`——比 P1 时的只有 pid 更有诊断价值。
- **强杀 → 立刻重启仍被拒（正确）**：`taskkill /F` 后马上启动，心跳还在 45s 窗口内 ⇒ 拒绝。
- **等窗口过期 → 接手 + warn**：~132s 后重启，日志出现
  `WARN previous run died without shutdown; taking over the lock … reason="the lock file's heartbeat
  had stopped"`，新锁写着新 pid，`recovered=true`。
- **Ctrl-Break 优雅停机（§9 缺口的关闭证据）**：`GenerateConsoleCtrlEvent(1, 0)` 打到 daemon 的控制台，
  日志依次 `stop signal: Ctrl-Break` → `event timeline flushed at shutdown closed=1`（**停机补扫**）→
  `camera stopped … frames=982 recorded=65` → `wal checkpointed` → `ingest daemon stopped`；进程干净
  退出、锁文件被删、health 终态 `stopped`。
- **修正后的 `seen_for_s` 与真实驻留一致**：同一次运行先修正语义再复测，chair 的
  `disappeared` 报 `seen_for_s=34.4`，与该 daemon 实际观测的 ~35s 吻合（修正前同一场景报 0.7，
  是"最后一次命中到停机"，见上面的 P4 记录补注）。

本轮**没有**跑到的：Linux/macOS 的服务化只到"模板 + 语法校验"级别（plist 过了 XML 解析、unit 过了
肉眼核对、PS 脚本过了 PowerShell 的 `Parser::ParseFile`）——三平台各装一次、重启机器、验强杀拉起，
仍需要那三台机器；Windows 计划任务的**注册**也没在本机执行（沙箱里只做了脚本语法校验与路径预检查），
因为注册一个真实任务会改机器状态，留给部署者按 `deploy/README.md` 做。

**实机验证记录（2026-09-14，P1/P2）**：在这台 Windows 机器上用真实设备把三条链路各跑了一遍
（网络环境变化，局域网海康摄像头当时不可达，所以 RTSP 走本地回环）：

- **内置摄像头 + 真 YOLO（单相机 CLI）**：`--webcam 0 --detector yolo`，120 帧；真实检出
  person / chair / refrigerator / cup；4 张标注快照落盘；`item-web` 指向同一个库，
  `/api/observations` 读回这些 observation，`/api/observation/{id}/snapshot` 返回
  200 `image/jpeg`（278095 字节）。**P1 那条「快照能被 item-web 显示」的验收，这是第一次
  用真数据成立。**
- **内置摄像头 + daemon（`[[camera]] webcam = 0`）**：health.json 从 `starting` 走到
  `running`，frames / detections / recorded 递增到 22 / 22 / 82，推理 EWMA 522ms，
  `last_frame_age_s = 0`，7 张快照落在 config 目录下而不是进程 cwd。
- **RTSP（本地回环）**：`vendor/mediamtx.exe` 推 `vendor/test.mp4`，
  `--rtsp rtsp://127.0.0.1:8554/test` 拉到 90 帧、12 次检测、退出码 0（test.mp4 里没有
  YOLO 认得的 COCO 目标，所以 0 条 observation，但解码 + 检测链路是真的）。

实机跑出了三个缺陷，都已修：

1. **致命失败的相机在 health.json 里显示成 `starting`**：`drive()` 在构造 detector 失败
   时直接 return，从没发布过健康数据。现在会发布 `failed` + `last_error`（§5 要求
   `failed` 可见）。顺带：`open()` 成功后也立刻发布一次，所以"连上了但还没出帧"显示
   `running` 而不是 `starting`。
2. **`--model` 是 cwd 相对路径**：daemon 由服务管理器拉起时 cwd 不确定，模型找不到会让
   每个相机在启动时死掉（实机上就是这么死的）。现在 `absolutize()` 也覆盖 `model`，
   相对路径按 config 目录解析。**注意这与 §8.2 / §8.3 的模板不一致**：那两份模板把 config
   放在 `/etc/where-i-put`、`WorkingDirectory` 设成 `/opt/where-i-put`，于是 `data/` 与
   `models/` 都会解析到 `/etc/where-i-put` 下。按 §3 的规则，模板要么把数据目录放在 config
   旁边，要么在 config 里写绝对路径。
3. **daemon 无法驱动 webcam**：`camera_tasks()` 只认 `url`，内置摄像头根本进不了 daemon。
   新增 `[[camera]] webcam = <index>`（`url` 优先），并补了单测。

**实机验证记录（2026-09-22，P3）**：用真实二进制（`target/debug/item-ingest.exe`；cwd 在仓库根、
config 在临时沙箱里，顺带又验了一次 §3 的路径绝对化——沙箱外的 `data/` 一个字节都没动）跑了一遍，
全部通过：

- **daemon 自己清理**：沙箱 config（`retention_days = 1`、`sweep_secs = 5`、`checkpoint_secs = 5`）
  下用 webhook 投三条 Frigate 事件（两条 `frame_time` 回拨 2 天、一条当前），日志出现
  `retention sweep rows=2 files=0`，`item-query --db <沙箱库> log` 只剩当前那条；
  `items.db-wal` 全程 0 字节——5s 一次的 checkpoint 真的在截断。
- **`--maintenance` 的四种状态**：daemon 在跑 ⇒ 拒绝并报出 `pid=25264` 与锁文件路径，退出码 1；
  加 `--force` ⇒ 警告"health 3 秒前刚写过，daemon 大概还活着"，随后清理跑完（孤儿 1.jpg / 2.jpg 被删，
  3.jpg 因行还在而保留）；硬杀 daemon 留下陈旧锁 ⇒ 不带 `--force` 仍拒绝，把 health 的 `updated_at`
  回拨 10 分钟后 `--force` 报"锁多半是被杀死的进程留下的"；手工删掉锁文件 ⇒ 干净跑通。
- **Windows 占用文件（§10 那条）**：用 PowerShell 以 `FileShare.None` 持有 `2.jpg`，真实二进制打出
  `snapshot could not be deleted ... (os error 32)`（共享冲突）后**照常跑完**、退出码 0、报告
  `orphans: 0 file(s), 1 kept`；放开句柄后再跑一次，该文件被删掉。
- **1 万行 + 1 万张图**：`crates/item-ingest/tests/retention_acceptance.rs`（`#[ignore]`：播种一万行是
  一万次 fsync，约 7s，不该拖慢 `cargo test`）造 10000 过期行 + 100 存活行与各自的快照文件，跑完
  `rows_deleted=10000`、`files_deleted=10000`，且**存活行的快照逐个仍在**（§10 的"无指向已删文件的
  sample_snapshot"）；Windows 下额外以 `share_mode(0)` 持有其中一个文件，断言 `files_kept=1` 且清理
  照常完成。命令：`cargo test -p item-ingest --test retention_acceptance -- --ignored`。

本轮**没有**跑到的（与 §9 一并看）：跨平台验证——只在 Windows 上跑过，Linux/macOS 的清理路径仅由
`cargo test` 覆盖（`share_mode(0)` 那个测试是 `#[cfg(windows)]`）；另外 daemon 停机时 maintenance 线程
按 tick（200ms）退出，正在跑的一次大清理会拖慢停机到那次清理结束为止（1 万文件约 1.7s，在 §3 的 10s
预算内，但没有硬性中断点）。

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

**落地（P4，2026-09-25；P5 修正一处语义）**：本节设计已实现于 `crates/item-ingest/src/events.rs`
（`Event` / `EventLog` / `Tracker`），接线见 §4 的 P4 落地注记，实机验证见同节的 P4 与 P5 记录。
原文与实现的差异（`seen_for_s` 只出现在 disappeared 行、事件类型用字符串、每次写都开关句柄、
`daemon_started` 启动标记、判定跟着 `step()` 走、webhook 路径不产出、`is_new` 不作"到达"信号）
都已在该注记里写明。**P5 期间修正了一处语义错误**：`seen_for_s` 起初被实现为"最后一次命中 → 判定
时刻"（静默时长），实机立刻暴露出它把仍在场的物体报成 `0.7`；按 §6 的样例（`hits: 64` ↔
`seen_for_s: 128.4`，约 0.5s 采样下的驻留时长）与 §10 的验收（与 `last_seen - first_seen` 一致），
正确语义是**驻留时长 = 首次命中 → 最后命中**，`Tracker` 因此多记一个 `first_hit_at`。下面保留
原文，作为设计意图的记录。

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
| snapshots 目录 | 不单独扫描孤儿文件；删除行时精确删对应文件；每 7 天做一次孤儿对账（DB 无该 id 就删文件，且只认 id ≤ `max(id)` 的 `{数字}.jpg`——见 §4 的 P3 注记） | 开启 |
| WAL | 每 `checkpoint_secs` 一次 `PRAGMA wal_checkpoint(TRUNCATE)` | 300s |
| VACUUM | **不自动做**（会长时间持写锁）。提供 `item-ingest --maintenance` 手动子命令 | 手动 |
| events.jsonl | 见 §6 轮转 | 64MB×3 |
| 平台注记 | **Windows** 无法删除/重命名被打开的文件，所以"删行+删图"（本节）与 §5/§6 的 rename 在该平台都要"重试 + 失败不致命"；`--maintenance` 在 Windows 上要求先停 daemon（无 `fork`），Unix 则可在 WAL 仍开着时做 checkpoint | — |

**落地**（P3，2026-09-22）：上表三个开关是 `[daemon]` 的 `retention_days` / `sweep_secs` /
`reconcile_secs`（0 一律表示关），实现落在 `crates/item-ingest/src/retention.rs` + daemon 的
maintenance 线程 + `--maintenance` 子命令；取舍与实测见 §4 的 P3 落地注记与实机验证记录。

保留策略的**硬约束**：清理必须发生在推断规则用到这些数据之后。所以 v2 的规则引擎
上线前，保留窗口默认取大（90 天）；规则引擎上线后按"最长容器滞留时间"反推。
这一条要写进代码注释，否则将来有人把默认值调小会静默毁掉推断证据。
（已写进 `config::defaults::RETENTION_DAYS` 与 `retention` 模块文档。）

删除必须**事务内先 DELETE 行再删文件**（文件删除失败只 warn）：宁可留孤儿 JPEG，
不可留"指向已删文件的行"。

## 8. 服务化：三平台开机自启

**落地（P5，2026-09-26）**：三份模板已在 `deploy/`（`item-ingest.service` /
`com.whereiput.ingest.plist` / `install-windows-task.ps1` + `deploy/README.md` 的安装前必读），
daemon 侧补了锁的心跳/接手与完整停机信号面。与本文原文的差异见 §4 的 P5 落地注记；两条要点：
**①** §8.2/8.3 的模板路径有坑（config 在 `/etc`、数据却按 config 目录解析 ⇒ `data/` 落在 `/etc` 下），
`deploy/` 里的版本已修正为"config 与数据同树或写绝对路径"；
**②** 原文说 Ctrl-Break"tokio 没有封装"已过时——tokio 1.53 提供了
`signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown}`，现在五个控制台事件全部接住。

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
| 优雅停机信号 | `SIGTERM`（默认）、`SIGINT`、`SIGHUP`（宽限期由 `ExitTimeOut` 控制） | `SIGTERM`、`SIGINT`、`SIGHUP`（宽限期由 `ExitTimeOut` 控制） | 无 `SIGTERM`；控制台事件五种全接（Ctrl-C / Ctrl-Break / 关闭 / 注销 / 关机，NSSM 会转发） |
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
- **"上次异常退出"的痕迹**：启动时若锁文件的心跳已停（且 health.json 也不新鲜）⇒
  接手并 `tracing::warn!("previous run died without shutdown")`；见 §4 的 P5 注记。
  判定必须用锁 + health 文件，不能用 `kill(pid, 0)`（§5：pid 不可移植且会被复用）。
- **日志去向各平台不同**（journald / launchd 文件 / Windows 重定向），但**内容契约
  一致**（心跳、重连、异常退出），别让某个平台的日志缺一块。

## 9. 已知限制 / 本阶段不解决的

- **事件是观测，不是真相**（P4）：`data/events.jsonl` 只是给 v2 的 events 表攒证据；
  v2 落地后表才是权威，JSONL 降级为调试产物。**任何东西都不应把它当状态读回来。**
- **事件表的漂移会变粗**（P4）：内存表可能与 DB 不一致（手工删行、dedup 窗口边缘反复），
  缓解是"`obs_id` 变了就记成旧的 disappeared + 新的 appeared"——语义仍正确，粒度更粗。
- **webhook 相机没有事件**（P4）：Frigate 只发离散事件、无连续帧，观测不到消失，所以
  那条路径不产出（见 §6/§4 的 P4 注记）。这不是缺口，是"要么 Frigate 要么本地检测"的推论。
- **轮转上限之外的历史会丢**（P4）：60MB × 3 代是一条硬边界；超出的最老内容被丢弃，
  这是 §6「先有界，再谈入表」的直接取舍。
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
- **保留策略有两个盲区**（P3 落地时明确）：换过 `snapshots_dir` 之后，旧目录里的文件不再被任何一次
  扫描覆盖（删除键是"当前目录 + id"，见 §7）；整表被清空后，最后一个 id 的残留文件要等**下一条**
  observation 落库才会被对账扫掉。两者都只会留下孤儿文件，不会删错东西——这是故意的方向。
- **平台缺口（与 §0 的全平台约束一并看）**：
  - **全平台 CI 尚未覆盖常驻**：现有 CI 只跑 `cargo check`/`test`（三平台 runner），
    §10 的 P1–P5 验收需在 Windows / Linux / macOS 各跑一遍才算真的全平台。
  - **服务化模板只在 Windows 实机验过行为**（P5）：daemon 的锁接手与 Ctrl-Break 停机在本机跑通；
    Linux/macOS 的 unit/plist 只做了语法校验（plist 过 XML 解析、unit 过核对），三平台各装一次、
    重启机器、验强杀拉起仍是部署者的事——见 `deploy/README.md`。
  - **Windows 计划任务未在本机注册过**（P5）：脚本过了 PowerShell 语法解析与路径预检查，
    但注册真实任务会改机器状态，有意留给部署者。
  - **接手陈旧锁要等 ~90s**（P5）：心跳 45s 陈旧 + health 90s 窗口取大者。服务管理器的重启间隔
    通常跨得过（systemd 会重试；Task Scheduler 一分钟一次），但**手工重启一个刚被强杀的 daemon
    会先被拒**——这是保守方向，唯一代价是等一会儿或删锁文件。
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

**验收状态**：P0–P2 见 §4 的实机验证记录（2026-09-14，含当时未覆盖的缺口）；P3 见同节的
2026-09-22 记录（1 万行 + 1 万张图、Windows 占用文件、`--maintenance` 的四种状态）；P4 见同节的
2026-09-25 记录（内置摄像头真实跑出 appeared→disappeared、`hits` 与库一致）；P5 见同节的 2026-09-26
记录（第二个 daemon 被拒且报心跳、强杀后接手游 `previous run died without shutdown`、Ctrl-Break 优雅
停机全链路、`seen_for_s` 修正后与真实驻留一致）。**P0–P5 全部落地。**

两处口径说明：**①** P4 的 `seen_for_s` 在 P5 期间修正为"驻留时长"（见 §6 的 P5 注记），修正后
与 `last_seen - first_seen` 一致（容差一个 detect 周期）；**②** P5 的"三平台各验一次"只做到了
Windows 实机行为 + 三份模板的语法校验，其余两平台留给部署者（见 §9 的平台缺口）。

## 11. 决策记录（2026-09-13 定案）

本节原先列了 5 个待拍板项，现已定案；实施以本节为准。正文其余部分若与此冲突，
以此处为准。

1. **单进程多相机**（原推荐）：一个 daemon 同时跑 webhook + preview + 多相机线程，
   共享一个 `Arc<Mutex<Store>>`、一个 health 文件、一处停机逻辑。代价是模型常驻
   内存、单个相机的崩溃面波及同进程其他相机 —— 靠 `Supervisor` 的错误分类与隔离
   兜住（模型加载失败这类致命错误让该相机进 `failed` 并停止重启，不刷屏）。
2. **常驻入口形态 `--daemon --config <file>`**（原推荐）：`--config` 单独出现时仍
   只表示"播种 regions + 给其他参数提供默认值"，不会把命令悄悄变成常驻；
   `--rtsp` / `--webcam` / `--demo` / `--detect` 的旧语义一律不变。
3. **保留窗口默认 90 天**（原推荐）：为 v2 的规则引擎留足推断证据。§7 那条硬约束
   不变 —— 清理必须发生在推断规则用到这些数据之后；将来要调小默认值，先读
   `Store` 里那条注释。
4. **P4 事件观测纳入本轮**（原文未给推荐，按"v2 需要真数据验证"取此默认）：接受
   §6 记录的"内存表可能与 DB 漂移"风险，缓解手段见 §6 末段（以 `obs_id` 为准，
   变了就记成旧的 disappeared + 新的 appeared）。
5. **服务化推迟到 P5，日常先用临时启动器**（原文未给推荐，取最小动作）：§8 的三份
   模板已经写好，P5 阶段在 Windows / Linux / macOS 各验一次；在那之前前台或手动
   启动即可，不影响 daemon 自身的设计与行为。

第 4、5 条原文没有推荐口径，以上是按"少欠债、少动作"取的默认；若不同意，改这两条
不影响第 1–3 条决定的架构形状。
