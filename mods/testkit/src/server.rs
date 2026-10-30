//! Server half: the operations tests run, as the command `/tk` (see `OPS`). The engine parses
//! their arguments like any command's, and the answer goes back to the test that ran it.

use std::sync::atomic::{AtomicU32, Ordering};

use loom_sdk::abi::entry_kind as ek;
use loom_sdk::*;

/// How many of each event arrived since `events reset`, in `EVENT_NAMES` order.
static COUNTS: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
const EVENT_NAMES: [&str; 8] = ["block_broken", "block_placed", "item_picked_up", "entity_died", "entity_hurt", "player_joined", "item_used", "smelted"];

use base_api::SMELTED;
static GOLD: Block = Block::named("base:gold_block");
/// Shared with everyone who sees the entity, for the sync test.
pub static MARK: Component<u32> = Component::named("testkit:mark");
/// Server-side stack data that is not saved either.
pub static SECRET: Component<u32> = Component::named("testkit:secret");
static PIG: Entity = Entity::named("base:pig");
pub static COUNT_MARKS: Config<bool> = Config::named("count_marks");
pub static PROTECT_GOLD: Config<bool> = Config::named("protect_gold");
pub static HALF_DAMAGE: Config<bool> = Config::named("half_damage");
/// Mobs may not attack while it is on.
pub static PEACE: Config<bool> = Config::named("peace");
/// Players can't be hurt (`question::HURT`) or use items (`question::USE_ITEM`) while on.
pub static NO_HURT: Config<bool> = Config::named("no_hurt");
pub static NO_USE: Config<bool> = Config::named("no_use");
value! {
    /// A number sent to the server, which answers with it doubled (typed channels).
    pub struct Echo { pub n: u32 }
}
pub static ECHO: Channel<Echo> = Channel::named("echo");
pub static ECHO_BACK: Channel<Echo> = Channel::named("echo_back");
/// Text both ways: the server answers with a greeting.
pub static GREET: Channel<String> = Channel::named("greet");
/// Fired by `after` timers; counted.
static RANG: Event<u64> = Event::named("rang");
static RINGS: AtomicU32 = AtomicU32::new(0);
/// Mobs walked by tests (`drive`): (entity, direction, ticks left).
static DRIVEN: std::sync::Mutex<Vec<(u64, [f32; 2], u32)>> = std::sync::Mutex::new(Vec::new());
/// A key whose presses reach the server: the test counts them there.
pub static PING: Key = Key::named("ping");
static PINGS: AtomicU32 = AtomicU32::new(0);

fn count(i: usize, n: usize) {
    COUNTS[i].fetch_add(n as u32, Ordering::Relaxed);
}

pub fn register(r: &mut Registry) {
    r.entity_component(&MARK, 0, component_flags::SYNC_WATCHERS);
    r.attach(&PIG, &MARK);
    r.config(&COUNT_MARKS).bool(false).world();
    // While the rule is on, every marked pig counts up once a tick.
    r.system(&MARK, |b| {
        if !COUNT_MARKS.get() {
            return;
        }
        for m in b.values().iter_mut().filter(|m| **m > 0) {
            *m += 1;
        }
    });
    r.stack_component(&SECRET, abi::stack_flags::SERVER_ONLY | abi::stack_flags::NO_SAVE);
    // Block properties are components: another mod's block glows blue (testkit is never shipped).
    r.set("base:mossy_cobblestone", &LIGHT, [0, 0, 9]);
    // Switches for the tests of the extension mechanisms, off unless a test turns them on.
    r.config(&PROTECT_GOLD).bool(false).world();
    r.config(&PEACE).bool(false).world();
    r.intercept(&question::ATTACK, |q| {
        let w = q.world();
        let mob = w.entity(q.entity()).is_some_and(|e| !w.each_player().any(|p| p == q.entity()) && e.health > 0.0);
        if mob && PEACE.get() { Verdict::Deny } else { Verdict::Abstain }
    });
    r.config(&NO_HURT).bool(false).world();
    r.config(&NO_USE).bool(false).world();
    let player = |w: &World, id: u64| w.each_player().any(|p| p == id);
    r.intercept(&question::HURT, move |q| if player(&q.world(), q.entity()) && NO_HURT.get() { Verdict::Deny } else { Verdict::Abstain });
    r.intercept(&question::USE_ITEM, |_| if NO_USE.get() { Verdict::Deny } else { Verdict::Abstain });
    r.on(&RANG, |b| {
        RINGS.fetch_add(b.len() as u32, Ordering::Relaxed);
    });
    r.config(&HALF_DAMAGE).bool(false).world();
    r.intercept(&question::BREAK_BLOCK, |q| if GOLD.is(q.state()) && PROTECT_GOLD.get() { Verdict::Deny } else { Verdict::Abstain });
    r.modify(&chain::DAMAGE, stage::FINAL, |m| {
        if HALF_DAMAGE.get() {
            m.value *= 0.5;
        }
    });
    r.on(&BLOCK_BROKEN, |b| count(0, b.len()));
    r.on(&BLOCK_PLACED, |b| count(1, b.len()));
    r.on(&ITEM_PICKED_UP, |b| count(2, b.len()));
    r.on(&ENTITY_DIED, |b| count(3, b.len()));
    r.on(&ENTITY_HURT, |b| count(4, b.len()));
    r.on(&PLAYER_JOINED, |b| count(5, b.len()));
    r.on(&ITEM_USED, |b| count(6, b.len()));
    r.on(&SMELTED, |b| count(7, b.len()));
    r.key(&PING, "F10").on_press_server(|_| {
        PINGS.fetch_add(1, Ordering::Relaxed);
    });
    r.packet(&ECHO, |p, e| p.reply(&ECHO_BACK, &Echo { n: e.n * 2 }));
    r.packet(&GREET, |p, name| p.reply(&GREET, &format!("hello, {name}")));
    // A mob walks where a test says for a while, after its own AI had its say.
    r.world_tick(|w| {
        let mut driven = DRIVEN.lock().unwrap();
        for (id, dir, left) in driven.iter_mut() {
            if w.entity(*id).is_some() && *left > 0 {
                w.walk(*id, *dir, 1.0, false);
                *left -= 1;
            }
        }
        driven.retain(|d| d.2 > 0);
    });
    register_ops(r);
}

/// The operations tests run on the server, as one command (`/tk set ~ ~ ~2 stone`): the engine
/// parses positions (`~dx`), block, item and entity names, and the answer goes back to the test
/// that ran it (`Client::run_command`), as `ok ..` or `err ..`.
const OPS: &[(&str, &str)] = &[
    ("set", "<pos:pos> <block:block>"),
    ("fill", "<from:pos> <to:pos> <block:block>"),
    ("get", "<pos:pos>"),
    ("crater", "<r:int>"),
    ("crater_fast", "<r:int>"),
    ("group", "@owner <name:word>"),
    ("pings", ""),
    ("setprop", "<pos:pos> <prop:word> <value:word>"),
    ("sblock", "<id:int> <pos:pos> [block:block]"),
    ("light", "<pos:pos>"),
    ("slot", "@owner <index:int>"),
    ("put", "@owner <index:int> <item:item> [count:int]"),
    ("give", "<item:item> [count:int]"),
    ("insert", "<pos:pos> <item:item> [count:int]"),
    ("inv", "@owner"),
    ("spawn", "<kind:entity> <pos:pos>"),
    ("entity", "@entity"),
    ("size", "@entity"),
    ("near", "<pos:pos> <r:float>"),
    ("animate", "@owner [name:word]"),
    ("survey", "<r:int>"),
    ("drops", "<n:int>"),
    ("find", "<r:int> <block:block>"),
    ("census", "<r:int> <blocks:word>"),
    ("onto", "<id:int> <h:float> [rider:int]"),
    ("navigate", "@owner <to:pos>"),
    ("structure", "<name:word> <r:int>"),
    ("after", "<ticks:int>"),
    ("rings", ""),
    ("look", ""),
    ("place", "<template:word> <pos:pos>"),
    ("heal", ""),
    ("push", "<x:float> <y:float> <z:float> [id:int]"),
    ("drive", "<id:int> <dx:float> <dz:float> <ticks:int>"),
    ("health", "[id:int]"),
    ("time", "<ticks:int>"),
    ("mode", "survival | creative | spectator"),
    ("menu", "<which:word> <pos:pos>"),
    ("vmenu", "<which:word> <slots:int> [items:text]"),
    ("remove", "<id:int>"),
    ("mark", "<id:int> <value:int>"),
    ("secret", "@owner <index:int> [value:int]"),
    ("events", " | reset"),
    ("attr", "@owner <attribute:word>"),
    ("attr_mod", "@owner <attribute:word> <mod:word> <op:int> <amount:float> <ticks:int>"),
    ("attr_rm", "@owner <attribute:word> <mod:word>"),
    ("loot", "<table:word> <n:int> [ctx:text]"),
    ("break", "<pos:pos> [tool:item]"),
    ("damage", "<amount:float> [cause:int]"),
    ("snapshot", "<from:pos> <to:pos>"),
];

/// The usage of `/tk`: every operation, an owner (`@owner`) written as `me`, an entity id or a
/// block entity's position, an entity (`@entity`) as `me` or an id.
fn usage() -> String {
    let mut alts = Vec::new();
    for (op, args) in OPS {
        for args in args.split(" | ") {
            let forms: &[&str] = if args.contains("@owner") {
                &["me", "<id:int>", "<at:pos>"]
            } else if args.contains("@entity") {
                &["me", "<id:int>"]
            } else {
                &[""]
            };
            for f in forms {
                let a = args.replace("@owner", f).replace("@entity", f);
                alts.push(format!("{op} {a}").trim().to_string());
            }
        }
    }
    alts.join(" | ")
}

pub fn register_ops(r: &mut Registry) {
    r.command("tk", &usage(), |cmd| {
        let w = cmd.world();
        let answer = match run(&w, cmd) {
            Ok(s) => ("testkit.ok", s),
            Err(e) => ("testkit.err", e),
        };
        cmd.reply(answer.0, &[Arg::Text(&answer.1)]);
    });
}

fn pos(cmd: &Command, name: &str) -> Result<[i32; 3], String> {
    let p = cmd.pos(name).ok_or_else(|| format!("missing {name}"))?;
    Ok([p[0].floor() as i32, p[1].floor() as i32, p[2].floor() as i32])
}

fn int<T: TryFrom<i64>>(cmd: &Command, name: &str) -> Result<T, String> {
    cmd.int(name).and_then(|v| T::try_from(v).ok()).ok_or_else(|| format!("missing {name}"))
}

fn word<'a>(cmd: &'a Command, name: &str) -> Result<&'a str, String> {
    cmd.text(name).ok_or_else(|| format!("missing {name}"))
}

fn id(cmd: &Command, name: &str) -> Result<u32, String> {
    cmd.id(name).ok_or_else(|| format!("missing {name}"))
}

/// `me`, an entity id or the block entity at a position.
fn owner(w: &World, cmd: &Command) -> Result<u64, String> {
    if let Some(id) = cmd.int("id") {
        return Ok(id as u64);
    }
    if cmd.pos("at").is_none() {
        return Ok(cmd.player());
    }
    let p = pos(cmd, "at")?;
    match w.block_entity(p) {
        0 => Err(format!("no block entity at {p:?} ({})", block_name(w.get(p)))),
        id => Ok(id),
    }
}

/// An item by name, base's when no namespace is given (lists inside one argument).
fn item(name: &str) -> Result<u32, String> {
    let n = if name.contains(':') { name.to_string() } else { format!("base:{name}") };
    Item::lookup(&n).ok_or_else(|| format!("no item {n}"))
}

fn stack_text(s: ItemStack) -> String {
    if s.is_empty() { "- 0 0".into() } else { format!("{} {} {}", item_name(s.item), s.count, s.damage()) }
}

fn run(w: &World, cmd: &Command) -> Result<String, String> {
    let player = cmd.player();
    let me = w.entity(player).map(|e| [e.pos[0].floor() as i32, e.pos[1].floor() as i32, e.pos[2].floor() as i32]).unwrap_or_default();
    // The operation is the literal that came first; a parameter may share its name (`slot`).
    let op = OPS.iter().map(|o| o.0).find(|o| cmd.text(o) == Some(*o)).ok_or("no operation")?;
    match op {
        "set" => {
            let p = pos(cmd, "pos")?;
            if !w.set(p, id(cmd, "block")?) {
                return Err(format!("{p:?} is not loaded"));
            }
            Ok(String::new())
        }
        "fill" => {
            let (p, q) = (pos(cmd, "from")?, pos(cmd, "to")?);
            let s = id(cmd, "block")?;
            let (lo, hi) = ([p[0].min(q[0]), p[1].min(q[1]), p[2].min(q[2])], [p[0].max(q[0]), p[1].max(q[1]), p[2].max(q[2])]);
            let volume = (0..3).map(|i| (hi[i] - lo[i] + 1) as i64).product::<i64>();
            if volume > 1 << 18 {
                return Err(format!("fill of {volume} blocks is too big"));
            }
            let mut missed = 0;
            for y in lo[1]..=hi[1] {
                for z in lo[2]..=hi[2] {
                    for x in lo[0]..=hi[0] {
                        missed += !w.set([x, y, z], s) as u32;
                    }
                }
            }
            if missed > 0 { Err(format!("{missed} blocks not loaded")) } else { Ok(volume.to_string()) }
        }
        "get" => Ok(block_name(w.get(pos(cmd, "pos")?)).to_string()),
        "crater_fast" => {
            // The same through the area edit: one call, done at the end of the tick.
            let r: i32 = int(cmd, "r")?;
            w.fill_sphere([me[0] as f64 + 0.5, me[1] as f64 + 0.5, me[2] as f64 + 0.5], r as f64, 0, true);
            Ok(String::new())
        }
        "crater" => {
            // Every loaded block within `r` of the player turned to air, one `w.set` each, the
            // way a mod would do it today: changed, already air, not loaded, milliseconds.
            let r: i32 = int(cmd, "r")?;
            let start = std::time::Instant::now();
            let (mut changed, mut air, mut missed) = (0u64, 0u64, 0u64);
            for y in -r..=r {
                for z in -r..=r {
                    for x in -r..=r {
                        if x * x + y * y + z * z > r * r {
                            continue;
                        }
                        let p = [me[0] + x, me[1] + y, me[2] + z];
                        let s = w.get(p);
                        if s == 0 {
                            air += 1;
                        } else if w.set(p, 0) {
                            changed += 1;
                        } else {
                            missed += 1;
                        }
                    }
                }
            }
            Ok(format!("{changed} {air} {missed} {:.1}", start.elapsed().as_secs_f64() * 1000.0))
        }
        "group" => {
            let g = w.inventory(owner(w, cmd)?).group(word(cmd, "name")?);
            Ok(format!("{} {}", g.len(), (0..g.len()).filter(|&i| !g.get(i).is_empty()).count()))
        }
        "pings" => Ok(PINGS.load(Ordering::Relaxed).to_string()),
        "setprop" => Ok((w.set_prop(pos(cmd, "pos")?, word(cmd, "prop")?, word(cmd, "value")?) as u32).to_string()),
        "sblock" => {
            // A block of a moving structure, or set it.
            let (id_, p) = (int::<u64>(cmd, "id")?, pos(cmd, "pos")?);
            match cmd.id("block") {
                Some(b) => Ok((w.set_structure_block(id_, p, b) as u32).to_string()),
                None => w.structure_block(id_, p).map(|s| block_name(s).to_string()).ok_or_else(|| "not a structure".to_string()),
            }
        }
        "light" => Ok(w.light(pos(cmd, "pos")?).to_string()),
        "slot" => Ok(stack_text(w.inventory(owner(w, cmd)?).get(int(cmd, "index")?))),
        "put" => {
            let o = owner(w, cmd)?;
            w.inventory(o).set(int(cmd, "index")?, ItemStack::new(id(cmd, "item")?, int(cmd, "count").unwrap_or(1)));
            Ok(String::new())
        }
        "give" => {
            let count: u32 = int(cmd, "count").unwrap_or(1);
            let left = w.inventory(player).insert(ItemStack::new(id(cmd, "item")?, count));
            Ok((count - left.count).to_string())
        }
        "insert" => {
            let p = pos(cmd, "pos")?;
            let o = match w.block_entity(p) {
                0 => return Err(format!("no block entity at {p:?} ({})", block_name(w.get(p)))),
                id => id,
            };
            let count: u32 = int(cmd, "count").unwrap_or(1);
            let left = w.inventory(o).insert(ItemStack::new(id(cmd, "item")?, count));
            Ok((count - left.count).to_string())
        }
        "inv" => {
            let inv = w.inventory(owner(w, cmd)?);
            let parts: Vec<String> = (0..inv.size()).map(|i| (i, inv.get(i))).filter(|(_, s)| !s.is_empty()).map(|(i, s)| format!("{i}:{}:{}", item_name(s.item), s.count)).collect();
            Ok(parts.join(" "))
        }
        "spawn" => {
            let p = pos(cmd, "pos")?;
            Ok(w.spawn_kind(id(cmd, "kind")?, [p[0] as f64 + 0.5, p[1] as f64, p[2] as f64 + 0.5]).to_string())
        }
        "entity" => {
            let e = w.entity(owner(w, cmd)?).ok_or("no such entity")?;
            Ok(format!("{} {:.2} {:.2} {:.2} {}", name_of(ek::ENTITY, e.kind), e.pos[0], e.pos[1], e.pos[2], e.health))
        }
        "size" => {
            let s = w.entity(owner(w, cmd)?).ok_or("no such entity")?.size;
            Ok(format!("{:.3} {:.3} {:.3}", s[0], s[1], s[2]))
        }
        "near" => {
            let p = pos(cmd, "pos")?;
            let r = cmd.float("r").ok_or("missing r")?;
            let ids = w.near([p[0] as f64 + 0.5, p[1] as f64, p[2] as f64 + 0.5], r, None);
            let parts: Vec<String> = ids.iter().filter(|&&id| id != player).filter_map(|&id| w.entity(id).map(|e| format!("{id}:{}", name_of(ek::ENTITY, e.kind)))).collect();
            Ok(parts.join(" "))
        }
        "animate" => {
            w.play_animation(owner(w, cmd)?, cmd.text("name").unwrap_or(""), false);
            Ok(String::new())
        }
        "survey" => {
            // Biomes of every block within `r` of the player, and where glow crystals are.
            let r: i32 = int(cmd, "r")?;
            let crystal = Block::lookup("testmod:glow_crystal").ok_or("no crystal block")?;
            let cave = Tag::named("cave");
            let mut biomes: std::collections::BTreeMap<&str, u32> = Default::default();
            let (mut in_cave, mut elsewhere) = (0, 0);
            for y in me[1] - r..=me[1] + r {
                for z in me[2] - r..=me[2] + r {
                    for x in me[0] - r..=me[0] + r {
                        let b = w.biome([x, y, z]);
                        *biomes.entry(name_of(ek::BIOME, b)).or_default() += 1;
                        if block_of(w.get([x, y, z])) == block_of(crystal) {
                            if cave.on_biome(b) { in_cave += 1 } else { elsewhere += 1 }
                        }
                    }
                }
            }
            let list: Vec<String> = biomes.iter().map(|(n, k)| format!("{n}={k}")).collect();
            Ok(format!("{} | crystals {in_cave} {elsewhere}", list.join(" ")))
        }
        "drops" => {
            // `n` dropped stacks of a few kinds (blocks and sprites) in a grid north of the
            // player, 1.1 apart so they do not merge.
            let n: u32 = int(cmd, "n")?;
            let kinds: Vec<u32> = ["base:dirt", "base:coal", "base:iron_pickaxe", "base:dandelion", "base:stick", "base:torch"].iter().filter_map(|k| Item::lookup(k)).collect();
            let side = (n as f64).sqrt().ceil() as u32;
            for i in 0..n {
                let (x, z) = ((i % side) as f64 * 1.1 - side as f64 * 0.55, -3.0 - (i / side) as f64 * 1.1);
                w.spawn_item([me[0] as f64 + x, me[1] as f64 + 0.5, me[2] as f64 + z], ItemStack::new(kinds[i as usize % kinds.len()], 1));
            }
            Ok(format!("{n} drops"))
        }
        "find" => {
            // The first block of that kind within `r` of the player.
            let r: i32 = int(cmd, "r")?;
            let want = block_of(id(cmd, "block")?);
            for y in me[1] - r..=me[1] + r {
                for z in me[2] - r..=me[2] + r {
                    for x in me[0] - r..=me[0] + r {
                        if block_of(w.get([x, y, z])) == want {
                            return Ok(format!("{x} {y} {z}"));
                        }
                    }
                }
            }
            Err("not found".into())
        }
        "census" => {
            // How many of each block (`a,b`) lie within `r` of the player.
            let r: i32 = int(cmd, "r")?;
            let names: Vec<&str> = word(cmd, "blocks")?.split(',').collect();
            let ids: Vec<u32> = names.iter().map(|n| Block::lookup(n).map(block_of).ok_or(format!("no block {n}"))).collect::<Result<_, _>>()?;
            let mut counts = vec![0u32; ids.len()];
            for y in me[1] - r..=me[1] + r {
                for z in me[2] - r..=me[2] + r {
                    for x in me[0] - r..=me[0] + r {
                        let b = block_of(w.get([x, y, z]));
                        if let Some(i) = ids.iter().position(|&id| id == b) {
                            counts[i] += 1;
                        }
                    }
                }
            }
            Ok(names.iter().zip(&counts).map(|(n, k)| format!("{n}={k}")).collect::<Vec<_>>().join(" "))
        }
        "onto" => {
            // The player (or the rider) on top of an entity `h` high, in the same tick (moving
            // platforms move on while a teleport by command makes its way).
            let e = w.entity(int(cmd, "id")?).ok_or("no such entity")?;
            let rider = cmd.int("rider").map_or(player, |r| r as u64);
            w.teleport(rider, None, [e.pos[0], e.pos[1] + cmd.float("h").unwrap_or(0.0) + 0.05, e.pos[2]]);
            Ok(String::new())
        }
        "navigate" => {
            let p = pos(cmd, "to")?;
            Ok((w.navigate(owner(w, cmd)?, [p[0] as f64 + 0.5, p[1] as f64, p[2] as f64 + 0.5], 1.0) as u32).to_string())
        }
        "structure" => Ok(w.structure_near(word(cmd, "name")?, me, int(cmd, "r")?).map_or("none".into(), |a| format!("{} {} {}", a[0], a[1], a[2]))),
        // A timer on the player: `after 60`, then `rings` counts the ones that fired.
        "after" => {
            w.after(player, int(cmd, "ticks")?, &RANG, player);
            Ok(String::new())
        }
        "rings" => Ok(RINGS.load(Ordering::Relaxed).to_string()),
        "place" => Ok((w.place(&Template::generated(word(cmd, "template")?.to_string()), pos(cmd, "pos")?) as u32).to_string()),
        "look" => Ok(w.look_at(player, 5.0, false).map_or("none".into(), |l| format!("{} {} {} {} {}", l.frame, l.pos[0], l.pos[1], l.pos[2], block_name(l.state)))),
        "drive" => {
            let dir = [cmd.float("dx").unwrap_or(0.0) as f32, cmd.float("dz").unwrap_or(0.0) as f32];
            DRIVEN.lock().unwrap().push((int(cmd, "id")?, dir, int(cmd, "ticks")?));
            Ok(String::new())
        }
        "push" => {
            let v = ["x", "y", "z"].map(|a| cmd.float(a).unwrap_or(0.0));
            w.push(cmd.int("id").map_or(player, |i| i as u64), v);
            Ok(String::new())
        }
        "heal" => {
            let mut st = w.entity(player).ok_or("no player")?;
            st.health = st.max_health;
            w.set_entity(player, &st);
            Ok(String::new())
        }
        "health" => {
            let id = cmd.int("id").map_or(player, |i| i as u64);
            Ok(w.entity(id).map_or("none".into(), |e| format!("{}", e.health)))
        }
        "time" => {
            w.set_time(int(cmd, "ticks")?);
            Ok(String::new())
        }
        "mode" => {
            let m = ["survival", "creative", "spectator"].iter().position(|m| cmd.has(m)).ok_or("no game mode")?;
            w.set_game_mode(player, m as u32);
            Ok(String::new())
        }
        "menu" => {
            let name = word(cmd, "which")?;
            let m = Menu::lookup(name).ok_or_else(|| format!("no menu {name}"))?;
            let p = pos(cmd, "pos")?;
            if !w.open_menu_kind_at(player, m, p) {
                return Err(format!("no block entity at {p:?} ({})", block_name(w.get(p))));
            }
            Ok(String::new())
        }
        "vmenu" => {
            let name = word(cmd, "which")?;
            let m = Menu::lookup(name).ok_or_else(|| format!("no menu {name}"))?;
            let pos = w.entity(player).map_or([0.0; 3], |e| e.pos);
            let id = w.virtual_inventory(int(cmd, "slots")?, pos);
            let inv = w.inventory(id);
            // Pairs of item and count.
            let words: Vec<&str> = cmd.text("items").unwrap_or("").split_whitespace().collect();
            for (i, pair) in words.chunks(2).enumerate() {
                if let [n, c] = pair {
                    inv.set(i as u32, ItemStack::new(item(n)?, c.parse().map_err(|_| format!("bad count {c}"))?));
                }
            }
            w.open_menu_kind(player, m, id);
            Ok(id.to_string())
        }
        "remove" => {
            w.remove(int(cmd, "id")?);
            Ok(String::new())
        }
        "mark" => {
            *w.component_mut(int(cmd, "id")?, &MARK).ok_or("no mark component")? = int(cmd, "value")?;
            Ok(String::new())
        }
        "secret" => {
            let inv = w.inventory(owner(w, cmd)?);
            let slot: u32 = int(cmd, "index")?;
            let s = inv.get(slot);
            match cmd.int("value") {
                Some(v) => {
                    inv.set(slot, s.with(&SECRET, v as u32));
                    Ok(String::new())
                }
                None => Ok(SECRET.stack_only(&s).map_or("none".into(), |v| v.to_string())),
            }
        }
        "events" => {
            if cmd.has("reset") {
                COUNTS.iter().for_each(|c| c.store(0, Ordering::Relaxed));
            }
            Ok(EVENT_NAMES.iter().zip(&COUNTS).map(|(n, c)| format!("{n}={}", c.load(Ordering::Relaxed))).collect::<Vec<_>>().join(" "))
        }
        "attr" => {
            let (v, text) = w.explain_attribute(owner(w, cmd)?, Attribute::generated(word(cmd, "attribute")?.to_string()));
            Ok(format!("{v} | {text}"))
        }
        "attr_mod" => {
            let o = owner(w, cmd)?;
            let amount = cmd.float("amount").ok_or("missing amount")?;
            w.add_modifier(o, Attribute::generated(word(cmd, "attribute")?.to_string()), word(cmd, "mod")?, int(cmd, "op")?, amount, int(cmd, "ticks")?);
            Ok(String::new())
        }
        "attr_rm" => {
            w.remove_modifier(owner(w, cmd)?, Attribute::generated(word(cmd, "attribute")?.to_string()), word(cmd, "mod")?);
            Ok(String::new())
        }
        "loot" => {
            let table = LootTable::generated(word(cmd, "table")?.to_string());
            let n: u32 = int(cmd, "n")?;
            let mut ctx = LootCtx::default();
            for word in cmd.text("ctx").unwrap_or("").split_whitespace() {
                match word {
                    "fire" => ctx.on_fire = true,
                    "player" => ctx.by_player = true,
                    // Mined by the tool, from the block the table is named after.
                    "mined" => ctx.mined = Block::lookup(table.name()).ok_or("not a block's table")?,
                    tool => ctx.tool = ItemStack::new(item(tool)?, 1),
                }
            }
            let mut totals: std::collections::BTreeMap<String, u32> = Default::default();
            for _ in 0..n {
                for s in w.roll_loot(table, &ctx) {
                    *totals.entry(item_name(s.item).to_string()).or_default() += s.count;
                }
            }
            Ok(totals.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(" "))
        }
        "break" => {
            let tool = cmd.id("tool").map_or(ItemStack::EMPTY, |t| ItemStack::new(t, 1));
            Ok((w.break_block(pos(cmd, "pos")?, tool, player, true) as u32).to_string())
        }
        "damage" => {
            let amount = cmd.float("amount").ok_or("missing amount")?;
            let cause: u32 = int(cmd, "cause").unwrap_or(abi::damage::GENERIC);
            let q = abi::Query { entity: player, cause, amount, ..query() };
            let (v, text) = w.explain(&chain::DAMAGE, amount, &q);
            Ok(format!("{v} | {text}"))
        }
        "snapshot" => {
            let (p, q) = (pos(cmd, "from")?, pos(cmd, "to")?);
            let mut out = Vec::new();
            for y in p[1].min(q[1])..=p[1].max(q[1]) {
                for z in p[2].min(q[2])..=p[2].max(q[2]) {
                    for x in p[0].min(q[0])..=p[0].max(q[0]) {
                        out.push(w.get([x, y, z]).to_string());
                    }
                }
            }
            Ok(out.join(" "))
        }
        other => Err(format!("unknown op {other}")),
    }
}
