pub mod cube;
pub mod light;
pub mod view;

use glam::IVec3;

pub use cube::Cube;
pub use view::View;

/// Game ticks per second, the unit every duration in ticks is in.
pub const TPS: u32 = 20;
/// One tick in seconds.
pub const TICK: f64 = 1.0 / TPS as f64;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum GameMode {
    #[default]
    Survival,
    Creative,
    /// Flies through blocks, cannot touch anything, unseen by mobs and survival players.
    Spectator,
}

impl GameMode {
    pub const ALL: [GameMode; 3] = [GameMode::Survival, GameMode::Creative, GameMode::Spectator];

    pub fn name(self) -> &'static str {
        match self {
            GameMode::Survival => "survival",
            GameMode::Creative => "creative",
            GameMode::Spectator => "spectator",
        }
    }

    pub fn parse(s: &str) -> Option<GameMode> {
        GameMode::ALL.into_iter().find(|m| m.name() == s)
    }

    pub fn from_u8(v: u8) -> GameMode {
        GameMode::ALL.get(v as usize).copied().unwrap_or_default()
    }

    /// Takes no damage and needs no food.
    pub fn invulnerable(self) -> bool {
        self != GameMode::Survival
    }
}

#[inline]
pub fn cube_of(p: IVec3) -> IVec3 {
    p >> 4
}

#[inline]
pub fn local_index(p: IVec3) -> usize {
    cube::index((p.x & 15) as usize, (p.y & 15) as usize, (p.z & 15) as usize)
}

pub const DIRS: [IVec3; 6] = [IVec3::NEG_X, IVec3::X, IVec3::NEG_Y, IVec3::Y, IVec3::NEG_Z, IVec3::Z];

pub use loom_noise::{Rng, mix};

/// The corners of the box around every block that is not air (a structure's body).
pub fn block_bounds<'a>(cubes: impl Iterator<Item = (&'a IVec3, &'a cube::Cube)>) -> Option<(IVec3, IVec3)> {
    let mut out: Option<(IVec3, IVec3)> = None;
    for (&cp, cube) in cubes {
        if cube.uniform_state() == Some(0) {
            continue;
        }
        for i in 0..cube::VOLUME {
            if cube.get(i) != 0 {
                let p = cp * 16 + crate::world::cube::offset(i);
                out = Some(out.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
            }
        }
    }
    out
}



