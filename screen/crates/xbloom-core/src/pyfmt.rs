//! Small helpers that reproduce Python's numeric semantics where the reference relies on them.
//!
//! The Python reference uses `round()` (round-half-to-even) when it encodes the flow rate and
//! the ratio byte, and formats numbers with `str()`/`repr()` in validation messages. Matching
//! both keeps the Rust port byte-exact and its error messages identical.

use alloc::format;
use alloc::string::String;

/// Python's `round(x)`: round half to even ("banker's rounding").
pub(crate) fn round_half_even(x: f64) -> f64 {
    let r = libm::round(x);
    if libm::fabs(x - libm::trunc(x)) == 0.5 {
        2.0 * libm::round(x / 2.0)
    } else {
        r
    }
}

/// Python's `repr()` of a float: always shows a fractional part for integral values.
pub(crate) fn py_float(x: f64) -> String {
    if x.is_finite() && libm::fabs(x) < 1e16 && x == libm::trunc(x) {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// A number as Python would print it when the YAML gave an int for integral values.
pub(crate) fn py_num(x: f64) -> String {
    if x.is_finite() && libm::fabs(x) < 1e16 && x == libm::trunc(x) {
        format!("{}", x as i64)
    } else {
        format!("{x}")
    }
}

/// Python's `bool` repr.
pub(crate) fn py_bool(b: bool) -> &'static str {
    if b {
        "True"
    } else {
        "False"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_python() {
        assert_eq!(round_half_even(32.5), 32.0);
        assert_eq!(round_half_even(33.5), 34.0);
        assert_eq!(round_half_even(-2.5), -2.0);
        assert_eq!(round_half_even(30.499999999999996), 30.0);
        assert_eq!(round_half_even(31.000000000000004), 31.0);
    }

    #[test]
    fn formatting_matches_python() {
        assert_eq!(py_float(131.0), "131.0");
        assert_eq!(py_float(2.9), "2.9");
        assert_eq!(py_num(15.0), "15");
        assert_eq!(py_num(15.5), "15.5");
    }
}
