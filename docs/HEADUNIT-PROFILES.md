# 车机档案

更新：2026-11-04。权威来源是运行代码及有效远程配置，旧注释中的试验建议不作为当前值。

## 内置档案

| ID | 蓝牙音频配置保护 | 主动出站 iAP2 | 等待本轮 AirPlay 激活 | mDNS 优先 | 端口兜底 |
|---|---|---|---|---|---|
| bmw_id85 | false | false | true | true | 7000/7001/7100/17000 |
| generic | true | true | false | true | 7000 |
| winplay | false | true | false | false | 7000 |
| audi_mib3 | true | true | false | true | 7000/7001/7100/17000 |
| xpeng_g6 | false | false | false | true | 7000/7001/7100/17000 |

BMW / generic / 小鹏 G6 启用 sendCarPlayAvailability。BMW / generic 同时启用 openEaSessions 与 sendTransportIdentifier；WinPlay / Audi / 小鹏 G6 关闭这两项。五项档案均回复电源更新，且不因 CarPlay 窗口关闭立即主动中止。WinPlay 校验 deviceID，并保留历史触摸兜底 2732×794；这不是宝马的固定分辨率。小鹏 G6 的 touchDisplay 留空，触摸只用 /info 坐标，不改编码分辨率或画质。

### 小鹏 G6 屏幕流 SETUP

2025 海外 G6（AirTunes/475.8.3，srcvers=475.8.3）在 pair-verify、会话 SETUP、`/info`、RECORD 都 200 之后，主屏 `SETUP /` 对 catplay 原版报文体回 `400`、body=0。该体约 167 字节：`type=110`、`streamConnectionID`、`/info` 的 uuid、`latencyMs=25`。Mazda AirTunes 320 接受同一形状。

已核对、不是这次 400 的原因：

- 会话 SETUP 使用 `TimingServer` 绑定到的真实 UDP 端口。`timingPort=0` 才会让屏幕 SETUP 400；端口绑定失败时走不到会话 SETUP 的 200。成功日志现在带 `timingPort=`。
- 触摸设置仍可能是 1920×1080，而 `/info` 画布是 2560×1236。屏幕 SETUP 不含宽高；第二轮引擎配置已经是 2560×1236 时 SETUP 仍然 400。`/info` 有像素时触摸分母用显示尺寸，不把编码压到 720p。
- `enabledFeatures` 里的 `mainBuffered` 是缓冲音频，不是把主屏改成 type 111。111 是 altScreen，库存接收端对 111 走不支持分支并 400。`/info features=0x6104040280` 是顶层 AirPlay 标志（screen、audio、PCM、MFi、car、CarPlay control、HK），不是显示器特性位。

宝马仍先发原来的 25ms 体。只有这一次返回 400 时，同一条控制连接再用新的 `streamConnectionID` 发第二套，仍是 H.264 type 110：`latencyMs=70`（盖住 60fps 两帧，不改码率或分辨率）、uuid 转成 Apple `CFUUIDCreateString` 的大写形式、补上镜像 SETUP 的 `timestampInfo`（SubSu、BePxT、AfPxT、BefEn、EmEnc）。非 400 的失败不重试。

成功时日志有 `AIRPLAY: ✓ 主屏流就绪 dataPort=…`，随后状态为 connected。第二套仍 400 时引擎退出，错误里带上两次状态行。这还没有这台 G6 的实车确认。

BMW MGU-55 是当前实车基准。Audi 与小鹏 G6 尚未完成本项目实车验证；generic 的 verified 字段也不代表所有车型均已测试。未知 ID 回退到 BMW。

实现：[HeadUnitProfile](../app/src/main/java/io/github/hypercar/headunit/HeadUnitProfile.kt)。

## 远程配置

配置源为 [headunits.json](../profiles/headunits.json)，解析与合并见 [RemoteProfiles](../app/src/main/java/io/github/hypercar/headunit/RemoteProfiles.kt)。

当前 schema 为 5。达到当前 schema 的同 ID 远程档案覆盖内置档案；旧 schema 仅追加新的 ID，不覆盖内置值，也不为内置条目显示“远程”角标。省略字段继承对应内置档案；新 ID 使用 BMW 模板。刷新失败保留缓存，但旧缓存仍受 schema 隔离。

仓库下发配置已与内置连接策略对齐：BMW 的 dropProfilesBeforeIap2=false、requireAirplayActivation=true，端口兜底包含 17000。诊断仍应记录有效档案及来源，因为自定义配置可以显式覆盖这些值。

当前支持的策略字段包括：validatesDeviceId、sendCarPlayAvailability、openEaSessions、sendTransportIdentifier、replyPowerUpdates、dropProfilesBeforeIap2、allowOutboundIap2、abortWhenCarPlayWindowClosed、preferMdnsAirplay、requireAirplayActivation、fallbackAirplayPorts、touchDisplay，以及显示信息与 pairingHint。

requireAirplayActivation 已补齐解析与导出：省略时继承，显式 false 可关闭门槛。pairingHint 与 touchDisplay 省略时也继承；显式空字符串可清除。导出携带当前 _schema，保留策略及可选字段的清除状态。已废弃的 keepIap2AliveOnly 从下发配置移除，不恢复其旧行为。

新增车型先记录接收端角色、邀请、服务、触摸和日志证据，再修改对应字段；不要因另一车型文档中的固定端口、Session ID 或时间阈值就宣称已适配。

2026-11-03：用户实测主动断开宝马 HFP/A2DP 未改善二次上车，关闭该策略以继续对照。内置与仓库配置同步关闭，schema 升至 5，阻止旧缓存重新启用；其它内置车型策略沿用。
