//! Entity models and keyframe animations from files, in the Bedrock formats Blockbench writes
//! (`models/<name>.geo.json`, `animations/<name>.animation.json`), and posing a model's bones.
//!
//! Coordinates: model pixels, y up from the feet, the model looks along -Z, positions as in the
//! files. Rotations in the files are degrees about x, y, z; the engine applies x first, then y,
//! then z (Java ModelPart order) and flips the sign of x and z, which is how Blockbench maps
//! Bedrock angles onto a y-up frame (a Bedrock arm raised with x = -90 points forward).

use glam::{EulerRot, Mat4, Quat, Vec3};
use serde_json::Value;
use loom_abi::{MeshVertex, worn_parts};

use crate::registry::{Cuboid, Part};

fn num(v: &Value) -> f32 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0) as f32,
        // Molang expressions are not run; a plain number in a string still is one.
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn vec3(v: Option<&Value>) -> Option<[f32; 3]> {
    match v? {
        Value::Array(a) if a.len() >= 3 => Some([num(&a[0]), num(&a[1]), num(&a[2])]),
        n @ (Value::Number(_) | Value::String(_)) => {
            let x = num(n);
            Some([x; 3])
        }
        _ => None,
    }
}

fn vec2(v: Option<&Value>) -> Option<[f32; 2]> {
    match v? {
        Value::Array(a) if a.len() >= 2 => Some([num(&a[0]), num(&a[1])]),
        _ => None,
    }
}

/// Degrees from a file to the engine's radians (see the module docs).
fn angles(r: [f32; 3]) -> [f32; 3] {
    [-r[0].to_radians(), r[1].to_radians(), -r[2].to_radians()]
}

/// How a bone moves by itself, worked out from its name (`role_of`) so code models, model files
/// and animations agree.
pub mod role {
    pub const STATIC: u32 = 0;
    pub const HEAD: u32 = 1;
    pub const ARM_LEFT: u32 = 2;
    pub const ARM_RIGHT: u32 = 3;
    pub const LEG_LEFT: u32 = 4;
    pub const LEG_RIGHT: u32 = 5;
    /// Quadrupeds: front legs swing opposite to back legs.
    pub const LEG_FRONT_LEFT: u32 = 6;
    pub const LEG_FRONT_RIGHT: u32 = 7;
    /// Legs that sweep sideways while walking (spiders), in opposite phase.
    pub const LEG_SWEEP_A: u32 = 10;
    pub const LEG_SWEEP_B: u32 = 11;
}

/// The role vanilla's code gives a bone of this name (Bedrock's names, as Blockbench writes
/// them), so walking and looking work on any model without keyframes. `legs`: how many
/// `leg<n>` bones the model has; more than four are a spider's.
pub fn role_of(name: &str, legs: usize) -> u32 {
    let n = name.to_ascii_lowercase().replace('_', "");
    if let Some(i) = n.strip_prefix("leg").and_then(|i| i.parse::<u32>().ok()).filter(|_| legs > 4) {
        // Spider legs: pairs left, right from the back; neighbors and opposite sides alternate.
        return if (i / 2 + i % 2) % 2 == 0 { role::LEG_SWEEP_A } else { role::LEG_SWEEP_B };
    }
    match n.as_str() {
        "head" => role::HEAD,
        "rightarm" => role::ARM_LEFT,
        "leftarm" => role::ARM_RIGHT,
        "rightleg" | "leg1" => role::LEG_LEFT,
        "leftleg" | "leg0" => role::LEG_RIGHT,
        "leg2" => role::LEG_FRONT_RIGHT,
        "leg3" => role::LEG_FRONT_LEFT,
        _ => role::STATIC,
    }
}

/// Every part's role from its name.
pub fn assign_roles(parts: &mut [Part]) {
    let legs = parts.iter().filter(|p| p.name.to_ascii_lowercase().strip_prefix("leg").is_some_and(|i| i.parse::<u32>().is_ok())).count();
    for p in parts {
        p.role = role_of(&p.name, legs);
    }
}

/// The role bits (`1 << role`) of the model regions a worn item covers (`worn_parts`).
pub fn role_mask(regions: u32) -> u32 {
    let mut m = 0;
    if regions & worn_parts::HEAD != 0 {
        m |= 1 << role::HEAD;
    }
    if regions & worn_parts::BODY != 0 {
        m |= 1 << role::STATIC;
    }
    if regions & worn_parts::ARMS != 0 {
        m |= 1 << role::ARM_LEFT | 1 << role::ARM_RIGHT;
    }
    if regions & worn_parts::LEGS != 0 {
        m |= 1 << role::LEG_LEFT | 1 << role::LEG_RIGHT | 1 << role::LEG_FRONT_LEFT | 1 << role::LEG_FRONT_RIGHT;
    }
    m
}

/// A model file's parts and the texture size it was made for.
pub struct Geo {
    pub parts: Vec<Part>,
    pub texture: Option<[u32; 2]>,
}

/// The first geometry of a `.geo.json` (format 1.12+, or the old 1.8 `geometry.name` keys) as
/// model parts: bones with their boxes and poly meshes.
pub fn load_geo(src: &str) -> Result<Geo, String> {
    let root: Value = serde_json::from_str(src).map_err(|e| e.to_string())?;
    let geo = match root.get("minecraft:geometry") {
        Some(Value::Array(list)) => list.first().cloned(),
        _ => root.as_object().and_then(|o| o.iter().find(|(k, _)| k.starts_with("geometry.")).map(|(_, v)| v.clone())),
    }
    .ok_or("no geometry in the file")?;
    let bones = geo.get("bones").and_then(Value::as_array).ok_or("geometry without bones")?;
    let desc = geo.get("description").unwrap_or(&geo);
    let size = |k: &str| desc.get(k).and_then(Value::as_u64).map(|v| v as u32);
    let texture = size("texture_width").zip(size("texture_height")).map(|(w, h)| [w, h]);
    let tex = texture.map_or([64.0, 64.0], |t| [t[0] as f32, t[1] as f32]);
    let names: Vec<String> = bones.iter().map(|b| b.get("name").and_then(Value::as_str).unwrap_or("").to_string()).collect();
    let mut parts = Vec::new();
    for b in bones {
        let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let parent = b.get("parent").and_then(Value::as_str).and_then(|p| names.iter().position(|n| n == p)).map(|i| i as u32);
        let pivot = vec3(b.get("pivot")).unwrap_or([0.0; 3]);
        let rest = angles(vec3(b.get("rotation")).unwrap_or([0.0; 3]));
        let bone_mirror = b.get("mirror").and_then(Value::as_bool).unwrap_or(false);
        let bone_inflate = b.get("inflate").map(num).unwrap_or(0.0);
        let mut cuboids = Vec::new();
        let mut mesh = Vec::new();
        for c in b.get("cubes").and_then(Value::as_array).into_iter().flatten() {
            let origin = vec3(c.get("origin")).unwrap_or([0.0; 3]);
            let size = vec3(c.get("size")).unwrap_or([1.0; 3]);
            let inflate = c.get("inflate").map(num).unwrap_or(bone_inflate);
            let mirror = c.get("mirror").and_then(Value::as_bool).unwrap_or(bone_mirror);
            let rel = [origin[0] - pivot[0], origin[1] - pivot[1], origin[2] - pivot[2]];
            let mut cub = Cuboid { origin: rel, size, uv: [0.0; 2], inflate, mirror, faces: None };
            match c.get("uv") {
                Some(Value::Object(faces)) => {
                    // Per face; Blockbench writes "down" flipped in v compared with the box unwrap.
                    let mut rects = [[0.0f32; 4]; 6];
                    for (i, face) in ["east", "west", "down", "up", "north", "south"].into_iter().enumerate() {
                        let Some(f) = faces.get(face) else { continue };
                        let uv = vec2(f.get("uv")).unwrap_or([0.0; 2]);
                        let sz = vec2(f.get("uv_size")).unwrap_or([0.0; 2]);
                        rects[i] = if face == "down" { [uv[0], uv[1] + sz[1], sz[0], -sz[1]] } else { [uv[0], uv[1], sz[0], sz[1]] };
                    }
                    cub.faces = Some(rects);
                }
                v => cub.uv = vec2(v).unwrap_or([0.0; 2]),
            }
            if vec3(c.get("rotation")).is_some_and(|r| r != [0.0; 3]) {
                log::warn!("bone {name}: rotated cubes are drawn unrotated; put them in a rotated bone");
            }
            cuboids.push(cub);
        }
        if let Some(pm) = b.get("poly_mesh") {
            mesh = poly_mesh(pm, tex);
        }
        parts.push(Part { role: 0, name, parent, pivot, rest, cuboids, mesh });
    }
    assign_roles(&mut parts);
    Ok(Geo { parts, texture })
}

/// Bedrock `poly_mesh`: positions, uvs and polys of 3 or 4 corners `[position, normal, uv]`,
/// made into triangles. With `normalized_uvs` the uvs are 0..1 of a texture `tex` texels big.
fn poly_mesh(pm: &Value, tex: [f32; 2]) -> Vec<MeshVertex> {
    let list = |k: &str| pm.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let positions: Vec<[f32; 3]> = list("positions").iter().filter_map(|p| vec3(Some(p))).collect();
    let uvs: Vec<[f32; 2]> = list("uvs").iter().filter_map(|p| vec2(Some(p))).collect();
    let normalized = pm.get("normalized_uvs").and_then(Value::as_bool).unwrap_or(false);
    let polys = match pm.get("polys") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for poly in polys {
        let corners: Vec<MeshVertex> = poly
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let c = c.as_array()?;
                let p = *positions.get(num(c.first()?) as usize)?;
                let mut uv = c.get(2).and_then(|i| uvs.get(num(i) as usize)).copied().unwrap_or([0.0; 2]);
                if normalized {
                    // Normalized uvs count from the bottom of the texture.
                    uv = [uv[0] * tex[0], (1.0 - uv[1]) * tex[1]];
                }
                Some(MeshVertex { pos: p, uv })
            })
            .collect();
        if corners.len() >= 3 {
            out.extend([corners[0], corners[1], corners[2]]);
        }
        if corners.len() == 4 {
            out.extend([corners[0], corners[2], corners[3]]);
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Prop {
    Rotation,
    Position,
    Scale,
}

#[derive(Clone, Debug)]
struct Key {
    time: f32,
    pre: [f32; 3],
    post: [f32; 3],
    smooth: bool,
}

#[derive(Clone, Debug)]
pub struct Channel {
    pub bone: String,
    pub prop: Prop,
    keys: Vec<Key>,
}

#[derive(Clone, Debug)]
pub struct Animation {
    /// Short name: the part after the last dot of `animation.<model>.<name>`.
    pub name: String,
    pub length: f32,
    pub looping: bool,
    /// Stays on its last frame when done (`"loop": "hold_on_last_frame"`).
    pub hold: bool,
    pub channels: Vec<Channel>,
}

/// Every animation in a `.animation.json`.
pub fn load_animations(src: &str) -> Result<Vec<Animation>, String> {
    let root: Value = serde_json::from_str(src).map_err(|e| e.to_string())?;
    let anims = root.get("animations").and_then(Value::as_object).ok_or("no animations in the file")?;
    let mut out = Vec::new();
    for (full, a) in anims {
        let name = full.rsplit('.').next().unwrap_or(full).to_string();
        let looping = a.get("loop") == Some(&Value::Bool(true));
        let hold = a.get("loop").and_then(Value::as_str) == Some("hold_on_last_frame");
        let mut channels = Vec::new();
        let mut longest = 0.0f32;
        for (bone, props) in a.get("bones").and_then(Value::as_object).into_iter().flatten() {
            for (key, prop) in [("rotation", Prop::Rotation), ("position", Prop::Position), ("scale", Prop::Scale)] {
                let Some(v) = props.get(key) else { continue };
                let mut keys: Vec<Key> = match v {
                    Value::Object(frames) => frames
                        .iter()
                        .filter_map(|(t, f)| {
                            let time: f32 = t.parse().ok()?;
                            let (pre, post, smooth) = match f {
                                Value::Object(o) => {
                                    let post = vec3(o.get("post")).or_else(|| vec3(o.get("pre")))?;
                                    (vec3(o.get("pre")).unwrap_or(post), post, o.get("lerp_mode").and_then(Value::as_str) == Some("catmullrom"))
                                }
                                other => {
                                    let v = vec3(Some(other))?;
                                    (v, v, false)
                                }
                            };
                            Some(Key { time, pre, post, smooth })
                        })
                        .collect(),
                    constant => vec3(Some(constant)).map(|v| vec![Key { time: 0.0, pre: v, post: v, smooth: false }]).unwrap_or_default(),
                };
                keys.sort_by(|a, b| a.time.total_cmp(&b.time));
                longest = longest.max(keys.last().map_or(0.0, |k| k.time));
                if !keys.is_empty() {
                    channels.push(Channel { bone: bone.clone(), prop, keys });
                }
            }
        }
        let length = a.get("animation_length").map(num).unwrap_or(longest).max(1e-3);
        out.push(Animation { name, length, looping, hold, channels });
    }
    Ok(out)
}

impl Animation {
    /// Where a bone that no model has (the first person `hand`) is moved at `t`: its position
    /// in pixels and rotation, like a model bone's.
    pub fn free_bone(&self, bone: &str, t: f32) -> Option<Mat4> {
        let ch = |prop: Prop| self.channels.iter().find(|c| c.prop == prop && c.bone.eq_ignore_ascii_case(bone));
        let (rot, pos) = (ch(Prop::Rotation), ch(Prop::Position));
        if rot.is_none() && pos.is_none() {
            return None;
        }
        let r = rot.map_or(Vec3::ZERO, |c| Vec3::from_array(angles(c.sample(t))));
        let p = pos.map_or(Vec3::ZERO, |c| Vec3::from_array(c.sample(t)));
        Some(Mat4::from_translation(p / 16.0) * Mat4::from_quat(Quat::from_euler(EulerRot::ZYX, r.z, r.y, r.x)))
    }
}

impl Channel {
    /// Value at `t` seconds: linear between keys, uniform Catmull-Rom for smooth ones.
    pub fn sample(&self, t: f32) -> [f32; 3] {
        let k = &self.keys;
        let i = k.partition_point(|key| key.time <= t);
        if i == 0 {
            return k[0].pre;
        }
        if i == k.len() {
            return k[i - 1].post;
        }
        let (a, b) = (&k[i - 1], &k[i]);
        let f = ((t - a.time) / (b.time - a.time).max(1e-6)).clamp(0.0, 1.0);
        if a.smooth || b.smooth {
            let p0 = if i >= 2 { k[i - 2].post } else { a.post };
            let p3 = k.get(i + 1).map_or(b.pre, |k| k.pre);
            let (p1, p2) = (a.post, b.pre);
            std::array::from_fn(|j| {
                let (p0, p1, p2, p3) = (p0[j], p1[j], p2[j], p3[j]);
                0.5 * (2.0 * p1 + (p2 - p0) * f + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f + (3.0 * p1 - p0 - 3.0 * p2 + p3) * f * f * f)
            })
        } else {
            std::array::from_fn(|j| a.post[j] + (b.pre[j] - a.post[j]) * f)
        }
    }
}

/// An animation playing on a model: which channel drives each bone, resolved once.
pub struct Bound {
    pub anim: Animation,
    /// Per bone: (rotation, position, scale) channel indices.
    pub bones: Vec<[Option<usize>; 3]>,
}

impl Bound {
    pub fn new(anim: Animation, parts: &[Part]) -> Bound {
        let bones = parts
            .iter()
            .map(|p| {
                let find = |prop: Prop| anim.channels.iter().position(|c| c.prop == prop && c.bone.eq_ignore_ascii_case(&p.name));
                [find(Prop::Rotation), find(Prop::Position), find(Prop::Scale)]
            })
            .collect();
        Bound { anim, bones }
    }

    /// Time into the animation for `secs` since it started, looping if the file or the caller
    /// says so.
    pub fn at(&self, secs: f32, looping: bool) -> f32 {
        if looping || self.anim.looping { secs.rem_euclid(self.anim.length) } else { secs.min(self.anim.length) }
    }

    pub fn finished(&self, secs: f32, looping: bool) -> bool {
        !looping && !self.anim.looping && !self.anim.hold && secs >= self.anim.length
    }
}

/// What moves a model this frame: vanilla-style procedural angles by role (head look, limb
/// swing), and keyframe animations with their time and weight.
pub struct Motion<'a> {
    pub pitch: f32,
    pub swing: f32,
    pub anims: &'a [(&'a Bound, f32, f32)],
    /// Vanilla's sneaking pose for bipeds (the bones Bedrock names `body`, arms, legs, head).
    pub crouch: bool,
}

/// (about x, about y) added to a part's rest (`rest_x` about x) by its role.
pub fn role_angles(r: u32, rest_x: f32, pitch: f32, swing: f32) -> [f32; 2] {
    // Arms resting held out (a zombie's) only sway a little.
    let arm = if rest_x.abs() > 1.0 { swing * 0.2 } else { swing };
    match r {
        // Faces look along -Z: tilting the front up is a positive turn about X.
        role::HEAD => [pitch, 0.0],
        role::ARM_LEFT => [arm, 0.0],
        role::ARM_RIGHT => [-arm, 0.0],
        role::LEG_RIGHT | role::LEG_FRONT_LEFT => [swing, 0.0],
        role::LEG_LEFT | role::LEG_FRONT_RIGHT => [-swing, 0.0],
        role::LEG_SWEEP_A => [0.0, swing * 0.6],
        role::LEG_SWEEP_B => [0.0, -swing * 0.6],
        _ => [0.0, 0.0],
    }
}

/// Bone transforms (model pixels in, `root` space out), parents before children as models list
/// them; a part listed before its parent gets the root.
pub fn pose(parts: &[Part], root: Mat4, m: &Motion, out: &mut Vec<Mat4>) {
    out.clear();
    for (i, p) in parts.iter().enumerate() {
        let a = role_angles(p.role, p.rest[0], m.pitch, m.swing);
        let mut rot = Vec3::new(p.rest[0] + a[0], p.rest[1] + a[1], p.rest[2]);
        let mut offset = Vec3::ZERO;
        let mut scale = Vec3::ONE;
        if m.crouch {
            // ModelBiped.isSneak: body leans 0.5 forward, arms 0.4 more, legs move up 3 and back
            // 4 to stay under it, the head drops 1.
            match p.role {
                role::ARM_LEFT | role::ARM_RIGHT if p.rest[0].abs() <= 1.0 => rot.x -= 0.4,
                role::LEG_LEFT | role::LEG_RIGHT => offset += Vec3::new(0.0, 3.0, 4.0),
                role::HEAD => offset.y -= 1.0,
                _ if p.name.eq_ignore_ascii_case("body") => rot.x -= 0.5,
                _ => {}
            }
        }
        for (bound, t, w) in m.anims {
            let ch = bound.bones[i];
            if let Some(c) = ch[0] {
                rot += Vec3::from_array(angles(bound.anim.channels[c].sample(*t))) * *w;
            }
            if let Some(c) = ch[1] {
                offset += Vec3::from_array(bound.anim.channels[c].sample(*t)) * *w;
            }
            if let Some(c) = ch[2] {
                scale *= Vec3::ONE.lerp(Vec3::from_array(bound.anim.channels[c].sample(*t)), *w);
            }
        }
        let pivot = Vec3::from_array(p.pivot);
        let local = Mat4::from_translation(offset + pivot) * Mat4::from_quat(Quat::from_euler(EulerRot::ZYX, rot.z, rot.y, rot.x)) * Mat4::from_scale(scale) * Mat4::from_translation(-pivot);
        let parent = p.parent.and_then(|q| out.get(q as usize)).copied().unwrap_or(root);
        out.push(parent * local);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_from_names() {
        assert_eq!((role_of("rightArm", 0), role_of("left_leg", 0), role_of("body", 0)), (role::ARM_LEFT, role::LEG_RIGHT, role::STATIC));
        assert_eq!([0, 1, 2, 3].map(|i| role_of(&format!("leg{i}"), 4)), [role::LEG_RIGHT, role::LEG_LEFT, role::LEG_FRONT_RIGHT, role::LEG_FRONT_LEFT]);
        // A spider's eight: each left leg and its right twin sweep opposite, and so do neighbors.
        let (a, b) = (role::LEG_SWEEP_A, role::LEG_SWEEP_B);
        assert_eq!([0, 1, 2, 3].map(|i| role_of(&format!("leg{i}"), 8)), [a, b, b, a]);
        // An arm resting held out (a zombie's) only sways.
        assert_eq!(role_angles(role::ARM_LEFT, std::f32::consts::FRAC_PI_2, 0.0, 1.0), [0.2, 0.0]);
        assert_eq!(role_angles(role::ARM_LEFT, 0.0, 0.0, 1.0), [1.0, 0.0]);
    }

    #[test]
    fn bedrock_files() {
        let geo = r#"{"format_version":"1.12.0","minecraft:geometry":[{"description":{"identifier":"geometry.t","texture_width":64,"texture_height":32},
            "bones":[{"name":"body","pivot":[0,24,0],"cubes":[{"origin":[-4,12,-2],"size":[8,12,4],"uv":[16,16]}]},
                     {"name":"rightArm","parent":"body","pivot":[-5,22,0],"rotation":[-90,0,0],"cubes":[{"origin":[-8,12,-2],"size":[4,12,4],"uv":[40,16],"mirror":true}]},
                     {"name":"orb","poly_mesh":{"positions":[[0,0,0],[1,0,0],[1,1,0],[0,1,0]],"uvs":[[0,0],[1,0],[1,1],[0,1]],"polys":[[[0,0,0],[1,0,1],[2,0,2],[3,0,3]]]}}]}]}"#;
        let geo = load_geo(geo).unwrap();
        assert_eq!(geo.texture, Some([64, 32]));
        let parts = geo.parts;
        assert_eq!(parts.len(), 3);
        assert_eq!((parts[1].parent, parts[1].role), (Some(0), role::ARM_LEFT));
        assert_eq!(parts[1].cuboids[0].origin, [-3.0, -10.0, -2.0]);
        assert!(parts[1].cuboids[0].mirror && parts[2].mesh.len() == 6);
        // A Bedrock arm at x = -90 points forward (-Z): its hand ends up in front of the shoulder.
        let mut mats = Vec::new();
        pose(&parts, Mat4::IDENTITY, &Motion { pitch: 0.0, swing: 0.0, anims: &[], crouch: false }, &mut mats);
        let hand = mats[1].transform_point3(Vec3::new(-6.0, 12.0, 0.0));
        assert!(hand.z < -8.0 && (hand.y - 22.0).abs() < 0.01, "{hand}");

        let anim = r#"{"format_version":"1.8.0","animations":{"animation.t.wave":{"loop":true,"animation_length":2,
            "bones":{"rightArm":{"rotation":{"0":[0,0,0],"1":[0,0,-90],"2":[0,0,0]}}}}}}"#;
        let a = load_animations(anim).unwrap().remove(0);
        assert_eq!((a.name.as_str(), a.looping, a.length), ("wave", true, 2.0));
        assert_eq!(a.channels[0].sample(0.5), [0.0, 0.0, -45.0]);
        let b = Bound::new(a, &parts);
        assert_eq!((b.bones[1][0], b.bones[0][0]), (Some(0), None));
        assert_eq!(b.at(3.0, false), 1.0);
    }
}
