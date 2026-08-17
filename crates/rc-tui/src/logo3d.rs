//! A live ASCII render of the brand mark rotating in 3D, used for the in-turn
//! loading indicators (`thinking…` and a running tool, plus the one-cell
//! status-bar spinner that samples it) — the welcome card shows the static
//! half-block splash instead, so motion on screen always means "working."
//!
//! Same technique as the classic `donut.c`: build a point cloud with
//! surface normals, tumble it every frame with a couple of rotation
//! matrices, project with a weak-perspective camera, keep only the
//! nearest point per cell (a z-buffer), then pick a character by the angle
//! between that point's normal and a fixed light. [`frame`] returns one
//! `(char, brightness)` per cell; the caller (`theme::logo3d_lines`) turns
//! `brightness` into a color so the render stays inside the project's
//! one-hue color budget instead of a grayscale ramp.
//!
//! The shape itself is `logo.svg`'s single path — a six-petal flower — baked
//! in as its absolute cubic-bezier control points ([`LOGO_START`] +
//! [`LOGO_SEGS`]) so the binary needs no SVG/XML parser at runtime. It was
//! extracted once, offline, with a short script that regexes the numbers out
//! of the path's `d` attribute; re-run the same extraction if `logo.svg` ever
//! changes shape. The flat outline is then extruded into a thin solid disc
//! (front face + back face + a shaded side wall) so it reads as a real 3D
//! object rather than a cutout that vanishes edge-on.

use std::sync::OnceLock;

const LOGO_START: (f64, f64) = (0.0, 0.0);
#[rustfmt::skip]
const LOGO_SEGS: [[f64; 6]; 71] = [
    [7.92243908, 7.48804947, 8.53541498, 15.92655181, 9.08398438, 26.17382812],
    [9.61362752, 33.56208257, 11.06911557, 39.45990016, 16.8125, 44.4375],
    [21.85451426, 46.90907562, 25.82955, 47.61459415, 31.1875, 45.875],
    [35.6566456, 43.97323592, 37.83066521, 41.25607716, 40.05078125, 36.97265625],
    [41.43335143, 32.64287885, 41.51934735, 28.32926634, 41.8125, 23.8125],
    [42.61007021, 14.0641419, 43.97416874, 6.4810299, 51.375, -0.4375],
    [57.53016312, -5.14360744, 64.47437131, -7.5331747, 72.2265625, -6.78515625],
    [80.74146496, -5.24896251, 87.06981784, -1.04311737, 92.0, 6.0],
    [95.95266362, 12.54233979, 95.87354222, 19.52081473, 95.0, 27.0],
    [92.47680723, 35.37699998, 87.52012001, 40.42831191, 80.08203125, 44.66015625],
    [73.71624529, 47.29232609, 66.33429935, 46.11084583, 59.60351562, 45.8828125],
    [52.18018714, 45.72842785, 46.15410054, 45.84973908, 40.01171875, 50.5078125],
    [35.73017156, 55.73660464, 32.85992643, 60.85615693, 33.09375, 67.7734375],
    [34.11977966, 74.97537645, 36.29146222, 79.77554127, 41.765625, 84.53515625],
    [48.07492649, 88.85775021, 55.57355284, 89.06739645, 63.01000977, 88.38525391],
    [70.130068, 87.00683998, 74.72068089, 82.69312463, 79.4375, 77.5],
    [84.13655493, 72.58281107, 88.21757, 68.68588126, 95.0, 67.0],
    [95.66386719, 66.83242187, 96.32773438, 66.66484375, 97.01171875, 66.4921875],
    [104.75745123, 65.06173789, 111.79521594, 67.2211011, 118.21484375, 71.5859375],
    [122.9128235, 75.30731314, 126.64640034, 80.07929307, 128.0, 86.0],
    [129.00026308, 95.02435156, 128.45715319, 102.53593405, 123.0, 110.0],
    [118.24032065, 115.32496193, 112.91764673, 118.57001438, 105.796875, 119.3515625],
    [96.68072054, 119.72646929, 89.9953264, 118.29989861, 83.0078125, 112.28515625],
    [81.26606964, 110.5770522, 79.6188745, 108.82437929, 78.0, 107.0],
    [72.12870448, 100.52652032, 66.18351936, 97.49317902, 57.44140625, 96.97265625],
    [49.83236081, 97.05787756, 43.6767114, 99.93789318, 38.0, 105.0],
    [34.35253222, 109.94011433, 32.47317217, 115.84211264, 33.25390625, 122.00390625],
    [34.71155739, 128.92446615, 38.06684887, 134.04456592, 44.0, 138.0],
    [48.1650392, 139.65196723, 51.45887262, 140.20940777, 55.921875, 140.01953125],
    [57.03046875, 139.99052734, 58.1390625, 139.96152344, 59.28125, 139.93164062],
    [61.57857805, 139.85273465, 63.87552558, 139.76174856, 66.171875, 139.65820312],
    [74.69483241, 139.46584471, 81.46047849, 140.85339994, 87.90673828, 146.74169922],
    [94.37288444, 153.64098436, 95.63705279, 160.20525342, 95.41015625, 169.48828125],
    [94.59987872, 176.42580031, 91.74015686, 181.71492854, 86.625, 186.4375],
    [78.81558358, 192.31537632, 71.69123722, 193.13190201, 62.0, 192.0],
    [55.16505072, 189.94128034, 49.75676857, 186.10913768, 46.0, 180.0],
    [42.55985458, 173.39659891, 42.30909635, 167.16891985, 41.91601562, 159.82617188],
    [41.38637248, 152.43791743, 39.93088443, 146.54009984, 34.1875, 141.5625],
    [29.14548574, 139.09092438, 25.17045, 138.38540585, 19.8125, 140.125],
    [15.3433544, 142.02676408, 13.16933479, 144.74392284, 10.94921875, 149.02734375],
    [9.56664857, 153.35712115, 9.48065265, 157.67073366, 9.1875, 162.1875],
    [8.38992979, 171.9358581, 7.02583126, 179.5189701, -0.375, 186.4375],
    [-8.18441642, 192.31537632, -15.30876278, 193.13190201, -25.0, 192.0],
    [-33.53119278, 189.43036362, -38.35056036, 184.32741808, -42.875, 176.9375],
    [-45.29257789, 170.47120734, -45.08814501, 162.61264142, -43.12109375, 156.08203125],
    [-39.76846206, 148.76820435, -34.71916416, 143.67072502, -27.23925781, 140.57788086],
    [-21.26456573, 139.03600382, -14.71443663, 139.9101546, -8.60351562, 140.1171875],
    [-1.20316625, 140.27109425, 5.06593678, 140.27332287, 11.0234375, 135.31640625],
    [11.69246094, 134.44822266, 11.69246094, 134.44822266, 12.375, 133.5625],
    [12.83648437, 132.98628906, 13.29796875, 132.41007813, 13.7734375, 131.81640625],
    [17.22406654, 126.70639828, 18.61530257, 121.59203663, 17.56640625, 115.46875],
    [15.88638551, 109.16629932, 13.76904835, 104.46773482, 8.14453125, 100.78515625],
    [-0.1955205, 96.25216874, -7.97673364, 96.66249646, -17.0, 99.0],
    [-22.07431286, 101.67554678, -25.74694238, 105.4802076, -29.55078125, 109.734375],
    [-35.84873573, 116.51010533, -41.63532557, 119.1791132, -50.9375, 119.5625],
    [-58.50551644, 119.31758037, -64.16659396, 117.42646272, -69.83984375, 112.35546875],
    [-76.30042354, 105.35782365, -77.63951439, 98.89545834, -77.41015625, 89.51171875],
    [-76.59987872, 82.57419969, -73.74015686, 77.28507146, -68.625, 72.5625],
    [-62.46983688, 67.85639256, -55.52562869, 65.4668253, -47.7734375, 66.21484375],
    [-38.0031129, 67.97753118, -31.97529918, 73.62203085, -25.4753418, 80.72216797],
    [-21.10377129, 85.35834711, -16.82661976, 87.63330338, -10.38671875, 88.46875],
    [-1.33753803, 88.54519503, 5.83013961, 87.82683648, 12.6484375, 81.4609375],
    [16.70058626, 76.51872139, 18.35807028, 70.12876954, 17.80859375, 63.79296875],
    [16.35020892, 56.92147987, 12.79314071, 51.86209381, 7.0, 48.0],
    [2.8349608, 46.34803277, -0.45887262, 45.79059223, -4.921875, 45.98046875],
    [-6.03046875, 46.00947266, -7.1390625, 46.03847656, -8.28125, 46.06835938],
    [-10.57857805, 46.14726535, -12.87552558, 46.23825144, -15.171875, 46.34179688],
    [-23.69483241, 46.53415529, -30.46047849, 45.14660006, -36.90673828, 39.25830078],
    [-43.37242783, 32.35950284, -44.64277733, 25.7915101, -44.40625, 16.5078125],
    [-43.4382953, 8.14989596, -39.39224535, 2.49737667, -32.91015625, -2.6640625],
    [-22.8243563, -9.71793486, -9.16206677, -7.55505278, 0.0, 0.0],
];

/// Half the extruded solid's thickness (in the shape's normalized radius-1
/// units) — thick enough that the side wall catches light and reads as a
/// real edge when the flower turns broadside to the camera. Chunkier than a
/// coin (0.16 read as a near-flat cutout at a glance); this reads as an
/// actual slab.
const HALF_THICKNESS: f64 = 0.22;
/// Candidate samples per axis when filling the two faces; only points inside
/// the outline survive, so the actual point count is roughly this squared
/// times the outline's fill ratio.
const INTERIOR_GRID: usize = 44;
/// Bezier subdivisions per path segment for the outline itself.
const OUTLINE_SAMPLES_PER_SEG: usize = 6;
/// How many depth slices the side wall is sampled at.
const WALL_Z_STEPS: usize = 5;

/// One sample of the extruded solid: a position and outward unit normal, both
/// in the shape's rest frame (unrotated, centered on the origin, longest
/// radius normalized to 1.0).
#[derive(Clone, Copy)]
struct Point {
    pos: [f32; 3],
    normal: [f32; 3],
}

fn flatten_outline() -> Vec<(f64, f64)> {
    let mut pts = Vec::with_capacity(LOGO_SEGS.len() * OUTLINE_SAMPLES_PER_SEG + 1);
    let mut cur = LOGO_START;
    pts.push(cur);
    for seg in LOGO_SEGS {
        let [x1, y1, x2, y2, x, y] = seg;
        for k in 1..=OUTLINE_SAMPLES_PER_SEG {
            let t = k as f64 / OUTLINE_SAMPLES_PER_SEG as f64;
            let mt = 1.0 - t;
            let bx = mt * mt * mt * cur.0
                + 3.0 * mt * mt * t * x1
                + 3.0 * mt * t * t * x2
                + t * t * t * x;
            let by = mt * mt * mt * cur.1
                + 3.0 * mt * mt * t * y1
                + 3.0 * mt * t * t * y2
                + t * t * t * y;
            pts.push((bx, by));
        }
        cur = (x, y);
    }
    pts
}

/// Recenters on the outline's bounding-box middle and scales so the longer
/// bounding-box dimension is 2.0 (radius 1.0 from center). SVG y grows
/// downward; flip it so the shape's "up" matches the screen's.
fn normalize(outline: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let (mut minx, mut maxx, mut miny, mut maxy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for &(x, y) in outline {
        minx = minx.min(x);
        maxx = maxx.max(x);
        miny = miny.min(y);
        maxy = maxy.max(y);
    }
    let cx = (minx + maxx) / 2.0;
    let cy = (miny + maxy) / 2.0;
    let r = ((maxx - minx).max(maxy - miny) / 2.0).max(1e-6);
    outline
        .iter()
        .map(|&(x, y)| ((x - cx) / r, -(y - cy) / r))
        .collect()
}

/// Even-odd point-in-polygon test (ray casting) — the outline never
/// self-intersects, so even-odd and nonzero agree.
fn point_in_polygon(poly: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    let n = poly.len();
    for i in 0..n {
        let (x1, y1) = poly[i];
        let (x2, y2) = poly[(i + 1) % n];
        if (y1 > y) != (y2 > y) {
            let x_at_y = x1 + (y - y1) / (y2 - y1) * (x2 - x1);
            if x < x_at_y {
                inside = !inside;
            }
        }
    }
    inside
}

fn build_cloud() -> Vec<Point> {
    let outline = normalize(&flatten_outline());
    let mut out = Vec::new();

    // The two flat faces: sample a grid over the bounding box and keep the
    // points that land inside the outline.
    for iy in 0..INTERIOR_GRID {
        for ix in 0..INTERIOR_GRID {
            let x = -1.05 + 2.1 * (ix as f64 + 0.5) / INTERIOR_GRID as f64;
            let y = -1.05 + 2.1 * (iy as f64 + 0.5) / INTERIOR_GRID as f64;
            if point_in_polygon(&outline, x, y) {
                out.push(Point {
                    pos: [x as f32, y as f32, HALF_THICKNESS as f32],
                    normal: [0.0, 0.0, 1.0],
                });
                out.push(Point {
                    pos: [x as f32, y as f32, -HALF_THICKNESS as f32],
                    normal: [0.0, 0.0, -1.0],
                });
            }
        }
    }

    // The side wall: at each outline point, extrude across the thickness.
    // The local outward normal is the 2D tangent rotated a quarter turn,
    // sign-corrected to point away from the center — a good proxy for
    // "outward" on a star-shaped path like this one.
    let n = outline.len();
    for i in 0..n {
        let (x, y) = outline[i];
        let (px, py) = outline[(i + n - 1) % n];
        let (nx2, ny2) = outline[(i + 1) % n];
        let (tx, ty) = (nx2 - px, ny2 - py);
        let (mut nx, mut ny) = (ty, -tx);
        let len = (nx * nx + ny * ny).sqrt().max(1e-6);
        nx /= len;
        ny /= len;
        if nx * x + ny * y < 0.0 {
            nx = -nx;
            ny = -ny;
        }
        for s in 0..WALL_Z_STEPS {
            let t = (s as f64 + 0.5) / WALL_Z_STEPS as f64 - 0.5;
            let z = t * 2.0 * HALF_THICKNESS;
            out.push(Point {
                pos: [x as f32, y as f32, z as f32],
                normal: [nx as f32, ny as f32, 0.0],
            });
        }
    }
    out
}

fn cloud() -> &'static [Point] {
    static CLOUD: OnceLock<Vec<Point>> = OnceLock::new();
    CLOUD.get_or_init(build_cloud)
}

/// Radians/sec for the two tumble axes — deliberately not a common multiple,
/// so the object never repeats a face-on pose on a short cycle. Mirrors
/// `donut.c`'s trick of rotating around two axes at once instead of one: a
/// single spin axis reads as flat once it edges-on; two axes keeps the solid
/// looking three-dimensional continuously. Brisk enough to read as a
/// confident spin rather than a slow wobble.
const SPEED_A: f32 = 1.15;
const SPEED_B: f32 = 0.5;

/// Camera distance along z (weak-perspective denominator offset), i.e. the
/// camera sits at `z = -CAM_DIST` looking toward `+z`. The shape's rotated
/// extent reaches `z ≈ ±1.05` (radius ~1.0 plus the extrusion), so this stays
/// safely clear of zero while still giving real perspective: the near/far
/// depth ratio is roughly `(CAM_DIST+1.05)/(CAM_DIST-1.05) ≈ 3.6x`, vs. the
/// near-orthographic ~2.3x a more distant camera would give. That size
/// difference between the tumble's near and far points is what reads as
/// "solid object turning in space" instead of "flat shape stretching."
const CAM_DIST: f32 = 2.2;
/// Character cells are roughly twice as tall as wide; stretch x so the
/// rotating shape doesn't read squashed.
const ASPECT: f32 = 2.0;
/// The key light (upper-right-front), normalized — the main diffuse term and
/// what [`HALF_VEC`]'s specular highlight is built from. Its `z` is negative:
/// the camera sits at `z = -CAM_DIST` looking toward `+z`, so the surfaces it
/// sees have normals pointing back at it (`-z`), and a light with positive `z`
/// would illuminate only the far side the camera can't see — leaving every
/// visible cell at the ambient floor.
const LIGHT: [f32; 3] = [0.4082483, 0.4082483, -0.8164966];
/// A dim fill light from the opposite side (lower-left-front), standard
/// three-point-lighting practice: it keeps the side away from the key light
/// from reading as a single flat "unlit" tone, adding a second, softer
/// gradient across the surface instead of one hard light/dark split. Also
/// camera-side (negative `z`), for the same reason as [`LIGHT`].
const FILL_LIGHT: [f32; 3] = [-0.5050762, -0.3030458, -0.808122];
/// The halfway vector between [`LIGHT`] and the camera direction (`(0,0,-1)`)
/// — `normalize(LIGHT + (0,0,-1))`, precomputed since both operands are
/// constant. Used for the specular highlight below.
const HALF_VEC: [f32; 3] = [0.2141865, 0.2141865, -0.9530206];
/// Specular exponent. Tight enough that the highlight stays a highlight: the
/// shape's flat faces are large regions of *identical* normals, so a broad
/// exponent lights an entire face at once and the pose reads as one saturated
/// blob rather than a lit surface.
const SHININESS: f32 = 12.0;
/// Fresnel/rim exponent for the edge glow below — low enough that the glow
/// reaches a reasonable band of near-grazing normals, not just a hairline.
const RIM_POWER: f32 = 2.0;
/// Lighting weights (ambient + key diffuse + fill diffuse + rim + specular).
/// Budgeted so the *most common* pose — a flat face square to the camera,
/// which is one big region of identical normals — lands around 0.7 rather
/// than clamping at 1.0: saturating there would flatten the whole face to a
/// single character and throw the gradient away. That leaves headroom for
/// the rim term and the depth cue to push genuinely-brighter cells to the
/// top of the ramp. An unlit point still lands above the ramp's darkest
/// character (a true-black cell reads as a hole in the shape, not a shadow).
const AMBIENT: f32 = 0.1;
const DIFFUSE_WEIGHT: f32 = 0.45;
const FILL_WEIGHT: f32 = 0.12;
const RIM_WEIGHT: f32 = 0.2;
const SPECULAR_WEIGHT: f32 = 0.25;

/// donut.c's own luminance ramp — darkest to brightest. Reused verbatim as
/// the homage the welcome card is (see the module doc); background cells
/// (no point landed there) are a plain space, handled separately.
const RAMP: &[u8] = b".,-~:;=!*#$@";

/// One rendered cell: the chosen ramp character and its brightness in `0.0
/// ..= 1.0` (background cells the shape doesn't cover are `(' ', 0.0)`).
#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub brightness: f32,
}

/// Renders one frame of the tumbling logo into a `width`×`height` grid of
/// [`Cell`]s, `elapsed_secs` after the animation's epoch. Pass `0.0` (or any
/// fixed value) for a static, non-animated frame.
pub fn frame(width: u16, height: u16, elapsed_secs: f32) -> Vec<Vec<Cell>> {
    let (w, h) = (width as usize, height as usize);
    let mut grid = vec![
        vec![
            Cell {
                ch: ' ',
                brightness: 0.0
            };
            w
        ];
        h
    ];
    if w == 0 || h == 0 {
        return grid;
    }
    let mut depth = vec![f32::MIN; w * h];

    let a = elapsed_secs * SPEED_A;
    let b = elapsed_secs * SPEED_B;
    let (sa, ca) = a.sin_cos();
    let (sb, cb) = b.sin_cos();

    // K1 scales the unit-radius shape to fill most of the grid. Solve for the
    // K1 that puts a radius-1 point at 90% of the tighter half-extent once
    // divided by the projection's `CAM_DIST` denominator (the shape sits near
    // z=0, so `ooz ≈ 1 / CAM_DIST` there) — whichever axis is more
    // constrained (accounting for the x stretch) wins.
    let half_extent = (h as f32 / 2.0).min(w as f32 / (2.0 * ASPECT));
    let k1 = 0.9 * CAM_DIST * half_extent;

    for p in cloud() {
        let [x, y, z] = p.pos;
        // Rotate around X by `a`, then around Z by `b` (position and normal
        // share the same linear map).
        let (x1, y1, z1) = (x, y * ca - z * sa, y * sa + z * ca);
        let (x2, y2, _z2) = (x1 * cb - y1 * sb, x1 * sb + y1 * cb, z1);
        let z2 = z1;

        let [nx, ny, nz] = p.normal;
        let (nx1, ny1, nz1) = (nx, ny * ca - nz * sa, ny * sa + nz * ca);
        let (nx2, ny2, nz2) = (nx1 * cb - ny1 * sb, nx1 * sb + ny1 * cb, nz1);

        let ooz = 1.0 / (CAM_DIST + z2);
        let sx = (w as f32) / 2.0 + k1 * ASPECT * x2 * ooz;
        let sy = (h as f32) / 2.0 - k1 * y2 * ooz;
        let (col, row) = (sx.round() as isize, sy.round() as isize);
        if col < 0 || row < 0 || col as usize >= w || row as usize >= h {
            continue;
        }
        let idx = row as usize * w + col as usize;
        // z-buffer: larger ooz means closer to the camera; keep the nearest
        // point's shading per cell.
        if ooz <= depth[idx] {
            continue;
        }
        depth[idx] = ooz;

        // Four shading terms, stacked like a cheap three-point-lighting rig:
        //  - key diffuse: how square the surface faces the main light:
        let diffuse = (nx2 * LIGHT[0] + ny2 * LIGHT[1] + nz2 * LIGHT[2]).max(0.0);
        //  - fill diffuse: a second, dimmer light from the other side, so
        //    the key light's shadow side gets its own soft gradient instead
        //    of collapsing to one flat ambient tone;
        let fill = (nx2 * FILL_LIGHT[0] + ny2 * FILL_LIGHT[1] + nz2 * FILL_LIGHT[2]).max(0.0);
        //  - specular: how square the surface faces halfway between the key
        //    light and the camera — the glossy highlight, tight and bright;
        let n_dot_h = (nx2 * HALF_VEC[0] + ny2 * HALF_VEC[1] + nz2 * HALF_VEC[2]).max(0.0);
        let specular = n_dot_h.powf(SHININESS);
        //  - rim/Fresnel: brightens normals that graze the view direction
        //    (`(0,0,-1)`, so this reduces to `1 - |nz2|`) rather than face
        //    it head-on or point straight away — the silhouette-hugging
        //    edge glow that reads as "this surface is curving out of view
        //    here," the cue flat diffuse shading can't give at all.
        let rim = (1.0 - nz2.abs()).max(0.0).powf(RIM_POWER);
        // Depth cue: on top of all of the above, nudge nearer points a touch
        // brighter and farther points a touch dimmer (`ooz * CAM_DIST` is
        // 1.0 at the shape's own center depth) — a cheap fog/AO stand-in
        // that reinforces depth beyond what normals alone convey.
        let depth_cue = ((ooz * CAM_DIST - 1.0) * 0.25 + 1.0).clamp(0.7, 1.3);
        let brightness = ((AMBIENT
            + diffuse * DIFFUSE_WEIGHT
            + fill * FILL_WEIGHT
            + rim * RIM_WEIGHT
            + specular * SPECULAR_WEIGHT)
            * depth_cue)
            .clamp(0.0, 1.0);
        let ramp_i = ((brightness * (RAMP.len() - 1) as f32).round() as usize).min(RAMP.len() - 1);
        grid[row as usize][col as usize] = Cell {
            ch: RAMP[ramp_i] as char,
            brightness,
        };
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flattened outline should form a real hexafoil-ish blob, not a
    /// degenerate point — sanity-checks the baked-in path data.
    #[test]
    fn outline_has_spread_in_both_axes() {
        let outline = normalize(&flatten_outline());
        let (mut minx, mut maxx) = (f64::MAX, f64::MIN);
        for &(x, _) in &outline {
            minx = minx.min(x);
            maxx = maxx.max(x);
        }
        assert!(
            maxx - minx > 1.5,
            "outline should span most of the unit radius: {}",
            maxx - minx
        );
    }

    /// The point cloud covers both faces and the side wall — never empty,
    /// and not so sparse a typical welcome-card grid renders blank.
    #[test]
    fn cloud_is_populated() {
        assert!(
            cloud().len() > 500,
            "expected a dense point cloud, got {}",
            cloud().len()
        );
    }

    /// A rendered frame actually draws something inside its bounds — this is
    /// the property `draw_welcome_card` depends on to show a logo at all.
    #[test]
    fn frame_draws_non_blank_cells() {
        let grid = frame(18, 8, 0.0);
        let non_blank = grid.iter().flatten().filter(|c| c.ch != ' ').count();
        assert!(
            non_blank > 10,
            "expected a visible shape, got {non_blank} non-blank cells"
        );
    }

    /// Rotating over time changes the silhouette — this is what makes it
    /// read as "spinning" rather than a still frame.
    #[test]
    fn frame_changes_over_time() {
        let f0 = frame(18, 8, 0.0);
        let f1 = frame(18, 8, 1.5);
        assert_ne!(
            f0.iter().flatten().map(|c| c.ch).collect::<Vec<_>>(),
            f1.iter().flatten().map(|c| c.ch).collect::<Vec<_>>(),
            "expected the silhouette to change after rotating"
        );
    }

    /// Zero-sized grids (a terminal resize edge case) must not panic.
    #[test]
    fn frame_handles_zero_size() {
        assert_eq!(frame(0, 8, 0.0).iter().flatten().count(), 0);
        assert_eq!(frame(8, 0, 0.0).len(), 0);
    }

    /// Brightness stays a real gradient, not a constant — this is what
    /// `theme::logo3d_lines` shades between the accent's two RGB tones.
    #[test]
    fn frame_brightness_varies() {
        let grid = frame(36, 16, 0.6);
        let mut seen: Vec<f32> = grid.iter().flatten().map(|c| c.brightness).collect();
        seen.sort_by(|a, b| a.partial_cmp(b).unwrap());
        seen.dedup();
        assert!(
            seen.len() > 3,
            "expected varied brightness, got {} distinct values",
            seen.len()
        );
    }

    /// The specular term should, at some point during the tumble, push a
    /// cell brighter than diffuse lighting alone could reach (`AMBIENT +
    /// DIFFUSE_WEIGHT`, since diffuse maxes out at a normal pointing exactly
    /// at the light) — the "glossy highlight" that reads as curved,
    /// three-dimensional shading rather than flat diffuse lighting.
    #[test]
    fn frame_has_a_specular_highlight() {
        let diffuse_only_ceiling = AMBIENT + DIFFUSE_WEIGHT;
        let brightest = (0..40)
            .map(|i| frame(36, 16, i as f32 * 0.15))
            .flat_map(|grid| {
                grid.into_iter()
                    .flatten()
                    .map(|c| c.brightness)
                    .collect::<Vec<_>>()
            })
            .fold(0.0f32, f32::max);
        assert!(
            brightest > diffuse_only_ceiling + 0.05,
            "expected specular to exceed the diffuse-only ceiling ({diffuse_only_ceiling}) \
             across the tumble, got {brightest}"
        );
    }

    /// The fill light and rim term exist specifically to spread shading
    /// across more of the ramp than key-light diffuse alone would reach —
    /// this is what separates "one mid-gray blob" from a shape with a real
    /// light/dark gradient. Across a full tumble, most of the ramp
    /// (including some of its darkest characters, from the fill-lit and
    /// rim-lit sides) should show up somewhere.
    #[test]
    fn shading_spans_most_of_the_ramp() {
        let mut seen = [false; RAMP.len()];
        for i in 0..60 {
            let grid = frame(26, 11, i as f32 * 0.1);
            for cell in grid.iter().flatten() {
                if let Some(pos) = RAMP.iter().position(|&b| b as char == cell.ch) {
                    seen[pos] = true;
                }
            }
        }
        let distinct = seen.iter().filter(|&&s| s).count();
        assert!(
            distinct >= RAMP.len() - 2,
            "expected shading to reach nearly the whole ramp, got {distinct}/{} distinct characters: {:?}",
            RAMP.len(),
            RAMP.iter().zip(seen).map(|(&b, s)| (b as char, s)).collect::<Vec<_>>()
        );
    }

    /// At the app's animation tick rate (~60 Hz, `TICK_ANIM` in `app.rs`),
    /// consecutive frames should change gradually — most of the grid holding
    /// steady — rather than repainting wholesale. Wholesale repaints are what
    /// "choppy"/flickery motion looks like; a real rotation moves its
    /// silhouette's edge a cell or so per frame, not the whole shape.
    #[test]
    fn consecutive_frames_change_gradually() {
        let dt = 0.016;
        let (w, h) = (26, 11);
        let mut prev = frame(w, h, 1.0);
        for i in 1..=20 {
            let next = frame(w, h, 1.0 + dt * i as f32);
            let changed = prev
                .iter()
                .flatten()
                .zip(next.iter().flatten())
                .filter(|(a, b)| a.ch != b.ch)
                .count();
            let total = (w as usize) * (h as usize);
            assert!(
                changed * 100 < total * 40,
                "frame changed too much between 16ms ticks: {changed}/{total} cells"
            );
            prev = next;
        }
    }
}
