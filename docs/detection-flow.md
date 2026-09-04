# 检测流程与预期结果

> 本文对照代码描述检测闭环的数据流、每一步在 SQLite 里造成的精确变化，以及这些
> 变化如何映射到 Web UI 上看到的卡片。供跑流程前预览预期、跑流程后核对结果。
> 文中所有常数均以代码为准：`store.rs`（去重窗口）、`main.rs` rtsp_pass（NMS
> 0.45 / 默认路径）、`index.html`（刷新与 limit）。

## 0. 管线总览

```
RTSP (1280x720 @25fps)
  → RtspSource 解码           每帧都解（25/s）
  → detect-fps 节流           只取 ~1/s 帧送检测（其余 decode-only 丢弃）
  → YoloDetector (yolov8n)    每 280ms 一次推理 → raw Vec<Detection>（可能 0~N 个框）
  → NMS (IoU>0.45 去重)       同 label 重叠框合并
  → zone_for_point            框【中心点】落在哪个区域矩形 → zone 名，否则 "frame"
  → record_sighting           合并进活跃 observation 或 INSERT 新行，返回 (id, is_new)
  → is_new 时                 标注帧 JPEG 写 data/snapshots/{id}.jpg，路径回填该行
                              （烧入：本帧全部 NMS 存活框按 label 着色 + 区域灰虚线）
```

三个进程/端口相互独立：`item-ingest --rtsp`（写库）、`item-ingest --preview`
（8477，实时 MJPEG，不碰库）、`item-web`（8478，只读库）。

`--webcam <n>`（feature `camera`，nokhwa/DirectShow）走**同一条**泵帧-检测-落库
循环（`main.rs::camera_pump`），只是帧源换成内置/USB 摄像头：无 RTSP 的
decode-only 概念（每次 `next_frame` 阻塞等相机，帧率 ~2-30fps 由相机决定），
无 config 时 zone 恒为 `frame`，且不需要 FFmpeg/libclang 那套本机设置。

## 1. 数据库结构（data/items.db，WAL 模式）

```sql
CREATE TABLE observations (
    id              INTEGER PRIMARY KEY,   -- 自增；也是快照文件名
    camera_id       TEXT NOT NULL,         -- 命令行 --camera-id / config
    zone            TEXT NOT NULL,         -- 区域名，无 config 时恒为 'frame'
    label           TEXT NOT NULL,         -- COCO 80 类英文名
    first_seen      TEXT NOT NULL,         -- RFC3339 UTC
    last_seen       TEXT NOT NULL,         -- RFC3339 UTC
    hit_count       INTEGER NOT NULL,      -- 合并进该行的检测帧次数
    sample_snapshot TEXT                   -- 出生帧 JPEG 路径；或 frigate:// 引用
);
CREATE TABLE regions (id, camera_id, name, x0, y0, x1, y1, UNIQUE(camera_id,name));
```

要点：**没有逐帧事件表**。库里的每一行都是一个"物品在某区域的停留时段"
（observation），行数远小于检测次数——这是"记忆"与"日志"的区别。

## 2. 合并规则与逐帧演变（预期输出的核心）

同 `(camera_id, zone, label)` 三元组，若 `last_seen` 距新检测 ≤ **5 分钟**
（`DEFAULT_DEDUP_WINDOW = 300s`）则 UPDATE 否则 INSERT。以典型桌面场景
（detect-fps=1，人一直在画面里）推演 `person` 的变化：

| 时刻 | 事件 | SQL 效果 | 行状态 |
|---|---|---|---|
| 09:00:00 | 首次检出 person@desk | INSERT id=7 | hits=1, first=last=09:00:00, 写 snapshots/7.jpg |
| 09:00:01 | 再次检出 | UPDATE id=7 | hits=2, last_seen 前移；**不写快照** |
| …每秒重复 | | UPDATE id=7 | hits 线性涨（≈60/分钟） |
| 09:40:00 | 人已出画 5 分钟以上 | — | id=7 冻结在最后一次命中 |
| 09:41:00 | 人重新入画 | **INSERT id=12** | 新行新快照；id=7 成为历史 |

由这张表可推出的预期行为，也是核对清单：

1. **hit_count ≈ 检测次数**：静态场景跑 10 分钟，主要物体的 hits 会到几百。
   数值本身就是"检测有多稳"的读数。
2. **离开又回来 = 新 observation**：5 分钟是硬分界，回来会拿到新 id、新出生照。
   所以 UI 上同一物体可能出现多张卡（不同时段），这是特性不是重复 bug。
3. **快照每行只有一张，内容 = 该行诞生那一刻的画面（带烧入标注）**，此后永不更新。
   对着 last_seen 很新但图是"刚出现时"的情况不必惊讶。标注画的是**出生帧全部**
   存活检测框（同 label 多框都会画出来——这正是 hits 超速增长的视觉解释），外加
   该相机所有配置区域的灰色虚线矩形；实现在 `item-ingest/src/annotate.rs`。
4. **zone 由中心点单点判定**：一个框永远只属于一个 zone。人从 desk 区走到
   upper 区会分裂成两条 observation（各自计数、各自快照）。
5. **重叠抑制**：同一物体的多个冗余框在 NMS（同 label IoU>0.45）后只记一次，
   一次"椅子被框两遍"不会让 hits 翻倍。
6. `conf` 下限由检测器把守（默认 0.3，`--conf` 可调），进入落库环节的还有
   低分候选，所以偶发误报（如昨天的 chair→bed）会真实入库。

## 3. 冷启动第一分钟会看到什么

全新库 + `--camera-id living` 无 config（最常见跑法）：

- 前几秒：RTSP 握手 + ort 加载模型（首次推理稍慢，日志会刷 ort 的
  Initializer 警告——是 onnxruntime 的碎碎念，无害）。
- 第一帧检测后：`item-ingest: detections ingested frames=N recorded=M`
  开始按秒出现；`data/snapshots/` 出现 `1.jpg、2.jpg…`（编号=行 id）。
- 库里第一分钟大致长这样（zone 全是 frame）：

```
 id | camera | zone  | label        | first            | last             | hits
----+--------+-------+--------------+------------------+------------------+-----
  1 | living | frame | chair        | 18:00:03 | 18:00:58 | 45
  2 | living | frame | person       | 18:00:03 | 18:00:58 | 45
  3 | living | frame | dining table | 18:00:05 | 18:00:58 | 40
  4 | living | frame | cup          | 18:00:07 | 18:00:58 | 38
  5 | living | frame | tv           | 18:00:09 | 18:00:20 | 11     ← 被遮挡后停止增长但行还"活着"
```

（参照：昨天真机 90 秒 @2fps 产出 8 行，person×20、chair×64，数量级以此为准；
`--detect-fps 1` 时 hits 增速约为其一半。）

- **停止循环后**：行不再更新但也不删除——last_seen 从此就是"最后一次见到"。

## 4. Web UI 对照（8478 端口）

### 4.1 一张卡片 = 一行 observation

```
┌────────────────────────┐
│   snapshots/{id}.jpg    │  ← sample_snapshot 解析出的文件（框与区域线已烧入像素）；文件不存在/是
├────────────────────────┤    frigate:// 时显示 "no snapshot" 占位
│ person                  │  ← label
│ (desk)(living)   ×20    │  ← zone 芯片 · camera 芯片 · hit_count
│ last seen 5 min ago     │  ← last_seen（相对时间，随刷新滚动）
│  · first 42 min ago     │  ← first_seen
└────────────────────────┘
```

排序：`last_seen` 从新到旧；请求 limit=200（后端钳制 1..500）。每 10 秒整页
自动刷新，循环在跑时新卡会自己冒出来。

字段映射一览：

| UI 元素 | 来源 | 逻辑位置 |
|---|---|---|
| 标题 | `label` | 直读 |
| 两枚芯片 | `zone`、`camera_id` | 直读 |
| ×N | `hit_count` | 直读 |
| 时间两行 | `last_seen`/`first_seen` | 前端相对时间格式化 |
| 缩略图/放大 | `sample_snapshot` | `/api/observation/{id}/snapshot`：相对路径按 web 进程 cwd 解析，先 `is_file()` 才置 `has_snapshot=true` |
| 底部 "N observations" | 行数 | `list` 结果长度 |

### 4.2 搜索框与 Ask

- **搜索框**：服务端 `label LIKE '%词%'` 子串过滤（大小写不敏感），防抖 250ms。
  输入 `cup` 只剩杯子卡；输入无匹配词 → "No objects match ..."（与冷启动空库的
  "还没有数据"提示刻意区分）。
- **Ask**：`/api/ask?q=`。当前无 VLM 侧车时返回 `mode:"log"`——前端展示为
  "log match for ...: label @ zone (time)"，即把命中 observation 念出来。
  注意它的取词是个小停用词表（滤掉 where/is/my/the...取第一个实词），
  所以 "where did I put my **remote**" 实际搜 `remote`；命中 0 条也是合法回答
  （"nothing recorded that matches"）。设了 `ITEM_VLM_BASE_URL`+`ITEM_VLM_MODEL`
  后同一入口变成模型成句回答。

### 4.3 与 preview（8477）的分工

| | 8477 `/preview` | 8478 web UI |
|---|---|---|
| 回答的问题 | "现在镜头前是什么"（实况 MJPEG，不落库） | "什么东西在哪里、最后一次见到是啥时候"（查账本） |
| 数据依赖 | 无（直连摄像头） | items.db + snapshots/ |
| 是否写库 | 否 | 只读（`SQLITE_OPEN_READ_ONLY`） |

## 5. 已知的理论↔实际偏差（核对时别当 bug）

- **误分类会入库**：yolov8n 把办公椅认成 `bed` 已实际发生（COCO 类粗）。
  同类混淆还有 cushion/chair、book/laptop 等。计数照常合并。
- **空场也有货**：椅子一直空着坐，chair observation 会一直活着且 hits 猛涨——
  observation 记录的是"物体在场"，不是"物体被使用"。
- **快照目录与 db 的相对性**：`sample_snapshot` 存的是相对路径，换目录启动
  web 会导致 `has_snapshot=false`（图不出来，其余功能正常）。同一仓库根目录
  下启动即无此问题。
- **标注是烧进像素的，UI 无法关框/换框**：框数据不落库（设计上不存逐帧检测），
  所以老快照（该特性之前写的）不会有框，`frigate://` 引用图也不归我们画。
  同一帧里诞生的多个 observation 共享逐像素相同的图，卡片别当重复 bug。
- **时间全为 UTC**：UI 里浏览器会转成本地时区显示（`toLocaleString`/相对
  时间），db 原文是 `+00:00`，比对时差 8 小时（东八区）属正常。
- **一次检测可产多行**：一帧里 N 个框各归各的 zone/label，`recorded=M` 打印
  的是这批新增记录数，不是行数（合并行也算 recorded）。

## 6. 快速自检命令

```sh
# 行数与内容（等价于 UI 读的东西）
cargo run -p item-query -- --db data/items.db log

# 快照是否与新增行同步增长
ls data/snapshots | wc -l

# 直接戳 API
curl "http://127.0.0.1:8478/api/observations?limit=5"
curl -o /dev/null -w "%{http_code}\n" http://127.0.0.1:8478/api/observation/1/snapshot
```
