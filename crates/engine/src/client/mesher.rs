//! Turns a cube plus a one-block border into GPU records.
//!
//! Full block faces: 4 words (pos/dir/ao, texture/biome/tint, light for 4 corners).
//! Everything else (plants, boxes, liquids): 8-word quads with explicit corners.

use crate::gpu_layout::{face, quad};
use loom_abi::{Layer, Model, block_flags as bf};

use crate::content::Content;

pub const PAD: usize = 18;
pub const PAD_VOLUME: usize = PAD * PAD * PAD;

#[inline]
pub const fn pidx(x: i32, y: i32, z: i32) -> usize {
    (((y + 1) as usize * PAD) + (z + 1) as usize) * PAD + (x + 1) as usize
}

/// Input of one meshing job, copied out of the client world so it can run on any thread.
pub struct Neighborhood {
    pub states: Box<[u32; PAD_VOLUME]>,
    pub light: Box<[u32; PAD_VOLUME]>,
    /// Biome id per column of the center cube.
    pub biomes: [u16; 256],
    /// World position of the cube's first block.
    pub origin: [i32; 3],
}

#[derive(Default)]
pub struct Mesh {
    /// Per layer (opaque, cutout, translucent).
    pub faces: [Vec<[u32; 4]>; 3],
    pub quads: [Vec<[u32; 8]>; 3],
}

impl Mesh {
    pub fn is_empty(&self) -> bool {
        self.faces.iter().all(Vec::is_empty) && self.quads.iter().all(Vec::is_empty)
    }
}

use crate::gpu_layout::CORNERS;

const NORMALS: [[i32; 3]; 6] = [[-1, 0, 0], [1, 0, 0], [0, -1, 0], [0, 1, 0], [0, 0, -1], [0, 0, 1]];

#[cfg(test)]
pub fn corners(dir: usize) -> [[i32; 3]; 4] {
    CORNERS[dir]
}

#[inline]
fn light16(l: u32) -> u32 {
    crate::gpu_layout::corner_light([l & 0xFF, (l >> 8) & 0xFF, (l >> 16) & 0xFF, l >> 24])
}

pub fn build(c: &Content, n: &Neighborhood, ctm: &super::ctm::Ctm) -> Mesh {
    let mut mesh = Mesh::default();
    let states = &n.states;
    let looks = super::looks::current();
    // Rows along x of the padded volume (bit x + 1): blocks, and opaque ones. A block with an
    // opaque block on every side shows nothing, and underground that is nearly all of them.
    let mut filled = [0u32; PAD * PAD];
    let mut solid = [0u32; PAD * PAD];
    for (row, (f, o)) in filled.iter_mut().zip(solid.iter_mut()).enumerate() {
        for (x, &s) in states[row * PAD..(row + 1) * PAD].iter().enumerate() {
            if s != 0 {
                *f |= 1 << x;
                if c.tight[s as usize].flags & bf::OPAQUE != 0 {
                    *o |= 1 << x;
                }
            }
        }
    }
    let row = |y: i32, z: i32| ((y + 1) as usize) * PAD + (z + 1) as usize;
    // Padded index to its bit in `solid`.
    let opaque = |i: usize| solid[i / PAD] >> (i % PAD) & 1 != 0;

    for y in 0..16i32 {
        for z in 0..16i32 {
            let o = solid[row(y, z)];
            let enclosed = (o << 1) & (o >> 1) & solid[row(y - 1, z)] & solid[row(y + 1, z)] & solid[row(y, z - 1)] & solid[row(y, z + 1)];
            let mut todo = filled[row(y, z)] & !enclosed & 0x1FFFE;
            while todo != 0 {
                let x = todo.trailing_zeros() as i32 - 1;
                todo &= todo - 1;
                let i = pidx(x, y, z);
                let s = states[i];
                let info = &c.states[s as usize];
                let biome = n.biomes[(z * 16 + x) as usize] as u32;
                if info.liquid.is_some() {
                    liquid(c, n, &mut mesh, x, y, z, biome);
                    continue;
                }
                // A resource pack may draw it differently (its shape and light stay the mod's).
                let look = looks.state(s);
                let boxes: &[crate::content::ModelBox] = look.and_then(|l| l.boxes.as_deref()).unwrap_or(&info.boxes);
                let textures = look.and_then(|l| l.textures).unwrap_or(info.textures);
                let model = if look.is_some_and(|l| l.boxes.is_some()) { Model::Boxes } else { info.model };
                match model {
                    Model::None => {}
                    Model::Cube => {
                        for dir in 0..6 {
                            let [dx, dy, dz] = NORMALS[dir];
                            let ni = pidx(x + dx, y + dy, z + dz);
                            let ns = states[ni];
                            let ninfo = &c.states[ns as usize];
                            if ninfo.flags & bf::OPAQUE != 0 {
                                continue;
                            }
                            if ninfo.block == info.block && info.layer != Layer::Opaque && info.flags & bf::CULL_SAME != 0 {
                                continue;
                            }
                            let (ao, light) = corner_data(n, &opaque, x, y, z, dir);
                            let tex = ctm.resolve(c, n, x, y, z, dir, textures[dir] & face::TEX_MASK);
                            let tint = if info.tint_faces & (1 << dir) != 0 || ctm.has_overlay(tex) { info.tint as u32 } else { 0 };
                            mesh.faces[info.layer as usize].push(crate::gpu_layout::face_record([x, y, z], dir as u32, ao, tint, tex, biome, light));
                        }
                    }
                    Model::Cross => {
                        let l = light16(n.light[i]);
                        let t = textures[0];
                        let b = [x as f32, y as f32, z as f32];
                        let e = 0.15;
                        let (a0, a1) = ([b[0] + e, b[1], b[2] + e], [b[0] + 1.0 - e, b[1] + 1.0, b[2] + 1.0 - e]);
                        let (c0, c1) = ([b[0] + e, b[1], b[2] + 1.0 - e], [b[0] + 1.0 - e, b[1] + 1.0, b[2] + e]);
                        for (p0, p1) in [(a0, a1), (c0, c1)] {
                            let corners = [
                                [p0[0], p0[1], p0[2]],
                                [p1[0], p0[1], p1[2]],
                                [p1[0], p1[1], p1[2]],
                                [p0[0], p1[1], p0[2]],
                            ];
                            let back = [corners[1], corners[0], corners[3], corners[2]];
                            for q in [corners, back] {
                                mesh.quads[Layer::Cutout as usize].push(quad(q, [0, 0, 16, 16], t, biome, info.tint | FLAT, [l; 4]));
                            }
                        }
                    }
                    Model::Boxes => {
                        let l = light16(n.light[i]);
                        for bx in boxes {
                            for dir in 0..6 {
                                let [dx, dy, dz] = NORMALS[dir];
                                let axis = dir / 2;
                                let at_edge = if dir % 2 == 0 { bx.min[axis] <= 0.0 } else { bx.max[axis] >= 1.0 };
                                let (ns, nl) = (states[pidx(x + dx, y + dy, z + dz)], n.light[pidx(x + dx, y + dy, z + dz)]);
                                if at_edge && c.states[ns as usize].flags & bf::OPAQUE != 0 {
                                    continue;
                                }
                                let fl = if at_edge { light16(nl) } else { l };
                                let corners = CORNERS[dir].map(|k| {
                                    [
                                        x as f32 + if k[0] == 1 { bx.max[0] } else { bx.min[0] },
                                        y as f32 + if k[1] == 1 { bx.max[1] } else { bx.min[1] },
                                        z as f32 + if k[2] == 1 { bx.max[2] } else { bx.min[2] },
                                    ]
                                });
                                if bx.uv[dir][0] <= crate::block_model::NO_FACE {
                                    continue;
                                }
                                let uv = match bx.uv[dir] {
                                    r if r[0] >= 0.0 => r.map(|v| v.round().clamp(0.0, 31.0) as u32),
                                    _ => box_uv(dir, bx.min, bx.max),
                                };
                                mesh.quads[info.layer as usize].push(quad(corners, uv, bx.textures[dir], biome, info.tint, [fl; 4]));
                            }
                        }
                    }
                }
            }
        }
    }
    // Grouped by direction so the renderer can skip faces turned away from the camera.
    for faces in &mut mesh.faces {
        faces.sort_by_key(|f| DIR_RANK[crate::gpu_layout::face_dir(f)]);
    }
    mesh
}

/// Order of face directions in a mesh: all positive, then all negative, so the directions seen
/// from one octant are mostly one contiguous range.
pub const DIR_RANK: [u8; 6] = [3, 0, 4, 1, 5, 2];

fn box_uv(dir: usize, min: [f32; 3], max: [f32; 3]) -> [u32; 4] {
    let p = |v: f32| (v * 16.0).round().clamp(0.0, 16.0) as u32;
    // Texture u/v follow the same axes as the face corners.
    match dir {
        0 => [p(min[2]), 16 - p(max[1]), p(max[2]), 16 - p(min[1])],
        1 => [16 - p(max[2]), 16 - p(max[1]), 16 - p(min[2]), 16 - p(min[1])],
        2 => [p(min[0]), p(min[2]), p(max[0]), p(max[2])],
        3 => [p(min[0]), p(min[2]), p(max[0]), p(max[2])],
        4 => [16 - p(max[0]), 16 - p(max[1]), 16 - p(min[0]), 16 - p(min[1])],
        _ => [p(min[0]), 16 - p(max[1]), p(max[0]), 16 - p(min[1])],
    }
}

fn corner_data(n: &Neighborhood, opaque: &impl Fn(usize) -> bool, x: i32, y: i32, z: i32, dir: usize) -> (u32, [u32; 4]) {
    let nrm = NORMALS[dir];
    let axis = dir / 2;
    let (ta, tb) = match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    };
    let base = [x + nrm[0], y + nrm[1], z + nrm[2]];
    let mut ao_bits = 0u32;
    let mut lights = [0u32; 4];
    for (k, corner) in CORNERS[dir].iter().enumerate() {
        let sa = corner[ta] * 2 - 1;
        let sb = corner[tb] * 2 - 1;
        let mut p1 = base;
        p1[ta] += sa;
        let mut p2 = base;
        p2[tb] += sb;
        let mut p3 = base;
        p3[ta] += sa;
        p3[tb] += sb;
        let i0 = pidx(base[0], base[1], base[2]);
        let i1 = pidx(p1[0], p1[1], p1[2]);
        let i2 = pidx(p2[0], p2[1], p2[2]);
        let i3 = pidx(p3[0], p3[1], p3[2]);
        let (s1, s2, s3) = (opaque(i1), opaque(i2), opaque(i3));
        let ao = if s1 && s2 { 0 } else { 3 - (s1 as u32 + s2 as u32 + s3 as u32) };
        ao_bits |= ao << (k * 2);

        let mut sum = [0u32; 4];
        let mut count = 0;
        for (idx, solid) in [(i0, false), (i1, s1), (i2, s2), (i3, s3 || (s1 && s2))] {
            if solid {
                continue;
            }
            let l = n.light[idx];
            for (ch, s) in sum.iter_mut().enumerate() {
                *s += (l >> (ch * 8)) & 0xFF;
            }
            count += 1;
        }
        lights[k] = crate::gpu_layout::corner_light(sum.map(|v| (v + count / 2) / count.max(1)));
    }
    (ao_bits, lights)
}

const FLAT: u8 = quad::FLAT as u8;

fn quad(corners: [[f32; 3]; 4], uv: [u32; 4], texture: u32, biome: u32, tint: u8, light: [u32; 4]) -> [u32; 8] {
    use crate::gpu_layout::{quad_pos, quad_uv};
    [
        quad_pos(corners[0]),
        quad_pos(corners[1]),
        quad_pos(corners[2]),
        quad_pos(corners[3]),
        texture & face::TEX_MASK | biome << face::BIOME_SHIFT,
        quad_uv(uv, tint as u32),
        light[0] | light[1] << face::CORNER_SHIFT,
        light[2] | light[3] << face::CORNER_SHIFT,
    ]
}

fn liquid(c: &Content, n: &Neighborhood, mesh: &mut Mesh, x: i32, y: i32, z: i32, biome: u32) {
    let i = pidx(x, y, z);
    let s = n.states[i];
    let info = &c.states[s as usize];
    let block = info.block;
    let same = |xx: i32, yy: i32, zz: i32| c.states[n.states[pidx(xx, yy, zz)] as usize].block == block;
    let height = |xx: i32, zz: i32| -> Option<f32> {
        if !same(xx, y, zz) {
            return None;
        }
        if same(xx, y + 1, zz) {
            return Some(1.0);
        }
        let lvl = c.states[n.states[pidx(xx, y, zz)] as usize].level.min(7) as f32;
        Some((8.0 - lvl) / 9.0)
    };
    let corner_h = |cx: i32, cz: i32| -> f32 {
        // Corner (cx, cz) in {0,1}: average over the up to four columns sharing it.
        let mut sum = 0.0;
        let mut cnt = 0.0;
        for (ox, oz) in [(cx - 1, cz - 1), (cx, cz - 1), (cx - 1, cz), (cx, cz)] {
            if let Some(h) = height(x + ox, z + oz) {
                if h >= 1.0 {
                    return 1.0;
                }
                sum += h;
                cnt += 1.0;
            }
        }
        sum / cnt
    };
    let h = [corner_h(0, 0), corner_h(1, 0), corner_h(1, 1), corner_h(0, 1)];
    let hat = |cx: i32, cz: i32| match (cx, cz) {
        (0, 0) => h[0],
        (1, 0) => h[1],
        (1, 1) => h[2],
        _ => h[3],
    };
    let l = light16(n.light[i]);
    let t = info.textures[3];
    let ts = info.textures[0];
    let (fx, fy, fz) = (x as f32, y as f32, z as f32);
    let layer = info.layer as usize;
    if !same(x, y + 1, z) {
        let corners = [
            [fx, fy + hat(0, 1), fz + 1.0],
            [fx + 1.0, fy + hat(1, 1), fz + 1.0],
            [fx + 1.0, fy + hat(1, 0), fz],
            [fx, fy + hat(0, 0), fz],
        ];
        let lu = light16(n.light[pidx(x, y + 1, z)]).max(l);
        mesh.quads[layer].push(quad(corners, [0, 0, 16, 16], t, biome, info.tint, [lu; 4]));
        let back = [corners[1], corners[0], corners[3], corners[2]];
        mesh.quads[layer].push(quad(back, [0, 0, 16, 16], t, biome, info.tint, [lu; 4]));
    }
    for dir in [0usize, 1, 4, 5] {
        let [dx, _, dz] = NORMALS[dir];
        if same(x + dx, y, z + dz) {
            continue;
        }
        let ni = pidx(x + dx, y, z + dz);
        if c.states[n.states[ni] as usize].flags & bf::OPAQUE != 0 {
            continue;
        }
        let k = CORNERS[dir];
        let corners = k.map(|p| {
            let top = if p[1] == 1 { hat(p[0], p[2]) } else { 0.0 };
            [fx + p[0] as f32, fy + top, fz + p[2] as f32]
        });
        let nl = light16(n.light[ni]);
        mesh.quads[layer].push(quad(corners, [0, 0, 16, 16], ts, biome, info.tint, [nl; 4]));
    }
    let below = pidx(x, y - 1, z);
    if !same(x, y - 1, z) && c.states[n.states[below] as usize].flags & bf::OPAQUE == 0 {
        let corners = CORNERS[2].map(|p| [fx + p[0] as f32, fy, fz + p[2] as f32]);
        mesh.quads[layer].push(quad(corners, [0, 0, 16, 16], t, biome, info.tint, [light16(n.light[below]); 4]));
    }
}
