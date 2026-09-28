use super::{CatalogError, DatePrecision};

pub(crate) fn required(value: &str, field: &str) -> Result<(), CatalogError> {
    if value.trim().is_empty() {
        Err(CatalogError::Invalid(format!("{field} is required")))
    } else {
        Ok(())
    }
}

pub(crate) fn date(value: &str) -> Result<DatePrecision, CatalogError> {
    let pieces: Vec<&str> = value.split('-').collect();
    if !(pieces.len() == 1 || pieces.len() == 2 || pieces.len() == 3)
        || pieces.iter().enumerate().any(|(index, piece)| {
            piece.len() != if index == 0 { 4 } else { 2 }
                || !piece.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(CatalogError::Invalid(
            "date must be YYYY, YYYY-MM, or YYYY-MM-DD".into(),
        ));
    }
    let year: i32 = pieces[0]
        .parse()
        .map_err(|_| CatalogError::Invalid("invalid year".into()))?;
    if !(1..=9999).contains(&year) {
        return Err(CatalogError::Invalid("invalid year".into()));
    }
    if pieces.len() == 1 {
        return Ok(DatePrecision::Year);
    }
    let month: u32 = pieces[1]
        .parse()
        .map_err(|_| CatalogError::Invalid("invalid month".into()))?;
    if !(1..=12).contains(&month) {
        return Err(CatalogError::Invalid("invalid month".into()));
    }
    if pieces.len() == 2 {
        return Ok(DatePrecision::Month);
    }
    let day: u32 = pieces[2]
        .parse()
        .map_err(|_| CatalogError::Invalid("invalid day".into()))?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let maximum = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => unreachable!(),
    };
    if !(1..=maximum).contains(&day) {
        return Err(CatalogError::Invalid("invalid day".into()));
    }
    Ok(DatePrecision::Day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_preserve_precision_and_require_fixed_widths() {
        for (value, precision) in [
            ("0001", DatePrecision::Year),
            ("2001-01", DatePrecision::Month),
            ("2024-02-29", DatePrecision::Day),
        ] {
            assert_eq!(date(value).unwrap(), precision);
        }
        for value in [
            "1",
            "01",
            "001",
            "02001",
            "1-2-3",
            "2001-1",
            "2001-01-1",
            "2001-001",
            "2023-02-29",
            "0000",
            "2001-13",
        ] {
            assert!(date(value).is_err(), "accepted {value}");
        }
    }
}
