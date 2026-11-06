//! AirPlay 计时服务（NTP over RTCP，timesync 应答）。
//!
//! 协议依据（对照参考实现 cardeck `airplay/TimingServer.kt`）：
//! 初始 SETUP 通告本端口（timingPort）；车机（媒体时钟主）向它发 32 字节 timesync
//! 请求，我们按 NTP 语义应答：t1=回显请求的 t3（客户端发送时刻）、t2=本地接收时刻、
//! t3=本地发送时刻。缺失时序（曾通告 timingPort=0）时车机无法与我们同步媒体时钟，
//! 是屏幕流 SETUP 被 400 的头号嫌疑（catplay 真车实现始终绑定真实端口）。
//!
//! 线格式（大端）：`[0x80][pt][length=7][rtp_ts=0][t1(8)][t2(8)][t3(8)]`，
//! pt：0xD2 请求 / 0xD3 应答；时间 = NTP 64 位（秒<<32 | 2^32 分数），取自单调时钟
//! ——NTP 差值计算只用相对量，任意起点均可（与 Kotlin 的 System.nanoTime 同语义）。

use tokio::net::UdpSocket;
use std::sync::Arc;
use parking_lot::Mutex;

use once_cell::sync::Lazy;

/// 非 Android 测试环境的时间基。Android 直接使用 CLOCK_MONOTONIC。
/// timesync、音频同步包和视频 PTS **必须共用同一原点**，
/// 否则车机算出的媒体时钟锚点会带一个常量偏移（音画不同步）。
static PROCESS_START: Lazy<std::time::Instant> = Lazy::new(std::time::Instant::now);

#[derive(Clone,Debug,Default,serde::Serialize)]
pub struct TimingDiagnostics {
    pub requests:u64,
    pub replies:u64,
    pub invalid_datagrams:u64,
    pub send_errors:u64,
    pub receive_errors:u64,
    pub last_request_ms:Option<u64>,
    pub last_reply_ms:Option<u64>,
    pub request_age_ms:Option<u64>,
    pub reply_age_ms:Option<u64>,
    pub last_local_reply_us:Option<u64>,
}
impl TimingDiagnostics {
    pub fn refresh_age(&mut self,now_ms:u64) {
        self.request_age_ms=self.last_request_ms.map(|t|now_ms.saturating_sub(t));
        self.reply_age_ms=self.last_reply_ms.map(|t|now_ms.saturating_sub(t));
    }
    pub fn summary(&self)->String {
        format!("requests={} replies={} invalid={} send_errors={} receive_errors={} request_age_ms={:?} reply_age_ms={:?} local_reply_us={:?}",
            self.requests,self.replies,self.invalid_datagrams,self.send_errors,self.receive_errors,self.request_age_ms,self.reply_age_ms,self.last_local_reply_us)
    }
}

/// Android 与 Java System.nanoTime/MediaCodec PTS 同源的单调纳秒数。
pub fn monotonic_nanos() -> u64 {
    #[cfg(target_os = "android")]
    {
        // MediaCodec Surface PTS and Java System.nanoTime use CLOCK_MONOTONIC.
        // A process-relative Instant here makes timing/audio disagree with video by device uptime.
        let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } == 0 {
            return (time.tv_sec as u64).saturating_mul(1_000_000_000) + time.tv_nsec as u64;
        }
    }
    PROCESS_START.elapsed().as_nanos() as u64
}

/** timesync 请求 payload type（RTCP）。 */
const PT_TIMESYNC_REQUEST: u8 = 0xD2;
/** timesync 应答 payload type。 */
const PT_TIMESYNC_RESPONSE: u8 = 0xD3;
/** timesync 报文定长（8 × 4 字节字）。 */
const PACKET_BYTES: usize = 32;

/// 单调纳秒 → NTP 64 位（秒<<32 | 分数），大端 8 字节。
///
/// 分数 = (纳秒分数 << 32) / 1e9（对照 TimingServer.ntpFromNanos 的 floorDiv/floorMod
/// 语义；单调时钟 nanos 非负，直接用 `/`、`%` 即可）。
/// 秒字段截断到低 32 位（与 Kotlin `(secs shl 32)` 的静默截断一致）——单调时钟
/// 秒数超过 2^32（约 136 年）才会触发，无实际影响。
pub fn ntp_be(nanos: u64) -> [u8; 8] {
    let secs = nanos / 1_000_000_000;
    let frac_nanos = nanos % 1_000_000_000;
    let frac = (frac_nanos << 32) / 1_000_000_000;
    let packed = ((secs & 0xFFFF_FFFF) << 32) | (frac & 0xFFFF_FFFF);
    packed.to_be_bytes()
}

/// 构造 timesync 应答；非 timesync 请求返回 None。
///
/// 校验（对照 TimingServer.buildResponse 逐条）：
///  - 报文 ≥ 32 字节；
///  - 版本位（request[0] >> 6）== 2；
///  - pt == 0xD2；
///  - length 字段（request[2..4] 大端）== 7（= 32/4 - 1，RTCP 长度以 4 字节字计）。
///
/// `now_nanos`：单调时钟当前值（任意起点）。
/// 应答布局：`[0]=0x80 [1]=0xD3 [2..4]=00 07`、rtp_ts(4..8)=0、
/// t1(8..16)=回显请求的 t3(request[24..32))、t2(16..24)=t3(24..32)=now 的 NTP。
pub fn build_timesync_response(request: &[u8], now_nanos: u64) -> Option<[u8; PACKET_BYTES]> {
    if request.len() < PACKET_BYTES {
        return None;
    }
    if (request[0] >> 6) != 2 {
        return None;
    }
    if request[1] != PT_TIMESYNC_REQUEST {
        return None;
    }
    let length = ((request[2] as u16) << 8) | (request[3] as u16);
    if length as usize != PACKET_BYTES / 4 - 1 {
        return None;
    }

    let mut response = [0u8; PACKET_BYTES];
    response[0] = 2 << 6; // V=2
    response[1] = PT_TIMESYNC_RESPONSE;
    response[2] = 0;
    response[3] = (PACKET_BYTES / 4 - 1) as u8; // length = 7
    // rtp_ts(4..8) 保持 0
    response[8..16].copy_from_slice(&request[24..32]); // t1 = 请求的 t3（ntp_xmit 回显）
    let now = ntp_be(now_nanos);
    response[16..24].copy_from_slice(&now); // t2 = 接收时刻
    response[24..32].copy_from_slice(&now); // t3 = 发送时刻
    Some(response)
}

/// AirPlay 计时服务。端口由 OS 分配，会话 SETUP 把它作为 timingPort 通告给车机。
/// IPv4 车机绑 `0.0.0.0`。控制连接是 IPv6（小鹏 G6 的 `fe80::…`）时绑 `[::]`，
/// 否则车机的 timesync 发不到这个套接字，`requests` 会一直是 0。
pub struct TimingServer {
    /** 实际绑定的本地 UDP 端口（通告给车机的 timingPort）。 */
    pub port: u16,
    /** 应答循环任务句柄（`stop()` 时 abort）。 */
    task: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    diagnostics: Arc<Mutex<TimingDiagnostics>>,
}

/// 控制对端是 IPv6 时计时口必须能收 IPv6。解析失败按 IPv4 处理（宝马路径不变）。
pub fn peer_is_ipv6(car_ip: &str) -> bool {
    car_ip.parse::<std::net::IpAddr>().map(|addr| addr.is_ipv6()).unwrap_or(false)
}

impl TimingServer {
    /// 绑定 0.0.0.0:0 并启动永久应答循环（tokio::spawn）。
    ///
    /// 非 async 签名下无法直接 await 绑定，故走「std 绑定 → 非阻塞 →
    /// `UdpSocket::from_std`」路径；`from_std` 需要注册到 reactor，因此**必须在
    /// tokio runtime 上下文中调用**（会话流程本就跑在 async 里，天然满足）。
    pub fn start_with_pipeline(pipeline:Arc<Mutex<crate::pipeline::Pipeline>>)->std::io::Result<Self> {
        Self::start_impl(Some(pipeline), false)
    }
    /// `car_ip` 为 IPv6（含链路本地）时监听 `[::]`，否则仍是 `0.0.0.0`。
    pub fn start_for_car(pipeline:Arc<Mutex<crate::pipeline::Pipeline>>, car_ip:&str)->std::io::Result<Self> {
        let ipv6 = peer_is_ipv6(car_ip);
        match Self::start_impl(Some(pipeline.clone()), ipv6) {
            Ok(server) => Ok(server),
            Err(error) if ipv6 => {
                crate::emit_log(&format!("TIMING: IPv6 bind failed ({error}); falling back to 0.0.0.0 — link-local timesync cannot arrive"));
                Self::start_impl(Some(pipeline), false)
            }
            Err(error) => Err(error),
        }
    }
    fn start_impl(pipeline:Option<Arc<Mutex<crate::pipeline::Pipeline>>>, ipv6:bool)->std::io::Result<Self> {
        let std_socket = if ipv6 {
            std::net::UdpSocket::bind(std::net::SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 0, 0, 0))?
        } else {
            std::net::UdpSocket::bind("0.0.0.0:0")?
        };
        std_socket.set_nonblocking(true)?; // tokio 要求非阻塞模式
        let port = std_socket.local_addr()?.port();
        crate::emit_log(&format!(
            "TIMING: listening {} timingPort={port}",
            if ipv6 { "ipv6=[::]" } else { "ipv4=0.0.0.0" }
        ));
        let socket = UdpSocket::from_std(std_socket)?;

        let diagnostics=Arc::new(Mutex::new(TimingDiagnostics::default()));
        let stats=diagnostics.clone();
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 256];
            loop {
                let (len, peer) = match socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(error) => {
                        stats.lock().receive_errors+=1;
                        if let Some(p)=&pipeline {p.lock().timing=stats.lock().clone();}
                        crate::emit_log(&format!("TIMING: receive failed: {error}"));
                        break;
                    }
                };
                // 与音频/视频共用时间基（见 monotonic_nanos）
                let now_nanos = monotonic_nanos();
                // 非 timesync 报文静默丢弃（Kotlin 侧只计数不回包）
                if let Some(response) = build_timesync_response(&buf[..len], now_nanos) {
                    {
                        let mut s=stats.lock();s.requests+=1;s.last_request_ms=Some(now_nanos/1_000_000);
                    }
                    let sent=socket.send_to(&response, peer).await;
                    let done=monotonic_nanos();
                    let snapshot={
                        let mut s=stats.lock();
                        match sent {
                            Ok(PACKET_BYTES)=>{s.replies+=1;s.last_reply_ms=Some(done/1_000_000);s.last_local_reply_us=Some(done.saturating_sub(now_nanos)/1000);},
                            _=>s.send_errors+=1,
                        }
                        s.refresh_age(done/1_000_000);s.clone()
                    };
                    if let Some(p)=&pipeline {p.lock().timing=snapshot.clone();}
                    if snapshot.requests==1 || snapshot.requests%10==0 || !matches!(sent,Ok(PACKET_BYTES)) {
                        crate::emit_log(&format!("TIMING: {}; local reply cost is not network RTT",snapshot.summary()));
                    }
                } else {
                    let snapshot={let mut s=stats.lock();s.invalid_datagrams+=1;s.clone()};
                    if let Some(p)=&pipeline {p.lock().timing=snapshot;}
                }
            }
        });

        Ok(Self { port, task, #[cfg(test)] diagnostics })
    }


}

/// 等到第一条成功的 timesync 应答，或预算耗尽。屏幕 SETUP 用它避免在
/// `requests=0` 时就把 type 110 发出去。
pub async fn wait_for_reply(pipeline:&Arc<Mutex<crate::pipeline::Pipeline>>, budget:std::time::Duration) -> TimingDiagnostics {
    let start = tokio::time::Instant::now();
    loop {
        let snap = {
            let mut sample = pipeline.lock().timing.clone();
            sample.refresh_age(monotonic_nanos()/1_000_000);
            sample
        };
        if snap.replies > 0 || start.elapsed() >= budget {
            return snap;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

impl Drop for TimingServer {
    fn drop(&mut self) { self.task.abort(); }
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    #[tokio::test]
    async fn live_udp_counts_replies_and_invalid_datagrams() {
        let pipeline=Arc::new(Mutex::new(crate::pipeline::Pipeline::new()));
        let server=TimingServer::start_with_pipeline(pipeline.clone()).unwrap();
        let peer=UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.connect((std::net::Ipv4Addr::LOCALHOST,server.port)).await.unwrap();
        peer.send(&[1,2,3]).await.unwrap();
        let mut request=[0u8;32];request[0]=0x80;request[1]=0xd2;request[3]=7;
        request[24..32].copy_from_slice(&123456u64.to_be_bytes());
        peer.send(&request).await.unwrap();
        let mut reply=[0u8;256];
        let len=tokio::time::timeout(std::time::Duration::from_secs(1),peer.recv(&mut reply)).await.unwrap().unwrap();
        assert_eq!(len,32);assert_eq!(reply[1],0xd3);assert_eq!(reply[8..16],request[24..32]);
        let s=server.diagnostics.lock().clone();
        assert_eq!(s.requests,1);assert_eq!(s.replies,1);assert_eq!(s.invalid_datagrams,1);
        assert_eq!(s.send_errors,0);assert!(s.last_local_reply_us.is_some());
        let mut p=pipeline.lock().timing.clone();
        p.refresh_age(monotonic_nanos()/1_000_000);
        assert_eq!(p.requests,1);
        p.refresh_age(p.last_reply_ms.unwrap()+5000);assert_eq!(p.reply_age_ms,Some(5000));
        drop(server);
    }

    #[test]
    fn link_local_car_selects_ipv6_timing_socket() {
        assert!(peer_is_ipv6("fe80::382a:8bff:feca:3dae"));
        assert!(!peer_is_ipv6("192.168.43.1"));
        assert!(!peer_is_ipv6("not-an-ip"));
    }

    #[tokio::test]
    async fn ipv6_listener_answers_loopback_and_wait_returns_on_first_reply() {
        let pipeline=Arc::new(Mutex::new(crate::pipeline::Pipeline::new()));
        let server=TimingServer::start_for_car(pipeline.clone(),"fe80::382a:8bff:feca:3dae").unwrap();
        assert!(server.diagnostics.lock().requests==0);
        let peer=UdpSocket::bind("[::1]:0").await.unwrap();
        peer.connect(format!("[::1]:{}", server.port)).await.unwrap();
        let mut request=[0u8;32];request[0]=0x80;request[1]=0xd2;request[3]=7;
        peer.send(&request).await.unwrap();
        let snap=wait_for_reply(&pipeline, std::time::Duration::from_secs(1)).await;
        assert_eq!(snap.replies,1);
        assert!(snap.requests>=1);
        let missed=wait_for_reply(&Arc::new(Mutex::new(crate::pipeline::Pipeline::new())), std::time::Duration::from_millis(40)).await;
        assert_eq!(missed.replies,0);
        drop(server);
    }
}
