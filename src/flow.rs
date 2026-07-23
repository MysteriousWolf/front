//! Pyramidal Horn–Schunck optical-flow estimator.
//!
//! Pure numeric module: takes two equal-sized scalar grids (already
//! downsampled to a coarse resolution by the caller — see
//! `docs/design/radar-flow-interpolation.md`) and estimates a per-cell
//! displacement field describing how content moved from the first grid to
//! the second, in coarse-grid cell units.
//!
//! Single-scale Horn–Schunck only resolves displacements of roughly one
//! cell per iteration, so a coarse-to-fine (pyramidal) scheme is used: flow
//! is estimated on a heavily downsampled pair first, then upsampled and
//! refined at each finer level by warping the target grid with the current
//! estimate before re-solving. That warp-and-refine step is what recovers
//! translations of several cells. Both the pyramid depth and the
//! iterations spent per level are fixed, named constants — the loop always
//! terminates in bounded work, never on a convergence check.

/// Maximum number of pyramid levels (including the finest, full-resolution
/// level). Capped so a pathologically large input can't blow up the work
/// bound; actual depth also stops early once a level would fall below
/// [`MIN_PYRAMID_DIM`].
const MAX_PYRAMID_LEVELS: usize = 4;

/// Coarsest level is never downsampled below this many cells on its
/// shorter side — Horn–Schunck's neighbor-averaging breaks down on grids
/// too small to have an interior.
const MIN_PYRAMID_DIM: usize = 8;

/// Fixed number of Jacobi relaxation iterations run at every pyramid
/// level. Bounded, not convergence-driven.
const ITERATIONS_PER_LEVEL: usize = 30;

/// Horn–Schunck smoothness weight (alpha²). Higher values favor smoother,
/// more globally consistent flow over fidelity to the per-cell brightness-
/// constancy constraint.
const ALPHA_SQUARED: f32 = 0.1;

/// A dense per-cell motion field estimated between two coarse scalar
/// grids, in coarse-grid cell units (displacement from the first grid to
/// the second).
#[derive(Debug, Clone)]
pub(crate) struct FlowField {
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// Horizontal displacement per cell, row-major, length `width*height`.
    pub(crate) u: Vec<f32>,
    /// Vertical displacement per cell, row-major, length `width*height`.
    pub(crate) v: Vec<f32>,
    /// Caller-owned metadata: the ratio of full-resolution cells to one
    /// coarse cell in this field (e.g. the downsample factor CP2 divided
    /// by when it built the input grids). Defaults to `1.0`; the estimator
    /// itself is agnostic to it and never reads it.
    pub(crate) scale: f32,
}

impl FlowField {
    fn zeroed(width: usize, height: usize) -> Self {
        FlowField {
            width,
            height,
            u: vec![0.0; width * height],
            v: vec![0.0; width * height],
            scale: 1.0,
        }
    }

    /// Bilinearly samples the flow vector at a fractional coarse-grid
    /// coordinate, clamping out-of-range coordinates to the grid edge.
    pub(crate) fn vector_at(&self, x: f32, y: f32) -> (f32, f32) {
        if self.width == 0 || self.height == 0 {
            return (0.0, 0.0);
        }
        let max_x = (self.width - 1) as f32;
        let max_y = (self.height - 1) as f32;
        let x = x.clamp(0.0, max_x);
        let y = y.clamp(0.0, max_y);

        let x0 = x.floor() as usize;
        let y0 = y.floor() as usize;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;

        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let sample = |field: &[f32]| {
            let top = lerp(field[y0 * self.width + x0], field[y0 * self.width + x1], fx);
            let bot = lerp(field[y1 * self.width + x0], field[y1 * self.width + x1], fx);
            lerp(top, bot, fy)
        };

        (sample(&self.u), sample(&self.v))
    }
}

/// A single pyramid level's pair of grids.
struct Level {
    width: usize,
    height: usize,
    src: Vec<f32>,
    tgt: Vec<f32>,
}

/// Estimates a pyramidal Horn–Schunck flow field describing the motion
/// from `src` to `tgt`. Both grids must be `width * height` row-major
/// scalar arrays of equal size.
pub(crate) fn estimate_flow(src: &[f32], tgt: &[f32], width: usize, height: usize) -> FlowField {
    assert_eq!(src.len(), width * height, "src size must match width*height");
    assert_eq!(tgt.len(), width * height, "tgt size must match width*height");
    if width == 0 || height == 0 {
        return FlowField::zeroed(width, height);
    }

    let pyramid = build_pyramid(src, tgt, width, height);

    // Coarsest level first, refine down to the finest (index 0).
    let mut flow: Option<FlowField> = None;
    for level in pyramid.iter().rev() {
        let base = match flow.take() {
            Some(prev) => upsample_flow(&prev, level.width, level.height),
            None => FlowField::zeroed(level.width, level.height),
        };
        flow = Some(refine_level(level, base));
    }

    flow.expect("pyramid always has at least one level")
}

/// Builds the pyramid finest-to-coarsest (index 0 = original resolution).
fn build_pyramid(src: &[f32], tgt: &[f32], width: usize, height: usize) -> Vec<Level> {
    let mut levels = vec![Level {
        width,
        height,
        src: src.to_vec(),
        tgt: tgt.to_vec(),
    }];

    while levels.len() < MAX_PYRAMID_LEVELS {
        let last = levels.last().unwrap();
        let (nw, nh) = (last.width / 2, last.height / 2);
        if nw < MIN_PYRAMID_DIM || nh < MIN_PYRAMID_DIM {
            break;
        }
        let src_down = box_downsample(&last.src, last.width, last.height, nw, nh);
        let tgt_down = box_downsample(&last.tgt, last.width, last.height, nw, nh);
        levels.push(Level {
            width: nw,
            height: nh,
            src: src_down,
            tgt: tgt_down,
        });
    }

    levels
}

/// 2x2 box-filter downsample to an explicit target size (handles odd
/// source dimensions by clamping the second sample column/row).
fn box_downsample(data: &[f32], w: usize, h: usize, nw: usize, nh: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; nw * nh];
    for y in 0..nh {
        let sy0 = (2 * y).min(h - 1);
        let sy1 = (2 * y + 1).min(h - 1);
        for x in 0..nw {
            let sx0 = (2 * x).min(w - 1);
            let sx1 = (2 * x + 1).min(w - 1);
            let sum = data[sy0 * w + sx0] + data[sy0 * w + sx1] + data[sy1 * w + sx0] + data[sy1 * w + sx1];
            out[y * nw + x] = sum * 0.25;
        }
    }
    out
}

/// Upsamples a flow field to `(new_w, new_h)`, scaling displacement
/// magnitudes by the resolution ratio so vectors stay in the new level's
/// cell units.
fn upsample_flow(flow: &FlowField, new_w: usize, new_h: usize) -> FlowField {
    let scale_x = if flow.width > 0 {
        new_w as f32 / flow.width as f32
    } else {
        1.0
    };
    let scale_y = if flow.height > 0 {
        new_h as f32 / flow.height as f32
    } else {
        1.0
    };

    let mut out = FlowField::zeroed(new_w, new_h);
    for y in 0..new_h {
        let sy = if new_h > 1 {
            y as f32 * (flow.height.max(1) - 1) as f32 / (new_h - 1) as f32
        } else {
            0.0
        };
        for x in 0..new_w {
            let sx = if new_w > 1 {
                x as f32 * (flow.width.max(1) - 1) as f32 / (new_w - 1) as f32
            } else {
                0.0
            };
            let (u, v) = flow.vector_at(sx, sy);
            out.u[y * new_w + x] = u * scale_x;
            out.v[y * new_w + x] = v * scale_y;
        }
    }
    out
}

/// Refines `base` (this level's initial flow, upsampled from the coarser
/// level or zero at the coarsest) by warping `tgt` with it, computing
/// brightness-constancy gradients, and running bounded Jacobi iterations
/// to solve for an additive increment.
fn refine_level(level: &Level, base: FlowField) -> FlowField {
    let (w, h) = (level.width, level.height);
    let warped_tgt = warp_grid(&level.tgt, w, h, &base.u, &base.v);

    let (ix, iy) = central_gradients(&warped_tgt, w, h);
    let it: Vec<f32> = warped_tgt
        .iter()
        .zip(level.src.iter())
        .map(|(t, s)| t - s)
        .collect();

    let mut du = vec![0.0f32; w * h];
    let mut dv = vec![0.0f32; w * h];

    for _ in 0..ITERATIONS_PER_LEVEL {
        let du_prev = du.clone();
        let dv_prev = dv.clone();
        for y in 0..h {
            for x in 0..w {
                let idx = y * w + x;
                let du_bar = neighbor_average(&du_prev, w, h, x, y);
                let dv_bar = neighbor_average(&dv_prev, w, h, x, y);
                let gx = ix[idx];
                let gy = iy[idx];
                let denom = ALPHA_SQUARED + gx * gx + gy * gy;
                let numer = gx * du_bar + gy * dv_bar + it[idx];
                du[idx] = du_bar - gx * numer / denom;
                dv[idx] = dv_bar - gy * numer / denom;
            }
        }
    }

    let mut out = FlowField::zeroed(w, h);
    for i in 0..w * h {
        out.u[i] = base.u[i] + du[i];
        out.v[i] = base.v[i] + dv[i];
    }
    out
}

/// 4-neighbor average with clamped (edge-replicated) borders — the
/// standard Horn–Schunck Jacobi smoothing term.
fn neighbor_average(field: &[f32], w: usize, h: usize, x: usize, y: usize) -> f32 {
    let xm = x.saturating_sub(1);
    let xp = (x + 1).min(w - 1);
    let ym = y.saturating_sub(1);
    let yp = (y + 1).min(h - 1);
    (field[y * w + xm] + field[y * w + xp] + field[ym * w + x] + field[yp * w + x]) * 0.25
}

/// Central-difference image gradients with clamped borders.
fn central_gradients(data: &[f32], w: usize, h: usize) -> (Vec<f32>, Vec<f32>) {
    let mut gx = vec![0.0f32; w * h];
    let mut gy = vec![0.0f32; w * h];
    for y in 0..h {
        let ym = y.saturating_sub(1);
        let yp = (y + 1).min(h - 1);
        for x in 0..w {
            let xm = x.saturating_sub(1);
            let xp = (x + 1).min(w - 1);
            let idx = y * w + x;
            gx[idx] = (data[y * w + xp] - data[y * w + xm]) * 0.5;
            gy[idx] = (data[yp * w + x] - data[ym * w + x]) * 0.5;
        }
    }
    (gx, gy)
}

/// Backward-warps `data` by displacement field `(u, v)`: `out[y,x] =
/// data(x+u, y+v)`, bilinearly sampled with clamped borders.
fn warp_grid(data: &[f32], w: usize, h: usize, u: &[f32], v: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; w * h];
    let max_x = (w - 1) as f32;
    let max_y = (h - 1) as f32;
    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let sx = (x as f32 + u[idx]).clamp(0.0, max_x);
            let sy = (y as f32 + v[idx]).clamp(0.0, max_y);
            let x0 = sx.floor() as usize;
            let y0 = sy.floor() as usize;
            let x1 = (x0 + 1).min(w - 1);
            let y1 = (y0 + 1).min(h - 1);
            let fx = sx - x0 as f32;
            let fy = sy - y0 as f32;
            let top = data[y0 * w + x0] + (data[y0 * w + x1] - data[y0 * w + x0]) * fx;
            let bot = data[y1 * w + x0] + (data[y1 * w + x1] - data[y1 * w + x0]) * fx;
            out[idx] = top + (bot - top) * fy;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: usize = 32;

    /// A smooth gaussian blob so brightness-constancy gradients are
    /// well-defined; centered at `(cx, cy)` with a fixed sigma.
    fn blob(width: usize, height: usize, cx: f32, cy: f32) -> Vec<f32> {
        let sigma2 = 16.0f32;
        (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    (-(dx * dx + dy * dy) / (2.0 * sigma2)).exp()
                })
            })
            .collect()
    }

    #[test]
    fn static_input_yields_near_zero_flow() {
        let img = blob(SIZE, SIZE, SIZE as f32 / 2.0, SIZE as f32 / 2.0);
        let flow = estimate_flow(&img, &img, SIZE, SIZE);

        let mean_abs_u: f32 = flow.u.iter().map(|v| v.abs()).sum::<f32>() / flow.u.len() as f32;
        let mean_abs_v: f32 = flow.v.iter().map(|v| v.abs()).sum::<f32>() / flow.v.len() as f32;

        assert!(mean_abs_u < 0.05, "mean |u| too large: {mean_abs_u}");
        assert!(mean_abs_v < 0.05, "mean |v| too large: {mean_abs_v}");
    }

    #[test]
    fn recovers_known_translation() {
        let (cx, cy) = (SIZE as f32 / 2.0, SIZE as f32 / 2.0);
        let (dx, dy) = (3.0f32, 2.0f32);
        let src = blob(SIZE, SIZE, cx, cy);
        let tgt = blob(SIZE, SIZE, cx + dx, cy + dy);

        let flow = estimate_flow(&src, &tgt, SIZE, SIZE);

        // Sample near the blob center, where the brightness-constancy
        // signal is strongest, rather than averaging over the whole
        // (mostly flat, low-gradient) background.
        let (u, v) = flow.vector_at(cx, cy);

        assert!((u - dx).abs() < 0.75, "u = {u}, expected ~{dx}");
        assert!((v - dy).abs() < 0.75, "v = {v}, expected ~{dy}");
    }

    #[test]
    fn bounded_iterations_produce_finite_result() {
        // Deliberately un-smooth, high-frequency input: a worst case for
        // brightness constancy. The estimator must still terminate (no
        // convergence-driven loop) and return finite numbers.
        //
        // Boundedness itself is structurally guaranteed by the fixed `for`
        // loop counts (`MAX_PYRAMID_LEVELS`, `ITERATIONS_PER_LEVEL`) — there
        // is no convergence check to fail to converge. This test is the
        // finiteness/well-formedness smoke check on top of that guarantee:
        // a complete, correctly-dimensioned field with no NaN/Inf leaking
        // out of the worst-case brightness-constancy input.
        let mut src = vec![0.0f32; SIZE * SIZE];
        let mut tgt = vec![0.0f32; SIZE * SIZE];
        for i in 0..src.len() {
            src[i] = if i % 2 == 0 { 1.0 } else { 0.0 };
            tgt[i] = if i % 3 == 0 { 1.0 } else { 0.0 };
        }

        let flow = estimate_flow(&src, &tgt, SIZE, SIZE);

        assert_eq!(flow.width, SIZE);
        assert_eq!(flow.height, SIZE);
        assert!(flow.u.iter().all(|v| v.is_finite()));
        assert!(flow.v.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn vector_at_bilinearly_interpolates() {
        let mut flow = FlowField::zeroed(2, 2);
        flow.u = vec![0.0, 2.0, 0.0, 2.0];
        flow.v = vec![0.0, 0.0, 0.0, 0.0];

        let (u, _v) = flow.vector_at(0.5, 0.0);
        assert!((u - 1.0).abs() < 1e-6, "u = {u}");
    }
}
