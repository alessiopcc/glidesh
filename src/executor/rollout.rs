//! How a plan's `serial` splits its hosts into batches, and when `max-fail` stops it.

use glidesh::config::types::Amount;

/// The size of each batch for `total` hosts. `serial` values are used in order and the last
/// repeats; a percentage is rounded down, but never below one host. No `serial` is one batch.
pub fn batch_sizes(total: usize, serial: &[Amount]) -> Vec<usize> {
    if serial.is_empty() || total == 0 {
        return vec![total];
    }
    let mut sizes = Vec::new();
    let mut left = total;
    let mut values = serial.iter();
    let mut current = serial[0];
    while left > 0 {
        if let Some(next) = values.next() {
            current = *next;
        }
        let size = match current {
            Amount::Count(n) => n,
            Amount::Percent(p) => total * usize::from(p) / 100,
        }
        .max(1)
        .min(left);
        sizes.push(size);
        left -= size;
    }
    sizes
}

/// Why no further batch should start, or `None` to carry on. Checked after each batch.
///
/// With `max-fail`, the run stops once failures exceed it — a count, or a share of all the
/// plan's hosts. Without it, the run stops only when every host of the last batch failed: a
/// batch that fails completely is almost always a broken change, not a bad host.
pub fn stop_reason(
    max_fail: Option<Amount>,
    failed: usize,
    total: usize,
    batch: usize,
    batch_failed: usize,
) -> Option<String> {
    let exceeded = match max_fail {
        Some(Amount::Count(n)) => failed > n,
        Some(Amount::Percent(p)) => failed * 100 > usize::from(p) * total,
        None => batch > 0 && batch_failed == batch,
    };
    if !exceeded {
        return None;
    }
    Some(match max_fail {
        Some(Amount::Count(n)) => {
            format!("{failed} of {total} hosts failed, more than max-fail {n}")
        }
        Some(Amount::Percent(p)) => {
            format!("{failed} of {total} hosts failed, more than max-fail {p}%")
        }
        None => format!("every host in the last batch failed ({batch_failed} of {batch})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_serial_is_one_batch_of_everyone() {
        assert_eq!(batch_sizes(7, &[]), [7]);
    }

    #[test]
    fn a_count_repeats_and_the_last_batch_takes_the_rest() {
        assert_eq!(batch_sizes(7, &[Amount::Count(3)]), [3, 3, 1]);
    }

    #[test]
    fn values_are_used_in_order_and_the_last_repeats() {
        let serial = [Amount::Count(1), Amount::Count(2)];
        assert_eq!(batch_sizes(6, &serial), [1, 2, 2, 1]);
    }

    #[test]
    fn a_percentage_rounds_down_but_never_below_one_host() {
        assert_eq!(batch_sizes(10, &[Amount::Percent(25)]), [2, 2, 2, 2, 2]);
        assert_eq!(batch_sizes(3, &[Amount::Percent(10)]), [1, 1, 1]);
    }

    #[test]
    fn a_canary_then_a_share() {
        let serial = [Amount::Count(1), Amount::Percent(50)];
        assert_eq!(batch_sizes(9, &serial), [1, 4, 4]);
    }

    #[test]
    fn a_batch_larger_than_the_fleet_is_the_whole_fleet() {
        assert_eq!(batch_sizes(3, &[Amount::Count(10)]), [3]);
        assert_eq!(batch_sizes(4, &[Amount::Percent(100)]), [4]);
    }

    #[test]
    fn no_hosts_is_one_empty_batch() {
        assert_eq!(batch_sizes(0, &[Amount::Count(2)]), [0]);
    }

    #[test]
    fn a_count_limit_stops_only_when_exceeded() {
        let limit = Some(Amount::Count(1));
        assert!(stop_reason(limit, 1, 10, 2, 1).is_none());
        let reason = stop_reason(limit, 2, 10, 2, 1).unwrap();
        assert!(
            reason.contains("2 of 10") && reason.contains("max-fail 1"),
            "{reason}"
        );
    }

    #[test]
    fn max_fail_zero_stops_on_the_first_failure() {
        assert!(stop_reason(Some(Amount::Count(0)), 0, 10, 2, 0).is_none());
        assert!(stop_reason(Some(Amount::Count(0)), 1, 10, 2, 1).is_some());
    }

    /// Measured against every host in the plan, not the batch.
    #[test]
    fn a_percentage_limit_is_a_share_of_all_hosts() {
        let limit = Some(Amount::Percent(20));
        assert!(
            stop_reason(limit, 2, 10, 2, 2).is_none(),
            "20% is not more than 20%"
        );
        let reason = stop_reason(limit, 3, 10, 2, 1).unwrap();
        assert!(reason.contains("max-fail 20%"), "{reason}");
    }

    #[test]
    fn without_max_fail_only_a_wholly_failed_batch_stops() {
        assert!(stop_reason(None, 1, 10, 2, 1).is_none());
        assert!(
            stop_reason(None, 5, 10, 2, 1).is_none(),
            "earlier failures do not count"
        );
        let reason = stop_reason(None, 2, 10, 2, 2).unwrap();
        assert!(reason.contains("every host in the last batch"), "{reason}");
    }

    /// An explicit limit replaces the default, so `max-fail "100%"` never stops.
    #[test]
    fn an_explicit_limit_replaces_the_whole_batch_default() {
        assert!(stop_reason(Some(Amount::Percent(100)), 2, 10, 2, 2).is_none());
    }
}
