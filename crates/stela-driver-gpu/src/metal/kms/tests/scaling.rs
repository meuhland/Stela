//! Outputs with no mode of the display's size: scaled into a larger native
//! mode, else dark.
use super::outputs::{at, two_monitors};
use super::*;

/// What each of `outputs` runs for a display taken before the guest's mode
/// is set: the size [`plan`] picks, and each output's [`Fit`], the one that
/// gives the guest's mode in its own mode.
fn shown(outputs: &[&[ModeInfo]]) -> ((u32, u32), Vec<Fit>) {
    let any: Fits<'_> = &|_, _, _| true;
    let (size, gives) = plan(outputs, Wanted::Native((1920, 1080)), any).unwrap();
    let fits = outputs
        .iter()
        .enumerate()
        .map(|(index, modes)| match index == gives {
            true => Fit::Mode(mode_of(modes, size, any).unwrap()),
            false => fit(modes, size),
        })
        .collect();
    (size, fits)
}

#[test]
fn each_output_runs_a_mode_of_the_shared_size_or_a_larger_native_one() {
    let qhd = [at(2560, 1440, PREFERRED, 144), at(1920, 1080, 0, 60)];
    let fhd = [at(1920, 1080, PREFERRED, 60), at(1280, 720, 0, 60)];
    // The same monitor twice, and two that share a listed size: each runs
    // its own mode, nothing is scaled, as before.
    assert_eq!(
        shown(&[&qhd, &qhd]),
        ((2560, 1440), vec![Fit::Mode(qhd[0]); 2])
    );
    assert_eq!(
        shown(&[&qhd, &fhd]),
        ((1920, 1080), vec![Fit::Mode(qhd[1]), Fit::Mode(fhd[0])])
    );
    // No shared size, the smaller one first: the larger is scaled.
    let wide = [at(3440, 1440, PREFERRED, 100), at(2560, 1440, 0, 60)];
    let tall = [at(1920, 1200, PREFERRED, 60), at(1600, 1200, 0, 60)];
    assert_eq!(
        shown(&[&fhd, &wide]),
        ((1920, 1080), vec![Fit::Mode(fhd[0]), Fit::Scaled(wide[0])])
    );
    assert_eq!(
        shown(&[&fhd, &tall]),
        ((1920, 1080), vec![Fit::Mode(fhd[0]), Fit::Scaled(tall[0])])
    );
    // The larger one first: the smaller would need shrinking.
    assert_eq!(
        shown(&[&wide, &fhd]),
        ((3440, 1440), vec![Fit::Mode(wide[0]), Fit::Smaller(fhd[0])])
    );
    // A portrait panel is narrower than a landscape picture.
    let portrait = [at(1080, 1920, PREFERRED, 60)];
    assert_eq!(
        shown(&[&fhd, &portrait]),
        (
            (1920, 1080),
            vec![Fit::Mode(fhd[0]), Fit::Smaller(portrait[0])]
        )
    );
    assert_eq!(fit(&[], (1920, 1080)), Fit::Nothing);
}

#[test]
fn the_picture_keeps_its_shape_in_the_native_mode() {
    // Same shape: the whole raster.
    assert_eq!(letterbox((1920, 1080), (3840, 2160)), (3840, 2160));
    assert_eq!(letterbox((1920, 1080), (2560, 1440)), (2560, 1440));
    // Taller: bars above and below; wider: bars at the sides.
    assert_eq!(letterbox((1920, 1080), (1920, 1200)), (1920, 1080));
    assert_eq!(letterbox((1920, 1080), (3440, 1440)), (2560, 1440));
    assert_eq!(letterbox((1920, 1200), (2560, 1440)), (2304, 1440));
    assert_eq!(letterbox((0, 0), (2560, 1440)), (2560, 1440));
}

#[test]
fn the_native_mode_is_the_preferred_one_else_the_largest_progressive() {
    let interlaced = ModeInfo {
        flags: INTERLACED,
        ..at(1920, 1080, PREFERRED, 60)
    };
    let modes = [
        interlaced,
        at(1600, 1200, 0, 60),
        at(1920, 1080, 0, 75),
        ModeInfo {
            clock: 1,
            ..at(1920, 1080, 0, 75)
        },
        at(1920, 1080, 0, 60),
    ];
    // No progressive preferred mode: the widest, then fastest, the last
    // of equals.
    assert_eq!(native(&modes), Some(modes[3]));
    let preferred = [at(1600, 1200, 0, 60), at(1280, 1024, PREFERRED, 60)];
    assert_eq!(native(&preferred), Some(preferred[1]));
    assert_eq!(native(&[]), None);
}

/// Connector 50 lists only the small size and gives the picture; connector
/// 51's monitor is larger and lists no mode of it.
fn unshared() -> Card {
    let mut card = Card::new(vec![at(8, 6, PREFERRED, 60), at(6, 6, 0, 60)]);
    card.first = vec![at(4, 2, PREFERRED, 75)];
    card.first_connected = true;
    card.scaler = true;
    card
}

#[test]
fn an_output_with_no_mode_of_the_size_is_scaled_into_its_native_mode() {
    let mut card = unshared();
    let picture = card.first[0];
    let front;
    {
        let display =
            Display::take_fitting(&mut card, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!(display.size(), (4, 2));
        assert_eq!(display.refresh_millihertz(), 75_000);
        assert_eq!((display.outputs(), display.unshown()), (2, 0));
        let scaled: Vec<Scaled> = display.scaled().collect();
        assert_eq!(
            scaled,
            [Scaled {
                native: (8, 6),
                picture: (8, 4)
            }]
        );
        assert_eq!((display.smaller(), display.refused()), (0, None));
        front = display.buffers[0].framebuffer;
    }
    // Its controller takes the first output's mode as the picture, on the
    // same buffer, with "Full aspect" set before it is lit.
    assert_eq!(card.set, Some((picture, front)));
    assert_eq!(card.scalings, [(51, SCALE_ASPECT)]);
    assert_eq!(card.lit_scaling, [(40, SCALE_NONE), (41, SCALE_ASPECT)]);
}

#[test]
fn an_output_that_cannot_scale_or_would_shrink_stays_dark() {
    // No scaling property: dark, as before, its properties read.
    let mut card = unshared();
    card.scaler = false;
    {
        let display =
            Display::take_fitting(&mut card, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!((display.outputs(), display.unshown()), (1, 1));
        assert_eq!(display.smaller(), 0);
    }
    assert_eq!((card.set, card.property_reads), (None, 1));
    assert!(card.scalings.is_empty());

    // The larger monitor gives the picture: the smaller one would need
    // shrinking, and its properties are not read.
    let mut lit = unshared();
    lit.lit = 51;
    {
        let display =
            Display::take_fitting(&mut lit, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!(display.size(), (8, 6));
        assert_eq!((display.outputs(), display.unshown()), (1, 1));
        assert_eq!(display.smaller(), 1);
    }
    assert_eq!(lit.set_40, None);
    assert_eq!(lit.property_requests(), 0);

    // A refused scaling: dark, the refusal kept for the report.
    let mut refused = unshared();
    refused.scaling_error = Some(-22);
    {
        let display =
            Display::take_fitting(&mut refused, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!((display.outputs(), display.unshown()), (1, 1));
        assert_eq!(display.refused(), Some(Error::Request(-22)));
    }
    assert_eq!(refused.set, None);

    // A refused mode set puts the scaling back.
    let mut unset = unshared();
    unset.refused_crtc = Some(41);
    {
        let display =
            Display::take_fitting(&mut unset, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!((display.outputs(), display.unshown()), (1, 1));
        assert_eq!(display.refused(), Some(Error::Request(-22)));
    }
    assert_eq!(unset.scalings, [(51, SCALE_ASPECT), (51, SCALE_NONE)]);
}

#[test]
fn one_monitor_or_monitors_that_share_a_size_ask_nothing_of_the_scaler() {
    let mut single = Card::new(vec![at(4, 2, PREFERRED, 60)]);
    single.scaler = true;
    {
        let display =
            Display::take_fitting(&mut single, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!(display.scaled().count(), 0);
    }
    assert_eq!(single.property_requests(), 0);

    let mut shared = two_monitors();
    shared.scaler = true;
    {
        let mut display =
            Display::take_fitting(&mut shared, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        assert_eq!((display.outputs(), display.scaled().count()), (2, 0));
        display.present(&[7; 96]).unwrap();
        let _node = display.release();
    }
    assert_eq!(shared.property_requests(), 0);
    assert_eq!(shared.lit_scaling, [(40, SCALE_NONE), (41, SCALE_NONE)]);
}

#[test]
fn a_larger_monitor_connected_later_joins_scaled_and_is_set_back_on_release() {
    let mut card = unshared();
    card.connected = false;
    {
        let mut display = Display::take(&mut card, (4, 2)).unwrap();
        assert_eq!(display.outputs(), 1);
        display.drm.connected = true;
        let joined = Watched {
            joined: 1,
            scaled: 1,
            ..Watched::default()
        };
        assert_eq!(display.watch(), joined);
        assert_eq!(display.scaled().count(), 1);
        let _node = display.release();
    }
    assert_eq!(card.scalings, [(51, SCALE_ASPECT), (51, SCALE_NONE)]);
    assert_eq!(card.scaling, [SCALE_NONE; 2]);

    // A smaller one stays dark, counted as such.
    let mut small = unshared();
    small.connected = false;
    small.first = vec![at(8, 6, PREFERRED, 60)];
    small.modes = vec![at(4, 2, PREFERRED, 60)];
    let mut display = Display::take(&mut small, (8, 6)).unwrap();
    display.drm.connected = true;
    let dark = Watched {
        unshown: 1,
        smaller: 1,
        ..Watched::default()
    };
    for _ in 1..RESTORE_TRIES {
        assert_eq!(display.watch(), Watched::default());
    }
    assert_eq!(display.watch(), dark);
    assert_eq!(display.drm.property_requests(), 0);
}

#[test]
fn a_scaled_output_reconnected_to_a_monitor_with_the_size_runs_it_unscaled() {
    let mut card = unshared();
    let front;
    {
        let mut display =
            Display::take_fitting(&mut card, Wanted::Native((4, 2)), &|_, _, _| true).unwrap();
        front = display.buffers[0].framebuffer;
        display.drm.connected = false;
        let lost = Watched {
            lost: 1,
            ..Watched::default()
        };
        assert_eq!(display.watch(), lost);
        // The same larger monitor again: scaled again.
        display.drm.connected = true;
        let rescaled = Watched {
            restored: 1,
            scaled: 1,
            ..Watched::default()
        };
        assert_eq!(display.watch(), rescaled);
        display.drm.connected = false;
        display.watch();
        // Another one, which has the size: its own mode, the scaling put
        // back once its controller is off.
        display.drm.modes = vec![at(8, 6, PREFERRED, 60), at(4, 2, 0, 60)];
        display.drm.connected = true;
        let restored = Watched {
            restored: 1,
            ..Watched::default()
        };
        assert_eq!(display.watch(), restored);
        assert_eq!(display.scaled().count(), 0);
    }
    assert_eq!(card.set, Some((at(4, 2, 0, 60), front)));
    assert_eq!(
        card.scalings,
        [(51, SCALE_ASPECT), (51, SCALE_ASPECT), (51, SCALE_NONE)]
    );
    assert_eq!(
        card.lit_scaling[1..],
        [(41, SCALE_ASPECT), (41, SCALE_ASPECT), (41, SCALE_NONE)]
    );
}
