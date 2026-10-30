//! Terrain from parts: a base height plus height features, biome rules that pick each column's
//! biome and surface, water and caves. The mod that owns a dimension describes its terrain and
//! any mod can edit it afterwards (add a biome before another, a feature, a cave biome), so the
//! engine evaluates the merged result. Features are presets or plain functions of the noise
//! fields, so a mod can ship its own.
//!
//! ```ignore
//! r.terrain(&MARS)
//!     .stone("basalt")
//!     .base_height(60.0)
//!     .feature(preset::hills(8.0, 220.0))
//!     .feature(preset::mountains(60.0, 160.0).mask(Mask::noise(500.0, 0.2, 0.5)).named("mountains"))
//!     .biome(BiomeRule::new("peaks").feature("mountains", 0.5..))
//!     .biome(BiomeRule::new("dust"));
//! // Another mod, whose savanna biome declares its climate and surface:
//! r.terrain(&OVERWORLD).biome_before("base:desert", BiomeRule::new("savanna"));
//! ```

use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};

use crate::abi::{self, Slice, Str, rule_field as rf, terrain_op as op};
use crate::{Dimension, Gen, Registry, Scatter, callback, host, qualify};

pub use crate::noise::fixed::{Fields, ONE};

/// Surface `height` and `biome` id of a column.
pub type Column = abi::TerrainColumn;

/// Where a feature applies, 0..1.
#[derive(Clone)]
pub enum Mask {
    Noise { salt: u64, scale: f64, octaves: u32, from: f64, to: f64 },
    Continent(f64, f64),
    Temperature(f64, f64),
    Humidity(f64, f64),
    All(Vec<Mask>),
    /// 1 minus the mask: a biome's climate taking a feature away (flat tundra).
    Not(Box<Mask>),
}

impl Mask {
    /// Its own noise field: patches of about `scale` blocks, fading in between `from` and `to`.
    pub fn noise(scale: f64, from: f64, to: f64) -> Mask {
        Mask::Noise { salt: scale.to_bits(), scale, octaves: 3, from, to }
    }

    pub fn salt(self, salt: u64) -> Mask {
        match self {
            Mask::Noise { scale, octaves, from, to, .. } => Mask::Noise { salt, scale, octaves, from, to },
            m => m,
        }
    }

    pub fn not(self) -> Mask {
        Mask::Not(Box::new(self))
    }

    pub fn and(self, other: Mask) -> Mask {
        match self {
            Mask::All(mut v) => {
                v.push(other);
                Mask::All(v)
            }
            m => Mask::All(vec![m, other]),
        }
    }

    fn steps(&self, out: &mut Vec<abi::MaskStep>) {
        use abi::terrain_mask as k;
        let step = abi::MaskStep::DEFAULT;
        match self {
            &Mask::Noise { salt, scale, octaves, from, to } => out.push(abi::MaskStep { kind: k::NOISE, salt, scale, octaves, from, to, ..step }),
            &Mask::Continent(from, to) => out.push(abi::MaskStep { kind: k::CONTINENT, from, to, ..step }),
            &Mask::Temperature(from, to) => out.push(abi::MaskStep { kind: k::TEMPERATURE, from, to, ..step }),
            &Mask::Humidity(from, to) => out.push(abi::MaskStep { kind: k::HUMIDITY, from, to, ..step }),
            Mask::All(v) => {
                v.iter().for_each(|m| m.steps(out));
                out.push(abi::MaskStep { kind: k::ALL, count: v.len() as u32, ..step });
            }
            Mask::Not(m) => {
                m.steps(out);
                out.push(abi::MaskStep { kind: k::NOT, ..step });
            }
        }
    }
}

/// Adds to the height of every column, scaled by its mask.
pub struct Feature {
    name: String,
    preset: u32,
    params: [f64; 4],
    height: abi::Callback,
    mask: Option<Mask>,
}

impl Feature {
    /// A feature of your own: the height it adds at (x, z). Positions and the height are in
    /// 1/16 block and noise values in 1/4096 (`ONE`): terrain is integers only, so every
    /// machine and the GPU agree. It runs for every column, on many threads at once, and like a
    /// generator it must be a pure function of its input.
    pub fn new(height: impl Fn(&Fields, i32, i32) -> i32 + Send + Sync + 'static) -> Feature {
        let height = callback(move |a: &mut abi::FeatureArgs| a.height = height(&Fields { seed: a.seed }, a.x, a.z));
        Feature { name: String::new(), preset: abi::terrain_preset::CUSTOM, params: [0.0; 4], height, mask: None }
    }

    fn preset(preset: u32, params: [f64; 4]) -> Feature {
        Feature { name: String::new(), preset, params, height: abi::Callback::NONE, mask: None }
    }

    /// Biome rules can ask for the mask strength by this name, and other mods find the feature
    /// by it.
    pub fn named(mut self, name: &str) -> Feature {
        self.name = name.to_string();
        self
    }

    pub fn mask(mut self, mask: Mask) -> Feature {
        self.mask = Some(mask);
        self
    }
}

/// Ready-made features. Their code is in `loom_noise::fixed::shape`; copy one into a
/// `Feature::new` to make a new kind.
pub mod preset {
    use super::*;
    use crate::abi::terrain_preset as p;

    /// Oceans and land masses: `depth` blocks above or below the base height.
    pub fn continents(depth: f64) -> Feature {
        Feature::preset(p::CONTINENTS, [depth, 0.0, 0.0, 0.0])
    }

    /// Rolling hills.
    pub fn hills(height: f64, size: f64) -> Feature {
        Feature::preset(p::HILLS, [height, size, 0.0, 0.0])
    }

    /// Mountain ranges along ridge lines `width` blocks apart, with rough detail on top.
    pub fn mountains(height: f64, width: f64) -> Feature {
        Feature::preset(p::MOUNTAINS, [height, width, 0.0, 0.0])
    }

    /// Flat-topped mesas `height` blocks high in terraces of `step`.
    pub fn plateaus(height: f64, size: f64, step: f64) -> Feature {
        Feature::preset(p::PLATEAUS, [height, size, step, 0.0])
    }

    /// Winding cuts `depth` deep and about `width` wide; below sea level they fill with water.
    pub fn canyons(depth: f64, width: f64) -> Feature {
        Feature::preset(p::CANYONS, [depth, width, 0.0, 0.0])
    }

    /// Parallel sand waves `wavelength` blocks apart.
    pub fn dunes(height: f64, wavelength: f64) -> Feature {
        Feature::preset(p::DUNES, [height, wavelength, 0.0, 0.0])
    }

    /// Bowls with a raised rim, one per `spacing`-block cell at most.
    pub fn craters(radius: f64, depth: f64, spacing: f64) -> Feature {
        Feature::preset(p::CRATERS, [radius, depth, spacing, 0.0])
    }
}

/// Where a biome stands in a terrain. Its climate and surface come from the biome
/// (`r.biome(&X).temperature(..).surface(..)`); a rule adds where else it must hold (height,
/// continent, a feature) and sets only what differs here. Rules are tried in order; the first
/// match wins and the last one is used where none does.
#[derive(Clone)]
pub struct BiomeRule {
    name: String,
    fields: u32,
    biome: String,
    temperature: (f64, f64),
    humidity: (f64, f64),
    continent: (f64, f64),
    height: (i32, i32),
    feature: (String, f64, f64),
    top: String,
    filler: String,
    depth: i32,
    under: (String, i32),
    deep_top: (String, i32),
    bare_above: i32,
    ice: String,
}

impl BiomeRule {
    /// A rule for `biome`, named like it.
    pub fn new(biome: &str) -> BiomeRule {
        BiomeRule { fields: rf::BIOME, biome: biome.to_string(), ..BiomeRule::blank() }
    }

    pub(crate) fn blank() -> BiomeRule {
        let any = (f64::NEG_INFINITY, f64::INFINITY);
        BiomeRule {
            name: String::new(),
            fields: 0,
            biome: String::new(),
            temperature: any,
            humidity: any,
            continent: any,
            height: (i32::MIN, i32::MAX),
            feature: (String::new(), 0.0, 0.0),
            top: String::new(),
            filler: String::new(),
            depth: 3,
            under: (String::new(), 0),
            deep_top: (String::new(), i32::MIN),
            bare_above: i32::MAX,
            ice: String::new(),
        }
    }

    /// A name of its own, for a second rule of the same biome (other mods find rules by name).
    pub fn named(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }
    /// In an edit: the rule picks this biome instead.
    pub fn biome(mut self, biome: &str) -> Self {
        (self.biome, self.fields) = (biome.to_string(), self.fields | rf::BIOME);
        self
    }
    /// Climate ranges like `0.35..` or `..-0.45` (from inclusive, to exclusive); the fields are
    /// about -1.4..1.4. Usually the biome's.
    pub fn temperature(mut self, range: impl RangeBounds<f64>) -> Self {
        (self.temperature, self.fields) = (span(range), self.fields | rf::TEMPERATURE);
        self
    }
    pub fn humidity(mut self, range: impl RangeBounds<f64>) -> Self {
        (self.humidity, self.fields) = (span(range), self.fields | rf::HUMIDITY);
        self
    }
    /// Continentalness: below 0 is sea, coasts are about 0..0.1.
    pub fn continent(mut self, range: impl RangeBounds<f64>) -> Self {
        (self.continent, self.fields) = (span(range), self.fields | rf::CONTINENT);
        self
    }
    /// Surface heights, like `..=SEA + 1`.
    pub fn height(mut self, range: impl RangeBounds<i32>) -> Self {
        (self.height, self.fields) = (span_int(range), self.fields | rf::HEIGHT);
        self
    }
    /// Where the named feature's mask is in `range`.
    pub fn feature(mut self, name: &str, range: impl RangeBounds<f64>) -> Self {
        let (min, max) = span(range);
        (self.feature, self.fields) = ((name.to_string(), min, max), self.fields | rf::FEATURE);
        self
    }
    /// Top block, then `depth` blocks of filler, then the terrain's stone.
    pub fn surface(mut self, top: &str, filler: &str, depth: i32) -> Self {
        (self.top, self.filler, self.depth) = (top.to_string(), filler.to_string(), depth);
        self.fields |= rf::SURFACE;
        self
    }
    /// The lowest `layers` of the filler become `block` (sandstone under sand).
    pub fn under(mut self, block: &str, layers: i32) -> Self {
        (self.under, self.fields) = ((block.to_string(), layers), self.fields | rf::UNDER);
        self
    }
    /// Top block for columns lower than `below` (gravel on deep sea floors).
    pub fn deep_top(mut self, block: &str, below: i32) -> Self {
        (self.deep_top, self.fields) = ((block.to_string(), below), self.fields | rf::DEEP_TOP);
        self
    }
    /// Plain stone above this height (bare peaks).
    pub fn bare_above(mut self, y: i32) -> Self {
        (self.bare_above, self.fields) = (y, self.fields | rf::BARE_ABOVE);
        self
    }
    /// The water surface freezes into `block`.
    pub fn frozen(mut self, block: &str) -> Self {
        (self.ice, self.fields) = (block.to_string(), self.fields | rf::FROZEN);
        self
    }

    /// Its names, qualified with `ns`, in the order `desc` reads them.
    pub(crate) fn names(&self, ns: &str) -> [String; 8] {
        let q = |s: &str| if s.is_empty() { String::new() } else { qualify(ns, s) };
        [if self.name.is_empty() { &self.biome } else { &self.name }, &self.biome, &self.feature.0, &self.top, &self.filler, &self.under.0, &self.deep_top.0, &self.ice].map(|s| q(s))
    }

    pub(crate) fn desc(&self, names: &[String; 8]) -> abi::BiomeRuleDesc {
        abi::BiomeRuleDesc {
            name: Str::new(&names[0]),
            fields: self.fields,
            biome: Str::new(&names[1]),
            temperature: [self.temperature.0, self.temperature.1],
            humidity: [self.humidity.0, self.humidity.1],
            continent: [self.continent.0, self.continent.1],
            height: [self.height.0, self.height.1],
            feature: Str::new(&names[2]),
            feature_range: [self.feature.1, self.feature.2],
            top: Str::new(&names[3]),
            filler: Str::new(&names[4]),
            depth: self.depth,
            under: Str::new(&names[5]),
            under_layers: self.under.1,
            deep_top: Str::new(&names[6]),
            deep_below: self.deep_top.1,
            bare_above: self.bare_above,
            ice: Str::new(&names[7]),
        }
    }
}

fn span(r: impl RangeBounds<f64>) -> (f64, f64) {
    let at = |b: Bound<&f64>, open: f64| match b {
        Bound::Included(v) | Bound::Excluded(v) => *v,
        Bound::Unbounded => open,
    };
    (at(r.start_bound(), f64::NEG_INFINITY), at(r.end_bound(), f64::INFINITY))
}

/// Inclusive.
fn span_int(r: impl RangeBounds<i32>) -> (i32, i32) {
    let min = match r.start_bound() {
        Bound::Included(v) => *v,
        Bound::Excluded(v) => v + 1,
        Bound::Unbounded => i32::MIN,
    };
    let max = match r.end_bound() {
        Bound::Included(v) => *v,
        Bound::Excluded(v) => v - 1,
        Bound::Unbounded => i32::MAX,
    };
    (min, max)
}

/// Two-noise tunnels plus large caverns. Between `lava_from` and `lava_to` some layers of height
/// hold flat lava lakes.
pub struct Caves {
    lava: String,
    lava_from: i32,
    lava_to: i32,
}

impl Caves {
    pub fn new(lava: &str, lava_from: i32, lava_to: i32) -> Caves {
        Caves { lava: lava.to_string(), lava_from, lava_to }
    }
}

impl Registry {
    /// The terrain of a dimension: yours to describe, or another mod's to edit
    /// (`r.terrain(&OVERWORLD)`). Edits apply after the owner's description, in load order.
    pub fn terrain<'a>(&'a mut self, dimension: &crate::Dimension) -> TerrainBuilder<'a> {
        TerrainBuilder { raw: self.raw, ns: self.ns, dimension: dimension.name().to_string(), _p: PhantomData }
    }
}

pub struct TerrainBuilder<'a> {
    raw: *mut abi::Registrar,
    ns: &'static str,
    dimension: String,
    _p: PhantomData<&'a mut Registry>,
}

/// One edit, owning everything its `TerrainDesc` points at while it is sent.
struct Op {
    op: u32,
    anchor: String,
    block: String,
    level: i32,
    to: i32,
    value: f64,
    size: f64,
    feature: Option<Feature>,
    rule: Option<BiomeRule>,
}

impl Op {
    fn new(op: u32) -> Op {
        Op { op, anchor: String::new(), block: String::new(), level: 0, to: 0, value: 0.0, size: 0.0, feature: None, rule: None }
    }
}

impl TerrainBuilder<'_> {
    fn send(self, o: Op) -> Self {
        let q = |s: &str| if s.is_empty() { String::new() } else { qualify(self.ns, s) };
        let (anchor, block) = (q(&o.anchor), q(&o.block));
        let mut steps = Vec::new();
        let mut feature = abi::FeatureDesc::DEFAULT;
        let fname = o.feature.as_ref().map_or(String::new(), |f| q(&f.name));
        if let Some(f) = &o.feature {
            if let Some(m) = &f.mask {
                m.steps(&mut steps);
            }
            feature = abi::FeatureDesc { name: Str::new(&fname), preset: f.preset, params: f.params, height: f.height, mask: Slice::new(&steps) };
        }
        let r = o.rule.unwrap_or_else(BiomeRule::blank);
        let names = r.names(self.ns);
        let rule = r.desc(&names);
        let desc = abi::TerrainDesc {
            dimension: Str::new(&self.dimension),
            op: o.op,
            anchor: Str::new(&anchor),
            block: Str::new(&block),
            level: o.level,
            to: o.to,
            value: o.value,
            size: o.size,
            feature,
            rule,
        };
        unsafe { (host().reg_terrain)(self.raw, &desc) }
        self
    }

    /// The block the ground is made of.
    pub fn stone(self, block: &str) -> Self {
        self.send(Op { block: block.to_string(), ..Op::new(op::STONE) })
    }
    /// The ground height where no feature adds anything.
    pub fn base_height(self, y: f64) -> Self {
        self.send(Op { value: y, ..Op::new(op::BASE_HEIGHT) })
    }
    /// Everything up to `level` that isn't ground is `water`.
    pub fn sea(self, level: i32, water: &str) -> Self {
        self.send(Op { level, block: water.to_string(), ..Op::new(op::SEA) })
    }
    pub fn caves(self, caves: Caves) -> Self {
        self.send(Op { block: caves.lava, level: caves.lava_from, to: caves.lava_to, ..Op::new(op::CAVES) })
    }
    pub fn feature(self, f: Feature) -> Self {
        self.send(Op { feature: Some(f), ..Op::new(op::FEATURE) })
    }
    pub fn remove_feature(self, name: &str) -> Self {
        self.send(Op { anchor: name.to_string(), ..Op::new(op::REMOVE_FEATURE) })
    }
    /// Limits the named feature further: its mask is multiplied by `mask`.
    pub fn mask_feature(self, name: &str, mask: Mask) -> Self {
        self.send(Op { anchor: name.to_string(), feature: Some(Feature::preset(0, [0.0; 4]).mask(mask)), ..Op::new(op::MASK_FEATURE) })
    }
    /// A rule after all others.
    pub fn biome(self, rule: BiomeRule) -> Self {
        self.send(Op { rule: Some(rule), ..Op::new(op::BIOME) })
    }
    /// A rule tried right before the rule named `rule` (a biome's rule is named like the biome).
    pub fn biome_before(self, rule: &str, new: BiomeRule) -> Self {
        self.send(Op { anchor: rule.to_string(), rule: Some(new), ..Op::new(op::BIOME_BEFORE) })
    }
    pub fn biome_after(self, rule: &str, new: BiomeRule) -> Self {
        self.send(Op { anchor: rule.to_string(), rule: Some(new), ..Op::new(op::BIOME_AFTER) })
    }
    pub fn replace_biome(self, rule: &str, new: BiomeRule) -> Self {
        self.send(Op { anchor: rule.to_string(), rule: Some(new), ..Op::new(op::REPLACE_BIOME) })
    }
    pub fn remove_biome(self, rule: &str) -> Self {
        self.send(Op { anchor: rule.to_string(), ..Op::new(op::REMOVE_BIOME) })
    }
    /// Changes what the closure sets on the rule and keeps the rest:
    /// `edit_biome("base:desert", |r| r.temperature(0.5..))`.
    pub fn edit_biome(self, rule: &str, edit: impl FnOnce(BiomeRule) -> BiomeRule) -> Self {
        self.send(Op { anchor: rule.to_string(), rule: Some(edit(BiomeRule::blank())), ..Op::new(op::EDIT_BIOME) })
    }
    /// A biome in 3D: whole cubes at least `depth` blocks under the surface where a 3D noise of
    /// patches about `size` blocks across is above `threshold` (-1..1). Biomes are kept per
    /// column of each cube, so underground ones change every 16 blocks of height at most.
    pub fn cave_biome(self, biome: &str, depth: i32, size: f64, threshold: f64) -> Self {
        self.send(Op { rule: Some(BiomeRule::new(biome)), level: depth, size, value: threshold, ..Op::new(op::CAVE_BIOME) })
    }
    pub fn remove_cave_biome(self, biome: &str) -> Self {
        self.send(Op { anchor: biome.to_string(), ..Op::new(op::REMOVE_CAVE_BIOME) })
    }
}

impl Dimension {
    /// Height and biome of column (`x`, `z`) for `seed`, generated or not: from the terrain or
    /// the dimension's `column`. A dimension with neither gives height `i32::MIN`.
    pub fn column(&self, seed: u64, x: i32, z: i32) -> Column {
        let mut out = Column { height: i32::MIN, biome: 0 };
        unsafe { (host().dim_column)(self.id(), seed, x, z, &mut out) };
        out
    }

    /// Whether caves took the surface block of column (`x`, `z`), so nothing should stand there.
    pub fn surface_carved(&self, seed: u64, x: i32, z: i32) -> bool {
        unsafe { (host().dim_carved)(self.id(), seed, x, z) != 0 }
    }
}

impl Registry {
    /// Something scattered over the ground (trees): one site per `cell` x `cell` square at a
    /// seeded spot, kept with the chance the biome's `density` component gives, in every
    /// dimension where such biomes stand. The engine skips sites under the sea and where caves
    /// took the ground, fits the parts that reach into neighboring cubes, and keeps players
    /// from spawning in one.
    ///
    /// ```ignore
    /// static TREES: Scatter = Scatter::named("trees");
    /// r.scatter(&TREES).cell(5).reach(2).density(&TREE_DENSITY).shape(|s| {
    ///     let [x, y, z] = s.pos();
    ///     for dy in 0..5 { s.force([x, y + dy, z], LOG.id()); }
    /// });
    /// ```
    pub fn scatter<'a>(&'a mut self, scatter: &Scatter) -> ScatterBuilder<'a> {
        ScatterBuilder { raw: self.raw, name: scatter.name().to_string(), dimensions: String::new(), density: String::new(), desc: abi::ScatterDesc::DEFAULT, _p: PhantomData }
    }

    /// Runs `f` on every generated cube after the generator and scatters (grass, flowers,
    /// boulders), in every dimension but bare ones unless `.dimension(..)` limits it.
    /// `g.columns()` has the surface of every column. Pure, like a generator.
    pub fn decorate<'a>(&'a mut self, f: impl Fn(&mut Gen) + Send + Sync + 'static) -> DecorationBuilder<'a> {
        DecorationBuilder { raw: self.raw, dimensions: String::new(), cb: callback(move |a: &mut abi::GenArgs| f(Gen::wrap(a))), _p: PhantomData }
    }
}

pub struct ScatterBuilder<'a> {
    raw: *mut abi::Registrar,
    name: String,
    dimensions: String,
    density: String,
    desc: abi::ScatterDesc,
    _p: PhantomData<&'a mut Registry>,
}

impl ScatterBuilder<'_> {
    /// Grid cell size in blocks (8 by default): at most one site per cell.
    pub fn cell(mut self, blocks: i32) -> Self {
        self.desc.cell = blocks;
        self
    }
    /// How far the shape puts blocks sideways from its site. Up and down it may go 32 and 16.
    pub fn reach(mut self, blocks: i32) -> Self {
        self.desc.reach = blocks;
        self
    }
    /// The biome component (0..1) that is the chance of a site per cell; biomes without it get
    /// none.
    pub fn density(mut self, component: &crate::Component<f32>) -> Self {
        self.density = component.name().to_string();
        self
    }
    /// Only in this dimension (call again for more).
    pub fn dimension(mut self, dimension: &Dimension) -> Self {
        self.dimensions = format!("{} {}", self.dimensions, dimension.name());
        self
    }
    /// The blocks of one site. Pure: it runs again for each cube the site reaches.
    pub fn shape(mut self, f: impl Fn(&mut Site) + Send + Sync + 'static) -> Self {
        self.desc.shape = callback(move |a: &mut abi::ScatterArgs| f(Site::wrap(a)));
        self
    }
}

impl Drop for ScatterBuilder<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let desc = abi::ScatterDesc { name: Str::new(&self.name), dimensions: Str::new(&self.dimensions), density: Str::new(&self.density), ..self.desc };
        unsafe { (host().reg_scatter)(self.raw, &desc) }
    }
}

/// One site of a scatter: where it stands and where its blocks go (world coordinates).
pub struct Site {
    args: abi::ScatterArgs,
}

impl Site {
    fn wrap(a: &mut abi::ScatterArgs) -> &mut Site {
        unsafe { &mut *(a as *mut abi::ScatterArgs as *mut Site) }
    }
    /// The block above the ground.
    pub fn pos(&self) -> [i32; 3] {
        self.args.pos
    }
    pub fn biome(&self) -> u32 {
        self.args.biome
    }
    pub fn dimension(&self) -> u32 {
        self.args.dim
    }
    pub fn seed(&self) -> u64 {
        self.args.seed
    }
    /// Random bits of this site, the same every time it is shaped.
    pub fn random(&self) -> u64 {
        self.args.random
    }
    /// `state` at `pos` where there is air.
    pub fn put(&mut self, pos: [i32; 3], state: u32) {
        self.push(pos, state, 0);
    }
    /// `state` at `pos` whatever is there (a trunk through another tree's leaves).
    pub fn force(&mut self, pos: [i32; 3], state: u32) {
        self.push(pos, state, 1);
    }
    fn push(&mut self, pos: [i32; 3], state: u32, force: u32) {
        let a = &mut self.args;
        if a.len < a.cap {
            let offset = [pos[0] - a.pos[0], pos[1] - a.pos[1], pos[2] - a.pos[2]];
            unsafe { *a.out.add(a.len as usize) = abi::ScatterBlock { offset, state, force } };
            a.len += 1;
        }
    }
}

pub struct DecorationBuilder<'a> {
    raw: *mut abi::Registrar,
    dimensions: String,
    cb: abi::Callback,
    _p: PhantomData<&'a mut Registry>,
}

impl DecorationBuilder<'_> {
    /// Only in this dimension (call again for more).
    pub fn dimension(mut self, dimension: &Dimension) -> Self {
        self.dimensions = format!("{} {}", self.dimensions, dimension.name());
        self
    }
}

impl Drop for DecorationBuilder<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        unsafe { (host().reg_decoration)(self.raw, Str::new(&self.dimensions), self.cb) }
    }
}

impl Gen {
    /// The dimension being generated.
    pub fn dimension(&self) -> u32 {
        self.args.dim
    }

    /// The surface of the cube's 256 columns, index `z * 16 + x`: from the terrain or the
    /// dimension's `column`. None for a dimension with neither.
    pub fn columns(&self) -> Option<&[Column; 256]> {
        unsafe { self.args.columns.cast::<[Column; 256]>().as_ref() }
    }

    /// Any column of this dimension, in world coordinates.
    pub fn column(&self, x: i32, z: i32) -> Column {
        let mut out = Column { height: i32::MIN, biome: 0 };
        unsafe { (host().dim_column)(self.args.dim, self.args.seed, x, z, &mut out) };
        out
    }

    /// Whether caves took the surface block of column (`x`, `z`), in world coordinates.
    pub fn surface_carved(&self, x: i32, z: i32) -> bool {
        unsafe { (host().dim_carved)(self.args.dim, self.args.seed, x, z) != 0 }
    }
}
