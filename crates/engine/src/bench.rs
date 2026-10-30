//! Frame, GPU and server statistics over a time window (`client_op::BENCH`), logged as one line.

use std::time::{Duration, Instant};

/// CPU parts of a frame, in the order `Running::prof` records them.
pub const CPU_PARTS: [&str; 5] = ["update", "ui", "acquire", "record", "submit"];
/// GPU parts, split by the marks the renderer places.
pub const GPU_PARTS: [&str; 5] = ["upload", "world", "entities", "ui", "rest"];

pub struct Bench {
    label: String,
    until: Instant,
    frames: Vec<f32>,
    cpu: [f64; 5],
    gpu: [f64; 5],
    gpu_frames: u32,
    server: Vec<f32>,
    phases: [f64; 6],
    start: Instant,
    mods: crate::diag::ModWindow,
    /// Per frame sums: GPU draws (indirect records counted), draw commands, pipeline binds.
    calls: [u64; 3],
}

impl Bench {
    pub fn new(label: &str, secs: f32) -> Bench {
        let mut mods = crate::diag::ModWindow::default();
        mods.take();
        Bench {
            mods,
            start: Instant::now(),
            label: label.to_string(),
            until: Instant::now() + Duration::from_secs_f32(secs),
            frames: Vec::new(),
            cpu: [0.0; 5],
            gpu: [0.0; 5],
            gpu_frames: 0,
            server: Vec::new(),
            phases: [0.0; 6],
            calls: [0; 3],
        }
    }

    pub fn sample(&mut self, frame_ms: f32, cpu: &[f32; 5], gpu: &[f32], server_ms: f32, phases: [f32; 6], list: &loom_render::CommandList) {
        use loom_render::Cmd;
        for c in &list.cmds {
            let draws = match *c {
                Cmd::Draw { .. } | Cmd::DrawIndirectCount { .. } => 1,
                Cmd::DrawIndirect { count, .. } | Cmd::DrawIndexedIndirect { count, .. } => count as u64,
                Cmd::SetPipeline(_) => {
                    self.calls[2] += 1;
                    continue;
                }
                _ => continue,
            };
            self.calls[0] += draws;
            self.calls[1] += 1;
        }
        for (s, v) in self.phases.iter_mut().zip(phases) {
            *s += v as f64;
        }
        self.frames.push(frame_ms);
        for (s, v) in self.cpu.iter_mut().zip(cpu) {
            *s += *v as f64;
        }
        if gpu.len() >= 5 {
            for (s, v) in self.gpu.iter_mut().zip(gpu) {
                *s += *v as f64;
            }
            self.gpu_frames += 1;
        }
        self.server.push(server_ms);
    }

    pub fn done(&self) -> bool {
        Instant::now() >= self.until
    }

    /// fps, average and p99 frame ms, average GPU and server ms.
    pub fn summary(&self) -> [f32; 5] {
        let n = self.frames.len().max(1) as f64;
        let mut sorted = self.frames.clone();
        sorted.sort_by(f32::total_cmp);
        let p99 = sorted.get(((sorted.len() as f64 - 1.0) * 0.99).round() as usize).copied().unwrap_or(0.0);
        let avg = self.frames.iter().map(|&f| f as f64).sum::<f64>() / n;
        let gpu = self.gpu.iter().sum::<f64>() / self.gpu_frames.max(1) as f64;
        let srv = self.server.iter().map(|&s| s as f64).sum::<f64>() / self.server.len().max(1) as f64;
        [(1000.0 / avg.max(1e-3)) as f32, avg as f32, p99, gpu as f32, srv as f32]
    }

    pub fn report(&mut self, extra: &str) -> String {
        let n = self.frames.len().max(1) as f64;
        let mut sorted = self.frames.clone();
        sorted.sort_by(f32::total_cmp);
        let pct = |p: f64| sorted.get(((sorted.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0);
        let avg = self.frames.iter().map(|&f| f as f64).sum::<f64>() / n;
        let cpu: Vec<String> = CPU_PARTS.iter().zip(self.cpu).map(|(k, v)| format!("{k} {:.2}", v / n)).collect();
        let g = self.gpu_frames.max(1) as f64;
        let gpu_total: f64 = self.gpu.iter().sum::<f64>() / g;
        let gpu: Vec<String> = GPU_PARTS.iter().zip(self.gpu).map(|(k, v)| format!("{k} {:.2}", v / g)).collect();
        let srv_avg = self.server.iter().map(|&s| s as f64).sum::<f64>() / self.server.len().max(1) as f64;
        let srv_max = self.server.iter().copied().fold(0.0f32, f32::max);
        let ticks = self.start.elapsed().as_secs_f64() * crate::world::TPS as f64;
        let mods: Vec<String> = self.mods.take().0.into_iter().map(|(id, ns, calls)| format!("{id} {:.2} ms/{:.0} calls", ns as f64 / 1e6 / ticks, calls as f64 / ticks)).collect();
        let phases: Vec<String> =
            crate::server::PHASES.iter().zip(self.phases).map(|(k, v)| format!("{k} {:.2}", v / self.server.len().max(1) as f64)).collect();
        format!(
            "BENCH {} | {} frames, {:.0} fps, frame avg {avg:.2} p50 {:.2} p99 {:.2} max {:.2} ms | cpu ms: {} | gpu {gpu_total:.2} ms: {} | per frame: {:.0} draws in {:.0} commands, {:.0} pipeline binds | server avg {srv_avg:.2} max {srv_max:.2} ms ({}) | mods per tick: {} | {extra}",
            self.label,
            self.frames.len(),
            1000.0 / avg.max(1e-3),
            pct(0.5),
            pct(0.99),
            pct(1.0),
            cpu.join(", "),
            gpu.join(", "),
            self.calls[0] as f64 / n,
            self.calls[1] as f64 / n,
            self.calls[2] as f64 / n,
            phases.join(", "),
            mods.join(", "),
        )
    }
}
