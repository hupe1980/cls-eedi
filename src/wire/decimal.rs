//! JSON Schema integers are mathematical integers. A peer may send 4300.0 or
//! 4.3e3. Normalize exact integer tokens before serde_json converts them to f64.
//! A rounding-based conversion can change both control watts and meter readings.
use serde_json::Number;

/// The caller obtains the token from RawValue, which has checked JSON syntax. Keep
/// arbitrary extension numbers as genuine JSON numbers. Known fields can only
/// consume exact integers within their declared ranges.
pub(super) fn number(token: &str) -> Result<Number, serde_json::Error> {
    if let Some(integer) = exact_integer(token) {
        integer.parse()
    } else {
        token.parse()
    }
}

fn exact_integer(text: &str) -> Option<String> {
    if text == "-0" {
        return Some("0".into());
    }
    if !text.contains(['.', 'E', 'e']) {
        return None;
    }
    // RawValue has verified syntax, including exponent signs and leading zeros.
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (mantissa, exponent) = unsigned
        .split_once(['e', 'E'])
        .map_or((unsigned, 0), |(m, e)| {
            let exponent = e.parse::<i32>().unwrap_or(if e.starts_with('-') {
                i32::MIN
            } else {
                i32::MAX
            });
            (m, exponent)
        });
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Some("0".into());
    }
    let leading = significant.trim_end_matches('0');
    let zeros = significant.len() - leading.len();
    let shift =
        i64::from(exponent) - i64::try_from(fraction.len()).ok()? + i64::try_from(zeros).ok()?;
    let shift = usize::try_from(shift).ok()?;
    // The largest wire integer (u64) has 20 digits. The destination field enforces range.
    if leading.len().checked_add(shift)? > 20 {
        return None;
    }
    let mut integer = String::with_capacity(21);
    if text.starts_with('-') {
        integer.push('-');
    }
    integer.push_str(leading);
    integer.extend(std::iter::repeat_n('0', shift));
    Some(integer)
}
