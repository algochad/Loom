//! Liquids of mods with the base game's buckets and soil tags; flow itself is checked in the
//! scene.

use std::time::Duration;

use loom_sdk::*;

use super::add;
use crate::runner::{R, T};

/// The test mod's oil uses the base game's bucket without either knowing about buckets.
pub async fn mod_liquid_bucket(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let pit = add(f, [1, -1, 0]);
    t.set(pit, "testmod:oil").await?;
    t.op("put me 0 bucket 1").await?;
    t.c().select(0);
    t.look_at(pit);
    t.frames(3).await?;
    t.press("core:use");
    let oil_bucket = Item::lookup("testmod:oil_bucket").unwrap_or(u32::MAX);
    t.until("oil scooped", Duration::from_secs(5), |t| t.c().inventory_slot(0).item == oil_bucket).await?;
    t.check_eq(t.block(pit).await?, "core:air".into(), "oil taken from the pit");
    // Right clicks repeat at most every quarter second.
    t.secs(0.3).await?;
    t.look_at(add(pit, [0, -1, 0]));
    t.frames(3).await?;
    t.press("core:use");
    t.until("oil poured", Duration::from_secs(5), |t| t.c().inventory_slot(0).item == Item::lookup("base:bucket").unwrap_or(u32::MAX)).await?;
    t.check_eq(t.block(pit).await?, "testmod:oil[level=0]".into(), "oil back in the pit");
    Ok(())
}

/// A mod's block tagged `sand` holds cactus and sugar cane; dirt does not hold cactus.
pub async fn mod_sand_soil(t: T) -> R {
    t.flat().await?;
    let f = t.feet();
    let sand = add(f, [2, -1, 0]);
    t.set(sand, "testmod:red_sand").await?;
    t.op("put me 0 cactus 4").await?;
    t.c().select(0);
    t.look_at(sand);
    t.frames(3).await?;
    t.press("core:use");
    t.until("cactus placed", Duration::from_secs(5), |t| t.c().inventory_slot(0).count == 3).await?;
    t.check_eq(t.block(add(sand, [0, 1, 0])).await?, "base:cactus".into(), "cactus on red sand");
    let dirt = add(f, [-2, -1, 0]);
    t.set(dirt, "dirt").await?;
    t.secs(0.3).await?;
    t.look_at(dirt);
    t.frames(3).await?;
    t.press("core:use");
    t.secs(0.5).await?;
    t.check_eq(t.block(add(dirt, [0, 1, 0])).await?, "core:air".into(), "cactus refused on dirt");
    t.check_eq(t.c().inventory_slot(0).count, 3, "cactus kept after a refused placement");
    Ok(())
}
