# Python 0.3.4 功能对齐交接

参考实现是本机 `/Users/ougu/Downloads/spektrafilm-main` 里的 Python `0.3.4` 源码快照。Rust 审阅基点是 `31da6f8`（基于 Rust `v0.1.2`）。下文的源码行号以该提交为准。审阅按 CPU 光谱与胶片模型、RAW／文件／LUT、GUI／CLI／GPU 三条线展开。

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

## P3 进阶交付和持续验证

单 LUT 达标后，先稳定胶片 CMY 的 2-LUT 接口，再做 3／4-LUT、曝光 shaper、OCIO 和其他交付格式。输入／输出进阶色域算法、Hanatos adaptation surface、自定义 profile 形状校验各自验收。CI 增加 CPU f32／f64 回归和任意工作目录下的打包冒烟测试，Metal、CUDA 用对应硬件测画质。出处见 [现有发布流程](../.github/workflows/release.yml)。
