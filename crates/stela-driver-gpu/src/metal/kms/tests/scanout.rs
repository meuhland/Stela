use super::*;

#[test]
fn row_fill_failure_keeps_the_front_and_retries_the_hidden_buffer() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    card.busy = 2;
    card.pitches = vec![8, 16, 32];
    let hidden;
    {
        let mut display = Display::take(&mut card, (2, 2)).unwrap();
        let front = display.front();
        hidden = display.buffers[2].framebuffer;
        let failure = display
            .prepare_rows(None, |y, row| {
                row.fill(1);
                if y == 1 {
                    return Err(refuse!("second row failed"));
                }
                Ok(())
            })
            .err();
        assert_eq!(failure, Some(refuse!("second row failed")));
        assert_eq!(display.front(), front);
        assert!(display.drm.flips.is_empty());
        assert_eq!(display.drm.busy, 2);
        assert!(display.drm.memory[..2].iter().flatten().all(|&b| b == 0));
        assert_eq!(&display.drm.memory[2][..8], &[1; 8]);
        assert!(display.drm.memory[2][8..].iter().all(|&b| b == 0));

        let mut scratch = None;
        display
            .prepare_rows(None, |y, row| {
                assert_eq!(row.len(), 8);
                assert_eq!(*scratch.get_or_insert(row.as_ptr()), row.as_ptr());
                row.fill(if y == 0 { 2 } else { 3 });
                Ok(())
            })
            .unwrap()
            .present()
            .unwrap();
        // The busy engine keeps the flip queued until a flush finds it free.
        assert!(display.queued());
        display.flush().unwrap();
        display.flush().unwrap();
        assert_eq!(display.buffers[0].framebuffer, hidden);
    }
    assert_eq!(card.flips, [hidden]);
    let filled = &card.memory[2];
    assert_eq!(&filled[..8], &[2; 8]);
    assert_eq!(&filled[32..40], &[3; 8]);
    assert!(filled[8..32].iter().all(|&b| b == 0));
    assert!(filled[40..].iter().all(|&b| b == 0));
}

#[test]
fn a_failed_flip_keeps_the_front_and_retries_the_hidden_buffer() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    card.flip_error = Some(-5);
    let framebuffer;
    {
        let mut display = Display::take(&mut card, (2, 2)).unwrap();
        let front = display.front();
        framebuffer = display.buffers[2].framebuffer;
        assert_eq!(display.present(&[1; 16]), Err(Error::Request(-5)));
        assert_eq!(display.front(), front);
        display.present(&[2; 16]).unwrap();
        assert_eq!(display.buffers[0].framebuffer, framebuffer);
    }
    assert_eq!(card.flips, [framebuffer]);
    assert_eq!(&card.memory[2][..8], &[2; 8]);
}

#[test]
fn takeover_clears_every_scanout_buffer_before_modesetting() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    Display::take(&mut card, (2, 2)).unwrap();
    assert_eq!(card.memory.len(), 3);
    assert!(card.memory.iter().flatten().all(|byte| *byte == 0));
}

#[test]
fn each_buffer_keeps_its_own_pitch() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    card.pitches = vec![8, 16, 32];
    {
        let mut display = Display::take(&mut card, (2, 2)).unwrap();
        assert_eq!(display.front().1, 8);
        display.present(&[1; 16]).unwrap();
        assert_eq!(display.front().1, 32);
        display.present(&[2; 16]).unwrap();
        assert_eq!(display.front().1, 16);
    }
    assert_eq!(&card.memory[2][32..40], &[1; 8]);
    assert_eq!(&card.memory[1][16..24], &[2; 8]);
    assert!(card.memory[2][8..32].iter().all(|byte| *byte == 0));
}

#[test]
fn incomplete_or_oversized_frames_do_not_touch_or_flip_a_buffer() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    {
        let mut display = Display::take(&mut card, (2, 2)).unwrap();
        for actual in [0, 1, 15, 17, 32] {
            assert_eq!(
                display.present(&vec![1; actual]),
                Err(Error::FrameSize {
                    expected: 16,
                    actual
                })
            );
        }
    }
    assert!(card.flips.is_empty());
    assert!(card.memory.iter().flatten().all(|byte| *byte == 0));
}

#[test]
fn invalid_pitch_or_extent_prevents_mapping_and_modesetting() {
    for allocation in [(7, 16), (8, 15), (u32::MAX, 16)] {
        let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
        card.invalid_buffer = Some(allocation);
        assert_eq!(
            Display::take(&mut card, (2, 2)).err(),
            Some(Error::InvalidBuffer)
        );
        assert!(card.set.is_none());
        assert!(card.memory.is_empty());
    }
}

#[test]
fn enumeration_retries_hotplugged_resource_and_mode_lists() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    card.resource_changes = 2;
    card.mode_changes = 2;
    let display = Display::take(&mut card, (2, 2)).unwrap();
    assert_eq!(display.size(), (2, 2));
    assert_eq!(card.resource_changes, 0);
    assert_eq!(card.mode_changes, 0);
}

#[test]
fn unstable_resource_or_mode_lists_stop_without_modesetting() {
    for (resource_changes, mode_changes) in [(ENUMERATION_ATTEMPTS, 0), (0, ENUMERATION_ATTEMPTS)] {
        let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
        card.resource_changes = resource_changes;
        card.mode_changes = mode_changes;
        assert_eq!(Display::take(&mut card, (2, 2)).err(), Some(Error::Changed));
        assert!(card.set.is_none());
    }
}

#[test]
fn the_last_vblank_is_the_first_outputs_on_linuxs_clock() {
    let mut card = Card::new(vec![mode(2, 2, PREFERRED)]);
    card.vblank = Some((1, 5_000_000_123));
    let mut display = Display::take(&mut card, (2, 2)).unwrap();
    assert_eq!(display.last_vblank_ns(), Some(5_000_000_123));
    assert_eq!(display.drm.sequences, [41]);
    // A controller that is off, a refusal, or no time: none.
    display.drm.vblank = Some((0, 5_000_000_123));
    assert_eq!(display.last_vblank_ns(), None);
    display.drm.vblank = Some((1, 0));
    assert_eq!(display.last_vblank_ns(), None);
    display.drm.vblank = None;
    assert_eq!(display.last_vblank_ns(), None);
    // Not asked where a request must wait, nor while the outputs are dark.
    display.drm.vblank = Some((1, 9));
    display.drm.immediate = false;
    assert_eq!(display.last_vblank_ns(), None);
    display.drm.immediate = true;
    display.dark = true;
    assert_eq!(display.last_vblank_ns(), None);
    assert_eq!(display.drm.sequences.len(), 4);
}

#[test]
fn the_display_switches_between_the_rates_of_its_size_the_guest_can_be_told_of() {
    use super::outputs::at;
    let interlaced = ModeInfo {
        flags: INTERLACED,
        ..at(2, 2, 0, 240)
    };
    let mut card = Card::new(vec![
        at(2, 2, PREFERRED, 60),
        at(2, 2, 0, 144),
        at(2, 2, 0, 120),
        interlaced,
        at(2, 2, 0, 60),
        at(4, 2, 0, 75),
    ]);
    let no_200: Fits<'_> = &|_, _, refresh| refresh != 200_000;
    let mut display = Display::take_fitting(&mut card, Wanted::Size((2, 2)), no_200).unwrap();
    assert_eq!(display.refresh_millihertz(), 144_000);
    assert_eq!(display.refresh_rates(), [144_000, 120_000, 60_000]);
    let sets = display.drm.mode_sets;
    display.set_refresh(60_000).unwrap();
    assert_eq!(display.refresh_millihertz(), 60_000);
    assert_eq!(display.drm.mode_sets, sets + 1);
    assert_eq!(display.drm.set.map(|(mode, _)| mode.vrefresh), Some(60));
    assert_eq!(display.set_refresh(75_000), Err(Error::NoRate));
    // Where a request must wait, the next flush that may sets it.
    display.drm.immediate = false;
    display.set_refresh(120_000).unwrap();
    assert_eq!(display.refresh_millihertz(), 60_000);
    assert!(display.queued());
    display.drm.immediate = true;
    display.flush().unwrap();
    assert!(!display.queued());
    assert_eq!(display.refresh_millihertz(), 120_000);
    assert_eq!(display.drm.set.map(|(mode, _)| mode.vrefresh), Some(120));
    // A refused set keeps the mode that ran.
    display.drm.set_error = Some(-22);
    assert_eq!(display.set_refresh(144_000), Err(Error::Request(-22)));
    assert_eq!(display.refresh_millihertz(), 120_000);
    assert_eq!(display.heads[0].mode.vrefresh, 120);
}
