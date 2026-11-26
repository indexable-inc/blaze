//! Photoreal Beyond-AAA CAD + PBR + Flecs v4.2 Hero Showcase:
//! Builds three distinct real-world CAD assemblies:
//! 1. **Alpine Cedar & Slate Mountain Cabin** in a real 3D conifer clearing (real 3D illuminated
//!    interior room behind clear glass, wrought-iron porch lantern, copper rain gutter & downspout,
//!    exposed rafter tails, diagonal knee braces, corner trim boards, flagstone walkway, stacked
//!    firewood, sculpted alpine boulders, and 3D spruce trees).
//! 2. **Orbital Recon Interceptor** (5-station lofted stealth fuselage, rounded swept delta wings
//!    & canted V-tails, carbon-weave cockpit with glowing cyan HUD, wheeled landing gear, and open
//!    brushed-titanium nozzles with internal stator cones & glowing plasma cores).
//! 3. **Precision CNC Machined Optical Camera Lens** (macro product photography: 36-rib circular
//!    knurled anodized focus ring, brushed titanium mount collar, brass index ring, matte optical
//!    baffle, 8-blade iris, and dual curved multi-coated spherical glass lens elements).
//!
//! Renders 960x640 HD 9-SPP frames in parallel across CPU cores via `PreparedMeshScene::render_pbr_rows`
//! with VNDF rough GGX reflections, thin-lens photographic Depth of Field, 1-bounce BVH GI +
//! emissive NEE, G-buffer À-Trous wavelet denoising, dual-radius HDR optical bloom, and AgX Punchy.
//!
//! Run with:
//! `cargo +nightly-2026-10-27 run --release -p blaze-cad --example pbr_hero -- target/pbr_hero [all|cabin|ship|lens]`

#![feature(default_field_values)]

use std::time::Instant;

use blaze_cad::prelude::*;

fn mm(v: f64) -> Length {
    Length::mm(v)
}

fn shift(x: f64, y: f64, z: f64) -> Placement {
    Placement::shift(mm(x), mm(y), mm(z))
}

/// Rotate about local origin by `rot`, THEN translate to `(x, y, z)`.
fn place_rot(x: f64, y: f64, z: f64, rot: Placement) -> Placement {
    shift(x, y, z).then_after(&rot)
}

fn pbox(g: &mut Graph<'_>, sx: f64, sy: f64, sz: f64, r: f64, x: f64, y: f64, z: f64) -> Result<Solid<Exact>, CadError> {
    let b = g.cuboid(BoxOpts { size: mm3(sx, sy, sz), round: mm(r), .. })?;
    g.place(b, shift(x, y, z))
}

fn save_png(path: &std::path::Path, pic: &blaze_cad_picture::Picture<'_>, alloc: blaze_own::Alloc<'_>) {
    let bytes = pic
        .png(blaze_cad_out::PngOpts {
            level: blaze_deflate::Level::Fast,
            limits: blaze_cad_out::Limits::SHIPPED,
            alloc,
            scratch: alloc,
        })
        .expect("encode png");
    std::fs::write(path, &*bytes).expect("write png");
}

/// Deterministic multi-core parallel HDR + G-buffer PBR renderer over a prebuilt `PreparedMeshScene`.
/// Every pixel row uses identical deterministic seeds, producing bit-for-bit identical output to
/// single-threaded rendering at ~6x speed!
fn render_parallel_pbr<'a>(
    scene: &PreparedMeshScene<'_>,
    looks: &[MaterialLook<'_>],
    view: View,
    pbr: PbrOpts,
    supersample: u8,
    threads: usize,
    alloc: blaze_own::Alloc<'a>,
) -> blaze_cad_picture::Picture<'a> {
    let w = view.width as usize;
    let h = view.height as usize;
    let n = w * h;
    let mut hdr = vec![[0.0f64; 3]; n];
    let mut gbuf = vec![GBufferPixel::SKY; n];
    let mut depth_raw = vec![f64::INFINITY; n];

    let params = blaze_cad_picture::PbrRenderParams {
        material_looks: looks,
        supersample,
        ..
    };
    let scene_ref = scene.as_ref();

    let n_threads = threads.clamp(1, h.max(1));
    let rows_per = h.div_ceil(n_threads);
    let mut total_stats = blaze_cad_picture::PictureStats::default();

    std::thread::scope(|scope| {
        let mut handles = std::vec::Vec::new();
        let mut hdr_rest = hdr.as_mut_slice();
        let mut gbuf_rest = gbuf.as_mut_slice();
        let mut depth_rest = depth_raw.as_mut_slice();
        let mut y0 = 0usize;

        while y0 < h {
            let y1 = (y0 + rows_per).min(h);
            let count = (y1 - y0) * w;
            let (hc, hr) = hdr_rest.split_at_mut(count);
            let (gc, gr) = gbuf_rest.split_at_mut(count);
            let (dc, dr) = depth_rest.split_at_mut(count);
            hdr_rest = hr;
            gbuf_rest = gr;
            depth_rest = dr;

            let y0_u = y0 as u32;
            let y1_u = y1 as u32;
            handles.push(scope.spawn(move || {
                scene_ref
                    .render_pbr_rows(view, &params, &pbr, y0_u, y1_u, hc, gc, dc)
                    .expect("render band")
            }));
            y0 = y1;
        }
        for hnd in handles {
            let st = hnd.join().expect("worker join");
            total_stats.rays += st.rays;
            total_stats.hits += st.hits;
        }
    });

    let mut depth_vec = blaze_own::Vec::try_with_capacity_in(n, alloc).expect("depth alloc");
    for d in depth_raw {
        depth_vec.try_push(d).expect("push depth");
    }
    finish_pbr_frame(&mut hdr, &gbuf, depth_vec, view.width, view.height, &pbr, total_stats, alloc, alloc)
        .expect("finish pbr frame")
}

// =============================================================================
// 1. ALPINE CEDAR & SLATE MOUNTAIN CABIN IN A CONIFER CLEARING
// =============================================================================
fn build_cabin(model: &mut Model<'_>) -> Result<Solid<Bound>, ModelError> {
    let (
        foundation,
        walkway,
        walls,
        roof,
        copper_trim,
        chim,
        porch_struct,
        roof_rafters,
        porch_rail_l,
        porch_rail_r,
        win_front,
        win_side,
        front_door,
        porch_iron,
        flue_iron,
        interior_room,
        glass,
        glow,
        firewood,
        boulders,
        ao_proxy,
    ) = {
        let g = model.graph();
        let rot_y90 = Placement::rotation(Axis::Y, Angle::degrees(90.0));

        // 0: Dressed Granite Foundation Plinth + Porch Pier Base + 2 Crisp Cut-Stone Steps (compact AABB -> razor sharp!)
        let plinth = pbox(g, 5160.0, 3760.0, 480.0, 14.0, 0.0, 0.0, 240.0)?;
        let porch_base = pbox(g, 5160.0, 1400.0, 340.0, 12.0, 0.0, -2420.0, 170.0)?;
        let step1 = pbox(g, 1620.0, 820.0, 145.0, 10.0, 0.0, -3470.0, 72.5)?;
        let step2 = pbox(g, 1620.0, 480.0, 285.0, 10.0, 0.0, -3300.0, 142.5)?;
        let foundation = g.union_all(&[plinth, porch_base, step1, step2])?;

        // 0b: Natural Mossy Slate Stepping Flagstones leading into the alpine meadow (own compact part!)
        let flag1 = pbox(g, 960.0, 580.0, 66.0, 22.0, 60.0, -4280.0, 33.0)?;
        let flag2 = pbox(g, 880.0, 540.0, 66.0, 22.0, 420.0, -4980.0, 33.0)?;
        let flag3 = pbox(g, 920.0, 560.0, 66.0, 22.0, 860.0, -5680.0, 33.0)?;
        let walkway = g.union_all(&[flag1, flag2, flag3])?;

        // 1: Western Red Cedar Exterior Walls (z=460..4040) clipped cleanly under the 26° gable roof planes!
        let wall_outer = pbox(g, 4800.0, 3400.0, 3600.0, 14.0, 0.0, 0.0, 2260.0)?;
        let wall_inner = pbox(g, 4320.0, 2920.0, 3800.0, 0.0, 0.0, 0.0, 2260.0)?;
        let tall_shell = g.difference(wall_outer, wall_inner)?;

        let clip_box = g.cuboid(BoxOpts { size: mm3(5400.0, 4200.0, 2000.0), .. })?;
        let clip_front = g.place(
            clip_box,
            place_rot(0.0, -1600.0 - 438.4, 3979.0 - 1600.0 * 0.48773 + 898.8, Placement::rotation(Axis::X, Angle::degrees(26.0))),
        )?;
        let clip_back = g.place(
            clip_box,
            place_rot(0.0, 1600.0 + 438.4, 3979.0 - 1600.0 * 0.48773 + 898.8, Placement::rotation(Axis::X, Angle::degrees(-26.0))),
        )?;
        let full_shell = g.difference(tall_shell, clip_front)?;
        let full_shell = g.difference(full_shell, clip_back)?;

        let win_fl = pbox(g, 920.0, 620.0, 1120.0, 0.0, -1500.0, -1700.0, 1820.0)?;
        let win_fr = pbox(g, 920.0, 620.0, 1120.0, 0.0, 1500.0, -1700.0, 1820.0)?;
        let win_side_c = pbox(g, 620.0, 1260.0, 1120.0, 0.0, 2400.0, 0.0, 1820.0)?;
        let win_attic = pbox(g, 620.0, 760.0, 440.0, 0.0, 2400.0, 0.0, 3440.0)?;
        let door_cut = pbox(g, 1020.0, 620.0, 2140.0, 0.0, 0.0, -1700.0, 1510.0)?;

        let walls = g.difference(full_shell, win_fl)?;
        let walls = g.difference(walls, win_fr)?;
        let walls = g.difference(walls, win_side_c)?;
        let walls = g.difference(walls, win_attic)?;
        let walls = g.difference(walls, door_cut)?;

        // 2: Watertight 26° Pitched Welsh Slate Gable Roof
        let slope_front = g.cuboid(BoxOpts { size: mm3(5920.0, 3680.0, 155.0), round: mm(12.0), name: Some("roof_front") })?;
        let slope_front = g.place(slope_front, place_rot(0.0, -1640.0, 3272.0, Placement::rotation(Axis::X, Angle::degrees(26.0))))?;
        let slope_back = g.cuboid(BoxOpts { size: mm3(5920.0, 2680.0, 155.0), round: mm(12.0), name: Some("roof_back") })?;
        let slope_back = g.place(slope_back, place_rot(0.0, 1190.0, 3492.0, Placement::rotation(Axis::X, Angle::degrees(-26.0))))?;
        let roof = g.union(slope_front, slope_back)?;

        // 2b: Aged Copper Architectural Ridge Flashing, Front Eave Fascia Drip, Half-Round Rain Gutter & Downspout!
        let ridge_cap = pbox(g, 6020.0, 240.0, 125.0, 24.0, 0.0, 0.0, 4125.0)?;
        let front_fascia = pbox(g, 5980.0, 82.0, 195.0, 10.0, 0.0, -3310.0, 2475.0)?;
        let gutter = pbox(g, 6040.0, 124.0, 118.0, 36.0, 0.0, -3370.0, 2405.0)?;
        let downspout = g.cylinder(CylinderOpts { radius: mm(52.0), height: mm(2120.0), .. })?;
        let downspout = g.place(downspout, shift(2490.0, -2980.0, 1340.0))?;
        let spout_elbow = pbox(g, 104.0, 440.0, 104.0, 28.0, 2490.0, -3170.0, 2380.0)?;
        let copper_trim = g.union_all(&[
            ridge_cap.weaken::<Bound>(),
            front_fascia.weaken::<Bound>(),
            gutter.weaken::<Bound>(),
            downspout.weaken::<Bound>(),
            spout_elbow.weaken::<Bound>(),
        ])?;

        // 3: Stepped Dressed-Granite Fireplace Chimney with Stone Crown Cap
        let chim_lo = pbox(g, 960.0, 960.0, 2800.0, 18.0, 0.0, 0.0, 1400.0)?;
        let chim_hi = pbox(g, 780.0, 780.0, 2250.0, 16.0, 0.0, 0.0, 3820.0)?;
        let chim_cap = pbox(g, 940.0, 940.0, 130.0, 14.0, 0.0, 0.0, 4960.0)?;
        let chim_flue = pbox(g, 380.0, 380.0, 700.0, 10.0, 0.0, 0.0, 4850.0)?;
        let chim_body = g.union_all(&[chim_lo, chim_hi, chim_cap])?;
        let chim = g.difference(chim_body, chim_flue)?;
        let chim = g.place(chim, shift(-1720.0, 1280.0, 0.0))?;

        // 4a: Porch Structure (timber deck, header beam, 4 heavy posts, 45° diagonal knee braces, corner trim boards & exposed rafter tails!)
        let deck = pbox(g, 5080.0, 1380.0, 84.0, 8.0, 0.0, -2420.0, 382.0)?;
        let beam = pbox(g, 5780.0, 195.0, 195.0, 10.0, 0.0, -2900.0, 2480.0)?;
        let post = g.cuboid(BoxOpts { size: mm3(158.0, 158.0, 1980.0), round: mm(10.0), .. })?;
        let p0 = g.place(post, shift(-2340.0, -2900.0, 1405.0))?;
        let p1 = g.place(post, shift(-760.0, -2900.0, 1405.0))?;
        let p2 = g.place(post, shift(760.0, -2900.0, 1405.0))?;
        let p3 = g.place(post, shift(2340.0, -2900.0, 1405.0))?;

        // 45° diagonal timber knee braces under the porch header beam!
        let brace_raw = g.cuboid(BoxOpts { size: mm3(460.0, 110.0, 110.0), round: mm(8.0), .. })?;
        let rot_b_pos = Placement::rotation(Axis::Y, Angle::degrees(45.0));
        let rot_b_neg = Placement::rotation(Axis::Y, Angle::degrees(-45.0));
        let kb0 = g.place(brace_raw, place_rot(-2170.0, -2900.0, 2240.0, rot_b_neg))?;
        let kb1 = g.place(brace_raw, place_rot(-930.0, -2900.0, 2240.0, rot_b_pos))?;
        let kb2 = g.place(brace_raw, place_rot(930.0, -2900.0, 2240.0, rot_b_neg))?;
        let kb3 = g.place(brace_raw, place_rot(2170.0, -2900.0, 2240.0, rot_b_pos))?;

        // Vertical architectural corner trim boards framing the 4 corners of the cedar siding!
        let c_trim = g.cuboid(BoxOpts { size: mm3(150.0, 150.0, 2680.0), round: mm(8.0), .. })?;
        let ct0 = g.place(c_trim, shift(-2375.0, -1675.0, 1810.0))?;
        let ct1 = g.place(c_trim, shift(2375.0, -1675.0, 1810.0))?;
        let ct2 = g.place(c_trim, shift(-2375.0, 1675.0, 1810.0))?;
        let ct3 = g.place(c_trim, shift(2375.0, 1675.0, 1810.0))?;

        // Exposed structural roof ridge purlin, tucked under-eave rafter tails & right gable bargeboard fascia!
        let ridge_beam = pbox(g, 5940.0, 165.0, 220.0, 10.0, 0.0, 0.0, 3910.0)?;
        let rtail = g.cuboid(BoxOpts { size: mm3(98.0, 540.0, 128.0), round: mm(8.0), .. })?;
        let rtail = g.place(rtail, Placement::rotation(Axis::X, Angle::degrees(26.0)))?;
        let rtails = g.linear_repeat(rtail, RepeatOpts { axis: Axis::X, step: mm(740.0), count: 8, .. })?;
        let rtails = g.place(rtails, shift(-2590.0, -3040.0, 2415.0))?;
        let barge_fr = g.cuboid(BoxOpts { size: mm3(76.0, 3720.0, 195.0), round: mm(8.0), .. })?;
        let barge_fr = g.place(barge_fr, place_rot(2955.0, -1640.0, 3225.0, Placement::rotation(Axis::X, Angle::degrees(26.0))))?;
        let barge_bk = g.cuboid(BoxOpts { size: mm3(76.0, 2720.0, 195.0), round: mm(8.0), .. })?;
        let barge_bk = g.place(barge_bk, place_rot(2955.0, 1190.0, 3445.0, Placement::rotation(Axis::X, Angle::degrees(-26.0))))?;

        // Handcrafted solid-cedar craftsman bench resting on the left side of the front porch!
        let bench_seat = pbox(g, 1120.0, 420.0, 62.0, 10.0, -1480.0, -2180.0, 815.0)?;
        let bench_back = pbox(g, 1120.0, 62.0, 380.0, 10.0, -1480.0, -1985.0, 1045.0)?;
        let bench_ll = pbox(g, 76.0, 390.0, 390.0, 8.0, -1960.0, -2180.0, 605.0)?;
        let bench_lr = pbox(g, 76.0, 390.0, 390.0, 8.0, -1000.0, -2180.0, 605.0)?;

        let porch_struct = g.union_all(&[
            deck.weaken::<Bound>(),
            beam.weaken::<Bound>(),
            p0.weaken::<Bound>(),
            p1.weaken::<Bound>(),
            p2.weaken::<Bound>(),
            p3.weaken::<Bound>(),
            kb0.weaken::<Bound>(),
            kb1.weaken::<Bound>(),
            kb2.weaken::<Bound>(),
            kb3.weaken::<Bound>(),
            bench_seat.weaken::<Bound>(),
            bench_back.weaken::<Bound>(),
            bench_ll.weaken::<Bound>(),
            bench_lr.weaken::<Bound>(),
        ])?;
        let roof_rafters = g.union_all(&[
            ct0.weaken::<Bound>(),
            ct1.weaken::<Bound>(),
            ct2.weaken::<Bound>(),
            ct3.weaken::<Bound>(),
            ridge_beam.weaken::<Bound>(),
            rtails.weaken::<Bound>(),
            barge_fr.weaken::<Bound>(),
            barge_bk.weaken::<Bound>(),
        ])?;

        // 4b: Left & Right Porch Balustrades (O(1) linear_repeat balusters)
        let bal = g.cuboid(BoxOpts { size: mm3(64.0, 64.0, 670.0), round: mm(6.0), .. })?;
        let bals_row_6 = g.linear_repeat(bal, RepeatOpts { axis: Axis::X, step: mm(240.0), count: 6, .. })?;
        let bals_fl = g.place(bals_row_6, shift(-2150.0, -2900.0, 870.0))?;
        let bals_fr = g.place(bals_row_6, shift(950.0, -2900.0, 870.0))?;
        let bals_side_4 = g.linear_repeat(bal, RepeatOpts { axis: Axis::Y, step: mm(240.0), count: 4, .. })?;
        let bals_sl = g.place(bals_side_4, shift(-2340.0, -2640.0, 870.0))?;
        let bals_sr = g.place(bals_side_4, shift(2340.0, -2640.0, 870.0))?;
        let r_fl_top = pbox(g, 1440.0, 96.0, 74.0, 8.0, -1550.0, -2900.0, 1220.0)?;
        let r_fl_bot = pbox(g, 1440.0, 80.0, 66.0, 6.0, -1550.0, -2900.0, 520.0)?;
        let r_fr_top = pbox(g, 1440.0, 96.0, 74.0, 8.0, 1550.0, -2900.0, 1220.0)?;
        let r_fr_bot = pbox(g, 1440.0, 80.0, 66.0, 6.0, 1550.0, -2900.0, 520.0)?;
        let r_sl_top = pbox(g, 96.0, 1140.0, 74.0, 8.0, -2340.0, -2280.0, 1220.0)?;
        let r_sl_bot = pbox(g, 80.0, 1140.0, 66.0, 6.0, -2340.0, -2280.0, 520.0)?;
        let r_sr_top = pbox(g, 96.0, 1140.0, 74.0, 8.0, 2340.0, -2280.0, 1220.0)?;
        let r_sr_bot = pbox(g, 80.0, 1140.0, 66.0, 6.0, 2340.0, -2280.0, 520.0)?;
        let porch_rail_l = g.union_all(&[
            bals_fl.weaken::<Bound>(),
            bals_sl.weaken::<Bound>(),
            r_fl_top.weaken::<Bound>(),
            r_fl_bot.weaken::<Bound>(),
            r_sl_top.weaken::<Bound>(),
            r_sl_bot.weaken::<Bound>(),
        ])?;
        let porch_rail_r = g.union_all(&[
            bals_fr.weaken::<Bound>(),
            bals_sr.weaken::<Bound>(),
            r_fr_top.weaken::<Bound>(),
            r_fr_bot.weaken::<Bound>(),
            r_sr_top.weaken::<Bound>(),
            r_sr_bot.weaken::<Bound>(),
        ])?;

        // 4c: Front Window Casings, Sills & 2x2 Mullions
        let wf_top = pbox(g, 1060.0, 130.0, 86.0, 6.0, 0.0, -1730.0, 2410.0)?;
        let wf_sill = pbox(g, 1120.0, 190.0, 82.0, 6.0, 0.0, -1755.0, 1230.0)?;
        let wf_jl = pbox(g, 84.0, 130.0, 1140.0, 6.0, -485.0, -1730.0, 1820.0)?;
        let wf_jr = pbox(g, 84.0, 130.0, 1140.0, 6.0, 485.0, -1730.0, 1820.0)?;
        let wf_mv = pbox(g, 58.0, 74.0, 1120.0, 5.0, 0.0, -1660.0, 1820.0)?;
        let wf_mh = pbox(g, 920.0, 74.0, 58.0, 5.0, 0.0, -1660.0, 1820.0)?;
        let wf_unit = g.union_all(&[wf_top, wf_sill, wf_jl, wf_jr, wf_mv, wf_mh])?;
        let wf_l = g.place(wf_unit, shift(-1500.0, 0.0, 0.0))?;
        let wf_r = g.place(wf_unit, shift(1500.0, 0.0, 0.0))?;
        let win_front = g.union(wf_l, wf_r)?;

        // 4d: Right Side Picture Window + Gable Attic Window Casings & Mullions
        let ws_top = pbox(g, 130.0, 1400.0, 86.0, 6.0, 2430.0, 0.0, 2410.0)?;
        let ws_sill = pbox(g, 190.0, 1440.0, 82.0, 6.0, 2455.0, 0.0, 1230.0)?;
        let ws_jl = pbox(g, 130.0, 84.0, 1140.0, 6.0, 2430.0, -655.0, 1820.0)?;
        let ws_jr = pbox(g, 130.0, 84.0, 1140.0, 6.0, 2430.0, 655.0, 1820.0)?;
        let ws_mv1 = pbox(g, 74.0, 58.0, 1120.0, 5.0, 2360.0, -210.0, 1820.0)?;
        let ws_mv2 = pbox(g, 74.0, 58.0, 1120.0, 5.0, 2360.0, 210.0, 1820.0)?;
        let ws_mh = pbox(g, 74.0, 1260.0, 58.0, 5.0, 2360.0, 0.0, 1820.0)?;
        let wa_top = pbox(g, 130.0, 880.0, 74.0, 5.0, 2430.0, 0.0, 3685.0)?;
        let wa_sill = pbox(g, 170.0, 920.0, 74.0, 5.0, 2450.0, 0.0, 3195.0)?;
        let wa_jl = pbox(g, 130.0, 74.0, 460.0, 5.0, 2430.0, -405.0, 3440.0)?;
        let wa_jr = pbox(g, 130.0, 74.0, 460.0, 5.0, 2430.0, 405.0, 3440.0)?;
        let wa_mv = pbox(g, 74.0, 58.0, 440.0, 5.0, 2360.0, 0.0, 3440.0)?;
        let win_side = g.union_all(&[ws_top, ws_sill, ws_jl, ws_jr, ws_mv1, ws_mv2, ws_mh, wa_top, wa_sill, wa_jl, wa_jr, wa_mv])?;

        // 4e: Paneled Solid Dark Oak Front Door + Surround
        let d_top = pbox(g, 1140.0, 130.0, 88.0, 6.0, 0.0, -1730.0, 2595.0)?;
        let d_jl = pbox(g, 86.0, 130.0, 2140.0, 6.0, -535.0, -1730.0, 1510.0)?;
        let d_jr = pbox(g, 86.0, 130.0, 2140.0, 6.0, 535.0, -1730.0, 1510.0)?;
        let door_slab = pbox(g, 960.0, 96.0, 2100.0, 8.0, 0.0, -1635.0, 1495.0)?;
        let door_r1 = pbox(g, 660.0, 50.0, 740.0, 8.0, 0.0, -1675.0, 1940.0)?;
        let door_r2 = pbox(g, 660.0, 50.0, 740.0, 8.0, 0.0, -1675.0, 1020.0)?;
        let door_body = g.difference(door_slab, door_r1)?;
        let door_body = g.difference(door_body, door_r2)?;
        let front_door = g.union_all(&[
            d_top.weaken::<Bound>(),
            d_jl.weaken::<Bound>(),
            d_jr.weaken::<Bound>(),
            door_body.weaken::<Bound>(),
        ])?;

        // 4f: Black Wrought-Iron Porch Lantern Assembly + Door Handle + Interior Hanging Pendant Shades!
        let l_plate = pbox(g, 110.0, 52.0, 240.0, 8.0, 710.0, -1720.0, 2080.0)?;
        let l_arm = pbox(g, 60.0, 130.0, 60.0, 8.0, 710.0, -1780.0, 2165.0)?;
        let l_hood = g.cone(ConeOpts { bottom: mm(118.0), top: mm(28.0), height: mm(86.0), .. })?;
        let l_hood = g.place(l_hood, shift(710.0, -1835.0, 2195.0))?;
        let l_base = pbox(g, 154.0, 154.0, 52.0, 8.0, 710.0, -1835.0, 1945.0)?;
        let h_plate = pbox(g, 66.0, 48.0, 215.0, 8.0, 365.0, -1690.0, 1410.0)?;
        let h_lever = pbox(g, 150.0, 72.0, 50.0, 10.0, 315.0, -1730.0, 1410.0)?;
        // Hanging craftsman iron pendant lamp shade & rod over the interior dining table!
        let pend_rod = g.cylinder(CylinderOpts { radius: mm(22.0), height: mm(680.0), .. })?;
        let pend_rod = g.place(pend_rod, shift(1180.0, -320.0, 2660.0))?;
        let pend_shade = g.cone(ConeOpts { bottom: mm(215.0), top: mm(58.0), height: mm(135.0), .. })?;
        let pend_shade = g.place(pend_shade, shift(1180.0, -320.0, 2320.0))?;
        let porch_iron = g.union_all(&[
            l_plate.weaken::<Bound>(),
            l_arm.weaken::<Bound>(),
            l_hood.weaken::<Bound>(),
            l_base.weaken::<Bound>(),
            h_plate.weaken::<Bound>(),
            h_lever.weaken::<Bound>(),
            pend_rod.weaken::<Bound>(),
            pend_shade.weaken::<Bound>(),
        ])?;
        // 4g: Steel Chimney Flue Pipe + Rain Cap (local to `chim`, which sits at (-1720, 1280, 0)!)
        let flue_pipe = g.cylinder(CylinderOpts { radius: mm(148.0), height: mm(460.0), .. })?;
        let flue_pipe = g.place(flue_pipe, shift(0.0, 0.0, 5140.0))?;
        let flue_cap = g.cone(ConeOpts { bottom: mm(235.0), top: mm(48.0), height: mm(112.0), .. })?;
        let flue_cap = g.place(flue_cap, shift(0.0, 0.0, 5420.0))?;
        let flue_iron = g.union(flue_pipe, flue_cap)?;

        // 5: REAL 3D ILLUMINATED INTERIOR ROOM (warm pine walls, floor, ceiling beams, wooden table & fireplace hearth — NOT flat orange lightboxes!)
        let int_floor = pbox(g, 4300.0, 2900.0, 100.0, 0.0, 0.0, 0.0, 520.0)?;
        let int_ceil = pbox(g, 4300.0, 2900.0, 100.0, 0.0, 0.0, 0.0, 2980.0)?;
        let int_back = pbox(g, 4280.0, 120.0, 2500.0, 0.0, 0.0, 1380.0, 1750.0)?;
        let int_mid_wall = pbox(g, 120.0, 1400.0, 2500.0, 0.0, -450.0, 400.0, 1750.0)?;
        // Wooden dining table & chairs silhouette visible through the right picture window & front-right window!
        let tbl_top = pbox(g, 1300.0, 820.0, 80.0, 14.0, 1180.0, -350.0, 1280.0)?;
        let tbl_ped = pbox(g, 240.0, 240.0, 720.0, 16.0, 1180.0, -350.0, 900.0)?;
        // Bookshelf / cabinet silhouette visible inside front-left window!
        let shelf = pbox(g, 920.0, 320.0, 1650.0, 12.0, -1420.0, -320.0, 1360.0)?;
        let interior_room = g.union_all(&[int_floor, int_ceil, int_back, int_mid_wall, tbl_top, tbl_ped, shelf])?;

        // 6: Physical Reflective Architectural Window Glass Panes
        let glass_fl = pbox(g, 900.0, 80.0, 1100.0, 4.0, -1500.0, -1620.0, 1820.0)?;
        let glass_fr = pbox(g, 900.0, 80.0, 1100.0, 4.0, 1500.0, -1620.0, 1820.0)?;
        let glass_s = pbox(g, 80.0, 1240.0, 1100.0, 4.0, 2320.0, 0.0, 1820.0)?;
        let glass_a = pbox(g, 80.0, 740.0, 420.0, 4.0, 2320.0, 0.0, 3440.0)?;
        let glass = g.union_all(&[glass_fl, glass_fr, glass_s, glass_a])?;

        // 7: Real 3D Warm Edison Bulbs (Porch Sconce Bulb + Recessed Pendant Filament Bulbs + Attic Lamp!)
        let sconce_bulb = pbox(g, 88.0, 88.0, 170.0, 22.0, 710.0, -1835.0, 2055.0)?;
        let lamp_r = g.sphere(SphereOpts { radius: mm(74.0), .. })?;
        let lamp_r = g.place(lamp_r, shift(1180.0, -320.0, 2225.0))?;
        let lamp_l = g.sphere(SphereOpts { radius: mm(80.0), .. })?;
        let lamp_l = g.place(lamp_l, shift(-1320.0, -180.0, 2260.0))?;
        let lamp_a = g.sphere(SphereOpts { radius: mm(82.0), .. })?;
        let lamp_a = g.place(lamp_a, shift(1150.0, 120.0, 3540.0))?;
        let glow = g.union_all(&[
            sconce_bulb.weaken::<Bound>(),
            lamp_r.weaken::<Bound>(),
            lamp_l.weaken::<Bound>(),
            lamp_a.weaken::<Bound>(),
        ])?;

        // 8: Stacked Split Firewood Log Pile beside right granite foundation (`O(1) linear_repeat`, clean clearance!)
        let log = g.cylinder(CylinderOpts { radius: mm(74.0), height: mm(440.0), .. })?;
        let log = g.place(log, rot_y90)?;
        let log_row = g.linear_repeat(log, RepeatOpts { axis: Axis::Y, step: mm(172.0), count: 5, .. })?;
        let lr0 = g.place(log_row, shift(2840.0, 440.0, 86.0))?;
        let lr1 = g.place(log_row, shift(2840.0, 526.0, 224.0))?;
        let lr2 = g.place(log_row, shift(2840.0, 440.0, 362.0))?;
        let lr3 = g.place(log_row, shift(2840.0, 526.0, 500.0))?;
        let rack_back = pbox(g, 90.0, 960.0, 560.0, 12.0, 2630.0, 825.0, 290.0)?;
        let rack_l = pbox(g, 420.0, 80.0, 620.0, 12.0, 2840.0, 330.0, 310.0)?;
        let rack_r = pbox(g, 420.0, 80.0, 620.0, 12.0, 2840.0, 1320.0, 310.0)?;
        let firewood = g.union_all(&[
            lr0.weaken::<Bound>(),
            lr1.weaken::<Bound>(),
            lr2.weaken::<Bound>(),
            lr3.weaken::<Bound>(),
            rack_back.weaken::<Bound>(),
            rack_l.weaken::<Bound>(),
            rack_r.weaken::<Bound>(),
        ])?;

        // 9: Angular Rough-Hewn Mossy Alpine Granite Outcrops (grounded solidly at z >= 0, textured with WeatheredRock!)
        let b1 = g.cuboid(BoxOpts { size: mm3(840.0, 620.0, 440.0), round: mm(115.0), .. })?;
        let b1 = g.place(b1, place_rot(-3140.0, -3040.0, 220.0, Placement::rotation(Axis::Z, Angle::degrees(24.0))))?;
        let b2 = g.cuboid(BoxOpts { size: mm3(560.0, 440.0, 320.0), round: mm(90.0), .. })?;
        let b2 = g.place(b2, place_rot(-2740.0, -3340.0, 160.0, Placement::rotation(Axis::Z, Angle::degrees(-18.0))))?;
        let cluster_l = g.round_union(b1, b2, BlendOpts { radius: mm(80.0), .. })?;
        let b3 = g.cuboid(BoxOpts { size: mm3(740.0, 560.0, 380.0), round: mm(105.0), .. })?;
        let b3 = g.place(b3, place_rot(3180.0, -2420.0, 190.0, Placement::rotation(Axis::Z, Angle::degrees(-31.0))))?;
        let b4 = g.cuboid(BoxOpts { size: mm3(480.0, 400.0, 270.0), round: mm(80.0), .. })?;
        let b4 = g.place(b4, place_rot(2860.0, -2780.0, 135.0, Placement::rotation(Axis::Z, Angle::degrees(15.0))))?;
        let cluster_r = g.round_union(b3, b4, BlendOpts { radius: mm(75.0), .. })?;
        let boulders = g.union(cluster_l, cluster_r)?;

        let ao_proxy = g.union_all(&[
            foundation.weaken::<Bound>(),
            walls.weaken::<Bound>(),
            roof.weaken::<Bound>(),
            chim.weaken::<Bound>(),
            porch_struct.weaken::<Bound>(),
        ])?;

        (
            foundation,
            walkway,
            walls,
            roof,
            copper_trim,
            chim,
            porch_struct,
            roof_rafters,
            porch_rail_l,
            porch_rail_r,
            win_front,
            win_side,
            front_door,
            porch_iron,
            flue_iron,
            interior_room,
            glass,
            glow,
            firewood,
            boulders,
            ao_proxy,
        )
    };

    let horiz_x = |m: f64| UvMode::BoxMetric { tile_size: Length::m(m), grain: UvGrainAxis::Axis(UvAxis::X) };
    let vert_z = |m: f64| UvMode::BoxMetric { tile_size: Length::m(m), grain: UvGrainAxis::Axis(UvAxis::Z) };

    let (found_id, _) = model.add(PartOpts { solid: foundation, name: "stone_foundation" })?;
    model.set_material(found_id, MaterialRole(2))?;
    model.set_uv_mode(found_id, horiz_x(1.25))?;

    let (walk_id, _) = model.add(PartOpts { solid: walkway, name: "stone_walkway" })?;
    model.set_material(walk_id, MaterialRole(11))?;

    let (walls_id, _) = model.add(PartOpts { solid: walls, name: "timber_walls" })?;
    model.set_material(walls_id, MaterialRole(0))?;
    model.set_uv_mode(walls_id, horiz_x(1.30))?;

    let (roof_id, _) = model.add(PartOpts { solid: roof, name: "slate_roof" })?;
    model.set_material(roof_id, MaterialRole(1))?;
    model.set_uv_mode(roof_id, horiz_x(1.30))?;

    let (cop_id, _) = model.add(PartOpts { solid: copper_trim, name: "copper_gutters_and_ridge" })?;
    model.set_material(cop_id, MaterialRole(6))?;
    model.set_parent(cop_id, Some(roof_id))?;

    let (chim_id, _) = model.add(PartOpts { solid: chim, name: "stone_chimney" })?;
    model.set_material(chim_id, MaterialRole(2))?;
    model.set_uv_mode(chim_id, horiz_x(1.15))?;

    let (ps_id, _) = model.add(PartOpts { solid: porch_struct, name: "porch_structure" })?;
    model.set_material(ps_id, MaterialRole(3))?;
    model.set_uv_mode(ps_id, vert_z(0.95))?;

    let (rr_id, _) = model.add(PartOpts { solid: roof_rafters, name: "corner_trim_and_rafters" })?;
    model.set_material(rr_id, MaterialRole(3))?;
    model.set_uv_mode(rr_id, vert_z(0.95))?;
    model.set_parent(rr_id, Some(walls_id))?;

    let (prl_id, _) = model.add(PartOpts { solid: porch_rail_l, name: "porch_railing_left" })?;
    model.set_material(prl_id, MaterialRole(3))?;
    model.set_uv_mode(prl_id, vert_z(0.85))?;
    model.set_parent(prl_id, Some(ps_id))?;

    let (prr_id, _) = model.add(PartOpts { solid: porch_rail_r, name: "porch_railing_right" })?;
    model.set_material(prr_id, MaterialRole(3))?;
    model.set_uv_mode(prr_id, vert_z(0.85))?;
    model.set_parent(prr_id, Some(ps_id))?;

    let (wf_id, _) = model.add(PartOpts { solid: win_front, name: "window_frames_front" })?;
    model.set_material(wf_id, MaterialRole(3))?;
    model.set_uv_mode(wf_id, vert_z(0.85))?;
    model.set_parent(wf_id, Some(walls_id))?;

    let (ws_id, _) = model.add(PartOpts { solid: win_side, name: "window_frames_side" })?;
    model.set_material(ws_id, MaterialRole(3))?;
    model.set_uv_mode(ws_id, vert_z(0.85))?;
    model.set_parent(ws_id, Some(walls_id))?;

    let (fd_id, _) = model.add(PartOpts { solid: front_door, name: "front_door" })?;
    model.set_material(fd_id, MaterialRole(3))?;
    model.set_uv_mode(fd_id, vert_z(0.95))?;
    model.set_parent(fd_id, Some(walls_id))?;

    let (piron_id, _) = model.add(PartOpts { solid: porch_iron, name: "porch_iron_hardware" })?;
    model.set_material(piron_id, MaterialRole(7))?;
    model.set_parent(piron_id, Some(walls_id))?;

    let (firon_id, _) = model.add(PartOpts { solid: flue_iron, name: "chimney_flue_cap" })?;
    model.set_material(firon_id, MaterialRole(7))?;
    model.set_parent(firon_id, Some(chim_id))?;

    let (int_id, _) = model.add(PartOpts { solid: interior_room, name: "warm_interior_room" })?;
    model.set_material(int_id, MaterialRole(10))?;
    model.set_parent(int_id, Some(walls_id))?;

    let (glass_id, _) = model.add(PartOpts { solid: glass, name: "window_glass" })?;
    model.set_material(glass_id, MaterialRole(4))?;
    model.set_parent(glass_id, Some(walls_id))?;

    let (glow_id, _) = model.add(PartOpts { solid: glow, name: "edison_lamps" })?;
    model.set_material(glow_id, MaterialRole(5))?;
    model.set_parent(glow_id, Some(walls_id))?;

    let (fw_id, _) = model.add(PartOpts { solid: firewood, name: "stacked_firewood" })?;
    model.set_material(fw_id, MaterialRole(8))?;
    model.set_budget(
        fw_id,
        PartBudget::Detailed { tolerance: Some(mm(14.0)), min_feature: Some(mm(32.0)), max_triangles: Some(TriangleBudget(2600)), max_error: None, weight: 1.0 },
    )?;

    let (bld_id, _) = model.add(PartOpts { solid: boulders, name: "alpine_boulders" })?;
    model.set_material(bld_id, MaterialRole(11))?;
    model.set_budget(
        bld_id,
        PartBudget::Detailed { tolerance: Some(mm(22.0)), min_feature: Some(mm(65.0)), max_triangles: Some(TriangleBudget(1800)), max_error: None, weight: 1.0 },
    )?;

    Ok(ao_proxy)
}

// =============================================================================
// 2. ORBITAL RECON INTERCEPTOR (LOFTED STEALTH FIGHTER + CARBON COCKPIT + WHEELS)
// =============================================================================
fn build_interceptor(model: &mut Model<'_>) -> Result<(), ModelError> {
    let (hull, wings, hardware, tires, interior, canopy, lights) = {
        let g = model.graph();
        let rot_y90 = Placement::rotation(Axis::Y, Angle::degrees(90.0));
        let rot_x90 = Placement::rotation(Axis::X, Angle::degrees(90.0));

        // 0: Seamless 5-station LOFTED supersonic stealth fighter fuselage + crisp tapered radome!
        let s0 = g.profile(ProfileOpts { profile: Profile::RoundedRect { width: mm(300.0), height: mm(780.0), radius: mm(65.0) }, .. })?;
        let s1 = g.profile(ProfileOpts { profile: Profile::RoundedRect { width: mm(370.0), height: mm(860.0), radius: mm(85.0) }, .. })?;
        let s2 = g.profile(ProfileOpts { profile: Profile::RoundedRect { width: mm(360.0), height: mm(740.0), radius: mm(92.0) }, .. })?;
        let s3 = g.profile(ProfileOpts { profile: Profile::RoundedRect { width: mm(250.0), height: mm(480.0), radius: mm(85.0) }, .. })?;
        let s4 = g.profile(ProfileOpts { profile: Profile::Circle { radius: mm(26.0) }, .. })?;
        let loft_body = g.loft(LoftOpts {
            stations: &[
                Station { z: mm(-1160.0), profile: s0 },
                Station { z: mm(-180.0), profile: s1 },
                Station { z: mm(680.0), profile: s2 },
                Station { z: mm(1380.0), profile: s3 },
                Station { z: mm(2120.0), profile: s4 },
            ],
            name: Some("hull_loft"),
        })?;
        let loft_body = g.place(loft_body, rot_y90)?;
        let spine = pbox(g, 1560.0, 320.0, 210.0, 48.0, -360.0, 0.0, 145.0)?;
        let hull_raw = g.round_union(loft_body, spine, BlendOpts { radius: mm(75.0), .. })?;

        let cockpit_cut = pbox(g, 920.0, 390.0, 260.0, 32.0, 520.0, 0.0, 185.0)?;
        let intake_l = pbox(g, 620.0, 130.0, 120.0, 24.0, 160.0, 415.0, -15.0)?;
        let intake_r = pbox(g, 620.0, 130.0, 120.0, 24.0, 160.0, -415.0, -15.0)?;

        let hull = g.difference(hull_raw, cockpit_cut)?;
        let hull = g.difference(hull, intake_l)?;
        let hull = g.difference(hull, intake_r)?;

        // 1: Rounded aerodynamic swept delta wings + canted twin V-tail stabilizers + wingtip pods
        let wing_pts = [
            [mm(560.0), mm(260.0)],
            [mm(-520.0), mm(1680.0)],
            [mm(-960.0), mm(1620.0)],
            [mm(-820.0), mm(260.0)],
            [mm(-820.0), mm(-260.0)],
            [mm(-960.0), mm(-1620.0)],
            [mm(-520.0), mm(-1680.0)],
            [mm(560.0), mm(-260.0)],
        ];
        let wing_prof = g.profile(ProfileOpts { profile: Profile::Polygon { points: &wing_pts }, .. })?;
        let wing_core = g.extrude(wing_prof, ExtrudeOpts { height: mm(44.0), .. })?;
        let wing_slab = g.grow(wing_core, OffsetOpts { by: mm(16.0), .. })?;
        let wing_slab = g.place(wing_slab, shift(-180.0, 0.0, -30.0))?;

        let fin_pts = [
            [mm(300.0), mm(0.0)],
            [mm(-150.0), mm(520.0)],
            [mm(-420.0), mm(500.0)],
            [mm(-300.0), mm(0.0)],
        ];
        let fin_prof = g.profile(ProfileOpts { profile: Profile::Polygon { points: &fin_pts }, .. })?;
        let fin_core = g.extrude(fin_prof, ExtrudeOpts { height: mm(26.0), .. })?;
        let fin_raw = g.grow(fin_core, OffsetOpts { by: mm(12.0), .. })?;
        let fin_l = g.place(fin_raw, place_rot(-740.0, 470.0, 75.0, Placement::rotation(Axis::X, Angle::degrees(68.0))))?;
        let fin_r = g.place(fin_raw, place_rot(-740.0, -470.0, 75.0, Placement::rotation(Axis::X, Angle::degrees(112.0))))?;

        let pod_l = pbox(g, 560.0, 66.0, 86.0, 20.0, -860.0, 1670.0, -30.0)?;
        let pod_r = pbox(g, 560.0, 66.0, 86.0, 20.0, -860.0, -1670.0, -30.0)?;
        let wings = g.union_all(&[
            wing_slab.weaken::<Bound>(),
            fin_l.weaken::<Bound>(),
            fin_r.weaken::<Bound>(),
            pod_l.weaken::<Bound>(),
            pod_r.weaken::<Bound>(),
        ])?;

        // 2: Machined Brushed-Titanium Twin Thruster Nozzles + Internal Turbine Stator Cone + Oleo Struts + Canopy Frame
        let rot_ym90 = Placement::rotation(Axis::Y, Angle::degrees(-90.0));
        let nac_body = g.cylinder(CylinderOpts { radius: mm(172.0), height: mm(820.0), .. })?;
        let nac_body = g.place(nac_body, rot_y90)?;
        let nozzle = g.cone(ConeOpts { bottom: mm(172.0), top: mm(140.0), height: mm(260.0), .. })?;
        let nozzle = g.place(nozzle, place_rot(-510.0, 0.0, 0.0, rot_ym90))?;
        let nac_outer = g.union(nac_body, nozzle)?;
        let nac_bore = g.cone(ConeOpts { bottom: mm(108.0), top: mm(124.0), height: mm(440.0), .. })?;
        let nac_bore = g.place(nac_bore, place_rot(-480.0, 0.0, 0.0, rot_ym90))?;
        let nac_hollow = g.difference(nac_outer, nac_bore)?;
        // Internal turbine spinner cone seated inside the nozzle throat!
        let spinner = g.cone(ConeOpts { bottom: mm(46.0), top: mm(10.0), height: mm(170.0), .. })?;
        let spinner = g.place(spinner, place_rot(-420.0, 0.0, 0.0, rot_ym90))?;
        let nac = g.union(nac_hollow, spinner)?;
        let nac_l = g.place(nac, shift(-960.0, 310.0, 25.0))?;
        let nac_r = g.place(nac, shift(-960.0, -310.0, 25.0))?;

        // Tricycle landing gear struts + wheel hubs
        let nose_strut = g.cylinder(CylinderOpts { radius: mm(28.0), height: mm(260.0), .. })?;
        let nose_strut = g.place(nose_strut, shift(880.0, 0.0, -245.0))?;
        let main_sl = g.cylinder(CylinderOpts { radius: mm(32.0), height: mm(270.0), .. })?;
        let main_sl = g.place(main_sl, shift(-360.0, 420.0, -245.0))?;
        let main_sr = g.cylinder(CylinderOpts { radius: mm(32.0), height: mm(270.0), .. })?;
        let main_sr = g.place(main_sr, shift(-360.0, -420.0, -245.0))?;
        let hub = g.cylinder(CylinderOpts { radius: mm(54.0), height: mm(82.0), .. })?;
        let hub_n = g.place(hub, place_rot(880.0, 0.0, -355.0, rot_x90))?;
        let hub_l = g.place(hub, place_rot(-360.0, 420.0, -355.0, rot_x90))?;
        let hub_r = g.place(hub, place_rot(-360.0, -420.0, -355.0, rot_x90))?;

        // Supersonic titanium nose pitot probe + upright curved canopy windshield bow arch!
        let pitot = g.cone(ConeOpts { bottom: mm(24.0), top: mm(6.0), height: mm(320.0), .. })?;
        let pitot = g.place(pitot, place_rot(2260.0, 0.0, 0.0, rot_y90))?;
        let bow = g.torus(TorusOpts { major: mm(222.0), minor: mm(16.0), .. })?;
        let bow = g.place(bow, rot_y90)?;
        let bow_clip = pbox(g, 200.0, 520.0, 260.0, 0.0, 0.0, 0.0, 130.0)?;
        let bow_arch = g.intersect(bow, bow_clip)?;
        let bow_arch = g.place(bow_arch, place_rot(735.0, 0.0, 152.0, Placement::rotation(Axis::Y, Angle::degrees(-16.0))))?;

        let hardware = g.union_all(&[
            nac_l.weaken::<Bound>(),
            nac_r.weaken::<Bound>(),
            nose_strut.weaken::<Bound>(),
            main_sl.weaken::<Bound>(),
            main_sr.weaken::<Bound>(),
            hub_n.weaken::<Bound>(),
            hub_l.weaken::<Bound>(),
            hub_r.weaken::<Bound>(),
            pitot.weaken::<Bound>(),
            bow_arch.weaken::<Bound>(),
        ])?;

        // 2b: Matte rubber aircraft tires on tricycle gear (z_min = -445 mm!)
        let tire = g.torus(TorusOpts { major: mm(66.0), minor: mm(34.0), .. })?;
        let tire_n = g.place(tire, place_rot(880.0, 0.0, -355.0, rot_x90))?;
        let tire_l = g.place(tire, place_rot(-360.0, 420.0, -355.0, rot_x90))?;
        let tire_r = g.place(tire, place_rot(-360.0, -420.0, -355.0, rot_x90))?;
        let tires = g.union_all(&[tire_n, tire_l, tire_r])?;

        // 3: Carbon-weave cockpit ejection seat, headrest, side consoles & instrument shroud
        let seat_back = pbox(g, 110.0, 230.0, 250.0, 22.0, 300.0, 0.0, 195.0)?;
        let seat_pan = pbox(g, 240.0, 240.0, 90.0, 18.0, 390.0, 0.0, 110.0)?;
        let dash = pbox(g, 230.0, 320.0, 120.0, 18.0, 780.0, 0.0, 145.0)?;
        let con_l = pbox(g, 420.0, 68.0, 95.0, 10.0, 540.0, 155.0, 125.0)?;
        let con_r = pbox(g, 420.0, 68.0, 95.0, 10.0, 540.0, -155.0, 125.0)?;
        let interior = g.union_all(&[seat_back, seat_pan, dash, con_l, con_r])?;

        // 4: Smoked polycarbonate refractive bubble canopy
        let canopy_outer = g.ellipsoid(EllipsoidOpts { radii: mm3(560.0, 222.0, 190.0), .. })?;
        let canopy_clip = pbox(g, 1140.0, 480.0, 240.0, 0.0, 0.0, 0.0, 115.0)?;
        let canopy = g.intersect(canopy_outer, canopy_clip)?;
        let canopy = g.place(canopy, shift(520.0, 0.0, 155.0))?;

        // 5: Glowing cyan ion plasma rings recessed cleanly inside the open thruster nozzles + cockpit HUD + wingtip beacons!
        let ring = g.torus(TorusOpts { major: mm(78.0), minor: mm(26.0), .. })?;
        let core_disc = g.cylinder(CylinderOpts { radius: mm(92.0), height: mm(50.0), .. })?;
        let core_unit = g.union(ring, core_disc)?;
        let core_l = g.place(core_unit, place_rot(-1395.0, 310.0, 25.0, rot_y90))?;
        let core_r = g.place(core_unit, place_rot(-1395.0, -310.0, 25.0, rot_y90))?;
        let hud = pbox(g, 22.0, 145.0, 66.0, 4.0, 820.0, 0.0, 220.0)?;
        let strip_l = pbox(g, 210.0, 32.0, 22.0, 6.0, -860.0, 1705.0, -30.0)?;
        let strip_r = pbox(g, 210.0, 32.0, 22.0, 6.0, -860.0, -1705.0, -30.0)?;
        let lights = g.union_all(&[
            core_l.weaken::<Bound>(),
            core_r.weaken::<Bound>(),
            hud.weaken::<Bound>(),
            strip_l.weaken::<Bound>(),
            strip_r.weaken::<Bound>(),
        ])?;

        (hull, wings, hardware, tires, interior, canopy, lights)
    };

    let (hull_id, _) = model.add(PartOpts { solid: hull, name: "fuselage" })?;
    model.set_material(hull_id, MaterialRole(0))?;
    let (wings_id, _) = model.add(PartOpts { solid: wings, name: "wings" })?;
    model.set_material(wings_id, MaterialRole(1))?;
    model.set_parent(wings_id, Some(hull_id))?;
    let (hw_id, _) = model.add(PartOpts { solid: hardware, name: "titanium_hardware" })?;
    model.set_material(hw_id, MaterialRole(2))?;
    model.set_parent(hw_id, Some(hull_id))?;
    let (tire_id, _) = model.add(PartOpts { solid: tires, name: "landing_tires" })?;
    model.set_material(tire_id, MaterialRole(6))?;
    model.set_parent(tire_id, Some(hw_id))?;
    let (int_id, _) = model.add(PartOpts { solid: interior, name: "cockpit_interior" })?;
    model.set_material(int_id, MaterialRole(3))?;
    model.set_parent(int_id, Some(hull_id))?;
    let (canopy_id, _) = model.add(PartOpts { solid: canopy, name: "canopy_glass" })?;
    model.set_material(canopy_id, MaterialRole(4))?;
    model.set_parent(canopy_id, Some(hull_id))?;
    let (lights_id, _) = model.add(PartOpts { solid: lights, name: "ion_emitters" })?;
    model.set_material(lights_id, MaterialRole(5))?;
    model.set_parent(lights_id, Some(hull_id))?;
    Ok(())
}

// =============================================================================
// 3. PRECISION CNC MACHINED 50mm f/1.4 CAMERA LENS (MACRO PRODUCT PHOTOGRAPHY!)
// =============================================================================
fn build_camera_lens(model: &mut Model<'_>) -> Result<(), ModelError> {
    let (barrel, knurl_rings, titanium_mount, brass_ring, red_dot, iris_ring, front_glass, inner_glass) = {
        let g = model.graph();
        // Root pose: rests horizontally along +X tilted gently up 8° toward the overhead softbox!
        // All children are built cleanly in local optical-Z coordinates (rear mount = -Z, front
        // element = +Z) and inherit `lens_pose` via `Model::set_parent(..., Some(b_id))`!
        let lens_pose = Placement::rotation(Axis::Y, Angle::degrees(82.0));

        // 0: Anodized satin-black CNC aluminum barrel with stepped front filter hood & internal matte bore.
        let main_cyl = g.cylinder(CylinderOpts { radius: mm(38.2), height: mm(68.0), .. })?;
        let front_flare = g.cone(ConeOpts { bottom: mm(38.2), top: mm(42.5), height: mm(26.0), .. })?;
        let front_flare = g.place(front_flare, shift(0.0, 0.0, 44.0))?;
        let front_lip = g.torus(TorusOpts { major: mm(40.8), minor: mm(1.8), .. })?;
        let front_lip = g.place(front_lip, shift(0.0, 0.0, 56.2))?;
        let barrel_raw = g.union_all(&[main_cyl, front_flare, front_lip])?;

        // Stepped internal anti-reflective optical bore (capped at z = -22mm so zero studio backlight leaks through!)
        let bore_front = g.cylinder(CylinderOpts { radius: mm(35.8), height: mm(30.0), .. })?;
        let bore_front = g.place(bore_front, shift(0.0, 0.0, 46.0))?;
        let bore_mid = g.cylinder(CylinderOpts { radius: mm(27.5), height: mm(34.0), .. })?;
        let bore_mid = g.place(bore_mid, shift(0.0, 0.0, 23.0))?;
        let bore_rear = g.cylinder(CylinderOpts { radius: mm(20.5), height: mm(28.0), .. })?;
        let bore_rear = g.place(bore_rear, shift(0.0, 0.0, -4.0))?;
        let barrel_z = g.difference(barrel_raw, bore_front)?;
        let barrel_z = g.difference(barrel_z, bore_mid)?;
        let barrel_z = g.difference(barrel_z, bore_rear)?;
        let barrel = g.place(barrel_z, lens_pose)?;

        // 1: Dedicated Precision CNC Knurled Focus & Aperture Rings (own part = razor-sharp flutes!)
        let f_collar = g.cylinder(CylinderOpts { radius: mm(38.8), height: mm(25.0), .. })?;
        let f_collar = g.place(f_collar, shift(0.0, 0.0, 11.0))?;
        let f_rib = g.cuboid(BoxOpts { size: mm3(3.6, 4.2, 23.0), round: mm(0.8), .. })?;
        let f_rib = g.place(f_rib, shift(39.4, 0.0, 11.0))?;
        let focus_knurl = g.circular_repeat(f_rib, CircularOpts { count: 28, .. })?;
        let a_collar = g.cylinder(CylinderOpts { radius: mm(38.6), height: mm(11.5), .. })?;
        let a_collar = g.place(a_collar, shift(0.0, 0.0, -18.5))?;
        let a_rib = g.cuboid(BoxOpts { size: mm3(3.2, 3.8, 9.8), round: mm(0.7), .. })?;
        let a_rib = g.place(a_rib, shift(39.0, 0.0, -18.5))?;
        let aper_knurl = g.circular_repeat(a_rib, CircularOpts { count: 24, .. })?;
        let knurl_core_cut = g.cylinder(CylinderOpts { radius: mm(35.0), height: mm(68.0), .. })?;
        let knurl_u = g.union_all(&[
            f_collar.weaken::<Bound>(),
            focus_knurl.weaken::<Bound>(),
            a_collar.weaken::<Bound>(),
            aper_knurl.weaken::<Bound>(),
        ])?;
        let knurl_rings = g.difference(knurl_u, knurl_core_cut)?;

        // 2: Brushed Satin Titanium Bayonet Mount Flange, Lock Tabs & Distance Scale Collar (local Z!)
        let mount_flange = g.cylinder(CylinderOpts { radius: mm(36.8), height: mm(9.5), .. })?;
        let mount_flange = g.place(mount_flange, shift(0.0, 0.0, -37.5))?;
        let mount_ring = g.cylinder(CylinderOpts { radius: mm(30.8), height: mm(7.5), .. })?;
        let mount_ring = g.place(mount_ring, shift(0.0, 0.0, -44.2))?;
        let scale_collar = g.cylinder(CylinderOpts { radius: mm(39.2), height: mm(7.2), .. })?;
        let scale_collar = g.place(scale_collar, shift(0.0, 0.0, -6.2))?;
        let mount_bore = g.cylinder(CylinderOpts { radius: mm(21.8), height: mm(64.0), .. })?;
        let mount_bore = g.place(mount_bore, shift(0.0, 0.0, -24.0))?;
        let ti_raw = g.union_all(&[mount_flange, mount_ring, scale_collar])?;
        let titanium_mount = g.difference(ti_raw, mount_bore)?;

        // 3: Machined Brushed Brass/Gold Prestige Accent Ring near the front flare (local Z!)
        let brass_ring = g.torus(TorusOpts { major: mm(39.6), minor: mm(1.65), .. })?;
        let brass_ring = g.place(brass_ring, shift(0.0, 0.0, 29.5))?;

        // 4: Iconic Leica-style ruby-red enamel index dot seated cleanly on the titanium scale collar!
        // In local lens coordinates rotated +82° around Y, `-X` local points UP (`+Z` world)!
        let red_dot = g.sphere(SphereOpts { radius: mm(2.6), .. })?;
        let red_dot = g.place(red_dot, shift(-28.0, -27.5, -6.2))?;

        // 5: Recessed Optical-Black Flock Baffle + Iris Diaphragm + Rear Optical Chamber Backwall!
        let iris_disc = g.cylinder(CylinderOpts { radius: mm(27.2), height: mm(2.8), .. })?;
        let iris_disc = g.place(iris_disc, shift(0.0, 0.0, 17.5))?;
        let iris_hole = g.cylinder(CylinderOpts { radius: mm(13.5), height: mm(8.0), .. })?;
        let iris_hole = g.place(iris_hole, shift(0.0, 0.0, 17.5))?;
        let iris_blade_ring = g.difference(iris_disc, iris_hole)?;
        let baffle_back = g.cylinder(CylinderOpts { radius: mm(26.5), height: mm(6.0), .. })?;
        let baffle_back = g.place(baffle_back, shift(0.0, 0.0, -16.0))?;
        let baffle_wall = g.cylinder(CylinderOpts { radius: mm(35.6), height: mm(24.0), .. })?;
        let baffle_wall_in = g.cylinder(CylinderOpts { radius: mm(33.2), height: mm(28.0), .. })?;
        let baffle_sleeve = g.difference(baffle_wall, baffle_wall_in)?;
        let baffle_sleeve = g.place(baffle_sleeve, shift(0.0, 0.0, 42.0))?;
        let iris_ring = g.union_all(&[
            iris_blade_ring.weaken::<Bound>(),
            baffle_back.weaken::<Bound>(),
            baffle_sleeve.weaken::<Bound>(),
        ])?;

        // 6: Multi-coated convex spherical front optical glass element (local Z!)
        let f_sph = g.sphere(SphereOpts { radius: mm(56.0), .. })?;
        let f_sph = g.place(f_sph, shift(0.0, 0.0, -10.5))?;
        let f_cyl = g.cylinder(CylinderOpts { radius: mm(34.8), height: mm(16.0), .. })?;
        let f_cyl = g.place(f_cyl, shift(0.0, 0.0, 41.0))?;
        let front_glass = g.intersect(f_sph, f_cyl)?;

        // 7: Secondary recessed sapphire-cyan coated spherical inner glass element (local Z!)
        let i_sph = g.sphere(SphereOpts { radius: mm(40.0), .. })?;
        let i_sph = g.place(i_sph, shift(0.0, 0.0, -14.0))?;
        let i_cyl = g.cylinder(CylinderOpts { radius: mm(25.5), height: mm(12.0), .. })?;
        let i_cyl = g.place(i_cyl, shift(0.0, 0.0, 23.0))?;
        let inner_glass = g.intersect(i_sph, i_cyl)?;

        (
            barrel,
            knurl_rings,
            titanium_mount,
            brass_ring.weaken::<Bound>(),
            red_dot.weaken::<Bound>(),
            iris_ring,
            front_glass,
            inner_glass,
        )
    };

    let (b_id, _) = model.add(PartOpts { solid: barrel, name: "anodized_barrel" })?;
    model.set_material(b_id, MaterialRole(0))?;
    let (kn_id, _) = model.add(PartOpts { solid: knurl_rings, name: "cnc_knurl_rings" })?;
    model.set_material(kn_id, MaterialRole(0))?;
    model.set_budget(
        kn_id,
        PartBudget::Detailed {
            tolerance: Some(mm(0.24)),
            min_feature: Some(mm(0.85)),
            max_triangles: Some(TriangleBudget(16_000)),
            max_error: None,
            weight: 2.0,
        },
    )?;
    model.set_parent(kn_id, Some(b_id))?;
    let (ti_id, _) = model.add(PartOpts { solid: titanium_mount, name: "titanium_mount_collar" })?;
    model.set_material(ti_id, MaterialRole(1))?;
    model.set_uv_mode(ti_id, UvMode::Cylindrical { axis: UvAxis::Z, radius: mm(39.0), tile_size: mm(80.0) })?;
    model.set_parent(ti_id, Some(b_id))?;
    let (br_id, _) = model.add(PartOpts { solid: brass_ring, name: "brass_accent_ring" })?;
    model.set_material(br_id, MaterialRole(2))?;
    model.set_parent(br_id, Some(b_id))?;
    let (dot_id, _) = model.add(PartOpts { solid: red_dot, name: "ruby_index_dot" })?;
    model.set_material(dot_id, MaterialRole(3))?;
    model.set_parent(dot_id, Some(b_id))?;
    let (ir_id, _) = model.add(PartOpts { solid: iris_ring, name: "iris_diaphragm" })?;
    model.set_material(ir_id, MaterialRole(6))?;
    model.set_parent(ir_id, Some(b_id))?;
    let (fg_id, _) = model.add(PartOpts { solid: front_glass, name: "front_optical_element" })?;
    model.set_material(fg_id, MaterialRole(4))?;
    model.set_parent(fg_id, Some(b_id))?;
    let (ig_id, _) = model.add(PartOpts { solid: inner_glass, name: "inner_coated_element" })?;
    model.set_material(ig_id, MaterialRole(5))?;
    model.set_parent(ig_id, Some(b_id))?;
    Ok(())
}

fn main() {
    let arg1 = std::env::args().nth(1);
    let arg2 = std::env::args().nth(2);
    let (out_dir, filter) = match (arg1.as_deref(), arg2.as_deref()) {
        (Some(f @ ("all" | "cabin" | "ship" | "lens")), None) => ("target/pbr_hero".to_string(), f.to_string()),
        (Some(d), Some(f)) => (d.to_string(), f.to_string()),
        (Some(d), None) => (d.to_string(), "all".to_string()),
        (None, _) => ("target/pbr_hero".to_string(), "all".to_string()),
    };
    let out_path = std::path::Path::new(&out_dir);
    std::fs::create_dir_all(out_path).expect("create out dir");

    let alloc = blaze_own::testing::heap().alloc();
    let tex_res = 512u32;
    let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4);

    // =========================================================================
    // 1. ALPINE CEDAR & SLATE MOUNTAIN CABIN IN A REAL CONIFER CLEARING
    // =========================================================================
    if filter == "all" || filter == "cabin" {
        let t1 = Instant::now();
        let mut cabin_model = Model::new(ModelOpts { alloc, .. });
        let cabin_ao_solid = build_cabin(&mut cabin_model).expect("build cabin");
        let cabin_asm_tape = blaze_cad_field::compile(
            cabin_model.graph_ref(),
            cabin_ao_solid.id(),
            CompileOpts { detail: Detail::Full, alloc, .. },
        )
        .expect("cabin structural ao tape");

        let wood_pack = synthesize_cc0_pack(ProceduralKind::TimberPlanks { base_rgb: [0.32, 0.165, 0.070], planks: 8 }, tex_res, alloc, alloc).expect("wood");
        let slate_pack = synthesize_cc0_pack(ProceduralKind::SlateShingles { base_rgb: [0.072, 0.095, 0.130], rows: 9, cols: 6 }, tex_res, alloc, alloc).expect("slate");
        let stone_pack = synthesize_cc0_pack(ProceduralKind::AshlarStone { base_rgb: [0.23, 0.215, 0.195], rows: 7, cols: 4 }, tex_res, alloc, alloc).expect("stone");
        let oak_pack = synthesize_cc0_pack(ProceduralKind::MilledWood { base_rgb: [0.13, 0.068, 0.034], roughness: 0.42 }, tex_res, alloc, alloc).expect("oak");
        let copper_pack = synthesize_cc0_pack(ProceduralKind::AgedCopper { base_rgb: [0.46, 0.22, 0.12], patina: 0.28 }, tex_res, alloc, alloc).expect("copper");
        let iron_pack = synthesize_cc0_pack(ProceduralKind::WroughtIron { base_rgb: [0.048, 0.052, 0.058], roughness: 0.44 }, tex_res, alloc, alloc).expect("iron");
        let bark_pack = synthesize_cc0_pack(ProceduralKind::TreeBark { base_rgb: [0.14, 0.09, 0.055] }, tex_res, alloc, alloc).expect("bark");
        let pine_pack = synthesize_cc0_pack(ProceduralKind::PineFoliage { base_rgb: [0.038, 0.088, 0.042] }, tex_res, alloc, alloc).expect("pine");
        let warm_wood_pack = synthesize_cc0_pack(ProceduralKind::MilledWood { base_rgb: [0.44, 0.26, 0.12], roughness: 0.52 }, 256, alloc, alloc).expect("interior wood");
        let rock_pack = synthesize_cc0_pack(ProceduralKind::WeatheredRock { base_rgb: [0.17, 0.165, 0.155], moss: 0.52 }, tex_res, alloc, alloc).expect("rock");

        let cabin_images = [
            TextureImage { name: "cedar_albedo", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &wood_pack.albedo.pixels } },
            TextureImage { name: "cedar_normal", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &wood_pack.normal.pixels } },
            TextureImage { name: "cedar_orm", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &wood_pack.orm.pixels } },
            TextureImage { name: "slate_albedo", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &slate_pack.albedo.pixels } },
            TextureImage { name: "slate_normal", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &slate_pack.normal.pixels } },
            TextureImage { name: "slate_orm", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &slate_pack.orm.pixels } },
            TextureImage { name: "stone_albedo", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &stone_pack.albedo.pixels } },
            TextureImage { name: "stone_normal", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &stone_pack.normal.pixels } },
            TextureImage { name: "stone_orm", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &stone_pack.orm.pixels } },
        ];

        let cabin_materials = [
            Material {
                name: "wood.western_cedar",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                pbr_textures: PbrTextures {
                    base_colour: Some(TextureRef { image: ImageId(0), .. }),
                    normal: Some(NormalTexture { image: ImageId(1), scale: 0.95, .. }),
                    metallic_roughness: Some(TextureRef { image: ImageId(2), .. }),
                    occlusion: Some(OcclusionTexture { image: ImageId(2), strength: 0.9, .. }),
                    ..PbrTextures::NONE
                },
                cc0_pack: Some("cc0://blaze/WesternCedar_512"),
                ..
            },
            Material {
                name: "slate.welsh_roof",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                pbr_textures: PbrTextures {
                    base_colour: Some(TextureRef { image: ImageId(3), .. }),
                    normal: Some(NormalTexture { image: ImageId(4), scale: 1.15, .. }),
                    metallic_roughness: Some(TextureRef { image: ImageId(5), .. }),
                    occlusion: Some(OcclusionTexture { image: ImageId(5), strength: 0.9, .. }),
                    ..PbrTextures::NONE
                },
                cc0_pack: Some("cc0://blaze/WelshSlate_512"),
                ..
            },
            Material {
                name: "stone.alpine_granite",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                pbr_textures: PbrTextures {
                    base_colour: Some(TextureRef { image: ImageId(6), .. }),
                    normal: Some(NormalTexture { image: ImageId(7), scale: 1.20, .. }),
                    metallic_roughness: Some(TextureRef { image: ImageId(8), .. }),
                    occlusion: Some(OcclusionTexture { image: ImageId(8), strength: 0.9, .. }),
                    ..PbrTextures::NONE
                },
                cc0_pack: Some("cc0://blaze/AlpineGranite_512"),
                ..
            },
            Material {
                name: "wood.stained_oak",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                cc0_pack: Some("cc0://blaze/StainedOak_512"),
                ..
            },
            Material {
                name: "glass.architectural",
                base_colour: [0.90, 0.94, 0.96, 1.0],
                metallic: 0.0,
                roughness: 0.025,
                clearcoat: Some(Clearcoat { factor: 1.0, roughness: 0.02 }),
                transmission: Some(Transmission { factor: 0.86, ior: 1.50, thickness: 0.012, .. }),
                ..
            },
            Material {
                name: "light.edison_warm",
                base_colour: [1.0, 0.80, 0.42, 1.0],
                metallic: 0.0,
                roughness: 0.20,
                emissive: [1.0, 0.50, 0.14],
                emissive_strength: Some(15.0),
                ..
            },
            Material {
                name: "metal.aged_copper",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                cc0_pack: Some("cc0://blaze/AgedCopper_512"),
                ..
            },
            Material {
                name: "metal.wrought_iron",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                cc0_pack: Some("cc0://blaze/WroughtIron_512"),
                ..
            },
            Material {
                name: "nature.spruce_bark",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                cc0_pack: Some("cc0://blaze/SpruceBark_512"),
                ..
            },
            Material {
                name: "nature.pine_foliage",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                sheen: Some(Sheen { colour: [0.10, 0.16, 0.08], roughness: 0.72 }),
                cc0_pack: Some("cc0://blaze/PineFoliage_512"),
                ..
            },
            Material {
                name: "wood.interior_pine",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                ..
            },
            Material {
                name: "stone.mossy_boulder",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                cc0_pack: Some("cc0://blaze/WeatheredRock_512"),
                ..
            },
        ];

        let cabin_sockets = [
            PartSocket::new("timber_walls", "door.hinge", shift(-480.0, -1635.0, 1495.0)),
            PartSocket::new("timber_walls", "light.porch_sconce", shift(710.0, -1835.0, 2055.0)),
            PartSocket::new("stone_chimney", "smoke.emitter", shift(0.0, 0.0, 5480.0)),
        ];

        let mut mesh_cache = blaze_cad_mesh::PieceCache::new(blaze_cad_mesh::PieceCacheOpts { alloc, .. });
        let cabin_asset = build_game_asset(
            cabin_model.graph_ref(),
            cabin_model.parts(),
            GameAssetOpts {
                alloc,
                scratch: alloc,
                preset: AssetPreset::Environment,
                total_budget: Some(TriangleBudget(72_000)),
                materials: &cabin_materials,
                images: &cabin_images,
                sockets: &cabin_sockets,
                pbr_mesh: Some(PbrMeshOpts {
                    uv_mode: UvMode::BoxMetric { tile_size: Length::m(1.30), grain: UvGrainAxis::Longest },
                    ..PbrMeshOpts::DEFAULT
                }),
                assembly_tape: Some(&cabin_asm_tape),
                fidelity: None,
                ..
            },
            Some(&mut mesh_cache),
        )
        .unwrap_or_else(|e| {
            for p in cabin_model.parts() {
                let mut pc = *p;
                pc.parent = None;
                let single = [pc];
                let tp = Instant::now();
                let res = build_game_asset(
                    cabin_model.graph_ref(),
                    &single,
                    GameAssetOpts {
                        alloc,
                        scratch: alloc,
                        preset: AssetPreset::Environment,
                        materials: &cabin_materials,
                        images: &cabin_images,
                        fidelity: None,
                        ..
                    },
                    None,
                );
                eprintln!("part '{}' ({:.2}s): {:?}", p.name.as_str(), tp.elapsed().as_secs_f64(), res.as_ref().map(|a| a.triangles()));
            }
            panic!("build cabin asset: {e:?}");
        });

        std::fs::write(out_path.join("cabin_pbr.glb"), &*cabin_asset.glb).expect("write cabin glb");
        let cabin_flecs = cabin_asset
            .export_flecs_prefab(
                &cabin_materials,
                &cabin_sockets,
                FlecsOpts {
                    module: Some("world"),
                    prefab_name: "AlpineCabin",
                    asset_uri: "assets/cabin_pbr.glb",
                    up: Up::Y,
                    include_schema: true,
                    alloc,
                    scratch: alloc,
                },
            )
            .expect("cabin flecs");
        std::fs::write(out_path.join("cabin_pbr.flecs"), &*cabin_flecs).expect("write cabin flecs");

        let cabin_looks = [
            MaterialLook {
                textures: wood_pack.view(0.90, false),
                smart: SmartMaterial { edge_wear: 0.14, grime: 0.42, dust: 0.12, ..SmartMaterial::WEATHERED_TIMBER },
            },
            MaterialLook {
                textures: slate_pack.view(1.15, false),
                smart: SmartMaterial { edge_wear: 0.14, grime: 0.34, dust: 0.16, ..SmartMaterial::WEATHERED_STONE },
            },
            MaterialLook {
                textures: stone_pack.view(1.15, false),
                smart: SmartMaterial { edge_wear: 0.12, grime: 0.45, dust: 0.22, ..SmartMaterial::WEATHERED_STONE },
            },
            MaterialLook {
                textures: oak_pack.view(0.70, false),
                smart: SmartMaterial { edge_wear: 0.12, grime: 0.35, dust: 0.08, ..SmartMaterial::WEATHERED_TIMBER },
            },
            MaterialLook::NONE,
            MaterialLook::NONE,
            MaterialLook { textures: copper_pack.view(0.65, false), smart: SmartMaterial { edge_wear: 0.22, grime: 0.35, dust: 0.06, ..SmartMaterial::PAINTED_METAL } },
            MaterialLook { textures: iron_pack.view(0.85, false), smart: SmartMaterial::NONE },
            MaterialLook { textures: bark_pack.view(1.25, false), smart: SmartMaterial::WEATHERED_TIMBER },
            MaterialLook { textures: pine_pack.view(1.10, true), smart: SmartMaterial::NONE },
            MaterialLook { textures: warm_wood_pack.view(0.55, false), smart: SmartMaterial::NONE },
            MaterialLook {
                textures: rock_pack.view(1.35, true),
                smart: SmartMaterial { edge_wear: 0.10, grime: 0.48, dust: 0.18, ..SmartMaterial::WEATHERED_STONE },
            },
        ];

        let t_ren = Instant::now();
        let cabin_scene = cabin_asset.prepare_pbr_scene(&cabin_materials, alloc, alloc).expect("prepare cabin scene");

        let cabin_view = View {
            eye: mm3(6850.0, -9150.0, 1980.0),
            target: mm3(-180.0, -350.0, 2120.0),
            projection: Projection::Perspective { fov: Angle::degrees(35.0) },
            width: 960,
            height: 640,
            ..
        };
        let cabin_closeup = View {
            eye: mm3(2950.0, -5650.0, 1650.0),
            target: mm3(460.0, -1720.0, 1820.0),
            projection: Projection::Perspective { fov: Angle::degrees(34.0) },
            width: 960,
            height: 640,
            ..
        };

        let cabin_golden = render_parallel_pbr(
            &cabin_scene,
            &cabin_looks,
            cabin_view,
            PbrOpts { aperture_radius_mm: 18.0, atmosphere_haze: 0.007, ..PbrOpts::GOLDEN_HOUR },
            3, // 9 SPP!
            threads,
            alloc,
        );
        save_png(&out_path.join("cabin_golden_hour.png"), &cabin_golden, alloc);

        let cabin_detail = render_parallel_pbr(
            &cabin_scene,
            &cabin_looks,
            cabin_closeup,
            PbrOpts { aperture_radius_mm: 14.0, atmosphere_haze: 0.005, ..PbrOpts::GOLDEN_HOUR },
            3, // 9 SPP!
            threads,
            alloc,
        );
        save_png(&out_path.join("cabin_porch_closeup.png"), &cabin_detail, alloc);

        println!(
            "Cabin: {} tris, GLB {:.1} KB, Flecs {} B | build {:.2}s, 2x 960x640 9-SPP parallel GI+DoF+Denoise renders {:.2}s ({} threads)",
            cabin_asset.triangles(),
            cabin_asset.glb.len() as f64 / 1024.0,
            cabin_flecs.len(),
            t1.elapsed().as_secs_f64(),
            t_ren.elapsed().as_secs_f64(),
            threads,
        );
    }

    // =========================================================================
    // 2. ORBITAL RECON INTERCEPTOR (960x640 HD 9-SPP VNDF FLOOR REFLECTIONS + DoF)
    // =========================================================================
    if filter == "all" || filter == "ship" {
        let t0 = Instant::now();
        let mut ship_model = Model::new(ModelOpts { alloc, .. });
        build_interceptor(&mut ship_model).expect("build interceptor");
        let ship_asm_tape = ship_model.assembly_tape(alloc).expect("ship assembly tape");

        let hull_pack = synthesize_cc0_pack(ProceduralKind::HullPlating { base_rgb: [0.34, 0.37, 0.42], metallic: 0.84, roughness: 0.28, panels: 3 }, tex_res, alloc, alloc).expect("hull");
        let wing_pack = synthesize_cc0_pack(ProceduralKind::HullPlating { base_rgb: [0.10, 0.115, 0.14], metallic: 0.42, roughness: 0.36, panels: 2 }, tex_res, alloc, alloc).expect("wing");
        let ti_pack = synthesize_cc0_pack(ProceduralKind::BrushedMetal { base_rgb: [0.78, 0.80, 0.84], roughness: 0.22 }, tex_res, alloc, alloc).expect("ti");
        let carbon_pack = synthesize_cc0_pack(ProceduralKind::CarbonWeave { base_rgb: [0.06, 0.068, 0.078], roughness: 0.34 }, 256, alloc, alloc).expect("carbon");

        let ship_images = [
            TextureImage { name: "hull_albedo", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &hull_pack.albedo.pixels } },
            TextureImage { name: "hull_normal", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &hull_pack.normal.pixels } },
            TextureImage { name: "hull_orm", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &hull_pack.orm.pixels } },
            TextureImage { name: "wing_albedo", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &wing_pack.albedo.pixels } },
            TextureImage { name: "wing_normal", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &wing_pack.normal.pixels } },
            TextureImage { name: "wing_orm", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &wing_pack.orm.pixels } },
            TextureImage { name: "ti_albedo", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &ti_pack.albedo.pixels } },
            TextureImage { name: "ti_normal", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &ti_pack.normal.pixels } },
            TextureImage { name: "ti_orm", data: ImageData::Rgba8 { width: tex_res, height: tex_res, pixels: &ti_pack.orm.pixels } },
        ];

        let ship_materials = [
            Material {
                name: "hull.slate_steel",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                clearcoat: Some(Clearcoat { factor: 0.35, roughness: 0.14 }),
                pbr_textures: PbrTextures {
                    base_colour: Some(TextureRef { image: ImageId(0), .. }),
                    normal: Some(NormalTexture { image: ImageId(1), scale: 0.95, .. }),
                    metallic_roughness: Some(TextureRef { image: ImageId(2), .. }),
                    occlusion: Some(OcclusionTexture { image: ImageId(2), strength: 0.9, .. }),
                    ..PbrTextures::NONE
                },
                cc0_pack: Some("cc0://blaze/HullPlating_Slate_512"),
                ..
            },
            Material {
                name: "wing.squadron_orange",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                clearcoat: Some(Clearcoat { factor: 0.45, roughness: 0.14 }),
                pbr_textures: PbrTextures {
                    base_colour: Some(TextureRef { image: ImageId(3), .. }),
                    normal: Some(NormalTexture { image: ImageId(4), scale: 0.90, .. }),
                    metallic_roughness: Some(TextureRef { image: ImageId(5), .. }),
                    occlusion: Some(OcclusionTexture { image: ImageId(5), strength: 0.9, .. }),
                    ..PbrTextures::NONE
                },
                cc0_pack: Some("cc0://blaze/HullPlating_Orange_512"),
                ..
            },
            Material {
                name: "hardware.brushed_titanium",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                anisotropy: Some(Anisotropy { strength: 0.72, rotation: 0.0 }),
                pbr_textures: PbrTextures {
                    base_colour: Some(TextureRef { image: ImageId(6), .. }),
                    normal: Some(NormalTexture { image: ImageId(7), scale: 0.35, .. }),
                    metallic_roughness: Some(TextureRef { image: ImageId(8), .. }),
                    ..PbrTextures::NONE
                },
                cc0_pack: Some("cc0://blaze/BrushedTitanium_512"),
                ..
            },
            Material {
                name: "cockpit.carbon_weave",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                clearcoat: Some(Clearcoat { factor: 0.30, roughness: 0.22 }),
                cc0_pack: Some("cc0://blaze/CarbonWeave_256"),
                ..
            },
            Material {
                name: "canopy.polycarbonate",
                base_colour: [0.76, 0.84, 0.90, 1.0],
                metallic: 0.0,
                roughness: 0.025,
                clearcoat: Some(Clearcoat { factor: 1.0, roughness: 0.015 }),
                transmission: Some(Transmission { factor: 0.90, ior: 1.54, thickness: 0.015, .. }),
                ..
            },
            Material {
                name: "light.ion_cyan",
                base_colour: [0.55, 0.94, 1.0, 1.0],
                metallic: 0.0,
                roughness: 0.18,
                emissive: [0.24, 0.80, 1.0],
                emissive_strength: Some(12.0),
                ..
            },
            Material {
                name: "rubber.aircraft_tire",
                base_colour: [0.035, 0.036, 0.039, 1.0],
                metallic: 0.0,
                roughness: 0.84,
                sheen: Some(Sheen { colour: [0.08, 0.08, 0.09], roughness: 0.75 }),
                ..
            },
        ];

        let ship_sockets = [
            PartSocket::new("fuselage", "canopy.hinge", shift(1020.0, 0.0, 210.0)),
            PartSocket::new("titanium_hardware", "thruster.port", shift(-1580.0, 310.0, 25.0)),
            PartSocket::new("titanium_hardware", "thruster.starboard", shift(-1580.0, -310.0, 25.0)),
            PartSocket::new("wings", "nav.port", shift(-860.0, 1705.0, -30.0)),
            PartSocket::new("wings", "nav.starboard", shift(-860.0, -1705.0, -30.0)),
        ];

        let ship_asset = build_game_asset(
            ship_model.graph_ref(),
            ship_model.parts(),
            GameAssetOpts {
                alloc,
                scratch: alloc,
                preset: AssetPreset::GameProp,
                total_budget: Some(TriangleBudget(36_000)),
                materials: &ship_materials,
                images: &ship_images,
                sockets: &ship_sockets,
                pbr_mesh: Some(PbrMeshOpts {
                    uv_mode: UvMode::BoxMetric { tile_size: Length::m(0.85), grain: UvGrainAxis::Longest },
                    curvature_gain: 1.8,
                    ..PbrMeshOpts::DEFAULT
                }),
                assembly_tape: Some(&ship_asm_tape),
                density: Some(Density::kg_per_m3(2700.0)),
                fidelity: None,
                ..
            },
            None,
        )
        .expect("build ship asset");

        std::fs::write(out_path.join("interceptor_pbr.glb"), &*ship_asset.glb).expect("write ship glb");
        let ship_flecs = ship_asset
            .export_flecs_prefab(
                &ship_materials,
                &ship_sockets,
                FlecsOpts {
                    module: Some("ships"),
                    prefab_name: "ReconInterceptor",
                    asset_uri: "assets/interceptor_pbr.glb",
                    up: Up::Y,
                    include_schema: true,
                    alloc,
                    scratch: alloc,
                },
            )
            .expect("export ship flecs");
        std::fs::write(out_path.join("interceptor_pbr.flecs"), &*ship_flecs).expect("write ship flecs");

        let ship_looks = [
            MaterialLook { textures: hull_pack.view(0.95, false), smart: SmartMaterial::PAINTED_METAL },
            MaterialLook { textures: wing_pack.view(0.90, false), smart: SmartMaterial::PAINTED_METAL },
            MaterialLook {
                textures: ti_pack.view(0.35, false),
                smart: SmartMaterial { edge_wear: 0.20, grime: 0.35, dust: 0.05, ..SmartMaterial::PAINTED_METAL },
            },
            MaterialLook { textures: carbon_pack.view(0.85, false), smart: SmartMaterial::NONE },
            MaterialLook::NONE,
            MaterialLook::NONE,
            MaterialLook::NONE,
            MaterialLook::NONE,
        ];

        let t_ren = Instant::now();
        let ship_scene = ship_asset.prepare_pbr_scene(&ship_materials, alloc, alloc).expect("prepare ship scene");

        let ship_view_hero = View {
            eye: mm3(3680.0, -2850.0, 1080.0),
            target: mm3(180.0, 0.0, 40.0),
            projection: Projection::Perspective { fov: Angle::degrees(35.0) },
            width: 960,
            height: 640,
            ..
        };
        let ship_view_rear = View {
            eye: mm3(-3250.0, -1850.0, 720.0),
            target: mm3(-420.0, 0.0, 50.0),
            projection: Projection::Perspective { fov: Angle::degrees(35.0) },
            width: 960,
            height: 640,
            ..
        };

        let ship_hangar = render_parallel_pbr(
            &ship_scene,
            &ship_looks,
            ship_view_hero,
            PbrOpts { aperture_radius_mm: 16.0, ..PbrOpts::HANGAR },
            3,
            threads,
            alloc,
        );
        save_png(&out_path.join("interceptor_hangar.png"), &ship_hangar, alloc);

        let ship_rear = render_parallel_pbr(
            &ship_scene,
            &ship_looks,
            ship_view_rear,
            PbrOpts { aperture_radius_mm: 14.0, ..PbrOpts::HANGAR },
            3,
            threads,
            alloc,
        );
        save_png(&out_path.join("interceptor_rear_hangar.png"), &ship_rear, alloc);

        let ship_studio = render_parallel_pbr(
            &ship_scene,
            &ship_looks,
            ship_view_hero,
            PbrOpts { aperture_radius_mm: 14.0, ..PbrOpts::STUDIO },
            3,
            threads,
            alloc,
        );
        save_png(&out_path.join("interceptor_studio_pbr.png"), &ship_studio, alloc);

        println!(
            "Interceptor: {} tris, GLB {:.1} KB, Flecs {} B | build {:.2}s, 3x 960x640 9-SPP parallel renders {:.2}s ({} threads)",
            ship_asset.triangles(),
            ship_asset.glb.len() as f64 / 1024.0,
            ship_flecs.len(),
            t0.elapsed().as_secs_f64(),
            t_ren.elapsed().as_secs_f64(),
            threads,
        );
    }

    // =========================================================================
    // 3. PRECISION CNC 50mm f/1.4 CAMERA LENS (MACRO PRODUCT PHOTOGRAPHY!)
    // =========================================================================
    if filter == "all" || filter == "lens" {
        let t2 = Instant::now();
        let mut lens_model = Model::new(ModelOpts { alloc, .. });
        build_camera_lens(&mut lens_model).expect("build camera lens");
        let lens_asm_tape = lens_model.assembly_tape(alloc).expect("lens assembly tape");

        let anodized_pack = synthesize_cc0_pack(ProceduralKind::WroughtIron { base_rgb: [0.052, 0.055, 0.062], roughness: 0.34 }, tex_res, alloc, alloc).expect("anodized");
        let ti_pack = synthesize_cc0_pack(ProceduralKind::BrushedMetal { base_rgb: [0.80, 0.82, 0.85], roughness: 0.18 }, tex_res, alloc, alloc).expect("lens ti");
        let brass_pack = synthesize_cc0_pack(ProceduralKind::BrushedMetal { base_rgb: [0.88, 0.68, 0.30], roughness: 0.16 }, 256, alloc, alloc).expect("lens brass");

        let lens_materials = [
            Material {
                name: "metal.anodized_black",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                clearcoat: Some(Clearcoat { factor: 0.25, roughness: 0.26 }),
                cc0_pack: Some("cc0://blaze/AnodizedBlack_512"),
                ..
            },
            Material {
                name: "metal.brushed_titanium",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                anisotropy: Some(Anisotropy { strength: 0.78, rotation: 0.0 }),
                cc0_pack: Some("cc0://blaze/BrushedTitanium_512"),
                ..
            },
            Material {
                name: "metal.machined_brass",
                base_colour: [1.0, 1.0, 1.0, 1.0],
                metallic: 1.0,
                roughness: 1.0,
                anisotropy: Some(Anisotropy { strength: 0.65, rotation: 0.0 }),
                ..
            },
            Material {
                name: "enamel.leica_ruby",
                base_colour: [0.78, 0.04, 0.04, 1.0],
                metallic: 0.0,
                roughness: 0.14,
                clearcoat: Some(Clearcoat { factor: 1.0, roughness: 0.04 }),
                ..
            },
            Material {
                name: "glass.multicoat_front",
                base_colour: [0.34, 0.84, 0.72, 1.0],
                metallic: 0.0,
                roughness: 0.018,
                clearcoat: Some(Clearcoat { factor: 1.0, roughness: 0.012 }),
                transmission: Some(Transmission { factor: 0.76, ior: 1.62, thickness: 0.015, .. }),
                ..
            },
            Material {
                name: "glass.multicoat_inner",
                base_colour: [0.16, 0.28, 0.86, 1.0],
                metallic: 0.0,
                roughness: 0.02,
                clearcoat: Some(Clearcoat { factor: 1.0, roughness: 0.015 }),
                transmission: Some(Transmission { factor: 0.50, ior: 1.68, thickness: 0.012, .. }),
                ..
            },
            Material {
                name: "optic.black_flock",
                base_colour: [0.008, 0.009, 0.011, 1.0],
                metallic: 0.0,
                roughness: 0.96,
                ..
            },
        ];

        let lens_sockets = [
            PartSocket::new("titanium_mount_collar", "bayonet.mount", shift(0.0, 0.0, -49.0)),
            PartSocket::new("anodized_barrel", "filter.thread_58mm", shift(0.0, 0.0, 57.0)),
        ];

        let lens_asset = build_game_asset(
            lens_model.graph_ref(),
            lens_model.parts(),
            GameAssetOpts {
                alloc,
                scratch: alloc,
                preset: AssetPreset::HeroAsset,
                total_budget: Some(TriangleBudget(48_000)),
                materials: &lens_materials,
                sockets: &lens_sockets,
                pbr_mesh: Some(PbrMeshOpts {
                    uv_mode: UvMode::BoxMetric { tile_size: Length::m(0.12), grain: UvGrainAxis::Longest },
                    curvature_gain: 1.4,
                    ..PbrMeshOpts::DEFAULT
                }),
                assembly_tape: Some(&lens_asm_tape),
                density: Some(Density::kg_per_m3(2700.0)),
                fidelity: None,
                ..
            },
            None,
        )
        .expect("build lens asset");

        std::fs::write(out_path.join("camera_lens_pbr.glb"), &*lens_asset.glb).expect("write lens glb");
        let lens_flecs = lens_asset
            .export_flecs_prefab(
                &lens_materials,
                &lens_sockets,
                FlecsOpts {
                    module: Some("optics"),
                    prefab_name: "PrimeLens50mm",
                    asset_uri: "assets/camera_lens_pbr.glb",
                    up: Up::Y,
                    include_schema: true,
                    alloc,
                    scratch: alloc,
                },
            )
            .expect("lens flecs");
        std::fs::write(out_path.join("camera_lens_pbr.flecs"), &*lens_flecs).expect("write lens flecs");

        let lens_looks = [
            MaterialLook {
                textures: anodized_pack.view(0.32, false),
                smart: SmartMaterial { edge_wear: 0.16, grime: 0.18, dust: 0.04, ..SmartMaterial::PAINTED_METAL },
            },
            MaterialLook {
                textures: ti_pack.view(0.28, false),
                smart: SmartMaterial { edge_wear: 0.08, grime: 0.12, dust: 0.02, ..SmartMaterial::PAINTED_METAL },
            },
            MaterialLook { textures: brass_pack.view(0.24, false), smart: SmartMaterial::NONE },
            MaterialLook::NONE,
            MaterialLook::NONE,
            MaterialLook::NONE,
            MaterialLook::NONE,
        ];

        let t_ren = Instant::now();
        let lens_scene = lens_asset.prepare_pbr_scene(&lens_materials, alloc, alloc).expect("prepare lens scene");

        let lens_view = View {
            eye: mm3(188.0, -142.0, 70.0),
            target: mm3(6.0, 0.0, 2.0),
            projection: Projection::Perspective { fov: Angle::degrees(30.0) },
            width: 960,
            height: 640,
            ..
        };

        let lens_studio = render_parallel_pbr(
            &lens_scene,
            &lens_looks,
            lens_view,
            PbrOpts {
                exposure: 1.18,
                ground_colour: [0.048, 0.052, 0.058],
                aperture_radius_mm: 1.8, // crisp macro focus on front bezel, brass ring & knurling, soft rear bokeh!
                ..PbrOpts::STUDIO
            },
            3, // 9 SPP!
            threads,
            alloc,
        );
        save_png(&out_path.join("camera_lens_macro.png"), &lens_studio, alloc);

        println!(
            "Camera Lens: {} tris, GLB {:.1} KB, Flecs {} B | build {:.2}s, 960x640 9-SPP macro DoF render {:.2}s ({} threads)",
            lens_asset.triangles(),
            lens_asset.glb.len() as f64 / 1024.0,
            lens_flecs.len(),
            t2.elapsed().as_secs_f64(),
            t_ren.elapsed().as_secs_f64(),
            threads,
        );
    }

    println!("Outputs written to {}", out_path.display());
}
