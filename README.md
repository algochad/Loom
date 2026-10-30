<h1 align="center">Loom</h1>

<p align="center">
  <img src="https://img.shields.io/badge/Rust-1.90%2B-B7410E?logo=rust&logoColor=white" alt="Rust 1.90+">
  <img src="https://img.shields.io/badge/graphics-Vulkan-A41E22?logo=vulkan&logoColor=white" alt="Vulkan">
  <img src="https://img.shields.io/badge/platform-Windows-2A6CB0" alt="Windows">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MPL--2.0-3B2F7E" alt="MPL-2.0"></a>
</p>

<p align="center">
  <a href="https://github.com/VortexSo4/Loom/releases"><b>Download</b></a> ·
  <a href="docs/modding.md"><b>Modding</b></a> ·
  <a href="docs"><b>Documentation</b></a> ·
  <a href="#project-status"><b>Project status</b></a> ·
  <a href="#contributing"><b>Contributing</b></a>
</p>

Loom is a voxel sandbox engine written in Rust. The game itself is a mod, `mods/base`, built only on the public SDK: it has no private hooks into the engine, so anything it does, another mod can do or change. Loom is not a Minecraft clone and does not read Minecraft worlds; it plays like one and borrows its numbers where that makes sense.

> Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft. Loom contains no Minecraft code or assets.

## Features

- The world is made of cubic 16x16x16 chunks, so there is no limit on height or depth, and the terrain is generated on the GPU, which allows distant terrain to be drawn far past the view distance. World generation is deterministic and includes caves, several dimensions, and biomes whose colors blend into each other on the borders.

- Instead of random ticks, the world is split into regions that are ticked in parallel, and plants, furnaces and other timed blocks catch up on the time nobody was there for, so crops keep growing and furnaces keep smelting even while their chunks are not loaded.

- Blocks can emit colored RGB light and use connected or randomized texture variants, textures can be animated or glowing, and models are made in Blockbench with keyframe animations. The look of the game can be changed with resource packs and shader packs.

- As for gameplay, there are survival and creative modes, crafting and smelting, mobs with pathfinding, liquids that can be picked up with a bucket regardless of which liquid it is, machines powered by energy, and moving structures that you can stand and build on.

- Multiplayer works peer to peer: Open to Friends in the pause menu gives you a ticket, and a friend can join with it without any server or port forwarding.

- Mods are native libraries behind a C ABI, packed as zip files and loaded in parallel. Content is registered through builders, behavior hooks into the game through events, interceptors and value chains, and recipes, loot and world generation are driven by tags, which is described in more detail in [Modding](#modding).

- For development, textures, translations, shaders and UI can be reloaded in game, and there are debug overlays, a recipe viewer, and crash reports that name the mod which was running at the moment of the crash.

## Download

Get the latest build from [Releases](https://github.com/VortexSo4/Loom/releases), unpack it and run `loom`. You need a GPU with Vulkan. Only Windows is tested so far.

Loom ships with its own textures. If Minecraft is installed through the official launcher or Prism, Loom builds a resource pack from that copy when it starts. The pack stays on your computer and can be turned off in Settings > Resource Packs.

## Build from source

Requires Rust 1.90 or newer.

```sh
git clone https://github.com/VortexSo4/Loom.git
cd Loom
cargo run --release
```

Run it from the repository root. Release builds pack each mod in `mods/` into `run/mods/` on start; settings, saves and logs go to `run/`. `cargo run --release -- --dist <dir>` makes a standalone build to share.

## Modding

Copy [`examples/examplemod`](examples/examplemod): every kind of registration is there as a commented example. Then read the [modding cheat sheet](docs/modding.md); the guides for worldgen, machines, models, UI, tooltips, textures, CTM, resource packs and shaders are in [`docs/`](docs). [`mods/base`](mods/base) is the game itself and [`mods/testmod`](mods/testmod) shows a mod extending and replacing it without engine changes.

## Project status

Early. The mod interface and the save format can still change between versions. A mod built against another engine version refuses to load and asks to be rebuilt rather than crashing.

## Contributing

Issues and pull requests are welcome, for the engine, mods, textures or docs.

## License - [MPL-2.0](LICENSE)

Made by [VortexSo4](https://github.com/VortexSo4).
