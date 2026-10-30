//! Crafting and smelting recipes. Every ingredient is a `#tag`, so another mod's planks, coal or
//! dyes work in all of them without a recipe of its own.

use loom_sdk::*;

pub use base_api::{CRAFTING, SMELTING};
const C: &RecipeKind = &CRAFTING;
const S: &RecipeKind = &SMELTING;

pub fn register(r: &mut Registry) {
    // A crafting table's grid is 3 x 3 in the recipe viewer, whatever a recipe fills of it.
    r.recipe_kind(C).inputs(3, 3).outputs(1, 1);
    r.recipe_kind(S).inputs(1, 1).outputs(1, 1);
    r.shapeless(C, "#planks/{m}", 4, &["#logs/{m}"], 0);
    r.shaped(C, "stick", 4, &["#", "#"], &[('#', "#planks")]);
    r.shaped(C, "crafting_table", 1, &["##", "##"], &[('#', "#planks")]);
    r.shaped(C, "furnace", 1, &["###", "# #", "###"], &[('#', "#cobblestone")]);
    r.shaped(C, "chest", 1, &["###", "# #", "###"], &[('#', "#planks")]);
    r.shaped(C, "torch", 4, &["c", "s"], &[('c', "#gems/coal"), ('s', "#stick")]);
    r.shaped(C, "ladder", 3, &["s s", "sss", "s s"], &[('s', "#stick")]);
    r.shaped(C, "bread", 1, &["www"], &[('w', "#crops/wheat")]);
    r.shaped(C, "stone_bricks", 4, &["##", "##"], &[('#', "#smooth_stone")]);
    r.shaped(C, "wool", 1, &["ss", "ss"], &[('s', "#string")]);
    r.shaped(C, "bucket", 1, &["i i", " i "], &[('i', "#ingots/iron")]);
    r.shaped(C, "glowstone", 1, &["ttt", "tgt", "ttt"], &[('t', "#torches"), ('g', "#glass")]);
    r.shapeless(C, "#lamps/{m}", 1, &["#glowstone", "#dyes/{m}"], 0);
    r.shapeless(C, "#stained_glass/{m}", 1, &["#glass", "#dyes/{m}"], 0);

    // Every mod's `ore_blocks/<m>` packs its `ingots/<m>` or `gems/<m>`, and its ores smelt into them.
    for kind in ["#ingots/{m}", "#gems/{m}"] {
        r.shaped(C, "#ore_blocks/{m}", 1, &["###", "###", "###"], &[('#', kind)]);
        r.shapeless(C, kind, 9, &["#ore_blocks/{m}"], 0);
        r.shapeless(S, kind, 1, &["#ores/{m}"], 200);
    }
    r.shapeless(S, "#cooked_meat/{m}", 1, &["#raw_meat/{m}"], 200);
    r.shapeless(S, "#ingots/{m}", 1, &["#raw_ores/{m}"], 200);

    // A `pickaxes/<m>` or `armor_chest/<m>` item is made from `materials/<m>`.
    for (tool, pattern) in [
        ("pickaxes", &["XXX", " # ", " # "][..]),
        ("axes", &["XX", "X#", " #"]),
        ("shovels", &["X", "#", "#"]),
        ("hoes", &["XX", " #", " #"]),
        ("swords", &["X", "X", "#"]),
    ] {
        r.shaped(C, &format!("#{tool}/{{m}}"), 1, pattern, &[('X', "#materials/{m}"), ('#', "#stick")]);
    }
    for (slot, pattern) in crate::items::ARMOR_SLOTS.iter().zip([&["XXX", "X X"][..], &["X X", "XXX", "XXX"], &["XXX", "X X", "X X"], &["X X", "X X"]]) {
        r.shaped(C, &format!("#{slot}/{{m}}"), 1, pattern, &[('X', "#materials/{m}")]);
    }

    for (input, output, ticks) in [
        ("#sand", "glass", 200),
        ("#cobblestone", "stone", 200),
        ("#logs", "coal", 200),
        ("#clay", "bricks", 200),
    ] {
        r.shapeless(S, output, 1, &[input], ticks);
    }
}
