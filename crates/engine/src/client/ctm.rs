//! Connected and varied block textures. A `<texture>.ctm` file next to a block texture (in a mod
//! or a resource pack) holds rules for every cube face showing that texture, on any block and any
//! face (a log side is just its side texture); methods and keys follow
//! OptiFine/MCPatcher CTM so existing packs port with little more than renaming files:
//!
//! ```text
//! method = compact       # ctm (47 tiles), compact (5), horizontal, vertical, h+v, v+h (4/4/7/7),
//!                        # top (1), random, repeat, fixed (1)
//! tiles = glass/0-4      # relative to this file; ranges expand, <default> is the plain texture
//! connect = tile         # tile (default: neighbors showing this texture on that face, whatever
//!                        # block they are), block (same block), state (same block state)
//! faces = sides          # all, top, bottom, sides, north, south, east, west
//! inner_seams = true     # covered neighbors do not connect
//! weights = 8 1 1        # random
//! symmetry = opposite    # random: none, opposite (pairs opposite faces), all
//! width = 2              # repeat
//! height = 2
//! blocks = base:stone #ores/*
//! heights = 0-40 100-    # world y ranges
//! biomes = !base:desert #cold
//!
//! [another rule]         # starts a further rule; the first one whose filters pass wins
//! ```
//!
//! Resolving is a texture id swap at mesh time, so records and shaders are unchanged. Compact sets
//! are expanded to all 47 tiles when the block textures are listed.

use std::sync::{Arc, RwLock};

use rustc_hash::FxHashMap;

use super::mesher::{Neighborhood, pidx};
use crate::assets::Assets;
use crate::content::Content;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Method {
    Ctm,
    Horizontal,
    Vertical,
    HorizontalVertical,
    VerticalHorizontal,
    Top,
    Random,
    Repeat,
    Fixed,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Connect {
    Block,
    Tile,
    State,
}

#[derive(Debug)]
struct Rule {
    method: Method,
    connect: Connect,
    /// Bit per face direction.
    faces: u8,
    inner_seams: bool,
    blocks: Vec<u32>,
    heights: Vec<(i32, i32)>,
    biomes: Option<(bool, [u64; 4])>,
    tiles: Vec<u32>,
    /// Random: tile index for the top byte of the position hash.
    pick: Vec<u8>,
    /// Random: face groups sharing a hash (1 none, 2 opposite, 6 all).
    symmetry: u8,
    size: (i32, i32),
}

#[derive(Default)]
pub struct Ctm {
    /// Per texture id: range in `rules`.
    by_texture: Vec<(u32, u32)>,
    rules: Vec<Rule>,
    /// Per texture id: its `<name>_overlay` texture id + 1, or 0. The overlay is drawn over the
    /// face and takes the block's tint instead of the face (vanilla's grass sides).
    pub overlay: Vec<u32>,
}

/// Textures needed on top of the content's block textures.
#[derive(Default)]
pub struct Extra {
    /// Tile textures, ids continue after the content's block textures.
    pub names: Vec<String>,
    /// Compact sets expanded to 47 tiles: per tile, the source texture id of each quadrant
    /// (top-left, top-right, bottom-left, bottom-right). Ids continue after `names`.
    pub composites: Vec<[u32; 4]>,
}

static CURRENT: RwLock<Option<Arc<Ctm>>> = RwLock::new(None);

pub fn current() -> Arc<Ctm> {
    CURRENT.read().unwrap().clone().unwrap_or_default()
}

pub fn set(ctm: Ctm) {
    *CURRENT.write().unwrap() = Some(Arc::new(ctm));
}

/// MCPatcher's neighbor bits (L 1, DL 2, D 4, DR 8, R 16, UR 32, U 64, UL 128) to the 47-tile
/// template index.
const CTM47: [u8; 256] = [
    0, 3, 0, 3, 12, 5, 12, 15, 0, 3, 0, 3, 12, 5, 12, 15, 1, 2, 1, 2, 4, 7, 4, 29, 1, 2, 1, 2, 13, 31, 13, 14, //
    0, 3, 0, 3, 12, 5, 12, 15, 0, 3, 0, 3, 12, 5, 12, 15, 1, 2, 1, 2, 4, 7, 4, 29, 1, 2, 1, 2, 13, 31, 13, 14, //
    36, 17, 36, 17, 24, 19, 24, 43, 36, 17, 36, 17, 24, 19, 24, 43, 16, 18, 16, 18, 6, 46, 6, 21, 16, 18, 16, 18, 28, 9, 28, 22, //
    36, 17, 36, 17, 24, 19, 24, 43, 36, 17, 36, 17, 24, 19, 24, 43, 37, 40, 37, 40, 30, 8, 30, 34, 37, 40, 37, 40, 25, 23, 25, 45, //
    0, 3, 0, 3, 12, 5, 12, 15, 0, 3, 0, 3, 12, 5, 12, 15, 1, 2, 1, 2, 4, 7, 4, 29, 1, 2, 1, 2, 13, 31, 13, 14, //
    0, 3, 0, 3, 12, 5, 12, 15, 0, 3, 0, 3, 12, 5, 12, 15, 1, 2, 1, 2, 4, 7, 4, 29, 1, 2, 1, 2, 13, 31, 13, 14, //
    36, 39, 36, 39, 24, 41, 24, 27, 36, 39, 36, 39, 24, 41, 24, 27, 16, 42, 16, 42, 6, 20, 6, 10, 16, 42, 16, 42, 28, 35, 28, 44, //
    36, 39, 36, 39, 24, 41, 24, 27, 36, 39, 36, 39, 24, 41, 24, 27, 37, 38, 37, 38, 30, 11, 30, 32, 37, 38, 37, 38, 25, 33, 25, 26,
];

/// Compact tile per quadrant (TL, TR, BL, BR) of each 47 template tile: 0 isolated, 1 connected
/// all round, 2 connected up/down, 3 connected left/right, 4 inner corner. Tile 41 differs from
/// Angelica's generator, which repeats tile 27 there and loses the inner corner.
const COMPACT: [[u8; 4]; 47] = [
    [0, 0, 0, 0], [0, 3, 0, 3], [3, 3, 3, 3], [3, 0, 3, 0], [0, 3, 2, 4], [3, 0, 4, 2], [2, 4, 2, 4], [3, 3, 4, 4],
    [4, 1, 4, 4], [4, 4, 4, 1], [1, 4, 1, 4], [1, 1, 4, 4], [0, 0, 2, 2], [0, 3, 2, 1], [3, 3, 1, 1], [3, 0, 1, 2],
    [2, 4, 0, 3], [4, 2, 3, 0], [4, 4, 3, 3], [4, 2, 4, 2], [1, 4, 4, 4], [4, 4, 1, 4], [4, 4, 1, 1], [4, 1, 4, 1],
    [2, 2, 2, 2], [2, 1, 2, 1], [1, 1, 1, 1], [1, 2, 1, 2], [2, 4, 2, 1], [3, 3, 1, 4], [2, 1, 2, 4], [3, 3, 4, 1],
    [1, 1, 1, 4], [1, 1, 4, 1], [4, 1, 1, 4], [1, 4, 4, 1], [2, 2, 0, 0], [2, 1, 0, 3], [1, 1, 3, 3], [1, 2, 3, 0],
    [4, 1, 3, 3], [1, 2, 4, 2], [1, 4, 3, 3], [4, 2, 1, 2], [1, 4, 1, 1], [4, 1, 1, 1], [4, 4, 4, 4],
];

/// Texture u (right) and up directions of each face, matching the corner order in the mesher.
const AXES: [([i32; 3], [i32; 3]); 6] = [
    ([0, 0, 1], [0, 1, 0]),
    ([0, 0, -1], [0, 1, 0]),
    ([1, 0, 0], [0, 0, 1]),
    ([1, 0, 0], [0, 0, -1]),
    ([-1, 0, 0], [0, 1, 0]),
    ([1, 0, 0], [0, 1, 0]),
];

const NORMALS: [[i32; 3]; 6] = [[-1, 0, 0], [1, 0, 0], [0, -1, 0], [0, 1, 0], [0, 0, -1], [0, 0, 1]];

fn faces(v: &str) -> u8 {
    v.split_whitespace().fold(0, |m, f| {
        m | match f {
            "all" => 0b111111,
            "bottom" => 1 << 2,
            "top" => 1 << 3,
            "sides" => 0b110011,
            "west" => 1 << 0,
            "east" => 1 << 1,
            "north" => 1 << 4,
            "south" => 1 << 5,
            _ => 0,
        }
    })
}

fn ranges(v: &str) -> Vec<(i32, i32)> {
    v.split_whitespace()
        .filter_map(|r| match r.split_once('-') {
            Some((a, b)) => Some((a.parse().unwrap_or(i32::MIN), b.parse().unwrap_or(i32::MAX))),
            None => r.parse().ok().map(|y| (y, y)),
        })
        .collect()
}

/// Tile names relative to the folder of `texture`; `a-b` at the end of an entry expands to numbers.
fn tile_names(v: &str, texture: &str) -> Vec<Option<String>> {
    let dir = match texture.rfind('/') {
        Some(i) => texture[..=i].to_string(),
        None => texture.split_once(':').map_or(String::new(), |(ns, _)| format!("{ns}:")),
    };
    let full = |rel: &str| if rel.contains(':') { rel.to_string() } else { format!("{dir}{rel}") };
    let mut out = Vec::new();
    for t in v.split_whitespace() {
        if t == "<default>" {
            out.push(None);
            continue;
        }
        let (prefix, last) = t.rsplit_once('/').map_or(("", t), |(p, l)| (p, l));
        let prefix = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
        match last.split_once('-').and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))) {
            Some((a, b)) => out.extend((a..=b).map(|i| Some(full(&format!("{prefix}{i}"))))),
            None => out.push(Some(full(t))),
        }
    }
    out
}

/// Reads the `.ctm` files of all block textures. Tile textures get ids after the content's.
pub fn load(assets: &Assets, c: &Content) -> (Ctm, Extra) {
    let base = c.textures.len() as u32;
    let mut extra = Extra::default();
    let mut ids: FxHashMap<String, u32> = c.textures.iter().enumerate().map(|(i, n)| (n.clone(), i as u32)).collect();
    let mut ctm = Ctm { by_texture: vec![(0, 0); c.textures.len()], rules: Vec::new(), overlay: vec![0; c.textures.len()] };
    for (tex, name) in c.textures.iter().enumerate() {
        let over = format!("{name}_overlay");
        if assets.path(&over, "textures", "png").is_some() {
            extra.names.push(over);
            ctm.overlay[tex] = base + extra.names.len() as u32;
        }
    }
    // Compact sets waiting for their composite ids, which come after every tile name.
    let mut compact: Vec<(usize, Vec<u32>)> = Vec::new();
    for (tex, name) in c.textures.iter().enumerate() {
        let Some(src) = assets.read(name, "textures", "ctm").map(|b| String::from_utf8_lossy(&b).into_owned()) else { continue };
        let start = ctm.rules.len() as u32;
        for section in crate::kv::sections(&src) {
            let kv: FxHashMap<&str, &str> = section.iter().map(|p| (p.1, p.2)).collect();
            if kv.is_empty() {
                continue;
            }
            let method = match kv.get("method").copied().unwrap_or("ctm") {
                "ctm" | "glass" | "default" => Method::Ctm,
                "compact" | "ctm_compact" => Method::Ctm,
                "horizontal" | "bookshelf" => Method::Horizontal,
                "vertical" => Method::Vertical,
                "h+v" | "horizontal+vertical" => Method::HorizontalVertical,
                "v+h" | "vertical+horizontal" => Method::VerticalHorizontal,
                "top" | "sandstone" => Method::Top,
                "random" => Method::Random,
                "repeat" | "pattern" => Method::Repeat,
                "fixed" | "static" => Method::Fixed,
                m => {
                    log::warn!("{name}.ctm: unknown method {m}");
                    continue;
                }
            };
            let is_compact = matches!(kv.get("method").copied(), Some("compact" | "ctm_compact"));
            let mut tiles: Vec<u32> = tile_names(kv.get("tiles").copied().unwrap_or(""), name)
                .into_iter()
                .map(|t| match t {
                    None => tex as u32,
                    Some(n) => *ids.entry(n.clone()).or_insert_with(|| {
                        extra.names.push(n);
                        base + extra.names.len() as u32 - 1
                    }),
                })
                .collect();
            let needed = match method {
                Method::Ctm if is_compact => 5,
                Method::Ctm => 47,
                Method::Horizontal | Method::Vertical => 4,
                Method::HorizontalVertical | Method::VerticalHorizontal => 7,
                Method::Repeat => {
                    let w = kv.get("width").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1);
                    let h = kv.get("height").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1);
                    w * h
                }
                _ => 1,
            };
            if tiles.len() < needed {
                log::warn!("{name}.ctm: {:?} needs {needed} tiles, has {}", method, tiles.len());
                continue;
            }
            let rule_index = ctm.rules.len();
            if is_compact {
                compact.push((rule_index, tiles[..5].to_vec()));
                tiles.clear();
            }
            let pick = if method == Method::Random {
                let given: Vec<u32> = kv.get("weights").map_or(Vec::new(), |w| w.split_whitespace().filter_map(|x| x.parse().ok()).collect());
                let avg = if given.is_empty() { 1 } else { (given.iter().sum::<u32>() / given.len() as u32).max(1) };
                let weights: Vec<u32> = (0..tiles.len()).map(|i| given.get(i).copied().unwrap_or(avg)).collect();
                let total: u32 = weights.iter().sum::<u32>().max(1);
                let mut lut = Vec::with_capacity(256);
                let (mut acc, mut i) = (0u32, 0usize);
                for slot in 0..256u32 {
                    while i + 1 < weights.len() && (slot * total) / 256 >= acc + weights[i] {
                        acc += weights[i];
                        i += 1;
                    }
                    lut.push(i as u8);
                }
                lut
            } else {
                Vec::new()
            };
            // References like anywhere else: names, `name[prop=value]`, `#tag`, `#pattern/*`.
            let mut blocks = Vec::new();
            for b in kv.get("blocks").map_or("", |v| *v).split_whitespace() {
                match crate::refs::Ref::parse(b).and_then(|r| crate::refs::blocks(&r, &c.block_names, &c.blocks, &c.components, c.states.len() as u32)) {
                    Ok(list) => blocks.extend(list),
                    Err(e) => log::error!("{name}.ctm: blocks: {e}"),
                }
            }
            let biomes = kv.get("biomes").map(|v| {
                let (invert, list) = v.strip_prefix('!').map_or((false, *v), |r| (true, r));
                let mut set = [0u64; 4];
                for b in list.split_whitespace() {
                    match crate::refs::Ref::parse(b).and_then(|r| crate::refs::biomes(&r, &c.biome_names, &c.components, c.biomes.len() as u32)) {
                        Ok(list) => list.into_iter().filter(|&i| i < 256).for_each(|i| set[i as usize / 64] |= 1 << (i % 64)),
                        Err(e) => log::error!("{name}.ctm: biomes: {e}"),
                    }
                }
                (invert, set)
            });
            ctm.rules.push(Rule {
                method,
                connect: match kv.get("connect").copied() {
                    Some("block") => Connect::Block,
                    Some("state") => Connect::State,
                    _ => Connect::Tile,
                },
                faces: kv.get("faces").map_or(0b111111, |v| faces(v)),
                inner_seams: kv.get("inner_seams").is_some_and(|v| *v == "true"),
                blocks,
                heights: kv.get("heights").map_or(Vec::new(), |v| ranges(v)),
                biomes,
                tiles,
                pick,
                symmetry: match kv.get("symmetry").copied() {
                    Some("opposite") => 2,
                    Some("all") => 6,
                    _ => 1,
                },
                size: (
                    kv.get("width").and_then(|v| v.parse().ok()).unwrap_or(1).max(1),
                    kv.get("height").and_then(|v| v.parse().ok()).unwrap_or(1).max(1),
                ),
            });
        }
        ctm.by_texture[tex] = (start, ctm.rules.len() as u32 - start);
    }
    let first_composite = base + extra.names.len() as u32;
    for (rule, src) in compact {
        let start = first_composite + extra.composites.len() as u32;
        extra.composites.extend(COMPACT.iter().map(|q| q.map(|code| src[code as usize])));
        ctm.rules[rule].tiles = (start..start + 47).collect();
    }
    if !ctm.rules.is_empty() {
        log::info!("connected textures: {} rules, {} tiles, {} generated", ctm.rules.len(), extra.names.len(), extra.composites.len());
    }
    (ctm, extra)
}

fn hash(x: i32, y: i32, z: i32, f: u32) -> u64 {
    loom_noise::hash(f as u64, x as i64, y as i64, z as i64)
}

impl Ctm {
    #[inline]
    pub fn has_overlay(&self, tex: u32) -> bool {
        self.overlay.get(tex as usize).is_some_and(|&o| o != 0)
    }

    /// Texture for face `dir` of the block at `(x, y, z)` in the cube, `tex` being its plain one.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(&self, c: &Content, n: &Neighborhood, x: i32, y: i32, z: i32, dir: usize, tex: u32) -> u32 {
        match self.by_texture.get(tex as usize) {
            Some(&(start, len)) if len > 0 => self.resolve_rules(c, n, x, y, z, dir, tex, start as usize..(start + len) as usize),
            _ => tex,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_rules(&self, c: &Content, n: &Neighborhood, x: i32, y: i32, z: i32, dir: usize, tex: u32, rules: std::ops::Range<usize>) -> u32 {
        let s = n.states[pidx(x, y, z)];
        let info = &c.states[s as usize];
        let world = [n.origin[0] + x, n.origin[1] + y, n.origin[2] + z];
        let (r, u) = AXES[dir];
        let nrm = NORMALS[dir];
        let at = |a: i32, b: i32, extra: [i32; 3]| {
            pidx(x + r[0] * a + u[0] * b + extra[0], y + r[1] * a + u[1] * b + extra[1], z + r[2] * a + u[2] * b + extra[2])
        };
        for rule in &self.rules[rules] {
            if rule.faces & (1 << dir) == 0 || (!rule.blocks.is_empty() && !rule.blocks.contains(&info.block)) {
                continue;
            }
            if !rule.heights.is_empty() && !rule.heights.iter().any(|&(lo, hi)| world[1] >= lo && world[1] <= hi) {
                continue;
            }
            if let Some((invert, set)) = rule.biomes {
                let b = n.biomes[(z * 16 + x) as usize] as usize;
                if (set[b / 64] >> (b % 64) & 1 == 1) == invert {
                    continue;
                }
            }
            let connects = |a: i32, b: i32| -> bool {
                let ns = n.states[at(a, b, [0; 3])];
                let same = match rule.connect {
                    Connect::Block => c.states[ns as usize].block == info.block,
                    Connect::State => ns == s,
                    Connect::Tile => c.states[ns as usize].textures[dir] == tex,
                };
                // With inner seams a neighbor whose own face here is covered breaks the run.
                same && !(rule.inner_seams && {
                    let front = n.states[at(a, b, nrm)];
                    front != 0 && c.states[front as usize].flags & loom_abi::block_flags::OPAQUE != 0
                })
            };
            let tile = match rule.method {
                Method::Ctm => {
                    let mut bits = 0usize;
                    for (bit, (a, b)) in [(-1, 0), (-1, -1), (0, -1), (1, -1), (1, 0), (1, 1), (0, 1), (-1, 1)].into_iter().enumerate() {
                        if connects(a, b) {
                            bits |= 1 << bit;
                        }
                    }
                    CTM47[bits] as usize
                }
                Method::Horizontal | Method::HorizontalVertical => {
                    let h = [3, 2, 0, 1][connects(-1, 0) as usize | (connects(1, 0) as usize) << 1];
                    if h != 3 || rule.method == Method::Horizontal {
                        h
                    } else {
                        let down = connects(0, -1) && !connects(-1, -1) && !connects(1, -1);
                        let up = connects(0, 1) && !connects(-1, 1) && !connects(1, 1);
                        match (down, up) {
                            (false, true) => 4,
                            (true, true) => 5,
                            (true, false) => 6,
                            _ => 3,
                        }
                    }
                }
                Method::Vertical | Method::VerticalHorizontal => {
                    let v = [3, 2, 0, 1][connects(0, -1) as usize | (connects(0, 1) as usize) << 1];
                    if v != 3 || rule.method == Method::Vertical {
                        v
                    } else {
                        let left = connects(-1, 0) && !connects(-1, 1) && !connects(-1, -1);
                        let right = connects(1, 0) && !connects(1, 1) && !connects(1, -1);
                        match (left, right) {
                            (false, true) => 4,
                            (true, true) => 5,
                            (true, false) => 6,
                            _ => 3,
                        }
                    }
                }
                Method::Top => {
                    let above = n.states[pidx(x, y + 1, z)];
                    if dir == 2 || dir == 3 || c.states[above as usize].block != info.block {
                        continue;
                    }
                    0
                }
                Method::Random => {
                    let group = (dir as u32) / rule.symmetry as u32;
                    rule.pick[(hash(world[0], world[1], world[2], group) >> 56) as usize] as usize
                }
                Method::Repeat => {
                    let dot = |a: [i32; 3]| world[0] * a[0] + world[1] * a[1] + world[2] * a[2];
                    let (w, h) = rule.size;
                    (w * (-dot(u)).rem_euclid(h) + dot(r).rem_euclid(w)) as usize
                }
                Method::Fixed => 0,
            };
            return rule.tiles.get(tile).copied().unwrap_or(tex);
        }
        tex
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axes_match_mesher_corners() {
        // u runs from corner 0 to 1 and up from corner 0 to 3 (see mesher::CORNERS).
        for dir in 0..6 {
            let c = super::super::mesher::corners(dir);
            let sub = |a: [i32; 3], b: [i32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
            assert_eq!(sub(c[1], c[0]), AXES[dir].0, "u of face {dir}");
            assert_eq!(sub(c[3], c[0]), AXES[dir].1, "up of face {dir}");
        }
    }

    #[test]
    fn compact_table_agrees_with_quadrant_rule() {
        // Each quadrant of a 47 tile follows from its two edges and diagonal.
        for bits in 0..256usize {
            let has = |b: usize| bits & b != 0;
            let quad = |side_h: usize, side_v: usize, diag: usize| match (has(side_h), has(side_v)) {
                (true, true) if has(diag) => 1,
                (true, true) => 4,
                (true, false) => 3,
                (false, true) => 2,
                _ => 0,
            };
            let expect = [quad(1, 64, 128), quad(16, 64, 32), quad(1, 4, 2), quad(16, 4, 8)];
            assert_eq!(COMPACT[CTM47[bits] as usize], expect, "bits {bits:08b}");
        }
    }

    #[test]
    fn tile_ranges_expand() {
        let t = tile_names("glass/0-2 <default> other:x/y", "base:block/glass");
        assert_eq!(t, vec![Some("base:block/glass/0".into()), Some("base:block/glass/1".into()), Some("base:block/glass/2".into()), None, Some("other:x/y".into())]);
    }
}
