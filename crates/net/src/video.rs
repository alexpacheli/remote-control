//! Video over QUIC datagrams, with forward error correction.
//!
//! [`Packetizer`] cuts a serialized [`VideoFrame`] into datagram-sized
//! shards and adds Reed-Solomon parity per block, so a few lost packets are
//! rebuilt on arrival instead of retransmitted (which would hold back every
//! later frame, as on a stream). Packets are interleaved across blocks so a
//! burst of losses spreads over several of them. Keyframes, which are
//! large and expensive to lose (the viewer stops until the next one), carry
//! more parity.
//!
//! [`VideoSender`] paces the packets ([`Pacer`]): a large frame sent at
//! once arrives as a burst at the slowest link on the path, which overflows
//! its buffer and loses more packets than FEC can rebuild.
//!
//! [`Reassembler`] rebuilds frames and hands them out in order. A frame
//! that cannot be rebuilt breaks the chain of predicted frames after it, so
//! it is reported as [`Event::Lost`] (the viewer then asks for a keyframe)
//! and later frames are dropped until a keyframe arrives. While the screen
//! is still, the host repeats the number of its last frame
//! ([`VideoSender::send_idle`]), so a frame that lost every packet is
//! noticed even with nothing after it.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use hext_proto::{Datagram, MAX_VIDEO_FRAME_LEN, VideoFrame, VideoPacket};
use iroh::endpoint::{Connection, SendDatagramError};
use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::{NetError, Result};

/// Data shards per FEC block. Reed-Solomon over GF(256) allows 256 shards
/// in all; smaller blocks keep each loss local and the coding cheap.
pub const BLOCK_SIZE: usize = 64;

/// Parity shards per data shard (at least one per block).
pub const PARITY_RATIO: f32 = 0.2;

/// Parity shards per data shard in a keyframe.
pub const KEYFRAME_PARITY_RATIO: f32 = 0.4;

/// Packets go out at up to this many times the video bitrate: a typical
/// frame leaves within a few milliseconds, a keyframe (often 10× larger)
/// is spread over a few frame intervals instead of leaving in one burst.
pub const PACING_HEADROOM: u32 = 4;

/// A frame's first this long of sending goes out at once after a pause (a
/// typical frame, unpaced); longer frames are paced.
pub const PACING_BURST: Duration = Duration::from_millis(4);

/// Sending a wake-up this late may make up for at once (a timer tick on
/// Windows by default); beyond that the frame is sent slower.
const PACING_CATCH_UP: Duration = Duration::from_millis(16);

/// A send that waited this long was held back by congestion control.
const SEND_STALL: Duration = Duration::from_millis(1);

/// How long a missing frame may go without receiving a packet, while newer
/// frames exist (packets can arrive out of order), before it counts as lost.
pub const LOSS_WAIT: Duration = Duration::from_millis(40);

/// Complete frames that may wait behind a missing one before it counts as
/// lost, however recent.
const MAX_WAITING: usize = 3;

/// While waiting for a keyframe (the first one, or one asked for, was lost
/// too), the request is repeated this often.
pub const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

/// Complete frames kept while waiting for a keyframe (packets may arrive
/// out of order, so the keyframe they follow can still come).
const MAX_KEPT: usize = 16;

/// Frames being rebuilt at once; older ones are given up first.
const MAX_PARTIAL: usize = 32;

/// Smallest shard accepted: no real path has a smaller datagram.
const MIN_SHARD: usize = 64;

/// Bytes a [`VideoPacket`] adds to its shard once serialized, at most.
const PACKET_OVERHEAD: usize = 32;

/// Largest video datagram sent. A connection can have several paths (relay
/// and direct, Tailscale's MTU of 1280…) and the reported maximum may be a
/// larger path's: datagrams too big for the path in use are dropped
/// silently (seen in the `iroh` spike). Every QUIC path carries 1200-byte
/// packets, so this always fits.
pub const MAX_DATAGRAM: usize = 1100;

/// Parity shards for a block of `k` data shards.
fn parity_for(k: usize, keyframe: bool) -> usize {
    let ratio = if keyframe {
        KEYFRAME_PARITY_RATIO
    } else {
        PARITY_RATIO
    };
    ((k as f32 * ratio).ceil() as usize).max(1)
}

/// Cuts frames into packets, reusing the Reed-Solomon codecs.
#[derive(Default)]
pub struct Packetizer {
    codecs: HashMap<(usize, usize), ReedSolomon>,
}

impl Packetizer {
    /// Packets of one frame, ready to send (data and parity, interleaved).
    pub fn packetize(
        &mut self,
        frame: u64,
        bytes: &[u8],
        shard: usize,
        keyframe: bool,
    ) -> Vec<VideoPacket> {
        let shard = shard.clamp(MIN_SHARD, u16::MAX as usize);
        let data_shards = bytes.len().div_ceil(shard).max(1);
        let blocks = data_shards.div_ceil(BLOCK_SIZE);

        let mut per_block: Vec<Vec<VideoPacket>> = Vec::with_capacity(blocks);
        for block in 0..blocks {
            let first = block * BLOCK_SIZE;
            let k = (data_shards - first).min(BLOCK_SIZE);
            let parity = parity_for(k, keyframe);
            let mut shards: Vec<Vec<u8>> = (0..k + parity)
                .map(|i| {
                    let mut piece = vec![0; shard];
                    if i < k {
                        let start = (first + i) * shard;
                        let end = (start + shard).min(bytes.len());
                        if start < end {
                            piece[..end - start].copy_from_slice(&bytes[start..end]);
                        }
                    }
                    piece
                })
                .collect();
            self.codecs
                .entry((k, parity))
                .or_insert_with(|| ReedSolomon::new(k, parity).expect("valid shard counts"))
                .encode(&mut shards)
                .expect("shards of equal size");
            per_block.push(
                shards
                    .into_iter()
                    .enumerate()
                    .map(|(index, data)| VideoPacket {
                        frame,
                        len: bytes.len() as u32,
                        shard: shard as u16,
                        block_size: BLOCK_SIZE as u16,
                        block: block as u16,
                        index: index as u16,
                        parity: parity as u16,
                        data,
                    })
                    .collect(),
            );
        }

        // Interleave: packet 0 of every block, then packet 1, …
        let longest = per_block.iter().map(Vec::len).max().unwrap_or(0);
        let mut iters: Vec<_> = per_block.into_iter().map(Vec::into_iter).collect();
        let mut out = Vec::new();
        for _ in 0..longest {
            out.extend(iters.iter_mut().filter_map(Iterator::next));
        }
        out
    }
}

/// Spaces packets so they leave at a steady rate.
///
/// Each packet is given a send time: when the packets before it have had
/// time to leave at the pacing rate. A frame after a pause may send
/// [`PACING_BURST`] worth right away. A late wake-up (coarse timers) sends
/// what is due at once, so the average rate holds.
#[derive(Debug, Clone)]
pub struct Pacer {
    bytes_per_sec: f64,
    /// When the next packet may leave.
    next: Option<Instant>,
}

impl Pacer {
    pub fn new(bits_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: (bits_per_sec.max(1) as f64) / 8.0,
            next: None,
        }
    }

    /// Pacing for a video `bitrate_bps` ([`PACING_HEADROOM`] times it).
    pub fn for_bitrate(bitrate_bps: u32) -> Self {
        Self::new(u64::from(bitrate_bps) * u64::from(PACING_HEADROOM))
    }

    /// A new frame starts: time not used since the last one is not saved
    /// up, except for [`PACING_BURST`].
    pub fn begin_frame(&mut self, now: Instant) {
        let earliest = now.checked_sub(PACING_BURST).unwrap_or(now);
        self.next = Some(self.next.map_or(earliest, |next| next.max(earliest)));
    }

    /// When a packet of `len` bytes may leave (at or before `now` when it
    /// may leave right away); reserves its time.
    pub fn reserve(&mut self, len: usize, now: Instant) -> Instant {
        let earliest = now.checked_sub(PACING_CATCH_UP).unwrap_or(now);
        let at = self.next.unwrap_or(now).max(earliest);
        self.next = Some(at + Duration::from_secs_f64(len as f64 / self.bytes_per_sec));
        at
    }

    /// Sending was held back (congestion) until `now`: the time lost is
    /// not made up, which would be a burst into a congested path.
    pub fn stalled(&mut self, now: Instant) {
        self.next = Some(self.next.map_or(now, |next| next.max(now)));
    }
}

/// Sends frames as video datagrams (host side).
pub struct VideoSender {
    packetizer: Packetizer,
    pacer: Pacer,
    last_frame: Option<u64>,
    /// Held while frames flow, released while the screen is still.
    timers: Option<crate::timer::FineTimers>,
}

impl VideoSender {
    /// A sender for video of about `bitrate_bps`, which sets the pacing.
    pub fn new(bitrate_bps: u32) -> Self {
        Self {
            packetizer: Packetizer::default(),
            pacer: Pacer::for_bitrate(bitrate_bps),
            last_frame: None,
            timers: None,
        }
    }

    /// Sends one frame, paced. Waits when the connection is congested, so
    /// a slow network slows the host down instead of dropping packets here.
    pub async fn send(&mut self, conn: &Connection, frame: &VideoFrame) -> Result<()> {
        let bytes = hext_proto::encode(frame)?;
        let max = conn
            .max_datagram_size()
            .ok_or_else(|| NetError::Unexpected("o outro lado não aceita datagramas".into()))?;
        let shard = max.min(MAX_DATAGRAM).saturating_sub(PACKET_OVERHEAD);
        self.last_frame = Some(frame.seq);
        let packets = self
            .packetizer
            .packetize(frame.seq, &bytes, shard, frame.keyframe);
        self.timers
            .get_or_insert_with(crate::timer::FineTimers::new);
        self.pacer.begin_frame(Instant::now());
        for packet in packets {
            let datagram = hext_proto::encode(&Datagram::Video(packet))?;
            let now = Instant::now();
            let at = self.pacer.reserve(datagram.len(), now);
            if at > now {
                tokio::time::sleep_until(at.into()).await;
            }
            let before = Instant::now();
            let sent = conn.send_datagram_wait(datagram.into()).await;
            if before.elapsed() > SEND_STALL {
                self.pacer.stalled(Instant::now());
            }
            match sent {
                Ok(()) => {}
                // The path MTU shrank mid-frame: this frame is lost (the
                // viewer asks for a keyframe), the next one uses the new size.
                Err(SendDatagramError::TooLarge) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// While nothing changes on screen: tells the viewer which frame was the
    /// last, so it notices if that one never arrived. Call every few
    /// hundred milliseconds without frames.
    pub async fn send_idle(&mut self, conn: &Connection) -> Result<()> {
        self.timers = None;
        let Some(last_frame) = self.last_frame else {
            return Ok(());
        };
        let datagram = hext_proto::encode(&Datagram::VideoIdle { last_frame })?;
        conn.send_datagram_wait(datagram.into()).await?;
        Ok(())
    }
}

/// What [`Reassembler`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The next frame, in order.
    Frame(VideoFrame),
    /// A frame could not be rebuilt: ask the host for a keyframe. Frames
    /// are dropped until one arrives.
    Lost,
}

/// Totals since the session started, for the stats line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// FEC blocks rebuilt from parity (each hid one or more lost packets).
    pub recovered: u64,
    /// Frames lost despite FEC (or dropped behind a slow decoder).
    pub lost: u64,
}

/// Rebuilds frames from packets and hands them out in order.
pub struct Reassembler {
    /// Next frame to hand out; `None` until the first keyframe.
    next: Option<u64>,
    /// After a loss: drop frames until a keyframe.
    awaiting_keyframe: bool,
    partial: BTreeMap<u64, Partial>,
    complete: BTreeMap<u64, VideoFrame>,
    /// Newest frame number seen (in a packet or an idle notice).
    newest: Option<u64>,
    /// Since when the next frame is known to be missing.
    blocked_since: Option<Instant>,
    /// When a keyframe was last asked for (or the wait for one began).
    keyframe_asked: Option<Instant>,
    codecs: HashMap<(usize, usize), ReedSolomon>,
    pub counters: Counters,
}

struct Partial {
    len: usize,
    shard: usize,
    blocks: Vec<Block>,
    /// When the last packet of this frame arrived.
    last_packet: Instant,
}

struct Block {
    k: usize,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    done: bool,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            next: None,
            awaiting_keyframe: true,
            partial: BTreeMap::new(),
            complete: BTreeMap::new(),
            newest: None,
            blocked_since: None,
            keyframe_asked: None,
            codecs: HashMap::new(),
            counters: Counters::default(),
        }
    }

    /// Adds a packet; returns the frames (and losses) it made available.
    pub fn push(&mut self, packet: VideoPacket, now: Instant) -> Vec<Event> {
        if packet.frame == u64::MAX
            || self.next.is_some_and(|next| packet.frame < next)
            || self.complete.contains_key(&packet.frame)
        {
            return Vec::new(); // already handed out, or parity of a done frame
        }
        let frame = packet.frame;
        if let Some(complete) = self.add(packet, now) {
            self.complete.insert(complete.seq, complete);
        }
        self.saw(frame);
        self.drain(now)
    }

    /// The host's last frame, sent while its screen is still.
    pub fn idle(&mut self, last_frame: u64, now: Instant) -> Vec<Event> {
        if last_frame != u64::MAX {
            self.saw(last_frame);
        }
        self.drain(now)
    }

    /// Call periodically (e.g. every 10 ms): losses are noticed on time
    /// even when no packet arrives.
    pub fn tick(&mut self, now: Instant) -> Vec<Event> {
        self.drain(now)
    }

    /// Gives up on the frames in hand and waits for a keyframe (the caller
    /// could not keep up, e.g. a slow decoder); ask the host for one.
    pub fn resync(&mut self, now: Instant) {
        self.counters.lost += 1;
        self.awaiting_keyframe = true;
        self.keyframe_asked = Some(now);
        self.blocked_since = None;
        self.complete.clear();
        self.partial.clear();
    }

    fn saw(&mut self, frame: u64) {
        self.newest = Some(self.newest.map_or(frame, |n| n.max(frame)));
    }

    /// Stores the packet; returns the frame if it is now complete.
    fn add(&mut self, packet: VideoPacket, now: Instant) -> Option<VideoFrame> {
        let (len, shard) = (packet.len as usize, packet.shard as usize);
        let block_size = packet.block_size as usize;
        let parity = packet.parity as usize;
        // Check the packet against its own layout before allocating anything.
        if len > MAX_VIDEO_FRAME_LEN
            || shard < MIN_SHARD
            || !(16..=256).contains(&block_size)
            || packet.data.len() != shard
        {
            return None;
        }
        let data_shards = len.div_ceil(shard).max(1);
        let block_count = data_shards.div_ceil(block_size);
        let block = packet.block as usize;
        if block >= block_count {
            return None;
        }
        let k = (data_shards - block * block_size).min(block_size);
        let total = k + parity;
        if parity == 0 || total > 256 || packet.index as usize >= total {
            return None;
        }

        if !self.partial.contains_key(&packet.frame) && self.partial.len() >= MAX_PARTIAL {
            self.partial.pop_first();
        }
        let partial = self.partial.entry(packet.frame).or_insert_with(|| Partial {
            len,
            shard,
            blocks: (0..block_count)
                .map(|b| Block {
                    k: (data_shards - b * block_size).min(block_size),
                    shards: Vec::new(),
                    received: 0,
                    done: false,
                })
                .collect(),
            last_packet: now,
        });
        // Packets of one frame must agree on its layout.
        if (partial.len, partial.shard) != (len, shard) || partial.blocks.len() != block_count {
            return None;
        }
        partial.last_packet = now;
        let block = &mut partial.blocks[block];
        if block.done {
            return None;
        }
        if block.shards.is_empty() {
            block.shards = vec![None; total];
        } else if block.shards.len() != total {
            return None;
        }
        let slot = &mut block.shards[packet.index as usize];
        if slot.is_some() {
            return None; // duplicate
        }
        *slot = Some(packet.data);
        block.received += 1;
        if block.received < block.k {
            return None;
        }

        // Enough shards: rebuild missing data shards from parity.
        if block.shards[..block.k].iter().any(Option::is_none) {
            let codec = self
                .codecs
                .entry((block.k, parity))
                .or_insert_with(|| ReedSolomon::new(block.k, parity).expect("valid shard counts"));
            if codec.reconstruct_data(&mut block.shards).is_err() {
                return None;
            }
            self.counters.recovered += 1;
        }
        block.done = true;
        if !partial.blocks.iter().all(|b| b.done) {
            return None;
        }

        let partial = self.partial.remove(&packet.frame)?;
        let mut bytes = Vec::with_capacity(data_shards * partial.shard);
        for block in partial.blocks {
            for shard in block.shards.into_iter().take(block.k) {
                bytes.extend_from_slice(&shard?);
            }
        }
        bytes.truncate(partial.len);
        // A frame whose contents don't decode is as good as lost.
        let frame: VideoFrame = hext_proto::decode(&bytes).ok()?;
        (frame.seq == packet.frame).then_some(frame)
    }

    /// Hands out complete frames in order, and detects losses.
    fn drain(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            if self.awaiting_keyframe {
                // Resume at the newest complete keyframe; older frames are useless.
                let Some(&key) = self
                    .complete
                    .iter()
                    .rev()
                    .find_map(|(seq, frame)| frame.keyframe.then_some(seq))
                else {
                    // Their keyframe may still be on its way; keep the newest.
                    while self.complete.len() > MAX_KEPT {
                        self.complete.pop_first();
                    }
                    // Frames exist but no keyframe is arriving: ask (again).
                    if self.newest.is_some() && !self.progressing(now) {
                        let asked = *self.keyframe_asked.get_or_insert(now);
                        if now.duration_since(asked) > KEYFRAME_RETRY {
                            events.push(Event::Lost);
                            self.keyframe_asked = Some(now);
                        }
                    }
                    break;
                };
                self.complete = self.complete.split_off(&key);
                self.partial = self.partial.split_off(&key);
                self.next = Some(key);
                self.awaiting_keyframe = false;
                self.keyframe_asked = None;
                self.blocked_since = None;
            }
            let next = self.next.expect("set once a keyframe arrived");
            if let Some(frame) = self.complete.remove(&next) {
                events.push(Event::Frame(frame));
                self.next = Some(next + 1);
                self.blocked_since = None;
                continue;
            }
            if self.is_lost(next, now) {
                events.push(Event::Lost);
                self.counters.lost += 1;
                self.awaiting_keyframe = true;
                self.keyframe_asked = Some(now);
                self.blocked_since = None;
                self.partial = self.partial.split_off(&(next + 1));
                continue; // a keyframe may already be waiting
            }
            break;
        }
        // Drop partial frames nothing has added to for long.
        self.partial
            .retain(|_, p| now.duration_since(p.last_packet) < Duration::from_secs(2));
        events
    }

    /// Whether some frame is still receiving packets.
    fn progressing(&self, now: Instant) -> bool {
        self.partial
            .values()
            .any(|p| now.duration_since(p.last_packet) <= LOSS_WAIT)
    }

    /// Whether the missing frame `next` should be given up on.
    fn is_lost(&mut self, next: u64, now: Instant) -> bool {
        // Only frames known to exist can be missing.
        if !self.newest.is_some_and(|newest| newest >= next) {
            return false;
        }
        if self.complete.len() >= MAX_WAITING {
            return true;
        }
        // Lost once it has gone LOSS_WAIT without a packet: a large frame
        // still arriving on a slow link keeps receiving them.
        let since = *self.blocked_since.get_or_insert(now);
        let last_packet = self.partial.get(&next).map(|p| p.last_packet);
        let quiet_since = last_packet.map_or(since, |t| t.max(since));
        now.duration_since(quiet_since) > LOSS_WAIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64, keyframe: bool, size: usize) -> VideoFrame {
        VideoFrame {
            seq,
            width: 1920,
            height: 1080,
            keyframe,
            capture_micros: 1,
            encode_micros: 2,
            hardware: true,
            data: (0..size).map(|i| (i * 7 + seq as usize) as u8).collect(),
        }
    }

    fn packets(f: &VideoFrame) -> Vec<VideoPacket> {
        Packetizer::default().packetize(f.seq, &hext_proto::encode(f).unwrap(), 1100, f.keyframe)
    }

    fn frames(events: &[Event]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f.seq),
                Event::Lost => None,
            })
            .collect()
    }

    #[test]
    fn layout_and_interleaving() {
        let f = frame(0, true, 200_000);
        let bytes = hext_proto::encode(&f).unwrap().len();
        let pkts = packets(&f);
        let data_shards = bytes.div_ceil(1100);
        let blocks = data_shards.div_ceil(BLOCK_SIZE);
        assert_eq!(blocks, 3);
        let expected: usize = (0..blocks)
            .map(|b| {
                let k = (data_shards - b * BLOCK_SIZE).min(BLOCK_SIZE);
                k + parity_for(k, true)
            })
            .sum();
        assert_eq!(pkts.len(), expected);
        // Packet 0 of every block first: a burst hits different blocks.
        for (i, p) in pkts.iter().take(blocks).enumerate() {
            assert_eq!((p.block as usize, p.index), (i, 0));
        }
        assert!(
            pkts.iter()
                .all(|p| p.data.len() == 1100 && p.len as usize == bytes)
        );
        // 20% parity (40% in keyframes), at least one per block.
        assert_eq!(parity_for(64, false), 13);
        assert_eq!(parity_for(64, true), 26);
        assert_eq!(parity_for(1, false), 1);
    }

    #[test]
    fn delivers_in_order_without_loss() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        for seq in 0..5 {
            for p in packets(&frame(seq, seq == 0, 5_000)) {
                out.extend(r.push(p, now));
            }
        }
        assert_eq!(frames(&out), vec![0, 1, 2, 3, 4]);
        assert_eq!(r.counters, Counters::default());
    }

    /// Feeds `f`'s packets except those `lose` picks; returns the events.
    /// Delivers `f` (after the keyframe it follows, if it is not one)
    /// without the packets `lose` picks.
    fn deliver_with_loss(f: &VideoFrame, lose: impl Fn(usize) -> bool) -> (Vec<Event>, Counters) {
        let mut r = Reassembler::new();
        let now = Instant::now();
        if !f.keyframe {
            for p in packets(&frame(f.seq - 1, true, 1_000)) {
                r.push(p, now);
            }
        }
        let events = packets(f)
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !lose(*i))
            .flat_map(|(_, p)| r.push(p, now))
            .collect();
        (events, r.counters)
    }

    #[test]
    fn fec_rebuilds_random_loss() {
        let f = frame(1, false, 300_000);
        // ~10% loss, deterministic (a simple LCG).
        let lose = |i: usize| (i.wrapping_mul(2_654_435_761) >> 7).is_multiple_of(10);
        let (events, counters) = deliver_with_loss(&f, lose);
        assert_eq!(events, vec![Event::Frame(f)]);
        assert!(counters.recovered > 0);
        assert_eq!(counters.lost, 0);
    }

    #[test]
    fn interleaving_spreads_a_burst() {
        // 20 packets in a row: one block alone could not rebuild that many
        // (13 parity), but interleaved they hit every block, 4-5 each.
        let f = frame(1, false, 300_000);
        let (events, counters) = deliver_with_loss(&f, |i| (100..120).contains(&i));
        assert_eq!(events, vec![Event::Frame(f)]);
        // Blocks that only lost parity need no rebuilding.
        assert!(counters.recovered >= 4, "{counters:?}");
    }

    #[test]
    fn too_much_loss_is_not_a_frame() {
        for keyframe in [false, true] {
            let f = frame(1, keyframe, 300_000);
            let (events, _) = deliver_with_loss(&f, |i| i.is_multiple_of(2));
            assert!(events.is_empty(), "keyframe: {keyframe}");
        }
    }

    #[test]
    fn reordered_packets_and_frames() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut all: Vec<VideoPacket> = (0..4)
            .flat_map(|seq| packets(&frame(seq, seq == 0, 3_000)))
            .collect();
        all.reverse();
        let out: Vec<Event> = all.into_iter().flat_map(|p| r.push(p, now)).collect();
        assert_eq!(frames(&out), vec![0, 1, 2, 3]);
    }

    #[test]
    fn duplicates_are_ignored() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let pkts = packets(&frame(0, true, 3_000));
        let mut out = Vec::new();
        for p in pkts.iter().chain(&pkts) {
            out.extend(r.push(p.clone(), now));
        }
        assert_eq!(frames(&out), vec![0]);
    }

    #[test]
    fn unrecoverable_loss_waits_for_a_keyframe() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        out.extend(
            packets(&frame(0, true, 3_000))
                .into_iter()
                .flat_map(|p| r.push(p, t0)),
        );
        // Frame 1 loses all its packets; 2 and 3 arrive.
        for seq in 2..4 {
            out.extend(
                packets(&frame(seq, false, 3_000))
                    .into_iter()
                    .flat_map(|p| r.push(p, t0)),
            );
        }
        assert_eq!(frames(&out), vec![0], "2 and 3 wait for 1");
        // After the wait, 1 is lost: 2 and 3 are dropped.
        let later = t0 + LOSS_WAIT * 2;
        let events = r.tick(later);
        assert_eq!(events, vec![Event::Lost]);
        assert_eq!(r.counters.lost, 1);
        // P-frames are dropped until the keyframe (5) arrives.
        let p4: Vec<Event> = packets(&frame(4, false, 3_000))
            .into_iter()
            .flat_map(|p| r.push(p, later))
            .collect();
        assert!(p4.is_empty());
        let k5: Vec<Event> = packets(&frame(5, true, 3_000))
            .into_iter()
            .flat_map(|p| r.push(p, later))
            .collect();
        assert_eq!(frames(&k5), vec![5]);
        let p6: Vec<Event> = packets(&frame(6, false, 3_000))
            .into_iter()
            .flat_map(|p| r.push(p, later))
            .collect();
        assert_eq!(frames(&p6), vec![6]);
    }

    #[test]
    fn many_waiting_frames_declare_the_loss_at_once() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out: Vec<Event> = packets(&frame(0, true, 2_000))
            .into_iter()
            .flat_map(|p| r.push(p, now))
            .collect();
        for seq in 2..2 + MAX_WAITING as u64 {
            out.extend(
                packets(&frame(seq, false, 2_000))
                    .into_iter()
                    .flat_map(|p| r.push(p, now)),
            );
        }
        assert!(out.contains(&Event::Lost));
    }

    #[test]
    fn a_frame_stuck_alone_is_lost_after_a_while() {
        // The last update before a still screen loses too many packets.
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let _ = packets(&frame(0, true, 2_000))
            .into_iter()
            .flat_map(|p| r.push(p, t0))
            .count();
        let pkts = packets(&frame(1, false, 50_000));
        for p in pkts.into_iter().take(10) {
            assert!(r.push(p, t0).is_empty());
        }
        assert!(r.tick(t0 + LOSS_WAIT).is_empty(), "still within the wait");
        assert_eq!(r.tick(t0 + LOSS_WAIT * 4), vec![Event::Lost]);
    }

    #[test]
    fn a_lost_first_keyframe_is_asked_for_again() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        // Keyframe 0 never arrives; P-frames do.
        let push = |r: &mut Reassembler, seq, at| -> Vec<Event> {
            packets(&frame(seq, false, 2_000))
                .into_iter()
                .flat_map(|p| r.push(p, at))
                .collect()
        };
        assert!(push(&mut r, 1, t0).is_empty());
        assert!(push(&mut r, 2, t0 + KEYFRAME_RETRY / 2).is_empty());
        assert_eq!(push(&mut r, 3, t0 + KEYFRAME_RETRY * 2), vec![Event::Lost]);
        // Not again right away.
        assert!(push(&mut r, 4, t0 + KEYFRAME_RETRY * 2).is_empty());
    }

    #[test]
    fn a_large_frame_arriving_slowly_is_not_lost() {
        // A 300 KB keyframe over ~400 ms (a slow link): packets keep coming.
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let f = frame(0, true, 300_000);
        let pkts = packets(&f);
        let step = Duration::from_millis(400) / pkts.len() as u32;
        let mut out = Vec::new();
        for (i, p) in pkts.into_iter().enumerate() {
            let at = t0 + step * i as u32;
            out.extend(r.tick(at));
            out.extend(r.push(p, at));
        }
        assert_eq!(out, vec![Event::Frame(f)]);
    }

    #[test]
    fn idle_notice_reveals_a_frame_lost_entirely() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let out: Vec<Event> = packets(&frame(0, true, 2_000))
            .into_iter()
            .flat_map(|p| r.push(p, t0))
            .collect();
        assert_eq!(frames(&out), vec![0]);
        // Frame 1 loses every packet, then the screen stays still.
        assert!(
            r.tick(t0 + LOSS_WAIT * 10).is_empty(),
            "nothing hints at 1 yet"
        );
        let t1 = t0 + LOSS_WAIT * 10;
        assert!(r.idle(1, t1).is_empty());
        assert_eq!(r.tick(t1 + LOSS_WAIT * 2), vec![Event::Lost]);
    }

    #[test]
    fn a_lost_keyframe_on_a_still_screen_is_asked_for_again() {
        // The very first keyframe loses every packet; only idle notices follow.
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        assert!(r.idle(0, t0).is_empty());
        assert!(r.tick(t0 + KEYFRAME_RETRY / 2).is_empty());
        assert_eq!(r.tick(t0 + KEYFRAME_RETRY * 2), vec![Event::Lost]);
    }

    #[test]
    fn resync_drops_frames_until_a_keyframe() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let feed = |r: &mut Reassembler, seq, key| -> Vec<u64> {
            let events: Vec<Event> = packets(&frame(seq, key, 2_000))
                .into_iter()
                .flat_map(|p| r.push(p, now))
                .collect();
            frames(&events)
        };
        assert_eq!(feed(&mut r, 0, true), vec![0]);
        r.resync(now);
        assert!(feed(&mut r, 1, false).is_empty());
        assert_eq!(feed(&mut r, 2, true), vec![2]);
        assert_eq!(r.counters.lost, 1);
    }

    #[test]
    fn hostile_layouts_allocate_nothing() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut p = packets(&frame(0, true, 2_000)).remove(0);
        // 16 MiB in 1-shard blocks would mean a million blocks.
        p.len = MAX_VIDEO_FRAME_LEN as u32;
        p.block_size = 1;
        assert!(r.push(p.clone(), now).is_empty());
        assert!(r.partial.is_empty());
        // Many frame numbers: the partial frames kept stay bounded.
        p.block_size = BLOCK_SIZE as u16;
        for frame in 0..1000 {
            p.frame = frame;
            let _ = r.push(p.clone(), now);
        }
        assert!(r.partial.len() <= MAX_PARTIAL);
        p.frame = u64::MAX;
        assert!(r.push(p, now).is_empty());
    }

    #[test]
    fn nothing_before_the_first_keyframe() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let out: Vec<Event> = packets(&frame(3, false, 2_000))
            .into_iter()
            .flat_map(|p| r.push(p, now))
            .collect();
        assert!(out.is_empty());
        let out: Vec<Event> = packets(&frame(4, true, 2_000))
            .into_iter()
            .flat_map(|p| r.push(p, now))
            .collect();
        assert_eq!(frames(&out), vec![4]);
    }

    #[test]
    fn malformed_packets_are_ignored() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut p = packets(&frame(0, true, 2_000)).remove(0);
        p.data.truncate(10);
        assert!(r.push(p, now).is_empty());
        let mut p = packets(&frame(0, true, 2_000)).remove(0);
        p.index = 999;
        assert!(r.push(p, now).is_empty());
        let mut p = packets(&frame(0, true, 2_000)).remove(0);
        p.len = u32::MAX;
        assert!(r.push(p, now).is_empty());
    }

    #[test]
    fn keyframes_carry_more_parity() {
        let packets_of = |keyframe| packets(&frame(0, keyframe, 50_000)).len();
        let data = (hext_proto::encode(&frame(0, false, 50_000)).unwrap().len()).div_ceil(1100);
        assert_eq!(packets_of(false), data + parity_for(data, false));
        assert_eq!(packets_of(true), data + parity_for(data, true));

        // A quarter of the packets lost: a keyframe survives, a frame with
        // the usual parity does not.
        let lose = |i: usize| i.is_multiple_of(4);
        let (events, counters) = deliver_with_loss(&frame(1, true, 50_000), lose);
        assert_eq!(frames(&events), vec![1]);
        assert!(counters.recovered > 0);
        let (events, _) = deliver_with_loss(&frame(1, false, 50_000), lose);
        assert!(frames(&events).is_empty());
    }

    #[test]
    fn pacer_sends_a_small_frame_at_once() {
        let mut pacer = Pacer::new(32_000_000); // 4 MB/s: 4 ms is 16 kB
        let now = Instant::now() + Duration::from_secs(1);
        pacer.begin_frame(now);
        for _ in 0..14 {
            assert!(pacer.reserve(1100, now) <= now);
        }
    }

    #[test]
    fn pacer_spreads_a_large_frame() {
        let mut pacer = Pacer::new(32_000_000);
        let now = Instant::now() + Duration::from_secs(1);
        // 200 kB: 50 ms at 4 MB/s, the first 4 ms of it at once.
        pacer.begin_frame(now);
        let last = (0..200).map(|_| pacer.reserve(1000, now)).last().unwrap();
        let spread = last.duration_since(now);
        assert!(
            spread > Duration::from_millis(44) && spread < Duration::from_millis(47),
            "{spread:?}"
        );
    }

    #[test]
    fn pacer_catches_up_after_a_late_wakeup() {
        let mut pacer = Pacer::new(8_000_000); // 1 MB/s: 1 ms per kB
        let start = Instant::now() + Duration::from_secs(1);
        pacer.begin_frame(start);
        let first = (0..10).map(|_| pacer.reserve(1000, start)).last().unwrap();
        assert!(first > start);
        // Woken 10 ms late: what was due goes out now, the rest stays paced.
        let late = first + Duration::from_millis(10);
        let due = (0..30)
            .map(|_| pacer.reserve(1000, late))
            .filter(|&at| at <= late)
            .count();
        assert_eq!(due, 10);
        // Much later (the connection was congested): only some is made up.
        let stalled = late + Duration::from_secs(1);
        let due = (0..30)
            .map(|_| pacer.reserve(1000, stalled))
            .filter(|&at| at <= stalled)
            .count();
        assert_eq!(due, 17, "16 ms at 1 kB/ms, plus the packet due now");
    }

    #[test]
    fn pacer_does_not_burst_after_congestion() {
        let mut pacer = Pacer::new(8_000_000);
        let start = Instant::now() + Duration::from_secs(1);
        pacer.begin_frame(start);
        pacer.reserve(1000, start);
        // The connection held the next packet back for 30 ms.
        let unblocked = start + Duration::from_millis(30);
        pacer.stalled(unblocked);
        let due = (0..30)
            .map(|_| pacer.reserve(1000, unblocked))
            .filter(|&at| at <= unblocked)
            .count();
        assert_eq!(due, 1);
    }

    #[test]
    fn pacer_credit_does_not_build_up() {
        let mut pacer = Pacer::new(8_000_000);
        let start = Instant::now() + Duration::from_secs(1);
        pacer.begin_frame(start);
        pacer.reserve(1000, start);
        // A long pause gives only the burst credit, not the whole pause.
        let later = start + Duration::from_secs(10);
        pacer.begin_frame(later);
        let at_once = (0..100)
            .map(|_| pacer.reserve(1000, later))
            .filter(|&at| at <= later)
            .count();
        assert_eq!(
            at_once, 5,
            "4 ms credit at 1 kB/ms, plus the packet at `later`"
        );
    }
}
