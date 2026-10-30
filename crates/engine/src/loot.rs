//! Loot tables: pools of weighted entries with counts and conditions, for blocks (the table named
//! like the block, by default its own item when harvested), entities (named like the kind) and anything a mod
//! names (chests). Other mods change tables with modifiers instead of replacing them.

use rustc_hash::FxHashMap;
use loom_abi::{self as abi, ItemStack, ModId, loot_cond as lc};

use crate::components::{self as comps, Components};

#[derive(Clone, Debug)]
pub struct Cond {
    pub kind: u32,
    pub value: f32,
    /// Component id for tag conditions (`u32::MAX` = a tag nothing has).
    pub tag: u32,
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// 0 = nothing.
    pub item: u32,
    pub weight: u32,
    pub min: u32,
    pub max: u32,
    pub conds: Vec<Cond>,
    /// Recipe type that cooks it when the victim burns.
    pub smelt: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Pool {
    pub rolls: (u32, u32),
    pub conds: Vec<Cond>,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, Default)]
pub struct Table {
    pub pools: Vec<Pool>,
}

/// What a roll knows about the situation.
#[derive(Clone, Copy, Default)]
pub struct Ctx {
    pub tool: ItemStack,
    pub by_player: bool,
    pub on_fire: bool,
    /// The block state mined with `tool` (0 = none, or broken by the world).
    pub mined: u32,
}

// ---------------------------------------------------------------- registration

#[derive(Clone)]
pub struct RawCond {
    pub kind: u32,
    pub value: f32,
    pub text: String,
}

#[derive(Clone)]
pub struct RawEntry {
    pub item: String,
    pub weight: u32,
    pub min: u32,
    pub max: u32,
    pub conds: Vec<RawCond>,
    pub smelt: String,
}

#[derive(Clone)]
pub struct RawPool {
    pub rolls: (u32, u32),
    pub conds: Vec<RawCond>,
    pub entries: Vec<RawEntry>,
}

pub struct RawLoot {
    pub owner: ModId,
    pub name: String,
    pub pools: Vec<RawPool>,
}

pub struct RawLootMod {
    pub owner: ModId,
    pub table: String,
    pub op: u32,
    pub pool: RawPool,
    pub from: String,
    pub to: String,
}

unsafe fn conds(s: abi::Slice<abi::LootCond>) -> Vec<RawCond> {
    let mut out = Vec::new();
    unsafe { s.as_slice().iter().copied().for_each(|c| out.push(RawCond { kind: c.kind, value: c.value, text: c.text.as_str().to_string() })) };
    out
}

/// # Safety
/// The pool's pointers must be valid for this call.
pub unsafe fn copy_pool(p: &abi::LootPool) -> RawPool {
    unsafe {
        let mut entries = Vec::new();
        p.entries.as_slice().iter().copied().for_each(|e| {
            entries.push(RawEntry { item: e.item.as_str().to_string(), weight: e.weight, min: e.min, max: e.max.max(e.min), conds: conds(e.conditions), smelt: e.smelt.as_str().to_string() })
        });
        RawPool { rolls: (p.rolls_min, p.rolls_max.max(p.rolls_min)), conds: conds(p.conditions), entries }
    }
}

pub struct Resolve<'a> {
    pub items: &'a FxHashMap<String, u32>,
    pub tag_items: &'a FxHashMap<u32, Vec<u32>>,
    pub components: &'a Components,
}

impl Resolve<'_> {
    fn item(&self, s: &str, errors: &mut Vec<String>, table: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        crate::refs::Ref::parse(s).and_then(|r| crate::refs::item(&r, self.items, self.components, self.tag_items)).unwrap_or_else(|e| {
            errors.push(format!("loot table {table}: {e}"));
            0
        })
    }

    /// A condition naming a tag nobody declares could never pass: an error, not a silent no.
    fn conds(&self, c: &[RawCond], errors: &mut Vec<String>, table: &str) -> Vec<Cond> {
        c.iter()
            .map(|c| {
                let tag = if c.text.is_empty() {
                    u32::MAX
                } else {
                    crate::refs::tag(self.components, &c.text).unwrap_or_else(|e| {
                        errors.push(format!("loot table {table}: condition: {e}"));
                        u32::MAX
                    })
                };
                Cond { kind: c.kind, value: c.value, tag }
            })
            .collect()
    }

    pub fn pool(&self, p: &RawPool, errors: &mut Vec<String>, table: &str) -> Pool {
        let conds = self.conds(&p.conds, errors, table);
        let entries = p
            .entries
            .iter()
            .map(|e| Entry {
                item: self.item(&e.item, errors, table),
                weight: e.weight,
                min: e.min,
                max: e.max,
                conds: self.conds(&e.conds, errors, table),
                smelt: (!e.smelt.is_empty()).then(|| e.smelt.clone()),
            })
            .collect();
        Pool { rolls: p.rolls, conds, entries }
    }
}

/// Tables by name: mods' own, then modifiers in mod load order (`order` of the owner).
pub fn freeze(raw: Vec<RawLoot>, mut mods_list: Vec<RawLootMod>, order: &dyn Fn(ModId) -> u32, r: &Resolve, defaults: FxHashMap<String, Table>, errors: &mut Vec<String>) -> FxHashMap<String, Table> {
    let mut tables = defaults;
    let mut raw = raw;
    raw.sort_by_key(|t| order(t.owner));
    for t in raw {
        let pools = t.pools.iter().map(|p| r.pool(p, errors, &t.name)).collect();
        tables.insert(t.name, Table { pools });
    }
    mods_list.sort_by_key(|m| order(m.owner));
    for m in mods_list {
        let names: Vec<String> = tables.keys().filter(|n| comps::matches(&m.table, n)).cloned().collect();
        if names.is_empty() && !m.table.contains('*') {
            errors.push(format!("loot modifier for unknown table {}", m.table));
        }
        let from = (!m.from.is_empty()).then(|| r.item(&m.from, errors, &m.table));
        let to = r.item(&m.to, errors, &m.table);
        let pool = (m.op == abi::loot_op::ADD_POOL).then(|| r.pool(&m.pool, errors, &m.table));
        for n in names {
            let t = tables.get_mut(&n).unwrap();
            match m.op {
                abi::loot_op::ADD_POOL => t.pools.extend(pool.clone()),
                abi::loot_op::REPLACE_ITEM => t.pools.iter_mut().flat_map(|p| p.entries.iter_mut()).filter(|e| Some(e.item) == from).for_each(|e| e.item = to),
                abi::loot_op::REMOVE_ITEM => t.pools.iter_mut().for_each(|p| p.entries.retain(|e| Some(e.item) != from)),
                abi::loot_op::CLEAR => t.pools.clear(),
                op => errors.push(format!("loot modifier for {n}: unknown operation {op}")),
            }
        }
    }
    tables
}

/// One item when the block is harvested: a block's table when it names none.
pub fn harvested(item: u32, count: u32) -> Table {
    let conds = vec![Cond { kind: lc::HARVESTED, value: 1.0, tag: u32::MAX }];
    Table { pools: vec![Pool { rolls: (1, 1), conds, entries: vec![Entry { item, weight: 1, min: count, max: count, conds: Vec::new(), smelt: None }] }] }
}

// ---------------------------------------------------------------- rolling

fn passes(c: &crate::content::Content, conds: &[Cond], ctx: &Ctx, rng: &mut dyn FnMut() -> f32) -> bool {
    conds.iter().all(|cd| match cd.kind {
        lc::CHANCE => rng() < cd.value,
        lc::TOOL_TAG => !ctx.tool.is_empty() && c.components.has(comps::ITEM, ctx.tool.item, cd.tag),
        lc::NOT_TOOL_TAG => ctx.tool.is_empty() || !c.components.has(comps::ITEM, ctx.tool.item, cd.tag),
        lc::TOOL_LEVEL => !ctx.tool.is_empty() && c.item_value::<u32>(ctx.tool.item, c.ids.tool_level).unwrap_or(0) as f32 >= cd.value,
        lc::KILLED_BY_PLAYER => ctx.by_player,
        lc::ON_FIRE => ctx.on_fire,
        lc::HARVESTED => ctx.mined == 0 || c.harvests(ctx.mined, ctx.tool.item),
        _ => false,
    })
}

/// Rolls a table; `rng` gives uniform numbers in 0..1 from the region's stream, so results are
/// deterministic.
pub fn roll(c: &crate::content::Content, t: &Table, ctx: &Ctx, rng: &mut dyn FnMut() -> f32) -> Vec<ItemStack> {
    let mut out: Vec<ItemStack> = Vec::new();
    for p in &t.pools {
        if !passes(c, &p.conds, ctx, rng) {
            continue;
        }
        let rolls = p.rolls.0 + ((rng() * (p.rolls.1 - p.rolls.0 + 1) as f32) as u32).min(p.rolls.1 - p.rolls.0);
        for _ in 0..rolls {
            let ok: Vec<&Entry> = p.entries.iter().filter(|e| passes(c, &e.conds, ctx, rng)).collect();
            let total: u32 = ok.iter().map(|e| e.weight).sum();
            if total == 0 {
                continue;
            }
            let mut pick = (rng() * total as f32) as u32;
            let Some(e) = ok.iter().find(|e| {
                if pick < e.weight {
                    true
                } else {
                    pick -= e.weight;
                    false
                }
            }) else {
                continue;
            };
            let n = e.min + ((rng() * (e.max - e.min + 1) as f32) as u32).min(e.max - e.min);
            if e.item == 0 || n == 0 {
                continue;
            }
            let mut item = e.item;
            if let (true, Some(kind)) = (ctx.on_fire, &e.smelt) {
                if let Some(r) = c.game.process(kind, item, &c.components) {
                    let made = r.result();
                    if !made.is_empty() {
                        item = made.item;
                    }
                }
            }
            match out.iter_mut().find(|s| s.item == item && s.count + n <= c.max_stack(item)) {
                Some(s) => s.count += n,
                None => out.push(ItemStack::new(item, n)),
            }
        }
    }
    out
}
