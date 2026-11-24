//! Re-export of `gpu::slug`: pure Rust-authored `#[gpu::helper]` / `#[gpu::vertex(ir)]` /
//! `#[gpu::fragment(ir)]` port of `den.slang`'s Slug vector font rendering shader + unified
//! WebGPU WGSL & Metal MSL 2.4 SDF/Fillet/Ring/Shadow/Image pipeline.

pub use gpu::slug::{
    SiteInst, SiteUniform, SlugSiteVaryings, emit_slug_site_msl, emit_slug_site_wgsl, site_kind,
    slug_root_code, slug_site_fs, slug_site_vs, slug_solve_horiz, slug_solve_vert,
};
