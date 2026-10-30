//! Engine side of the mod ABI: the function table mods call, and the wrapper the engine uses to
//! call mod callbacks (profiling and fault isolation live there).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use glam::{DVec3, IVec3};
use loom_abi::{self as abi, Callback, Host, ItemStack, ModId, Str};

use crate::content::{self, RawRegistrar, content, try_content};
use crate::registry;
use crate::server::Dim;
use crate::server::menu::MenuCall;

pub static HOST: Host = Host {
    log: host_log,
    log_at,
    log_enabled,
    reg_block,
    reg_item,
    reg_biome,
    reg_dimension,
    reg_entity,
    reg_entity_model,
    reg_component,
    reg_world_tick,
    entity_after,
    reg_recipe,
    reg_menu,
    set_player_menu,
    set_block_entity,
    reg_hud_element,
    reg_command,
    reg_ore,
    reg_scatter,
    reg_decoration,
    reg_creative_tab,
    reg_set,
    find,
    block_of,
    state_prop,
    state_with,
    state_flags,
    find_component,
    components,
    stack_get,
    stack_with,
    get_block,
    set_block,
    schedule_tick,
    world_time,
    random,
    light,
    daylight,
    entity_spawn,
    entity_remove,
    entity_get,
    entity_push,
    entity_set,
    entity_walk,
    entity_damage,
    entity_attack,
    entity_look_at,
    entities_near,
    component,
    player_count,
    player_at,
    is_creative,
    inv_size,
    inv_get,
    inv_set,
    inv_insert,
    selected_slot,
    spawn_item,
    block_entity,
    open_menu,
    menu_group,
    inventory_group,
    menu_get,
    menu_set,
    recipe_match,
    recipe_process,
    recipe_find,
    recipe_info,
    recipe_output,
    recipe_value,
    recipe_consume,
    item_block,
    ui_open: crate::client::modui::ui_open,
    ui_close: crate::client::modui::ui_close,
    ui_text: crate::client::modui::ui_text,
    client_player: crate::client::modui::client_player,
    client_component: crate::client::modui::client_component,
    client_entity: crate::client::modui::client_entity,
    client_creative: crate::client::modui::client_creative,
    energy_insert,
    energy_extract,
    command_reply,
    gizmo_on,
    gizmo,
    reg_client_hook,
    reg_client_entity_tick,
    client_entity_defaults: crate::app::api::client_entity_defaults,
    reg_packet,
    reg_key,
    reg_error,
    launch_option: crate::app::api::launch_option,
    send_packet,
    open_menu_at,
    inventory_create,
    entity_teleport,
    set_time,
    area_fill,
    set_game_mode,
    client_state: crate::app::api::client_state,
    client_input: crate::app::api::client_input,
    client_pressed: crate::app::api::client_pressed,
    client_set_player: crate::app::api::client_set_player,
    client_command: crate::app::api::client_command,
    client_text: crate::app::api::client_text,
    client_command_reply: crate::app::api::client_command_reply,
    client_block: crate::app::api::client_block,
    client_light: crate::app::api::client_light,
    client_slot: crate::app::api::client_slot,
    client_menu: crate::app::api::client_menu,
    client_entities: crate::app::api::client_entities,
    client_ui_rect: crate::app::api::client_ui_rect,
    client_draw: crate::app::api::client_draw,
    client_send_packet: crate::app::api::client_send_packet,
    image_get: crate::app::api::image_get,
    image_load: crate::app::api::image_load,
    image_save: crate::app::api::image_save,
    image_free: crate::app::api::image_free,
    name_of,
    reg_liquid,
    reg_liquid_reaction,
    liquid_of,
    use_container,
    reg_event_handler,
    find_event,
    emit,
    reg_interceptor,
    ask,
    reg_modifier,
    compute,
    explanation,
    reg_attribute,
    attribute,
    attribute_modifier,
    attribute_remove,
    attribute_explain,
    reg_loot,
    reg_loot_modifier,
    roll_loot,
    break_block,
    reg_stack_component,
    client_menu_owner: crate::client::modui::client_menu_owner,
    reg_system,
    reg_client_command,
    reg_command_branch,
    reg_text_condition,
    reg_particle,
    particles,
    client_particles: crate::app::api::client_particles,
    reg_config,
    config_value,
    config_set,
    config_text,
    config_set_text,
    reg_sound,
    reg_music,
    play_animation,
    play_sound,
    client_play_sound: crate::app::api::client_play_sound,
    reg_spawn,
    biome_at,
    navigate,
    navigating,
    reg_structure,
    reg_template,
    ref_list,
    world_place,
    structure_near,
    fall_block,
    reg_attach_value,
    reg_recipe_kind,
    reg_entity_category,
    reg_sound_category,
    reg_tint_kind,
    reg_biome_tint,
    reg_player_inventory,
    reg_equipment_slot,
    player_group,
    reg_screen,
    reg_words,
    set_entity_size,
    set_entity_look,
    structure_block,
    set_structure_block,
    reg_terrain,
    reg_screen_part,
    dim_column,
    dim_carved,
};

unsafe extern "C" fn fall_block(w: *mut abi::WorldCtx, pos: *const [i32; 3]) -> u64 {
    unsafe { dim(w).fall_block(content(), ip(pos)) }
}

unsafe extern "C" fn reg_template(r: *mut abi::Registrar, d: *const abi::TemplateDesc) {
    unsafe {
        let d = &*d;
        reg(r).game.templates.push(registry::RawTemplate {
            name: d.name.as_str().to_string(),
            blocks: d.blocks.as_slice().iter().map(|b| (b.pos, b.block.as_str().to_string())).collect(),
        });
    }
}

unsafe extern "C" fn ref_list(index: u32) -> abi::Slice<[u32; 2]> {
    try_content().and_then(|c| c.ref_lists.get(index as usize)).map_or(abi::Slice::EMPTY, |l| abi::Slice::new(l))
}

unsafe extern "C" fn world_place(w: *mut abi::WorldCtx, name: Str, pos: *const [i32; 3]) -> u32 {
    unsafe {
        let c = content();
        let Some(&t) = c.template_names.get(name.as_str()) else {
            misuse("place", || format!("no template {}", name.as_str()));
            return 0;
        };
        let d = dim(w);
        let at = ip(pos);
        let blocks = &c.templates[t as usize].blocks;
        if blocks.iter().any(|(p, _)| d.get(at + *p).is_none()) {
            return 0;
        }
        for &(p, state) in blocks {
            d.set(c, at + p, state);
        }
        1
    }
}

unsafe extern "C" fn reg_structure(r: *mut abi::Registrar, d: *const abi::StructureDesc) {
    unsafe {
        let d = *d;
        reg(r).game.structures.push(registry::RawStructure {
            template: d.template.as_str().to_string(),
            dimension: d.dimension.as_str().to_string(),
            spacing: d.spacing,
            chance: d.chance,
            biome: d.biome.as_str().to_string(),
            sink: d.sink,
        });
    }
}

unsafe extern "C" fn structure_near(w: *mut abi::WorldCtx, name: Str, pos: *const [i32; 3], radius: i32, out: *mut [i32; 3]) -> u32 {
    unsafe {
        let d = dim(w);
        match crate::server::structure_near(content(), d.id, d.seed, name.as_str(), ip(pos), radius) {
            Some(p) => {
                *out = p.to_array();
                1
            }
            None => 0,
        }
    }
}

unsafe extern "C" fn navigate(w: *mut abi::WorldCtx, id: u64, target: *const [f64; 3], speed: f32) -> u32 {
    unsafe {
        let c = content();
        let d = dim(w);
        let target = DVec3::from_array(*target);
        let mul = d.attribute(c, id, crate::server::MOVEMENT_SPEED, false) as f32;
        let jump_mul = d.attribute(c, id, crate::server::JUMP_STRENGTH, false) as f32;
        if let Some(e) = d.entities.get_mut(id) {
            e.jump_mul = jump_mul;
        }
        d.navigate(id, target, speed * mul) as u32
    }
}

unsafe extern "C" fn navigating(w: *mut abi::WorldCtx, id: u64) -> u32 {
    unsafe { dim(w).entities.get(id).is_some_and(|e| e.nav.is_some()) as u32 }
}

unsafe extern "C" fn biome_at(w: *mut abi::WorldCtx, pos: *const [i32; 3]) -> u32 {
    unsafe {
        let p = ip(pos);
        dim(w).cubes.get(&crate::world::cube_of(p)).map_or(0, |c| c.biome((p.x & 15) as usize, (p.z & 15) as usize) as u32)
    }
}

unsafe extern "C" fn reg_spawn(r: *mut abi::Registrar, d: *const abi::SpawnDesc) {
    unsafe {
        let d = *d;
        let tag = |s: Str| s.as_str().to_string();
        reg(r).game.spawns.push(registry::RawSpawn {
            entity: d.entity.as_str().to_string(),
            biome: tag(d.biome),
            ground: tag(d.ground),
            dimension: d.dimension.as_str().to_string(),
            light: d.light,
            y: d.y,
            group: [d.group[0].max(1), d.group[1].max(d.group[0]).max(1)],
            weight: d.weight,
            chance: d.chance,
        });
    }
}

unsafe extern "C" fn reg_sound(r: *mut abi::Registrar, d: *const abi::SoundDesc) {
    unsafe {
        let d = *d;
        let def = registry::SoundDef {
            name: d.name.as_str().to_string(),
            files: d.files.as_slice().iter().map(|s| s.as_str().to_string()).collect(),
            category: 0,
            category_name: d.category_name.as_str().to_string(),
            volume: d.volume,
            pitch: [d.pitch[0].min(d.pitch[1]).max(0.01), d.pitch[0].max(d.pitch[1]).max(0.01)],
            range: d.range.max(1.0),
        };
        reg(r).game.sounds.push((d.cell as usize, def));
    }
}

unsafe extern "C" fn reg_music(r: *mut abi::Registrar, d: *const abi::MusicDesc) {
    unsafe {
        let d = *d;
        let m = registry::MusicDef { name: d.name.as_str().to_string(), tracks: d.tracks.as_slice().iter().map(|s| s.as_str().to_string()).collect(), dimension: None, when: d.when };
        reg(r).game.music.push((d.dimension.as_str().to_string(), m));
    }
}

unsafe extern "C" fn play_animation(w: *mut abi::WorldCtx, id: u64, name: Str, looping: u32) {
    unsafe {
        let d = dim(w);
        d.events.push(crate::server::DimEvent::Animation { id, name: name.as_str().to_string(), looping: looping != 0 });
    }
}

unsafe extern "C" fn play_sound(w: *mut abi::WorldCtx, sound: u32, pos: *const [f64; 3], volume: f32, pitch: f32) {
    unsafe {
        if (sound as usize) < content().game.sounds.len() {
            dim(w).sound(sound, DVec3::from_array(*pos), volume, pitch);
        } else {
            misuse("play_sound", || format!("no sound {sound}"));
        }
    }
}


// ---------------------------------------------------------------- profiling per mod

pub const MAX_MODS: usize = 256;

pub struct ModStats {
    pub nanos: AtomicU64,
    pub calls: AtomicU64,
    pub faults: AtomicU32,
}

static STATS: [ModStats; MAX_MODS] =
    [const { ModStats { nanos: AtomicU64::new(0), calls: AtomicU64::new(0), faults: AtomicU32::new(0) } }; MAX_MODS];

pub fn stats(m: ModId) -> &'static ModStats {
    &STATS[(m.0 as usize).min(MAX_MODS - 1)]
}

/// Callback nanoseconds and calls of a mod since start; readers diff two looks.
pub fn mod_totals(m: ModId) -> (u64, u64) {
    let s = stats(m);
    (s.nanos.load(Ordering::Relaxed), s.calls.load(Ordering::Relaxed))
}

/// Calls a mod callback. Time is charged to `owner`; a failing callback is logged with the mod's
/// name and the engine carries on.
pub fn invoke<A>(owner: ModId, cb: Callback, args: &mut A) -> bool {
    let Some(f) = cb.func else { return true };
    let start = Instant::now();
    let prev = crate::diag::enter_mod(owner);
    let r = unsafe { f(cb.data, args as *mut A as *mut c_void) };
    crate::diag::leave_mod(prev);
    let s = stats(owner);
    s.nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    s.calls.fetch_add(1, Ordering::Relaxed);
    if r != 0 {
        let n = s.faults.fetch_add(1, Ordering::Relaxed);
        if n < 10 {
            let name = try_content().map_or("?".to_string(), |c| c.mod_name(owner).to_string());
            log::error!("callback of mod '{name}' failed (see its panic above), continuing");
        }
        return false;
    }
    true
}

// ---------------------------------------------------------------- world access from callbacks

/// `WorldCtx` pointers handed to mods point at one of these. Only valid during the callback.
pub struct CallCtx {
    pub dim: *mut Dim,
}

impl CallCtx {
    pub fn raw(&mut self) -> *mut abi::WorldCtx {
        self as *mut CallCtx as *mut abi::WorldCtx
    }
}

unsafe fn dim<'a>(w: *mut abi::WorldCtx) -> &'a mut Dim {
    unsafe { &mut *(*(w as *mut CallCtx)).dim }
}

fn ip(p: *const [i32; 3]) -> IVec3 {
    IVec3::from_array(unsafe { *p })
}

unsafe extern "C" fn get_block(w: *mut abi::WorldCtx, pos: *const [i32; 3]) -> u32 {
    unsafe { dim(w).get(ip(pos)).unwrap_or(content::AIR) }
}

/// A mod called a host function with something that does not exist: logged, the first 20 times
/// per function (a mod doing it every tick would flood the log).
pub(crate) fn misuse(func: &'static str, detail: impl FnOnce() -> String) {
    static SEEN: std::sync::Mutex<Vec<(&'static str, u32)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let n = match seen.iter_mut().find(|s| s.0 == func) {
        Some(s) => {
            s.1 += 1;
            s.1
        }
        None => {
            seen.push((func, 1));
            1
        }
    };
    if n <= 20 {
        log::error!("{func}: {}{}", detail(), if n == 20 { " (not reported again)" } else { "" });
    }
}

unsafe extern "C" fn set_block(w: *mut abi::WorldCtx, pos: *const [i32; 3], state: u32) -> u32 {
    if state as usize >= content().states.len() {
        log::error!("mod tried to set invalid state {state}");
        return 0;
    }
    unsafe { dim(w).set(content(), ip(pos), state).is_some() as u32 }
}

unsafe extern "C" fn structure_block(w: *mut abi::WorldCtx, entity: u64, pos: *const [i32; 3]) -> u32 {
    unsafe {
        let d = dim(w);
        match d.structures.get(&entity) {
            Some(s) if entity != 0 => s.get(ip(pos)).unwrap_or(crate::content::AIR),
            _ => u32::MAX,
        }
    }
}

unsafe extern "C" fn set_structure_block(w: *mut abi::WorldCtx, entity: u64, pos: *const [i32; 3], state: u32) -> u32 {
    if state as usize >= content().states.len() || entity == 0 {
        return 0;
    }
    unsafe { dim(w).with_space(content(), entity, |d, _| d.set(content(), ip(pos), state).is_some()).unwrap_or(false) as u32 }
}

unsafe extern "C" fn schedule_tick(w: *mut abi::WorldCtx, pos: *const [i32; 3], delay: u32) {
    unsafe { dim(w).schedule(ip(pos), delay) }
}

unsafe extern "C" fn world_time(w: *mut abi::WorldCtx) -> u64 {
    unsafe { dim(w).time }
}

unsafe extern "C" fn random(w: *mut abi::WorldCtx) -> u64 {
    unsafe { dim(w).random() }
}

unsafe extern "C" fn light(w: *mut abi::WorldCtx, pos: *const [i32; 3]) -> u32 {
    unsafe { dim(w).light(ip(pos)) }
}

unsafe extern "C" fn daylight(w: *mut abi::WorldCtx) -> f32 {
    unsafe { dim(w).daylight(content()) }
}

unsafe extern "C" fn entity_spawn(w: *mut abi::WorldCtx, kind: u32, pos: *const [f64; 3]) -> u64 {
    if kind as usize >= content().game.entities.len() {
        misuse("entity_spawn", || format!("no entity kind {kind}"));
        return 0;
    }
    if content().game.is_block_entity(kind) {
        misuse("entity_spawn", || format!("{} belongs to a block, set the block instead", content().game.entities[kind as usize].name));
        return 0;
    }
    unsafe { dim(w).spawn(content(), kind, DVec3::from_array(*pos)) }
}

unsafe extern "C" fn entity_remove(w: *mut abi::WorldCtx, id: u64) {
    unsafe { dim(w).entities.remove(id) }
}

unsafe extern "C" fn entity_get(w: *mut abi::WorldCtx, id: u64, out: *mut abi::EntityState) -> u32 {
    unsafe {
        let d = dim(w);
        let c = content();
        match d.entities.get(id) {
            Some(e) => {
                *out = e.state(c, d.max_health(c, id));
                1
            }
            None => 0,
        }
    }
}

unsafe extern "C" fn entity_set(w: *mut abi::WorldCtx, id: u64, s: *const abi::EntityState) {
    unsafe {
        let s = &*s;
        if let Some(e) = dim(w).entities.get_mut(id) {
            let pos = DVec3::from_array(s.pos);
            if pos.is_finite() {
                e.pos = pos;
            }
            let vel = DVec3::from_array(s.vel);
            if vel.is_finite() {
                e.vel = vel;
            }
            e.yaw = s.yaw;
            e.pitch = s.pitch;
            e.health = s.health.min(s.max_health.max(e.health));
        }
    }
}

unsafe extern "C" fn entity_push(w: *mut abi::WorldCtx, id: u64, v: *const [f64; 3]) {
    unsafe {
        let v = DVec3::from_array(*v);
        if !v.is_finite() {
            return;
        }
        let d = dim(w);
        let Some(e) = d.entities.get_mut(id) else { return };
        if e.player {
            let t = v / crate::world::TPS as f64;
            d.entities.pushed.push((id, ([t.x as f32, t.y as f32, t.z as f32], false)));
        } else {
            e.vel += v;
        }
    }
}

unsafe extern "C" fn entity_walk(w: *mut abi::WorldCtx, id: u64, dir: *const [f32; 2], speed: f32, jump: u32) {
    unsafe {
        let d = dim(w);
        let mul = d.attribute(content(), id, crate::server::MOVEMENT_SPEED, false) as f32;
        let jump_mul = d.attribute(content(), id, crate::server::JUMP_STRENGTH, false) as f32;
        if let Some(e) = d.entities.get_mut(id) {
            e.walk = Some((*dir, speed * mul, jump != 0));
            e.jump_mul = jump_mul;
        }
    }
}

unsafe extern "C" fn entity_look_at(w: *mut abi::WorldCtx, id: u64, max: f64, liquids: u32, pos: *mut [i32; 3], face: *mut u32, frame: *mut u64) -> u32 {
    unsafe {
        let Some(hit) = dim(w).look_ray(content(), id, max, liquids != 0) else { return 0 };
        *pos = hit.block.to_array();
        *face = hit.face as u32;
        *frame = hit.frame;
        hit.state + 1
    }
}

unsafe extern "C" fn entity_damage(w: *mut abi::WorldCtx, id: u64, amount: f32, source: u64, cause: u32) -> u32 {
    unsafe { dim(w).damage(content(), id, amount, source, cause) as u32 }
}

unsafe extern "C" fn entity_attack(w: *mut abi::WorldCtx, attacker: u64, target: u64) -> u32 {
    unsafe { dim(w).attack(content(), attacker, target, ItemStack::EMPTY) as u32 }
}

unsafe extern "C" fn entities_near(w: *mut abi::WorldCtx, pos: *const [f64; 3], radius: f64, kind: u32, out: *mut u64, max: usize) -> usize {
    unsafe {
        let ids = dim(w).entities.near(DVec3::from_array(*pos), radius, kind);
        for (i, id) in ids.iter().take(max).enumerate() {
            *out.add(i) = *id;
        }
        ids.len()
    }
}

unsafe extern "C" fn component(w: *mut abi::WorldCtx, id: u64, comp: u32) -> *mut u8 {
    unsafe {
        let d = dim(w);
        // Writes are found by comparing at sync time (`Entities::find_changes`).
        d.entities.comps.get_mut(&(id, comp)).map_or(std::ptr::null_mut(), |b| b.as_mut_ptr())
    }
}

unsafe extern "C" fn player_count(w: *mut abi::WorldCtx) -> u32 {
    unsafe { dim(w).visible_players().len() as u32 }
}

unsafe extern "C" fn player_at(w: *mut abi::WorldCtx, index: u32) -> u64 {
    unsafe { dim(w).visible_players().get(index as usize).copied().unwrap_or(0) }
}

unsafe extern "C" fn inv_size(w: *mut abi::WorldCtx, owner: u64) -> u32 {
    unsafe { dim(w).entities.inventories.get(&owner).map_or(0, |i| i.len() as u32) }
}

unsafe extern "C" fn inv_get(w: *mut abi::WorldCtx, owner: u64, slot: u32) -> ItemStack {
    unsafe { dim(w).entities.inventories.get(&owner).and_then(|i| i.get(slot as usize)).copied().unwrap_or_default() }
}

unsafe extern "C" fn inv_set(w: *mut abi::WorldCtx, owner: u64, slot: u32, stack: ItemStack) {
    unsafe { dim(w).entities.set_slot(owner, slot, stack) }
}

unsafe extern "C" fn inv_insert(w: *mut abi::WorldCtx, owner: u64, stack: ItemStack) -> ItemStack {
    unsafe { dim(w).entities.insert_item(content(), owner, stack) }
}

unsafe extern "C" fn selected_slot(w: *mut abi::WorldCtx, player: u64) -> u32 {
    unsafe { dim(w).entities.get(player).map_or(0, |e| e.selected) }
}

unsafe extern "C" fn spawn_item(w: *mut abi::WorldCtx, pos: *const [f64; 3], stack: ItemStack) {
    unsafe {
        dim(w).spawn_item(content(), DVec3::from_array(*pos), stack);
    }
}

unsafe extern "C" fn block_entity(w: *mut abi::WorldCtx, pos: *const [i32; 3]) -> u64 {
    unsafe { dim(w).entities.anchors.get(&ip(pos)).copied().unwrap_or(0) }
}

unsafe extern "C" fn open_menu(w: *mut abi::WorldCtx, player: u64, menu: u32, owner: u64) {
    unsafe {
        let d = dim(w);
        let c = content();
        let Some(def) = c.game.menus.get(menu as usize) else {
            log::error!("open_menu: there is no menu {menu}");
            return;
        };
        if owner != 0 && d.entities.get(owner).is_none_or(|e| e.removed) {
            log::error!("open_menu {}: entity {owner} is not here (removed, or in another region)", def.name);
            return;
        }
        d.events.push(crate::server::DimEvent::OpenMenu { player, menu, owner, block: 0 })
    }
}

unsafe extern "C" fn open_menu_at(w: *mut abi::WorldCtx, player: u64, menu: u32, pos: *const [i32; 3]) -> u32 {
    unsafe {
        let d = dim(w);
        let p = ip(pos);
        let c = content();
        let name = c.game.menus.get(menu as usize).map_or("?", |m| m.name.as_str());
        match d.entities.anchors.get(&p).copied() {
            Some(owner) => {
                open_menu(w, player, menu, owner);
                1
            }
            None => {
                let what = d.get(p).map_or("an unloaded block".to_string(), |s| c.state_name(s));
                log::error!("cannot open menu {name} at {p}: {what} has no block entity");
                0
            }
        }
    }
}

unsafe extern "C" fn inventory_create(w: *mut abi::WorldCtx, slots: u32, pos: *const [f64; 3]) -> u64 {
    unsafe {
        let d = dim(w);
        let c = content();
        let id = d.new_id();
        let p = DVec3::from_array(*pos);
        let e = crate::server::entity::Entity::new(id, c.game.virtual_kind, if p.is_finite() { p } else { DVec3::ZERO }, 1.0);
        d.entities.insert(c, e);
        d.entities.inventories.insert(id, vec![ItemStack::EMPTY; slots.min(1 << 16) as usize]);
        id
    }
}

unsafe extern "C" fn entity_teleport(w: *mut abi::WorldCtx, id: u64, to: u32, pos: *const [f64; 3]) {
    unsafe {
        let d = dim(w);
        d.events.push(crate::server::DimEvent::Teleport { from: d.id, id, dim: to, pos: DVec3::from_array(*pos) })
    }
}

unsafe extern "C" fn set_time(w: *mut abi::WorldCtx, time: u64) {
    unsafe {
        let d = dim(w);
        d.events.push(crate::server::DimEvent::SetTime { dim: d.id, time })
    }
}

unsafe extern "C" fn area_fill(w: *mut abi::WorldCtx, desc: *const abi::AreaFillDesc) {
    unsafe {
        let d = dim(w);
        let a = &*desc;
        if d.framed || !a.radius.is_finite() || a.radius <= 0.0 || a.state as usize >= crate::content::content().states.len() {
            return;
        }
        let fill = crate::server::area::AreaFill { id: 0, center: DVec3::from_array(a.center), radius: a.radius, state: a.state, keep_unbreakable: a.flags & abi::area_flags::KEEP_UNBREAKABLE != 0 };
        d.events.push(crate::server::DimEvent::AreaFill { dim: d.id, fill });
    }
}

unsafe extern "C" fn set_game_mode(w: *mut abi::WorldCtx, player: u64, mode: u32) {
    let mode = crate::world::GameMode::from_u8(mode.min(255) as u8);
    unsafe { dim(w).events.push(crate::server::DimEvent::SetMode { player, mode }) }
}

unsafe extern "C" fn send_packet(w: *mut abi::WorldCtx, player: u64, name: Str, schema: Str, data: abi::Slice<u8>) {
    unsafe {
        if let Some(channel) = channel_for(name.as_str(), schema.as_str()) {
            dim(w).events.push(crate::server::DimEvent::Packet { player, channel, data: data.as_slice().to_vec() });
        }
    }
}

/// The channel's id if it is registered and carries `schema`; a reason in the log otherwise.
pub fn channel_for(name: &str, schema: &str) -> Option<u32> {
    let g = &content().game;
    let Some(&channel) = g.packet_names.get(name) else {
        misuse("send", || format!("no channel {name} is registered"));
        return None;
    };
    let have = &g.packets[channel as usize].schema;
    if have != schema {
        misuse("send", || format!("channel {name} carries '{have}', not '{schema}'"));
        return None;
    }
    Some(channel)
}

unsafe extern "C" fn find_component(name: Str, schema: Str) -> u32 {
    let (name, schema) = unsafe { (name.as_str(), schema.as_str()) };
    let Some(c) = try_content() else { return u32::MAX };
    let Some(id) = c.components.id(name) else { return u32::MAX };
    let have = &c.components.schemas[id as usize];
    if !schema.is_empty() && have != schema {
        log::error!("component {name} is '{have}' but a mod reads it as '{schema}'");
        return u32::MAX;
    }
    id
}

unsafe extern "C" fn components() -> *const abi::ComponentTable {
    try_content().map_or(std::ptr::null(), |c| c.components.table())
}

unsafe extern "C" fn stack_get(data: u32, comp: u32, len: *mut usize) -> *const u8 {
    match crate::stack::value(data, comp) {
        Some(b) => {
            if !len.is_null() {
                unsafe { *len = b.len() }
            }
            b.as_ptr()
        }
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn stack_with(data: u32, comp: u32, value: abi::Slice<u8>, remove: u32) -> u32 {
    if try_content().is_none_or(|c| comp >= c.components.count()) {
        return data;
    }
    let bytes = unsafe { value.as_slice() };
    crate::stack::with(data, comp, (remove == 0).then_some(bytes))
}

unsafe extern "C" fn energy_insert(w: *mut abi::WorldCtx, id: u64, amount: i64, simulate: u32) -> i64 {
    unsafe { crate::server::energy::insert(&mut dim(w).entities, content(), id, amount, simulate != 0) }
}

unsafe extern "C" fn energy_extract(w: *mut abi::WorldCtx, id: u64, amount: i64, simulate: u32) -> i64 {
    unsafe { crate::server::energy::extract(&mut dim(w).entities, content(), id, amount, simulate != 0) }
}

unsafe extern "C" fn gizmo_on(w: *mut abi::WorldCtx, group: Str) -> u32 {
    unsafe { dim(w).gizmo_groups.iter().any(|g| g == group.as_str()) as u32 }
}

unsafe extern "C" fn gizmo(w: *mut abi::WorldCtx, group: Str, kind: u32, a: *const [f64; 3], b: *const [f64; 3], color: u32, ticks: u32, key: Str, args: abi::Slice<abi::UiArg>) {
    unsafe {
        let d = dim(w);
        let group = group.as_str();
        if kind > abi::gizmo_kind::FILL || !d.gizmo_groups.iter().any(|g| g == group) {
            return;
        }
        d.gizmos.push(crate::net::GizmoShape {
            group: group.to_string(),
            kind: kind as u8,
            a: DVec3::from_array(*a),
            b: DVec3::from_array(*b),
            color,
            ticks: ticks.max(1),
            text: crate::ui::text::Msg::new(key.as_str(), crate::client::modui::args(args)),
        });
    }
}

// ---------------------------------------------------------------- menus

unsafe fn menu<'a>(m: *mut abi::MenuCtx) -> &'a mut MenuCall {
    unsafe { &mut *(m as *mut MenuCall) }
}

unsafe extern "C" fn menu_group(m: *mut abi::MenuCtx, name: Str, out: *mut [u32; 3]) -> u32 {
    unsafe {
        match menu(m).group(content(), name.as_str()) {
            Some((start, count, cols)) => {
                *out = [start, count, cols];
                1
            }
            None => {
                misuse("menu group", || format!("the menu has no group {}", name.as_str()));
                0
            }
        }
    }
}

/// A machine's group in its entity's inventory: the menu built for its kind says where.
unsafe extern "C" fn inventory_group(w: *mut abi::WorldCtx, entity: u64, name: Str, out: *mut [u32; 2]) -> u32 {
    unsafe {
        let c = content();
        let Some(e) = dim(w).entities.get(entity) else { return 0 };
        let layout = &c.game.player_layout;
        // A player's groups are the player inventory's.
        if e.player {
            return match layout.group(name.as_str()) {
                Some(g) => {
                    *out = [g.start, g.count];
                    1
                }
                None => 0,
            };
        }
        let kind_name = &c.game.entities[e.kind as usize].name;
        let found = c.game.menus.iter().filter(|d| &d.entity == kind_name).find_map(|d| d.group(layout, name.as_str())).filter(|g| g.0.owner == abi::slot_owner::BLOCK);
        match found {
            Some((g, _, n)) => {
                *out = [g.start, n];
                1
            }
            None => 0,
        }
    }
}

unsafe extern "C" fn reg_player_inventory(r: *mut abi::Registrar, groups: abi::Slice<abi::SlotGroupDesc>) {
    unsafe {
        let r = reg(r);
        let groups = groups.as_slice().iter().map(|g| (g.name.as_str().to_string(), g.cols.max(1), g.rows.max(1), g.flags)).collect();
        r.game.player_inventory = Some((r.owner, groups));
    }
}

unsafe extern "C" fn player_group(name: Str, out: *mut [u32; 2]) -> u32 {
    unsafe {
        match content().game.player_layout.group(name.as_str()) {
            Some(g) => {
                *out = [g.start, g.count];
                1
            }
            None => 0,
        }
    }
}

unsafe extern "C" fn reg_equipment_slot(r: *mut abi::Registrar, name: Str, parts: u32, background: Str) {
    unsafe {
        let r = reg(r);
        r.game.equipment.push(registry::EquipmentSlot { name: name.as_str().to_string(), parts, background: background.as_str().to_string() });
    }
}

unsafe extern "C" fn reg_screen(r: *mut abi::Registrar, d: *const abi::ScreenDesc) {
    unsafe {
        let (r, d) = (reg(r), &*d);
        r.game.screens.push(registry::ScreenDef { name: d.name.as_str().to_string(), owner: r.owner, cb: d.build, flags: d.flags, on_key: d.on_key });
    }
}


unsafe extern "C" fn menu_get(m: *mut abi::MenuCtx, slot: u32) -> ItemStack {
    unsafe { menu(m).get(slot) }
}

unsafe extern "C" fn menu_set(m: *mut abi::MenuCtx, slot: u32, stack: ItemStack) {
    unsafe { menu(m).set(slot, stack) }
}


// ---------------------------------------------------------------- lookups

unsafe extern "C" fn recipe_match(kind: Str, grid: *const ItemStack, width: u32, height: u32) -> ItemStack {
    let kind = unsafe { kind.as_str() };
    let cells: Vec<u32> = (0..(width * height) as usize).map(|i| unsafe { (*grid.add(i)).item }).collect();
    content()
        .game
        .match_grid(kind, &cells, width, height, &content().components)
        .map_or(ItemStack::EMPTY, |r| r.result())
}

fn recipe_at(kind: Str, recipe: u32) -> Option<&'static registry::Recipe> {
    content().game.recipes.get(unsafe { kind.as_str() })?.get(recipe as usize)
}

unsafe extern "C" fn recipe_find(kind: Str, stacks: *const ItemStack, count: u32) -> u32 {
    let c = content();
    let stacks = unsafe { std::slice::from_raw_parts(stacks, count as usize) };
    let Some(list) = c.game.recipes.get(unsafe { kind.as_str() }) else { return 0 };
    list.iter().position(|r| c.game.find_inputs(r, stacks, &c.components).is_some()).map_or(0, |i| i as u32 + 1)
}

unsafe extern "C" fn recipe_info(kind: Str, recipe: u32, out: *mut abi::RecipeInfo) -> u32 {
    let Some(r) = recipe_at(kind, recipe) else { return 0 };
    let flags = if r.hidden { abi::recipe_flags::HIDDEN } else { 0 };
    unsafe { *out = abi::RecipeInfo { time: r.time, inputs: r.inputs.len() as u32, outputs: r.outputs.len() as u32, flags } };
    1
}

unsafe extern "C" fn recipe_output(kind: Str, recipe: u32, index: u32, stack: *mut ItemStack, chance: *mut f32) -> u32 {
    let Some(o) = recipe_at(kind, recipe).and_then(|r| r.outputs.get(index as usize)) else { return 0 };
    unsafe {
        *stack = match o.what {
            registry::Ingredient::Item(i) => ItemStack::new(i, o.count),
            _ => ItemStack::EMPTY,
        };
        *chance = o.chance;
    }
    1
}

unsafe extern "C" fn recipe_value(kind: Str, recipe: u32, component: Str, out: *mut u8, cap: u32) -> u32 {
    let c = content();
    let Some(id) = c.components.id(unsafe { component.as_str() }) else { return 0 };
    let Some(v) = recipe_at(kind, recipe).and_then(|r| r.value(id)) else { return 0 };
    let n = v.len().min(cap as usize);
    unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), out, n) };
    v.len() as u32
}

unsafe extern "C" fn recipe_consume(kind: Str, recipe: u32, stacks: *mut ItemStack, count: u32) -> u32 {
    let c = content();
    let Some(r) = recipe_at(kind, recipe) else { return 0 };
    let stacks = unsafe { std::slice::from_raw_parts_mut(stacks, count as usize) };
    let Some(found) = c.game.find_inputs(r, stacks, &c.components) else { return 0 };
    let counts = r.inputs.iter().zip(&r.input_counts).filter(|(i, _)| !matches!(i, registry::Ingredient::Empty)).map(|(_, &n)| n);
    for (i, n) in found.into_iter().zip(counts) {
        stacks[i].count -= n;
        if stacks[i].count == 0 {
            stacks[i] = ItemStack::EMPTY;
        }
    }
    1
}

unsafe extern "C" fn recipe_process(kind: Str, input: u32, time_out: *mut u32) -> ItemStack {
    let kind = unsafe { kind.as_str() };
    match content().game.process(kind, input, &content().components) {
        Some(r) => {
            if !time_out.is_null() {
                unsafe { *time_out = r.time }
            }
            r.result()
        }
        None => ItemStack::EMPTY,
    }
}

unsafe extern "C" fn item_block(item: u32) -> u32 {
    try_content().and_then(|c| c.items.get(item as usize)).and_then(|d| d.block).unwrap_or(0)
}

unsafe extern "C" fn find(kind: u32, name: Str, count: *mut u32) -> u32 {
    let name = unsafe { name.as_str() };
    let Some(c) = try_content() else { return u32::MAX };
    let mut n = 1;
    let id = match kind {
        // `#tag`: the first block carrying it, in mod load order (biome surfaces by tag).
        // `#tag`: its preferred block (mod load order) or item.
        abi::entry_kind::BLOCK if name.starts_with('#') => crate::refs::Ref::parse(name).ok().and_then(|r| crate::refs::state(&r, &c.block_names, &c.blocks, &c.components).ok()).inspect(|&s| n = c.block_of(s).states),
        abi::entry_kind::ITEM if name.starts_with('#') => crate::refs::Ref::parse(name).ok().and_then(|r| crate::refs::item(&r, &c.item_names, &c.components, &c.tag_items).ok()),
        abi::entry_kind::BLOCK if name.contains('[') => c.state_by_name(name),
        abi::entry_kind::BLOCK => c.block_names.get(name).map(|&b| {
            n = c.blocks[b as usize].states;
            c.blocks[b as usize].first_state
        }),
        abi::entry_kind::ITEM => c.item_names.get(name).copied(),
        abi::entry_kind::ENTITY => c.game.entity_names.get(name).copied(),
        abi::entry_kind::BIOME => c.biome_names.get(name).copied(),
        abi::entry_kind::DIMENSION => c.dim_names.get(name).copied(),
        abi::entry_kind::MENU => c.game.menu_names.get(name).copied(),
        abi::entry_kind::PARTICLE => c.game.particle_names.get(name).copied(),
        abi::entry_kind::CONFIG => c.game.config_names.get(name).copied(),
        abi::entry_kind::SOUND => c.game.sound_names.get(name).copied(),
        _ => None,
    };
    if !count.is_null() {
        unsafe { *count = if id.is_some() { n } else { 0 } }
    }
    id.unwrap_or(u32::MAX)
}

unsafe extern "C" fn name_of(kind: u32, id: u32) -> Str {
    static STATE_NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let Some(c) = try_content() else { return Str::EMPTY };
    let name: Option<&str> = match kind {
        abi::entry_kind::BLOCK => {
            let names = STATE_NAMES.get_or_init(|| (0..c.states.len() as u32).map(|s| c.state_name(s)).collect());
            names.get(id as usize).map(|s| s.as_str())
        }
        abi::entry_kind::ITEM => c.items.get(id as usize).map(|i| i.name.as_str()),
        abi::entry_kind::ENTITY => c.game.entities.get(id as usize).map(|e| e.name.as_str()),
        abi::entry_kind::BIOME => c.biomes.get(id as usize).map(|b| b.name.as_str()),
        abi::entry_kind::DIMENSION => c.dims.get(id as usize).map(|d| d.name.as_str()),
        abi::entry_kind::MENU => c.game.menus.get(id as usize).map(|m| m.name.as_str()),
        abi::entry_kind::PARTICLE => c.game.particles.get(id as usize).map(|p| p.name.as_str()),
        abi::entry_kind::CONFIG => c.game.configs.get(id as usize).map(|d| d.name.as_str()),
        abi::entry_kind::SOUND => c.game.sounds.get(id as usize).map(|d| d.name.as_str()),
        _ => None,
    };
    name.map_or(Str::EMPTY, Str::new)
}

unsafe extern "C" fn state_prop(state: u32, name: Str) -> Str {
    let c = content();
    if state as usize >= c.states.len() {
        return Str::EMPTY;
    }
    c.state_prop(state, unsafe { name.as_str() }).map_or(Str::EMPTY, Str::new)
}

unsafe extern "C" fn state_with(state: u32, name: Str, value: Str) -> u32 {
    let c = content();
    if state as usize >= c.states.len() {
        return u32::MAX;
    }
    unsafe { c.state_with(state, name.as_str(), value.as_str()) }.unwrap_or(u32::MAX)
}

unsafe extern "C" fn block_of(state: u32) -> u32 {
    let c = content();
    c.states.get(state as usize).map_or(0, |s| c.blocks[s.block as usize].first_state)
}

unsafe extern "C" fn state_flags(state: u32) -> u32 {
    content().states.get(state as usize).map_or(0, |s| s.flags)
}

// ---------------------------------------------------------------- registration

fn level(level: u32) -> log::Level {
    match level {
        abi::level::ERROR => log::Level::Error,
        abi::level::WARN => log::Level::Warn,
        abi::level::DEBUG => log::Level::Debug,
        abi::level::TRACE => log::Level::Trace,
        _ => log::Level::Info,
    }
}

fn mod_target(me: ModId, sub: &str) -> String {
    let name = crate::modloader::mod_id_name(me);
    if sub.is_empty() { format!("mod:{name}") } else { format!("mod:{name}::{sub}") }
}

unsafe extern "C" fn host_log(me: ModId, lvl: u32, msg: Str) {
    unsafe { log_at(me, lvl, Str::EMPTY, msg) }
}

unsafe extern "C" fn log_at(me: ModId, lvl: u32, target: Str, msg: Str) {
    let (target, msg) = unsafe { (mod_target(me, target.as_str()), msg.as_str()) };
    log::log!(target: &target, level(lvl), "{msg}");
}

unsafe extern "C" fn log_enabled(me: ModId, lvl: u32, target: Str) -> u32 {
    let lvl = level(lvl);
    if lvl > log::max_level() {
        return 0;
    }
    crate::log::enabled(&mod_target(me, unsafe { target.as_str() }), lvl) as u32
}

unsafe fn reg<'a>(r: *mut abi::Registrar) -> &'a mut RawRegistrar {
    unsafe { &mut *(r as *mut RawRegistrar) }
}

unsafe extern "C" fn reg_block(r: *mut abi::Registrar, d: *const abi::BlockDesc) {
    unsafe {
        let b = content::copy_block(&*d);
        let r = reg(r);
        if b.states.is_empty() {
            r.errors.push(format!("block {} has no states", b.name));
            return;
        }
        r.blocks.push(b);
    }
}

unsafe extern "C" fn reg_item(r: *mut abi::Registrar, d: *const abi::ItemDesc) {
    unsafe {
        let r = reg(r);
        let mut item = content::copy_item(&*d);
        item.seq = r.items.len() as u32;
        r.items.push(item);
    }
}

unsafe extern "C" fn reg_biome(r: *mut abi::Registrar, d: *const abi::BiomeDesc) {
    unsafe {
        let d = &*d;
        reg(r).biomes.push((
            content::BiomeDef {
                name: d.name.as_str().to_string(),
                tints: Vec::new(),
            },
            d.cell as usize,
            crate::components::RawValue::list(d.components),
            crate::server::terrain::copy_rule(&d.rule),
        ));
    }
}

unsafe extern "C" fn reg_dimension(r: *mut abi::Registrar, d: *const abi::DimensionDesc) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.dims.push(content::copy_dimension(owner, &*d));
    }
}

unsafe extern "C" fn reg_entity(r: *mut abi::Registrar, d: *const abi::EntityDesc) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.entities.push(registry::copy_entity(owner, &*d));
    }
}

unsafe extern "C" fn reg_entity_model(r: *mut abi::Registrar, kind: Str, parts: abi::Slice<abi::ModelPart>, uv_width: u32, uv_height: u32) {
    unsafe {
        let parts = registry::copy_parts(parts);
        reg(r).game.entity_models.push((kind.as_str().to_string(), parts, [uv_width, uv_height]));
    }
}

unsafe extern "C" fn reg_component(r: *mut abi::Registrar, d: *const abi::ComponentDesc) {
    unsafe {
        let d = &*d;
        let mut default = d.default.as_slice().to_vec();
        default.resize(d.value_size as usize, 0);
        let r = reg(r);
        r.game.components.push(registry::RawComponent {
            owner: r.owner,
            def: registry::ComponentDef { name: d.name.as_str().to_string(), size: d.value_size, flags: d.flags, default, runtime: true },
            schema: d.schema.as_str().to_string(),
        });
    }
}

unsafe extern "C" fn reg_world_tick(r: *mut abi::Registrar, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.world_ticks.push((owner, cb));
    }
}

unsafe extern "C" fn entity_after(w: *mut abi::WorldCtx, entity: u64, ticks: u32, event: u32, data: abi::Slice<u8>) {
    unsafe {
        let c = content();
        let Some(def) = c.game.events.get(event as usize) else {
            misuse("after", || format!("no event {event}"));
            return;
        };
        let d = dim(w);
        if d.entities.get(entity).is_none() {
            return;
        }
        let until = d.time + ticks.max(1) as u64;
        let list = d.entities.comps.entry((entity, c.game.timers_comp)).or_insert_with(|| Box::new([]));
        // One timer per entity and event: setting it again moves it.
        let mut v = Vec::new();
        for (t, name, value) in crate::server::dim::read_timers(list) {
            if name != def.name {
                crate::server::dim::push_timer(&mut v, t, &name, &value);
            }
        }
        crate::server::dim::push_timer(&mut v, until, &def.name, data.as_slice());
        *list = v.into_boxed_slice();
    }
}

unsafe extern "C" fn reg_recipe(r: *mut abi::Registrar, d: *const abi::RecipeDesc) {
    unsafe { reg(r).game.recipes.push(registry::copy_recipe(&*d)) }
}

unsafe extern "C" fn reg_menu(r: *mut abi::Registrar, d: *const abi::MenuDesc) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.menus.push(registry::copy_menu(owner, &*d));
    }
}

unsafe extern "C" fn reg_set(r: *mut abi::Registrar, d: *const abi::SetDesc) {
    use crate::components::Target;
    unsafe {
        let d = *d;
        let r = reg(r);
        let name = d.target.as_str().to_string();
        let target = match d.target_kind {
            // A reference: `#pattern` for everything tagged, else a name.
            abi::set_target::NAME if name.starts_with('#') => Target::Tagged(name[1..].to_string()),
            abi::set_target::NAME => Target::Name(name),
            abi::set_target::TAGGED => Target::Tagged(name),
            abi::set_target::ENTITY => Target::Entity(name),
            abi::set_target::BIOME => Target::Biome(name),
            abi::set_target::DIMENSION => Target::Dimension(name),
            k => {
                r.errors.push(format!("set {}: unknown target kind {k}", d.value.name.as_str()));
                return;
            }
        };
        let value = crate::components::RawValue::copy(&d.value);
        r.game.sets.push(crate::components::RawSet { owner: r.owner, target, value, remove: d.remove != 0 });
    }
}

unsafe extern "C" fn reg_creative_tab(r: *mut abi::Registrar, name: Str, icon: Str) {
    unsafe {
        let r = reg(r);
        r.game.creative_tabs.push((name.as_str().to_string(), icon.as_str().to_string()));
    }
}

unsafe extern "C" fn reg_screen_part(r: *mut abi::Registrar, screen: Str, area: Str, order: i32, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.screen_parts.push(registry::ScreenPart { screen: screen.as_str().to_string(), area: area.as_str().to_string(), order, owner, cb });
    }
}

unsafe extern "C" fn reg_scatter(r: *mut abi::Registrar, d: *const abi::ScatterDesc) {
    unsafe {
        let r = reg(r);
        let d = &*d;
        r.game.scatters.push(registry::RawScatter {
            owner: r.owner,
            name: d.name.as_str().to_string(),
            dimensions: d.dimensions.as_str().split_whitespace().map(str::to_string).collect(),
            cell: d.cell,
            reach: d.reach,
            density: d.density.as_str().to_string(),
            shape: d.shape,
        });
    }
}

unsafe extern "C" fn reg_decoration(r: *mut abi::Registrar, dimensions: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        r.game.decorations.push((r.owner, dimensions.as_str().split_whitespace().map(str::to_string).collect(), cb));
    }
}

unsafe extern "C" fn reg_terrain(r: *mut abi::Registrar, d: *const abi::TerrainDesc) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.terrain.push(crate::server::terrain::copy(owner, &*d));
    }
}

unsafe extern "C" fn dim_column(dim: u32, seed: u64, x: i32, z: i32, out: *mut abi::TerrainColumn) -> u32 {
    let Some(col) = content().dims.get(dim as usize).and_then(|d| d.column(seed, x, z)) else { return 0 };
    unsafe { *out = col };
    1
}

unsafe extern "C" fn dim_carved(dim: u32, seed: u64, x: i32, z: i32) -> u32 {
    content().dims.get(dim as usize).is_some_and(|d| d.carved(seed, x, z)) as u32
}

unsafe extern "C" fn reg_ore(r: *mut abi::Registrar, d: *const abi::OreDesc) {
    unsafe {
        let r = reg(r);
        let d = &*d;
        r.game.ores.push(registry::RawOre {
            dimensions: d.dimensions.as_str().split_whitespace().map(str::to_string).collect(),
            block: d.block.as_str().to_string(),
            replace: d.replace.as_str().split_whitespace().map(str::to_string).collect(),
            min_y: d.min_y,
            max_y: d.max_y,
            veins: d.veins,
            size: d.blocks,
            biome: d.biome.as_str().to_string(),
        });
    }
}

unsafe extern "C" fn reg_command(r: *mut abi::Registrar, name: Str, usage: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.commands.push((owner, name.as_str().to_string(), usage.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn command_reply(args: *mut abi::CommandArgs, key: Str, list: abi::Slice<abi::UiArg>) {
    unsafe {
        let reply = &mut *((*args).reply as *mut Vec<crate::ui::text::Msg>);
        reply.push(crate::ui::text::Msg::new(key.as_str(), crate::client::modui::args(list)));
    }
}

unsafe extern "C" fn is_creative(w: *mut abi::WorldCtx, entity: u64) -> u32 {
    unsafe { dim(w).entities.get(entity).is_some_and(|e| e.mode.invulnerable()) as u32 }
}

unsafe extern "C" fn reg_hud_element(r: *mut abi::Registrar, name: Str, anchor: Str, order: i32, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.huds.push(registry::HudElement { name: name.as_str().to_string(), title: crate::ui::text::dotted(name.as_str()), anchor: anchor.as_str().to_string(), order, owner, cb });
    }
}

unsafe extern "C" fn set_player_menu(r: *mut abi::Registrar, menu: Str) {
    unsafe {
        let r = reg(r);
        r.game.player_menu = Some((r.owner, menu.as_str().to_string()));
    }
}

unsafe extern "C" fn reg_tint_kind(r: *mut abi::Registrar, name: Str, default: u32) {
    unsafe { reg(r).tint_kinds.push((name.as_str().to_string(), default)) }
}

unsafe extern "C" fn reg_biome_tint(r: *mut abi::Registrar, biome: Str, kind: Str, color: u32) {
    unsafe { reg(r).biome_tints.push((biome.as_str().to_string(), kind.as_str().to_string(), color)) }
}

unsafe extern "C" fn reg_sound_category(r: *mut abi::Registrar, name: Str, world: u32) {
    unsafe { reg(r).game.sound_categories.push(registry::SoundCategory::new(name.as_str(), world != 0)) }
}

unsafe extern "C" fn reg_entity_category(r: *mut abi::Registrar, name: Str, cap: u32, despawn: f32) {
    unsafe { reg(r).game.categories.push(crate::registry::CategoryDef { name: name.as_str().to_string(), cap, despawn: despawn as f64 }) }
}

unsafe extern "C" fn reg_recipe_kind(r: *mut abi::Registrar, d: *const abi::RecipeKindDesc) {
    unsafe {
        let d = &*d;
        let r = reg(r);
        let def = registry::RecipeKindDef {
            owner: r.owner,
            input_grid: d.input_grid,
            output_grid: d.output_grid,
            info: (d.info.len > 0).then(|| d.info.as_str().to_string()),
            info_values: d.info_values.as_slice().iter().map(|s| s.as_str().to_string()).collect(),
            display: d.display.is_some().then_some(d.display),
        };
        let owner = r.owner;
        r.game.recipe_kinds.push(registry::RawRecipeKind { owner, name: d.name.as_str().to_string(), def });
    }
}

unsafe extern "C" fn reg_attach_value(r: *mut abi::Registrar, kind: Str, component: Str, value: abi::Slice<u8>) {
    let value = unsafe { value.as_slice() };
    unsafe { reg(r).game.attach.push((kind.as_str().to_string(), component.as_str().to_string(), (!value.is_empty()).then(|| value.to_vec()))) }
}

unsafe extern "C" fn reg_liquid(r: *mut abi::Registrar, d: *const abi::LiquidDesc) {
    unsafe {
        let d = *d;
        reg(r).game.liquids.push((d.name.as_str().to_string(), d));
    }
}

unsafe extern "C" fn reg_liquid_reaction(r: *mut abi::Registrar, d: *const abi::ReactionDesc) {
    unsafe {
        let d = *d;
        reg(r).game.reactions.push([d.liquid, d.touching, d.source, d.flowing, d.falls_into].map(|s| s.as_str().to_string()));
    }
}

unsafe extern "C" fn liquid_of(state: u32) -> u32 {
    try_content().and_then(|c| c.states.get(state as usize)).and_then(|s| s.liquid).map_or(u32::MAX, |l| l as u32)
}

unsafe extern "C" fn use_container(w: *mut abi::WorldCtx, player: u64, stack: ItemStack, handled: *mut u32) -> ItemStack {
    unsafe {
        let r = dim(w).use_container(content(), player, stack);
        if !handled.is_null() {
            *handled = r.is_some() as u32;
        }
        r.unwrap_or(stack)
    }
}

unsafe extern "C" fn reg_event_handler(r: *mut abi::Registrar, name: Str, schema: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.event_handlers.push((owner, name.as_str().to_string(), schema.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn find_event(name: Str, schema: Str) -> u32 {
    let (name, schema) = unsafe { (name.as_str(), schema.as_str()) };
    let Some(c) = try_content() else { return u32::MAX };
    let Some(&id) = c.game.event_names.get(name) else { return u32::MAX };
    let def = &c.game.events[id as usize];
    if !schema.is_empty() && def.schema != schema {
        log::error!("event {name} is '{}' but a mod emits '{schema}'", def.schema);
        return u32::MAX;
    }
    if def.handlers.is_empty() { u32::MAX } else { id }
}

unsafe extern "C" fn emit(w: *mut abi::WorldCtx, event: u32, value: abi::Slice<u8>) {
    unsafe { dim(w).emit(content(), event, value.as_slice()) }
}

unsafe extern "C" fn reg_interceptor(r: *mut abi::Registrar, name: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.interceptors.push((owner, name.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn ask(w: *mut abi::WorldCtx, name: Str, query: *mut abi::Query) -> u32 {
    unsafe {
        let mut q = *query;
        let ok = dim(w).ask(content(), name.as_str(), &mut q);
        *query = q;
        ok as u32
    }
}

unsafe extern "C" fn reg_modifier(r: *mut abi::Registrar, name: Str, stage: u32, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.modifiers.push((owner, name.as_str().to_string(), stage, cb));
    }
}

unsafe extern "C" fn compute(w: *mut abi::WorldCtx, name: Str, base: f64, ctx: *const abi::Query, explain: u32) -> f64 {
    unsafe {
        let q = if ctx.is_null() { abi::Query::DEFAULT } else { *ctx };
        dim(w).compute(content(), name.as_str(), base, &q, explain != 0)
    }
}

unsafe extern "C" fn explanation() -> Str {
    thread_local! {
        static TEXT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }
    TEXT.with(|t| {
        let mut t = t.borrow_mut();
        *t = crate::server::explanation();
        Str { ptr: t.as_ptr(), len: t.len() }
    })
}

unsafe extern "C" fn reg_attribute(r: *mut abi::Registrar, name: Str, default: f64, min: f64, max: f64) {
    unsafe { reg(r).game.attributes.push((name.as_str().to_string(), default, min, max)) }
}

unsafe extern "C" fn attribute(w: *mut abi::WorldCtx, entity: u64, name: Str) -> f64 {
    unsafe { dim(w).attribute_named(content(), entity, name.as_str(), false) }
}

unsafe extern "C" fn attribute_explain(w: *mut abi::WorldCtx, entity: u64, name: Str) -> f64 {
    unsafe { dim(w).attribute_named(content(), entity, name.as_str(), true) }
}

unsafe extern "C" fn attribute_modifier(w: *mut abi::WorldCtx, entity: u64, name: Str, id: Str, op: u32, amount: f64, ticks: u64) {
    unsafe {
        let c = content();
        match c.game.attr_names.get(name.as_str()) {
            Some(&a) => dim(w).set_attr_modifier(c, entity, a, id.as_str(), op, amount, ticks),
            None => log::error!("no attribute {}", name.as_str()),
        }
    }
}

unsafe extern "C" fn attribute_remove(w: *mut abi::WorldCtx, entity: u64, name: Str, id: Str) {
    unsafe {
        let c = content();
        match c.game.attr_names.get(name.as_str()) {
            Some(&a) => dim(w).remove_attr_modifier(c, entity, a, id.as_str()),
            None => misuse("attribute_remove", || format!("no attribute {}", name.as_str())),
        }
    }
}

unsafe extern "C" fn reg_loot(r: *mut abi::Registrar, d: *const abi::LootDesc) {
    unsafe {
        let d = *d;
        let r = reg(r);
        let mut pools = Vec::new();
        d.pools.as_slice().iter().copied().for_each(|p| pools.push(crate::loot::copy_pool(&p)));
        r.game.loot.push(crate::loot::RawLoot { owner: r.owner, name: d.name.as_str().to_string(), pools });
    }
}

unsafe extern "C" fn reg_loot_modifier(r: *mut abi::Registrar, d: *const abi::LootModDesc) {
    unsafe {
        let d = *d;
        let r = reg(r);
        let pool = crate::loot::copy_pool(&d.pool);
        r.game.loot_mods.push(crate::loot::RawLootMod { owner: r.owner, table: d.table.as_str().to_string(), op: d.op, pool, from: d.from.as_str().to_string(), to: d.to.as_str().to_string() });
    }
}

unsafe extern "C" fn roll_loot(w: *mut abi::WorldCtx, name: Str, ctx: *const abi::LootCtx, out: *mut ItemStack, max: usize) -> usize {
    unsafe {
        let x = if ctx.is_null() { abi::LootCtx::DEFAULT } else { *ctx };
        let ctx = crate::loot::Ctx { tool: x.tool, by_player: x.by_player != 0, on_fire: x.on_fire != 0, mined: x.mined };
        let stacks = dim(w).roll_loot(content(), name.as_str(), &ctx);
        for (i, s) in stacks.iter().take(max).enumerate() {
            *out.add(i) = *s;
        }
        stacks.len()
    }
}

unsafe extern "C" fn break_block(w: *mut abi::WorldCtx, pos: *const [i32; 3], tool: ItemStack, player: u64, drops: u32) -> u32 {
    unsafe { dim(w).break_block(content(), ip(pos), tool, player, drops != 0) as u32 }
}

unsafe extern "C" fn set_entity_size(w: *mut abi::WorldCtx, id: u64, width: f32, height: f32, eye: f32) {
    unsafe {
        let d = dim(w);
        if let Some(e) = d.entities.get_mut(id) {
            e.size = (width > 0.0).then_some([width, height.max(0.0), eye.clamp(0.0, height.max(0.0))]);
        }
    }
}

unsafe extern "C" fn set_entity_look(w: *mut abi::WorldCtx, id: u64, kind: Str) {
    unsafe {
        let name = kind.as_str();
        let look = match name {
            "" => None,
            n => match content().game.entity_names.get(n) {
                Some(&k) => Some(k),
                None => return misuse("set_entity_look", || format!("no entity kind {n}")),
            },
        };
        let d = dim(w);
        if let Some(e) = d.entities.get_mut(id) {
            e.look = look;
        }
    }
}

unsafe extern "C" fn reg_words(r: *mut abi::Registrar, set: Str, words: Str) {
    unsafe {
        let words = words.as_str().split(',').map(str::trim).filter(|w| !w.is_empty()).map(String::from).collect();
        reg(r).game.words.push((set.as_str().to_string(), words));
    }
}

unsafe extern "C" fn reg_command_branch(r: *mut abi::Registrar, command: Str, usage: Str, client: u32, cb: Callback) {
    unsafe {
        let r = reg(r);
        let ext = registry::CommandExtension::Branch { command: command.as_str().to_string(), usage: usage.as_str().to_string(), client: client != 0, cb };
        let owner = r.owner;
        r.game.command_extensions.push((owner, ext));
    }
}

unsafe extern "C" fn reg_client_command(r: *mut abi::Registrar, name: Str, usage: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.client_commands.push((owner, name.as_str().to_string(), usage.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn reg_system(r: *mut abi::Registrar, name: Str, schema: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.systems.push((owner, name.as_str().to_string(), schema.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn reg_stack_component(r: *mut abi::Registrar, name: Str, schema: Str, flags: u32) {
    unsafe { reg(r).game.stack_components.push((name.as_str().to_string(), schema.as_str().to_string(), flags)) }
}

unsafe extern "C" fn reg_client_hook(r: *mut abi::Registrar, hook: u32, cb: Callback) {
    unsafe {
        let r = reg(r);
        if hook > abi::client_hook::SCREEN {
            r.errors.push(format!("client hook {hook} does not exist"));
            return;
        }
        let owner = r.owner;
        r.game.client_hooks.push((owner, hook, cb));
    }
}

unsafe extern "C" fn reg_client_entity_tick(r: *mut abi::Registrar, kind: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.client_entity_ticks.push((owner, kind.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn reg_packet(r: *mut abi::Registrar, name: Str, schema: Str, side: u32, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.packets.push((owner, name.as_str().to_string(), schema.as_str().to_string(), side.min(1), cb));
    }
}

unsafe extern "C" fn reg_error(r: *mut abi::Registrar, message: Str) {
    unsafe { reg(r).errors.push(message.as_str().to_string()) }
}

unsafe extern "C" fn reg_key(r: *mut abi::Registrar, name: Str, key: Str) {
    unsafe {
        let r = reg(r);
        if crate::settings::parse_chord(key.as_str()).is_none() {
            r.errors.push(format!("key {}: unknown key name {:?}", name.as_str(), key.as_str()));
        }
        r.game.keys.push((name.as_str().to_string(), key.as_str().to_string()))
    }
}

unsafe extern "C" fn set_block_entity(r: *mut abi::Registrar, block: Str, entity: Str, slots: u32) {
    unsafe { reg(r).game.block_entities.push((block.as_str().to_string(), entity.as_str().to_string(), slots)) }
}

unsafe extern "C" fn reg_text_condition(r: *mut abi::Registrar, name: Str, cb: Callback) {
    unsafe {
        let r = reg(r);
        let owner = r.owner;
        r.game.text_conditions.push((owner, name.as_str().to_string(), cb));
    }
}

unsafe extern "C" fn reg_particle(r: *mut abi::Registrar, d: *const abi::ParticleDesc) {
    unsafe {
        let d = *d;
        let textures = d.textures.as_slice().iter().map(|t| t.as_str().to_string()).collect();
        let p = registry::RawParticle {
            name: d.name.as_str().to_string(),
            cell: d.cell as usize,
            textures,
            shader: d.shader.as_str().to_string(),
            desc: abi::ParticleDesc { name: Str::EMPTY, cell: std::ptr::null(), textures: abi::Slice::EMPTY, shader: Str::EMPTY, ..d },
        };
        reg(r).game.particles.push(p);
    }
}

unsafe extern "C" fn particles(w: *mut abi::WorldCtx, b: *const abi::ParticleBurst) {
    unsafe {
        let b = *b;
        if (b.kind as usize) < content().game.particles.len() {
            let d = dim(w);
            d.events.push(crate::server::DimEvent::Particles { dim: d.id, burst: b });
        } else {
            misuse("particles", || format!("no particle {}", b.kind));
        }
    }
}

unsafe extern "C" fn reg_config(r: *mut abi::Registrar, d: *const abi::ConfigDesc) {
    unsafe {
        let d = *d;
        let def = crate::config::ConfigDef {
            name: d.name.as_str().to_string(),
            kind: d.kind,
            default: d.default,
            min: d.min,
            max: d.max,
            step: d.step,
            choices: d.choices.as_slice().iter().map(|s| s.as_str().to_string()).collect(),
            flags: d.flags,
            scope: d.scope,
            text: d.text.as_str().to_string(),
        };
        if def.kind == abi::config_kind::TEXT && def.scope != abi::config_scope::CLIENT {
            reg(r).errors.push(format!("setting {}: text settings are the client's own", def.name));
            return;
        }
        let def = crate::config::ConfigDef { default: def.fit(d.default), ..def };
        reg(r).game.configs.push((d.cell as usize, def));
    }
}

unsafe extern "C" fn config_value(id: u32) -> f64 {
    crate::config::get(id)
}

unsafe extern "C" fn config_set(id: u32, value: f64) {
    crate::config::set(id, value);
}

unsafe extern "C" fn config_text(id: u32) -> Str {
    thread_local! {
        static TEXT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    }
    TEXT.with(|t| {
        *t.borrow_mut() = crate::config::text(id);
        let t = t.borrow();
        Str { ptr: t.as_ptr(), len: t.len() }
    })
}

unsafe extern "C" fn config_set_text(id: u32, value: Str) {
    unsafe { crate::config::set_text(id, value.as_str()) }
}
