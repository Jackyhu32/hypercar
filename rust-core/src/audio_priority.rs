//! Phone-side audio priority for a busy SoftAP (+xpeng4).
//!
//! 2026-11-06 +xpeng3.3 log: music RTP stays near 136/s until a ~1.7MB CarWith
//! IDR, then one second falls to 127/s and the next bursts to 147/s. During
//! that stress `队列 media=` sits in the tens of milliseconds, so one late
//! tick inserts silence. The old mix cap (500ms) was not what clipped that
//! trace. Screen SETUP, resolution, and PCM48k/SPF352 are unchanged.

use std::collections::VecDeque;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// 48 kHz stereo. Matches the audio pump; not a second clock.
const RATE: u64 = 48_000;

pub const MEDIA_CAP_MS: u32 = 2_000;
pub const MEDIA_CAP_WAS_MS: u32 = 500;
pub const TTS_CAP_MS: u32 = 500;
/// Steady cushion. CarUI in the same log held ~300ms and did not underrun.
pub const MEDIA_PREBUFFER_MS: u32 = 300;
/// Primed queue below this is about to feed silence into RTP.
pub const MEDIA_CRITICAL_MS: u32 = 40;
/// IDR-sized writes. P-frames stay on the existing immediate path.
pub const LARGE_VIDEO_WIRE: usize = 64 * 1024;
/// 352/48000 seconds, in microseconds. One AirPlay PCM packet.
pub const AUDIO_PACKET_US: u64 = 352 * 1_000_000 / RATE;
/// DSCP EF (46) << 2. WMM maps this to AC_VO, ahead of best-effort video TCP.
pub const AUDIO_IPTOS_EF: i32 = 0xB8;
/// Linux pfifo_fast band 0. No effect on a noqueue driver; DSCP still applies.
pub const AUDIO_SO_PRIORITY: i32 = 6;

pub fn samples_for_ms(ms: u32) -> usize {
    (RATE * 2 * u64::from(ms) / 1_000) as usize
}

pub fn queue_ms(interleaved_samples: usize) -> u32 {
    (interleaved_samples as u64 * 1_000 / RATE / 2) as u32
}

/// Drop oldest interleaved samples. Returns how many samples were removed.
pub fn trim_oldest(mix: &mut VecDeque<i16>, cap_samples: usize) -> usize {
    if mix.len() <= cap_samples {
        return 0;
    }
    let dropped = mix.len() - cap_samples;
    mix.drain(..dropped);
    dropped
}

/// Real media leaves the queue only after the prebuffer fills. `primed` stays
/// latched across a short underrun so the next samples are not delayed by
/// another full prebuffer of silence. `AudioStop` clears the latch.
pub fn media_drain_real(primed: bool, queued_samples: usize) -> bool {
    primed || queued_samples >= samples_for_ms(MEDIA_PREBUFFER_MS)
}

pub fn prebuffer_label(primed: bool, queued_samples: usize) -> &'static str {
    if primed {
        "ready"
    } else if queued_samples == 0 {
        "idle"
    } else {
        "fill"
    }
}

pub fn media_buffer_banner() -> String {
    format!(
        "AUDIO: 媒体混音上限 {MEDIA_CAP_MS}ms（原先 {MEDIA_CAP_WAS_MS}ms） 预缓冲 {MEDIA_PREBUFFER_MS}ms 提示音上限 {TTS_CAP_MS}ms；PCM48k 立体声 SPF352 不变"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YieldReason {
    pub media_ms: u32,
    pub lag_us: u32,
    pub low_queue: bool,
    pub send_behind: bool,
}

/// Shared hint from the audio pump to the video pump. Hints only; a stale read
/// delays one large frame, it does not change the bitstream.
pub struct AudioLink {
    playing: AtomicBool,
    primed: AtomicBool,
    media_samples: AtomicU32,
    lag_us: AtomicU32,
}

impl AudioLink {
    pub fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            primed: AtomicBool::new(false),
            media_samples: AtomicU32::new(0),
            lag_us: AtomicU32::new(0),
        }
    }

    pub fn publish(&self, playing: bool, primed: bool, media_samples: usize, lag: Duration) {
        self.playing.store(playing, Ordering::Relaxed);
        self.primed.store(primed, Ordering::Relaxed);
        self.media_samples.store(media_samples.min(u32::MAX as usize) as u32, Ordering::Relaxed);
        self.lag_us.store(lag.as_micros().min(u128::from(u32::MAX)) as u32, Ordering::Relaxed);
    }

    /// Pace only large video writes, and only while media is actually playing.
    /// Filling the prebuffer is not a low-queue event.
    pub fn video_pause(&self, wire_len: usize) -> Option<YieldReason> {
        if wire_len < LARGE_VIDEO_WIRE || !self.playing.load(Ordering::Relaxed) {
            return None;
        }
        let samples = self.media_samples.load(Ordering::Relaxed) as usize;
        let primed = self.primed.load(Ordering::Relaxed);
        let lag_us = self.lag_us.load(Ordering::Relaxed);
        let low_queue = primed && queue_ms(samples) < MEDIA_CRITICAL_MS;
        let send_behind = u64::from(lag_us) >= AUDIO_PACKET_US * 2;
        if !low_queue && !send_behind {
            return None;
        }
        Some(YieldReason {
            media_ms: queue_ms(samples),
            lag_us,
            low_queue,
            send_behind,
        })
    }
}

impl Default for AudioLink {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpPriority {
    pub ipv4_tos: Result<(), i32>,
    pub ipv6_tclass: Result<(), i32>,
    pub so_priority: Result<(), i32>,
}

fn set_int(fd: RawFd, level: i32, opt: i32, value: i32) -> Result<(), i32> {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            opt,
            &value as *const i32 as *const libc::c_void,
            std::mem::size_of::<i32>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(-1))
    }
}

/// Mark one audio UDP socket ahead of best-effort video. Failures are returned
/// so the caller can log them; audio still sends if the option is rejected.
pub fn apply_udp_priority(fd: RawFd) -> UdpPriority {
    UdpPriority {
        ipv4_tos: set_int(fd, libc::IPPROTO_IP, libc::IP_TOS, AUDIO_IPTOS_EF),
        ipv6_tclass: set_int(fd, libc::IPPROTO_IPV6, libc::IPV6_TCLASS, AUDIO_IPTOS_EF),
        so_priority: set_int(fd, libc::SOL_SOCKET, libc::SO_PRIORITY, AUDIO_SO_PRIORITY),
    }
}

fn sock_result(r: Result<(), i32>) -> String {
    match r {
        Ok(()) => "ok".into(),
        Err(err) => format!("err/{err}"),
    }
}

pub fn format_udp_priority(data: &UdpPriority, sync: Option<&UdpPriority>) -> String {
    let sync_text = match sync {
        Some(sync) => format!(
            "sync[ipv4={} ipv6={} prio={}]",
            sock_result(sync.ipv4_tos),
            sock_result(sync.ipv6_tclass),
            sock_result(sync.so_priority)
        ),
        None => "sync[none]".into(),
    };
    format!(
        "AUDIO: UDP 优先于视频 tos=0xb8 so_priority=6 data[ipv4={} ipv6={} prio={}] {sync_text}",
        sock_result(data.ipv4_tos),
        sock_result(data.ipv6_tclass),
        sock_result(data.so_priority)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn media_cap_is_two_seconds_and_tts_stays_half_a_second() {
        assert!(MEDIA_CAP_MS > MEDIA_CAP_WAS_MS);
        assert_eq!(samples_for_ms(MEDIA_CAP_WAS_MS), 48_000);
        assert_eq!(samples_for_ms(MEDIA_CAP_MS), 48_000 * 4);
        assert_eq!(samples_for_ms(TTS_CAP_MS), 48_000);
        assert_eq!(queue_ms(samples_for_ms(MEDIA_CAP_MS)), MEDIA_CAP_MS);
        assert_eq!(queue_ms(samples_for_ms(MEDIA_PREBUFFER_MS)), MEDIA_PREBUFFER_MS);
        assert_eq!(AUDIO_PACKET_US, 7_333);
        let banner = media_buffer_banner();
        assert!(banner.contains("2000ms"));
        assert!(banner.contains("原先 500ms"));
        assert!(banner.contains("预缓冲 300ms"));
        assert!(banner.contains("SPF352"));
    }

    #[test]
    fn trim_drops_oldest_samples_and_stops_at_the_cap() {
        let mut mix: VecDeque<i16> = (0..10).collect();
        assert_eq!(trim_oldest(&mut mix, 10), 0);
        assert_eq!(trim_oldest(&mut mix, 4), 6);
        assert_eq!(mix.into_iter().collect::<Vec<_>>(), vec![6, 7, 8, 9]);
        let mut bounded = VecDeque::from(vec![1, 2, 3, 4]);
        assert_eq!(trim_oldest(&mut bounded, samples_for_ms(MEDIA_CAP_MS)), 0);
    }

    #[test]
    fn prebuffer_latches_and_does_not_refill_after_a_blip() {
        let target = samples_for_ms(MEDIA_PREBUFFER_MS);
        assert!(!media_drain_real(false, target - 1));
        assert!(media_drain_real(false, target));
        assert!(media_drain_real(true, 0));
        assert_eq!(prebuffer_label(false, 0), "idle");
        assert_eq!(prebuffer_label(false, 8), "fill");
        assert_eq!(prebuffer_label(true, 0), "ready");
    }

    #[test]
    fn video_yields_only_for_a_large_frame_when_audio_is_behind_or_critical() {
        let link = AudioLink::new();
        let big = LARGE_VIDEO_WIRE;
        assert!(link.video_pause(big).is_none());

        link.publish(true, false, samples_for_ms(10), Duration::ZERO);
        assert!(link.video_pause(big).is_none(), "filling the prebuffer is not a stall");
        assert!(link.video_pause(1024).is_none(), "small frames are not paced");

        link.publish(true, false, samples_for_ms(10), Duration::from_micros(20_000));
        let behind = link.video_pause(big).unwrap();
        assert!(behind.send_behind);
        assert!(!behind.low_queue);

        link.publish(true, true, samples_for_ms(MEDIA_PREBUFFER_MS), Duration::ZERO);
        assert!(link.video_pause(big).is_none());

        link.publish(true, true, samples_for_ms(10), Duration::ZERO);
        let low = link.video_pause(big).unwrap();
        assert!(low.low_queue);
        assert!(!low.send_behind);
        assert_eq!(low.media_ms, 10);

        link.publish(false, true, samples_for_ms(10), Duration::from_micros(20_000));
        assert!(link.video_pause(big).is_none(), "idle audio does not delay video");
    }

    #[test]
    fn audio_udp_socket_is_marked_ahead_of_best_effort() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let fd = socket.as_raw_fd();
        let marked = apply_udp_priority(fd);
        assert_eq!(marked.ipv4_tos, Ok(()));
        assert_eq!(marked.so_priority, Ok(()));
        assert_eq!(get_int(fd, libc::IPPROTO_IP, libc::IP_TOS), AUDIO_IPTOS_EF);
        assert_eq!(get_int(fd, libc::SOL_SOCKET, libc::SO_PRIORITY), AUDIO_SO_PRIORITY);
        let line = format_udp_priority(&marked, None);
        assert!(line.contains("tos=0xb8"));
        assert!(line.contains("so_priority=6"));
        assert!(line.contains("sync[none]"));
        assert!(line.contains("ipv4=ok"));
    }

    fn get_int(fd: RawFd, level: i32, opt: i32) -> i32 {
        let mut value = 0i32;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                level,
                opt,
                &mut value as *mut i32 as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        value
    }
}
