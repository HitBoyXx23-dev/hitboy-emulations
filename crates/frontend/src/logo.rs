//! Brand marks: geometric monograms drawn from simple shapes (no font), set on a black
//! tile and filled with the brand's two-colour gradient. Rendered with resvg.

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};

use crate::theme::Brand;

fn hex(c: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())
}

/// 64x64 app mark for a brand.
pub fn svg(b: &Brand) -> String {
    let (p, s) = (hex(b.primary), hex(b.secondary));
    // Every mark shares one construction: 8-unit pillars on a 16..48 grid, slanted 10 degrees.
    // HitBoy's: an H whose crossbar is an arrow pointing forward.
    let (glyph, extra) = (
        r#"<rect x="16" y="16" width="8" height="32"/><rect x="40" y="16" width="8" height="32"/><path d="M24 28 H33 L40 32 L33 36 H24 Z"/>"#,
        String::new(),
    );
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
             <defs>
               <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
                 <stop offset="0" stop-color="{p}"/><stop offset="1" stop-color="{s}"/>
               </linearGradient>
               <linearGradient id="edge" x1="0" y1="0" x2="1" y2="1">
                 <stop offset="0" stop-color="{p}" stop-opacity="0.7"/><stop offset="1" stop-color="{s}" stop-opacity="0.35"/>
               </linearGradient>
             </defs>
             <rect x="0.5" y="0.5" width="63" height="63" rx="14" fill="#0A0A10" stroke="url(#edge)" stroke-width="1.5"/>
             <g fill="url(#g)" transform="translate(32 32) skewX(-10) translate(-32 -32)">{glyph}</g>
             {extra}
           </svg>"##
    )
}

/// Rasterises the brand mark to `size` x `size` straight-alpha RGBA.
pub fn rasterize(b: &Brand, size: u32, _tile: bool) -> Option<(Vec<u8>, u32)> {
    let mut opt = resvg::usvg::Options::default();
    std::sync::Arc::make_mut(&mut opt.fontdb).load_system_fonts();
    let tree = resvg::usvg::Tree::from_str(&svg(b), &opt).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia stores premultiplied alpha; convert to straight RGBA.
    let mut rgba = pixmap.take();
    for px in rgba.chunks_mut(4) {
        let a = px[3] as f32 / 255.0;
        if a > 0.0 {
            for c in &mut px[..3] {
                *c = ((*c as f32 / a).round()).min(255.0) as u8;
            }
        }
    }
    Some((rgba, size))
}

pub fn texture(ctx: &egui::Context, b: &Brand, size: u32) -> Option<TextureHandle> {
    let (rgba, n) = rasterize(b, size, false)?;
    let img = ColorImage::from_rgba_unmultiplied([n as usize, n as usize], &rgba);
    Some(ctx.load_texture("brand-mark", img, TextureOptions::LINEAR))
}

pub fn window_icon(b: &Brand) -> egui::IconData {
    match rasterize(b, 256, true) {
        Some((rgba, n)) => egui::IconData { rgba, width: n, height: n },
        None => egui::IconData::default(),
    }
}
