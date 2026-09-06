# VLM grounding sidecar 搭建 runbook（llama.cpp + Qwen2.5-VL）

`item-ingest --detector vlm`（`--features vlm`）不内置任何模型：它把当前帧
JPEG 后 base64 塞进 OpenAI 兼容的多模态 `POST {base}/chat/completions`，
让 sidecar 做开放词表 grounding，返回 JSON 数组
`[{"label", "bbox_2d":[xmin,ymin,xmax,ymax]}]`。任何支持 `image_url`
（data URL）多模态输入的 OpenAI 兼容服务都能当 sidecar（llama.cpp server、
Ollama、vLLM、云 API）。本文以 **llama.cpp server + Qwen2.5-VL-3B（GGUF）**
为准——纯 CPU 可跑，3B q4 约 1.9GB，本机无需 GPU。

## 1. 拿到 llama-server.exe（Windows）

去 <https://github.com/ggml-org/llama.cpp/releases> 下载 nightly 构建包。
真机验证版本：b10819（0.4.0-dev）。

- **首选 `llama-bXXXX-bin-win-vulkan-x64.zip`**：Intel Core Ultra 的 Arc 核显
  （本机 Arc 140T）实测全链路 ~2 倍速（下表），多模态支持完整。
- 无核显/驱动问题再用 `llama-bXXXX-bin-win-cpu-x64.zip`。
- GitHub 直连慢就在 URL 前加镜像前缀（如
  `https://gh-proxy.com/https://github.com/...`，本机 ~220KB/s）。
- 解压即用，无需安装；启动时若要钉死设备加 `--device Vulkan0`
  （设备名用 `--list-devices` 查）。注意：即便 GPU 在跑，日志也不会出现
  vulkan 字样——是否生效用 `--list-devices` 和耗时对比确认。

## 2. 下载模型（走 hf-mirror）

```sh
mkdir -p models/qwen2.5-vl-3b && cd models/qwen2.5-vl-3b
curl -L -o Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf \
  https://hf-mirror.com/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf
curl -L -o mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf \
  https://hf-mirror.com/ggml-org/Qwen2.5-VL-3B-Instruct-GGUF/resolve/main/mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf
```

（模型在 `models/` 下即可，该目录已 git-ignore；要更高精度换 Q8_0，
要更快/更省显存换 7B→3B 之外还有更小的 Q2/Q3 档。）

## 3. 起 sidecar

Vulkan 版（推荐，`--image-min-tokens 1024` 是 llama.cpp 对 Qwen-VL grounding
的官方建议，小图框位置明显更准）：

```sh
llama-server -m models/qwen2.5-vl-3b/Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf \
    --mmproj models/qwen2.5-vl-3b/mmproj-Qwen2.5-VL-3B-Instruct-f16.gguf \
    --host 127.0.0.1 --port 8080 --device Vulkan0 --image-min-tokens 1024
```

CPU 版去掉 `--device Vulkan0` 即可，其余相同。

验证：

```sh
curl http://127.0.0.1:8080/v1/models          # 应列出 qwen2.5-vl 模型名
curl http://127.0.0.1:8080/v1/chat/completions -H "Content-Type: application/json" \
  -d '{"model":"qwen2.5-vl-3b-instruct","messages":[{"role":"user","content":"hi"}]}'
```

## 4. 冒烟：单张图片出框（不需要摄像头）

```sh
cargo run --features vlm -p item-ingest -- \
    --detect path/to/photo.jpg --detector vlm --out out.png
```

stdout 打印每个框的 label/conf/坐标，`out.png` 是烧好框+标签芯片的标注图
（第一个框按"observation 快照"样式高亮）。中文提示：VLM 回复若用像素坐标
（任一坐标 >1000），ingest 会自动按像素解释，无需改参数；想强制某一约定用
`--vlm-coords norm1000|pixel`。

**给大图先缩图**：image token 数随分辨率涨，CPU 上 prompt eval ~27ms/token
（b10819 实测）。640x427 的猫图 ≈ 436 token、全程 14s；5184x3456 的原图
≈ 4164 token、仅吃图就要 ~110s，默认 60s 超时直接断连。`--detect` 传照片
前先缩到 ≤1280 宽（摄像头帧 720p/1080p 没这个问题）：

```sh
uv run --with pillow python -c "from PIL import Image; im=Image.open('big.jpg'); im.thumbnail((1280,1280)); im.convert('RGB').save('small.jpg', quality=80)"
```

真机实测（Q4_K_M，b10819；cats.png 640x427 / 436 token，单帧单图）：

```
CPU  (Ultra 9 285H 16 线程): prompt 37.4 t/s | decode 16.3 t/s | cats 全程 14.1s | bee 26.5s
Vulkan (Arc 140T 核显):      prompt 72.0 t/s | decode 32.0 t/s | cats 全程  7.3s | bee  9.0s
```

```
$ item-ingest --detect cats.png --detector vlm --targets "cat" --out cats-out.png
load 2ms | inference 14108ms | 1 raw -> 1 kept
  cat              100%  [90, 56, 263, 143]
```

## 5. 接入闭环

环境变量与 item-web 的 ask bar 共用（CLI 显式传参优先）：

```sh
export ITEM_VLM_BASE_URL=http://127.0.0.1:8080/v1
export ITEM_VLM_MODEL=qwen2.5-vl-3b-instruct

# 内置摄像头闭环（无 FFmpeg 依赖）
cargo run --features "camera,vlm" -p item-ingest -- --webcam 0 --camera-id desk

# RTSP 摄像头（需先 cargo xtask setup）
cargo run --features "rtsp,vlm" -p item-ingest -- \
    --rtsp "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102" --camera-id living
```

之后的 NMS → zone 映射 → 5 分钟去重 → 快照烧框（新行高亮）与 YOLO 完全一致，
`item-web` / `item-query log` 照常可查——但 label 不再限于 COCO 80 类，
`--targets` 想要什么写什么（逗号分隔；留空 = 开放模式"列出所有可见物体"）。

## 6. 参数速查

| 参数 | 默认 | 说明 |
|---|---|---|
| `--detector vlm` | `yolo` | 选 sidecar 后端（需 `--features vlm`） |
| `--vlm-base-url` | env `ITEM_VLM_BASE_URL` | 含 `/v1` 前缀，如 `http://127.0.0.1:8080/v1` |
| `--vlm-model` | env `ITEM_VLM_MODEL` | sidecar 侧的模型名 |
| `--targets` | 家居词表 12 类 | remote/keys/scissors/charger/glasses/wallet/umbrella/medicine/bottle/cup/laptop/phone；空串=开放列举 |
| `--vlm-coords` | `norm1000` | 让模型按 Qwen 约定输出 0-1000 归一化坐标；`pixel` 则按绝对像素 |
| `--vlm-timeout` | 60s | 单次请求超时；到点报错→该帧跳过，循环不死 |
| `--detect-fps` | yolo 1.0 / **vlm 0.2** | VLM 一帧一次 HTTP 往返，未显式给参时自动降到 0.2 |

## 7. 已知限制（用的时候别当 bug）

- **无标定置信度**：VLM 不给分数；回复带 `score`/`confidence` 就用（clamp 到
  0..1），没有就记 1.0。NMS 去重不受影响。
- **检测期间不读帧**：camera_pump 是同步循环，grounding 请求在途时帧不消费；
  RTSP 缓冲可能积压。本地 3B CPU 一帧数秒~十几秒，先把 `--detect-fps` 当
  0.2 用；要实时性等常驻化/异步化改造。
- **sidecar 挂了 ≠ 循环挂**：detector 报错只 warn + 跳帧（隔 2s 再试），
  这是相对 YOLO 路径新增的容错。
- **3B grounding 质量有限**：框位置偶尔漂、label 口语化（"tv remote" vs
  "remote"）——label 会被 trim+小写入库，zone 统计按字符串分组。真机样例：
  猫图框得准；蜜蜂的框贴着目标但偏了 ~100px（3B + 量化 mmproj 的精度所限，
  坐标映射本身有单测钉住）。召回也偏保守（两只猫只框一只），换 7B/8bit 会
  好一些。**启动 sidecar 务必带 `--image-min-tokens 1024`**：llama.cpp 明确
  提示 Qwen-VL grounding 需要至少 1024 个 image token，实测小图（640 宽）
  加上后框从"大半落在旁边的渔网"变成紧贴猫身；≥1024 token 的大图不受影响。
- **NPU 不可用于本场景**：OpenVINO 后端（`GGML_OPENVINO_DEVICE=NPU`）目前
  "multimodal features are a work in progress"，只支持纯文本模型——图像
  grounding 用不了 NPU，即便驱动在位。核显 Vulkan 是本机唯一的加速路径。
- **空回复是正常行为**：画面里没有目标物时模型回 `[]`（或纯文本，都会按
  0 检测处理），该帧不落任何行。想看模型到底回了什么，`RUST_LOG=item_ingest=debug`
  会打出原始 reply。
- **其他 sidecar**：Ollama = `ollama pull qwen2.5vl` 后 base url 写
  `http://127.0.0.1:11434/v1`；vLLM 同理。云 API 只要支持 image_url data URL
  即可，key 走各自网关（本客户端不发鉴权头）。
