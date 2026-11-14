//! A firmware request that a bound can cut short, tried again with longer
//! bounds: under a hypervisor a virtual CPU the host schedules late, or one
//! the firmware still holds, misses a bound it meets the next time.

/// Run `attempt` with each of `bounds` in turn, by index, until it
/// succeeds or fails in a way `retried` does not accept; on that failure, or
/// when every bound is spent, the last error and the tries made.
///
/// # Panics
/// When `bounds` is empty.
pub fn bounded<T, E>(
    bounds: &[u64],
    retried: impl Fn(&E) -> bool,
    mut attempt: impl FnMut(usize, u64) -> Result<T, E>,
) -> Result<T, (E, usize)> {
    assert!(!bounds.is_empty(), "a retried request needs a bound");
    let mut tries = 0;
    loop {
        let bound = bounds[tries];
        let error = match attempt(tries, bound) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        tries += 1;
        if tries == bounds.len() || !retried(&error) {
            return Err((error, tries));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[derive(Debug, PartialEq)]
    enum Failure {
        Late,
        Broken,
    }

    #[test]
    fn a_late_answer_is_tried_again_with_each_longer_bound() {
        let mut seen = Vec::new();
        let result = bounded(
            &[500, 2_000, 5_000],
            |error| *error == Failure::Late,
            |index, bound| {
                seen.push((index, bound));
                if index < 2 {
                    Err(Failure::Late)
                } else {
                    Ok("observed")
                }
            },
        );
        assert_eq!(result, Ok("observed"));
        assert_eq!(seen, [(0, 500), (1, 2_000), (2, 5_000)]);
    }

    #[test]
    fn every_bound_spent_gives_the_last_error_and_the_tries() {
        let result: Result<(), _> = bounded(&[500, 2_000], |_| true, |_, _| Err(Failure::Late));
        assert_eq!(result, Err((Failure::Late, 2)));
    }

    #[test]
    fn a_failure_that_is_not_retried_stops_at_once() {
        let mut calls = 0;
        let result: Result<(), _> = bounded(
            &[500, 2_000, 5_000],
            |error| *error == Failure::Late,
            |_, _| {
                calls += 1;
                Err(Failure::Broken)
            },
        );
        assert_eq!(result, Err((Failure::Broken, 1)));
        assert_eq!(calls, 1);
    }
}
