//! Survival rules on top of the engine's player: hunger, regeneration, drowning, armor, eating,
//! the HUD and the inventory screen with its 2x2 crafting grid.

use loom_sdk::*;

use crate::items::{ARMOR, FOOD};

pub use base_api::{FEET, HEAD, HUNGER, Hunger, LEGS};

/// Every 4 seconds a player with a full belly heals and a starving one hurts, like vanilla's.
static HUNGER_PULSE: Event<u64> = Event::named("hunger_pulse");
const PULSE_TICKS: u32 = 80;
pub static INVENTORY: Menu = Menu::named("inventory");
pub static CRAFTING: Menu = Menu::named("crafting_table");
static HUD_HEALTH: HudElement = HudElement::named("health");
static HUD_ARMOR: HudElement = HudElement::named("armor");
static HUD_FOOD: HudElement = HudElement::named("food");
static HUD_AIR: HudElement = HudElement::named("air");

const MAX_AIR: f32 = 300.0;

/// Head to feet.
pub const EQUIPMENT: [&EquipmentSlot; 4] = [&HEAD, &base_api::CHEST, &LEGS, &FEET];

/// Armor points of what `entity` wears.
fn armor_points(w: &World, entity: u64) -> u32 {
    let Some(worn) = w.inventory(entity).find_group(EQUIPMENT_GROUP) else { return 0 };
    (0..worn.len()).filter_map(|i| ARMOR.on_item(worn.get(i).item)).map(|a| a.points).sum()
}

pub fn register(r: &mut Registry) {
    r.player_inventory().group("hotbar", Slots::row(9).hotbar()).group("main", Slots::grid(9, 3));
    for (slot, parts, piece) in [(&HEAD, worn_parts::HEAD, "helmet"), (&base_api::CHEST, worn_parts::BODY | worn_parts::ARMS, "chestplate"), (&LEGS, worn_parts::BODY | worn_parts::LEGS, "leggings"), (&FEET, worn_parts::LEGS, "boots")] {
        r.equipment_slot(slot, parts, &format!("slot_{piece}"));
    }
    r.entity_component(
        &HUNGER,
        Hunger { food: 20.0, saturation: 5.0, exhaustion: 0.0, air: MAX_AIR },
        component_flags::SAVE | component_flags::SYNC_OWNER,
    );
    r.attach(&PLAYER, &HUNGER);
    r.world_tick(tick_players);
    r.on(&PLAYER_JOINED, |b| {
        let w = b.world();
        for j in b.iter() {
            w.after(j.player, PULSE_TICKS, &HUNGER_PULSE, j.player);
        }
    });
    r.on(&HUNGER_PULSE, |b| {
        let w = b.world();
        for &id in b.iter() {
            w.after(id, PULSE_TICKS, &HUNGER_PULSE, id);
            let (Some(mut st), Some(h)) = (w.entity(id), w.component_mut(id, &HUNGER)) else { continue };
            if w.creative(id) {
                continue;
            }
            if h.food >= 18.0 && st.health < st.max_health {
                st.health = (st.health + 1.0).min(st.max_health);
                h.exhaustion += 3.0;
                w.set_entity(id, &st);
            } else if h.food <= 0.0 && st.health > 1.0 {
                w.damage(id, 1.0, 0, damage::STARVE);
            }
        }
    });
    // Armor is a step of the damage chain (4% per point, like 1.7.10), and wears when the hit
    // actually lands, a quarter of the damage per piece (at least 1) like vanilla.
    r.modify(&chain::DAMAGE, stage::MULTIPLY, |m| {
        let q = *m.query();
        if !armor_applies(q.cause) {
            return;
        }
        m.value *= 1.0 - armor_points(&m.world(), q.entity).min(20) as f64 * 0.04;
    });
    r.on(&ENTITY_HURT, |b| {
        let w = b.world();
        for h in b.iter().filter(|h| armor_applies(h.cause)) {
            let Some(worn) = w.inventory(h.entity).find_group(EQUIPMENT_GROUP) else { continue };
            let wear = ((h.amount / 4.0) as u32).max(1);
            for slot in 0..worn.len() {
                let s = worn.get(slot);
                if ARMOR.on_item(s.item).is_some() {
                    let durability = DURABILITY.on_item(s.item).unwrap_or(0);
                    let damage = s.damage() + wear;
                    worn.set(slot, if durability > 0 && damage >= durability { ItemStack::EMPTY } else { s.with_damage(damage) });
                }
            }
        }
    });

    // Names the held stack, like an anvil: a chest placed from it is called so.
    r.command("rename", "[name:text]", |cmd| {
        let w = cmd.world();
        let hotbar = w.inventory(cmd.player()).group("hotbar");
        let slot = w.selected_slot(cmd.player());
        let s = hotbar.get(slot);
        if !s.is_empty() {
            hotbar.set(slot, with_name(s, cmd.text("name")));
        }
    });
    r.command("feed", "[food:int]", |cmd| {
        let food = cmd.int("food").unwrap_or(20).clamp(0, 20) as f32;
        if let Some(h) = cmd.world().component_mut(cmd.player(), &HUNGER) {
            h.food = food;
            h.saturation = h.saturation.min(food);
        }
        cmd.reply("command.base.feed", &[Arg::Int(food as i64)]);
    });

    // Survival bars; a negative value hides them (creative).
    r.hud_element(&HUD_HEALTH, hud_anchor::HOTBAR_LEFT, 0, |ui| {
        ui.icons("heart", 10, |c| if c.creative() { -1.0 } else { c.me().map_or(20.0, |s| s.health.ceil()) })
    });
    r.hud_element(&HUD_ARMOR, hud_anchor::HOTBAR_LEFT, 1, |ui| {
        ui.icons("armor", 10, |c| {
            // From what the client knows of its own inventory, like the server's damage chain.
            let points: u32 = c.inventory_group(EQUIPMENT_GROUP).iter().filter_map(|s| ARMOR.on_item(s.item)).map(|a| a.points).sum();
            if points > 0 && !c.creative() { points as f32 } else { -1.0 }
        })
    });
    r.hud_element(&HUD_FOOD, hud_anchor::HOTBAR_RIGHT, 0, |ui| {
        ui.icons_rtl("food", 10, |c| if c.creative() { -1.0 } else { c.component(&HUNGER).map_or(20.0, |h| h.food.ceil()) })
    });
    r.hud_element(&HUD_AIR, hud_anchor::HOTBAR_RIGHT, 1, |ui| {
        ui.icons_rtl("air", 10, |c| c.component(&HUNGER).map_or(-1.0, |h| if h.air < MAX_AIR { (h.air.max(0.0) / MAX_AIR * 20.0).ceil() } else { -1.0 }))
    });

    // The equipment slots take what is worn there and show its silhouette while empty.
    r.menu(&INVENTORY)
        .group("grid", Slots::grid(2, 2))
        .group("result", Slots::single().output().large())
        .equipment()
        .player_inventory()
        .recipes(&crate::recipes::CRAFTING)
        .on_change(craft_result)
        .on_take(consume_grid)
        .screen(|ui| {
            ui.panel("panel", S::col().pad(7.0).gap(5.0), |ui| {
                ui.row(S::row().gap(1.0), |ui| {
                    ui.equipment();
                    ui.player_model(51.0, 72.0);
                    ui.spacer();
                    ui.column(S::col().gap(2.0), |ui| {
                        ui.title();
                        ui.row(S::row().gap(4.0), |ui| {
                            ui.group("grid");
                            ui.recipe_arrow();
                            ui.group("result");
                        });
                    });
                });
                ui.player_inventory(false);
            });
        });

    r.menu(&CRAFTING)
        .group("grid", Slots::grid(3, 3))
        .group("result", Slots::single().output().large())
        .player_inventory()
        .recipes(&crate::recipes::CRAFTING)
        .on_change(craft_result)
        .on_take(consume_grid)
        .screen(|ui| {
            ui.panel("panel", S::col().pad(7.0).gap(4.0).stretch(), |ui| {
                ui.title();
                ui.row(S::row().gap(8.0), |ui| {
                    ui.spacer();
                    ui.group("grid");
                    ui.recipe_arrow();
                    ui.group("result");
                    ui.spacer();
                });
                ui.player_inventory(true);
            });
        });
    r.player_menu(&INVENTORY);
}

fn armor_applies(cause: u32) -> bool {
    cause & damage::BYPASSES_ARMOR == 0
}

/// The `result` slot shows what the `grid` makes.
fn craft_result(m: &MenuCtx) {
    let grid = m.group("grid");
    let cells: Vec<ItemStack> = (0..grid.len()).map(|i| grid.get(i)).collect();
    m.group("result").set(0, recipe_match(&crate::recipes::CRAFTING, &cells, grid.cols(), grid.rows()));
}

/// Takes one of each ingredient; the last of a stack leaves its `remainder` (a bucket).
fn consume_grid(m: &MenuCtx) {
    let grid = m.group("grid");
    for i in 0..grid.len() {
        let s = grid.get(i);
        if !s.is_empty() {
            grid.set(i, use_one(s));
        }
    }
}

fn tick_players(w: &World) {
    for id in w.players() {
        let Some(st) = w.entity(id) else { continue };
        if w.creative(id) {
            continue;
        }
        let Some(h) = w.component_mut(id, &HUNGER) else { continue };
        h.exhaustion += st.moved * 0.01 + if st.vel[1] > 3.0 { 0.01 } else { 0.0 };
        if h.exhaustion > 4.0 {
            h.exhaustion -= 4.0;
            if h.saturation > 0.0 {
                h.saturation = (h.saturation - 1.0).max(0.0);
            } else {
                h.food = (h.food - 1.0).max(0.0);
            }
        }
        let eye = [st.pos[0].floor() as i32, (st.pos[1] + st.size[2] as f64).floor() as i32, st.pos[2].floor() as i32];
        let underwater = w.flags(eye) & abi::block_flags::LIQUID != 0;
        let Some(h) = w.component_mut(id, &HUNGER) else { continue };
        if underwater {
            h.air -= 1.0;
            if h.air <= -20.0 {
                h.air = 0.0;
                w.damage(id, 2.0, 0, damage::DROWN);
            }
        } else if h.air < MAX_AIR {
            h.air = (h.air + 5.0).min(MAX_AIR);
        }
    }
}


/// Eating: instant, consumes one item.
pub fn eat(u: &mut ItemUse) -> bool {
    let stack = u.stack();
    let Some(food) = FOOD.on_stack(&stack) else { return false };
    let w = u.world();
    let Some(h) = w.component_mut(u.player(), &HUNGER) else { return false };
    if h.food >= 20.0 {
        return false;
    }
    h.food = (h.food + food.hunger as f32).min(20.0);
    h.saturation = (h.saturation + food.saturation).min(h.food);
    u.set_stack(stack.shrink(1));
    true
}
