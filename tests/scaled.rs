//! Human-readable metering retains the wire's exact signed decimal value.
use cls_eedi::wire::ScaledNumber;

#[test]
fn fixed_point_meter_readings_preserve_sign_quantum_and_integer_precision() {
    for (number, scale, expected) in [
        (2305, -1, "230.5"),
        (5000, -2, "50.00"),
        (-7, -4, "-0.0007"),
        (750, 2, "75000"),
        (9_007_199_254_740_993, -2, "90071992547409.93"),
        (i64::MIN, 0, "-9223372036854775808"),
        (0, i16::MIN, "0"),
        (0, i16::MAX, "0"),
    ] {
        assert_eq!(ScaledNumber { number, scale }.to_string(), expected);
    }
    let value = ScaledNumber {
        number: 15,
        scale: -1,
    };
    assert_eq!(format!("{value:>6}"), "   1.5");
    assert_eq!(format!("{value:+}"), "+1.5");
    assert_eq!(format!("{value:.0}"), "1.5");
}

#[test]
fn exponent_notation_begins_at_the_signed_display_budget() {
    for (number, scale, expected) in [
        (1, 31, "10000000000000000000000000000000"),
        (1, 32, "1e32"),
        (-1, 31, "-1e31"),
        (-1, -29, "-0.00000000000000000000000000001"),
        (-1, -30, "-1e-30"),
        (i64::MIN, i16::MIN, "-9223372036854775808e-32768"),
        (i64::MAX, i16::MAX, "9223372036854775807e32767"),
    ] {
        assert_eq!(ScaledNumber { number, scale }.to_string(), expected);
    }
}

fn normalized(mut coefficient: i128, mut exponent: i32) -> (i128, i32) {
    if coefficient == 0 {
        return (0, 0);
    }
    while coefficient % 10 == 0 {
        coefficient /= 10;
        exponent += 1;
    }
    (coefficient, exponent)
}

#[test]
fn all_wire_scales_remain_bounded_and_exact() {
    for number in [i64::MIN, -120, 1, 0, i64::MAX] {
        for scale in i16::MIN..=i16::MAX {
            let rendered = ScaledNumber { number, scale }.to_string();
            assert!(rendered.len() <= 32);
            let (mantissa, exponent) = rendered
                .split_once('e')
                .map_or((rendered.as_str(), 0), |(m, e)| {
                    (m, e.parse::<i32>().unwrap())
                });
            let places = mantissa
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len() as i32);
            let coefficient = mantissa.replace('.', "").parse::<i128>().unwrap();
            assert_eq!(
                normalized(coefficient, exponent - places),
                normalized(i128::from(number), i32::from(scale))
            );
        }
    }
}
