//! Sound: events (`registry::SoundDef`) play one of their files, positioned in the world or not;
//! every category has its own volume (`core:volume_*` settings). World sounds go through one
//! track whose low-pass filter muffles them under water and whose send feeds a reverb in caves.
//! Music streams on its own track after quiet stretches. Without an audio device everything
//! here does nothing.

use std::time::{Duration, Instant};

use glam::{DVec3, Quat};
use kira::effect::filter::{FilterBuilder, FilterHandle};
use kira::effect::reverb::ReverbBuilder;
use kira::sound::static_sound::StaticSoundData;
use kira::sound::streaming::{StreamingSoundData, StreamingSoundHandle};
use kira::track::{SendTrackBuilder, SendTrackHandle, SpatialTrackBuilder, TrackBuilder, TrackHandle};
use kira::{AudioManager, AudioManagerSettings, Decibels, DefaultBackend, Mix, Tween};
use rustc_hash::FxHashMap;

use crate::assets::Assets;
use crate::content::content;

/// A sound to play this frame: event id, position (`None` = not positioned), volume, pitch (0 =
/// the event's random range).
#[derive(Clone, Copy, Debug)]
pub struct Play {
    pub sound: u32,
    pub pos: Option<DVec3>,
    pub volume: f32,
    pub pitch: f32,
}

/// Where the listener is, for positions and the environment.
pub struct Listener {
    pub pos: DVec3,
    pub yaw: f32,
    pub pitch: f32,
    pub underwater: bool,
    /// No sky light at the head: caves echo.
    pub enclosed: bool,
    pub dim: u32,
    pub day: bool,
    pub creative: bool,
}

pub struct Audio {
    mgr: AudioManager<DefaultBackend>,
    listener: kira::listener::ListenerHandle,
    reverb: SendTrackHandle,
    world: TrackHandle,
    lowpass: FilterHandle,
    /// Per category (`sound_category`): its track.
    groups: Vec<TrackHandle>,
    files: FxHashMap<String, Option<StaticSoundData>>,
    /// Files of a `name*` pattern.
    globs: FxHashMap<String, Vec<String>>,
    music: Option<StreamingSoundHandle<kira::sound::FromFileError>>,
    next_music: Instant,
    state: (bool, bool),
    /// Per category, then the master volume.
    volumes: Vec<f64>,
    rng: loom_noise::Rng,
    /// Sounds started so far.
    pub played: u32,
    /// Automated runs (tests, scripts): everything plays and counts, silently.
    pub muted: bool,
    /// Each category's volume setting id, then the master volume's, looked up once.
    volume_ids: Option<Vec<Option<u32>>>,
}

fn sound_path(assets: &Assets, name: &str) -> Option<std::path::PathBuf> {
    assets.path(name, "sounds", "ogg").or_else(|| assets.path(name, "sounds", "wav"))
}

const QUICK: Tween = Tween { start_time: kira::StartTime::Immediate, duration: Duration::from_millis(400), easing: kira::Easing::Linear };

fn db(volume: f64) -> Decibels {
    if volume <= 0.001 { Decibels::SILENCE } else { Decibels((20.0 * volume.log10()) as f32) }
}

impl Audio {
    pub fn new() -> Option<Audio> {
        let mut mgr = match AudioManager::<DefaultBackend>::new(AudioManagerSettings::default()) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("no sound: {e}");
                return None;
            }
        };
        let made = (|| -> Result<Audio, kira::ResourceLimitReached> {
            let listener = mgr.add_listener(glam::Vec3::ZERO, Quat::IDENTITY)?;
            let reverb = mgr.add_send_track(SendTrackBuilder::new().with_effect(ReverbBuilder::new().mix(Mix::WET).damping(0.6).feedback(0.85)))?;
            let mut wb = TrackBuilder::new().sub_track_capacity(64).with_send(&reverb, Decibels::SILENCE);
            let lowpass = wb.add_effect(FilterBuilder::new().cutoff(20_000.0));
            let mut world = mgr.add_sub_track(wb)?;
            let mut groups = Vec::new();
            for k in &crate::content::content().game.sound_categories {
                let b = TrackBuilder::new().sub_track_capacity(256).sound_capacity(256);
                groups.push(if k.world { world.add_sub_track(b)? } else { mgr.add_sub_track(b)? });
            }
            Ok(Audio {
                mgr,
                listener,
                reverb,
                world,
                lowpass,
                groups,
                files: FxHashMap::default(),
                globs: FxHashMap::default(),
                music: None,
                next_music: Instant::now() + Duration::from_secs(20),
                state: (false, false),
                volumes: Vec::new(),
                rng: loom_noise::Rng(0x2545_F491_4F6C_DD1D),
                played: 0,
                muted: false,
                volume_ids: None,
            })
        })();
        match made {
            Ok(a) => Some(a),
            Err(e) => {
                log::warn!("no sound: {e:?}");
                None
            }
        }
    }

    fn random(&mut self) -> f32 {
        self.rng.float()
    }

    /// Forgets decoded files, for packs that changed them (F3+T).
    pub fn reload(&mut self) {
        self.files.clear();
        self.globs.clear();
    }

    /// Decoded once, on first use: a file is a few kilobytes and decodes in well under a
    /// millisecond, so nothing waits for a sound on a screen or in the world.
    fn file(&mut self, assets: &Assets, name: &str) -> Option<StaticSoundData> {
        if let Some(d) = self.files.get(name) {
            return d.clone();
        }
        let path = sound_path(assets, name);
        let data = path.and_then(|p| match StaticSoundData::from_file(&p) {
            Ok(d) => Some(d),
            Err(e) => {
                log::warn!("sound {name}: {e}");
                None
            }
        });
        if data.is_none() {
            log::warn!("missing sound {name}");
        }
        self.files.insert(name.to_string(), data.clone());
        data
    }

    /// Settings, listener, environment and music, once a frame.
    pub fn update(&mut self, assets: &Assets, l: Option<&Listener>) {
        let ids = self.volume_ids.get_or_insert_with(|| {
            let mut ids: Vec<Option<u32>> = crate::content::content().game.sound_categories.iter().map(|k| k.setting.as_deref().and_then(crate::config::by_name)).collect();
            ids.push(crate::config::by_name("core:volume_master"));
            ids
        });
        // UI has no setting of its own: full volume.
        let mut volumes: Vec<f64> = ids.iter().map(|id| id.map_or(1.0, |id| crate::config::get(id) / 100.0)).collect();
        if self.muted {
            *volumes.last_mut().unwrap() = 0.0;
        }
        if volumes != self.volumes {
            for (g, v) in self.groups.iter_mut().zip(&volumes) {
                g.set_volume(db(*v), Tween::default());
            }
            self.mgr.main_track().set_volume(db(*volumes.last().unwrap()), Tween::default());
            self.volumes = volumes;
        }
        let Some(l) = l else {
            if let Some(m) = &mut self.music {
                m.stop(Tween { duration: Duration::from_secs(2), ..Default::default() });
                self.music = None;
            }
            return;
        };
        let orientation = Quat::from_rotation_y(l.yaw) * Quat::from_rotation_x(l.pitch);
        self.listener.set_position(l.pos.as_vec3(), Tween::default());
        self.listener.set_orientation(orientation, Tween::default());
        if (l.underwater, l.enclosed) != self.state {
            self.state = (l.underwater, l.enclosed);
            self.lowpass.set_cutoff(if l.underwater { 900.0 } else { 20_000.0 }, QUICK);
            if let Err(e) = self.world.set_send(&self.reverb, if l.enclosed && !l.underwater { Decibels(-9.0) } else { Decibels::SILENCE }, QUICK) {
                log::debug!("cave reverb: {e:?}");
            }
        }
        self.music(assets, l);
    }

    fn music(&mut self, assets: &Assets, l: &Listener) {
        use loom_abi::music_when as mw;
        if self.music.as_ref().is_some_and(|m| m.state() != kira::sound::PlaybackState::Stopped) || Instant::now() < self.next_music {
            return;
        }
        self.music = None;
        let c = content();
        let fits = |m: &crate::registry::MusicDef| {
            let w = m.when;
            m.dimension.is_none_or(|d| d == l.dim)
                && (w & (mw::DAY | mw::NIGHT) == 0 || (w & mw::DAY != 0 && l.day) || (w & mw::NIGHT != 0 && !l.day))
                && (w & (mw::UNDERGROUND | mw::SURFACE) == 0 || (w & mw::UNDERGROUND != 0 && l.enclosed) || (w & mw::SURFACE != 0 && !l.enclosed))
                && (w & mw::CREATIVE == 0 || l.creative)
        };
        // Quiet for a few minutes between tracks, like vanilla's (shorter: there is less music).
        self.next_music = Instant::now() + Duration::from_secs(120 + (self.random() * 180.0) as u64);
        let Some(m) = c.game.music.iter().find(|m| fits(m) && !m.tracks.is_empty()) else { return };
        let track = m.tracks[(self.random() * m.tracks.len() as f32) as usize % m.tracks.len()].clone();
        let Some(path) = sound_path(assets, &track) else {
            log::warn!("missing music {track}");
            return;
        };
        match StreamingSoundData::from_file(&path) {
            Ok(data) => match self.groups[crate::registry::MUSIC_CATEGORY as usize].play(data.fade_in_tween(Tween { duration: Duration::from_secs(3), ..Default::default() })) {
                Ok(h) => {
                    log::debug!("music: {track}");
                    self.music = Some(h);
                }
                Err(e) => log::warn!("music {track}: {e}"),
            },
            Err(e) => log::warn!("music {track}: {e}"),
        }
    }

    /// Ends the current track now and lets the next start soon (tests, `/music`).
    pub fn skip_music(&mut self) {
        if let Some(m) = &mut self.music {
            m.stop(QUICK);
        }
        self.music = None;
        self.next_music = Instant::now();
    }

    pub fn music_playing(&self) -> bool {
        self.music.as_ref().is_some_and(|m| m.state() != kira::sound::PlaybackState::Stopped)
    }

    pub fn play(&mut self, assets: &Assets, p: Play) {
        let c = content();
        let Some(def) = c.game.sounds.get(p.sound as usize) else { return };
        if def.files.is_empty() {
            return;
        }
        let mut file = def.files[(self.random() * def.files.len() as f32) as usize % def.files.len()].clone();
        // `hurt*`: one of the files whose names start so, found once.
        if let Some(prefix) = file.strip_suffix('*') {
            let found = self.globs.entry(file.clone()).or_insert_with(|| {
                let mut v: Vec<String> = ["ogg", "wav"].iter().flat_map(|e| assets.list("sounds", e)).map(|f| f.0).filter(|n| n.starts_with(prefix)).collect();
                v.sort();
                v.dedup();
                if v.is_empty() {
                    log::warn!("no sound files {file}");
                }
                v
            });
            if found.is_empty() {
                return;
            }
            let n = found.len();
            let i = (self.random() * n as f32) as usize % n;
            file = self.globs[&file][i].clone();
        }
        let Some(data) = self.file(assets, &file) else { return };
        let pitch = if p.pitch > 0.0 { p.pitch } else { def.pitch[0] + (def.pitch[1] - def.pitch[0]) * self.random() };
        let volume = (p.volume * def.volume).max(0.0);
        let data = data.volume(db(volume as f64)).playback_rate(pitch as f64);
        let g = (def.category as usize).min(self.groups.len().saturating_sub(1));
        let group = &mut self.groups[g];
        let played = match p.pos {
            Some(pos) => {
                // Heard up to `range` blocks (further for loud ones), full volume within one.
                let reach = def.range * volume.max(1.0);
                let b = SpatialTrackBuilder::new().distances((1.0, reach.max(1.5))).persist_until_sounds_finish(true);
                group.add_spatial_sub_track(&self.listener, pos.as_vec3(), b).map_err(|e| format!("{e:?}")).and_then(|mut t| t.play(data).map(|_| ()).map_err(|e| e.to_string()))
            }
            None => group.play(data).map(|_| ()).map_err(|e| e.to_string()),
        };
        match played {
            Ok(()) => self.played += 1,
            Err(e) => log::debug!("sound {}: {e}", def.name),
        }
    }
}
