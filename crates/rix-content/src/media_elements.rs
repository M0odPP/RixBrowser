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
use rix_js::{HeapHandle, JsValue, JsVm};
use rix_layout::surface_id_for_node;
use rix_media::{
    create_audio_decoder, create_video_decoder, create_video_frame_channel, demux_bytes,
    AudioDecoder, AudioSampleQueue, AvSyncDecision, MediaClock, PacketKind, TrackKind,
    VideoDecoder, VideoFrame, VideoFrameConsumer, VideoFrameProducer,
};
use rix_render::{Compositor, SharedGpuDevice, VelloRenderer, YuvVideoRenderer};

use crate::dom_bridge::SharedHost;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{fire_object_event, SharedEvents};
use crate::media_source::SharedMediaSource;
use crate::web_apis::{
    arg, define_host_accessor, define_host_getter, define_host_method, SharedWebApis,
};

/// Live decoding & A/V presentation pipeline bound to a single `<video>` or `<audio>` DOM element.
pub struct MediaPlayerState {
    pub node_id: NodeId,
    pub surface_id: SurfaceId,
    pub clock: Arc<MediaClock>,
    pub audio_queue: Arc<AudioSampleQueue>,
    pub video_width: u32,
    pub video_height: u32,
    pub ready_state: u16,
    pub ended: bool,
    pub loop_enabled: bool,
    pub autoplay: bool,
    pub preload: String,
    pub duration_s: f64,
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
            ready_state: 0,
            ended: false,
            loop_enabled: false,
            autoplay: false,
            preload: "metadata".to_string(),
            duration_s: f64::NAN,
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
            yuv_renderer: None,
        }
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

/// Installs full HTML5 `HTMLMediaElement` / `HTMLVideoElement` playback properties and methods
/// (`play`, `pause`, `load`, `currentTime`, `paused`, `ended`, `volume`, `muted`, `playbackRate`,
/// `readyState`, `videoWidth`, `videoHeight`, `loop`, `autoplay`, `preload`).
pub fn install_media_elements(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    ms_sys: &SharedMediaSource,
) -> SharedMediaElements {
    let media_sys = Rc::new(RefCell::new(MediaElementSystem::new()));
    let media_proto = host.borrow().prototypes.html_media_element_proto;
    let video_proto = host.borrow().prototypes.html_video_element_proto;

    // `play() -> Promise<undefined>`
    {
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        let wa_cl = Rc::clone(web_apis);
        let ms_cl = Rc::clone(ms_sys);
        let me_cl = Rc::clone(&media_sys);
        define_host_method(vm, media_proto, "play", move |vm, this, _| {
            let Some(nid) = h_cl.borrow().node_for_value(vm, this) else {
                return Ok(JsValue::from_object(promise_resolve_value(
                    vm,
                    JsValue::UNDEFINED,
                )));
            };
            let this_h = this.as_object();
            {
                let mut sys = me_cl.borrow_mut();
                let player = sys.ensure_player(nid);
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
                player.ended = false;
                player.clock.play();
            }
            if let Some(target_h) = this_h {
                let h_t = Rc::clone(&h_cl);
                let ev_t = Rc::clone(&ev_cl);
                let el_t = Rc::clone(&el_cl);
                el_cl
                    .borrow_mut()
                    .post_host_task(TaskPriority::Timer, move |vm| {
                        fire_object_event(vm, &h_t, &ev_t, &el_t, target_h, "play", &[]);
                        fire_object_event(vm, &h_t, &ev_t, &el_t, target_h, "playing", &[]);
                    });
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
        let me_cl = Rc::clone(&media_sys);
        define_host_method(vm, media_proto, "pause", move |vm, this, _| {
            let Some(nid) = h_cl.borrow().node_for_value(vm, this) else {
                return Ok(JsValue::UNDEFINED);
            };
            me_cl.borrow_mut().ensure_player(nid).clock.pause();
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
            Ok(JsValue::UNDEFINED)
        });
    }

    // `load()`
    {
        let h_cl = Rc::clone(host);
        let me_cl = Rc::clone(&media_sys);
        define_host_method(vm, media_proto, "load", move |vm, this, _| {
            let Some(nid) = h_cl.borrow().node_for_value(vm, this) else {
                return Ok(JsValue::UNDEFINED);
            };
            let mut sys = me_cl.borrow_mut();
            let p = sys.ensure_player(nid);
            p.clock.pause();
            p.clock.seek_us(0);
            p.decoded_video_queue.clear();
            p.audio_queue.clear();
            p.mse_fed_counts.clear();
            p.ended = false;
            Ok(JsValue::UNDEFINED)
        });
    }

    // `currentTime` get/set
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(&media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(&media_sys);
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
                            .map(|p| p.clock.current_time_s())
                    })
                    .unwrap_or(0.0);
                Ok(JsValue::from_number(t))
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let sec = vm.to_number(arg(args, 0))?;
                if sec.is_finite() {
                    let mut sys = me_s.borrow_mut();
                    let p = sys.ensure_player(nid);
                    p.clock.seek_seconds(sec);
                    p.audio_queue.clear();
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `paused`, `ended`, `readyState`
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
        let h_s = Rc::clone(host);
        let me_s = Rc::clone(&media_sys);
        define_host_accessor(
            vm,
            media_proto,
            "loop",
            move |vm, this, _| {
                let v = h_g
                    .borrow()
                    .node_for_value(vm, this)
                    .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.loop_enabled))
                    .unwrap_or(false);
                Ok(JsValue::from_bool(v))
            },
            move |vm, this, args| {
                if let Some(nid) = h_s.borrow().node_for_value(vm, this) {
                    me_s.borrow_mut().ensure_player(nid).loop_enabled = vm.to_boolean(arg(args, 0));
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `videoWidth` & `videoHeight` on `HTMLVideoElement.prototype`
    {
        let h_g = Rc::clone(host);
        let me_g = Rc::clone(&media_sys);
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
        let me_g = Rc::clone(&media_sys);
        define_host_getter(vm, video_proto, "videoHeight", move |vm, this, _| {
            let h = h_g
                .borrow()
                .node_for_value(vm, this)
                .and_then(|nid| me_g.borrow().players.get(&nid).map(|p| p.video_height))
                .unwrap_or(0);
            Ok(JsValue::from_number(f64::from(h)))
        });
    }

    media_sys
}
