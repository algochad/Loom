//! Terrain described as data (`Host::reg_terrain`): a base height plus height features, biome
//! rules that pick each column's biome and surface, water and caves. The dimension's mod
//! describes it, other mods edit it, and the merged result is evaluated here, so structures,
//! decorations and spawn search all read the same columns from one cache.

use std::sync::Arc;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use loom_abi::{self as abi, Callback, ModId, TerrainColumn, rule_field as rf, terrain_op as op};
use loom_noise::fixed::{Fields, ONE, div_floor, hash, perlin2, perlin3, q4, q12, ramp, salt, seed32, shape};

use crate::host;

/// One `TerrainDesc` as sent, owned.
pub struct RawTerrain {
    pub owner: ModId,
    pub dimension: String,
    pub op: u32,
    pub anchor: String,
    pub block: String,
    pub level: i32,
    pub to: i32,
    pub value: f64,
    pub size: f64,
    pub feature: RawFeature,
    pub rule: RawRule,
}

#[derive(Clone)]
pub struct RawFeature {
    pub owner: ModId,
    pub name: String,
    pub preset: u32,
    pub params: [f64; 4],
    pub height: Callback,
    pub mask: Vec<abi::MaskStep>,
}

#[derive(Clone)]
pub struct RawRule {
    pub name: String,
    pub fields: u32,
    pub biome: String,
    pub temperature: (f64, f64),
    pub humidity: (f64, f64),
    pub continent: (f64, f64),
    pub height: (i32, i32),
    pub feature: (String, f64, f64),
    pub top: String,
    pub filler: String,
    pub depth: i32,
    pub under: (String, i32),
    pub deep_top: (String, i32),
    pub bare_above: i32,
    pub ice: String,
}

impl RawRule {
    /// The fields of `edit` named in its `fields` replace this rule's.
    fn apply(&mut self, edit: &RawRule) {
        let has = |f: u32| edit.fields & f != 0;
        if has(rf::BIOME) {
            self.biome = edit.biome.clone();
        }
        if has(rf::TEMPERATURE) {
            self.temperature = edit.temperature;
        }
        if has(rf::HUMIDITY) {
            self.humidity = edit.humidity;
        }
        if has(rf::CONTINENT) {
            self.continent = edit.continent;
        }
        if has(rf::HEIGHT) {
            self.height = edit.height;
        }
        if has(rf::FEATURE) {
            self.feature = edit.feature.clone();
        }
        if has(rf::SURFACE) {
            (self.top, self.filler, self.depth) = (edit.top.clone(), edit.filler.clone(), edit.depth);
        }
        if has(rf::UNDER) {
            self.under = edit.under.clone();
        }
        if has(rf::DEEP_TOP) {
            self.deep_top = edit.deep_top.clone();
        }
        if has(rf::BARE_ABOVE) {
            self.bare_above = edit.bare_above;
        }
        if has(rf::FROZEN) {
            self.ice = edit.ice.clone();
        }
    }
}

/// # Safety
/// The strings in `r` must be valid for the call.
pub unsafe fn copy_rule(r: &abi::BiomeRuleDesc) -> RawRule {
    unsafe {
        let s = |s: abi::Str| s.as_str().to_string();
        RawRule {
            name: s(r.name),
            fields: r.fields,
            biome: s(r.biome),
            temperature: (r.temperature[0], r.temperature[1]),
            humidity: (r.humidity[0], r.humidity[1]),
            continent: (r.continent[0], r.continent[1]),
            height: (r.height[0], r.height[1]),
            feature: (s(r.feature), r.feature_range[0], r.feature_range[1]),
            top: s(r.top),
            filler: s(r.filler),
            depth: r.depth,
            under: (s(r.under), r.under_layers),
            deep_top: (s(r.deep_top), r.deep_below),
            bare_above: r.bare_above,
            ice: s(r.ice),
        }
    }
}

/// # Safety
/// The pointers in `d` must be valid for the call.
pub unsafe fn copy(owner: ModId, d: &abi::TerrainDesc) -> RawTerrain {
    unsafe {
        let s = |s: abi::Str| s.as_str().to_string();
        let f = &d.feature;
        RawTerrain {
            owner,
            dimension: s(d.dimension),
            op: d.op,
            anchor: s(d.anchor),
            block: s(d.block),
            level: d.level,
            to: d.to,
            value: d.value,
            size: d.size,
            feature: RawFeature { owner, name: s(f.name), preset: f.preset, params: f.params, height: f.height, mask: f.mask.as_slice().to_vec() },
            rule: copy_rule(&d.rule),
        }
    }
}

/// Where a feature applies, 0..1 (Q12; scales Q4, see `loom_noise::fixed`).
#[derive(Clone, Debug)]
pub enum Mask {
    Noise { salt: u64, scale: i32, octaves: u32, from: i32, to: i32 },
    Continent(i32, i32),
    Temperature(i32, i32),
    Humidity(i32, i32),
    All(Vec<Mask>),
    Not(Box<Mask>),
}

impl Mask {
    /// From postfix steps; `None` for an empty or malformed list.
    fn from_steps(steps: &[abi::MaskStep]) -> Option<Mask> {
        use abi::terrain_mask as k;
        let mut stack: Vec<Mask> = Vec::new();
        for s in steps {
            let m = match s.kind {
                k::NOISE => Mask::Noise { salt: s.salt, scale: q4(s.scale), octaves: s.octaves, from: q12(s.from), to: q12(s.to) },
                k::CONTINENT => Mask::Continent(q12(s.from), q12(s.to)),
                k::TEMPERATURE => Mask::Temperature(q12(s.from), q12(s.to)),
                k::HUMIDITY => Mask::Humidity(q12(s.from), q12(s.to)),
                k::ALL => {
                    let at = stack.len().checked_sub(s.count as usize)?;
                    Mask::All(stack.split_off(at))
                }
                k::NOT => Mask::Not(Box::new(stack.pop()?)),
                _ => return None,
            };
            stack.push(m);
        }
        (stack.len() == 1).then(|| stack.pop()).flatten()
    }

    /// At (`x`, `z`) in Q4.
    pub fn value(&self, f: &Fields, x: i32, z: i32) -> i32 {
        match self {
            Mask::Noise { salt, scale, octaves, from, to } => ramp(f.fbm(*salt, x, z, *scale, *octaves), *from, *to),
            Mask::Continent(a, b) => ramp(f.continent(x, z), *a, *b),
            Mask::Temperature(a, b) => ramp(f.temperature(x, z), *a, *b),
            Mask::Humidity(a, b) => ramp(f.humidity(x, z), *a, *b),
            Mask::All(v) => v.iter().fold(ONE, |acc, m| (acc * m.value(f, x, z)) >> 12),
            Mask::Not(m) => ONE - m.value(f, x, z),
        }
    }
}

pub enum Height {
    /// `abi::terrain_preset` and its parameters (Q4).
    Preset(u32, [i32; 4]),
    Custom(ModId, Callback),
}

pub struct Feature {
    pub name: String,
    pub height: Height,
    pub mask: Option<Mask>,
}

impl Feature {
    /// Height (Q4) at (`x`, `z`) (Q4).
    fn height(&self, f: &Fields, x: i32, z: i32) -> i32 {
        use abi::terrain_preset as p;
        match self.height {
            Height::Preset(kind, [a, b, c, _]) => match kind {
                p::CONTINENTS => shape::continents(f, x, z, a),
                p::HILLS => shape::hills(f, x, z, a, b),
                p::MOUNTAINS => shape::mountains(f, x, z, a, b),
                p::PLATEAUS => shape::plateaus(f, x, z, a, b, c),
                p::CANYONS => shape::canyons(f, x, z, a, b),
                p::DUNES => shape::dunes(f, x, z, a, b),
                p::CRATERS => shape::craters(f, x, z, a, b, c),
                _ => 0,
            },
            Height::Custom(owner, cb) => {
                let mut args = abi::FeatureArgs { seed: f.seed, x, z, height: 0 };
                host::invoke(owner, cb, &mut args);
                args.height
            }
        }
    }

    /// Its height (Q4) at (`x`, `z`) (Q4).
    pub fn height_at(&self, f: &Fields, x: i32, z: i32) -> i32 {
        self.height(f, x, z)
    }
}

/// A biome rule with names resolved: block states and biome ids.
pub struct Rule {
    pub biome: u32,
    /// Ranges in Q12.
    pub temperature: (i32, i32),
    pub humidity: (i32, i32),
    pub continent: (i32, i32),
    pub height: (i32, i32),
    /// Index into `Terrain::features`, `usize::MAX` for a feature that does not exist, and
    /// the range of its strength (Q12).
    pub feature: Option<(usize, i32, i32)>,
    pub top: Option<u32>,
    pub filler: Option<u32>,
    pub depth: i32,
    pub under: Option<(u32, i32)>,
    pub deep_top: Option<(u32, i32)>,
    pub bare_above: Option<i32>,
    pub ice: Option<u32>,
}

pub struct Caves {
    pub lava: u32,
    pub lava_from: i32,
    pub lava_to: i32,
}

pub struct CaveBiome {
    pub biome: u32,
    pub depth: i32,
    /// Q4, Q12.
    pub size: i32,
    pub threshold: i32,
}

/// The columns of one column of cubes.
pub struct Columns {
    pub cols: [TerrainColumn; 256],
    pub rules: [u32; 256],
}

pub struct Terrain {
    pub stone: u32,
    /// Q4.
    pub base: i32,
    pub sea: Option<(i32, u32)>,
    pub features: Vec<Feature>,
    pub rules: Vec<Rule>,
    pub caves: Option<Caves>,
    /// Biomes of cubes deep underground.
    pub cave_biomes: Vec<CaveBiome>,
    cache: ColumnCache<Columns>,
    /// Column heights every 4 blocks over a cube's columns and its +X/+Z edge (5 x 5): caves
    /// measure depth from this smoothed surface, so steep slopes do not leave single-column
    /// pillars.
    corners: ColumnCache<[i32; 25]>,
}

/// The terrain being merged from the owner's description and the edits, still by name.
struct Draft {
    stone: Option<String>,
    base: f64,
    sea: Option<(i32, String)>,
    caves: Option<(String, i32, i32)>,
    features: Vec<RawFeature>,
    rules: Vec<RawRule>,
    cave_biomes: Vec<(String, i32, f64, f64)>,
}

/// Looks up content by full name while terrains are built.
pub struct Names<'a> {
    /// Name to first block state.
    pub block: &'a dyn Fn(&str) -> Option<u32>,
    pub biome: &'a dyn Fn(&str) -> Option<u32>,
    /// A biome's own climate and surface, which its rules start from.
    pub biome_rule: &'a dyn Fn(&str) -> Option<&'a RawRule>,
}

/// Merges `ops` (the owner's first, then the other mods' in load order) into a terrain.
/// Missing names are `errors`; an edit whose target is gone is skipped with a warning.
pub fn build(dimension: &str, owner_name: &dyn Fn(ModId) -> String, ops: Vec<RawTerrain>, names: &Names, errors: &mut Vec<String>) -> Terrain {
    let mut d = Draft { stone: None, base: 64.0, sea: None, caves: None, features: Vec::new(), rules: Vec::new(), cave_biomes: Vec::new() };
    let rule_name = |r: &RawRule| if r.name.is_empty() { r.biome.clone() } else { r.name.clone() };
    for o in ops {
        let who = owner_name(o.owner);
        let missing = |what: &str| log::warn!("{who}: the terrain of {dimension} has no {what} {}, edit skipped", o.anchor);
        let new_rule = |rules: &[RawRule], r: &RawRule, errors: &mut Vec<String>| {
            let n = rule_name(r);
            if rules.iter().any(|x| rule_name(x) == n) {
                errors.push(format!("{who}: the terrain of {dimension} already has a biome rule named {n} (name the new one, or use replace_biome)"));
                return false;
            }
            true
        };
        let rule_at = |rules: &[RawRule]| rules.iter().position(|r| rule_name(r) == o.anchor);
        match o.op {
            op::STONE => d.stone = Some(o.block),
            op::BASE_HEIGHT => d.base = o.value,
            op::SEA => d.sea = Some((o.level, o.block)),
            op::CAVES => d.caves = Some((o.block, o.level, o.to)),
            op::FEATURE => {
                if !o.feature.name.is_empty() && d.features.iter().any(|f| f.name == o.feature.name) {
                    errors.push(format!("{who}: the terrain of {dimension} already has a feature named {}", o.feature.name));
                } else {
                    d.features.push(o.feature);
                }
            }
            op::REMOVE_FEATURE => match d.features.iter().position(|f| f.name == o.anchor) {
                Some(i) => drop(d.features.remove(i)),
                None => missing("feature"),
            },
            op::MASK_FEATURE => match d.features.iter_mut().find(|f| f.name == o.anchor) {
                Some(f) if f.mask.is_empty() => f.mask = o.feature.mask,
                Some(f) => {
                    f.mask.extend(o.feature.mask);
                    f.mask.push(abi::MaskStep { kind: abi::terrain_mask::ALL, count: 2, ..abi::MaskStep::DEFAULT });
                }
                None => missing("feature"),
            },
            op::BIOME => {
                if new_rule(&d.rules, &o.rule, errors) {
                    d.rules.push(o.rule);
                }
            }
            op::BIOME_BEFORE | op::BIOME_AFTER => match rule_at(&d.rules) {
                Some(i) if new_rule(&d.rules, &o.rule, errors) => d.rules.insert(i + (o.op == op::BIOME_AFTER) as usize, o.rule),
                Some(_) => {}
                None => missing("biome rule"),
            },
            op::REPLACE_BIOME => match rule_at(&d.rules) {
                Some(i) => {
                    let old = d.rules.remove(i);
                    if new_rule(&d.rules, &o.rule, errors) {
                        d.rules.insert(i, o.rule);
                    } else {
                        d.rules.insert(i, old);
                    }
                }
                None => missing("biome rule"),
            },
            op::REMOVE_BIOME => match rule_at(&d.rules) {
                Some(i) => drop(d.rules.remove(i)),
                None => missing("biome rule"),
            },
            op::EDIT_BIOME => match rule_at(&d.rules) {
                Some(i) => d.rules[i].apply(&o.rule),
                None => missing("biome rule"),
            },
            op::CAVE_BIOME => d.cave_biomes.push((o.rule.biome, o.level, o.size, o.value)),
            op::REMOVE_CAVE_BIOME => match d.cave_biomes.iter().position(|c| c.0 == o.anchor) {
                Some(i) => drop(d.cave_biomes.remove(i)),
                None => missing("cave biome"),
            },

            n => errors.push(format!("{who}: unknown terrain edit {n} for {dimension}")),
        }
    }

    let block = |name: &str, what: &str, errors: &mut Vec<String>| -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        let b = (names.block)(name);
        if b.is_none() {
            errors.push(format!("terrain of {dimension}: unknown block {name} ({what})"));
        }
        b
    };
    let stone = match &d.stone {
        Some(s) => block(s, "stone", errors).unwrap_or(0),
        None => {
            errors.push(format!("terrain of {dimension}: no stone block (`.stone(..)`)"));
            0
        }
    };
    let sea = d.sea.as_ref().and_then(|(l, w)| Some((*l, block(w, "sea", errors)?)));
    let caves = d.caves.as_ref().and_then(|(lava, from, to)| Some(Caves { lava: block(lava, "lava", errors)?, lava_from: *from, lava_to: *to }));
    let features: Vec<Feature> = d
        .features
        .iter()
        .map(|f| Feature {
            name: f.name.clone(),
            height: if f.preset == abi::terrain_preset::CUSTOM { Height::Custom(f.owner, f.height) } else { Height::Preset(f.preset, f.params.map(q4)) },
            mask: Mask::from_steps(&f.mask),
        })
        .collect();
    let mut rules = Vec::with_capacity(d.rules.len());
    for own in &d.rules {
        let mut merged = (names.biome_rule)(&own.biome).cloned();
        let r = match &mut merged {
            Some(m) => {
                m.apply(own);
                m.name.clone_from(&own.name);
                &*m
            }
            None => own,
        };
        let biome = (names.biome)(&r.biome).unwrap_or_else(|| {
            errors.push(format!("terrain of {dimension}: unknown biome {}", r.biome));
            0
        });
        let feature = (!r.feature.0.is_empty()).then(|| {
            let i = features.iter().position(|f| f.name == r.feature.0);
            if i.is_none() {
                errors.push(format!("terrain of {dimension}: biome rule {} asks for feature {}, which it doesn't have", rule_name(r), r.feature.0));
            }
            (i.unwrap_or(usize::MAX), q12(r.feature.1), q12(r.feature.2))
        });
        let what = format!("biome rule {}", rule_name(r));
        rules.push(Rule {
            biome,
            temperature: (q12(r.temperature.0), q12(r.temperature.1)),
            humidity: (q12(r.humidity.0), q12(r.humidity.1)),
            continent: (q12(r.continent.0), q12(r.continent.1)),
            height: r.height,
            feature,
            top: block(&r.top, &what, errors),
            filler: block(&r.filler, &what, errors),
            depth: r.depth,
            under: block(&r.under.0, &what, errors).map(|b| (b, r.under.1)),
            deep_top: block(&r.deep_top.0, &what, errors).map(|b| (b, r.deep_top.1)),
            bare_above: (r.bare_above != i32::MAX).then_some(r.bare_above),
            ice: block(&r.ice, &what, errors),
        });
    }
    let cave_biomes = d
        .cave_biomes
        .iter()
        .map(|(b, depth, size, threshold)| CaveBiome {
            biome: (names.biome)(b).unwrap_or_else(|| {
                errors.push(format!("terrain of {dimension}: unknown cave biome {b}"));
                0
            }),
            depth: *depth,
            size: q4(*size),
            threshold: q12(*threshold),
        })
        .collect();
    Terrain::new(stone, q4(d.base), sea, features, rules, caves, cave_biomes)
}

impl Terrain {
    fn new(stone: u32, base: i32, sea: Option<(i32, u32)>, features: Vec<Feature>, rules: Vec<Rule>, caves: Option<Caves>, cave_biomes: Vec<CaveBiome>) -> Terrain {
        Terrain { stone, base, sea, features, rules, caves, cave_biomes, cache: ColumnCache::new(32768), corners: ColumnCache::new(32768) }
    }

    pub fn sea_level(&self) -> Option<i32> {
        self.sea.map(|s| s.0)
    }

    /// Height, biome and rule of column (`x`, `z`).
    fn compute(&self, seed: u64, x: i32, z: i32) -> (TerrainColumn, u32) {
        let f = Fields { seed };
        let (fx, fz) = (x * 16, z * 16);
        let mut h = self.base;
        let mut strengths = [0i32; 16];
        let mut more = Vec::new();
        for (i, feat) in self.features.iter().enumerate() {
            let w = feat.mask.as_ref().map_or(ONE, |m| m.value(&f, fx, fz));
            match strengths.get_mut(i) {
                Some(s) => *s = w,
                None => more.push(w),
            }
            if w > 0 {
                h += (w * feat.height(&f, fx, fz)) >> 12;
            }
        }
        let strength = |i: usize| if i < 16 { Some(strengths[i]) } else { more.get(i - 16).copied() };
        let height = h >> 4;
        let (temp, humid, cont) = (f.temperature(fx, fz), f.humidity(fx, fz), f.continent(fx, fz));
        let inside = |v: i32, r: (i32, i32)| v >= r.0 && v < r.1;
        let rule = self
            .rules
            .iter()
            .position(|b| {
                height >= b.height.0
                    && height <= b.height.1
                    && inside(temp, b.temperature)
                    && inside(humid, b.humidity)
                    && inside(cont, b.continent)
                    && b.feature.is_none_or(|(i, lo, hi)| strength(i).is_some_and(|s| s > lo && s <= hi))
            })
            .unwrap_or(self.rules.len().saturating_sub(1));
        (TerrainColumn { height, biome: self.rules.get(rule).map_or(0, |r| r.biome) }, rule as u32)
    }

    /// Column (`x`, `z`) from the cache when its cube column is there, computed otherwise (a
    /// single lookup should not cost a whole cube column).
    pub fn column(&self, seed: u64, x: i32, z: i32) -> TerrainColumn {
        match self.cache.peek(seed, x >> 4, z >> 4) {
            Some(c) => c.cols[((z & 15) * 16 + (x & 15)) as usize],
            None => self.compute(seed, x, z).0,
        }
    }

    /// The columns of cube column (`cx`, `cz`).
    pub fn columns(&self, seed: u64, cx: i32, cz: i32) -> Arc<Columns> {
        self.cache.get(seed, cx, cz, || {
            let mut c = Columns { cols: [TerrainColumn::default(); 256], rules: [0; 256] };
            for i in 0..256 {
                (c.cols[i], c.rules[i]) = self.compute(seed, cx * 16 + (i % 16) as i32, cz * 16 + (i / 16) as i32);
            }
            c
        })
    }

    /// Whether caves took the block at the surface of column (`x`, `z`): decorations placed from
    /// outside the cube (trees) and spawn search check it so they do not float over pits.
    pub fn surface_carved(&self, seed: u64, x: i32, z: i32) -> bool {
        if self.caves.is_none() {
            return false;
        }
        let h = self.column(seed, x, z).height;
        if self.sea.is_some_and(|s| h <= s.0) {
            return false;
        }
        let (gx, gy, gz) = (x.div_euclid(4) * 4, h.div_euclid(4) * 4, z.div_euclid(4) * 4);
        let (t, c) = interpolate(|dy, dz, dx| cave_node(seed, gx + dx as i32 * 4, gy + dy as i32 * 4, gz + dz as i32 * 4), (x - gx) as usize, (h - gy) as usize, (z - gz) as usize);
        let corners: [i32; 4] = [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(dx, dz)| self.column(seed, gx + dx * 4, gz + dz * 4).height);
        let smooth = smooth4(corners, (x - gx) as i32, (z - gz) as i32);
        carves(t > 0, c, cavern_roof(seed, x, z), smooth - h * 16)
    }

    /// Smoothed surface height (Q4) at local column (x, z) from the cube's corner grid.
    fn smooth(corners: &[i32; 25], x: usize, z: usize) -> i32 {
        let (cx, cz) = (x / 4, z / 4);
        let c = |dx: usize, dz: usize| corners[(cz + dz) * 5 + cx + dx];
        smooth4([c(0, 0), c(1, 0), c(0, 1), c(1, 1)], (x % 4) as i32, (z % 4) as i32)
    }

    /// Fills cube `pos` with stone, surface layers, water and caves and sets its biomes; returns
    /// its columns.
    pub fn generate(&self, seed: u64, pos: [i32; 3], blocks: &mut [u32], biomes: &mut [u16; 256]) -> Arc<Columns> {
        let [cx, cy, cz] = pos;
        let (ox, oy, oz) = (cx * 16, cy * 16, cz * 16);
        let cols = self.columns(seed, cx, cz);
        for (i, c) in cols.cols.iter().enumerate() {
            let (x, z) = ((ox + (i % 16) as i32) * 16, (oz + (i / 16) as i32) * 16);
            let deep = self.cave_biomes.iter().enumerate().find(|&(k, b)| oy + 15 < c.height - b.depth && perlin3(seed32(seed, salt::CAVE_BIOME ^ k as u64), x, (oy + 8) * 16, z, b.size, b.size, b.size) > b.threshold);
            biomes[i] = deep.map_or(c.biome, |(_, b)| b.biome) as u16;
        }
        let max_h = cols.cols.iter().map(|c| c.height).max().unwrap_or(0);
        let sea = self.sea.map_or(i32::MIN, |s| s.0);
        if oy > max_h.max(sea) + 16 {
            return cols;
        }
        // Decided per block from its column alone, so neighboring cubes agree: tunnels may open
        // onto the surface, caverns close up under it except where `cavern_roof` lets them out.
        let cave = match &self.caves {
            Some(_) if oy <= max_h => Some(cave_fields(seed, ox, oy, oz)),
            _ => None,
        };
        let corners = self.corners.get(seed, cx, cz, || std::array::from_fn(|i| self.column(seed, ox + (i % 5) as i32 * 4, oz + (i / 5) as i32 * 4).height));
        let stone = self.stone;
        let water = self.sea.map_or(0, |s| s.1);
        for z in 0..16usize {
            for x in 0..16usize {
                let i = z * 16 + x;
                let h = cols.cols[i].height;
                let rule = self.rules.get(cols.rules[i] as usize);
                let bare = rule.and_then(|r| r.bare_above).is_some_and(|y| h > y);
                let mut top = rule.and_then(|r| r.top).unwrap_or(stone);
                if let Some((b, below)) = rule.and_then(|r| r.deep_top) {
                    if h < below {
                        top = b;
                    }
                }
                let filler = rule.and_then(|r| r.filler).unwrap_or(stone);
                let depth = rule.map_or(0, |r| r.depth);
                let under = rule.and_then(|r| r.under);
                let ice = rule.and_then(|r| r.ice);
                let closed = if cave.is_some() { cavern_roof(seed, ox + x as i32, oz + z as i32) } else { 0 };
                let smooth = Self::smooth(&corners, x, z);
                // Keeps a seal under water so seas and canyons do not drain into caves.
                let carved = |ly: usize| {
                    let Some(cave) = &cave else { return false };
                    let wy = oy + ly as i32;
                    let (tunnel, cavern) = cave[(ly * 16 + z) * 16 + x];
                    wy <= h && !(h <= sea && wy > h - 4) && carves(tunnel, cavern, closed, smooth - wy * 16)
                };
                for y in 0..16usize {
                    let wy = oy + y as i32;
                    let mut s = if wy > h {
                        if wy <= sea {
                            if wy == sea { ice.unwrap_or(water) } else { water }
                        } else {
                            0
                        }
                    } else if bare {
                        stone
                    } else if wy == h {
                        top
                    } else if wy >= h - depth {
                        match under {
                            Some((b, layers)) if wy < h - depth + layers => b,
                            _ => filler,
                        }
                    } else {
                        stone
                    };
                    if let Some(c) = self.caves.as_ref().filter(|_| carved(y)) {
                        s = match lake(seed, wy).filter(|_| (c.lava_from..c.lava_to).contains(&wy)) {
                            Some(true) => c.lava,
                            Some(false) => stone,
                            None => 0,
                        };
                    }
                    blocks[(y * 16 + z) * 16 + x] = s;
                }
            }
        }
        cols
    }
}

/// Cave fields for the cube, sampled every 4 blocks on a grid aligned to the world and
/// trilinearly interpolated: whether a tunnel passes, and how far inside a cavern (> 0, Q12)
/// the block is.
fn cave_fields(seed: u64, ox: i32, oy: i32, oz: i32) -> Vec<(bool, i32)> {
    const G: usize = 5;
    let mut grid = [[[(0i32, 0i32); G]; G]; G];
    for (gy, row) in grid.iter_mut().enumerate() {
        for (gz, line) in row.iter_mut().enumerate() {
            for (gx, v) in line.iter_mut().enumerate() {
                *v = cave_node(seed, ox + gx as i32 * 4, oy + gy as i32 * 4, oz + gz as i32 * 4);
            }
        }
    }
    let mut out = vec![(false, 0i32); 4096];
    for y in 0..16 {
        for z in 0..16 {
            for x in 0..16 {
                let (t, c) = interpolate(|dy, dz, dx| grid[y / 4 + dy][z / 4 + dz][x / 4 + dx], x % 4, y % 4, z % 4);
                out[(y * 16 + z) * 16 + x] = (t > 0, c);
            }
        }
    }
    out
}

/// Trilinear blend of grid nodes (`node(dy, dz, dx)`) at offset (x, y, z) inside a 4-block cell.
fn interpolate(node: impl Fn(usize, usize, usize) -> (i32, i32), x: usize, y: usize, z: usize) -> (i32, i32) {
    let (fx, fy, fz) = (x as i32 * 1024, y as i32 * 1024, z as i32 * 1024);
    let l = |a: i32, b: i32, t: i32| a + (((b - a) * t) >> 12);
    let field = |k: usize| {
        let c = |dy, dz, dx| {
            let v = node(dy, dz, dx);
            if k == 0 { v.0 } else { v.1 }
        };
        l(l(l(c(0, 0, 0), c(0, 0, 1), fx), l(c(0, 1, 0), c(0, 1, 1), fx), fz), l(l(c(1, 0, 0), c(1, 0, 1), fx), l(c(1, 1, 0), c(1, 1, 1), fx), fz), fy)
    };
    (field(0), field(1))
}

/// Bilinear blend (Q4) of corner heights 4 blocks apart at offset (x, z) inside the cell.
fn smooth4(c: [i32; 4], x: i32, z: i32) -> i32 {
    (c[0] * (4 - x) + c[1] * x) * (4 - z) + (c[2] * (4 - x) + c[3] * x) * z
}

/// Whether a cave takes a block `depth` (Q4) under the smoothed surface. Near the surface a
/// cavern needs to be ever deeper inside its noise, so its ceiling domes over instead of breaking
/// through; where `cavern_roof` opens (`closed` < 1) it widens towards the surface into a pit.
fn carves(tunnel: bool, cavern: i32, closed: i32, depth: i32) -> bool {
    let d = depth.clamp(-(1 << 16), 1 << 16);
    let near = (ONE - d * ONE / (12 * 16)).max(0);
    let pit = (ONE - d * ONE / (24 * 16)).max(0);
    let need = ((((closed * 1024) >> 12) * near) >> 12) - (((((ONE - closed) * PIT) >> 12) * pit) >> 12);
    tunnel || cavern > need
}

/// What a cave holds at height `wy` instead of air: in about half of the 64-block bands of height
/// a lake crosses every cave at one level, 4 blocks of lava on 2 of stone, so lakes are flat,
/// span the whole cave and never hang over open space. `Some(true)` lava, `Some(false)` stone.
fn lake(seed: u64, wy: i32) -> Option<bool> {
    let band = div_floor(wy, 64).0;
    let h = hash(seed32(seed, salt::LAVA), 0, band, 0);
    if h & 1 == 0 {
        return None;
    }
    let level = band * 64 + 16 + ((h >> 8) % 32) as i32;
    match level - wy {
        1..=4 => Some(true),
        5..=6 => Some(false),
        _ => None,
    }
}

/// Cave fields at a grid node (every 4 blocks): tunnels, caverns (Q12).
fn cave_node(seed: u64, x: i32, y: i32, z: i32) -> (i32, i32) {
    let (x, y, z) = (x * 16, y * 16, z * 16);
    let a = perlin3(seed32(seed, salt::CAVE_A), x, y, z, 48 * 16, 32 * 16, 48 * 16);
    let b = perlin3(seed32(seed, salt::CAVE_B), x, y, z, 48 * 16, 32 * 16, 48 * 16);
    // Two thin noise zero-crossings intersect into winding tunnels.
    let spaghetti = 328 - (a.abs() + b.abs());
    let cheese = perlin3(seed32(seed, salt::CAVERN), x, y, z, 90 * 16, 50 * 16, 90 * 16) - 1720;
    (spaghetti, cheese)
}

/// 1 where caverns stay under a roof, falling to 0 in patches where they may open into wide pits
/// like vanilla's cave entrances (Q12).
const PIT: i32 = 492;

fn cavern_roof(seed: u64, x: i32, z: i32) -> i32 {
    ramp(perlin2(seed32(seed, salt::CAVERN_ROOF), x * 16, z * 16, 120 * 16), 1638, 1024)
}

const SHARDS: usize = 32;

/// Data computed once per column of cubes and shared by every cube of it. Generation runs on
/// many threads at once, so the map is split into locked shards; a shard that grows past its
/// share of the capacity drops a quarter of its entries (clearing it whole made a view of 64,
/// some 13k columns, compute most columns twice).
pub struct ColumnCache<T> {
    shards: [Mutex<FxHashMap<(u64, i32, i32), Arc<std::sync::OnceLock<Arc<T>>>>>; SHARDS],
    per_shard: usize,
}

impl<T> ColumnCache<T> {
    pub fn new(capacity: usize) -> ColumnCache<T> {
        ColumnCache { shards: std::array::from_fn(|_| Mutex::new(FxHashMap::default())), per_shard: capacity.div_ceil(SHARDS).max(1) }
    }

    fn shard(&self, x: i32, z: i32) -> &Mutex<FxHashMap<(u64, i32, i32), Arc<std::sync::OnceLock<Arc<T>>>>> {
        let h = loom_noise::hash(0, x as i64, 0, z as i64) as u32;
        &self.shards[(h >> 27) as usize % SHARDS]
    }

    fn peek(&self, seed: u64, x: i32, z: i32) -> Option<Arc<T>> {
        self.shard(x, z).lock().get(&(seed, x, z)).and_then(|c| c.get().cloned())
    }

    /// The value for cube column (`x`, `z`) under `seed`, computing it with `make` on a miss.
    /// Threads asking while it is computed wait for it: the cubes of a column come together,
    /// and each computed it again.
    pub fn get(&self, seed: u64, x: i32, z: i32, make: impl FnOnce() -> T) -> Arc<T> {
        let cell = {
            let mut map = self.shard(x, z).lock();
            match map.get(&(seed, x, z)) {
                Some(cell) => cell.clone(),
                None => {
                    if map.len() >= self.per_shard {
                        let mut n = 0usize;
                        map.retain(|_, _| {
                            n += 1;
                            n % 4 != 0
                        });
                    }
                    map.entry((seed, x, z)).or_default().clone()
                }
            }
        };
        cell.get_or_init(|| Arc::new(make())).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::terrain_preset as p;

    fn preset(kind: u32, params: [f64; 4]) -> Feature {
        Feature { name: String::new(), height: Height::Preset(kind, params.map(q4)), mask: None }
    }

    fn rule(biome: u32) -> Rule {
        let any = (i32::MIN, i32::MAX);
        Rule { biome, temperature: any, humidity: any, continent: any, height: (i32::MIN, i32::MAX), feature: None, top: None, filler: None, depth: 3, under: None, deep_top: None, bare_above: None, ice: None }
    }

    fn terrain(features: Vec<Feature>, rules: Vec<Rule>, lava: Option<u32>, base: f64) -> Terrain {
        Terrain::new(1, q4(base), None, features, rules, lava.map(|lava| Caves { lava, lava_from: if lava == 1 { 1 } else { i32::MIN }, lava_to: 11 }), Vec::new())
    }

    fn cube(t: &Terrain, seed: u64, pos: [i32; 3]) -> Vec<u32> {
        let mut blocks = vec![0u32; 4096];
        t.generate(seed, pos, &mut blocks, &mut [0; 256]);
        blocks
    }

    #[test]
    fn masks_ramp_both_ways() {
        assert_eq!(ramp(2048, 0, ONE), 2048);
        assert_eq!(ramp(-819, 0, -1638), 2048);
        assert_eq!(ramp(2 * ONE, 0, ONE), ONE);
    }

    /// Caverns end under a roof, not at the bottom of the surface cube (that leaves flat stone
    /// slabs hanging over open space).
    #[test]
    fn caverns_keep_a_roof() {
        let t = terrain(vec![preset(p::HILLS, [16.0, 110.0, 0.0, 0.0]), preset(p::MOUNTAINS, [150.0, 260.0, 0.0, 0.0])], Vec::new(), Some(1), 64.0);
        let (mut columns, mut breached) = (0, 0);
        for cx in -3..3 {
            for cz in -3..3 {
                let heights: Vec<i32> = (0..256).map(|i| t.column(7, cx * 16 + (i % 16) as i32, cz * 16 + (i / 16) as i32).height).collect();
                let top = heights.iter().max().unwrap() >> 4;
                let bottom = (heights.iter().min().unwrap() >> 4) - 1;
                // Air within the column, by height.
                let mut air = std::collections::HashSet::new();
                for cy in bottom..=top {
                    for (i, &b) in cube(&t, 7, [cx, cy, cz]).iter().enumerate() {
                        if b == 0 {
                            air.insert((i % 256, cy * 16 + (i / 256) as i32));
                        }
                    }
                }
                for (i, &h) in heights.iter().enumerate() {
                    if cavern_roof(7, cx * 16 + (i % 16) as i32, cz * 16 + (i / 16) as i32) < ONE {
                        continue;
                    }
                    columns += 1;
                    // Open space in the 8 blocks under the surface means the cave reached up.
                    let open = (h - 8..h).filter(|&y| air.contains(&(i, y))).count();
                    if open >= 6 {
                        breached += 1;
                    }
                }
            }
        }
        assert!(breached * 50 < columns, "{breached} of {columns} columns open under the surface");
    }

    /// The point check trees use agrees with what generation carved.
    #[test]
    fn surface_carved_matches_generation() {
        let t = terrain(vec![preset(p::HILLS, [16.0, 110.0, 0.0, 0.0])], Vec::new(), Some(1), 64.0);
        let mut checked = 0;
        for cx in -4..4 {
            for cz in -4..4 {
                for i in 0..256 {
                    let (x, z) = (cx * 16 + (i % 16) as i32, cz * 16 + (i / 16) as i32);
                    let h = t.column(3, x, z).height;
                    let air = cube(&t, 3, [cx, h >> 4, cz])[((h & 15) as usize * 16 + i / 16) * 16 + i % 16] == 0;
                    assert_eq!(air, t.surface_carved(3, x, z), "at {x} {z}");
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 16384);
    }

    /// Lava lakes lie on stone, not in every open block between two heights (lava hanging
    /// over caves below).
    #[test]
    fn lava_lakes_rest_on_stone() {
        let t = terrain(vec![preset(p::HILLS, [16.0, 110.0, 0.0, 0.0])], Vec::new(), Some(7), 64.0);
        let (mut lava, mut hanging) = (0, 0);
        for cx in -3..3 {
            for cz in -3..3 {
                let column: FxHashMap<i32, Vec<u32>> = (-32..1).map(|cy| (cy, cube(&t, 9, [cx, cy, cz]))).collect();
                let at = |i: usize, y: i32| column.get(&(y >> 4)).map(|b| b[((y & 15) as usize * 16 + i / 16) * 16 + i % 16]);
                for i in 0..256 {
                    for y in -511..16 {
                        if at(i, y) == Some(7) {
                            lava += 1;
                            if at(i, y - 1) == Some(0) {
                                hanging += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(lava > 0, "no lakes at all");
        assert_eq!(hanging, 0, "{hanging} of {lava} lava blocks over air");
    }

    #[test]
    fn first_matching_rule_wins() {
        let low = |b| Rule { height: (i32::MIN, 60), ..rule(b) };
        assert_eq!(terrain(Vec::new(), vec![low(1), rule(2)], None, 70.0).column(1, 0, 0).biome, 2);
        assert_eq!(terrain(Vec::new(), vec![low(1), rule(2)], None, 50.0).column(1, 0, 0).biome, 1);
    }

    fn raw(owner: u32, op: u32, anchor: &str, rule: Option<(&str, &str)>) -> RawTerrain {
        let r = rule.unwrap_or(("", ""));
        RawTerrain {
            owner: ModId(owner),
            dimension: "a:dim".into(),
            op,
            anchor: anchor.into(),
            block: if op == op::STONE { "a:stone".into() } else { String::new() },
            level: 0,
            to: 0,
            value: 0.0,
            size: 0.0,
            feature: RawFeature { owner: ModId(owner), name: String::new(), preset: p::HILLS, params: [0.0; 4], height: Callback::NONE, mask: Vec::new() },
            rule: RawRule {
                name: r.0.into(),
                fields: rf::BIOME,
                biome: r.1.into(),
                temperature: (f64::NEG_INFINITY, f64::INFINITY),
                humidity: (f64::NEG_INFINITY, f64::INFINITY),
                continent: (f64::NEG_INFINITY, f64::INFINITY),
                height: (i32::MIN, i32::MAX),
                feature: (String::new(), 0.0, 0.0),
                top: String::new(),
                filler: String::new(),
                depth: 3,
                under: (String::new(), 0),
                deep_top: (String::new(), i32::MIN),
                bare_above: i32::MAX,
                ice: String::new(),
            },
        }
    }

    fn built(ops: Vec<RawTerrain>) -> (Vec<u32>, Vec<String>) {
        let biomes = ["a:plains", "a:desert", "b:savanna", "b:dunes", "a:snow"];
        let names = Names { block: &|n| (n == "a:stone").then_some(1), biome: &|n| biomes.iter().position(|b| *b == n).map(|i| i as u32), biome_rule: &|_| None };
        let mut errors = Vec::new();
        let t = build("a:dim", &|m| format!("mod{}", m.0), ops, &names, &mut errors);
        (t.rules.iter().map(|r| r.biome).collect(), errors)
    }

    /// Edits find rules by name (a biome's own name by default) and keep the owner's order around them.
    #[test]
    fn edits_insert_by_name() {
        let base = || vec![raw(0, op::STONE, "", None), raw(0, op::BIOME, "", Some(("", "a:desert"))), raw(0, op::BIOME, "", Some(("", "a:plains")))];
        let mut ops = base();
        ops.push(raw(1, op::BIOME_BEFORE, "a:desert", Some(("", "b:savanna"))));
        ops.push(raw(1, op::BIOME_AFTER, "a:desert", Some(("", "b:dunes"))));
        assert_eq!(built(ops), (vec![2, 1, 3, 0], vec![]));

        let mut ops = base();
        ops.push(raw(1, op::REPLACE_BIOME, "a:desert", Some(("", "b:dunes"))));
        ops.push(raw(1, op::REMOVE_BIOME, "a:plains", None));
        assert_eq!(built(ops).0, vec![3]);

        // A second rule of the same biome needs a name of its own.
        let mut ops = base();
        ops.push(raw(1, op::BIOME_BEFORE, "a:plains", Some(("", "a:desert"))));
        assert_eq!(built(ops).1.len(), 1);
        let mut ops = base();
        ops.push(raw(1, op::BIOME_BEFORE, "a:plains", Some(("b:hot_coast", "a:desert"))));
        assert_eq!(built(ops), (vec![1, 1, 0], vec![]));

        // Edits whose target is missing are skipped, not errors.
        let mut ops = base();
        ops.push(raw(1, op::BIOME_BEFORE, "a:jungle", Some(("", "b:savanna"))));
        assert_eq!(built(ops), (vec![1, 0], vec![]));

        // Only the given fields change.
        let mut ops = base();
        let mut edit = raw(1, op::EDIT_BIOME, "a:desert", None);
        edit.rule.fields = rf::HEIGHT;
        edit.rule.height = (0, 10);
        ops.push(edit);
        let names = Names { block: &|_| Some(1), biome: &|_| Some(0), biome_rule: &|_| None };
        let t = build("a:dim", &|_| String::new(), ops, &names, &mut Vec::new());
        assert_eq!((t.rules[0].height, t.rules.len()), ((0, 10), 2));
    }

    /// A rule starts from its biome's climate and surface and overrides only what it sets.
    #[test]
    fn rules_inherit_the_biome() {
        let mut desert = raw(0, op::BIOME, "", Some(("", "a:desert"))).rule;
        desert.fields = rf::TEMPERATURE | rf::SURFACE;
        desert.temperature = (0.35, f64::INFINITY);
        (desert.top, desert.filler, desert.depth) = ("a:sand".into(), "a:sand".into(), 3);
        let mut coast = raw(0, op::BIOME, "", Some(("a:coast", "a:desert")));
        coast.rule.fields |= rf::TEMPERATURE | rf::HEIGHT;
        coast.rule.temperature = (0.0, 1.0);
        coast.rule.height = (0, 64);
        let ops = vec![raw(0, op::STONE, "", None), coast, raw(0, op::BIOME, "", Some(("", "a:desert")))];
        let names = Names { block: &|n| Some(if n == "a:sand" { 2 } else { 1 }), biome: &|_| Some(0), biome_rule: &|n| (n == "a:desert").then_some(&desert) };
        let mut errors = Vec::new();
        let t = build("a:dim", &|_| String::new(), ops, &names, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!((t.rules[0].temperature, t.rules[0].height, t.rules[0].top), ((0, ONE), (0, 64), Some(2)));
        assert_eq!((t.rules[1].temperature, t.rules[1].height, t.rules[1].top), ((q12(0.35), 1 << 18), (i32::MIN, i32::MAX), Some(2)));
    }

    #[test]
    fn masks_from_steps() {
        use abi::terrain_mask as k;
        let step = |kind, count| abi::MaskStep { kind, count, ..abi::MaskStep::DEFAULT };
        let m = Mask::from_steps(&[step(k::CONTINENT, 0), step(k::HUMIDITY, 0), step(k::NOT, 0), step(k::ALL, 2)]).unwrap();
        assert!(matches!(&m, Mask::All(v) if v.len() == 2 && matches!(v[1], Mask::Not(_))));
        assert!(Mask::from_steps(&[step(k::ALL, 2)]).is_none());
        assert!(Mask::from_steps(&[]).is_none());
    }
}
