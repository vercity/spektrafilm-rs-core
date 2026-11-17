# Python 0.3.4 功能对齐交接

参考实现是本机 `/Users/ougu/Downloads/spektrafilm-main` 里的 Python `0.3.4` 源码快照。Rust 初次审阅基点是 `31da6f8`（基于 Rust `v0.1.2`），后续以各节记录的 Rust 提交为准。初次审阅按 CPU 光谱与胶片模型、RAW／文件／LUT、GUI／CLI／GPU 三条线展开。

Rust 已有曝光、显影、印相和扫描主链。28 份共有片纸配置逐字节一致，Python 的 160 条中性滤镜记录逐值一致。Rust 另外提供 `kodak_doublex`、`kodak_trix` 和 `kodak_2302`，以及 `kodak_2302` 的 22 条中性滤镜记录。已有 Rust 0.3.4 回归覆盖 Portra 400／Portra Endura 的中灰和小渐变，关闭 AE、晕光、颗粒、DIR、眩光和锐化。出处见 [现有 Rust 测试](../crates/spektrafilm-core/src/stages/mod.rs#L98)。审阅时 Cargo 在拉取索引时超时，Python 临时环境的依赖下载也未完成。没有取得新的跨语言误差或 RAW、GPU 实测结果。

## P0 建立可复跑的基线

1. **修正 CLI 的有效配置和错误路径。** [`process`](../crates/spektrafilm-cli/src/main.rs#L153) 在读 JSON 前选相纸，默认命令行布尔值还会覆盖 JSON 的 `io.scan_film`。先确定有效模式再选相纸，显式 `--scan-film` 开启直扫，省略它时沿用 JSON。建立完整光谱管线失败时返回带上下文的非零错误，避免静默改用简化管线和默认参数。验收覆盖缺失光谱资产、非法上采样方法、JSON 单独开启无默认配纸的正片直扫和命令行开关。
2. **公开逐阶段浮点诊断输出。** Rust 现有环境变量可导出胶片曝光对数、胶片 CMY 和相纸 CMY。补印相曝光对数和编码、限幅前的线性扫描 RGB。`--raw-out` 的现有文件仍是 `process` 的最终浮点结果，帮助文本需描述真实语义。Python 对应数据边界在 `src/spektrafilm/runtime/pipeline.py`。基准先固定 CPU f64。出处见 [Rust 管线](../crates/spektrafilm-core/src/pipeline.rs#L503) 和 [CLI 最终缓冲](../crates/spektrafilm-cli/src/main.rs#L297)。
3. **统一输入及比较口径。** [旧脚本](../scripts/parity/spektra_compare.py#L66) 给 Python 指定 ACES2065-1，却在建议的 Rust 命令里省略输入色域和相纸。用同一份场景线性 float32 TIFF、显式片纸 ID 和色域，比较同配置的阶段缓冲。先选 Portra 400／Portra Endura、Gold 200／Fuji Crystal、Vision3 500T／2383，以及 Provia／Velvia 直扫。先做小尺寸灰阶、饱和色块和超白，再加入真实景物。第一轮关闭 AE、空间及随机效果和 DIR，随后单独测试逐像素 DIR 和内部印相／扫描 LUT。各阶段记录最大绝对误差、P99 和均值。把测得的当前差异与回归阈值一同记录。

### 分支验证结果

P0 的 CLI 修复、阶段探针和对照脚本已落地。复跑命令及 `metrics.json`、原始阶段文件的格式见 [对照脚本说明](../scripts/parity/README.md)。f64 核心测试通过 33 项，CLI 集成测试分别在 f32 和 f64 下通过 2 项。手动关掉色域压缩后，线性探针保留了 `-0.00662895`。将探针值限幅后的数组与关闭编码时的 `--raw-out` 完全一致。

以下数值来自 macOS arm64、Rust 1.89.0、Python 3.13.13、NumPy 2.5.4、SciPy 1.18.1、colour-science 0.4.7 和 OpenImageIO 3.1.19.0。合成样本为 8×8 线性 ACES2065-1。实拍用原版附带的 `img/test/portrait_leaves_32bit_linear_prophoto_rgb.tif`（667×1000）。这些是观察值，可用于给指定配置建立后续回归门槛。

| 配置 | 线性 RGB 最大绝对误差 | 最早可见的差异 |
| --- | ---: | --- |
| Portra 400／Portra Endura，合成图 | `4.11e-9` | 印相前后的中间值误差均小于 `3e-15` |
| Gold 200／Fuji Crystal，合成图 | `4.16e-9` | 单独开启印相 LUT 或扫描 LUT，RGB 误差仍约 `4.16e-9` |
| Gold 200／Fuji Crystal，仅开逐像素 DIR | `5.73e-8` | 胶片 CMY 最大误差 `1.11e-7` |
| Provia／Velvia，直接扫描 | `4.10e-9`／`3.77e-9` | 胶片 CMY 最大误差均小于 `6e-15` |
| Provia 直扫，仅开逐像素 DIR | `4.15e-2` | 胶片 CMY 最大误差 `0.273145`，单独开启扫描 LUT 时 RGB 误差仍约 `4.10e-9` |
| Vision3 500T／2383，合成图 | `0.259739` | 胶片曝光对数最大误差 `0.718375` |
| Portra 400／Portra Endura，实拍 TIFF | `1.37e-4` | 曝光对数最大误差 `1.84e-4`，最终 RGB P99 `4.75e-5`、均值 `2.15e-6` |

Vision3 的 `T` 参考光源会被 Rust 的曝光选择器回退为 D55，2383 的 `K75P` 观看光源也会被扫描选择器回退为 D50。原版分别使用 Incandescent 和 Kinoton 75P 的 81 波段光谱。P1 应先以这组对照验证光源取值，再测 Provia／Velvia 的片种专属 DIR。出处见 [Vision3 profile](../data/profiles/kodak_vision3_500t.json#L22)、[2383 profile](../data/profiles/kodak_2383.json#L23)、[曝光光源选择](../crates/spektrafilm-core/src/pipeline.rs#L1163) 和 [扫描光源选择](../crates/spektrafilm-core/src/stages/scanning.rs#L432)。

Python 快照的四份测试模块（`test_regression_baselines`、`test_topology`、`test_runtime_api` 和 `test_lut_mode`）通过 22 项并失败 4 项。三份印相 RGB 快照与当前执行结果不一致，另一个 LUT 测试仍引用已移除的 `GrainParams.mult_usm_amount`。跨语言报告采用当次执行的 Python 阶段输出。历史 `.npz` 快照的差异仍待核实。

## P1 确定性照片渲染与单 LUT

- **统一色域和传递函数。** Rust 核心尚未读取 `input_cctf_decoding`。扫描端按 sRGB 编码任意输出空间，并在核心中硬限幅。让 GUI 和 CLI 共享每张输入的色彩信息，向写出边界移动格式限幅。补 GUI 打开的 JPEG 在 f64 CLI 里无法重新读取的问题，测试 JPEG 往返、图片切换不继承前一张的色域、等价编码／线性灰阶、非 sRGB 和浮点超白。出处见 [输入曝光](../crates/spektrafilm-core/src/stages/filming.rs#L291) 和 [扫描编码](../crates/spektrafilm-core/src/stages/scanning.rs#L359)。
- **补裁切、AE 顺序和 UV／IR。** Python 先对全图测光，再裁切、缩放。物理像素尺寸取裁切前图幅并按缩放修正。Rust 有裁切及 UV／IR 字段，尚未消费。UV／IR 应按参考光源重新归一化感光度，让光谱重建和印相中灰标定共用。验收覆盖裁切区外高光、边缘矩形和饱和色。出处见 [Rust 几何入口](../crates/spektrafilm-core/src/pipeline.rs#L462) 和 [感光度构建](../crates/spektrafilm-core/src/pipeline.rs#L233)。
- **统一印相预闪和扫描端点。** Rust 黑白端点漏算预闪，黑白校正还会强制使用扫描 LUT。直扫胶片默认加眩光也与 Python 不同。共享端点和图像变换，分别验收预闪、黑／白校正、LUT 和正片直扫。出处见 [端点计算](../crates/spektrafilm-core/src/color_reference.rs#L213) 和 [扫描分支](../crates/spektrafilm-core/src/stages/scanning.rs#L234)。
- **让 `.cube` 复用完整光谱管线。** 现有 CLI 用简化构造，无片纸曝光校准，且关闭所有 DIR。按 Python LUT 模式保留逐像素 DIR，关闭扩散、颗粒、眩光、AE 和全图高光调整。明确色域、传递函数和白点曝光，批量采样并拒绝边长 0／1。验收要求 `n³` 行有限值、网格点匹配确定性直出、像素调序及热点不影响映射。SDR 离网格可采用原版 QA 的 ΔE00 P99 ≤ 1、最大 ≤ 2。出处见 [现有 LUT 导出](../crates/spektrafilm-cli/src/main.rs#L368)。

### P1 本轮验证

- Vision3 500T 直扫的胶片曝光对数最大误差从 `0.718375` 降到 `2.78e-15`。Gold 200／2383 的线性 RGB 最大误差从 `0.0424707` 降到 `4.35e-9`。Vision3 500T／2383 为 `4.29e-9`。两种照明都覆盖了同光源的其他片纸配置。
- Provia 仅开启逐像素 DIR 时胶片 CMY 最大误差从 `0.273145` 降到 `1.82e-7`，Velvia 为 `5.26e-7`。显式 JSON 系数按原数组格式输入，覆盖片种默认值。Rust `DirCouplersParams` 的四个系数字段改用 `Option` 表示覆盖值。
- Portra 400／Portra Endura 的原版实拍 ProPhoto TIFF，线性 RGB 最大误差从 `1.37e-4` 降到 `2.52e-9`。编码 sRGB 和裁切前测光一起开启时，测试色板误差 `1.22e-8`。预闪与扫描黑白校正一起开启时，直接积分和扫描 LUT 的最大误差都约 `5.27e-9`。眩光只应用于印相路径。
- Portra 400／Portra Endura 的 33³ sRGB 输入 SDR `.cube`，网格点最大绝对误差 `1.32e-7`。固定种子的 10,000 个离网格样本中，三线性和四面体插值的 ΔE00 P99 分别为 `0.187`、`0.156`，最大值分别为 `0.488`、`0.435`。这是原生曝光域的单 LUT。CLI 的输入域可选线性 ProPhoto 或编码 sRGB，命令见 [使用说明](../README.md#cli)。
- Rust 默认精度测试通过 82 项，f64 CLI、核心和数学通过 75 项，GUI 通过 3 项。GUI 的保存状态回归覆盖连续改参数、同路径重载和旧渲染完成。f64 导出用每个流输出 1 MiB 的测试子进程验收。Metal 上验证了四种输出传递格式和线性负值，CUDA 后端做了编译检查。

## P2 默认胶片质感、GPU 与文件工作流

- **CPU 光学和胶片颗粒已对齐。** 感光度应用 UV／IR 后保留参考光源下各通道的响应，曝光和印相中灰标定共用过滤后的感光度。晕光按片种预设，零半径不改变曝光，散射可独立生效。CPU f32／f64 默认使用多层颗粒、染料云和微结构。关闭多层或层数据不可用时走既有聚合模型。出处见 [感光度校正](../crates/spektrafilm-core/src/spectral_service.rs)、[晕光](../crates/spektrafilm-model/src/diffusion.rs) 和 [显影](../crates/spektrafilm-core/src/stages/filming.rs)。
- **GPU 以 CPU 同配置为基准。** WGSL 和 CUDA 常驻路径的印相曝光倍率漏乘预闪项（放大机柔焦关闭时），CUDA 在色域压缩之前限幅，常驻路径也未消费内部印相／扫描 LUT 选项。用曝光倍率 0.5／2.0、非零预闪、强饱和色和锐化边缘分别比较 CPU 直接光谱、CPU LUT、WGSL 和 CUDA。出处见 [GPU 参数](../crates/spektrafilm-core/src/pipeline.rs#L828)、[WGSL 印相](../crates/spektrafilm-shaders/wgsl/print_spectral.wgsl#L61) 和 [CUDA 扫描](../crates/spektrafilm-gpu/src/cuda_backend.rs#L2119)。
- **GUI 保存和进程管理。** 将预览结果绑定文件和参数代次，避免切换文件／片种时保存旧输出。导出子进程的日志应及时排空，避免写满管道。用延迟预览和大量日志的测试子进程验收。随后补独立预览分辨率、全尺寸导出和设置持久化。出处见 [预览保存](../crates/spektrafilm-gui/src/main.rs#L644) 和 [导出子进程](../crates/spektrafilm-gui/src/main.rs#L2206)。
- **RAW 和容器输出。** 在感光模拟前单独比较 rawpy 的线性 ACES AP0 和 rawler 的线性 sRGB 路径。共用 Rust 解码入口，先用非对称竖拍 RAW、Bayer 和 X-Trans 验收方向、裁切、灰阶和色块，再做白平衡、色温／色调和 Lensfun。优先补浮点 TIFF／EXR 和与成品相符的 ICC、EXIF。出处见 [Rust RAW](../crates/spektrafilm-cli/src/main.rs#L444)、[Rust 整数输出](../crates/spektrafilm-cli/src/main.rs#L551)。

### P2 本轮验证

f64 CPU 与 Python 0.3.4 共用 64×48 场景线性 ProPhoto RGB TIFF（红绿渐变、蓝色棋盘格，5 µm／像素）。各用例关闭自动曝光、DIR、颗粒和其他空间效果，单独开启待测项。下表是胶片曝光对数的最大绝对误差。

| 配置 | P1 | P2 |
| --- | ---: | ---: |
| Portra 400，Hanatos，UV `[1, 430, 8]`、IR `[1, 650, 15]` | `0.571244` | `2.50e-15` |
| Portra 400，Mallett，同一滤镜 | `8.663662` | `6.79e-14` |
| Portra 400，晕光半径和散射均为零 | `0.021189` | `2.61e-15` |
| Portra 400，零晕光、开启散射 | `0.053643` | `4.11e-15` |
| Portra 400，显式强度 `[0.18, 0.10, 0.12]` 和半径 `[0, 32, 0]` µm | `0.071882` | `6.88e-15` |
| Gold 200／Vision3 500T，各自的默认晕光，关闭散射 | `0.034506`／`0.139845` | `8.05e-14`／`9.63e-15` |

- 打开 UV／IR 时，Hanatos 和 Mallett 的印相曝光对数最大误差均小于 `3e-15`。关闭滤镜的曝光逐位与 P1 一致。Rust 的 `HalationParams` 强度和半径字段现用 `Option`，显式覆盖写作 `Some([…])`，JSON 仍接收数组。
- 颗粒用 192×192 线性灰阶图测胶片 CMY，三条色带的输入值分别为 0.045、0.18、0.72。统计区域排除每条色带边缘 6 像素。4 µm／像素时，Portra 400 和 Velvia 100 的最坏相对 RMS 误差由 `64.5%`／`61.8%` 降至 `4.57e-8`／`4.61e-8`（比例）。最大均值误差 `1.14e-11` OD，相邻像素和跨通道相关系数的最大差分别为 `3.39e-8` 和 `2.58e-9`。Rust 重跑及显式关闭分层后的 P1 对照均逐位一致。Double-X 的三通道共用同一颗粒场。
- 0.4 µm／像素时单独验证微结构，另以 4 µm／像素验证快速采样。该次运行的最坏相对 RMS 误差分别为 `0.83%` 和 `2.57%`。Python Numba 采样序列独立，按统计量比较。GPU 预览继续使用聚合颗粒。
- Rust 默认精度 91 项、f64 89 项、GUI 3 项测试通过，CUDA 通过编译检查。Metal 上零晕光与关闭晕光输出逐位一致，散射在零晕光时独立生效。UV／IR 场景的 GPU／CPU 线性 RGB 最大绝对差为 `6.26e-6`。独立散射场景的 GPU／CPU 差为 `0.0257`，常驻 GPU 散射近似需要单独对齐。

## CPU 测光、缩放与文件读写

本轮分支为 `codex/python-parity-geometry-io`，起点 `f4e02d3`。对照用本机 `/Users/ougu/Downloads/spektrafilm-main`，对应 Python `0.3.4` 官方提交 [`3bb2c2d`](https://github.com/andreavolpato/spektrafilm/commit/3bb2c2d2801ff68b92019cf1dbcbb133d60832bc)。环境沿用本交接的 macOS arm64、Rust 1.89.0 与 Python 3.13.13。

### CPU 测光与缩放

测光沿用全图、裁切前的顺序，先对缩小的轴做 Gaussian 抗混叠，再进行近邻采样和 CCTF 解码。工作图改为基于像素中心的三次 B-spline、镜像边界与原数据范围裁切。CPU 镜头模糊、扫描锐化与 DIR 的小半径 FIR 使用反射边界。共享输入保留文件存储的传递函数，Metal 常驻路径在上传前按同一顺序应用测光和解码。

| 用例 | 对照指标 | 本轮结果 |
| --- | --- | ---: |
| 512×384 线性棋盘，灰度交替 0.02／0.8，默认中心测光 | 胶片曝光对数最大绝对误差，此前对应约 3.81 EV | `4.66e-15` |
| 原版 667×1000 ProPhoto 人像，默认中心测光 | 胶片曝光对数最大绝对误差 | `7.99e-15` |
| 64×48 线性 ProPhoto 高频色板，工作比例 0.75×／1.5× | 胶片曝光对数最大绝对误差 | `3.22e-15`／`3.41e-15` |
| 带负值和超白的非对称边缘裁切，工作比例 1.5× | 胶片曝光对数最大绝对误差，输出 15×22 | `2.13e-14` |
| 相机模糊 8 µm、扫描模糊 2 px、扫描锐化、DIR，单项开启 | 相机曝光、扫描 RGB、扫描 RGB、胶片 CMY 的最大绝对误差 | `1.28e-15`／`1.22e-9`／`6.33e-9`／`1.39e-7` |

复跑时将色板写为 float32 TIFF。令 `x=0…63`、`y=0…47`，RGB 分别取 `0.035+0.65*x/63`、`0.06+0.60*y/47`、`0.08+0.4*((x+y)%2)`。Python 收集 `log_e_film` 或 `cmy_film`，Rust 用 `SPEKTRAFILM_DUMP_FILM_LOG_RAW` 或 `SPEKTRAFILM_DUMP_FILM_DENSITY` 对读，模型尺度用 `film_format_mm=0.32`，实拍测光用 35 mm。

共有 28 款片纸用同一高频色板和默认 35 mm 图幅遍历，负片配目标相纸、正片走直扫、相纸用 Portra 400。关闭 AE、颗粒和眩光，保留默认 DIR、晕光和锐化。最终线性 RGB 最坏最大绝对误差从 `0.003657` 降到 `3.16e-7`，最坏 P99 从 `0.000580` 降到 `2.32e-7`。出处见 [测光](../crates/spektrafilm-core/src/stages/filming.rs)、[缩放](../crates/spektrafilm-math/src/resize.rs) 和 [CPU 光学](../crates/spektrafilm-model/src/diffusion.rs)。

Double-X、Tri-X 和 2302 的 `log_sensitivity: null` 现转换为零线性感光度，真实 log 值 `0` 仍对应灵敏度 `1`。以旧二进制和替换为空波段近零灵敏度（log 值 −200）的临时 profile 作参考，两套片纸组合的线性输出从最大误差 `0.379351`／`0.514453` 降为逐位一致。这是 Rust 扩展片种自己的验证。出处见 [profile 读取](../crates/spektrafilm-core/src/profile.rs)。

### RAW 与图像读写

RAW 由 CLI、GUI 和 `decode_raw_gui` 共用。方向及有效区在模拟前应用，读取结果及诊断 TIFF 标注为线性 ACES2065-1。CLI 支持 as-shot、daylight、tungsten 和 custom 白平衡。原生 Rawler 的解码、去马赛克及高光响应与 Python 的 LibRaw 不同，以下误差在共同的线性 ACES AP0 中测量。

| 样本 | Rust 与 Python 的解码尺寸 | ACES AP0 P99 绝对误差 | 平均绝对误差 |
| --- | --- | ---: | ---: |
| 合成 Bayer DNG，Orientation=6，ActiveArea 92×60，DefaultCrop 88×52 | 60×92，此前 Rust 为 88×52 | `0.02403` | `0.000711` |
| [Nikon NEF](https://github.com/letmaik/rawpy/blob/v0.26.1/test/iss030e122639.NEF) | 4284×2844 | `0.00982` | `0.001549` |
| [Fuji X-T2 RAF](https://raw.pixls.us/getfile.php/865/nice/Fujifilm%20-%20X-T2%20-%2014bit%2014bit%20compressed%20%283%3A2%29.RAF)，X-Trans | 6032×4028 | `0.01406` | `0.001478` |

DNG 直接渲染与输出后重新读取带 ACES 标记的 float32 TIFF，在 f64 胶片曝光阶段逐位一致。以 DNG 测 as-shot、daylight、tungsten、自定义 5000 K／1.15，曝光对数 P99 绝对误差为 `0.0316–0.0504`。NEF 原生高光最大到 `1.4163`，Python 的 16 位 RAW 输出最大为 `1.0`。单像素最大差异为 `1.1444`，后续需单独验证去马赛克与高光。

CLI 使用 `--raw-lens-correction` 开启 Lensfun。匹配到直线投影镜头时应用暗角、自动缩放、径向畸变与逐通道 TCA，顺序在分析白平衡和 tint 前。未匹配到镜头时保持原图。CLI 端到端实测 Fuji 裁切图的线性 RGB 最大变化为 `0.00571`，缺镜头信息的 DNG 重跑逐位一致。以同一份 Python 解出的 Fuji 6032×4028 线性 ACES 输入隔离验证，XF35mmF1.4 R @35 mm、f/2.8 的校正结果与 Python lensfunpy 的 P99 绝对误差为 `0.001779`，平均 `0.0002291`，最大 `0.009976`。同一输入关闭校正时，对 Python 校正结果的 P99 为 `0.103072`。非直线投影到直线投影的转换仍待对齐。原生依赖是 [Lensfun Rust 0.7](https://github.com/vdavid/lensfun-rs)，算法许可为 LGPL-3.0-or-later，捆绑数据库许可为 CC BY-SA 3.0。

PNG、JPEG、TIFF、EXR 共用读写。文件中可识别的色域和传递函数优先用于读取，未标记 TIFF 沿用配置，未标记 PNG/JPEG 按 sRGB 编码读取，未标记 EXR 按线性 sRGB 读取。CLI 用 `process --bit-depth 32` 生成 float32 TIFF／EXR，GUI 在 **Output** 选择 **TIFF / EXR bit depth**。设置 `io.output_cctf_encoding=false` 可生成线性输出。

同一 f64 渲染缓冲由 Rust CLI 与 Python OpenImageIO 保存，输入为存有 EXIF 和 XMP 的 TIFF，输出尺寸 48×36。PNG8、TIFF 8／16／32 和 EXR half／float 的读回数据逐值一致。TIFF32／EXR32 保留线性范围约 `[-1.68706, 2.00211]`，EXR half 符合独立半精度转换。TIFF／PNG／JPEG 生成与实际样本匹配的 ICC，刷新 EXIF 方向、像素尺寸、软件名，并保留受支持的 EXIF、GPS、XMP 与 IPTC。Exiv2 在 TIFF、JPEG、PNG 之间的 9 种转存中均读回同一 IPTC 标题。EXR 写色度和传递函数。JPEG 沿用 Rust 质量 95，厂商 MakerNote、内嵌缩略图和 EXR 通用容器元数据仍待对齐。出处见 [加载与颜色识别](../crates/spektrafilm-io/src/input.rs)、[RAW](../crates/spektrafilm-io/src/raw.rs) 和 [栅格读写](../crates/spektrafilm-io/src/raster.rs)。

工作区默认精度（GUI 单列）通过 103 项，f64 通过 101 项，GUI 通过 3 项。GUI 子进程测试同时检查 32 位选项转发和大量日志排空。Metal 实测编码及线性输入的常驻路径，CUDA 通过编译检查。

## 色彩与光谱选项

基于 `fe18fe8`，对照本地 `/Users/ougu/Downloads/spektrafilm-main` 的 Python 0.3.4（`3bb2c2d`）和 colour-science 0.4.7。

- 照片渲染、图像元数据和单 LUT 共用 RGB 色彩空间定义。V-Gamut/V-Log 可通过 `io.input_color_space` 和 `io.input_cctf_decoding` 处理无标签 TIFF。线性 V-Gamut TIFF 写入 ICC，V-Log EXR 通过标准原色坐标和项目自定义的传递函数标签往返。编码 V-Log 的 TIFF、PNG、JPEG 沿用无 ICC 写入，TIFF 再读时需要显式指定空间和解码。
- CAM16-UCS 根据目标空间的原色和白点构建色域边界，OkLCh 和 OkLRab 也使用目标空间的矩阵。非 sRGB 的感知色域压缩走分段渲染，由 CPU 执行这一步。
- Hanatos 应用 profile 的 3×15 系数曝光校正，并支持沿波长轴的高斯模糊。模糊的 sigma 单位与 Python 的实际调用一致，为波长采样间隔。输入侧 `input_gamut_compress.active=false` 生效。放大机支持 `enlarger.illuminant="TH-KG3-L"`，缺少数据库中性滤片组合时使用配置的中性滤片。

### 对照验证

用 Ektar 100 / Ultra Endura 做 32×24 合成图，线性输入每像素为 `[0.035 + 0.65*x/31, 0.06 + 0.60*y/23, 0.08 + 0.4*((x+y)%2)]`。色彩空间缺省使用线性 ProPhoto RGB 输入和线性 sRGB 输出。V-Log 输入用 colour-science 编码后存为无标签 TIFF32。关闭自动测光、DIR、卤光、高光增强、颗粒、眩光、锐化和内部打印／扫描 LUT，保留默认输出 CAM16-UCS 及中性滤片查表。Python 使用 `SimulationPipeline.process` 的同名阶段，Rust 使用 f64 CLI 的阶段转储与 `--raw-out`。下表为相对 Python 的最大绝对误差。

| 用例 | 比对阶段 | `fe18fe8` | 本轮 CPU f64 |
|---|---|---:|---:|
| 默认 sRGB | 最终线性 RGB | 1.89e-9 | 1.89e-9，与修改前逐字节一致 |
| Rec.2020 / CAM16-UCS | 最终线性 RGB | 6.28e-2 | 1.88e-9 |
| ProPhoto RGB / CAM16-UCS | 最终线性 RGB | 2.15e-2 | 1.89e-9 |
| V-Log 输入，Hanatos / Mallett | 胶片 log E | 6.03e-1 / 9.98 | 2.66e-15 / 7.79e-14 |
| V-Log 输出 | 最终编码 RGB | 1.68 | 3.56e-10 |
| Hanatos 校正面 + 光谱模糊（sigma 1.25） | 胶片 log E | 4.61e-1 | 3.22e-15 |
| 光谱模糊，关闭光谱窗口 | 胶片 log E | 6.07e-2 | 3.89e-15 |
| `TH-KG3-L` | 相纸 log E | 2.20e-2 | 2.11e-15 |
| 关闭输入色域压缩 | 胶片 log E | 3.09e-1 | 3.72e-15 |

25³ V-Log → sRGB 单 LUT 使用同一胶片／相纸，与上游 `BundleSpec(topology="1lut", input_color_space="vlog", output_color_space="srgb", resolution=25)` 逐节点比较。全部通道最大误差 `6.76e-5`，P99 `6.18e-5`。对原生 V-Log 18% 中灰进行三线性采样，Rust 输出亮度 Y 为 `0.1579288`，Python 为 `0.1579216`，相对 18% 均约低 0.189 档，满足上游 0.5 档容差。

```bash
target/debug/spektrafilm-f64 export-lut --input-encoding vlog \
  --film kodak_ektar_100 --paper kodak_ultra_endura --size 25 -o /tmp/vlog.cube
```

Apple Silicon 的 Metal/wgpu f32 端到端验证覆盖 Rec.2020、ProPhoto、V-Log 输入与输出、Hanatos 校正面和模糊组合、`TH-KG3-L`，相对 Python 的最终 RGB 最大绝对误差为 `6.65e-6`。默认 workspace 测试通过 109 项，CLI 和相关库在 f64 下通过 104 项。CUDA feature 通过编译检查，硬件待测。

其他相机 Log 和非 sRGB 感知压缩的 GPU 常驻路径仍待对齐。GUI 工作流及 2／3／4-LUT 仍按独立任务推进。

## 输入 OkLCh 与输出 JzAzBz

基于 `858c9dd`，对照本地 `/Users/ougu/Downloads/spektrafilm-main` 的 Python 0.3.4（`3bb2c2d`）和 colour-science 0.4.7。GUI 的 **Color management** 提供两种新选项，CLI `process --params` 使用 `io.input_gamut_compress.algorithm="oklch"` 和 `io.output_gamut_compress.algorithm="jzazbz"`。

- 输入 OkLCh 在 Y=1 的 OkLab 坐标下从可见光谱轨迹构建 `C_max`，保留明度和色相，重映射预计算的 TC LUT。64×720 边界表的 46,080 项与 Python 逐位一致，包含 `y=0` 边界在内的 7 个 xy 样本最大误差为 `1.25e-16`。64×64×3 合成 TC LUT 重映射最大误差为 `5.60e-14`，P99 为 `1.54e-14`。
- 输出 JzAzBz 以目标空间白点和 100 cd/m² 参考亮度计算边界，沿用现有明度压缩配置。每个目标空间比较 523 组 RGB，包括 `np.random.default_rng(12017).uniform(-0.05, 2, (512, 3))` 和 11 组黑位、近黑、中灰、原色、负通道及超白样本。sRGB、Rec.2020、ProPhoto、ACES2065-1、V-Gamut 之中的最大绝对误差为 `5.41e-8`，关闭明度压缩时为 `7.25e-13`。浮点输出保留上游压缩结果的范围，个别饱和 ProPhoto 超白样本仍可超过 1。

### 端到端验证

使用 [阶段对照脚本](../scripts/parity/spektra_compare.py) 的 8×8 线性 ACES2065-1 色板。色板包含线性值 0–4 的灰阶、饱和色块和混合高光。相纸路线用 Ektar 100／Ultra Endura，直扫用 Provia 100F。关闭自动测光、DIR、空间和随机效果。下表为 CPU f64 相对 Python 的最大绝对误差。

| 输入 / 输出压缩和目标 | 胶片 log E | 最终线性 RGB |
|---|---:|---:|
| 默认 xy / CAM16-UCS，sRGB | 2.44e-15 | 2.91e-9，与 `858c9dd` 逐字节一致 |
| OkLCh / CAM16-UCS，sRGB | 1.11e-15 | 2.91e-9 |
| xy / JzAzBz，sRGB | 2.44e-15 | 2.04e-9，修改前为 0.21317 |
| xy / JzAzBz，ProPhoto | 2.44e-15 | 2.14e-9 |
| OkLCh / JzAzBz，Rec.2020 | 1.11e-15 | 2.03e-9 |
| 输入关闭 / JzAzBz，sRGB | 1.78e-15 | 2.04e-9 |
| OkLCh / JzAzBz，sRGB 直扫 | 1.44e-15 | 3.51e-9 |

```bash
PYTHONPATH="/Users/ougu/Downloads/spektrafilm-main/src" "$parity_python" \
  scripts/parity/spektra_compare.py --synthetic --film kodak_ektar_100 \
  --paper kodak_ultra_endura --input-gamut oklch --output-gamut jzazbz \
  --output-color-space 'ITU-R BT.2020' --rust-bin target/debug/spektrafilm-f64 \
  --out-dir "$(mktemp -d)"
```

Apple Silicon Metal/wgpu 对照中，输入 OkLCh 使用 GPU 常驻路径，JzAzBz 使用分段渲染并在 CPU 执行压缩。5 组配置相对 Python 的 Metal 最终 RGB 最大绝对误差为 `5.68e-6`，CPU f32 为 `9.56e-8`。Python 自带色域压缩测试通过 71 项。

Hanatos 与 Rust 扩展 `arctic2026alpha02` 均通过 CLI 验证。对两种上采样方法，选择 OkLCh 后设 `active=false` 与 `algorithm="off"` 的胶片 log E 逐字节一致。Arctic 验证仅使用 Rust。

默认 workspace 通过 111 项测试，CLI 和相关库在 f64 下通过 106 项。CUDA feature 通过编译检查，硬件待测。

## 当前优先级与剩余差距

本次以 Rust `main` 的 `8be7ec5` 和下载的 Python 0.3.4（`3bb2c2d`）为审阅基准。下文优先级适用于下一轮开发。GUI 审阅涵盖打开、预览、参数编辑、全尺寸输出和持久化，LUT 审阅涵盖阶段数值、包结构和离网格验收。

### P0 保证 GUI 导出的输入与目标文件完整

1. **导出成功后才替换目标。** 当前 f64 子进程直接写用户选中的路径，TIFF 写入会先截断旧文件。用 128×96 源图导出 64×48 的 TIFF32，已有文件为 1,504 字节。给子进程设置 1,024 字节的文件大小上限后，CLI 返回 1（`File too large`），目标已变成 1,024 字节。预览 Save 和 f64 Export 都应写到同目录临时目标，成功并关闭文件后再替换。验收涵盖成功覆盖旧文件、编码失败、取消和目标提交失败。失败或取消后旧文件哈希保持一致，新目标也只在成功后可见。沿用已有的预览代次校验与子进程日志处理。出处见 [GUI f64 导出](../crates/spektrafilm-gui/src/main.rs#L1993)、[取消路径](../crates/spektrafilm-gui/src/main.rs#L2026) 和 [图像写入](../crates/spektrafilm-io/src/raster.rs#L177)。
2. **让全尺寸导出使用本次打开的输入。** GUI 缓存解出的图像供预览，f64 导出按路径重新读取，外部覆盖同名文件后可能处理另一张图。Python 的 Preview 和 Scan 从已加载的两个图层取样。把源身份、开发选项、旋转和有效参数绑定到导出请求，采用固定源或在解码时验证源身份，保留 f64 导出的语义。验收覆盖打开图 A 后改名、删除或替换同路径为图 B、导出过程中切换图／片纸。请求要么使用图 A 和提交时的参数，要么给出明确的源变更错误。此项来自源码路径，未通过交互 GUI 复现。出处见 [Rust 载入](../crates/spektrafilm-gui/src/main.rs#L354)、[Rust 导出请求](../crates/spektrafilm-gui/src/main.rs#L681) 及 Python `spektrafilm_gui/controller.py:524,689`。

### P1 补齐 GUI 日常工作流

1. **独立的低分辨率预览与全尺寸 Scan。** Rust 默认用原图处理预览，Working resolution 有意同时控制预览和导出。CLI 已验证 0.5× 会把 128×96 的输出缩到 64×48。Python 保留原图，另建有长边上限的抗锯齿预览图，并支持手动 Preview、Scan 与自动预览。分开预览长边上限和用户主动选择的导出尺寸，首轮把发生在主线程的管线初始化移到预览 worker。验收在同一张大图上验证预览有上限、Scan 保持裁切后的指定全尺寸、相同裁切几何和曝光规则、连续修改时最新请求生效。已有 Crop 数值控件、旧结果防护和取消机制可复用。出处见 [Rust 预览](../crates/spektrafilm-gui/src/main.rs#L425)、[Working resolution](../crates/spektrafilm-gui/src/main.rs#L1439) 及 Python `spektrafilm_gui/controller.py:274,489,683` 和 `spektrafilm/utils/preview.py`。
2. **补输入色彩、显示与 RAW 开发选项。** GUI 已识别图像标签并在每次加载时重置回退值。无标签 V-Log TIFF 目前按线性 ProPhoto 处理，GUI 缺少空间／解码覆盖、目标输出空间选择，预览纹理按 sRGB 显示。原版具有输入解释、模拟输出、保存色彩空间和显示变换的独立语义。先复用 Rust 已有的色彩定义、RAW 白平衡与 Lensfun 实现，再接 GUI 控件、手动旋转和重新开发，将显示变换与写出的浮点数据及标签分开。验收覆盖带标签和无标签 V-Log、ProPhoto／Rec.2020 色块、带方向的 Bayer／X-Trans RAW、非默认 WB／Lensfun 及 TIFF32／EXR32 的保存读回。出处见 [Rust 输入](../crates/spektrafilm-gui/src/main.rs#L354)、[显示纹理](../crates/spektrafilm-gui/src/main.rs#L1802)、[RAW 开发选项](../crates/spektrafilm-io/src/raw.rs#L41) 及 Python `spektrafilm_gui/controller_runtime.py:137,158`、`controller.py:186,237,321`。
3. **可恢复的参数会话。** 原版支持启动默认、保存／载入 GUI JSON、恢复出厂设置和记住目录。Rust 当前从内置参数启动，导出 JSON 仅用于子进程。在渲染请求参数稳定之后加带版本的本机设置和可携带预设，区分渲染参数与窗口／路径状态。明确首次启动和换片纸规则。Python 默认 Gold 200／Supra Endura，Rust 默认 Gold 200 的 profile 目标 Portra Endura，同一配置换纸有可见影响。验收覆盖重启、旧 JSON 缺字段、损坏 JSON、切换负片／正片和恢复默认。出处见 [Rust 启动](../crates/spektrafilm-gui/src/main.rs#L289)、[片纸切换](../crates/spektrafilm-gui/src/main.rs#L881) 及 Python `spektrafilm_gui/persistence.py:45,57,75,94`、`state.py:351`。

### P1 先让 2-LUT 可构建、可复跑

1. **建立可注入和收集的确定性阶段接口。** Python 可通过 `SimulationPipeline.process(inject=…, collect=…)` 运行子链。Rust 已有阶段实现和只读转储，公开的 `Pipeline::process` 仍运行整条链。复用曝光／显影、印相、扫描和 `new_with_spectral`。首轮公开 `rgb_in → cmy_film` 与 `cmy_film → rgb_out`，并验证以底雾为基准的物理 CMY、每张相纸的中性滤片、预闪和曝光归一化、扫描参考值。LUT 模式保留逐像素 DIR，关闭空间、随机和全图相关操作，包括扫描黑白校正。验收先对照未量化阶段，再对照连接后的线性 RGB，分别验证单点、调序、拆批结果一致，以及换相纸只改变下游。出处见 [Rust 管线](../crates/spektrafilm-core/src/pipeline.rs#L454)、[印相](../crates/spektrafilm-core/src/stages/printing.rs#L153)、[扫描](../crates/spektrafilm-core/src/stages/scanning.rs#L145) 及 Python `spektrafilm/runtime/pipeline.py:101,174`、`params_builder.py:99`。
2. **把传递函数与曝光作为包契约。** 现有 Rust `export-lut` 为一张相纸生成一个 SDR sRGB CUBE，输入支持线性 ProPhoto、sRGB 和 V-Log。实测 `--topology 2lut` 返回参数错误。新包显式保存 `native` 和 Python `auto` 的有效曝光、输入／输出原色和传递函数。输入增益在解码之后，且中间值范围的探针和实际烘焙用相同增益。Python 的 sRGB `auto` 为中灰以上 4 档、增益 2.88，V-Log 原生约 8 档、增益约 1。Ektar 100／Ultra Endura 的 17³ sRGB 单 CUBE 对照 Python `auto`，最大代码值差为 `0.44246`，两边改用原生曝光后为 `6.73e-5`。旧单 CUBE 的原生默认应保持可用。出处见 [现有单 CUBE](../crates/spektrafilm-cli/src/main.rs#L414) 及 Python `spektrafilm_lut_creator/color_spaces.py:560,611`、`builders.py:879`。
3. **交付一卷胶片、两张相纸的 CUBE 目录。** 2-LUT 是一个共享胶片 CUBE 加每张相纸各一个 CUBE。按 Python 以 9³ 编码采样探测胶片 CMY 上界，解码后施加增益并转 float32，三通道 `d_min=-0.2`，`d_max` 取观测峰值×1.05 后向上取四位小数。记录 schema v1 `bundle.json` 的版本、片纸、有效配置、传递函数、中间值范围和每个 LUT 的角色／路径／范围，附 README 和许可证。验收三个 CUBE 的计数、红轴最快的网格、两张相纸输出有区分、同一胶片段实际共享、文件读回复合与 Python 同设定链的对照。首个画质用例可用 Ektar 100／Ultra Endura、33³ sRGB→sRGB、`auto`，上游实际链的三线性 ΔE00 P99／最大值为 `0.267／0.863`，四面体为 `0.264／0.837`。出处见 Python `spektrafilm_lut_creator/builders.py:443,797`、`wires.py` 和 `metadata.py:102,137,176`。

### P2 复用阶段与包结构推进 3／4-LUT

| 拓扑 | 外部阶段连接 | 两张相纸的实际 canonical CUBE 数 |
|---|---|---:|
| 2-LUT | `rgb_in → cmy_film → rgb_out` | 3 |
| 3-LUT | `rgb_in → log_e_film → cmy_film → rgb_out` | 4 |
| 4-LUT | `rgb_in → log_e_film → cmy_film → log_e_print → rgb_out` | 6 |

3-LUT 新增共享的胶片曝光对数边界。4-LUT 增加每张相纸各自的印相曝光与印相显影／扫描，`cmy_print` 仍是内部阶段。log E 边界的标量下／上限在探测极值上各外扩 0.1，再向外取四位小数。兼容模式需记录 Python 用首张相纸测共用 `log_e_print` 范围的行为。已验证 Portra 400 配 Portra Endura／2393 的 9³ 输入探针中，首张相纸范围 `[-1.0669, 0.9647]` 会让 2393 的 2,187 个通道值中有 11 个越界。验收要对调相纸顺序、测可达输入及中间全网格的越界量、保留贴边行为。跨纸并集或逐纸范围如需支持，应以显式新契约记录。另已验证 Python 新建管线从 `log_e_print` 注入且启用黑白校正时，因参考值未初始化而报错。Rust 若扩展到普通渲染的任意阶段入口，需要显式准备相纸参考值。出处见 Python `spektrafilm_lut_creator/builders.py:532,647,686,721,836`、`wires.py` 及 `spektrafilm/runtime/stages/printing.py:39`。

先以 Python 同一拓扑的阶段、范围、网格和文件读回结果验收移植精度，再测多段插值对完整模拟的绝对画质。Ektar 100／Ultra Endura 的 V-Log→sRGB 33³ 包，按原版固定种子抽 50,000 个离网格输入，逐级应用读回 CUBE 后，相对直接模拟的 ΔE00 如下。Python 自带报告还会先重采样为有效单 LUT，两个测试口径应各自报告。

| 拓扑 | 三线性 P99／最大 | 四面体 P99／最大 |
|---|---:|---:|
| 2-LUT | 0.684／8.832 | 0.590／11.958 |
| 3-LUT | 1.175／13.552 | 1.019／15.941 |
| 4-LUT | 1.189／13.601 | 1.025／15.936 |

因此本组 33³ 参考本身超过上游目标的最大 ΔE00 2。后续在确定分辨率、范围和样本覆盖后再设交付门槛。出处见 Python `spektrafilm_lut_creator/qa/suite.py:137`、`qa/tests/lut_fidelity.py:21` 和 `qa/reference.py:37,166`。

### P3 扩展交付和剩余功能

- canonical 包稳定后加 ZIP、组合 LUT、可读 QA 报告和 OCIO。Python 两张相纸开启组合后实测 2／3／4-LUT 共 5／9／17 个 CUBE。OCIO 配置应按 canonical 顺序组合 FileTransform，核验多相纸视图选择和处理器输出。此次只确认上游能生成配置，临时环境缺少 OCIO 绑定。
- 按真实宿主需求补更多色彩空间、HDR 输出增益和格式。Python 当前 CLI 有 Lumix V-Log 目标，格式模块另有 3DL 和 Hald PNG。当前快照没有 CLF writer，外部输入 log shaper 仍在开发。内部 CMY／log E 的仿射编码已经可用。
- GUI 次级项包括显示布局、边框、格式相关的位深提示，以及已移植但尚未暴露的 Hanatos 校正／模糊和放大机光源。其余核心工作包括 GPU 常驻的非 sRGB／JzAzBz、散射／分层颗粒对齐、CUDA 硬件、RAW 去马赛克和高光质量，以及自定义 profile 校验。打包回归应覆盖任意工作目录的 GUI 和 f64 子进程。出处见 [发布流程](../.github/workflows/release.yml)。

### 审阅验证范围

Rust GUI 的 3 项回归通过，Python GUI 参数映射通过 16 项。Python builder／combination 测试为 127 通过、1 失败，失败用例要求已安装发行包的 provenance 版本，本次通过 `PYTHONPATH` 读取源码得到 `0+unknown`。Python GUI 的交互窗口与持久化套件未实测，临时环境缺少 Qt／Napari。上述导出失败和尺寸是同一 f64 CLI 路径的实测。其余工作流差距根据 GUI 源码审阅。
