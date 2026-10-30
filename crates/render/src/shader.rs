use naga::valid::{Capabilities, ValidationFlags, Validator};

/// Parsed and validated WGSL. Backends translate it to their own format.
pub struct Shader {
    pub module: naga::Module,
    pub info: naga::valid::ModuleInfo,
    /// Content hash of the source (a key for caching compiled modules).
    pub hash: u64,
    pub push_constant_size: u32,
    /// Group 1 bindings: (binding, kind).
    pub buffers: Vec<(u32, BufferKind)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferKind {
    Uniform,
    StorageRead,
    StorageWrite,
}

#[derive(Debug)]
pub struct ShaderError(pub String);

impl std::fmt::Display for ShaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Shader {
    /// `name` only shows up in error messages.
    pub fn from_wgsl(name: &str, source: &str) -> Result<Shader, ShaderError> {
        let full = format!("{}\n{}", crate::PRELUDE, source);
        let prelude_lines = crate::PRELUDE.lines().count() + 1;
        let module = naga::front::wgsl::parse_str(&full).map_err(|e| {
            let msg = e.emit_to_string(&full);
            ShaderError(format!("{name}: {} (line numbers include {prelude_lines} prelude lines)", msg))
        })?;
        let info = Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .map_err(|e| ShaderError(format!("{name}: {}", e.emit_to_string(&full))))?;

        let mut push_constant_size = 0;
        let mut buffers = Vec::new();
        for (_, var) in module.global_variables.iter() {
            match var.space {
                naga::AddressSpace::Immediate => {
                    push_constant_size = module.types[var.ty].inner.size(module.to_ctx());
                }
                naga::AddressSpace::Uniform | naga::AddressSpace::Storage { .. } => {
                    let Some(b) = &var.binding else { continue };
                    if b.group != 1 {
                        continue;
                    }
                    let kind = match var.space {
                        naga::AddressSpace::Uniform => BufferKind::Uniform,
                        naga::AddressSpace::Storage { access } if access.contains(naga::StorageAccess::STORE) => {
                            BufferKind::StorageWrite
                        }
                        _ => BufferKind::StorageRead,
                    };
                    buffers.push((b.binding, kind));
                }
                _ => {}
            }
        }
        buffers.sort_by_key(|b| b.0);

        Ok(Shader { module, info, hash: loom_noise::fnv(full.as_bytes()), push_constant_size, buffers })
    }
}

