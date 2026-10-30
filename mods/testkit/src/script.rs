//! A line script language on the client mod API:
//!
//! ```text
//! wait 2.5            # seconds
//! loaded              # until the world around the player is meshed
//! idle                # until nothing is left to generate, light, send or mesh
//! press core:inventory
//! hold core:forward 1.5
//! click 0             # mouse button
//! cursor 0.5 0.4      # mouse at fractions of the window
//! scroll -3
//! type /give dia      # text into the focused field
//! key Tab             # a key by its settings name
//! look 90 -20         # yaw, pitch in degrees
//! chat /give stone 10
//! say_config testmod:x  # a config's value as this client has it, into the chat
//! shot name.png       # into run/screenshots (or --shot-dir)
//! bench 5 label       # BENCH line over 5 seconds
//! host ticket.txt     # host, write the join ticket there
//! join ticket.txt     # join with the ticket from that file (waits for it)
//! origin              # later via points are relative to where the player stands now
//! where label        # logs the player's position
//! rec start|stop     # logs a marker; tools/rec.ps1 presses the OBS hotkeys on it
//! via x y z yaw pitch # a camera waypoint (eye position), or: via x y z @ tx ty tz (look at a point)
//! travel 8            # fly through the waypoints in 8 s: spline, eased, a little hand shake
//! glide yaw pitch 3   # turn smoothly in place over 3 s
//! quit
//! ```

use std::time::Duration;

use loom_sdk::client::client_flags as cf;
use loom_sdk::launch_option;

use crate::runner::{R, T};

pub async fn run(t: T, src: String) -> R {
    let started = std::time::Instant::now();
    let mut via: Vec<[f64; 5]> = Vec::new();
    let mut origin = [0.0f64; 3];
    for line in src.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let nums: Vec<f32> = rest.split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let num = |i: usize, d: f32| nums.get(i).copied().unwrap_or(d);
        match cmd {
            "wait" => t.secs(num(0, 1.0)).await?,
            "loaded" => {
                t.loaded().await?;
                loom_sdk::info!("script: loaded at {:.2} s", started.elapsed().as_secs_f32());
            }
            "idle" => {
                t.idle().await?;
                let s = t.state();
                loom_sdk::info!("script: idle at {:.2} s", started.elapsed().as_secs_f32());
                loom_sdk::info!(
                    "load: {} cubes on client ({} of them made on the GPU, {} differing from the CPU), server generated {} ({:.0} us each on a worker), inserted {} ({:.0} us each on the server thread)",
                    s.cubes,
                    s.gpu_cubes,
                    s.gpu_mismatches,
                    s.generated,
                    s.gen_us,
                    s.inserted,
                    s.insert_us
                );
            }
            "press" => t.hold(rest, 0.0),
            "hold" => {
                let (a, secs) = rest.rsplit_once(' ').unwrap_or((rest, "1"));
                t.hold(a.trim(), secs.parse().unwrap_or(1.0));
            }
            "click" => t.c().click(num(0, 0.0) as u32, 0.0),
            "cursor" => t.c().cursor(num(0, 0.5), num(1, 0.5)),
            "scroll" => t.c().scroll(num(0, -1.0)),
            "type" => t.c().type_text(rest),
            "key" => {
                if !t.c().key(rest, 0.0) {
                    t.fail(format!("unknown key {rest}"));
                }
            }
            "look" => t.c().look(num(0, 0.0), num(1, 0.0)),
            "chat" => t.c().chat(rest),
            "say_config" => {
                let cfg = loom_sdk::Config::<f64>::named(Box::leak(rest.to_string().into_boxed_str()));
                t.c().chat(&format!("config {rest} = {}", cfg.get()));
            }
            "shot" => {
                let path = match launch_option("shot-dir") {
                    Some(d) => format!("{d}/{rest}"),
                    None => rest.to_string(),
                };
                t.c().screenshot(&path);
            }
            "bench" => {
                let (secs, label) = rest.split_once(' ').unwrap_or((rest, ""));
                t.bench(secs.parse().unwrap_or(5.0), label.trim()).await?;
            }
            "host" => {
                t.c().host_world();
                t.until("hosting", Duration::from_secs(60), |t| t.flag(cf::HOSTING)).await?;
                let _ = std::fs::write(rest, t.c().ticket());
            }
            "join" => {
                let mut ticket = String::new();
                t.until("ticket file", Duration::from_secs(120), |_| {
                    ticket = std::fs::read_to_string(rest).unwrap_or_default().trim().to_string();
                    !ticket.is_empty()
                })
                .await?;
                t.c().join(&ticket);
            }
            "origin" => origin = t.state().pos,
            "where" => {
                let s = t.state();
                loom_sdk::info!("script: where {:.3} {:.3} {:.3} {rest}", s.pos[0], s.pos[1], s.pos[2]);
            }
            "rec" => loom_sdk::info!("REC {rest}"),
            "via" => match waypoint(origin, rest) {
                Some(mut w) => {
                    for k in 0..3 {
                        w[k] += origin[k];
                    }
                    via.push(w)
                }
                None => t.fail(format!("bad waypoint '{line}'")),
            },
            "travel" => {
                let eye = t.c().entity(0).map_or(1.62, |e| e.size[2] as f64);
                let s = t.state();
                let mut pts = vec![[s.pos[0], s.pos[1] + eye, s.pos[2], (s.yaw as f64).to_degrees(), (s.pitch as f64).to_degrees()]];
                pts.append(&mut via);
                fly(&t, pts, num(0, 5.0) as f64, eye).await?;
            }
            "glide" => {
                let s = t.state();
                let from = [s.pos[0], s.pos[1] + 0.0, s.pos[2]];
                let eye = t.c().entity(0).map_or(1.62, |e| e.size[2] as f64);
                let a = [from[0], from[1] + eye, from[2], (s.yaw as f64).to_degrees(), (s.pitch as f64).to_degrees()];
                let b = [a[0], a[1], a[2], num(0, 0.0) as f64, num(1, 0.0) as f64];
                fly(&t, vec![a, b], num(2, 2.0) as f64, eye).await?;
            }
            "quit" => return Ok(()),
            _ => t.fail(format!("unknown script line '{line}'")),
        }
        // One step per frame.
        t.frames(1).await?;
    }
    Ok(())
}

fn waypoint(origin: [f64; 3], rest: &str) -> Option<[f64; 5]> {
    let (head, at) = match rest.split_once('@') {
        Some((h, a)) => (h, Some(a)),
        None => (rest, None),
    };
    let n: Vec<f64> = head.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    match at {
        Some(a) => {
            let m: Vec<f64> = a.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if n.len() < 3 || m.len() < 3 {
                return None;
            }
            let d = [m[0] - n[0], m[1] - n[1], m[2] - n[2]];
            let _ = origin;
            let yaw = (-d[0]).atan2(-d[2]).to_degrees();
            let pitch = d[1].atan2((d[0] * d[0] + d[2] * d[2]).sqrt()).to_degrees();
            Some([n[0], n[1], n[2], yaw, pitch])
        }
        None if n.len() >= 5 => Some([n[0], n[1], n[2], n[3], n[4]]),
        None => None,
    }
}

fn catmull(p: [&[f64; 5]; 4], u: f64) -> [f64; 5] {
    let mut out = [0.0; 5];
    for i in 0..5 {
        let (a, b, c, d) = (p[0][i], p[1][i], p[2][i], p[3][i]);
        out[i] = 0.5 * (2.0 * b + (-a + c) * u + (2.0 * a - 5.0 * b + 4.0 * c - d) * u * u + (-a + 3.0 * b - 3.0 * c + d) * u * u * u);
    }
    out
}

/// Flies the camera through `pts` (eye position, yaw and pitch in degrees) in `secs`: constant
/// speed along a spline, eased at both ends, with a slow uneven drift on the aim like a hand.
async fn fly(t: &T, mut pts: Vec<[f64; 5]>, secs: f64, eye: f64) -> R {
    // Yaw takes the short way round between neighbours.
    for i in 1..pts.len() {
        let mut d = (pts[i][3] - pts[i - 1][3]) % 360.0;
        if d > 180.0 {
            d -= 360.0;
        } else if d < -180.0 {
            d += 360.0;
        }
        pts[i][3] = pts[i - 1][3] + d;
    }
    let n = pts.len();
    let at = |i: isize| &pts[i.clamp(0, n as isize - 1) as usize];
    let mut dense: Vec<([f64; 5], f64)> = Vec::new();
    let mut len = 0.0;
    for seg in 0..n.saturating_sub(1) {
        for k in 0..64 {
            let p = catmull([at(seg as isize - 1), at(seg as isize), at(seg as isize + 1), at(seg as isize + 2)], k as f64 / 64.0);
            if let Some((q, _)) = dense.last() {
                len += ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt();
            }
            dense.push((p, len));
        }
    }
    dense.push((pts[n - 1], len + 1e-9));
    // Pure turns have no length: spread them over the samples instead.
    let turn = len < 1e-3;
    let last = dense.len() - 1;
    let mut elapsed = 0.0;
    while elapsed < secs {
        t.frames(1).await?;
        elapsed += (t.state().dt as f64).min(0.1);
        let u = (elapsed / secs).min(1.0);
        let e = u * u * u * (u * (u * 6.0 - 15.0) + 10.0);
        let e = (e + 0.012 * (std::f64::consts::PI * u).sin() * (u * secs * 1.7).sin()).clamp(0.0, 1.0);
        let i = if turn {
            ((e * last as f64) as usize).min(last)
        } else {
            let want = e * len;
            dense.partition_point(|(_, l)| *l < want).min(last)
        };
        let (a, la) = dense[i.saturating_sub(1)];
        let (b, lb) = dense[i];
        let f = if turn { 0.0 } else if lb > la { ((e * len - la) / (lb - la)).clamp(0.0, 1.0) } else { 1.0 };
        let mut p = [0.0; 5];
        for k in 0..5 {
            p[k] = a[k] + (b[k] - a[k]) * f;
        }
        let tm = elapsed;
        let shake = (u * std::f64::consts::PI).sin().min(1.0);
        p[3] += shake * (0.45 * (tm * 0.9).sin() + 0.25 * (tm * 2.3 + 1.0).sin());
        p[4] += shake * (0.35 * (tm * 0.7 + 2.0).sin() + 0.2 * (tm * 1.9).sin());
        let c = t.c();
        c.control(&loom_sdk::abi::PlayerControl {
            mask: loom_sdk::abi::control_mask::POS | loom_sdk::abi::control_mask::VEL | loom_sdk::abi::control_mask::LOOK,
            pos: [p[0], p[1] - eye, p[2]],
            vel: [0.0; 3],
            yaw: p[3].to_radians() as f32,
            pitch: p[4].to_radians() as f32,
            ..loom_sdk::abi::PlayerControl::DEFAULT
        });
    }
    Ok(())
}
