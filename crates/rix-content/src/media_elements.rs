//! HTML5 `<video>` and `<audio>` playback controller (`MediaElementSystem`):
//! - Bridges `HTMLMediaElement` / `HTMLVideoElement` / `HTMLAudioElement` JS APIs to
//!   `MediaSourceSystem` (MSE), pure-Rust AV1/H.264/VP8/VP9 video decoders, pure-Rust
//!   Opus/AAC/Vorbis/MP3/FLAC/PCM audio decoders, `MediaClock`, `AudioSampleQueue`,
//!   `TripleBufferShmem` (`VideoFrameProducer`/`VideoFrameConsumer`), and `YuvVideoRenderer`.
//! - Presents video frames directly onto `DisplayItem::ExternalSurface` with **zero CPU YUV->RGBA
//!   conversion loops**, **zero DOM mutations**, and **zero `DisplayList` rebuilds**.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use rix_dom::NodeId;
use rix_ipc::SurfaceId;
use rix_js::promise::promise_resolve_value;
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_layout::surface_id_for_node;
use rix_media::{
    create_audio_decoder, create_video_decoder, create_video_frame_channel, demux_bytes,
    AudioDecoder, AudioSampleQueue, AvSyncDecision, MediaClock, PacketKind, ReadyState, TimeRanges,
    TrackKind, VideoDecoder, VideoFrame, VideoFrameConsumer, VideoFrameProducer,
};
use rix_render::{Compositor, SharedGpuDevice, VelloRenderer, YuvVideoRenderer};

use crate::dom_bridge::{wrap_node, SharedHost};
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{fire_object_event, SharedEvents};
use crate::media_source::{detach_media_source_from_element, SharedMediaSource};
use crate::web_apis::{
    arg, bind_ctor_proto, define_global, define_host_accessor, define_host_getter,
    define_host_method, SharedWebApis,
};

/// HTML5 `HTMLMediaElement.networkState` constants.
pub const NETWORK_EMPTY: u16 = 0;
pub const NETWORK_IDLE: u16 = 1;
pub const NETWORK_LOADING: u16 = 2;
pub const NETWORK_NO_SOURCE: u16 = 3;

/// HTML5 `HTMLMediaElement.readyState` constants.
pub const HAVE_NOTHING: u16 = 0;
pub const HAVE_METADATA: u16 = 1;
pub const HAVE_CURRENT_DATA: u16 = 2;
pub const HAVE_FUTURE_DATA: u16 = 3;
pub const HAVE_ENOUGH_DATA: u16 = 4;

/// HTML5 `MediaError` code constants.
pub const MEDIA_ERR_ABORTED: u16 = 1;
pub const MEDIA_ERR_NETWORK: u16 = 2;
pub const MEDIA_ERR_DECODE: u16 = 3;
pub const MEDIA_ERR_SRC_NOT_SUPPORTED: u16 = 4;

/// Live decoding & A/V presentation pipeline bound to a single `<video>` or `<audio>` DOM element.
pub struct MediaPlayerState {
    pub node_id: NodeId,
    pub surface_id: SurfaceId,
    pub clock: Arc<MediaClock>,
    pub audio_queue: Arc<AudioSampleQueue>,
    pub video_width: u32,
    pub video_height: u32,
    pub ready_state: u16,
    pub network_state: u16,
    pub error: Option<(u16, String)>,
    pub seeking: bool,
    pub seek_target_s: Option<f64>,
    pub ended: bool,
    pub loop_enabled: bool,
    pub autoplay: bool,
    pub preload: String,
    pub duration_s: f64,
    pub has_fired_loadedmetadata: bool,
    pub has_fired_loadeddata: bool,
    pub has_fired_canplay: bool,
    pub has_fired_canplaythrough: bool,
    pub has_fired_playing: bool,
    pub last_vsync_ms: Option<f64>,
    pub last_timeupdate_s: f64,
    /// Full-precision playback/seek time in seconds (avoids integer-microsecond rounding on sub-us durations).
    pub exact_time_s: Option<f64>,
    video_decoder: Option<Box<dyn VideoDecoder>>,
    audio_decoder: Option<Box<dyn AudioDecoder>>,
    decoded_video_queue: VecDeque<VideoFrame>,
    /// Tracks how many MSE coded frames from each `SourceBuffer` index have already been fed.
    mse_fed_counts: Vec<usize>,
    mse_video_gen: u64,
    shmem_producer: Option<VideoFrameProducer>,
    shmem_consumer: Option<VideoFrameConsumer>,
    shmem_capacity_dims: (u32, u32),
    dirty_video_frame: bool,
    frames_presented: u64,
    last_video_pts_us: Option<i64>,
    estimated_fps: f32,
}

impl MediaPlayerState {
    fn new(node_id: NodeId) -> Self {
        Self {
            node_id,
            surface_id: surface_id_for_node(node_id),
            clock: Arc::new(MediaClock::new(48_000)),
            audio_queue: Arc::new(AudioSampleQueue::new(48_000 * 4)),
            video_width: 0,
            video_height: 0,
            ready_state: HAVE_NOTHING,
            network_state: NETWORK_EMPTY,
            error: None,
            seeking: false,
            seek_target_s: None,
            ended: false,
            loop_enabled: false,
            autoplay: false,
            preload: "metadata".to_string(),
            duration_s: f64::NAN,
            has_fired_loadedmetadata: false,
            has_fired_loadeddata: false,
            has_fired_canplay: false,
            has_fired_canplaythrough: false,
            has_fired_playing: false,
            last_vsync_ms: None,
            last_timeupdate_s: -1.0,
            exact_time_s: None,
            video_decoder: None,
            audio_decoder: None,
            decoded_video_queue: VecDeque::new(),
            mse_fed_counts: Vec::new(),
            mse_video_gen: 0,
            shmem_producer: None,
            shmem_consumer: None,
            shmem_capacity_dims: (0, 0),
            dirty_video_frame: false,
            frames_presented: 0,
            last_video_pts_us: None,
            estimated_fps: 0.0,
        }
    }

    /// Resets playback, readyState, error, seeking, and buffered decoder queues when media source changes.
    pub fn reset_for_source_change(&mut self) {
        self.clock.pause();
        self.clock.seek_us(0);
        self.video_width = 0;
        self.video_height = 0;
        self.ready_state = HAVE_NOTHING;
        self.network_state = NETWORK_EMPTY;
        self.error = None;
        self.seeking = false;
        self.seek_target_s = None;
        self.ended = false;
        self.duration_s = f64::NAN;
        self.has_fired_loadedmetadata = false;
        self.has_fired_loadeddata = false;
        self.has_fired_canplay = false;
        self.has_fired_canplaythrough = false;
        self.has_fired_playing = false;
        self.last_vsync_ms = None;
        self.last_timeupdate_s = -1.0;
        self.exact_time_s = None;
        self.decoded_video_queue.clear();
        self.audio_queue.clear();
        self.mse_fed_counts.clear();
    }

    fn ensure_shmem_channel(&mut self, width: u32, height: u32) -> bool {
        let w = width.max(16);
        let h = height.max(16);
        if self.shmem_producer.is_some()
            && self.shmem_capacity_dims.0 >= w
            && self.shmem_capacity_dims.1 >= h
        {
            return true;
        }
        if let Ok((prod, cons)) = create_video_frame_channel(w, h) {
            self.shmem_producer = Some(prod);
            self.shmem_consumer = Some(cons);
            self.shmem_capacity_dims = (w, h);
            true
        } else {
            false
        }
    }

    /// Loads a direct container byte buffer (MP4, WebM, WAV, Ogg, MP3, FLAC) into this player.
    pub fn load_container_bytes(&mut self, bytes: &[u8]) -> bool {
        let Ok(demuxed) = demux_bytes(bytes, None) else {
            return false;
        };
        self.decoded_video_queue.clear();
        self.audio_queue.clear();
        self.ended = false;
        self.clock.seek_us(0);
        if let Some(d) = demuxed.duration_us {
            self.duration_s = d as f64 / 1_000_000.0;
        }

        let mut v_track_id = None;
        let mut a_track_id = None;
        for track in &demuxed.tracks {
            match &track.kind {
                TrackKind::Video(v) if v_track_id.is_none() => {
                    self.video_width = v.width;
                    self.video_height = v.height;
                    if let Ok(dec) = create_video_decoder(track) {
                        self.video_decoder = Some(dec);
                        v_track_id = Some(track.id);
                    }
                }
                TrackKind::Audio(_) if a_track_id.is_none() => {
                    if let Ok(dec) = create_audio_decoder(track) {
                        self.audio_decoder = Some(dec);
                        a_track_id = Some(track.id);
                    }
                }
                _ => {}
            }
        }

        for pkt in &demuxed.packets {
            if Some(pkt.track_id) == v_track_id {
                if let Some(dec) = self.video_decoder.as_mut() {
                    if let Ok(frames) = dec.decode(pkt) {
                        self.decoded_video_queue.extend(frames);
                    }
                }
            } else if Some(pkt.track_id) == a_track_id {
                if let Some(dec) = self.audio_decoder.as_mut() {
                    if let Ok(frames) = dec.decode(pkt) {
                        for af in frames {
                            self.audio_queue.push_interleaved(&af.samples);
                        }
                    }
                }
            }
        }
        if let Some(dec) = self.video_decoder.as_mut() {
            if let Ok(flushed) = dec.flush() {
                self.decoded_video_queue.extend(flushed);
            }
        }
        if let Some(first) = self.decoded_video_queue.front() {
            self.video_width = first.width;
            self.video_height = first.height;
            self.ready_state = 4; // HAVE_ENOUGH_DATA
        } else if !self.audio_queue.is_empty() {
            self.ready_state = 4;
        }
        true
    }

    /// Pulls newly appended MSE coded frames from `ms_entry`, decodes them through our pure-Rust
    /// video & audio decoders, and queues decoded planar YUV frames and PCM audio samples.
    fn pump_mse_packets(&mut self, ms_entry: &mut crate::media_source::MediaSourceEntry) {
        let sb_count = ms_entry.engine.source_buffers.len();
        if self.mse_fed_counts.len() != sb_count {
            self.mse_fed_counts.resize(sb_count, 0);
        }

        if ms_entry.engine.duration.is_finite() {
            self.duration_s = ms_entry.engine.duration;
        }
        let is_ended = ms_entry.engine.ready_state == rix_media::ReadyState::Ended;

        for (sb_idx, sb) in ms_entry.engine.source_buffers.iter().enumerate() {
            let already_fed = self.mse_fed_counts[sb_idx];
            if already_fed >= sb.frames.len() {
                continue;
            }

            // Initialize or re-initialize decoders when a track appears or `changeType` bumps generation.
            for track in &sb.tracks {
                match &track.kind {
                    TrackKind::Video(v) => {
                        if self.video_decoder.is_none() || self.mse_video_gen != sb.codec_generation
                        {
                            if let Ok(dec) = create_video_decoder(track) {
                                self.video_decoder = Some(dec);
                                self.mse_video_gen = sb.codec_generation;
                                self.video_width = v.width;
                                self.video_height = v.height;
                            }
                        }
                    }
                    TrackKind::Audio(_) => {
                        if self.audio_decoder.is_none() {
                            if let Ok(dec) = create_audio_decoder(track) {
                                self.audio_decoder = Some(dec);
                            }
                        }
                    }
                }
            }

            for frame in &sb.frames[already_fed..] {
                let pkt = frame.to_encoded_packet();
                match frame.kind {
                    PacketKind::Video => {
                        if let Some(dec) = self.video_decoder.as_mut() {
                            if let Ok(decoded) = dec.decode(&pkt) {
                                self.decoded_video_queue.extend(decoded);
                            }
                        }
                    }
                    PacketKind::Audio => {
                        if let Some(dec) = self.audio_decoder.as_mut() {
                            if let Ok(decoded) = dec.decode(&pkt) {
                                for af in decoded {
                                    self.audio_queue.push_interleaved(&af.samples);
                                }
                            }
                        }
                    }
                }
            }
            self.mse_fed_counts[sb_idx] = sb.frames.len();
        }

        if is_ended {
            if let Some(dec) = self.video_decoder.as_mut() {
                if let Ok(flushed) = dec.flush() {
                    self.decoded_video_queue.extend(flushed);
                }
            }
        }

        if let Some(front) = self.decoded_video_queue.front() {
            if self.video_width == 0 {
                self.video_width = front.width;
                self.video_height = front.height;
            }
            self.ready_state = 4;
        } else if !self.audio_queue.is_empty() {
            self.ready_state = 4;
        }
    }

    /// Evaluates the master [`MediaClock`] against queued decoded video frames and publishes
    /// the ready planar YUV420p/NV12 frame into `TripleBufferShmem` (zero CPU RGB conversion!).
    /// Returns true if a new frame was published into shared memory.
    fn publish_due_video_frame(&mut self) -> bool {
        let mut published = false;
        while let Some(front) = self.decoded_video_queue.front() {
            let pts_us = front.pts_us;
            match self.clock.evaluate_video_frame(pts_us) {
                AvSyncDecision::Wait { .. } => break,
                AvSyncDecision::Drop { .. } => {
                    // Drop late frame only if another frame is waiting behind it.
                    if self.decoded_video_queue.len() > 1 {
                        self.decoded_video_queue.pop_front();
                        continue;
                    }
                    let frame = self.decoded_video_queue.pop_front().unwrap();
                    if self.publish_frame_to_shmem(&frame) {
                        published = true;
                    }
                    break;
                }
                AvSyncDecision::Present { .. } => {
                    let frame = self.decoded_video_queue.pop_front().unwrap();
                    if self.publish_frame_to_shmem(&frame) {
                        published = true;
                    }
                    break;
                }
            }
        }
        published
    }

    fn publish_frame_to_shmem(&mut self, frame: &VideoFrame) -> bool {
        if !self.ensure_shmem_channel(frame.width, frame.height) {
            return false;
        }
        if let Some(prod) = self.shmem_producer.as_mut() {
            if prod.publish(frame).is_ok() {
                if let Some(prev_pts) = self.last_video_pts_us {
                    let dt = frame.pts_us - prev_pts;
                    if dt > 0 {
                        let inst_fps = 1_000_000.0 / (dt as f32);
                        self.estimated_fps = if self.estimated_fps <= 0.0 {
                            inst_fps
                        } else {
                            self.estimated_fps * 0.8 + inst_fps * 0.2
                        };
                    }
                }
                self.last_video_pts_us = Some(frame.pts_us);
                self.video_width = frame.width;
                self.video_height = frame.height;
                self.dirty_video_frame = true;
                self.frames_presented = self.frames_presented.saturating_add(1);
                return true;
            }
        }
        false
    }
}

/// Snapshot of active media playback telemetry for the `F12` Developer HUD.
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaPlaybackTelemetry {
    pub video_fps: f32,
    pub av_sync_jitter_ms: f32,
    pub frames_presented: u64,
    pub frames_dropped: u64,
}

/// Per-`ContentRuntime` registry managing HTML5 `<video>` / `<audio>` playback pipelines and
/// the compositor-side zero-copy `wgpu` [`YuvVideoRenderer`].
pub struct MediaElementSystem {
    pub players: HashMap<NodeId, MediaPlayerState>,
    pub media_error_proto: Option<HeapHandle>,
    yuv_renderer: Option<YuvVideoRenderer>,
}

/// Shared handle to [`MediaElementSystem`].
pub type SharedMediaElements = Rc<RefCell<MediaElementSystem>>;

impl Default for MediaElementSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaElementSystem {
    pub fn new() -> Self {
        Self {
            players: HashMap::new(),
            media_error_proto: None,
            yuv_renderer: None,
        }
    }

    /// Roots JS prototype handles retained by `MediaElementSystem`.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        if let Some(p) = self.media_error_proto {
            out.push(JsValue::from_object(p));
        }
    }

    /// Returns true if any media element is actively playing and needs VSync clock ticks.
    #[must_use]
    pub fn has_active_playback(&self) -> bool {
        self.players.values().any(|p| {
            p.clock.is_playing() && !p.ended && !p.seeking && p.ready_state >= HAVE_CURRENT_DATA
        })
    }

    /// Allocates a JS `MediaError` object for `(code, message)`.
    pub fn alloc_media_error(&self, vm: &mut JsVm, code: u16, message: &str) -> JsValue {
        let proto = self.media_error_proto.unwrap_or(vm.object_proto);
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let ck = vm.heap.intern_key("code");
        vm.heap.object_define_data(
            h,
            ck,
            JsValue::from_number(f64::from(code)),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        let mk = vm.heap.intern_key("message");
        let ms = vm.heap.alloc_string(message);
        vm.heap.object_define_data(
            h,
            mk,
            JsValue::from_string(ms),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        JsValue::from_object(h)
    }

    /// Attaches the compositor's [`SharedGpuDevice`] to initialize the `wgpu` YUV->RGBA shader.
    pub fn attach_shared_gpu(&mut self, gpu: SharedGpuDevice) {
        self.yuv_renderer = Some(YuvVideoRenderer::new(gpu));
    }

    /// Ensures a [`MediaPlayerState`] exists for DOM element `node_id`.
    pub fn ensure_player(&mut self, node_id: NodeId) -> &mut MediaPlayerState {
        self.players
            .entry(node_id)
            .or_insert_with(|| MediaPlayerState::new(node_id))
    }

    /// Returns the shared [`AudioSampleQueue`] for `node_id` (used by WebAudio `createMediaElementSource`).
    pub fn audio_queue_for(&mut self, node_id: NodeId) -> Arc<AudioSampleQueue> {
        Arc::clone(&self.ensure_player(node_id).audio_queue)
    }

    /// Advances all playing media elements by `audio_frames` master clock samples (used in headless
    /// testing and software clock stepping; hardware `cpal` advances `MediaClock` automatically),
    /// pumping MSE packets and publishing due planar YUV frames into `TripleBufferShmem`.
    pub fn step_playback(&mut self, ms_sys: &SharedMediaSource, audio_frames: u64) -> usize {
        let mut published = 0usize;
        let attached_map: HashMap<NodeId, HeapHandle> = ms_sys.borrow().attached_sources.clone();

        // Ensure players exist for all elements attached to a MediaSource.
        for &nid in attached_map.keys() {
            self.ensure_player(nid);
        }

        let mut ms_borrow = ms_sys.borrow_mut();
        for (&nid, player) in &mut self.players {
            if let Some(&ms_h) = attached_map.get(&nid) {
                if let Some(entry) = ms_borrow.media_sources.get_mut(&ms_h) {
                    player.pump_mse_packets(entry);
                    entry.current_playback_time_s = player.clock.current_time_s();
                }
            }
            if player.clock.is_playing() {
                if player.publish_due_video_frame() {
                    published += 1;
                }
                if audio_frames > 0 {
                    player.clock.advance_audio_frames(audio_frames);
                }
            }
        }
        published
    }

    /// Consumes all newly published planar YUV420p/NV12 frames from `TripleBufferShmem` and
    /// uploads/converts them directly on the GPU via `wgpu` BT.709/BT.601 fragment shader into
    /// `DisplayItem::ExternalSurface` — with **zero CPU pixel conversion** and **zero `DisplayList` rebuilds**.
    pub fn present_dirty_to_compositor(
        &mut self,
        compositor: &mut Compositor,
        gpu: Option<&mut VelloRenderer>,
    ) -> usize {
        let Some(vello) = gpu else {
            return 0;
        };
        if self.yuv_renderer.is_none() {
            if let Some(shared) = vello.shared_gpu_device() {
                self.yuv_renderer = Some(YuvVideoRenderer::new(shared));
            } else {
                return 0;
            }
        }
        let Some(yuv_renderer) = self.yuv_renderer.as_mut() else {
            return 0;
        };

        let mut presented = 0usize;
        for player in self.players.values_mut() {
            if !player.dirty_video_frame {
                continue;
            }
            if let Some(cons) = player.shmem_consumer.as_mut() {
                if let Some(slot) = cons.try_acquire_slot() {
                    if yuv_renderer
                        .upload_from_shmem_slot(player.surface_id, slot, vello, compositor)
                        .unwrap_or(false)
                    {
                        player.dirty_video_frame = false;
                        presented += 1;
                    }
                }
            }
        }
        presented
    }

    /// Aggregates active video FPS and A/V sync jitter telemetry for the `F12` Developer HUD.
    #[must_use]
    pub fn telemetry(&self) -> MediaPlaybackTelemetry {
        let mut out = MediaPlaybackTelemetry::default();
        for p in self.players.values() {
            let (pres, drop_cnt) = p.clock.frame_stats();
            out.frames_presented += pres;
            out.frames_dropped += drop_cnt;
            if p.estimated_fps > out.video_fps {
                out.video_fps = p.estimated_fps;
            }
            let j = p.clock.jitter_ms();
            if j > out.av_sync_jitter_ms {
                out.av_sync_jitter_ms = j;
            }
        }
        out
    }
}

/// Checks if timestamp `t` is covered by any buffered interval in `ranges` (with small frame epsilon).
pub(crate) fn time_is_buffered(ranges: &TimeRanges, t: f64) -> bool {
    const EPS: f64 = 0.05;
    ranges
        .as_slice()
        .iter()
        .any(|&(s, e)| t >= s - EPS && t <= e + 1e-4)
}

/// Advances all actively playing / seeking media elements on each VSync tick, emitting HTML5
/// `resize`, `timeupdate`, `seeked`, `canplay`, `canplaythrough`, `playing`, and `ended` events.
pub fn tick_playback_with_events(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
    me_sys: &SharedMediaElements,
    timestamp_ms: f64,
) {
    let node_ids: Vec<NodeId> = me_sys.borrow().players.keys().copied().collect();
    if node_ids.is_empty() {
        return;
    }

    let mut pending_events: Vec<(NodeId, &'static str)> = Vec::new();
    for nid in node_ids {
        let ms_handle_opt = ms_sys.borrow().attached_sources.get(&nid).copied();
        let (is_ended_source, duration, buffered, frame_dims) = if let Some(ms_h) = ms_handle_opt {
            let ms = ms_sys.borrow();
            if let Some(entry) = ms.media_sources.get(&ms_h) {
                let ended = entry.engine.ready_state == ReadyState::Ended;
                let dur = entry.engine.duration;
                let buf = entry.engine.buffered();
                let mut dims: Vec<(f64, u32, u32)> = Vec::new();
                for sb in &entry.engine.source_buffers {
                    for f in &sb.frames {
                        if let Some((w, h)) = f.video_dims {
                            if w > 0 && h > 0 {
                                dims.push((f.pts_s, w, h));
                            }
                        }
                    }
                }
                dims.sort_by(|a, b| a.0.total_cmp(&b.0));
                (ended, dur, buf, dims)
            } else {
                (false, f64::NAN, TimeRanges::new(), Vec::new())
            }
        } else {
            let me = me_sys.borrow();
            let dur = me.players.get(&nid).map_or(f64::NAN, |p| p.duration_s);
            let mut tr = TimeRanges::new();
            if dur.is_finite() && dur > 0.0 {
                tr.add_range(0.0, dur);
            }
            (true, dur, tr, Vec::new())
        };

        let mut me = me_sys.borrow_mut();
        let Some(player) = me.players.get_mut(&nid) else {
            continue;
        };
        if duration.is_finite() {
            player.duration_s = duration;
        }

        // Complete pending seek once buffered covers the target or stream is ended.
        if player.seeking {
            let target = player
                .seek_target_s
                .unwrap_or_else(|| player.clock.current_time_s());
            if is_ended_source || time_is_buffered(&buffered, target) {
                player.seeking = false;
                player.seek_target_s = None;
                if player.ready_state < HAVE_ENOUGH_DATA
                    && (!buffered.is_empty() || is_ended_source)
                {
                    player.ready_state = HAVE_ENOUGH_DATA;
                }
                pending_events.push((nid, "timeupdate"));
                pending_events.push((nid, "seeked"));
                if player.clock.is_playing() && !player.has_fired_playing {
                    player.has_fired_playing = true;
                    pending_events.push((nid, "playing"));
                }
            }
            continue;
        }

        if !player.clock.is_playing() || player.ended || player.ready_state < HAVE_CURRENT_DATA {
            player.last_vsync_ms = Some(timestamp_ms);
            continue;
        }

        if !player.has_fired_playing && player.ready_state >= HAVE_FUTURE_DATA {
            player.has_fired_playing = true;
            pending_events.push((nid, "playing"));
        }

        let Some(prev_ms) = player.last_vsync_ms else {
            player.last_vsync_ms = Some(timestamp_ms);
            continue;
        };
        if timestamp_ms <= prev_ms {
            continue;
        }
        let dt_s = ((timestamp_ms - prev_ms) / 1000.0).clamp(0.01, 0.25);
        player.last_vsync_ms = Some(timestamp_ms);

        let cur_t = player.clock.current_time_s();
        let buf_end = buffered.highest_end();
        let can_advance = is_ended_source || buf_end > cur_t + 1e-4;
        if !can_advance {
            continue;
        }

        let rate = player.clock.playback_rate().max(0.0625);
        let mut new_t = cur_t + dt_s * rate;
        if !is_ended_source && buf_end > 0.0 {
            new_t = new_t.min(buf_end);
        }
        if is_ended_source && duration.is_finite() && duration >= 0.0 {
            new_t = new_t.min(duration);
        }

        // Check video dimension transitions across `[cur_t, new_t]` for `resize` events.
        for &(pts, w, h) in &frame_dims {
            if pts >= cur_t - 1e-6
                && pts <= new_t + 1e-6
                && (w != player.video_width || h != player.video_height)
            {
                player.video_width = w;
                player.video_height = h;
                pending_events.push((nid, "resize"));
            }
        }

        if is_ended_source && duration.is_finite() && new_t >= duration - 1e-4 {
            if player.loop_enabled && duration > 0.0 {
                player.clock.seek_seconds(0.0);
                player.exact_time_s = Some(0.0);
                player.last_timeupdate_s = 0.0;
                pending_events.push((nid, "timeupdate"));
            } else {
                let final_t = duration.max(0.0);
                player.clock.seek_seconds(final_t);
                player.exact_time_s = Some(final_t);
                player.ended = true;
                player.clock.pause();
                player.last_timeupdate_s = final_t;
                pending_events.push((nid, "timeupdate"));
                pending_events.push((nid, "ended"));
            }
        } else if new_t > cur_t + 1e-6 {
            player.clock.seek_seconds(new_t);
            player.exact_time_s = Some(new_t);
            player.frames_presented = player.frames_presented.saturating_add(1);
            if let Some(ms_h) = ms_handle_opt {
                if let Some(entry) = ms_sys.borrow_mut().media_sources.get_mut(&ms_h) {
                    entry.current_playback_time_s = new_t;
                }
            }
            if (new_t - player.last_timeupdate_s).abs() >= 0.04 {
                player.last_timeupdate_s = new_t;
                pending_events.push((nid, "timeupdate"));
            }
        }
    }

    for (nid, ev_type) in pending_events {
        let target_val = wrap_node(vm, host, nid);
        if let Some(target_h) = target_val.as_object() {
            fire_object_event(vm, host, events, event_loop, target_h, ev_type, &[]);
        }
    }
}

fn define_const_u16(vm: &mut JsVm, obj: HeapHandle, name: &str, val: u16) {
    let key = vm.heap.intern_key(name);
    vm.heap.object_define_data(
        obj,
        key,
        JsValue::from_number(f64::from(val)),
        PropertyAttrs::READONLY,
        &mut vm.shapes,
    );
}

/// Installs full HTML5 `HTMLMediaElement` / `HTMLVideoElement` playback properties and methods
/// (`play`, `pause`, `load`, `currentTime`, `paused`, `ended`, `seeking`, `seekable`, `error`,
/// `networkState`, `readyState`, `volume`, `muted`, `playbackRate`, `videoWidth`, `videoHeight`,
/// `loop`, `autoplay`, `preload`, and `MediaError`).
pub fn install_media_elements(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    ms_sys: &SharedMediaSource,
) -> SharedMediaElements {
    let media_sys = Rc::new(RefCell::new(MediaElementSystem::new()));
    install_media_elements_with_system(vm, host, events, event_loop, web_apis, ms_sys, &media_sys);
    media_sys
}

/// Installs HTML5 media element APIs bound to a caller-provided [`SharedMediaElements`] registry.
pub fn install_media_elements_with_system(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    ms_sys: &SharedMediaSource,
    media_sys: &SharedMediaElements,
) {
    let media_proto = host.borrow().prototypes.html_media_element_proto;
    let video_proto = host.borrow().prototypes.html_video_element_proto;

    // Install `MediaError` interface & constants.
    let err_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    vm.heap.get_object_mut(err_proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, err_proto, "MediaError");
    media_sys.borrow_mut().media_error_proto = Some(err_proto);
    let err_ctor = vm.alloc_host_fn("MediaError", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: MediaError"))
    });
    for &(name, code) in &[
        ("MEDIA_ERR_ABORTED", MEDIA_ERR_ABORTED),
        ("MEDIA_ERR_NETWORK", MEDIA_ERR_NETWORK),
        ("MEDIA_ERR_DECODE", MEDIA_ERR_DECODE),
        ("MEDIA_ERR_SRC_NOT_SUPPORTED", MEDIA_ERR_SRC_NOT_SUPPORTED),
    ] {
        define_const_u16(vm, err_proto, name, code);
        define_const_u16(vm, err_ctor, name, code);
    }
    bind_ctor_proto(vm, err_ctor, err_proto);
    define_global(vm, "MediaError", JsValue::from_object(err_ctor));

    // Install `HTMLMediaElement` networkState & readyState constants on both ctor and prototype.
    let media_ctor_opt = {
        let key = vm.heap.intern_key("HTMLMediaElement");
        vm.get_property_by_key(vm.global, key)
            .ok()
            .and_then(|v| v.as_object())
    };
    for &(name, val) in &[
        ("NETWORK_EMPTY", NETWORK_EMPTY),
        ("NETWORK_IDLE", NETWORK_IDLE),
        ("NETWORK_LOADING", NETWORK_LOADING),
        ("NETWORK_NO_SOURCE", NETWORK_NO_SOURCE),
        ("HAVE_NOTHING", HAVE_NOTHING),
        ("HAVE_METADATA", HAVE_METADATA),
        ("HAVE_CURRENT_DATA", HAVE_CURRENT_DATA),
        ("HAVE_FUTURE_DATA", HAVE_FUTURE_DATA),
        ("HAVE_ENOUGH_DATA", HAVE_ENOUGH_DATA),
    ] {
        define_const_u16(vm, media_proto, name, val);
        if let Some(ctor_h) = media_ctor_opt {
            define_const_u16(vm, ctor_h, name, val);
        }
    }

    // `error`, `networkState`, `seeking`, `seekable` getters
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "error", move |vm, this, _| {
            let err_opt = h_g.borrow().node_for_value(vm, this).and_then(|nid| {
                me_g.borrow()
                    .players
                    .get(&nid)
                    .and_then(|p| p.error.clone())
            });
            if let Some((code, msg)) = err_opt {
                Ok(me_g.borrow().alloc_media_error(vm, code, &msg))
            } else {
                Ok(JsValue::NULL)
            }
        });
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "networkState", move |vm, this, _| {
            let ns = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.network_state))
                .unwrap_or(NETWORK_EMPTY);
            Ok(JsValue::from_number(f64::from(ns)))
        });
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "seeking", move |vm, this, _| {
            let seeking = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.seeking))
                .unwrap_or(false);
            Ok(JsValue::from_bool(seeking))
        });
    }
    {
        let h_g = Rc::clone(host);
        let ms_g = Rc::clone(ms_sys);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "seekable", move |vm, this, _| {
            let ranges = if let Some(nid) = h_g.borrow().node_for_value(vm, this) {
                let s = ms_g.borrow();
                if let Some(entry) = s
                    .attached_sources
                    .get(&nid)
                    .and_then(|ms_h| s.media_sources.get(ms_h))
                {
                    entry.engine.seekable()
                } else {
                    let dur = me_g
                        .borrow()
                        .players
                        .get(&nid)
                        .map_or(f64::NAN, |p| p.duration_s);
                    let mut tr = TimeRanges::new();
                    if dur.is_finite() && dur > 0.0 {
                        tr.add_range(0.0, dur);
                    }
                    tr
                }
            } else {
                TimeRanges::new()
            };
            Ok(ms_g.borrow_mut().alloc_time_ranges(vm, ranges))
        });
    }

    // `play() -> Promise<undefined>`
    {
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        let wa_cl = Rc::clone(web_apis);
        let ms_cl = Rc::clone(ms_sys);
        let me_cl = Rc::clone(media_sys);
        define_host_method(vm, media_proto, "play", move |vm, this, _| {
            let Some(nid) = h_cl.borrow().node_for_value(vm, this) else {
                return Ok(JsValue::from_object(promise_resolve_value(
                    vm,
                    JsValue::UNDEFINED,
                )));
            };
            let this_h = this.as_object();
            let (should_fire_play, should_fire_playing) = {
                let mut sys = me_cl.borrow_mut();
                let player = sys.ensure_player(nid);
                let was_paused = !player.clock.is_playing();
                if player.ended {
                    player.ended = false;
                    player.clock.seek_us(0);
                    player.exact_time_s = Some(0.0);
                    player.has_fired_playing = false;
                }
                // If loaded from a Blob URL without MSE, load container bytes directly.
                if !ms_cl.borrow().attached_sources.contains_key(&nid) && player.ready_state == 0 {
                    let src_url = match h_cl.borrow().document.borrow().get(nid) {
                        Some(rix_dom::NodeData::Element(el)) => {
                            el.get_attribute("src").unwrap_or("").to_string()
                        }
                        _ => String::new(),
                    };
                    if let Some(blob_bytes) = wa_cl.borrow().object_urls.get(&src_url).cloned() {
                        player.load_container_bytes(&blob_bytes);
                    }
                }
                player.clock.play();
                let ready_now = player.ready_state >= HAVE_FUTURE_DATA && !player.seeking;
                let fire_playing = ready_now && !player.has_fired_playing;
                if fire_playing {
                    player.has_fired_playing = true;
                }
                (was_paused, fire_playing)
            };
            if let Some(target_h) = this_h {
                if should_fire_play || should_fire_playing {
                    let h_t = Rc::clone(&h_cl);
                    let ev_t = Rc::clone(&ev_cl);
                    let el_t = Rc::clone(&el_cl);
                    el_cl
                        .borrow_mut()
                        .post_host_task(TaskPriority::Timer, move |vm| {
                            if should_fire_play {
                                fire_object_event(vm, &h_t, &ev_t, &el_t, target_h, "play", &[]);
                            }
                            if should_fire_playing {
                                fire_object_event(vm, &h_t, &ev_t, &el_t, target_h, "playing", &[]);
                            }
                        });
                }
            }
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::UNDEFINED,
            )))
        });
    }

    // `pause()`
    {
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        let me_cl = Rc::clone(media_sys);
        define_host_method(vm, media_proto, "pause", move |vm, this, _| {
            let Some(nid) = h_cl.borrow().node_for_value(vm, this) else {
                return Ok(JsValue::UNDEFINED);
            };
            let was_playing = {
                let mut sys = me_cl.borrow_mut();
                let p = sys.ensure_player(nid);
                let playing = p.clock.is_playing();
                p.clock.pause();
                p.has_fired_playing = false;
                playing
            };
            if was_playing {
                if let Some(target_h) = this.as_object() {
                    let h_t = Rc::clone(&h_cl);
                    let ev_t = Rc::clone(&ev_cl);
                    let el_t = Rc::clone(&el_cl);
                    el_cl
                        .borrow_mut()
                        .post_host_task(TaskPriority::Timer, move |vm| {
                            fire_object_event(vm, &h_t, &ev_t, &el_t, target_h, "pause", &[]);
                        });
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `load()`
    {
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        let ms_cl = Rc::clone(ms_sys);
        let me_cl = Rc::clone(media_sys);
        define_host_method(vm, media_proto, "load", move |vm, this, _| {
            let Some(nid) = h_cl.borrow().node_for_value(vm, this) else {
                return Ok(JsValue::UNDEFINED);
            };
            detach_media_source_from_element(vm, nid, &h_cl, &ev_cl, &el_cl, &ms_cl);
            me_cl
                .borrow_mut()
                .ensure_player(nid)
                .reset_for_source_change();
            Ok(JsValue::UNDEFINED)
        });
    }

    // `currentTime` get/set
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let ms_s = Rc::clone(ms_sys);
        let me_s = Rc::clone(media_sys);
        define_host_accessor(
            vm,
            media_proto,
            "currentTime",
            move |vm, this, _| {
                let t = h_g
                    .borrow()
                    .node_for_value(vm, this)
                    .and_then(|nid| {
                        me_g.borrow()
                            .players
                            .get(&nid)
                            .map(|p| p.exact_time_s.unwrap_or_else(|| p.clock.current_time_s()))
                    })
                    .unwrap_or(0.0);
                Ok(JsValue::from_number(t))
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let raw_sec = vm.to_number(arg(args, 0))?;
                if !raw_sec.is_finite() {
                    return Err(vm.type_error("currentTime must be finite"));
                }
                let (dur, buffered, is_ended) = {
                    let s = ms_s.borrow();
                    if let Some(entry) = s
                        .attached_sources
                        .get(&nid)
                        .and_then(|ms_h| s.media_sources.get(ms_h))
                    {
                        (
                            entry.engine.duration,
                            entry.engine.buffered(),
                            entry.engine.ready_state == ReadyState::Ended,
                        )
                    } else {
                        let d = me_s
                            .borrow()
                            .players
                            .get(&nid)
                            .map_or(f64::NAN, |p| p.duration_s);
                        (d, TimeRanges::new(), false)
                    }
                };
                let mut target = raw_sec.max(0.0);
                if dur.is_finite() && dur >= 0.0 {
                    target = target.min(dur);
                }
                let ms_h_opt = ms_s.borrow().attached_sources.get(&nid).copied();
                if let Some(ms_h) = ms_h_opt {
                    if let Some(entry) = ms_s.borrow_mut().media_sources.get_mut(&ms_h) {
                        entry.current_playback_time_s = target;
                    }
                }

                let (should_seek_events, immediate_complete) = {
                    let mut sys = me_s.borrow_mut();
                    let p = sys.ensure_player(nid);
                    p.clock.seek_seconds(target);
                    p.exact_time_s = Some(target);
                    p.audio_queue.clear();
                    p.ended = false;
                    if p.ready_state == HAVE_NOTHING {
                        (false, false)
                    } else {
                        let covered = is_ended || time_is_buffered(&buffered, target);
                        p.seeking = true;
                        p.seek_target_s = Some(target);
                        if !covered && p.ready_state > HAVE_METADATA {
                            p.ready_state = HAVE_METADATA;
                        }
                        (true, covered)
                    }
                };

                if should_seek_events {
                    if let Some(target_h) = this.as_object() {
                        let h_t = Rc::clone(&h_s);
                        let ev_t = Rc::clone(&ev_s);
                        let el_t = Rc::clone(&el_s);
                        let me_t = Rc::clone(&me_s);
                        el_s.borrow_mut()
                            .post_host_task(TaskPriority::Timer, move |vm| {
                                fire_object_event(vm, &h_t, &ev_t, &el_t, target_h, "seeking", &[]);
                                if immediate_complete {
                                    let still_same =
                                        me_t.borrow().players.get(&nid).is_some_and(|p| {
                                            p.seeking && p.seek_target_s == Some(target)
                                        });
                                    if still_same {
                                        if let Some(p) = me_t.borrow_mut().players.get_mut(&nid) {
                                            p.seeking = false;
                                            p.seek_target_s = None;
                                        }
                                        fire_object_event(
                                            vm,
                                            &h_t,
                                            &ev_t,
                                            &el_t,
                                            target_h,
                                            "timeupdate",
                                            &[],
                                        );
                                        fire_object_event(
                                            vm,
                                            &h_t,
                                            &ev_t,
                                            &el_t,
                                            target_h,
                                            "seeked",
                                            &[],
                                        );
                                    }
                                }
                            });
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `paused`, `ended`, `readyState`
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "paused", move |vm, this, _| {
            let paused = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| {
                    me_g.borrow()
                        .players
                        .get(&nid)
                        .map(|p| !p.clock.is_playing())
                })
                .unwrap_or(true);
            Ok(JsValue::from_bool(paused))
        });
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "ended", move |vm, this, _| {
            let ended = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.ended))
                .unwrap_or(false);
            Ok(JsValue::from_bool(ended))
        });
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, media_proto, "readyState", move |vm, this, _| {
            let rs = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.ready_state))
                .unwrap_or(0);
            Ok(JsValue::from_number(f64::from(rs)))
        });
    }

    // `volume`, `muted`, `playbackRate`, `loop`, `autoplay`
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(media_sys);
        define_host_accessor(
            vm,
            media_proto,
            "volume",
            move |vm, this, _| {
                let v = h_g
                    .borrow()
                    .node_for_value(vm, this)
                    .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.clock.volume()))
                    .unwrap_or(1.0);
                Ok(JsValue::from_number(v))
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let v = vm.to_number(arg(args, 0))?;
                if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                    return Err(crate::web_apis::create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "HTMLMediaElement.volume must be between 0.0 and 1.0",
                    ));
                }
                me_s.borrow_mut().ensure_player(nid).clock.set_volume(v);
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(media_sys);
        define_host_accessor(
            vm,
            media_proto,
            "muted",
            move |vm, this, _| {
                let m = h_g
                    .borrow()
                    .node_for_value(vm, this)
                    .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.clock.is_muted()))
                    .unwrap_or(false);
                Ok(JsValue::from_bool(m))
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let m = vm.to_boolean(arg(args, 0));
                me_s.borrow_mut().ensure_player(nid).clock.set_muted(m);
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(media_sys);
        define_host_accessor(
            vm,
            media_proto,
            "playbackRate",
            move |vm, this, _| {
                let r = h_g
                    .borrow()
                    .node_for_value(vm, this)
                    .and_then(|nid| {
                        me_g.borrow()
                            .players
                            .get(&nid)
                            .map(|p| p.clock.playback_rate())
                    })
                    .unwrap_or(1.0);
                Ok(JsValue::from_number(r))
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let r = vm.to_number(arg(args, 0))?;
                if r.is_finite() && r > 0.0 {
                    me_s.borrow_mut()
                        .ensure_player(nid)
                        .clock
                        .set_playback_rate(r);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(media_sys);
        define_host_accessor(
            vm,
            media_proto,
            "loop",
            move |vm, this, _| {
                let nid_opt = h_g.borrow().node_for_value(vm, this);
                let v = nid_opt.is_some_and(|nid| {
                    h_g.borrow().document.borrow().get(nid).is_some_and(
                        |d| matches!(d, rix_dom::NodeData::Element(e) if e.has_attribute("loop")),
                    ) || me_g
                        .borrow()
                        .players
                        .get(&nid)
                        .is_some_and(|p| p.loop_enabled)
                });
                Ok(JsValue::from_bool(v))
            },
            move |vm, this, args| {
                let nid_opt = h_s.borrow().node_for_value(vm, this);
                if let Some(nid) = nid_opt {
                    let on = vm.to_boolean(arg(args, 0));
                    me_s.borrow_mut().ensure_player(nid).loop_enabled = on;
                    if on {
                        crate::dom_bridge::set_element_attr(vm, &h_s, nid, "loop", "");
                    } else {
                        crate::dom_bridge::remove_element_attr(vm, &h_s, nid, "loop");
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `videoWidth` & `videoHeight` on `HTMLVideoElement.prototype`
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, video_proto, "videoWidth", move |vm, this, _| {
            let w = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.video_width))
                .unwrap_or(0);
            Ok(JsValue::from_number(f64::from(w)))
        });
    }
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        define_host_getter(vm, video_proto, "videoHeight", move |vm, this, _| {
            let h = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.video_height))
                .unwrap_or(0);
            Ok(JsValue::from_number(f64::from(h)))
        });
    }

    // `HTMLVideoElement.prototype.getVideoPlaybackQuality()` (W3C Media Playback Quality)
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(media_sys);
        let el_g = Rc::clone(event_loop);
        define_host_method(
            vm,
            video_proto,
            "getVideoPlaybackQuality",
            move |vm, this, _| {
                let (total_frames, dropped_frames) = h_g
                    .borrow()
                    .node_for_value(vm, this)
                    .and_then(|nid| {
                        me_g.borrow().players.get(&nid).map(|p| {
                            let (pres, drop_cnt) = p.clock.frame_stats();
                            (pres.max(p.frames_presented), drop_cnt)
                        })
                    })
                    .unwrap_or((0, 0));
                let now_ms = el_g.borrow().now_ms();
                let obj = vm.heap.alloc_object(JsObject::with_kind(
                    ObjectKind::Ordinary,
                    Some(vm.object_proto),
                ));
                for &(name, val) in &[
                    ("creationTime", now_ms),
                    ("totalVideoFrames", total_frames as f64),
                    ("droppedVideoFrames", dropped_frames as f64),
                    ("corruptedVideoFrames", 0.0),
                ] {
                    let k = vm.heap.intern_key(name);
                    vm.heap.object_define_data(
                        obj,
                        k,
                        JsValue::from_number(val),
                        PropertyAttrs::METHOD,
                        &mut vm.shapes,
                    );
                }
                Ok(JsValue::from_object(obj))
            },
        );
    }
}
