//! World saves.
//!
//! Only cubes that changed after generation are written: the generator is deterministic, so an
//! untouched cube is cheaper to regenerate than to read back. Cubes go into region files of 8^3
//! cubes: a 512-entry (offset, length) header followed by LZ4 blobs appended at the end.
//!
//! Block states are stored as world ids. `ids.txt` maps them to names, so adding or removing mods
//! never shifts what is stored on disk; runtime ids are remapped through the palette on load.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use glam::IVec3;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::app::{Ctx, Request};
use crate::content::{AIR, Content};
use crate::screens::{BuildCx, Screen, button, frame, label};
use crate::ui::text::{self, Arg};
use crate::ui::{Kind, Style, Ui};
use crate::world::Cube;

const REGION: i32 = 8;
const ENTRIES: usize = (REGION * REGION * REGION) as usize;

/// Save layout version: worlds of another one are not opened (no conversions before release).
pub const SAVE_FORMAT: u32 = 3;

#[derive(Clone, Debug)]
pub struct WorldMeta {
    pub dir: PathBuf,
    /// `SAVE_FORMAT` of the build that wrote it.
    pub format: u32,
    pub name: String,
    pub seed: u64,
    pub last_played: u64,
    pub times: Vec<(String, u64)>,
    /// Area edits per dimension (`server::area::AreaFill::spec`), in order.
    pub edits: Vec<(String, String)>,
    pub next_entity: u64,
    /// The world's settings (`config_scope::WORLD`: game rules, `/rule`).
    pub configs: Vec<(String, f64)>,
    /// Mods (id, version) the world was last played with.
    pub mods: Vec<(String, String)>,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl WorldMeta {
    pub fn load(dir: &Path) -> Option<WorldMeta> {
        let src = std::fs::read_to_string(dir.join("world.txt")).ok()?;
        let mut m = WorldMeta { dir: dir.to_path_buf(), format: 1, name: String::new(), seed: 0, last_played: 0, times: Vec::new(), edits: Vec::new(), next_entity: 1, configs: Vec::new(), mods: Vec::new() };
        for (_, k, v) in crate::kv::pairs(&src) {
            match k {
                "name" => m.name = v.to_string(),
                "format" => m.format = v.parse().unwrap_or(1),
                "seed" => m.seed = v.parse().unwrap_or(0),
                "last_played" => m.last_played = v.parse().unwrap_or(0),
                "next_entity" => m.next_entity = v.parse().unwrap_or(1),
                _ => {
                    if let Some(d) = k.strip_prefix("time.") {
                        m.times.push((d.to_string(), v.parse().unwrap_or(0)));
                    } else if let Some(d) = k.strip_prefix("edit.") {
                        // `edit.<dimension>.<n>`: the dimension's name has a colon, not a dot.
                        let d = d.rsplit_once('.').map_or(d, |(d, _)| d);
                        m.edits.push((d.to_string(), v.to_string()));
                    } else if let Some(r) = k.strip_prefix("config.") {
                        m.configs.push((r.to_string(), v.parse().unwrap_or(0.0)));
                    } else if let Some(id) = k.strip_prefix("mod.") {
                        m.mods.push((id.to_string(), v.to_string()));
                    }
                }
            }
        }
        Some(m)
    }

    pub fn save(&self) {
        let mut s = format!(
            "format = {}\nname = {}\nseed = {}\nlast_played = {}\nnext_entity = {}\n",
            self.format, self.name, self.seed, self.last_played, self.next_entity
        );
        for (d, t) in &self.times {
            s.push_str(&format!("time.{d} = {t}\n"));
        }
        for (i, (d, e)) in self.edits.iter().enumerate() {
            s.push_str(&format!("edit.{d}.{i} = {e}\n"));
        }
        for (r, v) in &self.configs {
            s.push_str(&format!("config.{r} = {v}\n"));
        }
        for (id, v) in &self.mods {
            s.push_str(&format!("mod.{id} = {v}\n"));
        }
        let _ = std::fs::create_dir_all(&self.dir);
        if let Err(e) = std::fs::write(self.dir.join("world.txt"), s) {
            log::error!("saving world info: {e}");
        }
    }

    /// Mods the world was played with that are not loaded now, as "id version".
    pub fn missing_mods(&self, c: &Content) -> Vec<String> {
        self.mods.iter().filter(|(id, _)| !c.mods.iter().any(|m| &m.id == id)).map(|(id, v)| format!("{id} {v}")).collect()
    }
}

pub fn list(saves: &Path) -> Vec<WorldMeta> {
    let mut v: Vec<WorldMeta> = std::fs::read_dir(saves)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| WorldMeta::load(&e.path()))
        .collect();
    v.sort_by(|a, b| b.last_played.cmp(&a.last_played));
    v
}

pub fn create(saves: &Path, name: &str, seed: u64) -> WorldMeta {
    let base: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let base = if base.is_empty() { "world".to_string() } else { base };
    let mut dir = saves.join(&base);
    let mut n = 1;
    while dir.exists() {
        n += 1;
        dir = saves.join(format!("{base}_{n}"));
    }
    let m = WorldMeta { dir, format: SAVE_FORMAT, name: name.to_string(), seed, last_played: now_secs(), times: Vec::new(), edits: Vec::new(), next_entity: 1, configs: Vec::new(), mods: Vec::new() };
    m.save();
    m
}

/// Seed from user text: numbers are used as is, anything else is hashed.
pub fn parse_seed(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return loom_noise::mix(now_secs() ^ (std::process::id() as u64) << 32);
    }
    s.parse::<i64>().map(|v| v as u64).unwrap_or_else(|_| crate::world::mix(s.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))))
}

// ---------------------------------------------------------------- id table

pub struct IdTable {
    names: Vec<String>,
    to_runtime: Vec<u32>,
    to_world: FxHashMap<u32, u32>,
    dirty: bool,
}

impl IdTable {
    pub fn load(dir: &Path, c: &Content) -> IdTable {
        let names: Vec<String> = std::fs::read_to_string(dir.join("ids.txt"))
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let mut missing = Vec::new();
        let to_runtime: Vec<u32> = names
            .iter()
            .map(|n| {
                c.state_by_name(n).unwrap_or_else(|| {
                    missing.push(n.clone());
                    AIR
                })
            })
            .collect();
        if !missing.is_empty() {
            log::warn!("world contains {} block states from missing mods (shown as air): {}", missing.len(), missing.join(", "));
        }
        let to_world = to_runtime.iter().enumerate().filter(|(i, r)| **r != AIR || names[*i] == "core:air").map(|(i, &r)| (r, i as u32)).collect();
        IdTable { names, to_runtime, to_world, dirty: false }
    }

    pub fn world_id(&mut self, c: &Content, runtime: u32) -> u32 {
        if let Some(&w) = self.to_world.get(&runtime) {
            return w;
        }
        let w = self.names.len() as u32;
        self.names.push(c.state_name(runtime));
        self.to_runtime.push(runtime);
        self.to_world.insert(runtime, w);
        self.dirty = true;
        w
    }

    pub fn runtime_id(&self, world: u32) -> u32 {
        self.to_runtime.get(world as usize).copied().unwrap_or(AIR)
    }

    pub fn save(&mut self, dir: &Path) {
        if self.dirty {
            let _ = std::fs::write(dir.join("ids.txt"), self.names.join("\n"));
            self.dirty = false;
        }
    }
}

// ---------------------------------------------------------------- region files

struct Region {
    file: File,
    header: Vec<(u32, u32)>,
}

impl Region {
    fn open(path: &Path) -> std::io::Result<Region> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let len = file.metadata()?.len();
        let mut header = vec![(0u32, 0u32); ENTRIES];
        if len >= (ENTRIES * 8) as u64 {
            let mut raw = vec![0u8; ENTRIES * 8];
            file.read_exact(&mut raw)?;
            for (i, h) in header.iter_mut().enumerate() {
                let o = u32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().unwrap());
                let l = u32::from_le_bytes(raw[i * 8 + 4..i * 8 + 8].try_into().unwrap());
                *h = (o, l);
            }
        } else {
            file.write_all(&vec![0u8; ENTRIES * 8])?;
        }
        Ok(Region { file, header })
    }

    fn read(&mut self, i: usize) -> Option<Vec<u8>> {
        let (o, l) = self.header[i];
        if l == 0 {
            return None;
        }
        let mut buf = vec![0u8; l as usize];
        self.file.seek(SeekFrom::Start(o as u64)).ok()?;
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn write(&mut self, i: usize, data: &[u8]) -> std::io::Result<()> {
        let end = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(data)?;
        self.header[i] = (end as u32, data.len() as u32);
        self.file.seek(SeekFrom::Start((i * 8) as u64))?;
        self.file.write_all(&(end as u32).to_le_bytes())?;
        self.file.write_all(&(data.len() as u32).to_le_bytes())?;
        Ok(())
    }
}

pub struct Storage {
    pub meta: WorldMeta,
    pub ids: IdTable,
    regions: FxHashMap<(u32, IVec3), Region>,
    dim_dirs: Vec<PathBuf>,
    /// Region files on disk, listed once: most cubes were never saved, and asking the file
    /// system about each of them costs far more than generating it.
    on_disk: FxHashSet<(u32, IVec3)>,
    /// Per dimension, the cubes saved: what differs from generation (clients generate the
    /// rest themselves).
    saved: Vec<FxHashSet<IVec3>>,
}

fn region_of(cube: IVec3) -> (IVec3, usize) {
    let r = IVec3::new(cube.x.div_euclid(REGION), cube.y.div_euclid(REGION), cube.z.div_euclid(REGION));
    let l = cube - r * REGION;
    (r, ((l.y * REGION + l.z) * REGION + l.x) as usize)
}

impl Storage {
    pub fn open(mut meta: WorldMeta, c: &Content) -> Storage {
        meta.mods = c.mods.iter().map(|m| (m.id.clone(), m.version.clone())).collect();
        let ids = IdTable::load(&meta.dir, c);
        let dim_dirs: Vec<PathBuf> = c.dims.iter().map(|d| meta.dir.join("dims").join(d.name.replace(':', "_"))).collect();
        let mut on_disk = FxHashSet::default();
        let mut saved = vec![FxHashSet::default(); dim_dirs.len()];
        for (dim, dir) in dim_dirs.iter().enumerate() {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let parts: Vec<i32> = name.strip_prefix("r.").and_then(|n| n.strip_suffix(".bin")).map_or(Vec::new(), |n| n.split('.').filter_map(|v| v.parse().ok()).collect());
                if let [x, y, z] = parts[..] {
                    let r = IVec3::new(x, y, z);
                    on_disk.insert((dim as u32, r));
                    // The header says which of its cubes are there.
                    let mut raw = vec![0u8; ENTRIES * 8];
                    if std::fs::File::open(e.path()).and_then(|mut f| f.read_exact(&mut raw)).is_ok() {
                        for i in 0..ENTRIES {
                            if raw[i * 8 + 4..i * 8 + 8] != [0; 4] {
                                let i = i as i32;
                                saved[dim].insert(r * REGION + IVec3::new(i % REGION, i / (REGION * REGION), i / REGION % REGION));
                            }
                        }
                    }
                }
            }
        }
        Storage { meta, ids, regions: FxHashMap::default(), dim_dirs, on_disk, saved }
    }

    fn region(&mut self, dim: u32, r: IVec3) -> Option<&mut Region> {
        if !self.regions.contains_key(&(dim, r)) {
            let path = self.dim_dirs[dim as usize].join(format!("r.{}.{}.{}.bin", r.x, r.y, r.z));
            match Region::open(&path) {
                Ok(reg) => {
                    if self.regions.len() > 64 {
                        self.regions.clear();
                    }
                    self.regions.insert((dim, r), reg);
                    self.on_disk.insert((dim, r));
                }
                Err(e) => {
                    log::error!("opening {}: {e}", path.display());
                    return None;
                }
            }
        }
        self.regions.get_mut(&(dim, r))
    }

    fn region_exists(&self, dim: u32, r: IVec3) -> bool {
        self.on_disk.contains(&(dim, r))
    }

    /// Cubes of dimension `dim` that were saved.
    pub fn saved(&self, dim: u32) -> &FxHashSet<IVec3> {
        &self.saved[dim as usize]
    }

    /// The cube and the serialized entities that were in it.
    pub fn load(&mut self, dim: u32, pos: IVec3) -> Option<(Cube, Vec<u8>)> {
        let (r, i) = region_of(pos);
        if !self.region_exists(dim, r) {
            return None;
        }
        let raw = self.region(dim, r)?.read(i)?;
        let data = lz4_flex::decompress_size_prepended(&raw).ok()?;
        let len = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let ids = &self.ids;
        let cube = Cube::read(&mut data.get(4..4 + len)?, |w| ids.runtime_id(w));
        if cube.is_none() {
            log::error!("corrupt cube {pos} in dimension {dim}, regenerating it");
        }
        Some((cube?, data[4 + len..].to_vec()))
    }

    pub fn save(&mut self, c: &Content, dim: u32, pos: IVec3, cube: &Cube, entities: &[u8]) {
        if !c.dims[dim as usize].saved {
            return;
        }
        let mut buf = Vec::new();
        let mut cube = cube.clone();
        cube.compact();
        let mut map: FxHashMap<u32, u32> = FxHashMap::default();
        for &s in cube.palette() {
            map.insert(s, self.ids.world_id(c, s));
        }
        buf.extend_from_slice(&[0; 4]);
        cube.write(&mut buf, |s| map[&s]);
        let len = (buf.len() - 4) as u32;
        buf[..4].copy_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(entities);
        let packed = lz4_flex::compress_prepend_size(&buf);
        let (r, i) = region_of(pos);
        if let Some(reg) = self.region(dim, r) {
            if let Err(e) = reg.write(i, &packed) {
                log::error!("saving cube {pos}: {e}");
            }
        }
        self.saved[dim as usize].insert(pos);
    }

    pub fn flush(&mut self) {
        self.ids.save(&self.meta.dir);
        self.meta.last_played = now_secs();
        self.meta.save();
        for r in self.regions.values_mut() {
            let _ = r.file.flush();
        }
    }

    pub fn player_path(&self, name: &str) -> PathBuf {
        let safe: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        self.meta.dir.join("players").join(format!("{safe}.txt"))
    }
}

// ---------------------------------------------------------------- screens

/// Asks before a world is deleted; deleting moves it to `saves/.trash` so a misclick is recoverable.
pub struct ConfirmDelete {
    world: WorldMeta,
}

impl Screen for ConfirmDelete {
    fn name(&self) -> &str {
        "core:confirm_delete"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let name = self.world.name.clone();
        frame(ui, cx, "world.delete.title", |ui| {
            label(ui, cx, "world.delete.question", &[Arg::Text(name)], "default");
            label(ui, cx, "world.delete.note", &[], "dim");
            ui.container(Style::row().gap(6.0), |ui| {
                let dir = self.world.dir.clone();
                button(ui, cx, "world.delete", &[], 145.0, move |c| {
                    if let (Some(parent), Some(leaf)) = (dir.parent(), dir.file_name()) {
                        let trash = parent.join(".trash");
                        let _ = std::fs::create_dir_all(&trash);
                        let target = trash.join(format!("{}-{}", leaf.to_string_lossy(), now_secs()));
                        if std::fs::rename(&dir, &target).is_err() {
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                    }
                    c.requests.push(Request::Close);
                });
                button(ui, cx, "menu.cancel", &[], 145.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}

/// Opening a world played with mods that are not loaded now: their blocks and items are lost and
/// players in their dimensions start at the spawn, so the player decides.
pub struct MissingMods {
    pub world: WorldMeta,
    pub missing: Vec<String>,
}

impl Screen for MissingMods {
    fn name(&self) -> &str {
        "core:missing_mods"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        frame(ui, cx, "world.missing_mods.title", |ui| {
            label(ui, cx, "world.missing_mods.question", &[Arg::Text(self.world.name.clone())], "default");
            for m in &self.missing {
                ui.text(text::plain(m), "button");
            }
            label(ui, cx, "world.missing_mods.note", &[], "dim");
            ui.container(Style::row().gap(6.0), |ui| {
                let world = self.world.clone();
                let go = button(ui, cx, "world.missing_mods.continue", &[], 145.0, move |c| c.requests.push(Request::StartWorldAnyway(world.clone())));
                ui.set_id(go, "continue");
                button(ui, cx, "menu.cancel", &[], 145.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}

#[derive(Default)]
pub struct WorldSelect {
    /// The world's folder: a deleted one is simply no longer there.
    selected: Option<PathBuf>,
}

impl Screen for WorldSelect {
    fn name(&self) -> &str {
        "core:worlds"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        let worlds = list(&cx.settings.saves_dir());
        let selected = worlds.iter().position(|w| self.selected.as_ref() == Some(&w.dir));
        frame(ui, cx, "world.select", |ui| {
            // The list takes whatever height is left and scrolls; the buttons below always show.
            let list = ui.panel("panel", Style::default().pad(8.0).gap(4.0).stretch().w(300.0).h(0.0).grow().min_h(60.0).scroll(), |ui| {
                if worlds.is_empty() {
                    label(ui, cx, "world.empty", &[], "dim");
                }
                for (i, w) in worlds.iter().enumerate() {
                    let when = format_age(now_secs().saturating_sub(w.last_played));
                    let info = cx.t("world.info", &[Arg::Text(w.dir.file_name().unwrap_or_default().to_string_lossy().into()), Arg::Text(when)]);
                    let (name, dir) = (w.name.clone(), w.dir.clone());
                    let b = ui.button("button", Style::default().pad(4.0).stretch().gap(2.0), Rc::new(move |c: &mut Ctx| {
                        if let Some(s) = c.screen::<WorldSelect>() {
                            s.selected = Some(dir.clone());
                        }
                        c.requests.push(Request::Rebuild);
                    }), |ui| {
                        ui.text(text::plain(&name), "button");
                        ui.text(info, "dim");
                    });
                    if let Kind::Button { selected: s, .. } = &mut ui.nodes[b].kind {
                        *s = selected == Some(i);
                    }
                    ui.set_id(b, &format!("world:{i}"));
                }
            });
            ui.set_id(list, "worlds");
            ui.container(Style::row().gap(6.0), |ui| {
                let sel = selected.and_then(|i| worlds.get(i)).cloned();
                let play = button(ui, cx, "world.play", &[], 145.0, move |c| {
                    if let Some(w) = &sel {
                        c.requests.push(Request::StartWorld(w.clone()));
                    }
                });
                if let Kind::Button { enabled, .. } = &mut ui.nodes[play].kind {
                    *enabled = selected.is_some();
                }
                ui.set_id(play, "play");
                button(ui, cx, "world.create", &[], 145.0, |c| c.requests.push(Request::Screen("core:create_world".into())));
            });
            ui.container(Style::row().gap(6.0), |ui| {
                let sel = selected.and_then(|i| worlds.get(i)).cloned();
                let del = button(ui, cx, "world.delete", &[], 145.0, move |c| {
                    if let Some(w) = &sel {
                        c.requests.push(Request::Open(Box::new(ConfirmDelete { world: w.clone() })));
                    }
                });
                if let Kind::Button { enabled, .. } = &mut ui.nodes[del].kind {
                    *enabled = selected.is_some();
                }
                let dir = cx.settings.run_dir.join("saves");
                let open = button(ui, cx, "world.folder", &[], 70.0, move |_| crate::packs::open_folder(&dir));
                ui.set_id(open, "saves_folder");
                button(ui, cx, "menu.back", &[], 70.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}

fn format_age(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub struct CreateWorld;

impl Screen for CreateWorld {
    fn name(&self) -> &str {
        "core:create_world"
    }

    fn build(&mut self, ui: &mut Ui, cx: &BuildCx) {
        frame(ui, cx, "world.create", |ui| {
            ui.panel("panel", Style::default().pad(8.0).gap(4.0).stretch().w(260.0), |ui| {
                label(ui, cx, "world.name", &[], "default");
                let name = ui.add(Kind::input("New World", 32), Style::default().h(18.0).stretch());
                ui.set_id(name, "name");
                label(ui, cx, "world.seed", &[], "default");
                let seed = ui.add(Kind::input("", 32), Style::default().h(18.0).stretch());
                ui.set_id(seed, "seed");
            });
            ui.container(Style::row().gap(6.0), |ui| {
                button(ui, cx, "world.create", &[], 120.0, |c| {
                    let name = c.input_text("name").unwrap_or_default();
                    let seed = parse_seed(&c.input_text("seed").unwrap_or_default());
                    let name = if name.trim().is_empty() { "World".to_string() } else { name.trim().to_string() };
                    c.requests.push(Request::StartWorldAnyway(create(&c.settings.saves_dir(), &name, seed)));
                });
                button(ui, cx, "menu.cancel", &[], 120.0, |c| c.requests.push(Request::Close));
            });
        });
    }
}
