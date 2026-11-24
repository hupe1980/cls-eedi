use cls_eedi::metering::{
    controllable_total_power as total, AggregateError, MAX_PARTICIPANTS, MAX_PARTICIPANT_ID_BYTES,
};
use cls_eedi::wire::ScaledNumber;

fn watts(number: i64, scale: i16) -> Option<ScaledNumber> {
    Some(ScaledNumber { number, scale })
}

#[test]
fn missing_invalid_duplicate_and_unprovisioned_samples_are_distinct() {
    let expected = ["pump", "solar"];
    assert_eq!(total(&expected, &[("pump", watts(1, 0))]), Ok(None));
    assert_eq!(
        total(&expected, &[("solar", None), ("pump", watts(1, 0))]),
        Ok(None)
    );
    assert_eq!(
        total(&expected, &[("other", None)]),
        Err(AggregateError::UnexpectedParticipant)
    );
    assert_eq!(
        total(&expected, &[("pump", None), ("pump", watts(1, 0))]),
        Err(AggregateError::DuplicateSample)
    );
    assert_eq!(
        total(&["pump", "pump"], &[]),
        Err(AggregateError::DuplicateParticipant)
    );
    assert_eq!(total(&[], &[]), Err(AggregateError::Empty));
    assert_eq!(total(&[""], &[]), Err(AggregateError::InvalidId));
    assert_eq!(
        total(&[&"x".repeat(MAX_PARTICIPANT_ID_BYTES + 1)], &[]),
        Err(AggregateError::InvalidId)
    );
    assert_eq!(
        total(&vec!["x"; MAX_PARTICIPANTS + 1], &[]),
        Err(AggregateError::Capacity)
    );
    assert_eq!(
        total(&expected, &vec![("pump", None); MAX_PARTICIPANTS + 1]),
        Err(AggregateError::Capacity)
    );
}

#[test]
fn sample_order_and_extreme_cancellation_preserve_exact_decimal_precision() {
    let expected = ["a", "b", "c", "d"];
    let mut samples = [
        ("a", watts(1, i16::MAX)),
        ("b", watts(-9, i16::MAX - 1)),
        ("c", watts(-10, i16::MAX - 2)),
        ("d", watts(i64::MIN, i16::MIN)),
    ];
    assert_eq!(total(&expected, &samples), Ok(watts(i64::MIN, i16::MIN)));
    samples.reverse();
    assert_eq!(total(&expected, &samples), Ok(watts(i64::MIN, i16::MIN)));
    assert_eq!(
        total(
            &["a", "b"],
            &[("b", watts(0, i16::MIN)), ("a", watts(0, i16::MAX))]
        ),
        Ok(watts(0, i16::MIN))
    );
    assert_eq!(
        total(&["a", "b"], &[("a", watts(17, 3)), ("b", watts(-979, 0))]),
        Ok(watts(16_021, 0))
    );
}

#[test]
fn coarsens_only_when_necessary_and_rejects_true_overflow() {
    let expected = ["a", "b"];
    assert_eq!(
        total(&expected, &[("a", watts(i64::MAX, 0)), ("b", watts(3, 0))]),
        Ok(watts(922_337_203_685_477_581, 1))
    );
    assert_eq!(
        total(&expected, &[("a", watts(i64::MIN, 0)), ("b", watts(-2, 0))]),
        Ok(watts(-922_337_203_685_477_581, 1))
    );
    assert_eq!(
        total(
            &expected,
            &[("a", watts(i64::MAX, i16::MAX)), ("b", watts(3, i16::MAX))]
        ),
        Err(AggregateError::Unrepresentable)
    );
    assert_eq!(
        total(
            &expected,
            &[("a", watts(1, i16::MAX)), ("b", watts(1, i16::MIN))]
        ),
        Err(AggregateError::Unrepresentable)
    );
    assert_eq!(
        total(
            &expected,
            &[("a", watts(10, i16::MAX)), ("b", watts(0, i16::MIN))]
        ),
        Ok(watts(1_000_000_000_000_000_000, i16::MAX - 17))
    );
}

#[test]
fn complete_capacity_set_cancels_to_zero_at_finest_scale() {
    let ids: Vec<_> = (0..MAX_PARTICIPANTS).map(|i| i.to_string()).collect();
    let expected: Vec<_> = ids.iter().map(String::as_str).collect();
    let samples: Vec<_> = expected
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let power = if i % 2 == 0 { i64::MAX } else { -i64::MAX };
            (*id, watts(power, i16::MIN + (i / 2) as i16 * 250))
        })
        .collect();
    assert_eq!(total(&expected, &samples), Ok(watts(0, i16::MIN)));
}

#[test]
fn long_chain_of_borrows_cancels_before_wire_overflow_is_decided() {
    // 10^100 - (9 * 10^99 + ... + 9) = 1. The common-scale
    // intermediate exceeds i128, while the exact answer fits at scale zero.
    let ids: Vec<_> = (0..=100).map(|i| i.to_string()).collect();
    let expected: Vec<_> = ids.iter().map(String::as_str).collect();
    let samples: Vec<_> = expected
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, watts(if i == 100 { 1 } else { -9 }, i as i16)))
        .collect();
    assert_eq!(total(&expected, &samples), Ok(watts(1, 0)));
}

#[test]
fn mixed_sign_decimal_sums_agree_with_independent_common_scale_arithmetic() {
    // Every generated sum fits i128 at the common scale, independently of its
    // wire representability. Exercise both mantissa boundaries and opposite signs.
    let mut seed = 42u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };
    for _ in 0..2048 {
        let terms: Vec<_> = ["pump", "pv", "battery"]
            .into_iter()
            .map(|id| {
                (
                    id,
                    ScaledNumber {
                        number: next() as i64,
                        scale: (next() % 13) as i16 - 6,
                    },
                )
            })
            .collect();
        let mut scale = terms.iter().map(|(_, v)| v.scale).min().unwrap();
        let mut sum: i128 = terms
            .iter()
            .map(|(_, v)| i128::from(v.number) * 10i128.pow((v.scale - scale) as u32))
            .sum();
        while i64::try_from(sum).is_err() && sum % 10 == 0 {
            sum /= 10;
            scale += 1;
        }
        let expected = i64::try_from(sum)
            .map(|n| watts(n, scale))
            .map_err(|_| AggregateError::Unrepresentable);
        let samples: Vec<_> = terms.into_iter().map(|(id, v)| (id, Some(v))).collect();
        assert_eq!(total(&["pump", "pv", "battery"], &samples), expected);
    }
}
