use openvr::HmdVector2_t;
use openxr::{Extent2Di, Fovf, Offset2Di, Rect2Di};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

// Percentage of the original per-eye image removed at each edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FovCrop {
    top: u8,
    bottom: u8,
    outer: u8,
    inner: u8,
}

impl FovCrop {
    fn parse(text: &str) -> Option<Self> {
        let mut fields = text.split(',');
        let mut edge = || fields.next()?.parse::<u8>().ok().filter(|v| *v <= 25);
        let crop = Self {
            top: edge()?,
            bottom: edge()?,
            outer: edge()?,
            inner: edge()?,
        };
        fields.next().is_none().then_some(crop)
    }

    pub(super) fn from_environment() -> Self {
        let Ok(text) = std::env::var("XRIZER_FRAME_CROP") else {
            return Self::default();
        };
        match Self::parse(&text) {
            Some(crop) => {
                log::info!("Per-eye render crop (%): {crop:?}");
                crop
            }
            None => {
                log::warn!("Invalid XRIZER_FRAME_CROP; using full field of view");
                Self::default()
            }
        }
    }

    pub(super) fn target_size(self, width: u32, height: u32) -> (u32, u32) {
        let scale = |pixels: u32, cut: u8| {
            ((u64::from(pixels) * u64::from(100 - cut) + 50) / 100).max(1) as u32
        };
        (
            scale(width, self.outer + self.inner),
            scale(height, self.top + self.bottom),
        )
    }

    pub(super) fn projection(self, fov: Fovf, eye: usize) -> Fovf {
        // Pixels are linear in tangent space. Keep the original per-pixel rays
        // and mirror the temple/nose settings for the right eye.
        let trim = |edge: f32, opposite: f32, percent: u8| {
            if percent == 0 {
                return edge;
            }
            let t = edge.tan();
            (t + (opposite.tan() - t) * (f32::from(percent) / 100.0)).atan()
        };
        let (left, right) = if eye == 0 {
            (self.outer, self.inner)
        } else {
            (self.inner, self.outer)
        };
        Fovf {
            angle_left: trim(fov.angle_left, fov.angle_right, left),
            angle_right: trim(fov.angle_right, fov.angle_left, right),
            angle_up: trim(fov.angle_up, fov.angle_down, self.top),
            angle_down: trim(fov.angle_down, fov.angle_up, self.bottom),
        }
    }

    // The application rendered 'base'. Trim its existing image and its angular
    // bounds together so a ray keeps sampling the same texel in the headset.
    pub(crate) fn submitted_view(
        self,
        base: Self,
        eye: usize,
        extent: Extent2Di,
        flipped: bool,
        mut fov: Fovf,
    ) -> (Rect2Di, Fovf) {
        if self == base || extent.width <= 0 || extent.height <= 0 {
            if flipped {
                std::mem::swap(&mut fov.angle_up, &mut fov.angle_down);
            }
            return (
                Rect2Di {
                    offset: Offset2Di::default(),
                    extent,
                },
                fov,
            );
        }
        let axis = |pixels: i32, start: u8, end: u8, base_start: u8, base_end: u8| {
            let span = i64::from(100 - base_start - base_end);
            let cut = |value: u8, floor: u8| {
                ((i64::from(pixels) * i64::from(value.saturating_sub(floor)) + span / 2) / span)
                    as i32
            };
            let near = cut(start, base_start).min(pixels - 1);
            let far = cut(end, base_end).min(pixels - near - 1);
            (near, far)
        };
        let (outer, inner) = axis(extent.width, self.outer, self.inner, base.outer, base.inner);
        let (left, right) = if eye == 0 {
            (outer, inner)
        } else {
            (inner, outer)
        };
        let (top, bottom) = axis(extent.height, self.top, self.bottom, base.top, base.bottom);
        let trim_angles = |start: f32, end: f32, near: i32, far: i32, pixels: i32| {
            let trim = |a: f32, b: f32, n: i32| {
                if n == 0 {
                    a
                } else {
                    (a.tan() + (b.tan() - a.tan()) * (n as f32 / pixels as f32)).atan()
                }
            };
            (trim(start, end, near), trim(end, start, far))
        };
        (fov.angle_left, fov.angle_right) =
            trim_angles(fov.angle_left, fov.angle_right, left, right, extent.width);
        (fov.angle_up, fov.angle_down) =
            trim_angles(fov.angle_up, fov.angle_down, top, bottom, extent.height);
        if flipped {
            std::mem::swap(&mut fov.angle_up, &mut fov.angle_down);
        }
        let rect = Rect2Di {
            offset: Offset2Di {
                x: left,
                y: if flipped { bottom } else { top },
            },
            extent: Extent2Di {
                width: extent.width - left - right,
                height: extent.height - top - bottom,
            },
        };
        (rect, fov)
    }
}

pub(super) struct LiveCrop {
    path: Option<PathBuf>,
    next_poll: Instant,
    current: FovCrop,
}

impl LiveCrop {
    pub(super) fn new(base: FovCrop) -> Self {
        Self {
            path: std::env::var_os("XRIZER_FRAME_CROP_FILE").map(PathBuf::from),
            next_poll: Instant::now(),
            current: base,
        }
    }

    pub(super) fn read(&mut self, now: Instant) -> FovCrop {
        if let Some(path) = &self.path
            && now >= self.next_poll
        {
            self.next_poll = now + Duration::from_millis(50);
            if let Ok(value) = std::fs::read_to_string(path)
                && let Some(crop) = FovCrop::parse(value.trim())
            {
                self.current = crop;
            }
        }
        self.current
    }
}

// Vertices have already been projected into the cropped eye's [0,1] square.
// Intersect the polygon with each edge before returning OpenVR texture coordinates.
fn clip_polygon(vertices: &[HmdVector2_t]) -> Vec<HmdVector2_t> {
    let mut polygon = vertices.to_vec();
    for (axis, boundary) in [(0, 0.0), (0, 1.0), (1, 0.0), (1, 1.0)] {
        let Some(mut previous) = polygon.last().copied() else {
            break;
        };
        let inside = |p: HmdVector2_t| {
            if boundary == 0.0 {
                p.v[axis] >= boundary
            } else {
                p.v[axis] <= boundary
            }
        };
        let mut clipped = Vec::new();
        for current in polygon {
            if inside(previous) != inside(current) {
                let t = (boundary - previous.v[axis]) / (current.v[axis] - previous.v[axis]);
                let other = 1 - axis;
                let mut intersection = previous;
                intersection.v[axis] = boundary;
                intersection.v[other] += t * (current.v[other] - previous.v[other]);
                clipped.push(intersection);
            }
            if inside(current) {
                clipped.push(current);
            }
            previous = current;
        }
        polygon = clipped;
    }
    polygon.dedup_by(|a, b| a.v == b.v);
    if polygon.len() > 1 && polygon.first().unwrap().v == polygon.last().unwrap().v {
        polygon.pop();
    }
    polygon
}

pub(super) fn clip_mask(vertices: &[HmdVector2_t], line_loop: bool) -> Vec<HmdVector2_t> {
    if line_loop {
        let polygon = clip_polygon(vertices);
        return if polygon.len() >= 3 {
            polygon
        } else {
            Vec::new()
        };
    }
    let mut result = Vec::new();
    for triangle in vertices.chunks_exact(3) {
        let polygon = clip_polygon(triangle);
        for i in 2..polygon.len() {
            result.extend_from_slice(&[polygon[0], polygon[i - 1], polygon[i]]);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_submission_matches_source_texels_in_both_eyes_and_orientations() {
        let full = Fovf {
            angle_left: -0.9,
            angle_right: 0.7,
            angle_up: 0.8,
            angle_down: -0.6,
        };
        let base = FovCrop::parse("2,6,4,8").unwrap();
        let live = FovCrop::parse("10,8,12,9").unwrap();
        let extent = Extent2Di {
            width: 880,
            height: 920,
        };
        for eye in 0..2 {
            for flipped in [true, false] {
                let rendered = base.projection(full, eye);
                let (rect, fov) = live.submitted_view(base, eye, extent, flipped, rendered);
                let expected = live.projection(full, eye);
                assert_eq!((rect.extent.width, rect.extent.height), (790, 820));
                assert_eq!(rect.offset.x, if eye == 0 { 80 } else { 10 });
                assert_eq!(rect.offset.y, if flipped { 20 } else { 80 });
                assert!((fov.angle_left - expected.angle_left).abs() < 1e-6);
                assert!((fov.angle_right - expected.angle_right).abs() < 1e-6);
                let (up, down) = if flipped {
                    (fov.angle_down, fov.angle_up)
                } else {
                    (fov.angle_up, fov.angle_down)
                };
                assert!((up - expected.angle_up).abs() < 1e-6);
                assert!((down - expected.angle_down).abs() < 1e-6);
                let (restored, _) =
                    FovCrop::default().submitted_view(base, eye, extent, flipped, rendered);
                assert_eq!(
                    (
                        restored.offset.x,
                        restored.offset.y,
                        restored.extent.width,
                        restored.extent.height
                    ),
                    (0, 0, extent.width, extent.height)
                );
            }
        }
    }

    #[test]
    fn changing_preview_file_updates_a_running_session_and_can_restore_full_view() {
        let path =
            std::env::temp_dir().join(format!("xrizer-live-preview-test-{}", std::process::id()));
        let start = Instant::now();
        let mut poll = LiveCrop {
            path: Some(path.clone()),
            next_poll: start,
            current: FovCrop::default(),
        };
        for (i, (text, expected)) in [
            ("2,4,6,8", (860, 940)),
            ("partial", (860, 940)),
            ("8,1,3,12", (850, 910)),
            ("0,0,0,0", (1000, 1000)),
        ]
        .into_iter()
        .enumerate()
        {
            std::fs::write(&path, text).unwrap();
            assert_eq!(
                poll.read(start + Duration::from_millis(51 * i as u64))
                    .target_size(1000, 1000),
                expected
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn asymmetric_crop_preserves_pixel_rays_for_both_eyes() {
        let crop = FovCrop::parse("3,7,12,4").unwrap();
        let fov = Fovf {
            angle_left: (-1.4_f32).atan(),
            angle_right: 0.9_f32.atan(),
            angle_down: (-0.8_f32).atan(),
            angle_up: 1.25_f32.atan(),
        };
        assert_eq!(crop.target_size(2000, 1500), (1680, 1350));
        for (eye, left_trim) in [(0, 0.12), (1, 0.04)] {
            let cropped = crop.projection(fov, eye);
            for p in [0.0, 0.5, 1.0] {
                let x = cropped.angle_left.tan()
                    + p * (cropped.angle_right.tan() - cropped.angle_left.tan());
                let y = cropped.angle_down.tan()
                    + p * (cropped.angle_up.tan() - cropped.angle_down.tan());
                assert!((x - (-1.4 + 2.3 * (left_trim + p * 0.84))).abs() < 1e-6);
                assert!((y - (-0.8 + 2.05 * (0.07 + p * 0.90))).abs() < 1e-6);
            }
        }
        let full = FovCrop::default();
        assert_eq!(full.projection(fov, 1).angle_right, fov.angle_right);
        assert_eq!(full.target_size(2305, 1403), (2305, 1403));
        for bad in ["1,2,3", "2,2,2,2,2", "26,2,2,2", "2,-1,2,2"] {
            assert!(FovCrop::parse(bad).is_none());
        }
    }

    #[test]
    fn visibility_masks_intersect_the_new_viewport() {
        let v = |x, y| HmdVector2_t { v: [x, y] };
        let outside = [v(-1.0, 0.0), v(-0.2, 0.0), v(-0.1, 0.8)];
        assert!(clip_mask(&outside, false).is_empty());
        let cover = [v(-2.0, -2.0), v(4.0, -2.0), v(-2.0, 4.0)];
        let loop_verts = clip_mask(&cover, true);
        assert_eq!(loop_verts.len(), 4);
        let triangles = clip_mask(&cover, false);
        assert_eq!(triangles.len(), 6);
        let mut area = 0.;
        for tri in triangles.chunks_exact(3) {
            let [a, b, c] = [tri[0].v, tri[1].v, tri[2].v];
            assert!(
                tri.iter()
                    .all(|v| v.v.iter().all(|x| (0.0..=1.0).contains(x)))
            );
            area += ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])) / 2.;
        }
        assert!((area - 1.0).abs() < 1e-6);
    }
}
