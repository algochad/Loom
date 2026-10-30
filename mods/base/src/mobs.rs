//! Mobs: zombies and spiders at night or in the dark, pigs and cows on grass by day.

use loom_sdk::ai::{self, Goal};
use loom_sdk::*;

value! {
    #[derive(Default)]
    /// 1 while it runs about after being hurt.
    pub struct Brain { pub panicking: u32 }
}

pub static BRAIN: Component<Brain> = Component::named("base:brain");
/// Three seconds after being hurt an animal calms down.
static CALM: Event<u64> = Event::named("calm");
pub static ZOMBIE: Entity = Entity::named("zombie");
pub static SPIDER: Entity = Entity::named("spider");
pub static PIG: Entity = Entity::named("pig");
pub static COW: Entity = Entity::named("cow");


pub fn register(r: &mut Registry) {
    // The engine's player kind, drawn as the same humanoid as the zombie.
    r.entity_model(&Entity::named("core:player"), biped(false));
    r.entity_component(&BRAIN, Brain::default(), 0);
    r.on(&CALM, |b| {
        let w = b.world();
        for &id in b.iter() {
            if let Some(brain) = w.component_mut(id, &BRAIN) {
                brain.panicking = 0;
            }
        }
    });
    r.entity(&ZOMBIE)
        .sound("ambient").sound("hurt").sound("death")
        .size(0.6, 1.95)
        .health(20.0)
        .speed(2.6)
        .hostile()
        .flags(entity_flags::BURNS_IN_SUN)
        .model(biped(true))
        .attach(&BRAIN)
        .attack_damage(3.0)
        .goals(hostile(false));
    r.entity(&SPIDER)
        .sound("ambient").sound("hurt").sound("death")
        .size(1.3, 0.9)
        .health(16.0)
        .speed(3.6)
        .hostile()
        .model(spider())
        .attach(&BRAIN)
        .attack_damage(2.0)
        .goals(hostile(true));
    r.entity(&PIG)
        .sound("ambient").sound("hurt").sound("death")
        .size(0.9, 0.9)
        .health(10.0)
        .speed(2.5)
        .passive()
        .model(quadruped([10.0, 8.0, 16.0], 6.0, [8.0, 8.0, 8.0]))
        .attach(&BRAIN)
        .goals(passive())
        .on_hurt(panic);
    r.entity(&COW)
        .sound("ambient").sound("hurt").sound("death")
        .size(0.9, 1.3)
        .health(10.0)
        .speed(2.2)
        .passive()
        .model(quadruped([12.0, 10.0, 18.0], 12.0, [8.0, 8.0, 6.0]))
        .attach(&BRAIN)
        .goals(passive())
        .on_hurt(panic);
    // Vanilla amounts; meat comes out cooked when the animal burns.
    r.loot(&ZOMBIE).pool(Pool::new().items("rotten_flesh", 0, 2));
    r.loot(&SPIDER).pool(Pool::new().items("string", 0, 2));
    r.loot(&PIG).pool(Pool::new().entry(Entry::item("porkchop").count(1, 3).smelted(&crate::recipes::SMELTING)));
    r.loot(&COW).pool(Pool::new().entry(Entry::item("beef").count(1, 3).smelted(&crate::recipes::SMELTING))).pool(Pool::new().items("leather", 0, 2));
    // Natural spawning, as rules the engine runs: monsters in the dark (spiders more in deep
    // caves), animals in small herds on grass by day, rarely once the world is loaded.
    r.spawn(&ZOMBIE).light(0, 6).weight(2);
    r.spawn(&SPIDER).light(0, 6);
    r.spawn(&SPIDER).light(0, 6).biome("#cave").weight(4);
    for animal in [&PIG, &COW] {
        r.spawn(animal).biome("#animals").ground("#animal_spawn").light(7, 15).group(2, 3).chance(0.05);
    }
}


fn panic(w: &World, h: &Hurt) {
    if let Some(b) = w.component_mut(h.entity, &BRAIN) {
        b.panicking = 1;
        w.after(h.entity, 60, &CALM, h.entity);
    }
}

/// Monsters chase the nearest player and hit it when in reach (`ai::melee`, once a second);
/// spiders only in the dark (by day they leave players alone). Otherwise they wander.
fn hostile(only_dark: bool) -> ai::Goals {
    let angry = move |w: &World, st: &EntityState| {
        let feet = [st.pos[0].floor() as i32, (st.pos[1] + 0.5).floor() as i32, st.pos[2].floor() as i32];
        !only_dark || w.daylight() < 0.5 || w.sky_light(feet) < 10
    };
    ai::Goals::new()
        .then(move |w: &World, id: u64, st: &EntityState| angry(w, st) && ai::melee(1.6).run(w, id, st))
        .then(move |w: &World, id: u64, st: &EntityState| angry(w, st) && ai::follow_player(18.0, 1.0).run(w, id, st))
        .then(ai::wander(0.6, 10.0))
}

/// Animals run about for three seconds after being hurt, otherwise wander.
fn passive() -> ai::Goals {
    ai::Goals::new()
        .then(|w: &World, id: u64, st: &EntityState| {
            if w.component(id, &BRAIN).is_none_or(|b| b.panicking == 0) {
                return false;
            }
            // A new direction every second.
            if w.time() % 20 == id % 20 || !w.navigating(id) {
                let a = w.random_below(628) as f64 / 100.0;
                w.navigate(id, [st.pos[0] + a.cos() * 8.0, st.pos[1], st.pos[2] + a.sin() * 8.0], 1.6);
            }
            true
        })
        .then(ai::wander(0.5, 10.0))
}
