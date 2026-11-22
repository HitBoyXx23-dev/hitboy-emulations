use super::*;
use gcn::Vertex;

struct RenderState {
    width: u32,
    height: u32,
    viewport: [f64; 4],
    scissor: [u32; 4],
    blend: bool,
    mask: u32,
}

impl RenderState {
    fn new(registers: &BTreeMap<u32, u32>, width: u32, height: u32) -> Result<Self, String> {
        let register = |address| registers.get(&address).copied().unwrap_or(0);
        if width == 0 || height == 0 || width as u64 * height as u64 > 4194304 { return Err("GPU render surface exceeds the pixel limit".into()); }
        if register(0xc242) != 4 || register(0xa200) & 7 != 0 || register(0xa205) != 0x240 || register(0xa204) != 0x80000 || register(0xa206) != 0x43f { return Err("GPU raster topology, depth, culling or clip state is not implemented".into()); }
        let format = register(0xa31c);
        if (format >> 2) & 31 != 10 || (format >> 8) & 7 != 0 || format & 0x6003 != 0 || (format >> 11) & 3 > 1 || register(0xa08e) & !15 != 0 { return Err("GPU raster output currently requires one RGBA8 UNORM target".into()); }
        let control = register(0xa1e0);
        let blend = control & 0x40000000 != 0 && format & 0x10000 == 0;
        if blend && (control != 0x65010504 || format & 0x8000 == 0) { return Err("GPU blend mode is not implemented".into()); }
        if register(0xa202) & 0x70 != 0x10 || (!blend && control & 0x80000000 == 0 && (register(0xa202) >> 16) & 255 != 0xcc) { return Err("GPU color mode or logical operation is not implemented".into()); }
        let viewport = [0xa10f, 0xa110, 0xa111, 0xa112].map(|address| f32::from_bits(register(address)) as f64);
        if viewport.iter().any(|value| !value.is_finite()) || viewport[0] == 0.0 || viewport[2] == 0.0 || register(0xa080) != 0 { return Err("GPU viewport is invalid or requires unsupported window offsets".into()); }
        let mut scissor = [0, 0, width, height];
        for (top, bottom) in [(0xa00c, 0xa00d), (0xa081, 0xa082), (0xa094, 0xa095)] {
            let first = register(top);
            let last = register(bottom);
            scissor[0] = scissor[0].max(first & 0x7fff);
            scissor[1] = scissor[1].max((first >> 16) & 0x7fff);
            scissor[2] = scissor[2].min(last & 0x7fff);
            scissor[3] = scissor[3].min((last >> 16) & 0x7fff);
        }
        Ok(Self { width, height, viewport, scissor, blend, mask: register(0xa08e) & register(0xa08f) & 15 })
    }
}

fn distance(vertex: &Vertex, plane: usize) -> f64 {
    let [x, y, z, w] = vertex.position.map(|value| value as f64);
    match plane { 0 => x + w, 1 => w - x, 2 => y + w, 3 => w - y, 4 => z, 5 => w - z, _ => w - 1.0e-6 }
}

fn intersection(left: &Vertex, right: &Vertex, weight: f32) -> Vertex {
    let position = std::array::from_fn(|component| left.position[component] + weight * (right.position[component] - left.position[component]));
    let attributes = left.attributes.iter().map(|(key, value)| (*key, std::array::from_fn(|component| value[component] + weight * (right.attributes[key][component] - value[component])))).collect();
    Vertex { position, attributes }
}

fn clip(vertices: &[Vertex]) -> Result<Vec<[Vertex; 3]>, String> {
    if vertices.len() != 3 || vertices.iter().any(|vertex| vertex.position.iter().any(|value| !value.is_finite())) { return Err("GPU triangle positions are invalid".into()); }
    if !vertices[0].attributes.keys().eq(vertices[1].attributes.keys()) || !vertices[0].attributes.keys().eq(vertices[2].attributes.keys()) { return Err("GPU triangle attributes have inconsistent layouts".into()); }
    let mut polygon = vertices.to_vec();
    for plane in 0..7 {
        if polygon.len() < 3 { return Ok(Vec::new()); }
        let mut output = Vec::new();
        let mut previous = polygon.last().unwrap();
        let mut previous_distance = distance(previous, plane);
        for current in &polygon {
            let current_distance = distance(current, plane);
            if (previous_distance >= 0.0) != (current_distance >= 0.0) {
                let weight = (previous_distance / (previous_distance - current_distance)) as f32;
                output.push(intersection(previous, current, weight));
            }
            if current_distance >= 0.0 { output.push(current.clone()); }
            previous = current;
            previous_distance = current_distance;
        }
        if output.len() > 16 { return Err("GPU clipped polygon exceeds the vertex limit".into()); }
        polygon = output;
    }
    Ok((1..polygon.len().saturating_sub(1)).map(|index| [polygon[0].clone(), polygon[index].clone(), polygon[index + 1].clone()]).collect())
}

fn edge(left: [f64; 2], right: [f64; 2], point: [f64; 2]) -> f64 {
    (right[0] - left[0]) * (point[1] - left[1]) - (right[1] - left[1]) * (point[0] - left[0])
}

struct Triangle {
    vertices: [Vertex; 3],
    points: [[f64; 2]; 3],
    top_left: [bool; 3],
    area: f64,
    bounds: [u32; 4],
}

impl Triangle {
    fn new(mut vertices: [Vertex; 3], state: &RenderState) -> Option<Self> {
        let mut points = vertices.each_ref().map(|vertex| {
            let inverse = 1.0 / vertex.position[3] as f64;
            [vertex.position[0] as f64 * inverse * state.viewport[0] + state.viewport[1], vertex.position[1] as f64 * inverse * state.viewport[2] + state.viewport[3]]
        });
        let mut area = edge(points[0], points[1], points[2]);
        if !area.is_finite() || area.abs() < 1.0e-12 { return None; }
        if area < 0.0 { points.swap(1, 2); vertices.swap(1, 2); area = -area; }
        let top_left = [(1, 2), (2, 0), (0, 1)].map(|(first, last)| {
            let delta = [points[last][0] - points[first][0], points[last][1] - points[first][1]];
            delta[1] < 0.0 || (delta[1] == 0.0 && delta[0] > 0.0)
        });
        let minimum_x = points.iter().map(|point| point[0]).fold(f64::INFINITY, f64::min).floor();
        let minimum_y = points.iter().map(|point| point[1]).fold(f64::INFINITY, f64::min).floor();
        let maximum_x = points.iter().map(|point| point[0]).fold(f64::NEG_INFINITY, f64::max).ceil();
        let maximum_y = points.iter().map(|point| point[1]).fold(f64::NEG_INFINITY, f64::max).ceil();
        let bounds = [minimum_x.max(state.scissor[0] as f64).max(0.0) as u32, minimum_y.max(state.scissor[1] as f64).max(0.0) as u32, maximum_x.max(0.0).min(state.scissor[2] as f64).min(state.width as f64) as u32, maximum_y.max(0.0).min(state.scissor[3] as f64).min(state.height as f64) as u32];
        Some(Self { vertices, points, top_left, area, bounds })
    }

    fn sample(&self, x: u32, y: u32) -> Option<[f32; 2]> {
        let point = [x as f64 + 0.5, y as f64 + 0.5];
        let weights = [(1, 2), (2, 0), (0, 1)].map(|(first, last)| edge(self.points[first], self.points[last], point));
        if weights.iter().zip(self.top_left).any(|(value, top_left)| *value < 0.0 || (*value == 0.0 && !top_left)) { return None; }
        let weights = std::array::from_fn::<_, 3, _>(|index| weights[index] / self.area / self.vertices[index].position[3] as f64);
        let total = weights.iter().sum::<f64>();
        if total <= 0.0 || !total.is_finite() { return None; }
        Some([(weights[1] / total) as f32, (weights[2] / total) as f32])
    }
}

#[derive(Default)]
pub(super) struct RasterStats {
    pub triangles: u64,
    pub covered_samples: u64,
    pub shaded_samples: u64,
    pub pixel_wave_instructions: u64,
}

pub(super) struct RasterResult {
    pub rgba: Vec<u8>,
    pub stats: RasterStats,
}

fn blend_pixel(pixel: &mut [u8], source: [f32; 4], state: &RenderState) {
    let source = source.map(|value| value.clamp(0.0, 1.0));
    for component in 0..4 {
        if state.mask & (1 << component) == 0 { continue; }
        let destination = pixel[component] as f32 / 255.0;
        let value = if state.blend {
            source[component] * if component == 3 { 1.0 } else { source[3] } + destination * (1.0 - source[3])
        } else { source[component] };
        pixel[component] = (value.clamp(0.0, 1.0) * 255.0).round_ties_even() as u8;
    }
}

fn shade(mem: &Mem, registers: &BTreeMap<u32, u32>, triangle: &Triangle, batch: &[(usize, [f32; 2])], state: &RenderState, result: &mut RasterResult) -> Result<(), String> {
    if batch.is_empty() { return Ok(()); }
    let barycentrics = batch.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    let (colours, instructions) = gcn::pixel_exports(mem, registers, triangle.vertices.each_ref(), &barycentrics)?;
    result.stats.pixel_wave_instructions += instructions;
    for ((offset, _), colour) in batch.iter().zip(colours) {
        if let Some(colour) = colour {
            blend_pixel(&mut result.rgba[*offset..*offset + 4], colour, state);
            result.stats.shaded_samples += 1;
        }
    }
    Ok(())
}

pub(super) fn render(mem: &Mem, registers: &BTreeMap<u32, u32>, vertices: &[Vertex], width: u32, height: u32, initial_rgba: &[u8]) -> Result<RasterResult, String> {
    let state = RenderState::new(registers, width, height)?;
    if vertices.len() > 4096 || vertices.len() % 3 != 0 { return Err("GPU triangle-list vertex count is invalid".into()); }
    let size = width as usize * height as usize * 4;
    if initial_rgba.len() != size { return Err("GPU initial render surface size is invalid".into()); }
    let mut result = RasterResult { rgba: initial_rgba.to_vec(), stats: RasterStats::default() };
    let mut batch = Vec::with_capacity(64);
    for input in vertices.chunks_exact(3) {
        for vertices in clip(input)? {
            let Some(triangle) = Triangle::new(vertices, &state) else { continue; };
            result.stats.triangles += 1;
            for y in triangle.bounds[1]..triangle.bounds[3] {
                for x in triangle.bounds[0]..triangle.bounds[2] {
                    let Some(barycentrics) = triangle.sample(x, y) else { continue; };
                    if result.stats.covered_samples >= 16777216 { return Err("GPU raster sample budget exceeded".into()); }
                    result.stats.covered_samples += 1;
                    batch.push((((y as usize * width as usize) + x as usize) * 4, barycentrics));
                    if batch.len() == 64 {
                        shade(mem, registers, &triangle, &batch, &state, &mut result)?;
                        batch.clear();
                    }
                }
            }
            shade(mem, registers, &triangle, &batch, &state, &mut result)?;
            batch.clear();
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vertex(position: [f32; 4]) -> Vertex { Vertex { position, attributes: BTreeMap::from([(0, [position[0]; 4])]) } }

    fn state() -> RenderState { RenderState { width: 4, height: 4, viewport: [2.0, 2.0, -2.0, 2.0], scissor: [0, 0, 4, 4], blend: true, mask: 15 } }

    #[test]
    fn top_left_coverage_has_no_shared_edge_gaps_or_double_shading() {
        let a = vertex([-1.0, 1.0, 0.5, 1.0]);
        let b = vertex([1.0, 1.0, 0.5, 1.0]);
        let c = vertex([-1.0, -1.0, 0.5, 1.0]);
        let d = vertex([1.0, -1.0, 0.5, 1.0]);
        let first = Triangle::new([a.clone(), b.clone(), c.clone()], &state()).unwrap();
        let second = Triangle::new([c, b, d], &state()).unwrap();
        for y in 0..4 {
            for x in 0..4 { assert_eq!(first.sample(x, y).is_some() as u8 + second.sample(x, y).is_some() as u8, 1); }
        }
        assert_eq!(first.sample(0, 0).unwrap(), [0.125, 0.125]);
        let perspective = Triangle::new([a, vertex([2.0, 2.0, 1.0, 2.0]), vertex([-1.0, -1.0, 0.5, 1.0])], &state()).unwrap();
        let sample = perspective.sample(0, 0).unwrap();
        assert!((sample[0] - 1.0 / 15.0).abs() < 1.0e-7);
        assert!((sample[1] - 2.0 / 15.0).abs() < 1.0e-7);
    }

    #[test]
    fn homogeneous_clipping_preserves_attributes_at_new_boundary_vertices() {
        let input = [vertex([-2.0, 0.0, 0.5, 1.0]), vertex([0.0, -0.5, 0.5, 1.0]), vertex([0.0, 0.5, 0.5, 1.0])];
        let triangles = clip(&input).unwrap();
        assert_eq!(triangles.len(), 2);
        for vertex in triangles.iter().flatten() {
            assert!(vertex.position[0] >= -1.0);
            assert_eq!(vertex.attributes[&0][0], vertex.position[0]);
        }
        let outside = [vertex([-3.0, 0.0, 0.5, 1.0]), vertex([-2.0, -0.5, 0.5, 1.0]), vertex([-2.0, 0.5, 0.5, 1.0])];
        assert!(clip(&outside).unwrap().is_empty());
    }

    #[test]
    fn captured_alpha_blend_and_channel_masks_preserve_destination_channels() {
        let mut state = state();
        let mut pixel = [0, 0, 255, 255];
        blend_pixel(&mut pixel, [1.0, 0.0, 0.0, 0.5], &state);
        assert_eq!(pixel, [128, 0, 128, 255]);
        state.mask = 1;
        let mut pixel = [0, 7, 9, 11];
        blend_pixel(&mut pixel, [1.0; 4], &state);
        assert_eq!(pixel, [255, 7, 9, 11]);
    }

    #[test]
    #[ignore]
    fn real_ot11_first_draw_covers_the_full_surface_using_both_authentic_shaders() {
        let mut snapshot = gcn::fixtures::raster_snapshot();
        let (mut vertices, _) = gcn::vertex_exports(&mut snapshot.mem, &snapshot.registers, &snapshot.indices).unwrap();
        let (width, height) = (1920, 1080);
        let initial = [255, 0, 255, 0].repeat(width as usize * height as usize);
        let started = std::time::Instant::now();
        let rendered = render(&snapshot.mem, &snapshot.registers, &vertices, width, height, &initial).unwrap();
        assert_eq!(rendered.stats.triangles, 2);
        assert_eq!(rendered.stats.covered_samples, width as u64 * height as u64);
        assert_eq!(rendered.stats.shaded_samples, width as u64 * height as u64);
        assert!(rendered.rgba.chunks_exact(4).all(|pixel| pixel == [0, 0, 0, 255]));
        if let Some(directory) = std::env::var_os("HITBOY_PS4_RASTER_IMAGE_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("first-draw-1920x1080.rgba"), &rendered.rgba).unwrap();
        }
        println!("isolated_authentic_draw={}x{} triangles={} covered={} shaded={} pixel_wave_instructions={} seconds={:.3} black_clear_verified=true game_presentation_verified=false gameplay_verified=false", width, height, rendered.stats.triangles, rendered.stats.covered_samples, rendered.stats.shaded_samples, rendered.stats.pixel_wave_instructions, started.elapsed().as_secs_f64());
        for vertex in &mut vertices { vertex.attributes.insert(0, [vertex.position[0] * 0.5 + 0.5, 0.5 - vertex.position[1] * 0.5, 0.25, 0.5]); }
        let constants = snapshot.registers[&0x2c18] as u64 | ((snapshot.registers[&0x2c19] & 0xfff) as u64) << 32;
        snapshot.mem.write(constants + 4, &1.0f32.to_le_bytes());
        for (address, value) in [(0xa10f, 32.0f32), (0xa110, 32.0), (0xa111, -32.0), (0xa112, 32.0)] { snapshot.registers.insert(address, value.to_bits()); }
        let gradient = render(&snapshot.mem, &snapshot.registers, &vertices, 64, 64, &[0, 0, 255, 255].repeat(64 * 64)).unwrap();
        assert_eq!(gradient.stats.covered_samples, 4096);
        assert_eq!(gradient.stats.shaded_samples, 4096);
        for y in 0..64 {
            for x in 0..64 {
                let expected = [(((x as f32 + 0.5) / 64.0) * 0.5 * 255.0).round_ties_even() as u8, (((y as f32 + 0.5) / 64.0) * 0.5 * 255.0).round_ties_even() as u8, 159, 255];
                let offset = (y * 64 + x) * 4;
                assert_eq!(&gradient.rgba[offset..offset + 4], &expected, "controlled gradient pixel({x},{y})");
            }
        }
        if let Some(directory) = std::env::var_os("HITBOY_PS4_RASTER_IMAGE_DIR") {
            std::fs::write(std::path::PathBuf::from(directory).join("controlled-gradient-64x64.rgba"), &gradient.rgba).unwrap();
        }
        println!("controlled_gradient_replay_verified=true pixels=4096 both_triangle_edges_and_alpha_blending_verified=true gameplay_verified=false");
    }
}
