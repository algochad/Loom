//! Moving collision demos: a lift going up and down and a skiff flying back and forth fast
//! enough that jumping on it only works because riders move with it, both moving structures of
//! planks that blocks can be added to and broken off on the way, and a crate that falls and can
//! be stood on.

use loom_sdk::*;

static LIFT: Entity = Entity::named("lift");
static SKIFF: Entity = Entity::named("skiff");
static CRATE: Entity = Entity::named("crate");
static LIFT_DECK: Template = Template::named("lift_deck");
static SKIFF_DECK: Template = Template::named("skiff_deck");

pub fn register(r: &mut Registry) {
    // What the moving structures are built of, checked when the game loads.
    r.template(&LIFT_DECK).fill("base:oak_planks", [0, 0, 0], [1, 0, 1]);
    r.template(&SKIFF_DECK).fill("base:oak_planks", [-1, 0, -1], [1, 0, 1]);
    // 4 blocks up in 4 seconds, then down again.
    r.entity(&LIFT).health(100.0).invulnerable().solid().kinematic().blocks(&LIFT_DECK).on_tick(|t| {
        let w = t.world();
        for &id in t.ids() {
            let Some(mut st) = w.entity(id) else { continue };
            st.vel = [0.0, if (st.age / 80) % 2 == 0 { 1.0 } else { -1.0 }, 0.0];
            w.set_entity(id, &st);
        }
    });
    // 6 blocks a second along x for 3 seconds each way; riders stay with it up to 3 blocks up.
    r.entity(&SKIFF).health(100.0).invulnerable().solid().kinematic().carry_height(3.0).blocks(&SKIFF_DECK).on_tick(|t| {
        let w = t.world();
        for &id in t.ids() {
            let Some(mut st) = w.entity(id) else { continue };
            st.vel = [if (st.age / 60) % 2 == 0 { 6.0 } else { -6.0 }, 0.0, 0.0];
            w.set_entity(id, &st);
        }
    });
    r.entity(&CRATE)
        .size(1.0, 1.0)
        .health(10.0)
        .invulnerable()
        .solid()
        .texture("bodies")
        .model(Model::from(vec![Part::new("crate", [0.0; 3]).cube([-8.0, 0.0, -8.0], [16.0, 16.0, 16.0], [0.0, 24.0])]).uv(64, 64));
}
