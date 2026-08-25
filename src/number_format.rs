pub(crate) fn integer(value: impl ToString) -> String {
    let raw = value.to_string();
    let (sign, digits) = raw
        .strip_prefix('-')
        .map_or(("", raw.as_str()), |digits| ("-", digits));
    let mut grouped = String::with_capacity(raw.len() + raw.len() / 3);
    grouped.push_str(sign);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(character);
    }
    grouped
}

pub(crate) fn decimal(value: f64, precision: usize) -> String {
    let raw = format!("{value:.precision$}");
    let (whole, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
    let whole = integer(whole);
    if precision == 0 {
        whole
    } else {
        format!("{whole},{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::{decimal, integer};

    #[test]
    fn formats_numbers_with_argentine_separators() {
        assert_eq!(integer(1_234_567_u64), "1.234.567");
        assert_eq!(decimal(1_234_567.89, 2), "1.234.567,89");
        assert_eq!(decimal(-12_345.6, 2), "-12.345,60");
    }
}
