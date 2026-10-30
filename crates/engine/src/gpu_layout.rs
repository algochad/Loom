//! Everything the engine's shaders and its Rust code must agree on, written once here: bit
//! layouts and mode numbers (`<GROUP>_<NAME>` constants), the structs of GPU buffers and each
//! shader's bindings. `prelude(shader)` puts them in front of the shader, so an engine shader, a
//! shader pack's replacement and a snippet only write functions.

macro_rules! shared {
    ($($(#[$doc:meta])* $group:ident { $($(#[$cdoc:meta])* $name:ident = $v:expr),* $(,)? })*) => {
        $($(#[$doc])* pub mod $group { $($(#[$cdoc])* pub const $name: u32 = $v;)* })*

        /// The constants as WGSL.
        pub fn wgsl() -> String {
            let mut s = String::new();
            $($(s.push_str(&format!("const {}_{}: u32 = {}u;\n", stringify!($group).to_uppercase(), stringify!($name), $group::$name));)*)*
            s
        }
    };
}

shared! {
    /// Block face record, four words. Word 0: x, y, z in the cube (4 bits each), direction,
    /// corner AO (2 bits per corner), tint kind. Word 1: texture, biome. Words 2 and 3: corner
    /// light, 16 bits per corner.
    face {
        POS_MASK = 15,
        Y_SHIFT = 4,
        Z_SHIFT = 8,
        DIR_SHIFT = 12,
        DIR_MASK = 7,
        AO_SHIFT = 16,
        AO_MASK = 3,
        TINT_SHIFT = 24,
        TEX_MASK = 0xFFFF,
        BIOME_SHIFT = 16,
        /// Corner light: a channel per 4 bits, a corner per 16.
        LIGHT_MASK = 15,
        CORNER_SHIFT = 16,
    }
    /// Free-form quad record, eight words: four corners (10 bits per axis, 1/16 block from -2),
    /// texture and biome (like a face's word 1), uv (5 bits each, 1/16) and tint kind with
    /// `FLAT`, corner light (like a face's).
    quad {
        POS_MASK = 1023,
        POS_BITS = 10,
        /// Added to a corner in 1/16 block: corners reach 2 blocks outside the cube.
        POS_OFFSET = 32,
        UV_MASK = 31,
        UV_BITS = 5,
        TINT_SHIFT = 24,
        /// Tint flag: no directional shading (plants).
        FLAT = 16,
    }
    /// Distant terrain record (`client::lod`), four words in the faces' pages: x, z in the tile
    /// (11 bits each), direction, tint kind; texture and biome like a face's; y (signed 16 bits)
    /// and the box's height; its width and depth.
    lod_face {
        POS_MASK = 2047,
        Z_SHIFT = 11,
        DIR_SHIFT = 22,
        TINT_SHIFT = 25,
        HALF_SHIFT = 16,
        HALF_MASK = 0xFFFF,
    }
    /// Tint kinds of faces and quads (0 none); mods' kinds follow the engine's.
    tint {
        MASK = 15,
    }
    /// Entity box modes (`entity.wgsl`).
    box_mode {
        SKIN = 0,
        FACES = 1,
        SOLID = 2,
        FLAMES = 3,
        TEXEL = 4,
        FILLED = 5,
    }
    /// UI quad modes (`ui.wgsl`).
    ui_mode {
        SOLID = 0,
        TEXTURE = 1,
        ICON = 2,
        FONT = 3,
        GUI = 4,
    }
    /// Particle data word y: texture index and shader slot.
    particle {
        TEX_MASK = 0xFFFF,
        SLOT_SHIFT = 24,
    }
}

/// Terrain on the GPU (`terrain.wgsl`, `client::terrain_gpu`): the layout of the description,
/// the noise seeds, record sizes and what comes back, for both sides.
pub mod terrain {
    use loom_noise::fixed::salt;

    shared! {
        /// Header words of the description (`terrain_gpu::describe`).
        d {
            STONE = 0, BASE = 1, HAS_SEA = 2, SEA = 3, WATER = 4, HAS_CAVES = 5, LAVA = 6,
            LAVA_FROM = 7, LAVA_TO = 8, FEATURES = 9, RULES = 10, CAVE_BIOMES = 11,
            /// The noise seeds, in `SEEDS` order.
            SEEDS = 12,
            FEAT_OFF = 26, RULE_OFF = 27, CAVE_OFF = 28,
            /// Distant terrain's scattered things (`describe_lod`).
            SCATTER_OFF = 29,
            /// The header's length.
            LEN = 30,
        }
        /// Words per record of the description, per cube's mods' heights and per cube made.
        words {
            FEATURE = 8,
            /// A mask step: kind, then five words.
            MASK = 6,
            RULE = 24,
            CAVE = 5,
            /// Per scatter and biome: threshold (two words), canopy radius, top, block.
            SCATTER_BIOME = 5,
            /// Per mod feature and cube: heights at the 256 columns and the 25 corners caves
            /// measure from.
            CUSTOM = 281,
            OUT = 2560,
        }
        /// Sizes of the shader's own arrays.
        limit {
            FEATURES = 32,
            MASK_DEPTH = 8,
        }
        /// Where things are in a cube made (`words::OUT`): blocks two per word from 0, then biomes
        /// (cave biomes included) two per word, column heights, column biomes two per word.
        out {
            BIOMES = 2048,
            HEIGHTS = 2176,
            COLUMN_BIOMES = 2432,
        }
        /// Distant terrain tiles (`client::lod`): samples per side (a tile's 32 cells and one
        /// around it), all of them.
        lod {
            SIDE = 34,
            SAMPLES = 34 * 34,
        }
        /// Mask step kinds.
        mask {
            NOISE = loom_abi::terrain_mask::NOISE,
            CONTINENT = loom_abi::terrain_mask::CONTINENT,
            TEMPERATURE = loom_abi::terrain_mask::TEMPERATURE,
            HUMIDITY = loom_abi::terrain_mask::HUMIDITY,
            ALL = loom_abi::terrain_mask::ALL,
            NOT = loom_abi::terrain_mask::NOT,
        }
        /// Feature presets.
        preset {
            CONTINENTS = loom_abi::terrain_preset::CONTINENTS,
            HILLS = loom_abi::terrain_preset::HILLS,
            MOUNTAINS = loom_abi::terrain_preset::MOUNTAINS,
            PLATEAUS = loom_abi::terrain_preset::PLATEAUS,
            CANYONS = loom_abi::terrain_preset::CANYONS,
            DUNES = loom_abi::terrain_preset::DUNES,
            CRATERS = loom_abi::terrain_preset::CRATERS,
        }
    }

    /// The noise seeds the description carries from `d::SEEDS` on, the shader's `S_<name>`:
    /// made from the salts the CPU uses.
    pub const SEEDS: [(&str, u64); 14] = [
        ("CONT", salt::CONTINENT),
        ("TEMP", salt::TEMPERATURE),
        ("HUM", salt::HUMIDITY),
        ("HILLS", salt::HILLS),
        ("MTN", salt::MOUNTAINS),
        ("PLAT", salt::PLATEAUS),
        ("CAN", salt::CANYONS),
        ("DUNE", salt::DUNES),
        ("CRAT", salt::CRATERS),
        ("C1", salt::CAVE_A),
        ("C2", salt::CAVE_B),
        ("C3", salt::CAVERN),
        ("C4", salt::CAVERN_ROOF),
        ("LAKE", salt::LAVA),
    ];

    /// What goes in front of `terrain.wgsl`: the constants, the seeds' indices, the bindings
    /// and the push constants.
    pub fn prelude() -> String {
        let mut s = wgsl();
        s.push_str(&format!("const ONE: i32 = {};
", loom_noise::fixed::ONE));
        for (i, (name, _)) in SEEDS.iter().enumerate() {
            s.push_str(&format!("const S_{name}: u32 = {i}u;
"));
        }
        s.push_str(concat!(
            "// The description (`terrain_gpu::describe`).
",
            "@group(1) @binding(0) var<storage, read> desc: array<i32>;
",
            "// Per cube: x, y, z, unused, then per mod feature `WORDS_CUSTOM` heights.
",
            "@group(1) @binding(1) var<storage, read> jobs: array<i32>;
",
            "// Per cube `WORDS_OUT` words (`OUT_*`); per distant terrain tile its samples.
",
            "@group(1) @binding(2) var<storage, read_write> outp: array<u32>;
",
            "struct Params {
    cubes: u32,
    custom: u32,
}
",
            "var<immediate> params: Params;
",
        ));
        s
    }

    /// The bindings `prelude` declares, in order.
    pub mod binding {
        pub const DESC: u32 = 0;
        pub const JOBS: u32 = 1;
        pub const OUT: u32 = 2;
    }
}

/// A unit cube's faces (-X +X -Y +Y -Z +Z), four corners each counter-clockwise seen from
/// outside: bottom-left, bottom-right, top-right, top-left in texture space. The shaders get it
/// as `CORNERS`.
pub const CORNERS: [[[i32; 3]; 4]; 6] = [
    [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]],
    [[1, 0, 1], [1, 0, 0], [1, 1, 0], [1, 1, 1]],
    [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]],
    [[0, 1, 1], [1, 1, 1], [1, 1, 0], [0, 1, 0]],
    [[1, 0, 0], [0, 0, 0], [0, 1, 0], [1, 1, 0]],
    [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]],
];

/// A block face record (`face`): (x, y, z) in the cube, `ao` 2 bits per corner, `light` per
/// corner (`corner_light`).
#[inline]
pub fn face_record(pos: [i32; 3], dir: u32, ao: u32, tint: u32, tex: u32, biome: u32, light: [u32; 4]) -> [u32; 4] {
    let w0 = pos[0] as u32 | (pos[1] as u32) << face::Y_SHIFT | (pos[2] as u32) << face::Z_SHIFT | dir << face::DIR_SHIFT | ao << face::AO_SHIFT | tint << face::TINT_SHIFT;
    [w0, tex | biome << face::BIOME_SHIFT, light[0] | light[1] << face::CORNER_SHIFT, light[2] | light[3] << face::CORNER_SHIFT]
}

/// The direction of a face record.
#[inline]
pub fn face_dir(f: &[u32; 4]) -> usize {
    ((f[0] >> face::DIR_SHIFT) & face::DIR_MASK) as usize
}

/// Corner light of a face or quad from a cell's light (8 bits per channel), 0..15 each.
#[inline]
pub fn corner_light(channels: [u32; 4]) -> u32 {
    channels.iter().enumerate().fold(0, |w, (i, &c)| w | c.min(face::LIGHT_MASK) << (i * 4))
}

/// A quad corner (`quad`), in blocks from the cube's corner.
#[inline]
pub fn quad_pos(p: [f32; 3]) -> u32 {
    let e = |v: f32| ((v * 16.0).round() as i32 + quad::POS_OFFSET as i32).clamp(0, quad::POS_MASK as i32) as u32;
    e(p[0]) | e(p[1]) << quad::POS_BITS | e(p[2]) << (2 * quad::POS_BITS)
}

/// A quad's uv word: `uv` (u0, v0, u1, v1) in 1/16, tint kind and flags.
#[inline]
pub fn quad_uv(uv: [u32; 4], tint: u32) -> u32 {
    uv[0] | uv[1] << quad::UV_BITS | uv[2] << (2 * quad::UV_BITS) | uv[3] << (3 * quad::UV_BITS) | tint << quad::TINT_SHIFT
}

/// A distant terrain record (`lod_face`): the `dir` face of a box at `pos` in the tile, `size`
/// blocks.
#[inline]
pub fn lod_record(dir: u32, pos: [i32; 3], size: [i32; 3], tex: u32, tint: u32, biome: u32) -> [u32; 4] {
    let h = lod_face::HALF_SHIFT;
    [
        pos[0] as u32 | (pos[2] as u32) << lod_face::Z_SHIFT | dir << lod_face::DIR_SHIFT | tint << lod_face::TINT_SHIFT,
        tex | biome << face::BIOME_SHIFT,
        (pos[1] as u32 & lod_face::HALF_MASK) | (size[1] as u32) << h,
        size[0] as u32 | (size[2] as u32) << h,
    ]
}

/// The WGSL side of the records above: `CORNERS` and their decoders.
fn records() -> String {
    let corners: Vec<String> = CORNERS.iter().flatten().map(|c| format!("vec3({:.1}, {:.1}, {:.1})", c[0] as f32, c[1] as f32, c[2] as f32)).collect();
    let mut s = format!("var<private> CORNERS: array<vec3<f32>, 24> = array<vec3<f32>, 24>({});\n", corners.join(", "));
    s.push_str(concat!(
        "fn face_pos(w0: u32) -> vec3<f32> {\n",
        "    return vec3<f32>(f32(w0 & FACE_POS_MASK), f32((w0 >> FACE_Y_SHIFT) & FACE_POS_MASK), f32((w0 >> FACE_Z_SHIFT) & FACE_POS_MASK));\n",
        "}\n",
        "fn face_dir(w0: u32) -> u32 {\n    return (w0 >> FACE_DIR_SHIFT) & FACE_DIR_MASK;\n}\n",
        "// AO per corner, 0 darkest to 3 none.\n",
        "fn face_ao(w0: u32) -> vec4<u32> {\n",
        "    let ao = w0 >> FACE_AO_SHIFT;\n",
        "    return vec4<u32>(ao & FACE_AO_MASK, (ao >> 2u) & FACE_AO_MASK, (ao >> 4u) & FACE_AO_MASK, (ao >> 6u) & FACE_AO_MASK);\n",
        "}\n",
        "// Corner `c`'s light from a record's two light words, the channels 0..1.\n",
        "fn corner_light(lo: u32, hi: u32, c: u32) -> vec4<f32> {\n",
        "    let l = (select(lo, hi, c >= 2u) >> ((c & 1u) * FACE_CORNER_SHIFT)) & 65535u;\n",
        "    return vec4<f32>(f32(l & FACE_LIGHT_MASK), f32((l >> 4u) & FACE_LIGHT_MASK), f32((l >> 8u) & FACE_LIGHT_MASK), f32((l >> 12u) & FACE_LIGHT_MASK)) / 15.0;\n",
        "}\n",
        "fn quad_pos(w: u32) -> vec3<f32> {\n",
        "    let p = vec3<u32>(w, w >> QUAD_POS_BITS, w >> (2u * QUAD_POS_BITS)) & vec3(QUAD_POS_MASK);\n",
        "    return (vec3<f32>(p) - f32(QUAD_POS_OFFSET)) / 16.0;\n",
        "}\n",
        "// u0, v0, u1, v1.\n",
        "fn quad_uv(w: u32) -> vec4<f32> {\n",
        "    let b = QUAD_UV_BITS;\n",
        "    return vec4<f32>(vec4<u32>(w, w >> b, w >> (2u * b), w >> (3u * b)) & vec4(QUAD_UV_MASK)) / 16.0;\n",
        "}\n",
        "fn lod_pos(w0: u32, w2: u32) -> vec3<f32> {\n",
        "    return vec3<f32>(f32(w0 & LOD_FACE_POS_MASK), f32(bitcast<i32>(w2 << LOD_FACE_HALF_SHIFT) >> LOD_FACE_HALF_SHIFT), f32((w0 >> LOD_FACE_Z_SHIFT) & LOD_FACE_POS_MASK));\n",
        "}\n",
        "fn lod_size(w2: u32, w3: u32) -> vec3<f32> {\n",
        "    return vec3<f32>(f32(w3 & LOD_FACE_HALF_MASK), f32(w2 >> LOD_FACE_HALF_SHIFT), f32(w3 >> LOD_FACE_HALF_SHIFT));\n",
        "}\n",
        "fn lod_dir(w0: u32) -> u32 {\n    return (w0 >> LOD_FACE_DIR_SHIFT) & FACE_DIR_MASK;\n}\n",
        "fn lod_tint(w0: u32) -> u32 {\n    return (w0 >> LOD_FACE_TINT_SHIFT) & TINT_MASK;\n}\n",
    ));
    s
}

/// A host-visible buffer per frame in flight (`slots`), holding at least `needed` elements of
/// `stride` bytes; a slot too small is replaced by one grown to the next power of two.
pub fn frame_buffer(gpu: &mut dyn loom_render::Backend, slots: &mut Vec<(loom_render::BufferId, u32)>, slot: usize, needed: u32, stride: u64, usage: u32, label: &str) -> loom_render::BufferId {
    use loom_render::usage::HOST;
    while slots.len() < gpu.frames_in_flight() {
        slots.push((gpu.create_buffer(stride * 1024, usage | HOST, label), 1024));
    }
    if slots[slot].1 < needed {
        gpu.destroy_buffer(slots[slot].0);
        let cap = needed.next_power_of_two();
        slots[slot] = (gpu.create_buffer(stride * cap as u64, usage | HOST, label), cap);
    }
    slots[slot].0
}

/// WGSL spelling of a field type.
pub trait Wgsl {
    const TY: &'static str;
}

macro_rules! wgsl_types {
    ($($t:ty => $s:literal),* $(,)?) => { $(impl Wgsl for $t { const TY: &'static str = $s; })* };
}
wgsl_types! {
    f32 => "f32", u32 => "u32", i32 => "i32",
    [f32; 2] => "vec2<f32>", [f32; 3] => "vec3<f32>", [f32; 4] => "vec4<f32>",
    [u32; 4] => "vec4<u32>", [i32; 4] => "vec4<i32>",
    [[f32; 4]; 3] => "array<vec4<f32>, 3>", [[f32; 4]; 4] => "mat4x4<f32>", [[f32; 4]; 6] => "array<vec4<f32>, 6>",
}

/// Declares a GPU buffer struct: the Rust type and, from the same fields, its WGSL text
/// (`wgsl()`), under the WGSL name given after `as`.
macro_rules! gpu_struct {
    ($(#[$m:meta])* pub struct $name:ident as $wgsl:literal { $($(#[$fm:meta])* pub $f:ident: $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
        pub struct $name { $($(#[$fm])* pub $f: $t),* }

        impl $name {
            pub fn wgsl() -> String {
                let mut s = format!("struct {} {{\n", $wgsl);
                $(s.push_str(&format!("    {}: {},\n", stringify!($f), <$t as Wgsl>::TY));)*
                s.push_str("}\n");
                s
            }
        }
    };
}

gpu_struct! {
    /// What every world pass sees of the frame: camera, fog, light, time. One buffer a frame
    /// for the world, entities, models and particles; the hand and the inventory figure have
    /// their own camera.
    pub struct Scene as "Scene" {
        pub view_proj: [[f32; 4]; 4],
        /// Camera right and up in world space (particle billboards).
        pub right: [f32; 4],
        pub up: [f32; 4],
        pub fog_color: [f32; 4],
        /// x fog start, y fog end, z daylight 0..1, w seconds (shader effects).
        pub params: [f32; 4],
        /// x the dimension's ambient light 0..1, y the Brightness setting 0 (moody) .. 1
        /// (bright), z game ticks with the fraction of the current one (animated textures;
        /// wraps every 2^20).
        pub light: [f32; 4],
        /// xyz toward the sun (unit), w how far it is above the horizon (0 under it or in a
        /// dimension without sky light). For shader packs: the engine draws no sun.
        pub sun: [f32; 4],
    }
}

gpu_struct! {
    /// What only the world shader needs.
    pub struct Globals as "Globals" {
        /// Camera position inside its cube (0..16) in xyz.
        pub cam_frac: [f32; 4],
        /// z biome count, w blended color tiles.
        pub ids: [u32; 4],
        /// Origins are relative to this cube.
        pub cam_cube: [i32; 4],
        /// x start of the overlay table in `frames` (per texture: overlay texture + 1, or 0),
        /// z tint kinds, w half the width of `lod_mask`.
        pub extra: [u32; 4],
    }
}

gpu_struct! {
    /// A box of an entity or a held item (`entity.wgsl`): its unit cube's placement and how it
    /// is textured (`box_mode`).
    pub struct GpuBox as "Box" {
        /// Unit cube -> camera-relative world, rows of a 3x4 matrix.
        pub rows: [[f32; 4]; 3],
        /// Box size in texels (w, h, d) for the classic unwrap; block faces: min corner in the
        /// block, w = 1 when set.
        pub dims: [f32; 4],
        /// Texel origin and texture size.
        pub uvo: [f32; 4],
        /// Tint (rgb) and hurt flash (a).
        pub color: [f32; 4],
        /// Skin texture, `box_mode`, face textures packed 16 bit.
        pub ids: [u32; 4],
        /// x last face textures, y light as the world stores it (all ones: full light); block
        /// faces: z the faces tinted by w (0xRRGGBB).
        pub ids2: [u32; 4],
    }
}

gpu_struct! {
    /// One model drawn (`model.wgsl`).
    pub struct Instance as "Instance" {
        /// First bone matrix, first box and box count, first mesh corner of its kind (filled in
        /// when queued).
        pub bones: u32,
        pub cuboids: u32,
        pub cuboid_count: u32,
        pub vertices: u32,
        /// Tint (rgb) and hurt flash.
        pub color: [f32; 3],
        pub hurt: f32,
        pub tex: u32,
        /// Extra model pixels on every side (worn layers).
        pub inflate: f32,
        pub tex_size: [f32; 2],
        /// Bit per part role this layer draws.
        pub roles: u32,
        /// Entity shader slot (`shaders/entity/<name>.wgsl`, 0 none) and glow mask texture (all
        /// ones: none).
        pub shader: u32,
        pub glow: u32,
        /// Light as the world stores it, all ones: full light.
        pub light: u32,
    }
}

gpu_struct! {
    pub struct GpuCuboid as "Cuboid" {
        /// Corner in model pixels and the bone moving it.
        pub origin: [f32; 3],
        pub bone: u32,
        pub size: [f32; 3],
        pub inflate: f32,
        pub faces: [[f32; 4]; 6],
        pub role: u32,
        // Three words, not a vec3: WGSL would align that to 16 bytes.
        pub pad0: u32,
        pub pad1: u32,
        pub pad2: u32,
    }
}

gpu_struct! {
    pub struct GpuCorner as "Corner" {
        pub pos: [f32; 3],
        pub bone: u32,
        pub uv: [f32; 2],
        pub role: u32,
        pub pad: u32,
    }
}

gpu_struct! {
    /// A bone's transform, three rows.
    pub struct GpuBone as "Bone" {
        pub rows: [[f32; 4]; 3],
    }
}

gpu_struct! {
    pub struct GpuParticle as "Particle" {
        /// Center relative to the camera, and edge length in blocks.
        pub pos_size: [f32; 4],
        /// x color 0xRRGGBBAA, y texture (16 bits) | shader slot << 24, z uv rectangle as four
        /// bytes (u0 v0 u1 v1, /255), w light: sky, red, green, blue bytes (0..15), all ones =
        /// unlit.
        pub data: [u32; 4],
        /// x life fraction 0..1.
        pub extra: [f32; 4],
    }
}

gpu_struct! {
    /// A UI quad (`ui.wgsl`).
    pub struct Quad as "Quad" {
        /// Corner, and the two edge vectors, in physical pixels.
        pub p0: [f32; 2],
        pub u: [f32; 2],
        pub v: [f32; 2],
        /// Text effect slot (top bit set on shadows) or GUI fill slot.
        pub fx: u32,
        /// Glyph number in its text.
        pub index: f32,
        pub uv: [f32; 4],
        pub color: u32,
        pub tex: u32,
        /// A texture animated as a strip of square frames: frame count | ticks per frame << 16
        /// (0: still).
        pub layer: u32,
        pub mode: u32,
        /// Visible rectangle in physical pixels; empty (all zero) means unclipped.
        pub clip: [f32; 4],
    }
}

gpu_struct! {
    pub struct Screen as "Screen" {
        /// Pixels, seconds since start, GUI scale.
        pub size: [f32; 4],
    }
}

gpu_struct! {
    /// One screen effect pass (`post.wgsl`).
    pub struct Pass as "Pass" {
        /// Bindless index of the image to read, the effect's slot, its strength and the time.
        pub scene: u32,
        pub slot: u32,
        pub strength: f32,
        pub time: f32,
        pub size: [f32; 2],
        pub pad: [f32; 2],
    }
}

/// Group 1 of each engine shader, in order: a constant per binding (`bind::world::RECORDS`),
/// the WGSL name and type the prelude declares.
macro_rules! bindings {
    ($($module:ident $file:literal { $($konst:ident $name:literal: $ty:literal),* $(,)? })*) => {
        /// Binding numbers of each engine shader's group 1, by name.
        pub mod bind {
            $(pub mod $module { bindings!(@number 0u32; $($konst)*); })*
        }

        fn bindings(shader: &str) -> &'static [(&'static str, &'static str)] {
            match shader {
                $($file => &[$(($name, $ty)),*],)*
                _ => &[],
            }
        }
    };
    (@number $n:expr; $first:ident $($rest:ident)*) => {
        pub const $first: u32 = $n;
        bindings!(@number $n + 1; $($rest)*);
    };
    (@number $n:expr;) => {};
}

bindings! {
    world "world.wgsl" {
        G "g": "Globals",
        ORIGINS "origins": "array<vec4<i32>>",
        RECORDS "records": "array<u32>",
        // Per biome a color per tint kind (`g.extra.z` kinds), 0xRRGGBB. Then the blended
        // colors of loaded block columns, 0xFFRRGGBB, in 16x16 tiles addressed by column
        // position modulo g.ids.w.
        BIOMES "biomes": "array<u32>",
        // Two words per texture, see `GameTextures::world_tables`.
        TEX_INFO "tex_info": "array<u32>",
        // Animated frames: texture | ticks << 16; then the overlay table.
        FRAMES "frames": "array<u32>",
        // Camera-relative positions of moving structures drawn this frame (0: none, the
        // world's cubes): a cube's `origin.w` picks one, its `origin.xyz` is then the cube in
        // the structure.
        OFFSETS "offsets": "array<vec4<f32>>",
        // Glow mask texture per bindless texture index, all ones: none.
        GLOW_MAP "glow_map": "array<u32>",
        // Block shader slot (`shaders/block/<name>.wgsl`) per texture id, 0 none.
        TEX_SHADER "tex_shader": "array<u32>",
        SCENE "scene": "Scene",
        // A bit per cube column around the camera's, addressed modulo 2 * g.extra.w + 1:
        // its cubes show its ground, distant terrain is not drawn there (`client::lod`).
        LOD_MASK "lod_mask": "array<u32>",
        // Which blocks around the camera cast shadows, for shader packs that call `solid_at`
        // (`client::world_render::Solid`); a single word otherwise.
        SOLID "solid": "array<u32>",
    }
    // `item_boxes`: item models in their own space, kept between frames
    // (`EntityRenderer::item_model`); an item instance is a box in `boxes` whose rows place
    // the model, `ids.z` its first box, `color.a` the hurt flash and `ids2.y` the light.
    entity "entity.wgsl" {
        BOXES "boxes": "array<Box>",
        SCENE "scene": "Scene",
        GLOW_MAP "glow_map": "array<u32>",
        ITEM_BOXES "item_boxes": "array<Box>",
    }
    model "model.wgsl" {
        INSTANCES "instances": "array<Instance>",
        SCENE "scene": "Scene",
        CUBOIDS "cuboids": "array<Cuboid>",
        CORNERS "corners": "array<Corner>",
        BONES "bones": "array<Bone>",
    }
    particle "particle.wgsl" {
        PARTICLES "particles": "array<Particle>",
        SCENE "scene": "Scene",
    }
    ui "ui.wgsl" {
        QUADS "quads": "array<Quad>",
        SCREEN "screen": "Screen",
    }
    post "post.wgsl" {
        PASSES "passes": "array<Pass>",
    }
}

/// Whether the block at `p` (block coordinates) casts a shadow, as far as the client has it
/// within about 112 blocks of the camera; false beyond, for unloaded cubes and while no shader
/// pack asks (`client::world_render::Solid`).
const SOLID_AT: &str = "
fn solid_at(p: vec3<i32>) -> bool {
    if arrayLength(&solid) < 4096u * 132u {
        return false;
    }
    let c = p >> vec3(4u);
    let s = u32(c.y & 15) * 256u + u32(c.z & 15) * 16u + u32(c.x & 15);
    if solid[s * 4u + 3u] == 0u || bitcast<i32>(solid[s * 4u]) != c.x || bitcast<i32>(solid[s * 4u + 1u]) != c.y || bitcast<i32>(solid[s * 4u + 2u]) != c.z {
        return false;
    }
    let l = p & vec3(15);
    let i = u32((l.y * 16 + l.z) * 16 + l.x);
    return ((solid[4096u * 4u + s * 128u + i / 32u] >> (i % 32u)) & 1u) != 0u;
}
";

/// Every GPU struct as WGSL.
fn structs() -> String {
    [Scene::wgsl(), Globals::wgsl(), GpuBox::wgsl(), Instance::wgsl(), GpuCuboid::wgsl(), GpuCorner::wgsl(), GpuBone::wgsl(), GpuParticle::wgsl(), Quad::wgsl(), Screen::wgsl(), Pass::wgsl()].concat()
}

/// What goes in front of an engine shader (or a pack's replacement of it): the constants, every
/// GPU struct, the shader's bindings, then `common` (lighting, fog, samplers and the structs
/// snippets get).
pub fn prelude(shader: &str, common: &str) -> String {
    let mut s = wgsl();
    s.push_str(&records());
    s.push_str(&structs());
    for (i, (name, ty)) in bindings(shader).iter().enumerate() {
        s.push_str(&format!("@group(1) @binding({i}) var<storage, read> {name}: {ty};\n"));
    }
    if shader == "world.wgsl" {
        s.push_str(SOLID_AT);
    }
    s.push_str(common);
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records pack into the fields the constants say, and the prelude's decoders of them
    /// compile against the same constants (the pictures of the visual tests check the rest).
    #[test]
    fn records_round_trip() {
        let f = face_record([3, 14, 9], 5, 0b10_01_11_00, 7, 1234, 42, [0x1234, 0xFFFF, 0, 0xABCD]);
        assert_eq!(face_dir(&f), 5);
        assert_eq!(f[0] & face::POS_MASK, 3);
        assert_eq!((f[0] >> face::Y_SHIFT) & face::POS_MASK, 14);
        assert_eq!((f[0] >> face::Z_SHIFT) & face::POS_MASK, 9);
        assert_eq!((f[0] >> face::TINT_SHIFT) & tint::MASK, 7);
        assert_eq!(f[1] & face::TEX_MASK, 1234);
        assert_eq!(corner_light([200, 3, 15, 16]), 15 | 3 << 4 | 15 << 8 | 15 << 12);
        let q = quad_pos([-2.0, 0.5, 61.9375]);
        assert_eq!([q & quad::POS_MASK, (q >> quad::POS_BITS) & quad::POS_MASK, q >> (2 * quad::POS_BITS)], [0, 40, 1023]);
        let l = lod_record(4, [2047, -300, 5], [16, 7, 32], 9, 3, 2);
        assert_eq!(l[0] & lod_face::POS_MASK, 2047);
        assert_eq!((l[0] >> lod_face::Z_SHIFT) & lod_face::POS_MASK, 5);
        assert_eq!((l[0] >> lod_face::DIR_SHIFT) & face::DIR_MASK, 4);
        assert_eq!(((l[2] << 16) as i32) >> 16, -300);
        assert_eq!([l[3] & 0xFFFF, l[2] >> 16, l[3] >> 16], [16, 7, 32]);
        let src = format!(
            "{}{}const F0: u32 = {}u;\nconst Q: u32 = {}u;\nconst L0: u32 = {}u;\nconst L2: u32 = {}u;\nconst L3: u32 = {}u;\n\
             @compute @workgroup_size(1) fn main() {{ _ = face_pos(F0); _ = face_dir(F0); _ = face_ao(F0); _ = quad_pos(Q); _ = lod_pos(L0, L2); _ = lod_size(L2, L3); _ = corner_light(1u, 2u, 3u); _ = quad_uv(Q); _ = lod_dir(L0); _ = lod_tint(L0); }}\n",
            wgsl(),
            records(),
            f[0],
            q,
            l[0],
            l[2],
            l[3]
        );
        let module = naga::front::wgsl::parse_str(&src).expect("records parse");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all()).validate(&module).expect("records validate");
    }

    /// The terrain prelude parses, and the header has room for every seed.
    #[test]
    fn terrain_prelude() {
        use terrain::*;
        assert_eq!(d::SEEDS as usize + SEEDS.len(), d::FEAT_OFF as usize);
        assert!(d::SCATTER_OFF < d::LEN);
        naga::front::wgsl::parse_str(&prelude()).expect("terrain prelude parses");
    }

    /// naga's layout of every generated struct matches the Rust one, so a field the WGSL rules
    /// align differently (a lone vec3) can't slip in.
    #[test]
    fn layouts_match_rust() {
        let src = format!("{}{}{}", wgsl(), records(), structs());
        let module = naga::front::wgsl::parse_str(&src).expect("structs parse");
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).unwrap();
        let sizes = [
            ("Scene", size_of::<Scene>()),
            ("Globals", size_of::<Globals>()),
            ("Box", size_of::<GpuBox>()),
            ("Instance", size_of::<Instance>()),
            ("Cuboid", size_of::<GpuCuboid>()),
            ("Corner", size_of::<GpuCorner>()),
            ("Bone", size_of::<GpuBone>()),
            ("Particle", size_of::<GpuParticle>()),
            ("Quad", size_of::<Quad>()),
            ("Screen", size_of::<Screen>()),
            ("Pass", size_of::<Pass>()),
        ];
        for (name, rust) in sizes {
            let (h, _) = module.types.iter().find(|(_, t)| t.name.as_deref() == Some(name)).expect(name);
            assert_eq!(layouter[h].size as usize, rust, "{name}");
        }
    }
}
